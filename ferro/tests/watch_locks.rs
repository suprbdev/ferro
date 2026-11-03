mod common;

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ferro::db::Hook;
use ferro::{Db, Keep, Map, Options, Reclaim};

type Root = Map<u64, u64>;

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const MANUAL: Reclaim = Reclaim { every_bytes: 0 };

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

#[derive(Default)]
struct Gate {
    entered: AtomicBool,
    released: AtomicBool,
}

thread_local! {
    static ARMED: RefCell<Option<Arc<Gate>>> = const { RefCell::new(None) };
}

fn arm(gate: &Arc<Gate>) {
    ARMED.with(|armed| *armed.borrow_mut() = Some(gate.clone()));
}

#[unsafe(no_mangle)]
pub extern "C" fn fdatasync(fd: c_int) -> c_int {
    let gate = ARMED.try_with(|armed| armed.borrow_mut().take()).ok().flatten();
    if let Some(gate) = gate {
        gate.entered.store(true, Ordering::SeqCst);
        while !gate.released.load(Ordering::SeqCst) {
            thread::yield_now();
        }
    }
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fdatasync".as_ptr()) };
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    real(fd)
}

struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn put(db: &Db<Root>, key: u64, value: u64) {
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
}

fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        thread::sleep(Duration::from_millis(1));
    }
}

fn joined<R>(handle: JoinHandle<R>, what: &str) -> R {
    eventually(&format!("{what} finishing"), || handle.is_finished());
    handle.join().unwrap()
}

#[derive(Clone, Copy, PartialEq)]
enum Armed {
    AtStart,
    AfterCopy,
}

fn a_test_commits_while(what: &str, armed: Armed, during: fn(&Db<Root>)) -> Option<String> {
    let dir = tempfile::tempdir().unwrap();
    let held = Arc::new(Gate::default());
    let hook = {
        let held = held.clone();
        move |step: &'static str| {
            if armed == Armed::AfterCopy && step == "synced" {
                arm(&held);
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
    let committing = Arc::new(AtomicBool::new(false));
    let ran_on: Arc<Mutex<Option<String>>> = Arc::default();
    let watcher = {
        let (db, held) = (db.clone(), held.clone());
        let (fired, committing, ran_on) = (fired.clone(), committing.clone(), ran_on.clone());
        watched.watch_when(move |snapshot| {
            if snapshot.contains_key(&1) && fired.fetch_add(1, Ordering::SeqCst) == 0 {
                *ran_on.lock().unwrap() = thread::current().name().map(str::to_string);
                eventually("the runner's sync under tip", || {
                    held.entered.load(Ordering::SeqCst)
                });
                committing.store(true, Ordering::SeqCst);
                put(&db, 9, 9);
            }
            snapshot.contains_key(&1)
        })
    };
    let woken = Arc::new(Flag(AtomicBool::new(false)));
    let waker = Waker::from(woken.clone());
    let mut future = watcher.changed();
    let mut poll = || Pin::new(&mut future).poll(&mut Context::from_waker(&waker));
    assert!(poll().is_pending());
    put(&watched, 1, 1);
    eventually("the test firing", || fired.load(Ordering::SeqCst) > 0);
    let runner = {
        let (db, held) = (db.clone(), held.clone());
        thread::spawn(move || {
            if armed == Armed::AtStart {
                arm(&held);
            }
            during(&db);
        })
    };
    eventually("the test's own commit", || committing.load(Ordering::SeqCst));
    held.released.store(true, Ordering::SeqCst);
    joined(runner, what);
    eventually("the watcher's wake", || woken.0.load(Ordering::SeqCst));
    let Poll::Ready(Ok(snapshot)) = poll() else {
        panic!("the woken watcher found nothing");
    };
    assert!(snapshot.contains_key(&1));
    eventually("the test's commit landing", || db.read().contains_key(&9));
    assert_eq!(watched.version(), 2);
    db.verify().unwrap();
    ran_on.lock().unwrap().take()
}

#[test]
fn a_test_may_commit_while_drop_branch_holds_tip() {
    let ran_on = a_test_commits_while("drop_branch", Armed::AtStart, |db| {
        db.drop_branch("doomed").unwrap()
    });
    assert_eq!(ran_on.as_deref(), Some("ferro-watch"));
}

#[test]
fn a_test_may_commit_while_a_cycle_holds_tip() {
    let ran_on = a_test_commits_while("compact", Armed::AfterCopy, |db| {
        db.compact(Keep::All).unwrap();
    });
    assert_eq!(ran_on.as_deref(), Some("ferro-watch"));
}
