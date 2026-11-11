//! A model of the gate behind [`Options::shared`]'s whole-range takers, on a simulated clock.
//!
//! A cycle's prune and pause, a pull's or receive's landing and a `Keep::Last` prune lock every
//! branch byte of the file at once, beside writers in other processes that each lock one byte per
//! closure. The model runs those actors as tasks of a single-threaded executor whose clock moves
//! only when every task waits, so a run is fixed by its seed: which task runs next after each wake,
//! how long a wake, a sleep or a commit takes, when a thread is preempted, which thread runs first
//! after an unlock, and when an in-process lane hands itself over fairly.
//!
//! The pieces it models are the ones the protocols differ in:
//!
//! - open file description locks, exclusive, per open, whose blocked takers the kernel links in
//!   chains below the lock in their way and wakes and relinks a link at a time, as `fs/locks.c`
//!   does, and which other opens test with `F_OFD_GETLK`;
//! - each open's lanes, which behave as `parking_lot` mutexes do: a free lane goes to whoever asks
//!   first, an unlock wakes the longest parked thread, which parks again at the back if another
//!   took the lane meanwhile, and about once a millisecond an unlock hands the lane over instead;
//! - each open's branch bytes shared between its lanes, its `whole` mark, its table of the bytes
//!   its threads hold, through which the gate, the cycle lock and the placement lock are taken and
//!   whose condition variable wakes its waiters a thread at a time through its mutex, as
//!   `parking_lot`'s does, and the queue of awaited writes at each lane, with the thread each
//!   byte's held-up writes share;
//! - closures that sleep, write from inside themselves, or join another thread's write through any
//!   open, the one that cycles included, blocking or awaited;
//! - coalesced groups that keep their branch's byte between closures.
//!
//! [`Design`] lists the protocols: the shared epochs of 6fb68bc, the growing rounds of 51dbfc6,
//! and the phase gates that replaced both, with and without their rule for writers whose branch is
//! busy when their phase ends, with the ways of telling phases apart that [`Phasing`] lists, and
//! with the ways of queueing takers for the gate that [`Queue`] lists. Each scenario comes from an
//! issue that a review of one of them found, #128, #136, #142, #145, #146, #147 and #149, or is an
//! adversarial one added here, and each checks the properties the gate promises: no task waits for
//! ever (P1), a write a closure joins ends within a bound (P2), a whole-range taker finishes within
//! a bound beside closures of a given length (P3), and takers get the gate in the order they asked
//! for it (P6), which shows in how often each of several pulls. The tests check that the phase
//! gate passes every scenario under thousands of seeds, calm
//! and with threads preempted for milliseconds at a time, and that the earlier designs fail the
//! scenarios their reviews found, so that the model is known to see those failures.
//!
//! [`Options::shared`]: ferro::Options::shared

use std::cell::{RefCell, RefMut};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

type Task = Pin<Box<dyn Future<Output = ()>>>;

const US: u64 = 1_000;
const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

/// The placement lock, which a commit, the seal of a group and a holder that has every branch byte
/// take, waiting, through the table of the bytes their open's threads hold.
const COMMIT: u64 = 2;
/// The cycle lock, which a cycle holds from its prune to the end of its pause.
const CYCLE: u64 = 4;
/// The old gate byte, which the epochs and rounds hold, and which the phase gate's holder holds
/// for as long as it wants every branch byte, so that one process at a time does.
const GATE: u64 = 5;
/// The two phase bytes of [`Phasing::Pair`].
const PHASES: u64 = 6;
/// The byte [`Phasing::Counter`]'s holder holds for its whole hold.
const FLAG: u64 = 9;
/// The first of the phase bytes of [`Phasing::Pool`], every other byte from here.
const POOL: u64 = 100;
/// The byte a taker of [`Queue::Turns`] holds while it draws its turn.
const DOOR: u64 = 11;
/// The first of the [`TURNS`] turn bytes of [`Queue::Turns`], every other byte from here.
const TURN_AT: u64 = 4000;
/// How many turn bytes there are.
const TURNS: u64 = 64;
/// The first branch byte; branch `b` locks `BYTES + b`.
const BYTES: u64 = 1 << 20;
/// Every branch byte.
const RANGE: (u64, u64) = (BYTES, 2 * BYTES);

const EPOCH: (u64, u64) = (1_953_125, SEC);
const EPOCH_POLL: (u64, u64) = (50 * US, MS);
const EPOCH_LEAD: u64 = 150 * US;

const ROUND: (u64, u64) = (8 * MS, SEC);
const ROUND_OPEN: u64 = MS;
const ROUND_POLL: (u64, u64) = (50 * US, 500 * US);
const ROUND_WAIT: u64 = 2 * SEC;
const ROUND_RANGE_POLL: (u64, u64) = (20 * US, 200 * US);

const PHASE_POLL: (u64, u64) = (50 * US, MS);
const PHASE_RANGE_POLL: (u64, u64) = (20 * US, MS);
const PHASE_WAIT: u64 = 2 * SEC;

/// The protocols the model runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Design {
    /// 6fb68bc: writers held at the gate go on at the end of an epoch of `CLOCK_MONOTONIC` that
    /// grows with the hold, read from the holder's stamp; the holder takes its own lanes first and
    /// gives them up after three epochs of the hold if a thread of its own waits for one.
    Epochs,
    /// 51dbfc6: the holder waits in rounds, 8 ms and doubling, takes its lanes with a 1 ms timeout
    /// in every mode, lets everything go at a round's end if a thread of its own waits, and
    /// otherwise opens the gate for 1 ms; writers wait while the gate is held.
    Rounds,
    /// The phase gate: the holder takes nothing of its own while it waits, and holds one of two
    /// phase bytes in turn, switching at the end of each round; a writer held at the gate waits
    /// for the byte it found held to be let go.
    Phases(Knobs),
}

/// How a phase gate tells its writers which phase they wait for, and that it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phasing {
    /// b2741a3: two phase bytes, each hold starting on the first that is free and locking the
    /// next before it lets the old one go. A writer in another open finds the phase with a test
    /// over both and then a test of the first alone, and the holder's open numbers its phases
    /// from the byte it started on, again at each hold.
    Pair,
    /// [`Phasing::Pair`], with the holder keeping the gate this long after it lets its last
    /// phase byte go, so that no hold begins within that time of the last one ending.
    Spaced(u64),
    /// This many phase bytes, every other byte from [`POOL`]: each hold starts on one drawn at
    /// random, and each phase's end locks the next byte before it lets its own go. A writer in
    /// another open finds the byte held with one test, which reports the lowest of the bytes an
    /// open holds, and the holder's open numbers its phases without starting again at each hold.
    Pool(u64),
    /// A counter that the holder bumps at each end of a phase and of its hold, which writers in
    /// other opens read, as they would read a word of the file, while a flag byte says the gate
    /// is held.
    Counter,
}

/// What sets the phase gates apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Knobs {
    /// How many times in one wait a writer whose phase ended while its branch was busy waits for
    /// the next phase.
    busy_waits: u32,
    /// The first round.
    first: u64,
    /// The longest round.
    longest: u64,
    /// How the phases are told apart.
    phasing: Phasing,
    /// How long a holder that had to wait for the gate waits again before it holds its first
    /// phase.
    gap: u64,
    /// How takers queue for the gate.
    queue: Queue,
    /// Whether a pull's landing and integration take the gate in one place in line and one turn.
    together: bool,
}

/// How whole-range takers queue for the gate byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Queue {
    /// b919d64: a try for the gate byte, and if that fails a wait in the table of the bytes the
    /// open's threads hold and then in `F_OFD_SETLKW`, so that the order is the table's and the
    /// kernel's.
    Kernel,
    /// As [`Queue::Kernel`] with no try first, so that a taker that finds the gate held always
    /// queues: a blocking take from the start, with the gap after it if it had to wait.
    Blocking,
    /// Takers of one open queue for the gate in the order they came, by ticket, and the first in
    /// line then takes it as [`Queue::Kernel`] does.
    Local,
    /// As [`Queue::Local`], and the first in line of each open then draws a turn: under
    /// [`DOOR`] it locks the turn byte after the last one held, and waits for the one before it
    /// to be let go before it takes the gate, so that opens take the gate in the order they
    /// drew, each waiting on a byte no other waits on.
    Turns,
}

/// The phase gate of b2741a3.
const PAIR: Knobs = Knobs {
    busy_waits: 1,
    first: 4 * MS,
    longest: SEC,
    phasing: Phasing::Pair,
    gap: 0,
    queue: Queue::Kernel,
    together: false,
};

/// The phase gate with a 1.5 ms space between holds.
const SPACED: Knobs = Knobs {
    phasing: Phasing::Spaced(1_500 * US),
    ..PAIR
};

/// The phase gate with a counter in the file.
const COUNTER: Knobs = Knobs {
    phasing: Phasing::Counter,
    ..PAIR
};

/// A pool of 1,024 phase bytes, with no gap after a gate the holder waited for.
const POOLED: Knobs = Knobs {
    phasing: Phasing::Pool(1024),
    ..PAIR
};

/// The phase gate of b919d64: a pool of 1,024 phase bytes, and a gap as long as a writer's
/// longest sleep between tests of its phase byte after a gate the holder waited for, with takers
/// queued for the gate byte in the kernel.
const KERNEL: Knobs = Knobs {
    gap: PHASE_POLL.1,
    ..POOLED
};

/// [`KERNEL`] with no try for the gate byte before a blocking take.
const BLOCKING: Knobs = Knobs {
    queue: Queue::Blocking,
    ..KERNEL
};

/// [`KERNEL`] with takers of one open in line for the gate.
const LOCAL: Knobs = Knobs {
    queue: Queue::Local,
    ..KERNEL
};

/// [`KERNEL`] with takers in line in their open and turns across opens.
const TURNED: Knobs = Knobs {
    queue: Queue::Turns,
    ..KERNEL
};

/// The chosen phase gate: [`TURNED`], with a pull's landing and integration in one turn.
const CHOSEN: Knobs = Knobs {
    together: true,
    ..TURNED
};

/// The phase gate without its rule for busy branches.
const BLIND: Knobs = Knobs {
    busy_waits: 0,
    ..CHOSEN
};

/// The phase gate with a rule that waits for as many phases as the branch stays busy.
const ALWAYS: Knobs = Knobs {
    busy_waits: u32::MAX,
    ..CHOSEN
};

/// How long things take, drawn from these ranges.
#[derive(Clone, Copy, Debug)]
struct Timing {
    wake: (u64, u64),
    preempt: u64,
    preempted: (u64, u64),
    slack: (u64, u64),
    syscall: u64,
    commit: (u64, u64),
    pause: (u64, u64),
    fair: (u64, u64),
}

impl Timing {
    fn calm() -> Timing {
        Timing {
            wake: (US, 10 * US),
            preempt: 2_000,
            preempted: (100 * US, 4 * MS),
            slack: (50 * US, 80 * US),
            syscall: US,
            commit: (20 * US, 60 * US),
            pause: (300 * US, MS),
            fair: (0, MS),
        }
    }

    fn busy() -> Timing {
        Timing {
            wake: (5 * US, 300 * US),
            preempt: 40_000,
            preempted: (MS, 12 * MS),
            ..Timing::calm()
        }
    }

    fn hostile() -> Timing {
        Timing {
            wake: (5 * US, 500 * US),
            preempt: 10_000,
            preempted: (5 * MS, 50 * MS),
            ..Timing::calm()
        }
    }
}

/// A point on a grid of 400 offsets 25 µs apart, which a scenario adds to when a closure joins or
/// a thread starts, so that the seeds of a sweep cover 10 ms of phase against the rounds.
fn offset(seed: u64) -> u64 {
    (seed / 3) % 400 * 25 * US
}

struct Rng(u64);

impl Rng {
    fn bits(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn within(&mut self, (low, high): (u64, u64)) -> u64 {
        match high > low {
            true => low + self.bits() % (high - low + 1),
            false => low,
        }
    }

    fn chance(&mut self, per_million: u64) -> bool {
        self.bits() % 1_000_000 < per_million
    }
}

#[derive(Default)]
struct Meta {
    next: Option<u64>,
    done: bool,
    joiners: Vec<usize>,
    holds: usize,
}

#[derive(Default)]
struct Lane {
    holder: Option<usize>,
    parked: VecDeque<usize>,
    arriving: usize,
    queued: VecDeque<usize>,
    fair_at: u64,
    group: bool,
    crossed: bool,
}

#[derive(Default)]
struct Open {
    lanes: HashMap<usize, Lane>,
    attached: Vec<usize>,
    held: HashMap<u64, Option<u32>>,
    whole: bool,
    waiting: usize,
    moved: Vec<usize>,
    taken: Vec<u64>,
    freed: VecDeque<usize>,
    requeued: VecDeque<usize>,
    handing: bool,
    phase: Option<u64>,
    phased: Vec<usize>,
    numbered: u64,
    coalesce: bool,
    tickets: u64,
    serving: u64,
    served: Vec<usize>,
}

struct Lock {
    id: u64,
    owner: usize,
    start: u64,
    end: u64,
}

/// What a blocked `F_OFD_SETLKW` waits on, as the kernel links it: a lock, or a request that
/// was itself blocked first and that it conflicts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Blocker {
    Lock(u64),
    Task(usize),
}

/// A blocked `F_OFD_SETLKW`: linked under its blocker while it sleeps, and unlinked, with its own
/// waiters still linked under it, once woken.
struct Waiter {
    owner: usize,
    start: u64,
    end: u64,
    on: Option<Blocker>,
    seq: u64,
}

