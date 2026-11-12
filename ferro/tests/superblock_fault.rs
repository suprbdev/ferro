#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ferro::{Db, Durability, Map, Options};

type Root = Map<u64, u64>;

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const SLOT_BYTES: i64 = 4096;
const PATIENCE: Duration = Duration::from_secs(30);
const EITHER_SLOT: i64 = -2;

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

static SERIAL: Mutex<()> = Mutex::new(());
static STALLED: AtomicBool = AtomicBool::new(false);
static RESUMED: AtomicBool = AtomicBool::new(false);
static TEAR_AT: AtomicI64 = AtomicI64::new(-1);
static TORN: AtomicBool = AtomicBool::new(false);
static MENDED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static STALL_AFTER_SLOT_0: Cell<bool> = const { Cell::new(false) };
}

fn wait_for(flag: &AtomicBool) {
    let deadline = Instant::now() + PATIENCE;
    while !flag.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
}

/// Passes every call to glibc's `pread64`, and on a thread that set `STALL_AFTER_SLOT_0`, once
/// it has read from the start of the file, waits there until `RESUMED` is set, as a reader
/// preempted between the two superblock slots does.
#[unsafe(no_mangle)]
pub extern "C" fn pread64(fd: c_int, buf: *mut c_void, count: usize, offset: i64) -> isize {
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"pread64"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"pread64".as_ptr()) };
    // SAFETY: glibc defines `pread64`, so `symbol` is the non-null address of that function, and
    // its C prototype `ssize_t pread64(int, void *, size_t, off64_t)` matches this function pointer
    // type.
    let real: extern "C" fn(c_int, *mut c_void, usize, i64) -> isize =
        unsafe { std::mem::transmute(symbol) };
    let read = real(fd, buf, count, offset);
    if offset == 0 && STALL_AFTER_SLOT_0.with(|stall| stall.replace(false)) {
        STALLED.store(true, Ordering::SeqCst);
        wait_for(&RESUMED);
    }
    read
}

/// Passes every call to glibc's `pwrite64`, except that a superblock written to the slot at
/// `TEAR_AT`, or to either slot for `EITHER_SLOT`, is written half first, and the rest only once
/// `MENDED` is set, as a writer preempted in the middle of copying a superblock leaves it.
#[unsafe(no_mangle)]
pub extern "C" fn pwrite64(fd: c_int, buf: *const c_void, count: usize, offset: i64) -> isize {
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"pwrite64"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"pwrite64".as_ptr()) };
    // SAFETY: glibc defines `pwrite64`, so `symbol` is the non-null address of that function, and
    // its C prototype `ssize_t pwrite64(int, const void *, size_t, off64_t)` matches this function
    // pointer type.
    let real: extern "C" fn(c_int, *const c_void, usize, i64) -> isize =
        unsafe { std::mem::transmute(symbol) };
    let slot = (count as i64) < SLOT_BYTES && (offset == 0 || offset == SLOT_BYTES);
    let tearing = slot
        && [offset, EITHER_SLOT].into_iter().any(|at| {
            TEAR_AT
                .compare_exchange(at, -1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        });
    if !tearing {
        return real(fd, buf, count, offset);
    }
    let half = count / 2;
    if real(fd, buf, half, offset) != half as isize {
        return -1;
    }
    TORN.store(true, Ordering::SeqCst);
    wait_for(&MENDED);
    // SAFETY: the caller passes in `buf` a pointer valid for reads of `count` bytes, and `half` is
    // less than `count`.
    let rest = unsafe { buf.cast::<u8>().add(half) }.cast::<c_void>();
    match real(fd, rest, count - half, offset + half as i64) {
        written if written == (count - half) as isize => count as isize,
        _ => -1,
    }
}

fn reset() {
    for flag in [&STALLED, &RESUMED, &TORN, &MENDED] {
        flag.store(false, Ordering::SeqCst);
    }
    TEAR_AT.store(-1, Ordering::SeqCst);
}

fn newest_slot(path: &Path) -> i64 {
    let file = File::open(path).unwrap();
    let seq = |slot: i64| {
        let mut bytes = [0u8; 8];
        file.read_exact_at(&mut bytes, (slot * SLOT_BYTES + 8) as u64)
            .unwrap();
        u64::from_le_bytes(bytes)
    };
    i64::from(seq(1) > seq(0))
}

