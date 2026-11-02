mod common;

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use common::text;
use ferro::db::Hook;
use ferro::{Db, Durability, Error, Keep, Map, Options, Reclaim};
use tempfile::TempDir;

type Root = Map<u64, String>;

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const EIO: c_int = 5;
const ENOSPC: c_int = 28;
const SPLIT: usize = 200;
const MANUAL: Reclaim = Reclaim { every_bytes: 0 };

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn __errno_location() -> *mut c_int;
}

static RENAME_ERROR: AtomicI32 = AtomicI32::new(0);
static SYNCS_UNTIL_FAILURE: AtomicI64 = AtomicI64::new(-1);
static FAILED_SYNC: AtomicBool = AtomicBool::new(false);
static SERIAL: Mutex<()> = Mutex::new(());

thread_local! {
    static WRITES_FAIL: Cell<bool> = const { Cell::new(false) };
}

fn fails_now() -> bool {
    let left = SYNCS_UNTIL_FAILURE.load(Ordering::SeqCst);
    if left >= 0 {
        SYNCS_UNTIL_FAILURE.store(left - 1, Ordering::SeqCst);
        if left == 0 {
            FAILED_SYNC.store(true, Ordering::SeqCst);
            unsafe { *__errno_location() = EIO };
            return true;
        }
    }
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn fdatasync(fd: c_int) -> c_int {
    if fails_now() {
        return -1;
    }
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fdatasync".as_ptr()) };
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    real(fd)
}

#[unsafe(no_mangle)]
pub extern "C" fn fsync(fd: c_int) -> c_int {
    if fails_now() {
        return -1;
    }
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fsync".as_ptr()) };
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    real(fd)
}

#[unsafe(no_mangle)]
pub extern "C" fn pwrite64(fd: c_int, buf: *const c_void, count: usize, offset: i64) -> isize {
    if count > SPLIT && WRITES_FAIL.with(Cell::get) {
        unsafe { *__errno_location() = ENOSPC };
        return -1;
    }
    let symbol = unsafe { dlsym(RTLD_NEXT, c"pwrite64".as_ptr()) };
    let real: extern "C" fn(c_int, *const c_void, usize, i64) -> isize =
        unsafe { std::mem::transmute(symbol) };
    real(fd, buf, count, offset)
}

#[unsafe(no_mangle)]
pub extern "C" fn rename(from: *const c_char, to: *const c_char) -> c_int {
    let error = RENAME_ERROR.load(Ordering::SeqCst);
    if error != 0 {
        unsafe { *__errno_location() = error };
        return -1;
    }
    let symbol = unsafe { dlsym(RTLD_NEXT, c"rename".as_ptr()) };
    let real: extern "C" fn(*const c_char, *const c_char) -> c_int =
        unsafe { std::mem::transmute(symbol) };
    real(from, to)
}

fn serial() -> MutexGuard<'static, ()> {
    let turn = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    RENAME_ERROR.store(0, Ordering::SeqCst);
    SYNCS_UNTIL_FAILURE.store(-1, Ordering::SeqCst);
    FAILED_SYNC.store(false, Ordering::SeqCst);
    turn
}

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn options() -> Options {
    Options::new().durability(Durability::Buffered).cache_bytes(1).reclaim(MANUAL)
}

fn fill(db: &Db<Root>, start: u64, commits: u64) {
    for i in start..start + commits {
        db.update(|root| {
            root.insert(i % 50, text(i, 60));
        })
        .unwrap();
    }
}

fn dump(root: &Root) -> Vec<(u64, String)> {
    root.iter().map(|(k, v)| (k, (*v).clone())).collect()
}

fn no_temp(path: &Path) -> bool {
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".compacting");
    !Path::new(&temp).exists()
}

