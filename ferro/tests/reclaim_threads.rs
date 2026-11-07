mod common;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use common::{Rng, text};
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
        assert!(copies.windows(2).all(|pair| pair[0] == pair[1]), "{keep:?}");
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
