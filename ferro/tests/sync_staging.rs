#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

mod common;

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::{Tripwire, fast, text};
use ferro::db::Hook;
use ferro::{Db, Durability, Error, Keep, Map, Options, Outcome, Reclaim, Snapshot};
use tempfile::TempDir;

type Root = Map<u64, String>;

const OURS: u64 = 1 << 40;
const KEYS: u64 = 3_000;
const VALUE: usize = 1_500;

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    panic!("no merge expected")
}

fn fill(db: &Db<Root>, from: u64, to: u64) {
    fill_with(db, from, to, VALUE);
}

fn fill_with(db: &Db<Root>, from: u64, to: u64, value: usize) {
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

fn allocated(path: &Path) -> u64 {
    fs::metadata(path).unwrap().blocks() * 512
}

fn ours(db: &Db<Root>, key: u64) {
    db.update(|root| {
        root.insert(OURS + key, format!("ours {key}"));
    })
    .unwrap();
}

/// A sender holding `KEYS` keys, a receiver that pulled its first ten and forked `side`, closed,
/// and a bundle of the rest, well over the 1 MiB a receive holds in memory.
struct Case {
    dir: TempDir,
    a: Db<Root>,
    bundle: Vec<u8>,
}

impl Case {
    fn new() -> Case {
        Case::of(KEYS, VALUE)
    }

    fn of(keys: u64, value: usize) -> Case {
        let dir = dir();
        let a: Db<Root> = fast().open(dir.path().join("a.ferro")).unwrap();
        let b: Db<Root> = fast().open(dir.path().join("b.ferro")).unwrap();
        fill(&a, 0, 10);
        assert_eq!(
            b.pull(&a, "a", refuse).unwrap().outcome,
            Outcome::FastForward
        );
        ours(&b.fork("side").unwrap(), 0);
        fill_with(&a, 10, keys, value);
        let bundle: Vec<u8> = a.bundle(&b.have().unwrap()).unwrap().into();
        assert!(bundle.len() > 2 << 20, "{} bytes", bundle.len());
        Case { dir, a, bundle }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("b.ferro")
    }

    fn open(&self, options: Options) -> Db<Root> {
        options.open(self.path()).unwrap()
    }
}

type Cell = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

/// A hook that runs what `cell` holds, once, at the `nth` time the receive reaches `step`.
fn at_step(step: &'static str, nth: usize, cell: Cell) -> Hook {
    let seen = AtomicUsize::new(0);
    Hook(Arc::new(move |name| {
        if name == step && seen.fetch_add(1, Ordering::SeqCst) + 1 == nth {
            let run = cell.lock().unwrap().take();
            if let Some(run) = run {
                run();
            }
        }
    }))
}

fn shared_with(durability: Durability) -> Options {
    Options::new()
        .durability(durability)
        .shared(true)
        .reclaim(Reclaim::off())
}

fn durability_named(name: &str) -> Durability {
    match name {
        "fsync" => Durability::Fsync,
        _ => Durability::Buffered,
    }
}

fn child_receiver(path: &str, bundle: &str, step: &'static str) {
    let nth = match std::env::var("FERRO_STAGING_NTH") {
        Ok(nth) => nth.parse().unwrap(),
        Err(_) if step == "staging" => 3,
        Err(_) => 1,
    };
    let options = match std::env::var("FERRO_STAGING_SHARED") {
        Ok(durability) => shared_with(durability_named(&durability)),
        Err(_) => Options::new(),
    };
    let seen = AtomicUsize::new(0);
    let db: Db<Root> = options
        .hook(Hook(Arc::new(move |name| {
            if name == step && seen.fetch_add(1, Ordering::SeqCst) + 1 == nth {
                std::process::abort();
            }
        })))
        .open(path)
        .unwrap();
    let bytes = fs::read(bundle).unwrap();
    let pulled = db.receive_from("a", &bytes[..], refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    println!("LANDED");
    std::process::abort();
}

#[test]
fn process_child() {
    let Ok(path) = std::env::var("FERRO_STAGING_PATH") else {
        return;
    };
    let bundle = std::env::var("FERRO_STAGING_BUNDLE").unwrap();
    let step = std::env::var("FERRO_STAGING_STEP").unwrap();
    child_receiver(&path, &bundle, Box::leak(step.into_boxed_str()));
}

#[test]
fn killing_the_process_at_every_step_of_a_large_receive_keeps_the_file_or_the_landing() {
    let case = Case::new();
    let source = case.path();
    let bundle = case.dir.path().join("bundle.bin");
    fs::write(&bundle, &case.bundle).unwrap();
    let before = fs::read(&source).unwrap();
    let (head, remote) = {
        let b = case.open(fast());
        (b.head_hash(), b.branch("a").unwrap().head_hash())
    };
    for step in ["staging", "received", "placed", "landed"] {
        let path = case.dir.path().join(format!("killed-{step}.ferro"));
        fs::write(&path, &before).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("FERRO_STAGING_PATH", &path)
            .env("FERRO_STAGING_BUNDLE", &bundle)
            .env("FERRO_STAGING_STEP", step)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{step}: the child was not killed");
        let landed = String::from_utf8_lossy(&output.stdout).contains("LANDED");
        assert_eq!(landed, step == "landed", "{step}");
        let db: Db<Root> = fast().open(&path).unwrap();
        db.verify_typed().unwrap();
        let want = if landed { case.a.head_hash() } else { head };
        assert_eq!(db.head_hash(), want, "{step}");
        let len = fs::metadata(&path).unwrap().len();
        assert_eq!(
            len,
            db.log_bytes(),
            "{step}: the file keeps a partial frame"
        );
        match landed {
            true => assert_eq!(db.branch("a").unwrap().head_hash(), case.a.head_hash()),
            false => {
                assert_eq!(db.branch("a").unwrap().head_hash(), remote, "{step}");
                assert_eq!(
                    len,
                    before.len() as u64,
                    "{step}: the staging frames of a receive that never landed stayed"
                );
            }
        }
        drop(db);
        let db: Db<Root> = fast().open(&path).unwrap();
        db.verify_typed().unwrap();
    }
}

fn killed_beside_a_live_shared_writer(durability: Durability, name: &str) {
    let case = Case::new();
    let bundle = case.dir.path().join("bundle.bin");
    fs::write(&bundle, &case.bundle).unwrap();
    let before = fs::read(case.path()).unwrap();
    for (step, nth) in [
        ("staging", 1),
        ("staging", 3),
        ("received", 1),
        ("placed", 1),
    ] {
        let label = format!("{name}, killed at {step} {nth}");
        let path = case
            .dir
            .path()
            .join(format!("shared-{name}-{step}-{nth}.ferro"));
        fs::write(&path, &before).unwrap();
        let parent: Db<Root> = shared_with(durability).open(&path).unwrap();
        let remote = parent.branch("a").unwrap().head_hash();
        let side = parent.branch("side").unwrap();
        ours(&side, 1);
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("FERRO_STAGING_PATH", &path)
            .env("FERRO_STAGING_BUNDLE", &bundle)
            .env("FERRO_STAGING_STEP", step)
            .env("FERRO_STAGING_NTH", nth.to_string())
            .env("FERRO_STAGING_SHARED", name)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{label}: the child was not killed"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("LANDED"),
            "{label}"
        );
        for key in 2..6 {
            ours(&side, key);
        }
        assert_eq!(parent.branch("a").unwrap().head_hash(), remote, "{label}");
        parent.verify_typed().unwrap();
        let alongside: Db<Root> = shared_with(durability).open(&path).unwrap();
        alongside.verify_typed().unwrap();
        ours(&alongside.branch("side").unwrap(), 6);
        drop((side, parent, alongside));
        let reopened: Db<Root> = shared_with(durability).open(&path).unwrap();
        reopened.verify_typed().unwrap();
        let side = reopened.branch("side").unwrap().read();
        for key in 0..7 {
            assert_eq!(
                side.get(&(OURS + key)).as_deref(),
                Some(&format!("ours {key}")),
                "{label}: lost the side commit {key}"
            );
        }
        assert_eq!(reopened.branch("a").unwrap().head_hash(), remote, "{label}");
        let pulled = reopened
            .receive_from("a", &case.bundle[..], refuse)
            .unwrap();
        assert_eq!(pulled.outcome, Outcome::FastForward, "{label}");
        drop((side, reopened));
        let exclusive: Db<Root> = fast().open(&path).unwrap();
        exclusive.verify_typed().unwrap();
        assert_eq!(
            exclusive.branch("a").unwrap().head_hash(),
            case.a.head_hash(),
            "{label}"
        );
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            exclusive.log_bytes(),
            "{label}"
        );
    }
}

