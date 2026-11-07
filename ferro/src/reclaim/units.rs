//! The trees that log entries root, copied on several threads into the bytes one thread writes.
//!
//! The walk hands each tree it reaches through an entry's root link to [`Units::dispatch`] as a
//! unit and goes on down the chain. Worker threads discover units: they read each record, check
//! its hash, claim the objects its links name, and list the records the unit copies in the order
//! one thread writes them, children first. A unit whose root has two or more children that no
//! earlier unit holds is split, near the top of the tree, into a unit for each such child and one
//! for the root. Every claim carries its unit's [`Key`], which orders units as one thread reaches
//! them, and the smallest key holds an object, so two units that reach one object agree on which
//! copies it whichever thread got there first. Units are placed in key order: each record its unit
//! still holds takes the next offset, and its length follows from the offsets of its links, which
//! are all placed before it. Workers then write placed records in jobs of at least 256 KiB,
//! reading each one again. A unit that reaches anything the walk treats differently, such as a log
//! entry, a pruned hash or a link at offset 0, or whose discovery fails, is tainted, and the walk
//! copies it itself where it falls in the order.

use std::any::Any;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::mem;
use std::ops::Range;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use super::ByHash;
use crate::codec::{HASH_BYTES, Hash, Ptr, Staged};
use crate::error::{Error, Result};
use crate::platform::start_writeback;
use crate::store::{
    Store, Traced, Unpacked, append_staged, stored_within, varint_len, verify_stored, zigzag,
};

/// Where a unit falls in the order one thread copies in: the unit's place among those the walk
/// dispatched in the high 32 bits, then one byte for each level it was split at, the child's
/// index, with 255 below the last.
pub(super) type Key = u64;

/// The offset a link to a dispatched unit holds until the unit is placed.
pub(super) const PENDING: u64 = u64::MAX;
/// How many levels below a dispatched unit's root a unit may be split at.
const LEVELS: u32 = 3;
/// The most links a node split into units may have, so that each child's index fits its byte.
const MAX_CHILDREN: usize = 254;
const SHARDS: usize = 256;
const CHUNK: usize = 1 << 16;
const CHUNKS: usize = 1 << 16;
const JOB_BYTES: u64 = 256 << 10;
const KEEP_BYTES: usize = 1 << 20;
const WAIT: Duration = Duration::from_millis(10);
const BATCH: usize = 16;

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
    offset: AtomicU64,
}

impl Default for Slot {
    fn default() -> Slot {
        Slot {
            claim: AtomicU64::new(u64::MAX),
            offset: AtomicU64::new(0),
        }
    }
}

/// Which unit holds each object, by hash, and where each placed object sits in the copy.
///
/// A slot is found by hash under one of 256 locks, and read by its number without one.
struct Claims {
    shards: Box<[Mutex<HashMap<Hash, u32, ByHash>>]>,
    chunks: Box<[OnceLock<Box<[Slot]>>]>,
    next: AtomicU32,
}

impl Claims {
    fn new() -> Claims {
        Claims {
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
            chunks: (0..CHUNKS).map(|_| OnceLock::new()).collect(),
            next: AtomicU32::new(0),
        }
    }

    fn shard(&self, hash: &Hash) -> &Mutex<HashMap<Hash, u32, ByHash>> {
        &self.shards[usize::from(hash[8]) % SHARDS]
    }

    fn slot(&self, id: u32) -> &Slot {
        let id = id as usize;
        let chunk =
            self.chunks[id / CHUNK].get_or_init(|| (0..CHUNK).map(|_| Slot::default()).collect());
        &chunk[id % CHUNK]
    }

