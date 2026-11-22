//! Watchers: subscriptions to a branch's new snapshots.
//!
//! [`Db::watch`](crate::Db::watch), [`Db::watch_when`](crate::Db::watch_when) and
//! [`Db::watch_changes`](crate::Db::watch_changes) return a [`Watcher`]. Each branch has one feed
//! that holds its newest snapshot. A commit stores its snapshot there and wakes the branch's watch
//! thread, so it costs the same with no watchers or ten thousand and never waits for a filter. A
//! watcher receives by blocking, with a timeout, without waiting, or by awaiting [`Changed`], and a
//! [`Closer`] ends it from any thread.
//! [Watchers](https://github.com/suprbdev/ferro/blob/main/docs/internals/watchers.md) in the
//! internals describes the feed and the watch thread.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvError, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, OnceLock, Weak};
use std::task::{Context, Poll, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::db::{Commit, Snapshot};

type Each<T> = Box<dyn Fn(&Snapshot<T>) -> bool + Send + Sync>;
type Pair<T> = Box<dyn Fn(&Snapshot<T>, &Snapshot<T>) -> bool + Send + Sync>;

/// Which snapshots a watcher hands out.
pub(crate) enum Filter<T> {
    /// Every one.
    All,
    /// Those that pass the test.
    When(Each<T>),
    /// Those that pass the test against the snapshot the watcher handed out last.
    Changes(Pair<T>),
}

struct Gate<T> {
    filter: Filter<T>,
    seen: Mutex<Option<(u64, Snapshot<T>)>>,
    closed: AtomicBool,
    listed: AtomicBool,
    waiting: AtomicUsize,
    approved: AtomicU64,
    keys: AtomicU64,
    wakers: Mutex<Vec<(u64, Waker)>>,
    passed: Condvar,
}

impl<T> Gate<T> {
    fn new(filter: Filter<T>, seen: Option<(u64, Snapshot<T>)>) -> Arc<Self> {
        Arc::new(Gate {
            filter,
            seen: Mutex::new(seen),
            closed: AtomicBool::new(false),
            listed: AtomicBool::new(false),
            waiting: AtomicUsize::new(0),
            approved: AtomicU64::new(0),
            keys: AtomicU64::new(0),
            wakers: Mutex::new(Vec::new()),
            passed: Condvar::new(),
        })
    }

    fn filtered(&self) -> bool {
        !matches!(self.filter, Filter::All)
    }

    fn admits(&self, now: &Snapshot<T>) -> bool {
        match &self.filter {
            Filter::All => true,
            Filter::When(test) => test(now),
            Filter::Changes(test) => {
                let seen = self.seen.lock().as_ref().map(|(_, seen)| seen.clone());
                seen.is_none_or(|seen| test(&seen, now))
            }
        }
    }

    fn approved(&self, published: u64) -> bool {
        matches!(self.filter, Filter::When(_)) && self.approved.load(Ordering::Acquire) == published
    }

    fn remember(&self, count: u64, snapshot: &Snapshot<T>) {
        if !matches!(self.filter, Filter::Changes(_)) {
            return;
        }
        let mut seen = self.seen.lock();
        if seen.as_ref().is_some_and(|(was, _)| *was >= count) {
            return;
        }
        let replaced = seen.replace((count, snapshot.clone()));
        drop(seen);
        drop(replaced);
    }

    fn wake(&self, waiting: &AtomicUsize) {
        self.passed.notify_all();
        let wakers = mem::take(&mut *self.wakers.lock());
        if !wakers.is_empty() {
            self.waiting.fetch_sub(wakers.len(), Ordering::SeqCst);
            waiting.fetch_sub(wakers.len(), Ordering::SeqCst);
        }
        for (_, waker) in wakers {
            waker.wake();
        }
    }
}

struct Slot<T> {
    latest: Option<Snapshot<T>>,
    published: u64,
    closed: bool,
}

struct Plan<T> {
    gates: Vec<Weak<Gate<T>>>,
    prune: usize,
    closed: bool,
}

struct Tracked<T> {
    gates: Vec<Weak<Gate<T>>>,
    prune: usize,
}

/// The snapshots that a feed's [`Db::watch_changes`](crate::Db::watch_changes) watchers compare
/// against, from [`Feed::seats`]: each watcher with the place it had seen, and each commit seen
/// with the snapshot of it to put in place, if one was found.
///
/// [`Seats::seat`] puts them in place, and the notice it returns holds the gates and every
/// snapshot read or replaced, so that none of them is dropped under the caller's locks.
pub(crate) struct Seats<T> {
    gates: Vec<(Arc<Gate<T>>, u64, usize)>,
    commits: Vec<(Snapshot<T>, Option<Snapshot<T>>)>,
    replaced: Vec<Snapshot<T>>,
}

impl<T> Seats<T> {
    /// Puts each found snapshot in place of the one its watcher compares against, unless the
    /// watcher has returned a newer snapshot since [`Feed::seats`] read it, or `current` says the
    /// found snapshots no longer come from the file the branch reads.
    ///
    /// `current` is asked under each watcher's lock, so a later swap, which changes the file the
    /// branch reads before it reads the watchers, either stops this from putting an older file's
    /// snapshot in place or reads the snapshot this put there and moves it on.
    pub(crate) fn seat(mut self, current: impl Fn() -> bool) -> Notice<T> {
        for (gate, count, at) in &self.gates {
            let (seen, Some(found)) = &self.commits[*at] else {
                continue;
            };
            let mut held = gate.seen.lock();
            if let Some((was, held)) = held.as_mut()
                && *was == *count
                && ptr::eq(held.commit(), seen.commit())
                && current()
            {
                self.replaced.push(mem::replace(held, found.clone()));
            }
        }
        Notice {
            feed: None,
            replaced: None,
            seats: Some(self),
            later: None,
        }
    }
}

/// What publishing or replacing a snapshot leaves to do: wake the branch's watch thread after a
/// publish, free the snapshot replaced, or run work that must wait for the caller's locks.
///
/// All of it happens when it is dropped, so a caller keeps it until it has released its locks.
#[must_use]
pub(crate) struct Notice<T> {
    feed: Option<Arc<Feed<T>>>,
    replaced: Option<Snapshot<T>>,
    seats: Option<Seats<T>>,
    later: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl<T> Notice<T> {
    /// A notice that only frees `snapshot`, so that a value replaced under a lock is dropped once
    /// the lock is released.
    pub(crate) fn freeing(snapshot: Snapshot<T>) -> Self {
        Notice {
            feed: None,
            replaced: Some(snapshot),
            seats: None,
            later: None,
        }
    }

    /// A notice that runs `work` when it is dropped, once the caller has released its locks.
    pub(crate) fn later(work: impl FnOnce() + Send + Sync + 'static) -> Self {
        Notice {
            feed: None,
            replaced: None,
            seats: None,
            later: Some(Box::new(work)),
        }
    }
}

impl<T> Drop for Notice<T> {
    fn drop(&mut self) {
        if let Some(feed) = self.feed.take() {
            feed.kick();
        }
        self.replaced.take();
        self.seats.take();
        if let Some(work) = self.later.take() {
            work();
        }
    }
}

/// A branch's newest snapshot and the watchers waiting on it.
pub(crate) struct Feed<T> {
    slot: Mutex<Slot<T>>,
    fresh: Condvar,
    plan: Mutex<Plan<T>>,
    tracked: Mutex<Tracked<T>>,
    waiting: AtomicUsize,
    due: AtomicBool,
    notifier: OnceLock<Thread>,
}

impl<T> Feed<T> {
    /// A feed holding `current`, which the watchers made from it start at.
    pub(crate) fn new(current: Snapshot<T>) -> Self {
        Feed {
            slot: Mutex::new(Slot {
                latest: Some(current),
                published: 0,
                closed: false,
            }),
            fresh: Condvar::new(),
            plan: Mutex::new(Plan {
                gates: Vec::new(),
                prune: 64,
                closed: false,
            }),
            tracked: Mutex::new(Tracked {
                gates: Vec::new(),
                prune: 64,
            }),
            waiting: AtomicUsize::new(0),
            due: AtomicBool::new(false),
            notifier: OnceLock::new(),
        }
    }

    /// Makes `snapshot` the newest in place of `previous` and wakes the watchers that take every
    /// snapshot.
    ///
    /// Filtered and async watchers wake when the returned [`Notice`] is dropped, and the snapshot
    /// replaced is freed then. A closed feed keeps nothing, and its notice frees `previous`.
    pub(crate) fn publish(
        self: &Arc<Self>,
        snapshot: Snapshot<T>,
        previous: &Snapshot<T>,
    ) -> Notice<T> {
        let mut slot = self.slot.lock();
        if slot.closed {
            drop(slot);
            return Notice {
                feed: None,
                replaced: Some(previous.clone()),
                seats: None,
                later: None,
            };
        }
        let replaced = slot.latest.replace(snapshot);
        slot.published += 1;
        drop(slot);
        self.fresh.notify_all();
        Notice {
            feed: Some(self.clone()),
            replaced,
            seats: None,
            later: None,
        }
    }

    fn kick(&self) {
        if self.waiting.load(Ordering::SeqCst) == 0 || self.due.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(notifier) = self.notifier.get() {
            notifier.unpark();
        }
    }

    /// Replaces the newest snapshot with `snapshot` of the same version, read from another file,
    /// without waking any watcher.
    ///
    /// The snapshot it replaced is freed when the returned [`Notice`] is dropped.
    pub(crate) fn rebind(&self, snapshot: Snapshot<T>) -> Notice<T> {
        let mut slot = self.slot.lock();
        let replaced = match slot.closed {
            true => Some(snapshot),
            false => slot.latest.replace(snapshot),
        };
        drop(slot);
        Notice {
            feed: None,
            replaced,
            seats: None,
            later: None,
        }
    }

    /// Lists the gate of a [`Db::watch_changes`](crate::Db::watch_changes) watcher by a `Weak`, so
    /// that [`Feed::seats`] can reach the snapshot it compares against.
    fn track(&self, gate: &Arc<Gate<T>>) {
        let mut tracked = self.tracked.lock();
        if tracked.gates.len() >= tracked.prune {
            tracked.gates.retain(|gate| gate.strong_count() > 0);
            tracked.prune = (tracked.gates.len() * 2).max(64);
        }
        tracked.gates.push(Arc::downgrade(gate));
    }

    /// Reads the snapshot each [`Db::watch_changes`](crate::Db::watch_changes) watcher of the feed
    /// compares against, and asks `find` for one of the same commit to put in its place, as a
    /// cycle's swap puts the feed's own snapshot in place with [`Feed::rebind`].
    ///
    /// `find` runs once for each commit the watchers saw, whichever snapshot of it they hold.
    /// Nothing changes until [`Seats::seat`], and a gate's lock is held only to read what it saw,
    /// never while `find` runs.
    pub(crate) fn seats(
        &self,
        mut find: impl FnMut(&Snapshot<T>) -> Option<Snapshot<T>>,
    ) -> Seats<T> {
        let live: Vec<Arc<Gate<T>>> = {
            let mut tracked = self.tracked.lock();
            tracked.gates.retain(|gate| gate.strong_count() > 0);
            tracked.prune = (tracked.gates.len() * 2).max(64);
            tracked.gates.iter().filter_map(Weak::upgrade).collect()
        };
        let mut gates = Vec::with_capacity(live.len());
        let mut commits = Vec::new();
        let mut places: HashMap<*const Commit, usize> = HashMap::new();
        for gate in live {
            let read = {
                let seen = gate.seen.lock();
                seen.as_ref().map(|(count, seen)| {
                    let commit = ptr::from_ref(seen.commit());
                    let first = (!places.contains_key(&commit)).then(|| seen.clone());
                    (*count, commit, first)
                })
            };
            let Some((count, commit, first)) = read else {
                continue;
            };
            let at = match first {
                Some(seen) => {
                    let found = find(&seen);
                    commits.push((seen, found));
                    places.insert(commit, commits.len() - 1);
                    commits.len() - 1
                }
                None => places[&commit],
            };
            gates.push((gate, count, at));
        }
        Seats {
            gates,
            commits,
            replaced: Vec::new(),
        }
    }

    /// Ends every watcher of the feed and stops its watch thread.
    pub(crate) fn close(&self) {
        let mut slot = self.slot.lock();
        slot.closed = true;
        let replaced = slot.latest.take();
        drop(slot);
        self.fresh.notify_all();
        self.plan.lock().closed = true;
        self.due.store(true, Ordering::SeqCst);
        if let Some(notifier) = self.notifier.get() {
            notifier.unpark();
        }
        drop(replaced);
    }

    fn offer(&self, gate: &Weak<Gate<T>>, latest: &Snapshot<T>, published: u64) -> bool {
        let Some(gate) = gate.upgrade() else {
            return false;
        };
        if gate.closed.load(Ordering::Acquire) {
            return false;
        }
        if gate.waiting.load(Ordering::SeqCst) == 0 {
            return true;
        }
        let passed = panic::catch_unwind(AssertUnwindSafe(|| gate.admits(latest)));
        if let Ok(true) = passed {
            gate.approved.store(published, Ordering::Release);
        }
        if passed.unwrap_or(true) {
            gate.wake(&self.waiting);
        }
        true
    }

    fn release(&self, gates: Vec<Weak<Gate<T>>>) {
        for gate in gates {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                if let Some(gate) = gate.upgrade() {
                    gate.wake(&self.waiting);
                }
            }));
        }
    }

    fn sift(&self, share: Share<T>) -> Vec<Weak<Gate<T>>> {
        let mut kept = Vec::with_capacity(share.gates.len());
        for gate in share.gates {
            let offered = AssertUnwindSafe(|| self.offer(&gate, &share.latest, share.published));
            if panic::catch_unwind(offered).unwrap_or(true) {
                kept.push(gate);
            }
        }
        kept
    }
}

