use std::cell::RefCell;
use std::fmt::Debug;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle, LocalKey, Thread};
use std::time::{Duration, Instant};

use ferro::codec::{Decoder, Encoder};
use ferro::db::Hook;
use ferro::{Data, Db, Durability, Error, Keep, Map, Options, Reclaim, Snapshot};
use parking_lot::Mutex;
use tempfile::TempDir;

type Root = Map<u64, u64>;
type Kind = (&'static str, fn() -> Options);
type Outcome = Arc<Mutex<Option<ferro::Result<()>>>>;
type Results = Vec<(&'static str, ferro::Result<()>)>;

const MANUAL: Reclaim = Reclaim { every_bytes: 0 };
const PATIENCE: Duration = Duration::from_secs(20);

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn plain() -> Vec<Kind> {
    vec![
        ("fsync", || Options::new().reclaim(MANUAL)),
        ("buffered", || {
            Options::new()
                .durability(Durability::Buffered)
                .reclaim(MANUAL)
        }),
    ]
}

fn coalesced() -> Vec<Kind> {
    vec![
        ("fsync coalesced", || {
            Options::new().reclaim(MANUAL).coalesce(true)
        }),
        ("buffered coalesced", || {
            Options::new()
                .durability(Durability::Buffered)
                .reclaim(MANUAL)
                .coalesce(true)
        }),
    ]
}

fn kinds() -> Vec<Kind> {
    plain().into_iter().chain(coalesced()).collect()
}

fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn joined<R>(what: &str, handle: JoinHandle<R>) -> R {
    let deadline = Instant::now() + PATIENCE;
    while !handle.is_finished() {
        assert!(Instant::now() < deadline, "{what} deadlocked");
        thread::sleep(Duration::from_millis(1));
    }
    handle.join().unwrap()
}

fn finishes<R: Send + 'static>(what: &str, run: impl FnOnce() -> R + Send + 'static) -> R {
    joined(what, thread::spawn(run))
}

fn reentrant<R: Debug>(what: &str, result: &ferro::Result<R>) {
    assert!(
        matches!(result, Err(Error::Reentrant)),
        "{what}: {result:?}"
    );
}

fn set(root: &mut Root, key: u64) {
    root.insert(key, key);
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

fn merge(ours: &mut Root, _: &Snapshot<Root>, theirs: &Snapshot<Root>) -> (&'static str, ()) {
    for key in [5, 6] {
        if let Some(value) = theirs.get(&key) {
            ours.insert(key, *value);
        }
    }
    ("merge", ())
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
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        while !flag.woken.swap(false, Ordering::SeqCst) {
            thread::park();
        }
    }
}

struct Files {
    main: Db<Root>,
    a: Db<Root>,
    b: Db<Root>,
    c: Db<Root>,
    other: Db<Root>,
    _dir: TempDir,
}

fn files(options: fn() -> Options) -> Arc<Files> {
    let dir = dir();
    let main: Db<Root> = options().open(dir.path().join("one.ferro")).unwrap();
    put(&main, 0, 0);
    let a = main.fork("a").unwrap();
    let b = main.fork("b").unwrap();
    let c = main.fork("c").unwrap();
    let other: Db<Root> = options().open(dir.path().join("two.ferro")).unwrap();
    put(&other, 0, 0);
    Arc::new(Files {
        main,
        a,
        b,
        c,
        other,
        _dir: dir,
    })
}

fn inside_b<R: Send + 'static>(
    what: &str,
    files: &Arc<Files>,
    nested: impl FnOnce(&Files) -> R + Send + 'static,
) -> R {
    let files = files.clone();
    finishes(what, move || {
        let mut out = None;
        files
            .b
            .update(|root| {
                out = Some(nested(&files));
                root.insert(1, 1);
            })
            .unwrap();
        out.unwrap()
    })
}

