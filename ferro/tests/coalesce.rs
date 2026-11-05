#![cfg(unix)]

mod common;

use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::{c_char, c_int, c_void};
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::{fast, quiet_ferro_panics};
use ferro::codec::{Decoder, Encoder};
use ferro::db::Clock;
use ferro::{Committed, Data, Db, Durability, Error, Keep, Map, Options, Reclaim};
use tempfile::TempDir;

type Root = Map<u64, u64>;
type Images = Vec<(u64, Vec<u8>)>;

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const EIO: c_int = 5;

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn __errno_location() -> *mut c_int;
}

struct PowerCut {
    path: PathBuf,
    acked: Arc<AtomicU64>,
    syncing: Option<Vec<u8>>,
    durable: Option<Vec<u8>>,
    images: Vec<(u64, Vec<u8>)>,
}

impl PowerCut {
    fn enter(&mut self) {
        let floor = self.acked.load(Ordering::SeqCst);
        if let Some(image) = self.durable.take() {
            self.images.push((floor, image));
        }
        self.syncing = fs::read(&self.path).ok();
    }

    fn leave(&mut self) {
        self.durable = self.syncing.take();
    }
}

struct Hook {
    held: bool,
    fail: bool,
    calls: u64,
    inside: u64,
    power: Option<PowerCut>,
}

static HOOK: Mutex<Hook> = Mutex::new(Hook {
    held: false,
    fail: false,
    calls: 0,
    inside: 0,
    power: None,
});
static TURN: Condvar = Condvar::new();
static SERIAL: Mutex<()> = Mutex::new(());

fn hook() -> MutexGuard<'static, Hook> {
    HOOK.lock().unwrap_or_else(|error| error.into_inner())
}

fn real_fdatasync(fd: c_int) -> c_int {
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"fdatasync"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fdatasync".as_ptr()) };
    // SAFETY: glibc defines `fdatasync`, so `symbol` is the non-null address of that function, and
    // its C prototype `int fdatasync(int)` matches this function pointer type.
    let real: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(symbol) };
    real(fd)
}

#[unsafe(no_mangle)]
pub extern "C" fn fdatasync(fd: c_int) -> c_int {
    let mut state = hook();
    state.calls += 1;
    state.inside += 1;
    TURN.notify_all();
    while state.held {
        state = TURN.wait(state).unwrap_or_else(|error| error.into_inner());
    }
    state.inside -= 1;
    if state.fail {
        // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's `errno`,
        // valid for writes for as long as the thread runs.
        unsafe { *__errno_location() = EIO };
        return -1;
    }
    if let Some(power) = &mut state.power {
        power.enter();
    }
    drop(state);
    let result = real_fdatasync(fd);
    let mut state = hook();
    if result == 0
        && let Some(power) = &mut state.power
    {
        power.leave();
    }
    result
}

fn serial() -> MutexGuard<'static, ()> {
    let turn = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let mut state = hook();
    state.held = false;
    state.fail = false;
    state.calls = 0;
    state.power = None;
    turn
}

fn hold() {
    hook().held = true;
}

fn release(fail: bool) {
    let mut state = hook();
    state.fail = fail;
    state.held = false;
    TURN.notify_all();
}

fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn settle_queue() {
    thread::sleep(Duration::from_millis(30));
}

fn on_disk() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn coalesced() -> Options {
    Options::new().coalesce(true)
}

fn database() -> (TempDir, PathBuf, Db<Root>) {
    let dir = on_disk();
    let path = dir.path().join("coalesce.ferro");
    let db = coalesced().open::<Root>(&path).unwrap();
    db.update(|root| {
        root.insert(0, 1);
    })
    .unwrap();
    (dir, path, db)
}

fn counter(root: &Root) -> u64 {
    root.get(&0).map_or(0, |value| *value)
}

fn increment(root: &mut Root) -> u64 {
    let next = counter(root) + 1;
    root.insert(0, next);
    next
}

struct Ran(Arc<AtomicUsize>);

impl Ran {
    fn new() -> Ran {
        Ran(Arc::new(AtomicUsize::new(0)))
    }

    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    fn reach(&self, count: usize) {
        wait_until("closures to run", || self.count() >= count);
    }
}

fn bump(db: &Db<Root>, ran: &Ran, name: String) -> JoinHandle<ferro::Result<Committed<u64>>> {
    let (db, ran) = (db.clone(), ran.0.clone());
    thread::spawn(move || {
        db.commit_with(|root| {
            let next = increment(root);
            ran.fetch_add(1, Ordering::SeqCst);
            (name, next)
        })
    })
}

fn stall_leader(db: &Db<Root>, ran: &Ran) -> JoinHandle<ferro::Result<Committed<u64>>> {
    hold();
    let leader = bump(db, ran, "leader".to_string());
    wait_until("the leader to enter its sync", || hook().inside == 1);
    leader
}

