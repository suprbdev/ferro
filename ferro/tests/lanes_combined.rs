mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::{self, Read};
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use common::{Rng, cases, fast, text};
use ferro::db::Hook;
use ferro::store::{Op, Trace};
use ferro::{Db, Error, Keep, Map, Options, Outcome, Reclaim, Snapshot};
use parking_lot::Mutex;
use tempfile::TempDir;

type Root = Map<u64, String>;

const MANUAL: Reclaim = Reclaim { every_bytes: 0 };
const PATIENCE: Duration = Duration::from_secs(30);
const OURS: u64 = 1 << 40;
const DRAFTER: &str = "drafter";

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn dump(root: &Root) -> BTreeMap<u64, String> {
    root.iter()
        .map(|(key, value)| (key, (*value).clone()))
        .collect()
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

fn fill(db: &Db<Root>, from: u64, to: u64, value: usize) {
    let step = (to - from).div_ceil(4).max(1);
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

fn reread(path: &Path, options: Options) -> BTreeMap<String, BTreeMap<u64, String>> {
    let db: Db<Root> = options.open(path).unwrap();
    db.verify_typed().unwrap();
    let mut out = BTreeMap::new();
    for name in db.branches() {
        out.insert(name.clone(), dump(&db.branch(&name).unwrap().read()));
    }
    out
}

fn every_branch(db: &Db<Root>) -> BTreeMap<String, BTreeMap<u64, String>> {
    let mut out = BTreeMap::new();
    for name in db.branches() {
        out.insert(name.clone(), dump(&db.branch(&name).unwrap().read()));
    }
    out
}

struct Straddle {
    drafted: Sender<()>,
    placed: Receiver<()>,
}

type Cell = Arc<Mutex<Option<Db<Root>>>>;

fn straddling_hook(
    straddle: Arc<Mutex<Option<Straddle>>>,
    placed_tx: Arc<Mutex<Option<Sender<()>>>>,
    cell: Cell,
    stale: bool,
) -> Hook {
    Hook(Arc::new(move |step| {
        let drafter = thread::current().name() == Some(DRAFTER);
        if step == "drafted" && drafter {
            let held = straddle.lock().take();
            if let Some(Straddle { drafted, placed }) = held {
                drafted.send(()).unwrap();
                placed.recv_timeout(PATIENCE).unwrap();
            }
        }
        if step == "placed" {
            if stale && let Some(db) = cell.lock().take() {
                db.drop_branch("a").unwrap();
                for key in 0..2 {
                    db.commit("prune", |root| {
                        root.insert(OURS + 100 + key, "main".to_string());
                    })
                    .unwrap();
                }
            }
            if let Some(placed) = placed_tx.lock().take() {
                placed.send(()).unwrap();
            }
        }
    }))
}

fn draft_straddling_pieces(stale: bool) {
    let dir = dir();
    let path = dir.path().join("b.ferro");
    let a: Db<Root> = fast()
        .reclaim(MANUAL)
        .open(dir.path().join("a.ferro"))
        .unwrap();
    let straddle = Arc::new(Mutex::new(None));
    let placed_tx = Arc::new(Mutex::new(None));
    let cell: Cell = Arc::default();
    let history = if stale { Keep::Last(1) } else { Keep::All };
    let options = fast()
        .cache_bytes(1 << 28)
        .reclaim(MANUAL)
        .history(history)
        .hook(straddling_hook(
            straddle.clone(),
            placed_tx.clone(),
            cell.clone(),
            stale,
        ));
    let b: Db<Root> = options.clone().open(&path).unwrap();
    fill(&a, 0, 10, 1_500);
    assert_eq!(
        b.pull(&a, "a", merge_in).unwrap().outcome,
        Outcome::FastForward
    );
    let w = b.fork("w").unwrap();
    w.commit("start", |root| {
        root.insert(OURS, "w".to_string());
    })
    .unwrap();
    fill(&a, 10, 2_000, 1_500);
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    assert!(bundle.as_bytes().len() > 1 << 20);
    let (drafted_tx, drafted_rx) = mpsc::channel();
    let (placed_send, placed_rx) = mpsc::channel();
    *straddle.lock() = Some(Straddle {
        drafted: drafted_tx,
        placed: placed_rx,
    });
    *placed_tx.lock() = Some(placed_send);
    if stale {
        *cell.lock() = Some(b.clone());
    }
    let mut want_w = dump(&w.read());
    for key in (10..2_000).step_by(3) {
        want_w.insert(OURS + key, text(key, 1_500));
    }
    let drafting = {
        let w = w.clone();
        thread::Builder::new()
            .name(DRAFTER.to_string())
            .spawn(move || {
                w.commit("same values as the bundle", |root| {
                    for key in (10..2_000).step_by(3) {
                        root.insert(OURS + key, text(key, 1_500));
                    }
                })
            })
            .unwrap()
    };
    drafted_rx.recv_timeout(PATIENCE).unwrap();
    let size_before = b.size_on_disk();
    let received = b.receive("a", &bundle, merge_in);
    drafting.join().unwrap().unwrap();
    match (stale, &received) {
        (true, Err(Error::StaleHave(_))) => {}
        (false, Ok(pulled)) => assert_eq!(pulled.outcome, Outcome::FastForward),
        _ => panic!("stale {stale}: {received:?}"),
    }
    assert!(straddle.lock().is_none() && placed_tx.lock().is_none());
    let grew = b.size_on_disk() - size_before;
    assert!(
        grew < bundle.as_bytes().len() as u64 + (256 << 10),
        "stale {stale}: the moved draft kept its own copies, {grew} bytes"
    );
    assert_eq!(
        dump(&w.read()),
        want_w,
        "stale {stale}: the moved commit reads differently"
    );
    if !stale {
        assert_eq!(dump(&b.read()), dump(&a.read()));
    }
    b.verify_typed().unwrap();
    let before = every_branch(&b);
    let cold = reread(&path, fast().read_only(true).cache_bytes(1));
    assert_eq!(
        cold, before,
        "stale {stale}: a cold reader sees other branches"
    );
    b.compact(Keep::All).unwrap();
    assert_eq!(
        every_branch(&b),
        before,
        "stale {stale}: a cycle changed a branch"
    );
    b.verify_typed().unwrap();
    drop((w, b));
    assert_eq!(
        reread(&path, fast().cache_bytes(1)),
        before,
        "stale {stale}: reopened"
    );
}

#[test]
fn a_draft_that_straddles_a_receives_pieces_reads_back_after_it_lands() {
    draft_straddling_pieces(false);
}

#[test]
fn a_draft_that_straddles_a_refused_receives_pieces_reads_back_and_reclaims() {
    draft_straddling_pieces(true);
}

fn apply_op(image: &mut Vec<u8>, op: &Op) {
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

fn torn(image: &mut Vec<u8>, op: &Op, rng: &mut Rng) {
    let Op::Write(_, at, bytes) = op else {
        return;
    };
    let start = *at as usize;
    let end = start + bytes.len();
    if image.len() < end {
        image.resize(end, 0);
    }
    let mut offset = start;
    while offset < end {
        let stop = ((offset / 512 + 1) * 512).min(end);
        if rng.chance(50) {
            image[offset..stop].copy_from_slice(&bytes[offset - start..stop - start]);
        }
        offset = stop;
    }
}

fn image_at(base: &[u8], ops: &[Op], cut: usize, loss: usize, rng: &mut Rng) -> Vec<u8> {
    let mut image = base.to_vec();
    let mut pending = Vec::new();
    for op in &ops[..cut] {
        match op {
            Op::Write(..) | Op::SetLen(..) => pending.push(op),
            Op::Sync(_) => {
                for done in pending.drain(..) {
                    apply_op(&mut image, done);
                }
            }
            _ => {}
        }
    }
    for op in pending {
        match loss {
            0 => {}
            1 => apply_op(&mut image, op),
            2 if rng.chance(50) => apply_op(&mut image, op),
            2 => {}
            _ => torn(&mut image, op, rng),
        }
    }
    image
}

fn acked(ops: &[Op], mark: u64) -> usize {
    ops.iter()
        .position(|op| *op == Op::Acked(mark))
        .unwrap_or_else(|| panic!("no ack {mark}"))
}

struct Heads {
    main: Option<ferro::Hash>,
    remote: Option<ferro::Hash>,
    w: Option<ferro::Hash>,
    side: Option<ferro::Hash>,
}

fn heads(db: &Db<Root>) -> Heads {
    Heads {
        main: db.head_hash(),
        remote: db.branch("a").unwrap().head_hash(),
        w: db.branch("w").unwrap().head_hash(),
        side: db.branch("side").unwrap().head_hash(),
    }
}

#[test]
fn a_power_cut_while_a_draft_straddles_a_receives_pieces_keeps_every_acknowledged_commit() {
    let dir = dir();
    let path = dir.path().join("b.ferro");
    let a: Db<Root> = fast()
        .reclaim(MANUAL)
        .open(dir.path().join("a.ferro"))
        .unwrap();
    let durable = || Options::new().cache_bytes(1 << 28).reclaim(MANUAL);
    fill(&a, 0, 10, 1_500);
    let bundle;
    let before;
    let before_w;
    {
        let b: Db<Root> = durable().open(&path).unwrap();
        assert_eq!(
            b.pull(&a, "a", merge_in).unwrap().outcome,
            Outcome::FastForward
        );
        for name in ["side", "w"] {
            b.fork(name)
                .unwrap()
                .commit("start", |root| {
                    root.insert(OURS, name.to_string());
                })
                .unwrap();
        }
        fill(&a, 10, 2_000, 1_500);
        bundle = a.bundle(&b.have().unwrap()).unwrap();
        before = heads(&b);
        before_w = dump(&b.branch("w").unwrap().read());
    }
    let base = std::fs::read(&path).unwrap();
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let straddle = Arc::new(Mutex::new(None));
    let placed_tx = Arc::new(Mutex::new(None));
    let (committed_tx, committed_rx) = mpsc::channel();
    let committed = Arc::new(Mutex::new(Some(committed_rx)));
    let straddling = straddling_hook(straddle.clone(), placed_tx.clone(), Cell::default(), false);
    let hook = Hook(Arc::new(move |step| {
        (straddling.0)(step);
        if step == "placed"
            && let Some(committed) = committed.lock().take()
        {
            committed.recv_timeout(PATIENCE).unwrap();
        }
    }));
    let b: Db<Root> = durable()
        .trace(trace.clone())
        .hook(hook)
        .open(&path)
        .unwrap();
    let (drafted_tx, drafted_rx) = mpsc::channel();
    let (placed_send, placed_rx) = mpsc::channel();
    *straddle.lock() = Some(Straddle {
        drafted: drafted_tx,
        placed: placed_rx,
    });
    *placed_tx.lock() = Some(placed_send);
    let mut want_w = before_w.clone();
    for key in (10..2_000).step_by(3) {
        want_w.insert(OURS + key, text(key, 1_500));
    }
    let drafting = {
        let w = b.branch("w").unwrap();
        thread::Builder::new()
            .name(DRAFTER.to_string())
            .spawn(move || {
                w.commit("same values as the bundle", |root| {
                    for key in (10..2_000).step_by(3) {
                        root.insert(OURS + key, text(key, 1_500));
                    }
                })
                .unwrap();
                w.trace_op(Op::Acked(2));
                committed_tx.send(()).unwrap();
            })
            .unwrap()
    };
    drafted_rx.recv_timeout(PATIENCE).unwrap();
    let side = b.branch("side").unwrap();
    side.commit("side", |root| {
        root.insert(OURS + 1, "side".to_string());
    })
    .unwrap();
    b.trace_op(Op::Acked(1));
    let pulled = b.receive("a", &bundle, merge_in).unwrap();
    b.trace_op(Op::Acked(3));
    drafting.join().unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(dump(&b.branch("w").unwrap().read()), want_w);
    let after = heads(&b);
    let want_side = dump(&side.read());
    let want_main = dump(&b.read());
    drop((side, b));
    let ops = trace.lock().clone();
    let (first, second, third) = (acked(&ops, 1), acked(&ops, 2), acked(&ops, 3));
    assert!(first < second && second < third, "{first} {second} {third}");
    let target = dir.path().join("cut.ferro");
    let mut rng = Rng::new(41);
    let mut seen = BTreeSet::new();
    for cut in 0..=ops.len() {
        for loss in 0..4 {
            let context = format!("cut {cut} of {} loss {loss}", ops.len());
            std::fs::write(&target, image_at(&base, &ops, cut, loss, &mut rng)).unwrap();
            let db: Db<Root> = durable()
                .cache_bytes(1)
                .open(&target)
                .unwrap_or_else(|error| panic!("{context}: {error}"));
            db.verify()
                .unwrap_or_else(|error| panic!("{context}: {error}"));
            let found = heads(&db);
            let w_moved = found.w == after.w;
            let side_moved = found.side == after.side;
            let landed = found.remote == after.remote;
            let forwarded = found.main == after.main;
            assert!(w_moved || found.w == before.w, "{context}: w");
            assert!(side_moved || found.side == before.side, "{context}: side");
            assert!(landed || found.remote == before.remote, "{context}: remote");
            assert!(forwarded || found.main == before.main, "{context}: main");
            assert!(
                side_moved || !w_moved,
                "{context}: the moved draft outlived the side commit"
            );
            assert!(
                w_moved || !landed,
                "{context}: the remote landed without the moved draft"
            );
            assert!(
                landed || !forwarded,
                "{context}: main moved before the remote landed"
            );
            assert!(cut < first || side_moved, "{context}: lost the side commit");
            assert!(cut < second || w_moved, "{context}: lost the moved draft");
            assert!(
                cut < third || (landed && forwarded),
                "{context}: lost the pull"
            );
            if w_moved {
                assert_eq!(dump(&db.branch("w").unwrap().read()), want_w, "{context}");
            }
            if side_moved {
                assert_eq!(
                    dump(&db.branch("side").unwrap().read()),
                    want_side,
                    "{context}"
                );
            }
            if forwarded {
                assert_eq!(dump(&db.read()), want_main, "{context}");
            }
            seen.insert((w_moved, side_moved, landed, forwarded));
            db.branch("w")
                .unwrap()
                .commit("after", |root| {
                    root.insert(u64::MAX, "after".to_string());
                })
                .unwrap_or_else(|error| panic!("{context}: {error}"));
            db.verify()
                .unwrap_or_else(|error| panic!("{context}: {error}"));
        }
    }
    assert!(seen.len() >= 4, "{seen:?}");
}

type Mixed = (Map<u64, String>, Map<u64, Map<u64, u64>>);
type Plain = (BTreeMap<u64, String>, BTreeMap<u64, BTreeMap<u64, u64>>);

const BRANCHES: u64 = 4;
const THREAD_BITS: u64 = 36;
const COUNTER: u64 = 1 << 35;
const SHARED: u64 = 1 << 34;
const SHARED_VALUES: u64 = 40;
const PEER: u64 = 60 << 40;
const MAIN: u64 = 61 << 40;
const TEMP: u64 = 62 << 40;

#[derive(Clone)]
enum Edit {
    Put(u64, String),
    Remove(u64),
    Nest(u64, u64, u64),
    Unnest(u64),
}

fn shared(j: u64) -> String {
    text(j.wrapping_mul(7_919) + 11, 600 + (j % 5) as usize * 200)
}

fn space(thread: u64) -> u64 {
    ((thread / 4 + 1) << 40) | ((thread % 4) << THREAD_BITS)
}

fn writer_edits(thread: u64, k: u64) -> Vec<Edit> {
    let base = space(thread);
    let mut edits = vec![
        Edit::Put(base + COUNTER, (k + 1).to_string()),
        Edit::Put(base + k, format!("t{thread}-{k}")),
    ];
    if k.is_multiple_of(3) {
        let j = (k / 3 + thread) % SHARED_VALUES;
        edits.push(Edit::Put(base + SHARED + j, shared(j)));
    }
    if k.is_multiple_of(5) {
        edits.push(Edit::Nest(base + k % 13, k, k * (thread + 1)));
    }
    if k % 7 == 6 {
        edits.push(Edit::Remove(base + SHARED + (k / 7 + 5) % SHARED_VALUES));
    }
    if k % 11 == 10 {
        edits.push(Edit::Unnest(base + (k / 11) % 13));
    }
    edits
}

fn peer_edits(p: u64, width: u64) -> Vec<Edit> {
    let mut edits = Vec::new();
    for x in 0..width {
        let key = PEER + p * width + x;
        edits.push(Edit::Put(key, text(key, 900 + (key % 7) as usize * 150)));
    }
    let j = p % SHARED_VALUES;
    edits.push(Edit::Put(PEER + SHARED + j, shared(j)));
    edits.push(Edit::Nest(PEER + p % 17, p, p + 1));
    edits
}

fn main_edits(k: u64) -> Vec<Edit> {
    let j = (k * 3) % SHARED_VALUES;
    vec![
        Edit::Put(MAIN + k, format!("m{k}")),
        Edit::Put(MAIN + SHARED + j, shared(j)),
    ]
}

fn apply(root: &mut Mixed, edits: &[Edit]) {
    for edit in edits {
        match edit {
            Edit::Put(key, value) => {
                root.0.insert(*key, value.clone());
            }
            Edit::Remove(key) => {
                root.0.remove(key);
            }
            Edit::Nest(outer, key, value) => {
                let mut inner = root
                    .1
                    .get(outer)
                    .map(|inner| (*inner).clone())
                    .unwrap_or_default();
                inner.insert(*key, *value);
                root.1.insert(*outer, inner);
            }
            Edit::Unnest(outer) => {
                root.1.remove(outer);
            }
        }
    }
}

fn apply_plain(plain: &mut Plain, edits: &[Edit]) {
    for edit in edits {
        match edit {
            Edit::Put(key, value) => {
                plain.0.insert(*key, value.clone());
            }
            Edit::Remove(key) => {
                plain.0.remove(key);
            }
            Edit::Nest(outer, key, value) => {
                plain.1.entry(*outer).or_default().insert(*key, *value);
            }
            Edit::Unnest(outer) => {
                plain.1.remove(outer);
            }
        }
    }
}

fn plain(root: &Mixed) -> Plain {
    let flat = root
        .0
        .iter()
        .map(|(key, value)| (key, (*value).clone()))
        .collect();
    let nested = root
        .1
        .iter()
        .map(|(key, inner)| {
            (
                key,
                inner.iter().map(|(key, value)| (key, *value)).collect(),
            )
        })
        .collect();
    (flat, nested)
}

fn within_space(plain: &Plain, low: u64, high: u64) -> Plain {
    let flat = plain
        .0
        .range(low..high)
        .map(|(key, value)| (*key, value.clone()))
        .collect();
    let nested = plain
        .1
        .range(low..high)
        .map(|(key, inner)| (*key, inner.clone()))
        .collect();
    (flat, nested)
}

fn of_thread(plain: &Plain, thread: u64) -> Plain {
    within_space(plain, space(thread), space(thread) + (1 << THREAD_BITS))
}

fn merge_mixed(ours: &mut Mixed, base: &Snapshot<Mixed>, theirs: &Snapshot<Mixed>) -> (String, ()) {
    for (key, value) in theirs.0.iter() {
        if base.0.get(&key).as_deref() != Some(&*value) {
            ours.0.insert(key, (*value).clone());
        }
    }
    for (key, _) in base.0.iter() {
        if theirs.0.get(&key).is_none() {
            ours.0.remove(&key);
        }
    }
    for (key, inner) in theirs.1.iter() {
        if base.1.get(&key).as_deref() != Some(&*inner) {
            ours.1.insert(key, (*inner).clone());
        }
    }
    for (key, _) in base.1.iter() {
        if theirs.1.get(&key).is_none() {
            ours.1.remove(&key);
        }
    }
    ("merge".to_string(), ())
}

struct Models {
    cache: BTreeMap<u64, (u64, Plain)>,
}

impl Models {
    fn new() -> Models {
        Models {
            cache: BTreeMap::new(),
        }
    }

    fn at(&mut self, thread: u64, steps: u64) -> &Plain {
        let (done, model) = self.cache.entry(thread).or_insert((0, Plain::default()));
        assert!(
            *done <= steps,
            "thread {thread} went back from {done} to {steps} commits"
        );
        while *done < steps {
            apply_plain(model, &writer_edits(thread, *done));
            *done += 1;
        }
        model
    }
}

fn steps_of(plain: &Plain, thread: u64) -> u64 {
    plain
        .0
        .get(&(space(thread) + COUNTER))
        .map_or(0, |count| count.parse().unwrap())
}

fn check_branch(plain: &Plain, threads: &[u64], models: &mut Models, context: &str) {
    for &thread in threads {
        let steps = steps_of(plain, thread);
        let want = models.at(thread, steps);
        assert!(
            of_thread(plain, thread) == *want,
            "{context}: thread {thread} at {steps} commits reads differently"
        );
    }
}

struct Flag {
    thread: thread::Thread,
    woken: AtomicBool,
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::SeqCst);
        self.thread.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let flag = Arc::new(Flag {
        thread: thread::current(),
        woken: AtomicBool::new(false),
    });
    let waker = Waker::from(flag.clone());
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        while !flag.woken.swap(false, Ordering::SeqCst) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "an awaited write was never woken");
            thread::park_timeout(left);
        }
    }
}

