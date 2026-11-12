//! The trees a reclamation cycle reaches, copied on several threads into the bytes one thread
//! writes.
//!
//! The walk hands each tree it reaches through an entry's root link to [`Units::dispatch`] as a
//! unit and goes on down the chain. Any other tree it reaches, from the branch table or from a tree
//! it copies itself, it hands out once it has read the tree's root and found no link to a log
//! entry, a pruned hash or offset 0. Worker threads discover units: they read each record, check
//! its hash, claim the objects its links name, and list the records the unit copies in the order
//! one thread writes them, children first. A unit whose root has two or more children that no
//! earlier unit holds is split, near the top of the tree, into a unit for each such child and one
//! for the root. Every claim carries its unit's [`Key`], which orders units as one thread reaches
//! them, and the smallest key holds an object, so two units that reach one object agree on which
//! copies it whichever thread got there first. Units are placed in key order: each record its unit
//! still holds takes the next offset, and its length follows from the offsets of its links, which
//! are all placed before it. Workers then write placed records in jobs of at least 256 KiB, reading
//! each one again, records that lie next to each other in the file in one read, and hand each job
//! to whichever thread holds the file, which writes every job waiting before it lets the file go,
//! so no thread waits for another's write. No more jobs are built or wait for the file at once than
//! there are threads. A unit that reaches anything the walk treats differently, such as a log
//! entry, a pruned hash or a link at offset 0, or whose discovery fails, is tainted, and the walk
//! copies it itself where it falls in the order.
//!
//! While the walk goes down the chains before it hands out any unit, it passes the root of each
//! entry to [`Units::check_ahead`], and workers with nothing else to do check the hashes of the
//! records below, claiming nothing. Each slot records the offset of the record checked under its
//! hash, so discovery takes a record checked at the offset it reaches as checked, and only reads
//! and parses it. Before that walk goes down a chain, [`Units::read_ahead`] has threads read the
//! chain's entries ahead of it, as [`Ahead`] describes.
//!
//! A thread checks the hashes of the records it reads many at once, in [`Checks`], so it follows
//! the links of a record before that record is checked. It holds the units it discovers until
//! every record it queued before them is checked, and publishes them then, as tainted if any
//! failed, so a unit split into others hands them out only once its root is checked, and no
//! record reaches a write job unchecked. It marks a slot checked only once its record passed. It
//! checks what it holds before it reads past the frames written so far or a record of more than 64
//! KiB, before it checks at once a record too large to queue, which it decompresses whole, when
//! what it holds reaches 256 KiB or 64 units, before it takes work other than a unit while it holds
//! units, and before it would wait for work. A thread checking ahead hands other threads only links
//! of records already checked, and stops at a failure.
//!
//! A slot is numbered by hash in [`Ids`], which any thread reads without a lock, and each thread
//! remembers the numbers of the hashes it met lately. A thread numbers every link of a record it
//! reads at once, fetching their slots into the cache together, and a claim compares and lowers
//! the slot's key without a lock. The offsets placed records take are kept apart from the slots
//! that the claims and checks write, and the root of each unit the walk dispatched is found by its
//! number rather than its hash.

use std::any::Any;
use std::collections::{BinaryHeap, HashSet, VecDeque};
use std::mem;
use std::ops::Range;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use arc_swap::ArcSwap;
use parking_lot::{Condvar, Mutex};

use super::ahead::{Ahead, Read};
use super::checks::Checks;
use super::ids::Ids;
use super::{ByHash, CHECK_PAST};
use crate::codec::{HASH_BYTES, Hash, Ptr, Staged};
use crate::error::{Error, Result};
use crate::log::Chain;
use crate::map::prefetch;
use crate::platform::start_writeback;
use crate::store::{Store, Traced, append_staged, stored_within, varint_len, zigzag};

/// Where a unit falls in the order one thread copies in: the unit's place among those the walk
/// dispatched in the high 32 bits, then one byte for each level it was split at, the child's
/// index, with 255 below the last.
pub(super) type Key = u64;

/// The least offset a link to a dispatched unit holds until the unit is placed: the unit's
/// number in dispatch order taken from `u64::MAX`.
pub(super) const PENDING: u64 = u64::MAX - u32::MAX as u64;
/// How many levels below a dispatched unit's root a unit may be split at.
const LEVELS: u32 = 3;
/// The most links a node split into units may have, so that each child's index fits its byte.
const MAX_CHILDREN: usize = 254;
const CHUNK: usize = 1 << 16;
const CHUNKS: usize = 1 << 16;
const JOB_BYTES: u64 = 256 << 10;
const SPAN_BYTES: u64 = 256 << 10;
const SPARE_OUT: usize = 8;
const KEEP_BYTES: usize = 1 << 20;
const WAIT: Duration = Duration::from_millis(10);
const BATCH: usize = 16;
/// The most records waiting to be checked ahead of their units, past which no more are queued.
const CHECKS: usize = 1 << 16;
/// What a slot's `checked` holds while a thread checks the record.
const CHECKING: u64 = u64::MAX;
/// How many records a thread checks ahead between looks at the queue.
const SHARE: usize = 64;
/// The bytes of records a thread queues to check at once.
const CHECK_BYTES: usize = 256 << 10;
/// The most units a thread holds discovered while their records wait to be checked.
const HOLD_UNITS: usize = 64;

enum Failure {
    Failed(Error),
    Panicked(Box<dyn Any + Send>),
}

fn root_key(index: u32) -> Key {
    u64::from(index) << 32 | 0xffff_ffff
}

fn child_key(key: Key, depth: u32, index: usize) -> Key {
    let shift = 24 - 8 * depth;
    key & !(0xff << shift) | (index as u64) << shift
}

struct Slot {
    claim: AtomicU64,
    checked: AtomicU64,
}

impl Default for Slot {
    fn default() -> Slot {
        Slot {
            claim: AtomicU64::new(u64::MAX),
            checked: AtomicU64::new(0),
        }
    }
}

/// An array indexed by `u32` that allocates its chunks as they are first touched.
struct Chunked<T> {
    chunks: Box<[OnceLock<Box<[T]>>]>,
}

impl<T: Default> Chunked<T> {
    fn new() -> Chunked<T> {
        Chunked {
            chunks: (0..CHUNKS).map(|_| OnceLock::new()).collect(),
        }
    }

    fn get(&self, id: u32) -> &T {
        let id = id as usize;
        let chunk =
            self.chunks[id / CHUNK].get_or_init(|| (0..CHUNK).map(|_| T::default()).collect());
        &chunk[id % CHUNK]
    }
}

/// Which unit holds each object, by hash, and where each placed object sits in the copy.
///
/// A slot is numbered by hash in [`Ids`], which any thread reads without a lock, and read by its
/// number. The offsets placed objects take are kept apart from the slots, so that the thread
/// placing them reads the offsets of the objects they link from lines the other threads do not
/// write.
struct Claims {
    ids: Ids,
    slots: Chunked<Slot>,
    offsets: Chunked<AtomicU64>,
    next: AtomicU32,
}

