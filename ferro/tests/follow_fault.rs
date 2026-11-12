#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ferro::db::Hook;
use ferro::{Db, Durability, Error, Keep, Map, Options, Reclaim};

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const EIO: c_int = 5;
const PATIENCE: Duration = Duration::from_secs(30);
const SOON: Duration = Duration::from_secs(10);
const F_OFD_SETLK: c_int = 37;
const F_UNLCK: i16 = 2;
const LANES_AT: i64 = (1 << 62) + (1 << 20);

const PASS: u8 = 0;
const FAIL: u8 = 1;
const HOLD: u8 = 2;

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn __errno_location() -> *mut c_int;
}

static ARMED: AtomicU8 = AtomicU8::new(PASS);
static ENTERED: AtomicBool = AtomicBool::new(false);
static RELEASED: AtomicBool = AtomicBool::new(false);
static SERIAL: Mutex<()> = Mutex::new(());
static LANE_LET_GO: AtomicBool = AtomicBool::new(false);
static LANE_RESUMED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static HOLD_LANE: Cell<bool> = const { Cell::new(false) };
}

#[repr(C)]
struct Flock {
    l_type: i16,
    l_whence: i16,
    l_start: i64,
    l_len: i64,
    l_pid: i32,
}

#[unsafe(no_mangle)]
pub extern "C" fn fdatasync(fd: c_int) -> c_int {
    match ARMED.swap(PASS, Ordering::SeqCst) {
        FAIL => {
            // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's
            // `errno`, valid for writes for as long as the thread runs.
            unsafe { *__errno_location() = EIO };
            return -1;
        }
        HOLD => {
            ENTERED.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + PATIENCE;
            while !RELEASED.load(Ordering::SeqCst) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
        }
        _ => {}
    }
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"fdatasync"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fdatasync".as_ptr()) };
    // SAFETY: glibc defines `fdatasync`, so `symbol` is the non-null address of that function, and
    // its C prototype `int fdatasync(int)` matches this function pointer type.
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    real(fd)
}

