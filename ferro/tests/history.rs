mod common;

use std::collections::BTreeMap;

use common::{Rng, cases, fast, text, uncached};
use ferro::{Db, Error, Keep, Map, Options, Snapshot, admin};
use proptest::prelude::*;

type Root = Map<u32, String>;
type Model = BTreeMap<u32, String>;

fn assert_same(map: &Root, model: &Model) {
    assert_eq!(map.len(), model.len());
    let got: Vec<(u32, String)> = map.iter().map(|(k, v)| (k, (*v).clone())).collect();
    let wanted: Vec<(u32, String)> = model.iter().map(|(k, v)| (*k, v.clone())).collect();
    assert_eq!(got, wanted);
}

#[derive(Clone)]
struct BranchModel {
    name: String,
    versions: Vec<Model>,
    messages: Vec<String>,
}

fn check_branch(db: &Db<Root>, model: &BranchModel) {
    assert_eq!(db.version(), model.versions.len() as u64 - 1);
    assert_same(&db.read(), model.versions.last().unwrap());
    for (version, wanted) in model.versions.iter().enumerate() {
        let snapshot = db.at(version as u64).unwrap();
        assert_eq!(snapshot.version(), version as u64);
        assert_eq!(snapshot.commit().message, model.messages[version]);
        assert_same(&snapshot, wanted);
    }
    assert!(matches!(
        db.at(model.versions.len() as u64),
        Err(Error::NoVersion(_))
    ));
    let log = db.log();
    assert_eq!(log.len(), model.versions.len() - 1);
    let mut last = 0;
    for (version, commit) in log.iter() {
        assert_eq!(version, commit.version);
        assert!(commit.time_ms >= last);
        last = commit.time_ms;
    }
}

fn edit(rng: &mut Rng, base: &Model) -> Model {
    let mut next = base.clone();
    for _ in 0..1 + rng.below(6) {
        let key = rng.below(48) as u32;
        if rng.chance(30) {
            next.remove(&key);
        } else {
            let len = match rng.below(3) {
                0 => rng.below(20),
                1 => 500 + rng.below(30),
                _ => 1000 + rng.below(1000),
            };
            next.insert(key, text(rng.next(), len as usize));
        }
    }
    next
}

fn set_contents(root: &mut Root, wanted: &Model) {
    let old: Vec<u32> = root.keys().collect();
    for key in old {
        if !wanted.contains_key(&key) {
            root.remove(&key);
        }
    }
    for (key, value) in wanted {
        if root.get(key).as_deref() != Some(value) {
            root.insert(*key, value.clone());
        }
    }
}

fn run_history(seed: u64, options: fn() -> Options, steps: usize) {
    let mut rng = Rng::new(seed);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h.ferro");
    let open = || options().open::<Root>(&path).unwrap();
    let mut main = Some(open());
    let mut branches = vec![BranchModel {
        name: "main".to_string(),
        versions: vec![Model::new()],
        messages: vec![String::new()],
    }];
    let mut held: Vec<(usize, u64, Snapshot<Root>)> = Vec::new();
    let mut forks = 0;
    for step in 0..steps {
        let db = main.as_ref().unwrap();
        let index = rng.below(branches.len() as u64) as usize;
        let handle = db.branch(&branches[index].name).unwrap();
        match rng.below(12) {
            0..=5 => {
                let current = branches[index].versions.last().unwrap().clone();
                let next = edit(&mut rng, &current);
                let long = rng.chance(10);
                let message = if long {
                    text(rng.next(), 700)
                } else {
                    format!("step {step}")
                };
                handle
                    .commit(message.clone(), |root| set_contents(root, &next))
                    .unwrap();
                if next != current {
                    branches[index].versions.push(next);
                    branches[index].messages.push(message);
                }
            }
            6 => {
                let before = handle.version();
                let size = handle.size_on_disk();
                handle.update(|_| {}).unwrap();
                let current = branches[index].versions.last().unwrap().clone();
                handle.update(|root| set_contents(root, &current)).unwrap();
                assert_eq!(handle.version(), before);
                assert_eq!(handle.size_on_disk(), size);
            }
            7 => {
                forks += 1;
                let name = format!("fork-{forks}");
                let fork = handle.fork(&name).unwrap();
                assert_eq!(fork.version(), handle.version());
                assert!(matches!(handle.fork(&name), Err(Error::BranchExists(_))));
                let mut model = branches[index].clone();
                model.name = name;
                branches.push(model);
            }
            8 => {
                let target = rng.below(branches[index].versions.len() as u64);
                let snapshot = handle.at(target).unwrap();
                let wanted = branches[index].versions[target as usize].clone();
                let current = branches[index].versions.last().unwrap().clone();
                let version = handle.restore(&snapshot).unwrap().version;
                if wanted == current {
                    assert_eq!(version, branches[index].versions.len() as u64 - 1);
                } else {
                    branches[index].versions.push(wanted);
                    branches[index].messages.push(format!("restore v{target}"));
                    assert_eq!(version, branches[index].versions.len() as u64 - 1);
                }
            }
            9 => {
                let target = rng.below(branches[index].versions.len() as u64);
                held.push((index, target, handle.at(target).unwrap()));
                if held.len() > 16 {
                    held.remove(0);
                }
            }
            10 => {
                check_branch(&handle, &branches[index]);
            }
            _ => {
                held.clear();
                drop(handle);
                drop(main.take());
                main = Some(open());
                for model in &branches {
                    let handle = main.as_ref().unwrap().branch(&model.name).unwrap();
                    check_branch(&handle, model);
                }
            }
        }
        for (branch, version, snapshot) in &held {
            assert_same(snapshot, &branches[*branch].versions[*version as usize]);
        }
    }
    let db = main.as_ref().unwrap();
    db.verify().unwrap();
    let mut names = db.branches();
    names.sort();
    let mut wanted: Vec<String> = branches.iter().map(|b| b.name.clone()).collect();
    wanted.sort();
    assert_eq!(names, wanted);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(150), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn history_matches_model(seed in any::<u64>()) {
        run_history(seed, fast, 150);
    }

    #[test]
    fn history_matches_model_uncached(seed in any::<u64>()) {
        run_history(seed, uncached, 100);
    }
}