fn put(db: &Db<Root>, n: u64) {
    db.update(|root| {
        root.insert(n, n);
    })
    .unwrap();
}

fn put_until_newest_in(db: &Db<Root>, path: &Path, slot: i64, next: &mut u64) {
    while newest_slot(path) != slot {
        put(db, *next);
        *next += 1;
    }
}

fn reader_options() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20)
        .read_only(true)
}

fn tear_beside(db: &Db<Root>, next: &mut u64, read: impl FnOnce() -> u64 + Send) -> u64 {
    thread::scope(|scope| {
        let reader = scope.spawn(|| {
            STALL_AFTER_SLOT_0.with(|stall| stall.set(true));
            read()
        });
        wait_for(&STALLED);
        assert!(STALLED.load(Ordering::SeqCst));
        put(db, *next);
        *next += 1;
        TEAR_AT.store(SLOT_BYTES, Ordering::SeqCst);
        let n = *next;
        *next += 1;
        let committer = scope.spawn(move || put(db, n));
        wait_for(&TORN);
        assert!(TORN.load(Ordering::SeqCst));
        RESUMED.store(true, Ordering::SeqCst);
        let seen = reader.join();
        MENDED.store(true, Ordering::SeqCst);
        committer.join().unwrap();
        seen.unwrap()
    })
}

#[test]
fn a_read_only_open_sees_every_commit_acknowledged_beside_a_torn_superblock() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    reset();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("o.ferro");
    let db = Options::new()
        .cache_bytes(1 << 20)
        .open::<Root>(&path)
        .unwrap();
    let mut next = 0;
    put(&db, next);
    next += 1;
    put_until_newest_in(&db, &path, 1, &mut next);
    let acknowledged = db.read().version();
    let seen = tear_beside(&db, &mut next, || {
        reader_options().open::<Root>(&path).unwrap().version()
    });
    assert!(
        seen >= acknowledged,
        "a read-only open saw version {seen} after version {acknowledged} was acknowledged"
    );
    let reopened = reader_options().open::<Root>(&path).unwrap();
    assert_eq!(reopened.version(), db.read().version());
}

#[test]
fn a_refresh_sees_every_commit_acknowledged_beside_a_torn_superblock() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    reset();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("r.ferro");
    let db = Options::new()
        .cache_bytes(1 << 20)
        .open::<Root>(&path)
        .unwrap();
    let mut next = 0;
    put(&db, next);
    next += 1;
    put_until_newest_in(&db, &path, 0, &mut next);
    let reader = reader_options().open::<Root>(&path).unwrap();
    assert_eq!(reader.version(), db.read().version());
    put(&db, next);
    next += 1;
    assert_eq!(newest_slot(&path), 1);
    let acknowledged = db.read().version();
    let seen = tear_beside(&db, &mut next, || {
        reader.refresh().unwrap();
        reader.version()
    });
    assert!(
        seen >= acknowledged,
        "a refresh saw version {seen} after version {acknowledged} was acknowledged"
    );
    reader.refresh().unwrap();
    assert_eq!(reader.version(), db.read().version());
}

#[test]
fn a_read_only_open_beside_a_closing_writers_torn_superblock_sees_its_last_commit() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    reset();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("c.ferro");
    let db = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20)
        .open::<Root>(&path)
        .unwrap();
    for n in 0..4 {
        put(&db, n);
    }
    let acknowledged = db.read().version();
    TEAR_AT.store(EITHER_SLOT, Ordering::SeqCst);
    let closer = thread::spawn(move || drop(db));
    wait_for(&TORN);
    assert!(TORN.load(Ordering::SeqCst));
    let seen = reader_options().open::<Root>(&path).map(|db| db.version());
    MENDED.store(true, Ordering::SeqCst);
    closer.join().unwrap();
    let seen = seen.unwrap();
    assert!(
        seen >= acknowledged,
        "a read-only open saw version {seen} after version {acknowledged} was acknowledged"
    );
    assert_eq!(
        reader_options().open::<Root>(&path).unwrap().version(),
        acknowledged
    );
}
