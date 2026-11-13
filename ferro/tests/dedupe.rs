mod common;

use std::sync::Arc;

use common::{fast, text};
use ferro::db::Clock;
use ferro::{Db, Keep, Map, Options, Reclaim, Snapshot};

type Root = Map<u64, String>;

const LARGE: usize = 64 * 1024;

fn seeded(path: &std::path::Path) -> String {
    let value = text(1, LARGE);
    let db = fast().open::<Root>(path).unwrap();
    db.update(|root| {
        root.insert(1, value.clone());
    })
    .unwrap();
    value
}

#[test]
fn a_value_read_in_this_process_is_not_written_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.ferro");
    let value = seeded(&path);
    let db = fast().open::<Root>(&path).unwrap();
    let copy = (*db.read().get(&1).unwrap()).clone();
    let before = db.log_bytes();
    db.update(|root| {
        root.insert(2, copy);
    })
    .unwrap();
    let grown = db.log_bytes() - before;
    assert!(grown < (LARGE / 8) as u64, "{grown}");
    drop(db);
    let db = fast().open::<Root>(&path).unwrap();
    assert_eq!(*db.read().get(&2).unwrap(), value);
    db.verify().unwrap();
}

#[test]
fn a_loaded_value_that_is_encoded_again_keeps_its_object() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.ferro");
    let value = seeded(&path);
    let db = fast().open::<Root>(&path).unwrap();
    let before = db.log_bytes();
    db.update(|root| {
        root.get_mut(&1).unwrap();
        root.insert(3, "small".to_string());
    })
    .unwrap();
    let grown = db.log_bytes() - before;
    assert!(grown < (LARGE / 8) as u64, "{grown}");
    assert_eq!(*db.read().get(&1).unwrap(), value);
    db.verify().unwrap();
}

#[test]
fn a_removed_value_read_back_by_a_later_commit_keeps_its_object() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.ferro");
    let value = seeded(&path);
    let db = fast().open::<Root>(&path).unwrap();
    let removed = db.update(|root| root.remove(&1)).unwrap().unwrap();
    let before = db.log_bytes();
    db.update(|root| {
        root.insert(1, (*removed).clone());
    })
    .unwrap();
    let grown = db.log_bytes() - before;
    assert!(grown < (LARGE / 8) as u64, "{grown}");
    assert_eq!(*db.read().get(&1).unwrap(), value);
}

#[test]
fn a_deduplicated_commit_survives_a_reopen_and_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.ferro");
    let value = seeded(&path);
    {
        let db = fast().open::<Root>(&path).unwrap();
        let copy = (*db.read().get(&1).unwrap()).clone();
        db.update(|root| {
            root.remove(&1);
            root.insert(9, copy);
        })
        .unwrap();
    }
    ferro::admin::compact(&path, ferro::admin::Keep::All).unwrap();
    let db = fast().open::<Root>(&path).unwrap();
    assert_eq!(*db.read().get(&9).unwrap(), value);
    assert_eq!(*db.at(1).unwrap().get(&1).unwrap(), value);
    db.verify().unwrap();
}

fn same_clock() -> Options {
    fast()
        .reclaim(Reclaim::off())
        .clock(Clock(Arc::new(|| 1_700_000_000_000)))
}

fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    panic!("no merge expected")
}

fn first_kept(db: &Db<Root>) -> Option<u64> {
    db.log().first().map(|(version, _)| version)
}

#[test]
fn a_commit_equal_to_one_on_another_branch_keeps_the_history_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Root> = same_clock().open(dir.path().join("a.ferro")).unwrap();
    for key in 0..6 {
        a.commit(format!("c{key}"), |root| {
            root.insert(key, text(key, 100));
        })
        .unwrap();
    }
    let path = dir.path().join("b.ferro");
    let b: Db<Root> = same_clock().open(&path).unwrap();
    b.pull(&a, "a", refuse).unwrap();
    b.compact(Keep::Last(1)).unwrap();
    let pruned = b.fork("pruned").unwrap();
    b.drop_branch("a").unwrap();
    let empty: Db<Root> = same_clock().open(dir.path().join("empty.ferro")).unwrap();
    let everything = a.bundle(&empty.have().unwrap()).unwrap();
    b.receive("full", &everything, refuse).unwrap();
    let full = b.branch("full").unwrap();
    assert_eq!(first_kept(&pruned), Some(6));
    assert_eq!(first_kept(&full), Some(1));
    for db in [&pruned, &full] {
        db.commit("the same change", |root| {
            root.insert(99, "same".to_string());
        })
        .unwrap();
    }
    assert_eq!(pruned.head_hash(), full.head_hash());
    for version in 1..=7 {
        assert_eq!(full.at(version).unwrap().version(), version);
    }
    b.verify().unwrap();
    b.compact(Keep::All).unwrap();
    for version in 1..=7 {
        assert_eq!(full.at(version).unwrap().version(), version);
    }
    b.verify().unwrap();
    drop((pruned, full, b));
    let reopened: Db<Root> = same_clock().open(&path).unwrap();
    assert_eq!(first_kept(&reopened.branch("full").unwrap()), Some(1));
    reopened.verify().unwrap();
}
