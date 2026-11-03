use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

use crate::admin::Keep;
use crate::codec::{Data, Decoder, Encoder, Hash, Object, Ptr, Sink};
use crate::db::{Commit, Parts, now_ms};
use crate::error::{Error, Result};
use crate::store::Store;

const HAS_PREV: u8 = 1;
const HAS_SKIP: u8 = 2;
const WHOLE: u8 = 4;
const JOINED: u8 = 8;
const MERGED: u8 = 16;
const GRAPH: u8 = 32;
const BOUNDED: u8 = 64;
const EXTRA: u8 = 128;
const STAMPED: u64 = 1;
const KNOWN_EXTRA: u64 = STAMPED;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Merged {
    pub(crate) ptr: Ptr,
    pub(crate) floor: u64,
    pub(crate) after: Option<u64>,
}

impl Merged {
    pub(crate) fn after(&self, path: Option<u64>) -> Option<u64> {
        path.max(self.after)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Graph {
    pub(crate) lift: u64,
    pub(crate) reach: u64,
    pub(crate) last_merge: u64,
    pub(crate) bounded: bool,
}

#[derive(Clone)]
pub(crate) struct Entry {
    pub(crate) commit: Arc<Commit>,
    pub(crate) prev: Option<Ptr>,
    pub(crate) skip: Option<Ptr>,
    pub(crate) merged: Option<Merged>,
    pub(crate) graph: Graph,
}

impl Entry {
    pub(crate) fn generation(&self) -> u64 {
        self.commit.version + self.graph.lift
    }
}

impl Data for Entry {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.commit.encode(w)?;
        let parts = match &self.commit.parts {
            Parts::Lines => 0,
            Parts::Whole => WHOLE,
            Parts::Joined(_) => JOINED,
        };
        let graph = self.graph;
        let wide = (graph.lift, graph.reach, graph.last_merge) != (0, 0, 0);
        let extra = u64::from(self.commit.stamped) * STAMPED;
        let flags = u8::from(self.prev.is_some()) * HAS_PREV
            + u8::from(self.skip.is_some()) * HAS_SKIP
            + u8::from(self.merged.is_some()) * MERGED
            + u8::from(wide) * GRAPH
            + u8::from(graph.bounded) * BOUNDED
            + u8::from(extra != 0) * EXTRA;
        w.u8(flags | parts);
        if extra != 0 {
            w.varint(extra);
        }
        if let Parts::Joined(lengths) = &self.commit.parts {
            w.varint(lengths.len() as u64);
            for len in lengths.iter() {
                w.varint(*len as u64);
            }
        }
        if let Some(merged) = self.merged {
            w.varint(merged.floor);
            w.varint(merged.after.map_or(0, |after| after.saturating_add(1)));
        }
        if wide {
            w.varint(graph.lift);
            w.varint(graph.reach);
            w.varint(graph.last_merge);
        }
        if let Some(prev) = self.prev {
            w.link(prev);
        }
        if let Some(skip) = self.skip {
            w.link(skip);
        }
        if let Some(merged) = self.merged {
            w.link(merged.ptr);
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let mut commit = Commit::decode(r)?;
        let flags = r.u8()?;
        if flags & (WHOLE | JOINED) == WHOLE | JOINED {
            return Err(Error::Decode(format!("invalid log entry flags {flags}")));
        }
        let extra = if flags & EXTRA != 0 { r.varint()? } else { 0 };
        if extra & !KNOWN_EXTRA != 0 || (flags & EXTRA != 0 && extra == 0) {
            return Err(Error::Decode(format!("invalid log entry extra flags {extra}")));
        }
        if flags & WHOLE != 0 {
            commit.parts = Parts::Whole;
        }
        commit.stamped = extra & STAMPED != 0;
        if flags & JOINED != 0 {
            let count = r.varint()?;
            if count > commit.message.len() as u64 + 1 {
                return Err(Error::Decode(format!("{count} parts in a short commit message")));
            }
            let lengths = (0..count)
                .map(|_| Ok(usize::try_from(r.varint()?).unwrap_or(usize::MAX)))
                .collect::<Result<_>>()?;
            commit.parts = Parts::Joined(lengths);
            commit.parts.check(&commit.message)?;
        }
        let floor = match flags & MERGED {
            0 => None,
            _ => Some((r.varint()?, r.varint()?.checked_sub(1))),
        };
        let mut graph = Graph {
            bounded: flags & BOUNDED != 0,
            ..Graph::default()
        };
        if flags & GRAPH != 0 {
            graph.lift = r.varint()?;
            graph.reach = r.varint()?;
            graph.last_merge = r.varint()?;
        }
        let version = commit.version;
        let generation = version.checked_add(graph.lift);
        let merge_here = graph.last_merge == version;
        if generation.is_none_or(|generation| graph.reach >= generation)
            || graph.last_merge > version
            || floor.is_some() != merge_here
            || floor.is_some_and(|(floor, _)| floor == 0)
        {
            return Err(Error::Decode(format!("invalid merge graph in log entry {version}")));
        }
        let prev = if flags & HAS_PREV != 0 { Some(r.link()?) } else { None };
        let skip = if flags & HAS_SKIP != 0 { Some(r.link()?) } else { None };
        let merged = match floor {
            Some((floor, after)) => Some(Merged {
                ptr: r.link()?,
                floor,
                after,
            }),
            None => None,
        };
        Ok(Entry {
            commit: Arc::new(commit),
            prev,
            skip,
            merged,
            graph,
        })
    }
}

#[derive(Clone)]
pub(crate) struct Node {
    pub(crate) ptr: Ptr,
    pub(crate) entry: Arc<Entry>,
}

impl Node {
    pub(crate) fn load(store: &Store, ptr: Ptr) -> Result<Node> {
        Ok(Node {
            ptr,
            entry: store.load::<Entry>(ptr)?,
        })
    }

    pub(crate) fn version(&self) -> u64 {
        self.entry.commit.version
    }

    pub(crate) fn time(&self) -> u64 {
        self.entry.commit.time_ms
    }

    fn below(&self, store: &Store, link: Option<Ptr>) -> Result<Node> {
        let node = Node::load(store, link.ok_or_else(|| corrupt("chain ends early"))?)?;
        if node.version() >= self.version() {
            return Err(corrupt("versions do not descend"));
        }
        Ok(node)
    }

    pub(crate) fn down_to(&self, store: &Store, version: u64) -> Result<Node> {
        let mut node = self.clone();
        while node.version() > version {
            let link = match node.entry.skip {
                Some(skip) if skip_version(node.version()) >= version => Some(skip),
                _ => node.entry.prev,
            };
            node = node.below(store, link)?;
        }
        if node.version() != version {
            return Err(corrupt("skips a version"));
        }
        Ok(node)
    }

    pub(crate) fn last_where(
        &self,
        store: &Store,
        floor: u64,
        too_high: impl Fn(&Entry) -> bool,
    ) -> Result<Option<Node>> {
        let mut node = self.clone();
        while too_high(&node.entry) {
            let at = node.version();
            if at <= floor {
                return Ok(None);
            }
            if let Some(skip) = node.entry.skip
                && skip_version(at) >= floor
            {
                let far = node.below(store, Some(skip))?;
                if too_high(&far.entry) {
                    node = far;
                    continue;
                }
            }
            node = node.below(store, node.entry.prev)?;
        }
        Ok(Some(node))
    }

    pub(crate) fn lowest_kept(
        &self,
        store: &Store,
        floor: u64,
        kept: impl Fn(&Entry) -> bool,
    ) -> Result<u64> {
        let mut node = self.clone();
        loop {
            let at = node.version();
            if at <= floor {
                return Ok(at);
            }
            let far = node.entry.skip.filter(|skip| skip.offset != 0 && skip_version(at) >= floor);
            if let Some(far) = far.map(|skip| node.below(store, Some(skip))).transpose()?
                && kept(&far.entry)
            {
                node = far;
                continue;
            }
            match node.entry.prev.filter(|prev| prev.offset != 0) {
                Some(prev) => {
                    let next = node.below(store, Some(prev))?;
                    if !kept(&next.entry) {
                        return Ok(at);
                    }
                    node = next;
                }
                None => return Ok(at),
            }
        }
    }
}

type Seed = (u64, Option<u64>, Hash, u64);

#[derive(Default)]
pub(crate) struct Closures {
    queue: BinaryHeap<Reverse<Seed>>,
    walked: HashMap<Hash, Vec<(u64, Option<u64>)>>,
}

impl Closures {
    pub(crate) fn seed(&mut self, entry: &Entry, after: Option<u64>) {
        if let Some(merged) = entry.merged {
            let Ptr { hash, offset } = merged.ptr;
            self.queue.push(Reverse((merged.floor, merged.after(after), hash, offset)));
        }
    }

    pub(crate) fn mark(&mut self, ptr: Ptr, floor: u64, after: Option<u64>) {
        self.walked.entry(ptr.hash).or_default().push((floor, after));
    }

    fn covered(&self, hash: &Hash, floor: u64, after: Option<u64>) -> bool {
        let walks = self.walked.get(hash).map_or(&[][..], Vec::as_slice);
        walks.iter().any(|(walked, bound)| {
            *walked <= floor
                && match (bound, after) {
                    (None, _) => true,
                    (Some(_), None) => false,
                    (Some(bound), Some(after)) => bound <= &after,
                }
        })
    }

    pub(crate) fn walk(
        &mut self,
        store: &Store,
        mut each: impl FnMut(Ptr, &Entry, &Object, u32) -> Result<bool>,
    ) -> Result<()> {
        while let Some(Reverse((floor, after, hash, offset))) = self.queue.pop() {
            let mut next = Some(Ptr { hash, offset });
            let mut above = u64::MAX;
            while let Some(ptr) = next.take() {
                if ptr.offset == 0 || self.covered(&ptr.hash, floor, after) {
                    break;
                }
                let (object, len) = store.read_record(ptr)?;
                let entry = Entry::decode(&mut Decoder::new(&object.body, &object.links, None))?;
                let at = entry.commit.version;
                if at >= above {
                    return Err(corrupt("versions do not descend"));
                }
                let first = above == u64::MAX;
                above = at;
                if at < floor {
                    break;
                }
                if after.is_some_and(|after| entry.commit.time_ms < after) {
                    if first {
                        each(ptr, &entry, &object, len)?;
                    }
                    break;
                }
                self.walked.entry(ptr.hash).or_default().push((floor, after));
                if !each(ptr, &entry, &object, len)? {
                    break;
                }
                self.seed(&entry, after);
                next = entry.prev.filter(|_| at > floor);
            }
        }
        Ok(())
    }
}

fn skip_version(version: u64) -> u64 {
    version - (version & version.wrapping_neg())
}

fn corrupt(what: &str) -> Error {
    Error::Corrupt(format!("commit log {what}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chain {
    pub(crate) tip: Option<Ptr>,
    pub(crate) first: u64,
    pub(crate) last: u64,
}

impl Chain {
    pub(crate) fn empty() -> Self {
        Chain {
            tip: None,
            first: 0,
            last: 0,
        }
    }

    pub(crate) fn bounded(tip: Option<Ptr>, first: u64, last: u64) -> Result<Chain> {
        if tip.is_some() && (first == 0 || first > last) {
            return Err(corrupt(&format!("keeps versions {first} to {last}")));
        }
        Ok(Chain { tip, first, last })
    }

    pub(crate) fn len(&self) -> usize {
        match self.tip {
            Some(_) => (self.last - self.first + 1) as usize,
            None => 0,
        }
    }

    fn descend(
        &self,
        store: &Store,
        mut step: impl FnMut(&Entry) -> Step,
    ) -> Result<Option<Arc<Entry>>> {
        let Some(mut ptr) = self.tip else {
            return Ok(None);
        };
        let mut entry = store.load::<Entry>(ptr)?;
        loop {
            let next = match step(&entry) {
                Step::Found => return Ok(Some(entry)),
                Step::Missing => return Ok(None),
                Step::Skip => entry.skip,
                Step::Prev => entry.prev,
            };
            ptr = next.ok_or_else(|| corrupt("chain ends early"))?;
            let loaded = store.load::<Entry>(ptr)?;
            if loaded.commit.version >= entry.commit.version {
                return Err(corrupt("versions do not descend"));
            }
            entry = loaded;
        }
    }

    pub(crate) fn tip_entry(&self, store: &Store) -> Result<Option<Arc<Entry>>> {
        match self.tip {
            Some(ptr) => Ok(Some(store.load::<Entry>(ptr)?)),
            None => Ok(None),
        }
    }

    pub(crate) fn node_at(&self, store: &Store, version: u64) -> Result<Option<Node>> {
        let Some(tip) = self.tip.filter(|_| version >= self.first && version <= self.last) else {
            return Ok(None);
        };
        Node::load(store, tip)?.down_to(store, version).map(Some)
    }

    pub(crate) fn after(&self, store: &Store) -> Result<Option<u64>> {
        if self.first <= 1 {
            return Ok(None);
        }
        let first = self.get(store, self.first)?.ok_or_else(|| corrupt("lost its first entry"))?;
        Ok(Some(first.commit.time_ms))
    }

    pub(crate) fn get(&self, store: &Store, version: u64) -> Result<Option<Arc<Entry>>> {
        if self.tip.is_none() || version < self.first || version > self.last {
            return Ok(None);
        }
        self.descend(store, |entry| {
            let at = entry.commit.version;
            if at == version {
                Step::Found
            } else if at < version {
                Step::Missing
            } else if entry.skip.is_some() && skip_version(at) >= version {
                Step::Skip
            } else {
                Step::Prev
            }
        })
    }

    pub(crate) fn at_time(&self, store: &Store, time_ms: u64) -> Result<Option<Arc<Entry>>> {
        let Some(tip) = self.tip else {
            return Ok(None);
        };
        let mut entry = store.load::<Entry>(tip)?;
        while entry.commit.time_ms > time_ms {
            let at = entry.commit.version;
            let Some(prev) = entry.prev.filter(|_| at > self.first) else {
                return Ok(None);
            };
            let far = match entry.skip {
                Some(skip) if skip_version(at) >= self.first => Some(store.load::<Entry>(skip)?),
                _ => None,
            };
            let loaded = match far {
                Some(far) if far.commit.time_ms > time_ms => far,
                _ => store.load::<Entry>(prev)?,
            };
            if loaded.commit.version >= entry.commit.version {
                return Err(corrupt("versions do not descend"));
            }
            entry = loaded;
        }
        Ok(Some(entry))
    }

    pub(crate) fn walk(&self, store: &Store) -> Walk {
        Walk {
            store: store.clone(),
            next: self.tip,
            above: u64::MAX,
            first: self.first,
        }
    }

    pub(crate) fn walk_down(&self, store: &Store, to: u64, floor: u64) -> Result<Walk> {
        let next = if to == self.last {
            self.tip
        } else {
            self.pointer_to(store, to)?
        };
        Ok(Walk {
            store: store.clone(),
            next,
            above: u64::MAX,
            first: floor,
        })
    }

    pub(crate) fn retained(&self, versions: &impl RangeBounds<u64>) -> Option<(u64, u64)> {
        let from = match versions.start_bound() {
            Bound::Included(version) => *version,
            Bound::Excluded(version) => version.checked_add(1)?,
            Bound::Unbounded => 0,
        };
        let to = match versions.end_bound() {
            Bound::Included(version) => *version,
            Bound::Excluded(version) => version.checked_sub(1)?,
            Bound::Unbounded => u64::MAX,
        };
        let (from, to) = (from.max(self.first), to.min(self.last));
        (self.tip.is_some() && from <= to).then_some((from, to))
    }

    fn first_since(&self, store: &Store, time_ms: u64) -> Result<u64> {
        let older = self.at_time(store, time_ms.saturating_sub(1))?;
        Ok(older.map_or(self.first, |entry| entry.commit.version + 1))
    }

    pub(crate) fn retained_skip(&self, entry: &Entry) -> Option<Ptr> {
        entry.skip.filter(|_| skip_version(entry.commit.version) >= self.first)
    }

    pub(crate) fn pruned(&self, store: &Store, keep: Keep) -> Result<Chain> {
        if self.tip.is_none() {
            return Ok(*self);
        }
        let first = match keep {
            Keep::All => self.first,
            Keep::Last(count) => self.last.saturating_add(1).saturating_sub(count.max(1) as u64),
            Keep::Since(time_ms) => self.first_since(store, time_ms)?,
            Keep::Within(age) => {
                let age = u64::try_from(age.as_millis()).unwrap_or(u64::MAX);
                self.first_since(store, now_ms().saturating_sub(age))?
            }
        };
        Ok(Chain {
            first: first.clamp(self.first, self.last),
            ..*self
        })
    }

    pub(crate) fn visit(
        &self,
        store: &Store,
        mut each: impl FnMut(Ptr, &Entry, &Object, u32) -> Result<bool>,
    ) -> Result<()> {
        let mut next = self.tip;
        let mut above = u64::MAX;
        while let Some(ptr) = next {
            let (object, len) = store.read_record(ptr)?;
            let entry = Entry::decode(&mut Decoder::new(&object.body, &object.links, None))?;
            let at = entry.commit.version;
            if at >= above {
                return Err(corrupt("versions do not descend"));
            }
            above = at;
            if !each(ptr, &entry, &object, len)? {
                break;
            }
            next = entry.prev.filter(|_| at > self.first);
        }
        Ok(())
    }

    pub(crate) fn append(
        &self,
        store: &Store,
        sink: &mut dyn Sink,
        commit: Commit,
        merged: Option<Merged>,
    ) -> Result<Chain> {
        let version = commit.version;
        let first = if self.tip.is_some() { self.first } else { version };
        let target = skip_version(version);
        let skip = if target >= first && target != version - 1 {
            self.pointer_to(store, target)?
        } else {
            None
        };
        let below = self.tip_entry(store)?;
        let mut generation = below.as_ref().map_or(0, |entry| entry.generation()) + 1;
        let mut graph = below.as_ref().map_or(Graph::default(), |entry| entry.graph);
        if let Some(merged) = merged {
            let other = store.load::<Entry>(merged.ptr)?;
            generation = generation.max(other.generation() + 1);
            graph.reach = graph.reach.max(other.generation());
            graph.last_merge = version;
            graph.bounded |= merged.floor > 1 || merged.after.is_some() || other.graph.bounded;
        }
        graph.lift = generation
            .checked_sub(version)
            .ok_or_else(|| corrupt("generation below version"))?;
        let entry = Entry {
            commit: Arc::new(commit),
            prev: self.tip,
            skip,
            merged,
            graph,
        };
        let mut encoder = Encoder::new(sink);
        entry.encode(&mut encoder)?;
        let tip = encoder.finish()?;
        Ok(Chain {
            tip: Some(tip),
            first,
            last: version,
        })
    }

    fn pointer_to(&self, store: &Store, version: u64) -> Result<Option<Ptr>> {
        let mut ptr = self.tip.ok_or_else(|| corrupt("is empty"))?;
        loop {
            let entry = store.load::<Entry>(ptr)?;
            let at = entry.commit.version;
            if at == version {
                return Ok(Some(ptr));
            }
            let next = if at > version && entry.skip.is_some() && skip_version(at) >= version {
                entry.skip
            } else if at > version {
                entry.prev
            } else {
                return Err(corrupt("skip target missing"));
            };
            ptr = next.ok_or_else(|| corrupt("chain ends early"))?;
        }
    }
}

enum Step {
    Found,
    Missing,
    Skip,
    Prev,
}

pub(crate) struct Walk {
    store: Store,
    next: Option<Ptr>,
    above: u64,
    first: u64,
}

impl Walk {
    pub(crate) fn step(&mut self) -> Result<Option<Arc<Entry>>> {
        let Some(ptr) = self.next else {
            return Ok(None);
        };
        let entry = self.store.load::<Entry>(ptr)?;
        if entry.commit.version >= self.above {
            return Err(corrupt("versions do not descend"));
        }
        self.above = entry.commit.version;
        self.next = entry.prev.filter(|_| entry.commit.version > self.first);
        Ok(Some(entry))
    }
}

#[derive(Clone)]
pub struct Log {
    pub(crate) chain: Chain,
    pub(crate) store: Store,
}

impl Log {
    pub fn len(&self) -> usize {
        self.chain.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chain.tip.is_none()
    }

    pub fn try_get(&self, version: &u64) -> Result<Option<Arc<Commit>>> {
        Ok(self
            .chain
            .get(&self.store, *version)?
            .map(|entry| entry.commit.clone()))
    }

    pub fn get(&self, version: &u64) -> Option<Arc<Commit>> {
        crate::map::fatal(self.try_get(version))
    }

    pub fn first(&self) -> Option<(u64, Arc<Commit>)> {
        self.get(&self.chain.first).map(|commit| (self.chain.first, commit))
    }

    pub fn last(&self) -> Option<(u64, Arc<Commit>)> {
        let entry = crate::map::fatal(self.chain.tip_entry(&self.store))?;
        Some((self.chain.last, entry.commit.clone()))
    }

    pub fn hash_at(&self, version: &u64) -> Option<Hash> {
        if self.is_empty() || *version < self.chain.first || *version > self.chain.last {
            return None;
        }
        let ptr = crate::map::fatal(self.chain.pointer_to(&self.store, *version));
        ptr.map(|ptr| ptr.hash)
    }

    pub fn iter(&self) -> Iter {
        Iter {
            walk: self.chain.walk(&self.store),
            front: VecDeque::new(),
            remaining: self.chain.len(),
        }
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = Arc<Commit>> + ExactSizeIterator {
        self.iter().map(|(_, commit)| commit)
    }
}

pub struct Iter {
    walk: Walk,
    front: VecDeque<Arc<Commit>>,
    remaining: usize,
}

impl Iter {
    fn older(&mut self) -> Result<Arc<Commit>> {
        let entry = self.walk.step()?;
        entry
            .map(|entry| entry.commit.clone())
            .ok_or_else(|| corrupt("chain ends early"))
    }

    fn try_next(&mut self) -> Result<Option<Arc<Commit>>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        if self.front.is_empty() {
            for _ in 0..self.remaining {
                let commit = self.older()?;
                self.front.push_front(commit);
            }
        }
        self.remaining -= 1;
        Ok(self.front.pop_front())
    }

    fn try_next_back(&mut self) -> Result<Option<Arc<Commit>>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let commit = match self.front.pop_back() {
            Some(commit) => commit,
            None => self.older()?,
        };
        self.remaining -= 1;
        Ok(Some(commit))
    }
}

impl Iterator for Iter {
    type Item = (u64, Arc<Commit>);

    fn next(&mut self) -> Option<Self::Item> {
        let commit = crate::map::fatal(self.try_next())?;
        Some((commit.version, commit))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl DoubleEndedIterator for Iter {
    fn next_back(&mut self) -> Option<Self::Item> {
        let commit = crate::map::fatal(self.try_next_back())?;
        Some((commit.version, commit))
    }
}

impl ExactSizeIterator for Iter {}
