mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use common::fast;
use ferro::{Db, Map};

type Root = Map<u64, u64>;

fn bump(db: &Db<Root>, n: u64) {
    db.update(|root| {
        root.insert(0, n);
    })
    .unwrap();
}

#[test]
fn a_stuck_watcher_holds_only_the_newest_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Root>(dir.path().join("w.ferro")).unwrap();
    let watcher = db.watch();
    for n in 1..=1000 {
        bump(&db, n);
    }
    let snapshot = watcher.try_recv().unwrap();
    assert_eq!(snapshot.version(), 1000);
    assert_eq!(*snapshot.get(&0).unwrap(), 1000);
    assert!(matches!(watcher.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn recv_waits_for_a_snapshot_newer_than_the_last_returned() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Root>(dir.path().join("w.ferro")).unwrap();
    bump(&db, 1);
    let watcher = db.watch();
    assert!(matches!(
        watcher.recv_timeout(Duration::from_millis(5)),
        Err(RecvTimeoutError::Timeout)
    ));
    let (done, finished) = channel();
    let waiting = thread::spawn(move || {
        let snapshot = watcher.recv().unwrap();
        done.send(snapshot.version()).unwrap();
        watcher
    });
    assert!(finished.recv_timeout(Duration::from_millis(20)).is_err());
    bump(&db, 2);
    assert_eq!(finished.recv_timeout(Duration::from_secs(10)).unwrap(), 2);
    let watcher = waiting.join().unwrap();
    assert!(matches!(
        watcher.recv_timeout(Duration::from_millis(5)),
        Err(RecvTimeoutError::Timeout)
    ));
    assert!(matches!(watcher.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn each_watcher_keeps_its_own_place() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Root>(dir.path().join("w.ferro")).unwrap();
    let (a, b) = (db.watch(), db.watch());
    bump(&db, 1);
    assert_eq!(a.try_recv().unwrap().version(), 1);
    bump(&db, 2);
    assert_eq!(b.try_recv().unwrap().version(), 2);
    assert_eq!(a.try_recv().unwrap().version(), 2);
    let late = db.watch();
    assert!(matches!(late.try_recv(), Err(TryRecvError::Empty)));
    drop(a);
    bump(&db, 3);
    assert_eq!(b.try_recv().unwrap().version(), 3);
    assert_eq!(late.try_recv().unwrap().version(), 3);
}

#[test]
fn a_commit_that_changes_nothing_does_not_fire() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Root>(dir.path().join("w.ferro")).unwrap();
    bump(&db, 1);
    let watcher = db.watch();
    bump(&db, 1);
    assert!(matches!(watcher.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn a_parked_watcher_ends_when_the_last_handle_goes() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Root>(dir.path().join("w.ferro")).unwrap();
    let fork = db.fork("side").unwrap();
    let watcher = db.watch();
    let (done, finished) = channel();
    thread::spawn(move || done.send(watcher.recv().is_err()).unwrap());
    let other = db.clone();
    drop(db);
    assert!(finished.recv_timeout(Duration::from_millis(30)).is_err());
    drop(other);
    assert!(finished.recv_timeout(Duration::from_millis(30)).is_err());
    drop(fork);
    assert!(finished.recv_timeout(Duration::from_secs(10)).unwrap());
}

#[test]
fn a_watcher_with_an_unseen_snapshot_does_not_hold_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.ferro");
    let db = fast().open::<Root>(&path).unwrap();
    let watcher = db.watch();
    bump(&db, 1);
    drop(db);
    let reopened = fast().open::<Root>(&path).unwrap();
    assert_eq!(reopened.version(), 1);
    assert!(matches!(watcher.try_recv(), Err(TryRecvError::Disconnected)));
    assert!(matches!(
        watcher.recv_timeout(Duration::from_secs(10)),
        Err(RecvTimeoutError::Disconnected)
    ));
    assert!(watcher.recv().is_err());
}

#[test]
fn iterating_a_watcher_ends_with_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Root>(dir.path().join("w.ferro")).unwrap();
    let watcher = db.watch();
    let consumer = thread::spawn(move || watcher.map(|snapshot| snapshot.version()).max());
    for n in 1..=50 {
        bump(&db, n);
    }
    while db.version() < 50 {
        thread::yield_now();
    }
    drop(db);
    let last = consumer.join().unwrap();
    assert!(last.is_none_or(|version| version <= 50));
}

#[test]
fn a_read_only_watcher_coalesces_across_refreshes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.ferro");
    let db = fast().open::<Root>(&path).unwrap();
    let reader = fast().read_only().open::<Root>(&path).unwrap();
    let watcher = reader.watch();
    for n in 1..=3 {
        bump(&db, n);
        assert!(reader.refresh().unwrap());
    }
    assert_eq!(watcher.try_recv().unwrap().version(), 3);
    assert!(matches!(watcher.try_recv(), Err(TryRecvError::Empty)));
    assert!(!reader.refresh().unwrap());
    assert!(matches!(watcher.try_recv(), Err(TryRecvError::Empty)));
    drop(reader);
    assert!(matches!(watcher.try_recv(), Err(TryRecvError::Disconnected)));
}

fn parked(calls: &AtomicUsize, before: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while calls.load(Ordering::SeqCst) <= before {
        assert!(Instant::now() < deadline, "the test never ran on the commit path");
        thread::sleep(Duration::from_millis(1));
    }
}

fn opened() -> (tempfile::TempDir, Db<Root>) {
    let dir = tempfile::tempdir().unwrap();
    let db = common::fast().open::<Root>(dir.path().join("watch.ferro")).unwrap();
    (dir, db)
}

fn put(db: &Db<Root>, key: u64, value: u64) {
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
}

#[test]
fn a_closer_ends_a_parked_recv_without_a_commit() {
    let (_dir, db) = opened();
    let watcher = db.watch();
    let closer = watcher.closer();
    let parked = thread::spawn(move || watcher.recv().is_err());
    thread::sleep(Duration::from_millis(30));
    let version = db.version();
    closer.close();
    assert!(parked.join().unwrap());
    assert_eq!(db.version(), version);
}

#[test]
fn closing_reaches_every_clone_and_no_other_watcher() {
    let (_dir, db) = opened();
    let first = db.watch();
    let second = first.clone();
    let other = db.watch();
    let closer = first.closer().clone();
    let parked: Vec<_> = [first, second]
        .into_iter()
        .map(|watcher| thread::spawn(move || watcher.recv().is_err()))
        .collect();
    thread::sleep(Duration::from_millis(30));
    closer.close();
    for handle in parked {
        assert!(handle.join().unwrap());
    }
    put(&db, 1, 1);
    assert_eq!(other.recv_timeout(Duration::from_secs(5)).unwrap().version(), 1);
    let late = db.watch();
    put(&db, 1, 2);
    assert_eq!(late.recv_timeout(Duration::from_secs(5)).unwrap().version(), 2);
}

#[test]
fn a_closed_watcher_reports_it_every_way() {
    let (_dir, db) = opened();
    let mut watcher = db.watch();
    let timed = watcher.clone();
    let tried = watcher.clone();
    put(&db, 1, 1);
    watcher.closer().close();
    assert!(watcher.next().is_none());
    assert!(matches!(
        timed.recv_timeout(Duration::from_secs(1)),
        Err(RecvTimeoutError::Disconnected)
    ));
    assert!(matches!(tried.try_recv(), Err(TryRecvError::Disconnected)));
}

#[test]
fn a_closer_outlives_the_database() {
    let (_dir, db) = opened();
    let watcher = db.watch();
    let closer = watcher.closer();
    drop(db);
    assert!(watcher.recv().is_err());
    closer.close();
}

#[test]
fn a_closer_wakes_a_parked_timeout_and_a_filtered_watcher() {
    let (_dir, db) = opened();
    let timed = db.watch();
    let filtered = db.watch_when(|_| false);
    let closers = [timed.closer(), filtered.closer()];
    let started = Instant::now();
    let timed = thread::spawn(move || timed.recv_timeout(Duration::from_secs(60)).is_err());
    let filtered = thread::spawn(move || filtered.recv_timeout(Duration::from_secs(60)).is_err());
    thread::sleep(Duration::from_millis(30));
    for closer in &closers {
        closer.close();
    }
    assert!(timed.join().unwrap());
    assert!(filtered.join().unwrap());
    assert!(started.elapsed() < Duration::from_secs(30));
}

#[test]
fn a_filtered_watcher_sleeps_through_commits_that_do_not_concern_it() {
    let (_dir, db) = opened();
    let (on_commit, on_waiter) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let waiter: Arc<Mutex<Option<ThreadId>>> = Arc::default();
    let watcher = {
        let (on_commit, on_waiter, waiter) = (on_commit.clone(), on_waiter.clone(), waiter.clone());
        db.watch_when(move |snapshot| {
            let counter = match *waiter.lock().unwrap() == Some(thread::current().id()) {
                true => &on_waiter,
                false => &on_commit,
            };
            counter.fetch_add(1, Ordering::SeqCst);
            snapshot.contains_key(&7)
        })
    };
    put(&db, 0, 0);
    let parked_on = {
        let waiter = waiter.clone();
        thread::spawn(move || {
            *waiter.lock().unwrap() = Some(thread::current().id());
            watcher.recv().unwrap().version()
        })
    };
    let mut tries = 0;
    while on_commit.load(Ordering::SeqCst) == 0 {
        tries += 1;
        assert!(tries < 20_000, "the watcher never parked");
        put(&db, 1, tries);
    }
    let woken = on_waiter.load(Ordering::SeqCst);
    let evaluated = on_commit.load(Ordering::SeqCst);
    for i in 0..200 {
        put(&db, 2, i);
    }
    assert_eq!(on_waiter.load(Ordering::SeqCst), woken, "woken by a commit it does not want");
    assert_eq!(on_commit.load(Ordering::SeqCst), evaluated + 200);
    put(&db, 7, 7);
    assert_eq!(parked_on.join().unwrap(), db.version());
}

#[test]
fn a_filtered_watcher_sees_commits_that_landed_before_it_parked() {
    let (_dir, db) = opened();
    let watcher = db.watch_when(|snapshot| snapshot.contains_key(&7));
    put(&db, 1, 1);
    assert!(matches!(watcher.try_recv(), Err(TryRecvError::Empty)));
    put(&db, 7, 7);
    put(&db, 2, 2);
    assert_eq!(watcher.try_recv().unwrap().version(), 3);
    assert!(matches!(watcher.try_recv(), Err(TryRecvError::Empty)));
    assert!(matches!(
        watcher.recv_timeout(Duration::from_millis(10)),
        Err(RecvTimeoutError::Timeout)
    ));
    put(&db, 3, 3);
    assert_eq!(watcher.recv_timeout(Duration::from_secs(5)).unwrap().version(), 4);
}

#[test]
fn a_filtered_watcher_ignores_a_commit_that_no_longer_passes_when_it_looks() {
    let (_dir, db) = opened();
    let watcher = db.watch_when(|snapshot| snapshot.contains_key(&7));
    put(&db, 7, 7);
    db.update(|root| {
        root.remove(&7);
    })
    .unwrap();
    assert!(matches!(watcher.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn a_panicking_test_cannot_wedge_commits() {
    common::quiet_ferro_panics();
    let (_dir, db) = opened();
    let calls = Arc::new(AtomicUsize::new(0));
    let watcher = {
        let calls = calls.clone();
        db.watch_when(move |snapshot| {
            calls.fetch_add(1, Ordering::SeqCst);
            if snapshot.version() >= 2 {
                panic!("ferro: test refused");
            }
            false
        })
    };
    let parked_thread = thread::spawn(move || watcher.recv().map(|snapshot| snapshot.version()));
    put(&db, 1, 1);
    let mut value = 0;
    while calls.load(Ordering::SeqCst) < 2 && !parked_thread.is_finished() {
        value += 1;
        put(&db, 2, value);
    }
    put(&db, 3, 3);
    assert!(db.version() >= 3);
    assert!(parked_thread.join().is_err());
    put(&db, 4, 4);
}

#[test]
fn a_test_runs_without_the_writer_lock() {
    let (_dir, db) = opened();
    let (calls, hold) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(1)));
    let watcher = {
        let (calls, hold) = (calls.clone(), hold.clone());
        db.watch_when(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            while hold.load(Ordering::SeqCst) == 1 {
                thread::sleep(Duration::from_millis(1));
            }
            false
        })
    };
    let parked_thread = thread::spawn(move || watcher.recv().is_err());
    thread::sleep(Duration::from_millis(30));
    let before = calls.load(Ordering::SeqCst);
    let leader = {
        let db = db.clone();
        thread::spawn(move || put(&db, 1, 1))
    };
    parked(&calls, before);
    let ran = Arc::new(AtomicUsize::new(0));
    let writer = {
        let (db, ran) = (db.clone(), ran.clone());
        thread::spawn(move || {
            db.update(|root| {
                root.insert(2, 2);
                ran.store(1, Ordering::SeqCst);
            })
            .unwrap();
        })
    };
    parked(&ran, 0);
    hold.store(0, Ordering::SeqCst);
    leader.join().unwrap();
    writer.join().unwrap();
    assert_eq!(db.version(), 2);
    drop(db);
    assert!(parked_thread.join().unwrap());
}

#[test]
fn a_filtered_watcher_ends_with_its_branch_and_its_database() {
    let (_dir, db) = opened();
    let side = db.fork("side").unwrap();
    let on_side = side.watch_when(|_| false);
    let on_main = db.watch_when(|_| false);
    let on_side = thread::spawn(move || on_side.recv().is_err());
    let on_main = thread::spawn(move || on_main.recv().is_err());
    thread::sleep(Duration::from_millis(30));
    drop(side);
    db.drop_branch("side").unwrap();
    assert!(on_side.join().unwrap());
    drop(db);
    assert!(on_main.join().unwrap());
}

#[test]
fn a_filtered_watcher_follows_a_read_only_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watch.ferro");
    let db = common::fast().open::<Root>(&path).unwrap();
    put(&db, 1, 1);
    let reader = common::fast().read_only().open::<Root>(&path).unwrap();
    let watcher = reader.watch_when(|snapshot| snapshot.contains_key(&7));
    put(&db, 2, 2);
    reader.refresh().unwrap();
    assert!(matches!(watcher.try_recv(), Err(TryRecvError::Empty)));
    put(&db, 7, 7);
    reader.refresh().unwrap();
    assert_eq!(watcher.try_recv().unwrap().version(), 3);
}

