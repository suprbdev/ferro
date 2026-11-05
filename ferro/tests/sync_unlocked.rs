mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use common::{Rng, fast, text};
use ferro::db::Hook;
use ferro::store::{Op, Trace};
use ferro::{Db, Error, Keep, Map, Options, Outcome, Reclaim, Snapshot};
use parking_lot::Mutex;
use tempfile::TempDir;

type Root = Map<u64, String>;

const SPILL: usize = 1 << 20;
const OURS: u64 = 1 << 40;
const PATIENCE: Duration = Duration::from_secs(30);

fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    panic!("no merge expected")
}

fn merge_in(ours: &mut Root, base: &Snapshot<Root>, theirs: &Snapshot<Root>) -> (String, ()) {
    for (key, value) in theirs.iter() {
        let was = base.get(&key).map(|old| (*old).clone());
        if was.as_deref() != Some(value.as_str()) {
            ours.insert(key, (*value).clone());
        }
    }
    ("merge".to_string(), ())
}

fn dump(root: &Root) -> BTreeMap<u64, String> {
    root.iter()
        .map(|(key, value)| (key, (*value).clone()))
        .collect()
}

fn fill(db: &Db<Root>, from: u64, to: u64, value: usize) {
    let rounds = 4;
    let step = (to - from).div_ceil(rounds).max(1);
    let mut start = from;
    while start < to {
        let end = (start + step).min(to);
        db.commit(format!("keys {start} to {end}"), |root| {
            for key in start..end {
                root.insert(key, text(key, value));
            }
        })
        .unwrap();
        start = end;
    }
}

struct Pair {
    dir: TempDir,
    a: Db<Root>,
    b: Db<Root>,
    bundle: Vec<u8>,
}

impl Pair {
    fn new(keys: u64, value: usize, options: Options) -> Pair {
        let dir = tempfile::tempdir().unwrap();
        let a: Db<Root> = fast().open(dir.path().join("a.ferro")).unwrap();
        let b: Db<Root> = options.open(dir.path().join("b.ferro")).unwrap();
        fill(&a, 0, 10, value);
        assert_eq!(
            b.pull(&a, "a", refuse).unwrap().outcome,
            Outcome::FastForward
        );
        for name in ["side", "doomed"] {
            b.fork(name)
                .unwrap()
                .commit(format!("start {name}"), |root| {
                    root.insert(OURS - 1, name.to_string());
                })
                .unwrap();
        }
        fill(&a, 10, keys, value);
        let bundle = a.bundle(&b.have().unwrap()).unwrap().into();
        Pair { dir, a, b, bundle }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("b.ferro")
    }

    fn reopen(self) -> (TempDir, Db<Root>) {
        let Pair { dir, a, b, .. } = self;
        drop((a, b));
        let db = fast().open(dir.path().join("b.ferro")).unwrap();
        (dir, db)
    }
}

struct Gated {
    bytes: Vec<u8>,
    pos: usize,
    stop: usize,
    reached: Option<Sender<()>>,
    release: Receiver<()>,
}

fn gated(bytes: Vec<u8>, stop: usize) -> (Gated, Receiver<()>, Sender<()>) {
    let (reached, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let gate = Gated {
        bytes,
        pos: 0,
        stop,
        reached: Some(reached),
        release: released,
    };
    (gate, waiting, release)
}

impl Read for Gated {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.stop
            && let Some(reached) = self.reached.take()
        {
            let _ = reached.send(());
            let _ = self.release.recv();
        }
        let end = if self.pos < self.stop {
            self.stop
        } else {
            self.bytes.len()
        };
        let take = buf.len().min(end - self.pos);
        buf[..take].copy_from_slice(&self.bytes[self.pos..self.pos + take]);
        self.pos += take;
        Ok(take)
    }
}

struct Slow {
    bytes: Vec<u8>,
    pos: usize,
    chunk: usize,
    pause: Duration,
}

impl Read for Slow {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        thread::sleep(self.pause);
        let take = buf.len().min(self.chunk).min(self.bytes.len() - self.pos);
        buf[..take].copy_from_slice(&self.bytes[self.pos..self.pos + take]);
        self.pos += take;
        Ok(take)
    }
}