fn check(kind: &str, results: &[(&'static str, ferro::Result<()>)], allowed: &[&str]) {
    for (what, result) in results {
        match allowed.contains(what) {
            true => assert!(result.is_ok(), "{kind}: {what}: {result:?}"),
            false => reentrant(&format!("{kind}: {what}"), result),
        }
    }
}

fn intact(kind: &str, files: &Files) {
    assert_eq!(
        value(&files.b, 1),
        Some(1),
        "{kind}: the outer closure landed"
    );
    put(&files.b, 2, 2);
    put(&files.c, 2, 2);
    files.main.verify().unwrap();
    files.other.verify().unwrap();
}

#[test]
fn a_closure_cannot_commit_on_its_own_branch() {
    for (kind, options) in kinds() {
        let files = files(options);
        let results = inside_b(kind, &files, |f| {
            let again = f.main.branch("b").unwrap();
            vec![
                ("update", f.b.update(|root| set(root, 9))),
                ("commit", f.b.commit("x", |_| ())),
                ("commit_with", f.b.commit_with(|_| ("x", ())).map(drop)),
                (
                    "commit_stamped",
                    f.b.commit_stamped(|_, _| ("x", ())).map(drop),
                ),
                ("try_update", f.b.try_update(|_| Ok::<_, Error>(()))),
                ("try_commit", f.b.try_commit("x", |_| Ok::<_, Error>(()))),
                (
                    "try_commit_with",
                    f.b.try_commit_with(|_| Ok::<_, Error>(("x", ()))).map(drop),
                ),
                (
                    "try_commit_stamped",
                    f.b.try_commit_stamped(|_, _| Ok::<_, Error>(("x", ())))
                        .map(drop),
                ),
                ("another handle", again.update(|_| ())),
            ]
        });
        check(kind, &results, &[]);
        assert_eq!(value(&files.b, 9), None, "{kind}");
        intact(kind, &files);
    }
}

#[test]
fn a_closure_cannot_await_a_write_on_its_own_branch() {
    for (kind, options) in kinds() {
        let files = files(options);
        let results = inside_b(kind, &files, |f| {
            vec![
                (
                    "update_async",
                    block_on(f.b.update_async(|root| set(root, 9))),
                ),
                ("commit_async", block_on(f.b.commit_async("x", |_| ()))),
                (
                    "commit_with_async",
                    block_on(f.b.commit_with_async(|_| ("x", ()))).map(drop),
                ),
                (
                    "try_update_async",
                    block_on(f.b.try_update_async(|_| Ok::<_, Error>(()))),
                ),
                (
                    "try_commit_async",
                    block_on(f.b.try_commit_async("x", |_| Ok::<_, Error>(()))),
                ),
                (
                    "try_commit_with_async",
                    block_on(f.b.try_commit_with_async(|_| Ok::<_, Error>(("x", ())))).map(drop),
                ),
            ]
        });
        check(kind, &results, &[]);
        assert_eq!(value(&files.b, 9), None, "{kind}");
        intact(kind, &files);
    }
}

#[test]
fn a_closure_cannot_restore_or_merge_into_its_own_branch() {
    for (kind, options) in kinds() {
        let files = files(options);
        put(&files.c, 5, 5);
        let results = inside_b(kind, &files, |f| {
            let snapshot = f.b.read();
            vec![
                ("restore", f.b.restore(&snapshot).map(drop)),
                ("restore_with", f.b.restore_with(&snapshot, "x").map(drop)),
                ("merge_with", f.b.merge_with(&f.c, merge).map(drop)),
                (
                    "try_merge_with",
                    f.b.try_merge_with(&f.c, |_, _, _| Ok::<_, Error>(("x", ())))
                        .map(drop),
                ),
            ]
        });
        check(kind, &results, &[]);
        assert_eq!(value(&files.b, 5), None, "{kind}");
        intact(kind, &files);
    }
}

#[test]
fn a_closure_cannot_fork_or_drop_its_own_branch() {
    for (kind, options) in kinds() {
        let files = files(options);
        let results = inside_b(kind, &files, |f| {
            vec![
                ("fork", f.b.fork("x").map(drop)),
                ("drop_branch from main", f.main.drop_branch("b")),
                ("drop_branch from a", f.a.drop_branch("b")),
            ]
        });
        check(kind, &results, &[]);
        assert_eq!(files.main.branches(), vec!["a", "b", "c", "main"], "{kind}");
        intact(kind, &files);
    }
}

#[test]
fn a_closure_cannot_compact_pull_or_receive_into_its_file() {
    for (kind, options) in kinds() {
        let files = files(options);
        put(&files.other, 5, 5);
        let bundle = files.other.bundle(&files.c.have().unwrap()).unwrap();
        let results = inside_b(kind, &files, move |f| {
            vec![
                ("compact", f.b.compact(Keep::All).map(drop)),
                (
                    "compact from another branch",
                    f.c.compact(Keep::Last(1)).map(drop),
                ),
                ("pull", f.c.pull(&f.other, "remote", merge).map(drop)),
                ("push", f.other.push(&f.c, "remote", merge).map(drop)),
                ("receive", f.c.receive("remote", &bundle, merge).map(drop)),
            ]
        });
        check(kind, &results, &[]);
        assert_eq!(value(&files.c, 5), None, "{kind}");
        assert_eq!(files.main.branches(), vec!["a", "b", "c", "main"], "{kind}");
        intact(kind, &files);
    }
}

#[test]
fn a_merge_or_awaited_closure_cannot_write_its_own_branch() {
    for (kind, options) in kinds() {
        let files = files(options);
        put(&files.c, 5, 5);
        let from_merge = {
            let files = files.clone();
            finishes(kind, move || {
                let mut out = None;
                files
                    .b
                    .merge_with(&files.c, |ours, base, theirs| {
                        out = Some(files.b.update(|_| ()));
                        merge(ours, base, theirs)
                    })
                    .unwrap();
                out.unwrap()
            })
        };
        reentrant(&format!("{kind}: from a merge"), &from_merge);
        let from_async = {
            let files = files.clone();
            finishes(kind, move || {
                block_on(files.b.update_async(|root| {
                    root.insert(1, 1);
                    files.b.update(|_| ())
                }))
                .unwrap()
            })
        };
        reentrant(&format!("{kind}: from an awaited write"), &from_async);
        assert_eq!(value(&files.b, 5), Some(5), "{kind}");
        intact(kind, &files);
    }
}

#[test]
fn a_closure_cannot_write_a_branch_that_sorts_before_its_own() {
    for (kind, options) in kinds() {
        let files = files(options);
        let results = {
            let files = files.clone();
            finishes(kind, move || {
                let mut out = None;
                files
                    .c
                    .update(|root| {
                        out = Some(vec![
                            ("update", files.b.update(|_| ())),
                            ("update_async", block_on(files.a.update_async(|_| ()))),
                            ("restore", files.b.restore(&files.b.read()).map(drop)),
                            (
                                "merge into a",
                                files.a.merge_with(&files.b, merge).map(drop),
                            ),
                            ("fork from b", files.b.fork("y").map(drop)),
                            ("drop_branch", files.main.drop_branch("a")),
                        ]);
                        root.insert(1, 1);
                    })
                    .unwrap();
                out.unwrap()
            })
        };
        check(kind, &results, &[]);
        assert_eq!(value(&files.c, 1), Some(1), "{kind}");
        assert_eq!(files.main.branches(), vec!["a", "b", "c", "main"], "{kind}");
        files.main.verify().unwrap();
    }
}

#[test]
fn writing_down_the_lane_order_cannot_deadlock_against_a_cycle() {
    for (kind, options) in kinds() {
        let dir = dir();
        let synced = Arc::new(AtomicBool::new(false));
        let hook = {
            let synced = synced.clone();
            move |step: &'static str| {
                if step == "synced" {
                    synced.store(true, Ordering::SeqCst);
                }
            }
        };
        let main: Db<Root> = options()
            .hook(Hook(Arc::new(hook)))
            .open(dir.path().join("cycle.ferro"))
            .unwrap();
        for i in 0..50 {
            put(&main, i, i);
        }
        let b = main.fork("b").unwrap();
        let c = main.fork("c").unwrap();
        let inside = Arc::new(AtomicBool::new(false));
        let nested = {
            let (b, c, inside, synced) = (b.clone(), c.clone(), inside.clone(), synced.clone());
            thread::spawn(move || {
                let mut out = None;
                c.update(|root| {
                    inside.store(true, Ordering::SeqCst);
                    wait_until("the copy", || synced.load(Ordering::SeqCst));
                    thread::sleep(Duration::from_millis(100));
                    out = Some(b.update(|root| set(root, 70)));
                    root.insert(80, 80);
                })
                .unwrap();
                out.unwrap()
            })
        };
        wait_until("the closure", || inside.load(Ordering::SeqCst));
        let cycle = {
            let main = main.clone();
            thread::spawn(move || main.compact(Keep::All))
        };
        let result = joined(kind, nested);
        reentrant(kind, &result);
        joined(kind, cycle).unwrap();
        assert_eq!((value(&b, 70), value(&c, 80)), (None, Some(80)), "{kind}");
        put(&b, 70, 70);
        main.verify().unwrap();
    }
}

#[test]
fn a_closure_may_write_branches_that_sort_after_its_own() {
    for (kind, options) in kinds() {
        let files = files(options);
        put(&files.a, 5, 5);
        let results = inside_b(kind, &files, |f| {
            vec![
                ("update", f.c.update(|root| set(root, 3))),
                ("main", f.main.update(|root| set(root, 3))),
                ("commit_with", f.c.commit_with(|_| ("x", ())).map(drop)),
                (
                    "update_async",
                    block_on(f.c.update_async(|root| set(root, 4))),
                ),
                ("merge into c", f.c.merge_with(&f.a, merge).map(drop)),
                ("restore", f.main.restore(&f.main.read()).map(drop)),
                ("fork from c", f.c.fork("d").map(drop)),
                ("drop_branch", f.main.drop_branch("d")),
                ("branch", f.main.branch("c").map(drop)),
            ]
        });
        if kind.contains("coalesced") {
            check(kind, &results, &["branch"]);
        } else {
            let all: Vec<&str> = results.iter().map(|(what, _)| *what).collect();
            check(kind, &results, &all);
            assert_eq!(
                (value(&files.c, 3), value(&files.c, 4)),
                (Some(3), Some(4)),
                "{kind}"
            );
            assert_eq!(
                (value(&files.c, 5), value(&files.main, 3)),
                (Some(5), Some(3)),
                "{kind}"
            );
        }
        assert_eq!(files.main.branches(), vec!["a", "b", "c", "main"], "{kind}");
        intact(kind, &files);
    }
}

#[test]
fn writes_up_the_lane_order_survive_cycles_and_each_other() {
    for (kind, options) in plain() {
        let files = files(options);
        let rounds = if kind == "fsync" { 40 } else { 200 };
        let stop = Arc::new(AtomicBool::new(false));
        let cycles = {
            let (files, stop) = (files.clone(), stop.clone());
            thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::SeqCst) {
                    let keep = if n.is_multiple_of(2) {
                        Keep::All
                    } else {
                        Keep::Last(3)
                    };
                    files.main.compact(keep).unwrap();
                    n += 1;
                }
                n
            })
        };
        let spawn = |run: fn(&Files, u64)| {
            let files = files.clone();
            thread::spawn(move || (0..rounds).for_each(|i| run(&files, i)))
        };
        let writers = vec![
            spawn(|f, i| {
                f.b.update(|root| {
                    f.c.update(|root| set(root, i)).unwrap();
                    root.insert(i, i);
                })
                .unwrap();
            }),
            spawn(|f, i| {
                f.a.update(|root| {
                    f.b.update(|root| set(root, 1000 + i)).unwrap();
                    block_on(f.c.update_async(|root| set(root, 1000 + i))).unwrap();
                    root.insert(i, i);
                })
                .unwrap();
            }),
            spawn(|f, i| put(&f.c, 2000 + i, i)),
            spawn(|f, i| put(&f.a, 2000 + i, i)),
        ];
        for writer in writers {
            joined(kind, writer);
        }
        stop.store(true, Ordering::SeqCst);
        assert!(joined(kind, cycles) > 0, "{kind}");
        let last = rounds - 1;
        assert_eq!(value(&files.c, last), Some(last), "{kind}");
        assert_eq!(value(&files.c, 1000 + last), Some(1000 + last), "{kind}");
        assert_eq!(value(&files.b, 1000 + last), Some(1000 + last), "{kind}");
        files.main.verify().unwrap();
    }
}

