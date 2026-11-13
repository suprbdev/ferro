#![cfg(unix)]

mod common;

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use common::text;
use ferro::db::Hook;
use ferro::{Db, Durability, Error, Keep, Map, Options, Reclaim};
use tempfile::TempDir;

type Root = Map<u64, String>;

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const EIO: c_int = 5;
const ENOSPC: c_int = 28;
const SPLIT: usize = 200;
const MANUAL: Reclaim = Reclaim::off();

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn __errno_location() -> *mut c_int;
}

static RENAME_ERROR: AtomicI32 = AtomicI32::new(0);
static CLOCK_BACK_SECONDS: AtomicI64 = AtomicI64::new(0);
static SYNCS_UNTIL_FAILURE: AtomicI64 = AtomicI64::new(-1);
static FAILED_SYNC: AtomicBool = AtomicBool::new(false);
static SYNC_ENTERED: AtomicBool = AtomicBool::new(false);
static SYNC_RELEASED: AtomicBool = AtomicBool::new(false);
static SERIAL: Mutex<()> = Mutex::new(());

static COPY_SLOW: AtomicBool = AtomicBool::new(false);
static COPY_WRITES: AtomicI64 = AtomicI64::new(0);
static COPY_HELD: AtomicBool = AtomicBool::new(false);
static COPY_FAILS: AtomicBool = AtomicBool::new(false);

static READS_WATCHED: AtomicBool = AtomicBool::new(false);
static PAUSED: AtomicBool = AtomicBool::new(false);
static OLD_END: AtomicI64 = AtomicI64::new(0);
static OLD_READS_IN_PAUSE: AtomicI64 = AtomicI64::new(0);
static COPY_READS_FAIL_AT: Mutex<Vec<i64>> = Mutex::new(Vec::new());
static COPY_READS_FAILED: AtomicI64 = AtomicI64::new(0);

thread_local! {
    static WRITES_FAIL: Cell<bool> = const { Cell::new(false) };
    static SYNC_HELD: Cell<bool> = const { Cell::new(false) };
}

#[repr(C)]
struct Timespec {
    seconds: i64,
    nanos: i64,
}

#[unsafe(no_mangle)]
extern "C" fn clock_gettime(clock: c_int, out: *mut Timespec) -> c_int {
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"clock_gettime"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"clock_gettime".as_ptr()) };
    // SAFETY: glibc defines `clock_gettime`, so `symbol` is the non-null address of that function,
    // and its C prototype `int clock_gettime(clockid_t, struct timespec *)` matches this function
    // pointer type. `Timespec` is `repr(C)` with the two 64-bit fields of 64-bit Linux's
    // `timespec`.
    let real: extern "C" fn(c_int, *mut Timespec) -> c_int = unsafe { std::mem::transmute(symbol) };
    let code = real(clock, out);
    if clock == 0 {
        // SAFETY: a caller of `clock_gettime` passes in `out` a pointer valid for writes to a
        // `timespec`, which `Timespec` matches in layout. Clock 0 is `CLOCK_REALTIME`, which
        // always exists, so the real call has just filled it in.
        unsafe { (*out).seconds -= CLOCK_BACK_SECONDS.load(Ordering::SeqCst) };
    }
    code
}

