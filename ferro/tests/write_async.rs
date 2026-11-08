mod common;

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use common::{Rng, quiet_ferro_panics};
use ferro::db::Hook;
use ferro::store::{Op, Trace};
use ferro::{Committed, Db, Durability, Error, Map, Options, Reclaim};
use parking_lot::{Condvar, Mutex};
use tempfile::TempDir;

type Root = Map<u64, u64>;

const MANUAL: Reclaim = Reclaim { every_bytes: 0 };
const PATIENCE: Duration = Duration::from_secs(30);

struct Flag {
    thread: Thread,
    woken: AtomicBool,
    wakes: AtomicUsize,
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        self.woken.store(true, Ordering::SeqCst);
        self.thread.unpark();
    }
}

fn flag() -> (Arc<Flag>, Waker) {
    let flag = Arc::new(Flag {
        thread: thread::current(),
        woken: AtomicBool::new(false),
        wakes: AtomicUsize::new(0),
    });
    (flag.clone(), Waker::from(flag))
}

fn sleep_until_woken(flag: &Flag) {
    let deadline = Instant::now() + PATIENCE;
    while !flag.woken.swap(false, Ordering::SeqCst) {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "a pending write was never woken");
        thread::park_timeout(left);
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let (flag, waker) = flag();
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        sleep_until_woken(&flag);
    }
}

fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut futures: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    let (flag, waker) = flag();
    let mut cx = Context::from_waker(&waker);
    loop {
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_none()
                && let Poll::Ready(out) = future.as_mut().poll(&mut cx)
            {
                *output = Some(out);
            }
        }
        if outputs.iter().all(Option::is_some) {
            return outputs.into_iter().map(Option::unwrap).collect();
        }
        sleep_until_woken(&flag);
    }
}

fn poll_once<F: Future>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
    let (_, waker) = flag();
    future.as_mut().poll(&mut Context::from_waker(&waker))
}

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

fn every_kind() -> Vec<(&'static str, Options)> {
    vec![
        ("fsync", fsync()),
        ("buffered", buffered()),
        ("fsync coalesced", fsync().coalesce(true)),
        ("buffered coalesced", buffered().coalesce(true)),
    ]
}

fn dump(root: &Root) -> BTreeMap<u64, u64> {
    root.iter().map(|(k, v)| (k, *v)).collect()
}

#[derive(Debug, PartialEq)]
struct Refused(u64);

impl From<Error> for Refused {
    fn from(_: Error) -> Self {
        Refused(u64::MAX)
    }
}

#[test]
fn async_writes_resolve_with_what_the_sync_api_returns() {
    for (kind, options) in every_kind() {
        let dir = dir();
        let db: Db<Root> = options.open(dir.path().join("db.ferro")).unwrap();
        let value = block_on(db.update_async(|root| {
            root.insert(1, 10);
            7
        }))
        .unwrap();
        assert_eq!(value, 7, "{kind}");
        assert_eq!(db.version(), 1, "{kind}");
        let value = block_on(db.commit_async("second", |root| root.insert(2, 20))).unwrap();
        assert_eq!(value, None, "{kind}");
        assert_eq!(db.read().commit().message, "second", "{kind}");
        let done = block_on(db.commit_with_async(|root| {
            root.insert(3, 30);
            ("third", 3)
        }))
        .unwrap();
        assert_eq!(
            (done.value, done.version, done.changed),
            (3, 3, true),
            "{kind}"
        );
        assert_eq!(done.time_ms, db.read().commit().time_ms, "{kind}");
        let same = block_on(db.commit_with_async(|root| {
            root.insert(3, 30);
            ("nothing", ())
        }))
        .unwrap();
        assert_eq!((same.version, same.changed), (3, false), "{kind}");
        let refused = block_on(db.try_commit_async("refused", |root| {
            root.insert(4, 40);
            Err::<(), _>(Refused(4))
        }));
        assert_eq!(refused, Err(Refused(4)), "{kind}");
        let refused = block_on(db.try_update_async(|root| {
            root.insert(4, 40);
            Err::<(), _>(Refused(5))
        }));
        assert_eq!(refused, Err(Refused(5)), "{kind}");
        let refused = block_on(db.try_commit_with_async(|root| {
            root.insert(4, 40);
            Err::<(&str, ()), _>(Refused(6))
        }));
        assert_eq!(refused, Err(Refused(6)), "{kind}");
        assert_eq!(db.version(), 3, "{kind}");
        assert_eq!(db.read().get(&4), None, "{kind}");
        let kept = block_on(db.try_update_async(|root| {
            root.insert(4, 40);
            Ok::<_, Refused>(4)
        }));
        assert_eq!(kept, Ok(4), "{kind}");
        let kept = block_on(db.try_commit_with_async(|root| {
            root.insert(5, 50);
            Ok::<_, Refused>(("fifth", 5))
        }))
        .unwrap();
        assert_eq!((kept.value, kept.version), (5, 5), "{kind}");
        drop(db);
        let db: Db<Root> = Options::new().open(dir.path().join("db.ferro")).unwrap();
        assert_eq!(db.version(), 5, "{kind}");
        let expected: BTreeMap<u64, u64> = (1..=5).map(|key| (key, key * 10)).collect();
        assert_eq!(dump(&db.read()), expected, "{kind}");
    }
}