#[test]
fn a_closure_may_write_another_file() {
    for (kind, options) in kinds() {
        let files = files(options);
        put(&files.c, 6, 6);
        let results = inside_b(kind, &files, |f| {
            vec![
                ("update", f.other.update(|root| set(root, 3))),
                (
                    "update_async",
                    block_on(f.other.update_async(|root| set(root, 4))),
                ),
                ("fork", f.other.fork("side").map(drop)),
                ("drop_branch", f.other.drop_branch("side")),
                ("compact", f.other.compact(Keep::All).map(drop)),
                (
                    "pull from this file",
                    f.other.pull(&f.c, "remote", merge).map(drop),
                ),
                (
                    "back into this file",
                    f.other
                        .update(|_| f.main.update(|_| ()))
                        .and_then(|inner| inner),
                ),
            ]
        });
        let allowed = [
            "update",
            "update_async",
            "fork",
            "drop_branch",
            "compact",
            "pull from this file",
        ];
        check(kind, &results, &allowed);
        assert_eq!(
            (value(&files.other, 3), value(&files.other, 4)),
            (Some(3), Some(4)),
            "{kind}"
        );
        assert_eq!(value(&files.other, 6), Some(6), "{kind}");
        intact(kind, &files);
    }
}

#[test]
fn closures_writing_into_each_others_files_cannot_deadlock() {
    for (kind, options) in kinds() {
        let files = files(options);
        let barrier = Arc::new(Barrier::new(2));
        let across = |from: fn(&Files) -> &Db<Root>, to: fn(&Files) -> &Db<Root>| {
            let (files, barrier) = (files.clone(), barrier.clone());
            thread::spawn(move || {
                let mut out = None;
                from(&files)
                    .update(|root| {
                        barrier.wait();
                        out = Some(to(&files).update(|root| set(root, 4)));
                        root.insert(3, 3);
                    })
                    .unwrap();
                out.unwrap()
            })
        };
        let forth = across(|f| &f.b, |f| &f.other);
        let back = across(|f| &f.other, |f| &f.b);
        let results = [joined(kind, forth), joined(kind, back)];
        let refused = results
            .iter()
            .filter(|result| matches!(result, Err(Error::Reentrant)));
        assert_eq!(refused.count(), 1, "{kind}: {results:?}");
        assert!(results.iter().any(Result::is_ok), "{kind}: {results:?}");
        assert_eq!(
            (value(&files.b, 3), value(&files.other, 3)),
            (Some(3), Some(3)),
            "{kind}"
        );
        files.main.verify().unwrap();
        files.other.verify().unwrap();
    }
}

