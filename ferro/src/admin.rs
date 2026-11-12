//! Working on a database file by its path, without its types.
//!
//! These functions serve tools such as the
//! [`ferro` tool](https://github.com/archlemon/ferro/blob/main/docs/guide/cli.md) and scripts that
//! inspect a file another program owns. All but [`compact`] open the file read-only, so they run
//! beside a writer. [`Keep`], which says how much history to retain, is here too.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::BuildHasherDefault;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::codec::{
    Checked, Data, Decoder, Encoder, HASH_BYTES, Hash, HashBatch, HashSink, Kind, PairRoots,
    Pairing, Plans, Ptr, Tables, pair_roots,
};
use crate::db::{Commit, Refs};
use crate::error::{Error, Result};
use crate::log::{Closures, Entry};
use crate::map::Map;
pub use crate::reclaim::{Compacted, Reclaim, Space};
use crate::reclaim::{Copier, Roots, rename, sibling, sync_parent};
use crate::refs::{self, Root};
use crate::store::{Closing, Prehashed, Store, StoreOptions};

const CONFIRMED_KEPT: usize = 1 << 12;
/// The most threads [`rehashed`] walks on when [`StoreOptions::copy_threads`] is 0.
const REHASH_THREADS: usize = 8;
/// How long a log [`rehashed`] walks on one thread when [`StoreOptions::copy_threads`] is 0.
const REHASH_ALONE: u64 = 1 << 20;
/// How many sets the objects a [`Rehash`] has reached are kept in.
const REHASH_SHARDS: usize = 1024;
/// The most objects a thread of a [`Rehash`] takes from the shared stack at once.
const REHASH_TAKE: usize = 32;
/// How many of the objects it reached last a thread of a [`Rehash`] remembers, by their hashes.
const REHASH_RECENT: usize = 4096;

type ByHash = BuildHasherDefault<Prehashed>;

/// How much history each branch retains.
///
/// [`Options::history`](crate::Options::history) sets it for a database, and
/// [`Db::compact`](crate::Db::compact) and [`compact`] apply one for a single cycle. Every form
/// keeps at least each branch's newest commit. A pruned version is gone, whether or not a cycle has
/// reclaimed its bytes yet, and version 0 goes when version 1 does. See
/// [Retention and reclamation](https://github.com/archlemon/ferro/blob/main/docs/guide/retention.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Keep {
    /// Every version, the default.
    #[default]
    All,
    /// The given number of newest commits of each branch, with 0 counting as 1.
    ///
    /// As [`Options::history`](crate::Options::history) it prunes a branch as part of each commit
    /// on it, and as part of each pull or receive that brings a branch up to the branch it
    /// landed and records it, which it prunes too. A landed branch that a pull did not record,
    /// as when its merge failed or every commit it brought already held this branch's value,
    /// keeps what landed until a later pull records it or a cycle runs.
    Last(usize),
    /// The commits made at or after this time, in milliseconds since the Unix epoch.
    ///
    /// As [`Options::history`](crate::Options::history) it takes effect at each reclamation cycle.
    Since(u64),
    /// The commits no older than this, measured from the clock when a reclamation cycle runs.
    ///
    /// As [`Options::history`](crate::Options::history) it takes effect at each cycle.
    Within(Duration),
}

/// One branch as [`info`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchInfo {
    /// The branch's name.
    pub name: String,
    /// The version of its newest commit.
    pub version: u64,
    /// The number of commits it retains.
    pub commits: usize,
    /// The time of its newest commit, in milliseconds since the Unix epoch, or 0 if it has none.
    pub last_commit_ms: u64,
}

/// What [`info`] reports of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    /// The length of the file in bytes, zeroes a writer preallocated past its log included.
    pub file_bytes: u64,
    /// Every branch in the file, sorted by name. `main` is listed once it has a commit.
    pub branches: Vec<BranchInfo>,
}

fn read_only(path: &Path) -> Result<(Closing, Option<Ptr>)> {
    let (store, head) = Store::open(
        path,
        StoreOptions {
            read_only: true,
            ..StoreOptions::default()
        },
    )?;
    Ok((Closing(store), head))
}

fn load_refs(store: &Store, head: Option<Ptr>) -> Result<Refs> {
    Ok(refs::load(store, head, None)?.refs)
}