/// What a run saw.
#[derive(Default, Debug)]
struct Seen {
    cycles: Vec<u64>,
    joined: Vec<u64>,
    commits: u64,
    longest_commit: u64,
    marks: Vec<(&'static str, u64)>,
    gate_wait: u64,
    overwaits: u64,
    late_waits: u64,
    most_late: u64,
    pulls: Vec<u64>,
}

struct St {
    now: u64,
    rng: Rng,
    cur: usize,
    timing: Timing,
    design: Design,
    meta: Vec<Meta>,
    heap: BinaryHeap<Reverse<(u64, u64, usize)>>,
    spawned: Vec<(usize, Task)>,
    opens: Vec<Open>,
    locks: Vec<Lock>,
    lock_ids: u64,
    waiters: HashMap<usize, Waiter>,
    waiter_seq: u64,
    began: Option<u64>,
    stop: bool,
    seen: Seen,
    ends: u64,
    counter: u64,
    helpers: HashMap<(usize, usize, bool), usize>,
    hold_began: Option<u64>,
}

impl St {
    fn wake_at(&mut self, task: usize, at: u64) {
        let mut at = at.max(self.now) + self.rng.within(self.timing.wake);
        if self.rng.chance(self.timing.preempt) {
            at += self.rng.within(self.timing.preempted);
        }
        let meta = &mut self.meta[task];
        if meta.done || meta.next.is_some_and(|next| next <= at) {
            return;
        }
        meta.next = Some(at);
        let tie = self.rng.bits();
        self.heap.push(Reverse((at, tie, task)));
    }

    fn wake(&mut self, task: usize) {
        self.wake_at(task, self.now);
    }

    fn wake_all(&mut self, tasks: Vec<usize>) {
        for task in tasks {
            self.wake(task);
        }
    }

    fn conflict(&self, owner: usize, start: u64, end: u64) -> Option<u64> {
        self.locks
            .iter()
            .filter(|lock| lock.owner != owner && lock.start < end && start < lock.end)
            .map(|lock| lock.start)
            .min()
    }

    /// The lock in the way of `owner` taking `start..end`, the first in the file's list, which
    /// keeps each owner's locks together in the order the owners first locked.
    fn blocking(&self, owner: usize, start: u64, end: u64) -> Option<u64> {
        self.locks
            .iter()
            .find(|lock| lock.owner != owner && lock.start < end && start < lock.end)
            .map(|lock| lock.id)
    }

    fn grant(&mut self, owner: usize, start: u64, end: u64) {
        let same = |lock: &Lock| lock.owner == owner && lock.start == start && lock.end == end;
        if self.locks.iter().any(same) {
            return;
        }
        self.lock_ids += 1;
        let lock = Lock {
            id: self.lock_ids,
            owner,
            start,
            end,
        };
        let first = self.locks.iter().position(|held| held.owner == owner);
        let after = first.map_or(self.locks.len(), |first| {
            first
                + self.locks[first..]
                    .iter()
                    .take_while(|held| held.owner == owner && held.start < start)
                    .count()
        });
        self.locks.insert(after, lock);
    }

    fn try_lock(&mut self, owner: usize, start: u64, end: u64) -> bool {
        if self.conflict(owner, start, end).is_some() {
            return false;
        }
        self.grant(owner, start, end);
        true
    }

    /// The requests linked directly under `blocker`, in the order they were linked.
    fn linked(&self, blocker: Blocker) -> Vec<usize> {
        let mut linked: Vec<(u64, usize)> = self
            .waiters
            .iter()
            .filter(|(_, waiter)| waiter.on == Some(blocker))
            .map(|(task, waiter)| (waiter.seq, *task))
            .collect();
        linked.sort_unstable();
        linked.into_iter().map(|(_, task)| task).collect()
    }

    /// Unlinks and wakes the requests linked directly under `blocker`, as
    /// `__locks_wake_up_blocks` does; those linked under them stay linked there.
    fn wake_linked(&mut self, blocker: Blocker) {
        for task in self.linked(blocker) {
            if let Some(waiter) = self.waiters.get_mut(&task) {
                waiter.on = None;
            }
            self.wake(task);
        }
    }

    /// Links `me` under the lock `id` in its way, as `__locks_insert_block` does: below the first
    /// request linked there that it conflicts with, and so on down, at the tail. It first wakes
    /// the requests linked under it, which retry on their own.
    fn link(&mut self, me: usize, owner: usize, (start, end): (u64, u64), id: u64) {
        self.wake_linked(Blocker::Task(me));
        let mut on = Blocker::Lock(id);
        'down: loop {
            for task in self.linked(on) {
                let other = &self.waiters[&task];
                if other.owner != owner && other.start < end && start < other.end {
                    on = Blocker::Task(task);
                    continue 'down;
                }
            }
            break;
        }
        self.waiter_seq += 1;
        let seq = self.waiter_seq;
        self.waiters.insert(
            me,
            Waiter {
                owner,
                start,
                end,
                on: Some(on),
                seq,
            },
        );
    }

    fn unlock(&mut self, owner: usize, start: u64, end: u64) {
        let gone: Vec<u64> = self
            .locks
            .iter()
            .filter(|lock| lock.owner == owner && lock.start == start && lock.end == end)
            .map(|lock| lock.id)
            .collect();
        self.locks
            .retain(|lock| !(lock.owner == owner && lock.start == start && lock.end == end));
        for id in gone {
            self.wake_linked(Blocker::Lock(id));
        }
    }

    fn lane(&mut self, open: usize, branch: usize) -> &mut Lane {
        self.opens[open].lanes.entry(branch).or_default()
    }

    fn lane_try(&mut self, open: usize, branch: usize) -> bool {
        let me = self.cur;
        let lane = self.lane(open, branch);
        if lane.holder.is_some() {
            return false;
        }
        lane.holder = Some(me);
        self.meta[me].holds += 1;
        true
    }

    /// Lets the mutex of `open`'s table go: wakes the next thread moved to it from the condition
    /// variable, if any.
    fn hand_on(&mut self, open: usize) {
        let next = self.opens[open].requeued.pop_front();
        self.opens[open].handing = next.is_some();
        if let Some(next) = next {
            self.wake(next);
        }
    }

    fn moved(&mut self, open: usize) {
        let moved = std::mem::take(&mut self.opens[open].moved);
        self.wake_all(moved);
    }

    fn byte_release(&mut self, open: usize, branch: usize) {
        let at = BYTES + branch as u64;
        match self.opens[open].held.get_mut(&at) {
            Some(Some(count)) if *count > 1 => *count -= 1,
            _ => {
                self.opens[open].held.remove(&at);
                self.unlock(open, at, at + 1);
                self.moved(open);
            }
        }
    }

    fn lane_release(&mut self, open: usize, branch: usize, fair: bool) {
        let now = self.now;
        let fairly = self.rng.within(self.timing.fair);
        let lane = self.lane(open, branch);
        let holder = lane.holder.take().expect("a lane released by its holder");
        let uncross = !lane.group && lane.crossed;
        if uncross {
            lane.crossed = false;
        }
        let next = lane.parked.pop_front();
        let handed = next.filter(|_| fair || now >= lane.fair_at);
        if handed.is_some() {
            lane.holder = handed;
            lane.fair_at = now + fairly;
        }
        let queued: Vec<usize> = match fair {
            true => lane.queued.iter().copied().collect(),
            false => lane.queued.front().copied().into_iter().collect(),
        };
        self.meta[holder].holds -= 1;
        if uncross {
            self.byte_release(open, branch);
        }
        if let Some(next) = next {
            self.wake(next);
        }
        self.wake_all(queued);
    }

    fn on_the_way(&mut self, open: usize, branches: &[usize]) -> bool {
        branches.iter().any(|branch| {
            let lane = self.lane(open, *branch);
            lane.arriving > 0 || !lane.queued.is_empty()
        })
    }

    fn draw(&mut self, range: (u64, u64)) -> u64 {
        self.rng.within(range)
    }

    fn holds(&self) -> usize {
        self.meta[self.cur].holds
    }

    fn branch_busy(&self, open: usize, branch: usize) -> bool {
        let at = BYTES + branch as u64;
        self.conflict(open, at, at + 1).is_some() || self.opens[open].held.contains_key(&at)
    }
}

/// A handle on the model that tasks share.
#[derive(Clone)]
struct W(Rc<RefCell<St>>);

impl W {
    fn st(&self) -> RefMut<'_, St> {
        self.0.borrow_mut()
    }

    fn now(&self) -> u64 {
        self.st().now
    }

    fn design(&self) -> Design {
        self.st().design
    }

    fn spawn(&self, task: Task) -> usize {
        let mut st = self.st();
        let id = st.meta.len();
        st.meta.push(Meta::default());
        st.spawned.push((id, task));
        st.wake(id);
        id
    }

    async fn until(&self, mut ready: impl FnMut(&mut St, usize) -> bool) {
        poll_fn(|_| {
            let mut st = self.st();
            let me = st.cur;
            match ready(&mut st, me) {
                true => Poll::Ready(()),
                false => Poll::Pending,
            }
        })
        .await;
    }

    async fn at(&self, at: u64) {
        self.until(|st, me| {
            if st.now >= at {
                return true;
            }
            st.wake_at(me, at);
            false
        })
        .await;
    }

    async fn cpu(&self, length: u64) {
        let at = self.now() + length;
        self.at(at).await;
    }

    async fn sleep(&self, length: u64) {
        let at = {
            let mut st = self.st();
            let slack = st.timing.slack;
            st.now + length + st.rng.within(slack)
        };
        self.at(at).await;
    }

    async fn syscall(&self) {
        let length = self.st().timing.syscall;
        self.cpu(length).await;
    }

    async fn join(&self, task: usize) {
        self.until(|st, me| {
            if st.meta[task].done {
                return true;
            }
            st.meta[task].joiners.push(me);
            false
        })
        .await;
    }

    /// `F_OFD_SETLKW`: granted at once if nothing is in the way, and otherwise linked under what
    /// is, until an unlock or a request it was linked under wakes it to try again.
    async fn lock(&self, owner: usize, start: u64, end: u64) -> bool {
        self.syscall().await;
        let mut blocked = false;
        self.until(|st, me| {
            if st
                .waiters
                .get(&me)
                .is_some_and(|waiter| waiter.on.is_some())
            {
                return false;
            }
            match st.blocking(owner, start, end) {
                None => {
                    st.grant(owner, start, end);
                    st.wake_linked(Blocker::Task(me));
                    st.waiters.remove(&me);
                    true
                }
                Some(id) => {
                    blocked = true;
                    st.link(me, owner, (start, end), id);
                    false
                }
            }
        })
        .await;
        blocked
    }

    async fn conflict(&self, owner: usize, start: u64, end: u64) -> Option<u64> {
        self.syscall().await;
        self.st().conflict(owner, start, end)
    }

    async fn try_lock(&self, owner: usize, start: u64, end: u64) -> bool {
        self.syscall().await;
        self.st().try_lock(owner, start, end)
    }

    async fn unlock(&self, owner: usize, start: u64, end: u64) {
        self.syscall().await;
        self.st().unlock(owner, start, end);
    }

    async fn lane(&self, open: usize, branch: usize) {
        self.lane_within(open, branch, None).await;
    }

    async fn lane_within(&self, open: usize, branch: usize, within: Option<u64>) -> bool {
        let deadline = within.map(|length| self.now() + length);
        let mut parked = false;
        let mut got = false;
        self.until(|st, me| {
            let now = st.now;
            let lane = st.lane(open, branch);
            if lane.holder == Some(me) {
                lane.parked.retain(|task| *task != me);
                lane.arriving -= usize::from(parked);
                st.meta[me].holds += 1;
                got = true;
                return true;
            }
            if lane.holder.is_none() {
                lane.parked.retain(|task| *task != me);
                lane.holder = Some(me);
                lane.arriving -= usize::from(parked);
                st.meta[me].holds += 1;
                got = true;
                return true;
            }
            if deadline.is_some_and(|at| now >= at) {
                lane.parked.retain(|task| *task != me);
                lane.arriving -= usize::from(parked);
                return true;
            }
            if !parked {
                parked = true;
                lane.arriving += 1;
            }
            if !lane.parked.contains(&me) {
                lane.parked.push_back(me);
            }
            if let Some(at) = deadline {
                st.wake_at(me, at);
            }
            false
        })
        .await;
        got
    }

    async fn enter(&self, open: usize, branch: usize) {
        self.until(|st, me| {
            if st.lane_try(open, branch) {
                st.lane(open, branch).queued.retain(|task| *task != me);
                return true;
            }
            let lane = st.lane(open, branch);
            if !lane.queued.contains(&me) {
                lane.queued.push_back(me);
            }
            false
        })
        .await;
    }

    async fn take_byte(&self, open: usize, branch: usize, wait: bool) -> bool {
        let at = BYTES + branch as u64;
        let mut counted = false;
        let mut verdict = None;
        self.until(|st, me| {
            let shared = &mut st.opens[open];
            if shared.whole {
                if !wait {
                    verdict = Some(false);
                    return true;
                }
                if !counted {
                    counted = true;
                    shared.waiting += 1;
                }
                shared.moved.push(me);
                return false;
            }
            if counted {
                counted = false;
                shared.waiting -= 1;
            }
            match shared.held.get_mut(&at) {
                Some(Some(count)) => {
                    *count += 1;
                    verdict = Some(true);
                    true
                }
                Some(None) if wait => {
                    shared.moved.push(me);
                    false
                }
                Some(None) => {
                    verdict = Some(false);
                    true
                }
                None => {
                    shared.held.insert(at, None);
                    true
                }
            }
        })
        .await;
        if let Some(verdict) = verdict {
            return verdict;
        }
        let got = match wait {
            true => {
                self.lock(open, at, at + 1).await;
                true
            }
            false => self.try_lock(open, at, at + 1).await,
        };
        let mut st = self.st();
        match got {
            true => st.opens[open].held.insert(at, Some(1)),
            false => st.opens[open].held.remove(&at),
        };
        st.moved(open);
        got
    }

    /// Takes `at` through the table of the bytes `open`'s threads hold, as `Store::hold` does, and
    /// then in the kernel, waiting for both if `wait` is set. The table's condition variable
    /// behaves as `parking_lot`'s: a thread that runs takes a free byte at once, and a release
    /// moves every thread waiting on the condition variable, in order, to the mutex, which wakes
    /// them one at a time as each lets it go; one that finds its byte taken waits again at the
    /// back. It returns whether it took the byte, and with `wait` set whether it had to wait.
    async fn hold(&self, open: usize, at: u64, wait: bool) -> bool {
        self.held(open, at, wait).await.0
    }

    async fn held(&self, open: usize, at: u64, wait: bool) -> (bool, bool) {
        let mut free = false;
        let mut parked = false;
        self.until(|st, me| {
            let shared = &mut st.opens[open];
            if parked && (shared.freed.contains(&me) || shared.requeued.contains(&me)) {
                return false;
            }
            if !shared.taken.contains(&at) {
                shared.taken.push(at);
                free = true;
            }
            if parked {
                st.hand_on(open);
            }
            let shared = &mut st.opens[open];
            if free || !wait {
                return true;
            }
            shared.freed.push_back(me);
            parked = true;
            false
        })
        .await;
        if !free {
            return (false, parked);
        }
        let got = match wait {
            true => {
                let blocked = self.lock(open, at, at + 1).await;
                parked |= blocked;
                true
            }
            false => self.try_lock(open, at, at + 1).await,
        };
        if !got {
            self.release(open, at, false).await;
        }
        (got, parked)
    }

    async fn release(&self, open: usize, at: u64, locked: bool) {
        if locked {
            self.unlock(open, at, at + 1).await;
        }
        let mut st = self.st();
        let shared = &mut st.opens[open];
        shared.taken.retain(|held| *held != at);
        let freed: Vec<usize> = shared.freed.drain(..).collect();
        shared.requeued.extend(freed);
        if !shared.handing {
            st.hand_on(open);
        }
    }

    async fn seal(&self, open: usize, branch: usize) {
        if self.st().lane(open, branch).group {
            place(self, open).await;
            self.st().lane(open, branch).group = false;
        }
    }

    async fn uncross(&self, open: usize, branch: usize) {
        let mut st = self.st();
        let lane = st.lane(open, branch);
        if lane.crossed && !lane.group {
            lane.crossed = false;
            st.byte_release(open, branch);
        }
    }

    fn release_lanes(&self, open: usize, lanes: &[usize], fair: bool) {
        let mut st = self.st();
        for branch in lanes.iter().rev() {
            st.lane_release(open, *branch, fair);
        }
    }

    async fn pause(&self) {
        let length = {
            let mut st = self.st();
            {
                let range = st.timing.pause;
                st.draw(range)
            }
        };
        self.cpu(length).await;
    }

    fn mark(&self, what: &'static str) {
        let mut st = self.st();
        let now = st.now;
        st.seen.marks.push((what, now));
    }
}

