#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::task::{Context, Waker};
use std::thread;
use std::time::{Duration, Instant};

use common::fast;
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