/// The length of the file at `path`, and each branch with its version, commit count and last
/// commit time.
///
/// # Errors
///
/// [`Error::NoDatabase`] if there is no file, [`Error::NotFerro`] if it is not a database, and
/// read errors such as [`Error::Corrupt`].
///
/// # Examples
///
/// ```
/// use ferro::{Db, admin};
///
/// # fn main() -> ferro::Result<()> {
/// # let dir = tempfile::tempdir()?;
/// # let path = dir.path().join("count.ferro");
/// let db = Db::<u64>::open(&path)?;
/// db.update(|count| *count = 1)?;
/// db.fork("draft")?;
///
/// let info = admin::info(&path)?;
/// let names: Vec<&str> = info.branches.iter().map(|branch| branch.name.as_str()).collect();
/// assert_eq!(names, ["draft", "main"]);
/// assert_eq!(admin::log(&path, "main", 10)?.len(), 1);
/// assert!(admin::verify(&path)?.objects > 0);
/// # Ok(())
/// # }
/// ```
pub fn info(path: impl AsRef<Path>) -> Result<Info> {
    let (store, head) = read_only(path.as_ref())?;
    let file_bytes = store.file_len()?;
    let refs = load_refs(&store, head)?;
    let mut branches = Vec::new();
    for (name, branch) in refs.try_entries()? {
        let last = branch
            .log
            .tip_entry(&store)?
            .map(|entry| entry.commit.time_ms);
        branches.push(BranchInfo {
            name,
            version: branch.version,
            commits: branch.log.len(),
            last_commit_ms: last.unwrap_or(0),
        });
    }
    store.close();
    Ok(Info {
        file_bytes,
        branches,
    })
}

/// Up to `limit` of the newest retained commits of `branch` in the file at `path`, newest first.
///
/// # Errors
///
/// [`Error::NoBranch`] if the file has no branch of that name, which includes a `main` with no
/// commit yet, and the errors of [`info`].
pub fn log(path: impl AsRef<Path>, branch: &str, limit: usize) -> Result<Vec<Commit>> {
    let (store, head) = read_only(path.as_ref())?;
    let refs = load_refs(&store, head)?;
    let found = refs
        .try_get(branch)?
        .ok_or_else(|| Error::NoBranch(branch.to_string()))?;
    let mut walk = found.log.walk(&store);
    let mut commits = Vec::new();
    while commits.len() < limit {
        let Some(entry) = walk.step()? else {
            break;
        };
        commits.push((*entry.commit).clone());
    }
    store.close();
    Ok(commits)
}

/// Re-hashes every object reachable from every retained version of every branch in the file at
/// `path`, and checks the branch table, as [`Db::verify`](crate::Db::verify) does, on one thread
/// per core up to 8 once the log holds 1 MiB.
///
/// # Errors
///
/// [`Error::Corrupt`] for the first object whose bytes do not match its hash, for a branch table
/// that does not decode or whose map does not hold together, and for a branch that records a
/// merge of a branch the table does not hold, and the errors of [`info`].
pub fn verify(path: impl AsRef<Path>) -> Result<crate::Verified> {
    let (store, head) = read_only(path.as_ref())?;
    let report = verified(&store, head);
    store.close();
    report
}

/// Re-hashes every object reachable from the branch table at `head`, counting each once, then
/// checks the branch table and its steps as the types that read them, as [`typed`] checks the
/// values of a branch, and that each branch's merge records name a branch the table holds.
///
/// The objects are re-hashed by [`rehashed`], on several threads for a large file, and only if
/// that fails again by [`walked`], one at a time in a fixed order, so that the error is the first
/// in that order whatever the threads did.
pub(crate) fn verified(store: &Store, head: Option<Ptr>) -> Result<crate::Verified> {
    let mut report = crate::Verified {
        file_bytes: store.file_len()?,
        ..crate::Verified::default()
    };
    if let Some(ptr) = head {
        let roots = Roots::of(store, ptr)?;
        let (objects, bytes) = match rehashed(store, &roots) {
            Ok(found) => found,
            Err(_) => walked(store, &roots)?,
        };
        report.objects = objects;
        report.bytes = bytes;
        checked(
            store,
            vec![(ptr, Kind::value::<Root>(), false)],
            &mut Tables::default(),
        )?;
        named_merges(&load_refs(store, head)?)?;
    }
    Ok(report)
}