#[test]
fn closures_queued_behind_a_sync_share_one_version() {
    let _turn = serial();
    let (_dir, path, db) = database();
    let events = db.watch();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let followers: Vec<_> = (0..5).map(|i| bump(&db, &ran, format!("m{i}"))).collect();
    ran.reach(6);
    settle_queue();
    assert_eq!(db.version(), 1);
    assert_eq!(counter(&db.read()), 1);
    assert!(followers.iter().all(|follower| !follower.is_finished()));
    let calls = hook().calls;
    release(false);
    let first = leader.join().unwrap().unwrap();
    assert_eq!((first.version, first.value), (2, 2));
    let done: Vec<_> = followers
        .into_iter()
        .map(|f| f.join().unwrap().unwrap())
        .collect();
    assert!(done.iter().all(|done| done.version == 3 && done.changed));
    let mut values: Vec<u64> = done.iter().map(|done| done.value).collect();
    values.sort_unstable();
    assert_eq!(values, vec![3, 4, 5, 6, 7]);
    assert_eq!(hook().calls, calls + 1, "the group took more than one sync");
    assert_eq!(db.version(), 3);
    assert_eq!(counter(&db.read()), 7);
    let log = db.log();
    assert_eq!(log.len(), 3);
    let commit = log.get(&3).unwrap();
    let mut messages: Vec<&str> = commit.messages().collect();
    messages.sort_unstable();
    assert_eq!(messages, vec!["m0", "m1", "m2", "m3", "m4"]);
    assert_eq!(
        events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .version(),
        3
    );
    assert!(events.try_recv().is_err());
    drop((db, log));
    let reopened = Options::new().open::<Root>(&path).unwrap();
    assert_eq!(reopened.version(), 3);
    assert_eq!(counter(&reopened.read()), 7);
    reopened.verify().unwrap();
}

#[test]
fn multi_line_messages_stay_whole_alone_and_in_a_group() {
    let _turn = serial();
    let (_dir, path, db) = database();
    db.commit("alone\nsecond line", increment).unwrap();
    assert_eq!(db.version(), 2);
    let alone: Vec<String> = db.read().commit().messages().map(str::to_string).collect();
    assert_eq!(alone, vec!["alone\nsecond line".to_string()]);
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let names = ["one\ntwo", "three", "four\n\nfive\n", "\nsix"];
    let followers: Vec<_> = names
        .iter()
        .map(|name| bump(&db, &ran, name.to_string()))
        .collect();
    ran.reach(names.len() + 1);
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    for follower in followers {
        assert_eq!(follower.join().unwrap().unwrap().version, 4);
    }
    let read = |db: &Db<Root>| {
        let log = db.log();
        let mut group: Vec<String> = log
            .get(&4)
            .unwrap()
            .messages()
            .map(str::to_string)
            .collect();
        group.sort_unstable();
        let alone: Vec<String> = log
            .get(&2)
            .unwrap()
            .messages()
            .map(str::to_string)
            .collect();
        (alone, group)
    };
    let mut expected: Vec<String> = names.iter().map(|name| name.to_string()).collect();
    expected.sort_unstable();
    assert_eq!(
        read(&db),
        (vec!["alone\nsecond line".to_string()], expected.clone())
    );
    let message = db.log().get(&4).unwrap().message.clone();
    assert_eq!(message.matches('\n').count(), 8);
    drop(db);
    let reopened = Options::new().open::<Root>(&path).unwrap();
    assert_eq!(
        read(&reopened),
        (vec!["alone\nsecond line".to_string()], expected)
    );
}

#[test]
fn without_the_option_closures_keep_a_version_each() {
    let _turn = serial();
    let dir = on_disk();
    let db = Options::new()
        .open::<Root>(dir.path().join("plain.ferro"))
        .unwrap();
    db.update(|root| {
        root.insert(0, 1);
    })
    .unwrap();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let followers: Vec<_> = (0..4).map(|i| bump(&db, &ran, format!("m{i}"))).collect();
    ran.reach(5);
    release(false);
    leader.join().unwrap().unwrap();
    let mut versions: Vec<u64> = followers
        .into_iter()
        .map(|follower| follower.join().unwrap().unwrap().version)
        .collect();
    versions.sort_unstable();
    assert_eq!(versions, vec![3, 4, 5, 6]);
}

#[test]
fn an_error_rolls_back_only_its_own_closure_and_waits_for_the_group() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let before = bump(&db, &ran, "before".to_string());
    ran.reach(2);
    let refuser = {
        let (db, ran) = (db.clone(), ran.0.clone());
        thread::spawn(move || {
            db.try_update(|root| -> Result<(), Error> {
                let seen = counter(root);
                root.insert(0, 1000);
                root.insert(50, 50);
                ran.fetch_add(1, Ordering::SeqCst);
                Err(Error::Decode(format!("saw {seen}")))
            })
        })
    };
    ran.reach(3);
    let after = bump(&db, &ran, "after".to_string());
    ran.reach(4);
    settle_queue();
    assert!(!refuser.is_finished());
    release(false);
    leader.join().unwrap().unwrap();
    assert_eq!(before.join().unwrap().unwrap().value, 3);
    assert_eq!(after.join().unwrap().unwrap().value, 4);
    match refuser.join().unwrap() {
        Err(Error::Decode(message)) => assert_eq!(message, "saw 3"),
        other => panic!("unexpected {other:?}"),
    }
    let tip = db.read();
    assert_eq!(counter(&tip), 4);
    assert!(!tip.contains_key(&50));
    let messages: Vec<&str> = tip.commit().messages().collect();
    assert_eq!(messages, vec!["before", "after"]);
}

