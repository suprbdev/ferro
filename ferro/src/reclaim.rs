use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::codec::{Data, Decoder, Encoder, Hash, Object, Ptr, Sink};
use crate::db::Refs;
use crate::error::{Error, Result};
use crate::log::{Chain, Closures, Entry, skip_version};
use crate::platform::copy_owner;
pub(crate) use crate::platform::{rename, sync_parent};
use crate::store::{Basis, LOG_START, Store, StoreOptions, Traced, append_record, seal};

const FLUSH_BYTES: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reclaim {
    pub every_bytes: u64,
}

impl Default for Reclaim {
    fn default() -> Self {
        Reclaim {
            every_bytes: 64 << 20,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compacted {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub objects: u64,
    pub commits_dropped: u64,
    pub passes: u32,
    pub pause: Duration,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Space {
    pub file_bytes: u64,
    pub live_bytes: u64,
    pub retired_bytes: u64,
    pub cycles: u64,
    pub longest_pause: Duration,
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

pub(crate) fn sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".compacting");
    PathBuf::from(name)
}

#[derive(Default)]
pub(crate) struct Roots {
    pub(crate) trees: Vec<Ptr>,
    pub(crate) chains: Vec<Chain>,
}

impl Roots {
    pub(crate) fn of(head: Ptr, refs: &Refs) -> Result<Roots> {
        let chains = refs
            .try_entries()?
            .into_iter()
            .map(|(_, branch)| branch.log)
            .collect();
        Ok(Roots {
            trees: vec![head],
            chains,
        })
    }

    pub(crate) fn walk(
        &self,
        store: &Store,
        mut fresh: impl FnMut(Ptr) -> bool,
        mut visit: impl FnMut(Ptr, &Object, u32),
    ) -> Result<()> {
        let mut stack = Vec::new();
        let mut entries = HashSet::new();
        let mut closures = Closures::default();
        for chain in &self.chains {
            let after = chain.after(store)?;
            chain.visit(store, |ptr, entry, object, len| {
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

struct CopySink<'a> {
    source: Store,
    file: Traced,
    buf: Vec<u8>,
    end: u64,
    copied: HashMap<Hash, Copied>,
    entries: HashSet<Hash>,
    records: HashMap<u64, u64>,
    reach: HashMap<u64, u64>,
    pruned: HashSet<Hash>,
    objects: u64,
    stop: Option<&'a AtomicBool>,
}

#[derive(Clone, Copy)]
enum Copied {
    Object(u64),
    Entry,
}

struct Pending {
    from: Ptr,
    object: Object,
    next: usize,
    links: Vec<Ptr>,
    entry: Option<Rungs>,
}

#[derive(Clone, Copy)]
struct Rungs {
    version: u64,
    prev: Option<usize>,
    skip: Option<usize>,
}

impl Rungs {
    fn of(object: &Object) -> Result<Rungs> {
        let entry = Entry::decode(&mut Decoder::new(&object.body, &object.links, None))?;
        let prev = usize::from(entry.commit.root.is_some());
        let skip = prev + usize::from(entry.prev.is_some());
        Ok(Rungs {
            version: entry.commit.version,
            prev: entry.prev.map(|_| prev),
            skip: entry.skip.map(|_| skip),
        })
    }
}

impl CopySink<'_> {
    fn drain(&mut self) -> Result<()> {
        let start = self.end - self.buf.len() as u64;
        self.file.write_all_at(&self.buf, start)?;
        self.buf.clear();
        Ok(())
    }

    fn emit(
        &mut self,
        from: Ptr,
        object: &Object,
        links: &[Ptr],
        reach: Option<u64>,
    ) -> Result<Ptr> {
        let (hash, offset) = (from.hash, self.end);
        let before = self.buf.len();
        append_record(&mut self.buf, offset, &object.body, links);
        self.end += (self.buf.len() - before) as u64;
        match reach {
            Some(reach) => {
                self.copied.insert(hash, Copied::Entry);
                self.records.insert(from.offset, offset);
                self.reach.insert(offset, reach);
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
        if link.offset == 0 {
            return Some(tombstone);
        }
        self.remap(link)
            .or_else(|| self.pruned.contains(&link.hash).then_some(tombstone))
    }

    fn reached(&self, links: &[Ptr], rungs: Rungs) -> u64 {
        let prev = rungs
            .prev
            .map(|index| links[index])
            .filter(|prev| prev.offset != 0);
        prev.and_then(|prev| self.reach.get(&prev.offset).copied())
            .unwrap_or(rungs.version)
    }

    fn read(&self, ptr: Ptr) -> Result<Pending> {
        if self.stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
            return Err(Error::Interrupted);
        }
        let object = self.source.read_object(ptr)?;
        let entry = if self.entries.contains(&ptr.hash) {
            Some(Rungs::of(&object)?)
        } else {
            None
        };
        Ok(Pending {
            from: ptr,
            object,
            next: 0,
            links: Vec::new(),
            entry,
        })
    }

    fn copy(&mut self, root: Ptr) -> Result<Ptr> {
        if let Some(done) = self.settled(root) {
            return Ok(done);
        }
        let mut stack = vec![self.read(root)?];
        loop {
            let top = stack.last_mut().expect("stack is not empty");
            if top.next < top.object.links.len() {
                let link = top.object.links[top.next];
                let jumps = top
                    .entry
                    .filter(|rungs| rungs.skip == Some(top.next))
                    .is_some_and(|rungs| {
                        self.reached(&top.links, rungs) > skip_version(rungs.version)
                    });
                let settled = if jumps {
                    Some(Ptr {
                        hash: link.hash,
                        offset: 0,
                    })
                } else {
                    self.settled(link)
                };
                if let Some(done) = settled {
                    let top = stack.last_mut().expect("stack is not empty");
                    top.links.push(done);
                    top.next += 1;
                } else {
                    let pending = self.read(link)?;
                    stack.push(pending);
                }
                continue;
            }
            let done = stack.pop().expect("stack is not empty");
            let reach = done.entry.map(|rungs| self.reached(&done.links, rungs));
            let ptr = self.emit(done.from, &done.object, &done.links, reach)?;
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

impl Sink for CopySink<'_> {
    fn put(&mut self, object: Object, hash: Hash) -> Result<Ptr> {
        if let Some(done) = self.remap(Ptr { hash, offset: 0 }) {
            return Ok(done);
        }
        let mut links = Vec::with_capacity(object.links.len());
        for link in &object.links {
            links.push(self.copy(*link)?);
        }
        self.emit(Ptr { hash, offset: 0 }, &object, &links, None)
    }

    fn store_id(&self) -> u64 {
        self.source.id()
    }
}

pub(crate) struct Copier<'a> {
    sink: CopySink<'a>,
}

impl<'a> Copier<'a> {
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
                copied: HashMap::new(),
                entries: HashSet::new(),
                records: HashMap::new(),
                reach: HashMap::new(),
                pruned: HashSet::new(),
                objects: 0,
                stop,
            },
        })
    }

    fn encode(&mut self, refs: &Refs) -> Result<Ptr> {
        let sink = &mut self.sink;
        let mut retained = HashSet::new();
        let mut edge = Vec::new();
        let mut closures = Closures::default();
        for (_, branch) in refs.try_entries()? {
            let after = branch.log.after(&sink.source)?;
            branch.log.visit(&sink.source, |ptr, entry, _, _| {
                if sink.records.contains_key(&ptr.offset) {
                    return Ok(false);
                }
                retained.insert(ptr.hash);
                edge.extend(links_of(entry));
                closures.mark(ptr, branch.log.first, after);
                closures.seed(entry, after);
                Ok(true)
            })?;
        }
        closures.walk(&sink.source, |ptr, entry, _, _| {
            if sink.records.contains_key(&ptr.offset) {
                return Ok(false);
            }
            retained.insert(ptr.hash);
            edge.extend(links_of(entry));
            Ok(true)
        })?;
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
        encoder.finish()
    }

    pub(crate) fn pass(&mut self, refs: &Refs) -> Result<u64> {
        let before = self.sink.end;
        self.encode(refs)?;
        self.sink.drain()?;
        Ok(self.sink.end - before)
    }

    pub(crate) fn end(&self) -> u64 {
        self.sink.end
    }

    pub(crate) fn sync(&self) -> Result<()> {
        self.sink.file.sync_data()?;
        Ok(())
    }

    pub(crate) fn finish(&mut self, refs: &Refs, seq: u64, survival: f64) -> Result<(Ptr, u64)> {
        let head = self.encode(refs)?;
        self.sink.drain()?;
        let basis = Basis::new(self.sink.end, survival);
        seal(&self.sink.file, self.sink.end, head, seq, basis)?;
        Ok((head, self.sink.objects))
    }

    pub(crate) fn abandon(temp: &Path) {
        let _ = fs::remove_file(temp);
    }
}
