mod common;

use std::sync::{Arc, Barrier};
use std::time::Duration;

use common::fast;
use ferro::admin::{self, Keep};
use ferro::{Commit, Db, Error, Map, Options};
use proptest::prelude::*;

type Root = Map<u8, u32>;

const NAMES: [&str; 4] = ["main", "b1", "b2", "b3"];

#[derive(Clone, Debug)]
enum Step {
    Put(usize, u8, u32),
    Del(usize, u8),
    Same(usize),
    Fork(usize),
    Merge(usize, usize),
    Restore(usize, u64),
    Pause,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        10 => (0..4usize, 0..6u8, 0..4u32).prop_map(|(b, k, v)| Step::Put(b, k, v)),
        3 => (0..4usize, 0..6u8).prop_map(|(b, k)| Step::Del(b, k)),
        1 => (0..4usize).prop_map(Step::Same),
        2 => (0..4usize).prop_map(Step::Fork),
        3 => (0..4usize, 0..4usize).prop_map(|(a, b)| Step::Merge(a, b)),
        2 => (0..4usize, 0..20u64).prop_map(|(b, v)| Step::Restore(b, v)),
        2 => Just(Step::Pause),
    ]
}

fn dump(root: &Root) -> Vec<(u8, u32)> {
    root.iter().map(|(k, v)| (k, *v)).collect()
}

#[derive(Clone, Debug)]
struct Version {
    version: u64,
    time_ms: u64,
    message: String,
    restored: Option<u64>,
    content: Vec<(u8, u32)>,
    hash: Option<ferro::Hash>,
}

#[derive(Clone, Debug)]
struct BranchState {
    name: String,
    head: u64,
    versions: Vec<Version>,
    content: Vec<(u8, u32)>,
}

fn capture(main: &Db<Root>) -> Vec<BranchState> {
    let mut out = Vec::new();
    for name in main.branches() {
        let db = main.branch(&name).unwrap();
        let mut versions = Vec::new();
        for (version, commit) in db.log().iter() {
            let snapshot = db.at(version).unwrap();
            versions.push(Version {
                version,
                time_ms: commit.time_ms,
                message: commit.message.clone(),
                restored: commit.restored,
                content: dump(&snapshot),
                hash: snapshot.root_hash(),
            });
        }
        out.push(BranchState {
            name,
            head: db.version(),
            versions,
            content: dump(&db.read()),
        });
    }
    out
}

fn build(path: &std::path::Path, steps: &[Step]) -> Vec<BranchState> {
    let main: Db<Root> = fast().open(path).unwrap();
    let mut dbs: Vec<Option<Db<Root>>> = vec![Some(main.clone()), None, None, None];
    for step in steps {
        match step {
            Step::Put(b, k, v) => {
                if let Some(db) = &dbs[*b] {
                    db.update(|root| {
                        root.insert(*k, *v);
                    })
                    .unwrap();
                }
            }
            Step::Del(b, k) => {
                if let Some(db) = &dbs[*b] {
                    db.update(|root| {
                        root.remove(k);
                    })
                    .unwrap();
                }
            }
            Step::Same(b) => {
                if let Some(db) = &dbs[*b] {
                    db.update(|_| {}).unwrap();
                }
            }
            Step::Fork(from) => {
                if let (Some(db), Some(slot)) = (&dbs[*from], (1..4).find(|i| dbs[*i].is_none())) {
                    dbs[slot] = Some(db.fork(NAMES[slot]).unwrap());
                }
            }
            Step::Merge(a, b) => {
                if a != b
                    && let (Some(da), Some(db)) = (&dbs[*a], &dbs[*b])
                {
                    let _ = da.merge_with(db, |root, base, theirs| {
                        root.merge(base, theirs, |c| c.theirs.map(|v| *v));
                        ("merge", ())
                    });
                }
            }
            Step::Restore(b, v) => {
                if let Some(db) = &dbs[*b]
                    && let Ok(snapshot) = db.at(*v % (db.version() + 1))
                {
                    db.restore(&snapshot).unwrap();
                }
            }
            Step::Pause => std::thread::sleep(Duration::from_millis(3)),
        }
    }
    capture(&main)
}

