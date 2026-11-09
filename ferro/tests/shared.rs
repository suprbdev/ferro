#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

mod common;

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::task::{Context, Waker};
use std::thread;
use std::time::{Duration, Instant};

use common::{Rng, Spread, cases, fast, padded_links, spread_into, spread_of};
use ferro::db::{Clock, Hook};
use ferro::{Data, Db, Durability, Error, Keep, Map, Options, Reclaim, Snapshot};

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    n: u64,
    order: Map<u64, (u64, u64)>,
    last: Map<u64, u64>,
}

fn shared() -> Options {
    fast().shared(true)
}

fn bump(db: &Db<Root>, writer: u64, i: u64) -> u64 {
    db.update(|root| {
        root.n += 1;
        root.order.insert(root.n, (writer, i));
        root.last.insert(writer, i);
        root.n
    })
    .unwrap()
}

fn check_chain(root: &Root, writers: u64, each: u64) {
    assert_eq!(root.n, writers * each);
    assert_eq!(root.order.len() as u64, root.n);
    let mut next = vec![1u64; writers as usize];
    for n in 1..=root.n {
        let (writer, i) = *root.order.get(&n).unwrap();
        assert_eq!(
            i, next[writer as usize],
            "writer {writer} out of order at {n}"
        );
        next[writer as usize] += 1;
    }
    for writer in 0..writers {
        assert_eq!(*root.last.get(&writer).unwrap(), each);
    }
}

#[test]
fn two_handles_on_one_file_commit_on_top_of_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.ferro");
    let a: Db<Root> = shared().open(&path).unwrap();
    let b: Db<Root> = shared().open(&path).unwrap();
    for i in 1..=50 {
        assert_eq!(bump(&a, 0, i), 2 * i - 1);
        assert_eq!(bump(&b, 1, i), 2 * i);
    }
    assert!(a.refresh().unwrap());
    assert_eq!(a.read().n, 100);
    assert_eq!(a.version(), 100);
    check_chain(&a.read(), 2, 50);
    drop((a, b));
    let db: Db<Root> = Options::new().open(&path).unwrap();
    check_chain(&db.read(), 2, 50);
    db.verify().unwrap();
}

#[test]
fn a_writer_without_the_option_is_refused_while_shared_writers_are_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.ferro");
    let a: Db<Root> = shared().open(&path).unwrap();
    assert!(matches!(fast().open::<Root>(&path), Err(Error::Locked)));
    drop(a);
    let classic: Db<Root> = fast().open(&path).unwrap();
    assert!(matches!(shared().open::<Root>(&path), Err(Error::Locked)));
    drop(classic);
}

#[test]
fn threads_on_several_handles_lose_nothing() {
    for durability in [Durability::Buffered, Durability::Fsync] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("threads.ferro");
        let handles = 4u64;
        let each = 200u64;
        let barrier = Arc::new(Barrier::new(handles as usize));
        let threads: Vec<_> = (0..handles)
            .map(|writer| {
                let path = path.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    let db: Db<Root> = shared().durability(durability).open(&path).unwrap();
                    barrier.wait();
                    for i in 1..=each {
                        bump(&db, writer, i);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let db: Db<Root> = Options::new().open(&path).unwrap();
        check_chain(&db.read(), handles, each);
        assert_eq!(db.version(), handles * each);
        db.verify().unwrap();
    }
}

type Other = Arc<Mutex<Option<(Db<Spread>, BTreeMap<u64, String>, Rng)>>>;

#[test]
fn a_draft_moves_over_the_frames_another_handle_placed_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("moves.ferro");
    let armed = Arc::new(AtomicBool::new(false));
    let other: Other = Arc::default();
    let hook = {
        let (armed, other) = (armed.clone(), other.clone());
        move |step: &'static str| {
            if step != "drafted" || !armed.swap(false, Ordering::SeqCst) {
                return;
            }
            let mut other = other.lock().unwrap();
            let (db, model, rng) = other.as_mut().expect("the other handle is ready");
            let frames = 1 + rng.below(3);
            thread::scope(|scope| {
                scope
                    .spawn(|| {
                        for _ in 0..frames {
                            spread_into(db, model, rng, 1);
                        }
                    })
                    .join()
                    .unwrap();
            });
        }
    };
    let a: Db<Spread> = shared().hook(Hook(Arc::new(hook))).open(&path).unwrap();
    a.fork("other").unwrap();
    let b: Db<Spread> = shared().open(&path).unwrap();
    *other.lock().unwrap() = Some((b.branch("other").unwrap(), BTreeMap::new(), Rng::new(31)));
    let (mut model, mut rng) = (BTreeMap::new(), Rng::new(8));
    for round in 0..cases(40) {
        armed.store(true, Ordering::SeqCst);
        spread_into(&a, &mut model, &mut rng, 0);
        assert!(!armed.load(Ordering::SeqCst), "round {round} moved nothing");
    }
    let (b_other, other_model, _) = other.lock().unwrap().take().unwrap();
    assert_eq!(spread_of(&a), model);
    assert_eq!(spread_of(&b_other), other_model);
    a.verify().unwrap();
    drop((a, b_other, b));
    assert!(padded_links(&std::fs::read(&path).unwrap()) > 0);
    let db: Db<Spread> = Options::new().open(&path).unwrap();
    db.verify().unwrap();
    assert_eq!(spread_of(&db), model);
    assert_eq!(spread_of(&db.branch("other").unwrap()), other_model);
}

#[test]
fn branches_forked_in_one_handle_are_seen_and_written_by_another() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forks.ferro");
    let a: Db<Root> = shared().open(&path).unwrap();
    let b: Db<Root> = shared().open(&path).unwrap();
    bump(&a, 0, 1);
    let side = a.fork("side").unwrap();
    bump(&side, 1, 1);
    let theirs = b.branch("side").unwrap();
    assert_eq!(theirs.read().n, 2);
    bump(&theirs, 1, 2);
    bump(&side, 1, 3);
    assert_eq!(side.read().n, 4);
    assert!(matches!(b.fork("side"), Err(Error::BranchExists(_))));
    b.drop_branch("side").unwrap();
    assert!(matches!(
        side.update(|root| root.n += 1),
        Err(Error::NoBranch(_))
    ));
    assert!(a.refresh().unwrap());
    assert_eq!(a.branches(), vec!["main".to_string()]);
}

#[test]
fn a_cycle_in_one_handle_is_followed_by_another_that_keeps_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cycle.ferro");
    let manual = Reclaim { every_bytes: 0 };
    let a: Db<Root> = shared()
        .history(Keep::Last(5))
        .reclaim(manual)
        .open(&path)
        .unwrap();
    let b: Db<Root> = shared()
        .history(Keep::Last(5))
        .reclaim(manual)
        .open(&path)
        .unwrap();
    for i in 1..=20 {
        bump(&a, 0, i);
        bump(&b, 1, i);
    }
    a.compact(Keep::Last(5)).unwrap();
    for i in 21..=40 {
        bump(&b, 1, i);
        bump(&a, 0, i);
    }
    b.compact(Keep::Last(5)).unwrap();
    for i in 41..=60 {
        bump(&a, 0, i);
        bump(&b, 1, i);
    }
    a.refresh().unwrap();
    check_chain(&a.read(), 2, 60);
    a.verify().unwrap();
    drop((a, b));
    let db: Db<Root> = Options::new().open(&path).unwrap();
    check_chain(&db.read(), 2, 60);
    db.verify().unwrap();
}

type Counts = Map<u32, i64>;

fn count(db: &Db<Counts>, key: u32) {
    db.commit(format!("k{key}"), |root| {
        root.insert(key, 1);
    })
    .unwrap();
}

fn add_counts(
    root: &mut Counts,
    base: &Snapshot<Counts>,
    theirs: &Snapshot<Counts>,
) -> (&'static str, ()) {
    let mut keys: Vec<u32> = base.keys().chain(theirs.keys()).collect();
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        let delta = theirs.get(&key).map_or(0, |n| *n) - base.get(&key).map_or(0, |n| *n);
        if delta != 0 {
            let next = root.get(&key).map_or(0, |n| *n) + delta;
            root.insert(key, next);
        }
    }
    ("merge", ())
}

fn counts(db: &Db<Counts>) -> Vec<(u32, i64)> {
    db.read().iter().map(|(key, n)| (key, *n)).collect()
}

fn once(keys: impl IntoIterator<Item = u32>) -> Vec<(u32, i64)> {
    keys.into_iter().map(|key| (key, 1)).collect()
}

fn bases(found: ferro::Result<Vec<Snapshot<Counts>>>) -> Vec<String> {
    let found = found.unwrap();
    found.iter().map(|s| s.commit().message.clone()).collect()
}

fn ticking() -> Clock {
    let ticks = Arc::new(AtomicU64::new(1_700_000_000_000));
    Clock(Arc::new(move || ticks.fetch_add(1, Ordering::Relaxed)))
}

fn counted(path: &Path, keep: Keep, clock: &Clock) -> Db<Counts> {
    shared()
        .history(keep)
        .reclaim(Reclaim { every_bytes: 0 })
        .clock(clock.clone())
        .open(path)
        .unwrap()
}

fn checked(db: &Db<Counts>) {
    db.verify().unwrap();
    db.verify_typed().unwrap();
}

#[test]
fn a_merge_after_another_handle_pruned_the_other_branch_links_the_state_it_merged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pruned.ferro");
    let clock = ticking();
    let a = counted(&path, Keep::Last(2), &clock);
    count(&a, 1);
    let side = a.fork("side").unwrap();
    count(&side, 2);
    count(&a, 3);
    let b = counted(&path, Keep::Last(2), &clock);
    let theirs = b.branch("side").unwrap();
    count(&theirs, 4);
    count(&theirs, 5);
    let mut seen = Vec::new();
    a.merge_with(&side, |root, base, theirs| {
        seen.push((base.version(), theirs.version()));
        add_counts(root, base, theirs)
    })
    .unwrap();
    assert_eq!(seen, [(1, 2)]);
    assert_eq!(counts(&a), once([1, 2, 3]));
    a.refresh().unwrap();
    assert_eq!((side.version(), side.log().first().unwrap().0), (4, 3));
    assert_eq!(bases(a.common_ancestors(&side)), ["k2"]);

    a.compact(Keep::Last(2)).unwrap();
    checked(&a);
    assert_eq!(bases(a.common_ancestors(&side)), ["k2"]);
    assert_eq!(bases(side.common_ancestors(&a)), ["k2"]);
    let log: Vec<String> = a.log().values().map(|c| c.message.clone()).collect();
    assert_eq!(log, ["k3", "merge"]);
    let sizes: Vec<(u64, usize)> = a
        .history_of(|root| root.len())
        .map(|(commit, len)| (commit.version, len))
        .collect();
    assert_eq!(sizes, [(3, 3), (2, 2)]);

    seen.clear();
    a.merge_with(&side, |root, base, theirs| {
        seen.push((base.version(), theirs.version()));
        add_counts(root, base, theirs)
    })
    .unwrap();
    assert_eq!(seen, [(2, 4)]);
    assert_eq!(counts(&a), once(1..=5));
    theirs
        .merge_with(&b.branch("main").unwrap(), add_counts)
        .unwrap();
    assert_eq!(counts(&theirs), once(1..=5));
    b.compact(Keep::Last(2)).unwrap();
    a.refresh().unwrap();
    checked(&a);
    checked(&b);
}

