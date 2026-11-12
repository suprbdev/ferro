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
const FRAME_BYTES: u64 = 68;

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

static SERIAL: Mutex<()> = Mutex::new(());
static STALLED: AtomicBool = AtomicBool::new(false);
static RESUMED: AtomicBool = AtomicBool::new(false);
static TEAR_AT: AtomicI64 = AtomicI64::new(-1);
static TORN: AtomicBool = AtomicBool::new(false);
static MENDED: AtomicBool = AtomicBool::new(false);
static HOLD_FSYNC: AtomicBool = AtomicBool::new(false);
static FSYNCED: AtomicBool = AtomicBool::new(false);
static RECORDING: AtomicBool = AtomicBool::new(false);
static RECORDED: Mutex<Vec<Recorded>> = Mutex::new(Vec::new());

thread_local! {
    static STALL_AFTER_SLOT_0: Cell<bool> = const { Cell::new(false) };
    static STALL_AFTER_SLOT_1: Cell<bool> = const { Cell::new(false) };
}

/// A change to a file that the interposers saw while `RECORDING` was set, in the order made.
#[derive(Clone, Debug)]
enum Recorded {
    /// Bytes written at an offset, recorded before the write, since it may land in part.
    Write(i64, Vec<u8>),
    /// A new length, recorded before the file takes it.
    SetLen(i64),
    /// A sync that succeeded, after which every change before it is durable.
    Sync,
}

fn record(change: Recorded) {
    if RECORDING.load(Ordering::SeqCst) {
        RECORDED
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(change);
    }
}

fn wait_for(flag: &AtomicBool) {
    let deadline = Instant::now() + PATIENCE;
    while !flag.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
}

/// Passes every call to glibc's `pread64`, and on a thread that set `STALL_AFTER_SLOT_0`, once
/// it has read from the start of the file, or `STALL_AFTER_SLOT_1`, once it has read from the
/// second slot, waits there until `RESUMED` is set, as a reader preempted between the two
/// superblock slots, or after them, does.
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
    let stall = match offset {
        0 => STALL_AFTER_SLOT_0.with(|stall| stall.replace(false)),
        SLOT_BYTES => STALL_AFTER_SLOT_1.with(|stall| stall.replace(false)),
        _ => false,
    };
    if stall {
        STALLED.store(true, Ordering::SeqCst);
        wait_for(&RESUMED);
    }
    read
}

/// Passes every call to glibc's `pwrite64`, recording it while `RECORDING` is set, except that a
/// superblock written to the slot at `TEAR_AT`, or to either slot for `EITHER_SLOT`, is written
/// half first, and the rest only once `MENDED` is set, as a writer preempted in the middle of
/// copying a superblock leaves it.
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
    if RECORDING.load(Ordering::SeqCst) {
        // SAFETY: the caller passes in `buf` a pointer valid for reads of `count` bytes, which it
        // does not change during the call.
        let bytes = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), count) };
        record(Recorded::Write(offset, bytes.to_vec()));
    }
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

/// Passes every call to glibc's `fsync`, recording one that succeeds while `RECORDING` is set,
/// except that the first once `HOLD_FSYNC` is set, once it has synced, sets `FSYNCED` and waits
/// until `RESUMED` is set, as a writer held up in a sync leaves the file.
#[unsafe(no_mangle)]
pub extern "C" fn fsync(fd: c_int) -> c_int {
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"fsync"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fsync".as_ptr()) };
    // SAFETY: glibc defines `fsync`, so `symbol` is the non-null address of that function, and its
    // C prototype `int fsync(int)` matches this function pointer type.
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    let synced = real(fd);
    if synced == 0 {
        record(Recorded::Sync);
    }
    if HOLD_FSYNC.swap(false, Ordering::SeqCst) {
        FSYNCED.store(true, Ordering::SeqCst);
        wait_for(&RESUMED);
    }
    synced
}

/// Passes every call to glibc's `fdatasync`, recording one that succeeds while `RECORDING` is
/// set.
#[unsafe(no_mangle)]
pub extern "C" fn fdatasync(fd: c_int) -> c_int {
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"fdatasync"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fdatasync".as_ptr()) };
    // SAFETY: glibc defines `fdatasync`, so `symbol` is the non-null address of that function, and
    // its C prototype `int fdatasync(int)` matches this function pointer type.
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    let synced = real(fd);
    if synced == 0 {
        record(Recorded::Sync);
    }
    synced
}

