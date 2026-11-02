use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::codec::{Data, Encoder, Hash, Object, Ptr, Sink};
use crate::db::{Commit, Refs};
use crate::error::{Error, Result};
use crate::map::Map;
pub use crate::reclaim::{Compacted, Reclaim, Space};
use crate::reclaim::Roots;
use crate::store::{LOG_START, Store, StoreOptions, append_record, seal};

const FLUSH_BYTES: usize = 1 << 20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Keep {
    #[default]
    All,
    Last(usize),
    Since(u64),
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

fn read_only(path: &Path) -> Result<(Store, Option<Ptr>)> {
    Store::open(
        path,
        StoreOptions {
            read_only: true,
            ..StoreOptions::default()
        },
    )
}

fn load_refs(store: &Store, head: Option<Ptr>) -> Result<Refs> {
    match head {
        Some(ptr) => Ok((*store.load::<Refs>(ptr)?).clone()),
        None => Ok(Map::new()),
    }
}

pub fn info(path: impl AsRef<Path>) -> Result<Info> {
    let (store, head) = read_only(path.as_ref())?;
    let refs = load_refs(&store, head)?;
    let mut branches = Vec::new();
    for (name, branch) in refs.iter() {
        let last = branch.log.tip_entry(&store)?.map(|entry| entry.commit.time_ms);
        branches.push(BranchInfo {
            name,
            version: branch.version,
            commits: branch.log.len(),
            last_commit_ms: last.unwrap_or(0),
        });
    }
    let file_bytes = store.on_disk();
    store.close();
    Ok(Info {
        file_bytes,
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
    let report = verified(&store, head)?;
    store.close();
    Ok(report)
}

pub(crate) fn verified(store: &Store, head: Option<Ptr>) -> Result<crate::Verified> {
    let mut report = crate::Verified {
        file_bytes: store.on_disk(),
        ..crate::Verified::default()
    };
    if let Some(ptr) = head {
        let mut seen = HashSet::new();
        let roots = Roots::of(ptr, &load_refs(store, head)?)?;
        roots.walk(store, |ptr| seen.insert(ptr), |_, _, len| {
            report.objects += 1;
            report.bytes += u64::from(len);
        })?;
    }
    Ok(report)
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
        let refs = load_refs(&store, head)?;
        let mut roots = Roots::of(ptr, &refs)?;
        for chain in &mut roots.chains {
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
    Ok(Layout { file_bytes, extents })
}

#[derive(Clone, Default)]
struct Raw;

impl Data for Raw {
    fn encode(&self, _w: &mut Encoder<'_>) -> Result<()> {
        Ok(())
    }

    fn decode(_r: &mut crate::codec::Decoder<'_>) -> Result<Self> {
        Ok(Raw)
    }
}

pub fn reclaim(path: impl AsRef<Path>, keep: Keep) -> Result<Compacted> {
    let db: crate::Db<Raw> = crate::Options::new().reclaim(Reclaim {
        every_bytes: 0,
        ..Reclaim::default()
    })
    .open(path)?;
    db.compact(keep)
}

pub fn space(path: impl AsRef<Path>) -> Result<Space> {
    crate::Options::new().read_only().open::<Raw>(path)?.space()
}

struct CopySink {
    source: Store,
    file: File,
    buf: Vec<u8>,
    end: u64,
    copied: HashMap<Hash, u64>,
    objects: u64,
}

struct Frame {
    hash: Hash,
    object: Object,
    next: usize,
    links: Vec<Ptr>,
}

impl CopySink {
    fn drain(&mut self) -> Result<()> {
        let start = self.end - self.buf.len() as u64;
        self.file.write_all_at(&self.buf, start)?;
        self.buf.clear();
        Ok(())
    }

    fn emit(&mut self, hash: Hash, object: &Object, links: &[Ptr]) -> Result<Ptr> {
        let offset = self.end;
        let before = self.buf.len();
        append_record(&mut self.buf, offset, &object.body, links);
        self.end += (self.buf.len() - before) as u64;
        self.copied.insert(hash, offset);
        self.objects += 1;
        if self.buf.len() >= FLUSH_BYTES {
            self.drain()?;
        }
        Ok(Ptr { hash, offset })
    }

    fn remap(&self, ptr: Ptr) -> Option<Ptr> {
        self.copied.get(&ptr.hash).map(|offset| Ptr {
            hash: ptr.hash,
            offset: *offset,
        })
    }

    fn copy(&mut self, root: Ptr) -> Result<Ptr> {
        if let Some(done) = self.remap(root) {
            return Ok(done);
        }
        let mut stack = vec![Frame {
            hash: root.hash,
            object: self.source.read_object(root)?,
            next: 0,
            links: Vec::new(),
        }];
        loop {
            let top = stack.last_mut().expect("stack is not empty");
            if top.next < top.object.links.len() {
                let link = top.object.links[top.next];
                if let Some(done) = self.remap(link) {
                    let top = stack.last_mut().expect("stack is not empty");
                    top.links.push(done);
                    top.next += 1;
                } else {
                    let object = self.source.read_object(link)?;
                    stack.push(Frame {
                        hash: link.hash,
                        object,
                        next: 0,
                        links: Vec::new(),
                    });
                }
                continue;
            }
            let frame = stack.pop().expect("stack is not empty");
            let ptr = self.emit(frame.hash, &frame.object, &frame.links)?;
            match stack.last_mut() {
                Some(parent) => {
                    parent.links.push(ptr);
                    parent.next += 1;
                }
                None => return Ok(ptr),
            }
        }
    }
}

impl Sink for CopySink {
    fn put(&mut self, object: Object, hash: Hash) -> Result<Ptr> {
        if let Some(done) = self.remap(Ptr { hash, offset: 0 }) {
            return Ok(done);
        }
        let mut links = Vec::with_capacity(object.links.len());
        for link in &object.links {
            links.push(self.copy(*link)?);
        }
        self.emit(hash, &object, &links)
    }

    fn store_id(&self) -> u64 {
        self.source.id()
    }
}

fn sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".compacting");
    PathBuf::from(name)
}

pub fn compact(path: impl AsRef<Path>, keep: Keep) -> Result<Compacted> {
    let path = path.as_ref();
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
    let refs = load_refs(&store, Some(head))?;
    let temp = sibling(path);
    let outcome = write_copy(&store, &refs, keep, &temp);
    let result = outcome.and_then(|(objects, after_bytes, commits_dropped)| {
        fs::rename(&temp, path)?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            File::open(parent)?.sync_all()?;
        }
        Ok(Compacted {
            before_bytes,
            after_bytes,
            objects,
            commits_dropped,
            ..Compacted::default()
        })
    });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    store.close();
    result
}

fn write_copy(store: &Store, refs: &Refs, keep: Keep, temp: &Path) -> Result<(u64, u64, u64)> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(temp)?;
    file.set_len(LOG_START)?;
    let mut sink = CopySink {
        source: store.clone(),
        file,
        buf: Vec::new(),
        end: LOG_START,
        copied: HashMap::new(),
        objects: 0,
    };
    let mut pruned: Refs = Map::new();
    let mut commits_dropped = 0;
    let mut retained = HashSet::new();
    let mut edge = Vec::new();
    for (name, branch) in refs.iter() {
        let log = branch.log.pruned(store, keep)?;
        commits_dropped += (branch.log.len() - log.len()) as u64;
        log.visit(store, false, |ptr, entry, _, _| {
            retained.insert(ptr.hash);
            edge.extend(entry.prev.iter().chain(&entry.skip).map(|link| link.hash));
            Ok(())
        })?;
        let mut next = (*branch).clone();
        next.log = log;
        pruned.insert(name, next);
    }
    let tombstones: Vec<Hash> = edge.into_iter().filter(|hash| !retained.contains(hash)).collect();
    sink.copied.extend(tombstones.iter().map(|hash| (*hash, 0)));
    let mut encoder = Encoder::new(&mut sink);
    pruned.encode(&mut encoder)?;
    let head = encoder.finish()?;
    sink.drain()?;
    seal(&sink.file, sink.end, head)?;
    Ok((sink.objects, sink.end, commits_dropped))
}
