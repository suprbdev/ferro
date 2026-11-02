mod common;

use std::sync::mpsc::{RecvTimeoutError, TryRecvError, channel};
use std::thread;
use std::time::Duration;

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