fn merged_back_and_pruned(keep: Keep) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("back.ferro");
    let clock = ticking();
    let a = counted(&path, keep, &clock);
    count(&a, 1);
    let side = a.fork("side").unwrap();
    count(&side, 2);
    count(&a, 3);
    count(&side, 4);
    let b = counted(&path, keep, &clock);
    let theirs = b.branch("side").unwrap();
    theirs
        .merge_with(&b.branch("main").unwrap(), add_counts)
        .unwrap();
    count(&theirs, 5);
    count(&theirs, 6);
    assert_eq!(counts(&theirs), once(1..=6));
    let mut seen = Vec::new();
    a.merge_with(&side, |root, base, theirs| {
        seen.push((base.version(), theirs.version()));
        add_counts(root, base, theirs)
    })
    .unwrap();
    assert_eq!(seen, [(1, 3)], "{keep:?}");
    assert_eq!(counts(&a), once(1..=4), "{keep:?}");
    a.refresh().unwrap();
    assert_eq!(side.version(), 6);
    a.merge_with(&side, add_counts).unwrap();
    assert_eq!(counts(&a), once(1..=6), "{keep:?}");

    a.compact(keep).unwrap();
    checked(&a);
    let head = a.head_hash();
    a.merge_with(&side, add_counts).unwrap();
    assert_eq!(a.head_hash(), head, "{keep:?}");
    side.merge_with(&a, add_counts).unwrap();
    assert_eq!(counts(&side), once(1..=6), "{keep:?}");
    checked(&a);
}

#[test]
fn a_merge_after_another_handle_merged_back_and_pruned_applies_each_change_once() {
    for keep in [Keep::All, Keep::Last(3)] {
        merged_back_and_pruned(keep);
    }
}

fn criss_cross_over_a_pull_record(race: bool) -> ferro::Result<Vec<(u32, i64)>> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.ferro");
    let clock = ticking();
    let a = counted(&path, Keep::All, &clock);
    let remote: Db<Counts> = fast()
        .clock(clock.clone())
        .open(dir.path().join("r.ferro"))
        .unwrap();
    count(&remote, 10);
    let side = a.fork("side").unwrap();
    count(&side, 1);
    assert_eq!(
        a.pull(&remote, "g", add_counts).unwrap().outcome,
        ferro::Outcome::FastForward
    );
    assert_eq!(
        side.pull(&remote, "g", add_counts).unwrap().outcome,
        ferro::Outcome::Merged
    );
    let b = counted(&path, Keep::Last(2), &clock);
    let theirs = b.branch("side").unwrap();
    count(&theirs, 2);
    count(&theirs, 3);
    a.refresh().unwrap();
    count(&a, 4);
    b.refresh().unwrap();
    theirs
        .merge_with(&b.branch("main").unwrap(), add_counts)
        .unwrap();
    a.merge_with(&side, add_counts).unwrap();
    a.refresh().unwrap();
    assert_eq!(bases(a.common_ancestors(&side)), ["k3", "k4"]);
    if race {
        count(&theirs, 5);
    }
    let mut runs = Vec::new();
    a.merge_with(&side, |root, base, theirs| {
        runs.push((base.version(), theirs.version()));
        add_counts(root, base, theirs)
    })?;
    assert_eq!(runs, [(1, 2), (0, 5)]);
    let merged = counts(&a);
    a.refresh().unwrap();
    a.merge_with(&side, add_counts).unwrap();
    a.compact(Keep::Last(2)).unwrap();
    checked(&a);
    let mut all = once([1, 2, 3, 4, 10]);
    if race {
        all.insert(4, (5, 1));
    }
    assert_eq!(counts(&a), all);
    Ok(merged)
}

#[test]
fn a_criss_cross_over_a_pull_record_merges_while_another_handle_prunes_the_other_branch() {
    let all = once([1, 2, 3, 4, 10]);
    assert_eq!(criss_cross_over_a_pull_record(false).unwrap(), all);
    assert_eq!(criss_cross_over_a_pull_record(true).unwrap(), all);
}

struct Unpark(thread::Thread);

impl std::task::Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    loop {
        if let std::task::Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        thread::park();
    }
}

#[test]
fn watchers_fire_for_another_handles_commits_on_commit_and_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watch.ferro");
    let a: Db<Root> = shared().open(&path).unwrap();
    let b: Db<Root> = shared().open(&path).unwrap();
    let side = a.fork("side").unwrap();
    let watcher = a.watch();
    bump(&b, 1, 1);
    assert!(watcher.try_recv().is_err());
    assert!(a.refresh().unwrap());
    assert_eq!(watcher.try_recv().unwrap().n, 1);
    bump(&b, 1, 2);
    side.update(|root| root.n += 100).unwrap();
    assert_eq!(watcher.try_recv().unwrap().n, 2);
    assert_eq!(a.read().n, 2);
    assert!(!a.refresh().unwrap());
}

#[test]
fn coalesced_and_awaited_writes_from_several_handles_lose_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.ferro");
    let each = 150u64;
    let threads: Vec<_> = (0..4u64)
        .map(|writer| {
            let path = path.clone();
            thread::spawn(move || {
                let options = shared().coalesce(writer % 2 == 0);
                let db: Db<Root> = options.open(&path).unwrap();
                let inner: Vec<_> = (0..2u64)
                    .map(|half| {
                        let db = db.clone();
                        thread::spawn(move || {
                            for i in 1..=each {
                                let tag = writer * 2 + half;
                                let write = |root: &mut Root| {
                                    root.n += 1;
                                    root.order.insert(root.n, (tag, i));
                                    root.last.insert(tag, i);
                                };
                                match half {
                                    0 => db.update(write).unwrap(),
                                    _ => block_on(db.update_async(write)).unwrap(),
                                }
                            }
                        })
                    })
                    .collect();
                for handle in inner {
                    handle.join().unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let db: Db<Root> = Options::new().open(&path).unwrap();
    check_chain(&db.read(), 8, each);
    db.verify().unwrap();
}

#[test]
fn an_awaited_write_waits_off_thread_while_another_handle_holds_its_branch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("await.ferro");
    let a: Db<Root> = shared().open(&path).unwrap();
    let b: Db<Root> = shared().open(&path).unwrap();
    let (entered, release) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let holder = {
        let (entered, release) = (entered.clone(), release.clone());
        thread::spawn(move || {
            b.update(|root| {
                entered.wait();
                release.wait();
                root.n += 1;
            })
            .unwrap();
        })
    };
    entered.wait();
    let mut future = std::pin::pin!(a.update_async(|root| root.n += 1));
    let waker = std::task::Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    assert!(future.as_mut().poll(&mut cx).is_pending());
    release.wait();
    holder.join().unwrap();
    loop {
        if future.as_mut().poll(&mut cx).is_ready() {
            break;
        }
        thread::park_timeout(std::time::Duration::from_millis(50));
    }
    assert_eq!(a.read().n, 2);
}

#[test]
fn a_pull_into_a_shared_file_lands_beside_another_handles_commits() {
    let dir = tempfile::tempdir().unwrap();
    let source: Db<Root> = fast().open(dir.path().join("source.ferro")).unwrap();
    for i in 1..=30 {
        bump(&source, 9, i);
    }
    let path = dir.path().join("pulled.ferro");
    let a: Db<Root> = shared().open(&path).unwrap();
    let b: Db<Root> = shared().open(&path).unwrap();
    let other = b.fork("other").unwrap();
    let writer = thread::spawn(move || {
        for i in 1..=200 {
            bump(&other, 1, i);
        }
        other
    });
    a.pull(&source, "origin", |_, _, _| ("merge", ())).unwrap();
    let other = writer.join().unwrap();
    assert_eq!(a.read().n, 30);
    assert_eq!(other.read().n, 200);
    let origin = b.branch("origin").unwrap();
    assert_eq!(origin.read().n, 30);
    a.verify().unwrap();
}

#[test]
fn keep_last_prunes_a_pull_into_a_shared_file_for_every_handle() {
    let dir = tempfile::tempdir().unwrap();
    let source: Db<Root> = fast().open(dir.path().join("source.ferro")).unwrap();
    for i in 1..=20 {
        bump(&source, 9, i);
    }
    let path = dir.path().join("pulled.ferro");
    let options = || {
        shared()
            .history(Keep::Last(2))
            .reclaim(Reclaim { every_bytes: 0 })
    };
    let a: Db<Root> = options().open(&path).unwrap();
    let b: Db<Root> = options().open(&path).unwrap();
    let lens = |db: &Db<Root>| (db.log().len(), db.branch("origin").unwrap().log().len());
    let pulled = a.pull(&source, "origin", |_, _, _| ("merge", ())).unwrap();
    assert_eq!(pulled.outcome, ferro::Outcome::FastForward);
    assert_eq!(lens(&a), (2, 2));
    b.refresh().unwrap();
    assert_eq!(b.read().n, 20);
    assert_eq!(lens(&b), (2, 2));

    bump(&b, 1, 1);
    for i in 21..=25 {
        bump(&source, 9, i);
    }
    let pulled = a.pull(&source, "origin", |_, _, _| ("merge", ())).unwrap();
    assert_eq!(pulled.outcome, ferro::Outcome::Merged);
    assert_eq!(lens(&a), (2, 2));
    b.refresh().unwrap();
    assert_eq!(lens(&b), (2, 2));
    a.compact(Keep::Last(2)).unwrap();
    b.refresh().unwrap();
    for db in [&a, &b] {
        db.verify().unwrap();
        db.verify_typed().unwrap();
    }
}

#[test]
fn background_cycles_in_several_handles_run_one_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("background.ferro");
    let reclaim = Reclaim {
        every_bytes: 256 << 10,
    };
    let each = 400u64;
    let threads: Vec<_> = (0..3u64)
        .map(|writer| {
            let path = path.clone();
            thread::spawn(move || {
                let options = shared().history(Keep::Last(3)).reclaim(reclaim);
                let db: Db<Root> = options.open(&path).unwrap();
                for i in 1..=each {
                    bump(&db, writer, i);
                }
                db.space().unwrap().cycles
            })
        })
        .collect();
    let cycles: u64 = threads.into_iter().map(|t| t.join().unwrap()).sum();
    assert!(cycles > 0);
    let db: Db<Root> = Options::new().open(&path).unwrap();
    check_chain(&db.read(), 3, each);
    db.verify().unwrap();
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Padded {
    n: u64,
    pad: Vec<u64>,
}

#[test]
fn threads_of_two_handles_on_branches_of_their_own_place_in_turn() {
    for _ in 0..common::cases(3) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tip.ferro");
        let p: Db<Padded> = shared().open(&path).unwrap();
        let q: Db<Padded> = shared().open(&path).unwrap();
        let mut branches = Vec::new();
        for (h, db) in [&p, &q].into_iter().enumerate() {
            for t in 0..4 {
                let name = format!("b{h}{t}");
                branches.push(db.fork(&name).unwrap());
            }
        }
        let each = 300u64;
        let barrier = Arc::new(Barrier::new(branches.len()));
        let threads: Vec<_> = branches
            .into_iter()
            .map(|db| {
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    for i in 1..=each {
                        db.update(|root| {
                            root.n += 1;
                            root.pad = vec![i; (i % 7) as usize * 50];
                        })
                        .unwrap();
                    }
                    db.branch_name().to_string()
                })
            })
            .collect();
        let names: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        drop((p, q));
        let db: Db<Padded> = Options::new().open(&path).unwrap();
        for name in &names {
            assert_eq!(db.branch(name).unwrap().read().n, each, "{name}");
        }
        db.verify().unwrap();
    }
}