impl Claims {
    fn new() -> Claims {
        Claims {
            ids: Ids::new(),
            slots: Chunked::new(),
            offsets: Chunked::new(),
            next: AtomicU32::new(0),
        }
    }

    fn slot(&self, id: u32) -> &Slot {
        self.slots.get(id)
    }

    fn offset(&self, id: u32) -> &AtomicU64 {
        self.offsets.get(id)
    }

    fn claim(&self, hash: &Hash, key: Key, seen: &mut Seen) -> (u32, bool) {
        let id = self.id(hash, seen);
        (id, self.take(id, key))
    }

    fn take(&self, id: u32, key: Key) -> bool {
        let slot = self.slot(id);
        key < slot.claim.load(Ordering::Acquire)
            && key < slot.claim.fetch_min(key, Ordering::AcqRel)
    }

    fn number(&self, links: &[Ptr], ids: &mut Vec<u32>, seen: &mut Seen) {
        for link in links {
            self.ids.prefetch(&link.hash);
        }
        ids.clear();
        for link in links {
            let id = self.id(&link.hash, seen);
            prefetch(std::ptr::from_ref(self.slot(id)).addr());
            ids.push(id);
        }
    }

    fn id(&self, hash: &Hash, seen: &mut Seen) -> u32 {
        let at = seen.at(hash);
        if let Some((known, id)) = seen.slots.get(at)
            && *id != u32::MAX
            && known == hash
        {
            return *id;
        }
        let id = match self.ids.get(hash) {
            Some(id) => id,
            None => {
                if seen.ids.is_empty() {
                    let first = self.next.fetch_add(IDS, Ordering::Relaxed);
                    assert!(first < u32::MAX - IDS, "a copy of more than 2^32 objects");
                    seen.ids = first..first + IDS;
                }
                let id = self.ids.insert(hash, seen.ids.start);
                if id == seen.ids.start {
                    seen.ids.start += 1;
                }
                id
            }
        };
        seen.put(at, hash, id);
        id
    }

    fn held(&self, id: u32) -> Key {
        self.slot(id).claim.load(Ordering::Acquire)
    }

    fn placed(&self, hash: &Hash) -> Option<u64> {
        let id = self.ids.get(hash)?;
        let offset = self.offset(id).load(Ordering::Acquire);
        (offset != 0).then_some(offset)
    }

    fn place(&self, hash: &Hash, key: Key, offset: u64) {
        let id = match self.ids.get(hash) {
            Some(id) => id,
            None => self
                .ids
                .insert(hash, self.next.fetch_add(1, Ordering::Relaxed)),
        };
        self.slot(id).claim.store(key, Ordering::Release);
        self.offset(id).store(offset, Ordering::Release);
    }
}

/// How many hashes each thread remembers the slot of, so that claiming one it met lately takes no
/// lock.
const SEEN: usize = 1 << 12;
/// How many slot numbers a thread takes at once for the hashes it meets first.
const IDS: u32 = 1 << 10;

/// The slots of hashes a thread met lately, in a table indexed by the hash's first bytes, each
/// entry replacing the one before at its index, and the block of slot numbers the thread gives the
/// hashes it meets first.
///
/// An entry whose slot is `u32::MAX`, which no hash is given, is empty.
#[derive(Default)]
struct Seen {
    slots: Vec<(Hash, u32)>,
    ids: Range<u32>,
}

impl Seen {
    fn at(&self, hash: &Hash) -> usize {
        usize::from(u16::from_le_bytes([hash[0], hash[1]])) % SEEN
    }

    fn put(&mut self, at: usize, hash: &Hash, id: u32) {
        if self.slots.is_empty() {
            self.slots = vec![([0xff; HASH_BYTES], u32::MAX); SEEN];
        }
        self.slots[at] = (*hash, id);
    }
}

/// A tree to copy: a root and what its discovery found.
pub(super) struct Unit {
    key: Key,
    index: u32,
    root: Ptr,
    depth: u32,
    found: OnceLock<Found>,
}

impl Unit {
    /// The tree's root as the walk reached it.
    pub(super) fn root(&self) -> Ptr {
        self.root
    }

    /// The key the walk claims what it copies of a tainted unit with.
    pub(super) fn key(&self) -> Key {
        self.key
    }
}

enum Found {
    Copied {
        children: Vec<Arc<Unit>>,
        records: Records,
    },
    Tainted,
}

#[derive(Default)]
struct Records {
    list: Vec<Rec>,
    links: Vec<u32>,
}

struct Rec {
    slot: u32,
    end: u32,
    fixed: u32,
    len: u32,
    src: u64,
}

impl Records {
    fn push(&mut self, slot: u32, node: &Node, ids: &[u32]) -> Option<()> {
        self.links.extend_from_slice(ids);
        self.list.push(Rec {
            slot,
            end: u32::try_from(self.links.len()).ok()?,
            fixed: node.fixed,
            len: node.len,
            src: node.src,
        });
        Some(())
    }

    fn links(&self, index: usize) -> &[u32] {
        let start = match index {
            0 => 0,
            index => self.list[index - 1].end as usize,
        };
        &self.links[start..self.list[index].end as usize]
    }
}

struct Node {
    src: u64,
    fixed: u32,
    len: u32,
    links: Vec<Ptr>,
}

struct Queued(Key, Arc<Unit>);

impl Queued {
    fn of(unit: Arc<Unit>) -> Queued {
        Queued(unit.key, unit)
    }
}

impl PartialEq for Queued {
    fn eq(&self, other: &Queued) -> bool {
        self.0 == other.0
    }
}

impl Eq for Queued {}

impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Queued) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Queued {
    fn cmp(&self, other: &Queued) -> std::cmp::Ordering {
        other.0.cmp(&self.0)
    }
}

#[derive(Default)]
struct Job {
    start: u64,
    len: u64,
    parts: Vec<(Arc<Unit>, Range<usize>)>,
}

impl Job {
    fn add(&mut self, unit: &Arc<Unit>, index: usize, at: u64, size: u64) {
        if self.parts.is_empty() {
            self.start = at;
            self.len = 0;
        }
        match self.parts.last_mut() {
            Some((last, range)) if Arc::ptr_eq(last, unit) => range.end = index + 1,
            _ => self.parts.push((unit.clone(), index..index + 1)),
        }
        self.len += size;
    }
}

enum Item {
    Unit(Arc<Unit>),
    Job(Job),
    Check(Ptr),
    Read(Read),
    Flush,
}