#[test]
fn a_watcher_test_run_inside_a_closure_cannot_write_its_branch() {
    for (kind, options) in kinds() {
        let files = files(options);
        let outcome: Outcome = Arc::default();
        let watcher = {
            let (b, outcome) = (files.b.clone(), outcome.clone());
            files.b.watch_when(move |_| {
                let mut slot = outcome.lock();
                if slot.is_none() {
                    *slot = Some(b.update(|root| set(root, 9)));
                }
                true
            })
        };
        put(&files.b, 4, 4);
        let received = {
            let files = files.clone();
            finishes(kind, move || {
                files.b.update(|root| {
                    root.insert(1, 1);
                    watcher.try_recv().is_ok()
                })
            })
        };
        assert!(received.unwrap(), "{kind}");
        reentrant(kind, outcome.lock().as_ref().expect("the test ran"));
        assert_eq!(value(&files.b, 9), None, "{kind}");
        intact(kind, &files);
    }
}

#[test]
fn a_watcher_test_on_the_watch_thread_may_write_its_branch() {
    for (kind, options) in kinds() {
        let files = files(options);
        let outcome: Outcome = Arc::default();
        let watcher = {
            let (b, outcome) = (files.b.clone(), outcome.clone());
            files.b.watch_when(move |snapshot| {
                if snapshot.contains_key(&6) {
                    let mut slot = outcome.lock();
                    if slot.is_none() {
                        *slot = Some(b.update(|root| set(root, 7)));
                    }
                }
                snapshot.contains_key(&7)
            })
        };
        let waiting = thread::spawn(move || watcher.recv_timeout(PATIENCE).ok());
        thread::sleep(Duration::from_millis(20));
        put(&files.b, 6, 6);
        let seen = joined(kind, waiting).expect("the watcher woke");
        assert!(seen.contains_key(&7), "{kind}");
        assert!(matches!(*outcome.lock(), Some(Ok(()))), "{kind}");
    }
}

