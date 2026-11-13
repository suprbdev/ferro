#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

mod common;

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use common::{Lateness, fast};
use ferro::db::Hook;
use ferro::{Data, Db, Keep, Options, Reclaim, Snapshot};

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    n: u64,
}

type Kind = (&'static str, fn() -> Options);

fn manual() -> Options {
    fast().shared(true).reclaim(Reclaim::off())
}

fn plain() -> Options {
    fast().reclaim(Reclaim::off())
}

fn tick(db: &Db<Root>) {
    db.update(|root| root.n += 1).unwrap();
}

struct Unpark(thread::Thread);

impl std::task::Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    loop {
        if let std::task::Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        thread::park();
    }
}

fn collect<T>(finished: &mpsc::Receiver<T>, count: usize, limit: Duration) -> Vec<T> {
    let deadline = Instant::now() + limit;
    let mut ended = Vec::new();
    while ended.len() < count {
        match finished.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(what) => ended.push(what),
            Err(_) => break,
        }
    }
    ended
}

fn cycle_beside_threads_of_its_handle_taking_one_lane_in_turn(
    options: Options,
    threads: u64,
    closure: Duration,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("turns.ferro");
    let db: Db<Root> = options.open(&path).unwrap();
    let hot = db.fork("hot").unwrap();
    tick(&hot);
    tick(&db);
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..threads)
        .map(|_| {
            let (hot, stop) = (hot.clone(), stop.clone());
            thread::spawn(move || {
                let mut commits = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    hot.update(|root| {
                        thread::sleep(closure);
                        root.n += 1;
                    })
                    .unwrap();
                    commits += 1;
                }
                commits
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(200));
    let (done, finished) = mpsc::channel();
    let started = Instant::now();
    let cycler = db.clone();
    let cycle = thread::spawn(move || {
        let out = cycler.compact(Keep::Last(2));
        let _ = done.send(started.elapsed());
        out
    });
    let took = finished.recv_timeout(Duration::from_secs(20));
    stop.store(true, Ordering::Relaxed);
    let commits: Vec<u64> = writers.into_iter().map(|w| w.join().unwrap()).collect();
    let after = started.elapsed();
    cycle.join().unwrap().unwrap();
    assert!(
        took.is_ok(),
        "a cycle had not finished after 20 s beside {threads} threads of its own handle taking \
         the lane of one branch in turn with {closure:?} closures, which committed {commits:?} \
         times; it finished {after:?} after it began, once they stopped"
    );
}

#[test]
fn a_plain_cycle_gets_a_lane_that_threads_of_its_handle_take_in_turn_with_5_ms_closures() {
    cycle_beside_threads_of_its_handle_taking_one_lane_in_turn(
        plain(),
        3,
        Duration::from_millis(5),
    );
}

#[test]
fn a_shared_cycle_gets_a_lane_that_threads_of_its_handle_take_in_turn_with_5_ms_closures() {
    cycle_beside_threads_of_its_handle_taking_one_lane_in_turn(
        manual(),
        3,
        Duration::from_millis(5),
    );
}

#[test]
fn a_plain_cycle_gets_a_lane_that_two_threads_of_its_handle_take_in_turn_with_2_ms_closures() {
    cycle_beside_threads_of_its_handle_taking_one_lane_in_turn(
        plain(),
        2,
        Duration::from_millis(2),
    );
}

#[test]
fn a_coalesced_plain_cycle_gets_a_lane_that_threads_of_its_handle_take_in_turn() {
    cycle_beside_threads_of_its_handle_taking_one_lane_in_turn(
        plain().coalesce(true),
        3,
        Duration::from_millis(5),
    );
}

fn pull_beside_threads_of_its_handle_taking_one_lane_in_turn(threads: u64, closure: Duration) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pulls.ferro");
    let db: Db<Root> = plain().open(&path).unwrap();
    let source: Db<Root> = plain().open(dir.path().join("source.ferro")).unwrap();
    let hot = db.fork("hot").unwrap();
    tick(&hot);
    tick(&source);
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..threads)
        .map(|_| {
            let (hot, stop) = (hot.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    hot.update(|root| {
                        thread::sleep(closure);
                        root.n += 1;
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(200));
    let (done, finished) = mpsc::channel();
    let started = Instant::now();
    let puller = db.clone();
    let pull = thread::spawn(move || {
        let out = puller.pull(&source, "source", keeping_ours).map(|_| ());
        let _ = done.send(started.elapsed());
        out
    });
    let took = finished.recv_timeout(Duration::from_secs(20));
    stop.store(true, Ordering::Relaxed);
    writers.into_iter().for_each(|w| w.join().unwrap());
    pull.join().unwrap().unwrap();
    assert!(
        took.is_ok(),
        "a pull into `main` had not landed after 20 s beside {threads} threads of its handle \
         taking the lane of `hot` in turn with {closure:?} closures"
    );
}

#[test]
fn a_plain_pull_lands_beside_threads_of_its_handle_taking_one_lane_in_turn() {
    pull_beside_threads_of_its_handle_taking_one_lane_in_turn(3, Duration::from_millis(5));
}

fn keeping_ours(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (&'static str, ()) {
    ("merge", ())
}

#[test]
fn a_coalesced_cycle_holding_an_open_groups_byte_lets_a_joined_commit_through_another_handle_land()
{
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.ferro");
    let block = Arc::new(AtomicBool::new(false));
    let (blocked, sync_held) = mpsc::channel::<()>();
    let (at_synced, cycler_held) = mpsc::channel::<()>();
    let release = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let (steps, seen) = mpsc::channel::<(&'static str, Option<String>)>();
    let hook = {
        let (block, release, resume) = (block.clone(), release.clone(), resume.clone());
        let blocked = parking_lot::Mutex::new(blocked);
        let at_synced = parking_lot::Mutex::new(at_synced);
        let steps = parking_lot::Mutex::new(steps);
        Hook(Arc::new(move |step| {
            let name = thread::current().name().map(str::to_string);
            let cycler = name.as_deref() == Some("cycler");
            let _ = steps.lock().send((step, name));
            if step == "leading" && block.swap(false, Ordering::AcqRel) {
                let _ = blocked.lock().send(());
                release.wait();
            }
            if step == "synced" && cycler {
                let _ = at_synced.lock().send(());
                resume.wait();
            }
        }))
    };
    let h: Db<Root> = manual().coalesce(true).hook(hook).open(&path).unwrap();
    for name in ["a", "b"] {
        tick(&h.fork(name).unwrap());
    }
    tick(&h);
    let other: Db<Root> = manual().open(&path).unwrap();
    let a_other = other.branch("a").unwrap();
    let (a, b) = (h.branch("a").unwrap(), h.branch("b").unwrap());
    let (done, finished) = mpsc::channel::<&'static str>();
    let cycler = h.clone();
    let cycled = done.clone();
    thread::Builder::new()
        .name("cycler".to_string())
        .spawn(move || {
            cycler.compact(Keep::All).unwrap();
            let _ = cycled.send("the cycle");
        })
        .unwrap();
    cycler_held
        .recv_timeout(Duration::from_secs(10))
        .expect("the cycle never reached `synced`");
    block.store(true, Ordering::Release);
    let main = h.clone();
    let first = thread::spawn(move || tick(&main));
    sync_held
        .recv_timeout(Duration::from_secs(10))
        .expect("the sync never reached `leading`");
    let left_open = thread::spawn(move || tick(&a));
    thread::sleep(Duration::from_millis(100));
    let (entered, inside) = mpsc::channel();
    let closed = done.clone();
    thread::spawn(move || {
        b.update(|root| {
            entered.send(()).unwrap();
            thread::sleep(Duration::from_millis(600));
            thread::spawn(move || tick(&a_other)).join().unwrap();
            root.n += 1;
        })
        .unwrap();
        let _ = closed.send("the closure on `b`");
    });
    inside.recv().unwrap();
    resume.wait();
    thread::sleep(Duration::from_millis(200));
    release.wait();
    let ended = collect(&finished, 2, Duration::from_secs(20));
    let all_steps: Vec<(&str, Option<String>)> = seen.try_iter().collect();
    let cycler_steps: Vec<&str> = all_steps
        .iter()
        .filter(|(_, name)| name.as_deref() == Some("cycler"))
        .map(|(step, _)| *step)
        .collect();
    assert_eq!(
        ended.len(),
        2,
        "a coalesced handle's cycle that took the lane of `a` while a group was open on it, and \
         waited for a closure on `b` that joined a commit on `a` through another handle, had not \
         finished with the closure after 20 s: {ended:?}; the cycle's steps {cycler_steps:?}"
    );
    first.join().unwrap();
    left_open.join().unwrap();
}

fn sleepy_writers(
    path: &Path,
    writers: u64,
    closure: Duration,
    stop: &Arc<AtomicBool>,
) -> Vec<thread::JoinHandle<u64>> {
    let first: Db<Root> = manual().open(path).unwrap();
    for writer in 0..writers {
        first.fork(&format!("own-{writer}")).unwrap();
    }
    drop(first);
    (0..writers)
        .map(|writer| {
            let path = path.to_path_buf();
            let stop = stop.clone();
            thread::spawn(move || {
                let db: Db<Root> = manual().open(&path).unwrap();
                let db = db.branch(&format!("own-{writer}")).unwrap();
                let mut commits = 0;
                while !stop.load(Ordering::Relaxed) {
                    db.update(|root| {
                        thread::sleep(closure);
                        root.n += 1;
                    })
                    .unwrap();
                    commits += 1;
                }
                commits
            })
        })
        .collect()
}

struct Waits {
    count: usize,
    median: Duration,
    p99: Duration,
    longest: Duration,
}

fn waits(mut took: Vec<Duration>) -> Waits {
    took.sort();
    let at = |share: usize| {
        took.get(took.len().saturating_sub(1) * share / 100)
            .copied()
            .unwrap_or_default()
    };
    Waits {
        count: took.len(),
        median: at(50),
        p99: at(99),
        longest: at(100),
    }
}

fn commits_until(db: Db<Root>, stop: Arc<AtomicBool>) -> thread::JoinHandle<Vec<Duration>> {
    thread::spawn(move || {
        let mut took = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            tick(&db);
            took.push(started.elapsed());
        }
        took
    })
}

fn pulls_until(
    pulled: Db<Root>,
    source: Db<Root>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<Vec<Duration>> {
    thread::spawn(move || {
        let mut took = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            tick(&source);
            let started = Instant::now();
            pulled.pull(&source, "source", keeping_ours).unwrap();
            took.push(started.elapsed());
        }
        took
    })
}

fn commits_beside_back_to_back_pulls(pullers: usize, one_handle: bool) -> (Waits, Waits) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pulls.ferro");
    let first: Db<Root> = manual().open(&path).unwrap();
    tick(&first.fork("own").unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let pulls: Vec<_> = (0..pullers)
        .map(|at| {
            let puller: Db<Root> = match one_handle {
                true => first.clone(),
                false => manual().open(&path).unwrap(),
            };
            let pulled = puller.fork(&format!("pulled-{at}")).unwrap();
            let source: Db<Root> = fast()
                .open(dir.path().join(format!("source-{at}.ferro")))
                .unwrap();
            tick(&source);
            pulls_until(pulled, source, stop.clone())
        })
        .collect();
    let writer = match one_handle {
        true => first.branch("own").unwrap(),
        false => {
            let other: Db<Root> = manual().open(&path).unwrap();
            other.branch("own").unwrap()
        }
    };
    thread::sleep(Duration::from_millis(100));
    let writes = commits_until(writer, stop.clone());
    thread::sleep(Duration::from_secs(3));
    stop.store(true, Ordering::Relaxed);
    let commits = writes.join().unwrap();
    let pulled: Vec<Duration> = pulls.into_iter().flat_map(|p| p.join().unwrap()).collect();
    (waits(commits), waits(pulled))
}

fn assert_commits_wait_no_longer_than_holds(pullers: usize, one_handle: bool) {
    let (commits, pulls) = commits_beside_back_to_back_pulls(pullers, one_handle);
    let seen = format!(
        "{} commits, median {:?}, p99 {:?}, longest {:?}; {} pulls, median {:?}, longest {:?}",
        commits.count,
        commits.median,
        commits.p99,
        commits.longest,
        pulls.count,
        pulls.median,
        pulls.longest
    );
    println!("{pullers} pullers, one handle: {one_handle}: {seen}");
    assert!(
        commits.count >= 4 * pulls.count.max(1),
        "a writer beside {pullers} threads pulling back to back, in one handle: {one_handle}, \
         committed fewer than four times a pull, so it waited out holds that began after the one \
         it found: {seen}"
    );
}

#[test]
fn a_writer_beside_four_handles_pulling_back_to_back_waits_only_for_the_hold_it_finds() {
    assert_commits_wait_no_longer_than_holds(4, false);
}

#[test]
fn a_writer_in_a_handle_pulling_back_to_back_in_four_threads_waits_only_for_the_hold_it_finds() {
    assert_commits_wait_no_longer_than_holds(4, true);
}

#[test]
fn writers_held_at_a_pause_that_panics_go_on_as_it_unwinds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unwinds.ferro");
    let (paused, at_pause) = mpsc::channel::<()>();
    let armed = Arc::new(AtomicBool::new(true));
    let hook = {
        let armed = armed.clone();
        let paused = parking_lot::Mutex::new(paused);
        Hook(Arc::new(move |step| {
            if step == "paused" && armed.swap(false, Ordering::AcqRel) {
                let _ = paused.lock().send(());
                thread::sleep(Duration::from_millis(300));
                panic!("a pause that panics");
            }
        }))
    };
    let cycler: Db<Root> = manual().hook(hook).open(&path).unwrap();
    tick(&cycler.fork("own").unwrap());
    tick(&cycler.fork("mine").unwrap());
    let other: Db<Root> = manual().open(&path).unwrap();
    let own_elsewhere = other.branch("own").unwrap();
    let mine = cycler.branch("mine").unwrap();
    let cycling = thread::spawn(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cycler.compact(Keep::All)))
            .is_err()
    });
    at_pause.recv_timeout(Duration::from_secs(10)).unwrap();
    let watch = Lateness::watch();
    let started = Instant::now();
    let elsewhere = thread::spawn(move || {
        tick(&own_elsewhere);
        started.elapsed()
    });
    let within = thread::spawn(move || {
        tick(&mine);
        started.elapsed()
    });
    let (elsewhere, within) = (elsewhere.join().unwrap(), within.join().unwrap());
    let late = watch.stop();
    assert!(cycling.join().unwrap(), "the pause did not panic");
    let limit = late.bound(Duration::from_millis(600), Duration::from_millis(1_500));
    assert!(
        elsewhere < limit && within < limit,
        "writers that came during a 300 ms pause that then panicked took {elsewhere:?} in another \
         handle and {within:?} in the cycling handle, against {limit:?} with sleeps ending {late:?} \
         late"
    );
}