fn gate_epoch(held: u64) -> u64 {
    let (mut epoch, longest) = EPOCH;
    while epoch < longest && epoch.saturating_mul(2) <= held {
        epoch *= 2;
    }
    epoch
}

fn epoch_end(now: u64, epoch: u64) -> u64 {
    (now / epoch + 1) * epoch
}

fn patience(began: Option<u64>, now: u64) -> u64 {
    gate_epoch(began.map_or(0, |at| now.saturating_sub(at))) * 3
}

/// A writer's wait at the gate before it takes its lane, as each design has it.
async fn pass_gate(w: &W, open: usize, branch: usize) {
    match w.design() {
        Design::Epochs => {
            if w.conflict(open, GATE, GATE + 1).await.is_none() {
                return;
            }
            let until = {
                let st = w.st();
                let held = st.began.map_or(0, |at| st.now.saturating_sub(at));
                let held = held / EPOCH.0 * EPOCH.0;
                epoch_end(st.now, gate_epoch(held))
            };
            let mut pause = EPOCH_POLL.0;
            loop {
                let now = w.now();
                if now >= until {
                    return;
                }
                w.sleep(pause.min(until - now)).await;
                pause = (pause * 2).min(EPOCH_POLL.1);
                if w.conflict(open, GATE, GATE + 1).await.is_none() {
                    return;
                }
            }
        }
        Design::Rounds => {
            if w.conflict(open, GATE, GATE + 1).await.is_none() {
                return;
            }
            let started = w.now();
            let mut pause = ROUND_POLL.0;
            while w.now() - started < ROUND_WAIT {
                w.sleep(pause).await;
                pause = (pause * 2).min(ROUND_POLL.1);
                if w.conflict(open, GATE, GATE + 1).await.is_none() {
                    return;
                }
            }
        }
        Design::Phases(knobs) => pass_phase(w, open, branch, knobs).await,
    }
}

async fn pass_phase(w: &W, open: usize, branch: usize, knobs: Knobs) {
    let started = w.now();
    let mut deferred = 0;
    let mut age = None;
    loop {
        if !await_phase(w, open, knobs.phasing, started, &mut age).await
            || deferred >= knobs.busy_waits
        {
            break;
        }
        w.syscall().await;
        if !w.st().branch_busy(open, branch) {
            break;
        }
        deferred += 1;
    }
    let mut st = w.st();
    let took = st.now - started;
    st.seen.gate_wait = st.seen.gate_wait.max(took);
    if let Some(age) = age {
        let allowed = 3 * (age + knobs.first) + LATE;
        if took > allowed {
            st.seen.late_waits += 1;
            st.seen.most_late = st.seen.most_late.max(took - allowed);
        }
    }
}

/// How much longer than the gate promises a writer may wait there, for threads preempted for up to
/// 50 ms at a time: the round of the hold it found, at most the hold's age and the first round, and
/// the next round, twice as long.
const LATE: u64 = 200 * MS;

fn note_age(w: &W, age: &mut Option<u64>) {
    if age.is_none() {
        let st = w.st();
        *age = Some(
            st.hold_began
                .map_or(0, |began| st.now.saturating_sub(began)),
        );
    }
}

/// Waits until the end of the phase held now, or until [`PHASE_WAIT`] after `started`, and
/// returns whether a phase was held and ended. A writer that tests the phase after it ended and
/// still finds it held counts in [`Seen::overwaits`].
async fn await_phase(
    w: &W,
    open: usize,
    phasing: Phasing,
    started: u64,
    age: &mut Option<u64>,
) -> bool {
    let local = w.st().opens[open].phase;
    if let Some(phase) = local {
        note_age(w, age);
        let first = w.st().ends;
        let (mut ended, mut over) = (false, false);
        w.until(|st, me| {
            if st.opens[open].phase != Some(phase) {
                ended = true;
                return true;
            }
            if st.ends > first {
                over = true;
            }
            if st.now - started >= PHASE_WAIT {
                return true;
            }
            st.opens[open].phased.push(me);
            st.wake_at(me, started + PHASE_WAIT);
            false
        })
        .await;
        w.st().seen.overwaits += u64::from(over);
        return ended;
    }
    let (first, held) = match phasing {
        Phasing::Pair | Phasing::Spaced(_) => {
            let found = w.conflict(open, PHASES, PHASES + 2).await;
            let first = w.st().ends;
            if found.is_none() {
                return false;
            }
            note_age(w, age);
            let held = match w.conflict(open, PHASES, PHASES + 1).await {
                Some(_) => PHASES,
                None => PHASES + 1,
            };
            (first, held)
        }
        Phasing::Pool(count) => {
            let found = w.conflict(open, POOL, POOL + 2 * count).await;
            let first = w.st().ends;
            let Some(held) = found else {
                return false;
            };
            note_age(w, age);
            (first, held)
        }
        Phasing::Counter => {
            if w.conflict(open, FLAG, FLAG + 1).await.is_none() {
                return false;
            }
            note_age(w, age);
            w.syscall().await;
            let counted = w.st().counter;
            let mut pause = PHASE_POLL.0;
            loop {
                w.sleep(pause).await;
                pause = (pause * 2).min(PHASE_POLL.1);
                w.syscall().await;
                if w.st().counter != counted {
                    return true;
                }
                if w.now() - started >= PHASE_WAIT {
                    return false;
                }
            }
        }
    };
    let mut pause = PHASE_POLL.0;
    let mut over = false;
    let ended = loop {
        if w.now() - started >= PHASE_WAIT {
            break false;
        }
        w.sleep(pause).await;
        pause = (pause * 2).min(PHASE_POLL.1);
        if w.conflict(open, held, held + 1).await.is_none() {
            break true;
        }
        if w.st().ends > first {
            over = true;
        }
    };
    w.st().seen.overwaits += u64::from(over);
    ended
}

/// Whether a write that does not wait finds the gate held, testing it once.
async fn gate_held(w: &W, open: usize) -> bool {
    match w.design() {
        Design::Epochs | Design::Rounds => w.conflict(open, GATE, GATE + 1).await.is_some(),
        Design::Phases(knobs) => {
            if w.st().opens[open].phase.is_some() {
                return true;
            }
            let (start, end) = match knobs.phasing {
                Phasing::Pair | Phasing::Spaced(_) => (PHASES, PHASES + 2),
                Phasing::Pool(count) => (POOL, POOL + 2 * count),
                Phasing::Counter => (FLAG, FLAG + 1),
            };
            w.conflict(open, start, end).await.is_some()
        }
    }
}

/// A blocking write: the gate, the lane, the byte, the closure and the commit.
fn commit(w: W, open: usize, branch: usize, closure: Task) -> Task {
    Box::pin(async move {
        let started = w.now();
        if w.st().holds() == 0 {
            pass_gate(&w, open, branch).await;
        }
        w.lane(open, branch).await;
        w.seal(open, branch).await;
        let crossed = w.st().lane(open, branch).crossed;
        if !crossed {
            w.take_byte(open, branch, true).await;
        }
        closure.await;
        place(&w, open).await;
        let mut st = w.st();
        if !crossed {
            st.byte_release(open, branch);
        }
        st.lane_release(open, branch, false);
        st.seen.commits += 1;
        let took = st.now - started;
        st.seen.longest_commit = st.seen.longest_commit.max(took);
    })
}

/// A commit's placement: the placement lock, held while the frame is drafted and written.
async fn place(w: &W, open: usize) {
    w.hold(open, COMMIT, true).await;
    let length = {
        let mut st = w.st();
        let range = st.timing.commit;
        st.draw(range)
    };
    w.cpu(length).await;
    w.release(open, COMMIT, true).await;
}

/// The thread that an awaited write held up at the gate or its byte hands its wait to: one for
/// each byte and kind of wait, which every awaited write held up there joins, as `await_lane`
/// shares one.
fn waiting_thread(w: &W, open: usize, branch: usize, gated: bool) -> usize {
    let key = (open, branch, gated);
    let running = {
        let st = w.st();
        st.helpers
            .get(&key)
            .copied()
            .filter(|task| !st.meta[*task].done)
    };
    if let Some(task) = running {
        return task;
    }
    let w2 = w.clone();
    let task = w.spawn(Box::pin(async move {
        if gated {
            pass_gate(&w2, open, branch).await;
        }
        w2.take_byte(open, branch, true).await;
        let mut st = w2.st();
        st.byte_release(open, branch);
        st.helpers.remove(&key);
    }));
    w.st().helpers.insert(key, task);
    task
}

/// An awaited write under the phase gate, as db.rs polls one: the lane through the queue, then the
/// byte without waiting, after a test of the gate unless it was held up before; held up by
/// either, it lets the lane go, waits for the thread it hands the wait to and tries again, and
/// from then on goes to its byte without the gate.
fn commit_awaited_in_phases(w: W, open: usize, branch: usize, closure: Task) -> Task {
    Box::pin(async move {
        let mut held_up = false;
        loop {
            w.enter(open, branch).await;
            w.seal(open, branch).await;
            let crossed = w.st().lane(open, branch).crossed;
            if !crossed {
                let gated = !held_up && w.st().holds() == 1;
                let free =
                    !(gated && gate_held(&w, open).await) && w.take_byte(open, branch, false).await;
                if !free {
                    w.st().lane_release(open, branch, false);
                    held_up |= gated;
                    let thread = waiting_thread(&w, open, branch, gated);
                    w.join(thread).await;
                    continue;
                }
            }
            closure.await;
            place(&w, open).await;
            let mut st = w.st();
            if !crossed {
                st.byte_release(open, branch);
            }
            st.lane_release(open, branch, false);
            st.seen.commits += 1;
            return;
        }
    })
}