/// The work waiting for the copy's threads.
///
/// `writing` counts the write jobs closed and not yet written, those waiting in `jobs` among
/// them, and a thread takes a job only while fewer than `building` others are being built or
/// waiting for the file, so that a burst of units placed at once does not fill the budget with
/// jobs.
#[derive(Default)]
struct Queue {
    units: BinaryHeap<Queued>,
    jobs: VecDeque<Job>,
    checks: VecDeque<Ptr>,
    reads: VecDeque<Read>,
    writing: usize,
    building: usize,
}

impl Queue {
    fn work(&mut self) -> Option<Item> {
        if self.writing - self.jobs.len() < self.building
            && let Some(job) = self.jobs.pop_front()
        {
            return Some(Item::Job(job));
        }
        self.units.pop().map(|queued| Item::Unit(queued.1))
    }

    fn take(&mut self) -> Option<Item> {
        if let Some(read) = self.reads.pop_front() {
            return Some(Item::Read(read));
        }
        self.work()
            .or_else(|| self.checks.pop_front().map(Item::Check))
    }
}

#[derive(Default)]
struct Placer {
    end: u64,
    cursor: Vec<(Arc<Unit>, usize)>,
    job: Job,
    objects: u64,
    failed: Option<String>,
}

enum Progress {
    Done,
    Waiting,
    Tainted(Arc<Unit>),
}

/// Where placing the dispatched units stopped.
pub(super) enum Settled {
    /// Every unit is placed, and the copy ends at `end` with `objects` more objects in it.
    Done { end: u64, objects: u64 },
    /// The walk must copy `unit` itself from `end`, then call [`Units::resolved`].
    Tainted { unit: Arc<Unit>, end: u64 },
    /// No more units than were asked for are left to place, and the threads still hold the end
    /// of the copy.
    Ahead,
}

#[derive(Default)]
struct Scratch {
    bytes: Vec<u8>,
    links: Vec<Ptr>,
    new: Vec<Ptr>,
    out: Vec<u8>,
    spare: Vec<Vec<Ptr>>,
    ids: Vec<Vec<u32>>,
    checks: Vec<Ptr>,
    seen: Seen,
    unchecked: Unchecked,
}

/// A slot to mark checked once the record read under it passes: its number, the offset of the
/// record, and whether the thread holds the slot as checking it.
type Mark = (u32, u64, bool);

/// What a thread has read and not yet checked: the records queued in `checks`, the slot each of
/// them marks checked once it passes, and the units discovered from them, which the thread
/// publishes only once every record queued before them is checked. `failed` records that a check
/// failed since a thread checking ahead last cleared it.
struct Unchecked {
    checks: Checks,
    marks: Vec<Mark>,
    units: Vec<(Arc<Unit>, Found)>,
    failed: bool,
}

impl Default for Unchecked {
    fn default() -> Unchecked {
        Unchecked {
            checks: Checks::new(CHECK_BYTES),
            marks: Vec::new(),
            units: Vec::new(),
            failed: false,
        }
    }
}

impl Unchecked {
    fn holds(&self) -> bool {
        !self.checks.is_empty() || !self.units.is_empty()
    }
}

impl Scratch {
    fn recycle(&mut self, mut links: Vec<Ptr>, mut ids: Vec<u32>) {
        if self.spare.len() < 64 {
            links.clear();
            self.spare.push(links);
        }
        if self.ids.len() < 64 {
            ids.clear();
            self.ids.push(ids);
        }
    }
}

/// The bytes of records the copy's threads hold at once, as read or as written, past which a
/// thread waits for others to finish, unless it would hold them alone.
const BUDGET: u64 = 16 << 20;
/// The most bytes a compressed record and its body may take together for a thread to check it
/// without counting them against [`BUDGET`], since it decompresses so small a body into a buffer
/// it keeps from one record to the next.
const UNCOUNTED: usize = 4 << 10;

/// The bytes the copy's threads hold, counted without a lock while they fit [`BUDGET`].
///
/// A thread that would pass it counts itself in `waiting` and sleeps on `freed` under `lock`, and
/// a thread that gives bytes back wakes the sleepers only when `waiting` says there are any.
#[derive(Default)]
struct Budget {
    used: AtomicU64,
    waiting: AtomicU64,
    lock: Mutex<()>,
    freed: Condvar,
}

impl Budget {
    fn try_hold(&self, bytes: u64) -> bool {
        let mut used = self.used.load(Ordering::SeqCst);
        loop {
            if used > 0 && used.saturating_add(bytes) > BUDGET {
                return false;
            }
            match self.used.compare_exchange_weak(
                used,
                used + bytes,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(now) => used = now,
            }
        }
    }

    fn hold(&self, bytes: u64) {
        if self.try_hold(bytes) {
            return;
        }
        let mut lock = self.lock.lock();
        self.waiting.fetch_add(1, Ordering::SeqCst);
        while !self.try_hold(bytes) {
            self.freed.wait(&mut lock);
        }
        self.waiting.fetch_sub(1, Ordering::SeqCst);
    }

    fn give(&self, bytes: u64) {
        self.used.fetch_sub(bytes, Ordering::SeqCst);
        if self.waiting.load(Ordering::SeqCst) > 0 {
            let _lock = self.lock.lock();
            self.freed.notify_all();
        }
    }

    fn take(&self, bytes: u64) -> Taken<'_> {
        self.hold(bytes);
        Taken {
            budget: self,
            bytes,
        }
    }
}

struct Taken<'a> {
    budget: &'a Budget,
    bytes: u64,
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        self.budget.give(self.bytes);
    }
}

/// The bytes of a write job, built and waiting for the file, counted against the budget until
/// written.
struct Filled {
    bytes: Vec<u8>,
    start: u64,
    held: u64,
}

/// Write jobs built and waiting for whichever thread holds the file to write them, and the buffers
/// of jobs written, for the next to be built in.
#[derive(Default)]
struct Outbox {
    filled: Vec<Filled>,
    spare: Vec<Vec<u8>>,
}

/// Bytes of a record the walk reads, counted against the threads' budget until dropped.
pub(super) struct Reserved {
    shared: Arc<Shared>,
    bytes: u64,
}

impl Drop for Reserved {
    fn drop(&mut self) {
        self.shared.budget.give(self.bytes);
    }
}

struct Shared {
    source: Store,
    budget: Budget,
    file: Arc<Traced>,
    claims: Claims,
    dispatched: Mutex<VecDeque<Arc<Unit>>>,
    placed: AtomicU64,
    writing: Mutex<()>,
    outbox: Mutex<Outbox>,
    queue: Mutex<Queue>,
    ready: Condvar,
    moving: Mutex<()>,
    moved: Condvar,
    events: AtomicU64,
    watching: AtomicU64,
    placer: Mutex<Placer>,
    again: AtomicBool,
    failure: Mutex<Option<Failure>>,
    cancel: AtomicBool,
    checking: AtomicBool,
    clean: HashSet<Hash, ByHash>,
    taint: ArcSwap<HashSet<Hash, ByHash>>,
    threads: usize,
    ahead: Arc<Ahead>,
    roots: Chunked<AtomicU32>,
}