/// Passes every call to glibc's `ftruncate64`, recording it while `RECORDING` is set.
#[unsafe(no_mangle)]
pub extern "C" fn ftruncate64(fd: c_int, length: i64) -> c_int {
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"ftruncate64"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"ftruncate64".as_ptr()) };
    // SAFETY: glibc defines `ftruncate64`, so `symbol` is the non-null address of that function,
    // and its C prototype `int ftruncate64(int, off64_t)` matches this function pointer type.
    let real: extern "C" fn(c_int, i64) -> c_int = unsafe { std::mem::transmute(symbol) };
    record(Recorded::SetLen(length));
    real(fd, length)
}

fn reset() {
    HOLD_FSYNC.store(false, Ordering::SeqCst);
    RECORDING.store(false, Ordering::SeqCst);
    RECORDED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
    for flag in [&STALLED, &RESUMED, &TORN, &MENDED, &FSYNCED] {
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

fn newest_log_end(path: &Path) -> u64 {
    let file = File::open(path).unwrap();
    let mut bytes = [0u8; 8];
    file.read_exact_at(&mut bytes, (newest_slot(path) * SLOT_BYTES + 16) as u64)
        .unwrap();
    u64::from_le_bytes(bytes)
}

fn buffered() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20)
}

fn lose_the_last_of_four_frames(path: &Path, header: u64) {
    let db = buffered().open::<Root>(path).unwrap();
    let mut ends = Vec::new();
    for n in 0..4 {
        put(&db, n);
        ends.push(newest_log_end(path));
    }
    let mut image = std::fs::read(path).unwrap();
    drop(db);
    image.truncate(ends[3] as usize);
    image[(ends[2] + header) as usize..].fill(0);
    std::fs::write(path, &image).unwrap();
}

fn open_beside_a_rollback(path: &Path, held: &AtomicBool, release: &AtomicBool) {
    let writer = thread::spawn({
        let path = path.to_path_buf();
        move || buffered().open::<Root>(&path).map(|db| db.version())
    });
    wait_for(held);
    assert!(held.load(Ordering::SeqCst));
    let file = File::open(path).unwrap();
    let mut seqs = [0, SLOT_BYTES].map(|at| {
        let mut bytes = [0u8; 8];
        file.read_exact_at(&mut bytes, (at + 8) as u64).unwrap();
        u64::from_le_bytes(bytes)
    });
    seqs.sort_unstable();
    assert_eq!(seqs, [4, 5]);
    let seen = reader_options().open::<Root>(path).map(|db| db.version());
    release.store(true, Ordering::SeqCst);
    let recovered = writer.join().unwrap().unwrap();
    assert_eq!(recovered, 3);
    let seen = seen.unwrap_or_else(|error| {
        panic!("a read-only open beside a recovery that rolled back failed: {error}")
    });
    assert!(
        seen <= recovered,
        "a read-only open saw version {seen}, past the {recovered} kept"
    );
}

#[test]
fn a_read_only_open_beside_a_recovery_that_rolls_back_opens_at_a_kept_commit() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    reset();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("b.ferro");
    lose_the_last_of_four_frames(&path, 0);
    TEAR_AT.store((1 - newest_slot(&path)) * SLOT_BYTES, Ordering::SeqCst);
    open_beside_a_rollback(&path, &TORN, &MENDED);
}

#[test]
fn a_read_only_open_between_the_superblock_writes_of_a_rollback_opens_at_a_kept_commit() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    reset();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("s.ferro");
    lose_the_last_of_four_frames(&path, 0);
    HOLD_FSYNC.store(true, Ordering::SeqCst);
    open_beside_a_rollback(&path, &FSYNCED, &RESUMED);
}

#[test]
fn a_read_only_open_that_read_the_superblocks_before_a_rollback_opens_at_a_kept_commit() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    reset();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("e.ferro");
    lose_the_last_of_four_frames(&path, 0);
    thread::scope(|scope| {
        let reader = scope.spawn(|| {
            STALL_AFTER_SLOT_1.with(|stall| stall.set(true));
            reader_options().open::<Root>(&path).map(|db| db.version())
        });
        wait_for(&STALLED);
        assert!(STALLED.load(Ordering::SeqCst));
        let writer = buffered().open::<Root>(&path).unwrap();
        assert_eq!(writer.version(), 3);
        RESUMED.store(true, Ordering::SeqCst);
        let seen = reader.join().unwrap().unwrap_or_else(|error| {
            panic!("a read-only open that read the superblocks before a rollback failed: {error}")
        });
        assert_eq!(seen, 3);
    });
}