fn hooked(steps: &[&'static str]) -> (Hook, mpsc::Receiver<&'static str>, Arc<Barrier>) {
    let (sender, receiver) = mpsc::channel();
    let sender = parking_lot::Mutex::new(sender);
    let resume = Arc::new(Barrier::new(2));
    let steps: Vec<&'static str> = steps.to_vec();
    let paused = resume.clone();
    let hook = Hook(Arc::new(move |step| {
        if steps.contains(&step) {
            let _ = sender.lock().send(step);
        }
        if step == "paused" {
            paused.wait();
        }
    }));
    (hook, receiver, resume)
}

#[test]
fn an_awaited_writes_wait_never_opens_a_branch_inside_a_cycles_pause() {
    for _ in 0..common::cases(4) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hole.ferro");
        let manual = Reclaim { every_bytes: 0 };
        let (hook, steps, resume) = hooked(&["synced", "paused"]);
        let a: Db<Root> = shared().reclaim(manual).hook(hook).open(&path).unwrap();
        let b: Db<Root> = shared().reclaim(manual).open(&path).unwrap();
        for i in 1..=3 {
            bump(&a, 0, i);
        }
        let (entered, release) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
        let holder = {
            let (b, entered, release) = (b.clone(), entered.clone(), release.clone());
            thread::spawn(move || {
                b.update(|root| {
                    entered.wait();
                    release.wait();
                    root.n += 1;
                })
                .unwrap();
            })
        };
        entered.wait();
        let mut future = Box::pin(a.update_async(|root| root.n += 1));
        let waker = Waker::from(Arc::new(Unpark(thread::current())));
        let mut cx = Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
        let cycler = {
            let a = a.clone();
            thread::spawn(move || a.compact(Keep::All))
        };
        assert_eq!(steps.recv().unwrap(), "synced");
        thread::sleep(Duration::from_millis(50));
        release.wait();
        holder.join().unwrap();
        assert_eq!(steps.recv().unwrap(), "paused");
        let ran = Arc::new(AtomicBool::new(false));
        let intruder = {
            let (b, ran) = (b.clone(), ran.clone());
            thread::spawn(move || {
                b.update(|root| {
                    ran.store(true, Ordering::Release);
                    root.n += 1;
                })
            })
        };
        thread::sleep(Duration::from_millis(100));
        let inside = ran.load(Ordering::Acquire);
        resume.wait();
        cycler.join().unwrap().unwrap();
        let intruded = intruder.join().unwrap();
        while future.as_mut().poll(&mut cx).is_pending() {
            thread::park_timeout(Duration::from_millis(20));
        }
        assert!(!inside, "a closure ran inside the pause: {intruded:?}");
        intruded.unwrap();
    }
}

#[test]
fn a_branch_attached_while_a_followed_frame_waits_for_its_sync_reads_the_newest_head() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale.ferro");
    let armed = Arc::new(AtomicBool::new(false));
    let (at_hook, release) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let hook = {
        let (armed, at_hook, release) = (armed.clone(), at_hook.clone(), release.clone());
        Hook(Arc::new(move |step| {
            if step == "drafted" && armed.swap(false, Ordering::AcqRel) {
                at_hook.wait();
                release.wait();
            }
        }))
    };
    let a: Db<Root> = shared().hook(hook).open(&path).unwrap();
    let b: Db<Root> = shared().open(&path).unwrap();
    a.update(|root| root.n += 1).unwrap();
    let side_b = b.fork("side").unwrap();
    side_b.update(|root| root.n = 10).unwrap();
    a.refresh().unwrap();
    side_b.update(|root| root.n = 20).unwrap();
    armed.store(true, Ordering::Release);
    let writer = {
        let a = a.clone();
        thread::spawn(move || a.update(|root| root.n += 1).unwrap())
    };
    at_hook.wait();
    let side_a = a.branch("side").unwrap();
    release.wait();
    writer.join().unwrap();
    assert_eq!(side_a.read().n, 20);
    side_a.update(|root| root.n += 1).unwrap();
    side_a.update(|root| root.n += 1).unwrap();
    assert_eq!(side_a.read().n, 22);
}

#[test]
fn a_drop_and_fork_in_another_handle_wait_for_a_closure_on_that_branch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refork.ferro");
    let a: Db<Root> = shared().open(&path).unwrap();
    let b: Db<Root> = shared().open(&path).unwrap();
    bump(&a, 0, 1);
    let side = a.fork("side").unwrap();
    bump(&side, 1, 1);
    let (entered, release) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let closure = {
        let (entered, release) = (entered.clone(), release.clone());
        thread::spawn(move || {
            side.update(|root| {
                entered.wait();
                release.wait();
                root.n += 1;
            })
        })
    };
    entered.wait();
    let dropped = Arc::new(AtomicBool::new(false));
    let dropper = {
        let (b, dropped) = (b.clone(), dropped.clone());
        thread::spawn(move || {
            b.drop_branch("side").unwrap();
            dropped.store(true, Ordering::Release);
            b.fork("side").unwrap();
        })
    };
    thread::sleep(Duration::from_millis(100));
    let early = dropped.load(Ordering::Acquire);
    release.wait();
    let committed = closure.join().unwrap();
    dropper.join().unwrap();
    assert!(
        !early,
        "the drop did not wait for the closure: {committed:?}"
    );
    assert!(
        matches!(committed, Ok(()) | Err(Error::NoBranch(_))),
        "{committed:?}"
    );
    assert_eq!(b.branch("side").unwrap().read().n, 1);
}

#[test]
fn a_branch_another_handle_dropped_and_forked_again_leaves_the_old_handles_dead() {
    let manual = Reclaim { every_bytes: 0 };
    for (own_commits, cycle) in [(true, false), (false, false), (true, true), (false, true)] {
        let case = format!("own commits {own_commits}, cycle {cycle}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refork.ferro");
        let a: Db<Root> = shared().reclaim(manual).open(&path).unwrap();
        bump(&a, 0, 1);
        let old = a.fork("x").unwrap();
        if own_commits {
            for i in 1..=3 {
                bump(&old, 1, i);
            }
        }
        let b: Db<Root> = shared().reclaim(manual).open(&path).unwrap();
        let held = b.branch("x").unwrap();
        let had = (held.version(), held.read().n);
        let watcher = held.watch();
        if cycle {
            a.compact(Keep::All).unwrap();
            b.refresh().unwrap();
            assert!(
                matches!(watcher.try_recv(), Err(mpsc::TryRecvError::Empty)),
                "{case}: a cycle alone ended the watcher"
            );
        }
        drop(old);
        a.drop_branch("x").unwrap();
        let new = a.fork("x").unwrap();
        bump(&new, 2, 1);
        if cycle {
            a.compact(Keep::All).unwrap();
        }
        let now = (new.version(), new.read().n);

        let wrote = held.update(|root| root.n += 100);
        assert!(
            matches!(wrote, Err(Error::NoBranch(_))),
            "{case}: {wrote:?}"
        );
        assert_eq!((held.version(), held.read().n), had, "{case}");
        assert!(
            matches!(
                watcher.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ),
            "{case}: the old branch's watcher did not end"
        );
        a.refresh().unwrap();
        assert_eq!((new.version(), new.read().n), now, "{case}");
        let attached = b.branch("x").unwrap();
        assert_eq!((attached.version(), attached.read().n), now, "{case}");
        attached.update(|root| root.n += 1).unwrap();
        a.refresh().unwrap();
        assert_eq!(new.read().n, now.1 + 1, "{case}");
    }
}

#[test]
fn a_prune_another_handle_writes_moves_no_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prune.ferro");
    let manual = Reclaim { every_bytes: 0 };
    let other: Arc<std::sync::OnceLock<Db<Root>>> = Arc::default();
    let hook = {
        let other = other.clone();
        Hook(Arc::new(move |step| {
            if step == "created"
                && let Some(b) = other.get()
            {
                b.refresh().unwrap();
            }
        }))
    };
    let a: Db<Root> = shared().reclaim(manual).hook(hook).open(&path).unwrap();
    let b: Db<Root> = shared().reclaim(manual).open(&path).unwrap();
    for i in 1..=5 {
        bump(&a, 0, i);
    }
    b.refresh().unwrap();
    let watcher = b.watch();
    let changes = b.watch_changes(|old, new| old.n != new.n);
    other.set(b.clone()).ok();
    a.compact(Keep::Last(2)).unwrap();
    b.refresh().unwrap();
    assert!(matches!(watcher.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(matches!(changes.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert_eq!(b.version(), 5);
    assert_eq!(b.log().len(), 2);
    bump(&a, 0, 6);
    b.refresh().unwrap();
    assert_eq!(watcher.try_recv().unwrap().version(), 6);
}

#[test]
fn branch_after_another_handle_dropped_and_forked_again_gives_the_new_branch() {
    let manual = Reclaim { every_bytes: 0 };
    for refork in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rebranch.ferro");
        let a: Db<Root> = shared().reclaim(manual).open(&path).unwrap();
        bump(&a, 0, 1);
        let old = a.fork("x").unwrap();
        bump(&old, 1, 1);
        let b: Db<Root> = shared().reclaim(manual).open(&path).unwrap();
        let held = b.branch("x").unwrap();
        let had = (held.version(), held.read().n);
        drop(old);
        a.drop_branch("x").unwrap();
        if !refork {
            assert!(
                matches!(b.branch("x"), Err(Error::NoBranch(_))),
                "refork {refork}"
            );
            assert!(matches!(
                held.update(|root| root.n += 100),
                Err(Error::NoBranch(_))
            ));
            continue;
        }
        let new = a.fork("x").unwrap();
        bump(&new, 2, 1);
        let now = (new.version(), new.read().n);
        let attached = b.branch("x").unwrap();
        assert_eq!((attached.version(), attached.read().n), now);
        attached.update(|root| root.n += 1).unwrap();
        a.refresh().unwrap();
        assert_eq!(new.read().n, now.1 + 1);
        assert!(matches!(
            held.update(|root| root.n += 100),
            Err(Error::NoBranch(_))
        ));
        assert_eq!((held.version(), held.read().n), had);
    }
}

#[test]
fn a_writer_that_opens_during_another_handles_cycle_pause_writes_the_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pause.ferro");
    let manual = Reclaim { every_bytes: 0 };
    let armed = Arc::new(AtomicBool::new(false));
    let (paused, resume) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let hook = {
        let (armed, paused, resume) = (armed.clone(), paused.clone(), resume.clone());
        Hook(Arc::new(move |step| {
            if step == "paused" && armed.swap(false, Ordering::AcqRel) {
                paused.wait();
                resume.wait();
            }
        }))
    };
    let durable = shared().durability(Durability::Fsync).reclaim(manual);
    let a: Db<Root> = durable.clone().hook(hook).open(&path).unwrap();
    for i in 1..=10 {
        bump(&a, 0, i);
    }
    armed.store(true, Ordering::Release);
    let cycler = {
        let a = a.clone();
        thread::spawn(move || a.compact(Keep::All))
    };
    paused.wait();
    let opener = {
        let (path, durable) = (path.clone(), durable.clone());
        thread::spawn(move || durable.open::<Root>(&path).unwrap())
    };
    thread::sleep(Duration::from_millis(100));
    resume.wait();
    cycler.join().unwrap().unwrap();
    let b = opener.join().unwrap();
    let acked = bump(&b, 1, 1);
    assert_eq!(acked, 11);
    a.refresh().unwrap();
    assert_eq!(a.read().n, 11);
    drop((a, b));
    let db: Db<Root> = Options::new().reclaim(manual).open(&path).unwrap();
    assert_eq!(db.read().n, acked, "an acknowledged commit was lost");
    db.verify().unwrap();
}

#[test]
fn a_closure_on_a_branch_another_handle_dropped_never_runs() {
    for coalesce in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dropped.ferro");
        let a: Db<Root> = shared().coalesce(coalesce).open(&path).unwrap();
        let b: Db<Root> = shared().open(&path).unwrap();
        bump(&a, 0, 1);
        let side = a.fork("side").unwrap();
        b.drop_branch("side").unwrap();
        let ran = AtomicBool::new(false);
        let out = side.update(|root| {
            ran.store(true, Ordering::Release);
            root.n += 1;
        });
        assert!(matches!(out, Err(Error::NoBranch(_))), "{out:?}");
        assert!(!ran.load(Ordering::Acquire), "coalesce {coalesce}");
        a.compact(Keep::All).unwrap();
        bump(&a, 0, 2);
    }
}