#[test]
fn a_cycle_and_a_pull_in_two_handles_both_wanting_every_branch_finish_beside_writers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.ferro");
    let stop = Arc::new(AtomicBool::new(false));
    let others = sleepy_writers(&path, 4, Duration::from_millis(20), &stop);
    let cycler: Db<Root> = manual().open(&path).unwrap();
    let puller: Db<Root> = manual().open(&path).unwrap();
    let pulled = puller.fork("pulled").unwrap();
    let source: Db<Root> = fast().open(dir.path().join("source.ferro")).unwrap();
    thread::sleep(Duration::from_millis(200));
    let (done, finished) = mpsc::channel::<(&'static str, Duration)>();
    let cycled = done.clone();
    let cycles = thread::spawn(move || {
        for _ in 0..8 {
            let started = Instant::now();
            cycler.compact(Keep::Last(2)).unwrap();
            let _ = cycled.send(("cycle", started.elapsed()));
        }
    });
    let pulls = thread::spawn(move || {
        for _ in 0..8 {
            tick(&source);
            let started = Instant::now();
            pulled.pull(&source, "source", keeping_ours).unwrap();
            let _ = done.send(("pull", started.elapsed()));
        }
    });
    let ended = collect(&finished, 16, Duration::from_secs(60));
    stop.store(true, Ordering::Relaxed);
    let commits: Vec<u64> = others.into_iter().map(|w| w.join().unwrap()).collect();
    cycles.join().unwrap();
    pulls.join().unwrap();
    let longest = ended.iter().map(|(_, took)| *took).max();
    assert!(
        ended.len() == 16 && longest.is_some_and(|took| took < Duration::from_secs(2)),
        "eight cycles in one handle and eight pulls in another, beside four handles whose 20 ms \
         closures committed {commits:?} times: {ended:?}"
    );
}

