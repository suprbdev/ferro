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

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::hash::BuildHasherDefault;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::codec::{Data, Decoder, Encoder, Hash, Object, Ptr, Sink, Staged};
use crate::db::Refs;
use crate::error::{Error, Result};
use crate::log::{Chain, Closures, Entry, skip_version};
use crate::platform::{copy_owner, start_writeback};
pub(crate) use crate::platform::{rename, sync_parent};
use crate::refs;
use crate::store::{
    Basis, LOG_START, Prehashed, Store, StoreOptions, Traced, Unpacked, append_staged, body_of,
    compressed, seal, staged, verify_stored,
};

const FLUSH_BYTES: usize = 1 << 20;
const BATCH_BYTES: usize = 256 << 10;
const BATCH_RECORDS: usize = 4096;
const SPARE: usize = 64;
const SPARE_BYTES: usize = 64 << 10;
const COPY_THREADS: usize = 4;

type ByHash = BuildHasherDefault<Prehashed>;

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
    /// The size of the file before the cycle, in bytes.
    pub before_bytes: u64,
    /// The size of the new file, in bytes.
    pub after_bytes: u64,
    /// The number of objects copied into the new file.
    pub objects: u64,
    /// The number of commits the cycle pruned, across every branch.
    pub commits_dropped: u64,
    /// The number of copy passes made before the last, which runs while writers wait.
    pub passes: u32,
    /// How long writers waited for the last pass and the swap.
    pub pause: Duration,
}

/// How a database file's size compares with what it holds, from [`Db::space`](crate::Db::space)
/// or [`admin::space`](crate::admin::space).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Space {
    /// The size of the file, in bytes.
    pub file_bytes: u64,
    /// An estimate of the bytes that retained versions take: the size after the last cycle, plus
    /// the share of the growth since then that the last cycle found survives, or the whole file if
    /// no cycle has run on it.
    pub live_bytes: u64,
    /// The bytes of files that cycles replaced but that held snapshots, maps, logs or histories
    /// keep open.
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

fn links_of(entry: &Entry) -> Vec<Hash> {
    let merged = entry.merged.map(|merged| merged.ptr);
    entry
        .prev
        .iter()
        .chain(&entry.skip)
        .chain(&merged)
        .map(|link| link.hash)
        .collect()
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
    /// The retained commit log of each branch.
    pub(crate) chains: Vec<Chain>,
    /// The logs that older steps of the branch table record for the same branches, which the
    /// table still links.
    pub(crate) superseded: Vec<Chain>,
}

impl Roots {
    /// The roots of the branch table at `head`.
    pub(crate) fn of(store: &Store, head: Ptr) -> Result<Roots> {
        let heads = refs::heads(store, head)?;
        Ok(Roots {
            trees: vec![head],
            chains: heads.current,
            superseded: heads.superseded,
        })
    }

