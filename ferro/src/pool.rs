//! A pool of scoped threads for the checks that [`Db::receive`](crate::Db::receive) and
//! [`Db::verify_typed`](crate::Db::verify_typed) make that each table's indexes agree with its
//! rows.
//!
//! [`scope`] runs a closure beside the threads of a pool and returns once the closure has
//! returned and all the work queued on the pool has run, every thread joined. A thread of its own
//! starts the others: one whenever more work waits than threads are idle, up to [`workers`] of
//! them and no more than [`THREADS`], so a small check runs on one thread and a large one on
//! several, while the closure goes on with work of its own, such as the walk of a bundle. Checks
//! are queued with [`Pool::check`], at most [`CHECKS_AT_ONCE`] of them run at once, which bounds
//! what they hold, and a check queues the smaller work it splits into, such as the runs of a diff,
//! with [`Pool::queue`] and waits for it with [`Pool::help_until`], which runs queued work
//! meanwhile, so the work of several checks shares the threads.

use std::any::Any;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::Scope;

use parking_lot::{Condvar, Mutex};

use crate::map::workers;

/// The most checks that run at once.
pub(crate) const CHECKS_AT_ONCE: usize = 4;

/// The most threads a pool starts, which leaves cores to the walk that runs beside the checks.
const THREADS: usize = 8;

/// Work for a [`Pool`], given the pool to queue more on.
pub(crate) type Job<'p> = Box<dyn FnOnce(&Pool<'p>) + Send + 'p>;

/// `work` as a [`Job`].
pub(crate) fn job<'p>(work: impl FnOnce(&Pool<'p>) + Send + 'p) -> Job<'p> {
    Box::new(work)
}

/// The threads of one [`scope`] and the work waiting for them.
pub(crate) struct Pool<'p> {
    state: Mutex<State<'p>>,
    wake: Condvar,
    starter: Condvar,
    stopped: AtomicBool,
    panic: Mutex<Option<Box<dyn Any + Send>>>,
}

struct State<'p> {
    work: VecDeque<Job<'p>>,
    checks: VecDeque<Job<'p>>,
    checking: usize,
    running: usize,
    idle: usize,
    closed: bool,
}

impl State<'_> {
    fn waiting(&self) -> usize {
        let checks = if self.checking < CHECKS_AT_ONCE {
            self.checks.len()
        } else {
            0
        };
        self.work.len() + checks
    }

    fn take<'p>(state: &mut State<'p>, checks: bool) -> Option<(Job<'p>, bool)> {
        if let Some(job) = state.work.pop_front() {
            return Some((job, false));
        }
        if checks && state.checking < CHECKS_AT_ONCE {
            return state.checks.pop_front().map(|job| (job, true));
        }
        None
    }
}

/// Runs `f` beside the threads of a pool it is given, and returns what `f` returns once `f` has
/// returned and all the work queued on the pool has run, which the calling thread helps with.
///
/// # Panics
///
/// A panic in `f` or in a job carries on here once every thread has stopped. A panic in `f` tells
/// the checks not started yet not to start, as [`Pool::stop`] does.
pub(crate) fn scope<'p, R>(f: impl FnOnce(&Pool<'p>) -> R) -> R {
    let pool = Pool {
        state: Mutex::new(State {
            work: VecDeque::new(),
            checks: VecDeque::new(),
            checking: 0,
            running: 0,
            idle: 0,
            closed: false,
        }),
        wake: Condvar::new(),
        starter: Condvar::new(),
        stopped: AtomicBool::new(false),
        panic: Mutex::new(None),
    };
    let out = std::thread::scope(|threads| {
        let starter = &pool;
        threads.spawn(move || starter.start(threads));
        let out = catch_unwind(AssertUnwindSafe(|| f(&pool)));
        if out.is_err() {
            pool.stop();
        }
        pool.finish();
        out
    });
    if let Some(panic) = pool.panic.into_inner() {
        resume_unwind(panic);
    }
    out.unwrap_or_else(|panic| resume_unwind(panic))
}

impl<'p> Pool<'p> {
    /// Queues the check `job`, which a thread of the pool runs once fewer than
    /// [`CHECKS_AT_ONCE`] checks are running and no smaller work waits, first queued first.
    pub(crate) fn check(&self, job: Job<'p>) {
        let mut state = self.state.lock();
        state.checks.push_back(job);
        self.wake.notify_all();
        self.starter.notify_one();
    }