fn apply(image: &mut Vec<u8>, change: &Recorded, half: bool) {
    match change {
        Recorded::Write(at, bytes) => {
            let bytes = match half {
                true => &bytes[..bytes.len() / 2],
                false => &bytes[..],
            };
            let (at, end) = (*at as usize, *at as usize + bytes.len());
            if image.len() < end {
                image.resize(end, 0);
            }
            image[at..end].copy_from_slice(bytes);
        }
        Recorded::SetLen(len) => image.resize(*len as usize, 0),
        Recorded::Sync => {}
    }
}

fn keeps_three_commits(path: &Path, context: &str) {
    let reader = reader_options()
        .open::<Root>(path)
        .unwrap_or_else(|error| panic!("{context}: read-only open: {error}"));
    assert_eq!(reader.version(), 3, "{context}");
    drop(reader);
    let db = buffered()
        .open::<Root>(path)
        .unwrap_or_else(|error| panic!("{context}: open: {error}"));
    assert_eq!(db.version(), 3, "{context}");
    let kept: Vec<Option<u64>> = (0..4).map(|n| db.read().get(&n).map(|v| *v)).collect();
    assert_eq!(kept, [Some(0), Some(1), Some(2), None], "{context}");
    db.verify()
        .unwrap_or_else(|error| panic!("{context}: verify: {error}"));
    put(&db, 7);
    drop(db);
    let db = buffered()
        .open::<Root>(path)
        .unwrap_or_else(|error| panic!("{context}: reopen: {error}"));
    assert_eq!(db.version(), 4, "{context}");
    db.verify()
        .unwrap_or_else(|error| panic!("{context}: verify after a commit: {error}"));
}

#[test]
fn a_power_cut_anywhere_in_a_rollback_keeps_the_commits_it_keeps() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    reset();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("p.ferro");
    lose_the_last_of_four_frames(&path, FRAME_BYTES);
    let base = std::fs::read(&path).unwrap();
    let dropped = newest_slot(&path) * SLOT_BYTES;
    RECORDING.store(true, Ordering::SeqCst);
    let opened = buffered().open::<Root>(&path).map(|db| db.version());
    RECORDING.store(false, Ordering::SeqCst);
    assert_eq!(opened.unwrap(), 3);
    let changes = RECORDED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let kinds: Vec<String> = changes
        .iter()
        .map(|change| match change {
            Recorded::Write(at, bytes) => {
                let seq = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
                format!("superblock {seq} at {at}")
            }
            Recorded::SetLen(_) => "truncate".into(),
            Recorded::Sync => "sync".into(),
        })
        .collect();
    let kept = SLOT_BYTES - dropped;
    assert_eq!(
        kinds,
        [
            "sync".to_string(),
            format!("superblock 5 at {kept}"),
            "sync".into(),
            "truncate".into(),
            "sync".into(),
            format!("superblock 5 at {dropped}"),
            "sync".into(),
        ]
    );
    let image_path = dir.path().join("cut.ferro");
    let mut checked = 0;
    for cut in 0..=changes.len() {
        let durable = changes[..cut]
            .iter()
            .rposition(|change| matches!(change, Recorded::Sync))
            .map_or(0, |at| at + 1);
        let pending = &changes[durable..cut];
        let torn = matches!(pending.last(), Some(Recorded::Write(..)));
        for (kept, half) in [(false, false), (true, false), (true, true)] {
            if half && !torn {
                continue;
            }
            let mut image = base.clone();
            for change in &changes[..durable] {
                apply(&mut image, change, false);
            }
            if kept {
                for change in pending {
                    apply(&mut image, change, half);
                }
            }
            std::fs::write(&image_path, &image).unwrap();
            let context = format!(
                "cut {cut} after {:?}, kept {kept}, half {half}",
                &kinds[..cut]
            );
            keeps_three_commits(&image_path, &context);
            checked += 1;
        }
    }
    assert!(checked >= 15, "{checked}");
}