fn retained(versions: &[Version], keep: Keep) -> Vec<u64> {
    let mut all: Vec<&Version> = versions.iter().collect();
    match keep {
        Keep::All => {}
        Keep::Last(n) => {
            while all.len() > n.max(1) {
                all.remove(0);
            }
        }
        Keep::Since(t) => {
            while all.len() > 1 && all[0].time_ms < t {
                all.remove(0);
            }
        }
        Keep::Within(_) => unreachable!("the strategy never picks an age"),
    }
    all.iter().map(|v| v.version).collect()
}

fn check_after(path: &std::path::Path, before: &[BranchState], keep: Keep) {
    let main: Db<Root> = fast().open(path).unwrap();
    assert_eq!(
        main.branches(),
        before.iter().map(|b| b.name.clone()).collect::<Vec<_>>()
    );
    main.verify().unwrap();
    for state in before {
        let db = main.branch(&state.name).unwrap();
        assert_eq!(db.version(), state.head, "{}", state.name);
        assert_eq!(dump(&db.read()), state.content, "{}", state.name);
        let want = retained(&state.versions, keep);
        let log = db.log();
        let got: Vec<u64> = log.iter().map(|(v, _)| v).collect();
        assert_eq!(got, want, "{} {:?}", state.name, keep);
        for version in &state.versions {
            if want.contains(&version.version) {
                let snapshot = db.at(version.version).unwrap();
                assert_eq!(dump(&snapshot), version.content);
                assert_eq!(snapshot.root_hash(), version.hash);
                let commit: Arc<Commit> = log.get(&version.version).unwrap();
                assert_eq!(commit.time_ms, version.time_ms);
                assert_eq!(commit.message, version.message);
                assert_eq!(commit.restored, version.restored);
            } else {
                assert!(matches!(db.at(version.version), Err(Error::NoVersion(_))));
            }
        }
    }
    for (index, state) in before.iter().enumerate() {
        let db = main.branch(&state.name).unwrap();
        let marker = 1000 + index as u32;
        db.update(|root| {
            root.insert(200, marker);
        })
        .unwrap();
        assert_eq!(db.version(), state.head + 1);
    }
    drop(main);
    let again: Db<Root> = fast().open(path).unwrap();
    for (index, state) in before.iter().enumerate() {
        let db = again.branch(&state.name).unwrap();
        assert_eq!(*db.read().get(&200).unwrap(), 1000 + index as u32);
    }
}

fn keep_strategy() -> impl Strategy<Value = (u8, u64)> {
    (0..4u8, 0..6u64)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(40)))]

    #[test]
    fn compaction_preserves_every_retained_version(
        steps in prop::collection::vec(step(), 1..40),
        choice in keep_strategy(),
        junk in any::<bool>(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.ferro");
        let before = build(&path, &steps);
        let latest = before
            .iter()
            .flat_map(|b| b.versions.iter().map(|v| v.time_ms))
            .max()
            .unwrap_or(0);
        let oldest = before
            .iter()
            .flat_map(|b| b.versions.iter().map(|v| v.time_ms))
            .min()
            .unwrap_or(0);
        let keep = match choice.0 {
            0 => Keep::All,
            1 => Keep::Last(choice.1 as usize),
            2 => Keep::Since(oldest + choice.1 * 2),
            _ => Keep::Since(latest + 1),
        };
        if junk {
            std::fs::write(dir.path().join("h.ferro.compacting"), vec![0xAB; 100_000]).unwrap();
        }
        let size = std::fs::metadata(&path).unwrap().len();
        let report = admin::compact(&path, keep).unwrap();
        prop_assert_eq!(report.before_bytes, size);
        prop_assert!(report.objects == 0 || !dir.path().join("h.ferro.compacting").exists());
        check_after(&path, &before, keep);
        admin::verify(&path).unwrap();
    }
}