type Slot = RefCell<Option<Box<dyn FnOnce()>>>;

thread_local! {
    static ON_DROP: Slot = const { RefCell::new(None) };
    static ON_DECODE: Slot = const { RefCell::new(None) };
    static ON_DRAFT: Slot = const { RefCell::new(None) };
}

fn arm(slot: &'static LocalKey<Slot>, run: impl FnOnce() + 'static) {
    slot.with(|slot| *slot.borrow_mut() = Some(Box::new(run)));
}

fn run_armed(slot: &'static LocalKey<Slot>) {
    let armed = slot
        .try_with(|slot| slot.borrow_mut().take())
        .ok()
        .flatten();
    if let Some(run) = armed {
        run();
    }
}

fn fired(slot: &'static LocalKey<Slot>) -> bool {
    slot.with(|slot| slot.borrow_mut().take().is_none())
}

#[derive(Clone, Default)]
struct Probe;

impl Drop for Probe {
    fn drop(&mut self) {
        run_armed(&ON_DROP);
    }
}

impl Data for Probe {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.u8(0);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        r.u8()?;
        run_armed(&ON_DECODE);
        Ok(Probe)
    }
}

#[derive(Data, Clone, Default)]
struct Probed {
    rows: Map<u64, u64>,
    probe: Probe,
}