struct Config {
    name: &'static str,
    options: fn() -> Options,
    coalesce: bool,
    per_branch: u64,
    seconds: u64,
    cache: u64,
}

#[derive(Default)]
struct Run {
    stop: AtomicBool,
    peer_commits: AtomicU64,
    received: AtomicU64,
    audit: Mutex<BTreeSet<u64>>,
    other: Mutex<BTreeSet<u64>>,
    events: Mutex<BTreeMap<&'static str, u64>>,
}

impl Run {
    fn count(&self, what: &'static str) {
        *self.events.lock().entry(what).or_insert(0) += 1;
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

fn audit_key(thread: u64, k: u64) -> u64 {
    (thread << 32) | k
}

struct Sides<'a> {
    zz: &'a Db<Mixed>,
    other: &'a Db<Mixed>,
}

fn write_side(run: &Run, config: &Config, sides: &Sides<'_>, thread: u64, k: u64) {
    let key = audit_key(thread, k);
    if k % 3 == 1 {
        let inner = sides.zz.update(|root| {
            root.0.insert(key, format!("audit {thread} {k}"));
        });
        match (config.coalesce, inner) {
            (false, Ok(())) => {
                run.audit.lock().insert(key);
                run.count("closure wrote a later branch");
            }
            (true, Err(Error::Reentrant)) => run.count("coalesced closure refused"),
            (coalesce, outcome) => panic!("coalesce {coalesce}: inner write gave {outcome:?}"),
        }
    }
    if k % 4 == 2 {
        let inner = sides.other.update(|root| {
            root.0.insert(key, format!("other {thread} {k}"));
        });
        match inner {
            Ok(()) => {
                run.other.lock().insert(key);
                run.count("closure wrote another file");
            }
            Err(error) => panic!("a write into another file failed: {error}"),
        }
    }
}

fn run_writer(run: &Run, config: &Config, db: &Db<Mixed>, sides: &Sides<'_>, thread: u64) -> u64 {
    let mut k = 0;
    let mut attempt = 0u64;
    while !run.stopped() {
        attempt += 1;
        let edits = writer_edits(thread, k);
        if attempt % 13 == 7 {
            let refused: Result<(), Error> = db.try_update(|root| {
                apply(root, &edits);
                Err(Error::NoVersion(attempt))
            });
            assert!(matches!(refused, Err(Error::NoVersion(_))), "{refused:?}");
            run.count("refused closure");
            continue;
        }
        let body = |root: &mut Mixed| {
            apply(root, &edits);
            write_side(run, config, sides, thread, k);
        };
        if (thread / 4 + thread % 4) % 2 == 1 {
            block_on(db.update_async(body)).unwrap();
            run.count("awaited commit");
        } else {
            db.update(body).unwrap();
            run.count("blocking commit");
        }
        k += 1;
        thread::sleep(Duration::from_millis(1 + config.per_branch));
    }
    k
}

fn run_peer(run: &Run, peer: &Db<Mixed>) {
    let mut p = 0;
    while !run.stopped() {
        if p >= run.received.load(Ordering::SeqCst) + 100 {
            thread::sleep(Duration::from_millis(2));
            continue;
        }
        let edits = peer_edits(p, 20);
        peer.update(|root| apply(root, &edits)).unwrap();
        p += 1;
        run.peer_commits.store(p, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(6));
    }
}

struct Paced {
    bytes: Vec<u8>,
    pos: usize,
    chunk: usize,
}

impl Read for Paced {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos % (256 << 10) < self.chunk {
            thread::sleep(Duration::from_millis(1));
        }
        let take = buf.len().min(self.chunk).min(self.bytes.len() - self.pos);
        buf[..take].copy_from_slice(&self.bytes[self.pos..self.pos + take]);
        self.pos += take;
        Ok(take)
    }
}

