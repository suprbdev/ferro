//! Reclamation: copying what a database retains into a new file and swapping it in.
//!
//! A cycle prunes each branch, then copies every object that a retained version reaches into a file
//! beside the database, checking each object's hash as it goes. The copy runs in passes while
//! writers go on, and a last pass under every writer lock copies what they wrote meanwhile before
//! the new file is renamed over the old one. [`Db::compact`](crate::Db::compact), the background
//! cycles that [`Options::history`](crate::Options::history) starts, and
//! [`admin::compact`](crate::admin::compact) share the copy. [`Reclaim`] says when background
//! cycles run, [`Space`] reports how they have gone, and [`Compacted`] reports one cycle.
//! [Reclamation](https://github.com/archlemon/ferro/blob/main/docs/internals/reclamation.md)
//! describes cycles, generations and crashes.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::hash::{BuildHasherDefault, Hasher};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::codec::{Data, Decoder, Encoder, Hash, Object, Ptr, Sink, Staged};
use crate::db::{Head, Refs};
use crate::error::{Error, Result};
use crate::log::{Chain, Closures, Entry, skip_version};
use crate::platform::{copy_owner, start_writeback};
pub(crate) use crate::platform::{rename, sync_parent};
use crate::refs;
use crate::store::{
    Basis, LOG_START, Prehashed, Store, StoreOptions, Traced, Unpacked, append_staged, body_of,
    compressed, seal, staged, stored_within, verify_stored,
};
use ahead::Ahead;
use copies::Copies;
use units::{Key, PENDING, Reserved, Settled, Units};

mod ahead;
mod copies;
mod ids;
mod units;

const FLUSH_BYTES: usize = 1 << 20;
const SPAWN_PAST: u64 = 256 << 10;
const SPARE_BYTES: usize = 64 << 10;
const COPY_THREADS: usize = 8;
const AHEAD: usize = 16384;
const KEEP_BYTES: usize = 16 << 20;
const RESERVE: u64 = 1 << 16;

type ByHash = BuildHasherDefault<Prehashed>;
type ByOffset = BuildHasherDefault<Spread>;

/// A hasher for offsets in the file, which folds the 128-bit product of each word and an odd
/// constant, so that the low bits a table picks a group by and the high bits it tags a slot with
/// both depend on every bit of the offset.
#[derive(Default)]
struct Spread(u64);

impl Hasher for Spread {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    fn write_u64(&mut self, word: u64) {
        let product = u128::from(self.0 ^ word) * 0x9e37_79b9_7f4a_7c15;
        self.0 = product as u64 ^ (product >> 64) as u64;
    }
}

/// When background reclamation cycles run, set with [`Options::reclaim`](crate::Options::reclaim).
///
/// Background cycles run only on a writer whose [`Options::history`](crate::Options::history) is
/// not [`Keep::All`](crate::Keep::All).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reclaim {
    /// A cycle runs once the file has grown by this many bytes since the last one, or by its size
    /// after the last one if that is larger. Zero turns background cycles off. The default is 64
    /// MiB.
    pub every_bytes: u64,
}

impl Default for Reclaim {
    fn default() -> Self {
        Reclaim {
            every_bytes: 64 << 20,
        }
    }
}

/// What one reclamation cycle did, from [`Db::compact`](crate::Db::compact) or
/// [`admin::compact`](crate::admin::compact).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compacted {
    /// The length of the file before the cycle, in bytes, zeroes a writer preallocated past its
    /// log included.
    pub before_bytes: u64,
    /// The length of the new file, in bytes, once the cycle has swapped it in.
    pub after_bytes: u64,
    /// The number of objects copied into the new file.
    pub objects: u64,
    /// The number of commits the cycle pruned, across every branch.
    pub commits_dropped: u64,
    /// The number of copy passes made before the last, which runs while writers wait.
    pub passes: u32,
    /// How long writers waited for the last pass and the swap.
    pub pause: Duration,
    /// The number of trees the cycle handed out to copy as units, 0 with `copy_threads(1)`.
    ///
    /// In a file under 256 KiB with the default thread count, no other thread starts and the
    /// calling thread copies the units itself, so a count above 0 does not mean other threads
    /// ran.
    pub handed_out: u64,
}

/// How a database file's size compares with what it holds, from [`Db::space`](crate::Db::space)
/// or [`admin::space`](crate::admin::space).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Space {
    /// The length of the file, in bytes. It includes the zeroes that a writer under
    /// [`Durability::Fsync`](crate::Durability::Fsync) preallocates past the log, up to 4 MiB,
    /// and the staging frames that a receive of more than 1 MiB placed past the newest commit,
    /// while it streams, or once it was refused, as headers over holes on Linux. An exclusive
    /// writer truncates both when it closes and the next writer to open truncates them otherwise,
    /// so it is never less than [`Space::log_bytes`].
    pub file_bytes: u64,
    /// The bytes the file's log takes, from the start of the file to the end of its newest
    /// commit, as [`Db::size_on_disk`](crate::Db::size_on_disk) reports them. The staging frames
    /// of a refused receive that a commit followed count here until the next cycle, though on
    /// Linux their records take no disk.
    pub log_bytes: u64,
    /// An estimate of the bytes that retained versions take: the log's size after the last cycle,
    /// plus the share of the log's growth since then that the last cycle found survives, or the
    /// whole log if no cycle has run on it. It is never more than [`Space::log_bytes`].
    pub live_bytes: u64,
    /// The bytes of files that cycles replaced but that held snapshots, maps, logs, histories or
    /// [`Db::watch_changes`](crate::Db::watch_changes) watchers keep open. On a read-only handle it
    /// counts the files that [`Db::refresh`](crate::Db::refresh) followed past.
    pub retired_bytes: u64,
    /// The number of cycles started since the database was opened, background ones and
    /// [`Db::compact`](crate::Db::compact) alike, whether they succeeded or not.
    pub cycles: u64,
    /// The longest time a cycle since the database was opened made writers wait.
    pub longest_pause: Duration,
    /// The error the last failed cycle returned, as text, until a cycle succeeds. A cycle stopped
    /// because the database was closing is not recorded.
    pub last_error: Option<String>,
}

