use std::fmt;
use std::future::Future;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvError, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, OnceLock, Weak};
use std::task::{Context, Poll, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::db::Snapshot;

type Each<T> = Box<dyn Fn(&Snapshot<T>) -> bool + Send + Sync>;
type Pair<T> = Box<dyn Fn(&Snapshot<T>, &Snapshot<T>) -> bool + Send + Sync>;

pub(crate) enum Filter<T> {
    All,
    When(Each<T>),
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

#[must_use]
pub(crate) struct Notice<T> {
    feed: Option<Arc<Feed<T>>>,
    replaced: Option<Snapshot<T>>,
}

impl<T> Drop for Notice<T> {
    fn drop(&mut self) {
        if let Some(feed) = self.feed.take() {
            feed.kick();
        }
        self.replaced.take();
    }
}

pub(crate) struct Feed<T> {
    slot: Mutex<Slot<T>>,
    fresh: Condvar,
    plan: Mutex<Plan<T>>,
    waiting: AtomicUsize,
    due: AtomicBool,
    notifier: OnceLock<Thread>,
}

impl<T> Feed<T> {
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
            waiting: AtomicUsize::new(0),
            due: AtomicBool::new(false),
            notifier: OnceLock::new(),
        }
    }

    pub(crate) fn publish(self: &Arc<Self>, snapshot: Snapshot<T>) -> Notice<T> {
        let mut slot = self.slot.lock();
        if slot.closed {
            return Notice {
                feed: None,
                replaced: Some(snapshot),
            };
        }
        let replaced = slot.latest.replace(snapshot);
        slot.published += 1;
        drop(slot);
        self.fresh.notify_all();
        Notice {
            feed: Some(self.clone()),
            replaced,
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

    pub(crate) fn rebind(&self, snapshot: Snapshot<T>) {
        let mut slot = self.slot.lock();
        let replaced = match slot.closed {
            true => Some(snapshot),
            false => slot.latest.replace(snapshot),
        };
        drop(slot);
        drop(replaced);
    }

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
    pub(crate) fn new(feed: Arc<Feed<T>>, filter: Filter<T>) -> Self {
        let slot = feed.slot.lock();
        let place = slot.published;
        let seen = match (&filter, &slot.latest) {
            (Filter::Changes(_), Some(latest)) => Some((place, latest.clone())),
            _ => None,
        };
        drop(slot);
        Watcher {
            gate: Gate::new(filter, seen),
            feed,
            place: AtomicU64::new(place),
        }
    }

    pub fn closer(&self) -> Closer<T> {
        Closer {
            feed: self.feed.clone(),
            gate: self.gate.clone(),
        }
    }

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

    pub fn recv(&self) -> Result<Snapshot<T>, RecvError> {
        self.take(Wait::Forever).map_err(|_| RecvError)
    }

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

    pub fn try_recv(&self) -> Result<Snapshot<T>, TryRecvError> {
        self.take(Wait::Never).map_err(|stopped| match stopped {
            Stopped::Closed => TryRecvError::Disconnected,
            Stopped::Empty => TryRecvError::Empty,
        })
    }

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
