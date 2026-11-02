mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;

use common::fast;
use ferro::admin::{self, Keep};
use ferro::{Db, Error, Map};

type Root = Map<u64, u64>;

fn put(db: &Db<Root>, key: u64, value: u64) {
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
}

fn open(dir: &tempfile::TempDir) -> Db<Root> {
    fast().open::<Root>(dir.path().join("b.ferro")).unwrap()
}

#[test]
fn a_dropped_branch_is_gone_and_stays_gone() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    put(&db, 1, 1);
    let side = db.fork("side").unwrap();
    put(&side, 2, 2);
    drop(side);
    db.drop_branch("side").unwrap();
    assert_eq!(db.branches(), vec!["main".to_string()]);
    assert!(matches!(db.branch("side"), Err(Error::NoBranch(_))));
    assert_eq!(db.version(), 1);
    db.verify().unwrap();
    drop(db);
    let db = open(&dir);
    assert_eq!(db.branches(), vec!["main".to_string()]);
    assert!(matches!(db.branch("side"), Err(Error::NoBranch(_))));
    assert_eq!(*db.read().get(&1).unwrap(), 1);
}

#[test]
fn main_and_the_handles_own_branch_cannot_be_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    put(&db, 1, 1);
    let side = db.fork("side").unwrap();
    assert!(matches!(db.drop_branch("main"), Err(Error::BranchInUse(_))));
    assert!(matches!(side.drop_branch("main"), Err(Error::BranchInUse(_))));
    assert!(matches!(side.drop_branch("side"), Err(Error::BranchInUse(_))));
    assert!(matches!(db.drop_branch("ghost"), Err(Error::NoBranch(_))));
    let reader = fast().read_only().open::<Root>(dir.path().join("b.ferro")).unwrap();
    assert!(matches!(reader.drop_branch("side"), Err(Error::ReadOnly)));
    assert!(db.branches().contains(&"side".to_string()));
}

#[test]
fn another_handle_keeps_reading_but_cannot_write_or_resurrect_the_branch() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    put(&db, 1, 1);
    let side = db.fork("side").unwrap();
    put(&side, 2, 2);
    let before = side.read();
    db.drop_branch("side").unwrap();

    assert_eq!(side.version(), 2);
    assert_eq!(*side.read().get(&2).unwrap(), 2);
    assert_eq!(*side.at(1).unwrap().get(&1).unwrap(), 1);
    assert_eq!(side.log().len(), 2);
    assert_eq!(*before.get(&2).unwrap(), 2);

    assert!(matches!(
        side.update(|root| root.insert(3, 3)),
        Err(Error::NoBranch(_))
    ));
    assert!(matches!(
        side.try_commit("nope", |root| {
            root.insert(3, 3);
            Ok::<_, Error>(())
        }),
        Err(Error::NoBranch(_))
    ));
    assert!(matches!(side.fork("child"), Err(Error::NoBranch(_))));
    assert!(matches!(
        side.restore(&side.at(1).unwrap()),
        Err(Error::NoBranch(_))
    ));
    assert_eq!(db.branches(), vec!["main".to_string()]);
    assert!(matches!(db.branch("side"), Err(Error::NoBranch(_))));
}

#[test]
fn watchers_of_a_dropped_branch_end_and_others_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    put(&db, 1, 1);
    let side = db.fork("side").unwrap();
    let on_side = side.watch();
    let on_main = db.watch();
    let parked = thread::spawn(move || on_side.recv().is_err());
    db.drop_branch("side").unwrap();
    assert!(parked.join().unwrap());
    assert!(matches!(
        on_main.recv_timeout(Duration::from_millis(5)),
        Err(RecvTimeoutError::Timeout)
    ));
    put(&db, 2, 2);
    assert_eq!(on_main.try_recv().unwrap().version(), 2);
}

#[test]
fn a_new_branch_of_the_same_name_does_not_inherit_the_old_handles() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    put(&db, 1, 1);
    let old = db.fork("side").unwrap();
    db.drop_branch("side").unwrap();
    let new = db.fork("side").unwrap();
    assert!(matches!(old.update(|root| root.insert(5, 5)), Err(Error::NoBranch(_))));
    put(&new, 6, 6);
    assert_eq!(new.version(), 2);
    assert_eq!(db.branch("side").unwrap().version(), 2);
    assert_eq!(old.version(), 1);
    assert!(old.read().get(&6).is_none());
}

fn merge_counting_conflicts(ours: &Db<Root>, theirs: &Db<Root>) -> (usize, usize) {
    let mut conflicts = 0;
    let done = ours
        .merge_with(theirs, |root, base, other| {
            let applied = root.merge(base, other, |conflict| {
                conflicts += 1;
                conflict.ours.as_deref().copied()
            });
            ("merge", applied)
        })
        .unwrap();
    (done.value, conflicts)
}