fn links_of(entry: &Entry) -> impl Iterator<Item = Hash> {
    let merged = entry.merged.map(|merged| merged.ptr);
    entry
        .prev
        .into_iter()
        .chain(entry.skip)
        .chain(merged)
        .map(|link| link.hash)
}

fn copy_threads(options: &StoreOptions) -> usize {
    match options.copy_threads {
        0 => thread::available_parallelism().map_or(1, |cores| cores.get().min(COPY_THREADS)),
        threads => threads,
    }
}

/// The path a cycle writes its copy of the file at `path` to: the same name with `.compacting`
/// added.
pub(crate) fn sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".compacting");
    PathBuf::from(name)
}

/// Where a walk over everything a branch table reaches starts.
#[derive(Default)]
pub(crate) struct Roots {
    /// Objects walked as trees, such as the branch table itself.
    pub(crate) trees: Vec<Ptr>,
    /// The head of each branch, with its retained commit log.
    pub(crate) heads: Vec<Head>,
    /// The heads that older steps of the branch table record for the same branches, which the
    /// table still links.
    pub(crate) superseded: Vec<Head>,
    /// Whether a commit that merges brought in and that is stored at more than one offset is
    /// walked at each, as a compaction copies each, rather than once.
    pub(crate) exact: bool,
}

impl Roots {
    /// The roots of the branch table at `head`.
    pub(crate) fn of(store: &Store, head: Ptr) -> Result<Roots> {
        let heads = refs::heads(store, head)?;
        Ok(Roots {
            trees: vec![head],
            heads: heads.current,
            superseded: heads.superseded,
            exact: false,
        })
    }

    /// Visits each object reachable from these roots through retained versions: log entries, the
    /// commits that merges brought in, the trees they hold and the branch table.
    ///
    /// `fresh` is asked whether each object is new, with whether it is a log entry, and only a new
    /// one is visited and descended into. `visit` gets each new object with its length in the
    /// file.
    ///
    /// # Errors
    ///
    /// The first read error, such as [`Error::Corrupt`] for an object whose bytes do not match its
    /// hash.
    pub(crate) fn walk(
        &self,
        store: &Store,
        mut fresh: impl FnMut(Ptr, bool) -> bool,
        mut visit: impl FnMut(Ptr, &Object, u32),
    ) -> Result<()> {
        let mut stack = Vec::new();
        let mut entries: HashMap<u64, Hash> = HashMap::new();
        let mut floors: HashMap<u64, u64> = HashMap::new();
        let mut closures = match self.exact {
            true => Closures::exact(),
            false => Closures::default(),
        };
        let mut superseded: Vec<&Head> = self.superseded.iter().collect();
        superseded.sort_by_key(|head| head.log.first);
        let heads = self.heads.iter().map(|head| (head, false));
        for (head, replaced) in heads.chain(superseded.into_iter().map(|head| (head, true))) {
            let chain = &head.log;
            let after = chain.after(store)?;
            closures.chain(chain);
            chain.visit(store, head.root, |ptr, entry, object, len| {
                let floor = floors.entry(ptr.offset).or_insert(u64::MAX);
                if replaced && *floor <= chain.first {
                    return Ok(false);
                }
                *floor = (*floor).min(chain.first);
                if entries.insert(ptr.offset, ptr.hash).is_none() && fresh(ptr, true) {
                    visit(ptr, object, len);
                }
                stack.extend(entry.commit.root);
                stack.extend(chain.retained_skip(entry));
                closures.mark(ptr, chain.first, after);
                closures.seed(entry, after);
                Ok(true)
            })?;
        }
        closures.walk(store, |ptr, entry, object, len| {
            if entries.insert(ptr.offset, ptr.hash).is_none() && fresh(ptr, true) {
                visit(ptr, object, len);
            }
            stack.extend(entry.commit.root);
            Ok(true)
        })?;
        stack.extend(&self.trees);
        while let Some(ptr) = stack.pop() {
            if entries.get(&ptr.offset) == Some(&ptr.hash) || !fresh(ptr, false) {
                continue;
            }
            let (object, len) = store.read_record(ptr)?;
            visit(ptr, &object, len);
            stack.extend(object.links.iter().copied());
        }
        Ok(())
    }
}

struct CopySink<'a> {
    source: Store,
    file: Arc<Traced>,
    buf: Vec<u8>,
    end: u64,
    scratch: Unpacked,
    bytes: Vec<u8>,
    body: Vec<u8>,
    cache: EntryCache,
    old: Vec<Ptr>,
    frames: Vec<Pending>,
    held: Held,
    copied: Copies,
    walked: HashMap<u64, Hash, ByOffset>,
    reached: HashMap<String, u64>,
    records: HashMap<u64, u64, ByOffset>,
    kept: Needs,
    needs: Needs,
    below: HashMap<u64, Below, ByOffset>,
    pruned: HashSet<Hash, ByHash>,
    objects: u64,
    stop: Option<&'a AtomicBool>,
    reserved: Option<Reserved>,
    units: Option<Units>,
    fallback: Option<Key>,
    stale: bool,
}