fn receive_once(run: &Run, main: &Db<Mixed>, peer: &Db<Mixed>, round: u64) -> bool {
    let outcome = if round.is_multiple_of(2) {
        main.pull(peer, "peer", merge_mixed)
    } else {
        let bundle = peer.bundle(&main.have().unwrap()).unwrap();
        if bundle.as_bytes().len() > 1 << 20 {
            run.count("spilled bundle");
        }
        let paced = Paced {
            bytes: bundle.into(),
            pos: 0,
            chunk: 16 << 10,
        };
        main.receive_from("peer", paced, merge_mixed)
    };
    match outcome {
        Ok(pulled) => run.count(match pulled.outcome {
            Outcome::FastForward => "fast forward",
            Outcome::Merged => "merged",
            Outcome::UpToDate => "up to date",
            Outcome::Ahead => "ahead",
        }),
        Err(Error::StaleHave(_)) => {
            run.count("stale have");
            return false;
        }
        Err(error) => panic!("receive {round} failed: {error}"),
    }
    true
}

fn run_main(run: &Run, main: &Db<Mixed>, zz: &Db<Mixed>, coalesce: bool) -> Plain {
    let mut model = Plain::default();
    let mut k = 0;
    while !run.stopped() {
        let edits = main_edits(k);
        main.update(|root| {
            apply(root, &edits);
            if k % 5 == 0 {
                let inner = zz.update(|root| {
                    root.0.insert(audit_key(99, k), "main".to_string());
                });
                match (coalesce, inner) {
                    (false, Ok(())) => {
                        run.audit.lock().insert(audit_key(99, k));
                    }
                    (true, Err(Error::Reentrant)) => {}
                    (_, outcome) => panic!("main's inner write gave {outcome:?}"),
                }
            }
        })
        .unwrap();
        apply_plain(&mut model, &edits);
        k += 1;
        thread::sleep(Duration::from_millis(8));
    }
    model
}