impl Shared {
    fn read(
        &self,
        ptr: Ptr,
        id: u32,
        taint: &HashSet<Hash, ByHash>,
        s: &mut Scratch,
    ) -> Option<Node> {
        let mut taken = None;
        let checked = self.claims.slot(id).checked.load(Ordering::Acquire) == ptr.offset;
        let mark = (!checked).then_some((id, ptr.offset, false));
        let node = self.fetch(ptr, mark, taint, s, &mut taken);
        if s.bytes.capacity() > KEEP_BYTES {
            s.bytes = Vec::new();
        }
        drop(taken);
        node
    }

    fn fetch<'a>(
        &'a self,
        ptr: Ptr,
        mark: Option<Mark>,
        taint: &HashSet<Hash, ByHash>,
        s: &mut Scratch,
        taken: &mut Option<Taken<'a>>,
    ) -> Option<Node> {
        if ptr.offset == 0 {
            return None;
        }
        if ptr.offset >= self.source.written()
            && !s.unchecked.checks.is_empty()
            && !self.flush(&mut s.unchecked)
        {
            return None;
        }
        let mut links = s.spare.pop().unwrap_or_default();
        let unchecked = &mut s.unchecked;
        let claims = |claim: usize| {
            if claim > CHECK_PAST && !unchecked.checks.is_empty() && !self.flush(unchecked) {
                return Err(Error::Corrupt(
                    "a record read before failed its check".into(),
                ));
            }
            *taken = Some(self.budget.take(claim as u64));
            Ok(())
        };
        let (record, stored) = self
            .source
            .read_stored(ptr, &mut s.bytes, &mut links, claims)
            .ok()?;
        if let Some(mark) = mark {
            if !Checks::queues(stored.raw, links.len())
                && !s.unchecked.checks.is_empty()
                && !self.flush(&mut s.unchecked)
            {
                return None;
            }
            if stored.packed && stored.raw > record.len() && record.len() + stored.raw > UNCOUNTED {
                *taken = None;
                *taken = Some(self.budget.take((record.len() + stored.raw) as u64));
            }
            let unchecked = &mut s.unchecked;
            match unchecked.checks.queue(record, &stored, &links, ptr).ok()? {
                true => self.mark(mark),
                false => unchecked.marks.push(mark),
            }
            if unchecked.checks.full() && !self.flush(unchecked) {
                return None;
            }
        }
        let len = u32::try_from(stored.len).ok()?;
        if links
            .iter()
            .any(|link| link.offset == 0 || taint.contains(&link.hash))
        {
            return None;
        }
        let payload = stored.payload.len();
        let flagged = (links.len() as u64) << 1 | u64::from(stored.packed);
        let raw = match stored.packed {
            true => varint_len(stored.raw as u64),
            false => 0,
        };
        let fixed = varint_len(payload as u64) + varint_len(flagged) + raw + payload;
        let fixed = fixed.checked_add(links.len().checked_mul(HASH_BYTES)?)?;
        Some(Node {
            src: ptr.offset,
            fixed: u32::try_from(fixed).ok()?,
            len,
            links,
        })
    }

    fn check_ahead(&self, root: Ptr, s: &mut Scratch) {
        let mut stack = mem::take(&mut s.checks);
        stack.push(root);
        let mut count = 0;
        let mut safe = 0;
        s.unchecked.failed = false;
        while let Some(ptr) = stack.pop() {
            if !self.checking.load(Ordering::Relaxed) || self.cancel.load(Ordering::Relaxed) {
                stack.clear();
                break;
            }
            safe = safe.min(stack.len());
            self.check_one(ptr, &mut stack, s);
            count += 1;
            if s.unchecked.failed {
                stack.clear();
                break;
            }
            if s.unchecked.checks.is_empty() {
                safe = stack.len();
            }
            if stack.len() >= 2 * SHARE || count % SHARE == 0 {
                let mut queue = self.queue.lock();
                let busy =
                    !queue.jobs.is_empty() || !queue.units.is_empty() || !queue.reads.is_empty();
                if busy {
                    drop(queue);
                    if !self.flush(&mut s.unchecked) {
                        stack.clear();
                        break;
                    }
                    safe = stack.len();
                    queue = self.queue.lock();
                }
                let share = match busy {
                    true => stack.len(),
                    false => (stack.len() / 2).min(safe),
                };
                if share > 0 && queue.checks.len() + share <= CHECKS {
                    queue.checks.extend(stack.drain(..share));
                    safe -= share;
                    self.wake(share);
                }
                if busy {
                    stack.clear();
                    break;
                }
            }
        }
        s.checks = stack;
    }

    fn check_one(&self, ptr: Ptr, stack: &mut Vec<Ptr>, s: &mut Scratch) {
        if ptr.offset == 0 {
            return;
        }
        let id = self.claims.id(&ptr.hash, &mut s.seen);
        let started = self.claims.slot(id).checked.compare_exchange(
            0,
            CHECKING,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
        if started.is_err() {
            return;
        }
        let mut taken = None;
        let mark = Some((id, ptr.offset, true));
        let node = self.fetch(ptr, mark, &self.clean, s, &mut taken);
        if s.bytes.capacity() > KEEP_BYTES {
            s.bytes = Vec::new();
        }
        drop(taken);
        if let Some(node) = node {
            stack.extend(node.links.iter().rev());
            s.recycle(node.links, Vec::new());
        }
    }

    fn mark(&self, (id, offset, own): Mark) {
        let checked = &self.claims.slot(id).checked;
        match own {
            true => checked.store(offset, Ordering::Release),
            false => {
                let _ = checked.compare_exchange(0, offset, Ordering::AcqRel, Ordering::Relaxed);
            }
        }
    }

    /// Checks the records `unchecked` holds, marks the slot of each that passed as checked, and
    /// publishes its units, as discovered if every record passed and as tainted otherwise, handing
    /// out the units they were split into only in the first case. Returns whether every record
    /// passed.
    fn flush(&self, unchecked: &mut Unchecked) -> bool {
        let Unchecked {
            checks,
            marks,
            units,
            failed,
        } = unchecked;
        let checked = panic::catch_unwind(AssertUnwindSafe(|| {
            checks.check(|index| self.mark(marks[index]))
        }));
        let passed = matches!(checked, Ok(Ok(())));
        if checked.is_err() {
            *checks = Checks::new(CHECK_BYTES);
        }
        marks.clear();
        *failed |= !passed;
        if units.is_empty() {
            return passed;
        }
        for (unit, found) in units.drain(..) {
            let found = match passed {
                true => found,
                false => Found::Tainted,
            };
            if let Found::Copied { children, .. } = &found
                && !children.is_empty()
            {
                let mut queue = self.queue.lock();
                queue.units.extend(children.iter().cloned().map(Queued::of));
                self.wake(children.len());
            }
            let _ = unit.found.set(found);
        }
        self.try_place();
        passed
    }

    fn split(&self, unit: &Unit, node: &Node, ids: &[u32]) -> Option<Vec<Arc<Unit>>> {
        if unit.depth >= LEVELS || node.links.len() < 2 || node.links.len() > MAX_CHILDREN {
            return None;
        }
        let open: Vec<(Key, Ptr)> = node
            .links
            .iter()
            .zip(ids)
            .enumerate()
            .filter(|(index, (_, id))| {
                self.claims.held(**id) > child_key(unit.key, unit.depth, *index)
            })
            .map(|(index, (link, _))| (child_key(unit.key, unit.depth, index), *link))
            .collect();
        if open.len() < 2 {
            return None;
        }
        let units = open.into_iter().map(|(key, root)| {
            Arc::new(Unit {
                key,
                index: 0,
                root,
                depth: unit.depth + 1,
                found: OnceLock::new(),
            })
        });
        Some(units.collect())
    }

    fn discover(&self, unit: &Unit, s: &mut Scratch) -> Option<Found> {
        let mut records = Records::default();
        let (mut id, won) = self.claims.claim(&unit.root.hash, unit.key, &mut s.seen);
        if unit.depth == 0 {
            self.roots.get(unit.index).store(id + 1, Ordering::Release);
        }
        if !won {
            return Some(Found::Copied {
                children: Vec::new(),
                records,
            });
        }
        let taint = self.taint.load();
        let mut node = self.read(unit.root, id, &taint, s)?;
        let mut chain = Vec::new();
        while let [link] = node.links[..] {
            let (child, won) = self.claims.claim(&link.hash, unit.key, &mut s.seen);
            if !won {
                break;
            }
            let next = self.read(link, child, &taint, s)?;
            chain.push((id, mem::replace(&mut node, next), child));
            id = child;
        }
        let mut ids = s.ids.pop().unwrap_or_default();
        self.claims.number(&node.links, &mut ids, &mut s.seen);
        let children = match self.split(unit, &node, &ids) {
            Some(children) => {
                records.push(id, &node, &ids)?;
                s.recycle(node.links, ids);
                children
            }
            None => {
                self.descend(unit, &taint, (id, node, ids), &mut records, s)?;
                Vec::new()
            }
        };
        for (id, node, child) in chain.into_iter().rev() {
            records.push(id, &node, &[child])?;
        }
        Some(Found::Copied { children, records })
    }

    fn descend(
        &self,
        unit: &Unit,
        taint: &HashSet<Hash, ByHash>,
        root: (u32, Node, Vec<u32>),
        records: &mut Records,
        s: &mut Scratch,
    ) -> Option<()> {
        let mut stack = vec![(root, 0)];
        while let Some(((_, node, ids), next)) = stack.last_mut() {
            if let Some(&child) = ids.get(*next) {
                if self.cancel.load(Ordering::Relaxed) {
                    return None;
                }
                let link = node.links[*next];
                *next += 1;
                if self.claims.take(child, unit.key) {
                    let node = self.read(link, child, taint, s)?;
                    let mut ids = s.ids.pop().unwrap_or_default();
                    self.claims.number(&node.links, &mut ids, &mut s.seen);
                    stack.push(((child, node, ids), 0));
                }
                continue;
            }
            let ((id, node, ids), _) = stack.pop().expect("the stack is not empty");
            records.push(id, &node, &ids)?;
            s.recycle(node.links, ids);
        }
        Some(())
    }

    fn place(&self, placer: &mut Placer) -> Result<Progress> {
        if let Some(failed) = &placer.failed {
            return Err(Error::Corrupt(failed.clone()));
        }
        loop {
            if placer.cursor.is_empty() {
                let Some(unit) = self.dispatched.lock().pop_front() else {
                    return Ok(Progress::Done);
                };
                placer.cursor.push((unit, 0));
            }
            let (unit, next) = placer.cursor.last_mut().expect("the cursor is not empty");
            let unit = unit.clone();
            let Some(found) = unit.found.get() else {
                return Ok(Progress::Waiting);
            };
            let (children, records) = match found {
                Found::Tainted => return Ok(Progress::Tainted(unit)),
                Found::Copied { children, records } => (children, records),
            };
            if let Some(child) = children.get(*next) {
                *next += 1;
                placer.cursor.push((child.clone(), 0));
                continue;
            }
            if let Err(error) = self.place_records(placer, &unit, records) {
                placer.failed = Some(error.to_string());
                return Err(error);
            }
            self.pop(placer);
        }
    }

    fn pop(&self, placer: &mut Placer) {
        placer.cursor.pop();
        if placer.cursor.is_empty() {
            self.placed.fetch_add(1, Ordering::Release);
        }
    }

    fn place_records(
        &self,
        placer: &mut Placer,
        unit: &Arc<Unit>,
        records: &Records,
    ) -> Result<()> {
        for (index, rec) in records.list.iter().enumerate() {
            let slot = self.claims.slot(rec.slot);
            if slot.claim.load(Ordering::Relaxed) != unit.key {
                continue;
            }
            let at = placer.end;
            let mut size = u64::from(rec.fixed);
            for id in records.links(index) {
                let target = self.claims.offset(*id).load(Ordering::Acquire);
                if target == 0 {
                    return Err(Error::Corrupt(format!(
                        "the copy reached the record at {} before a record it links",
                        rec.src
                    )));
                }
                size += varint_len(zigzag(at.wrapping_sub(target) as i64)) as u64;
            }
            self.claims.offset(rec.slot).store(at, Ordering::Release);
            placer.job.add(unit, index, at, size);
            placer.end += size;
            placer.objects += 1;
            if placer.job.len >= JOB_BYTES {
                self.close_job(placer);
            }
        }
        Ok(())
    }

    fn close_job(&self, placer: &mut Placer) {
        if placer.job.parts.is_empty() {
            return;
        }
        let job = mem::take(&mut placer.job);
        let mut queue = self.queue.lock();
        queue.jobs.push_back(job);
        queue.writing += 1;
        self.ready.notify_one();
    }

    fn try_place(&self) {
        loop {
            self.again.store(true, Ordering::SeqCst);
            let Some(mut placer) = self.placer.try_lock() else {
                return;
            };
            while self.again.swap(false, Ordering::SeqCst) {
                let _ = self.place(&mut placer);
            }
            drop(placer);
            if !self.again.load(Ordering::SeqCst) {
                return;
            }
        }
    }

    fn write(&self, job: &Job, s: &mut Scratch) -> Result<()> {
        let held = job.len.saturating_mul(2);
        let taken = self.budget.take(held);
        if s.out.capacity() == 0
            && let Some(spare) = self.outbox.lock().spare.pop()
        {
            s.out = spare;
        }
        let built = self.write_job(job, s);
        if s.bytes.capacity() > KEEP_BYTES {
            s.bytes = Vec::new();
        }
        built?;
        mem::forget(taken);
        let filled = Filled {
            bytes: mem::take(&mut s.out),
            start: job.start,
            held,
        };
        self.outbox.lock().filled.push(filled);
        self.write_out();
        Ok(())
    }

    fn written_one(&self) {
        let mut queue = self.queue.lock();
        queue.writing -= 1;
        if !queue.jobs.is_empty() {
            self.ready.notify_one();
        }
    }

    fn write_out(&self) {
        while let Some(file) = self.writing.try_lock() {
            loop {
                let Some(filled) = self.outbox.lock().filled.pop() else {
                    break;
                };
                let written = self.file.write_all_at(&filled.bytes, filled.start);
                match written {
                    Ok(()) => start_writeback(&self.file, filled.start, filled.bytes.len() as u64),
                    Err(error) => self.fail(Failure::Failed(error.into())),
                }
                self.budget.give(filled.held);
                let mut bytes = filled.bytes;
                if bytes.capacity() <= 2 * KEEP_BYTES {
                    bytes.clear();
                    let mut outbox = self.outbox.lock();
                    if outbox.spare.len() < SPARE_OUT {
                        outbox.spare.push(bytes);
                    }
                }
                self.written_one();
            }
            drop(file);
            if self.outbox.lock().filled.is_empty() {
                return;
            }
        }
    }

    fn write_job(&self, job: &Job, s: &mut Scratch) -> Result<()> {
        s.out.clear();
        s.out.reserve(job.len as usize);
        for (unit, range) in &job.parts {
            let Some(Found::Copied { records, .. }) = unit.found.get() else {
                return Err(Error::Corrupt(
                    "a write job names a unit it cannot copy".into(),
                ));
            };
            let mut span = 0..0;
            for index in range.clone() {
                let rec = &records.list[index];
                let slot = self.claims.slot(rec.slot);
                if slot.claim.load(Ordering::Relaxed) != unit.key {
                    continue;
                }
                let at = self.claims.offset(rec.slot).load(Ordering::Acquire);
                if at != job.start + s.out.len() as u64 {
                    return Err(misplaced(rec.src));
                }
                let ids = records.links(index);
                let end = rec.src.saturating_add(u64::from(rec.len));
                if rec.src < span.start || end > span.end {
                    let mut last = end;
                    for next in &records.list[index + 1..range.end] {
                        if next.src != last || last - rec.src >= SPAN_BYTES {
                            break;
                        }
                        last = last.saturating_add(u64::from(next.len));
                    }
                    s.bytes.resize((last - rec.src) as usize, 0);
                    self.source.read_exact(rec.src, &mut s.bytes)?;
                    span = rec.src..last;
                }
                let mut from = (rec.src - span.start) as usize;
                let record = &s.bytes[from..from + rec.len as usize];
                let stored = match stored_within(record, rec.src, &mut s.links) {
                    Some(stored) => stored,
                    None => {
                        let ptr = Ptr {
                            hash: [0; HASH_BYTES],
                            offset: rec.src,
                        };
                        span = 0..0;
                        from = 0;
                        let (_, stored) =
                            self.source
                                .read_stored(ptr, &mut s.bytes, &mut s.links, |_| Ok(()))?;
                        stored
                    }
                };
                if stored.len != rec.len as usize || s.links.len() != ids.len() {
                    return Err(misplaced(rec.src));
                }
                s.new.clear();
                for (link, id) in s.links.iter().zip(ids) {
                    let offset = self.claims.offset(*id).load(Ordering::Acquire);
                    s.new.push(Ptr {
                        hash: link.hash,
                        offset,
                    });
                }
                let staged = Staged {
                    raw: stored.raw,
                    payload: &s.bytes[from + stored.payload.start..from + stored.payload.end],
                    packed: stored.packed,
                    links: &s.new,
                };
                append_staged(&mut s.out, at, staged);
            }
        }
        if s.out.len() as u64 != job.len {
            return Err(misplaced(job.start));
        }
        Ok(())
    }

    fn fail(&self, failure: Failure) {
        let mut slot = self.failure.lock();
        if slot.is_none() {
            *slot = Some(failure);
        }
    }

    fn check(&self) -> Result<()> {
        match self.failure.lock().take() {
            None => Ok(()),
            Some(Failure::Failed(error)) => Err(error),
            Some(Failure::Panicked(payload)) => panic::resume_unwind(payload),
        }
    }

    fn next(&self, holding: bool) -> Option<Item> {
        let mut queue = self.queue.lock();
        loop {
            if self.cancel.load(Ordering::Acquire) {
                return None;
            }
            if let Some(item) = queue.take() {
                return Some(item);
            }
            if holding {
                return Some(Item::Flush);
            }
            self.ready.wait(&mut queue);
        }
    }

    fn wake(&self, items: usize) {
        for _ in 0..items.min(self.threads) {
            if !self.ready.notify_one() {
                break;
            }
        }
    }

    fn run(&self, item: Item, s: &mut Scratch) {
        let flush = match item {
            Item::Unit(_) => false,
            Item::Flush => true,
            _ => !s.unchecked.units.is_empty(),
        };
        if flush {
            self.flush(&mut s.unchecked);
        }
        match item {
            Item::Unit(unit) => {
                let found = panic::catch_unwind(AssertUnwindSafe(|| self.discover(&unit, s)));
                let found = found.ok().flatten().unwrap_or(Found::Tainted);
                let split =
                    matches!(&found, Found::Copied { children, .. } if !children.is_empty());
                let unchecked = &mut s.unchecked;
                unchecked.units.push((unit, found));
                if split
                    || unchecked.checks.is_empty()
                    || unchecked.checks.full()
                    || unchecked.units.len() >= HOLD_UNITS
                {
                    self.flush(unchecked);
                }
            }
            Item::Flush => {}
            Item::Job(job) => {
                let failure = match panic::catch_unwind(AssertUnwindSafe(|| self.write(&job, s))) {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(Failure::Failed(error)),
                    Err(payload) => Some(Failure::Panicked(payload)),
                };
                if let Some(failure) = failure {
                    self.fail(failure);
                    self.written_one();
                }
            }
            Item::Check(ptr) => {
                let _ = panic::catch_unwind(AssertUnwindSafe(|| self.check_ahead(ptr, s)));
            }
            Item::Read(read) => {
                let source = &self.source;
                let ran = panic::catch_unwind(AssertUnwindSafe(|| {
                    self.ahead.read(source, &read, &self.cancel);
                }));
                if ran.is_err() {
                    self.ahead.finish(&read);
                }
            }
        }
        self.events.fetch_add(1, Ordering::SeqCst);
        if self.watching.load(Ordering::SeqCst) > 0 {
            let _moving = self.moving.lock();
            self.moved.notify_all();
        }
    }

    fn wait(&self, seen: u64, scratch: &mut Scratch, jobs_only: bool) {
        let mut queue = self.queue.lock();
        let item = match jobs_only {
            true => queue.jobs.pop_front().map(Item::Job),
            false => queue.work(),
        };
        if let Some(item) = item {
            drop(queue);
            self.run(item, scratch);
            return;
        }
        drop(queue);
        if scratch.unchecked.holds() {
            self.flush(&mut scratch.unchecked);
            return;
        }
        let mut moving = self.moving.lock();
        self.watching.fetch_add(1, Ordering::SeqCst);
        if self.events.load(Ordering::SeqCst) == seen {
            self.moved.wait_for(&mut moving, WAIT);
        }
        self.watching.fetch_sub(1, Ordering::SeqCst);
    }
}

