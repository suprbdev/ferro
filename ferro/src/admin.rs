//! Working on a database file by its path, without its types.
//!
//! These functions serve tools such as the
//! [`ferro` tool](https://github.com/archlemon/ferro/blob/main/docs/guide/cli.md) and scripts that
//! inspect a file another program owns. All but [`compact`] open the file read-only, so they run
//! beside a writer. [`Keep`], which says how much history to retain, is here too.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::codec::{Checked, Data, Decoder, Encoder, Kind, Ptr};
use crate::db::{Commit, Refs};
use crate::error::{Error, Result};
use crate::log::Closures;
use crate::map::Map;
pub use crate::reclaim::{Compacted, Reclaim, Space};
use crate::reclaim::{Copier, Roots, rename, sibling, sync_parent};
use crate::refs::{self, Root};
use crate::store::{Closing, Store, StoreOptions};

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
    /// As [`Options::history`](crate::Options::history) it prunes as part of each commit.
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
    /// The size of the file in bytes.
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
    Ok(refs::load(store, head, None)?.0)
}

/// The size of the file at `path`, and each branch with its version, commit count and last commit
/// time.
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
        file_bytes: store.size(),
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
/// `path`, as [`Db::verify`](crate::Db::verify) does.
///
/// # Errors
///
/// [`Error::Corrupt`] for the first object whose bytes do not match its hash, and the errors of
/// [`info`].
pub fn verify(path: impl AsRef<Path>) -> Result<crate::Verified> {
    let (store, head) = read_only(path.as_ref())?;
    let report = verified(&store, head);
    store.close();
    report
}

/// Re-hashes every object reachable from the branch table at `head`, counting each once.
pub(crate) fn verified(store: &Store, head: Option<Ptr>) -> Result<crate::Verified> {
    let mut report = crate::Verified {
        file_bytes: store.size(),
        ..crate::Verified::default()
    };
    if let Some(ptr) = head {
        let mut seen = HashSet::new();
        let roots = Roots::of(store, ptr)?;
        roots.walk(
            store,
            |ptr| seen.insert(ptr),
            |_, _, len| {
                report.objects += 1;
                report.bytes += u64::from(len);
            },
        )?;
    }
    Ok(report)
}

/// Decodes every object reachable from every retained version in `refs`, and the branch table at
/// `head`, as the type that reads it, checking each map node against what its parent records.
///
/// # Errors
///
/// [`Error::Corrupt`] naming the first object that does not decode or does not fit.
pub(crate) fn typed<T: Data>(store: &Store, head: Option<Ptr>, refs: &Refs) -> Result<()> {
    let mut roots: Vec<Ptr> = Vec::new();
    let mut commits = HashSet::new();
    let mut closures = Closures::default();
    for (_, branch) in refs.try_entries()? {
        let chain = branch.log;
        let after = chain.after(store)?;
        chain.visit(store, |ptr, entry, _, _| {
            if commits.insert(ptr.hash) {
                roots.extend(entry.commit.root);
            }
            closures.mark(ptr, chain.first, after);
            closures.seed(entry, after);
            Ok(true)
        })?;
    }
    closures.walk(store, |ptr, entry, _, _| {
        if commits.insert(ptr.hash) {
            roots.extend(entry.commit.root);
        }
        Ok(true)
    })?;
    let root = Kind::value::<T>();
    let mut queue: Vec<(Ptr, Kind)> = roots.into_iter().map(|ptr| (ptr, root.clone())).collect();
    queue.extend(head.map(|ptr| (ptr, Kind::value::<Root>())));
    let mut checked = Checked::default();
    while let Some((ptr, kind)) = queue.pop() {
        let shape = match checked.get(&ptr.hash, &kind) {
            Some(shape) => shape,
            None => {
                let object = store.read_object(ptr)?;
                let (shape, found) = kind.check(&object.body, &object.links).map_err(|error| {
                    Error::Corrupt(format!(
                        "the {} at {} does not decode: {error}",
                        kind.name, ptr.offset
                    ))
                })?;
                queue.extend(
                    found
                        .into_iter()
                        .map(|(index, child)| (object.links[index], child)),
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

/// Where the records that a compaction would keep lie in a file, from [`layout`].
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
/// # Errors
///
/// The errors of [`info`].
pub fn layout(path: impl AsRef<Path>, keep: Keep) -> Result<Layout> {
    let (store, head) = read_only(path.as_ref())?;
    let mut extents = Vec::new();
    if let Some(ptr) = head {
        let mut roots = Roots::of(&store, ptr)?;
        for chain in roots.chains.iter_mut().chain(&mut roots.superseded) {
            *chain = chain.pruned(&store, keep)?;
        }
        let mut seen = HashSet::new();
        roots.walk(
            &store,
            |ptr| seen.insert(ptr.offset),
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
/// and its `passes` 1. A program with the file open calls [`Db::compact`](crate::Db::compact)
/// instead, which runs while its writers do.
///
/// # Errors
///
/// [`Error::Io`] with [`NotFound`](std::io::ErrorKind::NotFound) if there is no file at `path`,
/// [`Error::Locked`] while a writer has the file open, and [`Error::NotFerro`], [`Error::Corrupt`]
/// or [`Error::Io`] if the file cannot be read or the copy written. A copy that fails is removed,
/// and the file is left as it was.
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
    let (store, head) = Store::open(path, StoreOptions::default())?;
    let before_bytes = store.size();
    let Some(head) = head else {
        store.close();
        return Ok(Compacted {
            before_bytes,
            after_bytes: before_bytes,
            ..Compacted::default()
        });
    };
    let refs = load_refs(&store, Some(head)).inspect_err(|_| store.close())?;
    let temp = sibling(path);
    let result = write_copy(&store, &refs, keep, &temp, before_bytes).and_then(|mut report| {
        rename(&temp, path)?;
        sync_parent(path)?;
        report.before_bytes = before_bytes;
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
    refs: &Refs,
    keep: Keep,
    temp: &Path,
    before: u64,
) -> Result<Compacted> {
    let (pruned, commits_dropped) = pruned(store, refs, keep)?;
    let mut copier = Copier::create(store, temp, &StoreOptions::default(), None)?;
    copier.pass(&pruned)?;
    let after = copier.end();
    let (_, objects) = copier.finish(&pruned, 1, after as f64 / before.max(1) as f64)?;
    Ok(Compacted {
        after_bytes: copier.end(),
        objects,
        commits_dropped,
        passes: 1,
        ..Compacted::default()
    })
}