#[test]
fn a_marker_left_by_a_dead_cycle_never_keeps_a_stranded_frame_after_a_power_cut() {
    use std::os::unix::fs::FileExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kept.ferro");
    let len = || std::fs::metadata(&path).unwrap().len();
    let manual = Reclaim { every_bytes: 0 };
    let commit = |db: &Db<Padded>, fill: u64| {
        db.update(|root| {
            root.n += 1;
            root.pad = vec![fill; 20];
        })
        .unwrap()
    };
    {
        let db: Db<Padded> = shared().reclaim(manual).open(&path).unwrap();
        for _ in 0..5 {
            commit(&db, 7);
        }
    }
    let end = len();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut marker = [0u8; 68];
    marker[..8].copy_from_slice(b"FERROGEN");
    file.write_all_at(&marker, end).unwrap();
    let db: Db<Padded> = shared().reclaim(manual).open(&path).unwrap();
    let mut slots = vec![0u8; 8192];
    file.read_exact_at(&mut slots, 0).unwrap();
    commit(&db, 7);
    commit(&db, 7);
    drop(db);
    file.write_all_at(&marker, end).unwrap();
    file.write_all_at(&slots, 0).unwrap();
    let db: Db<Padded> = shared().reclaim(manual).open(&path).unwrap();
    assert_eq!(db.read().n, 5);
    commit(&db, 8);
    drop(db);
    let db: Db<Padded> = Options::new().reclaim(manual).open(&path).unwrap();
    assert_eq!(db.read().n, 6, "a stranded frame lined up with the log");
    db.verify().unwrap();
}

enum Left {
    Pages(u64, bool),
    Flipped(u64),
}

