mod common;

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
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
const EOPNOTSUPP: c_int = 95;
const SPLIT: usize = 200;

const MANUAL: Reclaim = Reclaim {
    every_bytes: 0,
    fill: 2.0,
};

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn __errno_location() -> *mut c_int;
}

static PUNCH_ERROR: AtomicI32 = AtomicI32::new(0);
static SYNCS_UNTIL_FAILURE: AtomicI64 = AtomicI64::new(-1);
static FAILED_SYNC: AtomicBool = AtomicBool::new(false);
static SERIAL: Mutex<()> = Mutex::new(());

thread_local! {
    static WRITES_FAIL: Cell<bool> = const { Cell::new(false) };
}

#[unsafe(no_mangle)]
pub extern "C" fn fallocate(fd: c_int, mode: c_int, offset: i64, len: i64) -> c_int {
    let error = PUNCH_ERROR.load(Ordering::SeqCst);
    if error != 0 {
        unsafe { *__errno_location() = error };
        return -1;
    }
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fallocate".as_ptr()) };
    let real: extern "C" fn(c_int, c_int, i64, i64) -> c_int = unsafe { std::mem::transmute(symbol) };
    real(fd, mode, offset, len)
}

#[unsafe(no_mangle)]
pub extern "C" fn fdatasync(fd: c_int) -> c_int {
    let left = SYNCS_UNTIL_FAILURE.load(Ordering::SeqCst);
    if left >= 0 {
        SYNCS_UNTIL_FAILURE.store(left - 1, Ordering::SeqCst);
        if left == 0 {
            FAILED_SYNC.store(true, Ordering::SeqCst);
            unsafe { *__errno_location() = EIO };
            return -1;
        }
    }
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fdatasync".as_ptr()) };
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

fn serial() -> MutexGuard<'static, ()> {
    let turn = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    PUNCH_ERROR.store(0, Ordering::SeqCst);
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

#[test]
fn a_filesystem_that_cannot_punch_frees_nothing_and_loses_nothing() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let db: Db<Root> = options().open(&path).unwrap();
    fill(&db, 0, 1500);
    let want = dump(&db.read());
    PUNCH_ERROR.store(EOPNOTSUPP, Ordering::SeqCst);
    let report = db.compact(Keep::Last(1)).unwrap();
    assert_eq!(report.punched_bytes, 0);
    assert!(report.moved_bytes > 0);
    assert!(report.after_bytes + (1 << 20) >= report.before_bytes, "{report:?}");
    assert_eq!(dump(&db.read()), want);
    db.verify().unwrap();
    fill(&db, 2000, 10);
    let want = dump(&db.read());
    db.compact(Keep::Last(1)).unwrap();
    drop(db);
    PUNCH_ERROR.store(0, Ordering::SeqCst);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(dump(&db.read()), want);
    let report = db.compact(Keep::Last(1)).unwrap();
    assert!(report.punched_bytes > 0);
    assert!(report.after_bytes * 4 < report.before_bytes, "{report:?}");
    assert_eq!(dump(&db.read()), want);
    db.verify().unwrap();
}

#[test]
fn an_io_error_while_punching_fails_the_cycle_and_the_next_one_catches_up() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let db: Db<Root> = options().open(&path).unwrap();
    fill(&db, 0, 1500);
    let want = dump(&db.read());
    PUNCH_ERROR.store(EIO, Ordering::SeqCst);
    assert!(matches!(db.compact(Keep::Last(1)), Err(Error::Io(_))));
    assert!(db.space().unwrap().last_error.is_some());
    assert_eq!(dump(&db.read()), want);
    PUNCH_ERROR.store(0, Ordering::SeqCst);
    fill(&db, 3000, 1);
    let want = dump(&db.read());
    let report = db.compact(Keep::Last(1)).unwrap();
    assert!(report.after_bytes * 4 < report.before_bytes, "{report:?}");
    assert!(db.space().unwrap().last_error.is_none());
    assert_eq!(dump(&db.read()), want);
    db.verify().unwrap();
}

#[test]
fn running_out_of_space_during_the_copy_fails_the_cycle_and_commits_continue() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let hook = |name: &'static str| {
        if name == "marked" {
            WRITES_FAIL.with(|fail| fail.set(true));
        }
    };
    let db: Db<Root> = options().hook(Hook(Arc::new(hook))).open(&path).unwrap();
    fill(&db, 0, 1500);
    let outcome = db.compact(Keep::Last(1));
    WRITES_FAIL.with(|fail| fail.set(false));
    assert!(matches!(outcome, Err(Error::Io(_))), "{outcome:?}");
    fill(&db, 4000, 20);
    let want = dump(&db.read());
    db.verify().unwrap();
    drop(db);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(dump(&db.read()), want);
    db.compact(Keep::Last(1)).unwrap();
    assert_eq!(dump(&db.read()), want);
    db.verify().unwrap();
}

#[test]
fn a_failed_sync_at_any_point_in_a_cycle_poisons_and_loses_nothing() {
    let _turn = serial();
    for failing in 0..12 {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let hook = move |name: &'static str| {
            if name == "marked" {
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
        if FAILED_SYNC.load(Ordering::SeqCst) {
            assert!(outcome.is_err(), "sync {failing}: {outcome:?}");
            assert!(matches!(db.update(|root| {
                root.insert(u64::MAX, "late".into());
            }), Err(Error::Poisoned)), "sync {failing}");
        } else {
            outcome.unwrap();
        }
        assert_eq!(dump(&db.read()), want, "sync {failing}");
        drop(db);
        let db: Db<Root> = options().open(&path).unwrap();
        assert!(db.version() >= acked, "sync {failing}");
        assert_eq!(dump(&db.at(acked).unwrap()), want, "sync {failing}");
        db.verify().unwrap();
        db.update(|root| {
            root.insert(u64::MAX, "after".into());
        })
        .unwrap();
        db.compact(Keep::Last(1)).unwrap();
        db.verify().unwrap();
    }
}