fn run_cycles(run: &Run, db: &Db<Mixed>) {
    let mut round = 0u64;
    while !run.stopped() {
        let keep = match round % 3 {
            0 => Keep::All,
            1 => Keep::Last(400),
            _ => Keep::Last(250),
        };
        db.compact(keep).unwrap();
        run.count("cycle");
        round += 1;
        thread::sleep(Duration::from_millis(40));
    }
}

fn run_forks(run: &Run, main: &Db<Mixed>, writers: &[Db<Mixed>], per_branch: u64) {
    let mut round = 0u64;
    let mut models = Models::new();
    while !run.stopped() {
        let branch = round % BRANCHES;
        let forked = writers[branch as usize].fork("tmp").unwrap();
        let start = plain(&forked.read());
        let threads: Vec<u64> = (0..per_branch).map(|t| branch * 4 + t).collect();
        check_branch(&start, &threads, &mut models, "a fork");
        forked
            .update(|root| {
                root.0.insert(TEMP + round, "temp".to_string());
            })
            .unwrap();
        let mut want = start.clone();
        want.0.insert(TEMP + round, "temp".to_string());
        assert!(
            plain(&forked.read()) == want,
            "a commit on a fork reads differently"
        );
        drop(forked);
        main.drop_branch("tmp").unwrap();
        run.count("fork and drop");
        round += 1;
        thread::sleep(Duration::from_millis(5));
    }
}

