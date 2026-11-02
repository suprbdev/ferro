mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::{Outcome, Rng, fast, guarded, quiet_ferro_panics, text, uncached};
use ferro::db::Hook;
use ferro::{Data, Db, Durability, Error, Keep, Map, Options, Reclaim, Snapshot, admin};
use proptest::prelude::*;
use tempfile::TempDir;

type Root = Map<u64, String>;

const MANUAL: Reclaim = Reclaim {
    every_bytes: 0,
    fill: 0.5,
};

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn path_in(dir: &TempDir) -> PathBuf {
    dir.path().join("db.ferro")
}

fn options() -> Options {
    fast().reclaim(MANUAL)
}

fn fill(db: &Db<Root>, commits: u64, keys: u64) {
    for i in 0..commits {
        db.commit(format!("c{i}"), |root| {
            root.insert(i % keys, text(i, 40));
        })
        .unwrap();
    }
}

fn dump(root: &Root) -> Vec<(u64, String)> {
    root.iter().map(|(k, v)| (k, (*v).clone())).collect()
}

#[test]
fn pruning_reclaims_while_retained_versions_stay_exact() {
    let dir = dir();
    let db: Db<Root> = options().open(path_in(&dir)).unwrap();
    fill(&db, 3000, 40);
    let keep: Vec<_> = (2991..=3000).map(|v| dump(&db.at(v).unwrap())).collect();
    let before = db.size_on_disk();
    let report = db.compact(Keep::Last(10)).unwrap();
    assert_eq!(report.commits_dropped, 2990);
    assert!(report.after_bytes * 10 < report.before_bytes, "{report:?}");
    assert!(db.size_on_disk() * 10 < before);
    assert_eq!(db.log().len(), 10);
    assert!(matches!(db.at(2990), Err(Error::NoVersion(2990))));
    for (index, want) in keep.iter().enumerate() {
        assert_eq!(&dump(&db.at(2991 + index as u64).unwrap()), want);
    }
    db.verify().unwrap();
    db.update(|root| {
        root.insert(999, "after".into());
    })
    .unwrap();
    let path = db.path();
    drop(db);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(db.version(), 3001);
    assert_eq!(db.log().len(), 11);
    assert_eq!(*db.read().get(&999).unwrap(), "after");
    for (index, want) in keep.iter().enumerate() {
        assert_eq!(&dump(&db.at(2991 + index as u64).unwrap()), want);
    }
    db.verify().unwrap();
    admin::verify(&path).unwrap();
}

#[test]
fn head_hash_and_anchors_survive_pruning_and_reclaim() {
    let dir = dir();
    let db: Db<Root> = options().open(path_in(&dir)).unwrap();
    fill(&db, 200, 20);
    let head = db.head_hash();
    let anchor = db.log().hash_at(&195);
    db.compact(Keep::Last(10)).unwrap();
    assert_eq!(db.head_hash(), head);
    assert_eq!(db.log().hash_at(&195), anchor);
    assert_eq!(db.log().hash_at(&150), None);
    let path = db.path();
    drop(db);
    admin::compact(&path, Keep::All).unwrap();
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(db.head_hash(), head);
    assert_eq!(db.log().hash_at(&195), anchor);
    db.verify().unwrap();
}

#[test]
fn every_fill_setting_keeps_data_and_the_full_setting_reclaims_most() {
    let mut sizes = Vec::new();
    for fill_at in [0.0, 0.5, 2.0] {
        let dir = dir();
        let db: Db<Root> = fast()
            .reclaim(Reclaim {
                every_bytes: 0,
                fill: fill_at,
            })
            .open(path_in(&dir))
            .unwrap();
        fill(&db, 4000, 500);
        let want = dump(&db.read());
        let report = db.compact(Keep::Last(1)).unwrap();
        assert_eq!(dump(&db.read()), want);
        db.verify().unwrap();
        assert_eq!(report.moved_bytes > 0, fill_at > 0.0);
        sizes.push(report.after_bytes);
        let path = db.path();
        drop(db);
        let db: Db<Root> = options().open(&path).unwrap();
        assert_eq!(dump(&db.read()), want);
    }
    assert!(sizes[2] < sizes[0], "{sizes:?}");
    assert!(sizes[1] < sizes[0], "{sizes:?}");
}