fn within<R: Send + 'static>(what: &str, run: impl FnOnce() -> R + Send + 'static) -> R {
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(run());
    });
    finished
        .recv_timeout(PATIENCE)
        .unwrap_or_else(|_| panic!("{what} waited for the stream"))
}

fn ours(db: &Db<Root>, key: u64, label: &str) {
    let label = label.to_string();
    db.commit(format!("ours {key}"), move |root| {
        root.insert(OURS + key, label);
    })
    .unwrap();
}

fn stalled_stream_lets_everything_else_run(keys: u64, value: usize, spilled: bool) {
    let pair = Pair::new(keys, value, fast());
    let Pair { a, b, bundle, .. } = &pair;
    assert_eq!(bundle.len() > SPILL, spilled, "{} bytes", bundle.len());
    let (gate, reached, release) = gated(bundle.clone(), bundle.len() / 2);
    let receiver = b.clone();
    let receiving = thread::spawn(move || receiver.receive_from("a", gate, merge_in));
    reached.recv_timeout(PATIENCE).unwrap();
    let main = b.clone();
    within("a commit on the pulling branch", move || {
        ours(&main, 1, "main")
    });
    let side = b.branch("side").unwrap();
    within("a commit on another branch", move || ours(&side, 2, "side"));
    let forking = b.clone();
    within("a fork", move || forking.fork("forked").unwrap());
    let dropping = b.clone();
    within("a drop", move || dropping.drop_branch("doomed").unwrap());
    let compacting = b.clone();
    let cycle = within("a reclamation cycle", move || {
        compacting.compact(Keep::All).unwrap()
    });
    assert!(cycle.after_bytes > 0);
    let main = b.clone();
    within("a commit after the cycle", move || ours(&main, 3, "main"));
    release.send(()).unwrap();
    let pulled = receiving.join().unwrap().unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    let mut want = dump(&a.read());
    want.insert(OURS + 1, "main".to_string());
    want.insert(OURS + 3, "main".to_string());
    assert_eq!(dump(&b.read()), want);
    assert_eq!(
        b.branch("side").unwrap().read().get(&(OURS + 2)).as_deref(),
        Some(&"side".to_string())
    );
    let names: BTreeSet<String> = b.branches().into_iter().collect();
    let expected: BTreeSet<String> = ["a", "forked", "main", "side"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(names, expected);
    b.verify_typed().unwrap();
    let head = b.head_hash();
    let (_dir, reopened) = pair.reopen();
    assert_eq!(reopened.head_hash(), head);
    reopened.verify_typed().unwrap();
}

#[test]
fn a_stalled_stream_that_spills_lets_commits_forks_drops_and_cycles_run() {
    stalled_stream_lets_everything_else_run(2_000, 1_500, true);
}

#[test]
fn a_stalled_stream_held_in_memory_lets_commits_forks_drops_and_cycles_run() {
    stalled_stream_lets_everything_else_run(60, 1_500, false);
}

#[test]
fn a_stream_that_fast_forwards_lands_after_a_cycle_moved_the_file() {
    let pair = Pair::new(2_000, 1_500, fast());
    let Pair { a, b, bundle, .. } = &pair;
    let (gate, reached, release) = gated(bundle.clone(), bundle.len() / 3);
    let receiver = b.clone();
    let receiving = thread::spawn(move || receiver.receive_from("a", gate, refuse));
    reached.recv_timeout(PATIENCE).unwrap();
    let side = b.branch("side").unwrap();
    within("a commit on another branch", move || ours(&side, 2, "side"));
    for _ in 0..2 {
        let compacting = b.clone();
        within("a reclamation cycle", move || {
            compacting.compact(Keep::All).unwrap()
        });
    }
    release.send(()).unwrap();
    let pulled = receiving.join().unwrap().unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(b.head_hash(), a.head_hash());
    assert_eq!(dump(&b.read()), dump(&a.read()));
    b.verify_typed().unwrap();
}

struct Untouched(Arc<AtomicBool>);

impl Read for Untouched {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        self.0.store(true, Ordering::SeqCst);
        Ok(0)
    }
}