fn run_reader(run: &Run, writers: &[Db<Mixed>], per_branch: u64) {
    let mut models = Models::new();
    let mut versions = vec![0u64; writers.len()];
    while !run.stopped() {
        for (branch, db) in writers.iter().enumerate() {
            let snapshot = db.read();
            assert!(
                snapshot.version() >= versions[branch],
                "branch {branch} went back from v{} to v{}",
                versions[branch],
                snapshot.version()
            );
            versions[branch] = snapshot.version();
            let threads: Vec<u64> = (0..per_branch).map(|t| branch as u64 * 4 + t).collect();
            check_branch(&plain(&snapshot), &threads, &mut models, "a reader");
        }
        run.count("read round");
        thread::sleep(Duration::from_millis(2));
    }
}

fn run_watcher(run: &Run, db: &Db<Mixed>) {
    let watcher = db.watch();
    let mut last = 0;
    while !run.stopped() {
        if let Ok(snapshot) = watcher.recv_timeout(Duration::from_millis(20)) {
            assert!(
                snapshot.version() > last,
                "a watcher saw v{} after v{last}",
                snapshot.version()
            );
            last = snapshot.version();
            run.count("watched");
        }
    }
}

fn every_mixed(db: &Db<Mixed>) -> BTreeMap<String, Plain> {
    let mut out = BTreeMap::new();
    for name in db.branches() {
        out.insert(name.clone(), plain(&db.branch(&name).unwrap().read()));
    }
    out
}

