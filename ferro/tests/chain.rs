mod common;

use common::fast;
use ferro::admin::{self, Keep};
use ferro::{Db, Error, Map};

type Root = Map<u64, u64>;

fn build(path: &std::path::Path, commits: u64) -> Db<Root> {
    let db = fast().open::<Root>(path).unwrap();
    for i in 1..=commits {
        db.commit(format!("c{i}"), |root| {
            root.insert(0, i);
        })
        .unwrap();
    }
    db
}

#[test]
fn every_version_is_reachable_through_skip_pointers() {
    let dir = tempfile::tempdir().unwrap();
    let db = build(&dir.path().join("a.ferro"), 1500);
    assert_eq!(db.log().len(), 1500);
    for version in 1..=1500u64 {
        let snapshot = db.at(version).unwrap();
        assert_eq!(snapshot.version(), version);
        assert_eq!(*snapshot.get(&0).unwrap(), version);
        assert_eq!(
            db.log().get(version).unwrap().message,
            format!("c{version}")
        );
    }
    assert!(matches!(db.at(1501), Err(Error::NoVersion(1501))));
    let versions: Vec<u64> = db.log().iter().map(|(version, _)| version).collect();
    assert_eq!(versions, (1..=1500).collect::<Vec<_>>());
    assert_eq!(db.log().first().unwrap().0, 1);
    assert_eq!(db.log().last().unwrap().0, 1500);
}

#[test]
fn as_of_finds_the_newest_commit_not_after_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let db = build(&dir.path().join("a.ferro"), 700);
    let commits: Vec<_> = db.log().values().collect();
    for commit in &commits {
        let found = db.as_of(commit.time_ms).unwrap().version();
        let expected = commits
            .iter()
            .filter(|other| other.time_ms <= commit.time_ms)
            .map(|other| other.version)
            .max()
            .unwrap();
        assert_eq!(found, expected);
    }
    assert_eq!(db.as_of(0).unwrap().version(), 0);
    assert_eq!(db.as_of(u64::MAX).unwrap().version(), 700);
}

#[test]
fn log_survives_reopen_fork_and_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ferro");
    {
        let db = build(&path, 400);
        let side = db.fork("side").unwrap();
        side.update(|root| {
            root.insert(1, 1);
        })
        .unwrap();
        assert_eq!(side.log().len(), 401);
        assert_eq!(db.log().len(), 400);
        assert_eq!(side.common_ancestor(&db).unwrap().version(), 400);
    }
    let report = admin::compact(&path, Keep::Last(100)).unwrap();
    assert_eq!(report.commits_dropped, 601);
    let db = fast().open::<Root>(&path).unwrap();
    assert_eq!(db.log().len(), 100);
    assert_eq!(db.log().first().unwrap().0, 301);
    for version in 301..=400u64 {
        assert_eq!(*db.at(version).unwrap().get(&0).unwrap(), version);
    }
    assert!(matches!(db.at(300), Err(Error::NoVersion(300))));
    let side = db.branch("side").unwrap();
    assert_eq!(side.log().len(), 100);
    assert_eq!(side.common_ancestor(&db).unwrap().version(), 400);
    db.update(|root| {
        root.insert(0, 401);
    })
    .unwrap();
    assert_eq!(db.log().len(), 101);
    assert_eq!(*db.at(350).unwrap().get(&0).unwrap(), 350);
    db.verify().unwrap();
}

#[test]
fn compacting_a_long_chain_does_not_exhaust_the_stack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ferro");
    drop(build(&path, 20_000));
    let report = admin::compact(&path, Keep::All).unwrap();
    assert_eq!(report.commits_dropped, 0);
    let db = fast().open::<Root>(&path).unwrap();
    assert_eq!(db.log().len(), 20_000);
    assert_eq!(*db.at(1).unwrap().get(&0).unwrap(), 1);
    assert_eq!(*db.at(12_345).unwrap().get(&0).unwrap(), 12_345);
    db.verify().unwrap();
}