#[test]
fn killing_a_shared_receiving_process_at_every_step_keeps_a_live_writers_commits_after_it() {
    killed_beside_a_live_shared_writer(Durability::Fsync, "fsync");
    killed_beside_a_live_shared_writer(Durability::Buffered, "buffered");
}

#[derive(Default)]
struct Signals {
    raised: Mutex<HashSet<&'static str>>,
    changed: Condvar,
}

impl Signals {
    fn raise(&self, name: &'static str) {
        self.raised.lock().unwrap().insert(name);
        self.changed.notify_all();
    }

    fn has(&self, name: &'static str) -> bool {
        self.raised.lock().unwrap().contains(name)
    }

    fn wait(&self, name: &'static str) {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut raised = self.raised.lock().unwrap();
        while !raised.contains(name) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "waited too long for {name}");
            raised = self.changed.wait_timeout(raised, left).unwrap().0;
        }
    }
}

#[test]
fn a_cycle_that_closes_the_file_it_replaced_after_a_receive_wrote_past_its_end_lands_the_bundle() {
    let case = Case::new();
    let signals = Arc::new(Signals::default());
    let hook = {
        let signals = signals.clone();
        Hook(Arc::new(move |name| match name {
            "swapped" => {
                signals.raise("swapped");
                signals.wait("go");
            }
            "received" if signals.has("swapped") => {
                signals.raise("go");
                signals.wait("retired");
            }
            "retired" => signals.raise("retired"),
            _ => {}
        }))
    };
    let b = case.open(fast().reclaim(Reclaim::off()).hook(hook));
    let cycle = Mutex::new(None);
    let input = Tripwire::new(&case.bundle, 256 << 10, || {
        *cycle.lock().unwrap() = Some(compact_beside(&b));
        signals.wait("swapped");
    });
    let pulled = b.receive_from("a", input, refuse);
    signals.raise("go");
    let compacted = cycle
        .lock()
        .unwrap()
        .take()
        .expect("the cycle started")
        .join()
        .unwrap();
    compacted.unwrap();
    assert!(signals.has("retired"), "the cycle closed the replaced file");
    let pulled = pulled.unwrap_or_else(|error| {
        panic!("the receive failed once the cycle closed the file it had written past: {error:?}")
    });
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(b.branch("a").unwrap().head_hash(), case.a.head_hash());
    b.verify_typed().unwrap();
    drop(b);
    let b = case.open(fast());
    b.verify_typed().unwrap();
    assert_eq!(b.branch("a").unwrap().head_hash(), case.a.head_hash());
}