#[test]
fn a_panic_rolls_back_only_its_own_closure() {
    quiet_ferro_panics();
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let before = bump(&db, &ran, "before".to_string());
    ran.reach(2);
    let panicker = {
        let (db, ran) = (db.clone(), ran.0.clone());
        thread::spawn(move || {
            db.update(|root| {
                root.insert(0, 1000);
                root.insert(50, 50);
                ran.fetch_add(1, Ordering::SeqCst);
                panic!("ferro: closure gave up");
            })
        })
    };
    ran.reach(3);
    let error = panicker.join().unwrap_err();
    assert_eq!(common::panic_text(&*error), "ferro: closure gave up");
    let after = bump(&db, &ran, "after".to_string());
    ran.reach(4);
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    assert_eq!(before.join().unwrap().unwrap().value, 3);
    assert_eq!(after.join().unwrap().unwrap().value, 4);
    let tip = db.read();
    assert_eq!(counter(&tip), 4);
    assert!(!tip.contains_key(&50));
}

#[test]
fn a_panic_in_the_last_closure_still_writes_the_group() {
    quiet_ferro_panics();
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let member = bump(&db, &ran, "member".to_string());
    ran.reach(2);
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        db.update(|root| {
            root.insert(50, 50);
            panic!("ferro: closure gave up");
        })
    }));
    assert!(outcome.is_err());
    assert_eq!(member.join().unwrap().unwrap().value, 3);
    assert!(!db.read().contains_key(&50));
    let later = db.update(increment).unwrap();
    assert_eq!(later, 4);
}

#[test]
fn a_panic_with_nobody_else_writes_nothing() {
    quiet_ferro_panics();
    let _turn = serial();
    let (_dir, _path, db) = database();
    let size = db.size_on_disk();
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        db.update(|root| {
            root.insert(50, 50);
            panic!("ferro: closure gave up");
        })
    }));
    assert!(outcome.is_err());
    assert_eq!(db.size_on_disk(), size);
    assert_eq!(db.version(), 1);
    assert_eq!(db.update(increment).unwrap(), 2);
}

#[test]
fn a_panic_that_seals_the_group_hands_its_frame_to_a_member() {
    quiet_ferro_panics();
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let member = bump(&db, &ran, "member".to_string());
    ran.reach(2);
    settle_queue();
    let gate = Arc::new(AtomicBool::new(false));
    let holder = {
        let (db, gate, ran) = (db.clone(), gate.clone(), ran.0.clone());
        thread::spawn(move || {
            db.update(|root| {
                increment(root);
                ran.fetch_add(1, Ordering::SeqCst);
                while !gate.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
            })
        })
    };
    ran.reach(3);
    let panicker = {
        let db = db.clone();
        thread::spawn(move || {
            db.update(|root| {
                root.insert(50, 50);
                panic!("ferro: closure gave up");
            })
        })
    };
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    settle_queue();
    gate.store(true, Ordering::SeqCst);
    assert!(panicker.join().is_err());
    holder.join().unwrap().unwrap();
    assert_eq!(member.join().unwrap().unwrap().value, 3);
    assert_eq!(counter(&db.read()), 4);
    assert!(!db.read().contains_key(&50));
}

#[test]
fn a_group_of_errors_writes_nothing_and_still_waits_for_what_it_saw() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let refusers: Vec<_> = (0..2)
        .map(|_| {
            let (db, ran) = (db.clone(), ran.0.clone());
            thread::spawn(move || {
                db.try_update(|root| -> Result<(), Error> {
                    root.insert(50, 50);
                    ran.fetch_add(1, Ordering::SeqCst);
                    Err(Error::Decode(format!("saw {}", counter(root))))
                })
            })
        })
        .collect();
    ran.reach(3);
    settle_queue();
    assert!(refusers.iter().all(|refuser| !refuser.is_finished()));
    release(false);
    leader.join().unwrap().unwrap();
    for refuser in refusers {
        match refuser.join().unwrap() {
            Err(Error::Decode(message)) => assert_eq!(message, "saw 2"),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(db.version(), 2);
    assert!(!db.read().contains_key(&50));
}

#[test]
fn an_error_in_the_first_closure_leaves_the_next_a_clean_copy() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let refuser = {
        let (db, ran) = (db.clone(), ran.0.clone());
        thread::spawn(move || {
            db.try_update(|root| -> Result<(), Error> {
                root.insert(50, 50);
                ran.fetch_add(1, Ordering::SeqCst);
                Err(Error::Decode("no".to_string()))
            })
        })
    };
    ran.reach(2);
    let member = bump(&db, &ran, "member".to_string());
    ran.reach(3);
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    assert!(matches!(refuser.join().unwrap(), Err(Error::Decode(_))));
    assert_eq!(member.join().unwrap().unwrap().version, 3);
    assert!(!db.read().contains_key(&50));
}

#[test]
fn a_group_of_no_ops_writes_nothing_and_says_so() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let (db, ran) = (db.clone(), ran.0.clone());
            thread::spawn(move || {
                db.commit_with(|root| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    ("nothing", counter(root))
                })
            })
        })
        .collect();
    ran.reach(4);
    settle_queue();
    assert!(readers.iter().all(|reader| !reader.is_finished()));
    release(false);
    leader.join().unwrap().unwrap();
    for reader in readers {
        let done = reader.join().unwrap().unwrap();
        assert_eq!((done.value, done.version, done.changed), (2, 2, false));
    }
    assert_eq!(db.version(), 2);
    assert_eq!(db.log().len(), 2);
    assert_eq!(db.read().commit().message, "leader");
}