fn hogs(count: usize, stop: &Arc<AtomicBool>) -> Vec<thread::JoinHandle<()>> {
    (0..count)
        .map(|_| {
            let stop = stop.clone();
            thread::spawn(move || {
                let mut x = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    std::hint::black_box(x);
                }
            })
        })
        .collect()
}

fn joined_commits_beside_cycles(
    hog_threads: usize,
    rounds: u64,
    awaited: bool,
    gap: Duration,
) -> (Vec<Duration>, Lateness) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("joined.ferro");
    let y: Db<Root> = manual().open(&path).unwrap();
    for name in ["a", "b"] {
        tick(&y.fork(name).unwrap());
    }
    let cycler: Db<Root> = manual().open(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let hot = hogs(hog_threads, &stop);
    let (begun, began) = mpsc::channel();
    let cycling = {
        let stop = stop.clone();
        thread::spawn(move || {
            let (mut cycles, mut longest) = (0u64, Duration::ZERO);
            while !stop.load(Ordering::Relaxed) {
                if cycles == 0 {
                    begun.send(()).unwrap();
                }
                let started = Instant::now();
                cycler.compact(Keep::Last(1)).unwrap();
                longest = longest.max(started.elapsed());
                cycles += 1;
            }
            (cycles, longest)
        })
    };
    began.recv().unwrap();
    let (a, b) = (y.branch("a").unwrap(), y.branch("b").unwrap());
    let mut took = Vec::new();
    let (sent, waited) = mpsc::channel();
    let watch = Lateness::watch();
    for _ in 0..rounds {
        thread::sleep(gap);
        let (b, sent) = (b.clone(), sent.clone());
        let a = a.clone();
        thread::spawn(move || {
            a.update(|root| {
                thread::sleep(Duration::from_millis(20));
                let joined = Instant::now();
                thread::spawn(move || match awaited {
                    true => block_on(b.update_async(|inner| inner.n += 1)).unwrap(),
                    false => tick(&b),
                })
                .join()
                .unwrap();
                let _ = sent.send(joined.elapsed());
                root.n += 1;
            })
            .unwrap();
        });
        match waited.recv_timeout(Duration::from_secs(20)) {
            Ok(joined) => took.push(joined),
            Err(_) => break,
        }
    }
    let late = watch.stop();
    stop.store(true, Ordering::Relaxed);
    let (cycles, longest_cycle) = cycling.join().unwrap();
    hot.into_iter().for_each(|h| h.join().unwrap());
    took.sort();
    assert!(
        cycles > 0,
        "the other handle finished no cycle, the longest taking {longest_cycle:?}"
    );
    (took, late)
}