fn misplaced(at: u64) -> Error {
    Error::Corrupt(format!(
        "the copy of the record at {at} did not land where it was placed"
    ))
}

fn interrupted(stop: Option<&AtomicBool>) -> Result<()> {
    match stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
        true => Err(Error::Interrupted),
        false => Ok(()),
    }
}

/// The units of one copy and the threads that discover and write them.
pub(super) struct Units {
    shared: Arc<Shared>,
    threads: usize,
    handles: Vec<JoinHandle<()>>,
    started: bool,
    index: u32,
    outstanding: usize,
    dispatched: u64,
    batch: Vec<Arc<Unit>>,
    checks: Vec<Ptr>,
    scratch: Scratch,
}

impl Units {
    /// Units that copy from `source` into `file` on `threads` threads, the walk's included, which
    /// start with the first unit dispatched. With 1 the walk discovers, places and writes every
    /// unit itself while it settles them.
    pub(super) fn new(source: Store, file: Arc<Traced>, threads: usize) -> Units {
        Units {
            shared: Arc::new(Shared {
                source,
                budget: Budget::default(),
                file,
                claims: Claims::new(),
                dispatched: Mutex::default(),
                placed: AtomicU64::new(0),
                writing: Mutex::new(()),
                outbox: Mutex::default(),
                queue: Mutex::new(Queue {
                    building: threads,
                    ..Queue::default()
                }),
                ready: Condvar::new(),
                moving: Mutex::new(()),
                moved: Condvar::new(),
                events: AtomicU64::new(0),
                watching: AtomicU64::new(0),
                placer: Mutex::default(),
                again: AtomicBool::new(false),
                failure: Mutex::default(),
                cancel: AtomicBool::new(false),
                checking: AtomicBool::new(false),
                clean: HashSet::default(),
                taint: ArcSwap::default(),
                threads,
                ahead: Arc::default(),
                roots: Chunked::new(),
            }),
            threads,
            handles: Vec::new(),
            started: false,
            index: 0,
            outstanding: 0,
            dispatched: 0,
            batch: Vec::new(),
            checks: Vec::new(),
            scratch: Scratch::default(),
        }
    }

