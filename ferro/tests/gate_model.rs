//! A model of the gate behind [`Options::shared`]'s whole-range takers, on a simulated clock.
//!
//! A cycle's prune and pause, a pull's or receive's landing and a `Keep::Last` prune lock every
//! branch byte of the file at once, beside writers in other processes that each lock one byte per
//! closure. The model runs those actors as tasks of a single-threaded executor whose clock moves
//! only when every task waits, so a run is fixed by its seed: which task runs next after each wake,
//! how long a wake, a sleep or a commit takes, when a thread is preempted, which of the threads
//! woken by a kernel unlock wins the byte, and when an in-process lane hands itself over fairly.
//!
//! The pieces it models are the ones the protocols differ in:
//!
//! - open file description locks, exclusive, per open, granted to the first waiter that runs after
//!   an unlock, and tested by other opens with `F_OFD_GETLK`;
//! - each open's lanes, which behave as `parking_lot` mutexes do: a free lane goes to whoever asks
//!   first, an unlock wakes the longest parked thread, which parks again at the back if another
//!   took the lane meanwhile, and about once a millisecond an unlock hands the lane over instead;
//! - each open's branch bytes shared between its lanes, its `whole` mark, its table of held gate
//!   bytes, and the queue of awaited writes at each lane;
//! - closures that sleep, write from inside themselves, or join another thread's write through any
//!   open, the one that cycles included, blocking or awaited;
//! - coalesced groups that keep their branch's byte between closures.
//!
//! [`Design`] lists the protocols: the shared epochs of 6fb68bc, the growing rounds of 51dbfc6,
//! and the phase gate that replaced both, with and without its rule for writers whose branch is
//! busy when their phase ends. Each scenario comes from an issue that a review of one of them
//! found, #128, #136, #142, #145 and #146, or is an adversarial one added here, and each checks the
//! properties the gate promises: no task waits for ever (P1), a write a closure joins ends within a
//! bound (P2), and a whole-range taker finishes within a bound beside closures of a given length
//! (P3). The tests check that the phase gate passes every scenario under thousands of seeds, calm
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

/// The old gate byte, which the epochs and rounds hold, and which the phase gate's holder holds
/// for as long as it wants every branch byte, so that one process at a time does.
const GATE: u64 = 5;
/// The two phase bytes the phase gate's holder holds in turn.
const PHASES: u64 = 6;
/// The first branch byte; branch `b` locks `BYTES + b`.
const BYTES: u64 = 1_000;
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
}

const CHOSEN: Knobs = Knobs {
    busy_waits: 1,
    first: 4 * MS,
    longest: SEC,
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
    freed: Vec<usize>,
    phase: Option<u64>,
    phased: Vec<usize>,
    coalesce: bool,
}

struct Lock {
    owner: usize,
    start: u64,
    end: u64,
}