fn joined_commit_stays_short(hog_threads: usize, awaited: bool, gap: Duration) {
    let rounds = 40;
    let (took, late) = joined_commits_beside_cycles(hog_threads, rounds, awaited, gap);
    assert_eq!(took.len() as u64, rounds, "a joined commit hung: {took:?}");
    let longest = *took.last().unwrap();
    let most = took[took.len() * 9 / 10];
    let limit = late.bound(Duration::from_millis(100), Duration::from_millis(500));
    assert!(
        longest < Duration::from_secs(2) && most < limit,
        "a commit on `b` that a 20 ms closure on `a` joined, awaited: {awaited}, beside another \
         handle cycling as fast as it could and {hog_threads} spinning threads, took up to \
         {longest:?}, and nine in ten {most:?} or less against {limit:?}, with sleeps ending \
         {late:?} late: {took:?}"
    );
}

#[test]
fn a_joined_commit_beside_cycles_stays_short_on_an_idle_machine() {
    joined_commit_stays_short(0, false, Duration::ZERO);
}

#[test]
fn a_joined_commit_beside_cycles_stays_short_when_closures_start_5_ms_apart() {
    joined_commit_stays_short(0, false, Duration::from_millis(5));
}

#[test]
fn a_joined_commit_beside_cycles_stays_short_when_closures_start_5_ms_apart_beside_spinning_threads()
 {
    let cores = thread::available_parallelism().map_or(8, |n| n.get());
    joined_commit_stays_short(cores * 2, false, Duration::from_millis(5));
}

