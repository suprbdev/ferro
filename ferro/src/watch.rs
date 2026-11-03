use std::cell::Cell;
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvError, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::db::Snapshot;

type Test<T> = Box<dyn Fn(&Snapshot<T>) -> bool + Send + Sync>;

struct Gate<T> {
    test: Option<Test<T>>,
    closed: AtomicBool,
    parked: AtomicUsize,
    passed: Condvar,
}

impl<T> Gate<T> {
    fn new(test: Option<Test<T>>) -> Arc<Self> {
        Arc::new(Gate {
            test,
            closed: AtomicBool::new(false),
            parked: AtomicUsize::new(0),
            passed: Condvar::new(),
        })
    }

    fn admits(&self, snapshot: &Snapshot<T>) -> bool {
        let Some(test) = &self.test else {
            return true;
        };
        panic::catch_unwind(AssertUnwindSafe(|| test(snapshot))).unwrap_or(true)
    }
}

struct Slot<T> {
    latest: Option<Snapshot<T>>,
    published: u64,
    closed: bool,
    gates: Vec<Weak<Gate<T>>>,
}

impl<T> Slot<T> {
    fn live_gates(&mut self) -> Vec<Arc<Gate<T>>> {
        let mut live = Vec::with_capacity(self.gates.len());
        self.gates.retain(|gate| match gate.upgrade() {
            Some(gate) => {
                live.push(gate);
                true
            }
            None => false,
        });
        live
    }
}

pub(crate) struct Feed<T> {
    slot: Mutex<Slot<T>>,
    fresh: Condvar,
}

impl<T> Feed<T> {
    pub(crate) fn new() -> Self {
        Feed {
            slot: Mutex::new(Slot {
                latest: None,
                published: 0,
                closed: false,
                gates: Vec::new(),
            }),
            fresh: Condvar::new(),
        }
    }

    pub(crate) fn publish(&self, snapshot: Snapshot<T>) {
        let mut slot = self.slot.lock();
        if slot.closed {
            return;
        }
        let waiting = if slot.gates.is_empty() {
            Vec::new()
        } else {
            let mut live = slot.live_gates();
            live.retain(|gate| gate.parked.load(Ordering::Relaxed) > 0);
            live
        };
        let shown = (!waiting.is_empty()).then(|| snapshot.clone());
        let replaced = slot.latest.replace(snapshot);
        slot.published += 1;
        drop(slot);
        self.fresh.notify_all();
        if let Some(shown) = shown {
            for gate in waiting {
                if gate.admits(&shown) {
                    gate.passed.notify_all();
                }
            }
        }
        drop(replaced);
    }

    pub(crate) fn rebind(&self, snapshot: Snapshot<T>) {
        let mut slot = self.slot.lock();
        let replaced = if slot.latest.is_some() {
            slot.latest.replace(snapshot)
        } else {
            None
        };
        drop(slot);
        drop(replaced);
    }

    pub(crate) fn close(&self) {
        let mut slot = self.slot.lock();
        slot.closed = true;
        let replaced = slot.latest.take();
        let gates = slot.live_gates();
        drop(slot);
        self.fresh.notify_all();
        for gate in gates {
            gate.passed.notify_all();
        }
        drop(replaced);
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
        self.gate.passed.notify_all();
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

pub struct Watcher<T> {
    feed: Arc<Feed<T>>,
    gate: Arc<Gate<T>>,
    seen: Cell<u64>,
}

impl<T> Clone for Watcher<T> {
    fn clone(&self) -> Self {
        Watcher {
            feed: self.feed.clone(),
            gate: self.gate.clone(),
            seen: self.seen.clone(),
        }
    }
}

impl<T> fmt::Debug for Watcher<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watcher").field("seen", &self.seen.get()).finish_non_exhaustive()
    }
}

impl<T> Watcher<T> {
    pub(crate) fn new(feed: Arc<Feed<T>>, test: Option<Test<T>>) -> Self {
        let gate = Gate::new(test);
        let mut slot = feed.slot.lock();
        if gate.test.is_some() {
            slot.gates.push(Arc::downgrade(&gate));
        }
        let seen = slot.published;
        drop(slot);
        Watcher {
            feed,
            gate,
            seen: Cell::new(seen),
        }
    }

    pub fn closer(&self) -> Closer<T> {
        Closer {
            feed: self.feed.clone(),
            gate: self.gate.clone(),
        }
    }

    fn park(&self, slot: &mut MutexGuard<'_, Slot<T>>, wait: &Wait) -> bool {
        let condvar = match self.gate.test {
            Some(_) => &self.gate.passed,
            None => &self.feed.fresh,
        };
        self.gate.parked.fetch_add(1, Ordering::Relaxed);
        let expired = match wait {
            Wait::Until(deadline) => condvar.wait_until(slot, *deadline).timed_out(),
            _ => {
                condvar.wait(slot);
                false
            }
        };
        self.gate.parked.fetch_sub(1, Ordering::Relaxed);
        expired
    }

    fn take(&self, wait: Wait) -> Result<Snapshot<T>, Stopped> {
        let mut slot = self.feed.slot.lock();
        let mut expired = matches!(wait, Wait::Never);
        loop {
            if slot.closed || self.gate.closed.load(Ordering::Acquire) {
                return Err(Stopped::Closed);
            }
            if slot.published != self.seen.get() {
                self.seen.set(slot.published);
                if let Some(snapshot) = slot.latest.clone() {
                    let passed = match &self.gate.test {
                        Some(test) => MutexGuard::unlocked(&mut slot, || test(&snapshot)),
                        None => true,
                    };
                    if passed {
                        return Ok(snapshot);
                    }
                    continue;
                }
            }
            if expired {
                return Err(Stopped::Empty);
            }
            expired = self.park(&mut slot, &wait);
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
}

impl<T> Iterator for Watcher<T> {
    type Item = Snapshot<T>;

    fn next(&mut self) -> Option<Snapshot<T>> {
        self.recv().ok()
    }
}
