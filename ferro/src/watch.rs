use std::cell::Cell;
use std::fmt;
use std::sync::Arc;
use std::sync::mpsc::{RecvError, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::db::Snapshot;

struct Slot<T> {
    latest: Option<Snapshot<T>>,
    published: u64,
    closed: bool,
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
            }),
            fresh: Condvar::new(),
        }
    }

    pub(crate) fn publish(&self, snapshot: Snapshot<T>) {
        let mut slot = self.slot.lock();
        if slot.closed {
            return;
        }
        let replaced = slot.latest.replace(snapshot);
        slot.published += 1;
        drop(slot);
        self.fresh.notify_all();
        drop(replaced);
    }

    pub(crate) fn close(&self) {
        let mut slot = self.slot.lock();
        slot.closed = true;
        let replaced = slot.latest.take();
        drop(slot);
        self.fresh.notify_all();
        drop(replaced);
    }
}

pub struct Watcher<T> {
    feed: Arc<Feed<T>>,
    seen: Cell<u64>,
}

impl<T> fmt::Debug for Watcher<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watcher").field("seen", &self.seen.get()).finish_non_exhaustive()
    }
}

impl<T> Watcher<T> {
    pub(crate) fn new(feed: Arc<Feed<T>>) -> Self {
        let seen = feed.slot.lock().published;
        Watcher {
            feed,
            seen: Cell::new(seen),
        }
    }

    fn unseen(&self, slot: &MutexGuard<'_, Slot<T>>) -> Option<Snapshot<T>> {
        if slot.published == self.seen.get() {
            return None;
        }
        self.seen.set(slot.published);
        slot.latest.clone()
    }

    pub fn recv(&self) -> Result<Snapshot<T>, RecvError> {
        let mut slot = self.feed.slot.lock();
        loop {
            if slot.closed {
                return Err(RecvError);
            }
            if let Some(snapshot) = self.unseen(&slot) {
                return Ok(snapshot);
            }
            self.feed.fresh.wait(&mut slot);
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Snapshot<T>, RecvTimeoutError> {
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return self.recv().map_err(|_| RecvTimeoutError::Disconnected);
        };
        let mut slot = self.feed.slot.lock();
        let mut expired = false;
        loop {
            if slot.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            if let Some(snapshot) = self.unseen(&slot) {
                return Ok(snapshot);
            }
            if expired {
                return Err(RecvTimeoutError::Timeout);
            }
            expired = self.feed.fresh.wait_until(&mut slot, deadline).timed_out();
        }
    }

    pub fn try_recv(&self) -> Result<Snapshot<T>, TryRecvError> {
        let slot = self.feed.slot.lock();
        if slot.closed {
            return Err(TryRecvError::Disconnected);
        }
        self.unseen(&slot).ok_or(TryRecvError::Empty)
    }
}

impl<T> Iterator for Watcher<T> {
    type Item = Snapshot<T>;

    fn next(&mut self) -> Option<Snapshot<T>> {
        self.recv().ok()
    }
}
