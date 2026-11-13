mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use common::Rng;
use ferro::{Db, Durability, Error, Keep, Map, Options, Reclaim, Snapshot};
use parking_lot::Mutex;

type Root = Map<u64, u64>;

const PATIENCE: Duration = Duration::from_secs(60);
const SIDE: &str = "side";

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

fn drive<F: Future>(futures: Vec<F>, drop_after_first_poll: &[bool]) -> Vec<Option<F::Output>> {
    let mut futures: Vec<Option<Pin<Box<F>>>> = futures
        .into_iter()
        .map(|future| Some(Box::pin(future)))
        .collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    let flag = Arc::new(Flag {
        thread: thread::current(),
        woken: AtomicBool::new(false),
    });
    let waker = Waker::from(flag.clone());
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + PATIENCE;
    let mut first = true;
    loop {
        for (index, slot) in futures.iter_mut().enumerate() {
            let Some(future) = slot else {
                continue;
            };
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                outputs[index] = Some(out);
                *slot = None;
            } else if first && drop_after_first_poll[index] {
                *slot = None;
            }
        }
        first = false;
        if futures.iter().all(Option::is_none) {
            return outputs;
        }
        while !flag.woken.swap(false, Ordering::SeqCst) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "an awaited write was never woken");
            thread::park_timeout(left);
        }
    }
}

#[derive(Default)]
struct Ledger {
    ran: Mutex<Vec<(u64, Db<Root>)>>,
    acked: Mutex<Vec<(u64, Db<Root>)>>,
}

fn deltas(root: &mut Root, base: &Snapshot<Root>, theirs: &Snapshot<Root>) -> (String, ()) {
    let mut keys: Vec<u64> = base.keys().chain(theirs.keys()).collect();
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        let was = base.get(&key).map_or(0, |v| *v);
        let now = theirs.get(&key).map_or(0, |v| *v);
        if was != now {
            let next = root.get(&key).map_or(0, |v| *v) + now - was;
            root.insert(key, next);
        }
    }
    ("deltas".to_string(), ())
}

fn side_of(main: &Db<Root>) -> Option<Db<Root>> {
    match main.branch(SIDE) {
        Ok(side) => Some(side),
        Err(Error::NoBranch(_)) => None,
        Err(error) => panic!("{error}"),
    }
}

fn live(db: &Db<Root>) -> bool {
    match db.update(|_| ()) {
        Ok(()) => true,
        Err(Error::NoBranch(_)) => false,
        Err(error) => panic!("{error}"),
    }
}

fn async_writer(
    main: Db<Root>,
    ledger: Arc<Ledger>,
    next: Arc<AtomicU64>,
    seed: u64,
    rounds: usize,
) {
    let mut rng = Rng::new(seed);
    for _ in 0..rounds {
        let count = 1 + rng.below(4) as usize;
        let mut handles = Vec::new();
        for _ in 0..count {
            let target = match rng.chance(40) {
                true => side_of(&main),
                false => None,
            };
            handles.push(target.unwrap_or_else(|| main.clone()));
        }
        let keys: Vec<u64> = handles
            .iter()
            .map(|_| next.fetch_add(1, Ordering::SeqCst))
            .collect();
        let dropped: Vec<bool> = handles.iter().map(|_| rng.chance(15)).collect();
        let futures: Vec<_> = handles
            .iter()
            .zip(&keys)
            .map(|(db, key)| {
                let (key, ledger, handle) = (*key, ledger.clone(), db.clone());
                db.commit_async(format!("k{key}"), move |root| {
                    root.insert(key, 1);
                    ledger.ran.lock().push((key, handle));
                })
            })
            .collect();
        let outputs = drive(futures, &dropped);
        for ((output, key), db) in outputs.into_iter().zip(&keys).zip(&handles) {
            match output {
                Some(Ok(())) => ledger.acked.lock().push((*key, db.clone())),
                Some(Err(Error::NoBranch(_))) | None => {}
                Some(Err(error)) => panic!("{error}"),
            }
        }
    }
}