#[test]
fn nothing_runs_before_the_first_poll() {
    for (kind, options) in every_kind() {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let db: Db<Root> = options.open(&path).unwrap();
        let ran = AtomicBool::new(false);
        let future = db.update_async(|root| {
            ran.store(true, Ordering::SeqCst);
            root.insert(1, 1);
        });
        thread::sleep(Duration::from_millis(5));
        drop(future);
        assert!(!ran.load(Ordering::SeqCst), "{kind}");
        assert_eq!(db.version(), 0, "{kind}");
        drop(db);
        let db: Db<Root> = Options::new().open(&path).unwrap();
        assert_eq!(db.version(), 0, "{kind}");
    }
}

type Ran = Arc<Mutex<BTreeSet<u64>>>;

fn marked(
    db: &Db<Root>,
    ran: &Ran,
    key: u64,
    value: u64,
) -> impl Future<Output = ferro::Result<()>> {
    let ran = ran.clone();
    db.update_async(move |root| {
        ran.lock().insert(key);
        root.insert(key, value);
    })
}

#[test]
fn a_write_whose_closure_ran_lands_even_if_dropped() {
    for (kind, options) in every_kind() {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let db: Db<Root> = options.open(&path).unwrap();
        let watcher = db.watch();
        let ran = Ran::default();
        let mut futures: Vec<_> = (0..20u64)
            .map(|key| Box::pin(marked(&db, &ran, key, key)))
            .collect();
        for future in &mut futures {
            let _ = poll_once(future);
        }
        drop(futures);
        let ran = ran.lock().clone();
        assert!(!ran.is_empty(), "{kind}");
        let deadline = Instant::now() + PATIENCE;
        while db.read().len() < ran.len() {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "{kind}: dropped writes never landed");
            let _ = watcher.recv_timeout(left);
        }
        let expected: BTreeMap<u64, u64> = ran.iter().map(|key| (*key, *key)).collect();
        assert_eq!(dump(&db.read()), expected, "{kind}");
        drop(watcher);
        drop(db);
        let db: Db<Root> = Options::new().open(&path).unwrap();
        assert_eq!(dump(&db.read()), expected, "{kind}");
    }
}

#[test]
fn a_write_waits_for_the_lock_as_a_future_and_withdraws_cleanly() {
    for (kind, options) in every_kind() {
        let dir = dir();
        let db: Db<Root> = options.open(dir.path().join("db.ferro")).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let holder = {
            let (db, entered, release) = (db.clone(), entered.clone(), release.clone());
            thread::spawn(move || {
                db.update(|root| {
                    entered.wait();
                    release.wait();
                    root.insert(1, 1);
                })
                .unwrap();
            })
        };
        entered.wait();
        let ran = Ran::default();
        let mut withdrawn = Box::pin(marked(&db, &ran, 2, 2));
        let mut waiting = Box::pin(marked(&db, &ran, 3, 3));
        assert!(poll_once(&mut withdrawn).is_pending(), "{kind}");
        let (flag, waker) = flag();
        let polled = waiting.as_mut().poll(&mut Context::from_waker(&waker));
        assert!(polled.is_pending(), "{kind}");
        assert!(
            ran.lock().is_empty(),
            "{kind}: a closure ran without the lock"
        );
        drop(withdrawn);
        release.wait();
        holder.join().unwrap();
        sleep_until_woken(&flag);
        block_on(waiting).unwrap();
        assert_eq!(*ran.lock(), BTreeSet::from([3]), "{kind}");
        let keys: Vec<u64> = db.read().keys().collect();
        assert_eq!(keys, vec![1, 3], "{kind}");
        assert!(flag.wakes.load(Ordering::SeqCst) >= 1, "{kind}");
    }
}