#[test]
fn fork_at_version_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("z.ferro");
    {
        let db = fast().open::<Root>(&path).unwrap();
        let fork = db.fork("empty").unwrap();
        assert_eq!(fork.version(), 0);
        assert!(fork.read().is_empty());
        fork.update(|root| {
            root.insert(1, "fork".into());
        })
        .unwrap();
        assert_eq!(fork.version(), 1);
        assert_eq!(db.version(), 0);
        assert!(db.read().is_empty());
        assert_eq!(db.branches(), vec!["empty".to_string(), "main".to_string()]);
    }
    let db = fast().open::<Root>(&path).unwrap();
    assert_eq!(db.version(), 0);
    let fork = db.branch("empty").unwrap();
    assert_eq!(fork.version(), 1);
    assert_eq!(*fork.read().get(&1).unwrap(), "fork");
    assert!(fork.at(0).unwrap().is_empty());
    db.update(|root| {
        root.insert(2, "main".into());
    })
    .unwrap();
    assert_eq!(db.version(), 1);
    assert_eq!(*fork.read().get(&1).unwrap(), "fork");
    assert!(fork.read().get(&2).is_none());
    assert_eq!(db.read().len(), 1);
    db.verify().unwrap();
}

#[test]
fn fork_of_untouched_fork_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.ferro");
    {
        let db = fast().open::<Root>(&path).unwrap();
        db.fork("a").unwrap().fork("b").unwrap();
    }
    let db = fast().open::<Root>(&path).unwrap();
    assert_eq!(db.branches(), vec!["a", "b", "main"]);
    assert_eq!(db.branch("b").unwrap().version(), 0);
    assert!(matches!(db.branch("c"), Err(Error::NoBranch(_))));
    assert!(matches!(db.fork("main"), Err(Error::BranchExists(_))));
}

#[test]
fn restore_of_head_is_a_no_op_and_restore_of_genesis_commits() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Root>(dir.path().join("r.ferro")).unwrap();
    db.update(|root| {
        root.insert(1, "one".into());
    })
    .unwrap();
    let head = db.read();
    assert_eq!(db.restore(&head).unwrap().version, 1);
    assert_eq!(db.version(), 1);
    let genesis = db.at(0).unwrap();
    assert_eq!(db.restore(&genesis).unwrap().version, 2);
    assert!(db.read().is_empty());
    assert_eq!(db.restore(&db.at(0).unwrap()).unwrap().version, 2);
    assert_eq!(db.restore(&db.at(1).unwrap()).unwrap().version, 3);
    assert_eq!(*db.read().get(&1).unwrap(), "one");
    assert_eq!(db.read().root_hash(), db.at(1).unwrap().root_hash());
    assert_eq!(db.log().len(), 3);
}

#[test]
fn restore_from_another_branch_copies_contents() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Root>(dir.path().join("x.ferro")).unwrap();
    db.update(|root| {
        root.insert(1, "base".into());
    })
    .unwrap();
    let fork = db.fork("x").unwrap();
    fork.update(|root| {
        root.insert(2, "y".repeat(2000));
    })
    .unwrap();
    db.restore(&fork.read()).unwrap();
    assert_eq!(db.version(), 2);
    assert_eq!(db.read().root_hash(), fork.read().root_hash());
}