#[test]
fn a_merge_record_does_not_outlive_the_branch_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    put(&db, 1, 10);
    let draft = db.fork("draft").unwrap();
    put(&draft, 2, 20);
    assert_eq!(merge_counting_conflicts(&db, &draft), (1, 0));
    let inherited = db.fork("inherited").unwrap();
    drop(draft);

    db.drop_branch("draft").unwrap();
    put(&db, 1, 11);
    drop(inherited);
    drop(db);
    let db = open(&dir);

    let draft = db.fork("draft").unwrap();
    put(&db, 1, 12);
    assert_eq!(merge_counting_conflicts(&db, &draft), (0, 0));
    assert_eq!(*db.read().get(&1).unwrap(), 12);

    let inherited = db.branch("inherited").unwrap();
    assert_eq!(merge_counting_conflicts(&db, &inherited).1, 0);
    assert_eq!(*db.read().get(&1).unwrap(), 12);
}

#[test]
fn a_dropped_branch_cannot_be_merged_from_a_stale_handle() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    put(&db, 1, 1);
    let side = db.fork("side").unwrap();
    put(&side, 2, 2);
    db.drop_branch("side").unwrap();
    let merged = db.merge_with(&side, |root, base, theirs| {
        let applied = root.merge(base, theirs, |_| None);
        ("merge", applied)
    });
    assert!(matches!(merged, Err(Error::NoBranch(_))));
    assert_eq!(db.version(), 1);
    assert!(db.read().get(&2).is_none());
}

#[test]
fn a_read_only_handle_learns_of_the_drop_on_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    let db = fast().open::<Root>(&path).unwrap();
    put(&db, 1, 1);
    let side = db.fork("side").unwrap();
    put(&side, 2, 2);
    let reader = fast().read_only().open::<Root>(&path).unwrap();
    let handle = reader.branch("side").unwrap();
    let watcher = handle.watch();
    assert_eq!(handle.version(), 2);

    db.drop_branch("side").unwrap();
    assert!(reader.refresh().unwrap());

    assert_eq!(reader.branches(), vec!["main".to_string()]);
    assert_eq!(handle.version(), 2);
    assert_eq!(*handle.read().get(&2).unwrap(), 2);
    assert!(matches!(reader.branch("side"), Err(Error::NoBranch(_))));
    assert!(watcher.recv().is_err());

    let side = db.fork("side").unwrap();
    assert_eq!(side.version(), 1);
    assert!(reader.refresh().unwrap());
    assert_eq!(reader.branch("side").unwrap().version(), 1);
    assert_eq!(handle.version(), 2);
}

#[test]
fn compaction_reclaims_a_dropped_branch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    let db = fast().open::<Root>(&path).unwrap();
    put(&db, 1, 1);
    let side = db.fork("side").unwrap();
    for n in 0..200 {
        put(&side, n, n);
    }
    let kept = {
        let copy = dir.path().join("kept.ferro");
        drop(side);
        drop(db);
        std::fs::copy(&path, &copy).unwrap();
        admin::compact(&copy, Keep::All).unwrap().after_bytes
    };
    let db = fast().open::<Root>(&path).unwrap();
    db.drop_branch("side").unwrap();
    drop(db);
    let dropped = admin::compact(&path, Keep::All).unwrap().after_bytes;
    assert!(dropped * 2 < kept, "{dropped} {kept}");
    let db = fast().open::<Root>(&path).unwrap();
    db.verify().unwrap();
    assert_eq!(db.branches(), vec!["main".to_string()]);
}

#[test]
fn a_writer_racing_the_drop_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    let db = fast().open::<Root>(&path).unwrap();
    put(&db, 1, 1);
    for round in 0..20 {
        let name = format!("side-{round}");
        let side = db.fork(&name).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (side, stop) = (side.clone(), stop.clone());
            thread::spawn(move || {
                let mut n = 0;
                while !stop.load(Ordering::Relaxed) {
                    n += 1;
                    match side.update(|root| {
                        root.insert(2, n);
                    }) {
                        Ok(()) => {}
                        Err(Error::NoBranch(_)) => return true,
                        Err(other) => panic!("{other}"),
                    }
                }
                false
            })
        };
        thread::sleep(Duration::from_micros(200));
        db.drop_branch(&name).unwrap();
        stop.store(true, Ordering::Relaxed);
        assert!(writer.join().unwrap() || side.update(|_| ()).is_err());
        assert_eq!(db.branches(), vec!["main".to_string()]);
    }
    db.verify().unwrap();
    drop(db);
    let db = fast().open::<Root>(&path).unwrap();
    assert_eq!(db.branches(), vec!["main".to_string()]);
}