#[test]
fn a_read_only_test_may_open_a_branch_while_refresh_runs_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watch.ferro");
    let db = common::fast().open::<Root>(&path).unwrap();
    put(&db, 1, 1);
    db.fork("side").unwrap();
    let reader = common::fast().read_only().open::<Root>(&path).unwrap();
    let watcher = {
        let reader = reader.clone();
        reader.clone().watch_when(move |snapshot| {
            reader.branch("side").is_ok_and(|side| side.version() < snapshot.version())
        })
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let parked_thread = thread::spawn(move || watcher.recv().map(|snapshot| snapshot.version()));
    thread::sleep(Duration::from_millis(30));
    put(&db, 2, 2);
    let refresher = {
        let calls = calls.clone();
        thread::spawn(move || {
            let refreshed = reader.refresh().unwrap();
            calls.fetch_add(1, Ordering::SeqCst);
            refreshed
        })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while calls.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "refresh never returned from a test that opens a branch");
        thread::sleep(Duration::from_millis(1));
    }
    assert!(refresher.join().unwrap());
    assert_eq!(parked_thread.join().unwrap().unwrap(), 2);
}

fn within<R: Send + 'static>(what: &str, run: impl FnOnce() -> R + Send + 'static) -> R {
    let (done, finished) = channel();
    thread::spawn(move || {
        let _ = done.send(run());
    });
    match finished.recv_timeout(Duration::from_secs(20)) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => panic!("{what} deadlocked"),
        Err(RecvTimeoutError::Disconnected) => panic!("{what} panicked"),
    }
}

