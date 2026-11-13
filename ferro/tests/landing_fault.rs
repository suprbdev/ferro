#![cfg(unix)]

mod common;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_void};
use std::future::Future;
use std::path::Path;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use common::{Tripwire, quiet_ferro_panics};
use ferro::db::Hook;
use ferro::{Db, Durability, Error, Map, Options, Reclaim};
use tempfile::TempDir;

type Root = Map<u64, u64>;
type Text = Map<u64, String>;
type Kind = (&'static str, fn() -> Options);
type Grouping = (&'static str, fn() -> Options, bool);

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const EIO: c_int = 5;
const ENOSPC: c_int = 28;
const PATIENCE: Duration = Duration::from_secs(30);
const MANUAL: Reclaim = Reclaim::off();

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn __errno_location() -> *mut c_int;
}

static SERIAL: Mutex<()> = Mutex::new(());
static SYNC_ARMED: AtomicBool = AtomicBool::new(false);
static SYNC_ENTERED: AtomicBool = AtomicBool::new(false);
static SYNC_RELEASED: AtomicBool = AtomicBool::new(false);
static SYNC_FAILED: AtomicBool = AtomicBool::new(false);
static WRITE_HELD: AtomicBool = AtomicBool::new(false);
static WRITE_ENTERED: AtomicBool = AtomicBool::new(false);
static WRITE_RELEASED: AtomicBool = AtomicBool::new(false);
static SUPERS_FAIL: AtomicBool = AtomicBool::new(false);
static ZEROS_FAIL: AtomicBool = AtomicBool::new(false);
static ZEROS_FAILED: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
enum Garble {
    Zeroes,
    Flip,
}

thread_local! {
    static STAGED_FAIL: Cell<u32> = const { Cell::new(0) };
    static STAGED_GARBLE: Cell<(u32, Garble)> = const { Cell::new((0, Garble::Zeroes)) };
    static FRAMES_FAIL: Cell<bool> = const { Cell::new(false) };
    static FRAMES_HELD: Cell<bool> = const { Cell::new(false) };
}

#[unsafe(no_mangle)]
pub extern "C" fn fdatasync(fd: c_int) -> c_int {
    if SYNC_ARMED.swap(false, Ordering::SeqCst) {
        SYNC_ENTERED.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + PATIENCE;
        while !SYNC_RELEASED.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        SYNC_FAILED.store(true, Ordering::SeqCst);
        // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's `errno`,
        // valid for writes for as long as the thread runs.
        unsafe { *__errno_location() = EIO };
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

fn is_frame(buf: *const c_void, count: usize) -> bool {
    // SAFETY: `is_frame` is only called from `pwrite64` with its own `buf` and `count`, and a
    // caller of `pwrite64` guarantees that `buf` is readable for `count` bytes, and `count` is at
    // least 8 here.
    count >= 8 && unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), 8) } == b"FERROFRM"
}

fn is_zeros(buf: *const c_void, count: usize) -> bool {
    count >= 4096
        // SAFETY: `is_zeros` is only called from `pwrite64` with its own `buf` and `count`, and a
        // caller of `pwrite64` guarantees that `buf` is readable for `count` bytes, which is the
        // length read here.
        && unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), count) }
            .iter()
            .all(|byte| *byte == 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn pwrite64(fd: c_int, buf: *const c_void, count: usize, offset: i64) -> isize {
    if offset < 8192 && count > 0 && SUPERS_FAIL.swap(false, Ordering::SeqCst) {
        // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's `errno`,
        // valid for writes for as long as the thread runs.
        unsafe { *__errno_location() = EIO };
        return -1;
    }
    if offset >= 8192 && ZEROS_FAIL.load(Ordering::SeqCst) && is_zeros(buf, count) {
        ZEROS_FAILED.fetch_add(1, Ordering::SeqCst);
        // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's `errno`,
        // valid for writes for as long as the thread runs.
        unsafe { *__errno_location() = ENOSPC };
        return -1;
    }
    let mut garbled = None;
    if offset >= 8192 && count >= 256 << 10 && !is_frame(buf, count) {
        let left = STAGED_FAIL.with(|fail| fail.replace(fail.get().saturating_sub(1)));
        if left == 1 {
            // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's
            // `errno`, valid for writes for as long as the thread runs.
            unsafe { *__errno_location() = ENOSPC };
            return -1;
        }
        let (left, how) = STAGED_GARBLE.with(|garble| {
            let (left, how) = garble.get();
            garble.set((left.saturating_sub(1), how));
            (left, how)
        });
        if left == 1 {
            // SAFETY: a caller of `pwrite64` guarantees that `buf` is readable for `count` bytes,
            // which is the length read here.
            let mut bytes = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), count) }.to_vec();
            match how {
                Garble::Zeroes => bytes.fill(0),
                Garble::Flip => bytes[count / 2] ^= 0x10,
            }
            garbled = Some(bytes);
        }
    }
    let buf = garbled
        .as_ref()
        .map_or(buf, |bytes| bytes.as_ptr().cast::<c_void>());
    if is_frame(buf, count) {
        if FRAMES_FAIL.with(Cell::get) {
            // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's
            // `errno`, valid for writes for as long as the thread runs.
            unsafe { *__errno_location() = ENOSPC };
            return -1;
        }
        if FRAMES_HELD.with(Cell::get) && WRITE_HELD.swap(false, Ordering::SeqCst) {
            WRITE_ENTERED.store(true, Ordering::SeqCst);
            wait_until("the held write's release", || {
                WRITE_RELEASED.load(Ordering::SeqCst)
            });
        }
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

fn serial() -> MutexGuard<'static, ()> {
    let turn = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    for flag in [
        &SYNC_ARMED,
        &SYNC_ENTERED,
        &SYNC_RELEASED,
        &SYNC_FAILED,
        &WRITE_HELD,
        &WRITE_ENTERED,
        &WRITE_RELEASED,
        &SUPERS_FAIL,
        &ZEROS_FAIL,
    ] {
        flag.store(false, Ordering::SeqCst);
    }
    ZEROS_FAILED.store(0, Ordering::SeqCst);
    turn
}

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn fsync() -> Options {
    Options::new().reclaim(MANUAL)
}

