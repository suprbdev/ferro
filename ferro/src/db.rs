use std::cell::OnceCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::mem;
use std::ops::{Deref, DerefMut, RangeBounds};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::admin::Keep;
use crate::codec::{Data, Decoder, Encoder, Hash, Object, Ptr, Sink, try_hash_of};
use crate::error::{Error, Result};
use crate::graph::{self, Start};
use crate::history::History;
use crate::log::{Chain, Entry, Log, Merged, Node};
use crate::map::Map;
use crate::platform::canonicalize;
use crate::reclaim::{Compacted, Copier, Reclaim, Space, rename, sibling, sync_parent};
use crate::reentry::{self, Crossing, Needs};
use crate::refs;
use crate::store::{
    Durability, Frame, Landing, Op, Retired, Settling, Store, StoreOptions, Trace, Txn,
};
use crate::watch::{Feed, Filter, Notice, Watcher};

const MAIN: &str = "main";
const MAX_PASSES: u32 = 6;
const QUIET_BYTES: u64 = 256 << 10;
const MAX_GROUP: usize = 256;

#[derive(Clone, Debug)]
pub struct Commit {
    pub version: u64,
    pub time_ms: u64,
    pub message: String,
    pub root: Option<Ptr>,
    pub restored: Option<u64>,
    pub(crate) parts: Parts,
    pub(crate) stamped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Parts {
    Lines,
    Whole,
    Joined(Arc<[usize]>),
}

impl Parts {
    pub(crate) fn join(messages: Vec<String>) -> (String, Parts) {
        if messages.len() < 2 {
            return (
                messages.into_iter().next().unwrap_or_default(),
                Parts::Whole,
            );
        }
        let lengths = messages.iter().map(String::len).collect();
        (messages.join("\n"), Parts::Joined(lengths))
    }

    pub(crate) fn check(&self, message: &str) -> Result<()> {
        let Parts::Joined(lengths) = self else {
            return Ok(());
        };
        let mut start = 0usize;
        for (index, len) in lengths.iter().enumerate() {
            let end = start
                .checked_add(*len)
                .filter(|end| *end <= message.len() && message.is_char_boundary(*end))
                .ok_or_else(|| Error::Decode("commit message parts overrun".into()))?;
            let last = index + 1 == lengths.len();
            if last != (end == message.len()) || (!last && message.as_bytes()[end] != b'\n') {
                return Err(Error::Decode("commit message parts do not match".into()));
            }
            start = end + 1;
        }
        if lengths.len() < 2 {
            return Err(Error::Decode(
                "a joined commit message needs two parts".into(),
            ));
        }
        Ok(())
    }
}

impl PartialEq for Commit {
    fn eq(&self, other: &Commit) -> bool {
        self.version == other.version
            && self.time_ms == other.time_ms
            && self.message == other.message
            && self.parts == other.parts
            && self.stamped == other.stamped
            && self.restored == other.restored
            && self.root.map(|ptr| ptr.hash) == other.root.map(|ptr| ptr.hash)
    }
}

impl Commit {
    pub fn messages(&self) -> impl Iterator<Item = &str> {
        let messages: Vec<&str> = match &self.parts {
            Parts::Lines => self
                .message
                .split('\n')
                .filter(|line| !line.is_empty())
                .collect(),
            Parts::Whole if self.message.is_empty() => Vec::new(),
            Parts::Whole => vec![self.message.as_str()],
            Parts::Joined(lengths) => {
                let mut start = 0;
                lengths
                    .iter()
                    .map(|len| {
                        let message = &self.message[start..start + len];
                        start += len + 1;
                        message
                    })
                    .collect()
            }
        };
        messages.into_iter()
    }

    fn genesis() -> Self {
        Commit {
            version: 0,
            time_ms: 0,
            message: String::new(),
            root: None,
            restored: None,
            parts: Parts::Whole,
            stamped: false,
        }
    }

    fn floor(&self) -> u64 {
        self.time_ms.saturating_add(u64::from(self.stamped))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed<R> {
    pub value: R,
    pub version: u64,
    pub time_ms: u64,
    pub changed: bool,
}

#[derive(Debug)]
pub struct Stamp<'a> {
    version: u64,
    floor: u64,
    time: &'a OnceCell<u64>,
    clock: Option<&'a Clock>,
}

impl Stamp<'_> {
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn time_ms(&self) -> u64 {
        *self
            .time
            .get_or_init(|| read_clock(self.clock).max(self.floor))
    }
}

impl Data for Commit {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.version.encode(w)?;
        self.time_ms.encode(w)?;
        self.message.encode(w)?;
        self.root.encode(w)?;
        self.restored.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        Ok(Commit {
            version: u64::decode(r)?,
            time_ms: u64::decode(r)?,
            message: String::decode(r)?,
            root: Option::<Ptr>::decode(r)?,
            restored: Option::<u64>::decode(r)?,
            parts: Parts::Lines,
            stamped: false,
        })
    }
}

#[derive(Clone)]
pub(crate) struct Head {
    pub(crate) version: u64,
    pub(crate) root: Option<Ptr>,
    pub(crate) log: Chain,
    pub(crate) merged: Map<String, (u64, u64)>,
}

fn encode_link(w: &mut Encoder<'_>, link: Option<Ptr>) {
    w.u8(u8::from(link.is_some()));
    if let Some(ptr) = link {
        w.link(ptr);
    }
}

impl Data for Head {
    const LOCATED: bool = true;

    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.version.encode(w)?;
        self.log.first.encode(w)?;
        encode_link(w, self.root);
        encode_link(w, self.log.tip);
        self.merged.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let version = u64::decode(r)?;
        let first = u64::decode(r)?;
        let root = Option::<Ptr>::decode(r)?;
        let tip = Option::<Ptr>::decode(r)?;
        let merged = Map::decode(r)?;
        Ok(Head {
            version,
            root,
            log: Chain::bounded(tip, first, version)?,
            merged,
        })
    }
}

pub(crate) type Refs = Map<String, Head>;

#[derive(Clone)]
struct RefState {
    refs: Refs,
    ptr: Option<Ptr>,
    depth: usize,
}

enum Next {
    Table(Refs),
    Head(String, Head),
}

pub struct Snapshot<T> {
    value: Arc<T>,
    commit: Arc<Commit>,
}

impl<T> Clone for Snapshot<T> {
    fn clone(&self) -> Self {
        Snapshot {
            value: self.value.clone(),
            commit: self.commit.clone(),
        }
    }
}

impl<T> Deref for Snapshot<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> fmt::Debug for Snapshot<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("version", &self.commit.version)
            .field("message", &self.commit.message)
            .finish_non_exhaustive()
    }
}

impl<T> Snapshot<T> {
    pub fn version(&self) -> u64 {
        self.commit.version
    }

    pub fn commit(&self) -> &Commit {
        &self.commit
    }
}

struct State<T> {
    snapshot: Snapshot<T>,
    log: Chain,
    store: Store,
}

impl<T: Data> State<T> {
    fn root_at(&self, version: u64) -> Result<Arc<T>> {
        let entry = self.log.get(&self.store, version)?;
        let root = entry.and_then(|entry| entry.commit.root);
        self.store.load::<T>(root.ok_or(Error::NoVersion(version))?)
    }
}

fn changes_between<V: PartialEq>(
    low: (u64, &V),
    high: (u64, &V),
    value_at: &impl Fn(u64) -> Result<V>,
    out: &mut Vec<u64>,
) -> Result<()> {
    if low.1 == high.1 {
        return Ok(());
    }
    if high.0 == low.0 + 1 {
        out.push(high.0);
        return Ok(());
    }
    let middle = low.0 + (high.0 - low.0) / 2;
    let value = value_at(middle)?;
    changes_between(low, (middle, &value), value_at, out)?;
    changes_between((middle, &value), high, value_at, out)
}

#[derive(Default)]
struct Intent {
    restored: Option<u64>,
    merged: Option<Merged>,
    record: Option<(String, u64)>,
    time: Option<u64>,
}

struct Change<'a, T> {
    branch: &'a Arc<Branch<T>>,
    state: &'a State<T>,
    working: &'a T,
    message: String,
    parts: Parts,
    intent: Intent,
    seal: Option<&'a Arc<Seal>>,
    from: Option<&'a Branch<T>>,
}

struct Branch<T> {
    name: String,
    db: u64,
    state: ArcSwap<State<T>>,
    tip: ArcSwap<State<T>>,
    feed: Arc<Feed<T>>,
    retired: AtomicBool,
    lane: Mutex<Option<Group<T>>>,
    arriving: AtomicUsize,
    gate: Mutex<VecDeque<(u64, Waker)>>,
    gated: AtomicUsize,
    framed: AtomicU64,
}

struct Lane<'a, T> {
    guard: Option<MutexGuard<'a, Option<Group<T>>>>,
    branch: &'a Branch<T>,
}

impl<T> Deref for Lane<'_, T> {
    type Target = Option<Group<T>>;

    fn deref(&self) -> &Option<Group<T>> {
        self.guard.as_ref().expect("ferro: the branch lock is held")
    }
}

impl<T> DerefMut for Lane<'_, T> {
    fn deref_mut(&mut self) -> &mut Option<Group<T>> {
        self.guard.as_mut().expect("ferro: the branch lock is held")
    }
}

impl<T> Drop for Lane<'_, T> {
    fn drop(&mut self) {
        drop(self.guard.take());
        reentry::release(self.branch.db, self.branch.key());
        self.branch.open_gate();
    }
}

impl<T> Branch<T> {
    fn new(name: &str, db: u64, state: Arc<State<T>>) -> Branch<T> {
        Branch {
            name: name.to_string(),
            db,
            feed: Arc::new(Feed::new(state.snapshot.clone())),
            state: ArcSwap::new(state.clone()),
            tip: ArcSwap::new(state),
            retired: AtomicBool::new(false),
            lane: Mutex::new(None),
            arriving: AtomicUsize::new(0),
            gate: Mutex::new(VecDeque::new()),
            gated: AtomicUsize::new(0),
            framed: AtomicU64::new(0),
        }
    }

    fn key(&self) -> usize {
        std::ptr::from_ref(self).addr()
    }

    fn hold<'a>(&'a self, guard: MutexGuard<'a, Option<Group<T>>>) -> Lane<'a, T> {
        reentry::hold(self.db, self.key(), &self.name);
        Lane {
            guard: Some(guard),
            branch: self,
        }
    }

    fn open_gate(&self) {
        fence(Ordering::SeqCst);
        if self.gated.load(Ordering::Relaxed) == 0 {
            return;
        }
        let next = {
            let mut gate = self.gate.lock();
            let next = gate.pop_front();
            self.gated.store(gate.len(), Ordering::Relaxed);
            next
        };
        if let Some((_, waker)) = next {
            waker.wake();
        }
    }

    fn enter(&self, id: u64, waker: &Waker, again: bool) -> Option<Lane<'_, T>> {
        if let Some(guard) = self.lane.try_lock() {
            if again {
                self.quit_gate(id);
            }
            return Some(self.hold(guard));
        }
        let mut gate = self.gate.lock();
        match gate.iter_mut().find(|(other, _)| *other == id) {
            Some((_, held)) => held.clone_from(waker),
            None if again => gate.push_front((id, waker.clone())),
            None => gate.push_back((id, waker.clone())),
        }
        self.gated.store(gate.len(), Ordering::Relaxed);
        fence(Ordering::SeqCst);
        let guard = self.lane.try_lock()?;
        gate.retain(|(other, _)| *other != id);
        self.gated.store(gate.len(), Ordering::Relaxed);
        drop(gate);
        Some(self.hold(guard))
    }

    fn quit_gate(&self, id: u64) -> bool {
        let mut gate = self.gate.lock();
        let before = gate.len();
        gate.retain(|(other, _)| *other != id);
        self.gated.store(gate.len(), Ordering::Relaxed);
        gate.len() != before
    }

    fn on_the_way(&self) -> bool {
        self.arriving.load(Ordering::Acquire) > 0 || self.gated.load(Ordering::Acquire) > 0
    }

    fn publish(&self, state: Arc<State<T>>) -> Notice<T> {
        let snapshot = state.snapshot.clone();
        self.state.store(state);
        self.feed.publish(snapshot)
    }

    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.feed.close();
    }

    fn check_live(&self) -> Result<()> {
        if self.retired.load(Ordering::Acquire) {
            return Err(Error::NoBranch(self.name.clone()));
        }
        Ok(())
    }
}

