#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ferro::db::Hook;
use ferro::{Db, Error, Options};

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const EIO: c_int = 5;
const PATIENCE: Duration = Duration::from_secs(30);
const SOON: Duration = Duration::from_secs(10);

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