struct Share<T> {
    gates: Vec<Weak<Gate<T>>>,
    latest: Snapshot<T>,
    published: u64,
}

const SHARE: usize = 512;
const SLOW: u64 = 1_000_000;
const PROBE: u64 = 64;

struct Crew<T> {
    hands: Vec<mpsc::Sender<Share<T>>>,
    done: Option<mpsc::Receiver<Vec<Weak<Gate<T>>>>>,
    alone: u64,
    shared: u64,
    rounds: u64,
}

impl<T: Send + Sync + 'static> Crew<T> {
    fn new() -> Self {
        Crew {
            hands: Vec::new(),
            done: None,
            alone: 0,
            shared: 0,
            rounds: 0,
        }
    }

    fn shares(&self, gates: usize) -> bool {
        if gates < 2 * SHARE || self.alone.saturating_mul(gates as u64) < SLOW {
            return false;
        }
        let faster = self.shared == 0 || self.shared < self.alone;
        faster != self.rounds.is_multiple_of(PROBE)
    }

    fn record(&mut self, shared: bool, gates: usize, took: Duration) {
        let cost = (took.as_nanos() / gates.max(1) as u128).max(1) as u64;
        let estimate = match shared {
            false => &mut self.alone,
            true => &mut self.shared,
        };
        *estimate = match *estimate {
            0 => cost,
            was => (was * 3 + cost) / 4,
        };
        self.rounds += 1;
    }

    fn hire(&mut self, feed: &Arc<Feed<T>>) {
        if self.done.is_some() {
            return;
        }
        let (finished, done) = mpsc::channel();
        let helpers = thread::available_parallelism().map_or(1, |cores| cores.get() / 4);
        for _ in 0..helpers.clamp(1, 7) {
            let (sender, shares) = mpsc::channel::<Share<T>>();
            let (feed, finished) = (feed.clone(), finished.clone());
            let spawned = thread::Builder::new()
                .name("ferro-watch".to_string())
                .spawn(move || {
                    for share in shares {
                        if finished.send(feed.sift(share)).is_err() {
                            return;
                        }
                    }
                });
            if spawned.is_ok() {
                self.hands.push(sender);
            }
        }
        self.done = Some(done);
    }
}

