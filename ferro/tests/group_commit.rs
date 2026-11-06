#![cfg(unix)]

mod common;

use std::ffi::{c_char, c_int, c_void};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::{assert_damaged, fast, frames_past_the_anchor, judged};
use ferro::{Db, Error, Map, Options};
use tempfile::TempDir;

type Root = Map<u64, u64>;

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

fn on_disk() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn database() -> (TempDir, PathBuf, Db<Root>) {
    let dir = on_disk();
    let path = dir.path().join("group.ferro");
    let db = Options::new().open::<Root>(&path).unwrap();
    db.update(|root| {
        root.insert(0, 1);
    })
    .unwrap();
    (dir, path, db)
}

fn counter(root: &Root) -> u64 {
    root.get(&0).map_or(0, |value| *value)
}

fn bump(db: &Db<Root>, ran: &Arc<AtomicUsize>) -> JoinHandle<ferro::Result<u64>> {
    let (db, ran) = (db.clone(), ran.clone());
    thread::spawn(move || {
        let done = db.commit_with(|root| {
            let next = counter(root) + 1;
            root.insert(0, next);
            ran.fetch_add(1, Ordering::SeqCst);
            ("bump", next)
        })?;
        assert_eq!(done.value, done.version);
        Ok(done.version)
    })
}

fn stall_leader(db: &Db<Root>, ran: &Arc<AtomicUsize>) -> JoinHandle<ferro::Result<u64>> {
    hold();
    let leader = bump(db, ran);
    wait_until("the leader to enter its sync", || hook().inside == 1);
    leader
}

fn settle_queue() {
    thread::sleep(Duration::from_millis(30));
}

#[test]
fn queued_commits_stay_invisible_until_their_sync_returns() {
    let _turn = serial();
    let (_dir, path, db) = database();
    let events = db.watch();
    let outside = fast().read_only(true).open::<Root>(&path).unwrap();
    let ran = Arc::new(AtomicUsize::new(0));
    let leader = stall_leader(&db, &ran);
    let followers: Vec<_> = (0..4).map(|_| bump(&db, &ran)).collect();
    wait_until("every closure to run", || ran.load(Ordering::SeqCst) == 5);
    settle_queue();
    assert_eq!(db.version(), 1);
    assert_eq!(counter(&db.read()), 1);
    assert!(db.log().get(&2).is_none());
    assert!(db.at(2).is_err());
    assert!(events.try_recv().is_err());
    assert!(!outside.refresh().unwrap());
    assert_eq!(outside.version(), 1);
    let fresh = fast().read_only(true).open::<Root>(&path).unwrap();
    assert_eq!(fresh.version(), 1);
    assert_eq!(counter(&fresh.read()), 1);
    assert!(!leader.is_finished());
    assert!(followers.iter().all(|follower| !follower.is_finished()));
    release(false);
    let mut versions = vec![leader.join().unwrap().unwrap()];
    versions.extend(followers.into_iter().map(|f| f.join().unwrap().unwrap()));
    versions.sort_unstable();
    assert_eq!(versions, vec![2, 3, 4, 5, 6]);
    assert!(hook().calls < 5, "no group formed");
    assert_eq!(db.version(), 6);
    assert_eq!(counter(&db.read()), 6);
    let snapshot = events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(snapshot.version(), 6);
    assert_eq!(counter(&snapshot), 6);
    drop(snapshot);
    assert!(outside.refresh().unwrap());
    assert_eq!(outside.version(), 6);
    drop(db);
    let reopened = Options::new().open::<Root>(&path).unwrap();
    assert_eq!(reopened.version(), 6);
    reopened.verify().unwrap();
}

#[test]
fn a_failed_sync_fails_everything_queued_behind_it_and_poisons() {
    let _turn = serial();
    let (_dir, path, db) = database();
    let events = db.watch();
    let ran = Arc::new(AtomicUsize::new(0));
    let leader = stall_leader(&db, &ran);
    let followers: Vec<_> = (0..4).map(|_| bump(&db, &ran)).collect();
    wait_until("every closure to run", || ran.load(Ordering::SeqCst) == 5);
    settle_queue();
    release(true);
    assert!(matches!(leader.join().unwrap(), Err(Error::Io(_))));
    for follower in followers {
        assert!(matches!(follower.join().unwrap(), Err(Error::Poisoned)));
    }
    assert_eq!(db.version(), 1);
    assert_eq!(counter(&db.read()), 1);
    assert!(events.try_recv().is_err());
    let late = db.update(|root| {
        root.insert(9, 9);
    });
    assert!(matches!(late, Err(Error::Poisoned)));
    assert!(matches!(db.fork("late"), Err(Error::Poisoned)));
    assert!(matches!(db.update(|_| ()), Err(Error::Poisoned)));
    let fresh = fast().read_only(true).open::<Root>(&path).unwrap();
    assert_eq!(fresh.version(), 1);
    hook().fail = false;
    drop(db);
    let reopened = Options::new().open::<Root>(&path).unwrap();
    assert!(reopened.version() >= 1);
    assert_eq!(counter(&reopened.read()), reopened.version());
    reopened.verify().unwrap();
}

