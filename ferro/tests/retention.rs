mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::fast;
use ferro::db::Clock;
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
            Ok(snapshot) => format!(
                "at {version}: v{} len {}",
                snapshot.version(),
                snapshot.len()
            ),
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
    let history: Vec<u64> = db
        .history_of(|root| root.len())
        .map(|(commit, _)| commit.version)
        .collect();
    out.push(format!("history {history:?}"));
    out
}

fn reachable(db: &Db<Root>) -> Vec<u64> {
    (0..=db.version())
        .filter(|version| db.at(*version).is_ok())
        .collect()
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
    let db: Db<Root> = options()
        .history(Keep::Last(2))
        .open(dir.path().join("e.ferro"))
        .unwrap();
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

#[test]
fn keep_within_measures_its_age_from_the_clock_commits_take_their_times_from() {
    let dir = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicU64::new(1_000));
    let clock = {
        let now = now.clone();
        Clock(Arc::new(move || now.load(Ordering::Relaxed)))
    };
    let db: Db<Root> = options()
        .clock(clock)
        .open(dir.path().join("w.ferro"))
        .unwrap();
    for key in 1..=3 {
        now.store(key * 1_000, Ordering::Relaxed);
        put(&db, key);
    }
    now.store(3_500, Ordering::Relaxed);
    let report = db
        .compact(Keep::Within(Duration::from_millis(1_600)))
        .unwrap();
    assert_eq!(report.commits_dropped, 1);
    assert_eq!(reachable(&db), vec![2, 3]);
    now.store(5_100, Ordering::Relaxed);
    db.compact(Keep::Within(Duration::from_millis(2_000)))
        .unwrap();
    assert_eq!(reachable(&db), vec![3]);
}

type Versions = Vec<(u64, u64, Vec<(u64, u64)>)>;

struct Modeled {
    name: String,
    db: Db<Root>,
    versions: Versions,
    first: u64,
}

impl Modeled {
    fn record(&mut self) {
        let snapshot = self.db.read();
        let version = snapshot.version();
        if version == 0 || self.versions.last().is_some_and(|(v, _, _)| *v == version) {
            return;
        }
        let values = snapshot.iter().map(|(k, v)| (k, *v)).collect();
        self.versions
            .push((version, snapshot.commit().time_ms, values));
    }

    fn first_after(&self, keep: Keep, now: u64) -> u64 {
        let Some((last, _, _)) = self.versions.last() else {
            return self.first;
        };
        let since = |cut: u64| {
            self.versions
                .iter()
                .filter(|(version, time, _)| *version >= self.first && *time < cut)
                .map(|(version, _, _)| version + 1)
                .max()
                .unwrap_or(self.first)
        };
        let first = match keep {
            Keep::All => self.first,
            Keep::Last(count) => (last + 1).saturating_sub(count.max(1) as u64),
            Keep::Since(cut) => since(cut),
            Keep::Within(age) => since(now.saturating_sub(age.as_millis() as u64)),
        };
        first.max(self.first).min(*last)
    }

    fn check(&self, context: &str) {
        let name = &self.name;
        let logged: Vec<u64> = self.db.log().iter().map(|(v, _)| v).collect();
        let last = self.versions.last().map_or(0, |(v, _, _)| *v);
        let want: Vec<u64> = match last {
            0 => Vec::new(),
            last => (self.first..=last).collect(),
        };
        assert_eq!(logged, want, "{context}: the log of {name}");
        for (version, time, values) in &self.versions {
            if *version < self.first {
                assert!(
                    matches!(self.db.at(*version), Err(Error::NoVersion(_))),
                    "{context}: {name} v{version}"
                );
                continue;
            }
            let snapshot = self.db.at(*version).unwrap();
            let read: Vec<(u64, u64)> = snapshot.iter().map(|(k, v)| (k, *v)).collect();
            assert_eq!(&read, values, "{context}: {name} v{version}");
            let newest = self
                .versions
                .iter()
                .filter(|(_, at, _)| at <= time)
                .map(|(v, _, _)| *v)
                .max();
            let as_of = self.db.as_of(*time).unwrap();
            assert_eq!(
                Some(as_of.version()),
                newest,
                "{context}: {name} as of {time}"
            );
        }
    }
}