fn left_by_a_dead_writer(left: Left) {
    use std::os::unix::fs::FileExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dead.ferro");
    let manual = Reclaim { every_bytes: 0 };
    let mut rng = common::Rng::new(7);
    let mut commit = |db: &Db<Padded>| {
        let pad: Vec<u64> = (0..3000).map(|_| rng.next()).collect();
        db.update(|root| {
            root.n += 1;
            root.pad = pad;
            root.n
        })
    };
    let live: Db<Padded> = shared().reclaim(manual).open(&path).unwrap();
    for _ in 0..3 {
        commit(&live).unwrap();
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut slots = vec![0u8; 8192];
    file.read_exact_at(&mut slots, 0).unwrap();
    let start = live.size_on_disk();
    let dead: Db<Padded> = shared().reclaim(manual).open(&path).unwrap();
    commit(&dead).unwrap();
    let end = dead.size_on_disk();
    drop(dead);
    assert!(end - start > 4 * 4096, "a frame of {} bytes", end - start);
    let mut frame = vec![0u8; (end - start) as usize];
    file.read_exact_at(&mut frame, start).unwrap();
    file.write_all_at(&slots, 0).unwrap();
    let flipped = match left {
        Left::Pages(pages, zeroes) => {
            let cut = match pages {
                0 => start,
                _ => start.next_multiple_of(4096) + (pages - 1) * 4096,
            };
            assert!(cut < end);
            file.set_len(cut).unwrap();
            if zeroes {
                file.set_len(end + 4096).unwrap();
            }
            file.write_all_at(&frame[..(cut - start) as usize], start)
                .unwrap();
            None
        }
        Left::Flipped(offset) => {
            let at = start + offset;
            frame[offset as usize] ^= 0x08;
            file.write_all_at(&frame, start).unwrap();
            Some(at)
        }
    };
    let image = std::fs::read(&path).unwrap();
    if let Some(at) = flipped {
        let (at, start, end) = (at as usize, start as usize, end as usize);
        assert!(common::judged(&image, at, start, end));
    }
    let outcome = commit(&live);
    let Some(at) = flipped else {
        assert_eq!(
            outcome.unwrap(),
            4,
            "the next writer did not write over the half frame"
        );
        drop(live);
        let db: Db<Padded> = Options::new().reclaim(manual).open(&path).unwrap();
        assert_eq!(db.read().n, 4);
        db.verify().unwrap();
        return;
    };
    assert!(
        matches!(&outcome, Err(Error::Corrupt(message)) if message.ends_with(&format!("byte {at}"))),
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        image,
        "the damaged frame was written over"
    );
    drop(live);
    let reopened = shared().reclaim(manual).open::<Padded>(&path);
    assert!(
        matches!(reopened, Err(Error::Corrupt(_))),
        "{:?}",
        reopened.err()
    );
}

#[test]
fn a_frame_half_written_by_a_dead_writer_is_written_over_and_a_damaged_one_is_not() {
    for pages in 0..5 {
        for zeroes in [false, true] {
            left_by_a_dead_writer(Left::Pages(pages, zeroes));
        }
    }
    for offset in [3, 30, 70, 5000, 9000] {
        left_by_a_dead_writer(Left::Flipped(offset));
    }
}

#[derive(Clone, Copy, Debug)]
enum Rot {
    Block,
    Magic,
    Zeroed,
}

fn rotted(file: &std::fs::File, start: u64, rot: Rot) {
    use std::os::unix::fs::FileExt;
    let (at, len) = match rot {
        Rot::Block => (start + 68 + 200, 2),
        Rot::Magic => (start + 2, 2),
        Rot::Zeroed => (start, 84),
    };
    let mut bytes = vec![0u8; len];
    file.read_exact_at(&mut bytes, at).unwrap();
    match rot {
        Rot::Zeroed => bytes.fill(0),
        _ => bytes.iter_mut().for_each(|byte| *byte ^= 0x01),
    }
    file.write_all_at(&bytes, at).unwrap();
}

fn below_the_superblock(durability: Durability, rot: Rot) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lag.ferro");
    let manual = Reclaim { every_bytes: 0 };
    let mut rng = common::Rng::new(5);
    let pads: Vec<Vec<u64>> = (0..5)
        .map(|_| (0..600).map(|_| rng.next()).collect())
        .collect();
    let commit = |db: &Db<Padded>, pad: &[u64]| {
        db.update(|root| {
            root.n += 1;
            root.pad = pad.to_vec();
            root.n
        })
    };
    let open = || {
        shared()
            .durability(durability)
            .reclaim(manual)
            .open::<Padded>(&path)
            .unwrap()
    };
    let a = open();
    let b = open();
    let mut ends = vec![a.size_on_disk()];
    for pad in &pads[..4] {
        commit(&a, pad).unwrap();
        ends.push(a.size_on_disk());
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    rotted(&file, ends[1], rot);
    let image = std::fs::read(&path).unwrap();
    let context = format!("{durability:?}, {rot:?}");
    let outcome = commit(&b, &pads[4]);
    assert!(
        matches!(&outcome, Err(Error::Corrupt(message)) if message.contains(&format!("frame 2 at {} ", ends[1]))),
        "{context}: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        image,
        "{context}: the lagging writer changed the log a superblock names"
    );
    assert_eq!(commit(&a, &pads[4]).unwrap(), 5, "{context}");
    drop((a, b));
    let db: Db<Padded> = Options::new()
        .durability(durability)
        .reclaim(manual)
        .open(&path)
        .unwrap();
    assert_eq!(db.read().n, 5, "{context}: an acknowledged commit was lost");
}

#[test]
fn a_lagging_writer_reports_a_frame_below_the_superblock_that_does_not_check_and_writes_nothing() {
    for durability in [Durability::Fsync, Durability::Buffered] {
        for rot in [Rot::Block, Rot::Magic, Rot::Zeroed] {
            below_the_superblock(durability, rot);
        }
    }
}

#[test]
fn a_writer_that_opens_beside_a_live_one_reports_a_frame_below_the_superblock_that_does_not_check()
{
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("beside.ferro");
    let manual = Reclaim { every_bytes: 0 };
    let mut rng = common::Rng::new(9);
    let commit = |db: &Db<Padded>, rng: &mut common::Rng| {
        let pad: Vec<u64> = (0..600).map(|_| rng.next()).collect();
        db.update(|root| {
            root.n += 1;
            root.pad = pad;
            root.n
        })
    };
    let a: Db<Padded> = shared().reclaim(manual).open(&path).unwrap();
    let mut ends = vec![a.size_on_disk()];
    for _ in 0..4 {
        commit(&a, &mut rng).unwrap();
        ends.push(a.size_on_disk());
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    for rot in [Rot::Block, Rot::Magic, Rot::Zeroed] {
        let mut kept = vec![0u8; (ends[2] - ends[1]) as usize];
        std::os::unix::fs::FileExt::read_exact_at(&file, &mut kept, ends[1]).unwrap();
        rotted(&file, ends[1], rot);
        let image = std::fs::read(&path).unwrap();
        let opened = shared().reclaim(manual).open::<Padded>(&path);
        assert!(
            matches!(&opened, Err(Error::Corrupt(message)) if message.contains(&format!("frame 2 at {} ", ends[1]))),
            "{rot:?}: {:?}",
            opened.map(|db| db.read().n)
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            image,
            "{rot:?}: the opening writer changed the log a live writer's superblock names"
        );
        std::os::unix::fs::FileExt::write_all_at(&file, &kept, ends[1]).unwrap();
    }
    assert_eq!(commit(&a, &mut rng).unwrap(), 5);
    drop(a);
    let db: Db<Padded> = Options::new().reclaim(manual).open(&path).unwrap();
    assert_eq!(db.read().n, 5, "an acknowledged commit was lost");
    db.verify().unwrap();
}

#[test]
fn a_frame_written_where_a_dead_writer_left_a_near_copy_reads_as_torn_after_a_power_cut() {
    near_copy_left_by_a_dead_writer(false);
}

#[test]
fn a_frame_written_over_a_dead_spill_past_a_stale_marker_reads_as_torn_after_a_power_cut() {
    near_copy_left_by_a_dead_writer(true);
}

fn near_copy_left_by_a_dead_writer(marker: bool) {
    use ferro::store::{Op, Trace};
    use std::os::unix::fs::FileExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("near.ferro");
    let manual = Reclaim { every_bytes: 0 };
    let mut rng = common::Rng::new(11);
    let pads: Vec<Vec<u64>> = (0..4)
        .map(|_| (0..3000).map(|_| rng.next() | 1 << 63).collect())
        .collect();
    let commit = |db: &Db<Padded>, pad: &[u64]| {
        db.update(|root| {
            root.n += 1;
            root.pad = pad.to_vec();
        })
        .unwrap()
    };
    let trace: Trace = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let live: Db<Padded> = shared()
        .reclaim(manual)
        .trace(trace.clone())
        .open(&path)
        .unwrap();
    for pad in &pads[..3] {
        commit(&live, pad);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut slots = vec![0u8; 8192];
    file.read_exact_at(&mut slots, 0).unwrap();
    let start = live.size_on_disk();
    let dead: Db<Padded> = shared().reclaim(manual).open(&path).unwrap();
    commit(&dead, &pads[3]);
    let end = dead.size_on_disk();
    drop(dead);
    let mut left = vec![0u8; (end - start) as usize];
    file.read_exact_at(&mut left, start).unwrap();
    let mut near = pads[3].clone();
    near[1500] ^= 1;
    let cut = start.next_multiple_of(4096) + 4 * 4096;
    assert!(cut < end, "a frame of {} bytes", end - start);
    file.set_len(start).unwrap();
    file.write_all_at(&left[..(cut - start) as usize], start)
        .unwrap();
    if marker {
        let mut header = [0u8; 68];
        header[..8].copy_from_slice(b"FERROGEN");
        file.write_all_at(&header, start).unwrap();
    }
    file.write_all_at(&slots, 0).unwrap();
    let base = std::fs::read(&path).unwrap();
    trace.lock().clear();
    commit(&live, &near);
    let ops = trace.lock().clone();
    drop(live);
    let written = std::fs::read(&path).unwrap();
    let differ: Vec<u64> = (start..cut)
        .filter(|at| written[*at as usize] != base[*at as usize])
        .collect();
    let at = differ
        .iter()
        .copied()
        .find(|at| *at >= start + 68)
        .expect("the frames differ past their headers");
    let sector = at - at % 512;
    let bits: u32 = (sector..sector + 512)
        .map(|byte| (written[byte as usize] ^ base[byte as usize]).count_ones())
        .sum();
    assert_eq!(
        bits, 1,
        "the sector at {sector} differs from the dead writer's in one bit"
    );
    let frame = ops
        .iter()
        .position(|op| matches!(op, Op::Write(_, offset, _) if *offset == start))
        .expect("the frame is written where the dead writer's was");
    let cleared = ops[..frame]
        .iter()
        .position(|op| matches!(op, Op::SetLen(_, len) if *len == start));
    assert!(
        cleared.is_some_and(|set| ops[set..frame].iter().any(|op| matches!(op, Op::Sync(_)))),
        "the dead writer's frame was not truncated and synced before the next was written over it"
    );
    let mut image = base.clone();
    for op in &ops {
        match op {
            Op::Write(_, offset, bytes) if *offset >= 8192 => {
                let to = *offset as usize + bytes.len();
                if image.len() < to {
                    image.resize(to, 0);
                }
                for (index, byte) in bytes.iter().enumerate() {
                    let byte_at = *offset + index as u64;
                    if byte_at - byte_at % 512 != sector {
                        image[byte_at as usize] = *byte;
                    }
                }
            }
            Op::SetLen(_, len) => image.resize(*len as usize, 0),
            _ => {}
        }
    }
    let torn = dir.path().join("torn.ferro");
    std::fs::write(&torn, &image).unwrap();
    let readonly = fast().read_only(true).open::<Padded>(&torn).unwrap();
    assert_eq!(readonly.read().n, 3);
    drop(readonly);
    let reopened = fast().open::<Padded>(&torn).unwrap();
    assert_eq!(reopened.read().n, 3);
    reopened.verify().unwrap();
}

type Held = (Hook, Arc<AtomicBool>, mpsc::Receiver<()>, mpsc::Sender<()>);

fn held_at_leading() -> Held {
    let armed = Arc::new(AtomicBool::new(false));
    let (reached, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let (reached, released) = (
        parking_lot::Mutex::new(reached),
        parking_lot::Mutex::new(released),
    );
    let arm = armed.clone();
    let hook = Hook(Arc::new(move |step| {
        if step == "leading" && arm.swap(false, Ordering::AcqRel) {
            let _ = reached.lock().send(());
            let _ = released.lock().recv();
        }
    }));
    (hook, armed, waiting, release)
}

#[test]
fn a_following_shared_writer_sees_another_handles_commits_and_cycles_without_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("follow.ferro");
    let manual = Reclaim { every_bytes: 0 };
    let a: Db<Root> = shared().reclaim(manual).open(&path).unwrap();
    let b: Db<Root> = shared().reclaim(manual).follow(true).open(&path).unwrap();
    let watcher = b.watch();
    for i in 1..=30 {
        bump(&a, 0, i);
        if i % 10 == 0 {
            a.compact(Keep::Last(5)).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while b.read().n < i {
            let _ = watcher.recv_timeout(Duration::from_millis(20));
            assert!(Instant::now() < deadline, "b stopped at {}", b.read().n);
        }
    }
    let following = b.following().unwrap();
    assert!(
        following.notified && following.refreshes > 0,
        "{following:?}"
    );
    assert_eq!(following.last_error, None);
    let deadline = Instant::now() + Duration::from_secs(10);
    while b.size_on_disk() != a.size_on_disk() {
        assert!(Instant::now() < deadline, "b never moved to the new file");
        thread::sleep(Duration::from_millis(5));
    }
    bump(&b, 1, 1);
    a.refresh().unwrap();
    assert_eq!(a.read().n, 31);
}

#[test]
fn a_following_shared_writer_never_takes_a_frame_past_the_newest_superblock() {
    for poll in [None, Some(Duration::from_millis(2))] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bound.ferro");
        let (first, first_armed, first_reached, first_release) = held_at_leading();
        let (second, second_armed, second_reached, second_release) = held_at_leading();
        let a: Db<Root> = shared().hook(first).open(&path).unwrap();
        let c: Db<Root> = shared().hook(second).open(&path).unwrap();
        let follows = shared().follow(true);
        let follows = match poll {
            Some(every) => follows.follow_polling(every),
            None => follows,
        };
        let b: Db<Root> = follows.open(&path).unwrap();
        let watcher = b.watch();
        bump(&a, 0, 1);
        assert_eq!(watcher.recv_timeout(Duration::from_secs(10)).unwrap().n, 1);
        first_armed.store(true, Ordering::Release);
        let held = {
            let a = a.clone();
            thread::spawn(move || bump(&a, 0, 2))
        };
        first_reached.recv().unwrap();
        second_armed.store(true, Ordering::Release);
        let later = {
            let c = c.clone();
            thread::spawn(move || bump(&c, 2, 1))
        };
        second_reached.recv().unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(
            watcher.try_recv().is_err(),
            "b took a frame no superblock names"
        );
        first_release.send(()).unwrap();
        assert_eq!(held.join().unwrap(), 2);
        assert_eq!(watcher.recv_timeout(Duration::from_secs(10)).unwrap().n, 2);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            b.read().n,
            2,
            "b synced and published another handle's frame"
        );
        assert!(watcher.try_recv().is_err());
        second_release.send(()).unwrap();
        assert_eq!(later.join().unwrap(), 3);
        assert_eq!(watcher.recv_timeout(Duration::from_secs(10)).unwrap().n, 3);
        assert_eq!(b.following().unwrap().last_error, None);
    }
}

#[test]
fn a_following_shared_writer_reports_a_named_frame_that_does_not_check() {
    use std::os::unix::fs::FileExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("named.ferro");
    let a: Db<Root> = shared().open(&path).unwrap();
    let (hook, armed, reached, release) = held_at_leading();
    armed.store(true, Ordering::Release);
    let b: Db<Root> = shared().hook(hook).follow(true).open(&path).unwrap();
    let watcher = b.watch();
    bump(&a, 0, 1);
    reached.recv().unwrap();
    let mut ends = vec![a.size_on_disk()];
    for i in 2..=5 {
        bump(&a, 0, i);
        ends.push(a.size_on_disk());
    }
    let middle = (ends[1] + ends[2]) / 2;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut byte = [0u8];
    file.read_exact_at(&mut byte, middle).unwrap();
    file.write_all_at(&[byte[0] ^ 0x10], middle).unwrap();
    release.send(()).unwrap();
    assert_eq!(watcher.recv_timeout(Duration::from_secs(10)).unwrap().n, 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = loop {
        if let Some(error) = b.following().unwrap().last_error {
            break error;
        }
        assert!(Instant::now() < deadline, "the follower reported nothing");
        thread::sleep(Duration::from_millis(5));
    };
    assert!(
        error.contains("frame 3") && error.contains("does not check"),
        "{error}"
    );
    assert_eq!(b.read().n, 1);
    assert!(watcher.try_recv().is_err());
    file.write_all_at(&byte, middle).unwrap();
    assert_eq!(watcher.recv_timeout(Duration::from_secs(10)).unwrap().n, 5);
    let deadline = Instant::now() + Duration::from_secs(10);
    while b.following().unwrap().last_error.is_some() {
        assert!(Instant::now() < deadline, "the follower kept its error");
        thread::sleep(Duration::from_millis(5));
    }
}

fn manual() -> Options {
    shared().reclaim(Reclaim { every_bytes: 0 })
}

fn writers_on_branches_of_their_own(
    path: &Path,
    writers: u64,
    stop: &Arc<AtomicBool>,
    nested: bool,
) -> Vec<thread::JoinHandle<u64>> {
    let first: Db<Root> = manual().open(path).unwrap();
    for writer in 0..writers {
        first.fork(&format!("own-{writer}")).unwrap();
    }
    drop(first);
    let ready = Arc::new(Barrier::new(writers as usize + 1));
    let threads = (0..writers)
        .map(|writer| {
            let path = path.to_path_buf();
            let stop = stop.clone();
            let ready = ready.clone();
            thread::spawn(move || {
                let db: Db<Root> = manual().open(&path).unwrap();
                let db = db.branch(&format!("own-{writer}")).unwrap();
                let outer = path.with_file_name(format!("outer-{writer}.ferro"));
                let outer: Option<Db<Root>> = nested.then(|| fast().open(&outer).unwrap());
                ready.wait();
                let commit = |i: &mut u64| {
                    while !stop.load(Ordering::Relaxed) {
                        *i += 1;
                        bump(&db, writer, *i);
                    }
                };
                let mut i = 0;
                match &outer {
                    Some(outer) => outer.update(|_| commit(&mut i)).unwrap(),
                    None => commit(&mut i),
                }
                i
            })
        })
        .collect();
    ready.wait();
    threads
}

fn five_cycles_beside_writers_on_branches_of_their_own(nested: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gate.ferro");
    let stop = Arc::new(AtomicBool::new(false));
    let writers = writers_on_branches_of_their_own(&path, 4, &stop, nested);
    let cycler: Db<Root> = manual().open(&path).unwrap();
    bump(&cycler, 99, 1);
    thread::sleep(Duration::from_millis(100));
    let (done, finished) = mpsc::channel();
    let started = Instant::now();
    let cycle = thread::spawn(move || {
        for _ in 0..5 {
            cycler.compact(Keep::Last(2)).unwrap();
        }
        done.send(()).unwrap();
    });
    let waited = finished.recv_timeout(Duration::from_secs(20));
    stop.store(true, Ordering::Relaxed);
    let commits: Vec<u64> = writers.into_iter().map(|w| w.join().unwrap()).collect();
    cycle.join().unwrap();
    assert!(
        waited.is_ok(),
        "five cycles had not finished after 20 s while four handles committed {commits:?} times \
         on branches of their own, nested in closures on other files: {nested}; they finished \
         {:?} after they began, once those stopped",
        started.elapsed()
    );
}

#[test]
fn a_cycle_finishes_while_other_handles_commit_on_branches_of_their_own() {
    five_cycles_beside_writers_on_branches_of_their_own(false);
}

#[test]
fn a_cycle_finishes_while_writers_in_closures_on_other_files_commit_on_branches_of_their_own() {
    five_cycles_beside_writers_on_branches_of_their_own(true);
}

#[test]
fn a_handles_own_commits_wait_for_its_cycle_only_while_other_handles_finish_theirs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("own.ferro");
    let stop = Arc::new(AtomicBool::new(false));
    let writers = writers_on_branches_of_their_own(&path, 4, &stop, false);
    let cycler: Db<Root> = manual().open(&path).unwrap();
    let own = cycler.fork("mine").unwrap();
    thread::sleep(Duration::from_millis(100));
    let cycling = Arc::new(AtomicBool::new(true));
    let cycle = thread::spawn({
        let cycling = cycling.clone();
        move || {
            while cycling.load(Ordering::Relaxed) {
                cycler.compact(Keep::All).unwrap();
            }
        }
    });
    let (done, finished) = mpsc::channel();
    let committer = thread::spawn(move || {
        for i in 1..=50 {
            bump(&own, 77, i);
        }
        done.send(()).unwrap();
    });
    let waited = finished.recv_timeout(Duration::from_secs(20));
    stop.store(true, Ordering::Relaxed);
    cycling.store(false, Ordering::Relaxed);
    let commits: Vec<u64> = writers.into_iter().map(|w| w.join().unwrap()).collect();
    committer.join().unwrap();
    cycle.join().unwrap();
    assert!(
        waited.is_ok(),
        "50 commits on a branch of the cycling handle took over 20 s while four other handles \
         committed {commits:?} times"
    );
}

#[test]
fn nested_and_awaited_writes_go_on_through_another_handles_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested.ferro");
    let stop = Arc::new(AtomicBool::new(false));
    let writers = writers_on_branches_of_their_own(&path, 2, &stop, false);
    let db: Db<Root> = manual().open(&path).unwrap();
    db.fork("a").unwrap();
    db.fork("b").unwrap();
    let a = db.branch("a").unwrap();
    let b = db.branch("b").unwrap();
    let mut threads = Vec::new();
    for kind in 0..3 {
        let (a, b, stop) = (a.clone(), b.clone(), stop.clone());
        threads.push(thread::spawn(move || {
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) {
                i += 1;
                match kind {
                    0 => a
                        .update(|root| {
                            bump(&b, 10, i);
                            root.n += 1;
                        })
                        .unwrap(),
                    1 => a
                        .update(|root| {
                            block_on(b.update_async(|inner| inner.n += 1)).unwrap();
                            root.n += 1;
                        })
                        .unwrap(),
                    _ => {
                        bump(&b, 11, i);
                    }
                }
            }
        }));
    }
    let cycler: Db<Root> = manual().open(&path).unwrap();
    let (done, finished) = mpsc::channel();
    let cycle = thread::spawn(move || {
        for _ in 0..20 {
            cycler.compact(Keep::Last(3)).unwrap();
        }
        done.send(()).unwrap();
    });
    let waited = finished.recv_timeout(Duration::from_secs(60));
    stop.store(true, Ordering::Relaxed);
    assert!(waited.is_ok(), "twenty cycles did not finish in 60 s");
    cycle.join().unwrap();
    threads
        .into_iter()
        .for_each(|thread| thread.join().unwrap());
    let commits: Vec<u64> = writers.into_iter().map(|w| w.join().unwrap()).collect();
    assert!(commits.iter().all(|n| *n > 0), "{commits:?}");
    db.refresh().unwrap();
    db.verify_typed().unwrap();
}