impl<T: Send + Sync + 'static> Feed<T> {
    fn split(self: &Arc<Self>, share: Share<T>, crew: &mut Crew<T>) -> Vec<Weak<Gate<T>>> {
        let count = share.gates.len();
        let started = Instant::now();
        let parts = match crew.shares(count) {
            true => {
                crew.hire(self);
                (count / SHARE).min(crew.hands.len() + 1)
            }
            false => 1,
        };
        let Share {
            mut gates,
            latest,
            published,
        } = share;
        let size = gates.len().div_ceil(parts);
        let mut sent = 0;
        for hand in crew.hands.iter().take(parts - 1) {
            let part = Share {
                gates: gates.split_off(gates.len() - size),
                latest: latest.clone(),
                published,
            };
            match hand.send(part) {
                Ok(()) => sent += 1,
                Err(mpsc::SendError(part)) => gates.extend(part.gates),
            }
        }
        let mut kept = self.sift(Share {
            gates,
            latest,
            published,
        });
        if let Some(done) = &crew.done {
            for _ in 0..sent {
                match done.recv() {
                    Ok(mut part) => kept.append(&mut part),
                    Err(_) => break,
                }
            }
        }
        crew.record(parts > 1, count, started.elapsed());
        kept
    }

    fn round(self: &Arc<Self>, crew: &mut Crew<T>) -> bool {
        let (latest, published) = {
            let slot = self.slot.lock();
            match (&slot.latest, slot.closed) {
                (Some(latest), false) => (latest.clone(), slot.published),
                _ => {
                    drop(slot);
                    let gates = mem::take(&mut self.plan.lock().gates);
                    self.release(gates);
                    return false;
                }
            }
        };
        let gates = mem::take(&mut self.plan.lock().gates);
        let share = Share {
            gates,
            latest,
            published,
        };
        let mut kept = self.split(share, crew);
        let mut plan = self.plan.lock();
        if plan.closed {
            kept.append(&mut plan.gates);
            drop(plan);
            self.release(kept);
            return false;
        }
        kept.append(&mut plan.gates);
        plan.gates = kept;
        true
    }

    fn serve(self: Arc<Self>) {
        let mut crew = Crew::new();
        loop {
            if !self.due.swap(false, Ordering::SeqCst) {
                thread::park();
                continue;
            }
            if !self.round(&mut crew) {
                return;
            }
        }
    }

    fn enlist(self: &Arc<Self>, gate: &Arc<Gate<T>>) {
        if gate.listed.load(Ordering::Acquire) {
            return;
        }
        let mut plan = self.plan.lock();
        if gate.listed.load(Ordering::Relaxed) || plan.closed {
            return;
        }
        if self.notifier.get().is_none() {
            let feed = self.clone();
            let spawned = thread::Builder::new()
                .name("ferro-watch".to_string())
                .spawn(move || feed.serve())
                .expect("ferro: start the watch thread");
            let _ = self.notifier.set(spawned.thread().clone());
        }
        if plan.gates.len() >= plan.prune {
            plan.gates.retain(|gate| gate.strong_count() > 0);
            plan.prune = (plan.gates.len() * 2).max(64);
        }
        plan.gates.push(Arc::downgrade(gate));
        gate.listed.store(true, Ordering::Release);
    }
}