#[test]
fn repeated_cycles_stay_bounded_and_exact() {
    let dir = dir();
    let db: Db<Root> = options().open(path_in(&dir)).unwrap();
    let mut peak = 0;
    for round in 0..30 {
        fill(&db, 200, 50);
        let want = dump(&db.read());
        db.compact(Keep::Last(5)).unwrap();
        assert_eq!(dump(&db.read()), want, "round {round}");
        peak = peak.max(db.size_on_disk());
    }
    db.verify().unwrap();
    let space = db.space().unwrap();
    assert!(space.log_bytes > 10 * peak, "{space:?} peak {peak}");
    assert!(space.allocated_bytes < space.log_bytes / 10, "{space:?}");
    assert!(space.live_bytes < space.log_bytes / 10, "{space:?}");
}

#[test]
fn a_held_snapshot_keeps_reading_after_its_version_is_reclaimed() {
    let dir = dir();
    let db: Db<Root> = uncached().reclaim(MANUAL).open(path_in(&dir)).unwrap();
    fill(&db, 500, 300);
    let held = db.at(250).unwrap();
    let want = dump(&held);
    fill(&db, 500, 300);
    db.compact(Keep::Last(1)).unwrap();
    assert!(matches!(db.at(250), Err(Error::NoVersion(250))));
    assert_eq!(dump(&held), want);
    assert_eq!(held.try_get(&3).unwrap().as_deref(), want.iter().find(|e| e.0 == 3).map(|e| &e.1));
    db.compact(Keep::Last(1)).unwrap();
    assert_eq!(dump(&held), want);
    let pinned = db.size_on_disk();
    drop(held);
    db.compact(Keep::Last(1)).unwrap();
    assert!(db.size_on_disk() <= pinned);
    db.verify().unwrap();
}

#[test]
fn a_map_cloned_out_of_a_dropped_snapshot_fails_clearly() {
    quiet_ferro_panics();
    let dir = dir();
    let db: Db<Root> = uncached().reclaim(MANUAL).open(path_in(&dir)).unwrap();
    fill(&db, 400, 300);
    let map = (*db.at(100).unwrap()).clone();
    fill(&db, 800, 300);
    db.compact(Keep::Last(1)).unwrap();
    db.compact(Keep::Last(1)).unwrap();
    let mut pruned = 0;
    for key in 0..300 {
        match map.try_get(&key) {
            Err(Error::Pruned(_)) => pruned += 1,
            Ok(_) => {}
            Err(other) => panic!("{other}"),
        }
    }
    assert!(pruned > 0);
}

#[test]
fn a_pruned_map_cannot_be_committed_and_the_error_is_clear() {
    quiet_ferro_panics();
    let dir = dir();
    let db: Db<Root> = uncached().reclaim(MANUAL).open(path_in(&dir)).unwrap();
    fill(&db, 400, 300);
    let stale = (*db.at(100).unwrap()).clone();
    fill(&db, 800, 300);
    db.compact(Keep::Last(1)).unwrap();
    let version = db.version();
    let outcome = guarded(|| {
        db.update(|root| {
            *root = stale.clone();
        })
    });
    match outcome {
        Outcome::Value(Err(Error::Pruned(_))) => {}
        Outcome::Value(other) => panic!("committed: {other:?}"),
        Outcome::FerroPanic(message) => panic!("{message}"),
    }
    assert_eq!(db.version(), version);
    let dirty = guarded(|| {
        db.update(|root| {
            let mut copy = stale.clone();
            copy.insert(5000, "dirty".into());
            *root = copy;
        })
    });
    assert!(matches!(
        dirty,
        Outcome::Value(Err(Error::Pruned(_))) | Outcome::FerroPanic(_)
    ));
    assert_eq!(db.version(), version);
    db.verify().unwrap();
    fill(&db, 3, 3);
    db.verify().unwrap();
}

#[test]
fn restoring_a_held_pruned_snapshot_commits_and_survives_later_cycles() {
    let dir = dir();
    let db: Db<Root> = uncached().reclaim(MANUAL).open(path_in(&dir)).unwrap();
    fill(&db, 300, 200);
    let held = db.at(150).unwrap();
    let want = dump(&held);
    fill(&db, 300, 200);
    db.compact(Keep::Last(1)).unwrap();
    let restored = db.restore(&held).unwrap().version;
    assert_eq!(dump(&db.read()), want);
    drop(held);
    for _ in 0..3 {
        fill(&db, 50, 200);
        db.compact(Keep::Last(200)).unwrap();
    }
    assert_eq!(dump(&db.at(restored).unwrap()), want);
    db.verify().unwrap();
}