struct Expected<'a> {
    config: &'a Config,
    start: Plain,
    done: BTreeMap<u64, u64>,
    audit: BTreeSet<u64>,
    main: Plain,
}

fn check_all(local: &Db<Mixed>, peer: &Db<Mixed>, expected: &Expected<'_>, context: &str) {
    let name = expected.config.name;
    let mut models = Models::new();
    for branch in 0..BRANCHES {
        let writer = local.branch(&format!("w{branch}")).unwrap();
        let got = plain(&writer.read());
        let owned: Vec<u64> = (0..expected.config.per_branch)
            .map(|t| branch * 4 + t)
            .collect();
        for &thread in &owned {
            assert_eq!(
                steps_of(&got, thread),
                expected.done[&thread],
                "{name} {context}: thread {thread}"
            );
        }
        check_branch(&got, &owned, &mut models, &format!("{name} {context}"));
        let mut rest = got.clone();
        for &thread in &owned {
            let low = space(thread);
            rest.0
                .retain(|key, _| !(low..low + (1 << THREAD_BITS)).contains(key));
            rest.1
                .retain(|key, _| !(low..low + (1 << THREAD_BITS)).contains(key));
        }
        assert!(
            rest == expected.start,
            "{name} {context}: branch w{branch} holds foreign keys"
        );
    }
    let zz_keys: BTreeSet<u64> = local
        .branch("zz")
        .unwrap()
        .read()
        .0
        .iter()
        .map(|(key, _)| key)
        .collect();
    let mut want_zz: BTreeSet<u64> = expected.start.0.keys().copied().collect();
    want_zz.extend(expected.audit.iter().copied());
    assert!(
        zz_keys == want_zz,
        "{name} {context}: the audit branch differs"
    );
    let peer_plain = plain(&peer.read());
    assert!(
        plain(&local.branch("peer").unwrap().read()) == peer_plain,
        "{name} {context}: the tracking branch differs from the peer"
    );
    let mut want_main = peer_plain;
    want_main.0.extend(expected.main.0.clone());
    assert!(
        plain(&local.read()) == want_main,
        "{name} {context}: main is not the peer and its own commits"
    );
    local.verify_typed().unwrap();
}