fn buffered() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .reclaim(MANUAL)
}

fn coalesced_fsync() -> Options {
    fsync().coalesce(true)
}

fn coalesced_buffered() -> Options {
    buffered().coalesce(true)
}

fn kinds() -> [Kind; 2] {
    [("fsync", fsync), ("buffered", buffered)]
}

fn coalesced_kinds() -> [Kind; 2] {
    [
        ("fsync, coalesced", coalesced_fsync),
        ("buffered, coalesced", coalesced_buffered),
    ]
}

fn every_kind() -> [Grouping; 4] {
    [
        ("fsync", fsync, false),
        ("buffered", buffered, false),
        ("fsync, coalesced", coalesced_fsync, true),
        ("buffered, coalesced", coalesced_buffered, true),
    ]
}

fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn joined<T>(what: &str, handle: JoinHandle<T>) -> thread::Result<T> {
    wait_until(what, || handle.is_finished());
    handle.join()
}

fn contents(db: &Db<Root>) -> BTreeMap<u64, u64> {
    db.read().iter().map(|(key, value)| (key, *value)).collect()
}

fn put(db: &Db<Root>, key: u64, value: u64) -> ferro::Result<u64> {
    db.commit_with(|root| {
        root.insert(key, value);
        ("", ())
    })
    .map(|done| done.version)
}

#[derive(Default)]
struct Gate {
    reached: AtomicBool,
    open: AtomicBool,
}

impl Gate {
    fn pass(&self) {
        self.reached.store(true, Ordering::SeqCst);
        wait_until("a gate to open", || self.open.load(Ordering::SeqCst));
    }