#[test]
fn a_dropped_waiter_passes_its_turn_on() {
    for (kind, options) in every_kind() {
        let dir = dir();
        let db: Db<Root> = options.open(dir.path().join("db.ferro")).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let holder = {
            let (db, entered, release) = (db.clone(), entered.clone(), release.clone());
            thread::spawn(move || {
                db.update(|root| {
                    entered.wait();
                    release.wait();
                    root.insert(1, 1);
                })
                .unwrap();
            })
        };
        entered.wait();
        let ran = Ran::default();
        let mut first = Box::pin(marked(&db, &ran, 2, 2));
        let mut second = Box::pin(marked(&db, &ran, 3, 3));
        let (woken, first_waker) = flag();
        assert!(
            first
                .as_mut()
                .poll(&mut Context::from_waker(&first_waker))
                .is_pending()
        );
        let (flag, waker) = flag();
        assert!(
            second
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        release.wait();
        holder.join().unwrap();
        sleep_until_woken(&woken);
        drop(first);
        sleep_until_woken(&flag);
        block_on(second).unwrap();
        assert_eq!(*ran.lock(), BTreeSet::from([3]), "{kind}");
    }
}

#[test]
fn closing_right_after_dropping_a_polled_write_makes_it_durable() {
    for (kind, options) in every_kind() {
        for round in 0..20u64 {
            let dir = dir();
            let path = dir.path().join("db.ferro");
            drop(options.clone().open::<Root>(&path).unwrap());
            let base = fs::read(&path).unwrap();
            let trace: Trace = Arc::new(Mutex::new(Vec::new()));
            let db: Db<Root> = options.clone().trace(trace.clone()).open(&path).unwrap();
            let ran = Ran::default();
            let mut futures: Vec<_> = (0..round % 7 + 1)
                .map(|key| Box::pin(marked(&db, &ran, key, round)))
                .collect();
            for future in &mut futures {
                let _ = poll_once(future);
            }
            drop(futures);
            drop(db);
            let recording = Recording {
                base,
                ops: trace.lock().clone(),
                states: BTreeMap::new(),
                durable: true,
            };
            let cut = recording.ops.len();
            let (image, _) = crash(&recording, cut, Loss::Everything, &mut Rng::new(round));
            fs::write(&path, image).unwrap();
            let db: Db<Root> = Options::new().open(&path).unwrap();
            let expected: BTreeMap<u64, u64> = ran.lock().iter().map(|key| (*key, round)).collect();
            assert!(!expected.is_empty(), "{kind} round {round}");
            assert_eq!(dump(&db.read()), expected, "{kind} round {round}");
            db.verify().unwrap();
        }
    }
}

#[test]
fn a_dropped_coalesced_write_does_not_strand_its_group() {
    for options in [fsync().coalesce(true), buffered().coalesce(true)] {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let db: Db<Root> = options.open(&path).unwrap();
        let ran = Ran::default();
        for round in 0..30u64 {
            let mut futures: Vec<_> = (0..8u64)
                .map(|key| Box::pin(marked(&db, &ran, round * 8 + key, round)))
                .collect();
            let mut early: Vec<_> = futures
                .iter_mut()
                .map(|future| match poll_once(future) {
                    Poll::Ready(out) => Some(out),
                    Poll::Pending => None,
                })
                .collect();
            let kept = futures.split_off(4);
            let kept_early = early.split_off(4);
            drop(futures);
            let mut pending = Vec::new();
            for (future, out) in kept.into_iter().zip(kept_early) {
                match out {
                    Some(out) => {
                        out.unwrap();
                    }
                    None => pending.push(future),
                }
            }
            for done in join_all(pending) {
                done.unwrap();
            }
            let blocking = db.update(|root| root.len()).unwrap();
            assert_eq!(blocking, ran.lock().len(), "round {round}");
        }
        let ran = ran.lock().clone();
        assert!(ran.len() >= 120);
        assert_eq!(db.read().keys().collect::<BTreeSet<u64>>(), ran);
        drop(db);
        let db: Db<Root> = Options::new().open(&path).unwrap();
        assert_eq!(db.read().keys().collect::<BTreeSet<u64>>(), ran);
    }
}

#[test]
fn one_thread_carries_many_writes_at_once() {
    for (kind, options) in every_kind() {
        let dir = dir();
        let db: Db<Root> = options.open(dir.path().join("db.ferro")).unwrap();
        let futures: Vec<_> = (0..300u64)
            .map(|key| {
                db.commit_with_async(move |root| {
                    root.insert(key, key + 1);
                    (format!("put {key}"), key)
                })
            })
            .collect();
        let done: Vec<Committed<u64>> = join_all(futures).into_iter().map(Result::unwrap).collect();
        let versions: BTreeSet<u64> = done.iter().map(|done| done.version).collect();
        if kind.contains("coalesced") {
            assert_eq!(versions.last(), Some(&db.version()), "{kind}");
        } else {
            assert_eq!(versions, (1..=300).collect(), "{kind}");
        }
        assert_eq!(db.read().len(), 300, "{kind}");
        for done in &done {
            let snapshot = db.at(done.version).unwrap();
            assert_eq!(
                snapshot.get(&done.value).as_deref(),
                Some(&(done.value + 1)),
                "{kind}"
            );
            assert!(
                snapshot
                    .commit()
                    .messages()
                    .any(|message| message == format!("put {}", done.value))
            );
        }
    }
}

#[test]
fn async_and_blocking_writers_share_the_writer() {
    for (kind, options) in every_kind() {
        let dir = dir();
        let db: Db<Root> = options.open(dir.path().join("db.ferro")).unwrap();
        let threads = 4u64;
        let each = 60u64;
        let start = Arc::new(Barrier::new(threads as usize * 2));
        let mut handles = Vec::new();
        for thread in 0..threads {
            let (db, blocking) = (db.clone(), db.clone());
            let (start, gate) = (start.clone(), start.clone());
            handles.push(thread::spawn(move || {
                start.wait();
                let mut versions = Vec::new();
                for i in 0..each {
                    let done = db.commit_with_async(|root| {
                        let count = root.get(&u64::MAX).map_or(0, |count| *count);
                        root.insert(u64::MAX, count + 1);
                        root.insert(thread * 1000 + i, 1);
                        ("async", ())
                    });
                    versions.push(block_on(done).unwrap().version);
                }
                let batch: Vec<_> = (0..each)
                    .map(|i| {
                        let db = &db;
                        db.update_async(move |root| {
                            let count = root.get(&u64::MAX).map_or(0, |count| *count);
                            root.insert(u64::MAX, count + 1);
                            root.insert(thread * 1000 + 500 + i, 1);
                        })
                    })
                    .collect();
                for done in join_all(batch) {
                    done.unwrap();
                }
                versions
            }));
            handles.push(thread::spawn(move || {
                gate.wait();
                let mut versions = Vec::new();
                for i in 0..each * 2 {
                    let done = blocking
                        .commit_with(|root| {
                            let count = root.get(&u64::MAX).map_or(0, |count| *count);
                            root.insert(u64::MAX, count + 1);
                            root.insert(100_000 + thread * 1000 + i, 1);
                            ("blocking", ())
                        })
                        .unwrap();
                    versions.push(done.version);
                }
                versions
            }));
        }
        for handle in handles {
            let versions = handle.join().unwrap();
            assert!(versions.windows(2).all(|pair| pair[0] < pair[1]), "{kind}");
        }
        let snapshot = db.read();
        let total = threads * each * 4;
        assert_eq!(snapshot.get(&u64::MAX).as_deref(), Some(&total), "{kind}");
        assert_eq!(snapshot.len() as u64, total + 1, "{kind}");
        if !kind.contains("coalesced") {
            assert_eq!(db.version(), total, "{kind}");
        }
        db.verify().unwrap();
    }
}

#[test]
fn coalesced_async_writes_share_versions() {
    let dir = dir();
    let leading = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let hook = {
        let (leading, released) = (leading.clone(), released.clone());
        move |step: &'static str| {
            if step == "leading" && thread::current().name() == Some("leader") {
                leading.store(true, Ordering::SeqCst);
                while !released.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    };
    let db: Db<Root> = fsync()
        .coalesce(true)
        .hook(Hook(Arc::new(hook)))
        .open(dir.path().join("db.ferro"))
        .unwrap();
    let leader = {
        let db = db.clone();
        thread::Builder::new()
            .name("leader".to_string())
            .spawn(move || {
                db.update(|root| {
                    root.insert(1_000, 1_000);
                })
            })
            .unwrap()
    };
    let deadline = Instant::now() + PATIENCE;
    while !leading.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the leader never reached its sync"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let mut futures: Vec<_> = (0..200u64)
        .map(|key| {
            Box::pin(db.commit_with_async(move |root| {
                root.insert(key, key);
                (format!("k{key}"), key)
            }))
        })
        .collect();
    for future in &mut futures {
        assert!(poll_once(future).is_pending());
    }
    released.store(true, Ordering::SeqCst);
    leader.join().unwrap().unwrap();
    let done: Vec<_> = join_all(futures).into_iter().map(Result::unwrap).collect();
    let versions: BTreeSet<u64> = done.iter().map(|done| done.version).collect();
    assert_eq!(
        versions.into_iter().collect::<Vec<_>>(),
        vec![2],
        "200 writes queued behind one sync took more than one version"
    );
    assert_eq!(db.version(), 2);
    let mut messages = 0;
    for version in 1..=db.version() {
        let snapshot = db.at(version).unwrap();
        let mut seen: Vec<u64> = snapshot
            .commit()
            .messages()
            .map(|message| message[1..].parse().unwrap())
            .collect();
        messages += seen.len();
        seen.sort_unstable();
        for key in seen {
            assert!(
                done.iter()
                    .any(|done| done.value == key && done.version == version)
            );
        }
    }
    assert_eq!(messages, 200);
}

fn woken_by(flag: &Flag, deadline: Instant) -> bool {
    while !flag.woken.swap(false, Ordering::SeqCst) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        thread::park_timeout(left);
    }
    true
}

fn within<F: Future>(future: F, patience: Duration) -> Option<F::Output> {
    let mut future = pin!(future);
    let (flag, waker) = flag();
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + patience;
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return Some(out);
        }
        if !woken_by(&flag, deadline) {
            return None;
        }
    }
}