#[test]
fn a_joined_commit_beside_cycles_stays_short_beside_spinning_threads() {
    let cores = thread::available_parallelism().map_or(8, |n| n.get());
    joined_commit_stays_short(cores * 2, false, Duration::ZERO);
}

#[test]
fn a_joined_awaited_commit_beside_cycles_stays_short_beside_spinning_threads() {
    let cores = thread::available_parallelism().map_or(8, |n| n.get());
    joined_commit_stays_short(cores * 2, true, Duration::ZERO);
}

const GATE_AT: u64 = (1 << 62) + 5;
const PHASE_AT: u64 = (1 << 62) + (3 << 18);
const PHASE_BYTES: u64 = 2 << 10;
const LANES_AT: u64 = (1 << 62) + (1 << 20);
const LANES: u64 = 1 << 60;

fn locked_elsewhere(file: &File, at: u64, len: u64) -> bool {
    // SAFETY: `flock` is a C struct of plain integers, for which all zero bytes are a valid value.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = at as libc::off_t;
    lock.l_len = len as libc::off_t;
    // SAFETY: the descriptor belongs to `file`, which is borrowed for the call, and `lock` is a
    // live, initialized `flock` that `F_OFD_GETLK` reads and writes.
    let done = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_GETLK, &mut lock) };
    assert_ne!(done, -1, "fcntl: {}", std::io::Error::last_os_error());
    lock.l_type != libc::F_UNLCK as libc::c_short
}