#[derive(Clone, Copy)]
enum Copied {
    Object(u64),
    Entry,
    Both(u64),
}

impl Copied {
    fn entered(copied: Option<Copied>) -> Copied {
        match copied {
            None | Some(Copied::Entry) => Copied::Entry,
            Some(Copied::Object(offset) | Copied::Both(offset)) => Copied::Both(offset),
        }
    }

    fn is_entry(self) -> bool {
        !matches!(self, Copied::Object(_))
    }
}

enum Link {
    Copied(Ptr),
    Entry,
    Object,
}

impl Link {
    fn parent(found: Option<Ptr>) -> Link {
        found.map_or(Link::Entry, Link::Copied)
    }
}

#[derive(Clone, Copy)]
struct Below {
    from: u64,
    prev: u64,
    skip: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Need {
    floor: u64,
    after: u64,
}

impl Need {
    fn new(floor: u64, after: Option<u64>) -> Need {
        Need {
            floor,
            after: after.map_or(0, |after| after.saturating_add(1)),
        }
    }

    fn after(self) -> Option<u64> {
        self.after.checked_sub(1)
    }

    fn covers(self, other: Need) -> bool {
        self.floor <= other.floor && self.after <= other.after
    }
}

#[derive(Default)]
struct Needs {
    one: HashMap<u64, Need, ByOffset>,
    more: HashMap<u64, Vec<Need>, ByOffset>,
}

impl Needs {
    fn covers(&self, at: u64, need: Need) -> bool {
        let more = self.more.get(&at).into_iter().flatten();
        self.one
            .get(&at)
            .into_iter()
            .chain(more)
            .any(|kept| kept.covers(need))
    }

    fn take(&mut self, at: u64) -> impl Iterator<Item = Need> + use<> {
        let more = self.more.remove(&at).into_iter().flatten();
        self.one.remove(&at).into_iter().chain(more)
    }

    fn add(&mut self, at: u64, need: Need) {
        let Some(one) = self.one.get(&at).copied() else {
            self.one.insert(at, need);
            return;
        };
        let mut kept = vec![one];
        kept.extend(self.more.remove(&at).into_iter().flatten());
        kept.retain(|kept| !need.covers(*kept));
        self.one.insert(at, need);
        if !kept.is_empty() {
            self.more.insert(at, kept);
        }
    }

    fn hand(&mut self, at: u64, to: &mut Needs) {
        if let Some(one) = self.one.remove(&at) {
            to.one.insert(at, one);
        }
        if let Some(more) = self.more.remove(&at) {
            to.more.insert(at, more);
        }
    }

    fn clear(&mut self) {
        self.one.clear();
        self.more.clear();
    }
}

#[derive(Default)]
struct Held {
    payload: Vec<u8>,
    old: Vec<Ptr>,
    links: Vec<Ptr>,
}

impl Held {
    fn release(&mut self, pending: &Pending) {
        self.payload.truncate(pending.payload);
        self.old.truncate(pending.old);
        self.links.truncate(pending.links);
    }

    fn trim(&mut self) {
        self.payload.clear();
        self.old.clear();
        self.links.clear();
        self.payload.shrink_to(SPARE_BYTES);
        self.old.shrink_to(SPARE_BYTES / size_of::<Ptr>());
        self.links.shrink_to(SPARE_BYTES / size_of::<Ptr>());
    }
}

#[derive(Clone, Copy)]
struct Pending {
    from: Ptr,
    raw: usize,
    packed: bool,
    next: usize,
    payload: usize,
    old: usize,
    links: usize,
    entry: Option<Rungs>,
}

enum Step {
    Done(Ptr),
    Descend(Pending),
}

#[derive(Clone, Copy)]
struct Rungs {
    version: u64,
    root: bool,
    prev: bool,
    skip: bool,
}

impl Rungs {
    fn of(body: &[u8], links: &[Ptr]) -> Result<Rungs> {
        let entry = Entry::decode(&mut Decoder::new(body, links, None))?;
        Ok(Rungs::from_entry(&entry))
    }

    fn from_entry(entry: &Entry) -> Rungs {
        Rungs {
            version: entry.commit.version,
            root: entry.commit.root.is_some(),
            prev: entry.prev.is_some(),
            skip: entry.skip.is_some(),
        }
    }

    fn parents(self) -> usize {
        usize::from(self.root)
    }

    fn prev(self) -> Option<usize> {
        self.prev.then(|| self.parents())
    }

    fn skip(self) -> Option<usize> {
        self.skip.then(|| self.parents() + usize::from(self.prev))
    }
}

struct Spot {
    hash: Hash,
    start: u32,
    len: u32,
    rungs: Rungs,
}

/// The records of log entries that the walk down the chains read and the copy will want, kept by
/// offset so that the copy need not read and decode them again.
///
/// What its buffer and its map have allocated is counted against [`KEEP_BYTES`]. Once a read
/// takes that past the cap, the cache keeps nothing more until it is cleared, and the copy reads
/// the rest of the entries from the file, so it holds at most the cap and the last growth of its
/// buffer or map.
#[derive(Default)]
struct EntryCache {
    bytes: Vec<u8>,
    at: HashMap<u64, Spot, ByOffset>,
    last: Option<usize>,
    full: bool,
}

impl EntryCache {
    fn read(&mut self, source: &Store, ahead: Option<&Ahead>, ptr: Ptr) -> Result<(Object, u32)> {
        self.skip();
        if let Some(fetched) = ahead.and_then(|ahead| ahead.take(ptr))
            && let Some(object) = object_of(fetched.record(), ptr.offset)
        {
            self.hold(fetched.record());
            return Ok((object, fetched.len));
        }
        if self.full || self.held() >= KEEP_BYTES {
            self.full = true;
            return source.read_record(ptr);
        }
        let start = self.bytes.len();
        let read = source.read_record_into(ptr, Some(&mut self.bytes))?;
        if self.held() > KEEP_BYTES {
            self.bytes.truncate(start);
            self.full = true;
        } else {
            self.last = Some(start);
        }
        Ok(read)
    }