type Marks = HashMap<(&'static str, &'static str), (bool, bool)>;

#[derive(Default)]
struct Steps {
    held: Mutex<Marks>,
    moved: Condvar,
}

impl Steps {
    fn hook(self: &Arc<Self>) -> Hook {
        let steps = self.clone();
        Hook(Arc::new(move |step| {
            if let Some(name) = thread::current().name() {
                steps.pass(name, step);
            }
        }))
    }

    fn arm(&self, thread: &'static str, step: &'static str) {
        self.held.lock().insert((thread, step), (false, false));
    }

    fn pass(&self, thread: &str, step: &str) {
        let mut held = self.held.lock();
        let Some(key) = held
            .keys()
            .find(|(name, at)| *name == thread && *at == step)
            .copied()
        else {
            return;
        };
        if held[&key].0 {
            return;
        }
        held.insert(key, (true, false));
        self.moved.notify_all();
        while !held[&key].1 {
            self.moved.wait(&mut held);
        }
    }

    fn reached(&self, thread: &'static str, step: &'static str) {
        let deadline = Instant::now() + PATIENCE;
        let mut held = self.held.lock();
        while !held[&(thread, step)].0 {
            let late = self.moved.wait_until(&mut held, deadline).timed_out();
            assert!(!late, "{thread} never reached {step}");
        }
    }