#[test]
fn a_coalesced_cycle_that_panics_in_its_pause_leaves_its_lanes_the_bytes_and_the_gate_free() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("panic.ferro");
    let armed = Arc::new(AtomicBool::new(true));
    let hook = {
        let armed = armed.clone();
        Hook(Arc::new(move |step| {
            if step == "paused"
                && thread::current().name() == Some("cycler")
                && armed.swap(false, Ordering::AcqRel)
            {
                panic!("a hook panics in the cycle's pause");
            }
        }))
    };
    let db: Db<Root> = manual().coalesce(true).hook(hook).open(&path).unwrap();
    let (a, b) = (db.fork("a").unwrap(), db.fork("b").unwrap());
    tick(&a);
    tick(&b);
    tick(&db);
    let cycler = db.clone();
    let ran = thread::Builder::new()
        .name("cycler".to_string())
        .spawn(move || cycler.compact(Keep::Last(1)))
        .unwrap()
        .join();
    assert!(ran.is_err(), "the hook did not panic: {ran:?}");
    let probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let gate =
        locked_elsewhere(&probe, GATE_AT, 1) || locked_elsewhere(&probe, PHASE_AT, PHASE_BYTES);
    let range = locked_elsewhere(&probe, LANES_AT, LANES);
    let (done, finished) = mpsc::channel();
    let other: Db<Root> = manual().open(&path).unwrap();
    thread::spawn(move || {
        tick(&other.branch("a").unwrap());
        tick(&other.branch("b").unwrap());
        tick(&a);
        tick(&b);
        db.compact(Keep::Last(1)).unwrap();
        let _ = done.send(());
    });
    let after = finished.recv_timeout(Duration::from_secs(20));
    assert!(
        !gate && !range && after.is_ok(),
        "after a cycle panicked in its pause, the gate or a phase byte was locked: {gate}, a \
         branch byte: {range}, and commits on both branches and another cycle finished: {}",
        after.is_ok()
    );
}