#[test]
fn a_group_of_no_ops_sealed_early_still_waits_for_the_tip_it_read() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let reader = {
        let (db, ran) = (db.clone(), ran.0.clone());
        thread::spawn(move || {
            db.commit_with(|root| {
                ran.fetch_add(1, Ordering::SeqCst);
                ("nothing", counter(root))
            })
        })
    };
    ran.reach(2);
    settle_queue();
    let forker = {
        let db = db.clone();
        thread::spawn(move || db.fork("side"))
    };
    settle_queue();
    assert!(
        !reader.is_finished(),
        "a no-op returned before the tip it read was durable"
    );
    release(false);
    leader.join().unwrap().unwrap();
    let done = reader.join().unwrap().unwrap();
    assert_eq!((done.value, done.version, done.changed), (2, 2, false));
    assert_eq!(forker.join().unwrap().unwrap().version(), 2);
}

#[test]
fn a_no_op_beside_a_real_change_shares_the_groups_version() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let idle = {
        let (db, ran) = (db.clone(), ran.0.clone());
        thread::spawn(move || {
            db.commit_with(|_| {
                ran.fetch_add(1, Ordering::SeqCst);
                ("idle", ())
            })
        })
    };
    ran.reach(2);
    let real = bump(&db, &ran, "real".to_string());
    ran.reach(3);
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    let (idle, real) = (idle.join().unwrap().unwrap(), real.join().unwrap().unwrap());
    assert_eq!(real.version, 3);
    assert_eq!((idle.version, idle.changed), (3, true));
}

#[test]
fn a_closure_alone_keeps_the_plain_path() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let size = db.size_on_disk();
    let done = db.commit_with(|root| ("one", increment(root))).unwrap();
    assert_eq!((done.version, done.value, done.changed), (2, 2, true));
    assert!(db.size_on_disk() > size);
    let done = db.commit_with(|_| ("none", ())).unwrap();
    assert_eq!((done.version, done.changed), (2, false));
    assert_eq!(db.read().commit().message, "one");
}

fn closer_then_follower(db: &Db<Root>, arrivals: &Arc<AtomicUsize>) -> (u64, u64) {
    let before = arrivals.load(Ordering::SeqCst);
    let holding = Arc::new(AtomicBool::new(false));
    let closer = {
        let (db, arrivals, holding) = (db.clone(), arrivals.clone(), holding.clone());
        thread::spawn(move || {
            db.commit_with(|root| {
                holding.store(true, Ordering::SeqCst);
                wait_until("the follower to arrive", || {
                    arrivals.load(Ordering::SeqCst) > before
                });
                ("closer", increment(root))
            })
        })
    };
    wait_until("the closer to hold the lock", || {
        holding.load(Ordering::SeqCst)
    });
    let follower = {
        let db = db.clone();
        thread::Builder::new()
            .name("follower".to_string())
            .spawn(move || db.commit_with(|root| ("follower", increment(root))))
            .unwrap()
    };
    let closer = closer.join().unwrap().unwrap();
    let follower = follower.join().unwrap().unwrap();
    (closer.version, follower.version)
}