    /// Visits each object reachable from these roots through retained versions: log entries, the
    /// commits that merges brought in, the trees they hold and the branch table.
    ///
    /// `fresh` is asked whether each object is new, and only a new one is visited and descended
    /// into. `visit` gets each new object with its length in the file.
    ///
    /// # Errors
    ///
    /// The first read error, such as [`Error::Corrupt`] for an object whose bytes do not match its
    /// hash.
    pub(crate) fn walk(
        &self,
        store: &Store,
        mut fresh: impl FnMut(Ptr) -> bool,
        mut visit: impl FnMut(Ptr, &Object, u32),
    ) -> Result<()> {
        let mut stack = Vec::new();
        let mut entries = HashSet::new();
        let mut floors: HashMap<u64, u64> = HashMap::new();
        let mut closures = Closures::default();
        let mut superseded: Vec<&Chain> = self.superseded.iter().collect();
        superseded.sort_by_key(|chain| chain.first);
        let chains = self.chains.iter().map(|chain| (chain, false));
        for (chain, replaced) in chains.chain(superseded.into_iter().map(|chain| (chain, true))) {
            let after = chain.after(store)?;
            chain.visit(store, |ptr, entry, object, len| {
                let floor = floors.entry(ptr.offset).or_insert(u64::MAX);
                if replaced && *floor <= chain.first {
                    return Ok(false);
                }
                *floor = (*floor).min(chain.first);
                if entries.insert(ptr.offset) && fresh(ptr) {
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
            if entries.insert(ptr.offset) && fresh(ptr) {
                visit(ptr, object, len);
            }
            stack.extend(entry.commit.root);
            Ok(true)
        })?;
        stack.extend(&self.trees);
        while let Some(ptr) = stack.pop() {
            if entries.contains(&ptr.offset) || !fresh(ptr) {
                continue;
            }
            let (object, len) = store.read_record(ptr)?;
            visit(ptr, &object, len);
            stack.extend(object.links.iter().copied());
        }
        Ok(())
    }
}

#[derive(Default)]
struct Batch {
    seq: u64,
    bytes: Vec<u8>,
    records: Vec<(Ptr, usize)>,
}

impl Batch {
    fn full(&self) -> bool {
        self.bytes.len() >= BATCH_BYTES || self.records.len() >= BATCH_RECORDS
    }

    fn verify(&self, scratch: &mut Unpacked) -> Result<()> {
        let mut start = 0;
        for &(ptr, end) in &self.records {
            verify_stored(&self.bytes[start..end], ptr, scratch)?;
            start = end;
        }
        Ok(())
    }

    fn clear(&mut self) {
        self.bytes.clear();
        self.bytes.shrink_to(2 * BATCH_BYTES);
        self.records.clear();
    }
}

enum Failure {
    Failed(Error),
    Panicked(Box<dyn Any + Send>),
}

#[derive(Default)]
struct Progress {
    queued: usize,
    failure: Option<(u64, Failure)>,
}

struct Board {
    progress: Mutex<Progress>,
    idle: Condvar,
    failed: AtomicBool,
    first: AtomicU64,
    spare: Mutex<Vec<Batch>>,
}

impl Board {
    fn new() -> Board {
        Board {
            progress: Mutex::default(),
            idle: Condvar::new(),
            failed: AtomicBool::new(false),
            first: AtomicU64::new(u64::MAX),
            spare: Mutex::default(),
        }
    }

    fn fail(&self, progress: &mut Progress, seq: u64, failure: Failure) {
        self.failed.store(true, Ordering::Release);
        self.first.fetch_min(seq, Ordering::AcqRel);
        if progress
            .failure
            .as_ref()
            .is_none_or(|(first, _)| seq < *first)
        {
            progress.failure = Some((seq, failure));
        }
    }

    fn take(&self) -> Result<()> {
        let failure = self.progress.lock().failure.take();
        match failure {
            None => Ok(()),
            Some((_, Failure::Failed(error))) => Err(error),
            Some((_, Failure::Panicked(payload))) => panic::resume_unwind(payload),
        }
    }
}

fn verify_batches(work: &Mutex<Receiver<Batch>>, board: &Board) {
    let mut scratch = Unpacked::default();
    loop {
        let next = work.lock().recv();
        let Ok(mut batch) = next else {
            return;
        };
        let seq = batch.seq;
        let outcome = if seq > board.first.load(Ordering::Acquire) {
            Ok(Ok(()))
        } else {
            panic::catch_unwind(AssertUnwindSafe(|| batch.verify(&mut scratch)))
        };
        batch.clear();
        board.spare.lock().push(batch);
        let mut progress = board.progress.lock();
        progress.queued -= 1;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => board.fail(&mut progress, seq, Failure::Failed(error)),
            Err(payload) => board.fail(&mut progress, seq, Failure::Panicked(payload)),
        }
        if progress.queued == 0 {
            board.idle.notify_all();
        }
    }
}

struct Pool {
    queue: Option<SyncSender<Batch>>,
    board: Arc<Board>,
    threads: Vec<JoinHandle<()>>,
}

impl Pool {
    fn start(threads: usize) -> Option<Pool> {
        let (queue, work) = mpsc::sync_channel(threads * 2);
        let work = Arc::new(Mutex::new(work));
        let board = Arc::new(Board::new());
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let (work, board) = (work.clone(), board.clone());
            let spawned = thread::Builder::new()
                .name("ferro-verify".to_string())
                .spawn(move || verify_batches(&work, &board));
            match spawned {
                Ok(handle) => handles.push(handle),
                Err(_) => break,
            }
        }
        if handles.is_empty() {
            return None;
        }
        Some(Pool {
            queue: Some(queue),
            board,
            threads: handles,
        })
    }

