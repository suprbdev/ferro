#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::task::{Context, Waker};
use std::thread;
use std::time::Duration;

use common::fast;
use ferro::db::Hook;
use ferro::{Data, Db, Durability, Error, Keep, Map, Options, Reclaim};

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