fn a_closure_waits_for_another_thread_beside_another_handles_cycle(awaited: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("joined.ferro");
    let db: Db<Root> = manual().open(&path).unwrap();
    db.fork("a").unwrap();
    db.fork("b").unwrap();
    bump(&db, 0, 1);
    bump(&db, 0, 2);
    let (a, b) = (db.branch("a").unwrap(), db.branch("b").unwrap());
    let cycler: Db<Root> = manual().open(&path).unwrap();
    let (go, started) = mpsc::channel::<()>();
    let (done, finished) = mpsc::channel();
    let cycled = done.clone();
    thread::spawn(move || {
        started.recv().unwrap();
        cycler.compact(Keep::Last(1)).unwrap();
        cycled.send("the cycle").unwrap();
    });
    thread::spawn(move || {
        a.update(|root| {
            go.send(()).unwrap();
            thread::sleep(Duration::from_millis(300));
            let b = b.clone();
            thread::spawn(move || match awaited {
                true => block_on(b.update_async(|inner| inner.n += 1)).unwrap(),
                false => drop(bump(&b, 1, 1)),
            })
            .join()
            .unwrap();
            root.n += 1;
        })
        .unwrap();
        drop(db);
        done.send("the closure").unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut ended = Vec::new();
    while ended.len() < 2 {
        match finished.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(what) => ended.push(what),
            Err(_) => break,
        }
    }
    assert_eq!(
        ended.len(),
        2,
        "a closure on `a` joining a thread that commits on `b`, awaited: {awaited}, and another \
         handle's cycle had not both finished after 20 s, only {ended:?}"
    );
}

#[test]
fn a_closure_joining_a_thread_that_commits_on_a_later_branch_finishes_beside_another_handles_cycle()
{
    a_closure_waits_for_another_thread_beside_another_handles_cycle(false);
}

#[test]
fn a_closure_joining_a_thread_that_awaits_a_commit_finishes_beside_another_handles_cycle() {
    a_closure_waits_for_another_thread_beside_another_handles_cycle(true);
}

fn a_closure_waits_for_a_commit_through_the_handle_that_cycles(awaited: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("through.ferro");
    let y: Db<Root> = manual().open(&path).unwrap();
    y.fork("a").unwrap();
    y.fork("b").unwrap();
    bump(&y, 0, 1);
    bump(&y, 0, 2);
    let cycler: Db<Root> = manual().open(&path).unwrap();
    let b = cycler.branch("b").unwrap();
    let a = y.branch("a").unwrap();
    let (go, started) = mpsc::channel::<()>();
    let (done, finished) = mpsc::channel();
    let cycled = done.clone();
    thread::spawn(move || {
        started.recv().unwrap();
        cycler.compact(Keep::Last(1)).unwrap();
        let _ = cycled.send(None);
    });
    thread::spawn(move || {
        a.update(|root| {
            go.send(()).unwrap();
            thread::sleep(Duration::from_millis(200));
            let joined = Instant::now();
            thread::spawn(move || match awaited {
                true => block_on(b.update_async(|inner| inner.n += 1)).unwrap(),
                false => drop(bump(&b, 1, 1)),
            })
            .join()
            .unwrap();
            let _ = done.send(Some(joined.elapsed()));
            root.n += 1;
        })
        .unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut ended = Vec::new();
    while ended.len() < 2 {
        match finished.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(what) => ended.push(what),
            Err(_) => break,
        }
    }
    let joined = ended.iter().flatten().next().copied();
    assert_eq!(
        ended.len(),
        2,
        "a closure on `a` in one handle that joined a thread committing on `b` through another \
         handle, awaited: {awaited}, while that handle's cycle waited for `a`, had not finished \
         with the cycle after 20 s; the commit took {joined:?}"
    );
    assert!(
        joined.is_some_and(|took| took < Duration::from_secs(1)),
        "a commit through the cycling handle, awaited: {awaited}, that a closure on `a` joined \
         took {joined:?} beside that handle's cycle"
    );
}

#[test]
fn a_closure_joining_a_commit_through_the_handle_that_cycles_finishes_beside_its_cycle() {
    a_closure_waits_for_a_commit_through_the_handle_that_cycles(false);
}

#[test]
fn a_closure_joining_an_awaited_commit_through_the_handle_that_cycles_finishes_beside_its_cycle() {
    a_closure_waits_for_a_commit_through_the_handle_that_cycles(true);
}

fn stopping_at(stop: &'static str) -> (Hook, mpsc::Receiver<&'static str>, Arc<Barrier>) {
    let (sender, receiver) = mpsc::channel();
    let sender = parking_lot::Mutex::new(sender);
    let resume = Arc::new(Barrier::new(2));
    let held = resume.clone();
    let hook = Hook(Arc::new(move |step| {
        let _ = sender.lock().send(step);
        if step == stop {
            held.wait();
        }
    }));
    (hook, receiver, resume)
}

fn wait_for_step(steps: &mpsc::Receiver<&'static str>, step: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match steps.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(seen) if seen == step => return,
            Ok(_) => {}
            Err(_) => panic!("the cycle never reached {step}"),
        }
    }
}

#[test]
fn a_joined_commit_waits_at_the_gate_no_longer_than_the_hold_after_an_earlier_wait_ran_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale.ferro");
    let (hook, steps, resume) = stopping_at("copied");
    let cycler: Db<Root> = manual().hook(hook).open(&path).unwrap();
    let idle: Db<Root> = manual().open(&path).unwrap();
    for name in ["a", "b", "c", "d"] {
        idle.fork(name).unwrap();
    }
    for i in 1..=3 {
        bump(&idle, 0, i);
    }
    let other: Db<Root> = manual().open(&path).unwrap();
    let a = other.branch("a").unwrap();
    let (b, c, d) = (
        idle.branch("b").unwrap(),
        idle.branch("c").unwrap(),
        idle.branch("d").unwrap(),
    );
    let (entered, release) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let holder = {
        let (entered, release) = (entered.clone(), release.clone());
        thread::spawn(move || {
            c.update(|root| {
                entered.wait();
                release.wait();
                root.n += 1;
            })
            .unwrap();
        })
    };
    entered.wait();
    let cycle = thread::spawn(move || cycler.compact(Keep::Last(1)));
    thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    bump(&d, 1, 1);
    let first = started.elapsed();
    release.wait();
    holder.join().unwrap();
    wait_for_step(&steps, "copied");
    thread::sleep(Duration::from_millis(600));
    let (in_a, go) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let (sent, took) = mpsc::channel();
    let closure = {
        let (in_a, go) = (in_a.clone(), go.clone());
        thread::spawn(move || {
            a.update(|root| {
                in_a.wait();
                go.wait();
                let joined = Instant::now();
                thread::spawn(move || bump(&b, 2, 1)).join().unwrap();
                let _ = sent.send(joined.elapsed());
                root.n += 1;
            })
            .unwrap();
        })
    };
    in_a.wait();
    resume.wait();
    wait_for_step(&steps, "synced");
    thread::sleep(Duration::from_millis(50));
    go.wait();
    let waited = took
        .recv_timeout(Duration::from_secs(20))
        .expect("the joined commit never finished");
    closure.join().unwrap();
    cycle.join().unwrap().unwrap();
    assert!(
        waited < Duration::from_millis(300),
        "a commit that a closure on `a` joined waited {waited:?} at a gate held for about 50 ms, \
         after the same handle's last write had waited {first:?} at an earlier hold"
    );
}

fn a_cycle_beside_threads_of_one_handle_with_long_staggered_closures(
    writers: u64,
    closure: Duration,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.ferro");
    let db: Db<Root> = manual().open(&path).unwrap();
    for writer in 0..writers {
        db.fork(&format!("own-{writer}")).unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let threads: Vec<_> = (0..writers)
        .map(|writer| {
            let db = db.branch(&format!("own-{writer}")).unwrap();
            let stop = stop.clone();
            thread::spawn(move || {
                thread::sleep(closure * writer as u32 / writers as u32);
                let mut commits = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    db.update(|root| {
                        thread::sleep(closure);
                        root.n += 1;
                    })
                    .unwrap();
                    commits += 1;
                }
                commits
            })
        })
        .collect();
    let cycler: Db<Root> = manual().open(&path).unwrap();
    bump(&cycler, 99, 1);
    thread::sleep(closure * 2);
    let (done, finished) = mpsc::channel();
    let started = Instant::now();
    let cycle = thread::spawn(move || {
        let out = cycler.compact(Keep::All);
        let _ = done.send(started.elapsed());
        out
    });
    let waited = finished.recv_timeout(Duration::from_secs(20));
    stop.store(true, Ordering::Relaxed);
    let commits: Vec<u64> = threads.into_iter().map(|w| w.join().unwrap()).collect();
    let after = started.elapsed();
    cycle.join().unwrap().unwrap();
    assert!(
        waited.is_ok(),
        "a cycle had not finished after 20 s beside {writers} threads of one handle whose \
         closures last {closure:?}, started {:?} apart, which committed {commits:?} times; it \
         finished {after:?} after it began, once they stopped",
        closure / writers as u32
    );
}