/// A handle that ends a [`Watcher`] and its clones from any thread, without a commit.
///
/// [`Watcher::closer`] makes one, and clones of it end the same watcher. Closing wakes every
/// receive and every [`Changed`] waiting on that watcher, and they return an error. Other watchers
/// of the branch are not affected.
///
/// # Examples
///
/// ```
/// use std::sync::mpsc::TryRecvError;
///
/// use ferro::Db;
///
/// # fn main() -> ferro::Result<()> {
/// # let dir = tempfile::tempdir()?;
/// # let path = dir.path().join("count.ferro");
/// let db = Db::<u64>::open(&path)?;
/// let watcher = db.watch();
/// let other = db.watch();
///
/// watcher.closer().close();
/// db.update(|count| *count = 1)?;
/// assert!(matches!(watcher.try_recv(), Err(TryRecvError::Disconnected)));
/// assert_eq!(*other.try_recv().expect("a new version"), 1);
/// # Ok(())
/// # }
/// ```
pub struct Closer<T> {
    feed: Arc<Feed<T>>,
    gate: Arc<Gate<T>>,
}

impl<T> Clone for Closer<T> {
    fn clone(&self) -> Self {
        Closer {
            feed: self.feed.clone(),
            gate: self.gate.clone(),
        }
    }
}

