//! Followers: a thread that keeps a handle up to date with what other processes commit.
//!
//! [`Options::follow`](crate::Options::follow) starts one for a read-only handle or a writer opened
//! with [`Options::shared`](crate::Options::shared). It waits until the file may have changed and
//! looks: on a read-only handle a look is a refresh, and on a shared writer it reads the path's
//! identity and the newest superblock and catches up only to what they name, which fires the
//! handle's watchers. On 64-bit Linux it waits on inotify; elsewhere, or where inotify cannot be
//! had, it looks every 10 ms.
//! [`Db::following`](crate::Db::following) returns a [`Following`] that says how it has gone.
//! [Followers](https://github.com/archlemon/ferro/blob/main/docs/internals/processes.md#followers)
//! in the internals describes the thread and what it costs.

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::error::{Error, Result};
use crate::platform::{Bell, Watch};

/// How often a follower looks at the file when nothing tells it of changes.
const EVERY: Duration = Duration::from_millis(10);

/// How often a follower that is told of changes looks all the same.
const SPARE: Duration = Duration::from_secs(1);

/// The least time from the start of a look that refreshed to the start of the next, for a follower
/// that is told of changes, so that a busy writer costs it at most one refresh a millisecond.
const GAP: Duration = Duration::from_millis(1);

/// How the follower that [`Options::follow`](crate::Options::follow) started has gone, from
/// [`Db::following`](crate::Db::following).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Following {
    /// Whether the operating system tells it when the file changes, through inotify on 64-bit
    /// Linux. Without that it looks every 10 ms.
    pub notified: bool,
    /// How many times it has looked at the file.
    pub looks: u64,
    /// How many of those looks found commits the handle had not seen and refreshed it.
    pub refreshes: u64,
    /// The error the last look failed with, as text, until a look succeeds.
    ///
    /// A shared writer whose writes failed is poisoned until it is opened again, so its follower
    /// records [`Error::Poisoned`](crate::Error::Poisoned) here and stops looking, and the error
    /// stays.
    pub last_error: Option<String>,
}

struct Signal {
    stopped: Mutex<bool>,
    wake: Condvar,
    status: Mutex<Following>,
}

impl Signal {
    fn sleep(&self, pause: Duration) -> bool {
        let mut stopped = self.stopped.lock();
        if !*stopped {
            self.wake.wait_for(&mut stopped, pause);
        }
        !*stopped
    }

    fn record(&self, outcome: std::result::Result<bool, String>) {
        let mut status = self.status.lock();
        status.looks += 1;
        match outcome {
            Ok(moved) => {
                status.refreshes += u64::from(moved);
                status.last_error = None;
            }
            Err(error) => status.last_error = Some(error),
        }
    }

    fn unwatched(&self, error: &io::Error) {
        let mut status = self.status.lock();
        status.notified = false;
        status.last_error = Some(format!("the follower stopped watching the file: {error}"));
    }
}

enum Wait {
    Poll(Duration),
    Notify(Watch),
}

impl Wait {
    fn next(&mut self, signal: &Signal, refreshed: Option<Instant>) -> io::Result<bool> {
        match self {
            Wait::Poll(every) => Ok(signal.sleep(*every)),
            Wait::Notify(watch) => {
                if let Some(pause) = refreshed.and_then(|at| GAP.checked_sub(at.elapsed()))
                    && !watch.rest(pause)?
                {
                    return Ok(false);
                }
                watch.wait(SPARE)
            }
        }
    }
}

fn run(signal: &Signal, mut wait: Wait, mut look: impl FnMut() -> Option<Result<bool>>) {
    loop {
        let started = Instant::now();
        let (outcome, poisoned) = match panic::catch_unwind(AssertUnwindSafe(&mut look)) {
            Ok(None) => return,
            Ok(Some(outcome)) => {
                let poisoned = matches!(outcome, Err(Error::Poisoned));
                (outcome.map_err(|error| error.to_string()), poisoned)
            }
            Err(_) => (Err("the follower's refresh panicked".to_string()), false),
        };
        let refreshed = matches!(outcome, Ok(true)).then_some(started);
        signal.record(outcome);
        if poisoned {
            return;
        }
        let going = match wait.next(signal, refreshed) {
            Ok(going) => going,
            Err(error) => {
                signal.unwatched(&error);
                wait = Wait::Poll(EVERY);
                signal.sleep(EVERY)
            }
        };
        if !going {
            return;
        }
    }
}

/// The thread that follows a file for its handles, which stops and is joined when this is dropped.
pub(crate) struct Follower {
    signal: Arc<Signal>,
    bell: Option<Arc<Bell>>,
    thread: Option<JoinHandle<()>>,
}

impl Follower {
    /// Starts following the file at `path` on a thread named `ferro-follow`.
    ///
    /// The thread calls `look` at once, and again each time the file may have changed: on an
    /// inotify event for the file or its name, but no sooner than a millisecond after the start
    /// of a look that refreshed, and once a second besides; or every `poll`, or every 10 ms where
    /// no watch can be had. `look` returns whether it refreshed the handle, and `None` once the
    /// handle is gone, which ends the thread. A `poll` of its own makes it look at that interval
    /// even where a watch could be had.
    ///
    /// # Errors
    ///
    /// The error of starting the thread.
    pub(crate) fn start(
        path: &Path,
        poll: Option<Duration>,
        look: impl FnMut() -> Option<Result<bool>> + Send + 'static,
    ) -> io::Result<Follower> {
        let watch = match poll {
            Some(_) => None,
            None => Watch::new(path).ok(),
        };
        let signal = Arc::new(Signal {
            stopped: Mutex::new(false),
            wake: Condvar::new(),
            status: Mutex::new(Following {
                notified: watch.is_some(),
                ..Following::default()
            }),
        });
        let bell = watch.as_ref().map(Watch::bell);
        let wait = match watch {
            Some(watch) => Wait::Notify(watch),
            None => Wait::Poll(poll.unwrap_or(EVERY)),
        };
        let thread = thread::Builder::new()
            .name("ferro-follow".to_string())
            .spawn({
                let signal = signal.clone();
                move || run(&signal, wait, look)
            })?;
        Ok(Follower {
            signal,
            bell,
            thread: Some(thread),
        })
    }

    /// How the follower has gone so far.
    pub(crate) fn status(&self) -> Following {
        self.signal.status.lock().clone()
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        *self.signal.stopped.lock() = true;
        self.signal.wake.notify_all();
        if let Some(bell) = &self.bell {
            bell.ring();
        }
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != thread::current().id()
        {
            let _ = thread.join();
        }
    }
}