#[test]
fn a_closure_receiving_into_its_own_file_is_refused_before_the_stream_is_read() {
    let pair = Pair::new(60, 1_500, fast());
    let side = pair.b.branch("side").unwrap();
    let read = Arc::new(AtomicBool::new(false));
    let input = Untouched(read.clone());
    let b = pair.b.clone();
    let nested = within("a receive inside a closure", move || {
        side.update(|_| b.receive_from("a", input, refuse)).unwrap()
    });
    assert!(matches!(nested, Err(Error::Reentrant)), "{nested:?}");
    assert!(
        !read.load(Ordering::SeqCst),
        "the refused receive read its stream"
    );
    assert_eq!(pair.b.branch("a").unwrap().version(), 4);
}

#[test]
fn a_closure_on_the_receiving_file_may_write_back_while_the_stream_is_read() {
    let pair = Pair::new(60, 1_500, fast());
    let Pair { dir, a, b, bundle } = &pair;
    let other: Db<Root> = fast().open(dir.path().join("other.ferro")).unwrap();
    ours(&other, 0, "seed");
    let elsewhere = other.fork("x").unwrap();
    let (gate, reached, release) = gated(bundle.clone(), bundle.len() / 2);
    let receiving = {
        let (other, b) = (other.clone(), b.clone());
        thread::spawn(move || other.update(|_| b.receive_from("a", gate, refuse)).unwrap())
    };
    reached.recv_timeout(PATIENCE).unwrap();
    let side = b.branch("side").unwrap();
    let written = within(
        "a write back into the receiving closure's file",
        move || {
            side.update(|_| elsewhere.update(|root| set_ours(root, 9)))
                .unwrap()
        },
    );
    assert!(written.is_ok(), "{written:?}");
    release.send(()).unwrap();
    let pulled = receiving.join().unwrap().unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(b.head_hash(), a.head_hash());
    let x = other.branch("x").unwrap();
    assert_eq!(
        x.read().get(&(OURS + 9)).as_deref(),
        Some(&"back".to_string())
    );
    b.verify_typed().unwrap();
    other.verify().unwrap();
}

fn set_ours(root: &mut Root, key: u64) {
    root.insert(OURS + key, "back".to_string());
}

#[test]
fn a_slow_peer_does_not_stall_commits_on_any_branch() {
    let pair = Pair::new(2_000, 1_500, fast());
    let Pair { a, b, bundle, .. } = &pair;
    let slow = Slow {
        bytes: bundle.clone(),
        pos: 0,
        chunk: 32 << 10,
        pause: Duration::from_millis(12),
    };
    let receiver = b.clone();
    let started = Instant::now();
    let receiving = thread::spawn(move || {
        let pulled = receiver.receive_from("a", slow, merge_in);
        (pulled, Instant::now())
    });
    let writers: Vec<_> = ["main", "side"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let db = b.branch(name).unwrap();
            let done = Arc::new(OnceLock::new());
            let stop = done.clone();
            let handle = thread::spawn(move || {
                let mut latencies = Vec::new();
                let mut key = 10 * index as u64;
                while stop.get().is_none() {
                    let began = Instant::now();
                    ours(&db, key, name);
                    latencies.push((began, began.elapsed()));
                    key += 100;
                    thread::sleep(Duration::from_millis(5));
                }
                latencies
            });
            (name, done, handle)
        })
        .collect();
    let (pulled, finished) = receiving.join().unwrap();
    let transfer = finished - started;
    for (_, done, _) in &writers {
        done.set(()).unwrap();
    }
    assert_eq!(pulled.unwrap().outcome, Outcome::Merged);
    assert!(transfer > Duration::from_millis(800), "{transfer:?}");
    for (name, _, handle) in writers {
        let latencies = handle.join().unwrap();
        let during: Vec<Duration> = latencies
            .iter()
            .filter(|(began, _)| *began < finished)
            .map(|(_, took)| *took)
            .collect();
        let slowest = during.iter().max().copied().unwrap_or_default();
        println!(
            "{name}: {} commits during a {transfer:?} transfer, slowest {slowest:?}",
            during.len()
        );
        assert!(during.len() >= 20, "{name} made {} commits", during.len());
        assert!(
            slowest < transfer / 4,
            "a commit on {name} took {slowest:?} during a {transfer:?} transfer"
        );
    }
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    b.verify_typed().unwrap();
}