    /// Sets the hashes that taint a unit discovered from now on: those of log entries and of
    /// pruned history. The walk sets them while no unit it dispatched is left to place.
    pub(super) fn taint(&mut self, hashes: HashSet<Hash, ByHash>) {
        self.shared.taint.store(Arc::new(hashes));
    }

    /// The number of units dispatched since the walk last settled them all.
    pub(super) fn outstanding(&self) -> usize {
        self.outstanding
    }

    /// Counts `bytes` the walk reads itself against what the threads may hold at once, until the
    /// result is dropped.
    pub(super) fn reserve(&self, bytes: u64) -> Reserved {
        self.shared.budget.hold(bytes);
        Reserved {
            shared: self.shared.clone(),
            bytes,
        }
    }

    /// The number of units dispatched since the copy began.
    pub(super) fn handed_out(&self) -> u64 {
        u64::from(self.index)
    }

    /// The number of dispatched units not yet placed.
    pub(super) fn unplaced(&self) -> u64 {
        self.dispatched - self.shared.placed.load(Ordering::Acquire)
    }

    fn publish(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        self.shared
            .dispatched
            .lock()
            .extend(self.batch.iter().cloned());
        let count = self.batch.len();
        let mut queue = self.shared.queue.lock();
        queue.units.extend(self.batch.drain(..).map(Queued::of));
        self.shared.wake(count);
    }