    fn reached(&self) -> bool {
        self.reached.load(Ordering::SeqCst)
    }

    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Stalls {
    landing: BTreeMap<&'static str, Gate>,
    landed: BTreeMap<&'static str, Gate>,
    arriving: BTreeMap<&'static str, Gate>,
    leading: BTreeMap<&'static str, Gate>,
    fail_at_landing: Mutex<Vec<&'static str>>,
    panic_at_landing: Mutex<Vec<&'static str>>,
    hold_write: Mutex<Vec<&'static str>>,
    passed: AtomicUsize,
}

fn stalls(names: &[&'static str]) -> Arc<Stalls> {
    stalls_holding(names, &[], &[])
}

fn stalls_holding(
    names: &[&'static str],
    arriving: &[&'static str],
    leading: &[&'static str],
) -> Arc<Stalls> {
    let mut stalls = Stalls::default();
    for name in names {
        stalls.landing.insert(name, Gate::default());
        stalls.landed.insert(name, Gate::default());
    }
    for name in arriving {
        stalls.arriving.insert(name, Gate::default());
    }
    for name in leading {
        stalls.leading.insert(name, Gate::default());
    }
    Arc::new(stalls)
}

fn hook(stalls: &Arc<Stalls>) -> Hook {
    let stalls = stalls.clone();
    Hook(Arc::new(move |step| {
        let current = thread::current();
        let Some(name) = current.name() else {
            return;
        };
        let gates = match step {
            "landing" => &stalls.landing,
            "landed" => &stalls.landed,
            "arriving" => &stalls.arriving,
            "leading" => &stalls.leading,
            _ => return,
        };
        let Some((name, gate)) = gates.get_key_value(name) else {
            return;
        };
        gate.pass();
        stalls.passed.fetch_add(1, Ordering::SeqCst);
        if step != "landing" {
            return;
        }
        if stalls.fail_at_landing.lock().unwrap().contains(name) {
            FRAMES_FAIL.with(|fail| fail.set(true));
        }
        if stalls.hold_write.lock().unwrap().contains(name) {
            WRITE_HELD.store(true, Ordering::SeqCst);
            FRAMES_HELD.with(|held| held.set(true));
        }
        if stalls.panic_at_landing.lock().unwrap().contains(name) {
            panic!("ferro: a writer died between placing and writing its frame");
        }
    }))
}

fn writer(
    db: &Db<Root>,
    name: &'static str,
    key: u64,
    value: u64,
) -> JoinHandle<ferro::Result<u64>> {
    let db = db.clone();
    thread::Builder::new()
        .name(name.to_string())
        .spawn(move || put(&db, key, value))
        .unwrap()
}

fn seeded(options: Options, path: &Path, branches: &[&str]) -> (Db<Root>, Vec<Db<Root>>) {
    let db: Db<Root> = options.open(path).unwrap();
    put(&db, 0, 0).unwrap();
    let handles = branches.iter().map(|name| db.fork(name).unwrap()).collect();
    (db, handles)
}

#[test]
fn a_failed_checkpoint_sync_fails_the_commits_queued_behind_it() {
    let _turn = serial();
    let checkpointed: [Kind; 2] = [
        ("buffered", buffered),
        ("buffered, coalesced", coalesced_buffered),
    ];
    for (kind, options) in checkpointed {
        for flag in [&SYNC_ARMED, &SYNC_ENTERED, &SYNC_RELEASED, &SYNC_FAILED] {
            flag.store(false, Ordering::SeqCst);
        }
        let dir = dir();
        let path = dir.path().join("checkpoint.ferro");
        let stalls = stalls(&["queued"]);
        stalls.landing["queued"].open();
        let held = options().checkpoint_bytes(1).hook(hook(&stalls));
        let (db, branches) = seeded(held, &path, &["side"]);
        put(&db, 1, 1).unwrap();
        SYNC_ARMED.store(true, Ordering::SeqCst);
        let leader = writer(&db, "leader", 2, 2);
        wait_until("the checkpoint to start", || {
            SYNC_ENTERED.load(Ordering::SeqCst)
        });
        let queued = writer(&branches[0], "queued", 3, 3);
        wait_until("the queued frame to land", || {
            stalls.landed["queued"].reached()
        });
        SYNC_RELEASED.store(true, Ordering::SeqCst);
        let _ = joined("the leader", leader).unwrap();
        assert!(SYNC_FAILED.load(Ordering::SeqCst), "{kind}");
        stalls.landed["queued"].open();
        let queued = joined("the queued writer", queued).unwrap();
        assert!(
            matches!(queued, Err(Error::Poisoned)),
            "{kind}: a commit queued when its checkpoint's fdatasync failed was acknowledged: \
             {queued:?}"
        );
        assert!(matches!(put(&db, 4, 4), Err(Error::Poisoned)), "{kind}");
        drop((db, branches));
        let db: Db<Root> = options().open(&path).unwrap();
        assert_eq!(contents(&db).get(&2), Some(&2), "{kind}");
        db.verify().unwrap();
    }
}

fn check_failed_landing((kind, options, coalesced): Grouping, panics: bool) {
    let dir = dir();
    let path = dir.path().join("landing.ferro");
    let names = ["before", "failing", "ahead-1", "ahead-2"];
    let stalls = stalls(&names);
    for name in ["ahead-1", "ahead-2"] {
        stalls.landing[name].open();
        stalls.landed[name].open();
    }
    stalls.landed["before"].open();
    stalls.landed["failing"].open();
    match panics {
        true => stalls.panic_at_landing.lock().unwrap().push("failing"),
        false => stalls.fail_at_landing.lock().unwrap().push("failing"),
    }
    let (db, branches) = seeded(
        options().hook(hook(&stalls)),
        &path,
        &["b0", "b1", "b2", "b3"],
    );
    for (key, branch) in (10..).zip(&branches) {
        put(branch, key, 1).unwrap();
    }
    let before = writer(&branches[0], "before", 10, 2);
    wait_until("the first frame to be placed", || {
        stalls.landing["before"].reached()
    });
    let failing = writer(&branches[1], "failing", 11, 2);
    wait_until("the failing frame to be placed", || {
        stalls.landing["failing"].reached()
    });
    let ahead: Vec<_> = [("ahead-1", 2), ("ahead-2", 3)]
        .into_iter()
        .map(|(name, at)| writer(&branches[at], name, 10 + at as u64, 2))
        .collect();
    match coalesced {
        true => {
            wait_until("a frame ahead to land", || {
                stalls.landed["ahead-1"].reached() || stalls.landed["ahead-2"].reached()
            });
            thread::sleep(Duration::from_millis(30));
        }
        false => wait_until("the frames ahead to land", || {
            stalls.landed["ahead-1"].reached() && stalls.landed["ahead-2"].reached()
        }),
    }
    stalls.landing["failing"].open();
    if coalesced {
        stalls.landing["before"].open();
    }
    let failed = joined("the failing writer", failing);
    match (coalesced, panics, &failed) {
        (true, _, Ok(Err(Error::Poisoned)))
        | (false, true, Err(_))
        | (false, false, Ok(Err(Error::Io(_)))) => {}
        _ => panic!("{kind}: the failing writer returned {failed:?}"),
    }
    stalls.landing["before"].open();
    let before = joined("the writer before", before).unwrap();
    let ahead: Vec<_> = ahead
        .into_iter()
        .map(|handle| joined("a writer ahead", handle).unwrap())
        .collect();
    for result in &ahead {
        assert!(
            matches!(result, Err(Error::Poisoned)),
            "{kind}: a frame placed after a failed one was answered {result:?}"
        );
    }
    assert!(matches!(before, Ok(3)), "{kind}: {before:?}");
    assert!(matches!(put(&db, 1, 1), Err(Error::Poisoned)), "{kind}");
    let published: Vec<_> = branches.iter().map(contents).collect();
    for (at, seen) in published.iter().enumerate().skip(1) {
        assert_eq!(seen.get(&(10 + at as u64)), Some(&1), "{kind}: b{at}");
    }
    drop((db, branches));
    let db: Db<Root> = options().open(&path).unwrap();
    for at in 0..4u64 {
        let branch = db.branch(&format!("b{at}")).unwrap();
        let value = contents(&branch).get(&(10 + at)).copied();
        let expected = if at == 0 { 2 } else { 1 };
        assert_eq!(value, Some(expected), "{kind}: b{at}");
    }
    db.verify().unwrap();
    put(&db, 99, 99).unwrap();
    drop(db);
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(contents(&db).get(&99), Some(&99), "{kind}");
    db.verify().unwrap();
}

#[test]
fn a_frame_whose_write_fails_after_tip_fails_every_frame_placed_after_it() {
    let _turn = serial();
    for kind in every_kind() {
        check_failed_landing(kind, false);
    }
}

#[test]
fn a_writer_that_dies_between_placing_and_writing_strands_no_one() {
    let _turn = serial();
    quiet_ferro_panics();
    for kind in every_kind() {
        check_failed_landing(kind, true);
    }
}

fn check_frame_ahead_of_failure((kind, options, coalesced): Grouping, in_write: bool) {
    for flag in [&WRITE_HELD, &WRITE_ENTERED, &WRITE_RELEASED] {
        flag.store(false, Ordering::SeqCst);
    }
    let dir = dir();
    let path = dir.path().join("ahead.ferro");
    let stalls = stalls(&["before", "failing", "after"]);
    stalls.landing["before"].open();
    stalls.landing["after"].open();
    stalls.landed["after"].open();
    stalls.landed["failing"].open();
    stalls.fail_at_landing.lock().unwrap().push("failing");
    if in_write {
        stalls.hold_write.lock().unwrap().push("before");
        stalls.landed["before"].open();
    }
    let held = match in_write {
        true => &stalls.landing["before"],
        false => &stalls.landed["before"],
    };
    let (db, branches) = seeded(options().hook(hook(&stalls)), &path, &["b0", "b1", "b2"]);
    for (key, branch) in (10..).zip(&branches) {
        put(branch, key, 1).unwrap();
    }
    let before = writer(&branches[0], "before", 10, 2);
    wait_until("the first frame to stall", || {
        held.reached() && (!in_write || WRITE_ENTERED.load(Ordering::SeqCst))
    });
    let failing = writer(&branches[1], "failing", 11, 2);
    wait_until("the failing frame to be placed", || {
        stalls.landing["failing"].reached()
    });
    let after = writer(&branches[2], "after", 12, 2);
    wait_until("the frame after it to land", || {
        stalls.landed["after"].reached()
    });
    stalls.landing["failing"].open();
    if coalesced {
        WRITE_RELEASED.store(true, Ordering::SeqCst);
        stalls.landed["before"].open();
    }
    let failed = joined("the failing writer", failing).unwrap();
    match coalesced {
        true => assert!(matches!(failed, Err(Error::Poisoned)), "{kind}: {failed:?}"),
        false => assert!(matches!(failed, Err(Error::Io(_))), "{kind}: {failed:?}"),
    }
    WRITE_RELEASED.store(true, Ordering::SeqCst);
    stalls.landed["before"].open();
    let before = joined("the writer before", before).unwrap();
    let after = joined("the writer after", after).unwrap();
    assert!(
        matches!(before, Ok(3)),
        "{kind}: a whole frame placed ahead of the failed one was answered {before:?}"
    );
    assert!(
        matches!(after, Err(Error::Poisoned)),
        "{kind}: a frame placed after the failed one was answered {after:?}"
    );
    assert_eq!(contents(&branches[0]).get(&10), Some(&2), "{kind}");
    assert_eq!(contents(&branches[2]).get(&12), Some(&1), "{kind}");
    drop((db, branches));
    let db: Db<Root> = options().open(&path).unwrap();
    for (at, value) in [(0, 2), (1, 1), (2, 1)] {
        let branch = db.branch(&format!("b{at}")).unwrap();
        assert_eq!(
            contents(&branch).get(&(10 + at)),
            Some(&value),
            "{kind}: b{at}"
        );
    }
    db.verify().unwrap();
}

#[test]
fn a_frame_still_in_its_write_when_a_later_one_fails_is_acknowledged() {
    let _turn = serial();
    for kind in every_kind() {
        check_frame_ahead_of_failure(kind, true);
    }
}

#[test]
fn a_leader_whose_own_frame_follows_a_failed_one_is_not_acknowledged() {
    let _turn = serial();
    for kind in every_kind() {
        check_frame_ahead_of_failure(kind, false);
    }
}

struct Flag {
    thread: Thread,
    woken: AtomicBool,
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::SeqCst);
        self.thread.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let flag = Arc::new(Flag {
        thread: thread::current(),
        woken: AtomicBool::new(false),
    });
    let waker = Waker::from(flag.clone());
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        while !flag.woken.swap(false, Ordering::SeqCst) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "an awaited write was never woken");
            thread::park_timeout(left);
        }
    }
}

#[test]
fn awaited_writes_behind_a_failed_frame_are_woken_with_the_failure() {
    let _turn = serial();
    for (kind, options, coalesced) in every_kind() {
        let dir = dir();
        let path = dir.path().join("awaited.ferro");
        let stalls = stalls(&["failing"]);
        stalls.landed["failing"].open();
        stalls.fail_at_landing.lock().unwrap().push("failing");
        let (db, branches) = seeded(options().hook(hook(&stalls)), &path, &["b0", "b1", "b2"]);
        let failing = writer(&branches[0], "failing", 1, 1);
        wait_until("the failing frame to be placed", || {
            stalls.landing["failing"].reached()
        });
        let awaited: Vec<_> = branches[1..]
            .iter()
            .map(|branch| {
                let branch = branch.clone();
                thread::spawn(move || {
                    block_on(branch.update_async(|root| {
                        root.insert(2, 2);
                    }))
                })
            })
            .collect();
        thread::sleep(Duration::from_millis(30));
        stalls.landing["failing"].open();
        let failed = joined("the failing writer", failing);
        match coalesced {
            true => assert!(
                matches!(failed, Ok(Err(Error::Poisoned))),
                "{kind}: {failed:?}"
            ),
            false => assert!(
                matches!(failed, Ok(Err(Error::Io(_)))),
                "{kind}: {failed:?}"
            ),
        }
        for handle in awaited {
            let result = joined("an awaited writer", handle).unwrap();
            assert!(matches!(result, Err(Error::Poisoned)), "{kind}: {result:?}");
        }
        drop((db, branches));
        let db: Db<Root> = options().open(&path).unwrap();
        for at in 0..3 {
            let branch = db.branch(&format!("b{at}")).unwrap();
            assert_eq!(contents(&branch).get(&1), None, "{kind}");
            assert_eq!(contents(&branch).get(&2), None, "{kind}");
        }
        db.verify().unwrap();
    }
}

#[test]
fn a_write_held_inside_pwrite_holds_back_every_sync_and_superblock() {
    let _turn = serial();
    for (kind, options, _) in every_kind() {
        let dir = dir();
        let path = dir.path().join("held.ferro");
        let stalls = stalls(&["held"]);
        stalls.landing["held"].open();
        stalls.landed["held"].open();
        stalls.hold_write.lock().unwrap().push("held");
        let (db, branches) = seeded(options().hook(hook(&stalls)), &path, &["b0", "b1", "b2"]);
        let held = writer(&branches[0], "held", 1, 1);
        wait_until("the held write", || WRITE_ENTERED.load(Ordering::SeqCst));
        let size = db.log_bytes();
        let later: Vec<_> = branches[1..]
            .iter()
            .map(|branch| {
                let branch = branch.clone();
                thread::spawn(move || put(&branch, 2, 2))
            })
            .collect();
        let forker = {
            let db = db.clone();
            thread::spawn(move || db.fork("forked").map(|_| ()))
        };
        thread::sleep(Duration::from_millis(50));
        assert!(later.iter().all(|handle| !handle.is_finished()), "{kind}");
        assert!(!forker.is_finished(), "{kind}");
        assert_eq!(
            db.log_bytes(),
            size,
            "{kind}: a superblock moved past a held frame"
        );
        let reader: Db<Root> = Options::new().read_only(true).open(&path).unwrap();
        for at in 0..3 {
            let branch = reader.branch(&format!("b{at}")).unwrap();
            assert_eq!(contents(&branch).get(&2), None, "{kind}");
            assert_eq!(contents(&branch).get(&1), None, "{kind}");
        }
        WRITE_RELEASED.store(true, Ordering::SeqCst);
        assert_eq!(
            joined("the held writer", held).unwrap().unwrap(),
            2,
            "{kind}"
        );
        for handle in later {
            assert_eq!(joined("a later writer", handle).unwrap().unwrap(), 2);
        }
        joined("the fork", forker).unwrap().unwrap();
        reader.refresh().unwrap();
        for at in 0..3 {
            let branch = reader.branch(&format!("b{at}")).unwrap();
            let expected = if at == 0 { (1, 1) } else { (2, 2) };
            assert_eq!(
                contents(&branch).get(&expected.0),
                Some(&expected.1),
                "{kind}"
            );
        }
        assert!(reader.branches().contains(&"forked".to_string()), "{kind}");
        reader.verify().unwrap();
        drop((db, branches, reader));
        let db: Db<Root> = options().open(&path).unwrap();
        assert_eq!(db.branches().len(), 5, "{kind}");
        db.verify().unwrap();
        WRITE_RELEASED.store(false, Ordering::SeqCst);
        WRITE_ENTERED.store(false, Ordering::SeqCst);
    }
}

fn merge_theirs(
    root: &mut Text,
    base: &ferro::Snapshot<Text>,
    theirs: &ferro::Snapshot<Text>,
) -> (&'static str, ()) {
    root.merge(base, theirs, |conflict| {
        conflict.theirs.map(|value| (*value).clone())
    });
    ("merge", ())
}

#[test]
fn a_staging_frame_whose_records_fail_to_write_fails_the_receive_and_nothing_else() {
    let _turn = serial();
    for (kind, options) in kinds() {
        let dir = dir();
        let sender: Db<Text> = buffered().open(dir.path().join("sender.ferro")).unwrap();
        sender
            .update(|root| {
                for key in 0..3_000 {
                    root.insert(key, common::text(key, 1_000));
                }
            })
            .unwrap();
        let path = dir.path().join("receiver.ferro");
        let db: Db<Text> = options().open(&path).unwrap();
        db.update(|root| {
            root.insert(0, "base".to_string());
        })
        .unwrap();
        let side = db.fork("side").unwrap();
        let bundle = sender.bundle(&db.have().unwrap()).unwrap();
        assert!(bundle.as_bytes().len() > 2 << 20, "{kind}");
        STAGED_FAIL.with(|fail| fail.set(2));
        let received = db.receive("peer", &bundle, merge_theirs);
        let unfailed = STAGED_FAIL.with(|fail| fail.replace(0));
        assert_eq!(
            unfailed, 0,
            "{kind}: a second staging frame's records were never written"
        );
        match received {
            Err(Error::Io(error)) => assert_eq!(error.raw_os_error(), Some(ENOSPC), "{kind}"),
            other => panic!("{kind}: a receive whose records failed to write: {other:?}"),
        }
        assert!(db.branch("peer").is_err(), "{kind}");
        side.update(|root| {
            root.insert(1, "side".to_string());
        })
        .unwrap();
        db.update(|root| {
            root.insert(2, "main".to_string());
        })
        .unwrap();
        db.verify().unwrap();
        let pulled = db.receive("peer", &bundle, merge_theirs).unwrap();
        assert_eq!(pulled.outcome, ferro::Outcome::Merged, "{kind}");
        assert_eq!(db.branch("peer").unwrap().head_hash(), sender.head_hash());
        drop((db, side));
        let db: Db<Text> = options().open(&path).unwrap();
        db.verify_typed().unwrap();
        assert_eq!(db.branch("peer").unwrap().head_hash(), sender.head_hash());
        assert_eq!(
            db.branch("side").unwrap().read().get(&1).as_deref(),
            Some(&"side".to_string())
        );
    }
}

fn garbled_receive(how: Garble, swapped: bool, wanted: &str) {
    for (kind, options) in kinds() {
        let dir = dir();
        let sender: Db<Text> = buffered().open(dir.path().join("sender.ferro")).unwrap();
        sender
            .update(|root| {
                for key in 0..3_000 {
                    root.insert(key, common::text(key, 1_000));
                }
            })
            .unwrap();
        let path = dir.path().join("receiver.ferro");
        let db: Db<Text> = options().open(&path).unwrap();
        db.update(|root| {
            root.insert(0, "base".to_string());
        })
        .unwrap();
        let bundle = sender.bundle(&db.have().unwrap()).unwrap();
        assert!(bundle.as_bytes().len() > 2 << 20, "{kind}");
        let input = Tripwire::new(bundle.as_bytes(), 256 << 10, || {
            if swapped {
                let db = db.clone();
                thread::spawn(move || db.compact(ferro::Keep::All))
                    .join()
                    .unwrap()
                    .unwrap();
            }
        });
        STAGED_GARBLE.with(|garble| garble.set((1, how)));
        let received = db.receive_from("peer", input, merge_theirs);
        let (left, _) = STAGED_GARBLE.with(|garble| garble.replace((0, Garble::Zeroes)));
        assert_eq!(left, 0, "{kind}: no staged write was garbled");
        match received {
            Err(Error::Corrupt(message)) => {
                assert!(message.contains(wanted), "{kind}: {message}")
            }
            other => panic!("{kind}: a receive whose records were garbled: {other:?}"),
        }
        assert!(db.branch("peer").is_err(), "{kind}");
        db.verify().unwrap();
        let pulled = db.receive("peer", &bundle, merge_theirs).unwrap();
        assert_eq!(pulled.outcome, ferro::Outcome::Merged, "{kind}");
        drop(db);
        let db: Db<Text> = options().open(&path).unwrap();
        db.verify_typed().unwrap();
        assert_eq!(db.branch("peer").unwrap().head_hash(), sender.head_hash());
    }
}

#[test]
fn a_receive_whose_staged_records_were_written_as_zeroes_refuses_them_in_place() {
    let _turn = serial();
    garbled_receive(Garble::Zeroes, false, "reads as zeroes");
}

#[test]
fn a_receive_that_a_cycle_moved_refuses_records_written_past_the_old_file_as_zeroes() {
    let _turn = serial();
    garbled_receive(Garble::Zeroes, true, "reads as zeroes");
}

#[test]
fn a_receive_that_a_cycle_moved_refuses_a_record_whose_bytes_changed_past_the_old_file() {
    let _turn = serial();
    garbled_receive(Garble::Flip, true, "hash mismatch for the staged object");
}

#[test]
fn a_failed_sync_under_a_large_receive_fails_the_commits_queued_behind_it() {
    let _turn = serial();
    let dir = dir();
    let stalls = stalls(&["queued"]);
    stalls.landing["queued"].open();
    let armed = Arc::new(AtomicBool::new(false));
    let hooked = {
        let (inner, armed) = (hook(&stalls), armed.clone());
        Hook(Arc::new(move |step| {
            if step == "received" && armed.load(Ordering::SeqCst) {
                SYNC_RELEASED.store(true, Ordering::SeqCst);
                SYNC_ARMED.store(true, Ordering::SeqCst);
            }
            (inner.0)(step);
        }))
    };
    let sender: Db<Text> = buffered().open(dir.path().join("sender.ferro")).unwrap();
    sender
        .update(|root| {
            for key in 0..2_000 {
                root.insert(key, common::text(key, 1_000));
            }
        })
        .unwrap();
    let path = dir.path().join("receiver.ferro");
    let db: Db<Text> = fsync().hook(hooked).open(&path).unwrap();
    db.update(|root| {
        root.insert(0, "base".to_string());
    })
    .unwrap();
    let side = db.fork("side").unwrap();
    let bundle = sender.bundle(&db.have().unwrap()).unwrap();
    assert!(bundle.as_bytes().len() > 3 << 19);
    let queued = {
        let side = side.clone();
        thread::Builder::new()
            .name("queued".to_string())
            .spawn(move || {
                side.update(|root| {
                    root.insert(3, "queued".to_string());
                })
            })
            .unwrap()
    };
    wait_until("the queued frame to land", || {
        stalls.landed["queued"].reached()
    });
    armed.store(true, Ordering::SeqCst);
    let received = db.receive("peer", &bundle, |root: &mut Text, base, theirs| {
        root.merge(base, theirs, |conflict| {
            conflict.theirs.map(|value| (*value).clone())
        });
        ("merge", ())
    });
    assert!(SYNC_FAILED.load(Ordering::SeqCst));
    assert!(matches!(received, Err(Error::Io(_))), "{received:?}");
    stalls.landed["queued"].open();
    let queued = joined("the queued writer", queued).unwrap();
    assert!(
        matches!(queued, Err(Error::Poisoned)),
        "a commit queued when a receive's fdatasync failed was acknowledged: {queued:?}"
    );
    drop((db, side));
    let db: Db<Text> = fsync().open(&path).unwrap();
    db.verify().unwrap();
}

#[test]
fn a_closure_after_a_failed_landing_is_told_of_the_poison() {
    let _turn = serial();
    quiet_ferro_panics();
    for (kind, options, coalesced) in every_kind() {
        let dir = dir();
        let path = dir.path().join("after.ferro");
        let stalls = stalls(&["failing", "ahead"]);
        stalls.landed["failing"].open();
        stalls.landing["ahead"].open();
        stalls.landed["ahead"].open();
        stalls.fail_at_landing.lock().unwrap().push("failing");
        let (db, branches) = seeded(
            options().cache_bytes(1).hook(hook(&stalls)),
            &path,
            &["b0", "b1"],
        );
        let failing = writer(&branches[0], "failing", 1, 1);
        wait_until("the failing frame to be placed", || {
            stalls.landing["failing"].reached()
        });
        let ahead = writer(&branches[1], "ahead", 2, 2);
        wait_until("the frame ahead to land", || {
            stalls.landed["ahead"].reached()
        });
        stalls.landing["failing"].open();
        let failed = joined("the failing writer", failing);
        match coalesced {
            true => assert!(
                matches!(failed, Ok(Err(Error::Poisoned))),
                "{kind}: {failed:?}"
            ),
            false => assert!(
                matches!(failed, Ok(Err(Error::Io(_)))),
                "{kind}: {failed:?}"
            ),
        }
        let ahead = joined("the writer ahead", ahead).unwrap();
        assert!(matches!(ahead, Err(Error::Poisoned)), "{kind}: {ahead:?}");
        let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            branches[1].update(|root| root.get(&2).map(|value| *value))
        }));
        match read {
            Ok(Err(Error::Poisoned)) => {}
            Ok(other) => panic!("{kind}: a closure after the failure returned {other:?}"),
            Err(payload) => panic!(
                "{kind}: a closure after the failure panicked: {}",
                common::panic_text(&*payload)
            ),
        }
        assert_eq!(contents(&branches[1]).get(&2), None, "{kind}");
        drop((db, branches));
    }
}