    fn release(&self, thread: &'static str, step: &'static str) {
        self.held.lock().insert((thread, step), (true, true));
        self.moved.notify_all();
    }
}

fn sync_held(steps: &Steps, db: &Db<Root>, name: &'static str, key: u64) -> thread::JoinHandle<()> {
    steps.arm(name, "leading");
    let db = db.clone();
    let leader = thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            db.update(|root| {
                root.insert(key, key);
            })
            .unwrap();
        })
        .unwrap();
    steps.reached(name, "leading");
    leader
}

fn lands_promptly(db: &Db<Root>, key: u64, case: &str) {
    let started = Instant::now();
    let done = within(
        db.update_async(move |root| {
            root.insert(key, key);
        }),
        Duration::from_secs(5),
    );
    let took = started.elapsed();
    assert!(
        done.is_some_and(|done| done.is_ok()) && took < Duration::from_secs(1),
        "{case}: an awaited write on branch a waited {took:?} for a closure on branch b"
    );
}

fn on_branches(steps: &Arc<Steps>) -> (TempDir, Db<Root>, Db<Root>, Db<Root>) {
    let dir = dir();
    let main: Db<Root> = buffered()
        .coalesce(true)
        .hook(steps.hook())
        .open(dir.path().join("db.ferro"))
        .unwrap();
    let a = main.fork("a").unwrap();
    let b = main.fork("b").unwrap();
    (dir, main, a, b)
}

#[test]
fn a_long_closure_on_one_branch_holds_up_no_awaited_write_on_another() {
    let steps = Arc::new(Steps::default());
    let (_dir, main, a, b) = on_branches(&steps);
    let leader = sync_held(&steps, &main, "leader", 100);
    let mut member = Box::pin(b.update_async(|root| {
        root.insert(1, 1);
    }));
    assert!(poll_once(&mut member).is_pending());
    let (started, running) = mpsc::channel();
    let long = {
        let b = b.clone();
        thread::spawn(move || {
            b.update(|root| {
                started.send(()).unwrap();
                thread::sleep(Duration::from_secs(2));
                root.insert(2, 2);
            })
        })
    };
    running.recv().unwrap();
    steps.release("leader", "leading");
    leader.join().unwrap();
    lands_promptly(&a, 3, "a long closure");
    long.join().unwrap().unwrap();
    within(member, PATIENCE)
        .expect("the write on b resolves once its branch's closure ends")
        .unwrap();
    assert_eq!(b.read().keys().collect::<Vec<u64>>(), vec![1, 2]);
    assert_eq!(a.read().keys().collect::<Vec<u64>>(), vec![3]);
}

#[derive(Clone, Copy, Debug)]
enum Holder {
    Blocking,
    Awaited,
}

#[derive(Clone, Copy, Debug)]
enum Ends {
    Commits,
    Fails,
    Panics,
}

type Held = thread::Result<std::result::Result<(), Refused>>;