fn fails_now() -> bool {
    let left = SYNCS_UNTIL_FAILURE.load(Ordering::SeqCst);
    if left >= 0 {
        SYNCS_UNTIL_FAILURE.store(left - 1, Ordering::SeqCst);
        if left == 0 {
            FAILED_SYNC.store(true, Ordering::SeqCst);
            // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's
            // `errno`, valid for writes for as long as the thread runs.
            unsafe { *__errno_location() = EIO };
            return true;
        }
    }
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn fdatasync(fd: c_int) -> c_int {
    if SYNC_HELD.with(Cell::get) {
        SYNC_ENTERED.store(true, Ordering::SeqCst);
        while !SYNC_RELEASED.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }
    if fails_now() {
        return -1;
    }
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"fdatasync"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fdatasync".as_ptr()) };
    // SAFETY: glibc defines `fdatasync`, so `symbol` is the non-null address of that function, and
    // its C prototype `int fdatasync(int)` matches this function pointer type.
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    real(fd)
}

#[unsafe(no_mangle)]
pub extern "C" fn fsync(fd: c_int) -> c_int {
    if fails_now() {
        return -1;
    }
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"fsync"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fsync".as_ptr()) };
    // SAFETY: glibc defines `fsync`, so `symbol` is the non-null address of that function, and its
    // C prototype `int fsync(int)` matches this function pointer type.
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    real(fd)
}

#[unsafe(no_mangle)]
pub extern "C" fn pwrite64(fd: c_int, buf: *const c_void, count: usize, offset: i64) -> isize {
    let copying = std::thread::current().name() == Some("ferro-copy");
    if count > SPLIT
        && (WRITES_FAIL.with(Cell::get) || copying && COPY_FAILS.load(Ordering::SeqCst))
    {
        // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's `errno`,
        // valid for writes for as long as the thread runs.
        unsafe { *__errno_location() = ENOSPC };
        return -1;
    }
    if count > SPLIT && COPY_SLOW.load(Ordering::SeqCst) {
        if copying {
            COPY_WRITES.fetch_add(1, Ordering::SeqCst);
            while COPY_HELD.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"pwrite64"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"pwrite64".as_ptr()) };
    // SAFETY: glibc defines `pwrite64`, so `symbol` is the non-null address of that function, and
    // its C prototype `ssize_t pwrite64(int, const void *, size_t, off64_t)` matches this function
    // pointer type.
    let real: extern "C" fn(c_int, *const c_void, usize, i64) -> isize =
        unsafe { std::mem::transmute(symbol) };
    real(fd, buf, count, offset)
}

#[unsafe(no_mangle)]
pub extern "C" fn pread64(fd: c_int, buf: *mut c_void, count: usize, offset: i64) -> isize {
    if READS_WATCHED.load(Ordering::SeqCst) && std::thread::current().name() == Some("ferro-copy") {
        if PAUSED.load(Ordering::SeqCst) && offset < OLD_END.load(Ordering::SeqCst) {
            OLD_READS_IN_PAUSE.fetch_add(1, Ordering::SeqCst);
        }
        let failing = COPY_READS_FAIL_AT
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(&offset);
        if failing {
            COPY_READS_FAILED.fetch_add(1, Ordering::SeqCst);
            // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's `errno`,
            // valid for writes for as long as the thread runs.
            unsafe { *__errno_location() = EIO };
            return -1;
        }
    }
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"pread64"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"pread64".as_ptr()) };
    // SAFETY: glibc defines `pread64`, so `symbol` is the non-null address of that function, and
    // its C prototype `ssize_t pread64(int, void *, size_t, off64_t)` matches this function pointer
    // type.
    let real: extern "C" fn(c_int, *mut c_void, usize, i64) -> isize =
        unsafe { std::mem::transmute(symbol) };
    real(fd, buf, count, offset)
}

#[unsafe(no_mangle)]
pub extern "C" fn rename(from: *const c_char, to: *const c_char) -> c_int {
    let error = RENAME_ERROR.load(Ordering::SeqCst);
    if error != 0 {
        // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's `errno`,
        // valid for writes for as long as the thread runs.
        unsafe { *__errno_location() = error };
        return -1;
    }
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"rename"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"rename".as_ptr()) };
    // SAFETY: glibc defines `rename`, so `symbol` is the non-null address of that function, and its
    // C prototype `int rename(const char *, const char *)` matches this function pointer type.
    let real: extern "C" fn(*const c_char, *const c_char) -> c_int =
        unsafe { std::mem::transmute(symbol) };
    real(from, to)
}