/// Re-hashes every object reachable from `roots` one at a time, in the order [`Roots::walk`]
/// reaches them, and returns how many there are and the bytes they take.
///
/// # Errors
///
/// The first read error in that order, such as [`Error::Corrupt`] for an object whose bytes do
/// not match its hash.
fn walked(store: &Store, roots: &Roots) -> Result<(u64, u64)> {
    let mut seen = HashSet::new();
    let (mut objects, mut bytes) = (0, 0);
    roots.walk(
        store,
        |ptr, _| seen.insert(ptr),
        |_, _, len| {
            objects += 1;
            bytes += u64::from(len);
        },
    )?;
    Ok((objects, bytes))
}

/// Re-hashes the same objects as [`walked`] and returns the same counts, faster: the log entries
/// are walked as [`walked`] walks them, and the trees they reach on as many threads as
/// [`rehash_threads`] gives, each reading through [`Store::check_record`], which allocates nothing
/// for an object and decompresses a body straight into the bytes it hashes, and checking the
/// hashes of small objects in a [`HashBatch`], many at once.
///
/// An object's links are followed before its hash is checked, so after damage it can read
/// records that [`walked`] would not reach, but it fails wherever [`walked`] does.
///
/// # Errors
///
/// A read error if any object fails to read or check, though not always the first that
/// [`walked`] reports.
fn rehashed(store: &Store, roots: &Roots) -> Result<(u64, u64)> {
    let mut seen: HashSet<Ptr, ByHash> = HashSet::default();
    let mut trees = Vec::new();
    let (mut objects, mut bytes) = (0, 0);
    roots.walk(
        store,
        |ptr, entry| match entry {
            true => seen.insert(ptr),
            false => {
                trees.push(ptr);
                false
            }
        },
        |_, _, len| {
            objects += 1;
            bytes += u64::from(len);
        },
    )?;
    trees.reverse();
    let threads = rehash_threads(store);
    let (more, more_bytes) = match threads {
        1 => rehashed_alone(store, seen, trees)?,
        _ => Rehash::new(store, seen, trees, threads).run()?,
    };
    Ok((objects + more, bytes + more_bytes))
}

/// How many threads [`rehashed`] walks the trees of `store` on, counting its own: the
/// [`StoreOptions::copy_threads`] it was opened with, or for 0, the number of cores up to
/// [`REHASH_THREADS`] once the log holds [`REHASH_ALONE`] bytes, and 1 below that.
fn rehash_threads(store: &Store) -> usize {
    match store.copy_threads() {
        0 if store.size() < REHASH_ALONE => 1,
        0 => thread::available_parallelism().map_or(1, |cores| cores.get().min(REHASH_THREADS)),
        threads => threads,
    }
}

/// Re-hashes every object that `trees`, a stack, reaches and `seen` does not hold, on this thread,
/// and returns how many there are and the bytes they take.
fn rehashed_alone(
    store: &Store,
    mut seen: HashSet<Ptr, ByHash>,
    mut trees: Vec<Ptr>,
) -> Result<(u64, u64)> {
    let mut links = Vec::new();
    let mut batch = HashBatch::new();
    let (mut objects, mut bytes) = (0, 0);
    while let Some(ptr) = trees.pop() {
        if !seen.insert(ptr) {
            continue;
        }
        bytes += u64::from(store.check_record(ptr, &mut links, &mut batch)?);
        objects += 1;
        trees.extend_from_slice(&links);
    }
    batch.finish()?;
    Ok((objects, bytes))
}

/// A walk over trees that several threads share, each re-hashing the objects it reaches first.
///
/// Each thread walks depth first from the objects it took, and hands the older half of what it
/// has yet to walk to the shared stack whenever that is empty and another thread waits for work,
/// once it has checked every hash its [`HashBatch`] holds, so that every object on the shared
/// stack was reached through records whose hashes matched. The walk ends once every thread waits
/// with the shared stack empty, or one fails.
struct Rehash<'a> {
    store: &'a Store,
    /// The objects reached so far, in [`REHASH_SHARDS`] sets by the last bytes of their hashes.
    seen: Vec<Shard>,
    work: Mutex<Work>,
    wake: Condvar,
    /// Whether a thread waits for work and the shared stack is empty, as last seen under the lock.
    hungry: AtomicBool,
    failed: AtomicBool,
}

