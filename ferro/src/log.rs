//! The commit log of each branch.
//!
//! A branch's log is a chain of entries, newest first, each holding a [`Commit`] and links back: to
//! the entry before it, to one further back so that any version is a few steps away, and for a
//! merge to the head it merged. [`Log`] is the public view of one branch's retained entries, which
//! [`Db::log`](crate::Db::log) returns.
//! [History and the commit log](https://github.com/archlemon/ferro/blob/main/docs/internals/history.md)
//! describes the chain.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::hash::BuildHasherDefault;
use std::mem;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

use crate::admin::Keep;
use crate::codec::{Data, Decoder, Encoder, Hash, Object, Ptr, Sink};
use crate::db::{Commit, Parts, now_ms};
use crate::error::{Error, Result};
use crate::store::{Prehashed, Store};

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

/// A merge entry's link to the head it merged, with the bounds of what it brought in.
///
/// The merged history is retained from version `floor` up, and, where it is kept by time, from
/// time `after` on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Merged {
    pub(crate) ptr: Ptr,
    pub(crate) floor: u64,
    pub(crate) after: Option<u64>,
}

impl Merged {
    /// The later of the time bound a walk arrived with and this link's own.
    pub(crate) fn after(&self, path: Option<u64>) -> Option<u64> {
        path.max(self.after)
    }
}

/// The numbers an entry carries for the merge base search.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Graph {
    /// How far the entry's generation is above its version.
    pub(crate) lift: u64,
    /// The highest generation among the merged parents of the merge entries at or below this one
    /// on its chain.
    pub(crate) reach: u64,
    /// The version of the nearest merge entry at or below this one on its chain, or 0.
    pub(crate) last_merge: u64,
    /// Whether a merge at or below this one brought in history that was already pruned.
    pub(crate) bounded: bool,
}

/// The numbers of a log entry that the entries linking to it are checked against.
#[derive(Clone, Copy)]
pub(crate) struct Facts {
    pub(crate) version: u64,
    pub(crate) time: u64,
    pub(crate) generation: u64,
    pub(crate) reach: u64,
    pub(crate) last_merge: u64,
    pub(crate) bounded: bool,
}

impl Facts {
    pub(crate) fn of(entry: &Entry) -> Facts {
        Facts {
            version: entry.commit.version,
            time: entry.commit.time_ms,
            generation: entry.generation(),
            reach: entry.graph.reach,
            last_merge: entry.graph.last_merge,
            bounded: entry.graph.bounded,
        }
    }
}

/// What is wrong with an entry with `own` facts and merge link `link`, given its parents, or
/// `None` if it agrees with them.
///
/// It agrees when its time is no earlier than either parent's, its merge floor is not past the
/// merged commit's version, and its generation, reach, last merge and bounded bit are what
/// [`Chain::append`] computes from its parents. A parent is `None` where there is none and
/// `Some(None)` where it is not known, as when it lies in pruned history, and then only what that
/// parent cannot change is checked: the generation must exceed the known parent's, and so on.
pub(crate) fn disagreement(
    own: Facts,
    link: Option<Merged>,
    prev: Option<Option<Facts>>,
    merged: Option<Option<Facts>>,
) -> Option<&'static str> {
    let known = prev.is_none_or(|facts| facts.is_some()) && merged.is_none_or(|f| f.is_some());
    let (prev, linked) = (prev.flatten(), merged.flatten());
    let generation = |facts: Option<Facts>| facts.map_or(0, |facts| facts.generation);
    let parents = generation(prev).max(generation(linked));
    let reach = prev
        .map_or(0, |facts| facts.reach)
        .max(linked.map_or(0, |facts| facts.generation));
    let bounded = prev.is_some_and(|facts| facts.bounded)
        || link.is_some_and(|link| link.floor > 1 || link.after.is_some())
        || linked.is_some_and(|facts| facts.bounded);
    let last_merge = match (link, prev) {
        (Some(_), _) => Some(own.version),
        (None, Some(prev)) => Some(prev.last_merge),
        (None, None) if known => Some(0),
        (None, None) => None,
    };
    let consistent = if known {
        own.generation == parents + 1 && own.reach == reach && own.bounded == bounded
    } else {
        own.generation > parents && own.reach >= reach && (own.bounded || !bounded)
    };
    if prev.is_some_and(|facts| own.time < facts.time)
        || linked.is_some_and(|facts| own.time < facts.time)
    {
        return Some("is older than a commit it links to");
    }
    if let (Some(link), Some(facts)) = (link, linked)
        && link.floor > facts.version
    {
        return Some("keeps the history it merged from past the merged commit");
    }
    if !consistent || last_merge.is_some_and(|last| own.last_merge != last) {
        return Some("does not agree with the commits it links to");
    }
    None
}