fn retention_model(seed: u64) {
    let mut rng = common::Rng::new(seed);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model.ferro");
    let ticks = Arc::new(AtomicU64::new(1_700_000_000_000));
    let clock = {
        let ticks = ticks.clone();
        Clock(Arc::new(move || ticks.load(Ordering::SeqCst)))
    };
    let threads = [1, 2, 4][(seed % 3) as usize];
    let main: Db<Root> = options()
        .copy_threads(threads)
        .clock(clock)
        .open(&path)
        .unwrap();
    let mut models = vec![Modeled {
        name: "main".into(),
        db: main.clone(),
        versions: Vec::new(),
        first: 1,
    }];
    let mut counter = 0;
    for step in 0..300 {
        let pick = rng.below(models.len() as u64) as usize;
        match rng.below(100) {
            0..4 => {
                ticks.fetch_sub(rng.below(5), Ordering::SeqCst);
            }
            4..41 => {
                ticks.fetch_add(rng.below(4), Ordering::SeqCst);
                counter += 1;
                let key = rng.below(20);
                let model = &mut models[pick];
                if rng.chance(20) {
                    model
                        .db
                        .commit_stamped(|root, stamp| {
                            root.insert(key, stamp.time_ms());
                            ("stamped", ())
                        })
                        .unwrap();
                } else {
                    put_value(&model.db, key, counter);
                }
                model.record();
            }
            41..49 if models.len() < 6 => {
                let name = format!("b{step}");
                let parent = &models[pick];
                let fork = Modeled {
                    db: parent.db.fork(&name).unwrap(),
                    name,
                    versions: parent.versions.clone(),
                    first: parent.first,
                };
                models.push(fork);
            }
            49..63 if models.len() > 1 => {
                ticks.fetch_add(rng.below(3), Ordering::SeqCst);
                let from = rng.below(models.len() as u64) as usize;
                if from == pick {
                    continue;
                }
                let other = models[from].db.clone();
                let merged = models[pick].db.merge_with(&other, |ours, base, theirs| {
                    let applied = ours.merge(base, theirs, |conflict| conflict.keep_theirs());
                    ("merge", applied)
                });
                match merged {
                    Ok(_) => models[pick].record(),
                    Err(Error::NoCommonAncestor) => {}
                    Err(error) => panic!("seed {seed} step {step}: {error}"),
                }
            }
            63..67 if models.len() > 1 => {
                let index = 1 + rng.below(models.len() as u64 - 1) as usize;
                main.drop_branch(&models[index].name).unwrap();
                models.remove(index);
            }
            67..73 => {
                let model = &mut models[pick];
                let retained: Vec<u64> = model
                    .versions
                    .iter()
                    .map(|(v, _, _)| *v)
                    .filter(|v| *v >= model.first)
                    .collect();
                if retained.is_empty() {
                    continue;
                }
                let version = retained[rng.below(retained.len() as u64) as usize];
                let snapshot = model.db.at(version).unwrap();
                model.db.restore(&snapshot).unwrap();
                model.record();
            }
            73..81 => {
                let now = ticks.load(Ordering::SeqCst);
                let keep = match rng.below(4) {
                    0 => Keep::All,
                    1 => Keep::Last(1 + rng.below(8) as usize),
                    2 => Keep::Since(now.saturating_sub(rng.below(30))),
                    _ => Keep::Within(Duration::from_millis(rng.below(30))),
                };
                let context = format!("seed {seed} step {step} {keep:?} at {threads} threads");
                let heads: Vec<_> = models.iter().map(|m| m.db.head_hash()).collect();
                let firsts: Vec<u64> = models.iter().map(|m| m.first_after(keep, now)).collect();
                main.compact(keep)
                    .unwrap_or_else(|error| panic!("{context}: {error}"));
                for (model, first) in models.iter_mut().zip(firsts) {
                    model.first = first;
                }
                let after: Vec<_> = models.iter().map(|m| m.db.head_hash()).collect();
                assert_eq!(heads, after, "{context}");
                main.verify_typed()
                    .unwrap_or_else(|error| panic!("{context}: {error}"));
                for model in &models {
                    model.check(&context);
                }
            }
            _ => {}
        }
    }
    for model in &models {
        model.check(&format!("seed {seed} at the end"));
    }
    let heads: Vec<_> = models
        .iter()
        .map(|m| (m.name.clone(), m.db.head_hash()))
        .collect();
    drop(models);
    drop(main);
    let db: Db<Root> = options().open(&path).unwrap();
    db.verify_typed().unwrap();
    for (name, head) in heads {
        assert_eq!(
            db.branch(&name).unwrap().head_hash(),
            head,
            "seed {seed} {name}"
        );
    }
}

fn put_value(db: &Db<Root>, key: u64, value: u64) {
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
}

#[test]
fn every_kind_of_retention_keeps_exactly_its_versions_across_merges_and_a_clock_that_steps_back() {
    for seed in 0..u64::from(common::cases(12)) {
        retention_model(seed);
    }
}