/// One set of [`Rehash::seen`], on cache lines of its own.
#[repr(align(128))]
struct Shard(Mutex<HashSet<Ptr, ByHash>>);

/// The shared stack of a [`Rehash`], the threads walking and those of them waiting on it.
struct Work {
    ptrs: Vec<Ptr>,
    threads: usize,
    idle: usize,
    done: bool,
}

impl<'a> Rehash<'a> {
    /// A walk of `store` on `threads` threads from `trees`, a stack, past the objects in `seen`.
    fn new(
        store: &'a Store,
        seen: HashSet<Ptr, ByHash>,
        trees: Vec<Ptr>,
        threads: usize,
    ) -> Rehash<'a> {
        let mut shards: Vec<HashSet<Ptr, ByHash>> =
            (0..REHASH_SHARDS).map(|_| HashSet::default()).collect();
        for ptr in seen {
            shards[shard_of(&ptr)].insert(ptr);
        }
        Rehash {
            store,
            seen: shards
                .into_iter()
                .map(|shard| Shard(Mutex::new(shard)))
                .collect(),
            work: Mutex::new(Work {
                ptrs: trees,
                threads,
                idle: 0,
                done: false,
            }),
            wake: Condvar::new(),
            hungry: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        }
    }

    /// Runs the walk on this thread and the others, and returns how many objects they re-hashed
    /// and the bytes those take. A thread the system will not start is left out of the walk.
    ///
    /// # Errors
    ///
    /// The error of a thread that failed.
    fn run(&self) -> Result<(u64, u64)> {
        let threads = self.work.lock().threads;
        let parts = thread::scope(|scope| {
            let helpers: Vec<_> = (1..threads)
                .filter_map(|_| {
                    let spawned = thread::Builder::new()
                        .name("ferro-verify".to_string())
                        .spawn_scoped(scope, || self.part());
                    if spawned.is_err() {
                        self.work.lock().threads -= 1;
                        self.wake.notify_all();
                    }
                    spawned.ok()
                })
                .collect();
            let mut parts = vec![self.part()];
            for helper in helpers {
                match helper.join() {
                    Ok(part) => parts.push(part),
                    Err(panic) => resume_unwind(panic),
                }
            }
            parts
        });
        parts
            .into_iter()
            .try_fold((0, 0), |(objects, bytes), part| {
                let (more, more_bytes) = part?;
                Ok((objects + more, bytes + more_bytes))
            })
    }

    /// One thread's share of the walk: how many objects it re-hashed and the bytes those take.
    ///
    /// # Errors
    ///
    /// The first error of this thread, which ends the walk for every thread, as a panic does.
    fn part(&self) -> Result<(u64, u64)> {
        match catch_unwind(AssertUnwindSafe(|| self.walk_part())) {
            Ok(Ok(part)) => Ok(part),
            Ok(Err(error)) => {
                self.stop();
                Err(error)
            }
            Err(panic) => {
                self.stop();
                resume_unwind(panic)
            }
        }
    }

    /// The walk of [`Rehash::part`], which leaves ending the walk for the others to it.
    fn walk_part(&self) -> Result<(u64, u64)> {
        let mut stack = Vec::new();
        let mut links = Vec::new();
        let mut batch = HashBatch::new();
        let mut recent = vec![None; REHASH_RECENT];
        let (mut objects, mut bytes) = (0, 0);
        while self.take(&mut stack) {
            while let Some(ptr) = stack.pop() {
                if self.failed.load(Ordering::Relaxed) {
                    return Ok((objects, bytes));
                }
                let slot = &mut recent[recent_of(&ptr)];
                if *slot == Some(ptr) {
                    continue;
                }
                *slot = Some(ptr);
                if !self.seen[shard_of(&ptr)].0.lock().insert(ptr) {
                    continue;
                }
                bytes += u64::from(self.store.check_record(ptr, &mut links, &mut batch)?);
                objects += 1;
                stack.extend_from_slice(&links);
                if stack.len() > 1 && self.hungry.load(Ordering::Relaxed) {
                    batch.finish()?;
                    self.give(&mut stack);
                }
            }
        }
        batch.finish()?;
        Ok((objects, bytes))
    }

    /// Moves objects from the shared stack to `stack`, waiting while the stack is empty and
    /// another thread walks, or returns `false` once the walk is over.
    fn take(&self, stack: &mut Vec<Ptr>) -> bool {
        let mut work = self.work.lock();
        loop {
            if work.done {
                return false;
            }
            if !work.ptrs.is_empty() {
                let take = work.ptrs.len().div_ceil(work.threads).min(REHASH_TAKE);
                let from = work.ptrs.len() - take;
                stack.extend(work.ptrs.drain(from..));
                let hungry = work.idle > 0 && work.ptrs.is_empty();
                self.hungry.store(hungry, Ordering::Relaxed);
                return true;
            }
            if work.idle + 1 >= work.threads {
                work.done = true;
                self.wake.notify_all();
                return false;
            }
            work.idle += 1;
            self.hungry.store(true, Ordering::Relaxed);
            self.wake.wait(&mut work);
            work.idle -= 1;
        }
    }

    /// Hands the older half of `stack` to the shared stack if that is empty and a thread waits.
    fn give(&self, stack: &mut Vec<Ptr>) {
        let mut work = self.work.lock();
        if work.ptrs.is_empty() && work.idle > 0 {
            work.ptrs.extend(stack.drain(..stack.len() / 2));
            self.hungry.store(false, Ordering::Relaxed);
            self.wake.notify_all();
        }
    }

    /// Ends the walk early, after a thread failed.
    fn stop(&self) {
        self.failed.store(true, Ordering::Relaxed);
        self.work.lock().done = true;
        self.wake.notify_all();
    }
}