#[test]
fn objects_a_landing_refused_after_learning_them_serve_later_commits_through_reopen_and_cycle() {
    let case = Case::new();
    let cell: Arc<Mutex<Option<Db<Root>>>> = Arc::default();
    let hook = {
        let cell = cell.clone();
        Hook(Arc::new(move |name| {
            if name != "received" {
                return;
            }
            let Some(db) = cell.lock().unwrap().take() else {
                return;
            };
            thread::spawn(move || {
                db.drop_branch("a").unwrap();
                ours(&db, 100);
                ours(&db.branch("side").unwrap(), 101);
            })
            .join()
            .unwrap();
        }))
    };
    let b = case.open(
        fast()
            .cache_bytes(1 << 28)
            .reclaim(Reclaim::off())
            .history(Keep::Last(1))
            .hook(hook),
    );
    *cell.lock().unwrap() = Some(b.clone());
    match b.receive_from("a", &case.bundle[..], refuse) {
        Err(Error::StaleHave(_)) => {}
        other => panic!("landed on a base pruned before the landing: {other:?}"),
    }
    let from = b.log_bytes();
    fill(&b, 10, KEYS);
    let grown = b.log_bytes() - from;
    assert!(
        grown < case.bundle.len() as u64 / 8,
        "a commit of the refused bundle's values grew the log by {grown} bytes, not linking them"
    );
    b.verify_typed().unwrap();
    for key in [10, KEYS / 2, KEYS - 1] {
        assert_eq!(b.read().get(&key).as_deref(), Some(&text(key, VALUE)));
    }
    drop(b);
    let b = case.open(fast().reclaim(Reclaim::off()));
    b.verify_typed().unwrap();
    b.compact(Keep::All).unwrap();
    b.verify_typed().unwrap();
    assert_eq!(
        b.read().get(&(KEYS - 1)).as_deref(),
        Some(&text(KEYS - 1, VALUE))
    );
}

