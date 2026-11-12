mod common;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use common::{Rng, text};
use ferro::db::Clock;
use ferro::store::{Store, StoreOptions, record_len};
use ferro::{Db, Durability, Error, Keep, Map, Options, Reclaim, admin};
use tempfile::TempDir;

type Root = Map<u64, String>;

const LOG_START: u64 = 8192;
const MANUAL: Reclaim = Reclaim { every_bytes: 0 };

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|error| error.into_inner())
}

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn options(threads: usize) -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1)
        .reclaim(MANUAL)
        .copy_threads(threads)
}

fn build(path: &Path, commits: u64, keys: u64, len: usize) {
    let db: Db<Root> = options(1).open(path).unwrap();
    let mut rng = Rng::new(commits ^ keys);
    for _ in 0..commits {
        db.update(|root| {
            for _ in 0..4 {
                let key = rng.below(keys);
                root.insert(key, text(rng.next(), len + rng.below(len as u64) as usize));
            }
        })
        .unwrap();
    }
}

fn dump(root: &Root) -> Vec<(u64, String)> {
    root.iter().map(|(k, v)| (k, (*v).clone())).collect()
}

fn temp_of(path: &Path) -> PathBuf {
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".compacting");
    PathBuf::from(temp)
}

#[cfg(target_os = "linux")]
fn verifying_threads() -> usize {
    fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|task| fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .filter(|name| name.trim() == "ferro-copy")
        .count()
}

fn layout_of(path: &Path) -> Vec<(u64, u64, Vec<u64>)> {
    let options = StoreOptions {
        read_only: true,
        ..StoreOptions::default()
    };
    let (store, head) = Store::open(path, options).unwrap();
    let mut records = Vec::new();
    store
        .walk(head.unwrap(), |ptr, object| {
            let end = ptr.offset + record_len(ptr.offset, object) as u64;
            let links = object.links.iter().map(|link| link.offset).collect();
            records.push((ptr.offset, end, links));
        })
        .unwrap();
    store.close();
    records.sort_by_key(|record| record.0);
    records.dedup_by_key(|record| record.0);
    records
}

#[test]
fn several_threads_copy_the_same_bytes_as_one() {
    let _turn = serial();
    let dir = dir();
    let source = dir.path().join("source.ferro");
    build(&source, 1500, 600, 300);
    for keep in [Keep::All, Keep::Last(20)] {
        let mut firsts = Vec::new();
        let mut copies = Vec::new();
        for threads in [1, 2, 4, 8] {
            let path = dir.path().join(format!("copy-{threads}.ferro"));
            fs::copy(&source, &path).unwrap();
            let db: Db<Root> = options(threads).open(&path).unwrap();
            let want = dump(&db.read());
            let report = db.compact(keep).unwrap();
            assert!(report.after_bytes > 512 << 10, "{report:?}");
            assert_eq!(
                report.handed_out > 10,
                threads > 1,
                "{threads} threads: {report:?}"
            );
            firsts.push(fs::read(&path).unwrap());
            let again = db.compact(keep).unwrap();
            assert_eq!(
                again.handed_out > 10,
                threads > 1,
                "{threads} threads: {again:?}"
            );
            assert_eq!(dump(&db.read()), want);
            db.verify().unwrap();
            drop(db);
            copies.push(fs::read(&path).unwrap());
        }
        assert!(
            firsts.windows(2).all(|pair| pair[0] == pair[1]),
            "{keep:?}: the first copies"
        );
        assert!(copies.windows(2).all(|pair| pair[0] == pair[1]), "{keep:?}");
    }
}

type Nested = Map<u64, Map<u64, String>>;