impl<T> Drop for Branch<T> {
    fn drop(&mut self) {
        self.feed.close();
    }
}

type Update<T> = Option<(Arc<Branch<T>>, Arc<State<T>>)>;

struct Pending<T> {
    frame: Frame,
    refs: RefState,
    update: Update<T>,
    seal: Option<Arc<Seal>>,
}

struct Task {
    ticket: u64,
    id: u64,
    waker: Waker,
}

struct Queue<T> {
    waiting: Vec<Pending<T>>,
    open: Vec<(Arc<Seal>, Arc<Branch<T>>)>,
    settled: u64,
    syncing: bool,
    failed: bool,
    tasks: Vec<Task>,
}

#[derive(Clone, Copy)]
struct Sealed {
    version: u64,
    time_ms: u64,
    ticket: u64,
    framed: bool,
}

enum Waiter {
    Thread(Thread),
    Task(u64, Waker),
}

impl Waiter {
    fn wake(self) {
        match self {
            Waiter::Thread(thread) => thread.unpark(),
            Waiter::Task(_, waker) => waker.wake(),
        }
    }

    fn is(&self, id: u64) -> bool {
        matches!(self, Waiter::Task(task, _) if *task == id)
    }
}

#[derive(Default)]
struct Seal {
    outcome: OnceLock<std::result::Result<Sealed, String>>,
    settled: AtomicBool,
    parked: Mutex<Vec<Waiter>>,
}

impl Seal {
    fn wake_all(&self) {
        let parked = mem::take(&mut *self.parked.lock());
        for waiter in parked {
            waiter.wake();
        }
    }

    fn wake_one(&self) {
        let waiter = self.parked.lock().pop();
        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }

    fn listen(&self, id: u64, waker: &Waker) {
        let mut parked = self.parked.lock();
        match parked.iter_mut().find(|waiter| waiter.is(id)) {
            Some(Waiter::Task(_, held)) => held.clone_from(waker),
            _ => parked.push(Waiter::Task(id, waker.clone())),
        }
    }

    fn forget(&self, id: u64) {
        self.parked.lock().retain(|waiter| !waiter.is(id));
    }
}

struct Group<T> {
    state: Arc<State<T>>,
    working: T,
    messages: Vec<String>,
    members: usize,
    seal: Arc<Seal>,
    time: OnceCell<u64>,
    handed: Option<Instant>,
}

enum Grouped {
    Settled(u64, u64),
    Unframed(u64, u64, u64),
}

#[derive(Default)]
struct Costs {
    sync: AtomicU64,
    seal: AtomicU64,
    handoff: AtomicU64,
}

fn nanos(took: Duration) -> u64 {
    u64::try_from(took.as_nanos()).unwrap_or(u64::MAX)
}

fn adjust(slot: &AtomicU64, next: impl Fn(u64) -> u64) {
    let _ = slot.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(next(current))
    });
}

fn average(slot: &AtomicU64, took: Duration) {
    let sample = nanos(took);
    adjust(slot, |mean| mean - mean / 8 + sample / 8);
}

impl Costs {
    fn synced(&self, took: Duration) {
        average(&self.sync, took);
    }

    fn sealed(&self, took: Duration) {
        average(&self.seal, took);
    }

    fn handed_over(&self, took: Duration) {
        let sample = nanos(took);
        adjust(&self.handoff, |estimate| {
            if sample >= estimate {
                sample
            } else {
                estimate - (estimate - sample) / 8
            }
        });
    }

    fn worth_waiting(&self) -> bool {
        let handoff = self.handoff.load(Ordering::Relaxed);
        let saved = self
            .seal
            .load(Ordering::Relaxed)
            .saturating_add(self.sync.load(Ordering::Relaxed));
        if handoff < saved {
            return true;
        }
        adjust(&self.handoff, |estimate| estimate - estimate / 1024);
        false
    }
}

#[derive(Default)]
struct CycleLog {
    cycles: u64,
    longest_pause: Duration,
    last_error: Option<String>,
}

#[derive(Default)]
struct Signal {
    stop: AtomicBool,
    due: Mutex<bool>,
    wake: Condvar,
    base: AtomicU64,
}

impl Signal {
    fn raise(&self) {
        *self.due.lock() = true;
        self.wake.notify_one();
    }

    fn wait(&self) -> bool {
        let mut due = self.due.lock();
        while !*due && !self.stop.load(Ordering::Acquire) {
            self.wake.wait(&mut due);
        }
        *due = false;
        !self.stop.load(Ordering::Acquire)
    }
}

struct Keeper {
    signal: Arc<Signal>,
    thread: Mutex<Option<JoinHandle<()>>>,
    syncer: Mutex<Option<(Arc<Signal>, JoinHandle<()>)>>,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        self.signal.stop.store(true, Ordering::Release);
        self.signal.raise();
        if let Some(thread) = self.thread.lock().take() {
            let _ = thread.join();
        }
        if let Some((signal, thread)) = self.syncer.lock().take() {
            signal.stop.store(true, Ordering::Release);
            signal.raise();
            if thread.thread().id() != thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

struct Shared<T> {
    id: u64,
    store: ArcSwap<Store>,
    path: PathBuf,
    file: PathBuf,
    options: StoreOptions,
    coalesce: bool,
    costs: Costs,
    tip: Mutex<RefState>,
    published: Mutex<RefState>,
    queue: Mutex<Queue<T>>,
    done: Condvar,
    genesis: Arc<T>,
    genesis_hash: Hash,
    branches: Mutex<HashMap<String, Arc<Branch<T>>>>,
    history: Keep,
    reclaim: Reclaim,
    automatic: bool,
    hook: Option<Hook>,
    clock: Option<Clock>,
    cycle: Mutex<()>,
    log: Mutex<CycleLog>,
    retired: Mutex<Vec<Retired>>,
    signal: Arc<Signal>,
    syncer: OnceLock<Arc<Signal>>,
    ids: AtomicU64,
    flush: fn(&Shared<T>),
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        if let Some(syncer) = self.syncer.get() {
            (self.flush)(self);
            syncer.stop.store(true, Ordering::Release);
            syncer.raise();
        }
        let store = self.store.load();
        let _ = store.settle_anchor();
        store.close();
    }
}

impl<T> Shared<T> {
    fn store(&self) -> Store {
        (**self.store.load()).clone()
    }

    fn rouse(&self) {
        if let Some(syncer) = self.syncer.get() {
            syncer.raise();
        }
    }

    fn forget(&self, id: u64) {
        self.queue.lock().tasks.retain(|task| task.id != id);
    }
}

struct Staged<R, E> {
    result: std::result::Result<(R, u64, u64), E>,
    ticket: u64,
    before: u64,
}

impl<R, E> Staged<R, E> {
    fn finish(self) -> std::result::Result<Committed<R>, E> {
        let (value, version, time_ms) = self.result?;
        Ok(Committed {
            value,
            version,
            time_ms,
            changed: version != self.before,
        })
    }
}

struct Joined<R, E> {
    result: std::result::Result<R, E>,
    seal: Arc<Seal>,
    before: u64,
}

impl<R, E> Joined<R, E> {
    fn finish(self, version: u64, time_ms: u64) -> std::result::Result<Committed<R>, E> {
        Ok(Committed {
            value: self.result?,
            version,
            time_ms,
            changed: version != self.before,
        })
    }

    fn unframed(self, ticket: u64, version: u64, time_ms: u64) -> Staged<R, E> {
        Staged {
            result: self.result.map(|value| (value, version, time_ms)),
            ticket,
            before: self.before,
        }
    }
}

enum Wait<R, E> {
    Ticket(Staged<R, E>),
    Group(Joined<R, E>),
}

struct Writing<'a, T, F, R, E> {
    db: &'a Db<T>,
    id: u64,
    start: Option<F>,
    gated: bool,
    wait: Option<Wait<R, E>>,
    crossing: Option<Crossing>,
}

impl<T, F, R, E> Unpin for Writing<'_, T, F, R, E> {}

impl<T: Data, F, R, E> Future for Writing<'_, T, F, R, E>
where
    F: FnOnce(&mut T, &Stamp<'_>) -> std::result::Result<(String, R), E>,
    E: From<Error>,
{
    type Output = std::result::Result<Committed<R>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let db = this.db;
        let needs = match this.start {
            Some(_) => Needs::Lane(&db.branch.name),
            None => Needs::Ack,
        };
        match db.entering(needs) {
            Ok(crossing) => this.crossing = crossing,
            Err(error) => return Poll::Ready(Err(error.into())),
        }
        if let Some(f) = this.start.take() {
            db.shared.start_syncer(&db.keeper);
            let Some(lane) = db.branch.enter(this.id, cx.waker(), this.gated) else {
                this.start = Some(f);
                this.gated = true;
                return Poll::Pending;
            };
            this.gated = false;
            match db.begin(lane, f) {
                Ok(wait) => this.wait = Some(wait),
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
        loop {
            let wait = this
                .wait
                .take()
                .expect("ferro: a write was polled after it finished");
            match wait {
                Wait::Ticket(staged) => {
                    match db.shared.poll_settle(staged.ticket, this.id, cx.waker()) {
                        Poll::Pending => {
                            this.wait = Some(Wait::Ticket(staged));
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(error)) => return Poll::Ready(Err(error.into())),
                        Poll::Ready(Ok(())) => return Poll::Ready(staged.finish()),
                    }
                }
                Wait::Group(joined) => {
                    match db.shared.poll_group(&joined.seal, this.id, cx.waker()) {
                        Poll::Pending => {
                            this.wait = Some(Wait::Group(joined));
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(error)) => return Poll::Ready(Err(error.into())),
                        Poll::Ready(Ok(Grouped::Settled(version, time_ms))) => {
                            return Poll::Ready(joined.finish(version, time_ms));
                        }
                        Poll::Ready(Ok(Grouped::Unframed(ticket, version, time_ms))) => {
                            joined.seal.forget(this.id);
                            this.wait =
                                Some(Wait::Ticket(joined.unframed(ticket, version, time_ms)));
                        }
                    }
                }
            }
        }
    }
}

impl<T, F, R, E> Drop for Writing<'_, T, F, R, E> {
    fn drop(&mut self) {
        let shared = &self.db.shared;
        if self.gated {
            let branch = &self.db.branch;
            if !branch.quit_gate(self.id) {
                branch.open_gate();
            }
            shared.rouse();
        }
        match &self.wait {
            Some(Wait::Ticket(_)) => shared.forget(self.id),
            Some(Wait::Group(joined)) => {
                joined.seal.forget(self.id);
                if !joined.seal.settled.load(Ordering::Acquire) {
                    shared.rouse();
                }
            }
            None => {}
        }
    }
}

pub struct Db<T> {
    shared: Arc<Shared<T>>,
    branch: Arc<Branch<T>>,
    keeper: Arc<Keeper>,
}

impl<T> fmt::Debug for Db<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.shared.path)
            .field("branch", &self.branch.name)
            .field("version", &self.branch.state.load().snapshot.commit.version)
            .finish_non_exhaustive()
    }
}

impl<T> Clone for Db<T> {
    fn clone(&self) -> Self {
        Db {
            shared: self.shared.clone(),
            branch: self.branch.clone(),
            keeper: self.keeper.clone(),
        }
    }
}

