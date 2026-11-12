//! Threads kept between the commits that spread their work over several.
//!
//! [`run`] runs a closure of the caller's on the calling thread while up to a given number of the
//! crew's threads run another, and returns once every crew thread that took it has finished. Both
//! take their work from a queue they share, so the calling thread finishes all of it when no crew
//! thread is free, and a call never waits for a thread to start. The crew starts its threads the
//! first time calls need them, up to the limit its callers give, and parks them between calls, so a
//! commit that flushes on several threads does not start and join each of them, as
//! `std::thread::scope` did, which cost more than the work for commits of a few thousand keys.
//!
//! A crew thread only ever runs a closure some caller is blocked in [`run`] for, and that caller
//! takes from the same queue, so work never waits on a crew thread that is busy elsewhere, and a
//! closure that calls [`run`] again, from a crew thread or not, cannot deadlock.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};

/// How many idle threads a call's poster, and each thread that takes a seat, wakes, so that
/// neither the poster nor any one thread pays for every wake.
const FAN: usize = 2;

/// The crew: the calls waiting for threads, and the threads parked for them.
static CREW: Crew = Crew {
    state: Mutex::new(State {
        calls: Vec::new(),
        threads: 0,
        idle: 0,
    }),
    wake: Condvar::new(),
};

struct Crew {
    state: Mutex<State>,
    wake: Condvar,
}

struct State {
    calls: Vec<Call>,
    threads: usize,
    idle: usize,
}

/// A caller's closure, the seats left for crew threads to take it, and the latch the caller waits
/// on for those that did.
struct Call {
    work: Work,
    seats: usize,
    latch: Arc<Latch>,
}

/// A caller's closure with its lifetime erased, called only while its caller waits in [`run`].
#[derive(Clone, Copy)]
struct Work(*const (dyn Fn() + Sync + 'static));

// SAFETY: the pointer is to a closure that is `Sync`, so calling it from another thread is sound,
// and `run` keeps it alive until every thread that took it is done, as described there.
unsafe impl Send for Work {}

/// How many crew threads are inside a call, and the first panic one of them raised.
#[derive(Default)]
struct Latch {
    inside: Mutex<Inside>,
    left: Condvar,
}

#[derive(Default)]
struct Inside {
    running: usize,
    panic: Option<Box<dyn Any + Send>>,
}

/// Withdraws a call's seats and waits for the crew threads inside it, even when listing the call
/// or the caller's own share of the work unwinds.
struct Leave<'a>(&'a Arc<Latch>);

impl Drop for Leave<'_> {
    fn drop(&mut self) {
        CREW.state
            .lock()
            .calls
            .retain(|call| !Arc::ptr_eq(&call.latch, self.0));
        let mut inside = self.0.inside.lock();
        while inside.running > 0 {
            self.0.left.wait(&mut inside);
        }
    }
}

/// Wakes up to [`FAN`] idle crew threads for `seats` seats.
fn wake(state: &State, seats: usize) {
    for _ in 0..seats.min(state.idle).min(FAN) {
        CREW.wake.notify_one();
    }
}

/// Lists `work` under `latch` for up to `seats` crew threads, wakes a few idle ones, each of which
/// wakes a few more while seats are left, and starts more threads, up to `limit`, for the seats the
/// idle ones leave.
fn post(work: Work, seats: usize, limit: usize, latch: &Arc<Latch>) {
    let start = {
        let mut state = CREW.state.lock();
        state.calls.push(Call {
            work,
            seats,
            latch: latch.clone(),
        });
        wake(&state, seats);
        let start = seats
            .saturating_sub(state.idle)
            .min(limit.saturating_sub(state.threads));
        state.threads += start;
        start
    };
    let mut unstarted = Unstarted(start);
    #[cfg(test)]
    if tests::START_PANICS.get() {
        panic!("a crew thread failed to start");
    }
    for _ in 0..start {
        let spawned = std::thread::Builder::new()
            .name("ferro-crew".to_string())
            .spawn(serve);
        if spawned.is_ok() {
            unstarted.0 -= 1;
        }
    }
}

/// The threads [`post`] counted but has not started, which it gives back to the crew's count when
/// dropped, so that a thread that fails to start, or a panic while starting one, leaves room for
/// later calls to start another.
struct Unstarted(usize);

impl Drop for Unstarted {
    fn drop(&mut self) {
        if self.0 > 0 {
            CREW.state.lock().threads -= self.0;
        }
    }
}

/// Drops a panic's payload, and forgets the payload of a panic its drop raises, so that a payload
/// that panics when dropped cannot unwind out of a crew thread.
fn discard(payload: Box<dyn Any + Send>) {
    if let Err(again) = catch_unwind(AssertUnwindSafe(move || drop(payload))) {
        std::mem::forget(again);
    }
}