type Work = Box<dyn FnOnce() -> ferro::Result<()> + Send>;

fn heads(db: &Db<Root>) -> BTreeMap<String, BTreeMap<u64, u64>> {
    db.branches()
        .into_iter()
        .map(|name| (name.clone(), contents(&db.branch(&name).unwrap())))
        .collect()
}

#[test]
fn tables_cycles_and_receives_wait_out_a_frame_held_before_its_write() {
    let _turn = serial();
    for (kind, options) in kinds() {
        let dir = dir();
        let path = dir.path().join("tables.ferro");
        let stalls = stalls(&["held"]);
        stalls.landed["held"].open();
        let (db, branches) = seeded(
            options().hook(hook(&stalls)),
            &path,
            &["b0", "b1", "b2", "b3"],
        );
        let peer: Db<Root> = options().open(dir.path().join("peer.ferro")).unwrap();
        let merge =
            |root: &mut Root, base: &ferro::Snapshot<Root>, theirs: &ferro::Snapshot<Root>| {
                root.merge(base, theirs, |conflict| conflict.theirs.map(|value| *value));
                ("merge", ())
            };
        peer.pull(&db, "origin", merge).unwrap();
        put(&peer, 50, 50).unwrap();
        let held = writer(&branches[0], "held", 1, 1);
        wait_until("the held frame to be placed", || {
            stalls.landing["held"].reached()
        });
        let main = db.clone();
        let (b1, b2) = (branches[1].clone(), branches[2].clone());
        let work: Vec<(&str, Work)> = vec![
            (
                "drop",
                Box::new({
                    let main = main.clone();
                    move || main.drop_branch("b3")
                }),
            ),
            (
                "fork",
                Box::new({
                    let main = main.clone();
                    move || main.fork("x").map(|_| ())
                }),
            ),
            (
                "cycle",
                Box::new({
                    let main = main.clone();
                    move || main.compact(ferro::Keep::All).map(|_| ())
                }),
            ),
            (
                "pull",
                Box::new({
                    let peer = peer.clone();
                    move || b1.pull(&peer, "peer", merge).map(|_| ())
                }),
            ),
            ("commit", Box::new(move || put(&b2, 2, 2).map(|_| ()))),
        ];
        let waiting: Vec<_> = work
            .into_iter()
            .map(|(what, work)| (what, thread::spawn(work)))
            .collect();
        thread::sleep(Duration::from_millis(50));
        for (what, handle) in &waiting {
            assert!(
                !handle.is_finished(),
                "{kind}: {what} finished past a frame not yet written"
            );
        }
        stalls.landing["held"].open();
        joined("the held writer", held).unwrap().unwrap();
        for (what, handle) in waiting {
            joined(what, handle)
                .unwrap()
                .unwrap_or_else(|error| panic!("{kind}: {what}: {error}"));
        }
        let written = heads(&db);
        assert!(!written.contains_key("b3"), "{kind}");
        assert!(written.contains_key("x"), "{kind}");
        assert_eq!(written["b0"].get(&1), Some(&1), "{kind}");
        assert_eq!(written["b1"].get(&50), Some(&50), "{kind}");
        assert_eq!(written["b2"].get(&2), Some(&2), "{kind}");
        db.verify().unwrap();
        drop((db, main, branches));
        let db: Db<Root> = options().open(&path).unwrap();
        assert_eq!(heads(&db), written, "{kind}");
        db.verify().unwrap();
    }
}