/// One commit's record in a branch's log.
#[derive(Clone)]
pub(crate) struct Entry {
    pub(crate) commit: Arc<Commit>,
    pub(crate) prev: Option<Ptr>,
    pub(crate) skip: Option<Ptr>,
    pub(crate) merged: Option<Merged>,
    pub(crate) graph: Graph,
}

impl Entry {
    /// One more than the larger of its parents' generations, so a parent always has a lower one.
    pub(crate) fn generation(&self) -> u64 {
        self.commit.version + self.graph.lift
    }

    pub(crate) fn load(store: &Store, ptr: Ptr) -> Result<Arc<Entry>> {
        store.load(ptr)
    }
}

impl Data for Entry {
    const LOCATED: bool = true;

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
        if commit.version == 0 {
            return Err(Error::Decode("log entry claims version 0".into()));
        }
        let flags = r.u8()?;
        if flags & (WHOLE | JOINED) == WHOLE | JOINED {
            return Err(Error::Decode(format!("invalid log entry flags {flags}")));
        }
        let extra = if flags & EXTRA != 0 { r.varint()? } else { 0 };
        if extra & !KNOWN_EXTRA != 0 || (flags & EXTRA != 0 && extra == 0) {
            return Err(Error::Decode(format!(
                "invalid log entry extra flags {extra}"
            )));
        }
        if flags & WHOLE != 0 {
            commit.parts = Parts::Whole;
        }
        commit.stamped = extra & STAMPED != 0;
        if flags & JOINED != 0 {
            let count = r.varint()?;
            if count > commit.message.len() as u64 + 1 {
                return Err(Error::Decode(format!(
                    "{count} parts in a short commit message"
                )));
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
            return Err(Error::Decode(format!(
                "invalid merge graph in log entry {version}"
            )));
        }
        let prev = if flags & HAS_PREV != 0 {
            Some(r.link()?)
        } else {
            None
        };
        let skip = if flags & HAS_SKIP != 0 {
            Some(r.link()?)
        } else {
            None
        };
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

/// A log entry and the pointer it was read from.
#[derive(Clone)]
pub(crate) struct Node {
    pub(crate) ptr: Ptr,
    pub(crate) entry: Arc<Entry>,
}

impl Node {
    pub(crate) fn load(store: &Store, ptr: Ptr) -> Result<Node> {
        Ok(Node {
            ptr,
            entry: Entry::load(store, ptr)?,
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

    /// The entry at `version` on this node's chain, at or below it, reached through skip links.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] if the chain ends early, skips the version or does not descend.
    pub(crate) fn down_to(&self, store: &Store, version: u64) -> Result<Node> {
        let mut node = self.clone();
        while node.version() > version {
            let link = match node.entry.skip {
                Some(skip) if skip.offset != 0 && skip_version(node.version()) >= version => {
                    Some(skip)
                }
                _ => node.entry.prev,
            };
            node = node.below(store, link)?;
        }
        if node.version() != version {
            return Err(corrupt("skips a version"));
        }
        Ok(node)
    }

    /// The newest entry at or below this one for which `too_high` is false, or `None` if it holds
    /// all the way down to version `floor`.
    ///
    /// `too_high` must not hold below an entry for which it fails, as a bound on generation does,
    /// so that skip links can jump over the entries it holds for.
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
                && skip.offset != 0
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

    /// The lowest version, no lower than `floor`, down to which every entry from this one is
    /// `kept`.
    ///
    /// `kept` must fail for every entry below the first that fails it, as a bound on time does.
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
            let far = node
                .entry
                .skip
                .filter(|skip| skip.offset != 0 && skip_version(at) >= floor);
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
type Walks = Vec<(u64, Option<u64>)>;

/// A walk over the commits that merges brought into branches, from their merge links down.
///
/// Each start is queued with the bounds it was reached with, and a commit already walked with
/// bounds at least as wide is not walked again.
#[derive(Default)]
pub(crate) struct Closures {
    queue: BinaryHeap<Reverse<Seed>>,
    walked: HashMap<(Hash, u64), Walked, BuildHasherDefault<Prehashed>>,
    exact: bool,
    chains: Vec<(u64, Vec<Hash>)>,
    open: bool,
}

/// The bounds an entry was walked with, and where it sits on a chain [`Closures::chain`] recorded.
#[derive(Default)]
struct Walked {
    bounds: Walks,
    place: Option<(usize, u64)>,
}

impl Closures {
    /// A walk that tells records apart by offset as well as hash, for a copy that must visit
    /// every stored record and not only every distinct commit.
    pub(crate) fn exact() -> Closures {
        Closures {
            exact: true,
            ..Closures::default()
        }
    }

    fn key(&self, ptr: Ptr) -> (Hash, u64) {
        (ptr.hash, if self.exact { ptr.offset } else { 0 })
    }

    /// Queues the head `entry` merged, if it is a merge, under the time bound `after`.
    pub(crate) fn seed(&mut self, entry: &Entry, after: Option<u64>) {
        if let Some(merged) = entry.merged {
            self.push(merged.ptr, merged.floor, merged.after(after));
        }
    }

    /// Queues a walk down from `ptr` that stops below version `floor` and before time `after`.
    pub(crate) fn push(&mut self, ptr: Ptr, floor: u64, after: Option<u64>) {
        self.queue
            .push(Reverse((floor, after, ptr.hash, ptr.offset)));
    }

    /// Starts taking the entries [`Closures::mark`] is given, until the next call or the walk, as
    /// those of `chain` from its tip down, so that a walk of merged history that stops on one can
    /// check the skip links it passed against the chain without reading it.
    pub(crate) fn chain(&mut self, chain: &Chain) {
        self.chains.push((chain.last, Vec::new()));
        self.open = true;
    }

    /// Records that the entry at `ptr` was walked with these bounds, so a walk that reaches it
    /// with bounds no wider stops there.
    pub(crate) fn mark(&mut self, ptr: Ptr, floor: u64, after: Option<u64>) {
        let key = self.key(ptr);
        let index = self.chains.len().wrapping_sub(1);
        let place = match self.chains.last_mut().filter(|_| self.open) {
            Some((last, hashes)) => {
                let version = last.checked_sub(hashes.len() as u64);
                hashes.push(ptr.hash);
                version.map(|version| (index, version))
            }
            None => None,
        };
        let walked = self.walked.entry(key).or_default();
        walked.bounds.push((floor, after));
        walked.place = walked.place.or(place);
    }

    /// The hashes of the entries from `ptr` down a chain [`Closures::chain`] recorded, newest
    /// first, and the version of the first.
    fn placed(&self, ptr: Ptr) -> Option<(&[Hash], u64)> {
        let (index, version) = self.walked.get(&self.key(ptr))?.place?;
        let (last, hashes) = &self.chains[index];
        Some((&hashes[(last - version) as usize..], version))
    }

    fn covered(&self, ptr: Ptr, floor: u64, after: Option<u64>) -> bool {
        let walks = self
            .walked
            .get(&self.key(ptr))
            .map_or(&[][..], |walked| walked.bounds.as_slice());
        walks.iter().any(|(walked, bound)| {
            *walked <= floor
                && match (bound, after) {
                    (None, _) => true,
                    (Some(_), None) => false,
                    (Some(bound), Some(after)) => bound <= &after,
                }
        })
    }

    /// Walks every queued start, lowest floor first, and every merge link reached on the way,
    /// calling `each` on each entry; `each` returning `false` stops that chain.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] if an entry's `prev` link reaches another version than the one below
    /// it, an entry does not agree with the commits it links to as [`disagreement`] has it, a skip
    /// link above the floor names another entry than the one the chain reaches at its version, or
    /// leads past where the chain stops to a commit kept by time, and the first error from `each`
    /// or from reading an entry.
    pub(crate) fn walk(
        &mut self,
        store: &Store,
        mut each: impl FnMut(Ptr, &Entry, &Object, u32) -> Result<bool>,
    ) -> Result<()> {
        let read = |ptr| store.read_record(ptr);
        self.walk_bounded(read, |ptr, entry, object, len, _| {
            each(ptr, entry, object, len)
        })
    }

    /// [`Closures::walk`], reading each entry's record and its length with `read`, which must
    /// check its hash as [`Store::read_record`] does, and also handing `each` the bounds each
    /// entry was reached with.
    ///
    /// # Errors
    ///
    /// As for [`Closures::walk`].
    pub(crate) fn walk_bounded(
        &mut self,
        mut read: impl FnMut(Ptr) -> Result<(Object, u32)>,
        mut each: impl FnMut(Ptr, &Entry, &Object, u32, (u64, Option<u64>)) -> Result<bool>,
    ) -> Result<()> {
        self.open = false;
        while let Some(Reverse((floor, after, hash, offset))) = self.queue.pop() {
            let mut next = Some(Ptr { hash, offset });
            let mut trail = Trail::new(floor);
            while let Some(ptr) = next.take() {
                if ptr.offset == 0 {
                    break;
                }
                if self.covered(ptr, floor, after) {
                    trail.settle(&mut read, Some(ptr), self.placed(ptr))?;
                    break;
                }
                let (object, len) = read(ptr)?;
                let entry = decode(&object)?;
                let at = entry.commit.version;
                let first = trail.above == u64::MAX;
                trail.reach(ptr, &entry)?;
                if at < floor {
                    break;
                }
                if let Some(after) = after.filter(|after| entry.commit.time_ms < *after) {
                    if first {
                        each(ptr, &entry, &object, len, (floor, Some(after)))?;
                    }
                    trail.stop_by_time(&mut read, after)?;
                    break;
                }
                self.mark(ptr, floor, after);
                if !each(ptr, &entry, &object, len, (floor, after))? {
                    trail.settle(&mut read, entry.prev, None)?;
                    break;
                }
                self.seed(&entry, after);
                trail.pass(&mut read, &entry)?;
                next = entry.prev.filter(|_| at > floor);
            }
            trail.end()?;
        }
        Ok(())
    }
}

fn decode(object: &Object) -> Result<Entry> {
    Entry::decode(&mut Decoder::new(&object.body, &object.links, None))
}

/// An entry a walk has passed, waiting for the walk to read the one its `prev` link reaches.
struct Waiting {
    facts: Facts,
    link: Option<Merged>,
    merged: Option<Option<Facts>>,
    orphan: bool,
}

impl Waiting {
    /// Checks the entry against its parent through `prev`, given as for [`disagreement`].
    fn check(self, prev: Option<Option<Facts>>) -> Result<()> {
        match disagreement(self.facts, self.link, prev, self.merged) {
            Some(what) => Err(corrupt(&format!("v{} {what}", self.facts.version))),
            None => Ok(()),
        }
    }
}

/// What a walk down one chain still has to check: the entry it passed last, against the entry
/// below it, and the skip links it passed, against the entries at the versions they name.
struct Trail {
    floor: u64,
    above: u64,
    waiting: Option<Waiting>,
    skips: Vec<(u64, u64, Ptr)>,
}

impl Trail {
    fn new(floor: u64) -> Trail {
        Trail {
            floor,
            above: u64::MAX,
            waiting: None,
            skips: Vec::new(),
        }
    }

    /// Checks the entry the walk read at `ptr` against the entry above it and the skip links that
    /// name its version.
    fn reach(&mut self, ptr: Ptr, entry: &Entry) -> Result<()> {
        let at = entry.commit.version;
        step_down(self.above, at)?;
        self.above = at;
        if let Some(waiting) = self.waiting.take() {
            waiting.check(Some(Some(Facts::of(entry))))?;
        }
        if let Some(&(from, _, _)) = self
            .skips
            .iter()
            .find(|&&(_, to, skip)| to == at && skip.hash != ptr.hash)
        {
            return Err(corrupt(&format!(
                "v{from} skips to another entry than v{at}"
            )));
        }
        self.skips.retain(|&(_, to, _)| to < at);
        Ok(())
    }

    /// Records that the walk goes on below `entry`, reading the commit it merged, if any, to
    /// check it against.
    fn pass(
        &mut self,
        read: &mut impl FnMut(Ptr) -> Result<(Object, u32)>,
        entry: &Entry,
    ) -> Result<()> {
        let at = entry.commit.version;
        let merged = match entry.merged {
            None => None,
            Some(merged) if merged.ptr.offset == 0 => Some(None),
            Some(merged) => Some(Some(Facts::of(&decode(&read(merged.ptr)?.0)?))),
        };
        self.waiting = Some(Waiting {
            facts: Facts::of(entry),
            link: entry.merged,
            merged,
            orphan: entry.prev.is_none() && at == 1,
        });
        let skip = entry
            .skip
            .filter(|skip| skip.offset != 0 && skip_version(at) >= self.floor);
        self.skips
            .extend(skip.map(|skip| (at, skip_version(at), skip)));
        Ok(())
    }

    /// Reads on down from `next` without walking, where another walk took the chain from there,
    /// until nothing is left to check.
    ///
    /// `known` holds the hashes of the entries from `next` down, newest first, with the version
    /// of the first, where a chain walk recorded them, so the skip links that land there need no
    /// read.
    fn settle(
        &mut self,
        read: &mut impl FnMut(Ptr) -> Result<(Object, u32)>,
        mut next: Option<Ptr>,
        known: Option<(&[Hash], u64)>,
    ) -> Result<()> {
        if let Some((hashes, top)) = known {
            let held = |to: u64| top.checked_sub(to).and_then(|at| hashes.get(at as usize));
            for &(from, to, skip) in &self.skips {
                if held(to).is_some_and(|hash| *hash != skip.hash) {
                    return Err(corrupt(&format!(
                        "v{from} skips to another entry than v{to}"
                    )));
                }
            }
            self.skips.retain(|&(_, to, _)| held(to).is_none());
        }
        while self.waiting.is_some() || !self.skips.is_empty() {
            let Some(ptr) = next.filter(|ptr| ptr.offset != 0) else {
                return Ok(());
            };
            let entry = decode(&read(ptr)?.0)?;
            self.reach(ptr, &entry)?;
            next = entry.prev;
        }
        Ok(())
    }

    /// Checks, where the walk stops because the history before time `after` is not kept, that
    /// each skip link still waiting lands on a commit from before that time too, so that no
    /// reader of the history kept follows one past where the walk stopped.
    fn stop_by_time(
        &mut self,
        read: &mut impl FnMut(Ptr) -> Result<(Object, u32)>,
        after: u64,
    ) -> Result<()> {
        for (from, to, skip) in mem::take(&mut self.skips) {
            if decode(&read(skip)?.0)?.commit.time_ms >= after {
                return Err(corrupt(&format!(
                    "v{from} skips past v{}, where the history it keeps stops, to v{to}",
                    self.above
                )));
            }
        }
        Ok(())
    }

    /// Drops what is left to check where the caller stopped the walk.
    fn leave(&mut self) {
        self.skips.clear();
    }

    /// Checks what is left once the walk has ended: the last entry passed against a parent it
    /// did not read, and that no skip link leads past where the `prev` links stopped.
    fn end(mut self) -> Result<()> {
        if let Some(&(from, to, _)) = self.skips.first() {
            return Err(corrupt(&format!(
                "v{from} skips to v{to}, past where the history below v{} was cut",
                self.above
            )));
        }
        if let Some(waiting) = self.waiting.take() {
            let prev = if waiting.orphan { None } else { Some(None) };
            waiting.check(prev)?;
        }
        Ok(())
    }
}

/// The version an entry at `version` skips back to: `version` with its lowest set bit cleared.
pub(crate) fn skip_version(version: u64) -> u64 {
    version - (version & version.wrapping_neg())
}

fn corrupt(what: &str) -> Error {
    Error::Corrupt(format!("commit log {what}"))
}

/// Checks that the entry at `at`, reached through the `prev` link of the entry at `above`, is the
/// version just below it, where `above` is `u64::MAX` for the first entry of a walk.
fn step_down(above: u64, at: u64) -> Result<()> {
    if at >= above {
        return Err(corrupt("versions do not descend"));
    }
    if above != u64::MAX && at != above - 1 {
        return Err(corrupt(&format!(
            "v{above} links v{at} as the entry before it, not v{}",
            above - 1
        )));
    }
    Ok(())
}

/// A branch's retained commit log: its newest entry and the versions it keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chain {
    /// The newest entry, or `None` before the first commit.
    pub(crate) tip: Option<Ptr>,
    /// The oldest retained version. Entries below it may still be in the file, but are pruned.
    pub(crate) first: u64,
    /// The version of the newest entry.
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

    /// A chain from its parts, as decoded from a branch table.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] if a chain with a tip keeps no version or more than it holds.
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
        let mut entry = Entry::load(store, ptr)?;
        loop {
            let next = match step(&entry) {
                Step::Found => return Ok(Some(entry)),
                Step::Missing => return Ok(None),
                Step::Skip => entry.skip,
                Step::Prev => entry.prev,
            };
            ptr = next.ok_or_else(|| corrupt("chain ends early"))?;
            if ptr.offset == 0 {
                return Err(self.cut(entry.commit.version));
            }
            let loaded = Entry::load(store, ptr)?;
            if loaded.commit.version >= entry.commit.version {
                return Err(corrupt("versions do not descend"));
            }
            entry = loaded;
        }
    }

    pub(crate) fn tip_entry(&self, store: &Store) -> Result<Option<Arc<Entry>>> {
        match self.tip {
            Some(ptr) => Ok(Some(Entry::load(store, ptr)?)),
            None => Ok(None),
        }
    }

    /// The retained entry at `version` with its pointer, or `None` outside the retained versions.
    pub(crate) fn node_at(&self, store: &Store, version: u64) -> Result<Option<Node>> {
        let Some(tip) = self
            .tip
            .filter(|_| version >= self.first && version <= self.last)
        else {
            return Ok(None);
        };
        Node::load(store, tip)?.down_to(store, version).map(Some)
    }

    /// The time of the oldest retained entry once versions before it are pruned, or `None` while
    /// version 1 is retained.
    ///
    /// Commits that merges brought in from before this time count as pruned too.
    pub(crate) fn after(&self, store: &Store) -> Result<Option<u64>> {
        if self.first <= 1 {
            return Ok(None);
        }
        let first = self
            .get(store, self.first)?
            .ok_or_else(|| corrupt("lost its first entry"))?;
        Ok(Some(first.commit.time_ms))
    }

    /// The retained entry at `version`, or `None` outside the retained versions.
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

    /// The newest retained entry whose time is at most `time_ms`, or `None` if every retained
    /// entry is later.
    pub(crate) fn at_time(&self, store: &Store, time_ms: u64) -> Result<Option<Arc<Entry>>> {
        let Some(tip) = self.tip else {
            return Ok(None);
        };
        let mut entry = Entry::load(store, tip)?;
        while entry.commit.time_ms > time_ms {
            let at = entry.commit.version;
            let Some(prev) = entry.prev.filter(|_| at > self.first) else {
                return Ok(None);
            };
            let far = match entry.skip {
                Some(skip) if skip_version(at) >= self.first => Some(Entry::load(store, skip)?),
                _ => None,
            };
            let loaded = match far {
                Some(far) if far.commit.time_ms > time_ms => far,
                _ => Entry::load(store, prev)?,
            };
            if loaded.commit.version >= entry.commit.version {
                return Err(corrupt("versions do not descend"));
            }
            entry = loaded;
        }
        Ok(Some(entry))
    }

    /// A walk over every retained entry, newest first.
    pub(crate) fn walk(&self, store: &Store) -> Walk {
        Walk {
            store: store.clone(),
            next: self.tip,
            above: u64::MAX,
            first: self.first,
        }
    }

    /// A walk from the entry at `to` down to version `floor`, newest first.
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

    /// The first and last retained versions within `versions`, or `None` if there are none.
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
        Ok(older.map_or(self.first, |entry| entry.commit.version.saturating_add(1)))
    }

    /// The skip link of `entry`, if it lands on a retained version.
    pub(crate) fn retained_skip(&self, entry: &Entry) -> Option<Ptr> {
        entry
            .skip
            .filter(|_| skip_version(entry.commit.version) >= self.first)
    }

    /// This chain with its oldest retained version raised as `keep` asks, never past the newest,
    /// so at least one commit stays.
    pub(crate) fn pruned(&self, store: &Store, keep: Keep) -> Result<Chain> {
        if self.tip.is_none() {
            return Ok(*self);
        }
        let first = match keep {
            Keep::All => self.first,
            Keep::Last(count) => self
                .last
                .saturating_add(1)
                .saturating_sub(count.max(1) as u64),
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

    /// Reads every retained entry newest first, decoding each from its record, and calls `each`
    /// on it until `each` returns `false`.
    ///
    /// `root` is the root the branch's head names, which must be the tip's.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] if the tip is not at the chain's last version or holds another root than
    /// `root`, an entry's `prev` link reaches another version than the one below it, an entry
    /// links pruned history it needs or does not agree with the commits it links to as
    /// [`disagreement`] has it, or a kept skip link names another entry than the one the chain
    /// reaches at that version, by hash, and the first error from `each`.
    pub(crate) fn visit(
        &self,
        store: &Store,
        root: Option<Ptr>,
        each: impl FnMut(Ptr, &Entry, &Object, u32) -> Result<bool>,
    ) -> Result<()> {
        self.visit_with(root, |ptr| store.read_record(ptr), each)
    }

    /// [`Chain::visit`], reading each entry's record and its length with `read`, which must check
    /// its hash as [`Store::read_record`] does.
    ///
    /// # Errors
    ///
    /// As for [`Chain::visit`], and the first error from `read`.
    pub(crate) fn visit_with(
        &self,
        root: Option<Ptr>,
        mut read: impl FnMut(Ptr) -> Result<(Object, u32)>,
        mut each: impl FnMut(Ptr, &Entry, &Object, u32) -> Result<bool>,
    ) -> Result<()> {
        let mut next = self.tip;
        let mut trail = Trail::new(self.first);
        while let Some(ptr) = next {
            let (object, len) = read(ptr)?;
            let entry = decode(&object)?;
            let at = entry.commit.version;
            if trail.above == u64::MAX {
                if at != self.last {
                    return Err(corrupt(&format!(
                        "names v{} as its last but its tip is v{at}",
                        self.last
                    )));
                }
                if entry.commit.root.map(|root| root.hash) != root.map(|root| root.hash) {
                    return Err(corrupt(&format!(
                        "tip v{at} holds another root than its branch's head names"
                    )));
                }
            }
            trail.reach(ptr, &entry)?;
            if !each(ptr, &entry, &object, len)? {
                trail.leave();
                break;
            }
            trail.pass(&mut read, &entry)?;
            let skip = self.retained_skip(&entry);
            next = match entry.prev {
                _ if at <= self.first => None,
                Some(prev) if prev.offset != 0 && skip.is_none_or(|skip| skip.offset != 0) => {
                    Some(prev)
                }
                _ => return Err(self.cut(at)),
            };
        }
        trail.end()
    }

    fn cut(&self, at: u64) -> Error {
        corrupt(&format!(
            "keeps versions from {} but v{at} links pruned history",
            self.first
        ))
    }

    /// Writes the entry for `commit` on top of this chain into `sink`, and returns the longer
    /// chain.
    ///
    /// `merged` makes it a merge entry that links the head it merged, which raises its generation
    /// and sets its [`Graph`] from both parents.
    pub(crate) fn append(
        &self,
        store: &Store,
        sink: &mut dyn Sink,
        commit: Commit,
        merged: Option<Merged>,
    ) -> Result<Chain> {
        let version = commit.version;
        let first = if self.tip.is_some() {
            self.first
        } else {
            version
        };
        let target = skip_version(version);
        let skip = if target >= first && target != version - 1 {
            self.pointer_to(store, target)?
        } else {
            None
        };
        let below = self.tip_entry(store)?;
        let after = |generation: u64| {
            generation
                .checked_add(1)
                .ok_or_else(|| corrupt("generation overflows"))
        };
        let mut generation = after(below.as_ref().map_or(0, |entry| entry.generation()))?;
        let mut graph = below.as_ref().map_or(Graph::default(), |entry| entry.graph);
        if let Some(merged) = merged {
            let other = Entry::load(store, merged.ptr)?;
            generation = generation.max(after(other.generation())?);
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
            let entry = Entry::load(store, ptr)?;
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

/// A walk down a chain, newest first, that checks each entry is older than the last.
pub(crate) struct Walk {
    store: Store,
    next: Option<Ptr>,
    above: u64,
    first: u64,
}

impl Walk {
    /// The next older entry, or `None` past the walk's floor.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] if the versions do not descend, and read errors.
    pub(crate) fn step(&mut self) -> Result<Option<Arc<Entry>>> {
        let Some(ptr) = self.next else {
            return Ok(None);
        };
        let entry = Entry::load(&self.store, ptr)?;
        if entry.commit.version >= self.above {
            return Err(corrupt("versions do not descend"));
        }
        self.above = entry.commit.version;
        self.next = entry.prev.filter(|_| entry.commit.version > self.first);
        Ok(Some(entry))
    }
}

/// The retained commits of one branch, as of the moment [`Db::log`](crate::Db::log) was called.
///
/// It reads like a collection of [`Commit`]s keyed by version. It does not change as more commits
/// land, and it keeps reading the file it came from after a reclamation cycle replaces it.
#[derive(Clone)]
pub struct Log {
    pub(crate) chain: Chain,
    pub(crate) store: Store,
}

impl Log {
    /// The number of retained commits.
    pub fn len(&self) -> usize {
        self.chain.len()
    }

    /// Whether the branch has no commits.
    pub fn is_empty(&self) -> bool {
        self.chain.tip.is_none()
    }

    /// The commit at `version`, or `None` if it is not retained.
    ///
    /// # Errors
    ///
    /// A read error from the log, such as [`Error::Corrupt`].
    pub fn try_get(&self, version: &u64) -> Result<Option<Arc<Commit>>> {
        Ok(self
            .chain
            .get(&self.store, *version)?
            .map(|entry| entry.commit.clone()))
    }

    /// The commit at `version`, or `None` if it is not retained.
    ///
    /// It follows skip links, so it reads a few entries whatever the length of the log.
    ///
    /// # Panics
    ///
    /// On a read error, as collection reads do. [`Log::try_get`] returns it instead.
    pub fn get(&self, version: &u64) -> Option<Arc<Commit>> {
        crate::map::fatal(self.try_get(version))
    }

    /// The oldest retained commit and its version, or `None` before the first commit.
    ///
    /// # Panics
    ///
    /// On a read error, as collection reads do.
    pub fn first(&self) -> Option<(u64, Arc<Commit>)> {
        self.get(&self.chain.first)
            .map(|commit| (self.chain.first, commit))
    }

    /// The newest commit and its version, or `None` before the first commit.
    ///
    /// # Panics
    ///
    /// On a read error, as collection reads do.
    pub fn last(&self) -> Option<(u64, Arc<Commit>)> {
        let entry = crate::map::fatal(self.chain.tip_entry(&self.store))?;
        Some((self.chain.last, entry.commit.clone()))
    }

    /// The hash of the log entry at `version`, which covers that version and everything before
    /// it, or `None` if it is not retained.
    ///
    /// For the newest version it is [`Db::head_hash`](crate::Db::head_hash), so an anchor taken
    /// from `head_hash` earlier matches this at the same version only if nothing up to it has
    /// changed.
    ///
    /// # Panics
    ///
    /// On a read error, as collection reads do.
    pub fn hash_at(&self, version: &u64) -> Option<Hash> {
        if self.is_empty() || *version < self.chain.first || *version > self.chain.last {
            return None;
        }
        let ptr = crate::map::fatal(self.chain.pointer_to(&self.store, *version));
        ptr.map(|ptr| ptr.hash)
    }

    /// The retained commits with their versions, oldest first.
    ///
    /// Taken from the back with `rev`, it reads newest first, one entry a step, and stops when
    /// you stop. Taken from the front, its first step reads every entry not yet taken from the
    /// back, since the log links newest to oldest.
    pub fn iter(&self) -> Iter {
        Iter {
            walk: self.chain.walk(&self.store),
            front: VecDeque::new(),
            remaining: self.chain.len(),
        }
    }

    /// The retained commits, oldest first, walked as [`Log::iter`] walks them.
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
    /// for message in ["one", "two", "three"] {
    ///     db.commit(message, |count| *count += 1)?;
    /// }
    ///
    /// let newest: Vec<String> = db
    ///     .log()
    ///     .values()
    ///     .rev()
    ///     .take(2)
    ///     .map(|commit| commit.message.clone())
    ///     .collect();
    /// assert_eq!(newest, ["three", "two"]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn values(&self) -> impl DoubleEndedIterator<Item = Arc<Commit>> + ExactSizeIterator {
        self.iter().map(|(_, commit)| commit)
    }
}

/// An iterator over a [`Log`]'s commits and their versions, from [`Log::iter`].
///
/// # Panics
///
/// A step panics on a read error, as collection reads do.
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