fn hold_lane(
    b: &Db<Root>,
    holder: Holder,
    ends: Ends,
    started: mpsc::Sender<()>,
    go: mpsc::Receiver<()>,
) -> Held {
    let closure = move |root: &mut Root| -> std::result::Result<(), Refused> {
        started.send(()).unwrap();
        let _ = go.recv();
        root.insert(2, 2);
        match ends {
            Ends::Commits => Ok(()),
            Ends::Fails => Err(Refused(2)),
            Ends::Panics => panic!("ferro: the closure holding the lane gave up"),
        }
    };
    panic::catch_unwind(AssertUnwindSafe(|| match holder {
        Holder::Blocking => b.try_update(closure),
        Holder::Awaited => block_on(b.try_update_async(closure)),
    }))
}

fn a_busy_lane_strands_no_group(holder: Holder, ends: Ends) {
    let case = format!("{holder:?} closure that {ends:?}");
    let steps = Arc::new(Steps::default());
    let (_dir, main, a, b) = on_branches(&steps);
    let first = sync_held(&steps, &main, "first", 100);
    let mut member = Box::pin(b.update_async(|root| {
        root.insert(1, 1);
    }));
    assert!(poll_once(&mut member).is_pending(), "{case}");
    steps.arm("holder", "handed");
    let (started, running) = mpsc::channel();
    let (go, wait) = mpsc::channel();
    let holding = {
        let b = b.clone();
        thread::Builder::new()
            .name("holder".to_string())
            .spawn(move || hold_lane(&b, holder, ends, started, wait))
            .unwrap()
    };
    running.recv().unwrap();
    steps.release("first", "leading");
    first.join().unwrap();
    lands_promptly(&a, 3, &case);
    let second = sync_held(&steps, &main, "second", 101);
    go.send(()).unwrap();
    steps.reached("holder", "handed");
    steps.release("second", "leading");
    second.join().unwrap();
    let (flag, waker) = flag();
    let mut cx = Context::from_waker(&waker);
    assert!(member.as_mut().poll(&mut cx).is_pending(), "{case}");
    lands_promptly(&a, 4, &case);
    loop {
        thread::sleep(Duration::from_millis(100));
        if !flag.woken.swap(false, Ordering::SeqCst) {
            break;
        }
        assert!(member.as_mut().poll(&mut cx).is_pending(), "{case}");
    }
    steps.release("holder", "handed");
    let held = holding.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let done = loop {
        assert!(
            woken_by(&flag, deadline),
            "{case}: the group it handed on was never sealed"
        );
        if let Poll::Ready(done) = member.as_mut().poll(&mut cx) {
            break done;
        }
    };
    done.unwrap();
    let mut keys = vec![1];
    match (ends, held) {
        (Ends::Commits, Ok(Ok(()))) => keys.push(2),
        (Ends::Fails, Ok(Err(Refused(2)))) | (Ends::Panics, Err(_)) => {}
        (_, held) => panic!("{case}: the holder returned {held:?}"),
    }
    assert_eq!(b.read().keys().collect::<Vec<u64>>(), keys, "{case}");
    b.verify().unwrap();
}

#[test]
fn a_group_the_sync_thread_finds_busy_is_finished_by_the_closure_holding_its_lane() {
    quiet_ferro_panics();
    for holder in [Holder::Blocking, Holder::Awaited] {
        for ends in [Ends::Commits, Ends::Fails, Ends::Panics] {
            a_busy_lane_strands_no_group(holder, ends);
        }
    }
}

#[test]
fn a_panic_in_an_async_closure_unwinds_to_its_poller() {
    quiet_ferro_panics();
    for (kind, options) in every_kind() {
        let dir = dir();
        let db: Db<Root> = options.open(dir.path().join("db.ferro")).unwrap();
        block_on(db.update_async(|root| {
            root.insert(1, 1);
        }))
        .unwrap();
        let mut good = Box::pin(db.update_async(|root| {
            root.insert(2, 2);
        }));
        let early = poll_once(&mut good);
        let unwound = panic::catch_unwind(AssertUnwindSafe(|| {
            block_on(db.update_async(|root| {
                root.insert(3, 3);
                panic!("ferro: closure gave up");
            }))
        }));
        assert!(unwound.is_err(), "{kind}");
        match early {
            Poll::Ready(out) => out.unwrap(),
            Poll::Pending => block_on(good).unwrap(),
        }
        block_on(db.update_async(|root| {
            root.insert(4, 4);
        }))
        .unwrap();
        let keys: Vec<u64> = db.read().keys().collect();
        assert_eq!(keys, vec![1, 2, 4], "{kind}");
        db.verify().unwrap();
    }
}