#[test]
fn a_closure_stops_waiting_for_followers_that_are_slow_to_reach_the_lock() {
    let _turn = serial();
    let dir = on_disk();
    let arrivals = Arc::new(AtomicUsize::new(0));
    let slow = Arc::new(AtomicBool::new(true));
    let hook = {
        let (arrivals, slow) = (arrivals.clone(), slow.clone());
        move |step: &'static str| {
            if step == "arriving" && thread::current().name() == Some("follower") {
                arrivals.fetch_add(1, Ordering::SeqCst);
                if slow.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
    };
    let db = coalesced()
        .durability(Durability::Buffered)
        .hook(ferro::db::Hook(Arc::new(hook)))
        .open::<Root>(dir.path().join("slow.ferro"))
        .unwrap();
    db.update(|root| {
        root.insert(0, 1);
    })
    .unwrap();
    assert_eq!(closer_then_follower(&db, &arrivals), (2, 2));
    slow.store(false, Ordering::SeqCst);
    assert_eq!(closer_then_follower(&db, &arrivals), (3, 4));
    assert_eq!(counter(&db.read()), 5);
    let log = db.log();
    assert_eq!(log.get(&2).unwrap().messages().count(), 2);
    assert_eq!(
        log.get(&3).unwrap().messages().collect::<Vec<_>>(),
        vec!["closer"]
    );
}

#[test]
fn a_member_that_seals_a_newer_group_on_its_way_wakes_that_group() {
    let _turn = serial();
    let dir = on_disk();
    let armed = Arc::new(AtomicBool::new(false));
    let stalled = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(AtomicBool::new(false));
    let hook = {
        let (armed, stalled, gate) = (armed.clone(), stalled.clone(), gate.clone());
        move |step: &'static str| {
            if step == "arriving"
                && thread::current().name() == Some("member")
                && armed.swap(false, Ordering::SeqCst)
            {
                stalled.store(true, Ordering::SeqCst);
                while !gate.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    };
    let db = coalesced()
        .hook(ferro::db::Hook(Arc::new(hook)))
        .open::<Root>(dir.path().join("newer.ferro"))
        .unwrap();
    db.update(|root| {
        root.insert(0, 1);
    })
    .unwrap();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let member = {
        let (db, ran) = (db.clone(), ran.0.clone());
        thread::Builder::new()
            .name("member".to_string())
            .spawn(move || {
                db.commit_with(|root| {
                    let next = increment(root);
                    ran.fetch_add(1, Ordering::SeqCst);
                    ("member", next)
                })
            })
            .unwrap()
    };
    ran.reach(2);
    settle_queue();
    armed.store(true, Ordering::SeqCst);
    release(false);
    assert_eq!(leader.join().unwrap().unwrap().version, 2);
    wait_until("the member to set out to seal its group", || {
        stalled.load(Ordering::SeqCst)
    });
    let side = db.fork("side").unwrap();
    assert_eq!(side.version(), 3);
    let late = bump(&db, &ran, "late".to_string());
    ran.reach(3);
    settle_queue();
    gate.store(true, Ordering::SeqCst);
    assert_eq!(member.join().unwrap().unwrap().version, 3);
    wait_until("the newer group to be written", || late.is_finished());
    let late = late.join().unwrap().unwrap();
    assert_eq!((late.version, late.value), (4, 4));
    assert_eq!(counter(&db.read()), 4);
    let log = db.log();
    assert_eq!(
        log.get(&4).unwrap().messages().collect::<Vec<_>>(),
        vec!["late"]
    );
}

#[test]
fn a_failed_sync_fails_the_group_waiting_on_it_and_the_one_behind_it() {
    let _turn = serial();
    let (_dir, path, db) = database();
    let events = db.watch();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let followers: Vec<_> = (0..4).map(|i| bump(&db, &ran, format!("m{i}"))).collect();
    ran.reach(5);
    settle_queue();
    release(true);
    assert!(matches!(leader.join().unwrap(), Err(Error::Io(_))));
    for follower in followers {
        assert!(matches!(follower.join().unwrap(), Err(Error::Poisoned)));
    }
    assert_eq!(db.version(), 1);
    assert_eq!(counter(&db.read()), 1);
    assert!(events.try_recv().is_err());
    assert!(matches!(db.update(increment), Err(Error::Poisoned)));
    assert!(matches!(db.fork("late"), Err(Error::Poisoned)));
    hook().fail = false;
    drop(db);
    let reopened = Options::new().open::<Root>(&path).unwrap();
    reopened.verify().unwrap();
}

#[test]
fn a_failed_sync_reaches_frames_waiting_their_turn() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let members: Vec<_> = (0..300).map(|i| bump(&db, &ran, format!("m{i}"))).collect();
    ran.reach(301);
    settle_queue();
    release(true);
    assert!(leader.join().unwrap().is_err());
    for member in members {
        assert!(matches!(member.join().unwrap(), Err(Error::Poisoned)));
    }
    hook().fail = false;
}

#[derive(Clone, Debug, PartialEq)]
struct Bad(u8);

impl Data for Bad {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        match self.0 {
            1 => Err(Error::Decode("refused to encode".to_string())),
            2 => panic!("ferro: refused to encode"),
            _ => {
                w.u8(self.0);
                Ok(())
            }
        }
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Bad(r.u8()?))
    }
}

type Tricky = Map<u64, Bad>;

fn tricky_group(bad: u8) {
    quiet_ferro_panics();
    let _turn = serial();
    let dir = on_disk();
    let path = dir.path().join("tricky.ferro");
    let db = coalesced().open::<Tricky>(&path).unwrap();
    db.update(|root| {
        root.insert(0, Bad(0));
    })
    .unwrap();
    hold();
    let leader = {
        let db = db.clone();
        thread::spawn(move || {
            db.update(|root| {
                root.insert(1, Bad(0));
            })
        })
    };
    wait_until("the leader to enter its sync", || hook().inside == 1);
    let ran = Ran::new();
    let members: Vec<_> = [0u8, bad, 0]
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let (db, ran) = (db.clone(), ran.0.clone());
            thread::spawn(move || {
                db.update(|root| {
                    root.insert(10 + index as u64, Bad(value));
                    ran.fetch_add(1, Ordering::SeqCst);
                })
            })
        })
        .collect();
    ran.reach(3);
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    for member in members {
        match member.join().unwrap() {
            Err(Error::GroupFailed(_)) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(db.version(), 2);
    assert!(!db.read().contains_key(&10));
    db.update(|root| {
        root.insert(20, Bad(0));
    })
    .unwrap();
    assert_eq!(db.version(), 3);
    db.verify().unwrap();
    drop(db);
    let reopened = coalesced().open::<Tricky>(&path).unwrap();
    assert_eq!(reopened.version(), 3);
}

#[test]
fn an_encode_error_fails_the_group_and_leaves_the_database_usable() {
    tricky_group(1);
}

#[test]
fn an_encode_panic_fails_the_group_and_leaves_the_database_usable() {
    tricky_group(2);
}

#[test]
fn closures_run_on_their_own_threads_and_may_borrow_local_state() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let local = Rc::new(Cell::new(0u64));
    let borrowed = &local;
    let mine = thread::current().id();
    let ran_here = Cell::new(false);
    db.update(|root| {
        borrowed.set(increment(root));
        ran_here.set(thread::current().id() == mine);
    })
    .unwrap();
    assert_eq!(local.get(), 2);
    assert!(ran_here.get());
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let on_threads: Vec<_> = (0..3)
        .map(|_| {
            let (db, ran) = (db.clone(), ran.0.clone());
            thread::spawn(move || {
                let mine = thread::current().id();
                db.update(|root| {
                    increment(root);
                    ran.fetch_add(1, Ordering::SeqCst);
                    thread::current().id() == mine
                })
            })
        })
        .collect();
    ran.reach(4);
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    for handle in on_threads {
        assert!(handle.join().unwrap().unwrap());
    }
}

