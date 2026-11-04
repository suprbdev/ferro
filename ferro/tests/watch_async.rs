mod common;

use std::collections::HashSet;
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread, ThreadId};
use std::time::{Duration, Instant};

use ferro::{Db, Map, Snapshot, Watcher};

type Root = Map<u64, u64>;

struct Signal {
    thread: Thread,
    woken: AtomicUsize,
}

impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        self.thread.unpark();
    }
}

fn signal() -> (Arc<Signal>, Waker) {
    let signal = Arc::new(Signal {
        thread: thread::current(),
        woken: AtomicUsize::new(0),
    });
    (signal.clone(), Waker::from(signal))
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let (_, waker) = signal();
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        thread::park();
    }
}

fn poll<F: Future + Unpin>(future: &mut F, waker: &Waker) -> Poll<F::Output> {
    Pin::new(future).poll(&mut Context::from_waker(waker))
}

fn woken(signal: &Signal, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while signal.woken.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "{what} never woke");
        thread::sleep(Duration::from_millis(1));
    }
}

fn within<R: Send + 'static>(what: &str, run: impl FnOnce() -> R + Send + 'static) -> R {
    let (done, finished) = channel();
    thread::spawn(move || {
        let _ = done.send(run());
    });
    match finished.recv_timeout(Duration::from_secs(20)) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => panic!("{what} deadlocked"),
        Err(RecvTimeoutError::Disconnected) => panic!("{what} panicked"),
    }
}

fn opened() -> (tempfile::TempDir, Db<Root>) {
    let dir = tempfile::tempdir().unwrap();
    let db = common::fast()
        .open::<Root>(dir.path().join("watch.ferro"))
        .unwrap();
    (dir, db)
}

fn put(db: &Db<Root>, key: u64, value: u64) {
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
}

fn kinds(db: &Db<Root>) -> Vec<Watcher<Root>> {
    (0..3).map(|kind| fresh(db, kind)).collect()
}

fn fresh(db: &Db<Root>, kind: usize) -> Watcher<Root> {
    match kind {
        0 => db.watch(),
        1 => db.watch_when(|snapshot| snapshot.contains_key(&7)),
        _ => db.watch_changes(|seen, now| seen.get(&7) != now.get(&7)),
    }
}

#[test]
fn a_changed_future_can_cross_threads() {
    fn sendable<F: Send>(_: &F) {}
    let (_dir, db) = opened();
    let watcher = db.watch_when(|_| true);
    sendable(&watcher.changed());
}

#[test]
fn changed_resolves_with_the_next_commit() {
    let (_dir, db) = opened();
    put(&db, 1, 1);
    for kind in 0..3 {
        let watcher = fresh(&db, kind);
        let waiting =
            thread::spawn(move || block_on(watcher.changed()).map(|snapshot| snapshot.version()));
        thread::sleep(Duration::from_millis(20));
        let before = db.version();
        put(&db, 1, before + 1);
        put(&db, 7, before);
        let got = within("an async receiver", move || waiting.join().unwrap()).unwrap();
        assert!(got > before);
        assert!(got <= db.version());
    }
}