fn combined(config: &Config) -> BTreeMap<&'static str, u64> {
    let dir = dir();
    let path = dir.path().join("local.ferro");
    let options = || {
        (config.options)()
            .coalesce(config.coalesce)
            .cache_bytes(config.cache)
            .reclaim(MANUAL)
    };
    let local: Db<Mixed> = options().open(&path).unwrap();
    let peer: Db<Mixed> = fast()
        .reclaim(MANUAL)
        .open(dir.path().join("peer.ferro"))
        .unwrap();
    let other: Db<Mixed> = options().open(dir.path().join("other.ferro")).unwrap();
    peer.update(|root| apply(root, &peer_edits(1_000_000, 3)))
        .unwrap();
    assert_eq!(
        local.pull(&peer, "peer", merge_mixed).unwrap().outcome,
        Outcome::FastForward
    );
    let writers: Vec<Db<Mixed>> = (0..BRANCHES)
        .map(|branch| local.fork(&format!("w{branch}")).unwrap())
        .collect();
    let zz = local.fork("zz").unwrap();
    let start = plain(&local.read());
    let run = Run::default();
    let threads: Vec<u64> = (0..BRANCHES)
        .flat_map(|branch| (0..config.per_branch).map(move |t| branch * 4 + t))
        .collect();
    let sides = Sides {
        zz: &zz,
        other: &other,
    };
    let (main_model, done) = thread::scope(|scope| {
        let mut writing = Vec::new();
        for &thread in &threads {
            let (run, sides, db) = (&run, &sides, &writers[(thread / 4) as usize]);
            writing.push(scope.spawn(move || (thread, run_writer(run, config, db, sides, thread))));
        }
        let peering = scope.spawn(|| run_peer(&run, &peer));
        let receiving = scope.spawn(|| {
            let mut round = 0;
            let mut seen = 0;
            while !run.stopped() {
                if round % 5 == 4 {
                    while !run.stopped() && run.peer_commits.load(Ordering::SeqCst) < seen + 50 {
                        thread::sleep(Duration::from_millis(5));
                    }
                }
                seen = run.peer_commits.load(Ordering::SeqCst);
                if receive_once(&run, &local, &peer, round) {
                    run.received.store(seen, Ordering::SeqCst);
                }
                round += 1;
                thread::sleep(Duration::from_millis(20));
            }
        });
        let maining = scope.spawn(|| run_main(&run, &local, &zz, config.coalesce));
        let cycling = scope.spawn(|| run_cycles(&run, &local));
        let forking = scope.spawn(|| run_forks(&run, &local, &writers, config.per_branch));
        let reading = scope.spawn(|| run_reader(&run, &writers, config.per_branch));
        let watching: Vec<_> = writers
            .iter()
            .chain([&zz, &local])
            .map(|db| {
                let run = &run;
                scope.spawn(move || run_watcher(run, db))
            })
            .collect();
        thread::sleep(Duration::from_secs(config.seconds));
        run.stop.store(true, Ordering::SeqCst);
        let done: BTreeMap<u64, u64> = writing.into_iter().map(|h| h.join().unwrap()).collect();
        peering.join().unwrap();
        receiving.join().unwrap();
        let main_model = maining.join().unwrap();
        cycling.join().unwrap();
        forking.join().unwrap();
        reading.join().unwrap();
        watching.into_iter().for_each(|h| h.join().unwrap());
        (main_model, done)
    });
    let name = config.name;
    let last = local.pull(&peer, "peer", merge_mixed).unwrap();
    assert!(
        !matches!(last.outcome, Outcome::FastForward),
        "{name}: {:?}",
        last.outcome
    );
    let expected = Expected {
        config,
        start,
        done,
        audit: run.audit.lock().clone(),
        main: main_model,
    };
    check_all(&local, &peer, &expected, "after the run");
    let other_keys: BTreeSet<u64> = other.read().0.iter().map(|(key, _)| key).collect();
    assert!(
        other_keys == *run.other.lock(),
        "{name}: the other file differs"
    );
    other.verify_typed().unwrap();
    let before = every_mixed(&local);
    let cold: Db<Mixed> = (config.options)()
        .read_only(true)
        .cache_bytes(1)
        .open(&path)
        .unwrap();
    assert!(
        every_mixed(&cold) == before,
        "{name}: a cold reader differs"
    );
    drop(cold);
    drop((writers, zz, local));
    let local: Db<Mixed> = options().cache_bytes(1).open(&path).unwrap();
    assert!(
        every_mixed(&local) == before,
        "{name}: the reopened file differs"
    );
    check_all(&local, &peer, &expected, "after reopening");
    local.compact(Keep::Last(2)).unwrap();
    assert!(
        every_mixed(&local) == before,
        "{name}: compaction changed a branch"
    );
    check_all(&local, &peer, &expected, "after compacting");
    let events = run.events.into_inner();
    println!("{name}: {events:?}");
    events
}

fn durable() -> Options {
    Options::new()
}

#[test]
fn branches_closures_files_receives_and_cycles_match_a_model() {
    let configs = [
        Config {
            name: "buffered",
            options: fast,
            coalesce: false,
            per_branch: 1,
            seconds: u64::from(cases(4)),
            cache: 1 << 26,
        },
        Config {
            name: "buffered coalesced",
            options: fast,
            coalesce: true,
            per_branch: 2,
            seconds: u64::from(cases(4)),
            cache: 1 << 16,
        },
        Config {
            name: "fsync",
            options: durable,
            coalesce: false,
            per_branch: 1,
            seconds: u64::from(cases(3)),
            cache: 1 << 20,
        },
    ];
    for config in &configs {
        let events = combined(config);
        for wanted in ["cycle", "fork and drop", "read round", "watched"] {
            assert!(
                events.get(wanted).is_some_and(|n| *n > 0),
                "{}: no {wanted}",
                config.name
            );
        }
    }
}
