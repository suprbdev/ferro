//! Opening a database, reading it, writing it and branching it.
//!
//! A [`Db`] is a handle on one branch of an open file, and [`Options`] opens one with settings
//! other than the defaults. [`Db::read`] returns a [`Snapshot`] of the branch's newest version. The
//! commit methods run a closure over a working copy of it and publish what changed as the next
//! version, which a [`Commit`] describes; the `_with` and `_stamped` forms return a [`Committed`]
//! and hand the closure a [`Stamp`]. The same handle reads old versions, forks, merges, lists and
//! drops branches, and makes [`Watcher`]s. Sync between files is in [`sync`](crate::sync), the
//! lazy walks over history in [`history`](crate::history), and the commit log in
//! [`log`](crate::log).

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet, VecDeque};
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
use crate::follow::{Follower, Following};
use crate::graph::{self, Start};
use crate::history::History;
use crate::log::{Chain, Entry, Log, Merged, Node};
use crate::map::Map;
use crate::platform::canonicalize;
use crate::reclaim::{Compacted, Copier, Reclaim, Space, rename, sibling, sync_parent};
use crate::reentry::{self, Crossing, Needs};
use crate::refs;
use crate::store::{
    Durability, Followed, Frame, Held, Landing, News, Op, Retired, Settling, Store, StoreOptions,
    Trace, Txn, lane_of,
};
use crate::watch::{Feed, Filter, Notice, Watcher};

const MAIN: &str = "main";
const MAX_PASSES: u32 = 6;
const QUIET_BYTES: u64 = 256 << 10;
const MAX_GROUP: usize = 256;
const LOCK_POLL: Duration = Duration::from_millis(10);

/// One version of a branch: its number, time and message, and the value it holds.
///
/// [`Snapshot::commit`], [`Db::log`] and [`Db::history_of`] hand these out. Equality compares the
/// root by hash, so the same commit read from two files that synced it compares equal.
#[derive(Clone, Debug)]
pub struct Commit {
    /// The version on its branch, counting from 1.
    ///
    /// Version 0, the value before the first commit, has no commit of its own and reads as one with
    /// this field 0, an empty message and no root.
    pub version: u64,
    /// When the commit was made, in milliseconds since the Unix epoch.
    ///
    /// Times never go back along a branch: a commit's time is the later of the clock and a floor
    /// set by the commit before it, as
    /// [History](https://github.com/archlemon/ferro/blob/main/docs/guide/history.md#reading-old-versions)
    /// describes.
    pub time_ms: u64,
    /// The message given to the commit.
    ///
    /// A commit that [`Options::coalesce`] made of several closures holds their messages joined by
    /// newlines, and [`Commit::messages`] separates them again.
    pub message: String,
    /// The stored root value, or `None` for version 0 and for the virtual base of a merge after a
    /// criss-cross.
    pub root: Option<Ptr>,
    /// The version this commit restored, if [`Db::restore`] made it.
    pub restored: Option<u64>,
    pub(crate) parts: Parts,
    pub(crate) stamped: bool,
}

/// How a commit's message divides into the messages of the closures that made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Parts {
    /// The log entry does not record it, so the message splits at newlines.
    Lines,
    /// The message is one closure's, whole.
    Whole,
    /// The message joins messages of these byte lengths with newlines.
    Joined(Arc<[usize]>),
}

impl Parts {
    /// Joins `messages` with newlines, recording their lengths when there are two or more.
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

    /// Checks that joined lengths cut `message` at newlines, on character boundaries, into at least
    /// two parts.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] if they do not, as for a damaged log entry.
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
    /// The messages of the closures this commit holds, in the order they ran.
    ///
    /// A commit of one closure yields its message whole, or nothing if it is empty. A commit that
    /// [`Options::coalesce`] made of several yields each closure's non-empty message, whole even if
    /// it holds newlines of its own. A commit whose log entry does not record how its message was
    /// joined yields the message's non-empty lines.
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

/// What a write's closure returned, and the commit that holds its change.
///
/// [`Db::commit_with`], [`Db::commit_stamped`], their `try_` and `_async` forms, the merges and
/// [`Db::restore`] return one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed<R> {
    /// The value the closure returned.
    pub value: R,
    /// The version of the commit that holds the change, or of the head the closure ran on if it
    /// changed nothing.
    pub version: u64,
    /// The time of that commit, in milliseconds since the Unix epoch.
    pub time_ms: u64,
    /// Whether the write made a new version.
    ///
    /// With [`Options::coalesce`] it says whether the closure's group made one, so a closure that
    /// changed nothing reads `true` when another closure in its group changed something.
    pub changed: bool,
}

/// The version and time of the commit a closure is writing.
///
/// [`Db::commit_stamped`] and its `try_` and `_async` forms hand one to the closure, so that a row
/// can record the commit that writes it. A stamp describes the commit that would hold the
/// closure's changes: if the closure returns `Err`, panics or changes nothing, no commit holds
/// them, and the stamp claims nothing.
#[derive(Debug)]
pub struct Stamp<'a> {
    version: u64,
    floor: u64,
    time: &'a OnceCell<u64>,
    clock: Option<&'a Clock>,
}

impl Stamp<'_> {
    /// The version the commit will have, one past the head the closure runs on.
    ///
    /// [`Db::at`] of it later shows the state the closure wrote. With [`Options::coalesce`] it is
    /// the group's version, the same for every closure in the group, so that state includes the
    /// closures that ran after this one.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The commit's time, in milliseconds since the Unix epoch.
    ///
    /// The first call reads the clock and fixes the time, no earlier than the commit before it
    /// allows, and later calls return the same. No later commit then shares that millisecond: the
    /// next commit on the branch gets a later time, so [`Db::as_of`] of this time returns exactly
    /// this commit for as long as it is retained. An earlier commit can share it, if the clock has
    /// not moved on since that commit, and `as_of` of that earlier commit's time then returns this
    /// one. A branch that reads the time in more than 1,000 commits a second runs ahead of the
    /// clock until the rate drops, so read it only when it is needed.
    ///
    /// With [`Options::coalesce`] the first closure of a group to call it fixes the group's time,
    /// and if that closure fails, the next one to call it fixes it again.
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

/// A branch's entry in the branch table.
#[derive(Clone)]
pub(crate) struct Head {
    /// The version of the branch's newest commit.
    pub(crate) version: u64,
    /// The root value of that commit, or `None` before the first.
    pub(crate) root: Option<Ptr>,
    /// The branch's retained commit log.
    pub(crate) log: Chain,
    /// For each branch merged into this one by name, the version of it last merged and this
    /// branch's version when it was.
    pub(crate) merged: Map<String, (u64, u64)>,
    /// The sequence number after the newest frame when the branch was created, which tells a
    /// branch from a later one of the same name, or 0 for `main` and for a head encoded without
    /// one.
    pub(crate) born: u64,
}

const BORN: u8 = 2;

impl Head {
    /// The [`Head::born`] of a branch that the next frames placed in `store` create.
    ///
    /// The caller holds the `tip` mutex, under which every frame is placed, and with
    /// [`Options::shared`] has caught up under the placement lock, so the number is past that of
    /// every frame that dropped a branch of the same name.
    pub(crate) fn born_next(store: &Store) -> u64 {
        store.last_seq().saturating_add(1)
    }
}

fn encode_link(w: &mut Encoder<'_>, link: Option<Ptr>, flags: u8) {
    w.u8(u8::from(link.is_some()) | flags);
    if let Some(ptr) = link {
        w.link(ptr);
    }
}

impl Data for Head {
    const LOCATED: bool = true;

    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.version.encode(w)?;
        self.log.first.encode(w)?;
        encode_link(w, self.root, if self.born == 0 { 0 } else { BORN });
        encode_link(w, self.log.tip, 0);
        self.merged.encode(w)?;
        if self.born != 0 {
            self.born.encode(w)?;
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let version = u64::decode(r)?;
        let first = u64::decode(r)?;
        let flags = r.u8()?;
        if flags & !(1 | BORN) != 0 {
            return Err(Error::Decode(format!("invalid branch head flags {flags}")));
        }
        let root = match flags & 1 {
            0 => None,
            _ => Some(r.link()?),
        };
        let tip = Option::<Ptr>::decode(r)?;
        let merged = Map::decode(r)?;
        let born = match flags & BORN {
            0 => 0,
            _ => match u64::decode(r)? {
                0 => {
                    return Err(Error::Decode(
                        "a branch head flags the frame that created it and names none".into(),
                    ));
                }
                born => born,
            },
        };
        if tip.is_none() && version != 0 {
            return Err(Error::Corrupt(format!(
                "a branch with no commit names v{version} as its last"
            )));
        }
        if tip.is_none() && root.is_some() {
            return Err(Error::Corrupt(
                "a branch with no commit names a root".into(),
            ));
        }
        Ok(Head {
            version,
            root,
            log: Chain::bounded(tip, first, version)?,
            merged,
            born,
        })
    }
}

/// The branch table: every branch's [`Head`] by name.
pub(crate) type Refs = Map<String, Head>;

#[derive(Clone)]
struct RefState {
    refs: Refs,
    ptr: Option<Ptr>,
    depth: usize,
}

struct Byte {
    store: Store,
    at: u64,
}

impl Drop for Byte {
    fn drop(&mut self) {
        self.store.unlock_lane(self.at);
    }
}

/// The reclamation cycle locks of this process and, with [`Options::shared`], of the file, held
/// so that no cycle starts until it is dropped.
///
/// The thread records that it holds them, so that a call from it that would wait for them fails
/// with [`Error::Reentrant`]. The states that the catch-up made on taking them replaced are freed
/// once they are released.
pub(crate) struct Cycles<'a, T> {
    _across: Option<Held>,
    _local: MutexGuard<'a, ()>,
    _held: reentry::Cycle,
    _freed: Vec<Notice<T>>,
}

struct Tip<'a> {
    commit: Option<Held>,
    guard: MutexGuard<'a, RefState>,
}

impl Deref for Tip<'_> {
    type Target = RefState;

    fn deref(&self) -> &RefState {
        &self.guard
    }
}

impl DerefMut for Tip<'_> {
    fn deref_mut(&mut self) -> &mut RefState {
        &mut self.guard
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Caught {
    Still,
    Moved,
    Swapped,
}

fn moved(old: Option<&Head>, new: Option<&Head>) -> bool {
    match (old, new) {
        (Some(old), Some(new)) => {
            old.born != new.born
                || old.log != new.log
                || old.root.map(|ptr| ptr.hash) != new.root.map(|ptr| ptr.hash)
        }
        (None, None) => false,
        _ => true,
    }
}

enum Next {
    Table(Refs),
    Head(String, Head),
}

/// An immutable view of one version of a branch.
///
/// It derefs to `T`, never changes, and stays valid for as long as it is held, whatever is
/// committed, pruned or reclaimed meanwhile. Cloning one is cheap and shares the value. A snapshot
/// keeps the file it was read from open, though not the writer lock, so the space of a file that a
/// reclamation cycle replaced comes back only once the last snapshot of it is dropped.
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
    /// The version this snapshot shows, 0 for the value before the first commit.
    pub fn version(&self) -> u64 {
        self.commit.version
    }

    /// The commit this snapshot shows.
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

struct Slot<T> {
    group: Option<Group<T>>,
    crossed: Option<Store>,
}

struct Branch<T> {
    name: String,
    born: u64,
    db: u64,
    lane_at: u64,
    state: ArcSwap<State<T>>,
    tip: ArcSwap<State<T>>,
    feed: Arc<Feed<T>>,
    retired: AtomicBool,
    lane: Mutex<Slot<T>>,
    arriving: AtomicUsize,
    gate: Mutex<VecDeque<(u64, Waker)>>,
    gated: AtomicUsize,
    framed: AtomicU64,
}

struct Lane<'a, T> {
    guard: Option<MutexGuard<'a, Slot<T>>>,
    branch: &'a Branch<T>,
}

impl<T> Lane<'_, T> {
    fn crossed(&self) -> bool {
        self.guard
            .as_ref()
            .is_some_and(|slot| slot.crossed.is_some())
    }

    fn cross(&mut self, store: Store) {
        if let Some(slot) = self.guard.as_mut() {
            slot.crossed = Some(store);
        }
    }

    fn uncross(&mut self) {
        if let Some(store) = self.guard.as_mut().and_then(|slot| slot.crossed.take()) {
            store.unlock_lane(self.branch.lane_at);
        }
    }
}

impl<T> Deref for Lane<'_, T> {
    type Target = Option<Group<T>>;

    fn deref(&self) -> &Option<Group<T>> {
        &self
            .guard
            .as_ref()
            .expect("ferro: the branch lock is held")
            .group
    }
}

impl<T> DerefMut for Lane<'_, T> {
    fn deref_mut(&mut self) -> &mut Option<Group<T>> {
        &mut self
            .guard
            .as_mut()
            .expect("ferro: the branch lock is held")
            .group
    }
}

impl<T> Drop for Lane<'_, T> {
    fn drop(&mut self) {
        if let Some(slot) = self.guard.as_mut()
            && slot.group.is_none()
            && let Some(store) = slot.crossed.take()
        {
            store.unlock_lane(self.branch.lane_at);
        }
        drop(self.guard.take());
        reentry::release(self.branch.db, self.branch.key());
        self.branch.open_gate();
    }
}

impl<T> Branch<T> {
    fn new(name: &str, born: u64, db: u64, state: Arc<State<T>>) -> Branch<T> {
        Branch {
            name: name.to_string(),
            born,
            db,
            lane_at: lane_of(name),
            feed: Arc::new(Feed::new(state.snapshot.clone())),
            state: ArcSwap::new(state.clone()),
            tip: ArcSwap::new(state),
            retired: AtomicBool::new(false),
            lane: Mutex::new(Slot {
                group: None,
                crossed: None,
            }),
            arriving: AtomicUsize::new(0),
            gate: Mutex::new(VecDeque::new()),
            gated: AtomicUsize::new(0),
            framed: AtomicU64::new(0),
        }
    }

    fn key(&self) -> usize {
        std::ptr::from_ref(self).addr()
    }