fn serial() -> MutexGuard<'static, ()> {
    let turn = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    RENAME_ERROR.store(0, Ordering::SeqCst);
    CLOCK_BACK_SECONDS.store(0, Ordering::SeqCst);
    SYNCS_UNTIL_FAILURE.store(-1, Ordering::SeqCst);
    FAILED_SYNC.store(false, Ordering::SeqCst);
    turn
}

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn options() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1)
        .reclaim(MANUAL)
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
            COPY_FAILS.store(true, Ordering::SeqCst);
        }
    };
    let db: Db<Root> = options().hook(Hook(Arc::new(hook))).open(&path).unwrap();
    fill(&db, 0, 1500);
    let size = db.log_bytes();
    let outcome = db.compact(Keep::Last(1));
    WRITES_FAIL.with(|fail| fail.set(false));
    COPY_FAILS.store(false, Ordering::SeqCst);
    assert!(matches!(outcome, Err(Error::Io(_))), "{outcome:?}");
    assert!(no_temp(&path));
    assert!(db.space().unwrap().last_error.is_some());
    assert!(db.log_bytes() >= size);
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
fn a_cycle_does_not_heal_a_store_poisoned_during_it() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let slot: Arc<Mutex<Option<Db<Root>>>> = Arc::new(Mutex::new(None));
    let hook = {
        let slot = slot.clone();
        move |name: &'static str| {
            if name == "synced" {
                let db = slot.lock().unwrap().clone().unwrap();
                WRITES_FAIL.with(|fail| fail.set(true));
                let failed = db.update(|root| {
                    root.insert(1, text(9, 400));
                });
                WRITES_FAIL.with(|fail| fail.set(false));
                assert!(matches!(failed, Err(Error::Io(_))), "{failed:?}");
            }
        }
    };
    let db: Db<Root> = options().hook(Hook(Arc::new(hook))).open(&path).unwrap();
    *slot.lock().unwrap() = Some(db.clone());
    fill(&db, 0, 50);
    let want = dump(&db.read());
    let outcome = db.compact(Keep::Last(1));
    *slot.lock().unwrap() = None;
    assert!(matches!(outcome, Err(Error::Poisoned)), "{outcome:?}");
    assert!(no_temp(&path));
    let late = db.update(|root| {
        root.insert(u64::MAX, "late".into());
    });
    assert!(matches!(late, Err(Error::Poisoned)), "{late:?}");
    drop(db);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(dump(&db.read()), want);
    db.verify().unwrap();
}

fn base_then_fork(db: &Db<Root>, name: &str) -> Db<Root> {
    for i in 0..3u64 {
        db.commit("base", |root| {
            root.insert(i, format!("v{i}"));
        })
        .unwrap();
    }
    let side = db.fork(name).unwrap();
    CLOCK_BACK_SECONDS.store(3600, Ordering::SeqCst);
    side.commit("sync", |root| {
        root.insert(9, "same".into());
    })
    .unwrap();
    side.commit("more", |root| {
        root.insert(10, "other".into());
    })
    .unwrap();
    side
}

#[test]
fn a_commit_with_the_hash_of_a_pruned_entry_during_a_cycle_is_copied() {
    let _turn = serial();
    for stage in ["created", "copied", "synced"] {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let slot: Arc<Mutex<Option<Db<Root>>>> = Arc::new(Mutex::new(None));
        let hook = {
            let slot = slot.clone();
            move |name: &'static str| {
                if name == stage {
                    let db = slot.lock().unwrap().clone().unwrap();
                    db.commit("sync", |root| {
                        root.insert(9, "same".into());
                    })
                    .unwrap();
                }
            }
        };
        let db: Db<Root> = options().hook(Hook(Arc::new(hook))).open(&path).unwrap();
        let side = base_then_fork(&db, "side");
        *slot.lock().unwrap() = Some(db.clone());
        db.compact(Keep::Last(1)).unwrap();
        *slot.lock().unwrap() = None;
        let want = dump(&db.read());
        assert_eq!(
            want.iter().find(|row| row.0 == 9).unwrap().1,
            "same",
            "{stage}"
        );
        db.update(|root| {
            root.insert(1, "after".into());
        })
        .unwrap();
        db.verify().unwrap();
        let want = dump(&db.read());
        drop((db, side));
        CLOCK_BACK_SECONDS.store(0, Ordering::SeqCst);
        let db: Db<Root> = options().open(&path).unwrap();
        assert_eq!(dump(&db.read()), want, "{stage}");
        db.verify().unwrap();
        ferro::admin::verify(&path).unwrap();
    }
}