impl<T> fmt::Debug for Closer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Closer")
            .field("closed", &self.gate.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl<T> Closer<T> {
    /// Ends the watcher and its clones, waking any that wait.
    ///
    /// Closing twice does nothing more.
    pub fn close(&self) {
        let slot = self.feed.slot.lock();
        self.gate.closed.store(true, Ordering::Release);
        drop(slot);
        self.feed.fresh.notify_all();
        self.gate.wake(&self.feed.waiting);
    }
}

enum Wait {
    Never,
    Until(Instant),
    Forever,
}

enum Stopped {
    Closed,
    Empty,
}

enum Look<T> {
    Closed,
    Found(u64, Snapshot<T>),
    Nothing,
}

/// A subscription to the new snapshots of one branch.
///
/// A watcher starts at the version current when it was made, so its first receive waits for the
/// next commit. It does not queue: each receive returns the newest snapshot it has not returned
/// yet, so a slow consumer skips versions, costs the writers nothing and holds no backlog. As an
/// [`Iterator`] it blocks on [`Watcher::recv`] and ends when the watcher does.
///
/// A watcher ends when its branch is dropped, when its [`Closer`] closes it, or when the last
/// [`Db`](crate::Db) handle on the file is dropped; it does not keep the file open, and a snapshot
/// it had not taken by then is lost. Clones share the closer, the filter and, for
/// [`Db::watch_changes`](crate::Db::watch_changes), what was seen, and each keeps its own place.
///
/// # Panics
///
/// A receive that runs a filter on its own thread panics if the filter panics.
#[must_use = "a watcher hands out new snapshots only while it is held"]
pub struct Watcher<T> {
    feed: Arc<Feed<T>>,
    gate: Arc<Gate<T>>,
    place: AtomicU64,
}

impl<T> Clone for Watcher<T> {
    fn clone(&self) -> Self {
        Watcher {
            feed: self.feed.clone(),
            gate: self.gate.clone(),
            place: AtomicU64::new(self.place.load(Ordering::Relaxed)),
        }
    }
}

impl<T> fmt::Debug for Watcher<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watcher")
            .field("place", &self.place.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<T> Watcher<T> {
    /// A watcher of `feed` that starts at its current snapshot.
    ///
    /// `read` returns the snapshot of the handle it is made from, which a [`Filter::Changes`]
    /// watcher has seen instead when the feed is closed and holds none.
    pub(crate) fn new(
        feed: Arc<Feed<T>>,
        filter: Filter<T>,
        read: impl FnOnce() -> Snapshot<T>,
    ) -> Self {
        let slot = feed.slot.lock();
        let place = slot.published;
        let seen = match (&filter, &slot.latest) {
            (Filter::Changes(_), Some(latest)) => Some((place, latest.clone())),
            (Filter::Changes(_), None) => Some((place, read())),
            _ => None,
        };
        drop(slot);
        let gate = Gate::new(filter, seen);
        if matches!(gate.filter, Filter::Changes(_)) {
            feed.track(&gate);
        }
        Watcher {
            gate,
            feed,
            place: AtomicU64::new(place),
        }
    }

    /// A [`Closer`] that ends this watcher and its clones.
    #[must_use]
    pub fn closer(&self) -> Closer<T> {
        Closer {
            feed: self.feed.clone(),
            gate: self.gate.clone(),
        }
    }

    /// For a watcher from [`Db::watch_changes`](crate::Db::watch_changes), the snapshot its test
    /// compares new ones with, and `None` for any other watcher.
    ///
    /// That is the snapshot it handed out last, or the one current when it was made, so a
    /// consumer that starts from it misses nothing that lands meanwhile.
    #[must_use]
    pub fn seen(&self) -> Option<Snapshot<T>> {
        self.gate.seen.lock().as_ref().map(|(_, seen)| seen.clone())
    }

    fn look(&self, slot: &mut MutexGuard<'_, Slot<T>>) -> Look<T> {
        loop {
            if slot.closed || self.gate.closed.load(Ordering::Acquire) {
                return Look::Closed;
            }
            let published = slot.published;
            if published == self.place.load(Ordering::Relaxed) {
                return Look::Nothing;
            }
            self.place.store(published, Ordering::Relaxed);
            let Some(snapshot) = slot.latest.clone() else {
                return Look::Closed;
            };
            let passed = !self.gate.filtered()
                || self.gate.approved(published)
                || MutexGuard::unlocked(slot, || self.gate.admits(&snapshot));
            if passed {
                return Look::Found(published, snapshot);
            }
        }
    }

    fn park(&self, slot: &mut MutexGuard<'_, Slot<T>>, wait: &Wait) -> bool {
        let filtered = self.gate.filtered();
        let condvar = match filtered {
            true => &self.gate.passed,
            false => &self.feed.fresh,
        };
        if filtered {
            self.gate.waiting.fetch_add(1, Ordering::SeqCst);
            self.feed.waiting.fetch_add(1, Ordering::SeqCst);
        }
        let expired = match wait {
            Wait::Until(deadline) => condvar.wait_until(slot, *deadline).timed_out(),
            _ => {
                condvar.wait(slot);
                false
            }
        };
        if filtered {
            self.gate.waiting.fetch_sub(1, Ordering::SeqCst);
            self.feed.waiting.fetch_sub(1, Ordering::SeqCst);
        }
        expired
    }
}

impl<T: Send + Sync + 'static> Watcher<T> {
    fn take(&self, wait: Wait) -> Result<Snapshot<T>, Stopped> {
        if self.gate.filtered() && !matches!(wait, Wait::Never) {
            self.feed.enlist(&self.gate);
        }
        let mut slot = self.feed.slot.lock();
        let mut expired = matches!(wait, Wait::Never);
        loop {
            match self.look(&mut slot) {
                Look::Closed => return Err(Stopped::Closed),
                Look::Found(count, snapshot) => {
                    drop(slot);
                    self.gate.remember(count, &snapshot);
                    return Ok(snapshot);
                }
                Look::Nothing if expired => return Err(Stopped::Empty),
                Look::Nothing => expired = self.park(&mut slot, &wait),
            }
        }
    }

    /// Blocks until the branch's newest snapshot is newer than the last one this watcher returned
    /// and passes its filter, and returns it.
    ///
    /// # Errors
    ///
    /// [`RecvError`] once the watcher has ended.
    pub fn recv(&self) -> Result<Snapshot<T>, RecvError> {
        self.take(Wait::Forever).map_err(|_| RecvError)
    }

    /// Like [`Watcher::recv`], waiting at most `timeout`.
    ///
    /// # Errors
    ///
    /// [`RecvTimeoutError::Timeout`] if nothing arrived in time, and
    /// [`RecvTimeoutError::Disconnected`] once the watcher has ended.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Snapshot<T>, RecvTimeoutError> {
        let wait = match Instant::now().checked_add(timeout) {
            Some(deadline) => Wait::Until(deadline),
            None => Wait::Forever,
        };
        self.take(wait).map_err(|stopped| match stopped {
            Stopped::Closed => RecvTimeoutError::Disconnected,
            Stopped::Empty => RecvTimeoutError::Timeout,
        })
    }

    /// Like [`Watcher::recv`], without waiting.
    ///
    /// A filtered watcher runs its test on the calling thread.
    ///
    /// # Errors
    ///
    /// [`TryRecvError::Empty`] if there is nothing new that passes the filter, and
    /// [`TryRecvError::Disconnected`] once the watcher has ended.
    pub fn try_recv(&self) -> Result<Snapshot<T>, TryRecvError> {
        self.take(Wait::Never).map_err(|stopped| match stopped {
            Stopped::Closed => TryRecvError::Disconnected,
            Stopped::Empty => TryRecvError::Empty,
        })
    }

    /// [`Watcher::recv`] as a future, for async code.
    ///
    /// It waits through a [`Waker`], so it runs on any executor and needs no thread of its own.
    /// Dropping the future stops the wait.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Map};
    ///
    /// async fn follow(db: &Db<Map<String, u32>>) {
    ///     let watcher = db.watch_changes(|seen, now| seen.get("anvil") != now.get("anvil"));
    ///     let mut seen = watcher.seen().expect("a watch_changes watcher has seen a snapshot");
    ///     while let Ok(now) = watcher.changed().await {
    ///         for change in seen.diff(&now) {
    ///             println!("{:?} changed", change.key());
    ///         }
    ///         seen = now;
    ///     }
    /// }
    /// ```
    pub fn changed(&self) -> Changed<'_, T> {
        Changed {
            watcher: self,
            key: None,
        }
    }
}