#[test]
fn changed_returns_at_once_when_a_commit_is_already_waiting() {
    let (_dir, db) = opened();
    for kind in 0..3 {
        let watcher = fresh(&db, kind);
        put(&db, 7, db.version());
        let (signal, waker) = signal();
        let mut future = watcher.changed();
        let Poll::Ready(Ok(snapshot)) = poll(&mut future, &waker) else {
            panic!("a waiting commit was not returned");
        };
        assert_eq!(snapshot.version(), db.version());
        assert_eq!(signal.woken.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn a_filtered_changed_wakes_only_when_its_test_passes() {
    let (_dir, db) = opened();
    let on_committer = Arc::new(AtomicUsize::new(0));
    let committer = thread::current().id();
    let watcher = {
        let on_committer = on_committer.clone();
        db.watch_changes(move |seen, now| {
            if thread::current().id() == committer {
                on_committer.fetch_add(1, Ordering::SeqCst);
            }
            seen.get(&7) != now.get(&7)
        })
    };
    let (signal, waker) = signal();
    let mut future = watcher.changed();
    assert!(poll(&mut future, &waker).is_pending());
    for value in 0..50 {
        put(&db, 1, value);
    }
    thread::sleep(Duration::from_millis(30));
    assert_eq!(signal.woken.load(Ordering::SeqCst), 0);
    put(&db, 7, 7);
    woken(&signal, "a passing commit");
    assert_eq!(on_committer.load(Ordering::SeqCst), 0);
    let Poll::Ready(Ok(snapshot)) = poll(&mut future, &waker) else {
        panic!("a woken receiver found nothing");
    };
    assert_eq!(snapshot.get(&7).as_deref(), Some(&7));
}

#[test]
fn a_closer_ends_a_pending_changed() {
    let (_dir, db) = opened();
    for watcher in kinds(&db) {
        let (signal, waker) = signal();
        let mut future = watcher.changed();
        assert!(poll(&mut future, &waker).is_pending());
        watcher.closer().close();
        assert_eq!(signal.woken.load(Ordering::SeqCst), 1);
        assert!(matches!(poll(&mut future, &waker), Poll::Ready(Err(_))));
    }
}

#[test]
fn dropping_the_database_ends_a_pending_changed() {
    let (_dir, db) = opened();
    let side = db.fork("side").unwrap();
    let pending: Vec<_> = kinds(&db)
        .into_iter()
        .chain(kinds(&side))
        .map(|watcher| (watcher, signal()))
        .collect();
    let mut futures: Vec<_> = pending
        .iter()
        .map(|(watcher, _)| watcher.changed())
        .collect();
    for (future, (_, (_, waker))) in futures.iter_mut().zip(&pending) {
        assert!(poll(future, waker).is_pending());
    }
    drop(side);
    db.drop_branch("side").unwrap();
    for (future, (_, (signal, waker))) in futures.iter_mut().zip(&pending).skip(3) {
        woken(signal, "a receiver on a dropped branch");
        assert!(matches!(poll(future, waker), Poll::Ready(Err(_))));
    }
    drop(db);
    for (future, (_, (signal, waker))) in futures.iter_mut().zip(&pending).take(3) {
        woken(signal, "a receiver on a closed database");
        assert!(matches!(poll(future, waker), Poll::Ready(Err(_))));
    }
}

#[test]
fn dropping_a_pending_changed_forgets_its_waker() {
    let (_dir, db) = opened();
    for kind in 0..3 {
        let watcher = fresh(&db, kind);
        let (signal, waker) = signal();
        let mut future = watcher.changed();
        assert!(poll(&mut future, &waker).is_pending());
        assert!(poll(&mut future, &waker).is_pending());
        assert_eq!(Arc::strong_count(&signal), 3);
        drop(future);
        assert_eq!(Arc::strong_count(&signal), 2);
        put(&db, 7, db.version());
        thread::sleep(Duration::from_millis(20));
        assert_eq!(signal.woken.load(Ordering::SeqCst), 0);
        assert!(watcher.try_recv().is_ok());
    }
}

#[test]
fn a_dropped_waiting_watcher_lets_go_of_its_test() {
    let (_dir, db) = opened();
    let held = Arc::new(());
    let watcher = {
        let held = held.clone();
        db.watch_when(move |_| {
            let _ = &held;
            false
        })
    };
    let (signal, waker) = signal();
    let mut future = watcher.changed();
    assert!(poll(&mut future, &waker).is_pending());
    put(&db, 1, 1);
    drop(future);
    assert_eq!(Arc::strong_count(&signal), 2);
    drop(watcher);
    let deadline = Instant::now() + Duration::from_secs(20);
    while Arc::strong_count(&held) > 1 {
        assert!(
            Instant::now() < deadline,
            "the feed kept a dropped watcher's test"
        );
        thread::sleep(Duration::from_millis(1));
    }
    for value in 2..20 {
        put(&db, 1, value);
    }
    assert_eq!(signal.woken.load(Ordering::SeqCst), 0);
}

#[test]
fn a_waker_that_changes_is_the_one_woken() {
    let (_dir, db) = opened();
    let watcher = db.watch_when(|snapshot| snapshot.contains_key(&7));
    let (first, first_waker) = signal();
    let (second, second_waker) = signal();
    let mut future = watcher.changed();
    assert!(poll(&mut future, &first_waker).is_pending());
    assert!(poll(&mut future, &second_waker).is_pending());
    assert_eq!(Arc::strong_count(&first), 2);
    put(&db, 7, 7);
    woken(&second, "the newer waker");
    assert_eq!(first.woken.load(Ordering::SeqCst), 0);
    assert!(matches!(
        poll(&mut future, &second_waker),
        Poll::Ready(Ok(_))
    ));
}

#[test]
fn many_pending_futures_wake_only_where_their_test_passes() {
    let (_dir, db) = opened();
    let on_committer = Arc::new(AtomicUsize::new(0));
    let committer = thread::current().id();
    let watchers: Vec<_> = (0..1000u64)
        .map(|index| {
            let on_committer = on_committer.clone();
            db.watch_when(move |snapshot| {
                if thread::current().id() == committer {
                    on_committer.fetch_add(1, Ordering::SeqCst);
                }
                snapshot.contains_key(&(1000 + index))
            })
        })
        .collect();
    let signals: Vec<_> = watchers.iter().map(|_| signal()).collect();
    let mut futures: Vec<_> = watchers.iter().map(Watcher::changed).collect();
    for (future, (_, waker)) in futures.iter_mut().zip(&signals) {
        assert!(poll(future, waker).is_pending());
    }
    for value in 0..20 {
        put(&db, 1, value);
    }
    put(&db, 1500, 0);
    woken(&signals[500].0, "the one receiver whose test passed");
    thread::sleep(Duration::from_millis(20));
    let rung: Vec<usize> = signals
        .iter()
        .enumerate()
        .filter(|(_, (signal, _))| signal.woken.load(Ordering::SeqCst) > 0)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(rung, vec![500]);
    assert_eq!(on_committer.load(Ordering::SeqCst), 0);
    let Poll::Ready(Ok(snapshot)) = poll(&mut futures[500], &signals[500].1) else {
        panic!("the woken receiver found nothing");
    };
    assert!(snapshot.contains_key(&1500));
}

#[test]
fn a_long_round_is_shared_between_watch_threads() {
    let (_dir, db) = opened();
    let ran_on: Arc<Mutex<HashSet<ThreadId>>> = Arc::default();
    let watchers: Vec<_> = (0..1100u64)
        .map(|index| {
            let ran_on = ran_on.clone();
            db.watch_when(move |snapshot| {
                let until = Instant::now() + Duration::from_micros(3);
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
                ran_on.lock().unwrap().insert(thread::current().id());
                snapshot.contains_key(&(10_000 + index))
            })
        })
        .collect();
    let signals: Vec<_> = watchers.iter().map(|_| signal()).collect();
    let mut futures: Vec<_> = watchers.iter().map(Watcher::changed).collect();
    for (future, (_, waker)) in futures.iter_mut().zip(&signals) {
        assert!(poll(future, waker).is_pending());
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut value = 0;
    while ran_on.lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline, "no round was ever shared");
        value += 1;
        put(&db, 1, value);
        thread::sleep(Duration::from_millis(10));
    }
    put(&db, 10_777, 0);
    woken(
        &signals[777].0,
        "the receiver whose test passed in a shared round",
    );
    thread::sleep(Duration::from_millis(20));
    let rung: Vec<usize> = signals
        .iter()
        .enumerate()
        .filter(|(_, (signal, _))| signal.woken.load(Ordering::SeqCst) > 0)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(rung, vec![777]);
    assert!(!ran_on.lock().unwrap().contains(&thread::current().id()));
    assert!(matches!(
        poll(&mut futures[777], &signals[777].1),
        Poll::Ready(Ok(_))
    ));
    drop(futures);
    drop(db);
}

#[test]
fn async_receivers_never_see_versions_out_of_order() {
    let (_dir, db) = opened();
    let seen: Arc<Mutex<Vec<Vec<u64>>>> = Arc::default();
    let consumers: Vec<_> = kinds(&db)
        .into_iter()
        .map(|watcher| {
            let seen = seen.clone();
            thread::spawn(move || {
                let mut versions = Vec::new();
                let last = block_on(async {
                    loop {
                        let snapshot: Snapshot<Root> = watcher.changed().await?;
                        versions.push(snapshot.version());
                        if snapshot.contains_key(&99) {
                            return Ok::<_, std::sync::mpsc::RecvError>(snapshot.version());
                        }
                    }
                });
                seen.lock().unwrap().push(versions);
                last
            })
        })
        .collect();
    let writers: Vec<_> = (0..4)
        .map(|writer| {
            let db = db.clone();
            thread::spawn(move || {
                for step in 0..200 {
                    put(&db, if writer == 0 { 7 } else { writer }, step);
                }
            })
        })
        .collect();
    writers
        .into_iter()
        .for_each(|writer| writer.join().unwrap());
    db.update(|root| {
        root.insert(7, 1000);
        root.insert(99, 99);
    })
    .unwrap();
    let last = db.version();
    for consumer in consumers {
        assert_eq!(
            within("an async consumer", move || consumer.join().unwrap()).unwrap(),
            last
        );
    }
    for versions in seen.lock().unwrap().iter() {
        assert!(
            versions.windows(2).all(|pair| pair[0] < pair[1]),
            "{versions:?}"
        );
    }
}

#[test]
fn an_async_test_may_commit() {
    let (_dir, db) = opened();
    let fired = Arc::new(AtomicUsize::new(0));
    let watcher = {
        let (db, fired) = (db.clone(), fired.clone());
        db.clone().watch_when(move |snapshot| {
            if snapshot.contains_key(&1) && fired.fetch_add(1, Ordering::SeqCst) == 0 {
                put(&db, 2, 2);
            }
            snapshot.contains_key(&2)
        })
    };
    let waiting =
        thread::spawn(move || block_on(watcher.changed()).map(|snapshot| snapshot.version()));
    thread::sleep(Duration::from_millis(20));
    let writer = db.clone();
    within("a commit whose test commits", move || put(&writer, 1, 1));
    assert_eq!(
        within("the async receiver", move || waiting.join().unwrap()).unwrap(),
        2
    );
}