/// Passes every call to glibc's `fcntl`, and on a thread that set `HOLD_LANE`, once it has let
/// go of a branch lock, waits there until `LANE_RESUMED` is set.
///
/// # Safety
///
/// The caller keeps `fcntl`'s contract: `arg` is the argument `command` takes, a pointer to a
/// `struct flock` for `F_OFD_SETLK`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fcntl(fd: c_int, command: c_int, arg: *mut c_void) -> c_int {
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"fcntl"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fcntl".as_ptr()) };
    // SAFETY: glibc defines `fcntl`, so `symbol` is the non-null address of that function, and its
    // C prototype `int fcntl(int, int, ...)` matches this function pointer type.
    let real: unsafe extern "C" fn(c_int, c_int, ...) -> c_int =
        unsafe { std::mem::transmute(symbol) };
    // SAFETY: the arguments are the caller's, passed on unchanged under the same contract.
    let result = unsafe { real(fd, command, arg) };
    if result == 0 && command == F_OFD_SETLK && HOLD_LANE.with(Cell::get) {
        // SAFETY: for `F_OFD_SETLK` the caller passes a pointer to a live `struct flock`, whose
        // layout on 64-bit Linux `Flock` repeats.
        let lock = unsafe { &*(arg as *const Flock) };
        if lock.l_type == F_UNLCK && lock.l_start >= LANES_AT && lock.l_len == 1 {
            HOLD_LANE.with(|hold| hold.set(false));
            LANE_LET_GO.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + PATIENCE;
            while !LANE_RESUMED.load(Ordering::SeqCst) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    result
}

fn shared() -> Options {
    Options::new().cache_bytes(1 << 20).shared(true)
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + SOON;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn counting(step: &'static str) -> (Hook, Arc<AtomicU64>) {
    let count = Arc::new(AtomicU64::new(0));
    let counted = count.clone();
    let hook = Hook(Arc::new(move |name| {
        if name == step {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    }));
    (hook, count)
}

#[test]
fn a_poisoned_following_shared_writer_follows_nothing_more_and_keeps_its_error() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("poisoned.ferro");
    let (hook, adopted) = counting("adopted");
    let a: Db<String> = shared().follow(true).hook(hook).open(&path).unwrap();
    let b: Db<String> = shared().open(&path).unwrap();
    a.update(|text| *text = "first".into()).unwrap();
    until("a followed nothing yet", || a.version() == 1);
    ARMED.store(FAIL, Ordering::SeqCst);
    assert!(a.update(|text| *text = "second".into()).is_err());
    ARMED.store(PASS, Ordering::SeqCst);
    assert!(matches!(
        a.update(|text| *text = "third".into()),
        Err(Error::Poisoned)
    ));
    let poisoned = Error::Poisoned.to_string();
    until("the follower reported the poisoned store", || {
        a.following().unwrap().last_error.as_deref() == Some(poisoned.as_str())
    });
    let looks = a.following().unwrap().looks;
    let before = adopted.load(Ordering::SeqCst);
    for i in 0..100 {
        let text = format!("{i:>65535}");
        b.update(move |root| *root = text).unwrap();
        assert!(matches!(a.refresh(), Err(Error::Poisoned)));
    }
    thread::sleep(Duration::from_millis(1500));
    let following = a.following().unwrap();
    assert_eq!(following.last_error.as_deref(), Some(poisoned.as_str()));
    assert_eq!(following.looks, looks, "the follower kept looking");
    assert_eq!(
        adopted.load(Ordering::SeqCst),
        before,
        "a poisoned writer adopted and queued another handle's frames"
    );
    assert_eq!(a.version(), 1);
    assert!(b.version() >= 101);
}

#[test]
fn dropping_a_shared_follower_waits_for_no_other_handles_superblock_lock() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ENTERED.store(false, Ordering::SeqCst);
    RELEASED.store(false, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drop.ferro");
    let a: Db<u64> = shared()
        .follow(true)
        .follow_polling(Duration::from_secs(1))
        .open(&path)
        .unwrap();
    let b: Db<u64> = shared().open(&path).unwrap();
    let c: Db<u64> = shared().open(&path).unwrap();
    until("the follower looked", || a.following().unwrap().looks > 0);
    c.update(|n| *n = 1).unwrap();
    ARMED.store(HOLD, Ordering::SeqCst);
    let held = thread::spawn(move || {
        b.update(|n| *n = 2).unwrap();
        b
    });
    until("b holds the superblock lock in its sync", || {
        ENTERED.load(Ordering::SeqCst)
    });
    let caught = Instant::now() + SOON;
    while a.version() < 1 && Instant::now() < caught {
        thread::sleep(Duration::from_millis(1));
    }
    let version = a.version();
    let started = Instant::now();
    let dropping = thread::spawn(move || drop(a));
    while !dropping.is_finished() && started.elapsed() < Duration::from_secs(3) {
        thread::sleep(Duration::from_millis(1));
    }
    let dropped = dropping.is_finished();
    RELEASED.store(true, Ordering::SeqCst);
    dropping.join().unwrap();
    drop((held.join().unwrap(), c));
    assert_eq!(
        version, 1,
        "the follower waited for the superblock lock to publish what a superblock named"
    );
    assert!(
        dropped,
        "dropping the follower waited for another handle's sync"
    );
}

type Nested = Map<u64, Map<u64, String>>;

#[test]
fn a_file_a_cycle_retires_while_a_writer_caches_what_it_wrote_closes_once_nothing_reads_it() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    LANE_LET_GO.store(false, Ordering::SeqCst);
    LANE_RESUMED.store(false, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retired.ferro");
    let open = || {
        Options::new()
            .durability(Durability::Buffered)
            .cache_bytes(64 << 20)
            .reclaim(Reclaim { every_bytes: 0 })
            .shared(true)
            .open::<Nested>(&path)
            .unwrap()
    };
    let (a, b) = (open(), open());
    for outer in 0..20u64 {
        a.update(|root| {
            let mut inner = Map::new();
            for key in 0..50u64 {
                inner.insert(key, format!("{outer}.{key}"));
            }
            root.insert(outer, inner);
        })
        .unwrap();
    }
    let writer = a.clone();
    let writing = thread::spawn(move || {
        HOLD_LANE.with(|hold| hold.set(true));
        writer
            .update(|root| {
                for outer in 0..20u64 {
                    let mut inner = root
                        .get(&outer)
                        .map(|inner| (*inner).clone())
                        .unwrap_or_default();
                    inner.insert(1000, "late".to_string());
                    root.insert(outer, inner);
                }
            })
            .unwrap();
        HOLD_LANE.with(|hold| hold.set(false));
    });
    until("the writer let its branch lock go", || {
        LANE_LET_GO.load(Ordering::SeqCst)
    });
    b.compact(Keep::Last(1)).unwrap();
    assert!(a.refresh().unwrap(), "a did not follow the cycle");
    LANE_RESUMED.store(true, Ordering::SeqCst);
    writing.join().unwrap();
    let snapshot = a.read();
    let late = snapshot
        .get(&7)
        .and_then(|inner| inner.get(&1000).map(|value| (*value).clone()));
    assert_eq!(late.as_deref(), Some("late"));
    drop(snapshot);
    let deadline = Instant::now() + SOON;
    let mut key = 100u64;
    let mut retired = a.space().unwrap().retired_bytes;
    while retired > 0 && Instant::now() < deadline {
        key += 1;
        a.update(|root| {
            root.insert(key, Map::new());
        })
        .unwrap();
        thread::sleep(Duration::from_millis(10));
        retired = a.space().unwrap().retired_bytes;
    }
    assert_eq!(
        retired, 0,
        "nothing reads the file the cycle retired, yet the writer's cache keeps it open"
    );
}