#[test]
fn closures_and_results_need_not_be_send() {
    let dir = dir();
    let db: Db<Root> = fsync().open(dir.path().join("db.ferro")).unwrap();
    let local = Rc::new(Cell::new(0u64));
    let seen = block_on(db.update_async(|root| {
        local.set(local.get() + 1);
        root.insert(1, local.get());
        local.clone()
    }))
    .unwrap();
    assert_eq!(seen.get(), 1);
    let mut borrowed = Vec::new();
    block_on(db.update_async(|root| borrowed.extend(root.keys()))).unwrap();
    assert_eq!(borrowed, vec![1]);
}

fn sendable<F: Future + Send>(future: F) -> F {
    future
}

#[test]
fn stamped_async_writes_record_the_version_and_time_they_commit_at() {
    for (kind, options) in every_kind() {
        let dir = dir();
        let db: Db<Root> = options.open(dir.path().join("db.ferro")).unwrap();
        let done = block_on(db.commit_stamped_async(|root, stamp| {
            root.insert(stamp.version(), stamp.time_ms());
            ("first", stamp.version())
        }))
        .unwrap();
        assert_eq!((done.value, done.version), (1, 1), "{kind}");
        assert_eq!(
            db.read().get(&1).map(|at| *at),
            Some(done.time_ms),
            "{kind}"
        );
        let done = block_on(db.try_commit_stamped_async(|root, stamp| {
            root.insert(stamp.version(), stamp.time_ms());
            Ok::<_, Refused>(("second", stamp.time_ms()))
        }))
        .unwrap();
        assert_eq!((done.version, done.value), (2, done.time_ms), "{kind}");
        assert_eq!(db.as_of(done.time_ms).unwrap().version(), 2, "{kind}");
        let refused = block_on(db.try_commit_stamped_async(|root, stamp| {
            root.insert(stamp.version(), stamp.time_ms());
            Err::<(&str, ()), _>(Refused(stamp.version()))
        }));
        assert_eq!(refused, Err(Refused(3)), "{kind}");
        assert_eq!(db.version(), 2, "{kind}");
        assert_eq!(db.read().get(&3), None, "{kind}");
        let future = sendable(db.try_commit_stamped_async(|root, stamp| {
            root.insert(stamp.version(), 0);
            Ok::<_, Error>(("sent", stamp.version()))
        }));
        let sent = thread::scope(|scope| scope.spawn(|| block_on(future)).join().unwrap()).unwrap();
        assert_eq!((sent.value, sent.version), (3, 3), "{kind}");
    }
}

#[test]
fn the_future_is_send_when_its_closure_is() {
    let dir = dir();
    let db: Db<Root> = fsync()
        .coalesce(true)
        .open(dir.path().join("db.ferro"))
        .unwrap();
    let future = sendable(db.try_commit_with_async(|root| {
        root.insert(1, 1);
        Ok::<_, Error>(("sent", 1))
    }));
    let done = thread::scope(|scope| scope.spawn(|| block_on(future)).join().unwrap()).unwrap();
    assert_eq!(done.version, 1);
}

#[test]
fn an_async_write_to_a_dropped_branch_fails() {
    for (kind, options) in every_kind() {
        let dir = dir();
        let db: Db<Root> = options.open(dir.path().join("db.ferro")).unwrap();
        let side = db.fork("side").unwrap();
        db.drop_branch("side").unwrap();
        let failed = block_on(side.update_async(|root| {
            root.insert(1, 1);
        }));
        assert!(
            matches!(failed, Err(Error::NoBranch(_))),
            "{kind}: {failed:?}"
        );
        assert_eq!(db.version(), 0, "{kind}");
    }
}

fn last_frame_write(ops: &[Op]) -> Option<usize> {
    ops.iter()
        .rposition(|op| matches!(op, Op::Write(_, at, _) if *at >= 8192))
}

fn last_sync(ops: &[Op]) -> Option<usize> {
    ops.iter().rposition(|op| matches!(op, Op::Sync(_)))
}

#[test]
fn an_fsync_write_resolves_only_after_its_sync() {
    for coalesce in [false, true] {
        let dir = dir();
        let trace: Trace = Arc::new(Mutex::new(Vec::new()));
        let db: Db<Root> = fsync()
            .coalesce(coalesce)
            .trace(trace.clone())
            .open(dir.path().join("db.ferro"))
            .unwrap();
        for key in 0..30u64 {
            let start = trace.lock().len();
            let done = block_on(db.commit_with_async(|root| {
                root.insert(key, key);
                ("w", ())
            }))
            .unwrap();
            assert_eq!(done.version, key + 1);
            let ops = trace.lock()[start..].to_vec();
            let wrote = last_frame_write(&ops).expect("a frame was written");
            let synced = last_sync(&ops).expect("the frame was synced");
            assert!(synced > wrote, "v{} resolved before its sync", done.version);
        }
    }
}