#[test]
fn a_no_op_on_an_unpublished_tip_waits_for_that_tip_to_become_durable() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Arc::new(AtomicUsize::new(0));
    let leader = stall_leader(&db, &ran);
    let observed = Arc::new(AtomicU64::new(0));
    let reader = {
        let (db, observed) = (db.clone(), observed.clone());
        thread::spawn(move || {
            db.commit_with(|root| {
                observed.store(counter(root), Ordering::SeqCst);
                ("nothing", counter(root))
            })
        })
    };
    wait_until("the no-op closure to run", || {
        observed.load(Ordering::SeqCst) != 0
    });
    assert_eq!(observed.load(Ordering::SeqCst), 2);
    settle_queue();
    assert!(!reader.is_finished());
    release(false);
    assert_eq!(leader.join().unwrap().unwrap(), 2);
    let done = reader.join().unwrap().unwrap();
    assert_eq!(done.value, 2);
    assert_eq!(done.version, 2);
    assert!(!done.changed);
}

#[test]
fn a_no_op_on_a_lost_tip_reports_the_failure_instead_of_the_value() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Arc::new(AtomicUsize::new(0));
    let leader = stall_leader(&db, &ran);
    let observed = Arc::new(AtomicU64::new(0));
    let reader = {
        let (db, observed) = (db.clone(), observed.clone());
        thread::spawn(move || {
            db.update(|root| {
                observed.store(counter(root), Ordering::SeqCst);
                counter(root)
            })
        })
    };
    wait_until("the no-op closure to run", || {
        observed.load(Ordering::SeqCst) != 0
    });
    settle_queue();
    release(true);
    assert!(leader.join().unwrap().is_err());
    assert!(matches!(reader.join().unwrap(), Err(Error::Poisoned)));
}