fn stale_after_pruning(keys: u64, value: usize) {
    let pair = Pair::new(keys, value, fast());
    let Pair { a, b, bundle, .. } = &pair;
    let (gate, reached, release) = gated(bundle.clone(), bundle.len() / 2);
    let receiver = b.clone();
    let receiving = thread::spawn(move || receiver.receive_from("a", gate, merge_in));
    reached.recv_timeout(PATIENCE).unwrap();
    let pruning = b.clone();
    within("pruning the base away", move || {
        pruning.drop_branch("a").unwrap();
        ours(&pruning, 1, "main");
        ours(&pruning, 2, "main");
        pruning.compact(Keep::Last(1)).unwrap();
    });
    release.send(()).unwrap();
    match receiving.join().unwrap() {
        Err(Error::StaleHave(_)) => {}
        other => panic!("landed on a pruned base: {other:?}"),
    }
    assert_eq!(b.branches().len(), 3);
    b.verify_typed().unwrap();
    match b.pull(a, "a", merge_in) {
        Ok(_) | Err(Error::NoCommonAncestor) => {}
        Err(error) => panic!("a fresh pull failed: {error}"),
    }
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    b.verify_typed().unwrap();
}

#[test]
fn a_base_pruned_while_a_spilling_stream_is_read_fails_with_stale_have() {
    stale_after_pruning(2_000, 1_500);
}

#[test]
fn a_base_pruned_while_a_small_stream_is_read_fails_with_stale_have() {
    stale_after_pruning(60, 1_500);
}

type Cell = Arc<Mutex<Option<Db<Root>>>>;

fn hooked(cell: Cell, at: &'static str, run: fn(&Db<Root>)) -> Hook {
    Hook(Arc::new(move |name| {
        if name != at {
            return;
        }
        let db = cell.lock().take();
        if let Some(db) = db {
            run(&db);
        }
    }))
}

fn prune_away_base(db: &Db<Root>) {
    db.drop_branch("a").unwrap();
    ours(db, 1, "main");
    ours(db, 2, "main");
}

#[test]
fn a_base_pruned_between_placing_and_landing_fails_and_leaves_a_file_that_reclaims() {
    let cell: Cell = Arc::default();
    let options = fast()
        .history(Keep::Last(1))
        .reclaim(Reclaim { every_bytes: 0 })
        .hook(hooked(cell.clone(), "placed", prune_away_base));
    let pair = Pair::new(2_000, 1_500, options);
    let Pair { a, b, bundle, .. } = &pair;
    *cell.lock() = Some(b.clone());
    let size = b.size_on_disk();
    match b.receive_from("a", &bundle[..], merge_in) {
        Err(Error::StaleHave(_)) => {}
        other => panic!("landed on a pruned base: {other:?}"),
    }
    assert!(cell.lock().is_none());
    assert!(b.size_on_disk() > size + bundle.len() as u64 / 2);
    b.verify_typed().unwrap();
    let reclaimed = b.compact(Keep::Last(1)).unwrap();
    assert!(
        reclaimed.after_bytes + bundle.len() as u64 / 2 < reclaimed.before_bytes,
        "{reclaimed:?}"
    );
    match b.pull(a, "a", merge_in) {
        Ok(_) | Err(Error::NoCommonAncestor) => {}
        Err(error) => panic!("a fresh pull failed: {error}"),
    }
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    b.verify_typed().unwrap();
}

