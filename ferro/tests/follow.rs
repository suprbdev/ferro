mod common;

use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use common::fast;
use ferro::{Db, Error, Keep, Map, Reclaim};

type Root = Map<u64, u64>;

const SOON: Duration = Duration::from_secs(10);
const MANUAL: Reclaim = Reclaim::off();
const QUICK: Option<Duration> = Some(Duration::from_millis(2));

fn put(db: &Db<Root>, key: u64, value: u64) {
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
}

fn follower(path: &Path, poll: Option<Duration>) -> Db<Root> {
    let options = fast().read_only(true).follow(true);
    let options = match poll {
        Some(every) => options.follow_polling(every),
        None => options,
    };
    options.open(path).unwrap()
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + SOON;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn within<R: Send + 'static>(what: &str, work: impl FnOnce() -> R + Send + 'static) -> R {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver
        .recv_timeout(SOON)
        .unwrap_or_else(|_| panic!("{what} did not finish"))
}

fn watchers_fire_without_refresh(poll: Option<Duration>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("follow.ferro");
    let writer = fast().open::<Root>(&path).unwrap();
    put(&writer, 1, 1);
    let reader = follower(&path, poll);
    let watcher = reader.watch();
    let filtered = reader.watch_when(|root| root.contains_key(&3));
    for n in 2..=3 {
        put(&writer, n, n);
        let seen = watcher.recv_timeout(SOON).unwrap();
        assert!(seen.version() <= n);
        assert_eq!(*seen.get(&seen.version()).unwrap(), seen.version());
    }
    until("the reader reached version 3", || reader.version() == 3);
    assert_eq!(filtered.recv_timeout(SOON).unwrap().version(), 3);
    assert_eq!(*reader.read().get(&3).unwrap(), 3);
    let following = reader.following().unwrap();
    assert_eq!(following.notified, poll.is_none());
    assert!(following.refreshes >= 1, "{following:?}");
    assert_eq!(following.last_error, None);
}

#[test]
fn a_read_only_followers_watchers_fire_on_another_handles_commits_without_refresh() {
    watchers_fire_without_refresh(None);
}

#[test]
fn a_polling_followers_watchers_fire_on_another_handles_commits_without_refresh() {
    watchers_fire_without_refresh(QUICK);
}

#[test]
fn every_handle_on_a_followed_file_shares_its_follower_and_a_lone_writer_starts_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("follow.ferro");
    let writer = fast().follow(true).open::<Root>(&path).unwrap();
    assert!(writer.following().is_none());
    writer.fork("side").unwrap();
    let reader = follower(&path, None);
    let side = reader.branch("side").unwrap();
    let watcher = side.watch();
    writer
        .branch("side")
        .unwrap()
        .update(|root| {
            root.insert(9, 9);
        })
        .unwrap();
    assert_eq!(*watcher.recv_timeout(SOON).unwrap().get(&9).unwrap(), 9);
    assert_eq!(side.following(), reader.following());
}

fn follows_cycles(poll: Option<Duration>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cycle.ferro");
    let writer = fast()
        .history(Keep::Last(2))
        .reclaim(MANUAL)
        .open::<Root>(&path)
        .unwrap();
    for n in 1..=20 {
        put(&writer, n % 4, n);
    }
    let reader = follower(&path, poll);
    let watcher = reader.watch();
    for round in 0..3 {
        writer.compact(Keep::Last(2)).unwrap();
        put(&writer, 100 + round, round);
        let goal = writer.version();
        until("the reader followed the cycle", || {
            let _ = watcher.recv_timeout(Duration::from_millis(20));
            reader.version() == goal
        });
        assert_eq!(*reader.read().get(&(100 + round)).unwrap(), round);
        assert_eq!(
            reader.size_on_disk(),
            writer.size_on_disk(),
            "the reader reads the new file"
        );
    }
    assert_eq!(reader.following().unwrap().last_error, None);
}

#[test]
fn a_follower_follows_the_file_a_reclamation_cycle_renames_over_the_path() {
    follows_cycles(None);
}

#[test]
fn a_polling_follower_follows_the_file_a_reclamation_cycle_renames_over_the_path() {
    follows_cycles(QUICK);
}