fn merge_in(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    ("merge".to_string(), ())
}

fn received_at_once(handles: &[Db<Root>; 3], case: &Case) {
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let side = handles[2].branch("side").unwrap();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut key = 1;
            while !stop.load(Ordering::Relaxed) {
                ours(&side, key);
                key += 1;
            }
            key
        })
    };
    let receivers: Vec<_> = [("a", 0), ("x", 1)]
        .into_iter()
        .map(|(remote, at)| {
            let b = handles[at].clone();
            let bundle = case.bundle.clone();
            thread::spawn(move || b.receive_from(remote, &bundle[..], merge_in).map(|_| ()))
        })
        .collect();
    for receiver in receivers {
        receiver.join().unwrap().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let last = writer.join().unwrap();
    for handle in handles {
        for remote in ["a", "x"] {
            assert_eq!(
                handle.branch(remote).unwrap().head_hash(),
                case.a.head_hash()
            );
        }
        handle.verify_typed().unwrap();
    }
    let side = handles[0].branch("side").unwrap().read();
    for key in 1..last {
        assert_eq!(
            side.get(&(OURS + key)).as_deref(),
            Some(&format!("ours {key}"))
        );
    }
}

#[test]
fn two_receives_staging_at_once_beside_commits_both_land() {
    let case = Case::new();
    for durability in [Durability::Fsync, Durability::Buffered] {
        let path = case.dir.path().join(format!("two-{durability:?}.ferro"));
        fs::copy(case.path(), &path).unwrap();
        let b: Db<Root> = Options::new()
            .durability(durability)
            .reclaim(Reclaim::off())
            .open(&path)
            .unwrap();
        received_at_once(&[b.clone(), b.clone(), b.clone()], &case);
        drop(b);
        let b: Db<Root> = fast().open(&path).unwrap();
        b.verify_typed().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), b.log_bytes());
    }
}

#[test]
fn two_shared_handles_receive_at_once_beside_a_third_committing() {
    let case = Case::new();
    for durability in [Durability::Fsync, Durability::Buffered] {
        let path = case
            .dir
            .path()
            .join(format!("shared-two-{durability:?}.ferro"));
        fs::copy(case.path(), &path).unwrap();
        let handles: [Db<Root>; 3] =
            std::array::from_fn(|_| shared_with(durability).open(&path).unwrap());
        received_at_once(&handles, &case);
        drop(handles);
        let b: Db<Root> = fast().open(&path).unwrap();
        b.verify_typed().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), b.log_bytes());
    }
}

#[test]
fn a_shared_handle_lands_a_bundle_after_another_handle_cycles_mid_stream() {
    let case = Case::new();
    for (step, nth) in [("staging", 1), ("staging", 2), ("received", 1)] {
        let path = case.dir.path().join(format!("cycled-{step}-{nth}.ferro"));
        fs::copy(case.path(), &path).unwrap();
        let other: Db<Root> = shared_with(Durability::Fsync).open(&path).unwrap();
        let cell: Cell = Arc::default();
        {
            let other = other.clone();
            *cell.lock().unwrap() = Some(Box::new(move || {
                compact_beside(&other).join().unwrap().unwrap();
            }));
        }
        let b: Db<Root> = shared_with(Durability::Fsync)
            .hook(at_step(step, nth, cell))
            .open(&path)
            .unwrap();
        let pulled = b.receive_from("a", &case.bundle[..], refuse).unwrap();
        assert_eq!(pulled.outcome, Outcome::FastForward, "{step} {nth}");
        ours(&other.branch("side").unwrap(), 9);
        for handle in [&b, &other] {
            assert_eq!(handle.branch("a").unwrap().head_hash(), case.a.head_hash());
            handle.verify_typed().unwrap();
        }
        drop((b, other));
        let b: Db<Root> = fast().open(&path).unwrap();
        b.verify_typed().unwrap();
        assert_eq!(b.branch("a").unwrap().head_hash(), case.a.head_hash());
        assert_eq!(
            b.branch("side").unwrap().read().get(&(OURS + 9)).as_deref(),
            Some(&"ours 9".to_string())
        );
    }
}