#[test]
fn forks_and_drops_joined_from_closures_finish_beside_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fork.ferro");
    let cycler: Db<Root> = manual().open(&path).unwrap();
    for name in ["a", "b"] {
        tick(&cycler.fork(name).unwrap());
    }
    let y: Db<Root> = manual().open(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let cycling = {
        let (stop, cycler) = (stop.clone(), cycler.clone());
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                cycler.compact(Keep::Last(1)).unwrap();
            }
        })
    };
    let (sent, waited) = mpsc::channel();
    let mut took = Vec::new();
    for i in 0..30u64 {
        let (outer, inner) = match i % 3 {
            0 => (y.branch("a").unwrap(), y.branch("b").unwrap()),
            1 => (cycler.branch("b").unwrap(), cycler.branch("a").unwrap()),
            _ => (cycler.branch("b").unwrap(), cycler.clone()),
        };
        let sent = sent.clone();
        thread::spawn(move || {
            outer
                .update(|root| {
                    thread::sleep(Duration::from_millis(10));
                    let joined = Instant::now();
                    thread::spawn(move || {
                        let name = format!("x{i}");
                        match i % 3 {
                            0 | 1 => drop(inner.fork(&name).unwrap()),
                            _ => {
                                drop(inner.fork(&name).unwrap());
                                inner.drop_branch(&name).unwrap();
                            }
                        }
                    })
                    .join()
                    .unwrap();
                    let _ = sent.send((i, joined.elapsed()));
                    root.n += 1;
                })
                .unwrap();
        });
        match waited.recv_timeout(Duration::from_secs(20)) {
            Ok(joined) => took.push(joined),
            Err(_) => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
    let cycled = cycling.join();
    assert!(
        took.len() == 30 && cycled.is_ok(),
        "forks and drops joined from closures beside cycles: {took:?}"
    );
}

fn plain_cycles_beside_writers(writers: u64, closure: Duration) -> (Vec<Duration>, Duration) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.ferro");
    let db: Db<Root> = plain().open(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let longest = Arc::new(AtomicU64::new(0));
    let threads: Vec<_> = (0..writers)
        .map(|writer| {
            let db = db.fork(&format!("own-{writer}")).unwrap();
            let (stop, longest) = (stop.clone(), longest.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    db.update(|root| {
                        if !closure.is_zero() {
                            thread::sleep(closure);
                        }
                        root.n += 1;
                    })
                    .unwrap();
                    let took = u64::try_from(started.elapsed().as_micros()).unwrap();
                    longest.fetch_max(took, Ordering::Relaxed);
                }
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(100));
    let mut took = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        db.compact(Keep::Last(2)).unwrap();
        took.push(started.elapsed());
    }
    stop.store(true, Ordering::Relaxed);
    threads.into_iter().for_each(|t| t.join().unwrap());
    took.sort();
    (took, Duration::from_micros(longest.load(Ordering::Relaxed)))
}

#[test]
fn plain_cycles_beside_writers_on_branches_of_their_own_report_their_times() {
    for (writers, closure) in [(8, 0), (8, 2), (16, 5)] {
        let (took, longest) = plain_cycles_beside_writers(writers, Duration::from_millis(closure));
        println!(
            "plain cycles beside {writers} writers with {closure} ms closures: median {:?}, \
             longest {:?}, longest commit {longest:?}",
            took[took.len() / 2],
            took.last().unwrap()
        );
        assert!(*took.last().unwrap() < Duration::from_secs(5), "{took:?}");
    }
}

#[test]
fn closures_joining_commits_through_the_cycling_handle_back_to_back_wait_a_bounded_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("through.ferro");
    let cycler: Db<Root> = manual().open(&path).unwrap();
    for name in ["a", "b"] {
        tick(&cycler.fork(name).unwrap());
    }
    let y: Db<Root> = manual().open(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let cycling = {
        let (stop, cycler) = (stop.clone(), cycler.clone());
        thread::spawn(move || {
            let (mut cycles, mut longest) = (0u64, Duration::ZERO);
            while !stop.load(Ordering::Relaxed) {
                let started = Instant::now();
                cycler.compact(Keep::Last(1)).unwrap();
                longest = longest.max(started.elapsed());
                cycles += 1;
            }
            (cycles, longest)
        })
    };
    let (a, b) = (y.branch("a").unwrap(), cycler.branch("b").unwrap());
    let (sent, waited) = mpsc::channel();
    let mut took = Vec::new();
    let watch = Lateness::watch();
    let started = Instant::now();
    while took.len() < 40 && started.elapsed() < Duration::from_secs(30) {
        let (a, b, sent) = (a.clone(), b.clone(), sent.clone());
        thread::spawn(move || {
            a.update(|root| {
                thread::sleep(Duration::from_millis(20));
                let joined = Instant::now();
                thread::spawn(move || tick(&b)).join().unwrap();
                let _ = sent.send(joined.elapsed());
                root.n += 1;
            })
            .unwrap();
        });
        match waited.recv_timeout(Duration::from_secs(30)) {
            Ok(joined) => took.push(joined),
            Err(_) => break,
        }
    }
    let late = watch.stop();
    stop.store(true, Ordering::Relaxed);
    let (cycles, longest_cycle) = cycling.join().unwrap();
    let mut sorted = took.clone();
    sorted.sort();
    let longest = sorted.last().copied();
    let most = sorted.get(sorted.len() * 9 / 10).copied();
    let limit = late.bound(Duration::from_millis(100), Duration::from_millis(500));
    assert!(
        took.len() == 40
            && longest.is_some_and(|took| took < Duration::from_secs(2))
            && most.is_some_and(|took| took < limit),
        "commits through the cycling handle that 20 ms closures on `a` in another handle joined \
         back to back took {took:?}, nine in ten against {limit:?} with sleeps ending {late:?} \
         late; {cycles} cycles finished meanwhile, the longest in {longest_cycle:?}"
    );
}

#[test]
fn a_closure_joining_a_commit_on_an_earlier_branch_of_its_handle_finishes_beside_its_cycle() {
    let kinds: [Kind; 2] = [
        ("shared", manual),
        ("shared coalesced", || manual().coalesce(true)),
    ];
    for (kind, options) in kinds {
        let dir = tempfile::tempdir().unwrap();
        let synced = Arc::new(AtomicBool::new(false));
        let hook = {
            let synced = synced.clone();
            Hook(Arc::new(move |step| {
                if step == "synced" {
                    synced.store(true, Ordering::SeqCst);
                }
            }))
        };
        let main: Db<Root> = options()
            .hook(hook)
            .open(dir.path().join("earlier.ferro"))
            .unwrap();
        tick(&main);
        let (b, c) = (main.fork("b").unwrap(), main.fork("c").unwrap());
        let (entered, inside) = mpsc::channel();
        let (done, finished) = mpsc::channel::<&'static str>();
        let closed = done.clone();
        thread::spawn(move || {
            c.update(|root| {
                entered.send(()).unwrap();
                while !synced.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
                thread::sleep(Duration::from_millis(100));
                thread::spawn(move || tick(&b)).join().unwrap();
                root.n += 1;
            })
            .unwrap();
            let _ = closed.send("the closure on `c`");
        });
        inside.recv().unwrap();
        thread::spawn(move || {
            main.compact(Keep::All).unwrap();
            let _ = done.send("the cycle");
        });
        let ended = collect(&finished, 2, Duration::from_secs(20));
        assert_eq!(
            ended.len(),
            2,
            "{kind}: a closure on `c` joined a commit on `b` through its own handle while that \
             handle cycled; after 20 s only {ended:?} had finished"
        );
    }
}

#[test]
#[ignore = "prints how often each of FERRO_PULLERS threads, of one handle if FERRO_ONE_HANDLE is \
            set, or the first FERRO_IN_ONE_HANDLE of them of one handle and the rest of handles \
            of their own, pulls beside FERRO_WRITERS handles whose closures sleep \
            FERRO_CLOSURE_US, over FERRO_SECS"]
fn threads_pulling_beside_writers_report_how_often_each_pulls() {
    let setting =
        |name: &str, default: u64| std::env::var(name).map_or(default, |v| v.parse().unwrap());
    let pullers = setting("FERRO_PULLERS", 16);
    let writers = setting("FERRO_WRITERS", 4);
    let closure = Duration::from_micros(setting("FERRO_CLOSURE_US", 5_000));
    let secs = setting("FERRO_SECS", 10);
    let in_one = match std::env::var("FERRO_ONE_HANDLE").is_ok() {
        true => pullers,
        false => setting("FERRO_IN_ONE_HANDLE", 0),
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pulls.ferro");
    let stop = Arc::new(AtomicBool::new(false));
    let writing = sleepy_writers(&path, writers, closure, &stop);
    let first: Db<Root> = manual().open(&path).unwrap();
    let pulls: Vec<_> = (0..pullers)
        .map(|at| {
            let puller: Db<Root> = match at < in_one {
                true => first.clone(),
                false => manual().open(&path).unwrap(),
            };
            let pulled = puller.fork(&format!("pulled-{at}")).unwrap();
            let source: Db<Root> = fast()
                .open(dir.path().join(format!("source-{at}.ferro")))
                .unwrap();
            tick(&source);
            pulls_until(pulled, source, stop.clone())
        })
        .collect();
    thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    let mut each: Vec<Waits> = pulls
        .into_iter()
        .map(|p| waits(p.join().unwrap()))
        .collect();
    let commits: u64 = writing.into_iter().map(|w| w.join().unwrap()).sum();
    let longest_of = |each: &[Waits]| {
        each.iter()
            .map(|waits| waits.longest)
            .max()
            .unwrap_or_default()
    };
    let (shared, own) = each.split_at(usize::try_from(in_one).unwrap());
    let mut of_one: Vec<usize> = shared.iter().map(|waits| waits.count).collect();
    of_one.sort_unstable();
    let longest_of_one = longest_of(shared);
    let longest_of_own = longest_of(own);
    each.sort_by_key(|waits| waits.count);
    let counts: Vec<usize> = each.iter().map(|waits| waits.count).collect();
    let mut medians: Vec<Duration> = each.iter().map(|waits| waits.median).collect();
    medians.sort();
    let longest = longest_of(&each);
    println!(
        "PULLING THREADS {pullers}, {in_one} of one handle, beside {writers} writers, {secs} s: \
         pulls each {counts:?}, {} in all, of one handle {of_one:?}; median pull per thread {:?} \
         to {:?}; longest pull {longest:?}, of one handle {longest_of_one:?}, of the others \
         {longest_of_own:?}; writer commits {commits}",
        counts.iter().sum::<usize>(),
        medians.first().copied().unwrap_or_default(),
        medians.last().copied().unwrap_or_default(),
    );
}