#[test]
fn restore_merge_fork_and_drop_flush_the_open_group_first() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let side = db.fork("side").unwrap();
    let doomed = db.fork("doomed").unwrap();
    let old = db.read();
    side.update(|root| {
        root.insert(5, 5);
    })
    .unwrap();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let member = bump(&db, &ran, "member".to_string());
    ran.reach(2);
    settle_queue();
    let forker = {
        let db = db.clone();
        thread::spawn(move || db.fork("late"))
    };
    wait_until("the fork to flush", || hook().inside == 1);
    settle_queue();
    let restorer = {
        let (db, old) = (db.clone(), old.clone());
        thread::spawn(move || db.restore(&old))
    };
    settle_queue();
    let merger = {
        let (db, side) = (db.clone(), side.clone());
        thread::spawn(move || {
            db.merge_with(&side, |root, _, theirs| {
                root.insert(5, *theirs.get(&5).unwrap());
                ("merge", ())
            })
        })
    };
    settle_queue();
    let dropper = {
        let db = db.clone();
        thread::spawn(move || db.drop_branch("doomed"))
    };
    settle_queue();
    drop(doomed);
    release(false);
    leader.join().unwrap().unwrap();
    assert_eq!(member.join().unwrap().unwrap().version, 3);
    let late = forker.join().unwrap().unwrap();
    assert_eq!(late.version(), 3);
    assert_eq!(counter(&late.read()), 3);
    let restored = restorer.join().unwrap().unwrap();
    assert!(restored.changed);
    let merged = merger.join().unwrap().unwrap();
    assert!(merged.changed);
    dropper.join().unwrap().unwrap();
    assert_eq!(db.branches(), vec!["late", "main", "side"]);
    let log = db.log();
    assert_eq!(
        log.get(&restored.version).unwrap().restored,
        Some(old.version())
    );
    assert!(log.len() >= 5);
    db.verify().unwrap();
}

#[test]
fn a_closure_on_another_branch_flushes_the_open_group() {
    let _turn = serial();
    let (_dir, path, db) = database();
    let side = db.fork("side").unwrap();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let on_main = bump(&db, &ran, "main".to_string());
    ran.reach(2);
    let on_side = bump(&side, &ran, "side".to_string());
    ran.reach(3);
    let more_main = bump(&db, &ran, "main again".to_string());
    ran.reach(4);
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    assert_eq!(on_main.join().unwrap().unwrap().version, 3);
    assert_eq!(on_side.join().unwrap().unwrap().value, 2);
    assert_eq!(more_main.join().unwrap().unwrap().version, 4);
    assert_eq!((db.version(), side.version()), (4, 2));
    assert_eq!((counter(&db.read()), counter(&side.read())), (4, 2));
    drop((db, side));
    let reopened = Options::new().open::<Root>(&path).unwrap();
    assert_eq!(reopened.version(), 4);
    assert_eq!(reopened.branch("side").unwrap().version(), 2);
    reopened.verify().unwrap();
}

#[test]
fn groups_left_open_on_many_branches_share_the_next_sync() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let sides: Vec<_> = (0..4)
        .map(|i| db.fork(&format!("side-{i}")).unwrap())
        .collect();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let members: Vec<_> = sides
        .iter()
        .enumerate()
        .map(|(i, side)| bump(side, &ran, format!("side {i}")))
        .collect();
    ran.reach(5);
    settle_queue();
    let before = hook().calls;
    release(false);
    leader.join().unwrap().unwrap();
    for member in members {
        assert_eq!(member.join().unwrap().unwrap().version, 2);
    }
    assert_eq!(
        hook().calls - before,
        1,
        "the open groups took more than one sync"
    );
    for side in &sides {
        assert_eq!((side.version(), counter(&side.read())), (2, 2));
    }
    db.verify().unwrap();
}

#[test]
fn a_dead_branch_cannot_strand_the_open_group() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let doomed = db.fork("doomed").unwrap();
    db.drop_branch("doomed").unwrap();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let member = bump(&db, &ran, "member".to_string());
    ran.reach(2);
    settle_queue();
    assert!(matches!(doomed.update(increment), Err(Error::NoBranch(_))));
    release(false);
    leader.join().unwrap().unwrap();
    assert_eq!(member.join().unwrap().unwrap().version, 3);
}

#[test]
fn a_dead_branch_that_others_waited_for_still_hands_on_the_group() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let doomed = db.fork("doomed").unwrap();
    db.drop_branch("doomed").unwrap();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let member = bump(&db, &ran, "member".to_string());
    ran.reach(2);
    settle_queue();
    let gate = Arc::new(AtomicBool::new(false));
    let holder = {
        let (db, gate, ran) = (db.clone(), gate.clone(), ran.0.clone());
        thread::spawn(move || {
            db.update(|root| {
                increment(root);
                ran.fetch_add(1, Ordering::SeqCst);
                while !gate.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
            })
        })
    };
    ran.reach(3);
    let refused = thread::spawn(move || doomed.update(increment));
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    settle_queue();
    gate.store(true, Ordering::SeqCst);
    assert!(matches!(refused.join().unwrap(), Err(Error::NoBranch(_))));
    holder.join().unwrap().unwrap();
    assert_eq!(member.join().unwrap().unwrap().value, 3);
    assert_eq!(counter(&db.read()), 4);
}

