#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void};
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use ferro::{Db, Error, Map, Options, Reclaim};
use tempfile::TempDir;

type Root = Map<u64, u64>;

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const MANUAL: Reclaim = Reclaim::off();
const PATIENCE: Duration = Duration::from_secs(20);

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

#[unsafe(no_mangle)]
pub extern "C" fn fdatasync(fd: c_int) -> c_int {
    let gate = ARMED
        .try_with(|armed| armed.borrow_mut().take())
        .ok()
        .flatten();
    if let Some(gate) = gate {
        gate.entered.store(true, Ordering::SeqCst);
        while !gate.released.load(Ordering::SeqCst) {
            thread::yield_now();
        }
    }
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"fdatasync"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fdatasync".as_ptr()) };
    // SAFETY: glibc defines `fdatasync`, so `symbol` is the non-null address of that function, and
    // its C prototype `int fdatasync(int)` matches this function pointer type.
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    real(fd)
}

fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn joined<R>(what: &str, handle: JoinHandle<R>) -> R {
    let deadline = Instant::now() + PATIENCE;
    while !handle.is_finished() {
        assert!(Instant::now() < deadline, "{what} deadlocked");
        thread::sleep(Duration::from_millis(1));
    }
    handle.join().unwrap()
}

fn put(db: &Db<Root>, key: u64) {
    db.update(|root| {
        root.insert(key, key);
    })
    .unwrap();
}

fn value(db: &Db<Root>, key: u64) -> Option<u64> {
    db.read().get(&key).map(|value| *value)
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

fn flag() -> (Arc<Flag>, Waker) {
    let flag = Arc::new(Flag {
        thread: thread::current(),
        woken: AtomicBool::new(false),
    });
    let waker = Waker::from(flag.clone());
    (flag, waker)
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let (flag, waker) = flag();
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        while !flag.woken.swap(false, Ordering::SeqCst) {
            thread::park();
        }
    }
}

fn coalesced(dir: &TempDir) -> Db<Root> {
    let main: Db<Root> = Options::new()
        .reclaim(MANUAL)
        .coalesce(true)
        .open(dir.path().join("held.ferro"))
        .unwrap();
    put(&main, 0);
    main
}

fn hold_a_sync(db: &Db<Root>) -> (Arc<Gate>, JoinHandle<()>) {
    let gate = Arc::new(Gate::default());
    let holder = {
        let (db, gate) = (db.clone(), gate.clone());
        thread::spawn(move || {
            ARMED.with(|armed| *armed.borrow_mut() = Some(gate));
            put(&db, 99);
        })
    };
    wait_until("a sync to start", || gate.entered.load(Ordering::SeqCst));
    (gate, holder)
}

#[test]
fn a_coalesced_closure_writing_another_branch_cannot_deadlock() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let main = coalesced(&dir);
    let b = main.fork("b").unwrap();
    let c = main.fork("c").unwrap();
    let d = main.fork("d").unwrap();
    let (gate, holder) = hold_a_sync(&d);
    let first_ran = Arc::new(AtomicBool::new(false));
    let first = {
        let (b, ran) = (b.clone(), first_ran.clone());
        thread::spawn(move || {
            b.update(|root| {
                root.insert(1, 1);
                ran.store(true, Ordering::SeqCst);
            })
        })
    };
    wait_until("the first closure", || first_ran.load(Ordering::SeqCst));
    thread::sleep(Duration::from_millis(50));
    let nesting = Arc::new(AtomicBool::new(false));
    let second = {
        let (b, c, nesting) = (b.clone(), c.clone(), nesting.clone());
        thread::spawn(move || {
            b.update(|root| {
                root.insert(2, 2);
                nesting.store(true, Ordering::SeqCst);
                c.update(|root| {
                    root.insert(3, 3);
                })
            })
        })
    };
    wait_until("the nested write", || nesting.load(Ordering::SeqCst));
    thread::sleep(Duration::from_millis(50));
    gate.released.store(true, Ordering::SeqCst);
    joined("the sync holder", holder);
    let nested = joined("the nesting closure", second).unwrap();
    assert!(matches!(nested, Err(Error::Reentrant)), "{nested:?}");
    joined("the first closure", first).unwrap();
    assert_eq!(
        (value(&b, 1), value(&b, 2), value(&c, 3)),
        (Some(1), Some(2), None)
    );
    main.verify().unwrap();
}

#[test]
fn awaiting_a_coalesced_write_inside_a_closure_cannot_deadlock() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let main = coalesced(&dir);
    let b = main.fork("b").unwrap();
    let d = main.fork("d").unwrap();
    let (gate, holder) = hold_a_sync(&d);
    let mut started: Pin<Box<dyn Future<Output = ferro::Result<()>> + Send>> = {
        let b = b.clone();
        Box::pin(async move {
            b.update_async(|root| {
                root.insert(1, 1);
            })
            .await
        })
    };
    let (_, waker) = flag();
    let first = started.as_mut().poll(&mut Context::from_waker(&waker));
    assert!(
        first.is_pending(),
        "the write was acknowledged during a held sync"
    );
    let awaiting = {
        let b = b.clone();
        thread::spawn(move || {
            b.update(|root| {
                root.insert(2, 2);
                block_on(started)
            })
        })
    };
    thread::sleep(Duration::from_millis(50));
    gate.released.store(true, Ordering::SeqCst);
    joined("the sync holder", holder);
    let awaited = joined("the awaiting closure", awaiting).unwrap();
    assert!(matches!(awaited, Err(Error::Reentrant)), "{awaited:?}");
    wait_until("the started write to land", || value(&b, 1) == Some(1));
    assert_eq!(value(&b, 2), Some(2));
    main.verify().unwrap();
}