#[test]
fn history_of_pruned_log_returned_values_and_forks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hist.ferro");
    {
        let main: Db<Root> = fast().open(&path).unwrap();
        for v in [1u32, 2, 1, 1, 3, 1, 1, 2] {
            main.update(|root| {
                root.insert(0, v);
                root.insert(1, v * 10);
            })
            .unwrap();
        }
        let side = main.fork("side").unwrap();
        side.update(|root| {
            root.insert(0, 1);
            root.insert(5, 5);
        })
        .unwrap();
        side.update(|root| {
            root.insert(0, 9);
        })
        .unwrap();
        main.update(|root| {
            root.insert(0, 3);
        })
        .unwrap();

        let project = |root: &Root| root.get(&0).map(|v| *v);
        let versions: Vec<(u64, Option<u32>)> = main
            .history_of(project)
            .map(|(c, v)| (c.version, v))
            .collect();
        assert_eq!(
            versions,
            vec![
                (7, Some(3)),
                (6, Some(2)),
                (5, Some(1)),
                (4, Some(3)),
                (3, Some(1)),
                (2, Some(2)),
                (1, Some(1))
            ]
        );
        let side_versions: Vec<(u64, Option<u32>)> = side
            .history_of(project)
            .map(|(c, v)| (c.version, v))
            .collect();
        assert_eq!(
            side_versions,
            vec![
                (8, Some(9)),
                (7, Some(1)),
                (6, Some(2)),
                (5, Some(1)),
                (4, Some(3)),
                (3, Some(1)),
                (2, Some(2)),
                (1, Some(1))
            ]
        );
        assert_eq!(main.history_of(|root| root.get(&7).map(|v| *v)).count(), 0);
        assert_eq!(main.history_of(|root| root.len()).count(), 1);
    }
    admin::compact(&path, Keep::Last(4)).unwrap();
    let main: Db<Root> = fast().open(&path).unwrap();
    let project = |root: &Root| root.get(&0).map(|v| *v);
    let pruned: Vec<(u64, Option<u32>)> = main
        .history_of(project)
        .map(|(c, v)| (c.version, v))
        .collect();
    assert_eq!(
        pruned,
        vec![(7, Some(3)), (6, Some(2)), (5, Some(1)), (4, Some(3))]
    );
    assert_eq!(main.history_of(|root| root.get(&1).map(|v| *v)).count(), 3);
    let side = main.branch("side").unwrap();
    let side_pruned: Vec<(u64, Option<u32>)> = side
        .history_of(project)
        .map(|(c, v)| (c.version, v))
        .collect();
    assert_eq!(
        side_pruned,
        vec![(8, Some(9)), (7, Some(1)), (6, Some(2)), (5, Some(1))]
    );
}

#[test]
fn history_of_starts_from_genesis_for_a_custom_initial_value() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = Db::open_with(dir.path().join("g.ferro"), || {
        let mut root = Map::new();
        root.insert(0, 7);
        root
    })
    .unwrap();
    db.update(|root| {
        root.insert(0, 7);
        root.insert(1, 1);
    })
    .unwrap();
    db.update(|root| {
        root.insert(0, 8);
    })
    .unwrap();
    db.update(|root| {
        root.insert(0, 7);
    })
    .unwrap();
    let changes: Vec<(u64, Option<u32>)> = db
        .history_of(|root| root.get(&0).map(|v| *v))
        .map(|(c, v)| (c.version, v))
        .collect();
    assert_eq!(changes, vec![(3, Some(7)), (2, Some(8))]);
}

fn populate(path: &std::path::Path, commits: u32) -> Vec<(u8, u32)> {
    let db: Db<Root> = fast().open(path).unwrap();
    for i in 0..commits {
        db.update(|root| {
            root.insert((i % 40) as u8, i);
        })
        .unwrap();
    }
    dump(&db.read())
}