    fn hold<'a>(&'a self, guard: MutexGuard<'a, Slot<T>>) -> Lane<'a, T> {
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
        let replaced = self.state.swap(state);
        if replaced.snapshot.commit == snapshot.commit {
            return self.feed.rebind(snapshot);
        }
        self.feed.publish(snapshot, &replaced.snapshot)
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

    fn dropped_in(&self, head: Option<&Head>) -> bool {
        self.name != MAIN && head.is_none_or(|head| head.born != self.born)
    }
}

impl<T> Drop for Branch<T> {
    fn drop(&mut self) {
        self.feed.close();
    }
}

type Moved<T> = (Arc<Branch<T>>, Arc<State<T>>);

type Update<T> = Option<Moved<T>>;

struct Pending<T> {
    frame: Frame,
    refs: RefState,
    update: Update<T>,
    seal: Option<Arc<Seal>>,
    followed: Vec<Moved<T>>,
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
    follower: Option<Follower>,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        drop(self.follower.take());
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
    lane_waits: Arc<Mutex<HashMap<u64, Vec<Waker>>>>,
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

    fn await_lane(&self, store: Store, at: u64, waker: &Waker) {
        let mut waits = self.lane_waits.lock();
        if let Some(wakers) = waits.get_mut(&at) {
            if !wakers.iter().any(|held| held.will_wake(waker)) {
                wakers.push(waker.clone());
            }
            return;
        }
        waits.insert(at, vec![waker.clone()]);
        drop(waits);
        let waits = self.lane_waits.clone();
        let waiting = waits.clone();
        let spawned = thread::Builder::new()
            .name("ferro-lane".to_string())
            .spawn(move || {
                if store.lock_lane(at).is_ok() {
                    store.unlock_lane(at);
                }
                let woken = waiting.lock().remove(&at).unwrap_or_default();
                woken.into_iter().for_each(Waker::wake);
            });
        if spawned.is_err() {
            let woken = waits.lock().remove(&at).unwrap_or_default();
            woken.into_iter().for_each(Waker::wake);
        }
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
            let Some(mut lane) = db.branch.enter(this.id, cx.waker(), this.gated) else {
                this.start = Some(f);
                this.gated = true;
                return Poll::Pending;
            };
            this.gated = false;
            let mut notices = Vec::new();
            match db.shared.crossing(&mut lane, false, &mut notices) {
                Err(error) => return Poll::Ready(Err(error.into())),
                Ok(Some(busy)) => {
                    let at = lane.branch.lane_at;
                    drop(lane);
                    db.shared.await_lane(busy, at, cx.waker());
                    this.start = Some(f);
                    return Poll::Pending;
                }
                Ok(None) => {}
            }
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

/// A handle on one branch of an open database file.
///
/// `T` is the value the database holds. [`Db::open`] opens or creates a file with the default
/// [`Options`] and returns a handle on its `main` branch, and [`Db::branch`] and [`Db::fork`]
/// return handles on other branches of the same file. Cloning a handle is cheap, and the handles
/// that come from one open share its cache, its writer lock per branch and its sync, so clones are
/// how threads share a database.
///
/// Readers never wait. Writers run one closure at a time on each branch, under that branch's
/// writer lock, while closures on different branches run at the same time. A write returns once
/// its commit is published, which under the default [`Durability::Fsync`] means it is on disk.
///
/// Other writers stay locked out until the last handle on the file is dropped. Dropping it stops
/// the background reclamation thread, interrupting a cycle in progress, and writes and syncs what
/// dropped awaited writes left queued. Snapshots held after that keep reading the file.
///
/// # Examples
///
/// ```
/// use ferro::{Data, Db, Map};
///
/// #[derive(Data, Clone, Default)]
/// struct Site {
///     hits: Map<String, u64>,
/// }
///
/// # fn main() -> ferro::Result<()> {
/// # let dir = tempfile::tempdir()?;
/// # let path = dir.path().join("site.ferro");
/// let db = Db::<Site>::open(&path)?;
/// db.commit("first hit", |site| {
///     site.hits.insert("/".to_string(), 1);
/// })?;
///
/// let before = db.read();
/// db.update(|site| {
///     if let Some(hits) = site.hits.get_mut("/") {
///         *hits += 1;
///     }
/// })?;
///
/// assert_eq!(before.hits.get("/").as_deref(), Some(&1));
/// assert_eq!(db.read().hits.get("/").as_deref(), Some(&2));
/// assert_eq!(db.version(), 2);
/// # Ok(())
/// # }
/// ```
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

/// A callback that commits, cycles and sync call at named steps, for tests that inject faults
/// between them.
#[doc(hidden)]
#[derive(Clone)]
pub struct Hook(
    /// The callback, given the step's name.
    pub Arc<dyn Fn(&'static str) + Send + Sync>,
);

impl fmt::Debug for Hook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Hook")
    }
}

/// A source of commit times that replaces the system clock, for tests.
#[doc(hidden)]
#[derive(Clone)]
pub struct Clock(
    /// Returns the time in milliseconds since the Unix epoch.
    pub Arc<dyn Fn() -> u64 + Send + Sync>,
);

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Clock")
    }
}

fn read_clock(clock: Option<&Clock>) -> u64 {
    clock.map_or_else(now_ms, |clock| (clock.0)())
}

/// Settings for opening a database, set by chaining.
///
/// [`Options::new`] starts from the defaults that [`Db::open`] uses, each method changes one
/// setting, and [`Options::open`] or [`Options::open_with`] opens the file.
/// [Getting started](https://github.com/archlemon/ferro/blob/main/docs/guide/getting-started.md#options)
/// lists the defaults in a table.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
///
/// use ferro::{Durability, Keep, Map, Options};
///
/// # fn main() -> ferro::Result<()> {
/// # let dir = tempfile::tempdir()?;
/// # let path = dir.path().join("jobs.ferro");
/// let db = Options::new()
///     .durability(Durability::Buffered)
///     .history(Keep::Last(1000))
///     .lock_wait(Duration::from_secs(1))
///     .open::<Map<u64, String>>(&path)?;
///
/// db.update(|jobs| {
///     jobs.insert(1, "send the invoice".to_string());
/// })?;
/// assert_eq!(db.read().len(), 1);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, Default)]
pub struct Options {
    store: StoreOptions,
    history: Keep,
    reclaim: Reclaim,
    coalesce: bool,
    lock_wait: Duration,
    follow: bool,
    follow_poll: Option<Duration>,
    hook: Option<Hook>,
    clock: Option<Clock>,
}

impl Options {
    /// The defaults: an fsync for every commit, a 1 GiB cache, all history kept, one writer
    /// process, no coalescing, no follower, and no wait for the writer lock.
    pub fn new() -> Self {
        Options::default()
    }

    /// How each commit reaches the disk.
    ///
    /// The default, [`Durability::Fsync`], syncs each commit before acknowledging it, with one
    /// fsync covering every commit queued meanwhile. [`Durability::Buffered`] skips that sync, so a
    /// power cut can lose recent commits, back to the last checkpoint at worst, but never the whole
    /// file. See
    /// [Durability and crash safety](https://github.com/archlemon/ferro/blob/main/docs/guide/durability.md).
    pub fn durability(mut self, durability: Durability) -> Self {
        self.store.durability = durability;
        self
    }

    /// An approximate bound, in bytes, on the decoded nodes and values the cache holds.
    ///
    /// The default is 1 GiB.
    /// [Cache weight](https://github.com/archlemon/ferro/blob/main/docs/internals/read-scaling.md#cache-weight)
    /// describes what counts against it.
    pub fn cache_bytes(mut self, bytes: u64) -> Self {
        self.store.cache_bytes = bytes;
        self
    }

    /// Under [`Durability::Buffered`], how many bytes of log a checkpoint covers.
    ///
    /// A checkpoint is an fsync, taken once this much log has been written since the last one and
    /// when the database closes cleanly, so it bounds what a power cut can lose. The default is 64
    /// MiB.
    pub fn checkpoint_bytes(mut self, bytes: u64) -> Self {
        self.store.checkpoint_bytes = bytes;
        self
    }