fn build_branched(path: &Path, seed: u64) -> u64 {
    let ticks = Arc::new(AtomicU64::new(1_700_000_000_000));
    let clock = {
        let ticks = ticks.clone();
        Clock(Arc::new(move || ticks.fetch_add(1, Ordering::SeqCst)))
    };
    let main: Db<Nested> = options(1)
        .cache_bytes(1 << 20)
        .clock(clock)
        .open(path)
        .unwrap();
    let mut rng = Rng::new(seed);
    let mut branches = vec![main.clone()];
    for step in 0..2500u64 {
        let db = branches[rng.below(branches.len() as u64) as usize].clone();
        match rng.below(20) {
            0 if branches.len() < 8 => branches.push(db.fork(&format!("b{step}")).unwrap()),
            1 => {
                let other = branches[rng.below(branches.len() as u64) as usize].clone();
                if other.branch_name() != db.branch_name() {
                    let _ = db.merge_with(&other, |ours, base, theirs| {
                        let applied = ours.merge(base, theirs, |conflict| conflict.keep_ours());
                        ("merge", applied)
                    });
                }
            }
            2 if db.version() > 3 => {
                let back = db.at(db.version() - 1 - rng.below(3)).unwrap();
                db.restore(&back).unwrap();
            }
            _ => {
                let (outer, inner) = (rng.below(40), rng.below(200));
                let len = match rng.below(100) {
                    0..3 => 20_000,
                    _ => 30 + rng.below(200) as usize,
                };
                let value = text(rng.next(), len);
                db.update(|root| {
                    let mut map = root.get(&outer).map(|m| (*m).clone()).unwrap_or_default();
                    map.insert(inner, value.clone());
                    root.insert(outer, map);
                })
                .unwrap();
            }
        }
    }
    ticks.load(Ordering::SeqCst)
}

#[test]
fn several_threads_copy_branched_merged_nested_history_into_the_same_bytes() {
    let _turn = serial();
    let dir = dir();
    let source = dir.path().join("source.ferro");
    let end = build_branched(&source, 7);
    assert!(fs::metadata(&source).unwrap().len() > 2 << 20);
    for keep in [Keep::All, Keep::Last(15), Keep::Since(end - 300)] {
        let mut copies = Vec::new();
        for threads in [1, 2, 3, 8] {
            let path = dir.path().join(format!("copy-{threads}.ferro"));
            fs::copy(&source, &path).unwrap();
            let db: Db<Nested> = options(threads).open(&path).unwrap();
            db.compact(keep).unwrap();
            db.verify_typed()
                .unwrap_or_else(|error| panic!("{keep:?} at {threads} threads: {error}"));
            let first = fs::read(&path).unwrap();
            db.compact(keep).unwrap();
            db.verify_typed().unwrap();
            drop(db);
            copies.push((threads, first, fs::read(&path).unwrap()));
        }
        for pair in copies.windows(2) {
            let (one, other) = (pair[0].0, pair[1].0);
            assert!(
                pair[0].1 == pair[1].1,
                "{keep:?}: first copies at {one} and {other}"
            );
            assert!(
                pair[0].2 == pair[1].2,
                "{keep:?}: second copies at {one} and {other}"
            );
        }
    }
}

#[test]
fn records_follow_their_children_and_tile_the_copy() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    build(&path, 1200, 500, 300);
    let db: Db<Root> = options(4).open(&path).unwrap();
    db.compact(Keep::All).unwrap();
    drop(db);
    let records = layout_of(&path);
    assert!(records.len() > 4096, "{}", records.len());
    let mut at = LOG_START;
    for (start, end, links) in &records {
        assert_eq!(
            *start, at,
            "a gap or an overlap before the record at {start}"
        );
        for link in links {
            assert!(
                link < start,
                "the record at {start} links forward to {link}"
            );
        }
        at = *end;
    }
    assert_eq!(at, fs::metadata(&path).unwrap().len());
}

fn retained_only_by_history(path: &Path) -> (u64, u64) {
    let all = admin::layout(path, Keep::All).unwrap().extents;
    let recent: HashSet<u64> = admin::layout(path, Keep::Last(50))
        .unwrap()
        .extents
        .iter()
        .map(|extent| extent.0)
        .collect();
    let bytes = fs::read(path).unwrap();
    all.iter()
        .filter(|(offset, _)| !recent.contains(offset))
        .map(|(offset, len)| (*offset, u64::from(*len)))
        .find(|(offset, len)| {
            let record = &bytes[*offset as usize..(*offset + *len) as usize];
            let mut flags = 0;
            let mut at = 0;
            for _ in 0..2 {
                flags = 0;
                let mut shift = 0;
                loop {
                    let byte = record[at];
                    at += 1;
                    flags |= u64::from(byte & 0x7f) << shift;
                    shift += 7;
                    if byte & 0x80 == 0 {
                        break;
                    }
                }
            }
            flags >> 1 == 0 && *len > 64
        })
        .unwrap()
}

fn damage(path: &Path, at: u64) {
    let mut bytes = fs::read(path).unwrap();
    bytes[at as usize] ^= 0x20;
    fs::write(path, bytes).unwrap();
}

