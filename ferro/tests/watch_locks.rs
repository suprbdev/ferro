mod common;

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use ferro::db::Hook;
use ferro::{Db, Keep, Map, Options, Reclaim};

type Root = Map<u64, u64>;

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const MANUAL: Reclaim = Reclaim { every_bytes: 0 };

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

static SYNC_ENTERED: AtomicBool = AtomicBool::new(false);
static SYNC_RELEASED: AtomicBool = AtomicBool::new(false);
static SERIAL: Mutex<()> = Mutex::new(());

thread_local! {
    static SYNC_HELD: Cell<bool> = const { Cell::new(false) };
}

#[unsafe(no_mangle)]
pub extern "C" fn fdatasync(fd: c_int) -> c_int {
    if SYNC_HELD.with(Cell::get) {
        SYNC_ENTERED.store(true, Ordering::SeqCst);
        while !SYNC_RELEASED.load(Ordering::SeqCst) {
            thread::yield_now();
        }
    }
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fdatasync".as_ptr()) };
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    real(fd)
}

fn serial() -> MutexGuard<'static, ()> {
    let turn = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    SYNC_ENTERED.store(false, Ordering::SeqCst);
    SYNC_RELEASED.store(false, Ordering::SeqCst);
    turn
}

fn put(db: &Db<Root>, key: u64, value: u64) {
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
}

fn wait_for(flag: &AtomicBool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !flag.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "{what} never happened");
        thread::sleep(Duration::from_millis(1));
    }
}

fn joined<R>(handle: JoinHandle<R>, what: &str) -> R {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !handle.is_finished() {
        assert!(Instant::now() < deadline, "{what} deadlocked");
        thread::sleep(Duration::from_millis(1));
    }
    handle.join().unwrap()
}

fn a_test_commits_while(what: &str, during: fn(&Db<Root>)) -> bool {
    let _turn = serial();
    let dir = tempfile::tempdir().unwrap();
    let synced = Arc::new(AtomicBool::new(false));
    let hook = {
        let synced = synced.clone();
        move |step: &'static str| {
            if step == "synced" {
                synced.store(true, Ordering::SeqCst);
            }
        }
    };
    let db: Db<Root> = Options::new()
        .coalesce(true)
        .reclaim(MANUAL)
        .hook(Hook(Arc::new(hook)))
        .open(dir.path().join("watch.ferro"))
        .unwrap();
    put(&db, 0, 0);
    let watched = db.fork("watched").unwrap();
    db.fork("doomed").unwrap();
    let fired = Arc::new(AtomicUsize::new(0));
    let committed_on: Arc<Mutex<Option<ThreadId>>> = Arc::default();
    let watcher = {
        let (db, fired, committed_on) = (db.clone(), fired.clone(), committed_on.clone());
        watched.watch_when(move |snapshot| {
            if snapshot.contains_key(&1) && fired.fetch_add(1, Ordering::SeqCst) == 0 {
                *committed_on.lock().unwrap() = Some(thread::current().id());
                put(&db, 9, 9);
            }
            snapshot.contains_key(&1)
        })
    };
    let parked_thread = thread::spawn(move || watcher.recv().map(|snapshot| snapshot.version()));
    thread::sleep(Duration::from_millis(30));
    let leader = {
        let db = db.clone();
        thread::spawn(move || {
            SYNC_HELD.with(|held| held.set(true));
            put(&db, 1, 1);
            SYNC_HELD.with(|held| held.set(false));
        })
    };
    wait_for(&SYNC_ENTERED, "the leader's sync");
    let member = {
        let watched = watched.clone();
        thread::spawn(move || put(&watched, 1, 1))
    };
    thread::sleep(Duration::from_millis(30));
    let runner = {
        let db = db.clone();
        thread::spawn(move || during(&db))
    };
    if what == "compact" {
        wait_for(&synced, "the cycle's copy");
    }
    thread::sleep(Duration::from_millis(50));
    SYNC_RELEASED.store(true, Ordering::SeqCst);
    let runner_id = runner.thread().id();
    joined(runner, what);
    joined(member, "the member's commit");
    joined(leader, "the leader's commit");
    assert_eq!(joined(parked_thread, "the watcher").unwrap(), 2);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !db.read().contains_key(&9) {
        assert!(Instant::now() < deadline, "the test's own commit never landed");
        thread::sleep(Duration::from_millis(1));
    }
    assert!(fired.load(Ordering::SeqCst) >= 1);
    assert_eq!(watched.version(), 2);
    db.verify().unwrap();
    let committed_on = *committed_on.lock().unwrap();
    committed_on == Some(runner_id)
}

#[test]
fn a_test_may_commit_while_drop_branch_leads_its_batch() {
    let led = (0..8).any(|_| {
        a_test_commits_while("drop_branch", |db| db.drop_branch("doomed").unwrap())
    });
    assert!(led, "drop_branch never led the batch that ran the test");
}

#[test]
fn a_test_may_commit_while_a_cycle_leads_its_batch() {
    let led = (0..8).any(|_| {
        a_test_commits_while("compact", |db| {
            db.compact(Keep::All).unwrap();
        })
    });
    assert!(led, "the cycle never led the batch that ran the test");
}