    fn held(&self) -> usize {
        self.bytes.capacity() + self.at.capacity() * (size_of::<(u64, Spot)>() + 1)
    }

    fn hold(&mut self, record: &[u8]) {
        if self.full || self.held() >= KEEP_BYTES {
            self.full = true;
            return;
        }
        let start = self.bytes.len();
        self.bytes.extend_from_slice(record);
        if self.held() > KEEP_BYTES {
            self.bytes.truncate(start);
            self.full = true;
        } else {
            self.last = Some(start);
        }
    }

    fn keep(&mut self, ptr: Ptr, entry: &Entry) {
        if let Some(start) = self.last.take()
            && let (Ok(at), Ok(len)) = (
                u32::try_from(start),
                u32::try_from(self.bytes.len() - start),
            )
        {
            let spot = Spot {
                hash: ptr.hash,
                start: at,
                len,
                rungs: Rungs::from_entry(entry),
            };
            self.at.insert(ptr.offset, spot);
        }
    }

    fn skip(&mut self) {
        if let Some(start) = self.last.take() {
            self.bytes.truncate(start);
        }
    }

    fn take(&mut self, ptr: Ptr) -> Option<(&[u8], Rungs)> {
        let spot = self.at.remove(&ptr.offset)?;
        let start = spot.start as usize;
        let record = &self.bytes[start..start + spot.len as usize];
        (spot.hash == ptr.hash).then_some((record, spot.rungs))
    }

    fn clear(&mut self) {
        *self = EntryCache::default();
    }
}

/// The object the record `record`, read at `offset`, holds, or `None` if it does not parse whole.
fn object_of(record: &[u8], offset: u64) -> Option<Object> {
    let mut links = Vec::new();
    let stored = stored_within(record, offset, &mut links)?;
    let mut out = Vec::new();
    let body = body_of(record, &stored, offset, &mut out).ok()?;
    let body = match stored.packed {
        true => out,
        false => body.to_vec(),
    };
    Some(Object { body, links })
}

impl CopySink<'_> {
    fn drain(&mut self) -> Result<()> {
        let start = self.end - self.buf.len() as u64;
        self.file.write_all_at(&self.buf, start)?;
        start_writeback(&self.file, start, self.buf.len() as u64);
        self.buf.clear();
        self.buf.shrink_to(2 * FLUSH_BYTES);
        Ok(())
    }

    fn emit(&mut self, from: Ptr, staged: Staged<'_>, below: Option<Below>) -> Result<Ptr> {
        self.flush()?;
        let copied = match below {
            Some(_) => self.records.get(&from.offset).map(|offset| Ptr {
                hash: from.hash,
                offset: *offset,
            }),
            None => self.object(from.hash),
        };
        if let Some(copied) = copied {
            return Ok(copied);
        }
        let (hash, offset) = (from.hash, self.end);
        let before = self.buf.len();
        append_staged(&mut self.buf, offset, staged);
        self.end += (self.buf.len() - before) as u64;
        match below {
            Some(below) => {
                self.copied.update(&hash, Copied::entered);
                self.records.insert(from.offset, offset);
                self.below.insert(offset, below);
                self.needs.hand(from.offset, &mut self.kept);
            }
            None => {
                self.copied.update(&hash, |copied| match copied {
                    None => Copied::Object(offset),
                    Some(Copied::Entry) => Copied::Both(offset),
                    Some(copied) => copied,
                });
                if let Some(units) = &self.units {
                    let key = self.fallback.unwrap_or_else(|| units.walk_key());
                    units.place(&hash, key, offset);
                }
            }
        }
        self.objects += 1;
        if self.buf.len() >= FLUSH_BYTES {
            self.drain()?;
        }
        Ok(Ptr { hash, offset })
    }

    fn wanted(&mut self, ptr: Ptr, need: Need, again: &mut Vec<(Ptr, Need)>) -> bool {
        let at = ptr.offset;
        if self.needs.covers(at, need) {
            return false;
        }
        if self.records.contains_key(&at) {
            if self.kept.covers(at, need) {
                return false;
            }
            self.records.remove(&at);
            let kept = self.kept.take(at).filter(|kept| !need.covers(*kept));
            again.extend(kept.map(|kept| (ptr, kept)));
        }
        self.needs.add(at, need);
        true
    }

