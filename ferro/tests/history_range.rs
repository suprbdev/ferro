mod common;

use std::ops::{Bound, RangeBounds};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Rng, fast};
use ferro::{Commit, Db, Keep, Map};
use tempfile::TempDir;

type Root = Map<u64, u64>;

const WATCHED: u64 = 3;

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn seeded(seed: u64, commits: u64) -> (TempDir, Db<Root>, Vec<Option<u64>>) {
    let dir = dir();
    let db = fast().open::<Root>(dir.path().join("range.ferro")).unwrap();
    let mut rng = Rng::new(seed);
    let mut states = vec![None];
    for _ in 0..commits {
        let (key, value) = (rng.below(6), rng.below(4));
        let remove = rng.chance(20);
        db.update(|root| {
            if remove {
                root.remove(&key);
            } else {
                root.insert(key, value);
            }
        })
        .unwrap();
        while states.len() <= db.version() as usize {
            states.push(db.read().get(&WATCHED).map(|v| *v));
        }
    }
    (dir, db, states)
}

fn backwards() -> std::ops::Range<u64> {
    let (high, low) = (9, 4);
    high..low
}

fn watched(root: &Root) -> Option<u64> {
    root.get(&WATCHED).map(|v| *v)
}

fn changes(items: impl Iterator<Item = (Commit, Option<u64>)>) -> Vec<(u64, Option<u64>)> {
    items
        .map(|(commit, value)| (commit.version, value))
        .collect()
}

fn expected(states: &[Option<u64>], from: u64, to: u64) -> Vec<(u64, Option<u64>)> {
    let last = states.len() as u64 - 1;
    (from.max(1)..=to.min(last))
        .rev()
        .filter(|version| states[*version as usize] != states[*version as usize - 1])
        .map(|version| (version, states[version as usize]))
        .collect()
}

#[test]
fn a_range_reports_exactly_the_changes_inside_it() {
    for seed in 1..8 {
        let (_dir, db, states) = seeded(seed, 40);
        let last = db.version();
        for from in 0..=last + 2 {
            for to in 0..=last + 2 {
                assert_eq!(
                    changes(db.history_of_range(from..=to, watched)),
                    expected(&states, from, to),
                    "seed {seed} {from}..={to}"
                );
            }
        }
        assert_eq!(changes(db.history_of(watched)), expected(&states, 0, last));
    }
}

#[test]
fn every_kind_of_bound_means_what_a_range_means() {
    let (_dir, db, states) = seeded(9, 30);
    let last = db.version();
    assert_eq!(
        changes(db.history_of_range(.., watched)),
        expected(&states, 0, last)
    );
    assert_eq!(
        changes(db.history_of_range(5.., watched)),
        expected(&states, 5, last)
    );
    assert_eq!(
        changes(db.history_of_range(..9, watched)),
        expected(&states, 0, 8)
    );
    assert_eq!(
        changes(db.history_of_range(4..9, watched)),
        expected(&states, 4, 8)
    );
    assert_eq!(changes(db.history_of_range(..0, watched)), Vec::new());
    assert_eq!(
        changes(db.history_of_range(backwards(), watched)),
        Vec::new()
    );
    let excluded = (Bound::Excluded(4), Bound::Included(9));
    assert_eq!(excluded.start_bound(), Bound::Excluded(&4));
    assert_eq!(
        changes(db.history_of_range(excluded, watched)),
        expected(&states, 5, 9)
    );
    let top = (Bound::Excluded(u64::MAX), Bound::Unbounded);
    assert_eq!(changes(db.history_of_range(top, watched)), Vec::new());
}

#[test]
fn a_range_reads_only_the_versions_it_covers() {
    let (_dir, db, _) = seeded(10, 200);
    let calls = AtomicUsize::new(0);
    let counted = |root: &Root| {
        calls.fetch_add(1, Ordering::Relaxed);
        watched(root)
    };
    let _ = db.history_of_range(50..=59, counted).count();
    assert_eq!(calls.load(Ordering::Relaxed), 11);
    calls.store(0, Ordering::Relaxed);
    let _ = db.history_of_range(..=9, counted).count();
    assert!(
        calls.load(Ordering::Relaxed) <= 11,
        "{}",
        calls.load(Ordering::Relaxed)
    );
}