#[test]
fn a_commit_with_the_hash_of_a_tombstone_does_not_stop_later_cycles() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let db: Db<Root> = options().open(&path).unwrap();
    let side = base_then_fork(&db, "a");
    db.compact(Keep::Last(1)).unwrap();
    db.commit("sync", |root| {
        root.insert(9, "same".into());
    })
    .unwrap();
    db.verify().unwrap();
    let want = dump(&db.read());
    for _ in 0..3 {
        db.compact(Keep::All).unwrap();
        assert_eq!(dump(&db.read()), want);
        db.verify().unwrap();
    }
    assert!(db.space().unwrap().last_error.is_none());
    assert_eq!(*side.read().get(&10).unwrap(), "other");
}

#[test]
fn a_swap_waits_for_commits_that_are_still_syncing() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let db: Db<Root> = Options::new()
        .cache_bytes(1)
        .reclaim(MANUAL)
        .open(&path)
        .unwrap();
    fill(&db, 0, 100);
    SYNC_ENTERED.store(false, Ordering::SeqCst);
    SYNC_RELEASED.store(false, Ordering::SeqCst);
    let leader = {
        let db = db.clone();
        std::thread::spawn(move || {
            SYNC_HELD.with(|held| held.set(true));
            db.update(|root| {
                root.insert(1000, "leader".into());
            })
            .unwrap();
            SYNC_HELD.with(|held| held.set(false));
        })
    };
    while !SYNC_ENTERED.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    let follower = {
        let db = db.clone();
        std::thread::spawn(move || {
            db.update(|root| {
                root.insert(1001, "follower".into());
            })
            .unwrap();
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    let release = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(200));
        SYNC_RELEASED.store(true, Ordering::SeqCst);
    });
    let report = db.compact(Keep::All).unwrap();
    release.join().unwrap();
    leader.join().unwrap();
    follower.join().unwrap();
    assert!(report.pause >= Duration::from_millis(100), "{report:?}");
    db.update(|root| {
        root.insert(1002, "after".into());
    })
    .unwrap();
    let want = dump(&db.read());
    assert!(want.iter().any(|row| row.0 == 1001));
    db.verify().unwrap();
    drop(db);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(dump(&db.read()), want);
    assert_eq!(db.version(), 103);
    db.verify().unwrap();
}

#[test]
fn a_failed_rename_fails_the_cycle_and_commits_continue() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let db: Db<Root> = options().open(&path).unwrap();
    fill(&db, 0, 800);
    let size = db.log_bytes();
    RENAME_ERROR.store(EIO, Ordering::SeqCst);
    let outcome = db.compact(Keep::Last(1));
    RENAME_ERROR.store(0, Ordering::SeqCst);
    assert!(matches!(outcome, Err(Error::Io(_))), "{outcome:?}");
    assert!(no_temp(&path));
    assert!(db.log_bytes() >= size);
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
    for (failing, cache) in (0..12).flat_map(|failing| [(failing, 1), (failing, 1 << 20)]) {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let hook = move |name: &'static str| {
            if name == "created" {
                SYNCS_UNTIL_FAILURE.store(failing, Ordering::SeqCst);
            }
        };
        let db: Db<Root> = Options::new()
            .cache_bytes(cache)
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
        assert!(
            db.version() >= last,
            "sync {failing}: {} below {last}",
            db.version()
        );
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

#[cfg(target_os = "linux")]
fn verifying_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .filter(|name| name.trim() == "ferro-copy")
        .count()
}