/// An awaited write polled on one thread: the lane through the queue of awaited writes, a test of
/// the gate and the byte that does not wait, and a waiting thread when either is not free.
fn commit_awaited(w: W, open: usize, branch: usize, closure: Task) -> Task {
    if matches!(w.design(), Design::Phases(_)) {
        return commit_awaited_in_phases(w, open, branch, closure);
    }
    Box::pin(async move {
        let mut held_up: Option<u64> = None;
        let mut released = false;
        loop {
            w.enter(open, branch).await;
            w.seal(open, branch).await;
            let gated = w.st().holds() == 1
                && match w.design() {
                    Design::Epochs => held_up.is_none_or(|until| w.now() < until),
                    Design::Rounds => held_up.is_none(),
                    Design::Phases(_) => !released,
                };
            if gated && gate_held(&w, open).await {
                if held_up.is_none() {
                    held_up = Some(match w.design() {
                        Design::Epochs => {
                            let st = w.st();
                            let held = st.began.map_or(0, |at| st.now.saturating_sub(at));
                            epoch_end(st.now, gate_epoch(held / EPOCH.0 * EPOCH.0))
                        }
                        _ => 0,
                    });
                }
                w.st().lane_release(open, branch, false);
                let helper = {
                    let w2 = w.clone();
                    w.spawn(Box::pin(async move {
                        pass_gate(&w2, open, branch).await;
                        w2.take_byte(open, branch, true).await;
                        w2.st().byte_release(open, branch);
                    }))
                };
                w.join(helper).await;
                released = true;
                continue;
            }
            let crossed = w.st().lane(open, branch).crossed;
            if !crossed && !w.take_byte(open, branch, false).await {
                w.st().lane_release(open, branch, false);
                let helper = {
                    let w2 = w.clone();
                    w.spawn(Box::pin(async move {
                        w2.take_byte(open, branch, true).await;
                        w2.st().byte_release(open, branch);
                    }))
                };
                w.join(helper).await;
                continue;
            }
            closure.await;
            place(&w, open).await;
            let mut st = w.st();
            if !crossed {
                st.byte_release(open, branch);
            }
            st.lane_release(open, branch, false);
            st.seen.commits += 1;
            return;
        }
    })
}

/// Every branch lock of `open` at once, held for a pause.
fn exclusively(w: W, open: usize) -> Task {
    Box::pin(async move {
        match w.design() {
            Design::Epochs => exclusively_in_epochs(&w, open).await,
            Design::Rounds => exclusively_in_rounds(&w, open).await,
            Design::Phases(knobs) => {
                exclusively_in_phases(&w, open, knobs, None, false).await;
            }
        }
    })
}

fn attached(w: &W, open: usize) -> Vec<usize> {
    let st = w.st();
    let shared = &st.opens[open];
    let mut branches = shared.attached.clone();
    for (branch, lane) in &shared.lanes {
        if lane.group && !branches.contains(branch) {
            branches.push(*branch);
        }
    }
    branches.sort_unstable();
    branches
}

async fn wait_whole(w: &W, open: usize) {
    w.until(|st, me| {
        if !st.opens[open].whole {
            st.opens[open].whole = true;
            return true;
        }
        st.opens[open].moved.push(me);
        false
    })
    .await;
}

async fn drain_own(w: &W, open: usize) {
    loop {
        if w.st().opens[open].held.is_empty() {
            return;
        }
        let at = w.now() + MS;
        w.until(|st, me| {
            if st.opens[open].held.is_empty() || st.now >= at {
                return true;
            }
            st.opens[open].moved.push(me);
            st.wake_at(me, at);
            false
        })
        .await;
    }
}

fn clear_whole(w: &W, open: usize) {
    let mut st = w.st();
    st.opens[open].whole = false;
    st.moved(open);
}

async fn finish(w: &W, open: usize, lanes: &[usize]) {
    w.hold(open, COMMIT, true).await;
    w.pause().await;
    w.release(open, COMMIT, true).await;
    w.unlock(open, RANGE.0, RANGE.1).await;
    clear_whole(w, open);
    w.release_lanes(open, lanes, false);
}

async fn exclusively_in_epochs(w: &W, open: usize) {
    let mut began = None;
    loop {
        let branches = attached(w, open);
        for branch in &branches {
            w.lane(open, *branch).await;
            w.seal(open, *branch).await;
        }
        for branch in &branches {
            w.uncross(open, *branch).await;
        }
        wait_whole(w, open).await;
        drain_own(w, open).await;
        let started = w.now();
        let allowed = patience(began, started);
        let mut pause = EPOCH_POLL.0;
        let mut gate = false;
        let yielded = loop {
            if !gate && w.hold(open, GATE, false).await {
                gate = true;
                let mut st = w.st();
                let at = *began.get_or_insert(st.now);
                st.began = Some(at);
            }
            if gate && w.try_lock(open, RANGE.0, RANGE.1).await {
                break false;
            }
            let now = w.now();
            let waiting = {
                let mut st = w.st();
                st.opens[open].waiting > 0 || st.on_the_way(open, &branches)
            };
            if now - started >= allowed && waiting {
                break true;
            }
            let lead = epoch_end(now, EPOCH.0).saturating_sub(EPOCH_LEAD);
            let until = match lead > now {
                true => lead.min(now + pause),
                false => now + pause,
            };
            w.cpu(until - now).await;
            pause = (pause * 2).min(EPOCH_POLL.1);
        };
        if !yielded {
            finish(w, open, &branches).await;
            w.st().began = None;
            w.release(open, GATE, true).await;
            return;
        }
        if gate {
            w.st().began = None;
            w.release(open, GATE, true).await;
        }
        clear_whole(w, open);
        w.release_lanes(open, &branches, true);
        let start = w.now();
        let mut pause = 20 * US;
        loop {
            if !w.st().on_the_way(open, &branches) {
                break;
            }
            let now = w.now();
            if now - start >= patience(began, now) {
                break;
            }
            w.sleep(pause).await;
            pause = (pause * 2).min(MS);
        }
    }
}

struct Rounds {
    length: u64,
    ends: u64,
}

impl Rounds {
    fn over(&mut self, now: u64) -> bool {
        if now < self.ends {
            return false;
        }
        self.length *= 2;
        self.ends = now + self.length;
        true
    }
}

async fn exclusively_in_rounds(w: &W, open: usize) {
    let mut rounds = Rounds {
        length: ROUND.0,
        ends: w.now() + ROUND.0,
    };
    'again: loop {
        let branches = attached(w, open);
        let mut lanes: Vec<usize> = Vec::new();
        for branch in &branches {
            let coalesce = w.st().opens[open].coalesce;
            let free = !coalesce && w.st().lane_try(open, *branch);
            if !free {
                loop {
                    if w.lane_within(open, *branch, Some(MS)).await {
                        break;
                    }
                    let now = w.now();
                    let waited = w.st().on_the_way(open, &lanes);
                    if rounds.over(now) && waited {
                        w.release_lanes(open, &lanes, true);
                        w.sleep(ROUND_OPEN).await;
                        continue 'again;
                    }
                }
            }
            w.seal(open, *branch).await;
            lanes.push(*branch);
        }
        for branch in &lanes {
            w.uncross(open, *branch).await;
        }
        wait_whole(w, open).await;
        loop {
            if w.st().opens[open].held.is_empty() {
                break;
            }
            let at = w.now() + MS;
            w.until(|st, me| {
                if st.opens[open].held.is_empty() || st.now >= at {
                    return true;
                }
                st.opens[open].moved.push(me);
                st.wake_at(me, at);
                false
            })
            .await;
            let now = w.now();
            let others = w.st().opens[open].waiting > 0;
            let waited = w.st().on_the_way(open, &lanes);
            if rounds.over(now) && (others || waited) {
                clear_whole(w, open);
                w.release_lanes(open, &lanes, true);
                w.sleep(ROUND_OPEN).await;
                continue 'again;
            }
        }
        let mut gate = false;
        let mut taken: Option<u64> = None;
        let mut closed = false;
        let mut opened: Option<u64> = None;
        let mut pause = ROUND_RANGE_POLL.0;
        loop {
            if !gate {
                if w.conflict(open, GATE, GATE + 1).await.is_some() {
                    (closed, opened) = (true, None);
                } else {
                    let now = w.now();
                    if !closed || now - *opened.get_or_insert(now) >= ROUND_OPEN {
                        gate = w.hold(open, GATE, false).await;
                        match gate {
                            true => taken = Some(w.now()),
                            false => (closed, opened) = (true, None),
                        }
                    }
                }
            }
            if gate && w.try_lock(open, RANGE.0, RANGE.1).await {
                finish(w, open, &lanes).await;
                w.release(open, GATE, true).await;
                return;
            }
            let now = w.now();
            let over = rounds.over(now);
            let waiting = {
                let mut st = w.st();
                st.opens[open].waiting > 0 || st.on_the_way(open, &lanes)
            };
            if over && waiting {
                if gate {
                    w.release(open, GATE, true).await;
                }
                clear_whole(w, open);
                w.release_lanes(open, &lanes, true);
                w.sleep(ROUND_OPEN).await;
                continue 'again;
            }
            if over || taken.is_some_and(|at| now - at >= ROUND.1) {
                if gate {
                    gate = false;
                    w.release(open, GATE, true).await;
                    w.sleep(ROUND_OPEN).await;
                    (taken, closed) = (None, false);
                }
                pause = ROUND_RANGE_POLL.0;
                continue;
            }
            w.sleep(pause).await;
            pause = (pause * 2).min(ROUND_RANGE_POLL.1);
        }
    }
}

/// One try at every lane of `open`, its bytes and the range, all at once or none.
async fn try_everything(w: &W, open: usize) -> Option<Vec<usize>> {
    let grouped: Vec<usize> = {
        let st = w.st();
        let lanes = &st.opens[open].lanes;
        let mut grouped: Vec<usize> = lanes
            .iter()
            .filter(|(_, lane)| lane.group)
            .map(|(b, _)| *b)
            .collect();
        grouped.sort_unstable();
        grouped
    };
    for branch in grouped {
        if w.st().lane_try(open, branch) {
            w.seal(open, branch).await;
            w.release_lanes(open, &[branch], false);
        }
    }
    if !w.st().opens[open].held.is_empty() {
        return None;
    }
    if w.conflict(open, RANGE.0, RANGE.1).await.is_some() {
        return None;
    }
    let branches = attached(w, open);
    let mut lanes = Vec::new();
    for branch in &branches {
        if !w.st().lane_try(open, *branch) {
            w.release_lanes(open, &lanes, false);
            return None;
        }
        w.seal(open, *branch).await;
        lanes.push(*branch);
    }
    for branch in &lanes {
        w.uncross(open, *branch).await;
    }
    let marked = {
        let mut st = w.st();
        let shared = &mut st.opens[open];
        let free = shared.held.is_empty() && !shared.whole;
        if free {
            shared.whole = true;
        }
        free
    };
    if marked && w.try_lock(open, RANGE.0, RANGE.1).await {
        return Some(lanes);
    }
    if marked {
        clear_whole(w, open);
    }
    w.release_lanes(open, &lanes, false);
    None
}

/// The phase byte the holder of a [`Phasing::Pool`] gate moves to from `at`.
fn next_in_pool(at: u64, count: u64) -> u64 {
    POOL + (at - POOL + 2) % (2 * count)
}

/// Locks the first free phase byte from `at` on, trying a few, and returns it.
async fn lock_from(w: &W, open: usize, mut at: u64, count: u64) -> Option<u64> {
    for _ in 0..4 {
        if w.try_lock(open, at, at + 1).await {
            return Some(at);
        }
        at = next_in_pool(at, count);
    }
    None
}

/// Waits for this taker's ticket in the line of `open`'s takers, and returns whether it had to.
async fn enter_line(w: &W, open: usize) -> bool {
    let ticket = {
        let mut st = w.st();
        let shared = &mut st.opens[open];
        shared.tickets += 1;
        shared.tickets - 1
    };
    let mut waited = false;
    w.until(|st, me| {
        if st.opens[open].serving == ticket {
            return true;
        }
        waited = true;
        st.opens[open].served.push(me);
        false
    })
    .await;
    waited
}

fn leave_line(w: &W, open: usize) {
    let mut st = w.st();
    st.opens[open].serving += 1;
    let served = std::mem::take(&mut st.opens[open].served);
    st.wake_all(served);
}

fn turn_byte(index: u64) -> u64 {
    TURN_AT + 2 * (index % TURNS)
}

/// Draws a turn under [`DOOR`] and waits for the turn before it to be let go: returns the turn
/// byte held, if one was free, and whether a turn came before it, as the code counts a wait.
async fn take_turn(w: &W, open: usize) -> (Option<u64>, bool) {
    w.lock(open, DOOR, DOOR + 1).await;
    let found = w.conflict(open, TURN_AT, TURN_AT + 2 * TURNS).await;
    let last = match found {
        None => None,
        Some(at) => {
            for _ in 0..TURNS.ilog2() {
                w.syscall().await;
            }
            let st = w.st();
            let first = (at - TURN_AT) / 2;
            let last = (1..TURNS / 2)
                .rev()
                .map(|ahead| first + ahead)
                .find(|index| {
                    let byte = turn_byte(*index);
                    st.conflict(open, byte, byte + 1).is_some()
                })
                .unwrap_or(first);
            Some(last)
        }
    };
    let mine = last.map_or(0, |last| last + 1);
    let held = w.try_lock(open, turn_byte(mine), turn_byte(mine) + 1).await;
    w.unlock(open, DOOR, DOOR + 1).await;
    if !held {
        return (None, false);
    }
    let Some(last) = last else {
        return (Some(turn_byte(mine)), false);
    };
    let before = turn_byte(last);
    w.lock(open, before, before + 1).await;
    w.unlock(open, before, before + 1).await;
    (Some(turn_byte(mine)), true)
}