#[test]
fn history_log_and_watchers_work_across_cycles() {
    let dir = dir();
    let db: Db<Root> = options().open(path_in(&dir)).unwrap();
    let watcher = db.watch();
    fill(&db, 100, 10);
    let log = db.log();
    let mut iter = log.iter();
    let walk: Vec<u64> = iter.by_ref().rev().take(3).map(|(v, _)| v).collect();
    assert_eq!(walk, vec![100, 99, 98]);
    let mut history = db.history_of(|root: &Root| root.get(&1).map(|v| (*v).clone()));
    let newest = history.next().unwrap();
    db.compact(Keep::Last(1)).unwrap();
    db.compact(Keep::Last(1)).unwrap();
    assert_eq!(iter.next_back().unwrap().0, 97);
    assert_eq!(iter.len(), 96);
    assert!(history.next().unwrap().0.version < newest.0.version);
    assert_eq!(log.iter().len(), 100);
    assert_eq!(watcher.try_recv().unwrap().version(), 100);
    fill(&db, 1, 10);
    assert_eq!(watcher.recv().unwrap().version(), 101);
    assert_eq!(db.history_of(|root: &Root| root.len()).count(), 1);
    assert_eq!(db.log().len(), 2);
}

#[test]
fn branches_merges_and_records_survive() {
    let dir = dir();
    let main: Db<Root> = options().open(path_in(&dir)).unwrap();
    fill(&main, 20, 5);
    let side = main.fork("side").unwrap();
    side.update(|root| {
        root.insert(100, "side".into());
    })
    .unwrap();
    main.update(|root| {
        root.insert(200, "main".into());
    })
    .unwrap();
    main.merge_with(&side, |root, base, theirs| {
        root.merge(base, theirs, |c| c.theirs.map(|v| (*v).clone()));
        ("merge", ())
    })
    .unwrap();
    main.compact(Keep::Last(2)).unwrap();
    assert_eq!(*main.read().get(&100).unwrap(), "side");
    assert_eq!(*side.read().get(&100).unwrap(), "side");
    assert_eq!(side.log().len(), 2);
    match main.common_ancestor(&side) {
        Ok(_) | Err(Error::NoCommonAncestor) => {}
        Err(other) => panic!("{other}"),
    }
    side.update(|root| {
        root.insert(101, "more".into());
    })
    .unwrap();
    main.drop_branch("side").unwrap();
    let report = main.compact(Keep::All).unwrap();
    assert!(report.objects > 0);
    assert_eq!(main.branches(), vec!["main".to_string()]);
    main.verify().unwrap();
    assert!(matches!(side.update(|_| {}), Err(Error::NoBranch(_))));
    assert_eq!(*side.read().get(&101).unwrap(), "more");
}