fn blocking_writer(
    main: Db<Root>,
    ledger: Arc<Ledger>,
    next: Arc<AtomicU64>,
    seed: u64,
    rounds: usize,
) {
    let mut rng = Rng::new(seed);
    for _ in 0..rounds {
        let db = match rng.chance(40) {
            true => side_of(&main).unwrap_or_else(|| main.clone()),
            false => main.clone(),
        };
        let key = next.fetch_add(1, Ordering::SeqCst);
        let handle = db.clone();
        let done = db.commit(format!("k{key}"), |root| {
            root.insert(key, 1);
            ledger.ran.lock().push((key, handle));
        });
        match done {
            Ok(()) => ledger.acked.lock().push((key, db.clone())),
            Err(Error::NoBranch(_)) => {}
            Err(error) => panic!("{error}"),
        }
    }
}

fn chaos(main: Db<Root>, stop: Arc<AtomicBool>, seed: u64) -> BTreeMap<&'static str, usize> {
    let mut rng = Rng::new(seed);
    let mut tally = BTreeMap::new();
    while !stop.load(Ordering::SeqCst) {
        let what = match rng.below(6) {
            0 => match side_of(&main) {
                Some(_) => {
                    main.drop_branch(SIDE).unwrap();
                    "drop"
                }
                None => {
                    main.fork(SIDE).unwrap();
                    "fork"
                }
            },
            1 => match side_of(&main) {
                Some(side) => match main.merge_with(&side, deltas) {
                    Ok(_) | Err(Error::NoBranch(_)) => "merge",
                    Err(error) => panic!("{error}"),
                },
                None => "no side",
            },
            2 => {
                main.compact(Keep::All).unwrap();
                "compact"
            }
            3 => {
                main.verify().unwrap();
                "verify"
            }
            4 => match side_of(&main) {
                Some(side) => match side.merge_with(&main, deltas) {
                    Ok(_) | Err(Error::NoBranch(_)) => "merge back",
                    Err(error) => panic!("{error}"),
                },
                None => "no side",
            },
            _ => {
                thread::sleep(Duration::from_millis(1));
                "sleep"
            }
        };
        *tally.entry(what).or_default() += 1;
    }
    tally
}

fn watcher(main: Db<Root>, stop: Arc<AtomicBool>) -> usize {
    let watcher = main.watch();
    let mut last = main.version();
    let mut seen = 0;
    while !stop.load(Ordering::SeqCst) {
        if let Ok(snapshot) = watcher.recv_timeout(Duration::from_millis(20)) {
            assert!(snapshot.version() >= last, "a watcher went back");
            last = snapshot.version();
            seen += 1;
        }
    }
    seen
}

fn peer(
    main: Db<Root>,
    peer: Db<Root>,
    next: Arc<AtomicU64>,
    seed: u64,
    rounds: usize,
) -> Vec<u64> {
    let mut rng = Rng::new(seed);
    let mut written = Vec::new();
    for _ in 0..rounds {
        let key = next.fetch_add(1, Ordering::SeqCst);
        peer.commit(format!("p{key}"), |root| {
            root.insert(key, 1);
        })
        .unwrap();
        written.push(key);
        if rng.chance(30) {
            peer.pull(&main, "hub", deltas).unwrap();
        }
        if rng.chance(30) {
            main.pull(&peer, "peer", deltas).unwrap();
        }
    }
    written
}

fn joined<R>(what: &str, handle: thread::JoinHandle<R>, done: &mpsc::Receiver<()>) -> R {
    let finished = done.recv_timeout(PATIENCE);
    assert!(finished.is_ok() || handle.is_finished(), "{what} is stuck");
    handle.join().unwrap()
}