#[test]
fn a_full_group_stops_growing() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let members: Vec<_> = (0..300).map(|i| bump(&db, &ran, format!("m{i}"))).collect();
    ran.reach(301);
    settle_queue();
    assert!(members.iter().all(|member| !member.is_finished()));
    release(false);
    leader.join().unwrap().unwrap();
    let mut seen = HashSet::new();
    for member in members {
        seen.insert(member.join().unwrap().unwrap().version);
    }
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(counter(&db.read()), 302);
    assert!(
        db.log()
            .values()
            .all(|commit| commit.messages().count() <= 256)
    );
    db.verify().unwrap();
}

#[test]
fn history_of_sees_one_change_per_version_with_every_message() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let followers: Vec<_> = (0..3).map(|i| bump(&db, &ran, format!("m{i}"))).collect();
    ran.reach(4);
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    for follower in followers {
        follower.join().unwrap().unwrap();
    }
    let changes: Vec<_> = db.history_of(counter).collect();
    let versions: Vec<u64> = changes.iter().map(|(commit, _)| commit.version).collect();
    assert_eq!(versions, vec![3, 2, 1]);
    assert_eq!(changes[0].1, 5);
    assert_eq!(changes[0].0.messages().count(), 3);
    assert_eq!(db.at(2).unwrap().get(&0).map(|v| *v), Some(2));
}