/// Queues for the gate byte as `queue` has it and takes it: returns the turn byte held, if any,
/// and whether it had to wait. A taker that kept its place in line and its turn from its last
/// hold, as `kept` says, goes straight to the gate byte.
async fn queue_for_gate(
    w: &W,
    open: usize,
    queue: Queue,
    kept: Option<Option<u64>>,
) -> (Option<u64>, bool) {
    let mut waited = false;
    if kept.is_none() && matches!(queue, Queue::Local | Queue::Turns) {
        waited |= enter_line(w, open).await;
    }
    let mut turn = kept.flatten();
    if kept.is_none() && queue == Queue::Turns {
        let (held, behind) = take_turn(w, open).await;
        turn = held;
        waited |= behind;
    }
    if queue == Queue::Blocking {
        let (_, blocked) = w.held(open, GATE, true).await;
        return (turn, waited || blocked);
    }
    if !w.hold(open, GATE, false).await {
        w.hold(open, GATE, true).await;
        waited = true;
    }
    (turn, waited)
}

/// Takes the gate and the first phase as `phasing` has it: the phase byte held, if any, the
/// number the holder's open waits on and the turn byte held, if any.
async fn take_phase_gate(
    w: &W,
    open: usize,
    knobs: Knobs,
    kept: Option<Option<u64>>,
) -> (Option<u64>, Option<u64>, Option<u64>) {
    let phasing = knobs.phasing;
    let (turn, waited) = queue_for_gate(w, open, knobs.queue, kept).await;
    if waited && knobs.gap > 0 {
        w.sleep(knobs.gap).await;
    }
    {
        let mut st = w.st();
        st.hold_began = Some(st.now);
    }
    let (byte, number) = match phasing {
        Phasing::Pair | Phasing::Spaced(_) => {
            let mut byte = None;
            for at in [PHASES, PHASES + 1] {
                if w.try_lock(open, at, at + 1).await {
                    byte = Some(at);
                    break;
                }
            }
            (byte, byte.map(|at| at - PHASES))
        }
        Phasing::Pool(count) => {
            let start = {
                let mut st = w.st();
                POOL + 2 * (st.rng.bits() % count)
            };
            let byte = lock_from(w, open, start, count).await;
            (byte, byte.map(|_| fresh_number(w, open)))
        }
        Phasing::Counter => {
            let held = w.try_lock(open, FLAG, FLAG + 1).await;
            let byte = held.then_some(FLAG);
            (byte, byte.map(|_| fresh_number(w, open)))
        }
    };
    if number.is_some() {
        w.st().opens[open].phase = number;
    }
    (byte, number, turn)
}

fn fresh_number(w: &W, open: usize) -> u64 {
    let mut st = w.st();
    let shared = &mut st.opens[open];
    shared.numbered += 1;
    shared.numbered
}

/// Ends the phase: the next phase byte and number, as `phasing` has it.
async fn end_phase(
    w: &W,
    open: usize,
    phasing: Phasing,
    byte: Option<u64>,
    number: Option<u64>,
) -> (Option<u64>, Option<u64>) {
    let (Some(at), Some(numbered)) = (byte, number) else {
        return (byte, number);
    };
    let (byte, number) = match phasing {
        Phasing::Pair | Phasing::Spaced(_) => {
            let next = PHASES + (numbered + 1) % 2;
            if !w.try_lock(open, next, next + 1).await {
                return (byte, number);
            }
            w.unlock(open, at, at + 1).await;
            (Some(next), Some(numbered + 1))
        }
        Phasing::Pool(count) => {
            let Some(next) = lock_from(w, open, next_in_pool(at, count), count).await else {
                return (byte, number);
            };
            w.unlock(open, at, at + 1).await;
            (Some(next), Some(fresh_number(w, open)))
        }
        Phasing::Counter => {
            w.syscall().await;
            w.st().counter += 1;
            (byte, Some(fresh_number(w, open)))
        }
    };
    let mut st = w.st();
    st.ends += 1;
    st.opens[open].phase = number.filter(|_| byte.is_some());
    let phased = std::mem::take(&mut st.opens[open].phased);
    st.wake_all(phased);
    (byte, number)
}

/// Lets the phase byte, the gate and the turn go, as `knobs` has them, or with `keep` set keeps
/// the place in line and the turn, and returns them.
async fn leave_phase_gate(
    w: &W,
    open: usize,
    knobs: Knobs,
    byte: Option<u64>,
    turn: Option<u64>,
    keep: bool,
) -> Option<Option<u64>> {
    let phasing = knobs.phasing;
    if let Some(at) = byte {
        if phasing == Phasing::Counter {
            w.syscall().await;
            w.st().counter += 1;
        }
        w.unlock(open, at, at + 1).await;
        let mut st = w.st();
        st.ends += 1;
        st.opens[open].phase = None;
        let phased = std::mem::take(&mut st.opens[open].phased);
        st.wake_all(phased);
    }
    if let Phasing::Spaced(space) = phasing {
        w.sleep(space).await;
    }
    w.st().hold_began = None;
    w.release(open, GATE, true).await;
    if keep {
        return Some(turn);
    }
    if let Some(at) = turn {
        w.unlock(open, at, at + 1).await;
    }
    if matches!(knobs.queue, Queue::Local | Queue::Turns) {
        leave_line(w, open);
    }
    None
}

async fn exclusively_in_phases(
    w: &W,
    open: usize,
    knobs: Knobs,
    kept: Option<Option<u64>>,
    keep: bool,
) -> Option<Option<u64>> {
    let (mut byte, mut number, turn) = take_phase_gate(w, open, knobs, kept).await;
    let mut round = knobs.first;
    let mut ends = w.now() + round;
    let mut pause = PHASE_RANGE_POLL.0;
    let lanes = loop {
        if let Some(lanes) = try_everything(w, open).await {
            break lanes;
        }
        let now = w.now();
        if now >= ends {
            (byte, number) = end_phase(w, open, knobs.phasing, byte, number).await;
            round = (round * 2).min(knobs.longest);
            ends = w.now() + round;
            pause = PHASE_RANGE_POLL.0;
            continue;
        }
        w.sleep(pause.min(ends - now)).await;
        pause = (pause * 2).min(PHASE_RANGE_POLL.1).min(round / 8);
    };
    finish(w, open, &lanes).await;
    leave_phase_gate(w, open, knobs, byte, turn, keep).await
}

/// A pull's landing and integration, each taking every branch, `decide` apart: under a phase gate
/// with [`Knobs::together`] set, in one place in line and one turn.
async fn land_and_integrate(w: &W, open: usize, decide: u64) {
    match w.design() {
        Design::Phases(knobs) if knobs.together => {
            let kept = exclusively_in_phases(w, open, knobs, None, true).await;
            w.cpu(decide).await;
            exclusively_in_phases(w, open, knobs, kept, false).await;
        }
        _ => {
            exclusively(w.clone(), open).await;
            w.cpu(decide).await;
            exclusively(w.clone(), open).await;
        }
    }
}

/// The model of one run: the state the tasks share and the executor that runs them.
struct Model {
    w: W,
    tasks: Vec<Option<Task>>,
}

/// How a run ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    Done,
    Stuck,
    Late,
}

impl Model {
    fn new(design: Design, timing: Timing, seed: u64) -> Model {
        let mut rng = Rng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d).wrapping_add(seed));
        let now = 10 * SEC + rng.bits() % SEC;
        let st = St {
            now,
            rng,
            cur: 0,
            timing,
            design,
            meta: Vec::new(),
            heap: BinaryHeap::new(),
            spawned: Vec::new(),
            opens: Vec::new(),
            locks: Vec::new(),
            lock_ids: 0,
            waiters: HashMap::new(),
            waiter_seq: 0,
            began: None,
            stop: false,
            seen: Seen::default(),
            ends: 0,
            counter: 0,
            helpers: HashMap::new(),
            hold_began: None,
        };
        Model {
            w: W(Rc::new(RefCell::new(st))),
            tasks: Vec::new(),
        }
    }

    fn open(&self, attached: &[usize], coalesce: bool) -> usize {
        let mut st = self.w.st();
        st.opens.push(Open {
            attached: attached.to_vec(),
            coalesce,
            ..Open::default()
        });
        st.opens.len() - 1
    }

    fn spawn(&self, task: Task) -> usize {
        self.w.spawn(task)
    }

    fn run(&mut self, limit: u64, done: impl Fn(&St) -> bool) -> End {
        let deadline = self.w.now() + limit;
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            let spawned = std::mem::take(&mut self.w.st().spawned);
            for (id, task) in spawned {
                if self.tasks.len() <= id {
                    self.tasks.resize_with(id + 1, || None);
                }
                self.tasks[id] = Some(task);
            }
            if done(&self.w.st()) {
                return End::Done;
            }
            let next = {
                let mut st = self.w.st();
                loop {
                    match st.heap.pop() {
                        None => break None,
                        Some(Reverse((at, _, id))) if st.meta[id].next == Some(at) => {
                            st.meta[id].next = None;
                            break Some((at, id));
                        }
                        Some(_) => {}
                    }
                }
            };
            let Some((at, id)) = next else {
                return End::Stuck;
            };
            if at > deadline {
                return End::Late;
            }
            {
                let mut st = self.w.st();
                st.now = at;
                st.cur = id;
            }
            let Some(task) = self.tasks.get_mut(id).and_then(Option::as_mut) else {
                continue;
            };
            if task.as_mut().poll(&mut cx).is_ready() {
                self.tasks[id] = None;
                let mut st = self.w.st();
                st.meta[id].done = true;
                let joiners = std::mem::take(&mut st.meta[id].joiners);
                st.wake_all(joiners);
            }
        }
    }
}

fn closure(w: &W, length: (u64, u64)) -> Task {
    let w = w.clone();
    Box::pin(async move {
        let length = {
            let mut st = w.st();
            st.rng.within(length)
        };
        if length > 0 {
            w.sleep(length).await;
        }
    })
}

fn nothing() -> Task {
    Box::pin(async {})
}

/// A thread that commits on `branch` through `open` until the run stops, after `delay`, each
/// closure lasting `length`.
fn writer(w: &W, open: usize, branch: usize, length: (u64, u64), delay: u64) -> Task {
    let w = w.clone();
    Box::pin(async move {
        if delay > 0 {
            w.sleep(delay).await;
        }
        while !w.st().stop {
            let body = closure(&w, length);
            commit(w.clone(), open, branch, body).await;
        }
    })
}

/// `count` cycles through `open`, each a prune if `prune` is set, a copy of `copy` and a pause,
/// noting how long each took.
fn cycles(w: &W, open: usize, count: usize, prune: bool, copy: (u64, u64)) -> Task {
    let w = w.clone();
    Box::pin(async move {
        for _ in 0..count {
            if w.st().stop {
                return;
            }
            let started = w.now();
            w.hold(open, CYCLE, true).await;
            if prune {
                exclusively(w.clone(), open).await;
            }
            let length = {
                let mut st = w.st();
                st.rng.within(copy)
            };
            w.cpu(length).await;
            exclusively(w.clone(), open).await;
            w.release(open, CYCLE, true).await;
            let mut st = w.st();
            let took = st.now - started;
            st.seen.cycles.push(took);
        }
    })
}

/// A closure body that joins a thread committing on `branch` through `open`, awaited or not,
/// noting how long the commit took.
fn join_commit(w: &W, open: usize, branch: usize, awaited: bool) -> Task {
    let w = w.clone();
    Box::pin(async move {
        let started = w.now();
        let write = match awaited {
            true => commit_awaited(w.clone(), open, branch, nothing()),
            false => commit(w.clone(), open, branch, nothing()),
        };
        let thread = w.spawn(write);
        w.join(thread).await;
        let mut st = w.st();
        let took = st.now - started;
        st.seen.joined.push(took);
    })
}

fn then(first: Task, second: Task) -> Task {
    Box::pin(async move {
        first.await;
        second.await;
    })
}

/// What a scenario promises, and the run it is checked on.
struct Verdict {
    end: End,
    seen: Seen,
    failure: Option<String>,
}

impl Verdict {
    fn check(end: End, seen: Seen, ok: impl FnOnce(&Seen) -> Option<String>) -> Verdict {
        let failure = match end {
            End::Done => ok(&seen),
            End::Stuck => Some("every task waits for good".to_string()),
            End::Late => Some("not done in time".to_string()),
        };
        Verdict { end, seen, failure }
    }

    /// As [`Verdict::check`], for a scenario in which the taker is not promised to finish: only
    /// that no task waits for good and that `ok` holds of what was seen.
    fn check_unfinished(end: End, seen: Seen, ok: impl FnOnce(&Seen) -> Option<String>) -> Verdict {
        let failure = match end {
            End::Stuck => Some("every task waits for good".to_string()),
            End::Done | End::Late => ok(&seen),
        };
        Verdict { end, seen, failure }
    }
}

fn longest(values: &[u64]) -> u64 {
    values.iter().copied().max().unwrap_or(0)
}

fn under(what: &str, values: &[u64], bound: u64) -> Option<String> {
    let most = longest(values);
    (most >= bound).then(|| format!("{what} took up to {} ms", most / MS))
}

const A: usize = 1;
const B: usize = 2;
const C: usize = 3;
const HOT: usize = 4;
const MAIN: usize = 5;
const OWN: usize = 10;

/// #128 and #136: five cycles beside `opens` other opens committing on branches of their own,
/// with closures of `length`.
fn own_branches(design: Design, timing: Timing, seed: u64, opens: usize, length: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    for writer_at in 0..opens {
        let open = m.open(&[MAIN, OWN + writer_at], false);
        m.spawn(writer(&m.w, open, OWN + writer_at, (length, length), 0));
    }
    let cycler = m.open(&[MAIN], false);
    let w = m.w.clone();
    m.spawn(then(
        Box::pin(async move { w.sleep(50 * MS).await }),
        cycles(&m.w, cycler, 5, false, (MS, 2 * MS)),
    ));
    let end = m.run(20 * SEC, |st| st.seen.cycles.len() == 5);
    let bound = 250 * MS + 30 * length;
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("a cycle", &seen.cycles, bound)
    })
}

