#![cfg(unix)]

mod common;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::quiet_ferro_panics;
use ferro::db::Hook;
use ferro::{Db, Durability, Error, Map, Options, Reclaim};
use tempfile::TempDir;

type Root = Map<u64, u64>;
type Text = Map<u64, String>;
type Kind = (&'static str, fn() -> Options);

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const EIO: c_int = 5;
const ENOSPC: c_int = 28;
const PATIENCE: Duration = Duration::from_secs(30);
const MANUAL: Reclaim = Reclaim { every_bytes: 0 };

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

thread_local! {
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
        unsafe { *__errno_location() = EIO };
        return -1;
    }
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fdatasync".as_ptr()) };
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    real(fd)
}

fn is_frame(buf: *const c_void, count: usize) -> bool {
    count >= 8 && unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), 8) } == b"FERROFRM"
}

#[unsafe(no_mangle)]
pub extern "C" fn pwrite64(fd: c_int, buf: *const c_void, count: usize, offset: i64) -> isize {
    if is_frame(buf, count) {
        if FRAMES_FAIL.with(Cell::get) {
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
    let symbol = unsafe { dlsym(RTLD_NEXT, c"pwrite64".as_ptr()) };
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
    ] {
        flag.store(false, Ordering::SeqCst);
    }
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

fn kinds() -> [Kind; 2] {
    [("fsync", fsync), ("buffered", buffered)]
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
    fail_at_landing: Mutex<Vec<&'static str>>,
    panic_at_landing: Mutex<Vec<&'static str>>,
    hold_write: Mutex<Vec<&'static str>>,
    passed: AtomicUsize,
}

fn stalls(names: &[&'static str]) -> Arc<Stalls> {
    let mut stalls = Stalls::default();
    for name in names {
        stalls.landing.insert(name, Gate::default());
        stalls.landed.insert(name, Gate::default());
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
    let dir = dir();
    let path = dir.path().join("checkpoint.ferro");
    let stalls = stalls(&["queued"]);
    stalls.landing["queued"].open();
    let options = buffered().checkpoint_bytes(1).hook(hook(&stalls));
    let (db, branches) = seeded(options, &path, &["side"]);
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
    assert!(SYNC_FAILED.load(Ordering::SeqCst));
    stalls.landed["queued"].open();
    let queued = joined("the queued writer", queued).unwrap();
    assert!(
        matches!(queued, Err(Error::Poisoned)),
        "a commit queued when its checkpoint's fdatasync failed was acknowledged: {queued:?}"
    );
    assert!(matches!(put(&db, 4, 4), Err(Error::Poisoned)));
    drop((db, branches));
    let db: Db<Root> = buffered().open(&path).unwrap();
    assert_eq!(contents(&db).get(&2), Some(&2));
    db.verify().unwrap();
}

fn check_failed_landing(kind: &str, options: fn() -> Options, panics: bool) {
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
    wait_until("the frames ahead to land", || {
        stalls.landed["ahead-1"].reached() && stalls.landed["ahead-2"].reached()
    });
    stalls.landing["failing"].open();
    let failed = joined("the failing writer", failing);
    match (panics, &failed) {
        (true, Err(_)) | (false, Ok(Err(Error::Io(_)))) => {}
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
    assert!(
        matches!(before, Ok(3) | Err(Error::Poisoned)),
        "{kind}: {before:?}"
    );
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
        match at {
            0 if before.is_ok() => assert_eq!(value, Some(2), "{kind}: b0"),
            0 => assert!(matches!(value, Some(1 | 2)), "{kind}: b0"),
            _ => assert_eq!(value, Some(1), "{kind}: b{at}"),
        }
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
    for (kind, options) in kinds() {
        check_failed_landing(kind, options, false);
    }
}

#[test]
fn a_writer_that_dies_between_placing_and_writing_strands_no_one() {
    let _turn = serial();
    quiet_ferro_panics();
    for (kind, options) in kinds() {
        check_failed_landing(kind, options, true);
    }
}

#[test]
fn a_write_held_inside_pwrite_holds_back_every_sync_and_superblock() {
    let _turn = serial();
    for (kind, options) in kinds() {
        let dir = dir();
        let path = dir.path().join("held.ferro");
        let stalls = stalls(&["held"]);
        stalls.landing["held"].open();
        stalls.landed["held"].open();
        stalls.hold_write.lock().unwrap().push("held");
        let (db, branches) = seeded(options().hook(hook(&stalls)), &path, &["b0", "b1", "b2"]);
        let held = writer(&branches[0], "held", 1, 1);
        wait_until("the held write", || WRITE_ENTERED.load(Ordering::SeqCst));
        let size = db.size_on_disk();
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
            db.size_on_disk(),
            size,
            "{kind}: a superblock moved past a held frame"
        );
        let reader: Db<Root> = Options::new().read_only().open(&path).unwrap();
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
