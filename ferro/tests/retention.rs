mod common;

use common::fast;
use ferro::{Db, Error, Keep, Map, Options, Reclaim};

type Root = Map<u64, u64>;

fn options() -> Options {
    fast().reclaim(Reclaim { every_bytes: 0 })
}

fn put(db: &Db<Root>, key: u64) {
    db.update(|root| {
        root.insert(key, key);
    })
    .unwrap();
}

fn answers(db: &Db<Root>) -> Vec<String> {
    let mut out = Vec::new();
    let last = db.version();
    for version in 0..=last {
        out.push(match db.at(version) {
            Ok(snapshot) => format!("at {version}: v{} len {}", snapshot.version(), snapshot.len()),
            Err(error) => format!("at {version}: {error}"),
        });
    }
    let mut times = vec![0, 1];
    times.extend(db.log().values().map(|commit| commit.time_ms));
    times.push(u64::MAX);
    for time in times {
        out.push(match db.as_of(time) {
            Ok(snapshot) => format!("as_of {time}: v{}", snapshot.version()),
            Err(error) => format!("as_of {time}: {error}"),
        });
    }
    let history: Vec<u64> = db.history_of(|root| root.len()).map(|(commit, _)| commit.version).collect();
    out.push(format!("history {history:?}"));
    out
}

fn reachable(db: &Db<Root>) -> Vec<u64> {
    (0..=db.version()).filter(|version| db.at(*version).is_ok()).collect()
}

#[test]
fn version_zero_is_retained_exactly_as_long_as_version_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.ferro");
    let db: Db<Root> = options().history(Keep::Last(3)).open(&path).unwrap();
    assert_eq!(db.at(0).unwrap().version(), 0);
    assert_eq!(db.as_of(0).unwrap().version(), 0);
    put(&db, 1);
    put(&db, 2);
    assert_eq!(reachable(&db), vec![0, 1, 2]);
    assert_eq!(db.as_of(0).unwrap().version(), 0);
    for key in 3..=6 {
        put(&db, key);
    }
    assert_eq!(db.log().first().unwrap().0, 4);
    assert_eq!(reachable(&db), vec![4, 5, 6]);
    assert!(matches!(db.at(0), Err(Error::NoVersion(0))));
    assert!(matches!(db.at(3), Err(Error::NoVersion(3))));
    assert!(matches!(db.as_of(0), Err(Error::NoVersion(3))));
    assert!(matches!(db.as_of(u64::MAX), Ok(snapshot) if snapshot.version() == 6));
    let fork = db.fork("fork").unwrap();
    assert_eq!(reachable(&fork), vec![4, 5, 6]);
    assert!(matches!(fork.at(0), Err(Error::NoVersion(0))));
    assert!(matches!(fork.as_of(0), Err(Error::NoVersion(3))));
    assert_eq!(answers(&fork), answers(&db));
    let pruned = answers(&db);
    db.compact(Keep::Last(3)).unwrap();
    assert_eq!(answers(&db), pruned);
    assert_eq!(answers(&fork), pruned);
    drop((db, fork));
    let db: Db<Root> = options().history(Keep::Last(3)).open(&path).unwrap();
    assert_eq!(answers(&db), pruned);
    assert_eq!(answers(&db.branch("fork").unwrap()), pruned);
}

#[test]
fn without_pruning_version_zero_stays_the_default_value() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = options().open(dir.path().join("a.ferro")).unwrap();
    for key in 1..=5 {
        put(&db, key);
    }
    db.compact(Keep::All).unwrap();
    assert_eq!(reachable(&db), vec![0, 1, 2, 3, 4, 5]);
    assert!(db.at(0).unwrap().is_empty());
    assert_eq!(db.as_of(0).unwrap().version(), 0);
    let fork = db.fork("fork").unwrap();
    assert!(fork.at(0).unwrap().is_empty());
    assert_eq!(answers(&fork), answers(&db));
    assert_eq!(db.history_of(|root| root.len()).count(), 5);
}

#[test]
fn a_branch_forked_before_any_commit_keeps_version_zero_while_another_prunes() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = options().history(Keep::Last(2)).open(dir.path().join("e.ferro")).unwrap();
    let early = db.fork("early").unwrap();
    for key in 1..=5 {
        put(&db, key);
    }
    put(&early, 100);
    assert_eq!(reachable(&early), vec![0, 1]);
    assert_eq!(reachable(&db), vec![4, 5]);
    db.compact(Keep::Last(2)).unwrap();
    assert_eq!(reachable(&early), vec![0, 1]);
    assert_eq!(reachable(&db), vec![4, 5]);
}