#[test]
fn a_closure_error_from_an_unpublished_tip_waits_for_durability() {
    let _turn = serial();
    let (_dir, _path, db) = database();
    let ran = Arc::new(AtomicUsize::new(0));
    let leader = stall_leader(&db, &ran);
    let observed = Arc::new(AtomicU64::new(0));
    let refuser = {
        let (db, observed) = (db.clone(), observed.clone());
        thread::spawn(move || {
            db.try_update(|root| -> Result<(), Error> {
                observed.store(counter(root), Ordering::SeqCst);
                Err(Error::Decode(format!("saw {}", counter(root))))
            })
        })
    };
    wait_until("the refusing closure to run", || {
        observed.load(Ordering::SeqCst) != 0
    });
    settle_queue();
    assert!(!refuser.is_finished());
    release(false);
    leader.join().unwrap().unwrap();
    match refuser.join().unwrap() {
        Err(Error::Decode(message)) => assert_eq!(message, "saw 2"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn fork_builds_on_the_queued_tip_and_appears_only_once_durable() {
    let _turn = serial();
    let (_dir, path, db) = database();
    let ran = Arc::new(AtomicUsize::new(0));
    let leader = stall_leader(&db, &ran);
    let forker = {
        let db = db.clone();
        thread::spawn(move || db.fork("side"))
    };
    settle_queue();
    assert!(!forker.is_finished());
    assert_eq!(db.branches(), vec!["main".to_string()]);
    assert!(matches!(db.branch("side"), Err(Error::NoBranch(_))));
    release(false);
    leader.join().unwrap().unwrap();
    let side = forker.join().unwrap().unwrap();
    assert_eq!(side.version(), 2);
    assert_eq!(counter(&side.read()), 2);
    assert_eq!(db.branches(), vec!["main".to_string(), "side".to_string()]);
    side.update(|root| {
        root.insert(7, 7);
    })
    .unwrap();
    assert_eq!(db.version(), 2);
    assert_eq!(side.version(), 3);
    drop((db, side));
    let reopened = Options::new().open::<Root>(&path).unwrap();
    assert_eq!(reopened.branch("side").unwrap().version(), 3);
}

#[test]
fn branches_queue_together_and_publish_on_their_own_heads() {
    let _turn = serial();
    let (_dir, path, db) = database();
    let side = db.fork("side").unwrap();
    let ran = Arc::new(AtomicUsize::new(0));
    let leader = stall_leader(&db, &ran);
    let on_main = bump(&db, &ran);
    let on_side = bump(&side, &ran);
    wait_until("every closure to run", || ran.load(Ordering::SeqCst) == 3);
    settle_queue();
    assert_eq!((db.version(), side.version()), (1, 1));
    release(false);
    assert_eq!(
        leader.join().unwrap().unwrap() + 1,
        on_main.join().unwrap().unwrap()
    );
    assert_eq!(on_side.join().unwrap().unwrap(), 2);
    assert_eq!((db.version(), side.version()), (3, 2));
    drop((db, side));
    let reopened = Options::new().open::<Root>(&path).unwrap();
    assert_eq!(reopened.version(), 3);
    assert_eq!(reopened.branch("side").unwrap().version(), 2);
}

fn power_cut_run(writers: u64, rounds: u64) -> (TempDir, u64, Vec<(u64, Vec<u8>)>) {
    let dir = on_disk();
    let path = dir.path().join("power.ferro");
    let db = Options::new().open::<Root>(&path).unwrap();
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
                for round in 0..rounds {
                    let done = db
                        .commit_with(|root| {
                            let next = counter(root) + 1;
                            root.insert(0, next);
                            root.insert(1 + writer * 1000 + round, next);
                            ("", ())
                        })
                        .unwrap();
                    acked.fetch_max(done.version, Ordering::SeqCst);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let power = hook().power.take().unwrap();
    assert_eq!(db.version(), writers * rounds);
    (dir, acked.load(Ordering::SeqCst), power.images)
}

fn check_image(path: &Path, image: &[u8], floor: u64) {
    fs::write(path, image).unwrap();
    let readonly = fast().read_only(true).open::<Root>(path).unwrap();
    assert!(
        readonly.version() >= floor,
        "read-only lost an acknowledged commit"
    );
    assert_eq!(counter(&readonly.read()), readonly.version());
    drop(readonly);
    let db = fast().open::<Root>(path).unwrap();
    assert!(
        db.version() >= floor,
        "recovered v{} but v{floor} was acknowledged",
        db.version()
    );
    assert_eq!(counter(&db.read()), db.version());
    assert_eq!(db.read().len() as u64, db.version() + 1);
    db.verify().unwrap();
}

#[test]
fn a_power_cut_keeps_every_commit_acknowledged_before_it() {
    let _turn = serial();
    let (dir, acked, images) = power_cut_run(6, 40);
    assert_eq!(acked, 240);
    assert!(images.len() > 10, "only {} syncs sampled", images.len());
    let path = dir.path().join("image.ferro");
    let reached = images.iter().map(|(floor, _)| *floor).max().unwrap();
    assert!(reached > 200, "acknowledgements only reached {reached}");
    for (floor, image) in &images {
        check_image(&path, image, *floor);
    }
}

#[test]
fn a_flipped_bit_in_a_synced_frame_past_the_superblock_is_reported() {
    let _turn = serial();
    let (dir, _, images) = power_cut_run(6, 40);
    let path = dir.path().join("image.ferro");
    let (mut reported, mut acknowledged) = (0, 0);
    for (floor, image) in &images {
        let Some(&(start, end)) = frames_past_the_anchor(image).last() else {
            continue;
        };
        fs::write(&path, image).unwrap();
        let version = fast()
            .read_only(true)
            .open::<Root>(&path)
            .unwrap()
            .version();
        let at = start + (end - start) / 2;
        let mut damaged = image.clone();
        damaged[at] ^= 0x10;
        assert!(judged(&damaged, at, start, end));
        fs::write(&path, &damaged).unwrap();
        let context = format!("v{version} acknowledged to v{floor}: byte {at} of {start}..{end}");
        assert_damaged(
            fast().read_only(true).open::<Root>(&path),
            start,
            at,
            &context,
        );
        assert_damaged(Options::new().open::<Root>(&path), start, at, &context);
        assert_eq!(
            fs::read(&path).unwrap(),
            damaged,
            "{context}: the file changed"
        );
        reported += 1;
        if version <= *floor {
            acknowledged += 1;
        }
    }
    assert!(reported > 10, "only {reported} frames past a superblock");
    assert!(acknowledged > 0, "no acknowledged frame damaged");
}

#[test]
fn reservation_is_invisible_to_readers_and_released_on_close() {
    let _turn = serial();
    let (_dir, path, db) = database();
    db.update(|root| {
        root.insert(0, 2);
    })
    .unwrap();
    let size = db.size_on_disk();
    assert!(
        fs::metadata(&path).unwrap().len() > size,
        "no space was reserved"
    );
    let image = fs::read(&path).unwrap();
    let copy = path.with_extension("copy");
    fs::write(&copy, &image).unwrap();
    assert_eq!(
        fast()
            .read_only(true)
            .open::<Root>(&copy)
            .unwrap()
            .version(),
        2
    );
    assert_eq!(fast().open::<Root>(&copy).unwrap().version(), 2);
    assert_eq!(fs::metadata(&copy).unwrap().len(), size);
    drop(db);
    assert_eq!(fs::metadata(&path).unwrap().len(), size);
}