    /// Queues the tree at `root` to have its records' hashes checked by threads with nothing else
    /// to do, before any unit reaches them, starting the threads if this is the first. A unit then
    /// takes a record checked at the offset it reaches it by as checked.
    pub(super) fn check_ahead(&mut self, root: Ptr) {
        if self.threads <= 1 {
            return;
        }
        if !self.started {
            self.start();
        }
        self.checks.push(root);
        if self.checks.len() >= BATCH {
            self.publish_checks();
        }
    }

    /// Has the copy's other threads read the entries of `chain` ahead of the walk, which takes them
    /// through [`Units::ahead`], until [`Units::end_ahead`].
    pub(super) fn read_ahead(&mut self, chain: &Chain) {
        if self.threads <= 1 {
            return;
        }
        if !self.started {
            self.start();
        }
        let reads = self.shared.ahead.begin(chain, self.handles.len());
        if reads.is_empty() {
            return;
        }
        let count = reads.len();
        let mut queue = self.shared.queue.lock();
        queue.reads.extend(reads);
        self.shared.wake(count);
    }

    /// Stops the reading [`Units::read_ahead`] began and drops what it read.
    pub(super) fn end_ahead(&mut self) {
        self.shared.ahead.end();
    }

    /// The chain entries the copy's other threads read ahead of the walk.
    pub(super) fn ahead(&self) -> Arc<Ahead> {
        self.shared.ahead.clone()
    }

    /// Hands the trees queued by [`Units::check_ahead`] to the threads.
    pub(super) fn publish_checks(&mut self) {
        if self.checks.is_empty() {
            return;
        }
        self.shared.checking.store(true, Ordering::Relaxed);
        let mut queue = self.shared.queue.lock();
        if queue.checks.len() < CHECKS {
            let count = self.checks.len();
            queue.checks.extend(self.checks.drain(..));
            self.shared.wake(count);
        }
        self.checks.clear();
    }

    /// The key the walk claims the objects it writes itself with, after every unit dispatched.
    pub(super) fn walk_key(&self) -> Key {
        root_key(self.index)
    }

    /// The offset in the copy of the object with `hash`, if a unit or the walk has placed it.
    pub(super) fn placed(&self, hash: &Hash) -> Option<u64> {
        self.shared.claims.placed(hash)
    }