#[cfg(target_os = "linux")]
fn verifying_threads_after_join() -> usize {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let left = verifying_threads();
        if left == 0 || std::time::Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn shutting_down_while_threads_verify_the_copy_joins_them_and_leaves_no_temp_file() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".compacting");
    let temp = std::path::PathBuf::from(temp);
    let created = Arc::new(AtomicI64::new(0));
    COPY_WRITES.store(0, Ordering::SeqCst);
    let hook = {
        let created = created.clone();
        move |name: &'static str| {
            if name == "created" {
                created.fetch_add(1, Ordering::SeqCst);
                COPY_HELD.store(true, Ordering::SeqCst);
                COPY_SLOW.store(true, Ordering::SeqCst);
            }
        }
    };
    let db: Db<Root> = options()
        .copy_threads(4)
        .history(Keep::Last(1 << 20))
        .reclaim(Reclaim::every(8 << 20))
        .hook(Hook(Arc::new(hook)))
        .open(&path)
        .unwrap();
    let mut next = 0;
    while created.load(Ordering::SeqCst) == 0 {
        db.update(|root| {
            for _ in 0..4 {
                root.insert(next % 3000, text(next, 1500));
                next += 1;
            }
        })
        .unwrap();
    }
    let want = dump(&db.read());
    let began = std::time::Instant::now();
    while COPY_WRITES.load(Ordering::SeqCst) == 0 && began.elapsed() < Duration::from_secs(30) {
        std::thread::yield_now();
    }
    let wrote = COPY_WRITES.load(Ordering::SeqCst) > 0;
    let copying = temp.exists();
    #[cfg(target_os = "linux")]
    let threads = verifying_threads();
    COPY_HELD.store(false, Ordering::SeqCst);
    assert!(wrote, "no thread of the copy wrote in 30 s");
    assert!(copying, "the copy ended while a thread of it was held");
    #[cfg(target_os = "linux")]
    assert_eq!(threads, 3);
    drop(db);
    COPY_SLOW.store(false, Ordering::SeqCst);
    assert_eq!(created.load(Ordering::SeqCst), 1);
    assert!(no_temp(&path));
    #[cfg(target_os = "linux")]
    assert_eq!(verifying_threads_after_join(), 0);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(dump(&db.read()), want);
    db.verify().unwrap();
}

fn fill_one(db: &Db<Root>, start: u64, commits: u64) {
    for i in start..start + commits {
        db.update(|root| {
            root.insert(0, text(i, 60));
        })
        .unwrap();
    }
}

fn entry_offsets(path: &Path) -> Vec<i64> {
    let db: Db<Root> = options().read_only(true).open(path).unwrap();
    let roots: std::collections::HashSet<u64> = (1..=db.version())
        .map(|version| db.at(version).unwrap().commit().root.unwrap().offset)
        .collect();
    drop(db);
    let layout = ferro::admin::layout(path, Keep::All).unwrap();
    let bytes = std::fs::read(path).unwrap();
    layout
        .extents
        .iter()
        .filter(|(offset, _)| {
            let (links, _) = common::link_slots(&bytes, *offset as usize);
            links
                .first()
                .is_some_and(|link| roots.contains(&link.target))
        })
        .map(|(offset, _)| *offset as i64)
        .collect()
}