#[test]
fn as_of_returns_latest_commit_at_or_before_time() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast()
        .open::<Map<u64, u64>>(dir.path().join("t.ferro"))
        .unwrap();
    for n in 0..40 {
        db.update(|root| {
            root.insert(n, n);
        })
        .unwrap();
    }
    assert_eq!(db.as_of(0).unwrap().version(), 0);
    let log: Vec<_> = db.log().iter().map(|(_, commit)| commit).collect();
    assert!(
        log.windows(2)
            .any(|pair| pair[0].time_ms == pair[1].time_ms)
    );
    for commit in &log {
        let wanted = log
            .iter()
            .filter(|other| other.time_ms <= commit.time_ms)
            .map(|other| other.version)
            .max()
            .unwrap();
        assert_eq!(db.as_of(commit.time_ms).unwrap().version(), wanted);
        assert_eq!(
            db.as_of(commit.time_ms.saturating_sub(1))
                .unwrap()
                .version(),
            log.iter()
                .filter(|other| other.time_ms < commit.time_ms)
                .map(|other| other.version)
                .max()
                .unwrap_or(0)
        );
    }
    assert_eq!(db.as_of(u64::MAX).unwrap().version(), 40);
}

#[test]
fn old_versions_survive_deletion_of_everything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.ferro");
    let db = fast().open::<Root>(&path).unwrap();
    db.update(|root| {
        for n in 0..3000 {
            root.insert(n, text(u64::from(n), (n as usize) % 700));
        }
    })
    .unwrap();
    let full = db.read();
    db.update(|root| root.clear()).unwrap();
    assert!(db.read().is_empty());
    assert_eq!(full.len(), 3000);
    assert_eq!(db.at(1).unwrap().len(), 3000);
    assert_eq!(*full.get(&2500).unwrap(), text(2500, 2500 % 700));
    assert_eq!(db.at(2).unwrap().root_hash(), None);
    db.update(|root| *root = (*full).clone()).unwrap();
    assert_eq!(db.version(), 3);
    assert_eq!(db.read().root_hash(), full.root_hash());
}

#[test]
fn unusual_branch_names_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n.ferro");
    let names = [String::new(), "x".repeat(10_000), "ветка/🌿".to_string()];
    {
        let db = fast().open::<Root>(&path).unwrap();
        db.update(|root| {
            root.insert(1, "base".into());
        })
        .unwrap();
        for (index, name) in names.iter().enumerate() {
            let fork = db.fork(name).unwrap();
            fork.update(|root| {
                root.insert(index as u32 + 2, name.clone());
            })
            .unwrap();
        }
    }
    let db = uncached().open::<Root>(&path).unwrap();
    assert_eq!(db.branches().len(), names.len() + 1);
    for (index, name) in names.iter().enumerate() {
        let fork = db.branch(name).unwrap();
        assert_eq!(fork.branch_name(), name);
        assert_eq!(fork.read().get(&(index as u32 + 2)).as_deref(), Some(name));
    }
    db.verify().unwrap();
}

#[test]
fn large_keys_and_values_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("l.ferro");
    let mut model = BTreeMap::new();
    {
        let db = fast().open::<Map<String, Vec<u8>>>(&path).unwrap();
        db.update(|root| {
            for n in 0..64u64 {
                let key = text(n, 2000 + (n as usize) * 500);
                let value = vec![n as u8; 4000 + (n as usize) * 40_000];
                root.insert(key.clone(), value.clone());
                model.insert(key, value);
            }
        })
        .unwrap();
    }
    let db = uncached().open::<Map<String, Vec<u8>>>(&path).unwrap();
    let snapshot = db.read();
    assert_eq!(snapshot.len(), model.len());
    for (key, value) in &model {
        assert_eq!(snapshot.get(key).as_deref(), Some(value));
    }
    db.verify().unwrap();
}

#[test]
fn ptr_of_commit_points_at_the_recorded_root() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Root>(dir.path().join("p.ferro")).unwrap();
    db.commit("one", |root| {
        root.insert(1, "a".into());
    })
    .unwrap();
    db.commit("two", |root| {
        root.insert(2, "b".into());
    })
    .unwrap();
    let log = db.log();
    assert_eq!(log.get(&1).unwrap().message, "one");
    assert_eq!(log.get(&2).unwrap().message, "two");
    assert_ne!(log.get(&1).unwrap().root, log.get(&2).unwrap().root);
    assert!(log.get(&1).unwrap().root.is_some());
}

#[test]
fn open_with_custom_genesis_treats_equal_value_as_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.ferro");
    let seed = || {
        let mut map = Map::new();
        map.insert(1u32, "seed".to_string());
        map
    };
    let db = Db::<Root>::open_with(&path, seed).unwrap();
    assert_eq!(db.read().len(), 1);
    db.update(|root| {
        root.insert(1, "seed".into());
    })
    .unwrap();
    assert_eq!(db.version(), 0);
    db.update(|root| {
        root.insert(2, "more".into());
    })
    .unwrap();
    assert_eq!(db.version(), 1);
    assert_eq!(db.at(0).unwrap().len(), 1);
    assert_eq!(db.at(1).unwrap().len(), 2);
}