    fn send(&self, batch: Batch) -> Result<()> {
        self.board.progress.lock().queued += 1;
        let queue = self
            .queue
            .as_ref()
            .expect("the queue lives as long as the pool");
        let seq = batch.seq;
        if queue.send(batch).is_err() {
            let mut progress = self.board.progress.lock();
            progress.queued -= 1;
            let gone = Error::Io(std::io::Error::other("the copy's verifying threads exited"));
            self.board.fail(&mut progress, seq, Failure::Failed(gone));
        }
        self.check()
    }

    fn check(&self) -> Result<()> {
        if self.board.failed.load(Ordering::Acquire) {
            return self.settle();
        }
        Ok(())
    }

    fn settle(&self) -> Result<()> {
        let mut progress = self.board.progress.lock();
        while progress.queued > 0 {
            self.board.idle.wait(&mut progress);
        }
        drop(progress);
        self.board.take()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.board.failed.store(true, Ordering::Release);
        self.board.first.store(0, Ordering::Release);
        self.queue = None;
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

struct Verifier {
    threads: usize,
    batch: Batch,
    sent: u64,
    scratch: Unpacked,
    pool: Option<Pool>,
}

impl Verifier {
    fn new(threads: usize) -> Verifier {
        Verifier {
            threads,
            batch: Batch::default(),
            sent: 0,
            scratch: Unpacked::default(),
            pool: None,
        }
    }

    fn inline(&mut self, record: &[u8], ptr: Ptr) -> Result<()> {
        verify_stored(record, ptr, &mut self.scratch)
    }

    fn add(&mut self, ptr: Ptr, record: &[u8]) -> Result<()> {
        if self.threads <= 1 {
            return self.inline(record, ptr);
        }
        self.batch.bytes.extend_from_slice(record);
        self.batch.records.push((ptr, self.batch.bytes.len()));
        if !self.batch.full() {
            return Ok(());
        }
        if self.pool.is_none() {
            self.pool = Pool::start(self.threads - 1);
            if self.pool.is_none() {
                self.threads = 1;
            }
        }
        let Some(pool) = &self.pool else {
            let verified = self.batch.verify(&mut self.scratch);
            self.batch.clear();
            return verified;
        };
        let spare = pool.board.spare.lock().pop().unwrap_or_default();
        self.sent += 1;
        let mut batch = mem::replace(&mut self.batch, spare);
        batch.seq = self.sent;
        pool.send(batch)
    }

    fn check(&self) -> Result<()> {
        self.pool.as_ref().map_or(Ok(()), Pool::check)
    }

    fn settle(&mut self) -> Result<()> {
        let verified = self.batch.verify(&mut self.scratch);
        self.batch.clear();
        let settled = self.pool.as_ref().map_or(Ok(()), Pool::settle);
        settled.and(verified)
    }

    fn first(&mut self, error: Error) -> Error {
        match self.settle() {
            Err(earlier) => earlier,
            Ok(()) => error,
        }
    }
}

struct CopySink<'a> {
    source: Store,
    file: Traced,
    buf: Vec<u8>,
    end: u64,
    verifier: Verifier,
    bytes: Vec<u8>,
    body: Vec<u8>,
    spare: Vec<Buffers>,
    copied: HashMap<Hash, Copied, ByHash>,
    entries: HashSet<Hash, ByHash>,
    records: HashMap<u64, u64>,
    kept: Needs,
    needs: Needs,
    below: HashMap<u64, Below>,
    pruned: HashSet<Hash, ByHash>,
    objects: u64,
    stop: Option<&'a AtomicBool>,
}

#[derive(Clone, Copy)]
enum Copied {
    Object(u64),
    Entry,
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
    one: HashMap<u64, Need>,
    more: HashMap<u64, Vec<Need>>,
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
struct Buffers {
    payload: Vec<u8>,
    old: Vec<Ptr>,
    links: Vec<Ptr>,
}

struct Pending {
    from: Ptr,
    raw: usize,
    packed: bool,
    next: usize,
    held: Buffers,
    entry: Option<Rungs>,
}

enum Step {
    Done(Ptr),
    Descend(Pending),
}

#[derive(Clone, Copy)]
struct Rungs {
    version: u64,
    parents: usize,
    prev: Option<usize>,
    skip: Option<usize>,
}

impl Rungs {
    fn of(body: &[u8], links: &[Ptr]) -> Result<Rungs> {
        let entry = Entry::decode(&mut Decoder::new(body, links, None))?;
        let prev = usize::from(entry.commit.root.is_some());
        let skip = prev + usize::from(entry.prev.is_some());
        Ok(Rungs {
            version: entry.commit.version,
            parents: prev,
            prev: entry.prev.map(|_| prev),
            skip: entry.skip.map(|_| skip),
        })
    }
}

impl CopySink<'_> {
    fn drain(&mut self) -> Result<()> {
        let start = self.end - self.buf.len() as u64;
        self.file.write_all_at(&self.buf, start)?;
        start_writeback(&self.file, start, self.buf.len() as u64);
        self.buf.clear();
        Ok(())
    }