#[test]
fn a_cycle_finishes_beside_threads_of_one_handle_whose_600_ms_closures_end_at_staggered_times() {
    a_cycle_beside_threads_of_one_handle_with_long_staggered_closures(
        4,
        Duration::from_millis(600),
    );
}

fn tick(db: &Db<Root>) {
    db.update(|root| root.n += 1).unwrap();
}

fn a_closure_joins_a_commit_through_the_cycling_handle_while_a_write_there_waits_for_it(
    awaited: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("behind.ferro");
    let cycler: Db<Root> = manual().open(&path).unwrap();
    for name in ["a", "b"] {
        tick(&cycler.fork(name).unwrap());
    }
    tick(&cycler);
    let other: Db<Root> = manual().open(&path).unwrap();
    let b_other = other.branch("b").unwrap();
    let (a, b) = (cycler.branch("a").unwrap(), cycler.branch("b").unwrap());
    let (entered, inside) = mpsc::channel();
    let (done, finished) = mpsc::channel::<&'static str>();
    let closed = done.clone();
    thread::spawn(move || {
        b_other
            .update(|root| {
                entered.send(()).unwrap();
                thread::sleep(Duration::from_millis(400));
                thread::spawn(move || tick(&a)).join().unwrap();
                root.n += 1;
            })
            .unwrap();
        let _ = closed.send("the closure on `b`");
    });
    inside.recv().unwrap();
    let wrote = done.clone();
    thread::spawn(move || {
        match awaited {
            true => block_on(b.update_async(|root| root.n += 1)).unwrap(),
            false => tick(&b),
        }
        let _ = wrote.send("the write on `b` through the cycling handle");
    });
    thread::sleep(Duration::from_millis(100));
    thread::spawn(move || {
        cycler.compact(Keep::Last(1)).unwrap();
        let _ = done.send("the cycle");
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut ended = Vec::new();
    while ended.len() < 3 {
        match finished.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(what) => ended.push(what),
            Err(_) => break,
        }
    }
    assert_eq!(
        ended.len(),
        3,
        "a closure on `b` in one handle joined a commit on `a` through another handle, while a \
         write on `b` through that handle, awaited: {awaited}, waited for the closure and that \
         handle cycled; after 20 s only {ended:?} had finished"
    );
}

#[test]
fn a_closure_joining_a_commit_on_an_earlier_branch_finishes_beside_a_blocked_write_on_its_own() {
    a_closure_joins_a_commit_through_the_cycling_handle_while_a_write_there_waits_for_it(false);
}

#[test]
fn a_closure_joining_a_commit_on_an_earlier_branch_finishes_beside_an_awaited_write_on_its_own() {
    a_closure_joins_a_commit_through_the_cycling_handle_while_a_write_there_waits_for_it(true);
}

fn sleepy_writers(
    path: &Path,
    writers: u64,
    closure: Duration,
    stop: &Arc<AtomicBool>,
) -> Vec<thread::JoinHandle<u64>> {
    let first: Db<Root> = manual().open(path).unwrap();
    for writer in 0..writers {
        first.fork(&format!("own-{writer}")).unwrap();
    }
    drop(first);
    (0..writers)
        .map(|writer| {
            let path = path.to_path_buf();
            let stop = stop.clone();
            thread::spawn(move || {
                let db: Db<Root> = manual().open(&path).unwrap();
                let db = db.branch(&format!("own-{writer}")).unwrap();
                let mut commits = 0;
                while !stop.load(Ordering::Relaxed) {
                    db.update(|root| {
                        thread::sleep(closure);
                        root.n += 1;
                    })
                    .unwrap();
                    commits += 1;
                }
                commits
            })
        })
        .collect()
}

#[test]
fn cycles_beside_threads_of_the_cycling_handle_contending_for_one_branch_take_under_a_second() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("contended.ferro");
    let stop = Arc::new(AtomicBool::new(false));
    let others = sleepy_writers(&path, 4, Duration::from_millis(20), &stop);
    let cycler: Db<Root> = manual().open(&path).unwrap();
    let hot = cycler.fork("hot").unwrap();
    let longest = Arc::new(AtomicU64::new(0));
    let own: Vec<_> = (0..4)
        .map(|_| {
            let (hot, stop, longest) = (hot.clone(), stop.clone(), longest.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    tick(&hot);
                    let took = u64::try_from(started.elapsed().as_micros()).unwrap();
                    longest.fetch_max(took, Ordering::Relaxed);
                }
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(200));
    let (done, finished) = mpsc::channel();
    let cycle = thread::spawn(move || {
        for _ in 0..3 {
            let started = Instant::now();
            cycler.compact(Keep::Last(2)).unwrap();
            let _ = done.send(started.elapsed());
        }
    });
    let mut took = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while took.len() < 3 {
        match finished.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(elapsed) => took.push(elapsed),
            Err(_) => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
    let commits: Vec<u64> = others.into_iter().map(|w| w.join().unwrap()).collect();
    own.into_iter().for_each(|thread| thread.join().unwrap());
    cycle.join().unwrap();
    let longest = Duration::from_micros(longest.load(Ordering::Relaxed));
    assert!(
        took.len() == 3 && took.iter().all(|took| *took < Duration::from_secs(1)),
        "cycles beside four threads of the cycling handle committing on one branch, and four \
         other handles whose closures last 20 ms and committed {commits:?} times, took {took:?}; \
         the longest commit through the cycling handle took {longest:?}"
    );
}

fn keeping_ours(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (&'static str, ()) {
    ("merge", ())
}

#[test]
fn cycles_and_pulls_in_one_handle_all_finish_beside_other_handles_committing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.ferro");
    let stop = Arc::new(AtomicBool::new(false));
    let others = sleepy_writers(&path, 4, Duration::from_millis(20), &stop);
    let cycler: Db<Root> = manual().open(&path).unwrap();
    let source: Db<Root> = fast().open(dir.path().join("source.ferro")).unwrap();
    thread::sleep(Duration::from_millis(200));
    let (done, finished) = mpsc::channel::<&'static str>();
    let cycled = done.clone();
    let compactor = cycler.clone();
    let cycles = thread::spawn(move || {
        for _ in 0..5 {
            compactor.compact(Keep::Last(2)).unwrap();
            let _ = cycled.send("a cycle");
        }
    });
    let pulls = thread::spawn(move || {
        for _ in 0..5 {
            tick(&source);
            cycler.pull(&source, "source", keeping_ours).unwrap();
            let _ = done.send("a pull");
        }
    });
    let mut ended = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(40);
    while ended.len() < 10 {
        match finished.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(what) => ended.push(what),
            Err(_) => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
    let commits: Vec<u64> = others.into_iter().map(|w| w.join().unwrap()).collect();
    cycles.join().unwrap();
    pulls.join().unwrap();
    assert_eq!(
        ended.len(),
        10,
        "five cycles and five pulls in one handle, beside four other handles whose closures last \
         20 ms and committed {commits:?} times, had not all finished after 40 s: {ended:?}"
    );
}

const GATE_AT: u64 = (1 << 62) + 5;
const LANES_AT: u64 = (1 << 62) + (1 << 20);
const LANES: u64 = 1 << 60;

fn ofd(file: &File, command: libc::c_int, kind: libc::c_int, at: u64, len: u64) -> libc::flock {
    // SAFETY: `flock` is a C struct of plain integers, for which all zero bytes are a valid value.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = at as libc::off_t;
    lock.l_len = len as libc::off_t;
    // SAFETY: the descriptor belongs to `file`, which is borrowed for the call, and `lock` is a
    // live, initialized `flock`, which every command passed here takes a pointer to.
    let done = unsafe { libc::fcntl(file.as_raw_fd(), command, &mut lock) };
    assert_ne!(done, -1, "fcntl: {}", std::io::Error::last_os_error());
    lock
}

fn locked_elsewhere(file: &File, at: u64, len: u64) -> bool {
    let lock = ofd(file, libc::F_OFD_GETLK, libc::F_WRLCK, at, len);
    lock.l_type != libc::F_UNLCK as libc::c_short
}

fn raw_open(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

#[test]
fn a_cycle_that_panics_in_its_pause_leaves_neither_the_gate_nor_the_branch_range_locked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("panic.ferro");
    let hook = Hook(Arc::new(|step| {
        if step == "paused" {
            panic!("a hook panics in the pause");
        }
    }));
    let cycler: Db<Root> = manual().hook(hook).open(&path).unwrap();
    for name in ["a", "b"] {
        tick(&cycler.fork(name).unwrap());
    }
    tick(&cycler);
    let held = cycler.clone();
    let ran = thread::spawn(move || held.compact(Keep::Last(1))).join();
    assert!(ran.is_err(), "the hook did not panic: {ran:?}");
    let probe = raw_open(&path);
    let gate = locked_elsewhere(&probe, GATE_AT, 1);
    let range = locked_elsewhere(&probe, LANES_AT, LANES);
    drop(cycler);
    assert!(
        !gate && !range,
        "after a cycle panicked in its pause, with its handle still open, the gate was locked: \
         {gate}, and the branch range: {range}"
    );
}

fn commit_beside_a_gate_held_for(held: Option<Duration>) -> Duration {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("held.ferro");
    let db: Db<Root> = manual().open(&path).unwrap();
    let own = db.fork("own").unwrap();
    tick(&own);
    let holder = raw_open(&path);
    ofd(&holder, libc::F_OFD_SETLK, libc::F_WRLCK, GATE_AT, 1);
    let started = Instant::now();
    let writer = thread::spawn(move || tick(&own));
    if let Some(held) = held {
        thread::sleep(held);
        drop(holder);
    }
    writer.join().unwrap();
    started.elapsed()
}

#[test]
fn a_writer_waits_at_a_gate_that_another_open_holds_until_it_opens() {
    let took = commit_beside_a_gate_held_for(Some(Duration::from_millis(200)));
    assert!(
        took >= Duration::from_millis(200) && took < Duration::from_millis(400),
        "a commit beside a gate that another open held for 200 ms took {took:?}"
    );
}

#[test]
fn a_writer_goes_on_past_a_gate_that_is_never_opened_within_a_bounded_time() {
    let took = commit_beside_a_gate_held_for(None);
    assert!(
        took > Duration::from_secs(1) && took < Duration::from_secs(5),
        "a commit beside a gate that another open held for good took {took:?}"
    );
}

fn held_across_another_handles_cycles(follower: Options) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("held.ferro");
    let writer: Db<Root> = manual().history(Keep::Last(2)).open(&path).unwrap();
    for chunk in 0..30u64 {
        writer
            .update(|root| {
                for k in 0..100 {
                    root.n += 1;
                    root.order.insert(chunk * 100 + k, (chunk, k));
                }
            })
            .unwrap();
    }
    let follower: Db<Root> = follower.follow(true).open(&path).unwrap();
    let caught_up = |version: u64| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while follower.version() < version {
            assert!(Instant::now() < deadline, "{:?}", follower.following());
            thread::sleep(Duration::from_millis(2));
        }
    };
    caught_up(writer.version());
    let snapshot = follower.read();
    let mut iter = snapshot.order.iter();
    let mut seen: Vec<_> = iter.by_ref().take(100).map(|(k, v)| (k, *v)).collect();
    for round in 0..3u64 {
        writer
            .update(|root| {
                for k in 0..3000 {
                    root.order.insert(k, (round + 100, k));
                }
            })
            .unwrap();
        writer.update(|root| root.n += 1).unwrap();
        writer.compact(Keep::Last(1)).unwrap();
    }
    writer.update(|root| root.n += 1).unwrap();
    caught_up(writer.version());
    seen.extend(iter.map(|(k, v)| (k, *v)));
    let expected: Vec<_> = (0..3000u64).map(|k| (k, (k / 100, k % 100))).collect();
    assert_eq!(seen, expected);
    let again: Vec<_> = snapshot.order.iter().map(|(k, v)| (k, *v)).collect();
    assert_eq!(again, expected);
    assert_eq!(follower.read().order.get(&5).as_deref(), Some(&(102, 5)));
    follower.verify_typed().unwrap();
    assert!(follower.following().unwrap().last_error.is_none());
}

#[test]
fn a_read_only_follower_reads_what_it_held_through_another_handles_cycles() {
    held_across_another_handles_cycles(fast().read_only(true));
}

#[test]
fn a_shared_follower_reads_what_it_held_through_another_handles_cycles() {
    held_across_another_handles_cycles(manual());
}

type Kv = Map<u64, u64>;

fn merge_keeping_ours(
    kv: &mut Kv,
    base: &Snapshot<Kv>,
    theirs: &Snapshot<Kv>,
) -> (&'static str, ()) {
    kv.merge(base, theirs, |conflict| conflict.keep_ours());
    ("pull", ())
}

#[derive(Clone, Copy)]
enum Moved {
    Dropped,
    PulledAndPruned,
}

fn pull_after_another_handle_moved_its_tracking_branch(moved: Moved) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pulled.ferro");
    let remote: Db<Kv> = fast().open(dir.path().join("remote.ferro")).unwrap();
    let a: Db<Kv> = manual().open(&path).unwrap();
    let b: Db<Kv> = manual().history(Keep::Last(1)).open(&path).unwrap();
    a.update(|kv| kv.insert(100, 100)).unwrap();
    for i in 0..5u64 {
        remote.update(|kv| kv.insert(i, i)).unwrap();
    }
    a.pull(&remote, "origin", merge_keeping_ours).unwrap();
    b.refresh().unwrap();
    match moved {
        Moved::Dropped => b.drop_branch("origin").unwrap(),
        Moved::PulledAndPruned => {
            remote.update(|kv| kv.insert(5, 5)).unwrap();
            b.pull(&remote, "origin", merge_keeping_ours).unwrap();
        }
    }
    remote.update(|kv| kv.insert(10, 10)).unwrap();
    a.pull(&remote, "origin", merge_keeping_ours).unwrap();
    assert_eq!(a.read().get(&10).as_deref(), Some(&10));
    remote.update(|kv| kv.insert(11, 11)).unwrap();
    let have = a.have().unwrap();
    let bundle = remote.bundle(&have).unwrap();
    b.drop_branch("origin").unwrap();
    assert!(matches!(
        a.receive("origin", &bundle, merge_keeping_ours),
        Err(Error::StaleHave(_))
    ));
    let bundle = remote.bundle(&a.have().unwrap()).unwrap();
    a.receive("origin", &bundle, merge_keeping_ours).unwrap();
    assert_eq!(a.read().get(&11).as_deref(), Some(&11));
    a.verify_typed().unwrap();
}

