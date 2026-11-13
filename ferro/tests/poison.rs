#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::pin::pin;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use common::{fast, text, uncached};
use ferro::{Error, Map, Options};

type Root = Map<u64, String>;

const COALESCED_BRANCHES: [&str; 4] = ["main", "b1", "b2", "b3"];

struct Flag {
    thread: Thread,
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
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        while !flag.woken.swap(false, Ordering::SeqCst) {
            thread::park();
        }
    }
}

fn child_body(path: &str) {
    let db = fast().open::<Root>(path).unwrap();
    let mut model = BTreeMap::new();
    let mut failure = None;
    for key in 0..200_000u64 {
        let value = text(key, 700);
        match db.update(|root| {
            root.insert(key, value.clone());
        }) {
            Ok(()) => {
                model.insert(key, value);
            }
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    assert!(
        matches!(failure, Some(Error::Io(_))),
        "write limit never hit: {failure:?}"
    );
    let version = db.version();
    assert!(version > 0);
    assert!(matches!(
        db.update(|root| {
            root.insert(u64::MAX, "x".into());
        }),
        Err(Error::Poisoned)
    ));
    assert!(matches!(db.fork("other"), Err(Error::Poisoned)));
    assert_eq!(db.version(), version);
    let snapshot = db.read();
    assert_eq!(snapshot.len(), model.len());
    for (key, value) in &model {
        assert_eq!(snapshot.get(key).as_deref(), Some(value));
    }
    db.verify().unwrap();
    let report = format!("{version} {}", model.len());
    fs::write(format!("{path}.report"), report).unwrap();
}

#[test]
fn write_limit_child() {
    if let Ok(path) = std::env::var("FERRO_POISON_PATH") {
        child_body(&path);
    }
}

fn durable_group_child(path: &str) {
    let db = Options::new().open::<Root>(path).unwrap();
    let acked = Arc::new(Mutex::new(BTreeMap::new()));
    let handles: Vec<_> = (0..4u64)
        .map(|writer| {
            let (db, acked) = (db.clone(), acked.clone());
            thread::spawn(move || {
                for round in 0..200_000u64 {
                    let key = writer * 1_000_000 + round;
                    let value = text(key, 700);
                    match db.update(|root| {
                        root.insert(key, value.clone());
                    }) {
                        Ok(()) => {
                            acked.lock().unwrap().insert(key, value);
                        }
                        Err(Error::Io(_) | Error::Poisoned) => return,
                        Err(other) => panic!("unexpected {other}"),
                    }
                }
                panic!("write limit never hit");
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let acked = acked.lock().unwrap();
    assert!(!acked.is_empty());
    assert!(matches!(db.update(|_| ()), Err(Error::Poisoned)));
    let snapshot = db.read();
    assert_eq!(snapshot.len(), acked.len());
    let report: Vec<String> = acked.keys().map(u64::to_string).collect();
    fs::write(format!("{path}.report"), report.join(" ")).unwrap();
}

#[test]
fn durable_write_limit_child() {
    if let Ok(path) = std::env::var("FERRO_POISON_GROUP_PATH") {
        durable_group_child(&path);
    }
}

fn coalesced_group_child(path: &str) {
    let main = Options::new().coalesce(true).open::<Root>(path).unwrap();
    let mut dbs = vec![main.clone()];
    for name in &COALESCED_BRANCHES[1..] {
        dbs.push(main.fork(name).unwrap());
    }
    let acked = Arc::new(Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..12u64)
        .map(|writer| {
            let at = writer as usize % COALESCED_BRANCHES.len();
            let (db, acked) = (dbs[at].clone(), acked.clone());
            thread::spawn(move || {
                for round in 0..200_000u64 {
                    let key = writer * 1_000_000 + round;
                    let value = text(key, 700);
                    let write = |root: &mut Root| {
                        root.insert(key, value.clone());
                    };
                    let done = match writer % 3 {
                        0 => block_on(db.update_async(write)),
                        _ => db.update(write),
                    };
                    match done {
                        Ok(()) => acked.lock().unwrap().push((at, key)),
                        Err(Error::Io(_) | Error::Poisoned | Error::GroupFailed(_)) => return,
                        Err(other) => panic!("unexpected {other}"),
                    }
                }
                panic!("write limit never hit");
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let acked = acked.lock().unwrap();
    assert!(!acked.is_empty());
    for db in &dbs {
        assert!(matches!(db.update(|_| ()), Err(Error::Poisoned)));
    }
    for (at, key) in acked.iter() {
        let snapshot = dbs[*at].read();
        assert_eq!(snapshot.get(key).as_deref(), Some(&text(*key, 700)));
    }
    let report: Vec<String> = acked
        .iter()
        .map(|(at, key)| format!("{at}:{key}"))
        .collect();
    fs::write(format!("{path}.report"), report.join(" ")).unwrap();
}

#[test]
fn coalesced_write_limit_child() {
    if let Ok(path) = std::env::var("FERRO_POISON_COALESCED_PATH") {
        coalesced_group_child(&path);
    }
}

#[test]
fn failed_write_poisons_coalesced_writers_on_every_branch_and_keeps_what_was_acknowledged() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("coalesced.ferro");
    let exe = std::env::current_exe().unwrap();
    let status = Command::new("sh")
        .arg("-c")
        .arg("trap '' XFSZ; ulimit -f 512; exec \"$0\" --exact coalesced_write_limit_child --test-threads=1")
        .arg(&exe)
        .env("FERRO_POISON_COALESCED_PATH", &path)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let report = fs::read_to_string(format!("{}.report", path.display())).unwrap();
    let db = Options::new().open::<Root>(&path).unwrap();
    db.verify().unwrap();
    let snapshots: Vec<_> = COALESCED_BRANCHES
        .iter()
        .map(|name| db.branch(name).unwrap().read())
        .collect();
    for entry in report.split(' ') {
        let (at, key) = entry.split_once(':').unwrap();
        let (at, key) = (at.parse::<usize>().unwrap(), key.parse::<u64>().unwrap());
        assert_eq!(
            snapshots[at].get(&key).as_deref(),
            Some(&text(key, 700)),
            "{} lost acknowledged key {key}",
            COALESCED_BRANCHES[at]
        );
    }
}

#[test]
fn failed_write_poisons_queued_writers_and_keeps_what_was_acknowledged() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("group.ferro");
    let exe = std::env::current_exe().unwrap();
    let status = Command::new("sh")
        .arg("-c")
        .arg("trap '' XFSZ; ulimit -f 512; exec \"$0\" --exact durable_write_limit_child --test-threads=1")
        .arg(&exe)
        .env("FERRO_POISON_GROUP_PATH", &path)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let report = fs::read_to_string(format!("{}.report", path.display())).unwrap();
    let db = Options::new().open::<Root>(&path).unwrap();
    db.verify().unwrap();
    let snapshot = db.read();
    let mut acked = 0;
    for key in report.split(' ').map(|key| key.parse::<u64>().unwrap()) {
        assert_eq!(*snapshot.get(&key).unwrap(), text(key, 700));
        acked += 1;
    }
    assert!(snapshot.len() >= acked);
    assert_eq!(db.version() as usize, snapshot.len());
}

#[test]
fn failed_write_poisons_until_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("poison.ferro");
    let exe = std::env::current_exe().unwrap();
    let status = Command::new("sh")
        .arg("-c")
        .arg("trap '' XFSZ; ulimit -f 512; exec \"$0\" --exact write_limit_child --test-threads=1")
        .arg(&exe)
        .env("FERRO_POISON_PATH", &path)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let report = fs::read_to_string(format!("{}.report", path.display())).unwrap();
    let (version, length) = report.split_once(' ').unwrap();
    let version: u64 = version.parse().unwrap();
    let length: usize = length.parse().unwrap();
    let dirty = fs::metadata(&path).unwrap().len();
    let db = fast().open::<Root>(&path).unwrap();
    assert_eq!(db.version(), version);
    assert_eq!(db.read().len(), length);
    db.verify().unwrap();
    assert!(db.log_bytes() <= dirty);
    for key in 0..length as u64 {
        assert_eq!(*db.read().get(&key).unwrap(), text(key, 700));
    }
    db.update(|root| {
        root.insert(u64::MAX, "after".into());
    })
    .unwrap();
    assert_eq!(db.version(), version + 1);
    drop(db);
    let db = uncached().open::<Root>(&path).unwrap();
    assert_eq!(db.version(), version + 1);
    db.verify().unwrap();
}
