use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, fchown};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::codec::{Data, Encoder, Hash, Object, Ptr, Sink};
use crate::db::Refs;
use crate::error::{Error, Result};
use crate::log::Chain;
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

pub(crate) fn sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".compacting");
    PathBuf::from(name)
}

pub(crate) fn sync_parent(path: &Path) -> std::io::Result<()> {
    match path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        Some(parent) => File::open(parent)?.sync_all(),
        None => Ok(()),
    }
}

#[derive(Default)]
pub(crate) struct Roots {
    pub(crate) trees: Vec<Ptr>,
    pub(crate) chains: Vec<Chain>,
}

impl Roots {
    pub(crate) fn of(head: Ptr, refs: &Refs) -> Result<Roots> {
        let chains = refs.iter().map(|(_, branch)| branch.log).collect();
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
        for chain in &self.chains {
            chain.visit(store, |ptr, entry, object, len| {
                if entries.insert(ptr.offset) && fresh(ptr) {
                    visit(ptr, object, len);
                }
                stack.extend(entry.commit.root);
                stack.extend(chain.retained_skip(entry));
                Ok(true)
            })?;
        }
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
    copied: HashMap<Hash, u64>,
    objects: u64,
    stop: Option<&'a AtomicBool>,
}

struct Pending {
    hash: Hash,
    object: Object,
    next: usize,
    links: Vec<Ptr>,
}

impl CopySink<'_> {
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

    fn read(&self, ptr: Ptr) -> Result<Pending> {
        if self.stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
            return Err(Error::Interrupted);
        }
        Ok(Pending {
            hash: ptr.hash,
            object: self.source.read_object(ptr)?,
            next: 0,
            links: Vec::new(),
        })
    }

    fn copy(&mut self, root: Ptr) -> Result<Ptr> {
        if let Some(done) = self.remap(root) {
            return Ok(done);
        }
        let mut stack = vec![self.read(root)?];
        loop {
            let top = stack.last_mut().expect("stack is not empty");
            if top.next < top.object.links.len() {
                let link = top.object.links[top.next];
                if let Some(done) = self.remap(link) {
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
            let ptr = self.emit(done.hash, &done.object, &done.links)?;
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
        self.emit(hash, &object, &links)
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
        let source = fs::metadata(store.path())?;
        file.set_permissions(source.permissions())?;
        let _ = fchown(&file, Some(source.uid()), Some(source.gid()));
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
                objects: 0,
                stop,
            },
        })
    }

    fn encode(&mut self, refs: &Refs) -> Result<Ptr> {
        let sink = &mut self.sink;
        let mut retained = HashSet::new();
        let mut edge = Vec::new();
        for (_, branch) in refs.iter() {
            branch.log.visit(&sink.source, |ptr, entry, _, _| {
                if sink.copied.contains_key(&ptr.hash) {
                    return Ok(false);
                }
                retained.insert(ptr.hash);
                edge.extend(entry.prev.iter().chain(&entry.skip).map(|link| link.hash));
                Ok(true)
            })?;
        }
        for hash in edge {
            if !retained.contains(&hash) && !sink.copied.contains_key(&hash) {
                sink.copied.insert(hash, 0);
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
