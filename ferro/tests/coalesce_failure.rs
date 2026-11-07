mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use common::{Rng, quiet_ferro_panics};
use ferro::codec::{Decoder, Encoder};
use ferro::db::Hook;
use ferro::{Data, Db, Durability, Error, Map, Options, Reclaim};

type Root = Map<u64, u64>;

const PATIENCE: Duration = Duration::from_secs(30);
const MANUAL: Reclaim = Reclaim { every_bytes: 0 };
const BRANCHES: [&str; 4] = ["main", "a", "b", "c"];
const HUNG: [u64; 6] = [5, 183, 2344, 3255, 7393, 8082];

fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn joined<T>(what: &str, handle: JoinHandle<T>) -> thread::Result<T> {
    wait_until(what, || handle.is_finished());
    handle.join()
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
            assert!(!left.is_zero(), "an awaited write was never woken");
            thread::park_timeout(left);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    Landing,
    Leading,
}

impl Failure {
    fn step(self) -> &'static str {
        match self {
            Failure::Landing => "landing",
            Failure::Leading => "leading",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Case {
    seed: u64,
    durability: Durability,
    failure: Failure,
    awaited: bool,
}

struct Run {
    acked: BTreeSet<(usize, u64)>,
    failed: usize,
    fired: bool,
}

fn run(case: Case) -> Run {
    let Case {
        seed,
        durability,
        failure,
        awaited,
    } = case;
    let mut rng = Rng::new(seed);
    let fire_at = match failure {
        Failure::Landing => 20 + rng.below(300),
        Failure::Leading => 10 + rng.below(150),
    };
    let steps = Arc::new(AtomicU64::new(0));
    let fired = Arc::new(AtomicBool::new(false));
    let hook = {
        let (steps, fired) = (steps.clone(), fired.clone());
        Hook(Arc::new(move |step| {
            if step == failure.step()
                && steps.fetch_add(1, Ordering::SeqCst) == fire_at
                && !fired.swap(true, Ordering::SeqCst)
            {
                panic!("ferro: an injected failure at {step}");
            }
        }))
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("failure.ferro");
    let main: Db<Root> = Options::new()
        .durability(durability)
        .coalesce(true)
        .reclaim(MANUAL)
        .hook(hook)
        .open(&path)
        .unwrap();
    let mut dbs = vec![main.clone()];
    for name in &BRANCHES[1..] {
        dbs.push(main.fork(name).unwrap());
    }
    let writers = 12;
    let barrier = Arc::new(Barrier::new(writers + 1));
    let (tx, rx) = mpsc::channel();
    let next = Arc::new(AtomicU64::new(1));
    for w in 0..writers {
        let (dbs, barrier, tx, next) = (dbs.clone(), barrier.clone(), tx.clone(), next.clone());
        let mut rng = Rng::new(seed * 1000 + w as u64);
        let awaited = awaited && w % 3 == 0;
        thread::spawn(move || {
            barrier.wait();
            let mut acked = Vec::new();
            let mut failed = 0;
            for _ in 0..80 {
                let at = rng.below(BRANCHES.len() as u64) as usize;
                let db = &dbs[at];
                let key = next.fetch_add(1, Ordering::SeqCst);
                let spin = rng.chance(33);
                let write = |root: &mut Root| {
                    if spin {
                        let until = Instant::now() + Duration::from_micros(30);
                        while Instant::now() < until {}
                    }
                    root.insert(key, 1);
                };
                let done = panic::catch_unwind(AssertUnwindSafe(|| match awaited {
                    true => block_on(db.update_async(write)),
                    false => db.update(write),
                }));
                match done {
                    Ok(Ok(())) => acked.push((at, key)),
                    Ok(Err(Error::Poisoned | Error::GroupFailed(_))) | Err(_) => failed += 1,
                    Ok(Err(error)) => panic!("{case:?}: unexpected {error}"),
                }
            }
            drop(dbs);
            let _ = tx.send((acked, failed));
        });
    }
    barrier.wait();
    let mut outcome = Run {
        acked: BTreeSet::new(),
        failed: 0,
        fired: false,
    };
    for _ in 0..writers {
        let Ok((acked, failed)) = rx.recv_timeout(PATIENCE) else {
            panic!(
                "{case:?}: a writer never returned after the injected failure (fired {})",
                fired.load(Ordering::SeqCst)
            );
        };
        outcome.acked.extend(acked);
        outcome.failed += failed;
    }
    outcome.fired = fired.load(Ordering::SeqCst);
    if outcome.fired {
        assert!(
            matches!(main.update(|_| ()), Err(Error::Poisoned)),
            "{case:?}: the store took a write after the failure"
        );
    }
    drop((main, dbs));
    let db: Db<Root> = Options::new().durability(durability).open(&path).unwrap();
    for (at, name) in BRANCHES.iter().enumerate() {
        let snapshot = db.branch(name).unwrap().read();
        for (_, key) in outcome.acked.iter().filter(|(branch, _)| *branch == at) {
            assert!(
                snapshot.contains_key(key),
                "{case:?}: {name} lost acknowledged key {key}"
            );
        }
    }
    db.verify().unwrap();
    outcome
}

#[test]
fn every_coalesced_writer_returns_after_an_injected_failure() {
    quiet_ferro_panics();
    let seeds: Vec<u64> = HUNG
        .into_iter()
        .chain(0..u64::from(common::cases(128)))
        .collect();
    let mut fired = BTreeMap::new();
    for &seed in &seeds {
        for durability in [Durability::Buffered, Durability::Fsync] {
            for failure in [Failure::Landing, Failure::Leading] {
                for awaited in [false, true] {
                    let case = Case {
                        seed,
                        durability,
                        failure,
                        awaited,
                    };
                    let outcome = run(case);
                    assert!(
                        outcome.acked.len() + outcome.failed == 12 * 80,
                        "{case:?}: every write answers once"
                    );
                    *fired.entry(failure.step()).or_insert(0) += usize::from(outcome.fired);
                }
            }
        }
    }
    let runs = seeds.len() * 4;
    for (step, count) in fired {
        assert!(
            count * 2 >= runs,
            "the failure at {step} fired in only {count} of {runs} runs"
        );
    }
}

#[derive(Default)]
struct Gates {
    state: Mutex<BTreeMap<&'static str, (bool, bool)>>,
    moved: Condvar,
}

impl Gates {
    fn arm(&self, name: &'static str) {
        self.state.lock().unwrap().insert(name, (false, false));
    }

    fn hold(&self, name: &str) {
        let mut state = self.state.lock().unwrap();
        let Some(key) = state.keys().find(|key| **key == name).copied() else {
            return;
        };
        if state[key].0 {
            return;
        }
        state.get_mut(key).unwrap().0 = true;
        self.moved.notify_all();
        let deadline = Instant::now() + PATIENCE;
        while !state[key].1 {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "{name} was never let go");
            state = self.moved.wait_timeout(state, left).unwrap().0;
        }
    }

    fn reached(&self, name: &'static str) {
        let mut state = self.state.lock().unwrap();
        let deadline = Instant::now() + PATIENCE;
        while !state[name].0 {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "{name} never reached its step");
            state = self.moved.wait_timeout(state, left).unwrap().0;
        }
    }

    fn release(&self, name: &'static str) {
        self.state.lock().unwrap().get_mut(name).unwrap().1 = true;
        self.moved.notify_all();
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Bad(u8);

impl Data for Bad {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        match self.0 {
            1 => Err(Error::Decode("refused to encode".to_string())),
            _ => {
                w.u8(self.0);
                Ok(())
            }
        }
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Bad(r.u8()?))
    }
}

type Tricky = Map<u64, Bad>;

fn tricky(
    name: &'static str,
    db: &Db<Tricky>,
    (key, value): (u64, u8),
    ran: &Arc<AtomicUsize>,
    after: Option<(&Arc<Gates>, &'static str)>,
) -> JoinHandle<ferro::Result<u64>> {
    let (db, ran) = (db.clone(), ran.clone());
    let after = after.map(|(gates, other)| (gates.clone(), other));
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            db.commit_with(|root| {
                if let Some((gates, other)) = &after {
                    gates.reached(other);
                }
                root.insert(key, Bad(value));
                ran.fetch_add(1, Ordering::SeqCst);
                ("", ())
            })
            .map(|done| done.version)
        })
        .unwrap()
}