#[test]
fn a_hash_mismatch_found_while_copying_fails_the_cycle_and_leaves_the_file() {
    let _turn = serial();
    let dir = dir();
    let source = dir.path().join("source.ferro");
    build(&source, 2000, 600, 300);
    let (offset, len) = retained_only_by_history(&source);
    for threads in [1, 4] {
        let path = dir.path().join(format!("damaged-{threads}.ferro"));
        fs::copy(&source, &path).unwrap();
        damage(&path, offset + len - 8);
        let damaged = fs::read(&path).unwrap();
        let db: Db<Root> = options(threads).open(&path).unwrap();
        let want = dump(&db.read());
        let outcome = db.compact(Keep::All);
        assert!(
            matches!(&outcome, Err(Error::Corrupt(message)) if message.contains(&offset.to_string())),
            "{threads} threads: {outcome:?}"
        );
        assert!(!temp_of(&path).exists(), "{threads} threads");
        assert!(db.space().unwrap().last_error.is_some());
        db.update(|root| {
            root.insert(u64::MAX, "after".into());
        })
        .unwrap();
        let mut after = want.clone();
        after.push((u64::MAX, "after".into()));
        assert_eq!(dump(&db.read()), after);
        let report = db.compact(Keep::Last(50)).unwrap();
        assert!(report.after_bytes < report.before_bytes, "{report:?}");
        db.verify().unwrap();
        drop(db);
        let db: Db<Root> = options(threads).open(&path).unwrap();
        assert_eq!(dump(&db.read()), after);
        drop(db);
        let offline = dir.path().join(format!("offline-{threads}.ferro"));
        fs::write(&offline, &damaged).unwrap();
        let outcome = admin::compact(&offline, Keep::All);
        assert!(matches!(outcome, Err(Error::Corrupt(_))), "{outcome:?}");
        assert_eq!(fs::read(&offline).unwrap(), damaged);
        assert!(!temp_of(&offline).exists());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_small_cycle_verifies_on_the_calling_thread_and_a_large_one_on_more() {
    let _turn = serial();
    let dir = dir();
    let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
    let hook = {
        let seen = seen.clone();
        move |name: &'static str| {
            if name == "copied" {
                seen.lock().unwrap().push(verifying_threads());
            }
        }
    };
    let small = dir.path().join("small.ferro");
    build(&small, 40, 20, 100);
    let db: Db<Root> = options(0)
        .hook(ferro::db::Hook(std::sync::Arc::new(hook.clone())))
        .open(&small)
        .unwrap();
    db.compact(Keep::All).unwrap();
    drop(db);
    let large = dir.path().join("large.ferro");
    build(&large, 1500, 600, 300);
    for threads in [1, 4] {
        let db: Db<Root> = options(threads)
            .hook(ferro::db::Hook(std::sync::Arc::new(hook.clone())))
            .open(&large)
            .unwrap();
        db.compact(Keep::All).unwrap();
        drop(db);
    }
    assert_eq!(verifying_threads(), 0);
    assert_eq!(*seen.lock().unwrap(), [0, 0, 3]);
}

fn build_long(path: &Path, seed: u64) {
    let db: Db<Root> = options(1).open(path).unwrap();
    let mut rng = Rng::new(seed);
    let put = |db: &Db<Root>, rng: &mut Rng| {
        let (key, value) = (
            rng.below(400),
            text(rng.next(), 40 + rng.below(200) as usize),
        );
        db.update(|root| {
            root.insert(key, value.clone());
        })
        .unwrap();
    };
    for _ in 0..2600 {
        put(&db, &mut rng);
    }
    let side = db.fork("side").unwrap();
    for step in 0..2300u64 {
        put(&side, &mut rng);
        if step % 3 == 0 {
            put(&db, &mut rng);
        }
    }
    db.merge_with(&side, |root, base, theirs| {
        root.merge(base, theirs, |c| c.theirs.map(|v| (*v).clone()));
        ("merge", ())
    })
    .unwrap();
    for _ in 0..50 {
        put(&db, &mut rng);
    }
}

#[test]
fn long_chains_read_ahead_copy_the_same_bytes_at_every_thread_count() {
    let _turn = serial();
    let dir = dir();
    let source = dir.path().join("source.ferro");
    build_long(&source, 11);
    for keep in [Keep::All, Keep::Last(2100), Keep::Last(3000)] {
        let mut copies = Vec::new();
        for threads in [1, 2, 3, 5, 8, 16] {
            let path = dir.path().join(format!("long-{threads}.ferro"));
            fs::copy(&source, &path).unwrap();
            let db: Db<Root> = options(threads).open(&path).unwrap();
            let want = dump(&db.read());
            db.compact(keep).unwrap();
            assert_eq!(dump(&db.read()), want);
            db.verify().unwrap();
            let first = fs::read(&path).unwrap();
            db.compact(keep).unwrap();
            drop(db);
            copies.push((threads, first, fs::read(&path).unwrap()));
        }
        for pair in copies.windows(2) {
            let (one, other) = (pair[0].0, pair[1].0);
            assert!(
                pair[0].1 == pair[1].1,
                "{keep:?}: first copies at {one} and {other}"
            );
            assert!(
                pair[0].2 == pair[1].2,
                "{keep:?}: second copies at {one} and {other}"
            );
        }
    }
}

fn entries_by_version(path: &Path) -> Vec<(u64, u64)> {
    let db: Db<Root> = options(1).open(path).unwrap();
    let roots: std::collections::HashMap<u64, u64> = (1..=db.version())
        .map(|version| {
            (
                db.at(version).unwrap().commit().root.unwrap().offset,
                version,
            )
        })
        .collect();
    drop(db);
    let bytes = fs::read(path).unwrap();
    let mut entries: Vec<(u64, u64)> = admin::layout(path, Keep::All)
        .unwrap()
        .extents
        .iter()
        .filter_map(|(offset, _)| {
            let (links, _) = common::link_slots(&bytes, *offset as usize);
            Some((*roots.get(&links.first()?.target)?, *offset))
        })
        .collect();
    entries.sort();
    entries
}

#[test]
fn a_damaged_entry_in_a_long_chain_fails_alike_at_every_thread_count() {
    let _turn = serial();
    let dir = dir();
    let source = dir.path().join("source.ferro");
    build(&source, 3000, 600, 60);
    let entries = entries_by_version(&source);
    assert_eq!(entries.len(), 3000);
    let bytes = fs::read(&source).unwrap();
    for version in [
        2999, 2252, 2251, 2250, 2249, 1501, 1500, 1499, 751, 750, 749, 2, 1,
    ] {
        let at = entries[version - 1].1 as usize;
        let (links, _) = common::link_slots(&bytes, at);
        let payload = at + 3;
        let prev = links.get(1).map(|link| link.at);
        let skip = links.get(2).map(|link| link.at);
        for (what, byte) in [("payload", Some(payload)), ("prev", prev), ("skip", skip)] {
            let Some(byte) = byte else { continue };
            let mut outcomes = Vec::new();
            for threads in [1, 2, 3, 5, 8] {
                let path = dir.path().join(format!("damaged-{threads}.ferro"));
                let mut damaged = bytes.clone();
                damaged[byte] ^= 0x01;
                fs::write(&path, &damaged).unwrap();
                let db: Db<Root> = options(threads).open(&path).unwrap();
                let outcome = db.compact(Keep::All).map(|_| ());
                if outcome.is_ok() {
                    db.verify().unwrap();
                }
                drop(db);
                outcomes.push((threads, format!("{outcome:?}")));
            }
            for pair in outcomes.windows(2) {
                assert_eq!(
                    pair[0].1, pair[1].1,
                    "v{version} {what}: {} against {} threads",
                    pair[0].0, pair[1].0
                );
            }
        }
    }
}

#[test]
fn cycles_under_writers_on_long_chains_keep_every_retained_commit() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let db: Db<Root> = options(8).open(&path).unwrap();
    for i in 0..2400u64 {
        db.update(|root| {
            root.insert(i % 300, text(i, 50));
        })
        .unwrap();
    }
    let side = db.fork("side").unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = |db: Db<Root>, lane: u64| {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut made = Vec::new();
            let mut i = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let (key, value) = (lane * 1000 + i % 300, text(lane << 32 | i, 50));
                db.update(|root| {
                    root.insert(key, value.clone());
                })
                .unwrap();
                made.push((db.version(), key, value));
                i += 1;
            }
            made
        })
    };
    let writers = [writer(db.clone(), 1), writer(side.clone(), 2)];
    for _ in 0..8 {
        db.compact(Keep::Last(2300)).unwrap();
        side.compact(Keep::Last(2300)).unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    let made: Vec<_> = writers.into_iter().map(|w| w.join().unwrap()).collect();
    db.verify().unwrap();
    for (branch, made) in [&db, &side].into_iter().zip(&made) {
        let oldest = branch.version().saturating_sub(2000);
        for (version, key, value) in made.iter().filter(|m| m.0 > oldest) {
            let snapshot = branch.at(*version).unwrap();
            assert_eq!(&*snapshot.get(key).unwrap(), value, "v{version}");
        }
    }
}