#[test]
fn a_pull_whose_base_is_pruned_between_placing_and_landing_tries_again_and_lands() {
    let cell: Cell = Arc::default();
    let placed = Arc::new(AtomicUsize::new(0));
    let pruning = hooked(cell.clone(), "placed", prune_away_base);
    let hook = {
        let placed = placed.clone();
        Hook(Arc::new(move |name| {
            if name == "placed" {
                placed.fetch_add(1, Ordering::SeqCst);
            }
            (pruning.0)(name);
        }))
    };
    let options = fast()
        .history(Keep::Last(1))
        .reclaim(Reclaim { every_bytes: 0 })
        .hook(hook);
    let pair = Pair::new(2_000, 1_500, options);
    let Pair { a, b, bundle, .. } = &pair;
    let before = placed.load(Ordering::SeqCst);
    *cell.lock() = Some(b.clone());
    match b.pull(a, "a", merge_in) {
        Ok(_) | Err(Error::NoCommonAncestor) => {}
        Err(error) => panic!("a pull whose base went before it landed: {error}"),
    }
    assert!(cell.lock().is_none());
    assert_eq!(placed.load(Ordering::SeqCst) - before, 2);
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    b.verify_typed().unwrap();
    let reclaimed = b.compact(Keep::Last(1)).unwrap();
    assert!(
        reclaimed.after_bytes + bundle.len() as u64 / 2 < reclaimed.before_bytes,
        "{reclaimed:?}"
    );
    b.verify_typed().unwrap();
}

fn commit_side(db: &Db<Root>) {
    ours(&db.branch("side").unwrap(), 7, "side");
}

#[test]
fn commits_between_placing_and_landing_keep_their_frames() {
    let cell: Cell = Arc::default();
    let options = fast().hook(hooked(cell.clone(), "placed", commit_side));
    let pair = Pair::new(2_000, 1_500, options);
    let Pair { a, b, bundle, .. } = &pair;
    *cell.lock() = Some(b.clone());
    let pulled = b.receive_from("a", &bundle[..], refuse).unwrap();
    assert!(cell.lock().is_none());
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(b.head_hash(), a.head_hash());
    let side = b.branch("side").unwrap();
    assert_eq!(
        side.read().get(&(OURS + 7)).as_deref(),
        Some(&"side".to_string())
    );
    b.verify_typed().unwrap();
    let side_head = side.head_hash();
    drop(side);
    let (_dir, reopened) = pair.reopen();
    assert_eq!(reopened.branch("side").unwrap().head_hash(), side_head);
    reopened.verify_typed().unwrap();
}