/// #128: 50 commits on a branch of a handle that cycles over and over, beside four opens writing.
fn own_commits_beside_cycles(design: Design, timing: Timing, seed: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    for writer_at in 0..4 {
        let open = m.open(&[MAIN, OWN + writer_at], false);
        m.spawn(writer(&m.w, open, OWN + writer_at, (0, 0), 0));
    }
    let cycler = m.open(&[MAIN, HOT], false);
    m.spawn(cycles(&m.w, cycler, usize::MAX, false, (MS, 2 * MS)));
    let w = m.w.clone();
    m.spawn(Box::pin(async move {
        w.sleep(20 * MS).await;
        for _ in 0..50 {
            commit(w.clone(), cycler, HOT, nothing()).await;
        }
        w.mark("own commits");
    }));
    let end = m.run(20 * SEC, |st| !st.seen.marks.is_empty());
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        (seen.longest_commit >= SEC)
            .then(|| format!("a commit took {} ms", seen.longest_commit / MS))
    })
}

/// #136: a closure on `a` that joins a thread committing on `b` through its own handle, while
/// another handle cycles, awaited or not.
fn joined_beside_cycle(design: Design, timing: Timing, seed: u64, awaited: bool) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let y = m.open(&[A, B, MAIN], false);
    let cycler = m.open(&[MAIN], false);
    let w = m.w.clone();
    let body = then(
        Box::pin({
            let w = w.clone();
            async move {
                w.mark("in a");
                w.sleep(300 * MS + offset(seed)).await;
            }
        }),
        join_commit(&w, y, B, awaited),
    );
    m.spawn(commit(w.clone(), y, A, body));
    m.spawn(Box::pin(async move {
        w.until(|st, me| {
            if !st.seen.marks.is_empty() {
                return true;
            }
            st.wake_at(me, st.now + 100 * US);
            false
        })
        .await;
        cycles(&w, cycler, 1, true, (MS, 2 * MS)).await;
    }));
    let end = m.run(20 * SEC, |st| {
        st.seen.cycles.len() == 1 && st.seen.joined.len() == 1
    });
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("the joined commit", &seen.joined, SEC)
    })
}

/// #142: one cycle beside `threads` threads in each of `opens` handles, on branches of their own,
/// whose closures last `length` and begin `length / threads` apart.
fn staggered(
    design: Design,
    timing: Timing,
    seed: u64,
    opens: usize,
    threads: usize,
    length: u64,
) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let total = opens * threads;
    for open_at in 0..opens {
        let branches: Vec<usize> = (0..threads).map(|t| OWN + open_at * threads + t).collect();
        let open = m.open(&branches, false);
        for (t, branch) in branches.iter().enumerate() {
            let index = (open_at * threads + t) as u64;
            m.spawn(writer(
                &m.w,
                open,
                *branch,
                (length, length),
                length * index / total as u64,
            ));
        }
    }
    let cycler = m.open(&[MAIN], false);
    let w = m.w.clone();
    m.spawn(then(
        Box::pin(async move { w.sleep(2 * length).await }),
        cycles(&m.w, cycler, 1, false, (MS, 2 * MS)),
    ));
    let end = m.run(20 * SEC, |st| st.seen.cycles.len() == 1);
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("the cycle", &seen.cycles, 6 * length + 2 * SEC)
    })
}

/// #142: a closure on `a` in one handle that joins a commit on `b` through another handle while
/// that one cycles.
fn joined_through_cycler(design: Design, timing: Timing, seed: u64, awaited: bool) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let y = m.open(&[A, B, MAIN], false);
    let cycler = m.open(&[A, B, MAIN], false);
    let w = m.w.clone();
    let body = then(
        Box::pin({
            let w = w.clone();
            async move {
                w.mark("in a");
                w.sleep(200 * MS + offset(seed)).await;
            }
        }),
        join_commit(&w, cycler, B, awaited),
    );
    m.spawn(commit(w.clone(), y, A, body));
    m.spawn(Box::pin(async move {
        w.until(|st, me| {
            if !st.seen.marks.is_empty() {
                return true;
            }
            st.wake_at(me, st.now + 100 * US);
            false
        })
        .await;
        cycles(&w, cycler, 1, true, (MS, 2 * MS)).await;
    }));
    let end = m.run(20 * SEC, |st| {
        st.seen.cycles.len() == 1 && st.seen.joined.len() == 1
    });
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("the joined commit", &seen.joined, SEC)
    })
}

/// #145: three cycles beside four threads of the cycling handle on one branch and four other
/// handles whose closures last 20 ms.
fn contended_own_branch(design: Design, timing: Timing, seed: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    for writer_at in 0..4 {
        let open = m.open(&[MAIN, OWN + writer_at], false);
        m.spawn(writer(&m.w, open, OWN + writer_at, (20 * MS, 20 * MS), 0));
    }
    let cycler = m.open(&[HOT, MAIN], false);
    for _ in 0..4 {
        m.spawn(writer(&m.w, cycler, HOT, (0, 0), 0));
    }
    let w = m.w.clone();
    m.spawn(then(
        Box::pin(async move { w.sleep(100 * MS).await }),
        cycles(&m.w, cycler, 3, true, (MS, 2 * MS)),
    ));
    let end = m.run(20 * SEC, |st| st.seen.cycles.len() == 3);
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("a cycle", &seen.cycles, SEC)
    })
}

/// #145: a closure on `b` in one handle joins a commit on `a` through another handle while a write
/// on `b` through that handle waits for the closure's byte and that handle cycles.
fn joined_beside_a_write_and_cycle(
    design: Design,
    timing: Timing,
    seed: u64,
    awaited: bool,
) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let cycler = m.open(&[A, B, MAIN], false);
    let other = m.open(&[B, MAIN], false);
    let w = m.w.clone();
    let body = then(
        Box::pin({
            let w = w.clone();
            async move {
                w.sleep(400 * MS).await;
            }
        }),
        join_commit(&w, cycler, A, false),
    );
    m.spawn(then(
        commit(w.clone(), other, B, body),
        Box::pin({
            let w = w.clone();
            async move { w.mark("closure") }
        }),
    ));
    let write = {
        let w = w.clone();
        Box::pin(async move {
            w.sleep(20 * MS).await;
            match awaited {
                true => commit_awaited(w.clone(), cycler, B, nothing()).await,
                false => commit(w.clone(), cycler, B, nothing()).await,
            }
            w.mark("write");
        })
    };
    m.spawn(write);
    m.spawn(Box::pin(async move {
        w.sleep(120 * MS).await;
        cycles(&w, cycler, 1, true, (MS, 2 * MS)).await;
    }));
    let end = m.run(20 * SEC, |st| {
        st.seen.cycles.len() == 1 && st.seen.marks.len() == 2
    });
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |_| None)
}

/// #146: one cycle in a handle while `threads` of its own threads take one lane in turn with
/// closures of `length`.
fn turns(design: Design, timing: Timing, seed: u64, threads: usize, length: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let db = m.open(&[HOT, MAIN], false);
    for _ in 0..threads {
        m.spawn(writer(&m.w, db, HOT, (length, length), 0));
    }
    let w = m.w.clone();
    m.spawn(then(
        Box::pin(async move { w.sleep(50 * MS).await }),
        cycles(&m.w, db, 1, true, (MS, 2 * MS)),
    ));
    let end = m.run(20 * SEC, |st| st.seen.cycles.len() == 1);
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("the cycle", &seen.cycles, SEC)
    })
}

/// #146: closures on `a` that last 20 ms and join a commit on `b`, one after another, beside a
/// handle that cycles as fast as it can; the commits go through `y`, or through the cycling
/// handle if `through` is set. The next closure starts once the last joined commit returned and
/// `gap` has passed.
fn back_to_back(design: Design, timing: Timing, seed: u64, through: bool, gap: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let y = m.open(&[A, B, MAIN], false);
    let cycler = m.open(&[A, B, MAIN], false);
    m.spawn(cycles(&m.w, cycler, usize::MAX, true, (MS, 2 * MS)));
    let target = if through { cycler } else { y };
    let w = m.w.clone();
    m.spawn(Box::pin(async move {
        for _ in 0..40 {
            if gap > 0 {
                w.sleep(gap).await;
            }
            let body = then(
                closure(&w, (20 * MS, 20 * MS)),
                join_commit(&w, target, B, false),
            );
            let thread = w.spawn(commit(w.clone(), y, A, body));
            let count = w.st().seen.joined.len();
            w.until(|st, me| {
                if st.seen.joined.len() > count || st.meta[thread].done {
                    return true;
                }
                st.wake_at(me, st.now + 100 * US);
                false
            })
            .await;
        }
        w.mark("rounds");
    }));
    let end = m.run(40 * SEC, |st| !st.seen.marks.is_empty());
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("a joined commit", &seen.joined, 500 * MS)
    })
}

/// #146: a coalesced handle's cycle that seals an open group on `a`, whose byte the group kept,
/// while a closure on `b` in that handle joins a commit on `a` through another handle.
fn coalesced_group(design: Design, timing: Timing, seed: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let h = m.open(&[A, B, MAIN], true);
    let other = m.open(&[A, MAIN], false);
    {
        let mut st = m.w.st();
        st.opens[h].held.insert(BYTES + A as u64, Some(1));
        st.grant(h, BYTES + A as u64, BYTES + A as u64 + 1);
        let lane = st.lane(h, A);
        lane.group = true;
        lane.crossed = true;
    }
    let w = m.w.clone();
    let body = then(
        closure(&w, (600 * MS, 600 * MS)),
        join_commit(&w, other, A, false),
    );
    m.spawn(commit(w.clone(), h, B, body));
    m.spawn(Box::pin(async move {
        w.sleep(150 * MS).await;
        cycles(&w, h, 1, false, (0, 0)).await;
    }));
    let end = m.run(20 * SEC, |st| {
        st.seen.cycles.len() == 1 && st.seen.joined.len() == 1
    });
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |_| None)
}

/// #146: eight cycles in one handle and eight pulls landing in another, each wanting every
/// branch, beside four handles whose closures last 20 ms.
fn two_takers(design: Design, timing: Timing, seed: u64, one_handle: bool) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    for writer_at in 0..4 {
        let open = m.open(&[MAIN, OWN + writer_at], false);
        m.spawn(writer(&m.w, open, OWN + writer_at, (20 * MS, 20 * MS), 0));
    }
    let cycler = m.open(&[MAIN], false);
    let puller = match one_handle {
        true => cycler,
        false => m.open(&[MAIN, C], false),
    };
    let w = m.w.clone();
    m.spawn(then(
        Box::pin({
            let w = w.clone();
            async move { w.sleep(100 * MS).await }
        }),
        cycles(&m.w, cycler, 8, true, (MS, 2 * MS)),
    ));
    m.spawn(Box::pin(async move {
        w.sleep(100 * MS).await;
        for _ in 0..8 {
            let started = w.now();
            w.sleep(2 * MS).await;
            exclusively(w.clone(), puller).await;
            let mut st = w.st();
            let took = st.now - started;
            st.seen.cycles.push(took);
        }
    }));
    let end = m.run(60 * SEC, |st| st.seen.cycles.len() == 16);
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("a cycle or pull", &seen.cycles, 2 * SEC)
    })
}

/// #145: a closure on `c` joins a commit on `b` through its own handle while that handle cycles.
fn joined_earlier_own_handle(design: Design, timing: Timing, seed: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let db = m.open(&[B, C, MAIN], false);
    let w = m.w.clone();
    let body = then(
        Box::pin({
            let w = w.clone();
            async move {
                w.mark("in c");
                w.sleep(100 * MS).await;
            }
        }),
        join_commit(&w, db, B, false),
    );
    m.spawn(commit(w.clone(), db, C, body));
    m.spawn(Box::pin(async move {
        w.until(|st, me| {
            if !st.seen.marks.is_empty() {
                return true;
            }
            st.wake_at(me, st.now + 100 * US);
            false
        })
        .await;
        cycles(&w, db, 1, false, (MS, 2 * MS)).await;
    }));
    let end = m.run(20 * SEC, |st| {
        st.seen.cycles.len() == 1 && st.seen.joined.len() == 1
    });
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |_| None)
}

/// Adversarial: two threads take turns at closures on `a` that last 20 ms and join a commit on
/// `b`, each queued for the lane before the last one ends, beside a handle that cycles. The
/// chain can outlast a few rounds before the rule for a busy branch breaks it, so a cycle may
/// take a little over 2 s, about once in 5,000 runs.
fn pipelined(design: Design, timing: Timing, seed: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let y = m.open(&[A, B, MAIN], false);
    let cycler = m.open(&[MAIN], false);
    for thread_at in 0..2 {
        let w = m.w.clone();
        m.spawn(Box::pin(async move {
            w.sleep(thread_at * (5 * MS + offset(seed))).await;
            while !w.st().stop {
                let body = then(
                    closure(&w, (20 * MS, 20 * MS)),
                    join_commit(&w, y, B, false),
                );
                commit(w.clone(), y, A, body).await;
            }
        }));
    }
    let w = m.w.clone();
    m.spawn(then(
        Box::pin(async move { w.sleep(100 * MS).await }),
        cycles(&m.w, cycler, 5, true, (MS, 2 * MS)),
    ));
    let end = m.run(30 * SEC, |st| st.seen.cycles.len() == 5);
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("a cycle", &seen.cycles, 2500 * MS)
            .or_else(|| under("a joined commit", &seen.joined, 1500 * MS))
    })
}