#[test]
fn running_out_of_space_during_the_copy_fails_the_cycle_and_commits_continue() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let hook = |name: &'static str| {
        if name == "created" {
            WRITES_FAIL.with(|fail| fail.set(true));
        }
    };
    let db: Db<Root> = options().hook(Hook(Arc::new(hook))).open(&path).unwrap();
    fill(&db, 0, 1500);
    let size = db.size_on_disk();
    let outcome = db.compact(Keep::Last(1));
    WRITES_FAIL.with(|fail| fail.set(false));
    assert!(matches!(outcome, Err(Error::Io(_))), "{outcome:?}");
    assert!(no_temp(&path));
    assert!(db.space().unwrap().last_error.is_some());
    assert!(db.size_on_disk() >= size);
    fill(&db, 4000, 20);
    let want = dump(&db.read());
    db.verify().unwrap();
    drop(db);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(dump(&db.read()), want);
    let report = db.compact(Keep::Last(1)).unwrap();
    assert!(report.after_bytes * 4 < report.before_bytes, "{report:?}");
    assert!(db.space().unwrap().last_error.is_none());
    assert_eq!(dump(&db.read()), want);
    db.verify().unwrap();
}

#[test]
fn a_failed_rename_fails_the_cycle_and_commits_continue() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let db: Db<Root> = options().open(&path).unwrap();
    fill(&db, 0, 800);
    let size = db.size_on_disk();
    RENAME_ERROR.store(EIO, Ordering::SeqCst);
    let outcome = db.compact(Keep::Last(1));
    RENAME_ERROR.store(0, Ordering::SeqCst);
    assert!(matches!(outcome, Err(Error::Io(_))), "{outcome:?}");
    assert!(no_temp(&path));
    assert!(db.size_on_disk() >= size);
    fill(&db, 5000, 20);
    let want = dump(&db.read());
    db.verify().unwrap();
    let report = db.compact(Keep::Last(1)).unwrap();
    assert!(report.after_bytes * 4 < report.before_bytes, "{report:?}");
    assert_eq!(dump(&db.read()), want);
    drop(db);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(dump(&db.read()), want);
    db.verify().unwrap();
}

#[test]
fn a_failed_sync_at_any_point_in_a_cycle_loses_nothing() {
    let _turn = serial();
    let mut failed = 0;
    for failing in 0..12 {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let hook = move |name: &'static str| {
            if name == "created" {
                SYNCS_UNTIL_FAILURE.store(failing, Ordering::SeqCst);
            }
        };
        let db: Db<Root> = Options::new()
            .cache_bytes(1)
            .reclaim(MANUAL)
            .hook(Hook(Arc::new(hook)))
            .open(&path)
            .unwrap();
        fill(&db, 0, 120);
        let acked = db.version();
        let want = dump(&db.read());
        FAILED_SYNC.store(false, Ordering::SeqCst);
        let outcome = db.compact(Keep::Last(2));
        SYNCS_UNTIL_FAILURE.store(-1, Ordering::SeqCst);
        let mut model = want.clone();
        let mut last = acked;
        if FAILED_SYNC.load(Ordering::SeqCst) {
            failed += 1;
            assert!(outcome.is_err(), "sync {failing}: {outcome:?}");
            assert!(no_temp(&path), "sync {failing}");
            let late = db.update(|root| {
                root.insert(u64::MAX, "late".into());
            });
            match late {
                Ok(()) => {
                    model = dump(&db.read());
                    last = db.version();
                }
                Err(Error::Poisoned) => {}
                Err(other) => panic!("sync {failing}: {other}"),
            }
        } else {
            outcome.unwrap();
        }
        drop(db);
        let db: Db<Root> = options().open(&path).unwrap();
        assert!(db.version() >= last, "sync {failing}: {} below {last}", db.version());
        assert_eq!(dump(&db.at(last).unwrap()), model, "sync {failing}");
        db.verify().unwrap();
        db.update(|root| {
            root.insert(u64::MAX - 1, "after".into());
        })
        .unwrap();
        db.compact(Keep::Last(1)).unwrap();
        db.verify().unwrap();
    }
    assert!(failed >= 4, "only {failed} failed");
}
