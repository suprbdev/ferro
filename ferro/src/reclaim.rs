use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::hash::BuildHasherDefault;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::codec::{Data, Decoder, Encoder, Hash, Object, Ptr, Sink, Staged};
use crate::db::Refs;
use crate::error::{Error, Result};
use crate::log::{Chain, Closures, Entry, skip_version};
use crate::platform::copy_owner;
pub(crate) use crate::platform::{rename, sync_parent};
use crate::refs;
use crate::store::{
    Basis, LOG_START, Prehashed, Store, StoreOptions, Traced, Unpacked, append_staged, body_of,
    compressed, seal, staged, verify_stored,
};

const FLUSH_BYTES: usize = 1 << 20;
const SPARE: usize = 256;
const SPARE_BYTES: usize = 256 << 10;

type ByHash = BuildHasherDefault<Prehashed>;

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
    pub(crate) superseded: Vec<Chain>,
}

impl Roots {
    pub(crate) fn of(store: &Store, head: Ptr) -> Result<Roots> {
        let heads = refs::heads(store, head)?;
        Ok(Roots {
            trees: vec![head],
            chains: heads.current,
            superseded: heads.superseded,
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

struct CopySink<'a> {
    source: Store,
    file: Traced,
    buf: Vec<u8>,
    end: u64,
    scratch: Unpacked,
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
        if link.offset == 0 {
            return Some(tombstone);
        }
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
        let mut bytes = mem::take(&mut self.bytes);
        let step = self.step(ptr, &mut bytes);
        if bytes.capacity() <= SPARE_BYTES {
            self.bytes = bytes;
        }
        step
    }

    fn step(&mut self, ptr: Ptr, bytes: &mut Vec<u8>) -> Result<Step> {
        let mut held = self.spare.pop().unwrap_or_default();
        let (record, stored) = self.source.read_stored(ptr, bytes, &mut held.old)?;
        verify_stored(record, ptr, &mut self.scratch)?;
        let entry = if self.entries.contains(&ptr.hash) {
            let body = body_of(record, &stored, ptr.offset, &mut self.body)?;
            Some(Rungs::of(body, &held.old)?)
        } else {
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
                let settled = match top.entry.filter(|rungs| rungs.skip == Some(top.next)) {
                    Some(rungs) => self.skipped(&top.held.links, rungs, link),
                    None => self.settled(link),
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
                scratch: Unpacked::default(),
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