#[test]
fn compact_with_a_reader_open_keeps_the_reader_on_its_snapshot_until_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rd.ferro");
    let content = populate(&path, 200);
    let reader: Db<Root> = Options::new().read_only().open(&path).unwrap();
    let old = reader.at(50).unwrap();
    let old_content = dump(&old);
    admin::compact(&path, Keep::Last(5)).unwrap();
    assert_eq!(dump(&reader.read()), content);
    assert_eq!(dump(&reader.at(50).unwrap()), old_content);
    assert_eq!(reader.log().len(), 200);
    reader.verify().unwrap();
    assert!(reader.refresh().unwrap());
    assert_eq!(dump(&old), old_content);
    assert_eq!(reader.log().len(), 5);
    assert!(matches!(reader.at(50), Err(Error::NoVersion(50))));
    assert_eq!(dump(&reader.read()), content);
    let fresh: Db<Root> = Options::new().read_only().open(&path).unwrap();
    assert_eq!(fresh.log().len(), 5);
    assert_eq!(dump(&fresh.read()), content);
}

#[test]
fn concurrent_compactions_either_win_or_report_locked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cc.ferro");
    let content = populate(&path, 3000);
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let path = path.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for _ in 0..6 {
                results.push(admin::compact(&path, Keep::Last(100)).map(|_| ()));
            }
            results
        }));
    }
    let mut ok = 0;
    for handle in handles {
        for result in handle.join().unwrap() {
            match result {
                Ok(()) => ok += 1,
                Err(Error::Locked) => {}
                Err(other) => panic!("unexpected {other}"),
            }
        }
    }
    assert!(ok >= 1);
    admin::verify(&path).unwrap();
    let db: Db<Root> = fast().open(&path).unwrap();
    assert_eq!(dump(&db.read()), content);
    assert_eq!(db.log().len(), 100);
}

#[test]
fn readers_see_the_old_or_new_file_while_compactions_rename() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rn.ferro");
    let content = populate(&path, 400);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let path = path.clone();
        let stop = stop.clone();
        let content = content.clone();
        std::thread::spawn(move || {
            let mut opened = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let db: Db<Root> = Options::new().read_only().open(&path).unwrap();
                assert_eq!(dump(&db.read()), content);
                db.verify().unwrap();
                opened += 1;
            }
            opened
        })
    };
    for round in 0..15 {
        let keep = if round % 2 == 0 {
            Keep::Last(50)
        } else {
            Keep::All
        };
        admin::compact(&path, keep).unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(reader.join().unwrap() > 0);
}

#[test]
fn a_complete_leftover_temp_file_neither_loads_nor_blocks_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lo.ferro");
    let other = dir.path().join("other.ferro");
    let content = populate(&path, 50);
    populate(&other, 7);
    std::fs::copy(&other, dir.path().join("lo.ferro.compacting")).unwrap();
    let db: Db<Root> = fast().open(&path).unwrap();
    assert_eq!(dump(&db.read()), content);
    drop(db);
    admin::compact(&path, Keep::All).unwrap();
    assert!(!dir.path().join("lo.ferro.compacting").exists());
    let db: Db<Root> = fast().open(&path).unwrap();
    assert_eq!(dump(&db.read()), content);
}

#[test]
fn since_a_time_after_every_commit_keeps_only_the_head() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sn.ferro");
    {
        let main: Db<Root> = fast().open(&path).unwrap();
        for i in 0..5u32 {
            main.update(|root| {
                root.insert(0, i);
            })
            .unwrap();
        }
        let side = main.fork("side").unwrap();
        side.update(|root| {
            root.insert(1, 1);
        })
        .unwrap();
    }
    let report = admin::compact(&path, Keep::Since(u64::MAX)).unwrap();
    assert_eq!(report.commits_dropped, 4 + 5);
    let main: Db<Root> = fast().open(&path).unwrap();
    let side = main.branch("side").unwrap();
    assert_eq!(main.log().len(), 1);
    assert_eq!(side.log().len(), 1);
    assert_eq!(main.version(), 5);
    assert_eq!(side.version(), 6);
    assert_eq!(*main.read().get(&0).unwrap(), 4);
    assert_eq!(*side.read().get(&1).unwrap(), 1);
    main.update(|root| {
        root.insert(0, 99);
    })
    .unwrap();
    assert_eq!(main.version(), 6);
}