type Seen = Arc<Mutex<Results>>;

fn row(db: &Db<Probed>, key: u64) -> Option<u64> {
    db.read().rows.get(&key).map(|value| *value)
}

fn probed(options: Options, dir: &TempDir) -> (Db<Probed>, Db<Probed>, Db<Probed>) {
    let main: Db<Probed> = options.open(dir.path().join("probed.ferro")).unwrap();
    for i in 0..20 {
        main.update(|root| {
            root.rows.insert(i, i);
        })
        .unwrap();
    }
    let b = main.fork("b").unwrap();
    let c = main.fork("c").unwrap();
    (main, b, c)
}

#[test]
fn a_value_a_cycle_replaces_is_dropped_after_its_locks() {
    for (kind, options) in kinds() {
        let dir = dir();
        let (main, b, c) = probed(options(), &dir);
        let seen: Seen = Arc::default();
        let dropped = {
            let (main, b, c, seen) = (main.clone(), b.clone(), c.clone(), seen.clone());
            finishes(kind, move || {
                let inner = main.clone();
                arm(&ON_DROP, move || {
                    let mut seen = seen.lock();
                    seen.push(("update", b.update(|_| ())));
                    seen.push(("update_async", block_on(c.update_async(|_| ()))));
                    seen.push(("branch", inner.branch("c").map(drop)));
                    seen.push(("fork", c.fork("z").map(drop)));
                });
                main.compact(Keep::All).unwrap();
                fired(&ON_DROP)
            })
        };
        assert!(dropped, "{kind}: nothing was dropped in the cycle");
        check(
            kind,
            &seen.lock(),
            &["update", "update_async", "branch", "fork"],
        );
        b.update(|root| {
            root.rows.insert(100, 100);
        })
        .unwrap();
        assert_eq!(main.branches(), vec!["b", "c", "main", "z"], "{kind}");
        main.verify().unwrap();
    }
}