/// Where a thread of a [`Rehash`] keeps `ptr` among the objects it reached last, by bytes of its
/// hash that neither [`shard_of`] nor the sets use.
fn recent_of(ptr: &Ptr) -> usize {
    usize::from(u16::from_le_bytes([ptr.hash[8], ptr.hash[9]])) % REHASH_RECENT
}

/// The shard of [`Rehash::seen`] that holds `ptr`, by the last bytes of its hash, since the sets
/// place a hash by its first bytes.
fn shard_of(ptr: &Ptr) -> usize {
    let tail = [ptr.hash[HASH_BYTES - 2], ptr.hash[HASH_BYTES - 1]];
    usize::from(u16::from_le_bytes(tail)) % REHASH_SHARDS
}

/// Checks that every merge record in `refs` names a branch `refs` holds, as dropping a branch
/// removes the records that name it, so that a later branch of that name is not taken for one
/// already merged.
fn named_merges(refs: &Refs) -> Result<()> {
    for (name, head) in refs.try_entries()? {
        for (other, _) in head.merged.try_entries()? {
            if refs.try_get(&other)?.is_none() {
                return Err(Error::Corrupt(format!(
                    "branch {name} records a merge of branch {other}, which the file does not hold"
                )));
            }
        }
    }
    Ok(())
}

/// Decodes every object reachable from every retained version in `refs`, and from version 0 at
/// `genesis`, as the type that reads it, checking each map node against what its parent records.
/// The branch table is left to [`verified`].
///
/// Each object is checked without the store, and one whose summary needs it, because a value it
/// summarizes or a summary it folds holds a collection, is checked again with the store once every
/// object below it has passed, so that `of` and `combine` read only collections whose nodes were
/// checked. Once every object has passed, each table found is checked against its rows.
///
/// # Errors
///
/// [`Error::Corrupt`] naming the first object that does not decode or does not fit, or the first
/// table whose indexes do not agree with its rows.
pub(crate) fn typed<T: Data>(store: &Store, refs: &Refs, genesis: Option<Ptr>) -> Result<()> {
    let mut roots: Vec<Ptr> = genesis.into_iter().collect();
    let mut commits = HashSet::new();
    let mut versions: Vec<(u64, Hash, Option<Ptr>, Option<Hash>)> = Vec::new();
    let mut closures = Closures::default();
    for (_, branch) in refs.try_entries()? {
        let chain = branch.log;
        let after = chain.after(store)?;
        closures.chain(&chain);
        chain.visit(store, branch.root, |ptr, entry, _, _| {
            if commits.insert(ptr.hash) {
                roots.extend(entry.commit.root);
                versions.push(version_of(ptr, entry));
            }
            closures.mark(ptr, chain.first, after);
            closures.seed(entry, after);
            Ok(true)
        })?;
    }
    closures.walk(store, |ptr, entry, _, _| {
        if commits.insert(ptr.hash) {
            roots.extend(entry.commit.root);
            versions.push(version_of(ptr, entry));
        }
        Ok(true)
    })?;
    let root = Kind::value::<T>();
    let queue: Vec<(Ptr, Kind, bool)> = roots
        .into_iter()
        .map(|ptr| (ptr, root.clone(), false))
        .collect();
    let mut tables = Tables::default();
    checked(store, queue, &mut tables)?;
    if !tables.is_empty() {
        agreed(store, &root, pair_roots::<T>, versions, tables)?;
    }
    Ok(())
}