#[test]
fn a_test_may_commit_while_its_commit_publishes() {
    let (_dir, db) = opened();
    let fired = Arc::new(AtomicUsize::new(0));
    let watcher = {
        let (db, fired) = (db.clone(), fired.clone());
        db.clone().watch_when(move |snapshot| {
            if snapshot.contains_key(&1) && fired.fetch_add(1, Ordering::SeqCst) == 0 {
                put(&db, 2, 2);
            }
            snapshot.contains_key(&2)
        })
    };
    let parked_thread = thread::spawn(move || watcher.recv().map(|snapshot| snapshot.version()));
    thread::sleep(Duration::from_millis(30));
    let writer = db.clone();
    within("a commit whose test commits", move || put(&writer, 1, 1));
    assert_eq!(within("the watcher", move || parked_thread.join().unwrap()).unwrap(), 2);
    assert_eq!(db.version(), 2);
    assert!(fired.load(Ordering::SeqCst) >= 1);
}

#[test]
fn a_slow_test_holds_up_only_the_commit_that_ran_it() {
    let (_dir, db) = opened();
    let (calls, hold) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(1)));
    let watcher = {
        let (calls, hold) = (calls.clone(), hold.clone());
        db.watch_when(move |snapshot| {
            calls.fetch_add(1, Ordering::SeqCst);
            while !snapshot.contains_key(&2) && hold.load(Ordering::SeqCst) == 1 {
                thread::sleep(Duration::from_millis(1));
            }
            false
        })
    };
    let parked_thread = thread::spawn(move || watcher.recv().is_err());
    thread::sleep(Duration::from_millis(30));
    let before = calls.load(Ordering::SeqCst);
    let leader = {
        let db = db.clone();
        thread::spawn(move || put(&db, 1, 1))
    };
    parked(&calls, before);
    let writer = db.clone();
    within("a commit behind a slow test", move || put(&writer, 2, 2));
    assert_eq!(db.version(), 2);
    assert!(!leader.is_finished());
    hold.store(0, Ordering::SeqCst);
    within("the slow commit", move || leader.join().unwrap());
    drop(db);
    assert!(within("the watcher", move || parked_thread.join().unwrap()));
}