struct Recording {
    base: Vec<u8>,
    ops: Vec<Op>,
    states: BTreeMap<u64, BTreeMap<u64, u64>>,
    durable: bool,
}

fn record(path: &Path, options: fn() -> Options, durable: bool, seed: u64) -> Recording {
    {
        let db: Db<Root> = options().open(path).unwrap();
        block_on(db.update_async(|root| {
            root.insert(u64::MAX, 0);
        }))
        .unwrap();
    }
    let base = fs::read(path).unwrap();
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    {
        let db: Db<Root> = options().trace(trace.clone()).open(path).unwrap();
        let mut rng = Rng::new(seed);
        for round in 0..6u64 {
            let mut futures: Vec<_> = (0..rng.below(12) + 1)
                .map(|i| {
                    let key = rng.below(40);
                    let value = round * 100 + i;
                    Box::pin(db.update_async(move |root| {
                        root.insert(key, value);
                    }))
                })
                .collect();
            if rng.chance(30) {
                let mut early: Vec<_> = futures.iter_mut().map(poll_once).collect();
                let keep = futures.len() / 2;
                futures.truncate(keep);
                early.truncate(keep);
                let mut pending = Vec::new();
                for (future, out) in futures.into_iter().zip(early) {
                    match out {
                        Poll::Ready(out) => {
                            out.unwrap();
                            db.trace_op(Op::Acked(db.version()));
                        }
                        Poll::Pending => pending.push(future),
                    }
                }
                futures = pending;
            }
            let acked: Vec<_> = futures
                .into_iter()
                .map(|future| {
                    let db = &db;
                    async move {
                        future.await.unwrap();
                        db.trace_op(Op::Acked(db.version()));
                    }
                })
                .collect();
            join_all(acked);
        }
    }
    let db: Db<Root> = options().open(path).unwrap();
    let states = (1..=db.version())
        .map(|version| (version, dump(&db.at(version).unwrap())))
        .collect();
    Recording {
        base,
        ops: trace.lock().clone(),
        states,
        durable,
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
        apply(image, op);
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

fn is_superblock(op: &Op) -> bool {
    matches!(op, Op::Write(_, at, _) if *at < 8192)
}

fn crash(recording: &Recording, cut: usize, loss: Loss, rng: &mut Rng) -> (Vec<u8>, u64) {
    let mut image = recording.base.clone();
    let mut pending: Vec<&Op> = Vec::new();
    let (mut floor, mut acked) = (1, 1);
    for op in &recording.ops[..cut] {
        match op {
            Op::Write(..) | Op::SetLen(..) => pending.push(op),
            Op::Sync(_) => {
                for done in pending.drain(..) {
                    apply(&mut image, done);
                }
                floor = floor.max(acked);
            }
            Op::Acked(version) => {
                acked = acked.max(*version);
                if recording.durable {
                    floor = acked;
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
    (image, floor)
}

fn replay(recording: &Recording, options: fn() -> Options, seed: u64) -> usize {
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
            let (image, floor) = crash(recording, cut, loss, &mut rng);
            fs::write(&path, &image).unwrap();
            let context = format!("cut {cut} of {} {loss:?}", recording.ops.len());
            let db: Db<Root> = options()
                .open(&path)
                .unwrap_or_else(|error| panic!("{context}: open: {error}"));
            let version = db.version();
            assert!(
                version >= floor,
                "{context}: lost acknowledged v{floor}, opened v{version}"
            );
            assert_eq!(dump(&db.read()), recording.states[&version], "{context}");
            db.verify()
                .unwrap_or_else(|error| panic!("{context}: verify: {error}"));
            block_on(db.update_async(|root| {
                root.insert(u64::MAX - 1, 1);
            }))
            .unwrap_or_else(|error| panic!("{context}: write after recovery: {error}"));
            assert_eq!(db.version(), version + 1, "{context}");
            images += 1;
        }
        cut += stride;
    }
    images
}

type Kind = (&'static str, fn() -> Options, bool);

#[test]
fn a_crash_during_async_writes_keeps_every_acknowledged_version() {
    let kinds: [Kind; 4] = [
        ("fsync", fsync, true),
        ("buffered", || buffered().checkpoint_bytes(4096), false),
        ("fsync coalesced", || fsync().coalesce(true), true),
        (
            "buffered coalesced",
            || buffered().checkpoint_bytes(4096).coalesce(true),
            false,
        ),
    ];
    for (index, (kind, options, durable)) in kinds.into_iter().enumerate() {
        let dir = dir();
        let path = dir.path().join("trace.ferro");
        let recording = record(&path, options, durable, 70 + index as u64);
        assert!(recording.states.len() > 6, "{kind}: too few versions");
        let images = replay(&recording, options, 90 + index as u64);
        assert!(images > 100, "{kind}: only {images} images");
    }
}