#[test]
fn the_pause_reads_no_chain_ahead_when_an_earlier_pass_walked_it() {
    let _turn = serial();
    for added in [1, 2100] {
        let dir = dir();
        let path = dir.path().join("db.ferro");
        let slot: Arc<Mutex<Option<Db<Root>>>> = Arc::new(Mutex::new(None));
        let hook = {
            let slot = slot.clone();
            move |name: &'static str| match name {
                "synced" => {
                    let db = slot.lock().unwrap().clone().unwrap();
                    OLD_END.store(db.log_bytes() as i64, Ordering::SeqCst);
                    fill_one(&db, 9000, added);
                }
                "paused" => PAUSED.store(true, Ordering::SeqCst),
                "swapped" => PAUSED.store(false, Ordering::SeqCst),
                _ => {}
            }
        };
        let db: Db<Root> = options()
            .copy_threads(4)
            .hook(Hook(Arc::new(hook)))
            .open(&path)
            .unwrap();
        *slot.lock().unwrap() = Some(db.clone());
        fill_one(&db, 0, 3000);
        OLD_READS_IN_PAUSE.store(0, Ordering::SeqCst);
        READS_WATCHED.store(true, Ordering::SeqCst);
        let report = db.compact(Keep::All);
        READS_WATCHED.store(false, Ordering::SeqCst);
        PAUSED.store(false, Ordering::SeqCst);
        *slot.lock().unwrap() = None;
        let report = report.unwrap();
        db.verify().unwrap();
        assert_eq!(db.version(), 3000 + added);
        let latest = text(9000 + added - 1, 60);
        assert_eq!(*db.read().get(&0).unwrap(), latest);
        assert_eq!(
            OLD_READS_IN_PAUSE.load(Ordering::SeqCst),
            0,
            "{added} added: copy threads read entries an earlier pass walked during the pause: \
             {report:?}"
        );
        drop(db);
        let db: Db<Root> = options().open(&path).unwrap();
        assert_eq!(*db.read().get(&0).unwrap(), latest);
        db.verify().unwrap();
    }
}

#[test]
fn reads_ahead_that_fail_leave_the_walk_to_read_the_entries_itself() {
    let _turn = serial();
    let dir = dir();
    let source = dir.path().join("source.ferro");
    {
        let db: Db<Root> = options().open(&source).unwrap();
        fill_one(&db, 0, 3000);
    }
    let entries = entry_offsets(&source);
    assert!(entries.len() >= 3000, "{}", entries.len());
    let copy = |name: &str, threads: usize, failing: Vec<i64>| -> Vec<u8> {
        let path = dir.path().join(name);
        std::fs::copy(&source, &path).unwrap();
        let db: Db<Root> = options().copy_threads(threads).open(&path).unwrap();
        let want = dump(&db.read());
        *COPY_READS_FAIL_AT.lock().unwrap() = failing;
        READS_WATCHED.store(true, Ordering::SeqCst);
        let report = db.compact(Keep::All);
        READS_WATCHED.store(false, Ordering::SeqCst);
        COPY_READS_FAIL_AT.lock().unwrap().clear();
        report.unwrap();
        assert_eq!(dump(&db.read()), want);
        db.verify().unwrap();
        drop(db);
        std::fs::read(&path).unwrap()
    };
    let one = copy("one.ferro", 1, Vec::new());
    for threads in [2, 4, 8] {
        for every in [1, 7, 500] {
            COPY_READS_FAILED.store(0, Ordering::SeqCst);
            let failing = entries.iter().step_by(every).copied().collect();
            let bytes = copy(&format!("{threads}-{every}.ferro"), threads, failing);
            assert!(
                COPY_READS_FAILED.load(Ordering::SeqCst) > 0,
                "{threads} {every}"
            );
            assert!(bytes == one, "{threads} threads failing every {every}");
        }
    }
}

#[cfg(target_os = "linux")]
fn copies_held_open(dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| {
            target.starts_with(dir) && target.to_string_lossy().contains(".compacting")
        })
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn a_failed_rename_leaves_no_copy_held_open() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("db.ferro");
    let db: Db<Root> = options().open(&path).unwrap();
    fill(&db, 0, 800);
    RENAME_ERROR.store(EIO, Ordering::SeqCst);
    let outcome = db.compact(Keep::Last(1));
    RENAME_ERROR.store(0, Ordering::SeqCst);
    assert!(matches!(outcome, Err(Error::Io(_))), "{outcome:?}");
    assert!(no_temp(&path));
    let held = copies_held_open(dir.path());
    assert!(
        held.is_empty(),
        "the abandoned copy is still open: {held:?}"
    );
}