/// Runs `own` on this thread while up to `helpers` crew threads run `work`, and returns what `own`
/// returns once every crew thread that took `work` has returned from it.
///
/// `work` should take what it does from a queue it shares with `own` and return once the queue is
/// closed and empty, and `own` should close it and then drain it with `work`, since a crew thread
/// may take `work` only once `own` has done everything, or none may. The crew grows to `limit`
/// threads at most.
///
/// # Panics
///
/// A panic in `own` carries on once the crew threads have left `work`, so `own` must close its
/// queue as it unwinds, and a panic in a crew thread's `work` carries on here after that.
pub(crate) fn run<R>(
    helpers: usize,
    limit: usize,
    work: &(dyn Fn() + Sync),
    own: impl FnOnce() -> R,
) -> R {
    if helpers == 0 {
        return own();
    }
    // SAFETY: only the lifetime changes. `Leave` is made before `post` lists the call, so however
    // `run` returns or unwinds from then on, `Leave` withdraws the call and then waits until no
    // crew thread that took it is still inside, so no thread calls `work` once its borrow ends.
    let erased = unsafe {
        std::mem::transmute::<*const (dyn Fn() + Sync + '_), *const (dyn Fn() + Sync + 'static)>(
            work,
        )
    };
    let latch = Arc::new(Latch::default());
    let leave = Leave(&latch);
    post(Work(erased), helpers, limit, &latch);
    let out = own();
    drop(leave);
    if let Some(panic) = latch.inside.lock().panic.take() {
        resume_unwind(panic);
    }
    out
}

/// A crew thread's life: take a seat at the oldest call with one left, run it, and park when
/// none has.
fn serve() {
    let mut state = CREW.state.lock();
    loop {
        let Some(at) = state.calls.iter().position(|call| call.seats > 0) else {
            state.idle += 1;
            CREW.wake.wait(&mut state);
            state.idle -= 1;
            continue;
        };
        let call = &mut state.calls[at];
        call.seats -= 1;
        let work = call.work;
        let latch = call.latch.clone();
        latch.inside.lock().running += 1;
        let left = call.seats;
        if left == 0 {
            state.calls.remove(at);
        }
        wake(&state, left);
        drop(state);
        // SAFETY: the caller of `run` waits in `Leave` until `running`, raised above while the
        // call was still listed, falls back to zero, so the closure is alive for this call.
        let outcome = catch_unwind(AssertUnwindSafe(|| unsafe { (*work.0)() }));
        let mut spare = outcome.err();
        let mut inside = latch.inside.lock();
        if inside.panic.is_none() {
            inside.panic = spare.take();
        }
        inside.running -= 1;
        if inside.running == 0 {
            latch.left.notify_all();
        }
        drop(inside);
        drop(latch);
        if let Some(payload) = spare {
            discard(payload);
        }
        state = CREW.state.lock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    thread_local! {
        pub(super) static START_PANICS: Cell<bool> = const { Cell::new(false) };
    }

    #[test]
    fn every_item_is_taken_once_whoever_takes_it() {
        let items = Mutex::new((0..10_000).collect::<Vec<usize>>());
        let sum = AtomicUsize::new(0);
        let drain = || {
            while let Some(item) = items.lock().pop() {
                sum.fetch_add(item, Ordering::Relaxed);
            }
        };
        run(7, 15, &drain, drain);
        assert_eq!(sum.load(Ordering::Relaxed), (0..10_000).sum::<usize>());
    }

    #[test]
    fn a_panic_on_a_crew_thread_carries_on_in_the_caller() {
        let first = AtomicUsize::new(0);
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let work = || {
                if first.fetch_add(1, Ordering::SeqCst) > 0 {
                    panic!("crew");
                }
                let began = Instant::now();
                while first.load(Ordering::SeqCst) < 2 && began.elapsed() < Duration::from_secs(5) {
                    std::thread::yield_now();
                }
            };
            run(3, 15, &work, work)
        }));
        assert!(outcome.is_err());
    }

    fn take(left: &AtomicUsize) -> bool {
        left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
    }

    #[test]
    fn calls_from_many_threads_and_nested_calls_all_finish() {
        let total = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        let left = AtomicUsize::new(64);
                        let outer = || {
                            while take(&left) {
                                let inner = AtomicUsize::new(4);
                                let count = || {
                                    while take(&inner) {
                                        total.fetch_add(1, Ordering::Relaxed);
                                    }
                                };
                                run(2, 15, &count, count);
                            }
                        };
                        run(4, 15, &outer, outer);
                    }
                });
            }
        });
        assert_eq!(total.load(Ordering::Relaxed), 8 * 50 * 64 * 4);
    }

    #[test]
    fn a_panic_after_the_call_is_listed_leaves_nothing_listed() {
        let calls: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
        let work: &'static (dyn Fn() + Sync) = Box::leak(Box::new(move || {
            std::thread::sleep(Duration::from_millis(50));
            calls.fetch_add(1, Ordering::SeqCst);
        }));
        START_PANICS.set(true);
        let outcome = catch_unwind(AssertUnwindSafe(|| run(64, 15, work, || ())));
        START_PANICS.set(false);
        assert!(outcome.is_err());
        let listed = CREW
            .state
            .lock()
            .calls
            .iter()
            .any(|call| std::ptr::addr_eq(call.work.0, work));
        assert!(!listed);
    }

    struct Bomb;

    impl Drop for Bomb {
        fn drop(&mut self) {
            panic!("payload dropped");
        }
    }

    #[test]
    fn a_payload_that_panics_on_drop_does_not_hang_the_caller() {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = AtomicUsize::new(0);
            let meet = || {
                started.fetch_add(1, Ordering::SeqCst);
                let began = Instant::now();
                while started.load(Ordering::SeqCst) < 3 && began.elapsed() < Duration::from_secs(5)
                {
                    std::thread::yield_now();
                }
            };
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let work = || {
                    meet();
                    std::panic::panic_any(Bomb);
                };
                run(2, 15, &work, meet)
            }));
            let carried = outcome.is_err();
            std::mem::forget(outcome);
            let _ = sender.send(carried);
        });
        assert_eq!(receiver.recv_timeout(Duration::from_secs(20)), Ok(true));
    }
}
