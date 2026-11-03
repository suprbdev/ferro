use std::path::{Path, PathBuf};

use ferro::{Db, Map};
use tempfile::TempDir;

type Counters = Map<u8, i64>;

fn legacy() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.ferro");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy.db");
    std::fs::copy(fixture, &path).unwrap();
    (dir, path)
}

#[test]
fn messages_written_before_groups_were_recorded_still_split_on_newlines() {
    let (_dir, path) = legacy();
    let db: Db<Counters> = Db::open(&path).unwrap();
    let log = db.log();
    assert_eq!(log.get(&1).unwrap().messages().collect::<Vec<_>>(), vec!["first"]);
    let joined = log.get(&2).unwrap();
    assert_eq!(joined.message, "one\ntwo");
    assert_eq!(joined.messages().collect::<Vec<_>>(), vec!["one", "two"]);
    db.commit("three\nfour", |root| {
        root.insert(9, 9);
    })
    .unwrap();
    assert_eq!(
        db.log().get(&5).unwrap().messages().collect::<Vec<_>>(),
        vec!["three\nfour"]
    );
    assert_eq!(log.get(&2).unwrap().messages().count(), 2);
    db.verify().unwrap();
}