    /// Records that the walk wrote the object with `hash` at `offset`, holding it with `key`.
    pub(super) fn place(&self, hash: &Hash, key: Key, offset: u64) {
        self.shared.claims.place(hash, key, offset);
    }

    fn start(&mut self) {
        self.started = true;
        for _ in 1..self.threads {
            let shared = self.shared.clone();
            let spawned = thread::Builder::new()
                .name("ferro-copy".to_string())
                .spawn(move || {
                    let mut scratch = Scratch::default();
                    while let Some(item) = shared.next(scratch.unchecked.holds()) {
                        shared.run(item, &mut scratch);
                    }
                });
            match spawned {
                Ok(handle) => self.handles.push(handle),
                Err(_) => break,
            }
        }
    }

    /// Where the root of the unit that [`Units::dispatch`] returned `pending` for was placed, if
    /// its discovery found it and it is placed.
    pub(super) fn pending(&self, pending: u64) -> Option<u64> {
        let index = u32::try_from(u64::MAX - pending).ok()?;
        let id = self
            .shared
            .roots
            .get(index)
            .load(Ordering::Acquire)
            .checked_sub(1)?;
        let offset = self.shared.claims.offset(id).load(Ordering::Acquire);
        (offset != 0).then_some(offset)
    }

    /// Starts fetching into the cache what [`Units::placed`] reads first for `hash`.
    pub(super) fn prefetch(&self, hash: &Hash) {
        self.shared.claims.ids.prefetch(hash);
    }

    /// Starts fetching into the cache what [`Units::pending`] reads first for `link`, a link the
    /// walk wrote into a copy, if it holds an offset [`Units::dispatch`] returned, and otherwise
    /// what [`Units::placed`] reads first for its hash.
    ///
    /// A link read from the source goes to [`Units::prefetch`] instead, since its offset is
    /// whatever the record says, and one in the range of pending offsets would allocate the
    /// chunk of roots it names.
    pub(super) fn prefetch_copy(&self, link: Ptr) {
        match u32::try_from(u64::MAX - link.offset) {
            Ok(index) if link.offset >= PENDING => {
                prefetch(std::ptr::from_ref(self.shared.roots.get(index)).addr());
            }
            _ => self.prefetch(&link.hash),
        }
    }

    /// Hands the tree at `root` to the threads as the next unit, with the copy ending at `end`
    /// when no unit is outstanding, and returns the offset a link to it holds until it is placed,
    /// for [`Units::pending`].
    pub(super) fn dispatch(&mut self, root: Ptr, end: u64) -> u64 {
        if !self.started {
            self.start();
        }
        let first = self.outstanding == 0;
        self.index += 1;
        self.outstanding += 1;
        let unit = Arc::new(Unit {
            key: root_key(self.index),
            index: self.index,
            root,
            depth: 0,
            found: OnceLock::new(),
        });
        if first {
            self.shared.placer.lock().end = end;
        }
        self.dispatched += 1;
        self.batch.push(unit);
        if self.batch.len() >= BATCH {
            self.publish();
        }
        u64::MAX - u64::from(self.index)
    }

    /// Places units in order, discovering and writing them on this thread too while it waits,
    /// until every unit is placed, one is tainted, or, when `until` is not 0, no more than
    /// `until` units are left.
    ///
    /// # Errors
    ///
    /// [`Error::Interrupted`] once `stop` is set, and the first error a write job met.
    pub(super) fn settle(&mut self, stop: Option<&AtomicBool>, until: usize) -> Result<Settled> {
        let settled = self.placing(stop, until);
        if self.scratch.unchecked.holds() {
            self.shared.flush(&mut self.scratch.unchecked);
        }
        settled
    }

    fn placing(&mut self, stop: Option<&AtomicBool>, until: usize) -> Result<Settled> {
        self.publish();
        let shared = self.shared.clone();
        loop {
            interrupted(stop)?;
            shared.check()?;
            let seen = shared.events.load(Ordering::SeqCst);
            {
                let mut placer = shared.placer.lock();
                match shared.place(&mut placer)? {
                    Progress::Done => {
                        shared.close_job(&mut placer);
                        self.outstanding = 0;
                        let objects = mem::take(&mut placer.objects);
                        return Ok(Settled::Done {
                            end: placer.end,
                            objects,
                        });
                    }
                    Progress::Tainted(unit) => {
                        shared.close_job(&mut placer);
                        return Ok(Settled::Tainted {
                            unit,
                            end: placer.end,
                        });
                    }
                    Progress::Waiting if until > 0 && self.unplaced() <= until as u64 => {
                        return Ok(Settled::Ahead);
                    }
                    Progress::Waiting => {}
                }
            }
            shared.wait(seen, &mut self.scratch, false);
        }
    }

    /// Records that the walk copied the tainted unit [`Units::settle`] returned, ending at `end`.
    pub(super) fn resolved(&mut self, end: u64) {
        let mut placer = self.shared.placer.lock();
        self.shared.pop(&mut placer);
        placer.end = end;
    }

    /// Waits for every placed record to be written.
    ///
    /// # Errors
    ///
    /// [`Error::Interrupted`] once `stop` is set, and the first error a write job met.
    pub(super) fn written(&mut self, stop: Option<&AtomicBool>) -> Result<()> {
        let shared = self.shared.clone();
        loop {
            interrupted(stop)?;
            shared.check()?;
            let seen = shared.events.load(Ordering::SeqCst);
            if shared.queue.lock().writing == 0 {
                break;
            }
            shared.wait(seen, &mut self.scratch, true);
        }
        shared.checking.store(false, Ordering::Relaxed);
        shared.queue.lock().checks.clear();
        shared.check()
    }
}

impl Drop for Units {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Release);
        {
            let _queue = self.shared.queue.lock();
            self.shared.ready.notify_all();
        }
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reclaim::Copier;
    use crate::store::StoreOptions;

    #[test]
    fn fetching_ahead_for_links_read_from_the_source_allocates_no_roots() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) =
            Store::open(&dir.path().join("db.ferro"), StoreOptions::default()).unwrap();
        let options = StoreOptions {
            copy_threads: 2,
            ..StoreOptions::default()
        };
        let copier = Copier::create(&store, &dir.path().join("copy"), &options, None).unwrap();
        let start: u64 = 4096;
        let links: Vec<Ptr> = (0..64u64)
            .map(|k| Ptr {
                hash: [0; HASH_BYTES],
                offset: start.wrapping_sub(start + 1 + k * CHUNK as u64),
            })
            .collect();
        copier.sink.prefetch(&links);
        let units = copier
            .sink
            .units
            .as_ref()
            .expect("a copy on two threads has units");
        let allocated = units
            .shared
            .roots
            .chunks
            .iter()
            .filter(|chunk| chunk.get().is_some())
            .count();
        assert_eq!(
            allocated, 0,
            "{allocated} chunks of 256 KiB allocated by prefetches"
        );
    }
}