/// What a run saw.
#[derive(Default, Debug)]
struct Seen {
    cycles: Vec<u64>,
    joined: Vec<u64>,
    commits: u64,
    longest_commit: u64,
    marks: Vec<(&'static str, u64)>,
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
    blocked: Vec<usize>,
    began: Option<u64>,
    stop: bool,
    seen: Seen,
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

    fn try_lock(&mut self, owner: usize, start: u64, end: u64) -> bool {
        if self.conflict(owner, start, end).is_some() {
            return false;
        }
        let same = |lock: &Lock| lock.owner == owner && lock.start == start && lock.end == end;
        if !self.locks.iter().any(same) {
            self.locks.push(Lock { owner, start, end });
        }
        true
    }

    fn unlock(&mut self, owner: usize, start: u64, end: u64) {
        self.locks
            .retain(|lock| !(lock.owner == owner && lock.start == start && lock.end == end));
        let blocked = std::mem::take(&mut self.blocked);
        self.wake_all(blocked);
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

    async fn lock(&self, owner: usize, start: u64, end: u64) {
        self.syscall().await;
        self.until(|st, me| {
            if st.try_lock(owner, start, end) {
                return true;
            }
            st.blocked.push(me);
            false
        })
        .await;
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

    async fn hold(&self, open: usize, at: u64, wait: bool) -> bool {
        let mut free = false;
        self.until(|st, me| {
            let shared = &mut st.opens[open];
            if !shared.taken.contains(&at) {
                shared.taken.push(at);
                free = true;
                return true;
            }
            if !wait {
                return true;
            }
            shared.freed.push(me);
            false
        })
        .await;
        if !free {
            return false;
        }
        let got = match wait {
            true => {
                self.lock(open, at, at + 1).await;
                true
            }
            false => self.try_lock(open, at, at + 1).await,
        };
        if !got {
            self.release(open, at, false).await;
        }
        got
    }

    async fn release(&self, open: usize, at: u64, locked: bool) {
        if locked {
            self.unlock(open, at, at + 1).await;
        }
        let mut st = self.st();
        st.opens[open].taken.retain(|held| *held != at);
        let freed = std::mem::take(&mut st.opens[open].freed);
        st.wake_all(freed);
    }

    async fn seal(&self, open: usize, branch: usize) {
        if self.st().lane(open, branch).group {
            let length = {
                let mut st = self.st();
                {
                    let range = st.timing.commit;
                    st.draw(range)
                }
            };
            self.cpu(length).await;
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
    loop {
        let local = w.st().opens[open].phase;
        if let Some(phase) = local {
            w.until(|st, me| {
                if st.opens[open].phase != Some(phase) {
                    return true;
                }
                st.opens[open].phased.push(me);
                false
            })
            .await;
        } else {
            let Some(held) = w.conflict(open, PHASES, PHASES + 2).await else {
                return;
            };
            let mut pause = PHASE_POLL.0;
            loop {
                w.sleep(pause).await;
                pause = (pause * 2).min(PHASE_POLL.1);
                if w.conflict(open, held, held + 1).await.is_none() {
                    break;
                }
                if w.now() - started >= PHASE_WAIT {
                    return;
                }
            }
        }
        if deferred >= knobs.busy_waits {
            return;
        }
        w.syscall().await;
        if !w.st().branch_busy(open, branch) {
            return;
        }
        deferred += 1;
    }
}

/// Whether a write that does not wait finds the gate held, testing it once.
async fn gate_held(w: &W, open: usize) -> bool {
    match w.design() {
        Design::Epochs | Design::Rounds => w.conflict(open, GATE, GATE + 1).await.is_some(),
        Design::Phases(_) => {
            w.st().opens[open].phase.is_some()
                || w.conflict(open, PHASES, PHASES + 2).await.is_some()
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
        let length = {
            let mut st = w.st();
            {
                let range = st.timing.commit;
                st.draw(range)
            }
        };
        w.cpu(length).await;
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

/// An awaited write polled on one thread: the lane through the queue of awaited writes, a test of
/// the gate and the byte that does not wait, and a waiting thread when either is not free.
fn commit_awaited(w: W, open: usize, branch: usize, closure: Task) -> Task {
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
            let length = {
                let mut st = w.st();
                {
                    let range = st.timing.commit;
                    st.draw(range)
                }
            };
            w.cpu(length).await;
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
            Design::Phases(knobs) => exclusively_in_phases(&w, open, knobs).await,
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
    w.pause().await;
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

async fn exclusively_in_phases(w: &W, open: usize, knobs: Knobs) {
    w.hold(open, GATE, true).await;
    let mut phase = 0u64;
    let held = w.try_lock(open, PHASES, PHASES + 1).await;
    assert!(held, "the phase gate's holder found a phase byte held");
    {
        let mut st = w.st();
        st.opens[open].phase = Some(phase);
    }
    let mut round = knobs.first;
    let mut ends = w.now() + round;
    let mut pause = PHASE_RANGE_POLL.0;
    let lanes = loop {
        if let Some(lanes) = try_everything(w, open).await {
            break lanes;
        }
        let now = w.now();
        if now >= ends {
            let (old, new) = (PHASES + phase % 2, PHASES + (phase + 1) % 2);
            let held = w.try_lock(open, new, new + 1).await;
            assert!(
                held,
                "the phase gate's holder found the next phase byte held"
            );
            w.unlock(open, old, old + 1).await;
            phase += 1;
            {
                let mut st = w.st();
                st.opens[open].phase = Some(phase);
                let phased = std::mem::take(&mut st.opens[open].phased);
                st.wake_all(phased);
            }
            round = (round * 2).min(knobs.longest);
            ends = w.now() + round;
            pause = PHASE_RANGE_POLL.0;
            continue;
        }
        w.sleep(pause.min(ends - now)).await;
        pause = (pause * 2).min(PHASE_RANGE_POLL.1).min(round / 8);
    };
    finish(w, open, &lanes).await;
    let at = PHASES + phase % 2;
    w.unlock(open, at, at + 1).await;
    {
        let mut st = w.st();
        st.opens[open].phase = None;
        let phased = std::mem::take(&mut st.opens[open].phased);
        st.wake_all(phased);
    }
    w.release(open, GATE, true).await;
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
            blocked: Vec::new(),
            began: None,
            stop: false,
            seen: Seen::default(),
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
            if prune {
                exclusively(w.clone(), open).await;
            }
            let length = {
                let mut st = w.st();
                st.rng.within(copy)
            };
            w.cpu(length).await;
            exclusively(w.clone(), open).await;
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
    let bound = 200 * MS + 30 * length;
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
        st.locks.push(Lock {
            owner: h,
            start: BYTES + A as u64,
            end: BYTES + A as u64 + 1,
        });
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
/// `b`, each queued for the lane before the last one ends, beside a handle that cycles.
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
        under("a cycle", &seen.cycles, 2 * SEC)
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
    ]
}

fn seeds(default: u64) -> u64 {
    std::env::var("FERRO_GATE_SEEDS")
        .ok()
        .and_then(|seeds| seeds.parse().ok())
        .unwrap_or(default)
}

/// Runs `scenario` under `design` for `count` seeds, calm and busy, and returns how many failed
/// and the first failure.
fn sweep(design: Design, scenario: Scenario, count: u64) -> (u64, Option<String>, Vec<(u64, u64)>) {
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
        worst.push((longest(&verdict.seen.cycles), longest(&verdict.seen.joined)));
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
    let mut failures = Vec::new();
    for (name, scenario) in scenarios() {
        let (failed, first, worst) = sweep(design, scenario, count);
        let taken = spread(worst.iter().map(|(cycle, _)| *cycle).collect());
        let joined = spread(worst.iter().map(|(_, joined)| *joined).collect());
        println!(
            "{design:?} | {name} | failed {failed}/{count} | taker ms p50/p99/max {taken} | joined {joined}"
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
        (Design::Epochs, "#145 contended own branch"),
        (Design::Epochs, "#145 blocked write"),
        (Design::Epochs, "#145 awaited write"),
        (Design::Epochs, "#145 earlier own branch"),
        (Design::Epochs, "#146 through cycler"),
        (Design::Epochs, "#146 coalesced group"),
        (Design::Rounds, "#146 3 turns x 5 ms"),
        (Design::Rounds, "#146 2 turns x 2 ms"),
        (Design::Rounds, "#146 back to back"),
        (Design::Rounds, "#146 through cycler"),
        (Design::Rounds, "#146 coalesced group"),
        (Design::Phases(BLIND), "pipelined joins"),
        (Design::Phases(ALWAYS), "joined on a hot branch"),
    ];
    let all = scenarios();
    let mut missed = Vec::new();
    for (design, name) in found {
        let (_, scenario) = all
            .iter()
            .find(|(named, _)| *named == name)
            .expect("a scenario of that name");
        let (failed, _, _) = sweep(design, *scenario, 120);
        if failed == 0 {
            missed.push(format!("{design:?}: {name}"));
        }
    }
    assert!(missed.is_empty(), "the model never failed {missed:#?}");
}

#[test]
#[ignore = "a comparison of every design, printed; set FERRO_GATE_SEEDS for more seeds"]
fn compare_designs() {
    for design in [
        Design::Epochs,
        Design::Rounds,
        Design::Phases(BLIND),
        Design::Phases(ALWAYS),
        Design::Phases(CHOSEN),
    ] {
        let failures = report(design, seeds(300));
        println!("{design:?}: {failures:#?}");
    }
}
