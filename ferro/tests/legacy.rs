use std::path::{Path, PathBuf};

use ferro::{Db, Map, Options};
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

type Tally = Map<String, u64>;

struct Branch {
    name: &'static str,
    version: u64,
    first: u64,
    message: &'static str,
    values: &'static [(&'static str, u64)],
}

const TABLE_BRANCHES: [Branch; 4] = [
    Branch {
        name: "main",
        version: 10,
        first: 4,
        message: "last",
        values: &[("a", 1111), ("b", 10), ("c", 7), ("d", 3), ("p", 11)],
    },
    Branch {
        name: "peer",
        version: 2,
        first: 1,
        message: "peer 2",
        values: &[("p", 11)],
    },
    Branch {
        name: "quiet",
        version: 9,
        first: 4,
        message: "after cycle 2",
        values: &[("a", 1110), ("b", 10), ("c", 7), ("d", 3), ("p", 11)],
    },
    Branch {
        name: "side",
        version: 10,
        first: 6,
        message: "side after cycle",
        values: &[("a", 110), ("b", 1016)],
    },
];

fn branch_table() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("branch-table.ferro");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/branch-table.db");
    std::fs::copy(fixture, &path).unwrap();
    (dir, path)
}

fn tally(db: &Db<Tally>) -> Vec<(String, u64)> {
    db.read().iter().map(|(k, v)| (k, *v)).collect()
}

fn check_table_branches(db: &Db<Tally>, bumped: u64) {
    assert_eq!(db.branches(), vec!["main", "peer", "quiet", "side"]);
    for expected in &TABLE_BRANCHES {
        let branch = db.branch(expected.name).unwrap();
        let moved = u64::from(expected.name == "main" || expected.name == "side") * bumped;
        assert_eq!(
            branch.version(),
            expected.version + moved,
            "{}",
            expected.name
        );
        assert_eq!(
            branch.log().first().map(|(version, _)| version),
            Some(expected.first),
            "{}",
            expected.name
        );
        let mut values: Vec<(String, u64)> = expected
            .values
            .iter()
            .map(|(key, value)| (key.to_string(), *value))
            .collect();
        if moved > 0 {
            values.push(("moved".to_string(), moved));
            values.sort();
        } else {
            assert_eq!(branch.read().commit().message, expected.message);
        }
        assert_eq!(tally(&branch), values, "{}", expected.name);
    }
    let peer = db.branch("peer").unwrap();
    assert_eq!(
        db.common_ancestor(&peer).unwrap().commit().message,
        "peer 2"
    );
}

fn move_on(db: &Db<Tally>, name: &str) {
    db.branch(name)
        .unwrap()
        .update(|root| {
            let moved = root.get("moved").map_or(0, |value| *value);
            root.insert("moved".to_string(), moved + 1);
        })
        .unwrap();
}

#[test]
fn a_file_whose_heads_all_sit_in_one_table_opens_and_moves_on_to_steps() {
    let (_dir, path) = branch_table();
    let follower = Options::new().read_only().open::<Tally>(&path).unwrap();
    check_table_branches(&follower, 0);
    let db: Db<Tally> = Db::open(&path).unwrap();
    check_table_branches(&db, 0);
    db.verify().unwrap();
    db.verify_typed().unwrap();
    for round in 1..=3 {
        move_on(&db, "main");
        move_on(&db, "side");
        check_table_branches(&db, round);
        assert!(follower.refresh().unwrap());
        check_table_branches(&follower, round);
    }
    db.verify().unwrap();
    db.verify_typed().unwrap();
    drop((db, follower));
    let db: Db<Tally> = Db::open(&path).unwrap();
    check_table_branches(&db, 3);
    let read_only = Options::new().read_only().open::<Tally>(&path).unwrap();
    check_table_branches(&read_only, 3);
    db.compact(ferro::Keep::All).unwrap();
    check_table_branches(&db, 3);
    move_on(&db, "main");
    move_on(&db, "side");
    drop(db);
    let db: Db<Tally> = Db::open(&path).unwrap();
    check_table_branches(&db, 4);
    db.verify_typed().unwrap();
    let info = ferro::admin::info(&path);
    drop(db);
    let names: Vec<String> = info.unwrap().branches.into_iter().map(|b| b.name).collect();
    assert_eq!(names, vec!["main", "peer", "quiet", "side"]);
    ferro::admin::verify(&path).unwrap();
}