#[test]
fn many_writers_lose_nothing_and_never_expose_half_a_group() {
    let _turn = serial();
    let dir = on_disk();
    let path = dir.path().join("many.ferro");
    let db = coalesced().open::<Root>(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (db, stop) = (db.clone(), stop.clone());
        thread::spawn(move || {
            let mut last = 0;
            while !stop.load(Ordering::SeqCst) {
                let snapshot = db.read();
                assert!(snapshot.version() >= last);
                last = snapshot.version();
                let total: u64 = snapshot.iter().map(|(_, value)| *value).sum();
                assert_eq!(total % 3, 0);
            }
        })
    };
    let writers: Vec<_> = (0..8u64)
        .map(|writer| {
            let db = db.clone();
            thread::spawn(move || {
                let mut versions = Vec::new();
                for round in 0..40u64 {
                    let done = db
                        .commit_with(|root| {
                            let key = 1 + writer * 1000 + round;
                            root.insert(key, 3);
                            ("write", ())
                        })
                        .unwrap();
                    versions.push(done.version);
                }
                assert!(versions.windows(2).all(|pair| pair[0] < pair[1]));
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    reader.join().unwrap();
    assert_eq!(db.read().len(), 320);
    assert!(db.version() <= 320);
    let log: Vec<_> = db.log().values().collect();
    assert_eq!(
        log.iter()
            .map(|commit| commit.messages().count())
            .sum::<usize>(),
        320
    );
    db.verify().unwrap();
    drop(log);
    drop(db);
    assert_eq!(
        Options::new().open::<Root>(&path).unwrap().read().len(),
        320
    );
}

#[test]
fn a_cycle_during_coalesced_commits_loses_nothing() {
    let _turn = serial();
    let dir = on_disk();
    let path = dir.path().join("cycle.ferro");
    let db = fast()
        .coalesce(true)
        .history(Keep::Last(4))
        .reclaim(Reclaim {
            every_bytes: 64 << 10,
        })
        .open::<Root>(&path)
        .unwrap();
    let writers: Vec<_> = (0..6u64)
        .map(|writer| {
            let db = db.clone();
            thread::spawn(move || {
                for round in 0..1500u64 {
                    db.update(|root| {
                        root.insert(1 + writer * 10_000 + round, round);
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    db.compact(Keep::Last(2)).unwrap();
    assert_eq!(db.read().len(), 9000);
    assert!(db.space().unwrap().cycles >= 1);
    db.verify().unwrap();
    drop(db);
    let reopened = fast().open::<Root>(&path).unwrap();
    assert_eq!(reopened.read().len(), 9000);
}

fn power_cut_run(writers: u64, rounds: u64) -> (TempDir, Vec<(u64, u64)>, Images) {
    let dir = on_disk();
    let path = dir.path().join("power.ferro");
    let db = coalesced().open::<Root>(&path).unwrap();
    let acked = Arc::new(AtomicU64::new(0));
    hook().power = Some(PowerCut {
        path: path.clone(),
        acked: acked.clone(),
        syncing: None,
        durable: None,
        images: Vec::new(),
    });
    let handles: Vec<_> = (0..writers)
        .map(|writer| {
            let (db, acked) = (db.clone(), acked.clone());
            thread::spawn(move || {
                let mut keys = Vec::new();
                for round in 0..rounds {
                    let key = 1 + writer * 1000 + round;
                    let done = db
                        .commit_with(|root| {
                            increment(root);
                            root.insert(key, round);
                            ("", ())
                        })
                        .unwrap();
                    acked.fetch_max(done.version, Ordering::SeqCst);
                    keys.push((key, done.version));
                }
                keys
            })
        })
        .collect();
    let mut keys = Vec::new();
    for handle in handles {
        keys.extend(handle.join().unwrap());
    }
    let power = hook().power.take().unwrap();
    assert_eq!(db.read().len() as u64, writers * rounds + 1);
    (dir, keys, power.images)
}

fn check_image(path: &Path, image: &[u8], floor: u64, keys: &[(u64, u64)]) {
    fs::write(path, image).unwrap();
    let db = fast().open::<Root>(path).unwrap();
    assert!(
        db.version() >= floor,
        "recovered v{} but v{floor} was acknowledged",
        db.version()
    );
    let tip = db.read();
    assert_eq!(tip.len() as u64, counter(&tip) + 1);
    for (key, version) in keys.iter().filter(|(_, version)| *version <= db.version()) {
        assert!(
            tip.contains_key(key),
            "key {key} of v{version} is gone at v{}",
            db.version()
        );
    }
    db.verify().unwrap();
}

#[test]
fn a_power_cut_keeps_every_acknowledged_closure() {
    let _turn = serial();
    let (dir, keys, images) = power_cut_run(6, 40);
    assert_eq!(keys.len(), 240);
    assert!(images.len() > 5, "only {} syncs sampled", images.len());
    let path = dir.path().join("image.ferro");
    for (floor, image) in &images {
        check_image(&path, image, *floor, &keys);
    }
}

fn clocked(ms: u64) -> (TempDir, Db<Root>) {
    let dir = on_disk();
    let clock = Clock(Arc::new(move || ms));
    let db = coalesced()
        .clock(clock)
        .open::<Root>(dir.path().join("stamp.ferro"))
        .unwrap();
    db.update(|root| {
        root.insert(0, 1);
    })
    .unwrap();
    (dir, db)
}

type Stamped = JoinHandle<Result<Committed<(u64, u64)>, Error>>;

fn stamped(db: &Db<Root>, ran: &Ran, name: &str, refuse: bool) -> Stamped {
    let (db, ran, name) = (db.clone(), ran.0.clone(), name.to_string());
    thread::spawn(move || {
        db.try_commit_stamped(|root, stamp| {
            increment(root);
            let at = (stamp.version(), stamp.time_ms());
            ran.fetch_add(1, Ordering::SeqCst);
            if refuse {
                return Err(Error::Decode(format!("{name} refused at {at:?}")));
            }
            Ok((name, at))
        })
    })
}

#[test]
fn stamps_in_a_group_name_the_groups_version_and_time() {
    let _turn = serial();
    let (_dir, db) = clocked(8_000);
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let first = stamped(&db, &ran, "first", false);
    ran.reach(2);
    let plain = bump(&db, &ran, "plain".to_string());
    ran.reach(3);
    let second = stamped(&db, &ran, "second", false);
    ran.reach(4);
    settle_queue();
    release(false);
    let led = leader.join().unwrap().unwrap();
    assert_eq!((led.version, led.time_ms), (2, 8_000));
    let plain = plain.join().unwrap().unwrap();
    assert_eq!((plain.version, plain.time_ms), (3, 8_000));
    for member in [first, second] {
        let done = member.join().unwrap().unwrap();
        assert_eq!(done.value, (3, 8_000));
        assert_eq!((done.version, done.time_ms, done.changed), (3, 8_000, true));
    }
    assert_eq!(db.log().get(&3).unwrap().time_ms, 8_000);
    assert_eq!(db.as_of(8_000).unwrap().version(), 3);
    assert_eq!(counter(&db.as_of(8_000).unwrap()), 5);
    let after = db.commit_with(|root| ("after", increment(root))).unwrap();
    assert_eq!((after.version, after.time_ms), (4, 8_001));
    assert_eq!(db.as_of(8_000).unwrap().version(), 3);
}

#[test]
fn a_closure_that_fails_after_reading_the_time_takes_back_only_its_own_claim() {
    let _turn = serial();
    let (_dir, db) = clocked(9_000);
    let ran = Ran::new();
    let leader = stall_leader(&db, &ran);
    let refused = stamped(&db, &ran, "refused", true);
    ran.reach(2);
    let member = bump(&db, &ran, "member".to_string());
    ran.reach(3);
    settle_queue();
    release(false);
    leader.join().unwrap().unwrap();
    match refused.join().unwrap() {
        Err(Error::Decode(message)) => assert_eq!(message, "refused refused at (3, 9000)"),
        other => panic!("unexpected {other:?}"),
    }
    let member = member.join().unwrap().unwrap();
    assert_eq!((member.version, member.time_ms), (3, 9_000));
    let next = db.commit_with(|root| ("next", increment(root))).unwrap();
    assert_eq!((next.version, next.time_ms), (4, 9_000));

    let leader = stall_leader(&db, &ran);
    let kept = stamped(&db, &ran, "kept", false);
    ran.reach(5);
    let refused = stamped(&db, &ran, "refused", true);
    ran.reach(6);
    settle_queue();
    release(false);
    assert_eq!(leader.join().unwrap().unwrap().version, 5);
    assert_eq!(kept.join().unwrap().unwrap().value, (6, 9_000));
    assert!(refused.join().unwrap().is_err());
    let last = db.commit_with(|root| ("last", increment(root))).unwrap();
    assert_eq!((last.version, last.time_ms), (7, 9_001));
    assert_eq!(db.as_of(9_000).unwrap().version(), 6);
    let group = db.log().get(&6).unwrap();
    assert_eq!(group.messages().collect::<Vec<_>>(), vec!["kept"]);
}
