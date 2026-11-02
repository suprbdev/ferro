use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::codec::{Data, Encoder, Hash, Object, Ptr, Sink};
use crate::db::{Commit, Head, Refs};
use crate::error::{Error, Result};
use crate::log::Chain;
use crate::map::Map;
use crate::store::{LOG_START, Store, StoreOptions, append_record, seal};

const FLUSH_BYTES: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compacted {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub objects: u64,
    pub commits_dropped: u64,
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
    store.close();
    Ok(Info {
        file_bytes: store.size(),
        branches,
    })
}

pub fn log(path: impl AsRef<Path>, branch: &str) -> Result<Vec<Commit>> {
    let (store, head) = read_only(path.as_ref())?;
    let refs = load_refs(&store, head)?;
    let found = refs
        .try_get(branch)?
        .ok_or_else(|| Error::NoBranch(branch.to_string()))?;
    let commits = found.log.commits(&store)?;
    store.close();
    Ok(commits)
}

pub fn verify(path: impl AsRef<Path>) -> Result<crate::Verified> {
    let (store, head) = read_only(path.as_ref())?;
    let mut report = crate::Verified {
        file_bytes: store.size(),
        ..crate::Verified::default()
    };
    if let Some(ptr) = head {
        store.walk(ptr, |ptr, object| {
            report.objects += 1;
            report.bytes += crate::store::record_len(ptr.offset, object) as u64;
        })?;
    }
    store.close();
    Ok(report)
}

struct CopySink {
    source: Store,
    file: File,
    buf: Vec<u8>,
    end: u64,
    copied: HashMap<Hash, u64>,
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

fn prune(store: &Store, sink: &mut CopySink, head: &Head, keep: Keep) -> Result<(Head, u64)> {
    let total = head.log.len();
    let drop = match keep {
        Keep::All => 0,
        Keep::Last(count) => total.saturating_sub(count.max(1)),
        Keep::Since(time_ms) => {
            let commits = head.log.commits(store)?;
            let stale = commits.iter().take_while(|commit| commit.time_ms < time_ms).count();
            stale.min(total.saturating_sub(1))
        }
    };
    if drop == 0 {
        return Ok((head.clone(), 0));
    }
    let commits = head.log.commits(store)?;
    let log = Chain::rebuild(&commits[drop..], sink)?;
    Ok((
        Head {
            version: head.version,
            root: head.root,
            log,
            merged: head.merged.clone(),
        },
        drop as u64,
    ))
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
            objects: 0,
            commits_dropped: 0,
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
    };
    let mut pruned: Refs = Map::new();
    let mut commits_dropped = 0;
    for (name, branch) in refs.iter() {
        let (next, dropped) = prune(store, &mut sink, &branch, keep)?;
        commits_dropped += dropped;
        pruned.insert(name, next);
    }
    let mut encoder = Encoder::new(&mut sink);
    pruned.encode(&mut encoder)?;
    let head = encoder.finish()?;
    sink.drain()?;
    seal(&sink.file, sink.end, head)?;
    Ok((sink.copied.len() as u64, sink.end, commits_dropped))
}