/// #128 and #136: twenty cycles beside a closure on `a` that writes to `b` from inside, one that
/// awaits such a write, writers on `b` in the same handle and two other handles on branches of
/// their own.
fn nested_and_awaited(design: Design, timing: Timing, seed: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    for writer_at in 0..2 {
        let open = m.open(&[MAIN, OWN + writer_at], false);
        m.spawn(writer(&m.w, open, OWN + writer_at, (0, 0), 0));
    }
    let y = m.open(&[A, B, MAIN], false);
    for kind in 0..3 {
        let w = m.w.clone();
        m.spawn(Box::pin(async move {
            while !w.st().stop {
                match kind {
                    0 => {
                        let body = commit(w.clone(), y, B, nothing());
                        commit(w.clone(), y, A, body).await;
                    }
                    1 => {
                        let body = commit_awaited(w.clone(), y, B, nothing());
                        commit(w.clone(), y, A, body).await;
                    }
                    _ => commit(w.clone(), y, B, nothing()).await,
                }
            }
        }));
    }
    let cycler = m.open(&[MAIN], false);
    m.spawn(cycles(&m.w, cycler, 20, true, (MS, 2 * MS)));
    let end = m.run(60 * SEC, |st| st.seen.cycles.len() == 20);
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("a cycle", &seen.cycles, SEC)
    })
}

/// Adversarial, and beyond what the gate promises a taker: four threads each run closures on a
/// branch of their own that last 20 ms and join a commit on another branch of their own, beside
/// five cycles. Each closure that a phase's end lets begin holds its byte until the commit it
/// joins goes on at the next end, so once the threads begin at different ends the range need never
/// come free. Only the joined commits' waits are checked.
fn joins_of_their_own(design: Design, timing: Timing, seed: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let branches: Vec<usize> = (0..8).map(|at| OWN + at).collect();
    let y = m.open(&branches, false);
    for thread_at in 0..4 {
        let w = m.w.clone();
        m.spawn(Box::pin(async move {
            w.sleep(thread_at as u64 * 3 * MS + offset(seed)).await;
            while !w.st().stop {
                let body = then(
                    closure(&w, (20 * MS, 20 * MS)),
                    join_commit(&w, y, OWN + 4 + thread_at, false),
                );
                commit(w.clone(), y, OWN + thread_at, body).await;
            }
        }));
    }
    let cycler = m.open(&[MAIN], false);
    let w = m.w.clone();
    m.spawn(then(
        Box::pin(async move { w.sleep(100 * MS).await }),
        cycles(&m.w, cycler, 5, true, (MS, 2 * MS)),
    ));
    let end = m.run(10 * SEC, |st| st.seen.cycles.len() == 5);
    Verdict::check_unfinished(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("a joined commit", &seen.joined, 1200 * MS)
    })
}

/// Adversarial: closures on `a` that last 20 ms join a commit on `b`, twenty times, while three
/// other handles commit on `b` with 10 ms closures and another handle cycles.
fn joined_on_a_hot_branch(design: Design, timing: Timing, seed: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    for _ in 0..3 {
        let open = m.open(&[B, MAIN], false);
        m.spawn(writer(&m.w, open, B, (10 * MS, 10 * MS), 0));
    }
    let y = m.open(&[A, B, MAIN], false);
    let cycler = m.open(&[MAIN], false);
    m.spawn(cycles(&m.w, cycler, usize::MAX, true, (MS, 2 * MS)));
    let w = m.w.clone();
    m.spawn(Box::pin(async move {
        w.sleep(offset(seed)).await;
        for _ in 0..20 {
            let body = then(
                closure(&w, (20 * MS, 20 * MS)),
                join_commit(&w, y, B, false),
            );
            commit(w.clone(), y, A, body).await;
        }
        w.mark("joins");
    }));
    let end = m.run(60 * SEC, |st| !st.seen.marks.is_empty());
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("a joined commit", &seen.joined, 1500 * MS)
    })
}

/// #147: `takers` opens each take every branch over and over, `gap` apart, beside four writers
/// with closures of up to 100 µs, in four other opens or, if `local` is set, in the first taker's
/// open. Each hold ends within its first round, since the writers are held up, so the gate stays
/// held but for the gaps between holds, and a writer must still go on at the end of the hold it
/// found.
fn holds_back_to_back(
    design: Design,
    timing: Timing,
    seed: u64,
    takers: usize,
    gap: u64,
    local: bool,
) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let opens: Vec<usize> = (0..takers).map(|_| m.open(&[MAIN], false)).collect();
    for writer_at in 0..4 {
        let open = match local {
            true => {
                m.w.st().opens[opens[0]].attached.push(OWN + writer_at);
                opens[0]
            }
            false => m.open(&[MAIN, OWN + writer_at], false),
        };
        m.spawn(writer(&m.w, open, OWN + writer_at, (0, 100 * US), 0));
    }
    for (at, open) in opens.iter().copied().enumerate() {
        let w = m.w.clone();
        m.spawn(Box::pin(async move {
            w.sleep(20 * MS + at as u64 * 300 * US + offset(seed)).await;
            while !w.st().stop {
                exclusively(w.clone(), open).await;
                if gap > 0 {
                    w.sleep(gap).await;
                }
            }
        }));
    }
    stop_after(&m.w, 1500 * MS);
    let end = m.run(5 * SEC, |st| !st.seen.marks.is_empty());
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        held_up_briefly(seen)
    })
}

/// #147: `count` opens pull over and over, each pull a landing and an integration, each taking
/// every branch, and a merge committed through the pulling open, beside a writer in another open
/// whose closures last up to 100 µs.
fn pullers(design: Design, timing: Timing, seed: u64, count: usize) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    m.w.st().seen.pulls = vec![0; count];
    let open = m.open(&[MAIN, OWN], false);
    m.spawn(writer(&m.w, open, OWN, (0, 100 * US), 0));
    for at in 0..count {
        let branch = OWN + 1 + at;
        let puller = m.open(&[MAIN, branch], false);
        let w = m.w.clone();
        m.spawn(Box::pin(async move {
            w.sleep(at as u64 * 200 * US + offset(seed)).await;
            while !w.st().stop {
                let fetch = w.st().rng.within((100 * US, 300 * US));
                w.cpu(fetch).await;
                let started = w.now();
                let decide = w.st().rng.within((50 * US, 150 * US));
                land_and_integrate(&w, puller, decide).await;
                commit(w.clone(), puller, branch, nothing()).await;
                let mut st = w.st();
                let took = st.now - started;
                st.seen.cycles.push(took);
                if !st.stop {
                    st.seen.pulls[at] += 1;
                }
            }
        }));
    }
    stop_after(&m.w, 1500 * MS);
    let end = m.run(5 * SEC, |st| !st.seen.marks.is_empty());
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        held_up_briefly(seen).or_else(|| under("a pull", &seen.cycles, SEC))
    })
}

/// The spread of what each of several takers did in a run: how many pulls the one that made
/// fewest made, and the one that made most.
fn fewest_and_most(seen: &Seen) -> (u64, u64) {
    let fewest = seen.pulls.iter().copied().min().unwrap_or(0);
    let most = seen.pulls.iter().copied().max().unwrap_or(0);
    (fewest, most)
}

/// #149: `takers` pull over and over, each in an open of its own or, if `one_open` is set, in
/// threads of one open, each pull a landing and an integration, each taking every branch, and a
/// merge committed through the puller, beside `writers` other opens committing on branches of
/// their own in 5 ms closures, for 3 s. Taken in turn, each taker pulls once in about the time
/// all of them take to pull once each, 3 s times the takers over the pulls they made in all, so
/// each pull, and each wait for one that had not ended when the run stopped, must take at most
/// twice that and 100 ms, and the taker that pulled fewest times, with one more, at least half as
/// often as the one that pulled most.
fn fair_pulls(
    design: Design,
    timing: Timing,
    seed: u64,
    (takers, writers, one_open): (usize, usize, bool),
) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    for writer_at in 0..writers {
        let open = m.open(&[MAIN, OWN + writer_at], false);
        m.spawn(writer(&m.w, open, OWN + writer_at, (5 * MS, 5 * MS), 0));
    }
    let shared = one_open.then(|| m.open(&[MAIN], false));
    m.w.st().seen.pulls = vec![0; takers];
    let pulling = Rc::new(RefCell::new(vec![None; takers]));
    for at in 0..takers {
        let branch = OWN + writers + at;
        let puller = match shared {
            Some(open) => {
                m.w.st().opens[open].attached.push(branch);
                open
            }
            None => m.open(&[MAIN, branch], false),
        };
        let w = m.w.clone();
        let pulling = pulling.clone();
        m.spawn(Box::pin(async move {
            w.sleep(at as u64 * 200 * US + offset(seed)).await;
            while !w.st().stop {
                let fetch = w.st().rng.within((100 * US, 300 * US));
                w.cpu(fetch).await;
                let started = w.now();
                pulling.borrow_mut()[at] = Some(started);
                let decide = w.st().rng.within((50 * US, 150 * US));
                land_and_integrate(&w, puller, decide).await;
                commit(w.clone(), puller, branch, nothing()).await;
                pulling.borrow_mut()[at] = None;
                let mut st = w.st();
                if st.stop {
                    return;
                }
                let took = st.now - started;
                st.seen.cycles.push(took);
                st.seen.pulls[at] += 1;
            }
        }));
    }
    let window = 3 * SEC;
    stop_after(&m.w, window);
    let end = m.run(10 * SEC, |st| !st.seen.marks.is_empty());
    let now = m.w.now();
    let mut seen = std::mem::take(&mut m.w.st().seen);
    seen.cycles.extend(
        pulling
            .borrow()
            .iter()
            .flatten()
            .map(|started| now - started),
    );
    Verdict::check(end, seen, |seen| {
        let (fewest, most) = fewest_and_most(seen);
        let round = window * takers as u64 / seen.pulls.iter().sum::<u64>().max(1);
        under("a pull", &seen.cycles, 2 * round + 100 * MS).or_else(|| {
            (2 * (fewest + 1) < most)
                .then(|| format!("takers pulled from {fewest} to {most} times each"))
        })
    })
}

fn stop_after(w: &W, length: u64) {
    let w2 = w.clone();
    w.spawn(Box::pin(async move {
        w2.sleep(length).await;
        w2.st().stop = true;
        w2.mark("stopped");
    }));
}

/// No writer waited at the gate past the round of the hold it found and the next, and no commit
/// was held up for long.
fn held_up_briefly(seen: &Seen) -> Option<String> {
    (seen.late_waits > 0)
        .then(|| {
            format!(
                "{} writers waited at the gate past the rounds they found, one {} ms past",
                seen.late_waits,
                seen.most_late / MS
            )
        })
        .or_else(|| {
            (seen.longest_commit >= SEC)
                .then(|| format!("a commit took {} ms", seen.longest_commit / MS))
        })
}

/// #147: four threads of the cycling open each run 20 ms closures on a branch of their own that
/// join a commit on another branch of their own through the cycling open, beside its cycles, as
/// [`joins_of_their_own`] does through another open. Beyond what the gate promises a taker, so
/// only the joined commits' waits are checked.
fn joins_through_the_cycler(design: Design, timing: Timing, seed: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let mut branches: Vec<usize> = (0..8).map(|at| OWN + at).collect();
    branches.push(MAIN);
    let y = m.open(&branches, false);
    for thread_at in 0..4 {
        let w = m.w.clone();
        m.spawn(Box::pin(async move {
            w.sleep(thread_at as u64 * 3 * MS + offset(seed)).await;
            while !w.st().stop {
                let body = then(
                    closure(&w, (20 * MS, 20 * MS)),
                    join_commit(&w, y, OWN + 4 + thread_at, false),
                );
                commit(w.clone(), y, OWN + thread_at, body).await;
            }
        }));
    }
    let w = m.w.clone();
    m.spawn(then(
        Box::pin(async move { w.sleep(100 * MS).await }),
        cycles(&m.w, y, 5, true, (MS, 2 * MS)),
    ));
    let end = m.run(10 * SEC, |st| st.seen.cycles.len() == 5);
    Verdict::check_unfinished(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("a joined commit", &seen.joined, 2100 * MS)
    })
}

/// #147: one cycle beside two threads in each of three opens on branches of their own, whose
/// closures last `length`, just under or just over a round, begun `length / 6` apart. The gate
/// promises the cycle within about three times the closures and 4 ms; the bound allows 250 ms
/// more for threads preempted for up to 50 ms at a time.
fn round_edge(design: Design, timing: Timing, seed: u64, length: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    for open_at in 0..3 {
        let branches = [OWN + 2 * open_at, OWN + 2 * open_at + 1];
        let open = m.open(&[MAIN, branches[0], branches[1]], false);
        for (t, branch) in branches.iter().enumerate() {
            let index = (2 * open_at + t) as u64;
            m.spawn(writer(
                &m.w,
                open,
                *branch,
                (length, length),
                length * index / 6,
            ));
        }
    }
    let cycler = m.open(&[MAIN], false);
    let w = m.w.clone();
    m.spawn(then(
        Box::pin(async move { w.sleep(2 * length + offset(seed)).await }),
        cycles(&m.w, cycler, 1, false, (0, 0)),
    ));
    let end = m.run(30 * SEC, |st| st.seen.cycles.len() == 1);
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        under("the cycle", &seen.cycles, 3 * length + 4 * MS + 250 * MS)
    })
}