#[test]
fn a_group_sealed_for_another_branch_is_written_when_the_groups_after_it_fail() {
    let settle = || thread::sleep(Duration::from_millis(30));
    for durability in [Durability::Buffered, Durability::Fsync] {
        let gates = Arc::new(Gates::default());
        let hook = {
            let gates = gates.clone();
            Hook(Arc::new(move |step| {
                let current = thread::current();
                let name = current.name().unwrap_or("");
                if matches!((step, name), ("leading", "L") | ("arriving", "X" | "Z")) {
                    gates.hold(name);
                }
            }))
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tricky.ferro");
        let main: Db<Tricky> = Options::new()
            .durability(durability)
            .coalesce(true)
            .reclaim(MANUAL)
            .hook(hook)
            .open(&path)
            .unwrap();
        main.update(|root| {
            root.insert(0, Bad(0));
        })
        .unwrap();
        let (a, b, c) = (
            main.fork("a").unwrap(),
            main.fork("b").unwrap(),
            main.fork("c").unwrap(),
        );
        let ran = Arc::new(AtomicUsize::new(0));
        gates.arm("L");
        let leader = tricky("L", &main, (1, 0), &ran, None);
        gates.reached("L");
        let tb = tricky("TB", &b, (2, 0), &ran, None);
        wait_until("b's group to open", || ran.load(Ordering::SeqCst) == 2);
        settle();
        gates.arm("X");
        let x = tricky("X", &b, (3, 1), &ran, None);
        gates.reached("X");
        let ta = tricky("TA", &a, (4, 0), &ran, None);
        wait_until("a's group to open", || ran.load(Ordering::SeqCst) == 3);
        settle();
        gates.release("L");
        assert_eq!(joined("L", leader).unwrap().unwrap(), 2);
        settle();
        gates.arm("Z");
        let t = tricky("T", &c, (5, 0), &ran, Some((&gates, "Z")));
        let z = tricky("Z", &c, (6, 1), &ran, None);
        wait_until("c's closure to run", || ran.load(Ordering::SeqCst) == 4);
        settle();
        gates.release("Z");
        for (name, handle) in [("T", t), ("Z", z)] {
            let result = joined(name, handle).unwrap();
            assert!(
                matches!(result, Err(Error::GroupFailed(_))),
                "{durability:?}: {name} {result:?}"
            );
        }
        gates.release("X");
        for (name, handle) in [("X", x), ("TB", tb)] {
            let result = joined(name, handle).unwrap();
            assert!(
                matches!(result, Err(Error::GroupFailed(_))),
                "{durability:?}: {name} {result:?}"
            );
        }
        let sealed = joined("TA", ta).unwrap();
        assert!(matches!(sealed, Ok(2)), "{durability:?}: TA {sealed:?}");
        assert_eq!(a.read().get(&4).as_deref(), Some(&Bad(0)));
        for db in [&b, &c] {
            db.update(|root| {
                root.insert(7, Bad(0));
            })
            .unwrap();
        }
        main.verify().unwrap();
        drop((main, a, b, c));
        let db: Db<Tricky> = Options::new().open(&path).unwrap();
        let a = db.branch("a").unwrap();
        assert_eq!(a.read().get(&4).as_deref(), Some(&Bad(0)));
        db.verify().unwrap();
    }
}

#[test]
fn a_leader_that_panics_fails_its_batch_and_wakes_every_writer() {
    quiet_ferro_panics();
    for coalesce in [false, true] {
        for durability in [Durability::Buffered, Durability::Fsync] {
            let gates = Arc::new(Gates::default());
            let hook = {
                let gates = gates.clone();
                Hook(Arc::new(move |step| {
                    if step == "leading" && thread::current().name() == Some("leader") {
                        gates.hold("leader");
                        panic!("ferro: the leader panicked in its sync");
                    }
                }))
            };
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("leader.ferro");
            let main: Db<Root> = Options::new()
                .durability(durability)
                .coalesce(coalesce)
                .reclaim(MANUAL)
                .hook(hook)
                .open(&path)
                .unwrap();
            main.update(|root| {
                root.insert(0, 0);
            })
            .unwrap();
            let side = main.fork("side").unwrap();
            gates.arm("leader");
            let leader = {
                let main = main.clone();
                thread::Builder::new()
                    .name("leader".into())
                    .spawn(move || {
                        main.update(|root| {
                            root.insert(1, 1);
                        })
                    })
                    .unwrap()
            };
            gates.reached("leader");
            let ran = Arc::new(AtomicUsize::new(0));
            let queued: Vec<_> = (0..6u64)
                .map(|at| {
                    let db = if at % 2 == 0 {
                        main.clone()
                    } else {
                        side.clone()
                    };
                    let ran = ran.clone();
                    thread::spawn(move || {
                        let write = |root: &mut Root| {
                            root.insert(10 + at, 1);
                            ran.fetch_add(1, Ordering::SeqCst);
                        };
                        match at % 3 {
                            0 => block_on(db.update_async(write)),
                            _ => db.update(write),
                        }
                    })
                })
                .collect();
            wait_until("the queued closures to run", || {
                ran.load(Ordering::SeqCst) == 6
            });
            thread::sleep(Duration::from_millis(30));
            gates.release("leader");
            let led = joined("the leader", leader);
            assert!(
                led.is_err(),
                "{coalesce} {durability:?}: the leader returned {led:?}"
            );
            for handle in queued {
                let result = joined("a queued writer", handle).unwrap();
                assert!(
                    matches!(result, Err(Error::Poisoned)),
                    "{coalesce} {durability:?}: {result:?}"
                );
            }
            assert!(matches!(main.update(|_| ()), Err(Error::Poisoned)));
            assert!(matches!(side.update(|_| ()), Err(Error::Poisoned)));
            drop((main, side));
            let db: Db<Root> = Options::new().open(&path).unwrap();
            assert_eq!(db.read().get(&0).as_deref(), Some(&0));
            db.verify().unwrap();
        }
    }
}