#[test]
fn a_pull_after_another_handle_dropped_its_tracking_branch_lands() {
    pull_after_another_handle_moved_its_tracking_branch(Moved::Dropped);
}

#[test]
fn a_pull_after_another_handle_pulled_into_its_tracking_branch_and_pruned_it_lands() {
    pull_after_another_handle_moved_its_tracking_branch(Moved::PulledAndPruned);
}

fn merge_spread(
    spread: &mut Spread,
    base: &Snapshot<Spread>,
    theirs: &Snapshot<Spread>,
) -> (&'static str, ()) {
    spread.merge(base, theirs, |conflict| conflict.keep_theirs());
    ("pull", ())
}

fn receive_through_a_handle_behind_the_one_that_sent_the_have(count: u64, len: usize) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.ferro");
    let answers: Db<Spread> = manual().open(&path).unwrap();
    let applies: Db<Spread> = manual().open(&path).unwrap();
    let phone: Db<Spread> = fast().open(dir.path().join("phone.ferro")).unwrap();
    answers
        .update(|spread| spread.insert(0, "zero".to_string()))
        .unwrap();
    let pulled = phone.pull(&answers, "server", merge_spread).unwrap();
    assert_eq!(pulled.outcome, ferro::Outcome::FastForward);
    phone
        .update(|spread| {
            for key in 1..=count {
                spread.insert(key, common::text(key, len));
            }
        })
        .unwrap();
    let bundle = phone.bundle(&answers.have().unwrap()).unwrap();
    let pulled = applies.receive("phone", &bundle, merge_spread).unwrap();
    assert_eq!(pulled.outcome, ferro::Outcome::FastForward);
    let again = phone.bundle(&answers.have().unwrap()).unwrap();
    let pulled = applies.receive("phone", &again, merge_spread).unwrap();
    assert_eq!(pulled.outcome, ferro::Outcome::UpToDate);
    answers.refresh().unwrap();
    assert_eq!(answers.head_hash(), phone.head_hash());
    assert_eq!(spread_of(&answers), spread_of(&phone));
    answers.verify_typed().unwrap();
    bundle.as_bytes().len()
}

#[test]
fn a_bundle_built_from_another_handles_have_lands() {
    let sent = receive_through_a_handle_behind_the_one_that_sent_the_have(20, 16);
    assert!(sent < 1 << 20);
}

#[test]
fn a_bundle_over_a_mebibyte_built_from_another_handles_have_lands() {
    let sent = receive_through_a_handle_behind_the_one_that_sent_the_have(1_500, 1_000);
    assert!(sent > 1 << 20, "{sent}");
}

#[test]
fn a_pull_from_a_handle_behind_another_sends_the_newest_head() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.ferro");
    let writes: Db<Kv> = manual().open(&path).unwrap();
    let serves: Db<Kv> = manual().open(&path).unwrap();
    let phone: Db<Kv> = fast().open(dir.path().join("phone.ferro")).unwrap();
    writes.update(|kv| kv.insert(1, 1)).unwrap();
    let pulled = phone.pull(&serves, "server", merge_keeping_ours).unwrap();
    assert_eq!(pulled.outcome, ferro::Outcome::FastForward);
    assert_eq!(phone.head_hash(), writes.head_hash());
    writes.update(|kv| kv.insert(2, 2)).unwrap();
    let bundle = serves.bundle(&phone.have().unwrap()).unwrap();
    let pulled = phone
        .receive("server", &bundle, merge_keeping_ours)
        .unwrap();
    assert_eq!(pulled.outcome, ferro::Outcome::FastForward);
    assert_eq!(phone.head_hash(), writes.head_hash());
    writes.update(|kv| kv.insert(3, 3)).unwrap();
    let pulled = serves.push(&phone, "server", merge_keeping_ours).unwrap();
    assert_eq!(pulled.outcome, ferro::Outcome::FastForward);
    assert_eq!(phone.read().get(&3).as_deref(), Some(&3));
    phone.verify_typed().unwrap();
}

fn expected(error: &Error) -> bool {
    matches!(
        error,
        Error::NoBranch(_)
            | Error::BranchExists(_)
            | Error::BranchInUse(_)
            | Error::NoCommonAncestor
            | Error::NoVersion(_)
    )
}

fn churn(path: &Path, who: u64, ops: u64, seed: u64) -> Vec<String> {
    let keep = match who % 3 {
        0 => Keep::Last(4),
        1 => Keep::All,
        _ => Keep::Last(2),
    };
    let db: Db<Kv> = manual().history(keep).open(path).unwrap();
    let remote: Db<Kv> = fast()
        .open(path.with_extension(format!("remote{who}")))
        .unwrap();
    let mut rng = common::Rng::new(seed);
    let mut odd = Vec::new();
    let mut note = |what: &str, result: ferro::Result<()>| {
        if let Err(error) = result
            && !expected(&error)
        {
            odd.push(format!("{who} {what}: {error:?}"));
        }
    };
    for op in 0..ops {
        let name = match rng.chance(25) {
            true => format!("r{}", rng.below(3)),
            false => format!("f{}", rng.below(3)),
        };
        let value = who * 1_000_000 + op;
        match rng.below(12) {
            0..=2 => {
                let key = rng.below(64);
                note("update", db.update(|kv| drop(kv.insert(key, value))));
            }
            3 => {
                let key = rng.below(64);
                let result = db
                    .branch(&name)
                    .and_then(|branch| branch.update(|kv| drop(kv.insert(key, value))));
                note("branch update", result);
            }
            4 => note("fork", db.fork(&name).map(drop)),
            5 => note("drop", db.drop_branch(&name)),
            6 => {
                let result = db.branch(&name).and_then(|branch| {
                    db.merge_with(&branch, |kv, base, theirs| {
                        kv.merge(base, theirs, |conflict| conflict.keep_ours());
                        ("merge", ())
                    })
                    .map(drop)
                });
                note("merge", result);
            }
            7 => {
                let back = db.version().saturating_sub(1 + rng.below(3));
                let result = db
                    .at(back)
                    .and_then(|snapshot| db.restore(&snapshot).map(drop));
                note("restore", result);
            }
            8 => {
                let result = db.commit_stamped(|kv, stamp| {
                    kv.insert(1000 + who, stamp.time_ms());
                    ("stamped", ())
                });
                note("stamped", result.map(drop));
            }
            9 => match who == 0 && rng.chance(30) {
                true => note("compact", db.compact(keep).map(drop)),
                false => note("refresh", db.refresh().map(drop)),
            },
            _ => {
                let key = 500 + rng.below(16);
                remote.update(|kv| drop(kv.insert(key, value))).unwrap();
                let tracking = format!("r{who}");
                let pulled = db.pull(&remote, &tracking, merge_keeping_ours);
                note("pull", pulled.map(drop));
            }
        }
    }
    if let Err(error) = db.verify() {
        odd.push(format!("{who} verify at the end: {error:?}"));
    }
    odd
}

#[test]
fn three_handles_that_fork_drop_merge_restore_pull_and_cycle_meet_only_expected_errors() {
    for round in 0..u64::from(common::cases(4)) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("churn.ferro");
        drop(manual().open::<Kv>(&path).unwrap());
        let (done, finished) = mpsc::channel();
        for who in 0..3u64 {
            let path = path.clone();
            let done = done.clone();
            thread::spawn(move || done.send(churn(&path, who, 400, round * 10 + who + 1)));
        }
        let mut odd = Vec::new();
        for _ in 0..3 {
            let found = finished.recv_timeout(Duration::from_secs(120));
            odd.extend(found.expect("a handle hung or panicked"));
        }
        let db: Db<Kv> = fast()
            .reclaim(Reclaim { every_bytes: 0 })
            .open(&path)
            .unwrap();
        if let Err(error) = db.verify_typed() {
            odd.push(format!("verify_typed after a reopen: {error:?}"));
        }
        assert!(odd.is_empty(), "round {round}: {odd:#?}");
    }
}
