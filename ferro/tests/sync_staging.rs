#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use common::{fast, text};
use ferro::db::Hook;
use ferro::{Db, Error, Keep, Map, Options, Outcome, Reclaim, Snapshot};
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
    let step = (to - from).div_ceil(4).max(1);
    let mut start = from;
    while start < to {
        let end = (start + step).min(to);
        db.commit(format!("keys {start} to {end}"), |root| {
            for key in start..end {
                root.insert(key, text(key, VALUE));
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
        let dir = dir();
        let a: Db<Root> = fast().open(dir.path().join("a.ferro")).unwrap();
        let b: Db<Root> = fast().open(dir.path().join("b.ferro")).unwrap();
        fill(&a, 0, 10);
        assert_eq!(
            b.pull(&a, "a", refuse).unwrap().outcome,
            Outcome::FastForward
        );
        ours(&b.fork("side").unwrap(), 0);
        fill(&a, 10, KEYS);
        let bundle: Vec<u8> = a.bundle(&b.have().unwrap()).unwrap().into();
        assert!(bundle.len() > 4 << 20, "{} bytes", bundle.len());
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

fn child_receiver(path: &str, bundle: &str, step: &'static str) {
    let nth = match step {
        "staging" => 3,
        _ => 1,
    };
    let seen = AtomicUsize::new(0);
    let db: Db<Root> = Options::new()
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
            db.size_on_disk(),
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

fn compact_beside(db: &Db<Root>) -> JoinHandle<ferro::Result<ferro::Compacted>> {
    let db = db.clone();
    thread::spawn(move || db.compact(Keep::All))
}

fn cycle_at(step: &'static str, nth: usize) {
    let case = Case::new();
    let cell: Cell = Arc::default();
    let b = case.open(fast().reclaim(Reclaim { every_bytes: 0 }).hook(at_step(
        step,
        nth,
        cell.clone(),
    )));
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
    let size = b.size_on_disk();
    let again = b.compact(Keep::All).unwrap();
    assert!(again.after_bytes <= size, "{again:?}");
}

#[test]
fn a_cycle_that_replaces_the_file_while_a_bundle_streams_lands_it_in_the_new_file() {
    cycle_at("staging", 2);
}

#[test]
fn a_cycle_that_replaces_the_file_once_a_bundle_is_checked_lands_it_in_the_new_file() {
    cycle_at("received", 1);
}

#[test]
fn a_cycle_due_while_a_bundle_lands_runs_once_it_has_landed() {
    let case = Case::new();
    let cell: Cell = Arc::default();
    let b = case.open(fast().reclaim(Reclaim { every_bytes: 0 }).hook(at_step(
        "placed",
        1,
        cell.clone(),
    )));
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
    fast().shared(true).reclaim(Reclaim { every_bytes: 0 })
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
