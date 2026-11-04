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
    assert_eq!(
        log.get(&1).unwrap().messages().collect::<Vec<_>>(),
        vec!["first"]
    );
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

fn counter_merge(db: &Db<Counters>, other: &Db<Counters>, message: &str) -> String {
    db.merge_with(other, |root, base, theirs| {
        for key in 0..3u8 {
            let delta = theirs.get(&key).map_or(0, |v| *v) - base.get(&key).map_or(0, |v| *v);
            if delta != 0 {
                let current = root.get(&key).map_or(0, |v| *v);
                root.insert(key, current + delta);
            }
        }
        (message.to_string(), base.commit().message.clone())
    })
    .unwrap()
    .value
}

fn values(db: &Db<Counters>) -> Vec<(u8, i64)> {
    db.read().iter().map(|(k, v)| (k, *v)).collect()
}

#[test]
fn a_merge_recorded_before_merge_links_still_sets_the_base() {
    let (_dir, path) = legacy();
    let main: Db<Counters> = Db::open(&path).unwrap();
    let side = main.branch("side").unwrap();
    assert_eq!(
        main.common_ancestor(&side).unwrap().commit().message,
        "side one"
    );
    assert_eq!(
        side.common_ancestor(&main).unwrap().commit().message,
        "side one"
    );
    assert_eq!(counter_merge(&main, &side, "merge side again"), "side one");
    assert_eq!(values(&main), vec![(0, 3), (1, 1010), (2, 100)]);
    assert_eq!(counter_merge(&side, &main, "merge main"), "side two");
    assert_eq!(values(&side), vec![(0, 3), (1, 1010), (2, 100)]);
    main.compact(ferro::Keep::All).unwrap();
    main.verify().unwrap();
    assert_eq!(
        main.common_ancestor(&side).unwrap().commit().message,
        "merge side again"
    );
}

fn bump(db: &Db<Counters>, key: u8, by: i64) {
    db.update(|root| {
        let current = root.get(&key).map_or(0, |v| *v);
        root.insert(key, current + by);
    })
    .unwrap();
}

#[test]
fn a_criss_cross_over_a_merge_recorded_before_merge_links_applies_each_change_once() {
    let (_dir, path) = legacy();
    let main: Db<Counters> = Db::open(&path).unwrap();
    let side = main.branch("side").unwrap();
    let ours = main.fork("ours").unwrap();
    let theirs = side.fork("theirs").unwrap();
    assert_eq!(counter_merge(&ours, &theirs, "ours <- theirs"), "side one");
    assert_eq!(counter_merge(&theirs, &main, "theirs <- main"), "side one");
    bump(&ours, 0, 10_000);
    bump(&theirs, 2, 20_000);
    assert_eq!(ours.common_ancestors(&theirs).unwrap().len(), 2);
    counter_merge(&ours, &theirs, "ours <- theirs again");
    assert_eq!(values(&ours), vec![(0, 10_003), (1, 1010), (2, 20_100)]);
    main.verify().unwrap();
}
