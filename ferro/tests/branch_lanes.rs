mod common;

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use common::{Rng, cases, frame_shapes, padded_links, text};
use ferro::db::{Clock, Hook};
use ferro::store::{Op, Trace};
use ferro::{Db, Durability, Error, Keep, Map, Options, Reclaim};
use parking_lot::Mutex;
use tempfile::TempDir;

type Root = Map<u64, u64>;
type Text = Map<u64, String>;
type Model = BTreeMap<u64, String>;
type Kind = (&'static str, fn() -> Options);

const MANUAL: Reclaim = Reclaim { every_bytes: 0 };
const PATIENCE: Duration = Duration::from_secs(30);
const SPREAD: u64 = 1 << 32;
const SECTOR: usize = 512;
const LOG_START: u64 = 8192;

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn fsync() -> Options {
    Options::new().reclaim(MANUAL)
}

fn buffered() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .reclaim(MANUAL)
}

fn kinds() -> Vec<Kind> {
    vec![
        ("fsync", fsync),
        ("buffered", buffered),
        ("fsync coalesced", || fsync().coalesce(true)),
        ("buffered coalesced", || buffered().coalesce(true)),
    ]
}

fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn put(db: &Db<Root>, key: u64, value: u64) {
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
}

fn value(db: &Db<Root>, key: u64) -> Option<u64> {
    db.read().get(&key).map(|value| *value)
}

fn first_kept(db: &Db<Root>) -> u64 {
    db.log().first().map_or(1, |(version, _)| version)
}

#[derive(Default)]
struct Latch {
    inside: AtomicBool,
    open: AtomicBool,
}

impl Latch {
    fn hold(&self) {
        self.inside.store(true, Ordering::SeqCst);
        wait_until("the latch to open", || self.open.load(Ordering::SeqCst));
    }

    fn release(&self) {
        self.open.store(true, Ordering::SeqCst);
    }
}

fn holding(db: &Db<Root>, key: u64) -> (Arc<Latch>, JoinHandle<ferro::Result<()>>) {
    let latch = Arc::new(Latch::default());
    let handle = {
        let (db, latch) = (db.clone(), latch.clone());
        thread::spawn(move || {
            db.update(|root| {
                latch.hold();
                root.insert(key, key);
            })
        })
    };
    wait_until("the closure to start", || {
        latch.inside.load(Ordering::SeqCst)
    });
    (latch, handle)
}

fn spawn_put(db: &Db<Root>, key: u64, value: u64) -> JoinHandle<()> {
    let db = db.clone();
    thread::spawn(move || put(&db, key, value))
}

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
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        while !flag.woken.swap(false, Ordering::SeqCst) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "a pending write was never woken");
            thread::park_timeout(left);
        }
    }
}

fn poll_once<F: Future>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
    let flag = Arc::new(Flag {
        thread: thread::current(),
        woken: AtomicBool::new(false),
    });
    future
        .as_mut()
        .poll(&mut Context::from_waker(&Waker::from(flag)))
}

fn seeded(options: Options, path: &Path) -> Db<Root> {
    let db: Db<Root> = options.open(path).unwrap();
    put(&db, 0, 0);
    db
}