/// Checks each object in `queue` as its kind, and every object those reach through typed links,
/// adding each table found to `tables`.
///
/// # Errors
///
/// [`Error::Corrupt`] naming the first object that does not decode or does not fit.
fn checked(store: &Store, mut queue: Vec<(Ptr, Kind, bool)>, tables: &mut Tables) -> Result<()> {
    let mut checked = Checked::default();
    let mut confirmed = HashSet::new();
    let corrupt = |kind: &Kind, ptr: Ptr, error: Error| {
        Error::Corrupt(format!(
            "the {} at {} does not decode: {error}",
            kind.name, ptr.offset
        ))
    };
    while let Some((ptr, kind, settle)) = queue.pop() {
        if settle {
            let object = store.read_object(ptr)?;
            kind.recheck(&object.body, &object.links, store, None)
                .map_err(|error| corrupt(&kind, ptr, error))?;
            continue;
        }
        let shape = match checked.get(&ptr.hash, &kind) {
            Some((shape, true)) => shape,
            Some((shape, false)) => {
                let memo = kind.claim.as_deref().and_then(|claim| {
                    let claimed = (claim as &dyn Any).type_id();
                    claim
                        .fingerprint()
                        .map(|print| (ptr.hash, kind.id, claimed, print))
                });
                let seen = memo.is_some_and(|memo| confirmed.contains(&memo));
                if !seen {
                    (kind.held)(&kind, store, ptr).map_err(|error| corrupt(&kind, ptr, error))?;
                    if let Some(memo) = memo.filter(|_| confirmed.len() < CONFIRMED_KEPT) {
                        confirmed.insert(memo);
                    }
                }
                shape
            }
            None => {
                let object = store.read_object(ptr)?;
                let found_tables = RefCell::new(Vec::new());
                let (shape, found, _) = kind
                    .check_tables(&object.body, &object.links, &found_tables)
                    .map_err(|error| corrupt(&kind, ptr, error))?;
                tables.note(found_tables.into_inner());
                if shape.deferred {
                    queue.push((ptr, kind.clone(), true));
                }
                queue.extend(
                    found
                        .into_iter()
                        .map(|(index, child)| (object.links[index], child, false)),
                );
                checked.insert(ptr.hash, &kind, shape);
                shape
            }
        };
        if !kind.fits(shape) {
            return Err(Error::Corrupt(format!(
                "the {} at {} holds {} entries at level {}, not what its parent records",
                kind.name, ptr.offset, shape.count, shape.level
            )));
        }
    }
    Ok(())
}

fn version_of(ptr: Ptr, entry: &Entry) -> (u64, Hash, Option<Ptr>, Option<Hash>) {
    (
        entry.commit.version,
        ptr.hash,
        entry.commit.root,
        entry.prev.map(|prev| prev.hash),
    )
}