#[test]
fn a_read_only_handle_reads_beside_staging_frames_and_follows_to_the_landing() {
    let case = Case::new();
    let reader: Arc<Mutex<Option<Db<Root>>>> = Arc::default();
    let hook = {
        let (reader, path) = (reader.clone(), case.path());
        Hook(Arc::new(move |name| {
            if name == "staging" && reader.lock().unwrap().is_none() {
                let db: Db<Root> = fast().read_only(true).open(&path).unwrap();
                db.verify_typed().unwrap();
                *reader.lock().unwrap() = Some(db);
            }
        }))
    };
    let b = case.open(fast().hook(hook));
    let remote = b.branch("a").unwrap().head_hash();
    let pulled = b.receive_from("a", &case.bundle[..], refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    let reader = reader
        .lock()
        .unwrap()
        .take()
        .expect("a reader opened mid-stream");
    assert_eq!(reader.branch("a").unwrap().head_hash(), remote);
    reader.refresh().unwrap();
    assert_eq!(reader.branch("a").unwrap().head_hash(), case.a.head_hash());
    reader.verify_typed().unwrap();
}

#[test]
fn a_flipped_byte_in_landed_staged_records_passes_open_and_fails_verify() {
    let case = Case::new();
    let b = case.open(fast());
    let pulled = b.receive_from("a", &case.bundle[..], refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    drop(b);
    let mut image = fs::read(case.path()).unwrap();
    let staged = image
        .windows(8)
        .position(|window| window == b"FERROSTG")
        .expect("a staging frame");
    image[staged + 68 + (300 << 10)] ^= 0x10;
    fs::write(case.path(), &image).unwrap();
    let b = case.open(fast());
    match b.verify() {
        Err(Error::Corrupt(message)) => assert!(message.contains("hash mismatch"), "{message}"),
        other => panic!("verify passed a damaged landed record: {other:?}"),
    }
}

#[test]
fn a_refused_receive_keeps_its_staging_frames_in_the_file_length_until_the_writer_closes() {
    let case = Case::new();
    let path = case.path();
    let b = case.open(fast());
    let mut bytes = case.bundle.clone();
    let at = bytes.len() - 40;
    bytes[at] ^= 0x20;
    assert!(matches!(
        b.receive_from("a", &bytes[..], refuse),
        Err(Error::Bundle(_))
    ));
    let space = b.space().unwrap();
    assert!(
        space.file_bytes > space.log_bytes + (4 << 20),
        "{space:?}: the refused staging frames left the file"
    );
    assert!(
        allocated(&path) < 256 << 10,
        "the refused records kept their disk"
    );
    drop(b);
    let b = case.open(fast());
    let space = b.space().unwrap();
    assert_eq!(space.file_bytes, space.log_bytes, "{space:?}");
    let mut bytes = case.bundle.clone();
    bytes[at] ^= 0x20;
    bytes.truncate(bytes.len() - 3);
    assert!(b.receive_from("a", &bytes[..], refuse).is_err());
    ours(&b.branch("side").unwrap(), 1);
    let space = b.space().unwrap();
    assert!(
        space.log_bytes > (4 << 20) && allocated(&path) < space.log_bytes / 2,
        "{space:?}: the refused frames a commit followed count in the log, not on disk"
    );
}

fn compact_beside(db: &Db<Root>) -> JoinHandle<ferro::Result<ferro::Compacted>> {
    let db = db.clone();
    thread::spawn(move || db.compact(Keep::All))
}

fn cycle_at(step: &'static str, nth: usize) {
    let case = Case::new();
    let cell: Cell = Arc::default();
    let b = case.open(
        fast()
            .reclaim(Reclaim::off())
            .hook(at_step(step, nth, cell.clone())),
    );
    let ran = Arc::new(Mutex::new(None));
    {
        let (b, ran) = (b.clone(), ran.clone());
        *cell.lock().unwrap() = Some(Box::new(move || {
            let compacted = compact_beside(&b).join().unwrap().unwrap();
            *ran.lock().unwrap() = Some(compacted);
        }));
    }
    let pulled = b.receive_from("a", &case.bundle[..], refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    let compacted = ran.lock().unwrap().take().expect("the cycle ran");
    assert!(compacted.after_bytes > 0);
    assert!(
        compacted.after_bytes < case.bundle.len() as u64 / 4,
        "the cycle copied what the receive had staged: {compacted:?}"
    );
    assert_eq!(b.branch("a").unwrap().head_hash(), case.a.head_hash());
    b.verify_typed().unwrap();
    ours(&b.branch("side").unwrap(), 1);
    drop(b);
    let b = case.open(fast());
    assert_eq!(b.branch("a").unwrap().head_hash(), case.a.head_hash());
    b.verify_typed().unwrap();
    let size = b.log_bytes();
    let again = b.compact(Keep::All).unwrap();
    assert!(again.after_bytes <= size, "{again:?}");
}

#[test]
fn a_cycle_that_replaces_the_file_while_a_bundle_streams_lands_it_in_the_new_file() {
    cycle_at("staging", 2);
}

#[test]
fn a_cycle_at_any_staging_frame_lands_the_bundle_whatever_is_left_in_memory() {
    for (keys, value) in [
        (1_600, VALUE),
        (3_000, VALUE),
        (4_000, 600),
        (5_000, 520),
        (2_500, 900),
    ] {
        let case = Case::of(keys, value);
        let source = fs::read(case.path()).unwrap();
        let counted = Arc::new(AtomicUsize::new(0));
        let frames = {
            let counter = counted.clone();
            let path = case.dir.path().join("counted.ferro");
            fs::write(&path, &source).unwrap();
            let b: Db<Root> = fast()
                .hook(Hook(Arc::new(move |name| {
                    if name == "staging" {
                        counter.fetch_add(1, Ordering::SeqCst);
                    }
                })))
                .open(&path)
                .unwrap();
            b.receive_from("a", &case.bundle[..], refuse).unwrap();
            counted.load(Ordering::SeqCst)
        };
        assert!(frames >= 2, "{keys} keys: {frames} staging frames");
        for nth in 1..=frames {
            let label =
                format!("{keys} keys of {value} bytes, a cycle at staging frame {nth} of {frames}");
            let path = case.dir.path().join(format!("cycled-at-{nth}.ferro"));
            fs::write(&path, &source).unwrap();
            let cell: Cell = Arc::default();
            let b: Db<Root> = fast()
                .reclaim(Reclaim::off())
                .hook(at_step("staging", nth, cell.clone()))
                .open(&path)
                .unwrap();
            {
                let b = b.clone();
                *cell.lock().unwrap() = Some(Box::new(move || {
                    compact_beside(&b).join().unwrap().unwrap();
                }));
            }
            let pulled = b
                .receive_from("a", &case.bundle[..], refuse)
                .unwrap_or_else(|error| panic!("{label}: {error}"));
            assert_eq!(pulled.outcome, Outcome::FastForward, "{label}");
            assert_eq!(b.branch("a").unwrap().head_hash(), case.a.head_hash());
            b.verify_typed().unwrap();
        }
    }
}

#[test]
fn a_cycle_that_replaces_the_file_once_a_bundle_is_checked_lands_it_in_the_new_file() {
    cycle_at("received", 1);
}

#[test]
fn a_cycle_due_while_a_bundle_lands_runs_once_it_has_landed() {
    let case = Case::new();
    let cell: Cell = Arc::default();
    let b = case.open(
        fast()
            .reclaim(Reclaim::off())
            .hook(at_step("placed", 1, cell.clone())),
    );
    let cycle = Arc::new(Mutex::new(None));
    {
        let (b, cycle) = (b.clone(), cycle.clone());
        *cell.lock().unwrap() = Some(Box::new(move || {
            *cycle.lock().unwrap() = Some(compact_beside(&b));
        }));
    }
    let pulled = b.receive_from("a", &case.bundle[..], refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    let compacted = cycle
        .lock()
        .unwrap()
        .take()
        .expect("the cycle started")
        .join()
        .unwrap()
        .unwrap();
    assert!(
        compacted.after_bytes > case.bundle.len() as u64 / 2,
        "the cycle ran before the landing: {compacted:?}"
    );
    assert_eq!(b.branch("a").unwrap().head_hash(), case.a.head_hash());
    b.verify_typed().unwrap();
    drop(b);
    let b = case.open(fast());
    assert_eq!(b.branch("a").unwrap().head_hash(), case.a.head_hash());
    b.verify_typed().unwrap();
}

fn refused_after_a_commit(damage: impl Fn(&mut Vec<u8>)) {
    let case = Case::new();
    let cell: Cell = Arc::default();
    let path = case.path();
    let b = case.open(fast().hook(at_step("staging", 2, cell.clone())));
    let (head, remote) = (b.head_hash(), b.branch("a").unwrap().head_hash());
    let taken = allocated(&path);
    {
        let side = b.branch("side").unwrap();
        *cell.lock().unwrap() = Some(Box::new(move || ours(&side, 2)));
    }
    let mut bytes = case.bundle.clone();
    damage(&mut bytes);
    match b.receive_from("a", &bytes[..], refuse) {
        Err(Error::Bundle(_)) => {}
        other => panic!("accepted a damaged bundle: {other:?}"),
    }
    assert!(cell.lock().unwrap().is_none(), "the side commit never ran");
    assert_eq!(b.head_hash(), head);
    assert_eq!(b.branch("a").unwrap().head_hash(), remote);
    let side = b.branch("side").unwrap();
    assert_eq!(
        side.read().get(&(OURS + 2)).as_deref(),
        Some(&"ours 2".to_string())
    );
    let kept = allocated(&path).saturating_sub(taken);
    assert!(
        kept < 256 << 10,
        "a refused bundle kept {kept} bytes of disk"
    );
    assert!(fs::metadata(&path).unwrap().len() > case.bundle.len() as u64);
    b.verify_typed().unwrap();
    ours(&side, 3);
    drop((b, side));
    let b = case.open(fast());
    b.verify_typed().unwrap();
    let compacted = b.compact(Keep::All).unwrap();
    assert!(
        compacted.after_bytes + (1 << 20) < compacted.before_bytes,
        "{compacted:?}"
    );
    let pulled = b.receive_from("a", &case.bundle[..], refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(b.branch("a").unwrap().head_hash(), case.a.head_hash());
    b.verify_typed().unwrap();
}

#[test]
fn a_bundle_refused_once_checked_after_a_commit_followed_its_staging_frames_gives_its_disk_back() {
    refused_after_a_commit(|bytes| {
        let at = bytes.len() - 40;
        bytes[at] ^= 0x20;
    });
}

#[test]
fn a_bundle_cut_short_after_a_commit_followed_its_staging_frames_gives_its_disk_back() {
    refused_after_a_commit(|bytes| bytes.truncate(bytes.len() - 3));
}

fn shared() -> Options {
    fast().shared(true).reclaim(Reclaim::off())
}

fn shared_receive(damaged: bool) {
    let case = Case::new();
    let path = case.path();
    let other: Db<Root> = shared().open(&path).unwrap();
    let cell: Cell = Arc::default();
    let b: Db<Root> = shared()
        .hook(at_step("staging", 2, cell.clone()))
        .open(&path)
        .unwrap();
    let remote = b.branch("a").unwrap().head_hash();
    {
        let side = other.branch("side").unwrap();
        *cell.lock().unwrap() = Some(Box::new(move || ours(&side, 4)));
    }
    let mut bytes = case.bundle.clone();
    if damaged {
        let at = bytes.len() - 40;
        bytes[at] ^= 0x20;
    }
    let received = b.receive_from("a", &bytes[..], refuse);
    assert!(cell.lock().unwrap().is_none(), "the other writer never ran");
    match (damaged, received) {
        (false, Ok(pulled)) => assert_eq!(pulled.outcome, Outcome::FastForward),
        (true, Err(Error::Bundle(_))) => {}
        (_, other) => panic!("damaged {damaged}: {other:?}"),
    }
    let want = match damaged {
        false => case.a.head_hash(),
        true => remote,
    };
    let side = other.branch("side").unwrap();
    ours(&side, 5);
    assert_eq!(other.branch("a").unwrap().head_hash(), want);
    assert_eq!(
        b.branch("side").unwrap().read().get(&(OURS + 5)).as_deref(),
        Some(&"ours 5".to_string())
    );
    other.verify_typed().unwrap();
    b.verify_typed().unwrap();
    drop((b, other, side));
    let b: Db<Root> = shared().open(&path).unwrap();
    assert_eq!(b.branch("a").unwrap().head_hash(), want);
    b.verify_typed().unwrap();
}

#[test]
fn a_shared_writer_commits_between_the_staging_frames_of_another_writers_receive() {
    shared_receive(false);
}

#[test]
fn a_shared_writer_commits_past_the_staging_frames_of_another_writers_refused_receive() {
    shared_receive(true);
}