#[test]
fn a_bounded_take_while_matches_the_iterator_adaptor() {
    let (_dir, db, _) = seeded(11, 80);
    let last = db.version();
    for to in [last, last / 2, last / 3] {
        let plain: Vec<_> =
            Iterator::take_while(db.history_of_range(..=to, watched), |(_, value)| {
                value.is_some()
            })
            .collect();
        let inherent: Vec<_> = db
            .history_of_range(..=to, watched)
            .take_while(|(_, value)| value.is_some())
            .collect();
        assert_eq!(plain, inherent, "to {to}");
    }
}

#[test]
fn a_pruned_chain_keeps_reporting_its_oldest_commit() {
    let (_dir, db, states) = seeded(12, 60);
    let last = db.version();
    db.compact(Keep::Last(30)).unwrap();
    let first = db.log().first().unwrap().0;
    let unbounded = changes(db.history_of(watched));
    assert_eq!(changes(db.history_of_range(0..=last, watched)), unbounded);
    assert_eq!(
        changes(db.history_of_range(first..=last, watched)),
        unbounded
    );
    assert_eq!(unbounded.last().unwrap().0, first);
    let inside = changes(db.history_of_range(first + 1..=last, watched));
    assert_eq!(inside, expected(&states, first + 1, last));
    assert!(changes(db.history_of_range(..first, watched)).is_empty());
}

#[test]
fn a_range_works_across_a_cycle_and_on_a_fork() {
    let (_dir, db, states) = seeded(13, 40);
    let forked = db.fork("side").unwrap();
    db.compact(Keep::All).unwrap();
    let last = forked.version();
    forked
        .update(|root| {
            root.insert(WATCHED, 99);
        })
        .unwrap();
    assert_eq!(
        changes(db.history_of_range(5..=20, watched)),
        expected(&states, 5, 20)
    );
    assert_eq!(
        changes(forked.history_of_range(5..=20, watched)),
        expected(&states, 5, 20)
    );
    assert_eq!(
        changes(forked.history_of_range(last + 1.., watched)),
        vec![(last + 1, Some(99))]
    );
}

#[test]
fn bisect_finds_where_a_monotone_test_turns_true() {
    let dir = dir();
    let db = fast()
        .open::<Root>(dir.path().join("bisect.ferro"))
        .unwrap();
    for i in 0..300u64 {
        db.update(|root| {
            root.insert(i, i);
        })
        .unwrap();
    }
    for size in [1usize, 2, 77, 150, 299, 300] {
        let calls = AtomicUsize::new(0);
        let test = |root: &Root| {
            calls.fetch_add(1, Ordering::Relaxed);
            root.len() >= size
        };
        assert_eq!(db.bisect(.., test).unwrap(), Some(size as u64));
        assert!(
            calls.load(Ordering::Relaxed) <= 10,
            "{}",
            calls.load(Ordering::Relaxed)
        );
    }
    assert_eq!(db.bisect(.., |root| root.len() > 300).unwrap(), None);
    assert_eq!(db.bisect(.., |_| true).unwrap(), Some(1));
    assert_eq!(db.bisect(40.., |_| true).unwrap(), Some(40));
    assert_eq!(db.bisect(40..=50, |root| root.len() >= 60).unwrap(), None);
    assert_eq!(
        db.bisect(40..=50, |root| root.len() >= 45).unwrap(),
        Some(45)
    );
    assert_eq!(db.bisect(500.., |_| true).unwrap(), None);
    assert_eq!(db.bisect(backwards(), |_| true).unwrap(), None);
}