/// Checks that the indexes of each table in `tables` agree with their rows: each table of a
/// version against the table at the same place in its parent, as `pair_roots` pairs them, which
/// costs what changed between them, and every other table, those of the oldest versions among
/// them and those with no table at their place in the parent, by building its indexes from its
/// rows.
fn agreed(
    store: &Store,
    root: &Kind,
    pair_roots: PairRoots,
    mut versions: Vec<(u64, Hash, Option<Ptr>, Option<Hash>)>,
    tables: Tables,
) -> Result<()> {
    let roots: HashMap<Hash, Ptr> = versions
        .iter()
        .filter_map(|(_, commit, root, _)| root.map(|root| (*commit, root)))
        .collect();
    versions.sort_unstable_by_key(|(version, commit, _, _)| (*version, *commit));
    let mut plans = Plans::default();
    for (_, _, ptr, prev) in &versions {
        let (Some(ptr), Some(prev)) = (ptr, prev.and_then(|prev| roots.get(&prev))) else {
            continue;
        };
        let mut pairing = Pairing::new(store, false);
        pair_roots(&mut pairing, *prev, *ptr).map_err(|error| match error {
            Error::Decode(message) => Error::Corrupt(format!(
                "the {} at {} does not decode: {message}",
                root.name, ptr.offset
            )),
            other => other,
        })?;
        for (old, new) in pairing.into_paired().pairs {
            plans.build(&old);
            plans.from(&new, &old);
        }
    }
    let found = tables.into_found();
    match plans.check(&found, store) {
        Some((table, Error::Decode(message))) => Err(Error::Corrupt(format!(
            "a table ({}) has indexes that do not agree with its rows: {message}",
            table.name
        ))),
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}

/// Where the records that a compaction would copy lie in a file, from [`layout`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    /// The size of the file in bytes.
    pub file_bytes: u64,
    /// The offset and length in bytes of each record, sorted by offset.
    pub extents: Vec<(u64, u32)>,
}

/// The records that a compaction of the file at `path` with `keep` would copy, by where they lie.
///
/// It reaches what a cycle would retain without copying anything, for tests and benchmarks that
/// damage or measure parts of a file.
///
/// The records are the log entries of the versions each branch keeps under `keep`, those of the
/// commits that merges brought into them as far as a compaction keeps them, every object the
/// trees of those versions reach, every object the tree of version 0 reaches when the branch
/// table names one, which a compaction keeps whatever `keep` prunes, and the record each branch
/// keeps of the branches merged into it. A log entry stored at two offsets is listed at each
/// that a kept version reaches, as a compaction copies each, and any other object stored at two
/// offsets once, at the first offset reached, as a compaction copies it once.
///
/// Not listed are the branch table and the steps written on top of it, with the older heads
/// those steps record, since a compaction copies none of them: it writes a new branch table from
/// the pruned branches, so [`Compacted::objects`] counts that table's few records on top of the
/// extents. Nor is anything that only pruned versions reach. Each length is that of the record in
/// this file; a copy encodes the offsets of a record's links relative to where it lands, so it can
/// take a few bytes more or less there.
///
/// # Errors
///
/// The errors of [`info`].
pub fn layout(path: impl AsRef<Path>, keep: Keep) -> Result<Layout> {
    let (store, head) = read_only(path.as_ref())?;
    let mut extents = Vec::new();
    if head.is_some() {
        let loaded = refs::load(&store, head, None)?;
        let (pruned, _) = pruned(&store, &loaded.refs, keep)?;
        let mut roots = Roots {
            trees: loaded.genesis.into_iter().collect(),
            exact: true,
            ..Roots::default()
        };
        for (_, head) in pruned.try_entries()? {
            let mut sink = HashSink;
            let mut encoder = Encoder::new(&mut sink);
            head.encode(&mut encoder)?;
            roots.trees.extend_from_slice(encoder.links());
            roots.heads.push(head);
        }
        let (mut entries, mut objects) = (HashSet::new(), HashSet::new());
        roots.walk(
            &store,
            |ptr, entry| match entry {
                true => entries.insert(ptr.offset),
                false => objects.insert(ptr.hash),
            },
            |ptr, _, len| extents.push((ptr.offset, len)),
        )?;
    }
    extents.sort_unstable();
    let file_bytes = store.size();
    store.close();
    Ok(Layout {
        file_bytes,
        extents,
    })
}

#[derive(Clone, Default)]
struct Raw;

impl Data for Raw {
    fn encode(&self, _w: &mut Encoder<'_>) -> Result<()> {
        Ok(())
    }

    fn decode(_r: &mut Decoder<'_>) -> Result<Self> {
        Ok(Raw)
    }
}

/// The size of the file at `path` against an estimate of its live bytes, as
/// [`Db::space`](crate::Db::space) reports them.
///
/// It opens the file read-only, so the counts of cycles run and the last error are those of the
/// new handle: none.
///
/// # Errors
///
/// The errors of [`info`].
pub fn space(path: impl AsRef<Path>) -> Result<Space> {
    crate::Options::new()
        .read_only(true)
        .open::<Raw>(path)?
        .space()
}