#[doc(hidden)]
#[derive(Clone)]
pub struct Hook(pub Arc<dyn Fn(&'static str) + Send + Sync>);

impl fmt::Debug for Hook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Hook")
    }
}

#[doc(hidden)]
#[derive(Clone)]
pub struct Clock(pub Arc<dyn Fn() -> u64 + Send + Sync>);

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Clock")
    }
}

fn read_clock(clock: Option<&Clock>) -> u64 {
    clock.map_or_else(now_ms, |clock| (clock.0)())
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    store: StoreOptions,
    history: Keep,
    reclaim: Reclaim,
    coalesce: bool,
    hook: Option<Hook>,
    clock: Option<Clock>,
}

impl Options {
    pub fn new() -> Self {
        Options::default()
    }

    pub fn durability(mut self, durability: Durability) -> Self {
        self.store.durability = durability;
        self
    }

    pub fn cache_bytes(mut self, bytes: u64) -> Self {
        self.store.cache_bytes = bytes;
        self
    }

    pub fn checkpoint_bytes(mut self, bytes: u64) -> Self {
        self.store.checkpoint_bytes = bytes;
        self
    }

    pub fn read_only(mut self) -> Self {
        self.store.read_only = true;
        self
    }

    pub fn history(mut self, keep: Keep) -> Self {
        self.history = keep;
        self
    }

    pub fn reclaim(mut self, reclaim: Reclaim) -> Self {
        self.reclaim = reclaim;
        self
    }

    pub fn coalesce(mut self, coalesce: bool) -> Self {
        self.coalesce = coalesce;
        self
    }

    #[doc(hidden)]
    pub fn hook(mut self, hook: Hook) -> Self {
        self.hook = Some(hook);
        self
    }

    #[doc(hidden)]
    pub fn clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    #[doc(hidden)]
    pub fn trace(mut self, trace: Trace) -> Self {
        self.store.trace = Some(trace);
        self
    }

    pub fn open<T: Data + Default>(self, path: impl AsRef<Path>) -> Result<Db<T>> {
        self.open_with(path, T::default)
    }

    pub fn open_with<T: Data>(
        self,
        path: impl AsRef<Path>,
        init: impl FnOnce() -> T,
    ) -> Result<Db<T>> {
        let read_only = self.store.read_only;
        let options = self.store.clone();
        let (store, head) = Store::open(path.as_ref(), self.store)?;
        let genesis = Arc::new(init());
        let genesis_hash = try_hash_of(&*genesis)?;
        let (refs, depth) = refs::load(&store, head, None).inspect_err(|_| store.close())?;
        let settled = store.last_seq();
        let automatic = !read_only && self.history != Keep::All && self.reclaim.every_bytes > 0;
        let signal = Arc::new(Signal {
            base: AtomicU64::new(store.basis().bytes),
            ..Signal::default()
        });
        let file = canonicalize(path.as_ref())?;
        let mut options = options;
        let shared = Arc::new(Shared {
            id: reentry::next_db(),
            store: ArcSwap::from_pointee(store),
            path: path.as_ref().to_path_buf(),
            file,
            options: {
                options.read_only = read_only;
                options
            },
            coalesce: self.coalesce && !read_only,
            costs: Costs::default(),
            tip: Mutex::new(RefState {
                refs: refs.clone(),
                ptr: head,
                depth,
            }),
            published: Mutex::new(RefState {
                refs,
                ptr: head,
                depth,
            }),
            queue: Mutex::new(Queue {
                waiting: Vec::new(),
                open: Vec::new(),
                settled,
                syncing: false,
                failed: false,
                tasks: Vec::new(),
            }),
            done: Condvar::new(),
            genesis,
            genesis_hash,
            branches: Mutex::new(HashMap::new()),
            history: self.history,
            reclaim: self.reclaim,
            automatic,
            hook: self.hook,
            clock: self.clock,
            cycle: Mutex::new(()),
            log: Mutex::new(CycleLog::default()),
            retired: Mutex::new(Vec::new()),
            signal: signal.clone(),
            syncer: OnceLock::new(),
            ids: AtomicU64::new(0),
            flush: Shared::<T>::flush,
        });
        let branch = {
            let state = shared.published.lock();
            shared.attach(MAIN, &state.refs)?
        };
        let thread = automatic.then(|| Shared::spawn(&shared));
        let keeper = Arc::new(Keeper {
            signal,
            thread: Mutex::new(thread),
            syncer: Mutex::new(None),
        });
        Ok(Db {
            shared,
            branch,
            keeper,
        })
    }
}

fn write_decoded<D: Data>(value: &D, txn: &mut Txn, store: &Store) -> Result<(Ptr, D)> {
    let mut encoder = Encoder::new(txn);
    value.encode(&mut encoder)?;
    let object = encoder.into_object();
    let mut decoder = Decoder::new(&object.body, &object.links, Some(store));
    let decoded = D::decode(&mut decoder)?;
    let hash = object.hash();
    Ok((txn.put(object, hash)?, decoded))
}

type Placed = (u64, u64, Option<(Settling, Store, Landing)>);

struct Draft<T> {
    txn: Txn,
    store: Store,
    root: Ptr,
    value: T,
    object: Object,
    entry: Option<(Commit, Chain)>,
}

fn write_draft<T: Data>(working: &T, store: Store) -> Result<Draft<T>> {
    let mut txn = store.begin()?;
    let mut encoder = Encoder::new(&mut txn);
    working.encode(&mut encoder)?;
    let object = encoder.into_object();
    let value = T::decode(&mut Decoder::new(&object.body, &object.links, Some(&store)))?;
    let root = txn.put_ref(&object, object.hash())?;
    Ok(Draft {
        txn,
        store,
        root,
        value,
        object,
        entry: None,
    })
}

fn holds(found: &[Start], ptr: Ptr) -> bool {
    found.iter().any(|start| start.node.ptr.hash == ptr.hash)
}