#[test]
fn a_follower_that_finds_nothing_new_refreshes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idle.ferro");
    let writer = fast().open::<Root>(&path).unwrap();
    put(&writer, 1, 1);
    let reader = follower(&path, Some(Duration::from_millis(1)));
    let watcher = reader.watch();
    assert!(matches!(
        watcher.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    ));
    until("the follower looked six times", || {
        reader.following().unwrap().looks > 5
    });
    assert!(matches!(watcher.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let following = reader.following().unwrap();
    assert!(following.looks > 5, "{following:?}");
    assert_eq!(following.refreshes, 0);
}

fn waits_out_a_vanished_file(poll: Option<Duration>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gone.ferro");
    let spare = dir.path().join("spare.ferro");
    let writer = fast().open::<Root>(&path).unwrap();
    put(&writer, 1, 1);
    let reader = follower(&path, poll);
    let watcher = reader.watch();
    put(&writer, 2, 2);
    assert_eq!(watcher.recv_timeout(SOON).unwrap().version(), 2);
    drop(writer);
    std::fs::copy(&path, &spare).unwrap();
    std::fs::remove_file(&path).unwrap();
    until("the follower reported the missing file", || {
        reader.following().unwrap().last_error.is_some()
    });
    let error = reader.following().unwrap().last_error.unwrap();
    assert!(error.contains("No such file"), "{error}");
    assert!(matches!(reader.refresh(), Err(Error::Io(_))));
    assert_eq!(reader.version(), 2);
    assert_eq!(*reader.read().get(&2).unwrap(), 2);
    assert!(matches!(watcher.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let back = fast().open::<Root>(&spare).unwrap();
    put(&back, 3, 3);
    drop(back);
    std::fs::rename(&spare, &path).unwrap();
    assert_eq!(watcher.recv_timeout(SOON).unwrap().version(), 3);
    until("the follower cleared its error", || {
        reader.following().unwrap().last_error.is_none()
    });
}

#[test]
fn a_follower_keeps_what_it_saw_while_the_file_is_gone_and_follows_one_put_back() {
    waits_out_a_vanished_file(None);
}

#[test]
fn a_polling_follower_keeps_what_it_saw_while_the_file_is_gone() {
    waits_out_a_vanished_file(QUICK);
}

fn reports_damaged_superblocks(poll: Option<Duration>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("damaged.ferro");
    let writer = fast().open::<Root>(&path).unwrap();
    put(&writer, 1, 1);
    let reader = follower(&path, poll);
    let watcher = reader.watch();
    let mut slots = std::fs::read(&path).unwrap()[..2 * common::SLOT].to_vec();
    slots[0] ^= 1;
    slots[common::SLOT] ^= 1;
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.write_all(&slots).unwrap();
    until("the follower reported the damage", || {
        reader.following().unwrap().last_error.is_some()
    });
    let error = reader.following().unwrap().last_error.unwrap();
    assert!(error.contains("both superblocks are invalid"), "{error}");
    assert_eq!(reader.version(), 1);
    assert!(watcher.try_recv().is_err());
    drop(writer);
}

#[test]
fn a_follower_reports_damaged_superblocks_and_keeps_what_it_saw() {
    reports_damaged_superblocks(None);
}

#[test]
fn a_polling_follower_reports_damaged_superblocks() {
    reports_damaged_superblocks(QUICK);
}

#[test]
fn a_follower_whose_refresh_fails_keeps_trying_and_reporting_until_it_succeeds() {
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("failing.ferro");
    let armed = Arc::new(AtomicBool::new(false));
    let (reached, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let (reached, released) = (
        parking_lot::Mutex::new(reached),
        parking_lot::Mutex::new(released),
    );
    let arm = armed.clone();
    let hook = ferro::db::Hook(std::sync::Arc::new(move |step| {
        if step == "leading" && arm.swap(false, Ordering::AcqRel) {
            let _ = reached.lock().send(());
            let _ = released.lock().recv();
        }
    }));
    let writer = fast().hook(hook).open::<Root>(&path).unwrap();
    put(&writer, 1, 1);
    let reader = follower(&path, QUICK);
    let watcher = reader.watch();
    let start = writer.size_on_disk() + 100;
    armed.store(true, Ordering::Release);
    let held = {
        let writer = writer.clone();
        std::thread::spawn(move || put(&writer, 2, 2))
    };
    waiting.recv().unwrap();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut byte = [0u8];
    file.seek(SeekFrom::Start(start)).unwrap();
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(start)).unwrap();
    file.write_all(&[byte[0] ^ 0x10]).unwrap();
    release.send(()).unwrap();
    held.join().unwrap();
    until("the follower reported the damage", || {
        reader.following().unwrap().last_error.is_some()
    });
    let looks = reader.following().unwrap().looks;
    until("the follower looked again", || {
        reader.following().unwrap().looks > looks + 10
    });
    let following = reader.following().unwrap();
    assert!(following.last_error.is_some(), "{following:?}");
    assert_eq!(reader.version(), 1);
    assert!(watcher.try_recv().is_err());
    file.seek(SeekFrom::Start(start)).unwrap();
    file.write_all(&byte).unwrap();
    assert_eq!(watcher.recv_timeout(SOON).unwrap().version(), 2);
    until("the follower cleared its error", || {
        reader.following().unwrap().last_error.is_none()
    });
}

#[test]
fn a_test_on_a_followers_watcher_may_refresh_while_the_follower_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reentry.ferro");
    let writer = fast().open::<Root>(&path).unwrap();
    let reader = follower(&path, None);
    let probe = reader.clone();
    let watcher = reader.watch_when(move |root| {
        probe.refresh().is_ok() && probe.branch("main").is_ok() && root.contains_key(&50)
    });
    let filling = std::thread::spawn(move || {
        for n in 1..=50 {
            put(&writer, n, n);
        }
        writer
    });
    let seen = within("a watcher whose test refreshes", move || {
        watcher.recv_timeout(SOON).map(|seen| seen.version())
    });
    assert_eq!(seen, Ok(50));
    drop(filling.join().unwrap());
    assert_eq!(reader.following().unwrap().last_error, None);
}

#[cfg(target_os = "linux")]
mod lifetime {
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use ferro::db::Hook;

    use super::*;

    fn threads(name: &str) -> usize {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(Result::ok)
            .filter(|task| {
                std::fs::read_to_string(task.path().join("comm"))
                    .is_ok_and(|comm| comm.trim() == name)
            })
            .count()
    }

    fn descriptors(to: impl Fn(&str) -> bool) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|fd| std::fs::read_link(fd.path()).ok())
            .filter(|target| to(&target.to_string_lossy()))
            .count()
    }

    fn on(path: &Path) -> impl Fn(&str) -> bool {
        let path = std::fs::canonicalize(path).unwrap();
        let path = path.to_string_lossy().into_owned();
        move |target| target.starts_with(path.as_str())
    }

    fn watches(target: &str) -> bool {
        target == "anon_inode:inotify" || target == "anon_inode:[eventfd]"
    }

    fn ended() {
        let deadline = Instant::now() + Duration::from_secs(1);
        while threads("ferro-follow") > 0 {
            assert!(
                Instant::now() < deadline,
                "the follower thread outlived the drop"
            );
            std::thread::yield_now();
        }
    }

    fn released(path: &Path, poll: Option<Duration>) {
        let base = descriptors(on(path));
        let reader = follower(path, poll);
        let (clone, side) = (reader.clone(), reader.branch("main").unwrap());
        let watcher = reader.watch();
        until("the follower thread started", || {
            threads("ferro-follow") == 1
        });
        assert!(descriptors(on(path)) > base);
        assert_eq!(descriptors(watches), 2 * usize::from(poll.is_none()));
        drop((reader, clone, side));
        assert_eq!(
            descriptors(on(path)),
            base,
            "the file is open after the drop"
        );
        assert_eq!(descriptors(watches), 0, "a watch is open after the drop");
        assert!(matches!(
            watcher.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        ended();
    }

    fn lone_writer(path: &Path) {
        let shared = fast().shared(true).follow(true).open::<Root>(path).unwrap();
        let watcher = shared.watch();
        until("the follower thread started", || {
            threads("ferro-follow") == 1
        });
        assert!(matches!(fast().open::<Root>(path), Err(Error::Locked)));
        drop(shared);
        assert_eq!(descriptors(on(path)), 0, "the file is open after the drop");
        assert_eq!(descriptors(watches), 0, "a watch is open after the drop");
        drop(fast().open::<Root>(path).unwrap());
        assert!(matches!(
            watcher.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        ended();
    }

    fn joined_inside_a_look(path: &Path) {
        let writer = fast().shared(true).open::<Root>(path).unwrap();
        let base = descriptors(on(path));
        let armed = Arc::new(AtomicBool::new(true));
        let (entered, inside) = mpsc::channel();
        let entered = parking_lot::Mutex::new(entered);
        let hook = Hook(Arc::new(move |step| {
            if step == "adopted" && armed.swap(false, Ordering::AcqRel) {
                let _ = entered.lock().send(());
                std::thread::sleep(Duration::from_millis(300));
            }
        }));
        let follower = fast()
            .shared(true)
            .follow(true)
            .hook(hook)
            .open::<Root>(path)
            .unwrap();
        put(&writer, 2, 2);
        inside.recv_timeout(SOON).unwrap();
        let started = Instant::now();
        drop(follower);
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(200),
            "the drop returned after {took:?}, before the look in progress"
        );
        assert_eq!(
            descriptors(on(path)),
            base,
            "the file is open after the drop"
        );
        assert_eq!(descriptors(watches), 0, "a watch is open after the drop");
        ended();
    }

    #[test]
    fn follow_child() {
        let Ok(path) = std::env::var("FERRO_FOLLOW_PATH") else {
            return;
        };
        let path = Path::new(&path);
        let writer = fast().open::<Root>(path).unwrap();
        put(&writer, 1, 1);
        released(path, None);
        released(path, QUICK);
        drop(writer);
        if cfg!(target_pointer_width = "64") {
            lone_writer(path);
            joined_inside_a_look(path);
        }
        println!("RELEASED");
    }

    #[test]
    fn dropping_the_last_handle_ends_the_follower_thread_and_releases_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lifetime::follow_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("FERRO_FOLLOW_PATH", dir.path().join("drop.ferro"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stdout}{stderr}");
        assert!(stdout.contains("RELEASED"), "{stdout}{stderr}");
    }
}
