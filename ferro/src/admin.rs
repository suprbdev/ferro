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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Keep {
    #[default]
    All,
    Last(usize),
    Since(u64),
    Within(Duration),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub version: u64,
    pub commits: usize,
    pub last_commit_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub file_bytes: u64,
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

pub fn verify(path: impl AsRef<Path>) -> Result<crate::Verified> {
    let (store, head) = read_only(path.as_ref())?;
    let report = verified(&store, head);
    store.close();
    report
}

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub file_bytes: u64,
    pub extents: Vec<(u64, u32)>,
}

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

pub fn compact(path: impl AsRef<Path>, keep: Keep) -> Result<Compacted> {
    let path = path.as_ref();
    let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
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