fn recorded((refs, store): (&Refs, &Store), starts: &[Start]) -> Result<Vec<Start>> {
    let mut out = starts.to_vec();
    for (_, head) in refs.try_entries()? {
        if head.merged.is_empty() {
            continue;
        }
        for start in starts {
            let version = start.node.version();
            let on = head.log.node_at(store, version)?;
            if on.is_none_or(|node| node.ptr.hash != start.node.ptr.hash) {
                continue;
            }
            for (other, (theirs, ours)) in head.merged.try_entries()? {
                let Some(found) = refs.try_get(&other)?.filter(|_| ours <= version) else {
                    continue;
                };
                if let Some(node) = found.log.node_at(store, theirs)? {
                    out.push(Start {
                        store: store.clone(),
                        node,
                        low: found.log.first,
                        after: found.log.after(store)?,
                        bare: false,
                    });
                }
            }
        }
    }
    Ok(out)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

impl<T: Data> Shared<T> {
    fn state_of(&self, store: &Store, head: Option<&Head>) -> Result<State<T>> {
        let Some(head) = head else {
            return Ok(State {
                snapshot: Snapshot {
                    value: self.genesis.clone(),
                    commit: Arc::new(Commit::genesis()),
                },
                log: Chain::empty(),
                store: store.clone(),
            });
        };
        let value = match head.root {
            Some(ptr) => store.load::<T>(ptr)?,
            None => self.genesis.clone(),
        };
        let commit = head
            .log
            .tip_entry(store)?
            .map(|entry| entry.commit.clone())
            .unwrap_or_else(|| Arc::new(Commit::genesis()));
        Ok(State {
            snapshot: Snapshot { value, commit },
            log: head.log,
            store: store.clone(),
        })
    }

    fn attach(&self, name: &str, refs: &Refs) -> Result<Arc<Branch<T>>> {
        let mut branches = self.branches.lock();
        if let Some(branch) = branches.get(name) {
            return Ok(branch.clone());
        }
        let head = refs.try_get(name)?;
        if head.is_none() && name != MAIN {
            return Err(Error::NoBranch(name.to_string()));
        }
        let state = Arc::new(self.state_of(&self.store(), head.as_deref())?);
        let branch = Arc::new(Branch::new(name, self.id, state));
        branches.insert(name.to_string(), branch.clone());
        Ok(branch)
    }

    fn attached(&self, name: &str) -> Result<Arc<Branch<T>>> {
        if let Some(branch) = self.branches.lock().get(name) {
            return Ok(branch.clone());
        }
        let _tip = self.tip.lock();
        let _whole = reentry::whole(self.id);
        let published = self.published.lock();
        self.attach(name, &published.refs)
    }

    fn lane<'a>(&self, branch: &'a Arc<Branch<T>>) -> Lane<'a, T> {
        if !self.coalesce {
            return branch.hold(branch.lane.lock());
        }
        let mut lane = self.arrive(branch);
        if let Some(group) = lane.take() {
            self.seal_for_others(branch, group);
        }
        lane
    }

    fn exclusively<R>(&self, f: impl FnOnce(&mut RefState) -> R) -> R {
        loop {
            let mut branches: Vec<_> = self.branches.lock().values().cloned().collect();
            branches.sort_by(|a, b| a.name.cmp(&b.name));
            let lanes: Vec<_> = branches.iter().map(|branch| self.lane(branch)).collect();
            let mut tip = self.tip.lock();
            let attached = self.branches.lock();
            let same = attached.len() == branches.len()
                && branches.iter().all(|branch| {
                    attached
                        .get(&branch.name)
                        .is_some_and(|now| Arc::ptr_eq(now, branch))
                });
            drop(attached);
            if same {
                let whole = reentry::whole(self.id);
                let out = f(&mut tip);
                drop(whole);
                drop(tip);
                lanes.into_iter().rev().for_each(drop);
                return out;
            }
            drop(tip);
            lanes.into_iter().rev().for_each(drop);
        }
    }

    fn seal_for_others(&self, branch: &Arc<Branch<T>>, group: Group<T>) {
        let seal = group.seal.clone();
        self.seal_group(branch, group);
        seal.wake_one();
        self.rouse();
    }

    fn arrive<'a>(&self, branch: &'a Branch<T>) -> Lane<'a, T> {
        branch.arriving.fetch_add(1, Ordering::AcqRel);
        self.step("arriving");
        let guard = branch.lane.lock();
        branch.arriving.fetch_sub(1, Ordering::AcqRel);
        branch.hold(guard)
    }

    fn join_group(&self, lane: &mut Lane<'_, T>, branch: &Branch<T>) -> Group<T> {
        if let Some(mut group) = lane.take() {
            if let Some(handed) = group.handed.take() {
                self.costs.handed_over(handed.elapsed());
            }
            return group;
        }
        let state = branch.tip.load_full();
        Group {
            working: (*state.snapshot.value).clone(),
            state,
            messages: Vec::new(),
            members: 0,
            seal: Arc::new(Seal::default()),
            time: OnceCell::new(),
            handed: None,
        }
    }

    fn pass_on(
        &self,
        lane: &mut Lane<'_, T>,
        branch: &Arc<Branch<T>>,
        mut group: Group<T>,
        others: &mut Vec<Arc<Branch<T>>>,
    ) -> Option<Group<T>> {
        if group.members == 0 {
            return None;
        }
        let mut queue = self.queue.lock();
        let arriving = branch.on_the_way();
        let handed_on = queue.syncing || (arriving && self.costs.worth_waiting());
        if handed_on && group.members < MAX_GROUP {
            group.handed = arriving.then(Instant::now);
            others.extend(Self::other_groups(&queue, branch));
            if !queue
                .open
                .iter()
                .any(|(open, _)| Arc::ptr_eq(open, &group.seal))
            {
                queue.open.push((group.seal.clone(), branch.clone()));
            }
            **lane = Some(group);
            return None;
        }
        Some(group)
    }

    fn other_groups(queue: &Queue<T>, branch: &Arc<Branch<T>>) -> Vec<Arc<Branch<T>>> {
        let others = queue
            .open
            .iter()
            .filter(|(_, other)| !Arc::ptr_eq(other, branch));
        others.map(|(_, other)| other.clone()).collect()
    }

    fn seal_group(&self, branch: &Arc<Branch<T>>, group: Group<T>) {
        let Group {
            state,
            working,
            messages,
            seal,
            time,
            ..
        } = group;
        let (message, parts) = Parts::join(messages);
        let started = Instant::now();
        let framed = branch.framed.load(Ordering::Acquire);
        let written = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut change = Change {
                branch,
                state: &state,
                working: &working,
                message,
                parts,
                intent: Intent {
                    time: time.into_inner(),
                    ..Intent::default()
                },
                seal: Some(&seal),
                from: None,
            };
            let drafted = self.draft(&mut change)?;
            let mut tip = self.tip.lock();
            let whole = reentry::whole(self.id);
            let committed = self.commit_locked(&mut tip, change, drafted);
            drop(whole);
            drop(tip);
            let (version, time_ms, settling) = committed?;
            if let Some((settling, store, landing)) = settling {
                self.land(branch, &state, framed, landing)?;
                settling.finish(&store);
            }
            Ok::<_, Error>((version, time_ms))
        }));
        self.costs.sealed(started.elapsed());
        let ticket = branch.framed.load(Ordering::Acquire);
        let mut queue = self.queue.lock();
        queue.open.retain(|(open, _)| !Arc::ptr_eq(open, &seal));
        let _ = seal.outcome.set(match written {
            Ok(Ok((version, time_ms))) => Ok(Sealed {
                version,
                time_ms,
                ticket,
                framed: false,
            }),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("a value panicked while it was written".to_string()),
        });
        let framed = matches!(seal.outcome.get(), Some(Ok(sealed)) if sealed.framed);
        drop(queue);
        if !framed {
            seal.wake_all();
        }
    }

    fn await_group(&self, branch: &Arc<Branch<T>>, seal: &Arc<Seal>) -> Result<(u64, u64)> {
        let mut notices = Vec::new();
        loop {
            if seal.settled.load(Ordering::Acquire)
                && let Some(Ok(sealed)) = seal.outcome.get()
            {
                return Ok((sealed.version, sealed.time_ms));
            }
            let queue = self.queue.lock();
            if queue.failed {
                return Err(Error::Poisoned);
            }
            match seal.outcome.get() {
                Some(Err(failure)) => {
                    self.store().check_healthy()?;
                    return Err(Error::GroupFailed(failure.clone()));
                }
                Some(Ok(sealed)) if queue.settled >= sealed.ticket => {
                    return Ok((sealed.version, sealed.time_ms));
                }
                Some(Ok(sealed)) if !sealed.framed => {
                    drop(queue);
                    self.settle(sealed.ticket, &mut notices)?;
                    return Ok((sealed.version, sealed.time_ms));
                }
                Some(Ok(_)) if !queue.syncing && !queue.waiting.is_empty() => {
                    let others = Self::other_groups(&queue, branch);
                    if others.is_empty() {
                        self.lead(queue, &mut notices)?;
                        continue;
                    }
                    drop(queue);
                    self.seal_idle(&others);
                    let queue = self.queue.lock();
                    if !queue.syncing && !queue.waiting.is_empty() {
                        self.lead(queue, &mut notices)?;
                    }
                    continue;
                }
                None if !queue.syncing && branch.arriving.load(Ordering::Acquire) == 0 => {
                    drop(queue);
                    let mut lane = self.arrive(branch);
                    if let Some(group) = lane.take() {
                        self.seal_group(branch, group);
                    }
                    continue;
                }
                _ => {}
            }
            seal.parked.lock().push(Waiter::Thread(thread::current()));
            drop(queue);
            thread::park();
        }
    }

    fn poll_settle(&self, ticket: u64, id: u64, waker: &Waker) -> Poll<Result<()>> {
        let mut queue = self.queue.lock();
        if queue.settled >= ticket {
            return Poll::Ready(Ok(()));
        }
        if queue.failed {
            return Poll::Ready(Err(Error::Poisoned));
        }
        if !queue.syncing && queue.waiting.is_empty() {
            queue.tasks.retain(|task| task.id != id);
            return Poll::Ready(Ok(()));
        }
        match queue.tasks.iter_mut().find(|task| task.id == id) {
            Some(task) => task.waker.clone_from(waker),
            None => queue.tasks.push(Task {
                ticket,
                id,
                waker: waker.clone(),
            }),
        }
        let idle = !queue.syncing;
        drop(queue);
        if idle {
            self.rouse();
        }
        Poll::Pending
    }

    fn poll_group(&self, seal: &Arc<Seal>, id: u64, waker: &Waker) -> Poll<Result<Grouped>> {
        if seal.settled.load(Ordering::Acquire)
            && let Some(Ok(sealed)) = seal.outcome.get()
        {
            return Poll::Ready(Ok(Grouped::Settled(sealed.version, sealed.time_ms)));
        }
        let queue = self.queue.lock();
        if queue.failed {
            return Poll::Ready(Err(Error::Poisoned));
        }
        match seal.outcome.get() {
            Some(Err(failure)) => {
                self.store().check_healthy()?;
                return Poll::Ready(Err(Error::GroupFailed(failure.clone())));
            }
            Some(Ok(sealed)) if queue.settled >= sealed.ticket => {
                return Poll::Ready(Ok(Grouped::Settled(sealed.version, sealed.time_ms)));
            }
            Some(Ok(sealed)) if !sealed.framed => {
                let Sealed {
                    version,
                    time_ms,
                    ticket,
                    ..
                } = *sealed;
                return Poll::Ready(Ok(Grouped::Unframed(ticket, version, time_ms)));
            }
            _ => {}
        }
        seal.listen(id, waker);
        let idle = !queue.syncing;
        drop(queue);
        if idle {
            self.rouse();
        }
        Poll::Pending
    }

    fn start_syncer(self: &Arc<Self>, keeper: &Keeper) {
        if self.syncer.get().is_some() {
            return;
        }
        let mut slot = keeper.syncer.lock();
        if self.syncer.get().is_some() {
            return;
        }
        let signal = Arc::new(Signal::default());
        let weak = Arc::downgrade(self);
        let wake = signal.clone();
        let thread = thread::Builder::new()
            .name("ferro-sync".to_string())
            .spawn(move || {
                while wake.wait() {
                    let Some(shared) = weak.upgrade() else {
                        return;
                    };
                    let _ = panic::catch_unwind(AssertUnwindSafe(|| shared.drain()));
                }
            })
            .expect("ferro: start the sync thread");
        let _ = self.syncer.set(signal.clone());
        *slot = Some((signal, thread));
    }

    fn drain(&self) {
        let mut notices = Vec::new();
        loop {
            let mut queue = self.queue.lock();
            if queue.failed {
                return;
            }
            if queue.syncing {
                self.done.wait(&mut queue);
                continue;
            }
            let stranded: Vec<_> = queue
                .open
                .iter()
                .filter(|(_, branch)| !branch.on_the_way())
                .map(|(_, branch)| branch.clone())
                .collect();
            if !stranded.is_empty() {
                drop(queue);
                if self.close_stranded(&stranded) {
                    continue;
                }
                queue = self.queue.lock();
                if queue.failed || queue.syncing {
                    continue;
                }
            }
            if queue.waiting.is_empty() {
                return;
            }
            let _ = self.lead(queue, &mut notices);
            notices.clear();
        }
    }

    fn close_stranded(&self, stranded: &[Arc<Branch<T>>]) -> bool {
        let mut closed = false;
        for branch in stranded {
            let mut lane = self.arrive(branch);
            if self.queue.lock().syncing {
                return closed;
            }
            if let Some(group) = lane.take() {
                self.seal_group(branch, group);
                closed = true;
            }
        }
        closed
    }

    fn seal_idle(&self, open: &[Arc<Branch<T>>]) {
        for branch in open {
            if branch.on_the_way() {
                continue;
            }
            let Some(guard) = branch.lane.try_lock() else {
                continue;
            };
            let mut lane = branch.hold(guard);
            if let Some(group) = lane.take() {
                self.seal_group(branch, group);
            }
        }
    }

    fn flush(&self) {
        let mut notices = Vec::new();
        let branches: Vec<_> = self.branches.lock().values().cloned().collect();
        for branch in &branches {
            let mut lane = branch.lane.lock();
            if let Some(group) = lane.take() {
                self.seal_group(branch, group);
            }
        }
        let queue = self.queue.lock();
        if !queue.syncing && !queue.failed && !queue.waiting.is_empty() {
            let _ = self.lead(queue, &mut notices);
        }
    }

    fn stage(
        &self,
        tip: &mut RefState,
        next: Next,
        mut txn: Txn,
        branch: Option<(&Arc<Branch<T>>, State<T>)>,
        seal: Option<(&Arc<Seal>, u64, u64)>,
        settling: Option<&mut Settling>,
    ) -> Result<Landing> {
        txn.unshared();
        let store = self.store();
        let (ptr, refs, depth) = match (next, tip.ptr) {
            (Next::Head(name, head), Some(prev)) if tip.depth < refs::STEPS => {
                let (ptr, head) = refs::write_step(&mut txn, &store, prev, &name, &head)?;
                let mut refs = tip.refs.clone();
                refs.try_insert(name, head)?;
                (ptr, refs, tip.depth + 1)
            }
            (next, _) => {
                let refs = match next {
                    Next::Table(refs) => refs,
                    Next::Head(name, head) => {
                        let mut refs = tip.refs.clone();
                        refs.try_insert(name, head)?;
                        refs
                    }
                };
                let (ptr, refs) = write_decoded(&refs, &mut txn, &store)?;
                (ptr, refs, 0)
            }
        };
        if let Some(settling) = settling {
            settling.take_seen(&mut txn);
        }
        let landing = txn.place(ptr)?;
        let frame = landing.frame();
        let update = branch.map(|(branch, state)| {
            let state = Arc::new(state);
            branch.tip.store(state.clone());
            branch.framed.store(frame.seq, Ordering::Release);
            (branch.clone(), state)
        });
        tip.refs = refs;
        tip.ptr = Some(ptr);
        tip.depth = depth;
        let refs = tip.clone();
        let mut queue = self.queue.lock();
        if let Some((seal, version, time_ms)) = seal {
            let _ = seal.outcome.set(Ok(Sealed {
                version,
                time_ms,
                ticket: frame.seq,
                framed: true,
            }));
        }
        let seal = seal.map(|(seal, _, _)| seal.clone());
        queue.waiting.push(Pending {
            frame,
            refs,
            update,
            seal,
        });
        Ok(landing)
    }

    fn settle(&self, ticket: u64, notices: &mut Vec<Notice<T>>) -> Result<()> {
        let mut queue = self.queue.lock();
        loop {
            if queue.settled >= ticket {
                return Ok(());
            }
            if queue.failed {
                return Err(Error::Poisoned);
            }
            if !queue.syncing {
                if queue.waiting.is_empty() {
                    return Ok(());
                }
                self.lead(queue, notices)?;
                queue = self.queue.lock();
                continue;
            }
            self.done.wait(&mut queue);
        }
    }

    fn lead(
        &self,
        mut queue: MutexGuard<'_, Queue<T>>,
        notices: &mut Vec<Notice<T>>,
    ) -> Result<()> {
        queue.syncing = true;
        let mut batch = mem::take(&mut queue.waiting);
        drop(queue);
        let store = self.store();
        let end = batch.last().expect("a leader has queued commits").frame.end;
        let landed = store.landed_to(end);
        let kept = batch
            .iter()
            .take_while(|pending| pending.frame.end <= landed)
            .count();
        let cut = batch.split_off(kept);
        let started = Instant::now();
        let outcome = match batch.last() {
            Some(last) => store
                .sync(last.frame)
                .map(|()| (last.frame, last.refs.clone())),
            None => Err(Error::Poisoned),
        };
        self.costs.synced(started.elapsed());
        let synced = outcome.is_ok();
        let mut seals = Vec::new();
        if let Ok((_, refs)) = &outcome {
            *self.published.lock() = refs.clone();
        }
        for pending in batch {
            seals.extend(pending.seal.map(|seal| (seal, synced)));
            if let (true, Some((branch, state))) = (synced, pending.update) {
                notices.push(branch.publish(state));
            }
        }
        let lost = !cut.is_empty();
        seals.extend(
            cut.into_iter()
                .filter_map(|pending| pending.seal.map(|seal| (seal, false))),
        );
        let mut queue = self.queue.lock();
        queue.syncing = false;
        match &outcome {
            Ok((frame, _)) => queue.settled = frame.seq,
            Err(_) => queue.failed = true,
        }
        if lost {
            queue.failed = true;
        }
        let next = queue
            .waiting
            .first()
            .and_then(|pending| pending.seal.clone());
        let next = next.or_else(|| queue.open.first().map(|(seal, _)| seal.clone()));
        if queue.failed {
            seals.extend(
                queue
                    .waiting
                    .iter()
                    .filter_map(|pending| pending.seal.clone())
                    .map(|seal| (seal, false)),
            );
            seals.extend(queue.open.iter().map(|(seal, _)| (seal.clone(), false)));
        }
        let settled = !queue.failed;
        let upto = queue.settled;
        let ready: Vec<Task> = queue
            .tasks
            .extract_if(.., |task| !settled || task.ticket <= upto)
            .collect();
        let rouse = !queue.waiting.is_empty() || !queue.open.is_empty();
        drop(queue);
        self.done.notify_all();
        for (seal, settled) in seals {
            seal.settled.store(settled, Ordering::Release);
            seal.wake_all();
        }
        for task in ready {
            task.waker.wake();
        }
        if let Some(seal) = next {
            seal.wake_one();
        }
        if rouse {
            self.rouse();
        }
        if let Ok((frame, _)) = &outcome {
            store.checkpoint(*frame);
        }
        if self.automatic {
            let base = self.signal.base.load(Ordering::Relaxed);
            let goal = base.max(self.reclaim.every_bytes);
            if end.saturating_sub(base) >= goal {
                self.signal.raise();
            }
        }
        outcome.map(|_| ())
    }

    fn now(&self) -> u64 {
        read_clock(self.clock.as_ref())
    }

    fn land(
        &self,
        branch: &Branch<T>,
        state: &Arc<State<T>>,
        framed: u64,
        landing: Landing,
    ) -> Result<()> {
        self.step("landing");
        landing.land().inspect_err(|_| {
            branch.tip.store(state.clone());
            branch.framed.store(framed, Ordering::Release);
        })?;
        self.step("landed");
        Ok(())
    }

    fn draft(&self, change: &mut Change<'_, T>) -> Result<Draft<T>> {
        change.branch.check_live()?;
        let mut drafted = write_draft(change.working, self.store())?;
        if change.intent.merged.is_none()
            && change.intent.record.is_none()
            && drafted.store.id() == change.state.store.id()
        {
            drafted.entry = self.draft_entry(change, &mut drafted)?;
        }
        self.step("drafted");
        Ok(drafted)
    }

    fn draft_entry(
        &self,
        change: &mut Change<'_, T>,
        drafted: &mut Draft<T>,
    ) -> Result<Option<(Commit, Chain)>> {
        let state = change.state;
        let head = &state.snapshot.commit;
        if head.root.map_or(self.genesis_hash, |ptr| ptr.hash) == drafted.root.hash {
            return Ok(None);
        }
        let version = head
            .version
            .checked_add(1)
            .ok_or_else(|| Error::Corrupt("the branch has reached the last version".into()))?;
        let intent = &change.intent;
        let commit = Commit {
            version,
            time_ms: intent.time.unwrap_or_else(|| self.now().max(head.floor())),
            message: mem::take(&mut change.message),
            root: Some(drafted.root),
            restored: intent.restored,
            parts: mem::replace(&mut change.parts, Parts::Whole),
            stamped: intent.time.is_some(),
        };
        let (store, txn) = (&drafted.store, &mut drafted.txn);
        let log = self.append_entry(state, store, txn, commit.clone(), None)?;
        Ok(Some((commit, log)))
    }

    fn append_entry(
        &self,
        state: &State<T>,
        store: &Store,
        txn: &mut Txn,
        commit: Commit,
        merged: Option<Merged>,
    ) -> Result<Chain> {
        txn.unshared();
        let log = state.log.append(store, txn, commit, merged)?;
        match self.history {
            Keep::Last(_) => log.pruned(store, self.history),
            _ => Ok(log),
        }
    }

    fn commit_locked(
        &self,
        tip: &mut RefState,
        change: Change<'_, T>,
        drafted: Draft<T>,
    ) -> Result<Placed> {
        let Change {
            branch,
            state,
            working,
            message,
            parts,
            intent,
            seal,
            from,
        } = change;
        branch.check_live()?;
        if let Some(from) = from {
            from.check_live()?;
        }
        let (drafted, template) = match drafted.store.id() == state.store.id() {
            true => (drafted, None),
            false => {
                let template = drafted.entry.map(|(commit, _)| commit);
                (write_draft(working, self.store())?, template)
            }
        };
        let Draft {
            mut txn,
            store,
            mut root,
            mut value,
            object,
            entry,
        } = drafted;
        let previous = state.snapshot.commit.root.map(|ptr| ptr.hash);
        let version = state
            .snapshot
            .commit
            .version
            .checked_add(1)
            .ok_or_else(|| Error::Corrupt("the branch has reached the last version".into()))?;
        let mut merged = tip
            .refs
            .try_get(&branch.name)?
            .map(|head| head.merged.clone())
            .unwrap_or_default();
        let recorded = match intent.record {
            Some((other, theirs)) if merged.try_get(&other)?.map(|seen| seen.0) != Some(theirs) => {
                merged.insert(other, (theirs, version));
                true
            }
            _ => false,
        };
        let head = &state.snapshot.commit;
        if !recorded
            && intent.merged.is_none()
            && previous.unwrap_or(self.genesis_hash) == root.hash
        {
            return Ok((head.version, head.time_ms, None));
        }
        let mut settling = txn.rebase()?;
        let corrupt = || Error::Corrupt("a drafted root links a record its frame lost".into());
        if let Some(moves) = settling.moves() {
            root = moves.ptr(root).ok_or_else(corrupt)?;
            let links = object.links.iter().map(|link| moves.ptr(*link));
            let links = links.collect::<Option<Vec<_>>>().ok_or_else(corrupt)?;
            value = T::decode(&mut Decoder::new(&object.body, &links, Some(&store)))?;
        }
        let (commit, log) = match entry {
            Some((commit, log)) => {
                let tip = match (settling.moves(), log.tip) {
                    (Some(moves), Some(tip)) => Some(moves.ptr(tip).ok_or_else(corrupt)?),
                    (_, tip) => tip,
                };
                let commit = Commit {
                    root: Some(root),
                    ..commit
                };
                (commit, Chain { tip, ..log })
            }
            None => {
                let commit = match template {
                    Some(commit) => Commit {
                        root: Some(root),
                        ..commit
                    },
                    None => {
                        let mut time_ms =
                            intent.time.unwrap_or_else(|| self.now().max(head.floor()));
                        if let Some(link) = intent.merged {
                            time_ms = time_ms.max(Entry::load(&store, link.ptr)?.commit.time_ms);
                        }
                        Commit {
                            version,
                            time_ms,
                            message,
                            root: Some(root),
                            restored: intent.restored,
                            parts,
                            stamped: intent.time.is_some(),
                        }
                    }
                };
                let merged = intent.merged;
                let log = self.append_entry(state, &store, &mut txn, commit.clone(), merged)?;
                (commit, log)
            }
        };
        let time_ms = commit.time_ms;
        let snapshot = Snapshot {
            value: Arc::new(value),
            commit: Arc::new(commit),
        };
        let next = Next::Head(
            branch.name.clone(),
            Head {
                version,
                root: Some(root),
                log,
                merged,
            },
        );
        let state = State {
            snapshot,
            log,
            store: store.clone(),
        };
        let seal = seal.map(|seal| (seal, version, time_ms));
        let landing = self.stage(
            tip,
            next,
            txn,
            Some((branch, state)),
            seal,
            Some(&mut settling),
        )?;
        Ok((version, time_ms, Some((settling, store, landing))))
    }

    fn step(&self, name: &'static str) {
        if let Some(hook) = &self.hook {
            (hook.0)(name);
        }
    }

    fn spawn(shared: &Arc<Self>) -> JoinHandle<()> {
        let weak = Arc::downgrade(shared);
        let signal = shared.signal.clone();
        thread::spawn(move || {
            while signal.wait() {
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                let cycle = AssertUnwindSafe(|| shared.compact(shared.history, Some(&signal.stop)));
                if panic::catch_unwind(cycle).is_err() {
                    shared.log.lock().last_error = Some("reclamation panicked".to_string());
                }
                *signal.due.lock() = false;
            }
        })
    }

    fn prune(&self, keep: Keep, notices: &mut Vec<Notice<T>>) -> Result<u64> {
        if keep == Keep::All {
            return Ok(0);
        }
        let store = self.store();
        self.exclusively(|tip| {
            let mut next = tip.refs.clone();
            let mut dropped = 0;
            for (name, mut head) in tip.refs.try_entries()? {
                let log = head.log.pruned(&store, keep)?;
                if log != head.log {
                    dropped = u64::saturating_add(dropped, (head.log.len() - log.len()) as u64);
                    head.log = log;
                    next.try_insert(name, head)?;
                }
            }
            if dropped == 0 {
                return Ok(0);
            }
            let txn = store.begin()?;
            self.stage(tip, Next::Table(next), txn, None, None, None)?
                .land()?;
            self.settle(store.last_seq(), notices)?;
            let refs = tip.refs.clone();
            for branch in self.branches.lock().values() {
                if let Some(head) = refs.try_get(&branch.name)? {
                    let state = branch.tip.load_full();
                    let next = Arc::new(State {
                        snapshot: state.snapshot.clone(),
                        log: head.log,
                        store: state.store.clone(),
                    });
                    branch.tip.store(next.clone());
                    branch.state.store(next);
                }
            }
            Ok(dropped)
        })
    }

    fn compact(&self, keep: Keep, stop: Option<&AtomicBool>) -> Result<Compacted> {
        if self.options.read_only {
            return Err(Error::ReadOnly);
        }
        let mut notices = Vec::new();
        let _only = self.cycle.lock();
        let store = self.store();
        let outcome = self.run_cycle(&store, keep, stop, &mut notices);
        let mut log = self.log.lock();
        log.cycles += 1;
        match &outcome {
            Ok(report) => {
                log.longest_pause = log.longest_pause.max(report.pause);
                log.last_error = None;
            }
            Err(Error::Interrupted) => {}
            Err(error) => log.last_error = Some(error.to_string()),
        }
        drop(log);
        if outcome.is_err() {
            Copier::abandon(&sibling(&self.file));
            self.signal
                .base
                .store(self.store().size(), Ordering::Relaxed);
        }
        outcome
    }

    fn run_cycle(
        &self,
        store: &Store,
        keep: Keep,
        stop: Option<&AtomicBool>,
        notices: &mut Vec<Notice<T>>,
    ) -> Result<Compacted> {
        let mut report = Compacted {
            before_bytes: store.size(),
            commits_dropped: self.prune(keep, notices)?,
            ..Compacted::default()
        };
        let mut refs = self.published.lock().refs.clone();
        if self.published.lock().ptr.is_none() {
            report.after_bytes = report.before_bytes;
            return Ok(report);
        }
        let temp = sibling(&self.file);
        let mut copier = Copier::create(store, &temp, &self.options, stop)?;
        self.step("created");
        for _ in 0..MAX_PASSES {
            let copied = copier.pass(&refs)?;
            report.passes += 1;
            refs = self.published.lock().refs.clone();
            if copied < QUIET_BYTES {
                break;
            }
        }
        self.step("copied");
        copier.sync()?;
        self.step("synced");
        let paused = Instant::now();
        let (fresh, objects) = self.exclusively(|tip| -> Result<(Store, u64)> {
            self.step("paused");
            self.settle(store.last_seq(), notices)?;
            store.check_healthy()?;
            let basis = store.basis();
            let (head, objects) = {
                let grown = store.size().saturating_sub(basis.bytes).max(1) as f64;
                let survival = copier.end().saturating_sub(basis.bytes) as f64 / grown;
                copier.finish(&tip.refs, store.last_seq(), survival)?
            };
            let (fresh, _) = Store::open_as(&temp, &self.file, self.options.clone())?;
            let swapped = self.swap_in(tip, store, &fresh, &temp, head);
            swapped.inspect_err(|_| fresh.close())?;
            self.signal.base.store(fresh.size(), Ordering::Relaxed);
            Ok((fresh, objects))
        })?;
        report.pause = paused.elapsed();
        self.step("swapped");
        report.after_bytes = fresh.size();
        report.objects = objects;
        let mut retired = self.retired.lock();
        retired.retain(|old| old.bytes().is_some());
        retired.push(store.retired());
        drop(retired);
        store.close();
        self.step("retired");
        Ok(report)
    }

    fn swap_in(
        &self,
        tip: &mut RefState,
        old: &Store,
        fresh: &Store,
        temp: &Path,
        head: Ptr,
    ) -> Result<()> {
        let (refs, depth) = refs::load(fresh, Some(head), None)?;
        let mut published = self.published.lock();
        let branches = self.branches.lock();
        for (_, found) in refs.try_entries()? {
            for ptr in found.root.iter().chain(&found.log.tip) {
                fresh.read_object(*ptr)?;
            }
        }
        let mut states = Vec::new();
        for branch in branches.values() {
            let found = refs.try_get(&branch.name)?;
            states.push((branch, Arc::new(self.state_of(fresh, found.as_deref())?)));
        }
        rename(temp, &self.file)?;
        fresh.trace_rename();
        if let Err(error) = sync_parent(&self.file) {
            old.poison();
            return Err(error.into());
        }
        fresh.trace(Op::SyncDir);
        tip.refs = refs;
        tip.ptr = Some(head);
        tip.depth = depth;
        self.store.store(Arc::new(fresh.clone()));
        *published = tip.clone();
        for (branch, state) in states {
            branch.feed.rebind(state.snapshot.clone());
            branch.tip.store(state.clone());
            branch.state.store(state);
        }
        Ok(())
    }
}

