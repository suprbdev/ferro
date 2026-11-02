mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Rng, fast};
use ferro::admin::{self, Keep};
use ferro::{Commit, Db, Map, Options};

type Root = Map<u64, u64>;
type Change = (Commit, Option<u64>);

fn seeded(path: &std::path::Path, commits: u64) -> Db<Root> {
    let db = fast().open::<Root>(path).unwrap();
    for i in 1..=commits {
        db.commit(format!("c{i}"), |root| {
            root.insert(0, i);
        })
        .unwrap();
    }
    db
}

fn smash_older_half(path: &std::path::Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let middle = bytes.len() / 2;
    bytes[8192..middle].fill(0xAA);
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn reverse_log_walk_reads_only_what_it_yields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ferro");
    drop(seeded(&path, 400));
    smash_older_half(&path);
    let db = Options::new().read_only().open::<Root>(&path).unwrap();
    let newest: Vec<u64> = db.log().iter().rev().take(5).map(|(version, _)| version).collect();
    assert_eq!(newest, vec![400, 399, 398, 397, 396]);
    assert!(catch_unwind(AssertUnwindSafe(|| db.log().iter().count())).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| db.log().iter().rev().count())).is_err());
}

#[test]
fn log_iterator_is_ordered_from_both_ends() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded(&dir.path().join("a.ferro"), 10);
    let forward: Vec<u64> = db.log().iter().map(|(version, _)| version).collect();
    assert_eq!(forward, (1..=10).collect::<Vec<_>>());
    let backward: Vec<u64> = db.log().values().rev().map(|commit| commit.version).collect();
    assert_eq!(backward, (1..=10).rev().collect::<Vec<_>>());

    let mut both = db.log().iter();
    assert_eq!(both.len(), 10);
    assert_eq!(both.next_back().unwrap().0, 10);
    assert_eq!(both.next_back().unwrap().0, 9);
    assert_eq!(both.next().unwrap().0, 1);
    assert_eq!(both.next_back().unwrap().0, 8);
    assert_eq!(both.next().unwrap().0, 2);
    assert_eq!(both.len(), 5);
    let middle: Vec<u64> = both.map(|(version, _)| version).collect();
    assert_eq!(middle, vec![3, 4, 5, 6, 7]);
}

fn naive(db: &Db<Root>, key: u64) -> Vec<(u64, Option<u64>)> {
    let log = db.log();
    let first = log.first().map_or(1, |(version, _)| version);
    let mut previous = if first == 1 { Some(db.at(0).unwrap().get(&key).map(|v| *v)) } else { None };
    let mut changes = Vec::new();
    for version in first..=db.version() {
        let value = db.at(version).unwrap().get(&key).map(|v| *v);
        if previous != Some(value) {
            changes.push((version, value));
        }
        previous = Some(value);
    }
    changes.reverse();
    changes
}

fn walked(db: &Db<Root>, key: u64) -> Vec<(u64, Option<u64>)> {
    db.history_of(|root| root.get(&key).map(|v| *v))
        .map(|(commit, value)| (commit.version, value))
        .collect()
}

#[test]
fn history_matches_a_commit_by_commit_comparison() {
    for seed in 1..=8 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.ferro");
        let db = fast().open::<Root>(&path).unwrap();
        let mut rng = Rng::new(seed);
        for i in 0..120u64 {
            db.commit(format!("c{i}"), |root| {
                root.insert(99, i);
                let key = rng.below(3);
                match rng.below(4) {
                    0 => {
                        root.remove(&key);
                    }
                    other => {
                        root.insert(key, other);
                    }
                }
            })
            .unwrap();
        }
        for key in 0..3 {
            assert_eq!(walked(&db, key), naive(&db, key));
        }
        drop(db);
        admin::compact(&path, Keep::Last(40)).unwrap();
        let db = fast().open::<Root>(&path).unwrap();
        for key in 0..3 {
            let pruned = walked(&db, key);
            assert_eq!(pruned, naive(&db, key));
            assert_eq!(pruned.last().unwrap().0, 81);
        }
    }
}

#[test]
fn a_value_that_returns_is_reported_every_time_it_changes() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Root>(dir.path().join("a.ferro")).unwrap();
    for value in [5u64, 5, 6, 5, 5, 6] {
        db.update(|root| {
            root.insert(1, value);
        })
        .unwrap();
        db.update(|root| {
            root.insert(2, value + 100);
        })
        .unwrap();
    }
    assert_eq!(
        walked(&db, 1),
        vec![(7, Some(6)), (5, Some(5)), (3, Some(6)), (1, Some(5))]
    );
}