#[test]
fn a_value_dropped_inside_a_commit_cannot_deadlock_a_write() {
    for (kind, options) in kinds() {
        let dir = dir();
        let (main, b, c) = probed(options(), &dir);
        let seen: Seen = Arc::default();
        let dropped = {
            let (b, c, seen) = (b.clone(), c.clone(), seen.clone());
            finishes(kind, move || {
                let inner = b.clone();
                arm(&ON_DROP, move || {
                    let mut seen = seen.lock();
                    seen.push(("own branch", inner.update(|_| ())));
                    seen.push(("later branch", c.update(|_| ())));
                });
                b.update(|_| ()).unwrap();
                fired(&ON_DROP)
            })
        };
        assert!(dropped, "{kind}: nothing was dropped");
        check(kind, &seen.lock(), &[]);
        main.verify().unwrap();
    }
}

#[test]
fn user_code_run_under_tip_when_a_draft_moves_cannot_deadlock_a_write() {
    for (kind, options) in kinds() {
        let dir = dir();
        let hook = |step: &'static str| {
            if step == "drafted" {
                run_armed(&ON_DRAFT);
            }
        };
        let (main, b, c) = probed(options().hook(Hook(Arc::new(hook))), &dir);
        let a = main.fork("a").unwrap();
        let seen: Seen = Arc::default();
        let armed = {
            let (a, b, c, seen) = (a.clone(), b.clone(), c.clone(), seen.clone());
            finishes(kind, move || {
                arm(&ON_DRAFT, move || {
                    thread::scope(|scope| {
                        let landed = scope.spawn(|| {
                            a.update(|root| {
                                root.rows.insert(500, 500);
                            })
                        });
                        landed.join().unwrap().unwrap();
                    });
                    let decoding = (c.clone(), seen.clone());
                    arm(&ON_DECODE, move || {
                        let (c, seen) = decoding;
                        seen.lock().push(("decode", c.update(|_| ())));
                    });
                    arm(&ON_DROP, move || {
                        seen.lock().push(("drop", c.update(|_| ())));
                    });
                });
                b.update(|root| {
                    root.rows.insert(600, 600);
                })
                .unwrap();
                [fired(&ON_DRAFT), fired(&ON_DECODE), fired(&ON_DROP)]
            })
        };
        assert_eq!(armed, [true; 3], "{kind}: the draft did not move");
        let seen = seen.lock();
        let order: Vec<&str> = seen.iter().map(|(what, _)| *what).collect();
        assert_eq!(order, ["decode", "drop"], "{kind}");
        check(kind, &seen, &[]);
        assert_eq!(
            (row(&a, 500), row(&b, 600)),
            (Some(500), Some(600)),
            "{kind}"
        );
        main.verify().unwrap();
    }
}