    /// The part of `chain`, the branch `name`'s, for threads to read ahead of the walk.
    ///
    /// In the first pass it is the whole chain. In a later one, which `earlier` marks, it is only
    /// the versions above the newest an earlier pass walked on the branch, since the walk stops
    /// at an entry it walked before. It is `None` for a chain whose tip was walked, and in a later
    /// pass for a branch that no earlier pass walked or that has no newer versions.
    fn unread(&self, name: &str, chain: &Chain, earlier: bool) -> Option<Chain> {
        let walked = chain
            .tip
            .is_some_and(|tip| self.walked.get(&tip.offset) == Some(&tip.hash));
        match self.reached.get(name) {
            _ if walked => None,
            Some(&reached) => (reached < chain.last).then(|| Chain {
                first: chain.first.max(reached + 1),
                ..*chain
            }),
            None => (!earlier).then_some(*chain),
        }
    }

    fn placed(&self, hash: Hash) -> Option<Ptr> {
        let offset = self.units.as_ref()?.placed(&hash)?;
        Some(Ptr { hash, offset })
    }

    fn object(&self, hash: Hash) -> Option<Ptr> {
        let Some(copied) = self.copied.get(&hash) else {
            return self.placed(hash);
        };
        match copied {
            Copied::Object(offset) | Copied::Both(offset) => Some(Ptr { hash, offset }),
            Copied::Entry => None,
        }
    }

    fn entry(&self, ptr: Ptr) -> Option<Ptr> {
        match self.copied.get(&ptr.hash)? {
            Copied::Object(_) => None,
            Copied::Entry | Copied::Both(_) => self.records.get(&ptr.offset).map(|offset| Ptr {
                hash: ptr.hash,
                offset: *offset,
            }),
        }
    }

    fn linked(&self, link: Ptr) -> Result<Link> {
        if link.offset == 0 {
            return Ok(Link::Object);
        }
        let copied = match self.copied.get(&link.hash) {
            None if self.pruned.contains(&link.hash) => {
                return Err(Error::Corrupt(format!(
                    "link to the pruned log entry at {}",
                    link.offset
                )));
            }
            None => return Ok(self.placed(link.hash).map_or(Link::Object, Link::Copied)),
            Some(copied) => copied,
        };
        let walked = || self.walked.get(&link.offset) == Some(&link.hash);
        Ok(match copied {
            Copied::Object(offset) => Link::Copied(Ptr {
                hash: link.hash,
                offset,
            }),
            Copied::Entry | Copied::Both(_) if walked() => Link::parent(self.entry(link)),
            Copied::Both(offset) => Link::Copied(Ptr {
                hash: link.hash,
                offset,
            }),
            Copied::Entry => Link::Object,
        })
    }

    fn parent(&self, link: Ptr) -> Option<Ptr> {
        let tombstone = Ptr {
            hash: link.hash,
            offset: 0,
        };
        if link.offset == 0 {
            return Some(tombstone);
        }
        self.entry(link)
            .or_else(|| self.pruned.contains(&link.hash).then_some(tombstone))
    }

    fn below(from: Ptr, links: &[Ptr], rungs: Rungs) -> Below {
        let offset = |index: Option<usize>| index.map_or(0, |index| links[index].offset);
        Below {
            from: from.offset,
            prev: offset(rungs.prev()),
            skip: offset(rungs.skip()),
        }
    }

    fn on_path(&self, mut at: u64, mut version: u64, target: u64) -> Option<u64> {
        while version > target {
            let below = self.below.get(&at)?;
            let skip = skip_version(version);
            (at, version) = if below.skip != 0 && skip >= target {
                (below.skip, skip)
            } else {
                (below.prev, version - 1)
            };
            if at == 0 {
                return None;
            }
        }
        (version == target).then_some(at)
    }

    fn skipped(&self, links: &[Ptr], rungs: Rungs, link: Ptr) -> Result<Option<Ptr>> {
        let target = skip_version(rungs.version);
        let prev = rungs
            .prev()
            .map(|index| links[index])
            .filter(|prev| prev.offset != 0);
        let found = prev.and_then(|prev| self.on_path(prev.offset, rungs.version - 1, target));
        let found = found.map(|offset| (offset, self.below.get(&offset)));
        if let Some((_, Some(below))) = found
            && self
                .walked
                .get(&below.from)
                .is_some_and(|hash| *hash != link.hash)
        {
            return Err(Error::Corrupt(format!(
                "commit log v{} skips to another entry than v{target}",
                rungs.version
            )));
        }
        Ok(match found {
            None => Some(Ptr {
                hash: link.hash,
                offset: 0,
            }),
            Some((offset, Some(below))) if below.from == link.offset => Some(Ptr {
                hash: link.hash,
                offset,
            }),
            Some(_) => self.parent(link),
        })
    }