    fn emit(&mut self, from: Ptr, staged: Staged<'_>, below: Option<Below>) -> Result<Ptr> {
        let (hash, offset) = (from.hash, self.end);
        let before = self.buf.len();
        append_staged(&mut self.buf, offset, staged);
        self.end += (self.buf.len() - before) as u64;
        match below {
            Some(below) => {
                self.copied.insert(hash, Copied::Entry);
                self.records.insert(from.offset, offset);
                self.below.insert(offset, below);
                self.needs.hand(from.offset, &mut self.kept);
            }
            None => {
                self.copied.insert(hash, Copied::Object(offset));
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

    fn remap(&self, ptr: Ptr) -> Option<Ptr> {
        let offset = match self.copied.get(&ptr.hash)? {
            Copied::Object(offset) => *offset,
            Copied::Entry => *self.records.get(&ptr.offset)?,
        };
        Some(Ptr {
            hash: ptr.hash,
            offset,
        })
    }

    fn settled(&self, link: Ptr) -> Option<Ptr> {
        let tombstone = Ptr {
            hash: link.hash,
            offset: 0,
        };
        self.remap(link)
            .or_else(|| self.pruned.contains(&link.hash).then_some(tombstone))
    }

    fn below(from: Ptr, links: &[Ptr], rungs: Rungs) -> Below {
        let offset = |index: Option<usize>| index.map_or(0, |index| links[index].offset);
        Below {
            from: from.offset,
            prev: offset(rungs.prev),
            skip: offset(rungs.skip),
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

    fn skipped(&self, links: &[Ptr], rungs: Rungs, link: Ptr) -> Option<Ptr> {
        let target = skip_version(rungs.version);
        let prev = rungs
            .prev
            .map(|index| links[index])
            .filter(|prev| prev.offset != 0);
        let found = prev.and_then(|prev| self.on_path(prev.offset, rungs.version - 1, target));
        match found.map(|offset| (offset, self.below.get(&offset))) {
            None => Some(Ptr {
                hash: link.hash,
                offset: 0,
            }),
            Some((offset, Some(below))) if below.from == link.offset => Some(Ptr {
                hash: link.hash,
                offset,
            }),
            Some(_) => self.settled(link),
        }
    }

    fn read(&mut self, ptr: Ptr) -> Result<Step> {
        if self.stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
            return Err(Error::Interrupted);
        }
        self.verifier.check()?;
        let mut bytes = mem::take(&mut self.bytes);
        let step = self.step(ptr, &mut bytes);
        if bytes.capacity() <= SPARE_BYTES {
            self.bytes = bytes;
        }
        step.map_err(|error| self.verifier.first(error))
    }

    fn step(&mut self, ptr: Ptr, bytes: &mut Vec<u8>) -> Result<Step> {
        let mut held = self.spare.pop().unwrap_or_default();
        let verifier = &mut self.verifier;
        let large = |claim: usize| match claim > BATCH_BYTES {
            true => verifier.settle(),
            false => Ok(()),
        };
        let (record, stored) = self.source.read_stored(ptr, bytes, &mut held.old, large)?;
        let entry = if self.entries.contains(&ptr.hash) {
            self.verifier.inline(record, ptr)?;
            let body = body_of(record, &stored, ptr.offset, &mut self.body)?;
            Some(Rungs::of(body, &held.old)?)
        } else if record.len() > BATCH_BYTES {
            self.verifier.inline(record, ptr)?;
            None
        } else {
            self.verifier.add(ptr, record)?;
            None
        };
        let payload = &record[stored.payload.clone()];
        if entry.is_none() {
            while let Some(link) = held.old.get(held.links.len()) {
                let Some(done) = self.settled(*link) else {
                    break;
                };
                held.links.push(done);
            }
            if held.links.len() == held.old.len() {
                let staged = Staged {
                    raw: stored.raw,
                    payload,
                    packed: stored.packed,
                    links: &held.links,
                };
                let done = self.emit(ptr, staged, None)?;
                self.recycle(held);
                return Ok(Step::Done(done));
            }
        }
        held.payload.clear();
        held.payload.extend_from_slice(payload);
        Ok(Step::Descend(Pending {
            from: ptr,
            raw: stored.raw,
            packed: stored.packed,
            next: held.links.len(),
            held,
            entry,
        }))
    }

    fn copy(&mut self, root: Ptr) -> Result<Ptr> {
        if let Some(done) = self.settled(root) {
            return Ok(done);
        }
        let mut stack = match self.read(root)? {
            Step::Done(done) => return Ok(done),
            Step::Descend(pending) => vec![pending],
        };
        loop {
            let top = stack.last_mut().expect("stack is not empty");
            if top.next < top.held.old.len() {
                let link = top.held.old[top.next];
                let settled = match top.entry {
                    Some(rungs) if link.offset == 0 && top.next >= rungs.parents => Some(link),
                    Some(rungs) if rungs.skip == Some(top.next) => {
                        self.skipped(&top.held.links, rungs, link)
                    }
                    _ => self.settled(link),
                };
                let done = match settled {
                    Some(done) => done,
                    None => match self.read(link)? {
                        Step::Done(done) => done,
                        Step::Descend(pending) => {
                            stack.push(pending);
                            continue;
                        }
                    },
                };
                let top = stack.last_mut().expect("stack is not empty");
                top.held.links.push(done);
                top.next += 1;
                continue;
            }
            let done = stack.pop().expect("stack is not empty");
            let below = done
                .entry
                .map(|rungs| Self::below(done.from, &done.held.links, rungs));
            let staged = Staged {
                raw: done.raw,
                payload: &done.held.payload,
                packed: done.packed,
                links: &done.held.links,
            };
            let ptr = self.emit(done.from, staged, below)?;
            self.recycle(done.held);
            match stack.last_mut() {
                Some(parent) => {
                    parent.held.links.push(ptr);
                    parent.next += 1;
                }
                None => return Ok(ptr),
            }
        }
    }

    fn recycle(&mut self, mut held: Buffers) {
        if self.spare.len() < SPARE && held.payload.capacity() <= SPARE_BYTES {
            held.links.clear();
            self.spare.push(held);
        }
    }
}

impl Sink for CopySink<'_> {
    fn put(&mut self, object: Object, hash: Hash) -> Result<Ptr> {
        if let Some(done) = self.remap(Ptr { hash, offset: 0 }) {
            return Ok(done);
        }
        let mut links = Vec::with_capacity(object.links.len());
        for link in &object.links {
            links.push(self.copy(*link)?);
        }
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
        let file = Traced::new(file, options.trace.clone())?;
        file.created();
        file.set_len(LOG_START)?;
        Ok(Copier {
            sink: CopySink {
                source: store.clone(),
                file,
                buf: Vec::new(),
                end: LOG_START,
                verifier: Verifier::new(copy_threads(options)),
                bytes: Vec::new(),
                body: Vec::new(),
                spare: Vec::new(),
                copied: HashMap::default(),
                entries: HashSet::default(),
                records: HashMap::new(),
                kept: Needs::default(),
                needs: Needs::default(),
                below: HashMap::new(),
                pruned: HashSet::default(),
                objects: 0,
                stop,
            },
        })
    }

    fn encode(&mut self, refs: &Refs) -> Result<Ptr> {
        let sink = &mut self.sink;
        let source = sink.source.clone();
        let mut retained = HashSet::new();
        let mut edge = Vec::new();
        let mut again = Vec::new();
        let mut closures = Closures::exact();
        for (_, branch) in refs.try_entries()? {
            let (first, after) = (branch.log.first, branch.log.after(&source)?);
            branch.log.visit(&source, |ptr, entry, _, _| {
                if !sink.wanted(ptr, Need::new(first, after), &mut again) {
                    return Ok(false);
                }
                retained.insert(ptr.hash);
                edge.extend(links_of(entry));
                closures.mark(ptr, first, after);
                closures.seed(entry, after);
                Ok(true)
            })?;
        }
        loop {
            for (ptr, need) in again.drain(..) {
                closures.push(ptr, need.floor, need.after());
            }
            closures.walk_bounded(&source, |ptr, entry, _, _, (floor, after)| {
                if !sink.wanted(ptr, Need::new(floor, after), &mut again) {
                    return Ok(false);
                }
                retained.insert(ptr.hash);
                edge.extend(links_of(entry));
                Ok(true)
            })?;
            if again.is_empty() {
                break;
            }
        }
        for hash in &retained {
            sink.pruned.remove(hash);
            sink.entries.insert(*hash);
        }
        for hash in edge {
            sink.entries.insert(hash);
            if !retained.contains(&hash) && !sink.copied.contains_key(&hash) {
                sink.pruned.insert(hash);
            }
        }
        let mut encoder = Encoder::new(&mut *sink);
        refs.encode(&mut encoder)?;
        let head = encoder.finish();
        sink.needs.clear();
        head
    }

    /// Copies everything `refs` retains that earlier passes did not, checking every object's
    /// hash, and returns the bytes written.
    pub(crate) fn pass(&mut self, refs: &Refs) -> Result<u64> {
        let before = self.sink.end;
        self.encode(refs)?;
        self.sink.verifier.settle()?;
        self.sink.drain()?;
        Ok(self.sink.end - before)
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

    /// Copies what is left for `refs`, writes the branch table, and seals the copy with a
    /// superblock at sequence `seq` that records `survival`, the share of new bytes that survived.
    ///
    /// It returns the branch table's pointer in the copy and the number of objects copied in all.
    pub(crate) fn finish(&mut self, refs: &Refs, seq: u64, survival: f64) -> Result<(Ptr, u64)> {
        let head = self.encode(refs)?;
        self.sink.verifier.settle()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::append_record;

    fn records(count: u64, bad: &[u64]) -> Vec<(Ptr, Vec<u8>)> {
        (0..count)
            .map(|index| {
                let body = format!("record {index} ").repeat(1 + index as usize % 40);
                let object = Object {
                    body: body.into_bytes(),
                    links: Vec::new(),
                };
                let mut hash = object.hash();
                if bad.contains(&index) {
                    hash[0] ^= 1;
                }
                let offset = LOG_START + index * 4096;
                let mut record = Vec::new();
                append_record(&mut record, offset, &object.body, &object.links);
                (Ptr { hash, offset }, record)
            })
            .collect()
    }

    fn verified(threads: usize, records: &[(Ptr, Vec<u8>)]) -> Result<()> {
        let mut verifier = Verifier::new(threads);
        for (ptr, record) in records {
            verifier.check()?;
            verifier.add(*ptr, record)?;
        }
        verifier.settle()
    }

    #[test]
    fn every_thread_count_verifies_every_record_and_reports_the_damaged_one() {
        let good = records(30_000, &[]);
        for threads in [1, 2, 4] {
            verified(threads, &good).unwrap();
            for bad in [0, 4095, 4096, 17_000, 29_999] {
                let damaged = records(30_000, &[bad]);
                let at = format!(
                    "hash mismatch for object at {}",
                    damaged[bad as usize].0.offset
                );
                match verified(threads, &damaged) {
                    Err(Error::Corrupt(message)) => assert_eq!(message, at, "{threads}"),
                    other => panic!("{threads} threads, record {bad}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn the_first_damaged_record_in_walk_order_is_reported_at_every_thread_count() {
        for threads in [1, 2, 4] {
            for bad in [[4095, 4096], [1000, 29_999], [17_000, 17_001], [2000, 9000]] {
                let damaged = records(30_000, &bad);
                let at = format!(
                    "hash mismatch for object at {}",
                    damaged[bad[0] as usize].0.offset
                );
                for _ in 0..20 {
                    match verified(threads, &damaged) {
                        Err(Error::Corrupt(message)) => assert_eq!(message, at, "{threads}"),
                        other => panic!("{threads} threads, records {bad:?}: {other:?}"),
                    }
                }
            }
        }
    }

    #[test]
    fn a_verifier_dropped_with_batches_queued_joins_its_threads() {
        let good = records(40_000, &[]);
        let mut verifier = Verifier::new(4);
        for (ptr, record) in &good {
            verifier.add(*ptr, record).unwrap();
        }
        let board = verifier.pool.as_ref().unwrap().board.clone();
        drop(verifier);
        assert_eq!(Arc::strong_count(&board), 1);
    }
}