fn with_late_row(path: &std::path::Path, commits: u64) -> Db<Root> {
    let db = fast().open::<Root>(path).unwrap();
    for i in 1..=commits {
        db.commit(format!("c{i}"), |root| {
            root.insert(0, i);
            if i == commits - 6 {
                root.insert(7, 1);
            }
            if i == commits - 2 {
                root.insert(7, 2);
            }
        })
        .unwrap();
    }
    db
}

#[test]
fn take_stops_the_walk_at_the_last_item_it_wants() {
    let dir = tempfile::tempdir().unwrap();
    let db = with_late_row(&dir.path().join("a.ferro"), 500);
    let steps = AtomicUsize::new(0);
    let first: Vec<_> = db
        .history_of(|root| {
            steps.fetch_add(1, Ordering::Relaxed);
            root.get(&7).map(|v| *v)
        })
        .take(1)
        .collect();
    assert_eq!(first[0].0.version, 498);
    assert!(steps.load(Ordering::Relaxed) <= 4, "{steps:?}");
}

#[test]
fn take_while_some_costs_the_age_of_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = with_late_row(&dir.path().join("a.ferro"), 500);
    let steps = AtomicUsize::new(0);
    let life: Vec<(u64, Option<u64>)> = db
        .history_of(|root| {
            steps.fetch_add(1, Ordering::Relaxed);
            root.get(&7).map(|v| *v)
        })
        .take_while(|(_, row)| row.is_some())
        .map(|(commit, row)| (commit.version, row))
        .collect();
    assert_eq!(life, vec![(498, Some(2)), (494, Some(1))]);
    assert!(steps.load(Ordering::Relaxed) <= 8, "{steps:?}");
}

#[test]
fn take_while_gives_the_same_items_as_the_plain_adaptor() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Root>(dir.path().join("a.ferro")).unwrap();
    let script: [(&str, Option<u64>); 8] = [
        ("one", Some(1)),
        ("noise", Some(1)),
        ("gone", None),
        ("again", Some(2)),
        ("noise", Some(2)),
        ("two", Some(3)),
        ("noise", Some(3)),
        ("three", Some(4)),
    ];
    for (i, (message, value)) in script.iter().enumerate() {
        db.commit(*message, |root| {
            root.insert(9, i as u64);
            match value {
                Some(value) => {
                    root.insert(7, *value);
                }
                None => {
                    root.remove(&7);
                }
            }
        })
        .unwrap();
    }
    let project = |root: &Root| root.get(&7).map(|v| *v);
    let compare = |keep: &dyn Fn(&Change) -> bool| {
        let fast: Vec<_> = db.history_of(project).take_while(keep).collect();
        let plain: Vec<_> = db.history_of(project).by_ref().take_while(keep).collect();
        assert_eq!(fast, plain);
        fast.len()
    };
    assert_eq!(compare(&|(_, row)| row.is_some()), 3);
    assert_eq!(compare(&|(commit, _)| commit.message != "two"), 1);
    assert_eq!(compare(&|(commit, _)| commit.version > 3), 3);
    assert_eq!(compare(&|(commit, row)| row.is_some() && commit.version > 5), 2);
    assert_eq!(compare(&|_| true), 5);
    assert_eq!(compare(&|_| false), 0);
    let every: Vec<_> = db.history_of(project).collect();
    assert_eq!(every.len(), 5);
    assert_eq!(every[2].1, Some(2));
    assert_eq!(every[3].1, None);
}

#[test]
fn history_try_next_reports_corruption_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ferro");
    drop(seeded(&path, 400));
    smash_older_half(&path);
    let db = Options::new().read_only().open::<Root>(&path).unwrap();
    let mut history = db.history_of(|root| root.get(&0).map(|v| *v));
    let mut seen = 0;
    let failure = loop {
        match history.try_next() {
            Ok(Some(_)) => seen += 1,
            Ok(None) => panic!("the walk reached a damaged entry"),
            Err(error) => break error,
        }
    };
    assert!(seen > 0);
    assert!(failure.to_string().contains("corrupt"), "{failure}");
}