impl<T: Data + Default> Db<T> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Options::new().open(path)
    }
}

impl<T: Data> Db<T> {
    pub fn open_with(path: impl AsRef<Path>, init: impl FnOnce() -> T) -> Result<Self> {
        Options::new().open_with(path, init)
    }

    pub fn path(&self) -> PathBuf {
        self.shared.path.clone()
    }

    pub fn size_on_disk(&self) -> u64 {
        self.shared.store().size()
    }

    pub fn space(&self) -> Result<Space> {
        let store = self.shared.store();
        let file_bytes = store.size();
        let basis = store.basis();
        let live_bytes = if basis.bytes == 0 {
            file_bytes
        } else {
            let grown = file_bytes.saturating_sub(basis.bytes) as f64;
            let survived = (grown * basis.survival()) as u64;
            basis.bytes.saturating_add(survived).min(file_bytes)
        };
        let retired = self.shared.retired.lock();
        let retired_bytes = retired.iter().filter_map(Retired::bytes).sum();
        drop(retired);
        let log = self.shared.log.lock();
        Ok(Space {
            file_bytes,
            live_bytes,
            retired_bytes,
            cycles: log.cycles,
            longest_pause: log.longest_pause,
            last_error: log.last_error.clone(),
        })
    }

    pub fn compact(&self, keep: Keep) -> Result<Compacted> {
        let _crossing = self.entering(Needs::Every)?;
        self.shared.compact(keep, None)
    }