fn seeded(seed: &'static str) -> impl FnOnce() -> Root {
    move || {
        let mut map = Map::new();
        map.insert(0u32, seed.to_string());
        map
    }
}

fn seed_of(root: &Root) -> Option<String> {
    root.get(&0).map(|seed| (*seed).clone())
}

fn unused() -> Root {
    panic!("init ran for a file that holds version 0")
}

#[test]
fn version_zero_is_read_back_from_the_file_whatever_init_gives_later() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.ferro");
    let db = fast().open_with::<Root>(&path, seeded("first")).unwrap();
    db.update(|root| {
        root.insert(1, "one".into());
    })
    .unwrap();
    drop(db);
    let db = fast().open_with::<Root>(&path, unused).unwrap();
    assert_eq!(seed_of(&db.at(0).unwrap()).as_deref(), Some("first"));
    assert_eq!(db.history_of(seed_of).count(), 0);
    assert!(db.bisect_changes(.., seed_of).unwrap().is_empty());
    db.verify_typed().unwrap();
    db.compact(Keep::All).unwrap();
    drop(db);
    admin::compact(&path, Keep::All).unwrap();
    let db = fast().open_with::<Root>(&path, unused).unwrap();
    assert_eq!(seed_of(&db.at(0).unwrap()).as_deref(), Some("first"));
    assert_eq!(db.history_of(seed_of).count(), 0);
    db.verify_typed().unwrap();
}

#[test]
fn a_fork_of_an_empty_database_stores_the_value_it_starts_from() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.ferro");
    let db = fast().open_with::<Root>(&path, seeded("first")).unwrap();
    drop(db);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 8192);
    let db = fast().open_with::<Root>(&path, seeded("second")).unwrap();
    db.fork("side").unwrap();
    drop(db);
    let db = fast().open_with::<Root>(&path, unused).unwrap();
    let side = db.branch("side").unwrap();
    for branch in [&db, &side] {
        assert_eq!(seed_of(&branch.read()).as_deref(), Some("second"));
        assert_eq!(seed_of(&branch.at(0).unwrap()).as_deref(), Some("second"));
    }
    side.update(|root| {
        root.insert(1, "side".into());
    })
    .unwrap();
    db.update(|root| {
        root.insert(2, "main".into());
    })
    .unwrap();
    let base = db.common_ancestor(&side).unwrap();
    assert_eq!(
        (base.version(), seed_of(&base).as_deref()),
        (0, Some("second"))
    );
    db.merge_with(&side, |ours, base, theirs| {
        assert_eq!(seed_of(base).as_deref(), Some("second"));
        ours.insert(1, (*theirs.get(&1).unwrap()).clone());
        ("merge", ())
    })
    .unwrap();
    assert_eq!(db.read().len(), 3);
}

#[test]
fn a_reader_takes_version_0_from_the_file_once_a_writer_stores_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.ferro");
    let writer = fast().open_with::<Root>(&path, seeded("writer")).unwrap();
    let reader = fast()
        .read_only(true)
        .open_with::<Root>(&path, seeded("reader"))
        .unwrap();
    assert_eq!(seed_of(&reader.at(0).unwrap()).as_deref(), Some("reader"));
    writer
        .update(|root| {
            root.insert(1, "one".into());
        })
        .unwrap();
    assert!(reader.refresh().unwrap());
    assert_eq!(seed_of(&reader.at(0).unwrap()).as_deref(), Some("writer"));
    assert_eq!(reader.history_of(seed_of).count(), 0);
}

#[derive(Clone, Debug, Default, PartialEq, ferro::Data)]
struct Noted {
    count: u64,
    #[ferro(skip)]
    note: String,
}

#[test]
fn version_0_reads_the_same_in_the_process_that_stored_it_as_after_a_cycle_or_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.ferro");
    let init = || Noted {
        count: 1,
        note: "init".into(),
    };
    let db = fast().open_with::<Noted>(&path, init).unwrap();
    db.fork("side").unwrap();
    let stored = db.at(0).unwrap().note.clone();
    db.compact(Keep::All).unwrap();
    let cycled = db.at(0).unwrap().note.clone();
    drop(db);
    let db = fast()
        .open_with::<Noted>(&path, || panic!("init ran for a file that holds version 0"))
        .unwrap();
    let reopened = db.at(0).unwrap().note.clone();
    assert_eq!(
        [stored.as_str(), cycled.as_str()],
        [reopened.as_str(), reopened.as_str()],
        "version 0 in the process that stored it, then after a cycle, then after a reopen"
    );
}