    fn insert(&self, map: &mut HashMap<Hash, u32, ByHash>, hash: &Hash) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(id < u32::MAX, "a copy of more than 2^32 objects");
        map.insert(*hash, id);
        id
    }

    fn claim(&self, hash: &Hash, key: Key) -> (u32, bool) {
        let mut map = self.shard(hash).lock();
        let id = match map.get(hash) {
            Some(id) => *id,
            None => self.insert(&mut map, hash),
        };
        let slot = self.slot(id);
        let won = key < slot.claim.load(Ordering::Relaxed);
        if won {
            slot.claim.store(key, Ordering::Relaxed);
        }
        (id, won)
    }

    fn id(&self, hash: &Hash) -> u32 {
        let mut map = self.shard(hash).lock();
        match map.get(hash) {
            Some(id) => *id,
            None => self.insert(&mut map, hash),
        }
    }

    fn held(&self, hash: &Hash) -> Key {
        let map = self.shard(hash).lock();
        map.get(hash)
            .map_or(u64::MAX, |id| self.slot(*id).claim.load(Ordering::Relaxed))
    }

    fn placed(&self, hash: &Hash) -> Option<u64> {
        let id = *self.shard(hash).lock().get(hash)?;
        let offset = self.slot(id).offset.load(Ordering::Acquire);
        (offset != 0).then_some(offset)
    }

    fn place(&self, hash: &Hash, key: Key, offset: u64) {
        let mut map = self.shard(hash).lock();
        let id = match map.get(hash) {
            Some(id) => *id,
            None => self.insert(&mut map, hash),
        };
        let slot = self.slot(id);
        slot.claim.store(key, Ordering::Relaxed);
        slot.offset.store(offset, Ordering::Release);
    }
}