#[test]
fn a_superblock_that_fails_to_write_fails_its_batch_and_the_frames_reopen() {
    let _turn = serial();
    for (kind, options, _) in every_kind() {
        let dir = dir();
        let path = dir.path().join("super.ferro");
        let (db, branches) = seeded(options(), &path, &["b0"]);
        put(&branches[0], 1, 1).unwrap();
        SUPERS_FAIL.store(true, Ordering::SeqCst);
        let failed = put(&branches[0], 2, 2);
        assert!(matches!(failed, Err(Error::Io(_))), "{kind}: {failed:?}");
        assert!(matches!(put(&db, 3, 3), Err(Error::Poisoned)), "{kind}");
        assert!(matches!(db.fork("x"), Err(Error::Poisoned)), "{kind}");
        assert_eq!(contents(&branches[0]).get(&2), None, "{kind}");
        drop((db, branches));
        let db: Db<Root> = options().open(&path).unwrap();
        let b0 = db.branch("b0").unwrap();
        assert_eq!(contents(&b0).get(&1), Some(&1), "{kind}");
        assert!(matches!(contents(&b0).get(&2), None | Some(&2)), "{kind}");
        db.verify().unwrap();
        put(&db, 4, 4).unwrap();
    }
}

#[test]
fn a_reservation_that_fails_to_write_leaves_commits_running() {
    let _turn = serial();
    let dir = dir();
    let path = dir.path().join("reserve.ferro");
    let (db, branches) = seeded(fsync(), &path, &["b0", "b1"]);
    ZEROS_FAIL.store(true, Ordering::SeqCst);
    let mut round = 0;
    while ZEROS_FAILED.load(Ordering::SeqCst) == 0 {
        assert!(round < 5_000, "no reservation was written");
        put(&branches[(round % 2) as usize], round % 64, round).unwrap();
        round += 1;
    }
    for round in round..round + 40 {
        put(&branches[(round % 2) as usize], round % 64, round).unwrap();
    }
    ZEROS_FAIL.store(false, Ordering::SeqCst);
    for round in 0..20 {
        put(&branches[(round % 2) as usize], round, round).unwrap();
    }
    let written = heads(&db);
    drop((db, branches));
    let db: Db<Root> = fsync().open(&path).unwrap();
    assert_eq!(heads(&db), written);
    db.verify().unwrap();
}