fn run(options: Options, seed: u64) -> BTreeMap<&'static str, usize> {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("mix.ferro");
    let main: Db<Root> = options.clone().open(&path).unwrap();
    let other: Db<Root> = options.clone().open(dir.path().join("peer.ferro")).unwrap();
    let ledger = Arc::new(Ledger::default());
    let next = Arc::new(AtomicU64::new(1));
    let stop = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(7));
    let mut writers = Vec::new();
    for index in 0..3u64 {
        let (main, ledger, next, start) =
            (main.clone(), ledger.clone(), next.clone(), start.clone());
        let (sender, done) = mpsc::channel();
        writers.push((
            thread::spawn(move || {
                start.wait();
                async_writer(main, ledger, next, seed * 31 + index, 40);
                let _ = sender.send(());
            }),
            done,
        ));
    }
    {
        let (main, ledger, next, start) =
            (main.clone(), ledger.clone(), next.clone(), start.clone());
        let (sender, done) = mpsc::channel();
        writers.push((
            thread::spawn(move || {
                start.wait();
                blocking_writer(main, ledger, next, seed * 31 + 7, 80);
                let _ = sender.send(());
            }),
            done,
        ));
    }
    let peering = {
        let (main, other, next, start) = (main.clone(), other.clone(), next.clone(), start.clone());
        let (sender, done) = mpsc::channel();
        (
            thread::spawn(move || {
                start.wait();
                let written = peer(main, other, next, seed * 31 + 13, 40);
                let _ = sender.send(());
                written
            }),
            done,
        )
    };
    let chaotic = {
        let (main, stop, start) = (main.clone(), stop.clone(), start.clone());
        thread::spawn(move || {
            start.wait();
            chaos(main, stop, seed * 31 + 11)
        })
    };
    let watching = {
        let (main, stop, start) = (main.clone(), stop.clone(), start.clone());
        thread::spawn(move || {
            start.wait();
            watcher(main, stop)
        })
    };
    for (handle, done) in writers {
        joined("a writer", handle, &done);
    }
    let peer_keys = joined("the peer", peering.0, &peering.1);
    stop.store(true, Ordering::SeqCst);
    let mut tally = chaotic.join().unwrap();
    watching.join().unwrap();
    main.pull(&other, "peer", deltas).unwrap();
    main.verify().unwrap();
    main.verify_typed().unwrap();
    other.verify_typed().unwrap();
    let side = side_of(&main);
    let content =
        |db: &Db<Root>| -> BTreeMap<u64, u64> { db.read().iter().map(|(k, v)| (k, *v)).collect() };
    let main_keys = content(&main);
    assert!(
        main_keys.values().all(|value| *value == 1),
        "a delta counted twice or undone"
    );
    assert!(
        content(&other).values().all(|value| *value == 1),
        "a delta counted twice or undone"
    );
    if let Some(side) = &side {
        assert!(
            content(side).values().all(|value| *value == 1),
            "a delta counted twice or undone"
        );
    }
    assert!(
        peer_keys.iter().all(|key| main_keys.contains_key(key)),
        "main lost a peer write"
    );
    let ran: Vec<(u64, Db<Root>)> = ledger.ran.lock().clone();
    let mut all_ran: BTreeSet<u64> = ran.iter().map(|(key, _)| *key).collect();
    all_ran.extend(&peer_keys);
    assert!(
        main_keys.keys().all(|key| all_ran.contains(key)),
        "main holds a key no closure wrote"
    );
    for (key, db) in &ran {
        if db.path() == main.path() && db.branch_name() == "main" {
            assert!(
                main_keys.contains_key(key),
                "main lost k{key} whose closure ran"
            );
        } else if live(db) {
            let side = side.as_ref().expect("a live side handle has a branch");
            assert!(
                side.read().get(key).is_some(),
                "side lost k{key} whose closure ran"
            );
        }
    }
    let acked = ledger.acked.lock().len();
    *tally.entry("acked").or_default() += acked;
    *tally.entry("ran").or_default() += ran.len();
    drop(side);
    drop(ran);
    ledger.ran.lock().clear();
    ledger.acked.lock().clear();
    drop(main);
    drop(other);
    let reopened: Db<Root> = options.open(&path).unwrap();
    assert_eq!(content(&reopened), main_keys);
    reopened.verify_typed().unwrap();
    tally
}

#[test]
fn awaited_and_blocking_writes_survive_forks_drops_merges_and_compaction() {
    let manual = Reclaim::off();
    let background = Reclaim::every(1 << 15);
    let kinds = [
        Options::new()
            .durability(Durability::Buffered)
            .reclaim(manual),
        Options::new()
            .durability(Durability::Buffered)
            .coalesce(true)
            .reclaim(background),
        Options::new().reclaim(background),
        Options::new().coalesce(true).reclaim(manual),
    ];
    let mut tally: BTreeMap<&'static str, usize> = BTreeMap::new();
    for seed in 0..common::cases(6) as u64 {
        for options in &kinds {
            for (what, seen) in run(options.clone(), seed) {
                *tally.entry(what).or_default() += seen;
            }
        }
    }
    for what in ["fork", "drop", "merge", "merge back", "compact"] {
        assert!(
            tally.get(what).is_some_and(|seen| *seen > 0),
            "{what}: {tally:?}"
        );
    }
    assert!(tally["ran"] > tally["acked"], "{tally:?}");
}