    /// Queues `job`, smaller work that a check split off, which the threads of the pool and those
    /// waiting in [`help_until`](Pool::help_until) run before any check, first queued first.
    pub(crate) fn queue(&self, job: Job<'p>) {
        let mut state = self.state.lock();
        state.work.push_back(job);
        self.wake.notify_one();
        if state.waiting() > state.idle {
            self.starter.notify_one();
        }
    }

    /// Runs queued work until `done` holds, waiting while none is queued: the smaller work alone,
    /// or checks as well with `checks`, which a check itself must not pass. Whatever makes `done`
    /// hold must then call [`wake`](Pool::wake).
    pub(crate) fn help_until(&self, checks: bool, done: impl Fn() -> bool) {
        let mut state = self.state.lock();
        loop {
            if done() {
                if state.waiting() > 0 {
                    self.wake.notify_one();
                }
                return;
            }
            if let Some((job, check)) = State::take(&mut state, checks) {
                state = self.perform(state, job, check);
                continue;
            }
            self.wake.wait(&mut state);
        }
    }

    /// Wakes every thread waiting in [`help_until`](Pool::help_until) to look at what it waits
    /// for again.
    pub(crate) fn wake(&self) {
        let _state = self.state.lock();
        self.wake.notify_all();
    }

    /// Tells the checks and work that have not started yet to do nothing, as a receive whose walk
    /// failed does.
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// Whether [`stop`](Pool::stop) was called.
    pub(crate) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    fn start<'s>(&'s self, threads: &'s Scope<'s, '_>) {
        let most = workers().clamp(1, THREADS);
        let mut started = 0;
        let mut state = self.state.lock();
        loop {
            if state.closed {
                return;
            }
            if started < most && state.waiting() > state.idle {
                started += 1;
                drop(state);
                threads.spawn(move || self.work());
                state = self.state.lock();
                continue;
            }
            self.starter.wait(&mut state);
        }
    }

    fn work(&self) {
        let mut state = self.state.lock();
        loop {
            if let Some((job, check)) = State::take(&mut state, true) {
                state = self.perform(state, job, check);
                continue;
            }
            if state.closed {
                return;
            }
            state.idle += 1;
            self.wake.wait(&mut state);
            state.idle -= 1;
        }
    }

    fn finish(&self) {
        let mut state = self.state.lock();
        loop {
            if let Some((job, check)) = State::take(&mut state, true) {
                state = self.perform(state, job, check);
                continue;
            }
            if state.running == 0 && state.waiting() == 0 {
                state.closed = true;
                self.wake.notify_all();
                self.starter.notify_one();
                return;
            }
            self.wake.wait(&mut state);
        }
    }

    fn perform<'a>(
        &'a self,
        mut state: parking_lot::MutexGuard<'a, State<'p>>,
        job: Job<'p>,
        check: bool,
    ) -> parking_lot::MutexGuard<'a, State<'p>> {
        state.running += 1;
        state.checking += usize::from(check);
        drop(state);
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| job(self))) {
            self.stop();
            self.panic.lock().get_or_insert(panic);
        }
        let mut state = self.state.lock();
        state.running -= 1;
        if check {
            state.checking -= 1;
            if !state.checks.is_empty() {
                self.wake.notify_all();
                self.starter.notify_one();
            }
        }
        if state.running == 0 {
            self.wake.notify_all();
        }
        state
    }
}

/// Counts the work a check queued that has not run yet, and wakes the check when the last of it
/// has.
pub(crate) struct Latch(AtomicUsize);

impl Latch {
    /// A latch waiting for `count` pieces of work.
    pub(crate) fn new(count: usize) -> Latch {
        Latch(AtomicUsize::new(count))
    }

    /// Counts one more piece of work.
    pub(crate) fn add(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }

    /// Counts one piece of work done, and wakes the threads waiting in `pool` if it was the last.
    pub(crate) fn done(&self, pool: &Pool<'_>) {
        if self.0.fetch_sub(1, Ordering::AcqRel) == 1 {
            pool.wake();
        }
    }

    /// Whether every piece of work it counts is done.
    pub(crate) fn is_open(&self) -> bool {
        self.0.load(Ordering::Acquire) == 0
    }
}