    #[doc(hidden)]
    pub fn trace_op(&self, op: Op) {
        self.shared.store().trace(op);
    }

    pub fn branch_name(&self) -> &str {
        &self.branch.name
    }

    pub fn read(&self) -> Snapshot<T> {
        self.branch.state.load().snapshot.clone()
    }

    pub fn version(&self) -> u64 {
        self.branch.state.load().snapshot.commit.version
    }

    pub fn head_hash(&self) -> Option<Hash> {
        self.branch.state.load().log.tip.map(|ptr| ptr.hash)
    }

    pub fn log(&self) -> Log {
        let state = self.branch.state.load();
        Log {
            chain: state.log,
            store: state.store.clone(),
        }
    }

    pub fn update<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R> {
        self.commit("", f)
    }

    pub fn commit<R>(&self, message: impl Into<String>, f: impl FnOnce(&mut T) -> R) -> Result<R> {
        let message = message.into();
        let done = self.transact::<R, Error>(|root, _| Ok((message, f(root))))?;
        Ok(done.value)
    }

    pub fn commit_with<R, S: Into<String>>(
        &self,
        f: impl FnOnce(&mut T) -> (S, R),
    ) -> Result<Committed<R>> {
        self.transact::<R, Error>(|root, _| {
            let (message, value) = f(root);
            Ok((message.into(), value))
        })
    }

    pub fn commit_stamped<R, S: Into<String>>(
        &self,
        f: impl FnOnce(&mut T, &Stamp<'_>) -> (S, R),
    ) -> Result<Committed<R>> {
        self.transact::<R, Error>(|root, stamp| {
            let (message, value) = f(root, stamp);
            Ok((message.into(), value))
        })
    }

    pub fn try_update<R, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        self.try_commit("", f)
    }

    pub fn try_commit<R, E: From<Error>>(
        &self,
        message: impl Into<String>,
        f: impl FnOnce(&mut T) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        let message = message.into();
        let done = self.transact::<R, E>(|root, _| f(root).map(|value| (message, value)))?;
        Ok(done.value)
    }