#[test]
fn closures_on_different_branches_run_at_once() {
    for (kind, options) in kinds() {
        let dir = dir();
        let db = seeded(options(), &dir.path().join("db.ferro"));
        let branches: Vec<_> = (0..4).map(|i| db.fork(&format!("b{i}")).unwrap()).collect();
        let inside = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = branches
            .iter()
            .enumerate()
            .map(|(i, branch)| {
                let (branch, inside) = (branch.clone(), inside.clone());
                thread::spawn(move || {
                    branch.update(|root| {
                        inside.fetch_add(1, Ordering::SeqCst);
                        wait_until("every closure to be running at once", || {
                            inside.load(Ordering::SeqCst) == 4
                        });
                        root.insert(1, i as u64);
                    })
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        for (i, branch) in branches.iter().enumerate() {
            assert_eq!(branch.version(), 2, "{kind}");
            assert_eq!(value(branch, 1), Some(i as u64), "{kind}");
        }
        assert_eq!((db.version(), value(&db, 1)), (1, None), "{kind}");
        db.verify().unwrap();
    }
}

#[test]
fn a_long_closure_holds_up_only_its_own_branch() {
    for (kind, options) in kinds() {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let db = seeded(options(), &path);
        let slow = db.fork("slow").unwrap();
        let quick = db.fork("quick").unwrap();
        let (latch, held) = holding(&slow, 7);
        let behind = spawn_put(&slow, 8, 8);
        for i in 0..20 {
            put(&quick, 1, i);
        }
        put(&db, 2, 2);
        let side = db.fork("side").unwrap();
        put(&side, 3, 3);
        assert_eq!(quick.version(), 21, "{kind}");
        assert!(!held.is_finished(), "{kind}");
        assert!(!behind.is_finished(), "{kind}");
        assert_eq!(slow.version(), 1, "{kind}");
        latch.release();
        held.join().unwrap().unwrap();
        behind.join().unwrap();
        assert_eq!(
            (value(&slow, 7), value(&slow, 8)),
            (Some(7), Some(8)),
            "{kind}"
        );
        assert!(slow.version() >= 2, "{kind}");
        drop((db, slow, quick, side));
        let reopened: Db<Root> = options().open(&path).unwrap();
        assert_eq!(
            value(&reopened.branch("slow").unwrap(), 8),
            Some(8),
            "{kind}"
        );
        assert_eq!(reopened.branch("quick").unwrap().version(), 21, "{kind}");
        reopened.verify().unwrap();
    }
}

#[test]
fn an_awaited_write_waits_only_for_its_own_branch() {
    for (kind, options) in kinds() {
        let dir = dir();
        let db = seeded(options(), &dir.path().join("db.ferro"));
        let slow = db.fork("slow").unwrap();
        let other = db.fork("other").unwrap();
        let (latch, held) = holding(&slow, 7);
        let mut behind = Box::pin(slow.update_async(|root| {
            root.insert(8, 8);
        }));
        assert!(poll_once(&mut behind).is_pending(), "{kind}");
        block_on(other.update_async(|root| {
            root.insert(9, 9);
        }))
        .unwrap();
        block_on(db.update_async(|root| {
            root.insert(10, 10);
        }))
        .unwrap();
        assert_eq!((other.version(), db.version()), (2, 2), "{kind}");
        assert!(!held.is_finished(), "{kind}");
        assert_eq!(value(&slow, 8), None, "{kind}");
        latch.release();
        held.join().unwrap().unwrap();
        block_on(behind).unwrap();
        assert_eq!(
            (value(&slow, 7), value(&slow, 8)),
            (Some(7), Some(8)),
            "{kind}"
        );
        db.verify().unwrap();
    }
}

#[test]
fn a_merge_closure_holds_up_only_its_own_branch() {
    for (kind, options) in kinds() {
        let dir = dir();
        let db = seeded(options(), &dir.path().join("db.ferro"));
        let side = db.fork("side").unwrap();
        let other = db.fork("other").unwrap();
        put(&side, 5, 5);
        let inside = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let merger = {
            let (db, side) = (db.clone(), side.clone());
            let (inside, done) = (inside.clone(), done.clone());
            thread::spawn(move || {
                db.merge_with(&side, |root, base, theirs| {
                    inside.store(true, Ordering::SeqCst);
                    wait_until("commits on other branches", || done.load(Ordering::SeqCst));
                    root.merge(base, theirs, |conflict| conflict.theirs.map(|value| *value));
                    ("merge", ())
                })
            })
        };
        wait_until("the merge closure to start", || {
            inside.load(Ordering::SeqCst)
        });
        put(&other, 6, 6);
        put(&side, 7, 7);
        let late = other.fork("late").unwrap();
        assert!(!merger.is_finished(), "{kind}");
        done.store(true, Ordering::SeqCst);
        assert!(merger.join().unwrap().unwrap().changed, "{kind}");
        assert_eq!((value(&db, 5), value(&db, 7)), (Some(5), None), "{kind}");
        assert_eq!(value(&late, 6), Some(6), "{kind}");
        db.merge_with(&side, |root, base, theirs| {
            root.merge(base, theirs, |conflict| conflict.theirs.map(|value| *value));
            ("again", ())
        })
        .unwrap();
        assert_eq!(value(&db, 7), Some(7), "{kind}");
        db.verify().unwrap();
    }
}

#[test]
fn a_merge_from_a_branch_dropped_while_it_ran_records_nothing() {
    let dir = dir();
    let db = seeded(buffered(), &dir.path().join("db.ferro"));
    let side = db.fork("side").unwrap();
    put(&side, 5, 5);
    let other = db.fork("other").unwrap();
    let inside = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let merger = {
        let (db, side) = (db.clone(), side.clone());
        let (inside, done) = (inside.clone(), done.clone());
        thread::spawn(move || {
            db.merge_with(&side, |root, base, theirs| {
                inside.store(true, Ordering::SeqCst);
                wait_until("the drop", || done.load(Ordering::SeqCst));
                root.merge(base, theirs, |conflict| conflict.theirs.map(|value| *value));
                ("merge", ())
            })
        })
    };
    wait_until("the merge closure to start", || {
        inside.load(Ordering::SeqCst)
    });
    other.drop_branch("side").unwrap();
    done.store(true, Ordering::SeqCst);
    assert!(matches!(merger.join().unwrap(), Err(Error::NoBranch(_))));
    assert_eq!((db.version(), value(&db, 5)), (1, None));
    assert_eq!(db.branches(), vec!["main", "other"]);
    db.verify().unwrap();
}

#[test]
fn fork_and_drop_wait_only_for_the_branch_they_touch() {
    for (kind, options) in kinds() {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let db = seeded(options(), &path);
        let source = db.fork("source").unwrap();
        let doomed = db.fork("doomed").unwrap();
        let (source_latch, on_source) = holding(&source, 7);
        let forker = {
            let source = source.clone();
            thread::spawn(move || source.fork("copy"))
        };
        let (doomed_latch, on_doomed) = holding(&doomed, 8);
        let dropper = {
            let db = db.clone();
            thread::spawn(move || db.drop_branch("doomed"))
        };
        put(&db, 1, 1);
        let other = db.fork("other").unwrap();
        put(&other, 2, 2);
        thread::sleep(Duration::from_millis(20));
        assert!(!forker.is_finished(), "{kind}");
        assert!(!dropper.is_finished(), "{kind}");
        source_latch.release();
        on_source.join().unwrap().unwrap();
        let copy = forker.join().unwrap().unwrap();
        assert_eq!(value(&copy, 7), Some(7), "{kind}");
        doomed_latch.release();
        on_doomed.join().unwrap().unwrap();
        dropper.join().unwrap().unwrap();
        assert!(
            matches!(doomed.update(|_| ()), Err(Error::NoBranch(_))),
            "{kind}"
        );
        assert_eq!(
            db.branches(),
            vec!["copy", "main", "other", "source"],
            "{kind}"
        );
        db.verify().unwrap();
        drop((db, source, doomed, other, copy));
        let reopened: Db<Root> = options().open(&path).unwrap();
        assert_eq!(
            value(&reopened.branch("copy").unwrap(), 7),
            Some(7),
            "{kind}"
        );
        reopened.verify().unwrap();
    }
}

#[test]
fn a_closure_queued_behind_a_drop_of_its_branch_fails_cleanly() {
    for (kind, options) in kinds() {
        let dir = dir();
        let db = seeded(options(), &dir.path().join("db.ferro"));
        let doomed = db.fork("doomed").unwrap();
        let (latch, held) = holding(&doomed, 7);
        let dropper = {
            let db = db.clone();
            thread::spawn(move || db.drop_branch("doomed"))
        };
        thread::sleep(Duration::from_millis(10));
        let ran = Arc::new(AtomicBool::new(false));
        let behind = {
            let (doomed, ran) = (doomed.clone(), ran.clone());
            thread::spawn(move || {
                doomed.update(|root| {
                    ran.store(true, Ordering::SeqCst);
                    root.insert(9, 9);
                })
            })
        };
        thread::sleep(Duration::from_millis(10));
        latch.release();
        held.join().unwrap().unwrap();
        dropper.join().unwrap().unwrap();
        match behind.join().unwrap() {
            Err(Error::NoBranch(_)) => {}
            Ok(()) => assert!(ran.load(Ordering::SeqCst), "{kind}"),
            Err(other) => panic!("{kind}: {other}"),
        }
        assert!(
            matches!(doomed.update(|_| ()), Err(Error::NoBranch(_))),
            "{kind}"
        );
        assert_eq!(db.branches(), vec!["main"], "{kind}");
        db.verify().unwrap();
    }
}

fn recording_steps() -> (Arc<Mutex<Vec<&'static str>>>, Hook) {
    let steps: Arc<Mutex<Vec<&'static str>>> = Arc::default();
    let hook = {
        let steps = steps.clone();
        move |step: &'static str| {
            if step != "arriving" {
                steps.lock().push(step);
            }
        }
    };
    (steps, Hook(Arc::new(hook)))
}

#[test]
fn a_cycle_swaps_only_between_closures() {
    for (kind, options) in kinds() {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let (steps, hook) = recording_steps();
        let db = seeded(options().hook(hook), &path);
        for i in 1..50 {
            put(&db, i, i);
        }
        let slow = db.fork("slow").unwrap();
        let other = db.fork("other").unwrap();
        put(&other, 1, 100);
        let (latch, held) = holding(&slow, 7);
        let cycle = {
            let db = db.clone();
            thread::spawn(move || db.compact(Keep::All))
        };
        wait_until("the copy", || steps.lock().contains(&"synced"));
        thread::sleep(Duration::from_millis(20));
        assert!(
            !steps.lock().contains(&"paused"),
            "{kind}: paused under a closure"
        );
        latch.release();
        held.join().unwrap().unwrap();
        let report = cycle.join().unwrap().unwrap();
        assert!(report.after_bytes > 0, "{kind}");
        assert!(steps.lock().contains(&"swapped"), "{kind}");
        steps.lock().clear();
        let (latch, held) = holding(&other, 8);
        let cycle = {
            let db = db.clone();
            thread::spawn(move || db.compact(Keep::Last(2)))
        };
        thread::sleep(Duration::from_millis(20));
        assert!(steps.lock().is_empty(), "{kind}: pruned under a closure");
        latch.release();
        held.join().unwrap().unwrap();
        let report = cycle.join().unwrap().unwrap();
        assert!(report.commits_dropped > 0, "{kind}");
        assert_eq!(first_kept(&db), db.version() - 1, "{kind}");
        put(&slow, 8, 8);
        put(&other, 9, 9);
        assert_eq!(
            (value(&slow, 7), value(&slow, 8)),
            (Some(7), Some(8)),
            "{kind}"
        );
        assert_eq!(
            (value(&other, 1), value(&other, 8)),
            (Some(100), Some(8)),
            "{kind}"
        );
        db.verify().unwrap();
        drop((db, slow, other));
        let reopened: Db<Root> = options().open(&path).unwrap();
        let slow = reopened.branch("slow").unwrap();
        assert_eq!(value(&slow, 7), Some(7), "{kind}");
        for version in first_kept(&slow)..=slow.version() {
            slow.at(version).unwrap();
        }
        reopened.verify().unwrap();
    }
}

#[test]
fn a_branch_attached_during_a_pause_starts_from_the_new_file() {
    for (kind, options) in kinds() {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        {
            let db = seeded(options(), &path);
            for i in 1..40 {
                put(&db, i, i);
            }
            let late = db.fork("late").unwrap();
            put(&late, 1, 1000);
        }
        let shared: Arc<Mutex<Option<Db<Root>>>> = Arc::default();
        let attacher: Arc<Mutex<Option<JoinHandle<ferro::Result<()>>>>> = Arc::default();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_pause = Arc::new(AtomicBool::new(false));
        let hook = {
            let (shared, attacher) = (shared.clone(), attacher.clone());
            let (ran, ran_in_pause) = (ran.clone(), ran_in_pause.clone());
            move |step: &'static str| {
                if step != "paused" {
                    return;
                }
                let Some(db) = shared.lock().clone() else {
                    return;
                };
                let marker = ran.clone();
                *attacher.lock() = Some(thread::spawn(move || {
                    let late = db.branch("late")?;
                    late.update(|root| {
                        marker.store(true, Ordering::SeqCst);
                        root.insert(2, 2000);
                    })
                }));
                thread::sleep(Duration::from_millis(50));
                ran_in_pause.store(ran.load(Ordering::SeqCst), Ordering::SeqCst);
            }
        };
        let db: Db<Root> = options().hook(Hook(Arc::new(hook))).open(&path).unwrap();
        *shared.lock() = Some(db.clone());
        db.compact(Keep::All).unwrap();
        shared.lock().take();
        let handle = attacher.lock().take().expect("the pause ran the hook");
        handle.join().unwrap().unwrap();
        assert!(ran.load(Ordering::SeqCst), "{kind}");
        assert!(
            !ran_in_pause.load(Ordering::SeqCst),
            "{kind}: a closure ran in the pause"
        );
        let late = db.branch("late").unwrap();
        assert_eq!(
            (value(&late, 1), value(&late, 2)),
            (Some(1000), Some(2000)),
            "{kind}"
        );
        for version in first_kept(&late)..=late.version() {
            late.at(version).unwrap();
        }
        db.verify().unwrap();
        drop((db, late));
        let reopened: Db<Root> = options().open(&path).unwrap();
        let late = reopened.branch("late").unwrap();
        assert_eq!(value(&late, 2), Some(2000), "{kind}");
        for version in first_kept(&late)..=late.version() {
            late.at(version).unwrap();
        }
        reopened.verify().unwrap();
    }
}

fn counter_key(branch: u64) -> u64 {
    branch * SPREAD
}

fn own_key(branch: u64, n: u64) -> u64 {
    branch * SPREAD + n
}

fn count_of(root: &Root, branch: u64) -> u64 {
    root.get(&counter_key(branch)).map_or(0, |value| *value)
}

fn assert_whole(root: &Root, branch: u64, context: &str) -> u64 {
    let count = count_of(root, branch);
    if count > 0 {
        assert_eq!(
            root.get(&own_key(branch, count)).map(|value| *value),
            Some(count),
            "{context}"
        );
    }
    assert!(
        !root.contains_key(&own_key(branch, count + 1)),
        "{context}: a half commit"
    );
    count
}

fn spin(work: Duration) {
    let until = Instant::now() + work;
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

fn stress(kind: &str, options: fn() -> Options, commits: u64, pruning: bool) {
    const WRITERS: u64 = 4;
    let dir = dir();
    let path = dir.path().join("stress.ferro");
    let db: Db<Root> = options().open(&path).unwrap();
    put(&db, 0, 0);
    let sink = db.fork("sink").unwrap();
    let writers: Vec<_> = (1..=WRITERS)
        .map(|i| db.fork(&format!("w{i}")).unwrap())
        .collect();
    let merge_into = |sink: &Db<Root>, from: &Db<Root>| {
        sink.merge_with(from, |root, base, theirs| {
            root.merge(base, theirs, |conflict| {
                panic!("ferro: no conflict expected at {}", conflict.key)
            });
            ("merge", ())
        })
        .unwrap();
    };
    for (index, writer) in writers.iter().enumerate() {
        let branch = index as u64 + 1;
        writer
            .update(|root| {
                root.insert(counter_key(branch), 1);
                root.insert(own_key(branch, 1), 1);
            })
            .unwrap();
        merge_into(&sink, writer);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let watchers: Vec<_> = writers
        .iter()
        .enumerate()
        .map(|(index, writer)| {
            let watcher = writer.watch();
            let stop = stop.clone();
            let kind = kind.to_string();
            thread::spawn(move || {
                let branch = index as u64 + 1;
                let mut last = 0;
                let mut seen = 0;
                while !stop.load(Ordering::SeqCst) {
                    let Ok(snapshot) = watcher.recv_timeout(Duration::from_millis(5)) else {
                        continue;
                    };
                    assert!(
                        snapshot.version() > last,
                        "{kind}: a watcher went backwards"
                    );
                    last = snapshot.version();
                    let count = assert_whole(&snapshot, branch, &kind);
                    assert!(count >= seen, "{kind}: a watcher lost commits");
                    seen = count;
                }
                seen
            })
        })
        .collect();
    let handles: Vec<_> = writers
        .iter()
        .enumerate()
        .map(|(index, writer)| {
            let writer = writer.clone();
            thread::spawn(move || {
                let branch = index as u64 + 1;
                let mut rng = Rng::new(branch);
                for _ in 1..commits {
                    let work = Duration::from_micros(rng.below(40));
                    let write = |root: &mut Root| {
                        let count = count_of(root, branch) + 1;
                        spin(work);
                        root.insert(counter_key(branch), count);
                        root.insert(own_key(branch, count), count);
                    };
                    if branch.is_multiple_of(2) {
                        block_on(writer.update_async(write)).unwrap();
                    } else {
                        writer.update(write).unwrap();
                    }
                }
            })
        })
        .collect();
    let forker = {
        let (db, writers, stop) = (db.clone(), writers.clone(), stop.clone());
        let kind = kind.to_string();
        thread::spawn(move || {
            let mut rng = Rng::new(99);
            let mut forks = 0;
            while !stop.load(Ordering::SeqCst) {
                let index = rng.below(WRITERS) as usize;
                let name = format!("tmp-{forks}");
                let copy = writers[index].fork(&name).unwrap();
                assert_whole(&copy.read(), index as u64 + 1, &kind);
                put(&copy, 7, forks);
                db.drop_branch(&name).unwrap();
                assert!(
                    matches!(copy.update(|_| ()), Err(Error::NoBranch(_))),
                    "{kind}"
                );
                forks += 1;
            }
            forks
        })
    };
    let merger = {
        let (sink, writers, stop) = (sink.clone(), writers.clone(), stop.clone());
        let kind = kind.to_string();
        thread::spawn(move || {
            let mut rng = Rng::new(7);
            let mut merges = 0;
            while !pruning && !stop.load(Ordering::SeqCst) {
                let index = rng.below(WRITERS) as usize;
                merge_into(&sink, &writers[index]);
                let root = sink.read();
                for branch in 1..=WRITERS {
                    assert_whole(&root, branch, &kind);
                }
                merges += 1;
            }
            merges
        })
    };
    let compactor = {
        let (db, stop) = (db.clone(), stop.clone());
        thread::spawn(move || {
            let mut cycles = 0;
            while !stop.load(Ordering::SeqCst) {
                let keep = if pruning && cycles % 2 == 0 {
                    Keep::Last(4)
                } else {
                    Keep::All
                };
                db.compact(keep).unwrap();
                cycles += 1;
                thread::sleep(Duration::from_millis(2));
            }
            cycles
        })
    };
    let reader = {
        let (writers, stop) = (writers.clone(), stop.clone());
        let kind = kind.to_string();
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                for (index, writer) in writers.iter().enumerate() {
                    assert_whole(&writer.read(), index as u64 + 1, &kind);
                }
            }
        })
    };
    for handle in handles {
        handle.join().unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    let forks = forker.join().unwrap();
    let merges = merger.join().unwrap();
    let cycles = compactor.join().unwrap();
    reader.join().unwrap();
    for watcher in watchers {
        watcher.join().unwrap();
    }
    let busy = forks > 0 && (pruning || merges > 0) && cycles > 0;
    assert!(busy, "{kind}: {forks} {merges} {cycles}");
    for (index, writer) in writers.iter().enumerate() {
        let branch = index as u64 + 1;
        assert_eq!(
            assert_whole(&writer.read(), branch, kind),
            commits,
            "{kind}"
        );
        assert_eq!(writer.version(), commits + 1, "{kind}");
        for version in first_kept(writer)..=writer.version() {
            assert_whole(&writer.at(version).unwrap(), branch, kind);
        }
        if !pruning {
            merge_into(&sink, writer);
        }
    }
    if !pruning {
        let root = sink.read();
        for branch in 1..=WRITERS {
            assert_eq!(assert_whole(&root, branch, kind), commits, "{kind}");
        }
    }
    db.verify().unwrap();
    let mut names = vec!["main".to_string(), "sink".to_string()];
    names.extend((1..=WRITERS).map(|i| format!("w{i}")));
    assert_eq!(db.branches(), names, "{kind}");
    drop((db, sink, writers));
    let reopened: Db<Root> = options().open(&path).unwrap();
    assert_eq!(reopened.branches(), names, "{kind}");
    let sink = reopened.branch("sink").unwrap();
    for branch in 1..=WRITERS {
        let writer = reopened.branch(&format!("w{branch}")).unwrap();
        assert_eq!(
            assert_whole(&writer.read(), branch, kind),
            commits,
            "{kind}"
        );
        if !pruning {
            assert_eq!(assert_whole(&sink.read(), branch, kind), commits, "{kind}");
        }
    }
    reopened.verify().unwrap();
}

#[test]
fn branches_stay_whole_beside_forks_drops_merges_and_cycles() {
    for (kind, options) in kinds() {
        let commits = if kind.starts_with("fsync") { 120 } else { 400 };
        stress(kind, options, commits, false);
    }
}

#[test]
fn branches_stay_whole_beside_forks_drops_and_pruning_cycles() {
    for (kind, options) in kinds() {
        let commits = if kind.starts_with("fsync") { 120 } else { 400 };
        stress(kind, options, commits, true);
    }
}

struct Recording {
    base: Vec<u8>,
    ops: Vec<Op>,
    writers: u64,
    commits: u64,
}

fn branch_name(branch: u64) -> String {
    format!("w{branch}")
}

fn model(branch: u64, version: u64) -> Model {
    let mut model = Model::new();
    model.insert(0, "base".to_string());
    for n in 1..version {
        model.insert(n, text(branch * 1000 + n, 40));
    }
    model
}

fn dump(root: &Text) -> Model {
    root.iter()
        .map(|(key, value)| (key, (*value).clone()))
        .collect()
}

fn record(path: &Path, options: fn() -> Options, writers: u64, commits: u64) -> Recording {
    {
        let db: Db<Text> = options().open(path).unwrap();
        db.update(|root| {
            root.insert(0, "base".to_string());
        })
        .unwrap();
        for branch in 0..writers {
            db.fork(&branch_name(branch)).unwrap();
        }
    }
    let base = fs::read(path).unwrap();
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    {
        let db: Db<Text> = options().trace(trace.clone()).open(path).unwrap();
        let mut handles: Vec<_> = (0..writers)
            .map(|branch| {
                let db = db.branch(&branch_name(branch)).unwrap();
                thread::spawn(move || {
                    for n in 1..=commits {
                        let done = db
                            .commit_with(|root| {
                                root.insert(n, text(branch * 1000 + n, 40));
                                ("", ())
                            })
                            .unwrap();
                        assert_eq!(done.version, n + 1);
                        db.trace_op(Op::Acked(branch * SPREAD + done.version));
                    }
                })
            })
            .collect();
        let forker = db.clone();
        handles.push(thread::spawn(move || {
            for round in 0..3 {
                let name = format!("extra-{round}");
                forker.fork(&name).unwrap();
                forker.drop_branch(&name).unwrap();
            }
        }));
        for handle in handles {
            handle.join().unwrap();
        }
    }
    let ops = trace.lock().clone();
    Recording {
        base,
        ops,
        writers,
        commits,
    }
}

#[derive(Clone, Copy, Debug)]
enum Loss {
    Everything,
    Nothing,
    Some,
    Torn,
    SuperblocksOnly,
    FramesOnly,
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
        let stop = ((offset / SECTOR + 1) * SECTOR).min(end);
        if rng.chance(50) {
            image[offset..stop].copy_from_slice(&bytes[offset - start..stop - start]);
        }
        offset = stop;
    }
}

fn is_superblock(op: &Op) -> bool {
    matches!(op, Op::Write(_, at, _) if *at < LOG_START)
}

struct Crash {
    image: Vec<u8>,
    floors: Vec<u64>,
}

fn crash(recording: &Recording, cut: usize, loss: Loss, durable: bool, rng: &mut Rng) -> Crash {
    let mut image = recording.base.clone();
    let mut pending: Vec<&Op> = Vec::new();
    let mut floors = vec![1; recording.writers as usize];
    let mut newest = floors.clone();
    for op in &recording.ops[..cut] {
        match op {
            Op::Write(..) | Op::SetLen(..) => pending.push(op),
            Op::Sync(_) => {
                for done in pending.drain(..) {
                    apply(&mut image, done);
                }
                floors.clone_from(&newest);
            }
            Op::Acked(marker) => {
                let branch = (marker / SPREAD) as usize;
                newest[branch] = newest[branch].max(marker % SPREAD);
                if durable {
                    floors[branch] = floors[branch].max(marker % SPREAD);
                }
            }
            _ => {}
        }
    }
    for op in pending {
        match loss {
            Loss::Everything => {}
            Loss::Nothing => apply(&mut image, op),
            Loss::Some => {
                if rng.chance(50) {
                    apply(&mut image, op);
                }
            }
            Loss::Torn => torn(&mut image, op, rng),
            Loss::SuperblocksOnly if is_superblock(op) => apply(&mut image, op),
            Loss::FramesOnly if !is_superblock(op) => apply(&mut image, op),
            Loss::SuperblocksOnly | Loss::FramesOnly => {}
        }
    }
    Crash { image, floors }
}

fn check_branches(db: &Db<Text>, recording: &Recording, floors: &[u64], context: &str) {
    for branch in 0..recording.writers {
        let handle = db
            .branch(&branch_name(branch))
            .unwrap_or_else(|error| panic!("{context}: branch {branch}: {error}"));
        let version = handle.version();
        assert!(
            version >= floors[branch as usize],
            "{context}: branch {branch} at v{version} lost v{}",
            floors[branch as usize]
        );
        assert!(
            version <= recording.commits + 1,
            "{context}: branch {branch} ran ahead"
        );
        assert_eq!(
            dump(&handle.read()),
            model(branch, version),
            "{context}: branch {branch}"
        );
    }
    let main = db.read();
    assert_eq!(main.version(), 1, "{context}");
    for name in db.branches() {
        if name.starts_with("extra-") {
            assert_eq!(
                dump(&db.branch(&name).unwrap().read()),
                model(0, 1),
                "{context}"
            );
        }
    }
}

fn check(
    path: &Path,
    recording: &Recording,
    state: &Crash,
    options: fn() -> Options,
    context: &str,
) {
    let reader: Db<Text> = Options::new()
        .read_only(true)
        .cache_bytes(1)
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: read-only open: {error}"));
    check_branches(&reader, recording, &state.floors, context);
    drop(reader);
    let db: Db<Text> = options()
        .cache_bytes(1)
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: open: {error}"));
    check_branches(&db, recording, &state.floors, context);
    db.verify()
        .unwrap_or_else(|error| panic!("{context}: verify: {error}"));
    let first = db.branch(&branch_name(0)).unwrap();
    first
        .update(|root| {
            root.insert(u64::MAX, "after".to_string());
        })
        .unwrap_or_else(|error| panic!("{context}: commit: {error}"));
    let after = first.version();
    drop((db, first));
    let again: Db<Text> = options()
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: reopen: {error}"));
    assert_eq!(
        again.branch(&branch_name(0)).unwrap().version(),
        after,
        "{context}"
    );
    again
        .verify()
        .unwrap_or_else(|error| panic!("{context}: verify again: {error}"));
}

fn syncs(recording: &Recording) -> usize {
    recording
        .ops
        .iter()
        .filter(|op| matches!(op, Op::Sync(_)))
        .count()
}

fn replay(recording: &Recording, options: fn() -> Options, durable: bool, seed: u64) -> usize {
    let dir = dir();
    let path = dir.path().join("image.ferro");
    let mut rng = Rng::new(seed);
    let mut images = 0;
    let stride = (recording.ops.len() / 60).max(1);
    let mut cut = 0;
    while cut <= recording.ops.len() {
        for loss in [
            Loss::Everything,
            Loss::Nothing,
            Loss::Some,
            Loss::Torn,
            Loss::SuperblocksOnly,
            Loss::FramesOnly,
        ] {
            let state = crash(recording, cut, loss, durable, &mut rng);
            fs::write(&path, &state.image).unwrap();
            let context = format!("cut {cut} of {} {loss:?}", recording.ops.len());
            check(&path, recording, &state, options, &context);
            images += 1;
        }
        cut += stride;
    }
    images
}

#[test]
fn a_power_cut_keeps_every_acknowledged_commit_on_every_branch() {
    let durable: [Kind; 2] = [
        ("fsync", fsync),
        ("fsync coalesced", || fsync().coalesce(true)),
    ];
    for (seed, (kind, options)) in durable.into_iter().enumerate() {
        let dir = dir();
        let recording = record(&dir.path().join("trace.ferro"), options, 4, 10);
        let syncs = syncs(&recording);
        assert!(syncs > 4, "{kind}: only {syncs} syncs");
        assert!(
            replay(&recording, options, true, seed as u64) > 100,
            "{kind}"
        );
    }
}

#[test]
fn a_power_cut_leaves_buffered_branches_whole() {
    let small: fn() -> Options = || buffered().checkpoint_bytes(2048);
    let coalesced: fn() -> Options = || buffered().checkpoint_bytes(2048).coalesce(true);
    let lossy: [Kind; 2] = [("buffered", small), ("buffered coalesced", coalesced)];
    for (seed, (kind, options)) in lossy.into_iter().enumerate() {
        let dir = dir();
        let recording = [12, 24, 48]
            .into_iter()
            .map(|commits| {
                let path = dir.path().join(format!("trace-{commits}.ferro"));
                record(&path, options, 4, commits)
            })
            .find(|recording| syncs(recording) >= 3)
            .unwrap_or_else(|| panic!("{kind}: under 3 syncs in every recording"));
        assert!(
            replay(&recording, options, false, 10 + seed as u64) > 100,
            "{kind}"
        );
    }
}

struct Held {
    placed: AtomicBool,
    released: AtomicBool,
    landed: AtomicUsize,
}

fn holding_landings() -> (Arc<Held>, Hook) {
    let held = Arc::new(Held {
        placed: AtomicBool::new(false),
        released: AtomicBool::new(false),
        landed: AtomicUsize::new(0),
    });
    let seen = held.clone();
    let hook = Hook(Arc::new(move |step| {
        match (thread::current().name(), step) {
            (Some("held"), "landing") => {
                seen.placed.store(true, Ordering::SeqCst);
                wait_until("the held frame's release", || {
                    seen.released.load(Ordering::SeqCst)
                });
            }
            (Some("ahead"), "landed") => {
                seen.landed.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }));
    (held, hook)
}

fn commit_named(db: &Db<Text>, branch: u64, n: u64, name: &str) -> JoinHandle<()> {
    let db = db.branch(&branch_name(branch)).unwrap();
    thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let done = db
                .commit_with(|root| {
                    root.insert(n, text(branch * 1000 + n, 40));
                    ("", ())
                })
                .unwrap();
            assert_eq!(done.version, n + 1);
            db.trace_op(Op::Acked(branch * SPREAD + done.version));
        })
        .unwrap()
}

fn record_out_of_order(path: &Path, options: fn() -> Options, commits: u64) -> Recording {
    let writers = 3;
    {
        let db: Db<Text> = options().open(path).unwrap();
        db.update(|root| {
            root.insert(0, "base".to_string());
        })
        .unwrap();
        for branch in 0..writers {
            db.fork(&branch_name(branch)).unwrap();
        }
    }
    let base = fs::read(path).unwrap();
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let (held, hook) = holding_landings();
    {
        let db: Db<Text> = options()
            .trace(trace.clone())
            .hook(hook)
            .open(path)
            .unwrap();
        for n in 1..=commits {
            held.placed.store(false, Ordering::SeqCst);
            held.released.store(false, Ordering::SeqCst);
            held.landed.store(0, Ordering::SeqCst);
            let first = commit_named(&db, 0, n, "held");
            wait_until("the held frame to be placed", || {
                held.placed.load(Ordering::SeqCst)
            });
            let later: Vec<_> = (1..writers)
                .map(|branch| commit_named(&db, branch, n, "ahead"))
                .collect();
            wait_until("the later frames to land", || {
                held.landed.load(Ordering::SeqCst) == later.len()
            });
            held.released.store(true, Ordering::SeqCst);
            first.join().unwrap();
            for handle in later {
                handle.join().unwrap();
            }
        }
    }
    let ops = trace.lock().clone();
    Recording {
        base,
        ops,
        writers,
        commits,
    }
}

fn landed_behind(ops: &[Op]) -> usize {
    let mut highest = 0;
    let mut behind = 0;
    for op in ops {
        if let Op::Write(_, at, bytes) = op
            && *at >= LOG_START
            && bytes.starts_with(b"FERROFRM")
        {
            behind += usize::from(*at < highest);
            highest = highest.max(*at);
        }
    }
    behind
}

#[test]
fn a_power_cut_between_frames_that_landed_out_of_order_keeps_a_prefix() {
    let small: fn() -> Options = || buffered().checkpoint_bytes(4096);
    let modes: [Kind; 2] = [("fsync", fsync), ("buffered", small)];
    for (seed, (kind, options)) in modes.into_iter().enumerate() {
        let durable = kind == "fsync";
        let dir = dir();
        let commits = 8;
        let recording = record_out_of_order(&dir.path().join("trace.ferro"), options, commits);
        let behind = landed_behind(&recording.ops);
        assert!(
            behind >= commits as usize,
            "{kind}: only {behind} frames landed behind a later one"
        );
        assert!(
            replay(&recording, options, durable, 30 + seed as u64) > 100,
            "{kind}"
        );
    }
}

fn record_in_turn(path: &Path, options: fn() -> Options, writers: u64, rounds: u64) -> Recording {
    {
        let db: Db<Text> = options().open(path).unwrap();
        db.update(|root| {
            root.insert(0, "base".to_string());
        })
        .unwrap();
        for branch in 0..writers {
            db.fork(&branch_name(branch)).unwrap();
        }
    }
    let base = fs::read(path).unwrap();
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    {
        let db: Db<Text> = options().trace(trace.clone()).open(path).unwrap();
        let branches: Vec<_> = (0..writers)
            .map(|branch| db.branch(&branch_name(branch)).unwrap())
            .collect();
        for n in 1..=rounds {
            for (branch, handle) in (0..writers).zip(&branches) {
                let done = handle
                    .commit_with(|root| {
                        root.insert(n, text(branch * 1000 + n, 40));
                        ("", ())
                    })
                    .unwrap();
                handle.trace_op(Op::Acked(branch * SPREAD + done.version));
            }
            if n % 25 == 0 {
                let name = format!("extra-{n}");
                db.fork(&name).unwrap();
                db.drop_branch(&name).unwrap();
            }
        }
    }
    let ops = trace.lock().clone();
    Recording {
        base,
        ops,
        writers,
        commits: rounds,
    }
}

fn one_prefix(db: &Db<Text>, recording: &Recording, context: &str) {
    let versions: Vec<u64> = (0..recording.writers)
        .map(|branch| db.branch(&branch_name(branch)).unwrap().version())
        .collect();
    let last = versions[versions.len() - 1];
    assert!(
        versions.windows(2).all(|pair| pair[0] >= pair[1]) && versions[0] <= last + 1,
        "{context}: heads {versions:?} name no single prefix of the commits"
    );
}

#[test]
fn a_power_cut_across_branch_table_steps_keeps_one_set_of_heads() {
    let small: fn() -> Options = || buffered().checkpoint_bytes(4096);
    let modes: [Kind; 2] = [("fsync", fsync), ("buffered", small)];
    for (seed, (kind, options)) in modes.into_iter().enumerate() {
        let dir = dir();
        let rounds = 40 * u64::from(cases(1));
        let recording = record_in_turn(&dir.path().join("trace.ferro"), options, 4, rounds);
        let image = dir.path().join("image.ferro");
        let mut rng = Rng::new(60 + seed as u64);
        let stride = (recording.ops.len() / 80).max(1);
        let mut images = 0;
        for cut in (0..=recording.ops.len()).step_by(stride) {
            for loss in [Loss::Everything, Loss::Nothing, Loss::Some, Loss::Torn] {
                let state = crash(&recording, cut, loss, kind == "fsync", &mut rng);
                fs::write(&image, &state.image).unwrap();
                let context = format!("{kind}: cut {cut} of {} {loss:?}", recording.ops.len());
                let reader: Db<Text> = Options::new()
                    .read_only(true)
                    .cache_bytes(1)
                    .open(&image)
                    .unwrap_or_else(|error| panic!("{context}: read-only open: {error}"));
                one_prefix(&reader, &recording, &context);
                drop(reader);
                check(&image, &recording, &state, options, &context);
                images += 1;
            }
        }
        assert!(images > 200, "{kind}: only {images} images");
    }
}

#[test]
fn a_commit_that_reads_a_frame_not_landed_yet_waits_for_it() {
    for (kind, options) in [("fsync", fsync as fn() -> Options), ("buffered", buffered)] {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let (held, hook) = holding_landings();
        let db = seeded(options().cache_bytes(1).hook(hook), &path);
        let side = db.fork("side").unwrap();
        let merging = db.fork("merged").unwrap();
        let first = {
            let side = side.clone();
            thread::Builder::new()
                .name("held".to_string())
                .spawn(move || put(&side, 1, 1))
                .unwrap()
        };
        wait_until("the held frame to be placed", || {
            held.placed.load(Ordering::SeqCst)
        });
        let second = spawn_put(&db, 2, 2);
        let merged = {
            let (db, side) = (merging, side.clone());
            thread::spawn(move || {
                db.merge_with(&side, |root, base, theirs| {
                    root.merge(base, theirs, |conflict| conflict.theirs.map(|value| *value));
                    ("merge", ())
                })
                .unwrap();
                db
            })
        };
        thread::sleep(Duration::from_millis(50));
        assert!(
            !second.is_finished(),
            "{kind}: a commit finished before the frame ahead of it landed"
        );
        held.released.store(true, Ordering::SeqCst);
        first.join().unwrap();
        second.join().unwrap();
        let merged = merged.join().unwrap();
        assert_eq!(value(&side, 1), Some(1), "{kind}");
        assert_eq!(value(&db, 2), Some(2), "{kind}");
        assert_eq!(value(&merged, 0), Some(0), "{kind}");
        db.verify().unwrap();
        drop((db, side, merged));
        let again: Db<Root> = options().cache_bytes(1).open(&path).unwrap();
        assert_eq!(value(&again, 2), Some(2), "{kind}");
        assert_eq!(value(&again.branch("side").unwrap(), 1), Some(1), "{kind}");
        again.verify().unwrap();
    }
}

#[test]
fn every_acknowledged_commit_counts_once_across_branches() {
    let dir = dir();
    let db = seeded(buffered(), &dir.path().join("db.ferro"));
    let branches: Vec<_> = (0..6).map(|i| db.fork(&format!("b{i}")).unwrap()).collect();
    let total = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = branches
        .iter()
        .flat_map(|branch| (0..3).map(move |_| branch.clone()))
        .map(|branch| {
            let total = total.clone();
            thread::spawn(move || {
                for _ in 0..200 {
                    branch
                        .update(|root| {
                            let next = root.get(&0).map_or(0, |value| *value) + 1;
                            root.insert(0, next);
                        })
                        .unwrap();
                    total.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(total.load(Ordering::SeqCst), 3600);
    for branch in &branches {
        assert_eq!(value(branch, 0), Some(600));
        assert_eq!(branch.version(), 601);
    }
    db.verify().unwrap();
}

type Mixed = (Map<u64, String>, Map<u64, Map<u64, u64>>);

const SEED_ROWS: u64 = 6_000;

struct Edits {
    rng: Rng,
    half: u64,
    tag: &'static str,
    round: u64,
}

impl Edits {
    fn new(half: u64, tag: &'static str) -> Edits {
        Edits {
            rng: Rng::new(half + 1),
            half,
            tag,
            round: 0,
        }
    }

    fn row(&mut self) -> u64 {
        self.half * SEED_ROWS / 2 + self.rng.below(SEED_ROWS / 2)
    }

    fn apply(&mut self, root: &mut Mixed) {
        self.round += 1;
        let inserts = if self.round % 10 == 7 { 3_000 } else { 24 };
        for _ in 0..inserts {
            let key = 4 * self.row() + 1;
            let value = match self.rng.chance(15) {
                true => format!("{}-{}", self.tag, text(self.rng.next(), 600)),
                false => format!("{}-{}", self.tag, self.rng.next()),
            };
            root.0.insert(key, value);
        }
        let shared = 4 * self.row() + 2;
        if self.round.is_multiple_of(2) {
            root.0.insert(shared, text(self.round, 650));
        }
        for _ in 0..3 {
            let key = 4 * self.row();
            root.0.remove(&key);
        }
        for _ in 0..6 {
            let outer = self.half * 500 + self.rng.below(500);
            let mut inner = root
                .1
                .get(&outer)
                .map(|inner| (*inner).clone())
                .unwrap_or_default();
            inner.insert(100 + self.rng.below(1 << 20), self.rng.next());
            if self.rng.chance(30) {
                inner.remove(&self.rng.below(7));
            }
            root.1.insert(outer, inner);
        }
    }
}

fn mixed_options(options: fn() -> Options) -> Options {
    options()
        .cache_bytes(1 << 30)
        .clock(Clock(Arc::new(|| 1_700_000_000_000)))
}

fn mixed_seed(db: &Db<Mixed>) -> Db<Mixed> {
    db.update(|root| {
        for row in 0..SEED_ROWS {
            let value = match row % 13 {
                0 => text(row, 700),
                _ => format!("seed-{row}"),
            };
            root.0.insert(4 * row, value);
        }
        for outer in 0..1_000u64 {
            root.1.insert(
                outer,
                (0..outer % 7).map(|key| (key, outer * key)).collect(),
            );
        }
    })
    .unwrap();
    db.fork("other").unwrap()
}

type Entries = (Vec<(u64, String)>, Vec<(u64, Vec<(u64, u64)>)>);

type Other = Arc<Mutex<Option<(Db<Mixed>, Edits)>>>;

fn entries(db: &Db<Mixed>) -> Entries {
    let root = db.read();
    let plain = root
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
    (plain, nested)
}

#[test]
fn a_frame_another_branch_moves_is_the_frame_written_in_its_place() {
    let rounds = cases(30) as u64;
    for (kind, options) in kinds() {
        let dir = dir();
        let moved_path = dir.path().join("moved.ferro");
        let placed_path = dir.path().join("placed.ferro");
        let armed = Arc::new(AtomicBool::new(false));
        let other: Other = Arc::default();
        let hook = {
            let (armed, other) = (armed.clone(), other.clone());
            move |step: &'static str| {
                if step != "drafted" || !armed.swap(false, Ordering::SeqCst) {
                    return;
                }
                let mut other = other.lock();
                let (db, edits) = other.as_mut().expect("the other branch is ready");
                thread::scope(|scope| {
                    let committed = scope.spawn(|| db.update(|root| edits.apply(root)));
                    committed.join().unwrap().unwrap();
                });
            }
        };
        let moved: Db<Mixed> = mixed_options(options)
            .hook(Hook(Arc::new(hook)))
            .open(&moved_path)
            .unwrap();
        *other.lock() = Some((mixed_seed(&moved), Edits::new(1, "other")));
        let mut main = Edits::new(0, "main");
        for round in 0..rounds {
            armed.store(true, Ordering::SeqCst);
            moved.update(|root| main.apply(root)).unwrap();
            assert!(
                !armed.load(Ordering::SeqCst),
                "{kind}: round {round} moved nothing"
            );
        }
        let placed: Db<Mixed> = mixed_options(options).open(&placed_path).unwrap();
        let placed_other = mixed_seed(&placed);
        let (mut main, mut second) = (Edits::new(0, "main"), Edits::new(1, "other"));
        for _ in 0..rounds {
            placed_other.update(|root| second.apply(root)).unwrap();
            placed.update(|root| main.apply(root)).unwrap();
        }
        let moved_other = other.lock().take().expect("the other branch is held").0;
        assert!(
            entries(&moved) == entries(&placed),
            "{kind}: main reads differently"
        );
        assert!(
            entries(&moved_other) == entries(&placed_other),
            "{kind}: other reads differently"
        );
        moved.verify().unwrap();
        drop((moved, moved_other, placed, placed_other));
        let (moved_image, placed_image) = (
            fs::read(&moved_path).unwrap(),
            fs::read(&placed_path).unwrap(),
        );
        assert!(
            frame_shapes(&moved_image) == frame_shapes(&placed_image),
            "{kind}: a moved frame holds different records or links"
        );
        assert!(
            frame_shapes(&moved_image).len() > 2 * rounds as usize,
            "{kind}: too few frames read"
        );
        assert!(
            padded_links(&moved_image) > 0 && padded_links(&placed_image) == 0,
            "{kind}: padded {} links moved and {} placed",
            padded_links(&moved_image),
            padded_links(&placed_image)
        );
        let reopened: Db<Mixed> = options().cache_bytes(1).open(&moved_path).unwrap();
        let placed: Db<Mixed> = options().cache_bytes(1).open(&placed_path).unwrap();
        assert!(
            entries(&reopened) == entries(&placed),
            "{kind}: reopened reads differently"
        );
        reopened.verify().unwrap();
    }
}

#[test]
fn drafts_moved_over_several_frames_of_any_size_read_verify_and_compact_alike() {
    let rounds = cases(20) as u64;
    for cache in [1 << 30, 1] {
        let dir = dir();
        let moved_path = dir.path().join("moved.ferro");
        let placed_path = dir.path().join("placed.ferro");
        let armed = Arc::new(AtomicBool::new(false));
        let other: Other = Arc::default();
        let frames = Arc::new(Mutex::new((Rng::new(cache + 3), Vec::new())));
        let hook = {
            let (armed, other, frames) = (armed.clone(), other.clone(), frames.clone());
            move |step: &'static str| {
                if step != "drafted" || !armed.swap(false, Ordering::SeqCst) {
                    return;
                }
                let mut frames = frames.lock();
                let count = 1 + frames.0.below(3);
                frames.1.push(count);
                let mut other = other.lock();
                let (db, edits) = other.as_mut().expect("the other branch is ready");
                thread::scope(|scope| {
                    scope
                        .spawn(|| {
                            for _ in 0..count {
                                db.update(|root| edits.apply(root)).unwrap();
                            }
                        })
                        .join()
                        .unwrap();
                });
            }
        };
        let moved: Db<Mixed> = mixed_options(buffered)
            .cache_bytes(cache)
            .hook(Hook(Arc::new(hook)))
            .open(&moved_path)
            .unwrap();
        *other.lock() = Some((mixed_seed(&moved), Edits::new(1, "other")));
        let mut main = Edits::new(0, "main");
        for round in 0..rounds {
            armed.store(true, Ordering::SeqCst);
            moved.update(|root| main.apply(root)).unwrap();
            assert!(
                !armed.load(Ordering::SeqCst),
                "cache {cache}: round {round} moved nothing"
            );
        }
        let placed: Db<Mixed> = mixed_options(buffered).open(&placed_path).unwrap();
        let placed_other = mixed_seed(&placed);
        let (mut main, mut second) = (Edits::new(0, "main"), Edits::new(1, "other"));
        for count in std::mem::take(&mut frames.lock().1) {
            for _ in 0..count {
                placed_other.update(|root| second.apply(root)).unwrap();
            }
            placed.update(|root| main.apply(root)).unwrap();
        }
        let moved_other = other.lock().take().expect("the other branch is held").0;
        assert!(
            entries(&moved) == entries(&placed),
            "cache {cache}: main reads differently"
        );
        assert!(
            entries(&moved_other) == entries(&placed_other),
            "cache {cache}: other reads differently"
        );
        moved.verify().unwrap();
        drop((moved, moved_other));
        assert!(
            padded_links(&fs::read(&moved_path).unwrap()) > 0,
            "cache {cache}: no draft moved in place"
        );
        let reopened: Db<Mixed> = buffered().cache_bytes(1).open(&moved_path).unwrap();
        reopened.verify().unwrap();
        reopened.compact(Keep::All).unwrap();
        reopened.verify().unwrap();
        assert!(
            entries(&reopened) == entries(&placed),
            "cache {cache}: main reads differently once compacted"
        );
        assert!(
            entries(&reopened.branch("other").unwrap()) == entries(&placed_other),
            "cache {cache}: other reads differently once compacted"
        );
    }
}

#[test]
fn own_branches_drafting_at_once_read_as_if_written_in_turn() {
    let lanes = 8;
    let rounds = cases(12) as u64;
    let dir = dir();
    let path = dir.path().join("lanes.ferro");
    let db: Db<Mixed> = mixed_options(buffered)
        .cache_bytes(1 << 16)
        .open(&path)
        .unwrap();
    let placed: Db<Mixed> = mixed_options(buffered)
        .open(dir.path().join("placed.ferro"))
        .unwrap();
    let forks = |db: &Db<Mixed>| -> Vec<Db<Mixed>> {
        mixed_seed(db);
        (0..lanes)
            .map(|lane| db.fork(&format!("lane-{lane}")).unwrap())
            .collect()
    };
    let branches = forks(&db);
    thread::scope(|scope| {
        for (lane, branch) in branches.iter().enumerate() {
            scope.spawn(move || {
                let mut edits = Edits::new(lane as u64, "lane");
                for _ in 0..rounds {
                    branch.update(|root| edits.apply(root)).unwrap();
                }
            });
        }
    });
    for (lane, branch) in forks(&placed).iter().enumerate() {
        let mut edits = Edits::new(lane as u64, "lane");
        for _ in 0..rounds {
            branch.update(|root| edits.apply(root)).unwrap();
        }
    }
    db.verify().unwrap();
    let alike = |db: &Db<Mixed>, context: &str| {
        for lane in 0..lanes {
            let name = format!("lane-{lane}");
            assert!(
                entries(&db.branch(&name).unwrap()) == entries(&placed.branch(&name).unwrap()),
                "{context}: {name} reads differently"
            );
        }
    };
    alike(&db, "open");
    drop((branches, db));
    let reopened: Db<Mixed> = buffered().cache_bytes(1).open(&path).unwrap();
    reopened.verify().unwrap();
    alike(&reopened, "reopened");
    reopened.compact(Keep::All).unwrap();
    reopened.verify().unwrap();
    alike(&reopened, "compacted");
}