fn grouped(
    db: &Db<Root>,
    name: &'static str,
    key: u64,
    ran: &Arc<AtomicUsize>,
    after: Option<(&Arc<Stalls>, &'static str)>,
) -> JoinHandle<ferro::Result<u64>> {
    let (db, ran) = (db.clone(), ran.clone());
    let after = after.map(|(stalls, other)| (stalls.clone(), other));
    thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            db.commit_with(|root| {
                if let Some((stalls, other)) = &after {
                    wait_until("another writer to set out for the lock", || {
                        stalls.arriving[other].reached()
                    });
                }
                root.insert(key, 1);
                ran.fetch_add(1, Ordering::SeqCst);
                ("", ())
            })
            .map(|done| done.version)
        })
        .unwrap()
}

fn check_group_sealed_for_another_branch(kind: &str, options: fn() -> Options, panics: bool) {
    let dir = dir();
    let path = dir.path().join("sealed.ferro");
    let stalls = stalls_holding(&["T"], &["X", "Z"], &["L"]);
    stalls.landing["T"].open();
    stalls.landed["T"].open();
    match panics {
        true => stalls.panic_at_landing.lock().unwrap().push("T"),
        false => stalls.fail_at_landing.lock().unwrap().push("T"),
    }
    let (main, branches) = seeded(options().hook(hook(&stalls)), &path, &["a", "b", "c"]);
    let (a, b, c) = (&branches[0], &branches[1], &branches[2]);
    let ran = Arc::new(AtomicUsize::new(0));
    let settle = || thread::sleep(Duration::from_millis(30));
    let leader = grouped(&main, "L", 1, &ran, None);
    wait_until("the leader to start its sync", || {
        stalls.leading["L"].reached()
    });
    let tb = grouped(b, "TB", 2, &ran, None);
    wait_until("b's group to open", || ran.load(Ordering::SeqCst) == 2);
    settle();
    let x = grouped(b, "X", 3, &ran, None);
    wait_until("a writer to set out for b's lock", || {
        stalls.arriving["X"].reached()
    });
    let ta = grouped(a, "TA", 4, &ran, None);
    wait_until("a's group to open", || ran.load(Ordering::SeqCst) == 3);
    settle();
    stalls.leading["L"].open();
    let led = joined("the leader", leader).unwrap();
    assert!(matches!(led, Ok(2)), "{kind}: {led:?}");
    settle();
    let t = grouped(c, "T", 5, &ran, Some((&stalls, "Z")));
    let z = grouped(c, "Z", 6, &ran, None);
    wait_until("c's writer to write a's frame", || {
        stalls.landing["T"].reached()
    });
    settle();
    stalls.arriving["Z"].open();
    for (name, handle) in [("T", t), ("Z", z)] {
        let result = joined(name, handle).unwrap();
        assert!(
            matches!(result, Err(Error::Poisoned)),
            "{kind}: {name} {result:?}"
        );
    }
    stalls.arriving["X"].open();
    for (name, handle) in [("X", x), ("TB", tb), ("TA", ta)] {
        let result = joined(name, handle).unwrap();
        assert!(
            matches!(result, Err(Error::Poisoned)),
            "{kind}: {name} {result:?}"
        );
    }
    assert!(matches!(put(&main, 7, 7), Err(Error::Poisoned)), "{kind}");
    drop((main, branches));
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(contents(&db).get(&1), Some(&1), "{kind}");
    for (name, keys) in [("a", &[4][..]), ("b", &[2, 3]), ("c", &[5, 6])] {
        let seen = contents(&db.branch(name).unwrap());
        for key in keys {
            assert_eq!(seen.get(key), None, "{kind}: {name} kept {key}");
        }
    }
    db.verify().unwrap();
}

#[test]
fn a_group_another_branch_sealed_strands_no_one_when_its_frame_write_fails() {
    let _turn = serial();
    for (kind, options) in coalesced_kinds() {
        check_group_sealed_for_another_branch(kind, options, false);
    }
}

#[test]
fn a_group_another_branch_sealed_strands_no_one_when_its_sealer_dies_writing_it() {
    let _turn = serial();
    quiet_ferro_panics();
    for (kind, options) in coalesced_kinds() {
        check_group_sealed_for_another_branch(kind, options, true);
    }
}