    /// Opens the file without the writer lock, to read while another process writes.
    ///
    /// Every write then fails with [`Error::ReadOnly`], and [`Db::refresh`] picks up what the
    /// writer committed since, as [`Options::follow`] does on its own. Opening a missing file
    /// fails with [`Error::NoDatabase`] instead of creating it. A read-only handle runs no
    /// reclamation and ignores [`Options::shared`] and [`Options::coalesce`].
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.store.read_only = read_only;
        self
    }

    /// How much history each branch retains, [`Keep::All`] by default.
    ///
    /// [`Keep::Last`] prunes as part of each commit, while [`Keep::Since`] and [`Keep::Within`]
    /// take effect at each reclamation cycle. A writer that keeps less than everything also runs
    /// cycles on a background thread, as [`Options::reclaim`] sets, to give the space back. A
    /// pruned version is gone, whether or not a cycle has reclaimed its bytes yet. See
    /// [Retention and reclamation](https://github.com/archlemon/ferro/blob/main/docs/guide/retention.md).
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Keep, Options};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("count.ferro");
    /// let db = Options::new().history(Keep::Last(2)).open::<u64>(&path)?;
    /// for count in 1..=5 {
    ///     db.update(|value| *value = count)?;
    /// }
    ///
    /// assert_eq!(db.log().len(), 2);
    /// assert!(db.at(3).is_err());
    /// assert_eq!(*db.at(4)?, 4);
    /// # Ok(())
    /// # }
    /// ```
    pub fn history(mut self, keep: Keep) -> Self {
        self.history = keep;
        self
    }

    /// When background reclamation cycles run.
    ///
    /// A cycle runs once the file has grown by [`Reclaim::every_bytes`], or by its size after the
    /// last cycle if that is larger. Cycles run only on a writer whose [`Options::history`] is not
    /// [`Keep::All`], and `every_bytes: 0` turns them off.
    pub fn reclaim(mut self, reclaim: Reclaim) -> Self {
        self.reclaim = reclaim;
        self
    }

    /// How many threads a reclamation cycle copies and checks on, counting the one it runs on.
    ///
    /// Zero, the default, means the number of cores up to 8, and 1 keeps a cycle on one thread.
    pub fn copy_threads(mut self, threads: usize) -> Self {
        self.store.copy_threads = threads;
        self
    }

    /// Lets closures that queue up on one branch while a sync runs share one version.
    ///
    /// They still run one at a time, each on its caller's thread and each seeing what the ones
    /// before it did, and an `Err` or a panic rolls back that closure alone. They share one write
    /// of every tree path they touched and one commit, so [`Committed::version`] and
    /// [`Stamp::version`] are the group's, [`Db::at`] cannot show the state between two of them,
    /// and if the group cannot be written every closure in it gets [`Error::GroupFailed`].
    /// [`Db::restore`] and the merges always commit alone. Off by default. See
    /// [Sharing a version between writers](https://github.com/archlemon/ferro/blob/main/docs/guide/concurrency.md#sharing-a-version-between-writers).
    pub fn coalesce(mut self, coalesce: bool) -> Self {
        self.coalesce = coalesce;
        self
    }

    /// Lets several processes write to the file, if every one of them opens it with this.
    ///
    /// Each commit lands on top of the newest one from any process, and a closure runs under a
    /// lock on its branch that holds across processes, so it starts from the newest commit on its
    /// branch. A process sees the others' commits when it next commits, attaches a branch with
    /// [`Db::branch`] or calls [`Db::refresh`], or, with [`Options::follow`], soon after they are
    /// acknowledged. A writer without the option and shared writers lock each other out with
    /// [`Error::Locked`].
    ///
    /// It needs open file description locks, so it works on 64-bit Linux only, on one machine and a
    /// local file system. Elsewhere the open fails with an
    /// [`io::ErrorKind::Unsupported`](std::io::ErrorKind::Unsupported) error. See
    /// [Several processes](https://github.com/archlemon/ferro/blob/main/docs/guide/concurrency.md#several-processes).
    pub fn shared(mut self, shared: bool) -> Self {
        self.store.shared = shared;
        self
    }

    /// How long [`Options::open`] waits for another writer to close the file before it fails with
    /// [`Error::Locked`].
    ///
    /// It checks every 10 ms. Zero, the default, fails at once.
    pub fn lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    /// Follows the file on a thread of its own, so that other processes' commits reach this
    /// handle without a call to [`Db::refresh`].
    ///
    /// A read-only handle, or a writer opened with [`Options::shared`], then picks up each commit
    /// another process makes soon after it is acknowledged: [`Db::read`] shows it and the watchers
    /// of every branch it moved fire, also after a reclamation cycle replaced the file. On 64-bit
    /// Linux inotify wakes the thread when the file changes, and under a busy writer it refreshes
    /// at most once a millisecond; elsewhere, or where inotify cannot be had, it looks every
    /// 10 ms. A look reads the newest superblock and refreshes only when that names a commit the
    /// handle has not seen. On a shared writer it catches up only to what that superblock names,
    /// and waits for a lock another process holds only to open the file a cycle renamed, to sync
    /// its own process's commits, or, under [`Durability::Fsync`], to sync a commit that a process
    /// using [`Durability::Buffered`] named without syncing it. Under `Buffered` it syncs the file
    /// once every [`Options::checkpoint_bytes`] it follows, as every writer does. A writer without
    /// [`Options::shared`] is the only one and starts no thread. Off by default.
    ///
    /// The thread ends when the last [`Db`] handle on the file is dropped, which waits for a look
    /// in progress, and keeps neither the file nor a lock. A look that fails, as when the file was
    /// removed or is damaged, changes nothing: reads show what was seen last, the thread looks
    /// again at the next change, and [`Db::following`] reports the error. A shared writer whose
    /// writes failed reports [`Error::Poisoned`] and stops following. See
    /// [Following other processes](https://github.com/archlemon/ferro/blob/main/docs/guide/concurrency.md#following-other-processes).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use ferro::{Db, Options};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("count.ferro");
    /// let writer = Db::<u64>::open(&path)?;
    /// let reader = Options::new().read_only(true).follow(true).open::<u64>(&path)?;
    /// let watcher = reader.watch();
    ///
    /// writer.update(|count| *count = 1)?;
    /// let seen = watcher.recv_timeout(Duration::from_secs(10)).expect("the commit arrives");
    /// assert_eq!(*seen, 1);
    /// assert_eq!(*reader.read(), 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn follow(mut self, follow: bool) -> Self {
        self.follow = follow;
        self
    }

    /// Makes a follower look every `every` even where inotify could tell it of changes, for tests
    /// and benchmarks.
    #[doc(hidden)]
    pub fn follow_polling(mut self, every: Duration) -> Self {
        self.follow_poll = Some(every);
        self
    }

    /// Calls `hook` at named steps of commits, cycles and sync, for fault injection in tests.
    #[doc(hidden)]
    pub fn hook(mut self, hook: Hook) -> Self {
        self.hook = Some(hook);
        self
    }

    /// Takes commit times from `clock` instead of the system clock, for tests.
    #[doc(hidden)]
    pub fn clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Records the file operations of the store in `trace`, for crash tests.
    #[doc(hidden)]
    pub fn trace(mut self, trace: Trace) -> Self {
        self.store.trace = Some(trace);
        self
    }

    /// Opens the database at `path` with these options, creating the file if it does not exist.
    ///
    /// The handle is on the `main` branch, and version 0, the value before the first commit, is
    /// `T::default()`. A file that a crash left behind opens at its last complete commit.
    ///
    /// # Errors
    ///
    /// - [`Error::Locked`] if another writer has the file open and does not close it within
    ///   [`Options::lock_wait`].
    /// - [`Error::NoDatabase`] for a read-only open of a missing file, and [`Error::NotFerro`] for
    ///   a file that is not a ferro database.
    /// - [`Error::Corrupt`] or [`Error::Decode`] if what open reads of the file is damaged.
    /// - [`Error::Io`] if the file cannot be created, opened or read, or [`Options::shared`] is not
    ///   supported here.
    pub fn open<T: Data + Default>(self, path: impl AsRef<Path>) -> Result<Db<T>> {
        self.open_with(path, T::default)
    }

    /// Opens the database at `path` like [`Options::open`], with version 0 from `init`.
    ///
    /// Version 0 is not stored in the file, so `init` runs on every open and should give the same
    /// value each time. It is also what a branch with no commits holds.
    ///
    /// # Errors
    ///
    /// As for [`Options::open`].
    pub fn open_with<T: Data>(
        mut self,
        path: impl AsRef<Path>,
        init: impl FnOnce() -> T,
    ) -> Result<Db<T>> {
        let read_only = self.store.read_only;
        self.store.shared &= !read_only;
        let options = self.store.clone();
        let deadline = Instant::now().checked_add(self.lock_wait);
        let (store, head) = loop {
            match Store::open(path.as_ref(), self.store.clone()) {
                Err(Error::Locked) if deadline.is_none_or(|at| Instant::now() < at) => {
                    let left = deadline
                        .map_or(LOCK_POLL, |at| at.saturating_duration_since(Instant::now()));
                    thread::sleep(left.min(LOCK_POLL));
                }
                opened => break opened?,
            }
        };
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
            lane_waits: Arc::default(),
            flush: Shared::<T>::flush,
        });
        let refs = shared.published.lock().refs.clone();
        let branch = shared.attach(MAIN, &refs)?;
        let follower = match self.follow && (read_only || shared.options.shared) {
            true => Some(Shared::follow_file(&shared, self.follow_poll)?),
            false => None,
        };
        let thread = automatic.then(|| Shared::spawn(&shared));
        let keeper = Arc::new(Keeper {
            signal,
            thread: Mutex::new(thread),
            syncer: Mutex::new(None),
            follower,
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

fn free<T>(notices: &mut Vec<Notice<T>>, state: &State<T>) {
    notices.push(Notice::freeing(state.snapshot.clone()));
}

fn rebind<T>(branch: &Branch<T>, state: Arc<State<T>>, notices: &mut Vec<Notice<T>>) {
    free(notices, &branch.tip.swap(state.clone()));
    if state.snapshot.commit == branch.state.load().snapshot.commit {
        notices.push(branch.feed.rebind(state.snapshot.clone()));
        free(notices, &branch.state.swap(state));
    } else {
        free(notices, &branch.state.load());
        notices.push(branch.publish(state));
    }
}

type Tracked<R> = std::result::Result<R, Option<u64>>;

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
                    let at = ours.max(start.low);
                    let under = (at <= version)
                        .then(|| start.node.down_to(&start.store, at))
                        .transpose()?;
                    out.push(Start {
                        store: store.clone(),
                        node,
                        low: found.log.first,
                        after: found.log.after(store)?,
                        bare: false,
                        under,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Whether the history of `theirs` holds a commit that is not an ancestor of `ours` and whose
/// value differs from the value of `ours`, given the merge-base search starts of `ours`.
///
/// It walks down from their head through the commits that hold our value, and stops on each path
/// at a commit of our chain, at the first commit whose value differs, or where history is pruned.
/// The merge-base search then tells whether each commit it stopped at for its value is an
/// ancestor of ours; one it cannot place counts as a change.
fn brings_change<T>(ours: &State<T>, starts: Vec<Start>, theirs: &State<T>) -> Result<bool> {
    let Some(tip) = theirs.log.tip else {
        return Ok(false);
    };
    let root = ours.snapshot.commit.root.map(|ptr| ptr.hash);
    let mut walk = vec![Start {
        store: theirs.store.clone(),
        node: Node::load(&theirs.store, tip)?,
        low: theirs.log.first.max(1),
        after: theirs.log.after(&theirs.store)?,
        bare: false,
        under: None,
    }];
    let mut seen = HashSet::new();
    let mut differ = Vec::new();
    while let Some(step) = walk.pop() {
        let node = &step.node;
        if !seen.insert(node.ptr.hash) {
            continue;
        }
        let on_ours = ours.log.node_at(&ours.store, node.version())?;
        if on_ours.is_some_and(|on| on.ptr.hash == node.ptr.hash) {
            continue;
        }
        if node.entry.commit.root.map(|ptr| ptr.hash) != root {
            differ.push(step);
            continue;
        }
        let kept = |after: Option<u64>, node: &Node| after.is_none_or(|after| node.time() >= after);
        let prev = node.entry.prev.filter(|prev| prev.offset != 0);
        if let Some(prev) = prev.filter(|_| node.version() > step.low) {
            let below = Node::load(&step.store, prev)?;
            if kept(step.after, &below) {
                walk.push(Start {
                    node: below,
                    ..step.clone()
                });
            }
        }
        if let Some(link) = node.entry.merged.filter(|link| link.ptr.offset != 0) {
            let below = Node::load(&step.store, link.ptr)?;
            let after = link.after(step.after);
            if kept(after, &below) {
                let held = |entry: &Entry| after.is_none_or(|after| entry.commit.time_ms >= after);
                walk.push(Start {
                    low: below.lowest_kept(&step.store, link.floor, held)?,
                    store: step.store.clone(),
                    node: below,
                    after,
                    bare: false,
                    under: None,
                });
            }
        }
    }
    if differ.is_empty() {
        return Ok(false);
    }
    let found = match graph::best(starts.clone(), differ.clone()) {
        Err(Error::NoCommonAncestor) => return Ok(true),
        found => found?,
    };
    for start in differ {
        let ptr = start.node.ptr;
        if holds(&found, ptr) {
            continue;
        }
        match graph::best(starts.clone(), vec![start]) {
            Ok(alone) if holds(&alone, ptr) => {}
            Ok(_) | Err(Error::NoCommonAncestor) => return Ok(true),
            Err(error) => return Err(error),
        }
    }
    Ok(false)
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

    fn moved_states(&self, store: &Store, before: &Refs, after: &Refs) -> Result<Vec<Moved<T>>> {
        for (name, head) in after.try_entries()? {
            if let Some(root) = head.root
                && moved(before.try_get(&name)?.as_deref(), Some(&head))
            {
                store.load::<T>(root)?;
            }
        }
        let attached: Vec<_> = self.branches.lock().values().cloned().collect();
        let mut states = Vec::new();
        for branch in attached {
            let new = after.try_get(&branch.name)?;
            if moved(before.try_get(&branch.name)?.as_deref(), new.as_deref()) {
                states.push((branch, Arc::new(self.state_of(store, new.as_deref())?)));
            }
        }
        Ok(states)
    }

    fn write_records(&self, tip: &mut RefState, mut txn: Txn) -> Result<()> {
        txn.spill()?;
        txn.unreserved();
        match tip.ptr {
            Some(head) => {
                let frame = txn.write(head)?;
                self.queue.lock().waiting.push(Pending {
                    frame,
                    refs: tip.clone(),
                    update: None,
                    seal: None,
                    followed: Vec::new(),
                });
            }
            None => {
                let refs = tip.refs.clone();
                self.stage(tip, Next::Table(refs), txn, None, None, None)?
                    .land()?;
            }
        }
        Ok(())
    }

    fn attach(&self, name: &str, refs: &Refs) -> Result<Arc<Branch<T>>> {
        if let Some(branch) = self.branches.lock().get(name) {
            return Ok(branch.clone());
        }
        let head = refs.try_get(name)?;
        if head.is_none() && name != MAIN {
            return Err(Error::NoBranch(name.to_string()));
        }
        let state = Arc::new(self.state_of(&self.store(), head.as_deref())?);
        let born = head.as_ref().map_or(0, |head| head.born);
        let branch = Arc::new(Branch::new(name, born, self.id, state));
        let attached = self
            .branches
            .lock()
            .entry(name.to_string())
            .or_insert_with(|| branch.clone())
            .clone();
        Ok(attached)
    }

    fn attached(&self, name: &str) -> Result<Arc<Branch<T>>> {
        loop {
            if self.options.shared {
                self.catch_up(&mut Vec::new())?;
            }
            if let Some(branch) = self.branches.lock().get(name) {
                return Ok(branch.clone());
            }
            let tip = self.tip.lock();
            let _whole = reentry::whole(self.id);
            let published = self.published.lock().refs.clone();
            if self.options.shared {
                let newest = tip.refs.try_get(name)?;
                let seen = published.try_get(name)?;
                if moved(seen.as_deref(), newest.as_deref()) {
                    continue;
                }
            }
            return self.attach(name, &published);
        }
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

    fn byte_of(&self, name: &str) -> Result<Option<Byte>> {
        if !self.options.shared {
            return Ok(None);
        }
        let store = self.store();
        let at = lane_of(name);
        store.lock_lane(at)?;
        Ok(Some(Byte { store, at }))
    }

    fn every_lane(&self) -> Vec<Arc<Branch<T>>> {
        let mut branches: Vec<_> = self.branches.lock().values().cloned().collect();
        if self.options.shared {
            for (_, branch) in &self.queue.lock().open {
                if !branches.iter().any(|held| Arc::ptr_eq(held, branch)) {
                    branches.push(branch.clone());
                }
            }
        }
        branches.sort_by(|a, b| a.name.cmp(&b.name));
        branches
    }

    fn covers(&self, branches: &[Arc<Branch<T>>]) -> bool {
        let held = |branch: &Arc<Branch<T>>| branches.iter().any(|lane| Arc::ptr_eq(lane, branch));
        let attached = self.branches.lock();
        if !self.options.shared {
            return attached.len() == branches.len() && attached.values().all(held);
        }
        if !attached.values().all(held) {
            return false;
        }
        drop(attached);
        self.queue
            .lock()
            .open
            .iter()
            .all(|(_, branch)| held(branch))
    }

    fn exclusively<R>(
        &self,
        notices: &mut Vec<Notice<T>>,
        f: impl FnOnce(&mut RefState, &mut Vec<Notice<T>>) -> Result<R>,
    ) -> Result<R> {
        loop {
            let branches = self.every_lane();
            let mut lanes: Vec<_> = branches.iter().map(|branch| self.lane(branch)).collect();
            lanes.iter_mut().for_each(Lane::uncross);
            let store = self.store();
            let stale = || !self.covers(&branches);
            let range = match store.is_shared() {
                true => match store.lock_lanes(stale)? {
                    Some(range) => Some(range),
                    None => continue,
                },
                false => None,
            };
            let mut tip = self.tip.lock();
            let commit = store.is_shared().then(|| store.lock_commit()).transpose()?;
            let current = self.store().id() == store.id()
                && self.follow(&mut tip, true, notices)? != Caught::Swapped;
            if current && self.covers(&branches) {
                let whole = reentry::whole(self.id);
                let out = f(&mut tip, notices);
                drop(whole);
                drop(commit);
                drop(tip);
                drop(range);
                lanes.into_iter().rev().for_each(drop);
                return out;
            }
            drop(commit);
            drop(tip);
            drop(range);
            lanes.into_iter().rev().for_each(drop);
        }
    }

    fn cross(&self, lane: &mut Lane<'_, T>, notices: &mut Vec<Notice<T>>) -> Result<()> {
        self.crossing(lane, true, notices).map(|_| ())
    }

    fn crossing(
        &self,
        lane: &mut Lane<'_, T>,
        wait: bool,
        notices: &mut Vec<Notice<T>>,
    ) -> Result<Option<Store>> {
        if !self.options.shared || lane.crossed() {
            return Ok(None);
        }
        loop {
            let store = self.store();
            if !store.is_shared() {
                return Ok(None);
            }
            let at = lane.branch.lane_at;
            match wait {
                true => store.lock_lane(at)?,
                false if store.try_lock_lane(at)? => {}
                false => return Ok(Some(store)),
            }
            let mut tip = self.tip.lock();
            let caught = match self.store().id() == store.id() {
                true => self.follow(&mut tip, false, notices),
                false => Ok(Caught::Swapped),
            };
            drop(tip);
            match caught {
                Ok(Caught::Swapped) => store.unlock_lane(at),
                Ok(_) => {
                    lane.cross(store);
                    return Ok(None);
                }
                Err(error) => {
                    store.unlock_lane(at);
                    return Err(error);
                }
            }
        }
    }

    fn lock_tip(&self, notices: &mut Vec<Notice<T>>) -> Result<Tip<'_>> {
        loop {
            let mut guard = self.tip.lock();
            if !self.options.shared {
                return Ok(Tip {
                    guard,
                    commit: None,
                });
            }
            let store = self.store();
            let commit = store.lock_commit()?;
            if self.follow(&mut guard, true, notices)? != Caught::Swapped {
                return Ok(Tip {
                    guard,
                    commit: Some(commit),
                });
            }
        }
    }

    fn follow(
        &self,
        tip: &mut RefState,
        holds: bool,
        notices: &mut Vec<Notice<T>>,
    ) -> Result<Caught> {
        self.follow_to(tip, holds, u64::MAX, notices)
    }

    fn follow_to(
        &self,
        tip: &mut RefState,
        holds: bool,
        upto: u64,
        notices: &mut Vec<Notice<T>>,
    ) -> Result<Caught> {
        if !self.options.shared {
            return Ok(Caught::Still);
        }
        let store = self.store();
        store.check_healthy()?;
        match store.follow(holds, upto)? {
            Followed::Still => Ok(Caught::Still),
            Followed::Moved(frame) => {
                let _whole = reentry::whole(self.id);
                self.adopt(tip, &store, frame, notices)?;
                Ok(Caught::Moved)
            }
            Followed::Retired => {
                let _whole = reentry::whole(self.id);
                self.follow_swap(tip, &store, notices)?;
                Ok(Caught::Swapped)
            }
        }
    }

    fn adopt(
        &self,
        tip: &mut RefState,
        store: &Store,
        frame: Frame,
        notices: &mut Vec<Notice<T>>,
    ) -> Result<()> {
        let since = tip.ptr.map(|known| (known, &tip.refs, tip.depth));
        let (refs, depth) = refs::load(store, Some(frame.head), since)?;
        let attached: Vec<_> = self.branches.lock().values().cloned().collect();
        let mut followed = Vec::new();
        let mut gone = Vec::new();
        for branch in attached {
            let new = refs.try_get(&branch.name)?;
            if branch.dropped_in(new.as_deref()) {
                gone.push(branch);
                continue;
            }
            let old = tip.refs.try_get(&branch.name)?;
            if moved(old.as_deref(), new.as_deref()) {
                let state = Arc::new(self.state_of(store, new.as_deref())?);
                followed.push((branch, state));
            }
        }
        store.advance(frame);
        tip.refs = refs;
        tip.ptr = Some(frame.head);
        tip.depth = depth;
        for (branch, state) in &followed {
            free(notices, &branch.tip.swap(state.clone()));
            branch.framed.store(frame.seq, Ordering::Release);
        }
        if !gone.is_empty() {
            let published = self.published.lock();
            let mut branches = self.branches.lock();
            for branch in &gone {
                branches.remove(&branch.name);
            }
            drop(branches);
            drop(published);
            for branch in &gone {
                free(notices, &branch.tip.load());
                free(notices, &branch.state.load());
                branch.retire();
            }
        }
        self.step("adopted");
        self.queue.lock().waiting.push(Pending {
            frame,
            refs: tip.clone(),
            update: None,
            seal: None,
            followed,
        });
        Ok(())
    }

    fn follow_swap(
        &self,
        tip: &mut RefState,
        old: &Store,
        notices: &mut Vec<Notice<T>>,
    ) -> Result<()> {
        self.settle(old.last_seq(), notices)?;
        let (fresh, head) = Store::open(&self.file, self.options.clone())?;
        let rebound = (|| -> Result<_> {
            let (refs, depth) = refs::load(&fresh, head, None)?;
            let attached: Vec<_> = self.branches.lock().values().cloned().collect();
            let mut states = Vec::new();
            let mut gone = Vec::new();
            for branch in attached {
                let found = refs.try_get(&branch.name)?;
                if branch.dropped_in(found.as_deref()) {
                    gone.push(branch);
                    continue;
                }
                states.push((branch, Arc::new(self.state_of(&fresh, found.as_deref())?)));
            }
            Ok((refs, depth, states, gone))
        })();
        let (refs, depth, states, gone) = rebound.inspect_err(|_| fresh.close())?;
        let mut published = self.published.lock();
        tip.refs = refs;
        tip.ptr = head;
        tip.depth = depth;
        self.store.store(Arc::new(fresh.clone()));
        let replaced = mem::replace(&mut *published, tip.clone());
        for (branch, state) in states {
            rebind(&branch, state, notices);
        }
        let mut branches = self.branches.lock();
        for branch in &gone {
            branches.remove(&branch.name);
        }
        drop(branches);
        drop(published);
        drop(replaced);
        for branch in &gone {
            free(notices, &branch.tip.load());
            free(notices, &branch.state.load());
            branch.retire();
        }
        let mut retired = self.retired.lock();
        retired.retain(|old| old.bytes().is_some());
        retired.push(old.retired());
        drop(retired);
        old.close();
        self.signal.base.store(fresh.size(), Ordering::Relaxed);
        self.step("followed");
        Ok(())
    }

    fn catch_up(&self, notices: &mut Vec<Notice<T>>) -> Result<bool> {
        self.catch_up_to(u64::MAX, notices)
    }

    fn catch_up_to(&self, upto: u64, notices: &mut Vec<Notice<T>>) -> Result<bool> {
        let before = self.published.lock().ptr;
        let mut tip = self.tip.lock();
        let caught = self.follow_to(&mut tip, false, upto, notices)?;
        let ticket = self.store().last_seq();
        drop(tip);
        self.settle(ticket, notices)?;
        Ok(caught == Caught::Swapped || self.published.lock().ptr != before)
    }

    fn refresh(&self) -> Result<bool> {
        let mut notices = Vec::new();
        let mut tip = self.tip.lock();
        let _whole = reentry::whole(self.id);
        let current = self.store();
        let (store, head) = match current.refresh() {
            Err(Error::Replaced) => Store::open(&self.file, self.options.clone())?,
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
        let attached: Vec<_> = self.branches.lock().values().cloned().collect();
        let mut states = Vec::new();
        let mut gone = Vec::new();
        for branch in attached {
            let head = refs.try_get(&branch.name)?;
            if branch.dropped_in(head.as_deref()) {
                gone.push(branch);
                continue;
            }
            let state = Arc::new(self.state_of(&store, head.as_deref())?);
            states.push((branch, state));
        }
        let mut published = self.published.lock();
        tip.refs = refs;
        tip.ptr = Some(ptr);
        tip.depth = depth;
        if replaced {
            self.store.store(Arc::new(store.clone()));
        }
        let previous = mem::replace(&mut *published, tip.clone());
        for (branch, state) in states {
            rebind(&branch, state, &mut notices);
        }
        let mut branches = self.branches.lock();
        for branch in &gone {
            branches.remove(&branch.name);
        }
        drop(branches);
        drop(published);
        drop(previous);
        for branch in &gone {
            free(&mut notices, &branch.tip.load());
            free(&mut notices, &branch.state.load());
            branch.retire();
        }
        if replaced {
            current.close();
        }
        Ok(true)
    }

    fn follow_file(shared: &Arc<Self>, poll: Option<Duration>) -> Result<Follower> {
        let weak = Arc::downgrade(shared);
        let look = move || weak.upgrade().map(|shared| shared.look());
        Ok(Follower::start(&shared.file, poll, look)?)
    }

    fn look(&self) -> Result<bool> {
        if self.options.read_only {
            return self.refresh();
        }
        let store = self.store();
        store.check_healthy()?;
        match store.news()? {
            News::Nothing => Ok(false),
            News::Named(seq) => self.catch_up_to(seq, &mut Vec::new()),
            News::Replaced => self.catch_up(&mut Vec::new()),
        }
    }

    fn place_change(
        &self,
        (branch, state): (&Branch<T>, &Arc<State<T>>),
        change: Change<'_, T>,
        drafted: Draft<T>,
        framed: u64,
        notices: &mut Vec<Notice<T>>,
    ) -> Result<(u64, u64)> {
        let mut tip = self.lock_tip(notices)?;
        let whole = reentry::whole(self.id);
        let committed = self.commit_locked(&mut tip, change, drafted);
        drop(whole);
        let (version, time_ms, settling) = committed?;
        let Some((settling, store, landing)) = settling else {
            return Ok((version, time_ms));
        };
        if tip.commit.is_some() {
            self.land(branch, state, framed, landing)?;
            drop(tip);
        } else {
            drop(tip);
            self.land(branch, state, framed, landing)?;
        }
        settling.finish(&store);
        Ok((version, time_ms))
    }

    fn seal_for_others(&self, branch: &Arc<Branch<T>>, group: Group<T>) {
        let seal = group.seal.clone();
        self.seal_group(branch, group);
        seal.wake_one();
        self.rouse();
    }

    fn arrive<'a>(&self, branch: &'a Branch<T>) -> Lane<'a, T> {
        struct Arriving<'b>(&'b AtomicUsize);
        impl Drop for Arriving<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        branch.arriving.fetch_add(1, Ordering::AcqRel);
        let arriving = Arriving(&branch.arriving);
        self.step("arriving");
        let guard = branch.lane.lock();
        drop(arriving);
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
            let mut notices = Vec::new();
            self.place_change((branch, &state), change, drafted, framed, &mut notices)
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
            if let Some(Ok(sealed)) = seal.outcome.get()
                && queue.settled >= sealed.ticket
            {
                return Ok((sealed.version, sealed.time_ms));
            }
            if queue.failed {
                return Err(Error::Poisoned);
            }
            match seal.outcome.get() {
                Some(Err(failure)) => {
                    drop(queue);
                    self.lead_after_failure(branch, &mut notices);
                    self.store().check_healthy()?;
                    return Err(Error::GroupFailed(failure.clone()));
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
                    if !queue.syncing && !queue.failed && !queue.waiting.is_empty() {
                        self.lead(queue, &mut notices)?;
                    }
                    continue;
                }
                None if !queue.syncing && branch.arriving.load(Ordering::Acquire) == 0 => {
                    drop(queue);
                    let mut lane = self.arrive(branch);
                    match lane.take() {
                        Some(group) if Arc::ptr_eq(&group.seal, seal) => {
                            self.seal_group(branch, group);
                        }
                        Some(group) => self.seal_for_others(branch, group),
                        None => {}
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
        if let Some(Ok(sealed)) = seal.outcome.get()
            && queue.settled >= sealed.ticket
        {
            return Poll::Ready(Ok(Grouped::Settled(sealed.version, sealed.time_ms)));
        }
        if queue.failed {
            return Poll::Ready(Err(Error::Poisoned));
        }
        match seal.outcome.get() {
            Some(Err(failure)) => {
                drop(queue);
                self.rouse();
                self.store().check_healthy()?;
                return Poll::Ready(Err(Error::GroupFailed(failure.clone())));
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

    fn lead_after_failure(&self, branch: &Arc<Branch<T>>, notices: &mut Vec<Notice<T>>) {
        let queue = self.queue.lock();
        if queue.syncing || queue.failed {
            return;
        }
        let others = Self::other_groups(&queue, branch);
        drop(queue);
        self.seal_idle(&others);
        let queue = self.queue.lock();
        if !queue.syncing && !queue.failed && !queue.waiting.is_empty() {
            let _ = self.lead(queue, notices);
        }
    }

    fn flush(&self) {
        let mut notices = Vec::new();
        let branches: Vec<_> = self.branches.lock().values().cloned().collect();
        for branch in &branches {
            let mut lane = branch.hold(branch.lane.lock());
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
        txn: Txn,
        branch: Option<(&Arc<Branch<T>>, State<T>)>,
        seal: Option<(&Arc<Seal>, u64, u64)>,
        settling: Option<&mut Settling>,
    ) -> Result<Landing> {
        let (landing, pending) = self.place_frame(tip, next, txn, branch, settling)?;
        let mut queue = self.queue.lock();
        if let Some((seal, version, time_ms)) = seal {
            let _ = seal.outcome.set(Ok(Sealed {
                version,
                time_ms,
                ticket: pending.frame.seq,
                framed: true,
            }));
        }
        queue.waiting.push(Pending {
            seal: seal.map(|(seal, _, _)| seal.clone()),
            ..pending
        });
        Ok(landing)
    }

    fn place_frame(
        &self,
        tip: &mut RefState,
        next: Next,
        mut txn: Txn,
        branch: Option<(&Arc<Branch<T>>, State<T>)>,
        settling: Option<&mut Settling>,
    ) -> Result<(Landing, Pending<T>)> {
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
        let pending = Pending {
            frame,
            refs: tip.clone(),
            update,
            seal: None,
            followed: Vec::new(),
        };
        Ok((landing, pending))
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
        let attempt = panic::catch_unwind(AssertUnwindSafe(|| {
            self.step("leading");
            let landed = store.landed_to(end);
            let kept = batch
                .iter()
                .take_while(|pending| pending.frame.end <= landed)
                .count();
            let started = Instant::now();
            let synced = match kept.checked_sub(1) {
                Some(last) => store.sync(batch[last].frame),
                None => Err(Error::Poisoned),
            };
            self.costs.synced(started.elapsed());
            (kept, synced)
        }));
        let (kept, synced, panicked) = match attempt {
            Ok((kept, synced)) => (kept, synced, None),
            Err(panicked) => {
                store.poison_unsynced();
                (0, Err(Error::Poisoned), Some(panicked))
            }
        };
        let cut = batch.split_off(kept);
        let outcome = synced.map(|()| {
            let last = batch.last().expect("a synced batch has a last frame");
            (last.frame, last.refs.clone())
        });
        let synced = outcome.is_ok();
        let mut seals = Vec::new();
        let mut late = None;
        let published = outcome.as_ref().ok().map(|(_, refs)| {
            let mut published = self.published.lock();
            let replaced = mem::replace(&mut *published, refs.clone());
            late = panic::catch_unwind(AssertUnwindSafe(|| self.step("publishing"))).err();
            (published, replaced)
        });
        for pending in &mut batch {
            seals.extend(pending.seal.take().map(|seal| (seal, synced)));
            if !synced {
                continue;
            }
            if let Some((branch, state)) = pending.update.take() {
                notices.push(branch.publish(state));
            }
            for (branch, state) in pending.followed.drain(..) {
                notices.push(branch.publish(state));
            }
        }
        if published.is_some() && late.is_none() {
            late = panic::catch_unwind(AssertUnwindSafe(|| self.step("published"))).err();
        }
        drop(published);
        drop(batch);
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
            .iter()
            .find_map(|pending| pending.seal.clone());
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
        if let Some(panicked) = panicked.or(late) {
            panic::resume_unwind(panicked);
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
        if self.options.shared {
            if drafted.store.id() != self.store().id() {
                return Err(Error::Corrupt(
                    "the file was replaced while a closure held its branch".into(),
                ));
            }
            let head = tip.refs.try_get(&branch.name)?;
            let at = head.and_then(|head| head.log.tip.map(|ptr| ptr.hash));
            if at != state.log.tip.map(|ptr| ptr.hash) {
                return Err(Error::Corrupt(format!(
                    "another process moved branch {} while this one held its lock",
                    branch.name
                )));
            }
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
        let current = tip.refs.try_get(&branch.name)?;
        let born = current.as_ref().map_or(0, |head| head.born);
        let mut merged = current.map(|head| head.merged.clone()).unwrap_or_default();
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
                born,
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
        self.exclusively(notices, |tip, notices| {
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
            let attached: Vec<_> = self.branches.lock().values().cloned().collect();
            for branch in &attached {
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
        let only = self.cycle.lock();
        let cycle = reentry::cycle(self.id);
        let across = self.hold_cycle(stop.is_none(), &mut notices)?;
        if stop.is_some() && self.options.shared && !self.due() {
            return Err(Error::Interrupted);
        }
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
        let swapped = outcome.is_ok() && self.store().id() != store.id();
        drop(across);
        drop(cycle);
        drop(only);
        if swapped {
            store.close();
            self.step("retired");
        }
        outcome
    }

    fn hold_cycle(&self, wait: bool, notices: &mut Vec<Notice<T>>) -> Result<Option<Held>> {
        loop {
            let store = self.store();
            if !store.is_shared() {
                return Ok(None);
            }
            let Some(held) = store.lock_cycle(wait)? else {
                return Err(Error::Interrupted);
            };
            self.catch_up(notices)?;
            if self.store().id() == store.id() {
                return Ok(Some(held));
            }
        }
    }

    fn due(&self) -> bool {
        let base = self.signal.base.load(Ordering::Relaxed);
        let goal = base.max(self.reclaim.every_bytes);
        self.store().size().saturating_sub(base) >= goal
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
        let mut refs = self.copied_refs(notices)?;
        if self.published.lock().ptr.is_none() && refs.is_empty() {
            report.after_bytes = report.before_bytes;
            return Ok(report);
        }
        let temp = sibling(&self.file);
        let mut copier = Copier::create(store, &temp, &self.options, stop)?;
        self.step("created");
        for _ in 0..MAX_PASSES {
            let copied = copier.pass(&refs)?;
            report.passes += 1;
            refs = self.copied_refs(notices)?;
            if copied < QUIET_BYTES {
                break;
            }
        }
        self.step("copied");
        copier.sync()?;
        self.step("synced");
        let paused = Instant::now();
        let (fresh, objects) =
            self.exclusively(notices, |tip, notices| -> Result<(Store, u64)> {
                self.step("paused");
                self.settle(store.last_seq(), notices)?;
                store.check_healthy()?;
                let basis = store.basis();
                let (head, objects) = {
                    let grown = store.size().saturating_sub(basis.bytes).max(1) as f64;
                    let survival = copier.end().saturating_sub(basis.bytes) as f64 / grown;
                    copier.finish(&tip.refs, store.last_seq(), survival)?
                };
                report.handed_out = copier.handed_out();
                let (fresh, _) = Store::open_as(&temp, &self.file, self.options.clone())?;
                let swapped = self.swap_in(tip, store, &fresh, &temp, head, notices);
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
        Ok(report)
    }

    fn copied_refs(&self, notices: &mut Vec<Notice<T>>) -> Result<Refs> {
        if !self.options.shared {
            return Ok(self.published.lock().refs.clone());
        }
        let mut tip = self.tip.lock();
        self.follow(&mut tip, false, notices)?;
        Ok(tip.refs.clone())
    }

    fn swap_in(
        &self,
        tip: &mut RefState,
        old: &Store,
        fresh: &Store,
        temp: &Path,
        head: Ptr,
        notices: &mut Vec<Notice<T>>,
    ) -> Result<()> {
        let (refs, depth) = refs::load(fresh, Some(head), None)?;
        let _fresh = fresh.is_shared().then(|| fresh.lock_commit()).transpose()?;
        for (_, found) in refs.try_entries()? {
            for ptr in found.root.iter().chain(&found.log.tip) {
                fresh.read_object(*ptr)?;
            }
        }
        let attached: Vec<_> = self.branches.lock().values().cloned().collect();
        let mut states = Vec::new();
        for branch in attached {
            let found = refs.try_get(&branch.name)?;
            let state = Arc::new(self.state_of(fresh, found.as_deref())?);
            states.push((branch, state));
        }
        let mut published = self.published.lock();
        if old.is_shared() {
            old.mark_retired()?;
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
        let replaced = mem::replace(&mut *published, tip.clone());
        for (branch, state) in states {
            notices.push(branch.feed.rebind(state.snapshot.clone()));
            free(notices, &branch.tip.swap(state.clone()));
            free(notices, &branch.state.swap(state));
        }
        drop(published);
        drop(replaced);
        Ok(())
    }
}

impl<T: Data + Default> Db<T> {
    /// Opens the database at `path` with the default [`Options`], creating the file if it does not
    /// exist, and returns a handle on its `main` branch.
    ///
    /// Version 0, the value before the first commit, is `T::default()`.
    ///
    /// # Errors
    ///
    /// As for [`Options::open`]: [`Error::Locked`] while another writer has the file open, and
    /// [`Error::NotFerro`], [`Error::Corrupt`] or [`Error::Io`] for a file it cannot open as a
    /// database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Options::new().open(path)
    }
}

impl<T: Data> Db<T> {
    /// Opens the database at `path` like [`Db::open`], with version 0 from `init`, for a `T` that
    /// is not [`Default`].
    ///
    /// Version 0 is not stored in the file, so `init` runs on every open and should give the same
    /// value each time.
    ///
    /// # Errors
    ///
    /// As for [`Options::open`].
    pub fn open_with(path: impl AsRef<Path>, init: impl FnOnce() -> T) -> Result<Self> {
        Options::new().open_with(path, init)
    }

    /// The path the database was opened with, as given.
    pub fn path(&self) -> PathBuf {
        self.shared.path.clone()
    }

    /// The size of the file in bytes.
    pub fn size_on_disk(&self) -> u64 {
        self.shared.store().size()
    }

    /// How the file's size compares with what it holds, and how reclamation has gone.
    ///
    /// [`Space::live_bytes`] is an estimate from the share of bytes the last reclamation cycle
    /// kept, or the whole file before any cycle has run.
    ///
    /// # Errors
    ///
    /// This call does not fail at present.
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

    /// Runs one reclamation cycle now, pruning every branch to `keep` first.
    ///
    /// The cycle copies what is retained into a new file beside this one, in passes while writers
    /// keep running, then holds up writers for a few milliseconds to copy what they wrote meanwhile
    /// and swap the new file in. Readers do not wait at all, and snapshots held from before keep
    /// reading the old file. `keep` takes the place of [`Options::history`] for this cycle, and
    /// nothing already pruned comes back. A cycle needs free disk for everything retained. See
    /// [Retention and reclamation](https://github.com/archlemon/ferro/blob/main/docs/guide/retention.md).
    ///
    /// # Errors
    ///
    /// - [`Error::ReadOnly`] on a read-only handle.
    /// - [`Error::Reentrant`] when called from inside a closure on this file, or from code that
    ///   runs while this thread holds the file's cycle lock, such as a `Drop` run while a cycle
    ///   copies the file or a large bundle is placed in it.
    /// - [`Error::Corrupt`] or [`Error::Io`] if the copy fails. The partial copy is removed and the
    ///   database goes on in the old file.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Keep, Map, Options};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("log.ferro");
    /// let db = Options::new().open::<Map<u64, u64>>(&path)?;
    /// for i in 0..10 {
    ///     db.update(|root| {
    ///         root.insert(i, i);
    ///     })?;
    /// }
    ///
    /// let report = db.compact(Keep::Last(3))?;
    /// assert_eq!(report.commits_dropped, 7);
    /// assert_eq!(db.log().len(), 3);
    /// assert!(db.at(7).is_err());
    /// assert_eq!(db.at(8)?.len(), 8);
    /// # Ok(())
    /// # }
    /// ```
    pub fn compact(&self, keep: Keep) -> Result<Compacted> {
        let _crossing = self.entering(Needs::Cycle)?;
        self.shared.compact(keep, None)
    }

    /// Records `op` in the trace that [`Options::trace`] set, for crash tests.
    #[doc(hidden)]
    pub fn trace_op(&self, op: Op) {
        self.shared.store().trace(op);
    }

    /// The name of the branch this handle reads and writes.
    pub fn branch_name(&self) -> &str {
        &self.branch.name
    }

    /// The newest published version of this branch.
    ///
    /// It never blocks. With [`Options::shared`], commits from other processes appear once this
    /// process catches up with them, as described there.
    pub fn read(&self) -> Snapshot<T> {
        self.branch.state.load().snapshot.clone()
    }

    /// The version [`Db::read`] would show, 0 before the first commit.
    pub fn version(&self) -> u64 {
        self.branch.state.load().snapshot.commit.version
    }

    /// A hash over this branch's whole history, or `None` before its first commit.
    ///
    /// It is the hash of the newest commit's log entry, which links the entry before it, and so on
    /// back, so pruning does not change it. Published somewhere an attacker cannot edit, it is a
    /// tamper-evidence anchor: check it later with [`Log::hash_at`] of the same version and
    /// [`Db::verify`], and a match shows that version and everything before it are unchanged. It is
    /// not a signature and says nothing about later commits, other branches or whether the times
    /// were true. See
    /// [`head_hash` and tamper evidence](https://github.com/archlemon/ferro/blob/main/docs/internals/history.md#head_hash-and-tamper-evidence).
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::Db;
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("ledger.ferro");
    /// let db = Db::<u64>::open(&path)?;
    /// db.update(|balance| *balance = 100)?;
    /// let (version, anchor) = (db.version(), db.head_hash());
    ///
    /// db.update(|balance| *balance = 80)?;
    /// assert_eq!(db.log().hash_at(&version), anchor);
    /// db.verify()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn head_hash(&self) -> Option<Hash> {
        self.branch.state.load().log.tip.map(|ptr| ptr.hash)
    }

    /// The log of this branch's retained commits, as of its newest published version.
    pub fn log(&self) -> Log {
        let state = self.branch.state.load();
        Log {
            chain: state.log,
            store: state.store.clone(),
        }
    }

    /// Runs `f` on a working copy of the branch's newest value and commits what it changed, with
    /// an empty message.
    ///
    /// This is [`Db::commit`] without a message.
    ///
    /// # Errors
    ///
    /// As for [`Db::commit`].
    ///
    /// # Panics
    ///
    /// As for [`Db::commit`].
    pub fn update<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R> {
        self.commit("", f)
    }

    /// Runs `f` on a working copy of the branch's newest value and commits what it changed as the
    /// next version, with `message`, returning what `f` returned.
    ///
    /// The closure runs on the calling thread under the branch's writer lock, once every closure
    /// before it on this branch has finished, so it sees their changes and nothing else changes the
    /// branch while it runs. When it returns, its changes commit atomically, and the call returns
    /// once the commit is published: under [`Durability::Fsync`], once it is on disk. A closure
    /// that changes nothing makes no version. With [`Options::coalesce`], the closure may share
    /// its version with others queued behind the same sync.
    ///
    /// # Errors
    ///
    /// Nothing the closure did is committed when the call fails.
    ///
    /// - [`Error::ReadOnly`] on a read-only handle, and [`Error::NoBranch`] if the branch was
    ///   dropped.
    /// - [`Error::Reentrant`] from inside a closure on this file, where waiting for this branch's
    ///   writer lock could deadlock, as
    ///   [Writing from inside a closure](https://github.com/archlemon/ferro/blob/main/docs/guide/concurrency.md#writing-from-inside-a-closure)
    ///   lists.
    /// - [`Error::Poisoned`] after an earlier write or sync failed, and [`Error::GroupFailed`] if
    ///   the coalesced group this closure joined could not be written.
    /// - [`Error::Io`] if this commit's write or sync fails, and the errors of encoding the value,
    ///   such as [`Error::TooLarge`].
    ///
    /// # Panics
    ///
    /// A panic in `f` propagates once the branch's writer lock is released, and nothing `f` did is
    /// committed. Reads inside `f` panic on an I/O error or a damaged file, as all collection reads
    /// do.
    pub fn commit<R>(&self, message: impl Into<String>, f: impl FnOnce(&mut T) -> R) -> Result<R> {
        let message = message.into();
        let done = self.transact::<R, Error>(|root, _| Ok((message, f(root))))?;
        Ok(done.value)
    }

    /// Runs `f` like [`Db::commit`], taking the message from what it returns, and reports the
    /// commit as a [`Committed`].
    ///
    /// `f` returns the message and a value. The result holds the value and the version and time
    /// of the commit that holds the change, or of the head `f` ran on if it changed nothing.
    ///
    /// # Errors
    ///
    /// As for [`Db::commit`].
    ///
    /// # Panics
    ///
    /// As for [`Db::commit`].
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Map};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("orders.ferro");
    /// let db = Db::<Map<u64, String>>::open(&path)?;
    /// let placed = db.commit_with(|orders| {
    ///     let id = orders.len() as u64 + 1;
    ///     orders.insert(id, "anvil".to_string());
    ///     (format!("order #{id}"), id)
    /// })?;
    ///
    /// assert_eq!(placed.value, 1);
    /// assert_eq!(placed.version, 1);
    /// assert!(placed.changed);
    /// assert_eq!(db.read().commit().message, "order #1");
    ///
    /// let idle = db.commit_with(|_| ("nothing", ()))?;
    /// assert_eq!(idle.version, 1);
    /// assert!(!idle.changed);
    /// # Ok(())
    /// # }
    /// ```
    pub fn commit_with<R, S: Into<String>>(
        &self,
        f: impl FnOnce(&mut T) -> (S, R),
    ) -> Result<Committed<R>> {
        self.transact::<R, Error>(|root, _| {
            let (message, value) = f(root);
            Ok((message.into(), value))
        })
    }

    /// Runs `f` like [`Db::commit_with`], also handing it the [`Stamp`] of the commit it writes.
    ///
    /// The stamp's version and time are those the commit will have, so a row can record the
    /// commit that wrote it.
    ///
    /// # Errors
    ///
    /// As for [`Db::commit`].
    ///
    /// # Panics
    ///
    /// As for [`Db::commit`].
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Map};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("paid.ferro");
    /// let db = Db::<Map<u64, (u64, u64)>>::open(&path)?;
    /// let paid = db.commit_stamped(|paid, stamp| {
    ///     paid.insert(7, (stamp.version(), stamp.time_ms()));
    ///     ("pay order 7", ())
    /// })?;
    ///
    /// let (version, time_ms) = *db.read().get(&7).unwrap();
    /// assert_eq!(version, paid.version);
    /// assert_eq!(db.as_of(time_ms)?.version(), version);
    /// # Ok(())
    /// # }
    /// ```
    pub fn commit_stamped<R, S: Into<String>>(
        &self,
        f: impl FnOnce(&mut T, &Stamp<'_>) -> (S, R),
    ) -> Result<Committed<R>> {
        self.transact::<R, Error>(|root, stamp| {
            let (message, value) = f(root, stamp);
            Ok((message.into(), value))
        })
    }

    /// Runs `f` like [`Db::update`], with a closure that can fail.
    ///
    /// # Errors
    ///
    /// An `Err` from `f` discards its changes and comes back as it was. The errors of
    /// [`Db::commit`] come back through `E::from`.
    ///
    /// # Panics
    ///
    /// As for [`Db::commit`].
    pub fn try_update<R, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        self.try_commit("", f)
    }

    /// Runs `f` like [`Db::commit`], with a closure that can fail.
    ///
    /// # Errors
    ///
    /// An `Err` from `f` discards its changes and comes back as it was. The errors of
    /// [`Db::commit`] come back through `E::from`.
    ///
    /// # Panics
    ///
    /// As for [`Db::commit`].
    pub fn try_commit<R, E: From<Error>>(
        &self,
        message: impl Into<String>,
        f: impl FnOnce(&mut T) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        let message = message.into();
        let done = self.transact::<R, E>(|root, _| f(root).map(|value| (message, value)))?;
        Ok(done.value)
    }

    /// Runs `f` like [`Db::commit_with`], with a closure that can fail.
    ///
    /// When every error in `f` comes through `?` and the caller uses `?` on the result too, Rust
    /// cannot tell which error type is meant, so name it on the closure, as below.
    ///
    /// # Errors
    ///
    /// An `Err` from `f` discards its changes and comes back as it was. The errors of
    /// [`Db::commit`] come back through `E::from`.
    ///
    /// # Panics
    ///
    /// As for [`Db::commit`].
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Map};
    ///
    /// #[derive(Debug)]
    /// enum ShopError {
    ///     SoldOut,
    ///     Db(ferro::Error),
    /// }
    ///
    /// impl From<ferro::Error> for ShopError {
    ///     fn from(error: ferro::Error) -> Self {
    ///         ShopError::Db(error)
    ///     }
    /// }
    ///
    /// # fn main() -> Result<(), ShopError> {
    /// # let dir = tempfile::tempdir().map_err(ferro::Error::from)?;
    /// # let path = dir.path().join("stock.ferro");
    /// let db = Db::<Map<String, u32>>::open(&path)?;
    /// db.update(|stock| {
    ///     stock.insert("anvil".to_string(), 1);
    /// })?;
    ///
    /// let sell = || {
    ///     db.try_commit_with(|stock| -> Result<_, ShopError> {
    ///         let left = stock.get_mut("anvil").ok_or(ShopError::SoldOut)?;
    ///         *left = left.checked_sub(1).ok_or(ShopError::SoldOut)?;
    ///         Ok(("sell an anvil", *left))
    ///     })
    /// };
    ///
    /// assert_eq!(sell()?.value, 0);
    /// assert!(matches!(sell(), Err(ShopError::SoldOut)));
    /// assert_eq!(db.version(), 2);
    /// # Ok(())
    /// # }
    /// ```
    pub fn try_commit_with<R, S: Into<String>, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        self.transact(|root, _| {
            let (message, value) = f(root)?;
            Ok((message.into(), value))
        })
    }

    /// Runs `f` like [`Db::commit_stamped`], with a closure that can fail.
    ///
    /// # Errors
    ///
    /// An `Err` from `f` discards its changes and comes back as it was. The errors of
    /// [`Db::commit`] come back through `E::from`.
    ///
    /// # Panics
    ///
    /// As for [`Db::commit`].
    pub fn try_commit_stamped<R, S: Into<String>, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T, &Stamp<'_>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        self.transact(|root, stamp| {
            let (message, value) = f(root, stamp)?;
            Ok((message.into(), value))
        })
    }

    /// [`Db::update`] as a future that resolves once the commit is published.
    ///
    /// This is [`Db::commit_async`] without a message.
    ///
    /// # Errors
    ///
    /// As for [`Db::commit_async`].
    ///
    /// # Panics
    ///
    /// As for [`Db::commit_async`].
    pub async fn update_async<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R> {
        self.commit_async("", f).await
    }

    /// [`Db::commit`] as a future that resolves once the commit is published, which under
    /// [`Durability::Fsync`] means durable.
    ///
    /// No thread waits for the writer lock or the sync. The closure runs in the poll that finds the
    /// branch's writer lock free, on the thread that polls, under the same lock as a blocking
    /// write, so it need not be `Send` or `'static`, and the future is `Send` when the closure and
    /// its result are. A future that finds the lock taken waits in line and is woken in turn, but
    /// any poll that finds the lock free takes it, so a future polled for the first time, one in
    /// line that is polled without a wake, or a blocking writer can get in ahead of those in line:
    /// futures get the lock roughly in the order they asked, not exactly. One `ferro-sync` thread
    /// per open database, started by the first awaited write, syncs each batch and wakes the
    /// futures in it through their [`Waker`]s, so any executor works and ferro needs no runtime.
    ///
    /// Dropping the future before the closure ran writes nothing. Once the closure has run, the
    /// commit lands as if awaited, and dropping the future gives up only the answer, so whether a
    /// dropped write landed is exactly whether its closure ran. See
    /// [Awaited writes](https://github.com/archlemon/ferro/blob/main/docs/guide/concurrency.md#awaited-writes).
    ///
    /// # Errors
    ///
    /// As for [`Db::commit`]. With [`Options::coalesce`], a future polled inside a closure on the
    /// same file after its own closure ran also fails with [`Error::Reentrant`], since its commit
    /// could be waiting for that closure to end.
    ///
    /// # Panics
    ///
    /// A panic in `f` unwinds out of the poll that ran it, and nothing `f` did is committed.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Map};
    ///
    /// async fn hit(db: &Db<Map<String, u64>>, page: &str) -> ferro::Result<u64> {
    ///     db.commit_async(format!("hit {page}"), |hits| {
    ///         let count = hits.get(page).map_or(0, |count| *count) + 1;
    ///         hits.insert(page.to_string(), count);
    ///         count
    ///     })
    ///     .await
    /// }
    /// ```
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

    /// [`Db::commit_with`] as a future, awaited as [`Db::commit_async`] describes.
    ///
    /// # Errors
    ///
    /// As for [`Db::commit_async`].
    ///
    /// # Panics
    ///
    /// As for [`Db::commit_async`].
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

    /// [`Db::try_update`] as a future, awaited as [`Db::commit_async`] describes.
    ///
    /// # Errors
    ///
    /// An `Err` from `f` discards its changes and comes back as it was. The errors of
    /// [`Db::commit_async`] come back through `E::from`.
    ///
    /// # Panics
    ///
    /// As for [`Db::commit_async`].
    pub async fn try_update_async<R, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        self.try_commit_async("", f).await
    }

    /// [`Db::try_commit`] as a future, awaited as [`Db::commit_async`] describes.
    ///
    /// # Errors
    ///
    /// An `Err` from `f` discards its changes and comes back as it was. The errors of
    /// [`Db::commit_async`] come back through `E::from`.
    ///
    /// # Panics
    ///
    /// As for [`Db::commit_async`].
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

    /// [`Db::try_commit_with`] as a future, awaited as [`Db::commit_async`] describes.
    ///
    /// # Errors
    ///
    /// An `Err` from `f` discards its changes and comes back as it was. The errors of
    /// [`Db::commit_async`] come back through `E::from`.
    ///
    /// # Panics
    ///
    /// As for [`Db::commit_async`].
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

    /// [`Db::commit_stamped`] as a future, awaited as [`Db::commit_async`] describes.
    ///
    /// # Errors
    ///
    /// As for [`Db::commit_async`].
    ///
    /// # Panics
    ///
    /// As for [`Db::commit_async`].
    pub async fn commit_stamped_async<R, S: Into<String>>(
        &self,
        f: impl FnOnce(&mut T, &Stamp<'_>) -> (S, R),
    ) -> Result<Committed<R>> {
        self.writing::<R, Error, _>(|root, stamp| {
            let (message, value) = f(root, stamp);
            Ok((message.into(), value))
        })
        .await
    }

    /// [`Db::try_commit_stamped`] as a future, awaited as [`Db::commit_async`] describes.
    ///
    /// # Errors
    ///
    /// An `Err` from `f` discards its changes and comes back as it was. The errors of
    /// [`Db::commit_async`] come back through `E::from`.
    ///
    /// # Panics
    ///
    /// As for [`Db::commit_async`].
    pub async fn try_commit_stamped_async<R, S: Into<String>, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T, &Stamp<'_>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        self.writing(|root, stamp| {
            let (message, value) = f(root, stamp)?;
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
        mut lane: Lane<'_, T>,
        from: Option<&Branch<T>>,
        f: impl FnOnce(&State<T>, &mut T, &Stamp<'_>) -> std::result::Result<(String, R, Intent), E>,
    ) -> std::result::Result<Staged<R, E>, E> {
        self.branch.check_live()?;
        self.shared.store().check_healthy()?;
        let mut notices = Vec::new();
        self.shared.cross(&mut lane, &mut notices)?;
        self.branch.check_live()?;
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
            let branch = (&*self.branch, &state);
            let placed = self
                .shared
                .place_change(branch, change, drafted, framed, &mut notices);
            let (version, time_ms) = placed?;
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
        let mut notices = Vec::new();
        let ready = live.and_then(|()| shared.store().check_healthy());
        let crossed = ready.and_then(|()| shared.cross(&mut lane, &mut notices));
        if let Err(error) = crossed.and_then(|()| branch.check_live()) {
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

    /// The snapshot of version 0, whether or not the branch still retains it.
    pub(crate) fn genesis(&self) -> Snapshot<T> {
        Snapshot {
            value: self.shared.genesis.clone(),
            commit: Arc::new(Commit::genesis()),
        }
    }

    /// The snapshot of `version` on this branch.
    ///
    /// Version 0 is the value before the first commit, which the branch keeps until it prunes
    /// version 1.
    ///
    /// # Errors
    ///
    /// [`Error::NoVersion`] if the branch does not retain `version`, because it is newer than the
    /// head or was pruned, and read errors from loading it.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Error};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("count.ferro");
    /// let db = Db::<u64>::open(&path)?;
    /// db.update(|count| *count = 10)?;
    /// db.update(|count| *count = 20)?;
    ///
    /// assert_eq!(*db.at(0)?, 0);
    /// assert_eq!(*db.at(1)?, 10);
    /// assert!(matches!(db.at(3), Err(Error::NoVersion(3))));
    ///
    /// db.restore(&db.at(1)?)?;
    /// assert_eq!(*db.read(), 10);
    /// assert_eq!(db.read().commit().restored, Some(1));
    /// # Ok(())
    /// # }
    /// ```
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

    /// The snapshot of the newest commit on this branch whose time is at most `time_ms`, in
    /// milliseconds since the Unix epoch.
    ///
    /// Before the first commit that is version 0. Times never go back along a branch, so this is
    /// the state the branch was in at that time. Several commits can share a millisecond, when they
    /// come faster than the clock ticks or after it steps back, and then this returns the last of
    /// them. No later commit shares the millisecond of a commit whose closure read
    /// [`Stamp::time_ms`], so `as_of` of that time returns it, though an earlier commit may share
    /// it.
    ///
    /// # Errors
    ///
    /// [`Error::NoVersion`], holding the version below the oldest retained one, for a time before
    /// the oldest retained commit, and read errors.
    pub fn as_of(&self, time_ms: u64) -> Result<Snapshot<T>> {
        let state = self.branch.state.load_full();
        match state.log.at_time(&state.store, time_ms)? {
            Some(entry) => Self::snapshot_of(&state.store, &entry.commit),
            None if state.log.first > 1 => Err(Error::NoVersion(state.log.first - 1)),
            None => self.at(0),
        }
    }

    /// Commits the value of `snapshot` as a new version, with the message `restore v` and its
    /// version.
    ///
    /// History is not rewritten: the new commit records the version it restored in
    /// [`Commit::restored`], and every version between stays readable. The snapshot can come from
    /// any version or branch of this database. A restore always commits alone, even with
    /// [`Options::coalesce`], and makes no version if the value equals the head's. A snapshot held
    /// across a reclamation cycle is written again by value, which takes time in proportion to its
    /// size while the other writers wait.
    ///
    /// # Errors
    ///
    /// As for [`Db::commit`].
    pub fn restore(&self, snapshot: &Snapshot<T>) -> Result<Committed<()>> {
        let message = format!("restore v{}", snapshot.commit.version);
        self.restore_with(snapshot, message)
    }

    /// Commits the value of `snapshot` as a new version, like [`Db::restore`], with `message`.
    ///
    /// # Errors
    ///
    /// As for [`Db::commit`].
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

    /// Walks this branch's retained history newest first, yielding each commit that changed what
    /// `project` sees of the value.
    ///
    /// Each item is a commit and the projection at it, for every commit whose projection differs
    /// from the one before it, a change back to an earlier value included. The oldest retained
    /// commit counts as a change unless it is version 1 and its projection equals version 0's. The
    /// walk is lazy: it reads nothing until asked and stops when you stop, so
    /// [`Iterator::take`], [`Iterator::find`] and [`History::while_some`] cost the commits they
    /// walk, not the length of the log. It walks the branch as it was when called. To list
    /// changes oldest first, collect and reverse.
    ///
    /// # Panics
    ///
    /// The iterator panics on a read error, as collection reads do. [`History::try_next`] returns
    /// the error instead.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Map};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("prices.ferro");
    /// let db = Db::<Map<String, u32>>::open(&path)?;
    /// db.commit("list", |prices| {
    ///     prices.insert("anvil".to_string(), 50);
    /// })?;
    /// db.commit("other", |prices| {
    ///     prices.insert("rope".to_string(), 5);
    /// })?;
    /// db.commit("raise", |prices| {
    ///     prices.insert("anvil".to_string(), 60);
    /// })?;
    ///
    /// let anvil: Vec<_> = db
    ///     .history_of(|prices| prices.get("anvil").map(|price| *price))
    ///     .while_some()
    ///     .map(|(commit, price)| (commit.message, price))
    ///     .collect();
    /// assert_eq!(anvil, [("raise".to_string(), 60), ("list".to_string(), 50)]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn history_of<V, F>(&self, project: F) -> History<T, V, F>
    where
        V: PartialEq,
        F: Fn(&T) -> V,
    {
        self.history_of_range(.., project)
    }

    /// Walks the retained versions in `versions` like [`Db::history_of`].
    ///
    /// A commit in the range is yielded when its projection differs from the version before it,
    /// which can lie just below the range. When the range starts at or below the oldest retained
    /// commit, that commit counts as a change as in [`Db::history_of`]. The walk costs the span it
    /// covers.
    ///
    /// # Panics
    ///
    /// As for [`Db::history_of`].
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

    /// The first version in `versions` at which `test` holds, for a test that stays true once it
    /// turns true.
    ///
    /// It binary searches the retained versions in the range, in about log2 of their number of
    /// probes, each of which loads that version's value. It returns `None` if the test fails at the
    /// newest version in the range or the range holds no retained version. Version 0 is never
    /// tested, and a test that already holds at the oldest retained version returns that version.
    ///
    /// # Errors
    ///
    /// Read errors from loading a probed version.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::Db;
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("count.ferro");
    /// let db = Db::<u64>::open(&path)?;
    /// for _ in 0..100 {
    ///     db.update(|count| *count += 3)?;
    /// }
    ///
    /// assert_eq!(db.bisect(.., |count| *count >= 100)?, Some(34));
    /// assert_eq!(db.bisect(..=20, |count| *count >= 100)?, None);
    /// # Ok(())
    /// # }
    /// ```
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

    /// The commits in `versions` at which what `project` sees of the value changed, oldest first,
    /// with the projection at each.
    ///
    /// It cuts the range in half until the ends of each part agree, so it costs the number of
    /// changes rather than the span. In exchange it misses a change and its reversal that fall
    /// between two probes, so use it only for a value that never returns to an earlier one within
    /// the range, and [`Db::history_of_range`] otherwise. The first version of the range is
    /// compared with the one before it, version 0 included, and counts as a change when that
    /// version is not retained.
    ///
    /// # Errors
    ///
    /// Read errors from loading a probed version.
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

    /// Merges the branch of `other` into this one three ways, with `f` deciding the result.
    ///
    /// `other` is a handle on another branch of this database, from [`Db::fork`] or
    /// [`Db::branch`]; [`Db::pull`] brings in another file's commits. `f` gets a working copy of
    /// this branch's newest value as `ours`, the merge base and `other`'s newest snapshot as
    /// `theirs`, and returns the commit message and a value. Merge each collection of your type
    /// in it yourself, typically with [`Map::merge`]. The merge commits alone, under this branch's
    /// writer lock, and its commit links `other`'s head, so a later merge finds its base from
    /// there. A merge that keeps only our side still commits, so the link is not lost, and one
    /// whose `other` head this branch already holds links nothing and commits only what `f`
    /// changed.
    ///
    /// `theirs` is the snapshot [`Db::read`] on `other` returns. A commit on `other` that is still
    /// waiting for its sync is not merged, and neither is what it merged; a later merge takes it.
    ///
    /// The base is a best common ancestor over the whole merge graph, as git finds it, so a change
    /// is merged once however it arrived. After a criss-cross, where there are several, `f` first
    /// merges them into a virtual base, once for each after the first, and what it returns from
    /// those runs is dropped; that is why it is [`FnMut`]. Branches that share no commit merge
    /// against version 0. See
    /// [Merging](https://github.com/archlemon/ferro/blob/main/docs/guide/merging.md).
    ///
    /// # Errors
    ///
    /// - [`Error::ForeignHandle`] if `other` is a handle on another open database, before `f`
    ///   runs.
    /// - [`Error::NoCommonAncestor`] when the base could lie in pruned history.
    /// - [`Error::NoBranch`] if either branch was dropped.
    /// - The errors of [`Db::commit`].
    ///
    /// # Panics
    ///
    /// A panic in `f` propagates, and nothing is committed.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Map};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("prices.ferro");
    /// let db = Db::<Map<String, u32>>::open(&path)?;
    /// db.commit("list", |prices| {
    ///     prices.insert("anvil".to_string(), 50);
    ///     prices.insert("rope".to_string(), 5);
    /// })?;
    ///
    /// let sale = db.fork("sale")?;
    /// sale.commit("cut the anvil", |prices| {
    ///     prices.insert("anvil".to_string(), 40);
    /// })?;
    /// db.commit("raise the rope", |prices| {
    ///     prices.insert("rope".to_string(), 6);
    /// })?;
    ///
    /// db.merge_with(&sale, |prices, base, theirs| {
    ///     let applied = prices.merge(base, theirs, |conflict| conflict.keep_ours());
    ///     ("merge the sale", applied)
    /// })?;
    ///
    /// let prices = db.read();
    /// assert_eq!(prices.get("anvil").as_deref(), Some(&40));
    /// assert_eq!(prices.get("rope").as_deref(), Some(&6));
    /// # Ok(())
    /// # }
    /// ```
    pub fn merge_with<R, S: Into<String>>(
        &self,
        other: &Db<T>,
        mut f: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Committed<R>> {
        self.try_merge_with::<R, S, Error>(other, |ours, base, theirs| Ok(f(ours, base, theirs)))
    }

    /// Merges the branch of `other` into this one like [`Db::merge_with`], with a closure that can
    /// fail.
    ///
    /// # Errors
    ///
    /// An `Err` from any run of `f`, while a virtual base is built included, discards the merge:
    /// nothing is written or recorded, the next merge finds the same base, and the error comes
    /// back as it was. The errors of [`Db::merge_with`] come back through `E::from`.
    ///
    /// # Panics
    ///
    /// As for [`Db::merge_with`].
    pub fn try_merge_with<R, S: Into<String>, E: From<Error>>(
        &self,
        other: &Db<T>,
        mut f: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        self.same_file(other)?;
        self.alone(Some(&other.branch), |tip, ours, root| {
            let (theirs, published) = other.published_view()?;
            let store = self.shared.store();
            let found = Self::merge_base(
                &store,
                (&self.branch.name, ours, &tip.refs),
                (&other.branch.name, &theirs, &published),
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

    /// Merges `other` like [`Db::try_merge_with`] and records its newest version in this branch's
    /// merge records, as sync does for a remote branch.
    ///
    /// When the merge base already holds `other`'s head, `f` does not run, nothing is committed,
    /// and the inner result is `Err` with `other`'s newest version, for the caller to record.
    ///
    /// When `other`'s head holds this branch's value and so does every commit of its history that
    /// this branch lacks, the merge would bring no change in, so `f` does not run, nothing is
    /// committed and the inner result is `Err(None)`. Nothing is to be recorded either, since
    /// `other`'s head is not an ancestor of this branch's.
    pub(crate) fn merge_tracking<R, S: Into<String>, E: From<Error>>(
        &self,
        other: &Db<T>,
        mut f: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Tracked<Committed<R>>, E> {
        self.same_file(other)?;
        let from = Some(&*other.branch);
        let done = self.alone::<Tracked<R>, E>(from, |tip, ours, root| {
            let (theirs, published) = other.published_view()?;
            let store = self.shared.store();
            let found = Self::merge_base(
                &store,
                (&self.branch.name, ours, &tip.refs),
                (&other.branch.name, &theirs, &published),
            )?;
            let Some(ptr) = theirs.log.tip.filter(|ptr| !holds(&found, *ptr)) else {
                return Ok((String::new(), Err(Some(theirs.log.last)), Intent::default()));
            };
            let root_of = |state: &State<T>| state.snapshot.commit.root.map(|ptr| ptr.hash);
            if root_of(ours) == root_of(&theirs) {
                let starts = Self::starts(&store, (&self.branch.name, ours, &tip.refs))?;
                if !brings_change(ours, starts, &theirs)? {
                    return Ok((String::new(), Err(None), Intent::default()));
                }
            }
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

    /// The best common ancestor of this branch and `other`'s, the commit a merge between them
    /// starts from.
    ///
    /// After a criss-cross there are several, and this is the first that [`Db::common_ancestors`]
    /// returns. Branches that share no commit give version 0. It compares the snapshots
    /// [`Db::read`] returns on the two branches, as [`Db::merge_with`] takes `other`'s.
    ///
    /// # Errors
    ///
    /// - [`Error::ForeignHandle`] if `other` is a handle on another open database.
    /// - [`Error::NoBranch`] if either branch was dropped.
    /// - [`Error::NoCommonAncestor`] when the answer could lie in pruned history, and read errors.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::Db;
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("count.ferro");
    /// let db = Db::<u64>::open(&path)?;
    /// db.update(|count| *count = 1)?;
    /// let side = db.fork("side")?;
    /// side.update(|count| *count = 2)?;
    /// db.update(|count| *count = 3)?;
    ///
    /// let base = db.common_ancestor(&side)?;
    /// assert_eq!((base.version(), *base), (1, 1));
    /// # Ok(())
    /// # }
    /// ```
    pub fn common_ancestor(&self, other: &Db<T>) -> Result<Snapshot<T>> {
        let first = self.common_starts(other)?.into_iter().next();
        match first {
            Some(start) => Self::snapshot_of(&start.store, &start.node.entry.commit),
            None => Ok(self.genesis()),
        }
    }

    /// Every best common ancestor of this branch and `other`'s, best first.
    ///
    /// There is more than one after a criss-cross, when each branch has merged an older state of
    /// the other. They are ordered by generation, then time, then hash, so both sides and every
    /// run agree. Branches that share no commit give version 0 alone.
    ///
    /// # Errors
    ///
    /// As for [`Db::common_ancestor`].
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
        self.same_file(other)?;
        let (refs, store, ours, theirs) = {
            let published = self.shared.published.lock();
            self.branch.check_live()?;
            other.branch.check_live()?;
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
            &store,
            (&self.branch.name, &ours, &refs),
            (&other.branch.name, &theirs, &refs),
        )
    }

    fn same_file(&self, other: &Db<T>) -> Result<()> {
        match Arc::ptr_eq(&self.shared, &other.shared) {
            true => Ok(()),
            false => Err(Error::ForeignHandle),
        }
    }

    fn published_view(&self) -> Result<(Arc<State<T>>, Refs)> {
        let published = self.shared.published.lock();
        self.branch.check_live()?;
        Ok((self.branch.state.load_full(), published.refs.clone()))
    }

    fn merge_base(
        store: &Store,
        ours: (&str, &State<T>, &Refs),
        theirs: (&str, &State<T>, &Refs),
    ) -> Result<Vec<Start>> {
        if ours.1.log.tip.is_none() || theirs.1.log.tip.is_none() {
            return Ok(Vec::new());
        }
        graph::best(Self::starts(store, ours)?, Self::starts(store, theirs)?)
    }

    fn starts(store: &Store, (name, state, refs): (&str, &State<T>, &Refs)) -> Result<Vec<Start>> {
        let mut starts = Vec::new();
        if let Some(tip) = state.log.tip {
            starts.push(Start {
                store: state.store.clone(),
                node: Node::load(&state.store, tip)?,
                low: state.log.first,
                after: state.log.after(&state.store)?,
                bare: false,
                under: None,
            });
        }
        let Some(head) = refs.try_get(name)? else {
            return Err(Error::NoBranch(name.to_string()));
        };
        for (other, seen) in head.merged.try_entries()? {
            let Some(found) = refs.try_get(&other)? else {
                continue;
            };
            if let Some(node) = found.log.node_at(store, seen.0)? {
                let at = seen.1.max(state.log.first);
                starts.push(Start {
                    store: store.clone(),
                    node,
                    low: found.log.first,
                    after: found.log.after(store)?,
                    bare: false,
                    under: state.log.node_at(&state.store, at)?,
                });
            }
        }
        Ok(starts)
    }

    /// Creates the branch `name` at this branch's newest commit and returns a handle on it.
    ///
    /// A fork is one small write to the file whatever the size of the database, and the new
    /// branch shares all the history before it, its merge records included. It waits for a closure
    /// running on this branch, and returns once the new branch is durable.
    ///
    /// # Errors
    ///
    /// - [`Error::BranchExists`] if `name` is `main` or an existing branch.
    /// - [`Error::ReadOnly`] on a read-only handle, and [`Error::NoBranch`] if this branch was
    ///   dropped.
    /// - [`Error::Reentrant`] where waiting for this branch could deadlock, such as from inside a
    ///   closure on it, and write errors as for [`Db::commit`].
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::Db;
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("plan.ferro");
    /// let db = Db::<u64>::open(&path)?;
    /// db.update(|budget| *budget = 100)?;
    ///
    /// let what_if = db.fork("what-if")?;
    /// what_if.update(|budget| *budget = 250)?;
    ///
    /// assert_eq!(*db.read(), 100);
    /// assert_eq!(*db.branch("what-if")?.read(), 250);
    /// assert_eq!(db.branches(), ["main", "what-if"]);
    ///
    /// db.drop_branch("what-if")?;
    /// assert!(what_if.update(|budget| *budget = 300).is_err());
    /// assert_eq!(db.branches(), ["main"]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn fork(&self, name: &str) -> Result<Db<T>> {
        let _crossing = self.entering(Needs::Lane(&self.branch.name))?;
        let mut notices = Vec::new();
        let mut lane = self.shared.lane(&self.branch);
        self.shared.cross(&mut lane, &mut notices)?;
        self.branch.check_live()?;
        let mut tip = self.shared.lock_tip(&mut notices)?;
        let whole = reentry::whole(self.shared.id);
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
        let store = self.shared.store();
        let mut next = tip.refs.clone();
        next.insert(
            name.to_string(),
            Head {
                version: commit.version,
                root: commit.root,
                log: state.log,
                merged,
                born: Head::born_next(&store),
            },
        );
        let txn = store.begin()?;
        self.shared
            .stage(&mut tip, Next::Table(next), txn, None, None, None)?
            .land()?;
        let ticket = store.last_seq();
        drop(whole);
        drop(tip);
        drop(lane);
        self.shared.settle(ticket, &mut notices)?;
        self.branch(name)
    }

    /// A handle on the existing branch `name` of this database.
    ///
    /// `main` always exists, even before its first commit. With [`Options::shared`] this first
    /// catches up with commits from other processes.
    ///
    /// # Errors
    ///
    /// [`Error::NoBranch`] if no branch has that name, and read errors.
    pub fn branch(&self, name: &str) -> Result<Db<T>> {
        let _crossing = self.entering(Needs::Tip)?;
        let branch = self.shared.attached(name)?;
        Ok(Db {
            shared: self.shared.clone(),
            branch,
            keeper: self.keeper.clone(),
        })
    }

    /// Removes the branch `name` from the file.
    ///
    /// It waits for a closure running on that branch to finish. Afterwards other handles on it
    /// keep reading what they had, writes through them fail with [`Error::NoBranch`], and its
    /// watchers end. A handle in another process, or a read-only one, learns of the drop when it
    /// catches up or refreshes, and a later fork of the same name is a new branch that the old
    /// handles never move to. Merge records that name it go too. Its bytes stay in the file until
    /// the next reclamation cycle.
    ///
    /// # Errors
    ///
    /// - [`Error::BranchInUse`] for `main` and for this handle's own branch.
    /// - [`Error::NoBranch`] if no branch has that name.
    /// - [`Error::ReadOnly`] on a read-only handle, [`Error::Reentrant`] where waiting for that
    ///   branch could deadlock, such as from inside a closure on it, and write errors as for
    ///   [`Db::commit`].
    pub fn drop_branch(&self, name: &str) -> Result<()> {
        if name == MAIN || name == self.branch.name {
            return Err(Error::BranchInUse(name.to_string()));
        }
        let _crossing = self.entering(Needs::Lane(name))?;
        let mut notices = Vec::new();
        loop {
            let doomed = self.shared.branches.lock().get(name).cloned();
            let lane = doomed.as_ref().map(|branch| self.shared.lane(branch));
            let byte = self.shared.byte_of(name)?;
            let mut tip = self.shared.lock_tip(&mut notices)?;
            let _whole = reentry::whole(self.shared.id);
            let attached = self.shared.branches.lock().get(name).cloned();
            let moved_file = byte
                .as_ref()
                .is_some_and(|byte| byte.store.id() != self.shared.store().id());
            if moved_file || attached.as_ref().map(Arc::as_ptr) != doomed.as_ref().map(Arc::as_ptr)
            {
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
            tip.commit = None;
            let ticket = store.last_seq();
            self.shared.settle(ticket, &mut notices)?;
            let published = self.shared.published.lock();
            let retired = self.shared.branches.lock().remove(name);
            drop(published);
            if let Some(branch) = &retired {
                branch.retire();
            }
            drop(tip);
            drop(byte);
            drop(lane);
            return Ok(());
        }
    }

    /// The names of every branch in the file, sorted, `main` included.
    pub fn branches(&self) -> Vec<String> {
        let refs = self.shared.published.lock();
        let mut names: Vec<String> = refs.refs.keys().collect();
        if !names.iter().any(|name| name == MAIN) {
            names.push(MAIN.to_string());
            names.sort();
        }
        names
    }

    /// A watcher that hands out each new snapshot of this branch.
    ///
    /// It starts at the version current now, so its first receive waits for the next commit. It
    /// does not queue: a receive returns the newest snapshot it has not returned yet, so a slow
    /// consumer skips versions, costs the writers nothing and holds no backlog. It ends when the
    /// branch is dropped, when its [`Closer`](crate::Closer) closes it, or when the last [`Db`]
    /// handle on the file is dropped. A read-only handle fires its watchers on [`Db::refresh`], or
    /// as other processes commit if it was opened with [`Options::follow`].
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::Db;
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("count.ferro");
    /// let db = Db::<u64>::open(&path)?;
    /// let watcher = db.watch();
    /// assert!(watcher.try_recv().is_err());
    ///
    /// db.update(|count| *count = 1)?;
    /// db.update(|count| *count = 2)?;
    /// let newest = watcher.try_recv().expect("a new version");
    /// assert_eq!((newest.version(), *newest), (2, 2));
    /// assert!(watcher.try_recv().is_err());
    /// # Ok(())
    /// # }
    /// ```
    pub fn watch(&self) -> Watcher<T> {
        Watcher::new(self.branch.feed.clone(), Filter::All, || self.read())
    }

    /// A watcher like [`Db::watch`] that hands out only snapshots that pass `test`.
    ///
    /// It sleeps through commits whose snapshot fails. While a watcher waits, its test runs on a
    /// `ferro-watch` thread for the branch, started when a filtered or async watcher first waits; a
    /// receive that finds a new snapshot already there runs it on the receiving thread. Tests run
    /// under no lock, so a test may read, commit or refresh, but it must not wait for a watcher,
    /// and a slow one delays the branch's other watchers. See
    /// [Where tests run](https://github.com/archlemon/ferro/blob/main/docs/guide/watchers.md#where-tests-run).
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Map};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("stock.ferro");
    /// let db = Db::<Map<String, u32>>::open(&path)?;
    /// let low = db.watch_when(|stock| stock.get("anvil").is_some_and(|left| *left < 3));
    ///
    /// db.update(|stock| {
    ///     stock.insert("anvil".to_string(), 5);
    /// })?;
    /// assert!(low.try_recv().is_err());
    ///
    /// db.update(|stock| {
    ///     stock.insert("anvil".to_string(), 2);
    /// })?;
    /// let snapshot = low.try_recv().expect("the stock ran low");
    /// assert_eq!(snapshot.get("anvil").as_deref(), Some(&2));
    /// # Ok(())
    /// # }
    /// ```
    pub fn watch_when(
        &self,
        test: impl Fn(&Snapshot<T>) -> bool + Send + Sync + 'static,
    ) -> Watcher<T> {
        Watcher::new(
            self.branch.feed.clone(),
            Filter::When(Box::new(test)),
            || self.read(),
        )
    }

    /// A watcher like [`Db::watch_when`] whose test compares the snapshot it last handed out with
    /// the new one.
    ///
    /// `test(seen, now)` decides, so "wake me when this changed since I looked" needs no state of
    /// its own. Until the first receive, `seen` is the snapshot current when the watcher was made,
    /// which [`Watcher::seen`] returns, so a consumer that starts from it misses nothing that lands
    /// meanwhile. Clones share what was seen: a receive by one moves it for all.
    pub fn watch_changes(
        &self,
        test: impl Fn(&Snapshot<T>, &Snapshot<T>) -> bool + Send + Sync + 'static,
    ) -> Watcher<T> {
        Watcher::new(
            self.branch.feed.clone(),
            Filter::Changes(Box::new(test)),
            || self.read(),
        )
    }

    /// Picks up what other processes committed, and returns whether anything moved.
    ///
    /// On a read-only handle it reads the file's newest state, following a file that a
    /// reclamation cycle replaced, and fires the watchers of branches that moved; branches dropped
    /// meanwhile end their watchers. On a writer opened with [`Options::shared`] it catches up with
    /// the other writers' commits. Any other writer is the only one, so it returns `false` at once.
    /// A handle opened with [`Options::follow`] does this on its own, and may still call it, to
    /// look at once or to see the error a look meets.
    ///
    /// # Errors
    ///
    /// Read errors, such as [`Error::Corrupt`] for a damaged branch table, and [`Error::Io`]. A
    /// shared writer whose earlier write or sync failed returns [`Error::Poisoned`] and catches up
    /// with nothing.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Options};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("count.ferro");
    /// let writer = Db::<u64>::open(&path)?;
    /// writer.update(|count| *count = 1)?;
    ///
    /// let reader = Options::new().read_only(true).open::<u64>(&path)?;
    /// writer.update(|count| *count = 2)?;
    /// assert_eq!(*reader.read(), 1);
    ///
    /// assert!(reader.refresh()?);
    /// assert_eq!(*reader.read(), 2);
    /// assert!(!writer.refresh()?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn refresh(&self) -> Result<bool> {
        if !self.shared.options.read_only && !self.shared.options.shared {
            return Ok(false);
        }
        let _crossing = self.entering(Needs::Tip)?;
        match self.shared.options.read_only {
            true => self.shared.refresh(),
            false => self.shared.catch_up(&mut Vec::new()),
        }
    }

    /// How the thread that [`Options::follow`] started has gone, or `None` if this handle does
    /// not follow the file.
    ///
    /// It says whether inotify wakes the thread, how often it looked and refreshed, and the error
    /// its last look failed with, until a look succeeds. Every handle on the file shares one
    /// thread, so they all report the same.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Options};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("count.ferro");
    /// let writer = Db::<u64>::open(&path)?;
    /// let reader = Options::new().read_only(true).follow(true).open::<u64>(&path)?;
    ///
    /// let following = reader.following().expect("the reader follows");
    /// assert!(following.last_error.is_none());
    /// assert!(writer.following().is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub fn following(&self) -> Option<Following> {
        self.keeper.follower.as_ref().map(Follower::status)
    }

    /// Re-hashes every object reachable from every retained version of every branch.
    ///
    /// A match shows that each object is the one its link names, so together with
    /// [`Db::head_hash`] it shows history is unchanged. It reads everything retained once, without
    /// decoding it as `T`, as [`Db::verify_typed`] does.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] for the first object whose bytes do not match its hash, and other read
    /// errors.
    pub fn verify(&self) -> Result<Verified> {
        let (head, store) = {
            let published = self.shared.published.lock();
            (published.ptr, self.shared.store())
        };
        crate::admin::verified(&store, head)
    }

    /// Like [`Db::verify`], and also decodes every object reachable from every retained version
    /// as the type that reads it.
    ///
    /// It checks each map node's count, level and key range against its parent, recomputes
    /// each summary from the entries below it, and checks that each index a table stores under a
    /// fingerprint `T` knows holds exactly the entries its rows give, as [`Db::receive`] checks
    /// what it is sent, so it finds a damaged file or a type change that broke stored data before
    /// a read panics on it or a fold or lookup returns a stale answer. It costs a full decode of
    /// everything retained. A table held in a version's root is checked against the table at the
    /// same place in its parent's root, for what changed between them, and any other table by
    /// building its indexes from its rows.
    ///
    /// # Errors
    ///
    /// As for [`Db::verify`], and [`Error::Corrupt`] naming the first object that does not decode
    /// or does not fit its parent, which includes a summary that is not the fold of its entries
    /// and one whose `of` or `combine` panics to recompute, or a table whose index holds other
    /// entries than its rows give, whose row is stored under a key other than its own, or two of
    /// whose rows share a value of a unique index.
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

/// What [`Db::verify`], [`Db::verify_typed`] or [`admin::verify`](crate::admin::verify) checked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Verified {
    /// The number of distinct objects re-hashed.
    pub objects: u64,
    /// The bytes those objects take in the file.
    pub bytes: u64,
    /// The size of the file in bytes.
    pub file_bytes: u64,
}

impl<T: Data> Db<T> {
    /// Checks that a call needing `needs` cannot deadlock on a lock this thread holds, or on a
    /// write waiting on this thread, and records the crossing into this file while it runs.
    ///
    /// # Errors
    ///
    /// [`Error::Reentrant`] where it could.
    pub(crate) fn entering(&self, needs: Needs<'_>) -> Result<Option<Crossing>> {
        reentry::check(self.shared.id, self.shared.coalesce, needs)
    }

    /// The current store and the branch table as last published.
    pub(crate) fn published_refs(&self) -> (Store, Refs) {
        let published = self.shared.published.lock();
        (self.shared.store(), published.refs.clone())
    }

    /// Calls the [`Options::hook`] at the step `name`, if one is set.
    pub(crate) fn step(&self, name: &'static str) {
        self.shared.step(name);
    }

    /// Checks that this handle can write: its branch is live and the file is writable and
    /// healthy.
    ///
    /// # Errors
    ///
    /// [`Error::NoBranch`], [`Error::ReadOnly`] or [`Error::Poisoned`].
    pub(crate) fn check_writable(&self) -> Result<()> {
        self.branch.check_live()?;
        let store = self.shared.store();
        if store.is_read_only() {
            return Err(Error::ReadOnly);
        }
        store.check_healthy()
    }

    /// Waits for any reclamation cycle to finish and keeps another from starting until the
    /// returned guard is dropped.
    ///
    /// With [`Options::shared`] it holds the cycle lock across processes too, and catches up with
    /// a cycle another process ran.
    pub(crate) fn hold_cycles(&self) -> Result<Cycles<'_, T>> {
        let mut freed = Vec::new();
        let local = self.shared.cycle.lock();
        let held = reentry::cycle(self.shared.id);
        let across = self.shared.hold_cycle(true, &mut freed)?;
        Ok(Cycles {
            _across: across,
            _local: local,
            _held: held,
            _freed: freed,
        })
    }

    /// Writes the records `fill` stages as a frame of their own, without moving any branch.
    ///
    /// The frame is queued for the next sync behind the file's current branch table, so nothing
    /// can reach the records until a later frame links them. `fill` runs under the file's tip
    /// lock.
    pub(crate) fn append_frame<X>(
        &self,
        fill: impl FnOnce(&Store, &mut Txn) -> Result<X>,
    ) -> Result<X> {
        let shared = &self.shared;
        let mut notices = Vec::new();
        let mut tip = shared.lock_tip(&mut notices)?;
        let _whole = reentry::whole(shared.id);
        let store = shared.store();
        let mut txn = store.begin()?;
        let out = fill(&store, &mut txn)?;
        shared.write_records(&mut tip, txn)?;
        Ok(out)
    }

    /// Replaces the branch table with what `edit` returns, under every writer lock of the file.
    ///
    /// `edit` sees the current table and stages any records it needs in the transaction. If it
    /// returns a new table, the records it staged are written first as a frame of their own that
    /// keeps the current table, and the root of every head the new table moves is read as `T`.
    /// Only then does the new table land, and it is settled, and every attached branch it moved
    /// is published to its readers and watchers together with the table. `None` writes nothing.
    ///
    /// # Errors
    ///
    /// The error of `edit`, and of reading a moved head's root, such as [`Error::Decode`] for one
    /// that `T` cannot read, after which the table and every branch are as they were, and the
    /// staged records stay unlinked in the log until the next reclamation cycle drops them. Write
    /// and sync errors as for [`Db::commit`].
    pub(crate) fn rewrite_refs<X>(
        &self,
        edit: impl FnOnce(&Store, &mut Txn, &Refs) -> Result<(Option<Refs>, X)>,
    ) -> Result<X> {
        let shared = &self.shared;
        let mut notices = Vec::new();
        shared.exclusively(&mut notices, |tip, notices| {
            self.branch.check_live()?;
            let store = shared.store();
            let mut txn = store.begin()?;
            let (next, out) = edit(&store, &mut txn, &tip.refs)?;
            let Some(next) = next else {
                return Ok(out);
            };
            if txn.holds_records() {
                shared.write_records(tip, txn)?;
                txn = store.begin()?;
            }
            let states = shared.moved_states(&store, &tip.refs, &next)?;
            let (landing, mut pending) =
                shared.place_frame(tip, Next::Table(next), txn, None, None)?;
            let landed = landing.land();
            if landed.is_ok() {
                for (branch, state) in &states {
                    free(notices, &branch.tip.swap(state.clone()));
                    branch.framed.store(pending.frame.seq, Ordering::Release);
                }
                pending.followed = states;
            }
            shared.queue.lock().waiting.push(pending);
            landed?;
            shared.settle(store.last_seq(), notices)?;
            Ok(out)
        })
    }
}