#[test]
fn group_commit_frames_past_the_superblock_and_reservation_survive_a_cycle() {
    let dir = dir();
    let path = path_in(&dir);
    let db: Db<Root> = Options::new().reclaim(MANUAL).open(&path).unwrap();
    let barrier = Arc::new(Barrier::new(5));
    let handles: Vec<_> = (0..4u64)
        .map(|writer| {
            let (db, barrier) = (db.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                for i in 0..150 {
                    db.update(|root| {
                        root.insert(writer * 1000 + i, text(i, 50));
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    barrier.wait();
    for _ in 0..5 {
        db.compact(Keep::Last(3)).unwrap();
    }
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(db.read().len(), 600);
    assert_eq!(db.version(), 600);
    drop(db);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(db.read().len(), 600);
    db.verify().unwrap();
}

#[test]
fn read_only_handle_follows_moves_without_refresh_and_after_it() {
    quiet_ferro_panics();
    let dir = dir();
    let path = path_in(&dir);
    let db: Db<Root> = uncached().reclaim(MANUAL).open(&path).unwrap();
    fill(&db, 500, 100);
    let reader: Db<Root> = Options::new()
        .read_only()
        .cache_bytes(1)
        .open(&path)
        .unwrap();
    let held = reader.read();
    let want = dump(&held);
    db.compact(Keep::Last(1)).unwrap();
    assert_eq!(dump(&held), want);
    assert_eq!(dump(&reader.read()), want);
    fill(&db, 500, 100);
    db.compact(Keep::Last(1)).unwrap();
    let lagging = guarded(|| dump(&held));
    match lagging {
        Outcome::Value(found) => assert_eq!(found, want),
        Outcome::FerroPanic(message) => assert!(message.contains("pruned"), "{message}"),
    }
    assert!(reader.refresh().unwrap());
    assert_eq!(reader.version(), 1000);
    assert_eq!(dump(&reader.read()), dump(&db.read()));
    reader.verify().unwrap();
    assert!(matches!(reader.compact(Keep::All), Err(Error::ReadOnly)));
    let fresh: Db<Root> = Options::new().read_only().open(&path).unwrap();
    assert_eq!(dump(&fresh.read()), dump(&db.read()));
}

#[test]
fn a_read_only_reader_keeps_up_with_cycles_from_another_handle() {
    let dir = dir();
    let path = path_in(&dir);
    let db: Db<Root> = uncached().reclaim(MANUAL).open(&path).unwrap();
    fill(&db, 100, 30);
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let (done, path) = (done.clone(), path.clone());
        thread::spawn(move || {
            let reader: Db<Root> = Options::new().read_only().cache_bytes(1).open(&path).unwrap();
            let mut seen = 0;
            while !done.load(Ordering::Relaxed) {
                reader.refresh().unwrap();
                let snapshot = reader.read();
                if let Ok(rows) = catch(|| snapshot.iter().count()) {
                    assert_eq!(rows, 30);
                    seen += 1;
                }
                let fresh: Db<Root> = Options::new().read_only().cache_bytes(1).open(&path).unwrap();
                assert!(fresh.version() >= 100);
            }
            seen
        })
    };
    for _ in 0..60 {
        fill(&db, 40, 30);
        db.compact(Keep::Last(2)).unwrap();
    }
    done.store(true, Ordering::Relaxed);
    assert!(reader.join().unwrap() > 0);
}

fn catch<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    quiet_ferro_panics();
    match guarded(f) {
        Outcome::Value(value) => Ok(value),
        Outcome::FerroPanic(message) => {
            assert!(message.contains("pruned"), "{message}");
            Err(message)
        }
    }
}

fn hooked(path: &Path, cache: u64, hook: impl Fn(&'static str) + Send + Sync + 'static) -> Db<Root> {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(cache)
        .reclaim(MANUAL)
        .hook(Hook(Arc::new(hook)))
        .open(path)
        .unwrap()
}

#[test]
fn content_rewritten_after_its_first_copy_died_reads_through_the_new_copy() {
    for cache in [1, 1 << 20, 1 << 26] {
        let dir = dir();
        let path = path_in(&dir);
        let db: Db<Root> = fast().cache_bytes(cache).reclaim(MANUAL).open(&path).unwrap();
        for _ in 0..6 {
            fill(&db, 200, 40);
            db.compact(Keep::Last(3)).unwrap();
            db.verify().unwrap();
        }
        let want = dump(&db.read());
        fill(&db, 200, 40);
        assert_eq!(dump(&db.read()), want);
        db.verify().unwrap();
        drop(db);
        let db: Db<Root> = options().open(&path).unwrap();
        assert_eq!(dump(&db.read()), want);
    }
}

#[test]
fn a_dead_object_found_by_dedupe_is_not_linked_after_a_cycle() {
    let dir = dir();
    let path = path_in(&dir);
    let db = hooked(&path, 1 << 20, |_| {});
    let big = text(1, 4000);
    db.update(|root| {
        root.insert(1, big.clone());
    })
    .unwrap();
    db.update(|root| {
        root.remove(&1);
    })
    .unwrap();
    fill(&db, 3000, 3);
    db.compact(Keep::Last(1)).unwrap();
    db.update(|root| {
        root.insert(2, big.clone());
    })
    .unwrap();
    assert_eq!(*db.read().get(&2).unwrap(), big);
    db.verify().unwrap();
    db.compact(Keep::Last(1)).unwrap();
    assert_eq!(*db.read().get(&2).unwrap(), big);
}

#[test]
fn an_object_revived_by_dedupe_during_a_cycle_is_kept() {
    for stage in ["marked", "copied", "tabled"] {
        let dir = dir();
        let path = path_in(&dir);
        let slot: Arc<Mutex<Option<Db<Root>>>> = Arc::new(Mutex::new(None));
        let big = text(7, 5000);
        let hook = {
            let (slot, big) = (slot.clone(), big.clone());
            move |name: &'static str| {
                if name == stage {
                    let db = slot.lock().unwrap().clone().unwrap();
                    db.update(|root| {
                        root.insert(2, big.clone());
                    })
                    .unwrap();
                }
            }
        };
        let db = hooked(&path, 1 << 20, hook);
        *slot.lock().unwrap() = Some(db.clone());
        db.update(|root| {
            root.insert(1, big.clone());
        })
        .unwrap();
        db.update(|root| {
            root.remove(&1);
        })
        .unwrap();
        fill(&db, 3000, 3);
        db.compact(Keep::Last(1)).unwrap();
        *slot.lock().unwrap() = None;
        assert_eq!(*db.read().get(&2).unwrap(), big, "{stage}");
        db.verify().unwrap();
        drop(db);
        let db: Db<Root> = options().open(&path).unwrap();
        assert_eq!(*db.read().get(&2).unwrap(), big, "{stage}");
        db.compact(Keep::Last(1)).unwrap();
        assert_eq!(*db.read().get(&2).unwrap(), big, "{stage}");
        db.verify().unwrap();
    }
}

#[test]
fn commits_forks_and_drops_during_every_stage_of_a_cycle_are_kept() {
    for stage in ["marked", "copied", "tabled", "published", "punched"] {
        let dir = dir();
        let path = path_in(&dir);
        let slot: Arc<Mutex<Option<Db<Root>>>> = Arc::new(Mutex::new(None));
        let hook = {
            let slot = slot.clone();
            move |name: &'static str| {
                if name == stage {
                    let db = slot.lock().unwrap().clone().unwrap();
                    fill(&db, 30, 9);
                    let side = db.fork("late").unwrap();
                    side.update(|root| {
                        root.insert(77, "side".into());
                    })
                    .unwrap();
                    db.drop_branch("gone").unwrap();
                }
            }
        };
        let db = hooked(&path, 1 << 20, hook);
        *slot.lock().unwrap() = Some(db.clone());
        fill(&db, 500, 9);
        db.fork("gone").unwrap();
        db.compact(Keep::Last(4)).unwrap();
        *slot.lock().unwrap() = None;
        assert_eq!(db.version(), 530, "{stage}");
        let want = dump(&db.read());
        db.verify().unwrap();
        let side = db.branch("late").unwrap();
        assert_eq!(*side.read().get(&77).unwrap(), "side");
        drop((db, side));
        let db: Db<Root> = options().open(&path).unwrap();
        assert_eq!(dump(&db.read()), want, "{stage}");
        assert_eq!(db.branches(), vec!["late".to_string(), "main".to_string()]);
        db.compact(Keep::Last(1)).unwrap();
        assert_eq!(dump(&db.read()), want, "{stage}");
        db.verify().unwrap();
    }
}

#[test]
fn the_history_option_prunes_on_commit_and_reclaims_in_the_background() {
    let dir = dir();
    let path = path_in(&dir);
    let db: Db<Root> = fast()
        .history(Keep::Last(5))
        .reclaim(Reclaim {
            every_bytes: 256 << 10,
            fill: 0.5,
        })
        .open(&path)
        .unwrap();
    let mut peak = 0;
    let started = Instant::now();
    let mut commits = 0;
    while db.space().unwrap().cycles < 3 && started.elapsed() < Duration::from_secs(30) {
        fill(&db, 200, 40);
        commits += 200;
        peak = peak.max(db.size_on_disk());
    }
    assert!(db.space().unwrap().cycles >= 3, "{:?}", db.space());
    assert_eq!(db.log().len(), 5);
    assert_eq!(db.log().first().unwrap().0, commits - 4);
    assert!(matches!(db.at(commits - 5), Err(Error::NoVersion(_))));
    assert!(db.space().unwrap().last_error.is_none());
    assert!(db.space().unwrap().log_bytes > 2 * peak);
    let want = dump(&db.read());
    drop(db);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(dump(&db.read()), want);
    db.verify().unwrap();
}

#[test]
fn dropping_the_last_handle_stops_the_background_thread_and_frees_the_lock() {
    let dir = dir();
    let path = path_in(&dir);
    for _ in 0..5 {
        let db: Db<Root> = fast()
            .history(Keep::Last(1))
            .reclaim(Reclaim {
                every_bytes: 4 << 10,
                fill: 0.5,
            })
            .open(&path)
            .unwrap();
        fill(&db, 300, 20);
        drop(db);
        let again: Db<Root> = options().open(&path).unwrap();
        again.verify().unwrap();
    }
}

#[test]
fn shutting_down_during_a_punch_frees_nothing_that_was_kept() {
    let dir = dir();
    let path = path_in(&dir);
    let reached = Arc::new(Barrier::new(2));
    let once = Arc::new(AtomicBool::new(false));
    let hook = {
        let (reached, once) = (reached.clone(), once.clone());
        move |name: &'static str| {
            if name == "published" && !once.swap(true, Ordering::SeqCst) {
                reached.wait();
                thread::sleep(Duration::from_millis(100));
            }
        }
    };
    let db: Db<Root> = fast()
        .history(Keep::Last(2))
        .reclaim(Reclaim {
            every_bytes: 32 << 10,
            fill: 0.0,
        })
        .hook(Hook(Arc::new(hook)))
        .open(&path)
        .unwrap();
    while !once.load(Ordering::SeqCst) {
        fill(&db, 50, 400);
    }
    reached.wait();
    let want = dump(&db.read());
    drop(db);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(dump(&db.read()), want);
    db.verify().unwrap();
}

#[test]
fn offline_tools_work_after_online_cycles() {
    let dir = dir();
    let path = path_in(&dir);
    let want = {
        let db: Db<Root> = options().open(&path).unwrap();
        fill(&db, 800, 50);
        db.compact(Keep::Last(3)).unwrap();
        fill(&db, 100, 50);
        db.compact(Keep::Last(3)).unwrap();
        dump(&db.read())
    };
    assert_eq!(admin::info(&path).unwrap().branches[0].commits, 3);
    assert_eq!(admin::log(&path, "main", 10).unwrap().len(), 3);
    admin::verify(&path).unwrap();
    let report = admin::reclaim(&path, Keep::Last(1)).unwrap();
    assert_eq!(report.commits_dropped, 2);
    assert!(admin::space(&path).unwrap().allocated_bytes < 2 << 20);
    let sparse = std::fs::metadata(&path).unwrap().len();
    let copied = admin::compact(&path, Keep::All).unwrap();
    assert!(copied.after_bytes < sparse);
    assert!(std::fs::metadata(&path).unwrap().len() < sparse / 4);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(dump(&db.read()), want);
    assert_eq!(db.log().len(), 1);
    db.verify().unwrap();
}

#[test]
fn a_cycle_on_an_empty_database_does_nothing() {
    let dir = dir();
    let empty: Db<Root> = options().open(path_in(&dir)).unwrap();
    let none = empty.compact(Keep::Last(1)).unwrap();
    assert_eq!(none.objects, 0);
    assert_eq!(none.moved_bytes, 0);
    assert_eq!(empty.version(), 0);
}

#[test]
fn keep_within_prunes_by_age_at_each_cycle() {
    let dir = dir();
    let db: Db<Root> = options().open(path_in(&dir)).unwrap();
    fill(&db, 50, 5);
    thread::sleep(Duration::from_millis(60));
    let boundary = db.version();
    db.update(|root| {
        root.insert(100, "new".into());
    })
    .unwrap();
    db.compact(Keep::Within(Duration::from_millis(30))).unwrap();
    assert_eq!(db.log().first().unwrap().0, boundary + 1);
    assert_eq!(db.log().len(), 1);
    assert!(matches!(db.as_of(1), Err(Error::NoVersion(_))));
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Pair {
    left: Map<u64, u64>,
    right: Map<u64, Vec<u8>>,
    total: u64,
}

fn consistent(pair: &Pair) {
    let sum: u64 = pair.left.iter().map(|(_, v)| *v).sum();
    assert_eq!(sum, pair.total);
    for (key, value) in pair.right.iter() {
        assert_eq!(value.len(), 900 + (key % 16) as usize);
    }
}

#[test]
fn snapshots_stay_consistent_under_writers_readers_and_cycles() {
    quiet_ferro_panics();
    let dir = dir();
    let path = path_in(&dir);
    let db: Db<Pair> = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 16)
        .history(Keep::Last(8))
        .reclaim(MANUAL)
        .open(&path)
        .unwrap();
    db.update(|pair| {
        pair.total = 0;
    })
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let commits = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();
    for writer in 0..3u64 {
        let (db, stop, commits) = (db.clone(), stop.clone(), commits.clone());
        threads.push(thread::spawn(move || {
            let mut rng = Rng::new(writer + 1);
            while !stop.load(Ordering::Relaxed) {
                let key = rng.below(64);
                let bump = rng.below(5) + 1;
                db.update(|pair| {
                    let value = pair.left.get(&key).map_or(0, |v| *v);
                    pair.left.insert(key, value + bump);
                    pair.total += bump;
                    pair.right.insert(key, vec![bump as u8; 900 + (key % 16) as usize]);
                })
                .unwrap();
                commits.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for reader in 0..4u64 {
        let (db, stop, reads) = (db.clone(), stop.clone(), reads.clone());
        threads.push(thread::spawn(move || {
            let mut rng = Rng::new(reader + 100);
            let mut held: Vec<Snapshot<Pair>> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let snapshot = db.read();
                consistent(&snapshot);
                held.push(snapshot);
                if held.len() > 6 {
                    let index = rng.below(held.len() as u64) as usize;
                    consistent(&held.swap_remove(index));
                }
                match db.at(db.version().saturating_sub(rng.below(6))) {
                    Ok(snapshot) => consistent(&snapshot),
                    Err(Error::NoVersion(_)) => {}
                    Err(other) => panic!("{other}"),
                }
                let log = db.log();
                let versions: Vec<u64> = log.iter().rev().take(4).map(|(v, _)| v).collect();
                assert!(versions.windows(2).all(|pair| pair[0] == pair[1] + 1));
                reads.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    {
        let (db, stop) = (db.clone(), stop.clone());
        threads.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                db.verify().unwrap();
            }
        }));
    }
    let started = Instant::now();
    let mut cycles = 0;
    let mut pauses = Duration::ZERO;
    while started.elapsed() < Duration::from_secs(4) {
        let report = db.compact(Keep::Last(8)).unwrap();
        pauses = pauses.max(report.pause);
        cycles += 1;
    }
    stop.store(true, Ordering::Relaxed);
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(cycles > 3, "{cycles} cycles");
    assert!(commits.load(Ordering::Relaxed) > 100);
    assert!(reads.load(Ordering::Relaxed) > 10);
    consistent(&db.read());
    db.verify().unwrap();
    let want = db.read().total;
    drop(db);
    let db: Db<Pair> = fast().reclaim(MANUAL).open(&path).unwrap();
    assert_eq!(db.read().total, want);
    db.verify().unwrap();
}

#[derive(Clone, Debug)]
enum Step {
    Put(usize, u64, u64),
    Remove(usize, u64),
    Fork(usize),
    Drop(usize),
    Restore(usize, u64),
    Hold(usize, u64),
    Release(usize),
    Compact(u8, u64),
    Reopen,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        30 => (0..3usize, 0..24u64, 0..9u64).prop_map(|(b, k, v)| Step::Put(b, k, v)),
        6 => (0..3usize, 0..24u64).prop_map(|(b, k)| Step::Remove(b, k)),
        2 => (0..3usize).prop_map(Step::Fork),
        1 => (1..3usize).prop_map(Step::Drop),
        3 => (0..3usize, 0..60u64).prop_map(|(b, v)| Step::Restore(b, v)),
        4 => (0..3usize, 0..60u64).prop_map(|(b, v)| Step::Hold(b, v)),
        3 => (0..8usize).prop_map(Step::Release),
        7 => (0..4u8, 1..6u64).prop_map(|(mode, count)| Step::Compact(mode, count)),
        2 => Just(Step::Reopen),
    ]
}

const BRANCHES: [&str; 3] = ["main", "one", "two"];

type Held = (Snapshot<Root>, Vec<(u64, String)>);

#[derive(Default, Clone)]
struct BranchModel {
    versions: BTreeMap<u64, Vec<(u64, String)>>,
    head: u64,
}

fn body(key: u64, value: u64) -> String {
    text(key * 31 + value, if key.is_multiple_of(5) { 700 } else { 30 })
}

fn check_all(main: &Db<Root>, models: &[Option<BranchModel>]) {
    for (index, model) in models.iter().enumerate() {
        let Some(model) = model else {
            continue;
        };
        let db = main.branch(BRANCHES[index]).unwrap();
        assert_eq!(db.version(), model.head);
        let log: Vec<u64> = db.log().iter().map(|(v, _)| v).collect();
        for version in &log {
            assert_eq!(
                dump(&db.at(*version).unwrap()),
                model.versions[version],
                "{} v{version}",
                BRANCHES[index]
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(48)))]

    #[test]
    fn a_model_of_every_retained_version_matches_through_cycles_and_reopens(
        steps in prop::collection::vec(step(), 1..120),
        fill_at in prop_oneof![Just(0.0), Just(0.5), Just(2.0)],
        cache in prop_oneof![Just(1u64), Just(1u64 << 20)],
    ) {
        let dir = dir();
        let path = path_in(&dir);
        let open = |path: &Path| -> Db<Root> {
            fast()
                .cache_bytes(cache)
                .reclaim(Reclaim { every_bytes: 0, fill: fill_at })
                .open(path)
                .unwrap()
        };
        let mut main = open(&path);
        let mut models: Vec<Option<BranchModel>> = vec![Some(BranchModel::default()), None, None];
        models[0].as_mut().unwrap().versions.insert(0, Vec::new());
        let mut held: Vec<Held> = Vec::new();
        for step in &steps {
            match step {
                Step::Put(b, k, v) => {
                    if models[*b].is_some() {
                        let db = main.branch(BRANCHES[*b]).unwrap();
                        let (k, v) = (*k, *v);
                        let done = db.commit_with(|root| {
                            root.insert(k, body(k, v));
                            ("put", ())
                        }).unwrap();
                        if done.changed {
                            let model = models[*b].as_mut().unwrap();
                            model.head = done.version;
                            model.versions.insert(done.version, dump(&db.read()));
                        }
                    }
                }
                Step::Remove(b, k) => {
                    if models[*b].is_some() {
                        let db = main.branch(BRANCHES[*b]).unwrap();
                        let done = db.commit_with(|root| {
                            root.remove(k);
                            ("remove", ())
                        }).unwrap();
                        if done.changed {
                            let model = models[*b].as_mut().unwrap();
                            model.head = done.version;
                            model.versions.insert(done.version, dump(&db.read()));
                        }
                    }
                }
                Step::Fork(from) => {
                    if models[*from].is_some()
                        && let Some(slot) = (1..3).find(|i| models[*i].is_none())
                    {
                        main.branch(BRANCHES[*from]).unwrap().fork(BRANCHES[slot]).unwrap();
                        models[slot] = models[*from].clone();
                    }
                }
                Step::Drop(index) => {
                    if models[*index].is_some() {
                        main.drop_branch(BRANCHES[*index]).unwrap();
                        models[*index] = None;
                    }
                }
                Step::Restore(b, version) => {
                    if models[*b].is_some() {
                        let db = main.branch(BRANCHES[*b]).unwrap();
                        let log: Vec<u64> = db.log().iter().map(|(v, _)| v).collect();
                        if !log.is_empty() {
                            let chosen = log[*version as usize % log.len()];
                            let snapshot = db.at(chosen).unwrap();
                            let done = db.restore(&snapshot).unwrap();
                            if done.changed {
                                let model = models[*b].as_mut().unwrap();
                                model.head = done.version;
                                model.versions.insert(done.version, dump(&db.read()));
                            }
                        }
                    }
                }
                Step::Hold(b, version) => {
                    if models[*b].is_some() {
                        let db = main.branch(BRANCHES[*b]).unwrap();
                        let log: Vec<u64> = db.log().iter().map(|(v, _)| v).collect();
                        if !log.is_empty() {
                            let chosen = log[*version as usize % log.len()];
                            let snapshot = db.at(chosen).unwrap();
                            let content = dump(&snapshot);
                            held.push((snapshot, content));
                        }
                    }
                }
                Step::Release(index) => {
                    if !held.is_empty() {
                        held.remove(index % held.len());
                    }
                }
                Step::Compact(mode, count) => {
                    let keep = match mode {
                        0 => Keep::All,
                        1 => Keep::Last(*count as usize),
                        2 => Keep::Last(1),
                        _ => Keep::Since(u64::MAX),
                    };
                    main.compact(keep).unwrap();
                    main.verify().unwrap();
                    check_all(&main, &models);
                    for (snapshot, content) in &held {
                        prop_assert_eq!(&dump(snapshot), content);
                    }
                }
                Step::Reopen => {
                    held.clear();
                    drop(main);
                    main = open(&path);
                    check_all(&main, &models);
                }
            }
        }
        check_all(&main, &models);
        main.verify().unwrap();
        for (snapshot, content) in &held {
            prop_assert_eq!(&dump(snapshot), content);
        }
    }
}