fn pruned(store: &Store, refs: &Refs, keep: Keep) -> Result<(Refs, u64)> {
    let mut pruned: Refs = Map::new();
    let mut dropped = 0;
    for (name, branch) in refs.try_entries()? {
        let log = branch.log.pruned(store, keep)?;
        dropped = u64::saturating_add(dropped, (branch.log.len() - log.len()) as u64);
        let mut next = branch;
        next.log = log;
        pruned.insert(name, next);
    }
    Ok((pruned, dropped))
}

/// Rewrites the file at `path`, keeping what `keep` retains of every branch, with no writer
/// running.
///
/// It takes the writer lock, copies what is reachable to a new file beside this one, renames it
/// over the old one atomically and syncs the directory, so a crash leaves one file or the other.
/// It needs free disk for everything retained. No writer waits, so the report's `pause` is zero
/// and its `passes` 1. A file with no commit yet has nothing to copy: it is left as it is, and the
/// report's `passes` and `objects` are 0. The report's `before_bytes` is the file's length as the
/// call found it, zeroes that a writer preallocated past the log and opening trims included, and
/// its `after_bytes` the length of the file it leaves. A program with the file open calls
/// [`Db::compact`](crate::Db::compact) instead, which runs while its writers do.
///
/// # Errors
///
/// [`Error::Io`] with [`NotFound`](std::io::ErrorKind::NotFound) if there is no file at `path`,
/// [`Error::Locked`] while a writer has the file open, [`Error::NotFerro`] if the file is not a
/// database, which includes an empty file and one of only zeroes, as for [`info`], since a
/// compaction does not make a database of one as a writable [`Db::open`](crate::Db::open) does,
/// and [`Error::Corrupt`] or [`Error::Io`] if the file cannot be read or the copy written. A copy
/// that fails is removed, and the file is left as it was.
///
/// # Examples
///
/// ```
/// use ferro::{Db, Keep, admin};
///
/// # fn main() -> ferro::Result<()> {
/// # let dir = tempfile::tempdir()?;
/// # let path = dir.path().join("count.ferro");
/// let db = Db::<u64>::open(&path)?;
/// for count in 1..=3 {
///     db.update(|value| *value = count)?;
/// }
/// drop(db);
///
/// let report = admin::compact(&path, Keep::Last(1))?;
/// assert_eq!(report.commits_dropped, 2);
/// assert_eq!(Db::<u64>::open(&path)?.log().len(), 1);
/// # Ok(())
/// # }
/// ```
pub fn compact(path: impl AsRef<Path>, keep: Keep) -> Result<Compacted> {
    let real = fs::canonicalize(path.as_ref())?;
    let path = real.as_path();
    let before_bytes = fs::metadata(path)?.len();
    let (store, head) = Store::open_existing(path, StoreOptions::default())?;
    let Some(head) = head else {
        let after_bytes = store.file_len();
        store.close();
        return Ok(Compacted {
            before_bytes,
            after_bytes: after_bytes?,
            ..Compacted::default()
        });
    };
    let loaded = refs::load(&store, Some(head), None).inspect_err(|_| store.close())?;
    let temp = sibling(path);
    let copied = write_copy(&store, (&loaded.refs, loaded.genesis), keep, &temp);
    let result = copied.and_then(|mut report| {
        report.before_bytes = before_bytes;
        report.after_bytes = fs::metadata(&temp)?.len();
        rename(&temp, path)?;
        sync_parent(path)?;
        Ok(report)
    });
    if result.is_err() {
        Copier::abandon(&temp);
    }
    store.close();
    result
}

fn write_copy(
    store: &Store,
    (refs, genesis): (&Refs, Option<Ptr>),
    keep: Keep,
    temp: &Path,
) -> Result<Compacted> {
    let (pruned, commits_dropped) = pruned(store, refs, keep)?;
    let mut copier = Copier::create(store, temp, &StoreOptions::default(), None)?;
    copier.pass(&pruned, genesis)?;
    let after = copier.end();
    let survival = after as f64 / store.size().max(1) as f64;
    let (_, objects) = copier.finish(&pruned, genesis, store.last_seq(), survival)?;
    Ok(Compacted {
        objects,
        commits_dropped,
        passes: 1,
        handed_out: copier.handed_out(),
        ..Compacted::default()
    })
}