#[test]
fn reclamation_due_during_a_large_receive_runs_once_it_lands() {
    let options = fast().history(Keep::Last(8)).reclaim(Reclaim {
        every_bytes: 1 << 20,
    });
    let pair = Pair::new(2_000, 1_500, options);
    let Pair { a, b, bundle, .. } = &pair;
    let before = b.space().unwrap().cycles;
    let pulled = b.receive_from("a", &bundle[..], refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    let waited = Instant::now();
    while b.space().unwrap().cycles == before {
        assert!(waited.elapsed() < PATIENCE, "no cycle ran after the stream");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(b.head_hash(), a.head_hash());
    assert_eq!(b.space().unwrap().last_error, None);
    b.verify_typed().unwrap();
}

fn apply(image: &mut Vec<u8>, op: &Op) {
    match op {
        Op::Write(_, at, bytes) => {
            let end = *at as usize + bytes.len();
            if image.len() < end {
                image.resize(end, 0);
            }
            image[*at as usize..end].copy_from_slice(bytes);
        }
        Op::SetLen(_, len) => image.resize(*len as usize, 0),
        _ => {}
    }
}

fn images(base: &[u8], ops: &[Op], seed: u64) -> Vec<(usize, usize, Vec<u8>)> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::new();
    for cut in 0..=ops.len() {
        for loss in 0..4 {
            let mut image = base.to_vec();
            let mut pending = Vec::new();
            for op in &ops[..cut] {
                match op {
                    Op::Write(..) | Op::SetLen(..) => pending.push(op),
                    Op::Sync(_) => {
                        for done in pending.drain(..) {
                            apply(&mut image, done);
                        }
                    }
                    _ => {}
                }
            }
            for op in pending {
                let keep = match loss {
                    0 => false,
                    1 => true,
                    _ => rng.chance(50),
                };
                if keep {
                    apply(&mut image, op);
                }
            }
            out.push((cut, loss, image));
        }
    }
    out
}

fn acked_at(ops: &[Op], mark: u64) -> usize {
    ops.iter()
        .position(|op| *op == Op::Acked(mark))
        .unwrap_or_else(|| panic!("no ack {mark}"))
}

fn open_cut(target: &Path, image: &[u8], cut: usize, loss: usize) -> Db<Root> {
    fs::write(target, image).unwrap();
    let db = fast()
        .open::<Root>(target)
        .unwrap_or_else(|error| panic!("cut {cut} loss {loss}: {error}"));
    db.verify()
        .unwrap_or_else(|error| panic!("cut {cut} loss {loss}: {error}"));
    let size = fs::metadata(target).unwrap().len();
    assert_eq!(
        size,
        db.size_on_disk(),
        "cut {cut} loss {loss} kept a partial frame"
    );
    db
}

fn commit_side_acked(db: &Db<Root>) {
    commit_side(db);
    db.trace_op(Op::Acked(2));
}

#[test]
fn a_power_cut_at_any_write_of_an_unlocked_receive_keeps_every_acknowledged_commit() {
    let pair = Pair::new(2_000, 1_500, fast());
    let path = pair.path();
    let a_head = pair.a.head_hash();
    let bundle = pair.bundle.clone();
    let old = (pair.b.head_hash(), pair.b.branch("a").unwrap().head_hash());
    let old_side = pair.b.branch("side").unwrap().head_hash();
    drop(pair.b);
    let base = fs::read(&path).unwrap();
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let cell: Cell = Arc::default();
    let b: Db<Root> = Options::new()
        .trace(trace.clone())
        .hook(hooked(cell.clone(), "placed", commit_side_acked))
        .open(&path)
        .unwrap();
    *cell.lock() = Some(b.clone());
    let (gate, reached, release) = gated(bundle.clone(), bundle.len() / 2);
    let receiver = b.clone();
    let receiving = thread::spawn(move || receiver.receive_from("a", gate, merge_in));
    reached.recv_timeout(PATIENCE).unwrap();
    let main = b.clone();
    within("a commit on the pulling branch", move || {
        ours(&main, 1, "main");
        main.trace_op(Op::Acked(1));
    });
    let mid = b.head_hash();
    release.send(()).unwrap();
    let pulled = receiving.join().unwrap().unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    b.trace_op(Op::Acked(3));
    assert!(cell.lock().is_none());
    let merged = (b.head_hash(), dump(&b.read()));
    let new_side = b.branch("side").unwrap().head_hash();
    drop(b);
    let ops = trace.lock().clone();
    let staged = ops.iter().filter(|op| matches!(op, Op::Write(..))).count();
    assert!(staged > 8, "{staged} writes");
    let (first, second, last) = (acked_at(&ops, 1), acked_at(&ops, 2), acked_at(&ops, 3));
    assert!(first < second && second < last);
    let target = pair.dir.path().join("cut.ferro");
    let mut states = BTreeMap::new();
    for (cut, loss, image) in images(&base, &ops, 23) {
        let db = open_cut(&target, &image, cut, loss);
        let remote = db.branch("a").unwrap().head_hash();
        let side = db.branch("side").unwrap().head_hash();
        let main = db.head_hash();
        assert!(remote == old.1 || remote == a_head, "cut {cut} loss {loss}");
        assert!(
            side == old_side || side == new_side,
            "cut {cut} loss {loss}"
        );
        if cut >= first {
            assert_ne!(
                main, old.0,
                "cut {cut} loss {loss} lost an acknowledged commit"
            );
        }
        if cut >= second {
            assert_eq!(
                side, new_side,
                "cut {cut} loss {loss} lost an acknowledged commit"
            );
        }
        if cut >= last {
            assert_eq!(
                main, merged.0,
                "cut {cut} loss {loss} lost the acknowledged pull"
            );
        }
        let state = if main == merged.0 {
            assert_eq!(dump(&db.read()), merged.1, "cut {cut} loss {loss}");
            assert_eq!(remote, a_head, "cut {cut} loss {loss}");
            "merged"
        } else if remote == a_head {
            "landed"
        } else if main == mid {
            "committed"
        } else {
            assert_eq!(main, old.0, "cut {cut} loss {loss}");
            "previous"
        };
        *states.entry(state).or_insert(0) += 1;
    }
    println!("{states:?}");
    assert_eq!(states.len(), 4, "{states:?}");
}