impl<T: Send + Sync + 'static> Iterator for Watcher<T> {
    type Item = Snapshot<T>;

    fn next(&mut self) -> Option<Snapshot<T>> {
        self.recv().ok()
    }
}

/// The future [`Watcher::changed`] returns.
///
/// It resolves as [`Watcher::recv`] returns: to the branch's newest snapshot once that is new to
/// the watcher and passes its filter, or to [`RecvError`] once the watcher has ended. Dropping it
/// stops the wait.
///
/// # Panics
///
/// A poll that runs the watcher's filter panics if the filter panics.
#[must_use = "futures do nothing unless polled"]
pub struct Changed<'a, T> {
    watcher: &'a Watcher<T>,
    key: Option<u64>,
}

impl<T> fmt::Debug for Changed<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Changed")
            .field("watcher", self.watcher)
            .field("parked", &self.key.is_some())
            .finish()
    }
}

impl<T> Changed<'_, T> {
    fn hold(&mut self, waker: &Waker) {
        let gate = &self.watcher.gate;
        let mut wakers = gate.wakers.lock();
        if let Some(key) = self.key
            && let Some((_, held)) = wakers.iter_mut().find(|(held, _)| *held == key)
        {
            held.clone_from(waker);
            return;
        }
        let key = gate.keys.fetch_add(1, Ordering::Relaxed);
        wakers.push((key, waker.clone()));
        gate.waiting.fetch_add(1, Ordering::SeqCst);
        self.watcher.feed.waiting.fetch_add(1, Ordering::SeqCst);
        drop(wakers);
        self.key = Some(key);
    }

    fn forget(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        let gate = &self.watcher.gate;
        let mut wakers = gate.wakers.lock();
        let Some(index) = wakers.iter().position(|(held, _)| *held == key) else {
            return;
        };
        let (_, waker) = wakers.swap_remove(index);
        drop(wakers);
        gate.waiting.fetch_sub(1, Ordering::SeqCst);
        self.watcher.feed.waiting.fetch_sub(1, Ordering::SeqCst);
        drop(waker);
    }
}

impl<T: Send + Sync + 'static> Future for Changed<'_, T> {
    type Output = Result<Snapshot<T>, RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let watcher = self.watcher;
        watcher.feed.enlist(&watcher.gate);
        let mut slot = watcher.feed.slot.lock();
        match watcher.look(&mut slot) {
            Look::Closed => {
                drop(slot);
                self.forget();
                Poll::Ready(Err(RecvError))
            }
            Look::Found(count, snapshot) => {
                drop(slot);
                self.forget();
                watcher.gate.remember(count, &snapshot);
                Poll::Ready(Ok(snapshot))
            }
            Look::Nothing => {
                self.hold(cx.waker());
                drop(slot);
                Poll::Pending
            }
        }
    }
}

impl<T> Drop for Changed<'_, T> {
    fn drop(&mut self) {
        self.forget();
    }
}