#[test]
fn bisect_stays_inside_what_is_retained() {
    let dir = dir();
    let db = fast()
        .open::<Root>(dir.path().join("bisect.ferro"))
        .unwrap();
    for i in 0..100u64 {
        db.update(|root| {
            root.insert(i, i);
        })
        .unwrap();
    }
    db.compact(Keep::Last(10)).unwrap();
    assert_eq!(db.bisect(.., |_| true).unwrap(), Some(91));
    assert_eq!(db.bisect(.., |root| root.len() >= 95).unwrap(), Some(95));
    assert_eq!(db.bisect(.., |root| root.len() >= 5).unwrap(), Some(91));
}

fn revisions(commits: u64, every: u64) -> (TempDir, Db<Root>) {
    let dir = dir();
    let db = fast()
        .open::<Root>(dir.path().join("changes.ferro"))
        .unwrap();
    for i in 1..=commits {
        db.update(|root| {
            root.insert(1, i);
            if i % every == 0 {
                root.insert(WATCHED, i);
            }
        })
        .unwrap();
    }
    (dir, db)
}

#[test]
fn bisect_changes_matches_the_walk_for_a_value_that_never_returns() {
    let (_dir, db) = revisions(400, 37);
    let mut walked = changes(db.history_of(watched));
    walked.reverse();
    let found: Vec<_> = db
        .bisect_changes(.., watched)
        .unwrap()
        .into_iter()
        .map(|(commit, value)| (commit.version, value))
        .collect();
    assert_eq!(found, walked);
    let inside: Vec<u64> = db
        .bisect_changes(38..=200, watched)
        .unwrap()
        .into_iter()
        .map(|(commit, _)| commit.version)
        .collect();
    assert_eq!(inside, vec![74, 111, 148, 185]);
    assert!(db.bisect_changes(38..=73, watched).unwrap().is_empty());
    assert_eq!(db.bisect_changes(37..=37, watched).unwrap().len(), 1);
    assert_eq!(db.bisect_changes(38..=38, watched).unwrap().len(), 0);
}

#[test]
fn bisect_changes_costs_the_changes_not_the_span() {
    let (_dir, db) = revisions(3000, 1000);
    let calls = AtomicUsize::new(0);
    let found = db
        .bisect_changes(.., |root| {
            calls.fetch_add(1, Ordering::Relaxed);
            watched(root)
        })
        .unwrap();
    assert_eq!(found.len(), 3);
    assert!(
        calls.load(Ordering::Relaxed) < 80,
        "{}",
        calls.load(Ordering::Relaxed)
    );
}

#[test]
fn bisect_changes_reports_the_oldest_retained_commit_like_the_walk() {
    let (_dir, db) = revisions(100, 7);
    db.compact(Keep::Last(30)).unwrap();
    let mut walked = changes(db.history_of(watched));
    walked.reverse();
    let found: Vec<_> = db
        .bisect_changes(.., watched)
        .unwrap()
        .into_iter()
        .map(|(commit, value)| (commit.version, value))
        .collect();
    assert_eq!(found, walked);
    assert_eq!(found[0].0, 71);
}

#[test]
fn bisect_changes_takes_genesis_into_account() {
    let dir = dir();
    let db = fast()
        .open_with::<Root>(dir.path().join("genesis.ferro"), || {
            let mut root = Map::new();
            root.insert(WATCHED, 5);
            root
        })
        .unwrap();
    db.update(|root| {
        root.insert(1, 1);
    })
    .unwrap();
    db.update(|root| {
        root.insert(WATCHED, 6);
    })
    .unwrap();
    let found = db.bisect_changes(.., watched).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0.version, 2);
}

#[test]
fn bisect_changes_can_miss_a_change_that_was_reverted_between_probes() {
    let dir = dir();
    let db = fast()
        .open::<Root>(dir.path().join("reverted.ferro"))
        .unwrap();
    for i in 0..16u64 {
        db.update(|root| {
            root.insert(1, i);
            if i == 5 {
                root.insert(WATCHED, 1);
            }
            if i == 6 {
                root.remove(&WATCHED);
            }
        })
        .unwrap();
    }
    assert_eq!(changes(db.history_of(watched)).len(), 2);
    assert!(db.bisect_changes(.., watched).unwrap().is_empty());
}
