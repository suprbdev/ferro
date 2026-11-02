mod common;

use common::{fast, text};
use ferro::Map;

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
    let before = db.size_on_disk();
    db.update(|root| {
        root.insert(2, copy);
    })
    .unwrap();
    let grown = db.size_on_disk() - before;
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
    let before = db.size_on_disk();
    db.update(|root| {
        root.get_mut(&1).unwrap();
        root.insert(3, "small".to_string());
    })
    .unwrap();
    let grown = db.size_on_disk() - before;
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
    let before = db.size_on_disk();
    db.update(|root| {
        root.insert(1, (*removed).clone());
    })
    .unwrap();
    let grown = db.size_on_disk() - before;
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