    pub fn try_commit_with<R, S: Into<String>, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        self.transact(|root, _| {
            let (message, value) = f(root)?;
            Ok((message.into(), value))
        })
    }

    pub fn try_commit_stamped<R, S: Into<String>, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T, &Stamp<'_>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        self.transact(|root, stamp| {
            let (message, value) = f(root, stamp)?;
            Ok((message.into(), value))
        })
    }

    pub async fn update_async<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R> {
        self.commit_async("", f).await
    }

    pub async fn commit_async<R>(
        &self,
        message: impl Into<String>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R> {
        let message = message.into();
        let done = self
            .writing::<R, Error, _>(|root, _| Ok((message, f(root))))
            .await?;
        Ok(done.value)
    }

    pub async fn commit_with_async<R, S: Into<String>>(
        &self,
        f: impl FnOnce(&mut T) -> (S, R),
    ) -> Result<Committed<R>> {
        self.writing::<R, Error, _>(|root, _| {
            let (message, value) = f(root);
            Ok((message.into(), value))
        })
        .await
    }

    pub async fn try_update_async<R, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        self.try_commit_async("", f).await
    }

    pub async fn try_commit_async<R, E: From<Error>>(
        &self,
        message: impl Into<String>,
        f: impl FnOnce(&mut T) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        let message = message.into();
        let done = self
            .writing(|root, _| f(root).map(|value| (message, value)))
            .await?;
        Ok(done.value)
    }

    pub async fn try_commit_with_async<R, S: Into<String>, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        self.writing(|root, _| {
            let (message, value) = f(root)?;
            Ok((message.into(), value))
        })
        .await
    }

    fn transact<R, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T, &Stamp<'_>) -> std::result::Result<(String, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        let _crossing = self.entering(Needs::Lane(&self.branch.name))?;
        let lane = match self.shared.coalesce {
            true => self.shared.arrive(&self.branch),
            false => self.shared.lane(&self.branch),
        };
        match self.begin(lane, f)? {
            Wait::Ticket(staged) => self.settle(staged),
            Wait::Group(joined) => {
                let (version, time_ms) = self.shared.await_group(&self.branch, &joined.seal)?;
                joined.finish(version, time_ms)
            }
        }
    }

    fn begin<R, E: From<Error>>(
        &self,
        lane: Lane<'_, T>,
        f: impl FnOnce(&mut T, &Stamp<'_>) -> std::result::Result<(String, R), E>,
    ) -> std::result::Result<Wait<R, E>, E> {
        if self.shared.coalesce {
            return self.stage_group(lane, f).map(Wait::Group);
        }
        let staged = self.stage_alone(lane, None, |_, root, stamp| {
            f(root, stamp).map(|(message, value)| (message, value, Intent::default()))
        });
        staged.map(Wait::Ticket)
    }

    fn alone<R, E: From<Error>>(
        &self,
        from: Option<&Branch<T>>,
        f: impl FnOnce(&RefState, &State<T>, &mut T) -> std::result::Result<(String, R, Intent), E>,
    ) -> std::result::Result<Committed<R>, E> {
        let _crossing = self.entering(Needs::Lane(&self.branch.name))?;
        let lane = self.shared.lane(&self.branch);
        let staged = self.stage_alone(lane, from, |state, root, _| {
            let tip = self.shared.tip.lock().clone();
            f(&tip, state, root)
        })?;
        self.settle(staged)
    }

    fn settle<R, E: From<Error>>(
        &self,
        staged: Staged<R, E>,
    ) -> std::result::Result<Committed<R>, E> {
        let mut notices = Vec::new();
        self.shared.settle(staged.ticket, &mut notices)?;
        staged.finish()
    }

    fn stage_alone<R, E: From<Error>>(
        &self,
        lane: Lane<'_, T>,
        from: Option<&Branch<T>>,
        f: impl FnOnce(&State<T>, &mut T, &Stamp<'_>) -> std::result::Result<(String, R, Intent), E>,
    ) -> std::result::Result<Staged<R, E>, E> {
        self.branch.check_live()?;
        self.shared.store().check_healthy()?;
        let state = self.branch.tip.load_full();
        let framed = self.branch.framed.load(Ordering::Acquire);
        let mut working = (*state.snapshot.value).clone();
        let before = state.snapshot.commit.version;
        let time = OnceCell::new();
        let stamp = Stamp {
            version: before.saturating_add(1),
            floor: state.snapshot.commit.floor(),
            time: &time,
            clock: self.shared.clock.as_ref(),
        };
        let ran = f(&state, &mut working, &stamp);
        let result = ran.and_then(|(message, value, mut intent)| {
            intent.time = time.get().copied();
            let mut change = Change {
                branch: &self.branch,
                state: &state,
                working: &working,
                message,
                parts: Parts::Whole,
                intent,
                seal: None,
                from,
            };
            let drafted = self.shared.draft(&mut change)?;
            let mut tip = self.shared.tip.lock();
            let whole = reentry::whole(self.shared.id);
            let committed = self.shared.commit_locked(&mut tip, change, drafted);
            drop(whole);
            drop(tip);
            let (version, time_ms, settling) = committed?;
            if let Some((settling, store, landing)) = settling {
                self.shared.land(&self.branch, &state, framed, landing)?;
                settling.finish(&store);
            }
            Ok((value, version, time_ms))
        });
        let ticket = match &result {
            Ok(_) => self.branch.framed.load(Ordering::Acquire),
            Err(_) => framed,
        };
        drop(lane);
        Ok(Staged {
            result,
            ticket,
            before,
        })
    }

    fn stage_group<R, E: From<Error>>(
        &self,
        mut lane: Lane<'_, T>,
        f: impl FnOnce(&mut T, &Stamp<'_>) -> std::result::Result<(String, R), E>,
    ) -> std::result::Result<Joined<R, E>, E> {
        let shared = &self.shared;
        let branch = &self.branch;
        let live = branch.check_live();
        if let Err(error) = live.and_then(|()| shared.store().check_healthy()) {
            if let Some(group) = lane.take() {
                shared.seal_for_others(branch, group);
            }
            return Err(error.into());
        }
        let mut group = shared.join_group(&mut lane, branch);
        let checkpoint = (group.members > 0).then(|| group.working.clone());
        let before = group.state.snapshot.commit.version;
        let seal = group.seal.clone();
        let timed = group.time.get().is_some();
        let stamp = Stamp {
            version: before.saturating_add(1),
            floor: group.state.snapshot.commit.floor(),
            time: &group.time,
            clock: shared.clock.as_ref(),
        };
        let working = &mut group.working;
        let ran = panic::catch_unwind(AssertUnwindSafe(|| f(working, &stamp)));
        if !matches!(ran, Ok(Ok(_))) {
            group.working = match checkpoint {
                Some(checkpoint) => checkpoint,
                None => (*group.state.snapshot.value).clone(),
            };
            if !timed {
                group.time = OnceCell::new();
            }
        }
        let mut others = Vec::new();
        let result = match ran {
            Err(panicked) => {
                if let Some(group) = shared.pass_on(&mut lane, branch, group, &mut others) {
                    shared.seal_for_others(branch, group);
                }
                drop(lane);
                panic::resume_unwind(panicked);
            }
            Ok(Err(error)) => Err(error),
            Ok(Ok((message, value))) => {
                if !message.is_empty() {
                    group.messages.push(message);
                }
                Ok(value)
            }
        };
        group.members += 1;
        if let Some(group) = shared.pass_on(&mut lane, branch, group, &mut others) {
            shared.seal_group(branch, group);
        }
        drop(lane);
        if !others.is_empty() {
            shared.seal_idle(&others);
        }
        Ok(Joined {
            result,
            seal,
            before,
        })
    }

    fn writing<R, E: From<Error>, F>(&self, f: F) -> Writing<'_, T, F, R, E>
    where
        F: FnOnce(&mut T, &Stamp<'_>) -> std::result::Result<(String, R), E>,
    {
        Writing {
            db: self,
            id: self.shared.ids.fetch_add(1, Ordering::Relaxed),
            start: Some(f),
            gated: false,
            wait: None,
            crossing: None,
        }
    }

    pub(crate) fn genesis(&self) -> Snapshot<T> {
        Snapshot {
            value: self.shared.genesis.clone(),
            commit: Arc::new(Commit::genesis()),
        }
    }

    pub fn at(&self, version: u64) -> Result<Snapshot<T>> {
        let state = self.branch.state.load_full();
        if version == 0 {
            if state.log.first > 1 {
                return Err(Error::NoVersion(0));
            }
            return Ok(self.genesis());
        }
        let entry = state
            .log
            .get(&state.store, version)?
            .ok_or(Error::NoVersion(version))?;
        Self::snapshot_of(&state.store, &entry.commit)
    }

    fn snapshot_of(store: &Store, commit: &Arc<Commit>) -> Result<Snapshot<T>> {
        let root = commit.root.ok_or(Error::NoVersion(commit.version))?;
        Ok(Snapshot {
            value: store.load::<T>(root)?,
            commit: commit.clone(),
        })
    }

    pub fn as_of(&self, time_ms: u64) -> Result<Snapshot<T>> {
        let state = self.branch.state.load_full();
        match state.log.at_time(&state.store, time_ms)? {
            Some(entry) => Self::snapshot_of(&state.store, &entry.commit),
            None if state.log.first > 1 => Err(Error::NoVersion(state.log.first - 1)),
            None => self.at(0),
        }
    }

    pub fn restore(&self, snapshot: &Snapshot<T>) -> Result<Committed<()>> {
        let message = format!("restore v{}", snapshot.commit.version);
        self.restore_with(snapshot, message)
    }

    pub fn restore_with(
        &self,
        snapshot: &Snapshot<T>,
        message: impl Into<String>,
    ) -> Result<Committed<()>> {
        let value = (*snapshot.value).clone();
        let message = message.into();
        let intent = Intent {
            restored: Some(snapshot.commit.version),
            ..Intent::default()
        };
        self.alone::<(), Error>(None, move |_, _, root| {
            *root = value;
            Ok((message, (), intent))
        })
    }

    pub fn history_of<V, F>(&self, project: F) -> History<T, V, F>
    where
        V: PartialEq,
        F: Fn(&T) -> V,
    {
        self.history_of_range(.., project)
    }

    pub fn history_of_range<V, F>(
        &self,
        versions: impl RangeBounds<u64>,
        project: F,
    ) -> History<T, V, F>
    where
        V: PartialEq,
        F: Fn(&T) -> V,
    {
        let state = self.branch.state.load();
        let genesis = (state.log.first == 1).then(|| self.shared.genesis.clone());
        let bounds = state.log.retained(&versions);
        History::new(state.log, bounds, state.store.clone(), genesis, project)
    }

    pub fn bisect(
        &self,
        versions: impl RangeBounds<u64>,
        test: impl Fn(&T) -> bool,
    ) -> Result<Option<u64>> {
        let state = self.branch.state.load_full();
        let Some((mut low, mut high)) = state.log.retained(&versions) else {
            return Ok(None);
        };
        if !test(&*state.root_at(high)?) {
            return Ok(None);
        }
        while low < high {
            let middle = low + (high - low) / 2;
            if test(&*state.root_at(middle)?) {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        Ok(Some(low))
    }

    pub fn bisect_changes<V: PartialEq>(
        &self,
        versions: impl RangeBounds<u64>,
        project: impl Fn(&T) -> V,
    ) -> Result<Vec<(Commit, V)>> {
        let state = self.branch.state.load_full();
        let Some((from, to)) = state.log.retained(&versions) else {
            return Ok(Vec::new());
        };
        let value_at = |version: u64| -> Result<V> { Ok(project(&*state.root_at(version)?)) };
        let mut changed = Vec::new();
        let high = (to, value_at(to)?);
        if from == 1 || from > state.log.first {
            let before = if from == 1 {
                project(&self.shared.genesis)
            } else {
                value_at(from - 1)?
            };
            changes_between((from - 1, &before), (to, &high.1), &value_at, &mut changed)?;
        } else {
            changed.push(from);
            changes_between(
                (from, &value_at(from)?),
                (to, &high.1),
                &value_at,
                &mut changed,
            )?;
        }
        changed
            .into_iter()
            .map(|version| {
                let entry = state.log.get(&state.store, version)?;
                let entry = entry.ok_or(Error::NoVersion(version))?;
                Ok(((*entry.commit).clone(), value_at(version)?))
            })
            .collect()
    }

    pub fn merge_with<R, S: Into<String>>(
        &self,
        other: &Db<T>,
        mut f: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Committed<R>> {
        self.try_merge_with::<R, S, Error>(other, |ours, base, theirs| Ok(f(ours, base, theirs)))
    }

    pub fn try_merge_with<R, S: Into<String>, E: From<Error>>(
        &self,
        other: &Db<T>,
        mut f: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        self.alone(Some(&other.branch), |tip, ours, root| {
            other.branch.check_live()?;
            let theirs = other.branch.state.load_full();
            let store = self.shared.store();
            let found = Self::merge_base(
                &tip.refs,
                &store,
                (&self.branch.name, ours),
                (&other.branch.name, &theirs),
            )?;
            let link = match theirs.log.tip {
                Some(ptr) if !holds(&found, ptr) => Self::link_to(tip, other, &theirs, ptr)?,
                _ => None,
            };
            let base = self.base_of((&tip.refs, &store), found, &mut f)?;
            let (message, value) = f(root, &base, &theirs.snapshot)?;
            let intent = Intent {
                merged: link,
                ..Intent::default()
            };
            Ok((message.into(), value, intent))
        })
    }

    pub(crate) fn merge_tracking<R, S: Into<String>, E: From<Error>>(
        &self,
        other: &Db<T>,
        mut f: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<std::result::Result<Committed<R>, u64>, E> {
        let from = Some(&*other.branch);
        let done = self.alone::<std::result::Result<R, u64>, E>(from, |tip, ours, root| {
            other.branch.check_live()?;
            let theirs = other.branch.state.load_full();
            let store = self.shared.store();
            let found = Self::merge_base(
                &tip.refs,
                &store,
                (&self.branch.name, ours),
                (&other.branch.name, &theirs),
            )?;
            let Some(ptr) = theirs.log.tip.filter(|ptr| !holds(&found, *ptr)) else {
                return Ok((String::new(), Err(theirs.log.last), Intent::default()));
            };
            let link = Self::link_to(tip, other, &theirs, ptr)?;
            let base = self.base_of((&tip.refs, &store), found, &mut f)?;
            let (message, value) = f(root, &base, &theirs.snapshot)?;
            let intent = Intent {
                merged: link,
                record: Some((other.branch.name.clone(), theirs.log.last)),
                ..Intent::default()
            };
            Ok((message.into(), Ok(value), intent))
        })?;
        let (version, time_ms, changed) = (done.version, done.time_ms, done.changed);
        Ok(done.value.map(|value| Committed {
            value,
            version,
            time_ms,
            changed,
        }))
    }

    fn base_of<R, S: Into<String>, E: From<Error>>(
        &self,
        file: (&Refs, &Store),
        found: Vec<Start>,
        f: &mut impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Snapshot<T>, E> {
        let mut found = found.into_iter();
        let Some(first) = found.next() else {
            return Ok(self.genesis());
        };
        let mut base = Self::snapshot_of(&first.store, &first.node.entry.commit)?;
        let mut merged = vec![first];
        for next in found {
            let ours = recorded(file, &merged)?;
            let below = graph::best(ours, recorded(file, std::slice::from_ref(&next))?)?;
            if holds(&below, next.node.ptr) {
                continue;
            }
            let under = self.base_of(file, below, f)?;
            let theirs = Self::snapshot_of(&next.store, &next.node.entry.commit)?;
            let mut working = (*base.value).clone();
            let (message, _) = f(&mut working, &under, &theirs)?;
            let commit = Commit {
                version: 0,
                time_ms: base.commit.time_ms.max(theirs.commit.time_ms),
                message: message.into(),
                root: None,
                restored: None,
                parts: Parts::Whole,
                stamped: false,
            };
            base = Snapshot {
                value: Arc::new(working),
                commit: Arc::new(commit),
            };
            merged.push(next);
        }
        Ok(base)
    }

    fn link_to(
        tip: &RefState,
        other: &Db<T>,
        theirs: &State<T>,
        ptr: Ptr,
    ) -> Result<Option<Merged>> {
        let head = tip.refs.try_get(&other.branch.name)?;
        let floor = head
            .map_or(1, |head| head.log.first)
            .max(theirs.log.first)
            .max(1);
        if floor > theirs.log.last {
            return Ok(None);
        }
        let kept = Chain {
            first: floor,
            ..theirs.log
        };
        let after = kept.after(&theirs.store)?;
        Ok(Some(Merged { ptr, floor, after }))
    }

    pub fn common_ancestor(&self, other: &Db<T>) -> Result<Snapshot<T>> {
        let first = self.common_starts(other)?.into_iter().next();
        match first {
            Some(start) => Self::snapshot_of(&start.store, &start.node.entry.commit),
            None => Ok(self.genesis()),
        }
    }

    pub fn common_ancestors(&self, other: &Db<T>) -> Result<Vec<Snapshot<T>>> {
        let found = self.common_starts(other)?;
        if found.is_empty() {
            return Ok(vec![self.genesis()]);
        }
        found
            .iter()
            .map(|start| Self::snapshot_of(&start.store, &start.node.entry.commit))
            .collect()
    }

    fn common_starts(&self, other: &Db<T>) -> Result<Vec<Start>> {
        let (refs, store, ours, theirs) = {
            let published = self.shared.published.lock();
            let states = (
                self.branch.state.load_full(),
                other.branch.state.load_full(),
            );
            (
                published.refs.clone(),
                self.shared.store(),
                states.0,
                states.1,
            )
        };
        Self::merge_base(
            &refs,
            &store,
            (&self.branch.name, &ours),
            (&other.branch.name, &theirs),
        )
    }

    fn merge_base(
        refs: &Refs,
        store: &Store,
        ours: (&str, &State<T>),
        theirs: (&str, &State<T>),
    ) -> Result<Vec<Start>> {
        if ours.1.log.tip.is_none() || theirs.1.log.tip.is_none() {
            return Ok(Vec::new());
        }
        let starts = |(name, state): (&str, &State<T>)| -> Result<Vec<Start>> {
            let mut starts = Vec::new();
            if let Some(tip) = state.log.tip {
                starts.push(Start {
                    store: state.store.clone(),
                    node: Node::load(&state.store, tip)?,
                    low: state.log.first,
                    after: state.log.after(&state.store)?,
                    bare: false,
                });
            }
            let Some(head) = refs.try_get(name)? else {
                return Ok(starts);
            };
            for (other, seen) in head.merged.try_entries()? {
                let Some(found) = refs.try_get(&other)? else {
                    continue;
                };
                if let Some(node) = found.log.node_at(store, seen.0)? {
                    starts.push(Start {
                        store: store.clone(),
                        node,
                        low: found.log.first,
                        after: found.log.after(store)?,
                        bare: false,
                    });
                }
            }
            Ok(starts)
        };
        graph::best(starts(ours)?, starts(theirs)?)
    }

    pub fn fork(&self, name: &str) -> Result<Db<T>> {
        let _crossing = self.entering(Needs::Lane(&self.branch.name))?;
        let mut notices = Vec::new();
        let lane = self.shared.lane(&self.branch);
        self.branch.check_live()?;
        let mut tip = self.shared.tip.lock();
        if name == MAIN || tip.refs.try_get(name)?.is_some() {
            return Err(Error::BranchExists(name.to_string()));
        }
        let state = self.branch.tip.load_full();
        let commit = &state.snapshot.commit;
        let merged = tip
            .refs
            .try_get(&self.branch.name)?
            .map(|head| head.merged.clone())
            .unwrap_or_default();
        let mut next = tip.refs.clone();
        next.insert(
            name.to_string(),
            Head {
                version: commit.version,
                root: commit.root,
                log: state.log,
                merged,
            },
        );
        let store = self.shared.store();
        let txn = store.begin()?;
        self.shared
            .stage(&mut tip, Next::Table(next), txn, None, None, None)?
            .land()?;
        let ticket = store.last_seq();
        drop(tip);
        drop(lane);
        self.shared.settle(ticket, &mut notices)?;
        self.branch(name)
    }

    pub fn branch(&self, name: &str) -> Result<Db<T>> {
        let _crossing = self.entering(Needs::Tip)?;
        let branch = self.shared.attached(name)?;
        Ok(Db {
            shared: self.shared.clone(),
            branch,
            keeper: self.keeper.clone(),
        })
    }

    pub fn drop_branch(&self, name: &str) -> Result<()> {
        if name == MAIN || name == self.branch.name {
            return Err(Error::BranchInUse(name.to_string()));
        }
        let _crossing = self.entering(Needs::Lane(name))?;
        let mut notices = Vec::new();
        loop {
            let doomed = self.shared.branches.lock().get(name).cloned();
            let lane = doomed.as_ref().map(|branch| self.shared.lane(branch));
            let mut tip = self.shared.tip.lock();
            let _whole = reentry::whole(self.shared.id);
            let attached = self.shared.branches.lock().get(name).cloned();
            if attached.as_ref().map(Arc::as_ptr) != doomed.as_ref().map(Arc::as_ptr) {
                continue;
            }
            if tip.refs.try_get(name)?.is_none() {
                return Err(Error::NoBranch(name.to_string()));
            }
            let store = self.shared.store();
            let txn = store.begin()?;
            let mut next = tip.refs.clone();
            next.remove(name);
            for (branch, mut head) in next.try_entries()? {
                if head.merged.try_get(name)?.is_some() {
                    head.merged.remove(name);
                    next.insert(branch, head);
                }
            }
            self.shared
                .stage(&mut tip, Next::Table(next), txn, None, None, None)?
                .land()?;
            let ticket = store.last_seq();
            self.shared.settle(ticket, &mut notices)?;
            let published = self.shared.published.lock();
            let retired = self.shared.branches.lock().remove(name);
            drop(published);
            if let Some(branch) = &retired {
                branch.retire();
            }
            drop(tip);
            drop(lane);
            return Ok(());
        }
    }

    pub fn branches(&self) -> Vec<String> {
        let refs = self.shared.published.lock();
        let mut names: Vec<String> = refs.refs.keys().collect();
        if !names.iter().any(|name| name == MAIN) {
            names.push(MAIN.to_string());
            names.sort();
        }
        names
    }

    pub fn watch(&self) -> Watcher<T> {
        Watcher::new(self.branch.feed.clone(), Filter::All)
    }

    pub fn watch_when(
        &self,
        test: impl Fn(&Snapshot<T>) -> bool + Send + Sync + 'static,
    ) -> Watcher<T> {
        Watcher::new(self.branch.feed.clone(), Filter::When(Box::new(test)))
    }

    pub fn watch_changes(
        &self,
        test: impl Fn(&Snapshot<T>, &Snapshot<T>) -> bool + Send + Sync + 'static,
    ) -> Watcher<T> {
        Watcher::new(self.branch.feed.clone(), Filter::Changes(Box::new(test)))
    }

    pub fn refresh(&self) -> Result<bool> {
        if !self.shared.options.read_only {
            return Ok(false);
        }
        let _crossing = self.entering(Needs::Tip)?;
        let mut notices = Vec::new();
        let mut tip = self.shared.tip.lock();
        let _whole = reentry::whole(self.shared.id);
        let current = self.shared.store();
        let (store, head) = match current.refresh() {
            Err(Error::Replaced) => Store::open(&self.shared.file, self.shared.options.clone())?,
            other => (current.clone(), other?),
        };
        let replaced = store.id() != current.id();
        if !replaced && head == tip.ptr {
            return Ok(false);
        }
        let Some(ptr) = head else {
            return Ok(false);
        };
        let since = tip
            .ptr
            .filter(|_| !replaced)
            .map(|known| (known, &tip.refs, tip.depth));
        let (refs, depth) = refs::load(&store, Some(ptr), since).inspect_err(|_| {
            if replaced {
                store.close();
            }
        })?;
        let mut published = self.shared.published.lock();
        let attached: Vec<_> = self.shared.branches.lock().values().cloned().collect();
        let mut states = Vec::new();
        let mut gone = Vec::new();
        for branch in attached {
            let head = refs.try_get(&branch.name)?;
            if head.is_none() && branch.name != MAIN {
                gone.push(branch.name.clone());
                continue;
            }
            let state = Arc::new(self.shared.state_of(&store, head.as_deref())?);
            states.push((branch, state));
        }
        tip.refs = refs;
        tip.ptr = Some(ptr);
        tip.depth = depth;
        if replaced {
            self.shared.store.store(Arc::new(store.clone()));
        }
        *published = tip.clone();
        for (branch, state) in states {
            branch.tip.store(state.clone());
            if state.snapshot.commit == branch.state.load().snapshot.commit {
                branch.feed.rebind(state.snapshot.clone());
                branch.state.store(state);
            } else {
                notices.push(branch.publish(state));
            }
        }
        let mut branches = self.shared.branches.lock();
        for name in gone {
            if let Some(branch) = branches.remove(&name) {
                branch.retire();
            }
        }
        drop(branches);
        drop(published);
        if replaced {
            current.close();
        }
        Ok(true)
    }

    pub fn verify(&self) -> Result<Verified> {
        let (head, store) = {
            let published = self.shared.published.lock();
            (published.ptr, self.shared.store())
        };
        crate::admin::verified(&store, head)
    }

    pub fn verify_typed(&self) -> Result<Verified> {
        let (head, refs, store) = {
            let published = self.shared.published.lock();
            (published.ptr, published.refs.clone(), self.shared.store())
        };
        let report = crate::admin::verified(&store, head)?;
        crate::admin::typed::<T>(&store, head, &refs)?;
        Ok(report)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Verified {
    pub objects: u64,
    pub bytes: u64,
    pub file_bytes: u64,
}

impl<T: Data> Db<T> {
    pub(crate) fn entering(&self, needs: Needs<'_>) -> Result<Option<Crossing>> {
        reentry::check(self.shared.id, self.shared.coalesce, needs)
    }

    pub(crate) fn published_refs(&self) -> (Store, Refs) {
        let published = self.shared.published.lock();
        (self.shared.store(), published.refs.clone())
    }

    pub(crate) fn step(&self, name: &'static str) {
        self.shared.step(name);
    }

    pub(crate) fn check_writable(&self) -> Result<()> {
        self.branch.check_live()?;
        let store = self.shared.store();
        if store.is_read_only() {
            return Err(Error::ReadOnly);
        }
        store.check_healthy()
    }

    pub(crate) fn hold_cycles(&self) -> MutexGuard<'_, ()> {
        self.shared.cycle.lock()
    }

    pub(crate) fn append_frame<X>(
        &self,
        fill: impl FnOnce(&Store, &mut Txn) -> Result<X>,
    ) -> Result<X> {
        let shared = &self.shared;
        let mut tip = shared.tip.lock();
        let _whole = reentry::whole(shared.id);
        let store = shared.store();
        let mut txn = store.begin()?;
        let out = fill(&store, &mut txn)?;
        txn.spill()?;
        txn.unreserved();
        match tip.ptr {
            Some(head) => {
                let frame = txn.write(head)?;
                shared.queue.lock().waiting.push(Pending {
                    frame,
                    refs: tip.clone(),
                    update: None,
                    seal: None,
                });
            }
            None => {
                let refs = tip.refs.clone();
                shared
                    .stage(&mut tip, Next::Table(refs), txn, None, None, None)?
                    .land()?;
            }
        }
        Ok(out)
    }

    pub(crate) fn rewrite_refs<X>(
        &self,
        edit: impl FnOnce(&Store, &mut Txn, &Refs) -> Result<(Option<Refs>, X)>,
    ) -> Result<X> {
        let shared = &self.shared;
        let mut notices = Vec::new();
        shared.exclusively(|tip| {
            self.branch.check_live()?;
            let store = shared.store();
            let mut txn = store.begin()?;
            let (next, out) = edit(&store, &mut txn, &tip.refs)?;
            let Some(next) = next else {
                return Ok(out);
            };
            let before = tip.refs.clone();
            shared
                .stage(tip, Next::Table(next), txn, None, None, None)?
                .land()?;
            shared.settle(store.last_seq(), &mut notices)?;
            let attached: Vec<_> = shared.branches.lock().values().cloned().collect();
            let mut states = Vec::new();
            for branch in attached {
                let old = before.try_get(&branch.name)?;
                let new = tip.refs.try_get(&branch.name)?;
                let moved = match (&old, &new) {
                    (Some(old), Some(new)) => {
                        old.log != new.log
                            || old.root.map(|ptr| ptr.hash) != new.root.map(|ptr| ptr.hash)
                    }
                    (None, None) => false,
                    _ => true,
                };
                if moved {
                    states.push((branch, Arc::new(shared.state_of(&store, new.as_deref())?)));
                }
            }
            for (branch, state) in states {
                branch.tip.store(state.clone());
                notices.push(branch.publish(state));
            }
            Ok(out)
        })
    }
}