/// A tree to copy: a root and what its discovery found.
pub(super) struct Unit {
    key: Key,
    root: Ptr,
    depth: u32,
    taint: Arc<HashSet<Hash, ByHash>>,
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

struct Queued(Arc<Unit>);

impl PartialEq for Queued {
    fn eq(&self, other: &Queued) -> bool {
        self.0.key == other.0.key
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
        other.0.key.cmp(&self.0.key)
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
}

#[derive(Default)]
struct Queue {
    units: BinaryHeap<Queued>,
    jobs: VecDeque<Job>,
    writing: usize,
    events: u64,
}

impl Queue {
    fn take(&mut self) -> Option<Item> {
        if let Some(job) = self.jobs.pop_front() {
            return Some(Item::Job(job));
        }
        self.units.pop().map(|queued| Item::Unit(queued.0))
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
    unpacked: Unpacked,
    spare: Vec<Vec<Ptr>>,
    ids: Vec<Vec<u32>>,
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

#[derive(Default)]
struct Budget {
    used: Mutex<u64>,
    freed: Condvar,
}

impl Budget {
    fn hold(&self, bytes: u64) {
        let mut used = self.used.lock();
        while *used > 0 && used.saturating_add(bytes) > BUDGET {
            self.freed.wait(&mut used);
        }
        *used += bytes;
    }

    fn give(&self, bytes: u64) {
        let mut used = self.used.lock();
        *used -= bytes;
        self.freed.notify_all();
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
    queue: Mutex<Queue>,
    ready: Condvar,
    moved: Condvar,
    placer: Mutex<Placer>,
    again: AtomicBool,
    failure: Mutex<Option<Failure>>,
    cancel: AtomicBool,
}

impl Shared {
    fn read(&self, ptr: Ptr, taint: &HashSet<Hash, ByHash>, s: &mut Scratch) -> Option<Node> {
        let mut taken = None;
        let node = self.fetch(ptr, taint, s, &mut taken);
        if s.bytes.capacity() > KEEP_BYTES {
            s.bytes = Vec::new();
        }
        drop(taken);
        node
    }

    fn fetch<'a>(
        &'a self,
        ptr: Ptr,
        taint: &HashSet<Hash, ByHash>,
        s: &mut Scratch,
        taken: &mut Option<Taken<'a>>,
    ) -> Option<Node> {
        if ptr.offset == 0 {
            return None;
        }
        let mut links = s.spare.pop().unwrap_or_default();
        let claims = |claim: usize| {
            *taken = Some(self.budget.take(claim as u64));
            Ok(())
        };
        let (record, stored) = self
            .source
            .read_stored(ptr, &mut s.bytes, &mut links, claims)
            .ok()?;
        if stored.packed && stored.raw > record.len() {
            *taken = None;
            *taken = Some(self.budget.take((record.len() + stored.raw) as u64));
        }
        verify_stored(record, ptr, &mut s.unpacked).ok()?;
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

    fn split(&self, unit: &Unit, node: &Node) -> Option<Vec<Arc<Unit>>> {
        if unit.depth >= LEVELS || node.links.len() < 2 || node.links.len() > MAX_CHILDREN {
            return None;
        }
        let open: Vec<(Key, Ptr)> = node
            .links
            .iter()
            .enumerate()
            .map(|(index, link)| (child_key(unit.key, unit.depth, index), *link))
            .filter(|(key, link)| self.claims.held(&link.hash) > *key)
            .collect();
        if open.len() < 2 {
            return None;
        }
        let units = open.into_iter().map(|(key, root)| {
            Arc::new(Unit {
                key,
                root,
                depth: unit.depth + 1,
                taint: unit.taint.clone(),
                found: OnceLock::new(),
            })
        });
        Some(units.collect())
    }

    fn discover(&self, unit: &Unit, s: &mut Scratch) -> Option<Found> {
        let mut records = Records::default();
        let (mut id, won) = self.claims.claim(&unit.root.hash, unit.key);
        if !won {
            return Some(Found::Copied {
                children: Vec::new(),
                records,
            });
        }
        let mut node = self.read(unit.root, &unit.taint, s)?;
        let mut chain = Vec::new();
        while let [link] = node.links[..] {
            let (child, won) = self.claims.claim(&link.hash, unit.key);
            if !won {
                break;
            }
            let next = self.read(link, &unit.taint, s)?;
            chain.push((id, mem::replace(&mut node, next), child));
            id = child;
        }
        let children = match self.split(unit, &node) {
            Some(children) => {
                let ids: Vec<u32> = node
                    .links
                    .iter()
                    .map(|link| self.claims.id(&link.hash))
                    .collect();
                records.push(id, &node, &ids)?;
                children
            }
            None => {
                self.descend(unit, id, node, &mut records, s)?;
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
        id: u32,
        root: Node,
        records: &mut Records,
        s: &mut Scratch,
    ) -> Option<()> {
        let ids = s.ids.pop().unwrap_or_default();
        let mut stack = vec![(id, root, ids)];
        while let Some((_, node, ids)) = stack.last_mut() {
            if let Some(link) = node.links.get(ids.len()).copied() {
                if self.cancel.load(Ordering::Relaxed) {
                    return None;
                }
                let (child, won) = self.claims.claim(&link.hash, unit.key);
                ids.push(child);
                if won {
                    let node = self.read(link, &unit.taint, s)?;
                    let ids = s.ids.pop().unwrap_or_default();
                    stack.push((child, node, ids));
                }
                continue;
            }
            let (id, node, ids) = stack.pop().expect("the stack is not empty");
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
                let target = self.claims.slot(*id).offset.load(Ordering::Acquire);
                if target == 0 {
                    return Err(Error::Corrupt(format!(
                        "the copy reached the record at {} before a record it links",
                        rec.src
                    )));
                }
                size += varint_len(zigzag(at.wrapping_sub(target) as i64)) as u64;
            }
            slot.offset.store(at, Ordering::Release);
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
        let taken = self.budget.take(job.len.saturating_mul(2));
        let written = self.write_job(job, s);
        if s.out.capacity() > 2 * KEEP_BYTES {
            s.out = Vec::new();
        }
        if s.bytes.capacity() > KEEP_BYTES {
            s.bytes = Vec::new();
        }
        drop(taken);
        written
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
            for index in range.clone() {
                let rec = &records.list[index];
                let slot = self.claims.slot(rec.slot);
                if slot.claim.load(Ordering::Relaxed) != unit.key {
                    continue;
                }
                let at = slot.offset.load(Ordering::Acquire);
                if at != job.start + s.out.len() as u64 {
                    return Err(misplaced(rec.src));
                }
                let ids = records.links(index);
                s.bytes.resize(rec.len as usize, 0);
                self.source.read_exact(rec.src, &mut s.bytes)?;
                let stored = match stored_within(&s.bytes, rec.src, &mut s.links) {
                    Some(stored) => stored,
                    None => {
                        let ptr = Ptr {
                            hash: [0; HASH_BYTES],
                            offset: rec.src,
                        };
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
                    let offset = self.claims.slot(*id).offset.load(Ordering::Acquire);
                    s.new.push(Ptr {
                        hash: link.hash,
                        offset,
                    });
                }
                let staged = Staged {
                    raw: stored.raw,
                    payload: &s.bytes[stored.payload.clone()],
                    packed: stored.packed,
                    links: &s.new,
                };
                append_staged(&mut s.out, at, staged);
            }
        }
        if s.out.len() as u64 != job.len {
            return Err(misplaced(job.start));
        }
        {
            let _writing = self.writing.lock();
            self.file.write_all_at(&s.out, job.start)?;
            start_writeback(&self.file, job.start, job.len);
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

    fn next(&self) -> Option<Item> {
        let mut queue = self.queue.lock();
        loop {
            if self.cancel.load(Ordering::Acquire) {
                return None;
            }
            if let Some(item) = queue.take() {
                return Some(item);
            }
            self.ready.wait(&mut queue);
        }
    }

    fn run(&self, item: Item, s: &mut Scratch) {
        match item {
            Item::Unit(unit) => {
                let found = panic::catch_unwind(AssertUnwindSafe(|| self.discover(&unit, s)));
                let found = found.ok().flatten().unwrap_or(Found::Tainted);
                if let Found::Copied { children, .. } = &found
                    && !children.is_empty()
                {
                    let mut queue = self.queue.lock();
                    queue.units.extend(children.iter().cloned().map(Queued));
                    self.ready.notify_all();
                }
                let _ = unit.found.set(found);
                self.try_place();
            }
            Item::Job(job) => {
                match panic::catch_unwind(AssertUnwindSafe(|| self.write(&job, s))) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => self.fail(Failure::Failed(error)),
                    Err(payload) => self.fail(Failure::Panicked(payload)),
                }
                self.queue.lock().writing -= 1;
            }
        }
        let mut queue = self.queue.lock();
        queue.events += 1;
        self.moved.notify_all();
    }

    fn wait(&self, seen: u64, scratch: &mut Scratch, jobs_only: bool) {
        let mut queue = self.queue.lock();
        let item = match jobs_only {
            true => queue.jobs.pop_front().map(Item::Job),
            false => queue.take(),
        };
        if let Some(item) = item {
            drop(queue);
            self.run(item, scratch);
            return;
        }
        if queue.events == seen {
            self.moved.wait_for(&mut queue, WAIT);
        }
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
    taint: Arc<HashSet<Hash, ByHash>>,
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
                queue: Mutex::default(),
                ready: Condvar::new(),
                moved: Condvar::new(),
                placer: Mutex::default(),
                again: AtomicBool::new(false),
                failure: Mutex::default(),
                cancel: AtomicBool::new(false),
            }),
            threads,
            handles: Vec::new(),
            started: false,
            index: 0,
            outstanding: 0,
            dispatched: 0,
            batch: Vec::new(),
            taint: Arc::default(),
            scratch: Scratch::default(),
        }
    }

    /// Sets the hashes that taint a unit dispatched from now on: those of log entries and of
    /// pruned history.
    pub(super) fn taint(&mut self, hashes: HashSet<Hash, ByHash>) {
        self.taint = Arc::new(hashes);
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
        let mut queue = self.shared.queue.lock();
        queue.units.extend(self.batch.drain(..).map(Queued));
        self.shared.ready.notify_all();
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
                    while let Some(item) = shared.next() {
                        shared.run(item, &mut scratch);
                    }
                });
            match spawned {
                Ok(handle) => self.handles.push(handle),
                Err(_) => break,
            }
        }
    }

    /// Hands the tree at `root` to the threads as the next unit, with the copy ending at `end`
    /// when no unit is outstanding.
    pub(super) fn dispatch(&mut self, root: Ptr, end: u64) {
        if !self.started {
            self.start();
        }
        let first = self.outstanding == 0;
        self.index += 1;
        self.outstanding += 1;
        let unit = Arc::new(Unit {
            key: root_key(self.index),
            root,
            depth: 0,
            taint: self.taint.clone(),
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
    }

    /// Places units in order, discovering and writing them on this thread too while it waits,
    /// until every unit is placed, one is tainted, or, when `until` is not 0, no more than
    /// `until` units are left.
    ///
    /// # Errors
    ///
    /// [`Error::Interrupted`] once `stop` is set, and the first error a write job met.
    pub(super) fn settle(&mut self, stop: Option<&AtomicBool>, until: usize) -> Result<Settled> {
        self.publish();
        let shared = self.shared.clone();
        loop {
            interrupted(stop)?;
            shared.check()?;
            let seen = shared.queue.lock().events;
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
            let seen = {
                let queue = shared.queue.lock();
                if queue.writing == 0 {
                    break;
                }
                queue.events
            };
            shared.wait(seen, &mut self.scratch, true);
        }
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