#[test]
fn a_read_only_test_may_refresh_while_refresh_runs_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watch.ferro");
    let db = common::fast().open::<Root>(&path).unwrap();
    put(&db, 1, 1);
    let reader = common::fast().read_only().open::<Root>(&path).unwrap();
    let watcher = {
        let reader = reader.clone();
        reader.clone().watch_when(move |snapshot| {
            reader.refresh().is_ok() && snapshot.contains_key(&2)
        })
    };
    let parked_thread = thread::spawn(move || watcher.recv().map(|snapshot| snapshot.version()));
    thread::sleep(Duration::from_millis(30));
    put(&db, 2, 2);
    let refresher = reader.clone();
    assert!(within("a refresh whose test refreshes", move || refresher.refresh().unwrap()));
    assert_eq!(within("the watcher", move || parked_thread.join().unwrap()).unwrap(), 2);
    assert_eq!(reader.version(), 2);
}

#[test]
fn a_test_costs_commits_nothing_while_nobody_is_parked() {
    let (_dir, db) = opened();
    let calls = Arc::new(AtomicUsize::new(0));
    let watcher = {
        let calls = calls.clone();
        db.watch_when(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            true
        })
    };
    for i in 0..50 {
        put(&db, 1, i);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(watcher.try_recv().unwrap().version(), 50);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_dropped_filtered_watcher_is_forgotten() {
    let (_dir, db) = opened();
    let held = Arc::new(());
    let watcher = {
        let held = held.clone();
        db.watch_when(move |_| {
            let _ = &held;
            true
        })
    };
    assert_eq!(Arc::strong_count(&held), 2);
    drop(watcher);
    assert_eq!(Arc::strong_count(&held), 1);
    put(&db, 1, 1);
    put(&db, 1, 2);
}