/// #147: a cycle whose range a 5 s closure in another open keeps busy, so that its phases end
/// over and over, beside writers in four other opens whose closures last up to 30 ms. No writer
/// may test the phase it found after it ended and find it still held.
fn swap_race(design: Design, timing: Timing, seed: u64) -> Verdict {
    let mut m = Model::new(design, timing, seed);
    let long = m.open(&[MAIN, HOT], false);
    let w = m.w.clone();
    m.spawn(commit(
        w.clone(),
        long,
        HOT,
        closure(&w, (5 * SEC, 5 * SEC)),
    ));
    for writer_at in 0..4 {
        let open = m.open(&[MAIN, OWN + writer_at], false);
        m.spawn(writer(
            &m.w,
            open,
            OWN + writer_at,
            (0, 30 * MS),
            offset(seed),
        ));
    }
    let cycler = m.open(&[MAIN], false);
    m.spawn(then(
        Box::pin(async move { w.sleep(20 * MS).await }),
        cycles(&m.w, cycler, 1, false, (0, 0)),
    ));
    let end = m.run(10 * SEC, |st| st.seen.cycles.len() == 1);
    Verdict::check(end, std::mem::take(&mut m.w.st().seen), |seen| {
        (seen.overwaits > 0).then(|| {
            format!(
                "{} writers found the phase they waited for held after it ended",
                seen.overwaits
            )
        })
    })
}

type Scenario = fn(Design, Timing, u64) -> Verdict;

fn scenarios() -> Vec<(&'static str, Scenario)> {
    vec![
        ("#128 4 opens, 0 ms", |d, t, s| own_branches(d, t, s, 4, 0)),
        ("#128 4 opens, 2 ms", |d, t, s| {
            own_branches(d, t, s, 4, 2 * MS)
        }),
        ("#128 16 opens, 5 ms", |d, t, s| {
            own_branches(d, t, s, 16, 5 * MS)
        }),
        ("#128 4 opens, 20 ms", |d, t, s| {
            own_branches(d, t, s, 4, 20 * MS)
        }),
        ("#128 own commits", own_commits_beside_cycles),
        ("#136 joined", |d, t, s| joined_beside_cycle(d, t, s, false)),
        ("#136 joined awaited", |d, t, s| {
            joined_beside_cycle(d, t, s, true)
        }),
        ("#142 4 x 600 ms", |d, t, s| {
            staggered(d, t, s, 1, 4, 600 * MS)
        }),
        ("#142 8 x 300 ms", |d, t, s| {
            staggered(d, t, s, 1, 8, 300 * MS)
        }),
        ("#142 2 x 4 x 600 ms", |d, t, s| {
            staggered(d, t, s, 2, 4, 600 * MS)
        }),
        ("#142 through cycler", |d, t, s| {
            joined_through_cycler(d, t, s, false)
        }),
        ("#142 through cycler awaited", |d, t, s| {
            joined_through_cycler(d, t, s, true)
        }),
        ("#145 contended own branch", contended_own_branch),
        ("#145 blocked write", |d, t, s| {
            joined_beside_a_write_and_cycle(d, t, s, false)
        }),
        ("#145 awaited write", |d, t, s| {
            joined_beside_a_write_and_cycle(d, t, s, true)
        }),
        ("#145 earlier own branch", joined_earlier_own_handle),
        ("#146 3 turns x 5 ms", |d, t, s| turns(d, t, s, 3, 5 * MS)),
        ("#146 2 turns x 2 ms", |d, t, s| turns(d, t, s, 2, 2 * MS)),
        ("#146 back to back", |d, t, s| {
            back_to_back(d, t, s, false, 0)
        }),
        ("#146 back to back, 5 ms gap", |d, t, s| {
            back_to_back(d, t, s, false, 5 * MS)
        }),
        ("#146 through cycler", |d, t, s| {
            back_to_back(d, t, s, true, 0)
        }),
        ("#146 coalesced group", coalesced_group),
        ("#146 two takers", |d, t, s| two_takers(d, t, s, false)),
        ("#145 two takers, one handle", |d, t, s| {
            two_takers(d, t, s, true)
        }),
        ("#142 4 x 900 ms", |d, t, s| {
            staggered(d, t, s, 1, 4, 900 * MS)
        }),
        ("nested and awaited", nested_and_awaited),
        ("pipelined joins", pipelined),
        ("joins of their own", joins_of_their_own),
        ("joined on a hot branch", joined_on_a_hot_branch),
        ("#147 back to back, other opens", |d, t, s| {
            holds_back_to_back(d, t, s, 1, 0, false)
        }),
        ("#147 back to back, the taker's open", |d, t, s| {
            holds_back_to_back(d, t, s, 1, 0, true)
        }),
        ("#147 back to back, two takers", |d, t, s| {
            holds_back_to_back(d, t, s, 2, 0, false)
        }),
        ("#147 back to back, 100 us apart", |d, t, s| {
            holds_back_to_back(d, t, s, 1, 100 * US, false)
        }),
        ("#147 two pullers", |d, t, s| pullers(d, t, s, 2)),
        ("#147 four pullers", |d, t, s| pullers(d, t, s, 4)),
        ("#147 joins through the cycler", joins_through_the_cycler),
        ("#147 3.9 ms closures", |d, t, s| {
            round_edge(d, t, s, 3_900 * US)
        }),
        ("#147 4.1 ms closures", |d, t, s| {
            round_edge(d, t, s, 4_100 * US)
        }),
        ("#147 16.1 ms closures", |d, t, s| {
            round_edge(d, t, s, 16_100 * US)
        }),
        ("#147 128.1 ms closures", |d, t, s| {
            round_edge(d, t, s, 128_100 * US)
        }),
        ("#147 511.9 ms closures", |d, t, s| {
            round_edge(d, t, s, 511_900 * US)
        }),
        ("#147 512.1 ms closures", |d, t, s| {
            round_edge(d, t, s, 512_100 * US)
        }),
        ("#147 swap race", swap_race),
        ("#149 16 pullers beside 4 writers", |d, t, s| {
            fair_pulls(d, t, s, (16, 4, false))
        }),
        ("#149 16 pullers beside 16 writers", |d, t, s| {
            fair_pulls(d, t, s, (16, 16, false))
        }),
        ("#149 16 threads of one open pulling", |d, t, s| {
            fair_pulls(d, t, s, (16, 4, true))
        }),
    ]
}

fn seeds(default: u64) -> u64 {
    std::env::var("FERRO_GATE_SEEDS")
        .ok()
        .and_then(|seeds| seeds.parse().ok())
        .unwrap_or(default)
}

/// What one seed of a sweep saw at worst.
struct Worst {
    taker: u64,
    joined: u64,
    gate_wait: u64,
    commits: u64,
    fewest: u64,
    most: u64,
}

/// Runs `scenario` under `design` for `count` seeds, calm and busy, and returns how many failed
/// and the first failure.
fn sweep(design: Design, scenario: Scenario, count: u64) -> (u64, Option<String>, Vec<Worst>) {
    let mut failed = 0;
    let mut first = None;
    let mut worst = Vec::new();
    for seed in 0..count {
        let timing = match seed % 3 {
            0 => Timing::calm(),
            1 => Timing::busy(),
            _ => Timing::hostile(),
        };
        let verdict = scenario(design, timing, seed);
        let (fewest, most) = fewest_and_most(&verdict.seen);
        worst.push(Worst {
            taker: longest(&verdict.seen.cycles),
            joined: longest(&verdict.seen.joined),
            gate_wait: verdict.seen.gate_wait,
            commits: verdict.seen.commits,
            fewest,
            most,
        });
        if let Some(why) = verdict.failure {
            failed += 1;
            first.get_or_insert_with(|| format!("seed {seed}: {why} ({:?})", verdict.end));
        }
    }
    (failed, first, worst)
}

fn spread(mut values: Vec<u64>) -> String {
    values.sort_unstable();
    let at = |share: usize| {
        values
            .get((values.len() - 1) * share / 100)
            .copied()
            .unwrap_or(0)
    };
    format!(
        "{:.1}/{:.1}/{:.1}",
        at(50) as f64 / MS as f64,
        at(99) as f64 / MS as f64,
        at(100) as f64 / MS as f64
    )
}

fn report(design: Design, count: u64) -> Vec<String> {
    let only = std::env::var("FERRO_GATE_ONLY").ok();
    let mut failures = Vec::new();
    for (name, scenario) in scenarios() {
        if only.as_deref().is_some_and(|only| !name.contains(only)) {
            continue;
        }
        let (failed, first, worst) = sweep(design, scenario, count);
        let taken = spread(worst.iter().map(|worst| worst.taker).collect());
        let joined = spread(worst.iter().map(|worst| worst.joined).collect());
        let waited = spread(worst.iter().map(|worst| worst.gate_wait).collect());
        let median = |mut values: Vec<u64>| {
            values.sort_unstable();
            values.get(values.len() / 2).copied().unwrap_or(0)
        };
        let commits = median(worst.iter().map(|worst| worst.commits).collect());
        let fewest = median(worst.iter().map(|worst| worst.fewest).collect());
        let most = median(worst.iter().map(|worst| worst.most).collect());
        let least = worst.iter().map(|worst| worst.fewest).min().unwrap_or(0);
        println!(
            "{design:?} | {name} | failed {failed}/{count} | taker ms p50/p99/max {taken} | joined {joined} | gate wait {waited} | commits p50 {commits} | pulls each p50 {fewest}-{most}, fewest {least}"
        );
        if let Some(first) = first {
            failures.push(format!("{name}: {failed}/{count}, {first}"));
        }
    }
    failures
}

#[test]
fn the_phase_gate_passes_every_scenario() {
    let failures = report(Design::Phases(CHOSEN), seeds(1200));
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn the_model_finds_what_the_reviews_of_earlier_designs_found() {
    let found = [
        (Design::Epochs, "#145 contended own branch", 120),
        (Design::Epochs, "#145 blocked write", 120),
        (Design::Epochs, "#145 awaited write", 120),
        (Design::Epochs, "#145 earlier own branch", 120),
        (Design::Epochs, "#146 through cycler", 120),
        (Design::Epochs, "#146 coalesced group", 120),
        (Design::Rounds, "#146 3 turns x 5 ms", 120),
        (Design::Rounds, "#146 2 turns x 2 ms", 120),
        (Design::Rounds, "#146 back to back", 120),
        (Design::Rounds, "#146 through cycler", 120),
        (Design::Rounds, "#146 coalesced group", 120),
        (Design::Phases(BLIND), "pipelined joins", 120),
        (Design::Phases(ALWAYS), "joined on a hot branch", 120),
        (Design::Phases(PAIR), "#147 back to back, other opens", 120),
        (Design::Phases(PAIR), "#147 back to back, two takers", 120),
        (Design::Phases(PAIR), "#147 four pullers", 600),
        (Design::Phases(PAIR), "#147 swap race", 120),
        (
            Design::Phases(KERNEL),
            "#149 16 pullers beside 4 writers",
            120,
        ),
        (
            Design::Phases(KERNEL),
            "#149 16 pullers beside 16 writers",
            120,
        ),
        (
            Design::Phases(KERNEL),
            "#149 16 threads of one open pulling",
            120,
        ),
        (
            Design::Phases(LOCAL),
            "#149 16 pullers beside 4 writers",
            120,
        ),
    ];
    let all = scenarios();
    let mut missed = Vec::new();
    for (design, name, count) in found {
        let (_, scenario) = all
            .iter()
            .find(|(named, _)| *named == name)
            .expect("a scenario of that name");
        let (failed, _, _) = sweep(design, *scenario, count);
        if failed == 0 {
            missed.push(format!("{design:?}: {name}"));
        }
    }
    assert!(missed.is_empty(), "the model never failed {missed:#?}");
}

#[test]
#[ignore = "one design's sweep, printed: FERRO_GATE_DESIGN names it and FERRO_GATE_ONLY the scenarios"]
fn sweep_one_design() {
    let design = match std::env::var("FERRO_GATE_DESIGN").as_deref() {
        Ok("epochs") => Design::Epochs,
        Ok("rounds") => Design::Rounds,
        Ok("pair") => Design::Phases(PAIR),
        Ok("spaced") => Design::Phases(SPACED),
        Ok("counter") => Design::Phases(COUNTER),
        Ok("pooled") => Design::Phases(POOLED),
        Ok("blind") => Design::Phases(BLIND),
        Ok("always") => Design::Phases(ALWAYS),
        Ok("kernel") => Design::Phases(KERNEL),
        Ok("blocking") => Design::Phases(BLOCKING),
        Ok("local") => Design::Phases(LOCAL),
        Ok("turns") => Design::Phases(TURNED),
        _ => Design::Phases(CHOSEN),
    };
    let failures = report(design, seeds(300));
    println!("{design:?}: {failures:#?}");
}

#[test]
#[ignore = "a comparison of every design, printed; set FERRO_GATE_SEEDS for more seeds"]
fn compare_designs() {
    for design in [
        Design::Epochs,
        Design::Rounds,
        Design::Phases(BLIND),
        Design::Phases(ALWAYS),
        Design::Phases(PAIR),
        Design::Phases(SPACED),
        Design::Phases(COUNTER),
        Design::Phases(POOLED),
        Design::Phases(KERNEL),
        Design::Phases(BLOCKING),
        Design::Phases(LOCAL),
        Design::Phases(TURNED),
        Design::Phases(CHOSEN),
    ] {
        let failures = report(design, seeds(300));
        println!("{design:?}: {failures:#?}");
    }
}