    fn read(&mut self, ptr: Ptr, entry: bool) -> Result<Step> {
        if self.stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
            return Err(Error::Interrupted);
        }
        let mut bytes = mem::take(&mut self.bytes);
        let step = self.step(ptr, entry, &mut bytes);
        if bytes.capacity() <= SPARE_BYTES {
            self.bytes = bytes;
        }
        self.reserved = None;
        step.map_err(|error| self.first(error))
    }

    fn first(&mut self, error: Error) -> Error {
        if matches!(error, Error::Interrupted) {
            return error;
        }
        match self.flush() {
            Err(earlier) => earlier,
            Ok(()) => error,
        }
    }

    fn step(&mut self, ptr: Ptr, entry: bool, bytes: &mut Vec<u8>) -> Result<Step> {
        if entry
            && let Some((record, rungs)) = self.cache.take(ptr)
            && let Some(stored) = stored_within(record, ptr.offset, &mut self.old)
        {
            let pending = Pending {
                from: ptr,
                raw: stored.raw,
                packed: stored.packed,
                next: 0,
                payload: self.held.payload.len(),
                old: self.held.old.len(),
                links: self.held.links.len(),
                entry: Some(rungs),
            };
            self.held
                .payload
                .extend_from_slice(&record[stored.payload.clone()]);
            self.held.old.extend_from_slice(&self.old);
            self.prefetch(&self.old);
            return Ok(Step::Descend(pending));
        }
        let (units, reserved) = (&self.units, &mut self.reserved);
        let large = |claim: usize| {
            *reserved = None;
            *reserved = units.as_ref().map(|units| units.reserve(claim as u64));
            Ok(())
        };
        self.copied.prefetch(&ptr.hash);
        let read = self.source.read_stored(ptr, bytes, &mut self.old, large);
        self.reserved = None;
        let (record, stored) = read?;
        self.prefetch(&self.old);
        if !entry || self.walked.get(&ptr.offset) != Some(&ptr.hash) {
            verify_stored(record, &stored, &self.old, ptr, &mut self.scratch)?;
        }
        let entry = match entry {
            true => {
                let body = body_of(record, &stored, ptr.offset, &mut self.body)?;
                Some(Rungs::of(body, &self.old)?)
            }
            false => None,
        };
        let payload = &record[stored.payload.clone()];
        let links = self.held.links.len();
        if entry.is_none() {
            while let Some(link) = self.old.get(self.held.links.len() - links) {
                let Link::Copied(done) = self.linked(*link)? else {
                    break;
                };
                self.held.links.push(done);
            }
            if self.held.links.len() - links == self.old.len() {
                let held = mem::take(&mut self.held);
                let staged = Staged {
                    raw: stored.raw,
                    payload,
                    packed: stored.packed,
                    links: &held.links[links..],
                };
                let done = self.emit(ptr, staged, None);
                self.held = held;
                self.held.links.truncate(links);
                return Ok(Step::Done(done?));
            }
        }
        let pending = Pending {
            from: ptr,
            raw: stored.raw,
            packed: stored.packed,
            next: self.held.links.len() - links,
            payload: self.held.payload.len(),
            old: self.held.old.len(),
            links,
            entry,
        };
        self.held.payload.extend_from_slice(payload);
        self.held.old.extend_from_slice(&self.old);
        Ok(Step::Descend(pending))
    }

    fn prefetch(&self, links: &[Ptr]) {
        for link in links {
            self.copied.prefetch(&link.hash);
            if let Some(units) = &self.units {
                units.prefetch(&link.hash);
            }
        }
    }

    fn prefetch_copies(&self, links: &[Ptr]) {
        for link in links {
            self.copied.prefetch(&link.hash);
            if let Some(units) = &self.units {
                units.prefetch_copy(*link);
            }
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.settle(0)
    }

    fn settle(&mut self, until: usize) -> Result<()> {
        if self.fallback.is_some() {
            return Ok(());
        }
        loop {
            let stop = self.stop;
            let Some(units) = self.units.as_mut().filter(|units| units.outstanding() > 0) else {
                return Ok(());
            };
            match units.settle(stop, until)? {
                Settled::Ahead => return Ok(()),
                Settled::Done { end, objects } => {
                    self.end = end;
                    self.objects += objects;
                    return Ok(());
                }
                Settled::Tainted { unit, end } => {
                    self.end = end;
                    self.fallback = Some(unit.key());
                    let copied = self.copy(unit.root());
                    self.fallback = None;
                    copied?;
                    self.drain()?;
                    let end = self.end;
                    if let Some(units) = &mut self.units {
                        units.resolved(end);
                    }
                }
            }
        }
    }

    fn dispatch(&mut self, link: Ptr) -> Result<Ptr> {
        if mem::take(&mut self.stale) {
            let mut taint = self.pruned.clone();
            let entries = self.copied.iter().filter(|(_, copied)| copied.is_entry());
            taint.extend(entries.map(|(hash, _)| *hash));
            if let Some(units) = &mut self.units {
                units.taint(taint);
            }
        }
        if self
            .units
            .as_ref()
            .is_some_and(|units| units.unplaced() >= AHEAD as u64)
        {
            self.settle(AHEAD / 2)?;
        }
        if self
            .units
            .as_ref()
            .is_some_and(|units| units.outstanding() == 0)
        {
            self.drain()?;
        }
        let end = self.end;
        let offset = match &mut self.units {
            Some(units) => units.dispatch(link, end),
            None => PENDING,
        };
        Ok(Ptr {
            hash: link.hash,
            offset,
        })
    }

    fn resolve(&self, links: &mut [Ptr]) -> Result<()> {
        for link in links.iter_mut().filter(|link| link.offset >= PENDING) {
            let pending = self
                .units
                .as_ref()
                .and_then(|units| units.pending(link.offset));
            link.offset = pending
                .or_else(|| self.placed(link.hash).map(|placed| placed.offset))
                .ok_or_else(|| {
                    Error::Corrupt(format!("the copy lost the tree at {}", link.offset))
                })?;
        }
        Ok(())
    }

    fn dispatches(&self) -> bool {
        self.units.is_some() && self.fallback.is_none()
    }

    fn hands_out(&self, pending: &Pending) -> bool {
        pending.entry.is_none()
            && !self.held.old[pending.old..].iter().any(|link| {
                link.offset == 0
                    || self.pruned.contains(&link.hash)
                    || self
                        .copied
                        .get(&link.hash)
                        .is_some_and(|copied| copied.is_entry())
            })
    }

    fn copy(&mut self, root: Ptr) -> Result<Ptr> {
        let unit = self.dispatches();
        if !unit {
            self.flush()?;
        }
        let linked = self.linked(root);
        let entry = match linked.map_err(|error| self.first(error))? {
            Link::Copied(done) => return Ok(done),
            Link::Entry => true,
            Link::Object => false,
        };
        let base = self.frames.len();
        match self.read(root, entry)? {
            Step::Done(done) => return Ok(done),
            Step::Descend(pending) if unit && self.hands_out(&pending) => {
                self.held.release(&pending);
                return self.dispatch(root);
            }
            Step::Descend(pending) => self.frames.push(pending),
        }
        loop {
            let top = *self.frames.last().expect("stack is not empty");
            if top.old + top.next < self.held.old.len() {
                let link = self.held.old[top.old + top.next];
                let unit = self.dispatches();
                let rooted = top.entry.is_some();
                let linked = match top.entry {
                    Some(rungs) if rungs.skip() == Some(top.next) => {
                        let skipped = self.skipped(&self.held.links[top.links..], rungs, link);
                        Link::parent(skipped.map_err(|error| self.first(error))?)
                    }
                    Some(rungs) if top.next >= rungs.parents() => Link::parent(self.parent(link)),
                    _ => {
                        if !unit {
                            self.flush()?;
                        }
                        let linked = self.linked(link);
                        linked.map_err(|error| self.first(error))?
                    }
                };
                let done = match linked {
                    Link::Copied(done) => done,
                    Link::Object if unit && rooted => self.dispatch(link)?,
                    reached => match self.read(link, matches!(reached, Link::Entry))? {
                        Step::Done(done) => done,
                        Step::Descend(pending) if unit && self.hands_out(&pending) => {
                            self.held.release(&pending);
                            self.dispatch(link)?
                        }
                        Step::Descend(pending) => {
                            self.frames.push(pending);
                            continue;
                        }
                    },
                };
                self.held.links.push(done);
                self.frames.last_mut().expect("stack is not empty").next += 1;
                continue;
            }
            let done = self.frames.pop().expect("stack is not empty");
            self.flush()?;
            let mut held = mem::take(&mut self.held);
            let resolved = self.resolve(&mut held.links[done.links..]);
            let below = done
                .entry
                .map(|rungs| Self::below(done.from, &held.links[done.links..], rungs));
            let staged = Staged {
                raw: done.raw,
                payload: &held.payload[done.payload..],
                packed: done.packed,
                links: &held.links[done.links..],
            };
            let ptr = resolved.and_then(|()| self.emit(done.from, staged, below));
            held.release(&done);
            self.held = held;
            let ptr = ptr?;
            if self.frames.len() == base {
                return Ok(ptr);
            }
            let parent = self.frames.last().expect("stack is not empty");
            self.copied.prefetch(&parent.from.hash);
            self.prefetch_copies(&self.held.links[parent.links..]);
            self.held.links.push(ptr);
            self.frames.last_mut().expect("stack is not empty").next += 1;
        }
    }
}

impl Sink for CopySink<'_> {
    fn put(&mut self, object: Object, hash: Hash) -> Result<Ptr> {
        if let Some(done) = self.object(hash) {
            return Ok(done);
        }
        let mut links = Vec::with_capacity(object.links.len());
        for link in &object.links {
            links.push(self.copy(*link)?);
        }
        self.flush()?;
        self.resolve(&mut links)?;
        let packed = compressed(&object.body);
        let staged = staged(&object.body, packed.as_deref(), &links);
        self.emit(Ptr { hash, offset: 0 }, staged, None)
    }

    fn store_id(&self) -> u64 {
        self.source.id()
    }
}

/// A copy of what a branch table retains into a new file, made in passes.
///
/// Each pass copies what the earlier ones did not, and [`Copier::finish`] copies the rest, writes
/// the branch table and seals the file, which can then be renamed over the old one.
pub(crate) struct Copier<'a> {
    sink: CopySink<'a>,
}

impl<'a> Copier<'a> {
    /// Creates `temp`, empty, with the permissions and owner of the file `store` reads, to copy
    /// `store` into.
    ///
    /// Setting `stop` makes the copy fail with [`Error::Interrupted`] at its next read.
    pub(crate) fn create(
        store: &Store,
        temp: &Path,
        options: &StoreOptions,
        stop: Option<&'a AtomicBool>,
    ) -> Result<Copier<'a>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(temp)?;
        let source = store.metadata()?;
        file.set_permissions(source.permissions())?;
        copy_owner(&file, &source);
        let file = Arc::new(Traced::new(file, options.trace.clone())?);
        file.created();
        file.set_len(LOG_START)?;
        let threads = copy_threads(options);
        let large = store.size() >= LOG_START + SPAWN_PAST;
        let spawn = match large || options.copy_threads > 1 {
            true => threads,
            false => 1,
        };
        let units = (threads > 1).then(|| Units::new(store.clone(), file.clone(), spawn));
        Ok(Copier {
            sink: CopySink {
                source: store.clone(),
                file,
                buf: Vec::new(),
                end: LOG_START,
                scratch: Unpacked::default(),
                bytes: Vec::new(),
                body: Vec::new(),
                cache: EntryCache::default(),
                old: Vec::new(),
                frames: Vec::new(),
                held: Held::default(),
                copied: Copies::default(),
                walked: HashMap::default(),
                reached: HashMap::new(),
                records: HashMap::default(),
                kept: Needs::default(),
                needs: Needs::default(),
                below: HashMap::default(),
                pruned: HashSet::default(),
                objects: 0,
                stop,
                units,
                reserved: None,
                fallback: None,
                stale: true,
            },
        })
    }

    fn encode(&mut self, refs: &Refs, genesis: Option<Ptr>) -> Result<Ptr> {
        let sink = &mut self.sink;
        let source = sink.source.clone();
        let mut retained: HashSet<Hash, ByHash> = HashSet::default();
        let mut edge = Vec::new();
        let mut again = Vec::new();
        let mut closures = Closures::exact();
        let cache = RefCell::new(mem::take(&mut sink.cache));
        let ahead = sink.units.as_ref().map(Units::ahead);
        let read = |ptr| cache.borrow_mut().read(&source, ahead.as_deref(), ptr);
        let earlier = !sink.walked.is_empty();
        for (name, branch) in refs.try_entries()? {
            let (first, after) = (branch.log.first, branch.log.after(&source)?);
            let root = branch.root;
            closures.chain(&branch.log);
            let unread = sink.unread(&name, &branch.log, earlier);
            if let Some(unread) = unread {
                let versions = unread.last.saturating_sub(unread.first).saturating_add(1);
                edge.reserve(3 * versions.min(RESERVE) as usize);
            }
            if let (Some(units), Some(unread)) = (sink.units.as_mut(), unread) {
                units.read_ahead(&unread);
            }
            let visited = branch.log.visit_with(root, read, |ptr, entry, _, _| {
                if !sink.wanted(ptr, Need::new(first, after), &mut again) {
                    return Ok(false);
                }
                cache.borrow_mut().keep(ptr, entry);
                if let (Some(units), Some(root)) = (&mut sink.units, entry.commit.root) {
                    units.check_ahead(root);
                }
                retained.insert(ptr.hash);
                sink.walked.insert(ptr.offset, ptr.hash);
                edge.extend(links_of(entry));
                closures.mark(ptr, first, after);
                closures.seed(entry, after);
                Ok(true)
            });
            if let Some(units) = &mut sink.units {
                units.end_ahead();
            }
            visited?;
            sink.reached.insert(name, branch.log.last);
        }
        loop {
            for (ptr, need) in again.drain(..) {
                closures.push(ptr, need.floor, need.after());
            }
            closures.walk_bounded(read, |ptr, entry, _, _, (floor, after)| {
                if !sink.wanted(ptr, Need::new(floor, after), &mut again) {
                    return Ok(false);
                }
                cache.borrow_mut().keep(ptr, entry);
                if let (Some(units), Some(root)) = (&mut sink.units, entry.commit.root) {
                    units.check_ahead(root);
                }
                retained.insert(ptr.hash);
                sink.walked.insert(ptr.offset, ptr.hash);
                edge.extend(links_of(entry));
                Ok(true)
            })?;
            if again.is_empty() {
                break;
            }
        }
        sink.cache = cache.into_inner();
        sink.cache.skip();
        if let Some(units) = &mut sink.units {
            units.publish_checks();
        }
        for hash in &retained {
            sink.pruned.remove(hash);
            sink.copied.update(hash, Copied::entered);
        }
        for hash in edge {
            let copied = sink.copied.get(&hash);
            if !retained.contains(&hash) && !copied.is_some_and(Copied::is_entry) {
                sink.pruned.insert(hash);
            }
        }
        sink.stale = true;
        let mut encoder = Encoder::new(&mut *sink);
        refs::encode_table(&mut encoder, refs, genesis)?;
        let head = encoder.finish();
        sink.needs.clear();
        sink.cache.clear();
        sink.held.trim();
        sink.frames = Vec::new();
        let head = head?;
        sink.flush()?;
        if let Some(units) = &mut sink.units {
            let stop = sink.stop;
            units.written(stop)?;
        }
        Ok(head)
    }

    /// Copies everything `refs` retains, and version 0 at `genesis`, that earlier passes did not,
    /// checking every object's hash, and returns the bytes written.
    pub(crate) fn pass(&mut self, refs: &Refs, genesis: Option<Ptr>) -> Result<u64> {
        let before = self.sink.end;
        self.encode(refs, genesis)?;
        self.sink.drain()?;
        Ok(self.sink.end - before)
    }

    /// The number of trees handed to the copy's other threads so far.
    pub(crate) fn handed_out(&self) -> u64 {
        self.sink.units.as_ref().map_or(0, Units::handed_out)
    }

    /// The size the copy has reached, in bytes.
    pub(crate) fn end(&self) -> u64 {
        self.sink.end
    }

    /// Syncs what the copy has written to disk.
    pub(crate) fn sync(&self) -> Result<()> {
        self.sink.file.sync_data()?;
        Ok(())
    }

    /// Copies what is left for `refs` and `genesis`, writes the branch table, and seals the copy
    /// with a superblock at sequence `seq` that records `survival`, the share of new bytes that
    /// survived.
    ///
    /// It returns the branch table's pointer in the copy and the number of objects copied in all.
    pub(crate) fn finish(
        &mut self,
        refs: &Refs,
        genesis: Option<Ptr>,
        seq: u64,
        survival: f64,
    ) -> Result<(Ptr, u64)> {
        let head = self.encode(refs, genesis)?;
        self.sink.drain()?;
        let basis = Basis::new(self.sink.end, survival);
        seal(&self.sink.file, self.sink.end, head, seq, basis)?;
        Ok((head, self.sink.objects))
    }

    /// Removes a copy that will not be used, ignoring a failure to.
    pub(crate) fn abandon(temp: &Path) {
        let _ = fs::remove_file(temp);
    }
}
