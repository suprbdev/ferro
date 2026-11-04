use std::any::TypeId;
use std::collections::{HashMap, HashSet, hash_map};
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::mem;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use crate::codec::{
    Checked, Data, Decoder, HASH_BYTES, Hash, Kind, Object, Ptr, Shape, push_varint,
};
use crate::db::{Db, Head, Refs, Snapshot, now_ms};
use crate::error::{Error, Result};
use crate::log::{Chain, Entry, Merged, Node};
use crate::map::Map;
use crate::reentry::Needs;
use crate::store::{Record, Staging, Store, Txn, staged};

const HAVE_MAGIC: &[u8; 8] = b"FERROHAV";
const BUNDLE_MAGIC: &[u8; 8] = b"FERROBDL";
const FORMAT: u8 = 1;
const COMPRESS_MIN: usize = 96;
const MAX_RATIO: usize = 255;
const RECHECK_SLACK: usize = 16 << 20;
const EXPANSION: usize = 16;
const EXPANSION_SLACK: usize = 16 << 20;
const MAX_BODY: usize = u32::MAX as usize;
const MAX_AHEAD_MS: u64 = 24 * 60 * 60 * 1000;
const MAIN: &str = "main";
const MAX_VERSION: u64 = u64::MAX >> 1;
const READ_BUFFER: usize = 64 << 10;
const READ_STEP: usize = 1 << 20;
const WRITE_CHUNK: usize = 64 << 10;
const SPILL_AT: usize = 1 << 20;
const PLACE_AT: usize = 256 << 10;
const PIPE_CHUNKS: usize = 8;
const PIPE_OBJECTS: usize = 256;
const CACHED_LINKS: usize = 1 << 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Have(Vec<u8>);

impl Have {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Have {
    fn from(bytes: Vec<u8>) -> Self {
        Have(bytes)
    }
}

impl From<Have> for Vec<u8> {
    fn from(have: Have) -> Self {
        have.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle(Vec<u8>);

impl Bundle {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Bundle {
    fn from(bytes: Vec<u8>) -> Self {
        Bundle(bytes)
    }
}

impl From<Bundle> for Vec<u8> {
    fn from(bundle: Bundle) -> Self {
        bundle.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    Ahead,
    FastForward,
    Merged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pulled<R> {
    pub outcome: Outcome,
    pub value: Option<R>,
    pub version: u64,
    pub objects: u64,
    pub sent_bytes: u64,
    pub received_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mark {
    hash: Hash,
    version: u64,
}

struct Marks {
    marks: Vec<Mark>,
    tips: usize,
}

#[derive(Default)]
struct Header {
    tip: Option<Mark>,
    bases: Vec<Mark>,
    roots: Vec<usize>,
    hints: Vec<(usize, usize)>,
    count: u64,
}

fn bad(what: impl Into<String>) -> Error {
    Error::Bundle(what.into())
}

fn skip_version(version: u64) -> u64 {
    version - (version & version.wrapping_neg())
}

fn affordable(bodies: usize, read: usize) -> bool {
    bodies
        <= read
            .saturating_mul(EXPANSION)
            .saturating_add(EXPANSION_SLACK)
}

fn varint_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

fn from_io(error: io::Error) -> Error {
    if error.get_ref().is_some_and(|inner| inner.is::<Error>()) {
        let inner = error.into_inner().expect("the error has a source");
        return *inner
            .downcast::<Error>()
            .expect("the source is a ferro error");
    }
    Error::Io(error)
}

struct Input<R> {
    source: BufReader<R>,
    read: u64,
    bodies: usize,
}

impl<R: Read> Input<R> {
    fn open(source: R, magic: &[u8; 8]) -> Result<Input<R>> {
        let mut input = Input {
            source: BufReader::with_capacity(READ_BUFFER, source),
            read: 0,
            bodies: 0,
        };
        let mut head = [0u8; 9];
        match input.fill(&mut head) {
            Err(Error::Bundle(_)) => return Err(bad("not a ferro sync message")),
            other => other?,
        }
        if &head[..magic.len()] != magic {
            return Err(bad("not a ferro sync message"));
        }
        if head[magic.len()] != FORMAT {
            return Err(bad(format!(
                "format {} is not supported",
                head[magic.len()]
            )));
        }
        Ok(input)
    }

    fn fill(&mut self, out: &mut [u8]) -> Result<()> {
        match self.source.read_exact(out) {
            Ok(()) => {
                self.read += out.len() as u64;
                Ok(())
            }
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => Err(bad("truncated")),
            Err(error) => Err(from_io(error)),
        }
    }

    fn u8(&mut self) -> Result<u8> {
        let mut byte = [0u8];
        self.fill(&mut byte)?;
        Ok(byte[0])
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for index in 0..10 {
            let byte = self.u8()?;
            if index == 9 && byte > 1 {
                break;
            }
            value |= u64::from(byte & 0x7f) << (7 * index);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(bad("varint overflow"))
    }

    fn size(&mut self) -> Result<usize> {
        usize::try_from(self.varint()?).map_err(|_| bad("size overflow"))
    }

    fn below(&mut self, limit: usize) -> Result<usize> {
        let value = self.size()?;
        if value >= limit {
            return Err(bad("index out of range"));
        }
        Ok(value)
    }

    fn hash(&mut self) -> Result<Hash> {
        let mut hash = [0u8; HASH_BYTES];
        self.fill(&mut hash)?;
        Ok(hash)
    }

    fn mark(&mut self) -> Result<Mark> {
        Ok(Mark {
            hash: self.hash()?,
            version: self.varint()?,
        })
    }

    fn bytes(&mut self, len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len.min(READ_STEP));
        let got = (&mut self.source)
            .take(len as u64)
            .read_to_end(&mut out)
            .map_err(from_io)?;
        self.read += got as u64;
        if got != len {
            return Err(bad("truncated"));
        }
        Ok(out)
    }

    fn body(&mut self) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        let flagged = self.varint()?;
        let stored = usize::try_from(flagged >> 1).map_err(|_| bad("size overflow"))?;
        let packed = flagged & 1 == 1;
        let raw = if packed { self.size()? } else { stored };
        if raw.max(stored) > MAX_BODY {
            return Err(bad(format!(
                "an object of {} bytes is too large to store",
                raw.max(stored)
            )));
        }
        if packed && raw > stored.saturating_mul(MAX_RATIO).saturating_add(MAX_RATIO) {
            return Err(bad("implausible compression"));
        }
        let bodies = self.bodies.saturating_add(raw);
        let through = (self.read as usize).saturating_add(stored);
        if !affordable(bodies, through) {
            return Err(bad(format!(
                "its objects expand to {bodies} bytes from the first {through} it carries"
            )));
        }
        self.bodies = bodies;
        let bytes = self.bytes(stored)?;
        if !packed {
            return Ok((bytes, None));
        }
        let body = lz4_flex::block::decompress(&bytes, raw)
            .map_err(|error| bad(format!("bad compressed body: {error}")))?;
        if body.len() != raw {
            return Err(bad("compressed body has the wrong length"));
        }
        let smaller = raw >= COMPRESS_MIN && stored + varint_len(raw as u64) < raw;
        Ok((body, smaller.then_some(bytes)))
    }

    fn finish(&mut self) -> Result<()> {
        let mut byte = [0u8];
        loop {
            match self.source.read(&mut byte) {
                Ok(0) => return Ok(()),
                Ok(_) => return Err(bad("trailing bytes")),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(from_io(error)),
            }
        }
    }
}

fn put_mark(out: &mut Vec<u8>, mark: Mark) {
    out.extend_from_slice(&mark.hash);
    push_varint(out, mark.version);
}

fn put_body(
    out: &mut Vec<u8>,
    body: &[u8],
    packed: Option<Vec<u8>>,
    bodies: &mut usize,
    before: usize,
) {
    *bodies += body.len();
    let packed = packed.filter(|packed| affordable(*bodies, before + out.len() + packed.len()));
    match packed {
        Some(packed) => {
            push_varint(out, (packed.len() as u64) << 1 | 1);
            push_varint(out, body.len() as u64);
            out.extend_from_slice(&packed);
        }
        None => {
            push_varint(out, (body.len() as u64) << 1);
            out.extend_from_slice(body);
        }
    }
}

impl Marks {
    fn encode(&self) -> Vec<u8> {
        let mut out = HAVE_MAGIC.to_vec();
        out.push(FORMAT);
        push_varint(&mut out, self.tips as u64);
        push_varint(&mut out, self.marks.len() as u64);
        for mark in &self.marks {
            put_mark(&mut out, *mark);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Marks> {
        let mut input = Input::open(bytes, HAVE_MAGIC)?;
        let tips = input.size()?;
        let count = input.varint()?;
        if tips as u64 > count {
            return Err(bad("more tips than commits"));
        }
        let mut marks = Vec::new();
        for _ in 0..count {
            marks.push(input.mark()?);
        }
        input.finish()?;
        Ok(Marks { marks, tips })
    }
}

impl Header {
    fn encode(&self) -> Vec<u8> {
        let mut out = BUNDLE_MAGIC.to_vec();
        out.push(FORMAT);
        match self.tip {
            Some(tip) => {
                out.push(1);
                put_mark(&mut out, tip);
            }
            None => out.push(0),
        }
        push_varint(&mut out, self.bases.len() as u64);
        for base in &self.bases {
            put_mark(&mut out, *base);
        }
        push_varint(&mut out, self.roots.len() as u64);
        for root in &self.roots {
            push_varint(&mut out, *root as u64);
        }
        push_varint(&mut out, self.hints.len() as u64);
        for (parent, index) in &self.hints {
            push_varint(&mut out, *parent as u64);
            push_varint(&mut out, *index as u64);
        }
        push_varint(&mut out, self.count);
        out
    }

    fn decode<R: Read>(input: &mut Input<R>) -> Result<Header> {
        let tip = match input.u8()? {
            0 => None,
            1 => Some(input.mark()?),
            _ => return Err(bad("bad head flag")),
        };
        let mut bases = Vec::new();
        for _ in 0..input.varint()? {
            bases.push(input.mark()?);
        }
        let mut roots = Vec::new();
        for _ in 0..input.varint()? {
            roots.push(input.below(bases.len())?);
        }
        let mut hints = Vec::new();
        for _ in 0..input.varint()? {
            let parent = input.below(roots.len() + hints.len())?;
            hints.push((parent, input.size()?));
        }
        let count = input.varint()?;
        if tip.is_none() && !(bases.is_empty() && count == 0) {
            return Err(bad("it carries objects but no head"));
        }
        Ok(Header {
            tip,
            bases,
            roots,
            hints,
            count,
        })
    }
}

struct Emitter {
    store: Store,
    header: Vec<u8>,
    order: Vec<Ptr>,
    next: usize,
    references: HashMap<Hash, usize>,
    bodies: usize,
    sent: usize,
    out: Vec<u8>,
    pos: usize,
}

impl Emitter {
    fn new(
        store: &Store,
        header: &Header,
        order: Vec<Ptr>,
        references: HashMap<Hash, usize>,
    ) -> Emitter {
        Emitter {
            store: store.clone(),
            header: header.encode(),
            order,
            next: 0,
            references,
            bodies: 0,
            sent: 0,
            out: Vec::new(),
            pos: 0,
        }
    }

    fn fill(&mut self) -> Result<bool> {
        self.out.clear();
        self.pos = 0;
        if !self.header.is_empty() {
            mem::swap(&mut self.out, &mut self.header);
            self.sent += self.out.len();
            return Ok(true);
        }
        while self.out.len() < WRITE_CHUNK && self.next < self.order.len() {
            let (object, packed) = self.store.read_packed(self.order[self.next])?;
            self.next += 1;
            let before = self.sent;
            put_body(
                &mut self.out,
                &object.body,
                packed,
                &mut self.bodies,
                before,
            );
            push_varint(&mut self.out, object.links.len() as u64);
            for link in &object.links {
                self.out.extend_from_slice(&link.hash);
                let reference = self.references.get(&link.hash).copied().unwrap_or(0);
                push_varint(&mut self.out, reference as u64);
            }
        }
        self.sent += self.out.len();
        Ok(!self.out.is_empty())
    }

    fn send(mut self, pipe: SyncSender<Result<Vec<u8>>>) {
        loop {
            let chunk = match self.fill() {
                Ok(true) => Ok(mem::take(&mut self.out)),
                Ok(false) => return,
                Err(error) => Err(error),
            };
            let failed = chunk.is_err();
            if pipe.send(chunk).is_err() || failed {
                return;
            }
        }
    }

    fn write_to(mut self, out: &mut impl Write) -> Result<u64> {
        while self.fill()? {
            out.write_all(&self.out)?;
        }
        out.flush()?;
        Ok(self.sent as u64)
    }
}

struct Piped {
    pipe: Receiver<Result<Vec<u8>>>,
    chunk: Vec<u8>,
    pos: usize,
}

impl Read for Piped {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.chunk.len() {
            match self.pipe.recv() {
                Ok(Ok(chunk)) => (self.chunk, self.pos) = (chunk, 0),
                Ok(Err(error)) => return Err(io::Error::other(error)),
                Err(_) => return Ok(0),
            }
        }
        let take = buf.len().min(self.chunk.len() - self.pos);
        buf[..take].copy_from_slice(&self.chunk[self.pos..self.pos + take]);
        self.pos += take;
        Ok(take)
    }
}

impl Read for Emitter {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.out.len() && !self.fill().map_err(io::Error::other)? {
            return Ok(0);
        }
        let take = buf.len().min(self.out.len() - self.pos);
        buf[..take].copy_from_slice(&self.out[self.pos..self.pos + take]);
        self.pos += take;
        Ok(take)
    }
}

fn corrupt(what: &str) -> Error {
    Error::Corrupt(format!("commit log {what}"))
}

fn entry_at(store: &Store, chain: &Chain, version: u64) -> Result<Option<Ptr>> {
    let Some(mut ptr) = chain.tip else {
        return Ok(None);
    };
    if version < chain.first || version > chain.last {
        return Ok(None);
    }
    loop {
        let entry = Entry::load(store, ptr)?;
        let at = entry.commit.version;
        if at == version {
            return Ok(Some(ptr));
        }
        if at < version {
            return Err(corrupt("skipped a version"));
        }
        ptr = match entry.skip {
            Some(skip) if skip.offset != 0 && skip_version(at) >= version => skip,
            _ => entry.prev.ok_or_else(|| corrupt("chain ends early"))?,
        };
    }
}

fn hash_at(store: &Store, chain: &Chain, version: u64) -> Result<Option<Hash>> {
    Ok(entry_at(store, chain, version)?.map(|ptr| ptr.hash))
}

fn shared_prefix(store: &Store, a: &Chain, b: &Chain) -> Result<Option<u64>> {
    if a.tip.is_none() || b.tip.is_none() {
        return Ok(None);
    }
    let (low, high) = (a.first.max(b.first), a.last.min(b.last));
    if low > high {
        return Ok(None);
    }
    let same = |version: u64| -> Result<bool> {
        let ours = hash_at(store, a, version)?;
        Ok(ours.is_some() && ours == hash_at(store, b, version)?)
    };
    if same(high)? {
        return Ok(Some(high));
    }
    if !same(low)? {
        return Ok(None);
    }
    let (mut low, mut high) = (low, high);
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if same(middle)? {
            low = middle;
        } else {
            high = middle;
        }
    }
    Ok(Some(low))
}

fn locate(store: &Store, refs: &Refs, mark: Mark) -> Result<Option<(Ptr, Chain)>> {
    let mut found: Option<(Ptr, Chain)> = None;
    for (_, head) in refs.try_entries()? {
        if found.is_some_and(|(_, chain)| chain.first <= head.log.first) {
            continue;
        }
        if let Some(ptr) = entry_at(store, &head.log, mark.version)?
            && ptr.hash == mark.hash
        {
            let chain = Chain {
                tip: Some(ptr),
                first: head.log.first,
                last: mark.version,
            };
            found = Some((ptr, chain));
        }
    }
    Ok(found)
}

fn read_entry(object: &Object) -> Result<Entry> {
    Entry::decode(&mut Decoder::new(&object.body, &object.links, None))
}

#[derive(Clone, Copy)]
enum Origin {
    Root(usize),
    Child(Hash, usize),
}

struct Difference {
    known: HashMap<Hash, Origin>,
    send: HashSet<Hash>,
    links: HashMap<Hash, Vec<Ptr>>,
    cached: usize,
}

fn difference(store: &Store, old_roots: &[Ptr], new_roots: Vec<Ptr>) -> Result<Difference> {
    let mut found = Difference {
        known: HashMap::new(),
        send: HashSet::new(),
        links: HashMap::new(),
        cached: 0,
    };
    for (slot, root) in old_roots.iter().enumerate() {
        found.known.entry(root.hash).or_insert(Origin::Root(slot));
    }
    let mut previous = old_roots.to_vec();
    for root in new_roots {
        found.step(store, previous, root)?;
        previous = vec![root];
    }
    Ok(found)
}

impl Difference {
    fn classified(&self, hash: &Hash) -> bool {
        self.known.contains_key(hash) || self.send.contains(hash)
    }

    fn step(&mut self, store: &Store, mut old: Vec<Ptr>, root: Ptr) -> Result<()> {
        let mut seen = HashSet::from([root.hash]);
        let mut expanded = HashSet::new();
        let mut fresh = vec![root];
        loop {
            fresh.retain(|ptr| !self.classified(&ptr.hash));
            if fresh.is_empty() {
                return Ok(());
            }
            let mut deeper = Vec::new();
            for ptr in old.drain(..) {
                if seen.contains(&ptr.hash) || !expanded.insert(ptr.hash) {
                    continue;
                }
                let links = match self.links.get(&ptr.hash) {
                    Some(links) => links.clone(),
                    None => store.read_links(ptr)?,
                };
                for (index, link) in links.into_iter().enumerate() {
                    if link.offset == 0 {
                        continue;
                    }
                    if !self.classified(&link.hash) {
                        self.known.insert(link.hash, Origin::Child(ptr.hash, index));
                    }
                    deeper.push(link);
                }
            }
            old = deeper;
            let mut next = Vec::new();
            for ptr in fresh.drain(..) {
                if self.classified(&ptr.hash) {
                    continue;
                }
                if ptr.offset == 0 {
                    return Err(bad(
                        "the branch links to pruned history outside its commit log",
                    ));
                }
                let links = store.read_links(ptr)?;
                for link in &links {
                    if seen.insert(link.hash) {
                        next.push(*link);
                    }
                }
                self.send.insert(ptr.hash);
                if self.cached + 1 + links.len() <= CACHED_LINKS {
                    self.cached += 1 + links.len();
                    self.links.insert(ptr.hash, links);
                }
            }
            fresh = next;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    Waiting,
    Open,
    Done,
}

struct Ordered {
    order: Vec<Ptr>,
    ids: HashMap<Hash, usize>,
    hints: Vec<(usize, usize)>,
}

fn post_order(
    store: &Store,
    starts: &[Ptr],
    state: &mut HashMap<Hash, Visit>,
    mut cached: HashMap<Hash, Vec<Ptr>>,
    old: &HashMap<Hash, Origin>,
    anchors: usize,
) -> Result<Ordered> {
    let mut links_of = |ptr: Ptr| match cached.remove(&ptr.hash) {
        Some(links) => Ok(links),
        None => store.read_links(ptr),
    };
    let mut found = Ordered {
        order: Vec::with_capacity(state.len()),
        ids: HashMap::new(),
        hints: Vec::new(),
    };
    for start in starts {
        if state.get(&start.hash) != Some(&Visit::Waiting) {
            continue;
        }
        state.insert(start.hash, Visit::Open);
        let mut stack = vec![(*start, links_of(*start)?, 0usize)];
        while let Some((_, links, next)) = stack.last_mut() {
            if let Some(link) = links.get(*next).copied() {
                *next += 1;
                if link.offset != 0 && state.get(&link.hash) == Some(&Visit::Waiting) {
                    state.insert(link.hash, Visit::Open);
                    stack.push((link, links_of(link)?, 0));
                }
                continue;
            }
            let (ptr, links, _) = stack.pop().expect("the stack has a top");
            for link in &links {
                if !state.contains_key(&link.hash)
                    && matches!(old.get(&link.hash), Some(Origin::Child(..)))
                {
                    hint_for(link.hash, old, &mut found.ids, &mut found.hints, anchors);
                }
            }
            state.insert(ptr.hash, Visit::Done);
            found.order.push(ptr);
        }
    }
    Ok(found)
}

struct Known {
    mark: Mark,
    ptr: Ptr,
    chain: Chain,
    tip: bool,
}

struct Sending {
    ptr: Ptr,
    root: Option<Ptr>,
    merged: Option<Merged>,
    generation: u64,
}

impl Sending {
    fn of(ptr: Ptr, entry: &Entry) -> Sending {
        Sending {
            ptr,
            root: entry.commit.root,
            merged: entry.merged,
            generation: entry.generation(),
        }
    }
}

fn plan(store: &Store, refs: &Refs, name: &str, have: &Marks) -> Result<Emitter> {
    let mut header = Header::default();
    let chain = refs.try_get(name)?.map(|head| head.log);
    let Some(chain) = chain.filter(|chain| chain.tip.is_some()) else {
        return Ok(Emitter::new(store, &header, Vec::new(), HashMap::new()));
    };
    let tip = chain.tip.expect("chain has a tip");
    let mut known = Vec::new();
    for (index, mark) in have.marks.iter().enumerate() {
        if let Some((ptr, base)) = locate(store, refs, *mark)? {
            known.push(Known {
                mark: *mark,
                ptr,
                chain: base,
                tip: index < have.tips,
            });
        }
    }
    let mut prefix: Option<(u64, usize)> = None;
    for (slot, base) in known.iter().enumerate() {
        if let Some(version) = shared_prefix(store, &chain, &base.chain)?
            && prefix.is_none_or(|(best, _)| version > best)
        {
            prefix = Some((version, slot));
        }
    }
    let named = have.marks.iter().any(|mark| mark.hash == tip.hash);
    let floor = prefix
        .map_or(0, |(version, _)| version)
        .max(chain.first.saturating_sub(1))
        .min(if named { chain.last } else { chain.last - 1 });
    let mut entries = Vec::new();
    let mut next = Some(tip);
    while let Some(ptr) = next {
        let entry = read_entry(&store.read_object(ptr)?)?;
        let at = entry.commit.version;
        if at <= floor {
            break;
        }
        next = entry.prev.filter(|_| at > chain.first);
        entries.push(Sending::of(ptr, &entry));
    }
    header.tip = Some(Mark {
        hash: tip.hash,
        version: chain.last,
    });
    let mut slots: HashMap<usize, usize> = HashMap::new();
    let mut base_slot = |slot: usize, bases: &mut Vec<Mark>| -> usize {
        *slots.entry(slot).or_insert_with(|| {
            bases.push(known[slot].mark);
            bases.len() - 1
        })
    };
    let mut old_roots: Vec<Ptr> = Vec::new();
    for (slot, base) in known.iter().enumerate() {
        if !base.tip && prefix.map(|(_, chosen)| chosen) != Some(slot) {
            continue;
        }
        let entry = Entry::load(store, base.ptr)?;
        let Some(root) = entry.commit.root else {
            continue;
        };
        if old_roots.iter().any(|old| old.hash == root.hash) {
            continue;
        }
        header.roots.push(base_slot(slot, &mut header.bases));
        old_roots.push(root);
    }
    let extra = closures(store, &chain, &entries, &known, &mut header.bases)?;
    let mut new_roots = Vec::new();
    for sending in entries.iter().rev().chain(&extra) {
        new_roots.extend(sending.root);
    }
    let Difference {
        known: old,
        send,
        links,
        ..
    } = difference(store, &old_roots, new_roots)?;
    let mut state: HashMap<Hash, Visit> = send
        .into_iter()
        .map(|hash| (hash, Visit::Waiting))
        .collect();
    let mut starts: Vec<(u64, Ptr)> = Vec::with_capacity(entries.len() + extra.len());
    for sending in entries.iter().chain(&extra) {
        state.insert(sending.ptr.hash, Visit::Waiting);
        starts.push((sending.generation, sending.ptr));
    }
    starts.sort_by_key(|(generation, ptr)| (*generation, ptr.offset));
    let starts: Vec<Ptr> = starts.into_iter().map(|(_, ptr)| ptr).collect();
    let Ordered { order, ids, hints } =
        post_order(store, &starts, &mut state, links, &old, old_roots.len())?;
    if order.len() != state.len() {
        return Err(Error::Corrupt(
            "sync found objects its head does not reach".into(),
        ));
    }
    drop(state);
    header.hints = hints;
    header.count = order.len() as u64;
    let mut references = HashMap::with_capacity(old_roots.len() + ids.len());
    for (slot, root) in old_roots.iter().enumerate() {
        references.entry(root.hash).or_insert(1 + slot);
    }
    for (hash, id) in ids {
        if let Some(Origin::Child(..)) = old.get(&hash) {
            references.insert(hash, 1 + old_roots.len() + id);
        }
    }
    Ok(Emitter::new(store, &header, order, references))
}

fn closures(
    store: &Store,
    chain: &Chain,
    entries: &[Sending],
    known: &[Known],
    bases: &mut Vec<Mark>,
) -> Result<Vec<Sending>> {
    let after = chain.after(store)?;
    let mut sending: HashSet<Hash> = entries.iter().map(|sending| sending.ptr.hash).collect();
    let mut seeds: Vec<(Merged, Option<u64>)> = entries
        .iter()
        .filter_map(|sending| sending.merged.map(|merged| (merged, after)))
        .collect();
    let mut extra = Vec::new();
    while let Some((merged, path)) = seeds.pop() {
        let (ptr, after) = (merged.ptr, merged.after(path));
        let marked = |hash: &Hash| bases.iter().any(|mark| mark.hash == *hash);
        if ptr.offset == 0 || sending.contains(&ptr.hash) || marked(&ptr.hash) {
            continue;
        }
        let node = Node::load(store, ptr)?;
        let version = node.version();
        if known.iter().any(|held| held.mark.hash == ptr.hash) {
            bases.push(Mark {
                hash: ptr.hash,
                version,
            });
            continue;
        }
        let bare = after.is_some_and(|after| node.time() < after);
        let low = match after {
            _ if bare => version,
            Some(after) => {
                node.lowest_kept(store, merged.floor, |entry| entry.commit.time_ms >= after)?
            }
            None => node.lowest_kept(store, merged.floor, |_| true)?,
        };
        let reach = Chain {
            tip: Some(ptr),
            first: low,
            last: version,
        };
        let mut held = None;
        for base in known {
            held = held.max(shared_prefix(store, &reach, &base.chain)?);
        }
        let stop = held.map_or(low, |held| low.max(held + 1)).min(version);
        let mut next = Some(ptr);
        while let Some(at) = next.take() {
            if at.offset == 0 || sending.contains(&at.hash) {
                break;
            }
            let entry = read_entry(&store.read_object(at)?)?;
            let version = entry.commit.version;
            if version < stop {
                break;
            }
            sending.insert(at.hash);
            if let Some(nested) = entry.merged.filter(|_| !bare) {
                seeds.push((nested, after));
            }
            next = entry.prev.filter(|_| version > stop);
            extra.push(Sending::of(at, &entry));
        }
    }
    Ok(extra)
}

fn hint_for(
    hash: Hash,
    old: &HashMap<Hash, Origin>,
    ids: &mut HashMap<Hash, usize>,
    hints: &mut Vec<(usize, usize)>,
    roots: usize,
) -> usize {
    let mut path = Vec::new();
    let mut at = hash;
    let parent = loop {
        if let Some(id) = ids.get(&at) {
            break roots + id;
        }
        match old[&at] {
            Origin::Root(slot) => break slot,
            Origin::Child(parent, index) => {
                path.push((at, index));
                at = parent;
            }
        }
    };
    let mut parent = parent;
    for (child, index) in path.into_iter().rev() {
        hints.push((parent, index));
        let id = hints.len() - 1;
        ids.insert(child, id);
        parent = roots + id;
    }
    ids[&hash]
}

#[derive(Clone, Copy)]
struct Facts {
    version: u64,
    time: u64,
    generation: u64,
    reach: u64,
    last_merge: u64,
    bounded: bool,
}

impl Facts {
    fn of(entry: &Entry) -> Facts {
        Facts {
            version: entry.commit.version,
            time: entry.commit.time_ms,
            generation: entry.generation(),
            reach: entry.graph.reach,
            last_merge: entry.graph.last_merge,
            bounded: entry.graph.bounded,
        }
    }
}

fn check_graph(
    own: Facts,
    link: Option<Merged>,
    prev: Option<Option<Facts>>,
    merged: Option<Option<Facts>>,
) -> Result<()> {
    let known = prev.is_none_or(|facts| facts.is_some()) && merged.is_none_or(|f| f.is_some());
    let (prev, linked) = (prev.flatten(), merged.flatten());
    let generation = |facts: Option<Facts>| facts.map_or(0, |facts| facts.generation);
    let parents = generation(prev).max(generation(linked));
    let reach = prev
        .map_or(0, |facts| facts.reach)
        .max(linked.map_or(0, |facts| facts.generation));
    let bounded = prev.is_some_and(|facts| facts.bounded)
        || link.is_some_and(|link| link.floor > 1 || link.after.is_some())
        || linked.is_some_and(|facts| facts.bounded);
    let last_merge = match (link, prev) {
        (Some(_), _) => Some(own.version),
        (None, Some(prev)) => Some(prev.last_merge),
        (None, None) if known => Some(0),
        (None, None) => None,
    };
    let consistent = if known {
        own.generation == parents + 1 && own.reach == reach && own.bounded == bounded
    } else {
        own.generation > parents && own.reach >= reach && (own.bounded || !bounded)
    };
    let later = prev.is_none_or(|facts| own.time >= facts.time)
        && linked.is_none_or(|facts| own.time >= facts.time);
    let floored = match (link, linked) {
        (Some(link), Some(facts)) => link.floor <= facts.version,
        _ => true,
    };
    if !consistent || !later || !floored || last_merge.is_some_and(|last| own.last_merge != last) {
        return Err(bad(format!(
            "v{} does not agree with the commits it links to",
            own.version
        )));
    }
    Ok(())
}

enum Below {
    Joins(Hash),
    Chain(Chain),
    Gone,
}

struct Segment {
    versions: HashMap<u64, Hash>,
    bottom: u64,
    below: Below,
}

enum Found {
    Sent(Hash),
    Held(Ptr),
    Gone,
}

struct Sent {
    facts: Facts,
    root: Option<Ptr>,
    prev: Option<Ptr>,
    skip: Option<Ptr>,
    merged: Option<Merged>,
    segment: usize,
}

impl Sent {
    fn of(entry: &Entry, segment: usize) -> Sent {
        Sent {
            facts: Facts::of(entry),
            root: entry.commit.root,
            prev: entry.prev,
            skip: entry.skip,
            merged: entry.merged,
            segment,
        }
    }
}

fn decode_commit(object: &Object) -> Result<Entry> {
    let entry =
        Entry::decode(&mut Decoder::new(&object.body, &object.links, None)).map_err(|error| {
            bad(format!(
                "an object it links as a commit is not one: {error}"
            ))
        })?;
    let used = usize::from(entry.commit.root.is_some())
        + usize::from(entry.prev.is_some())
        + usize::from(entry.skip.is_some())
        + usize::from(entry.merged.is_some());
    if used != object.links.len() {
        return Err(bad("a commit carries links it does not use"));
    }
    Ok(entry)
}

trait Objects {
    fn offset_of(&self, hash: &Hash) -> Option<u64>;
    fn read_object(&self, ptr: Ptr) -> Result<Object>;
    fn relink(&mut self, object: Object, hash: Hash) -> Result<()>;
}

impl Objects for Staging {
    fn offset_of(&self, hash: &Hash) -> Option<u64> {
        Staging::offset_of(self, hash)
    }

    fn read_object(&self, ptr: Ptr) -> Result<Object> {
        Staging::read_object(self, ptr)
    }

    fn relink(&mut self, _: Object, _: Hash) -> Result<()> {
        Ok(())
    }
}

struct Placed<'a> {
    txn: &'a mut Txn,
    earlier: &'a HashMap<Hash, u64>,
}

impl Objects for Placed<'_> {
    fn offset_of(&self, hash: &Hash) -> Option<u64> {
        self.txn
            .offset_of(hash)
            .or_else(|| self.earlier.get(hash).copied())
    }

    fn read_object(&self, ptr: Ptr) -> Result<Object> {
        self.txn.read_object(ptr)
    }

    fn relink(&mut self, object: Object, hash: Hash) -> Result<()> {
        self.txn.put_fresh(object, hash)?;
        if self.txn.buffered() >= SPILL_AT {
            self.txn.spill()?;
        }
        Ok(())
    }
}

fn sent_ptr(objects: &impl Objects, hash: Hash) -> Ptr {
    let offset = objects
        .offset_of(&hash)
        .expect("a sent object has an offset");
    Ptr { hash, offset }
}

fn read_commit(objects: &impl Objects, hash: Hash) -> Result<Entry> {
    decode_commit(&objects.read_object(sent_ptr(objects, hash))?)
}

struct Landing<'a> {
    store: &'a Store,
    low: u64,
    segments: Vec<Segment>,
    commits: HashMap<Hash, Sent>,
}

impl Landing<'_> {
    fn find(&self, mut segment: usize, version: u64) -> Result<Found> {
        loop {
            let seg = &self.segments[segment];
            if version >= seg.bottom {
                return seg
                    .versions
                    .get(&version)
                    .map(|hash| Found::Sent(*hash))
                    .ok_or_else(|| bad("a commit links past the top of its chain"));
            }
            match &seg.below {
                Below::Joins(hash) => segment = self.commits[hash].segment,
                Below::Chain(chain) => {
                    let found = entry_at(self.store, chain, version)?;
                    return Ok(found.map_or(Found::Gone, Found::Held));
                }
                Below::Gone => return Ok(Found::Gone),
            }
        }
    }

    fn facts(&self, ptr: Ptr) -> Result<Option<Facts>> {
        if ptr.offset == 0 {
            return Ok(None);
        }
        match self.commits.get(&ptr.hash) {
            Some(sent) => Ok(Some(sent.facts)),
            None => Ok(Some(Facts::of(&*Entry::load(self.store, ptr)?))),
        }
    }

    fn chained(
        &self,
        objects: &impl Objects,
        unused: &mut HashSet<Hash>,
        segment: usize,
        want: Ptr,
        at: u64,
    ) -> Result<Ptr> {
        match self.find(segment, at)? {
            Found::Sent(hash) if hash == want.hash => {
                unused.remove(&hash);
                Ok(sent_ptr(objects, hash))
            }
            Found::Held(ptr) if ptr.hash == want.hash => Ok(ptr),
            Found::Gone => Ok(Ptr {
                hash: want.hash,
                offset: 0,
            }),
            Found::Held(_) if segment == 0 && at < self.low => Err(bad(format!(
                "v{at}, which it builds on, does not match this database"
            ))),
            _ => Err(bad(
                "a commit links to one that is not the commit at that version",
            )),
        }
    }

    fn segment(&mut self, refs: &Refs, objects: &impl Objects, top: Hash) -> Result<Vec<Hash>> {
        let id = self.segments.len();
        let mut versions = HashMap::new();
        let mut merged = Vec::new();
        let (mut at, mut expected) = (top, None);
        let below = loop {
            let entry = read_commit(objects, at)?;
            let version = entry.commit.version;
            let skipped = expected.is_some_and(|want| want != version);
            if version == 0 || version > MAX_VERSION || skipped {
                return Err(bad(
                    "a merged chain's versions do not descend one at a time",
                ));
            }
            versions.insert(version, at);
            merged.extend(entry.merged.map(|link| link.ptr.hash));
            let prev = entry.prev;
            self.commits.insert(at, Sent::of(&entry, id));
            let Some(prev) = prev else {
                if version != 1 {
                    return Err(bad(format!("v{version} has no parent")));
                }
                break Below::Gone;
            };
            if let Some(sent) = self.commits.get(&prev.hash) {
                if sent.facts.version != version - 1 {
                    return Err(bad("a merged chain joins another at the wrong version"));
                }
                break Below::Joins(prev.hash);
            }
            if objects.offset_of(&prev.hash).is_some() {
                (at, expected) = (prev.hash, Some(version - 1));
                continue;
            }
            let mark = Mark {
                hash: prev.hash,
                version: version - 1,
            };
            break match locate(self.store, refs, mark)? {
                Some((_, chain)) => Below::Chain(chain),
                None => Below::Gone,
            };
        };
        let bottom = versions.keys().copied().min().unwrap_or(1);
        self.segments.push(Segment {
            versions,
            bottom,
            below,
        });
        Ok(merged)
    }
}

type Parent = (Vec<Ptr>, HashMap<usize, Kind>);

struct Vetting<'a, O> {
    store: &'a Store,
    objects: &'a O,
    held_at: &'a HashMap<Hash, usize>,
    roots: &'a [Ptr],
    hinted: &'a [Ptr],
    hint_kinds: &'a [Option<Kind>],
    root_kind: &'a Kind,
    sent: Checked,
    held: Checked,
    spent: usize,
    budget: usize,
    queue: Vec<(Ptr, Kind)>,
}

fn rejected(error: Error) -> Error {
    match error {
        Error::Decode(message) => bad(message),
        other => other,
    }
}

impl<O: Objects> Vetting<'_, O> {
    fn run(&mut self) -> Result<()> {
        while let Some((ptr, kind)) = self.queue.pop() {
            let shape = match self.objects.offset_of(&ptr.hash) {
                Some(offset) => self.sent(
                    Ptr {
                        hash: ptr.hash,
                        offset,
                    },
                    &kind,
                )?,
                None => {
                    let reference = *self.held_at.get(&ptr.hash).ok_or_else(|| {
                        bad("an object links to one that was neither sent nor known")
                    })?;
                    match self.held(ptr.hash, reference, &kind)? {
                        Some(shape) => shape,
                        None => continue,
                    }
                }
            };
            if !kind.fits(shape) {
                return Err(bad(format!(
                    "a {} it links to holds {} entries at level {}, not what the link claims",
                    kind.name, shape.count, shape.level
                )));
            }
        }
        Ok(())
    }

    fn sent(&mut self, ptr: Ptr, kind: &Kind) -> Result<Shape> {
        if let Some(shape) = self.sent.get(&ptr.hash, kind) {
            return Ok(shape);
        }
        let object = self.objects.read_object(ptr)?;
        self.spent = self.spent.saturating_add(object.body.len());
        if self.spent > self.budget {
            return Err(bad("it reuses its objects under too many key ranges"));
        }
        let (shape, found) = kind.check(&object.body, &object.links).map_err(|error| {
            bad(format!(
                "a {} it carries does not decode: {error}",
                kind.name
            ))
        })?;
        for (index, child) in found {
            self.queue.push((object.links[index], child));
        }
        self.sent.insert(ptr.hash, kind, shape);
        Ok(shape)
    }

    fn held(&mut self, hash: Hash, reference: usize, kind: &Kind) -> Result<Option<Shape>> {
        let roots = self.roots.len();
        let hint_kinds = self.hint_kinds;
        let (ptr, known) = if reference <= roots {
            (self.roots[reference - 1], Some(self.root_kind))
        } else {
            let id = reference - 1 - roots;
            (self.hinted[id], hint_kinds[id].as_ref())
        };
        if ptr.hash != hash {
            return Err(bad("a link does not match the object it names"));
        }
        if known.is_some_and(|known| known.implies(kind)) {
            return Ok(None);
        }
        if known.is_some_and(|known| known.id == kind.id) {
            return (kind.held)(kind, self.store, ptr)
                .map(Some)
                .map_err(rejected);
        }
        if let Some(shape) = self.held.get(&hash, kind) {
            return Ok(Some(shape));
        }
        let object = self.store.read_object(ptr)?;
        let shape = match kind.check(&object.body, &object.links) {
            Ok((shape, found)) if found.is_empty() => shape,
            _ => {
                return Err(bad(format!(
                    "it links as a {} to an object this database holds as another type",
                    kind.name
                )));
            }
        };
        self.held.insert(hash, kind, shape);
        Ok(Some(shape))
    }
}

struct Anchors {
    generation: u64,
    bases: Vec<(Ptr, Chain)>,
    roots: Vec<Ptr>,
    hinted: Vec<Ptr>,
    hint_kinds: Vec<Option<Kind>>,
}

impl Anchors {
    fn held(&self) -> HashMap<Hash, Ptr> {
        let mut held = HashMap::with_capacity(self.roots.len() + self.hinted.len());
        for ptr in self.roots.iter().chain(&self.hinted) {
            held.entry(ptr.hash).or_insert(*ptr);
        }
        held
    }
}

fn located_bases(store: &Store, refs: &Refs, header: &Header) -> Result<Vec<(Ptr, Chain)>> {
    let mut bases = Vec::with_capacity(header.bases.len());
    for mark in &header.bases {
        let found = locate(store, refs, *mark)?.ok_or_else(|| {
            Error::StaleHave(format!(
                "it builds on v{} {}, which this database does not have",
                mark.version,
                hex(&mark.hash)
            ))
        })?;
        bases.push(found);
    }
    Ok(bases)
}

fn anchors(store: &Store, refs: &Refs, header: &Header, root_kind: &Kind) -> Result<Anchors> {
    let bases = located_bases(store, refs, header)?;
    let mut roots = Vec::with_capacity(header.roots.len());
    for slot in &header.roots {
        let entry = Entry::load(store, bases[*slot].0)?;
        roots.push(
            entry
                .commit
                .root
                .ok_or_else(|| bad("a base it names has no root"))?,
        );
    }
    let mut hinted: Vec<Ptr> = Vec::with_capacity(header.hints.len());
    let mut hint_kinds: Vec<Option<Kind>> = Vec::with_capacity(header.hints.len());
    let mut parents: HashMap<(Ptr, Option<TypeId>), Parent> = HashMap::new();
    for (parent, slot) in &header.hints {
        let (from, kind) = if *parent < roots.len() {
            (roots[*parent], Some(root_kind))
        } else {
            let id = *parent - roots.len();
            (hinted[id], hint_kinds[id].as_ref())
        };
        let (links, kinds) = match parents.entry((from, kind.map(|kind| kind.id))) {
            hash_map::Entry::Occupied(found) => found.into_mut(),
            hash_map::Entry::Vacant(empty) => {
                let object = store.read_object(from)?;
                let kinds = kind
                    .and_then(|kind| kind.check(&object.body, &object.links).ok())
                    .map(|(_, found)| found.into_iter().collect())
                    .unwrap_or_default();
                empty.insert((object.links, kinds))
            }
        };
        let link = *links
            .get(*slot)
            .ok_or_else(|| bad("a hint names a link that does not exist"))?;
        if link.offset == 0 {
            return Err(bad("a hint leads into pruned history"));
        }
        hinted.push(link);
        hint_kinds.push(kinds.get(slot).cloned());
    }
    Ok(Anchors {
        generation: store.id(),
        bases,
        roots,
        hinted,
        hint_kinds,
    })
}

fn reanchor(store: &Store, refs: &Refs, header: &Header, before: &Anchors) -> Result<Anchors> {
    let bases = located_bases(store, refs, header)?;
    let mut roots = Vec::with_capacity(header.roots.len());
    for (slot, was) in header.roots.iter().zip(&before.roots) {
        let entry = Entry::load(store, bases[*slot].0)?;
        let root = entry
            .commit
            .root
            .filter(|root| root.hash == was.hash)
            .ok_or_else(|| Error::Corrupt("a commit no longer names the root it had".into()))?;
        roots.push(root);
    }
    let hinted = if store.id() == before.generation {
        before.hinted.clone()
    } else {
        let mut hinted: Vec<Ptr> = Vec::with_capacity(before.hinted.len());
        let mut parents: HashMap<Ptr, Vec<Ptr>> = HashMap::new();
        for ((parent, slot), was) in header.hints.iter().zip(&before.hinted) {
            let from = match parent.checked_sub(roots.len()) {
                None => roots[*parent],
                Some(id) => hinted[id],
            };
            let links = match parents.entry(from) {
                hash_map::Entry::Occupied(found) => found.into_mut(),
                hash_map::Entry::Vacant(empty) => empty.insert(store.read_links(from)?),
            };
            let link = links
                .get(*slot)
                .copied()
                .filter(|link| link.hash == was.hash && link.offset != 0)
                .ok_or_else(|| {
                    Error::StaleHave("an object it builds on is no longer in this database".into())
                })?;
            hinted.push(link);
        }
        hinted
    };
    Ok(Anchors {
        generation: store.id(),
        bases,
        roots,
        hinted,
        hint_kinds: Vec::new(),
    })
}

struct Streamed {
    unused: HashSet<Hash>,
    loose: HashSet<Hash>,
    carried: usize,
}

fn stream<R: Read>(
    staging: &mut Staging,
    header: &Header,
    anchors: &Anchors,
    input: &mut Input<R>,
) -> Result<Streamed> {
    let (roots, hinted) = (&anchors.roots, &anchors.hinted);
    let references = 1 + roots.len() + hinted.len();
    let mut found = Streamed {
        unused: HashSet::new(),
        loose: HashSet::new(),
        carried: 0,
    };
    for _ in 0..header.count {
        let (body, packed) = input.body()?;
        let mut links = Vec::new();
        let mut open = false;
        for _ in 0..input.varint()? {
            let hash = input.hash()?;
            let ptr = match input.below(references)? {
                0 => match staging.offset_of(&hash) {
                    Some(offset) => {
                        found.unused.remove(&hash);
                        Ptr { hash, offset }
                    }
                    None => {
                        open = true;
                        Ptr { hash, offset: 0 }
                    }
                },
                reference if reference <= roots.len() => roots[reference - 1],
                reference => hinted[reference - 1 - roots.len()],
            };
            if ptr.hash != hash {
                return Err(bad("a link does not match the object it names"));
            }
            links.push(ptr);
        }
        let object = Object { body, links };
        let hash = object.hash();
        if staging.offset_of(&hash).is_some() {
            return Err(bad("it repeats an object"));
        }
        found.carried = found.carried.saturating_add(object.body.len());
        staging.put(hash, staged(&object.body, packed.as_deref(), &object.links))?;
        found.unused.insert(hash);
        if open {
            found.loose.insert(hash);
        }
    }
    input.finish()?;
    Ok(found)
}

fn place(
    txn: &mut Txn,
    record: &Record,
    placed: &mut HashMap<Hash, u64>,
    held: &HashMap<Hash, Ptr>,
    unlisted: bool,
) -> Result<()> {
    let mut links = Vec::with_capacity(record.links.len());
    for link in &record.links {
        links.push(match placed.get(&link.hash) {
            _ if link.offset == 0 => *link,
            Some(offset) => Ptr {
                hash: link.hash,
                offset: *offset,
            },
            None => *held.get(&link.hash).ok_or_else(|| {
                Error::Corrupt("a received object links to one that was not placed".into())
            })?,
        });
    }
    let staged = record.staged(&links);
    let ptr = match unlisted {
        true => txn.put_unlisted(record.hash, staged)?,
        false => txn.put_record(record.hash, staged)?,
    };
    placed.insert(record.hash, ptr.offset);
    if txn.buffered() >= SPILL_AT {
        txn.spill()?;
    }
    Ok(())
}

struct Vetted<'a> {
    streamed: Streamed,
    root_kind: &'a Kind,
}

struct Resolved {
    refs: Option<Refs>,
    commits: HashSet<Hash>,
}

struct Received {
    staging: Staging,
    anchors: Anchors,
    commits: HashSet<Hash>,
}

fn resolve<O: Objects>(
    store: &Store,
    objects: &mut O,
    refs: &Refs,
    remote: &str,
    header: &Header,
    anchors: &Anchors,
    vetted: Option<Vetted<'_>>,
) -> Result<Resolved> {
    let tip = header.tip.expect("a landing bundle has a head");
    let checking = vetted.is_some();
    let mut commits: HashMap<Hash, Sent> = HashMap::new();
    let mut versions: HashMap<u64, Hash> = HashMap::new();
    let mut below = Some(tip);
    while let Some(mark) = below {
        if objects.offset_of(&mark.hash).is_none() {
            break;
        }
        let entry = read_commit(objects, mark.hash)
            .map_err(|error| bad(format!("v{} is not a commit: {error}", mark.version)))?;
        if mark.version == 0 || entry.commit.version != mark.version {
            return Err(bad("its commit versions do not descend one at a time"));
        }
        below = match entry.prev {
            Some(prev) => Some(Mark {
                hash: prev.hash,
                version: mark.version - 1,
            }),
            None if mark.version == 1 => None,
            None => return Err(bad(format!("v{} has no parent", mark.version))),
        };
        versions.insert(mark.version, mark.hash);
        commits.insert(mark.hash, Sent::of(&entry, 0));
    }
    let landed = commits.len() as u64;
    let low = tip.version.saturating_sub(landed).saturating_add(1);
    let mut prefix = None;
    let mut first = low;
    let mut tip_ptr = None;
    if let Some(mark) = below {
        match locate(store, refs, mark)? {
            Some((ptr, chain)) => {
                first = chain.first;
                prefix = Some(chain);
                if landed == 0 {
                    tip_ptr = Some(ptr);
                }
            }
            None if landed == 0 && header.count == 0 => {
                return Err(Error::StaleHave(format!(
                    "its head is v{} {}, which this database does not have",
                    mark.version,
                    hex(&mark.hash)
                )));
            }
            None if landed == 0 => {
                return Err(bad("its head is neither in it nor in this database"));
            }
            None => {}
        }
    }
    let mut landing = Landing {
        store,
        low,
        segments: vec![Segment {
            versions,
            bottom: low,
            below: prefix.map_or(Below::Gone, Below::Chain),
        }],
        commits,
    };
    let mut pending: Vec<Hash> = landing
        .commits
        .values()
        .filter_map(|sent| sent.merged.map(|link| link.ptr.hash))
        .collect();
    while let Some(top) = pending.pop() {
        if landing.commits.contains_key(&top) || objects.offset_of(&top).is_none() {
            continue;
        }
        pending.extend(landing.segment(refs, objects, top)?);
    }
    let mut unused = HashSet::new();
    if let Some(Vetted {
        streamed,
        root_kind,
    }) = vetted
    {
        let now = now_ms();
        let horizon = now.saturating_add(MAX_AHEAD_MS);
        if let Some(sent) = landing
            .commits
            .values()
            .find(|sent| sent.facts.time > horizon)
        {
            return Err(bad(format!(
                "v{} is dated {} ms past this machine's clock, more than the {MAX_AHEAD_MS} it allows",
                sent.facts.version,
                sent.facts.time - now
            )));
        }
        let unresolved = streamed
            .loose
            .iter()
            .any(|hash| !landing.commits.contains_key(hash))
            || landing
                .commits
                .values()
                .any(|sent| sent.root.is_some_and(|root| root.offset == 0));
        if unresolved {
            return Err(bad(
                "an object links to one that was neither sent nor known",
            ));
        }
        let mut held_at: HashMap<Hash, usize> = HashMap::new();
        for (index, ptr) in anchors.roots.iter().chain(&anchors.hinted).enumerate() {
            held_at.entry(ptr.hash).or_insert(1 + index);
        }
        let mut vetting = Vetting {
            store,
            objects: &*objects,
            held_at: &held_at,
            roots: &anchors.roots,
            hinted: &anchors.hinted,
            hint_kinds: &anchors.hint_kinds,
            root_kind,
            sent: Checked::default(),
            held: Checked::default(),
            spent: 0,
            budget: streamed
                .carried
                .saturating_mul(2)
                .saturating_add(RECHECK_SLACK),
            queue: Vec::new(),
        };
        for sent in landing.commits.values() {
            if let Some(root) = sent.root {
                vetting.queue.push((root, root_kind.clone()));
            }
        }
        vetting.run()?;
        drop(vetting);
        unused = streamed.unused;
    }
    let based: HashMap<Hash, Ptr> = anchors
        .bases
        .iter()
        .map(|(ptr, _)| (ptr.hash, *ptr))
        .collect();
    let mut order: Vec<(u64, u64, Hash)> = landing
        .commits
        .iter()
        .map(|(hash, sent)| (sent.facts.generation, sent.facts.version, *hash))
        .collect();
    order.sort_unstable();
    let mut tip_root = None;
    for (_, _, hash) in order {
        let object = objects.read_object(sent_ptr(objects, hash))?;
        let sent = &landing.commits[&hash];
        let version = sent.facts.version;
        let mut links = Vec::with_capacity(object.links.len());
        links.extend(sent.root);
        let mut prev_facts = None;
        if let Some(prev) = sent.prev {
            let ptr = landing.chained(objects, &mut unused, sent.segment, prev, version - 1)?;
            prev_facts = Some(landing.facts(ptr)?);
            links.push(ptr);
        }
        if let Some(skip) = sent.skip {
            let at = skip_version(version);
            links.push(landing.chained(objects, &mut unused, sent.segment, skip, at)?);
        }
        let mut merged_facts = None;
        if let Some(link) = sent.merged {
            let merged = link.ptr.hash;
            let ptr = if landing.commits.contains_key(&merged) {
                unused.remove(&merged);
                sent_ptr(objects, merged)
            } else {
                based.get(&merged).copied().unwrap_or(Ptr {
                    hash: merged,
                    offset: 0,
                })
            };
            merged_facts = Some(landing.facts(ptr)?);
            links.push(ptr);
        }
        check_graph(sent.facts, sent.merged, prev_facts, merged_facts)?;
        if hash == tip.hash {
            tip_root = sent.root;
        }
        if links != object.links {
            objects.relink(
                Object {
                    body: object.body,
                    links,
                },
                hash,
            )?;
        }
    }
    unused.remove(&tip.hash);
    if checking && !unused.is_empty() {
        return Err(bad("it carries objects nothing links to"));
    }
    let tip_ptr = match tip_ptr {
        Some(ptr) => ptr,
        None => sent_ptr(objects, tip.hash),
    };
    let root = if landing.commits.contains_key(&tip.hash) {
        tip_root
    } else {
        Entry::load(store, tip_ptr)?.commit.root
    };
    let head = Head {
        version: tip.version,
        root,
        log: Chain {
            tip: Some(tip_ptr),
            first,
            last: tip.version,
        },
        merged: Map::new(),
    };
    let existing = refs.try_get(remote)?;
    if let Some(old) = &existing
        && (old.log == head.log || hash_at(store, &old.log, tip.version)? == Some(tip.hash))
    {
        return Ok(Resolved {
            refs: None,
            commits: landing.commits.into_keys().collect(),
        });
    }
    let mut next = refs.clone();
    let continues = match existing
        .as_ref()
        .and_then(|old| old.log.tip.map(|ptr| (ptr.hash, old.log.last)))
    {
        None => false,
        Some((hash, version)) => match landing.find(0, version) {
            Ok(Found::Sent(found)) => found == hash,
            Ok(Found::Held(ptr)) => ptr.hash == hash,
            _ => false,
        },
    };
    if !continues {
        for (name, mut head) in next.try_entries()? {
            if head.merged.try_get(remote)?.is_some() {
                head.merged.remove(remote);
                next.insert(name, head);
            }
        }
    }
    next.insert(remote.to_string(), head);
    Ok(Resolved {
        refs: Some(next),
        commits: landing.commits.into_keys().collect(),
    })
}

fn hex(hash: &Hash) -> String {
    hash.iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    UpToDate,
    Equal,
    Ahead,
    FastForward,
    Diverged,
}

fn decide(store: &Store, ours: Option<&Head>, theirs: &Head) -> Result<Decision> {
    let Some(their_tip) = theirs.log.tip else {
        return Ok(Decision::UpToDate);
    };
    let Some(ours) = ours.filter(|ours| ours.log.tip.is_some()) else {
        return Ok(Decision::FastForward);
    };
    let our_tip = ours.log.tip.expect("filtered on a tip");
    if our_tip.hash == their_tip.hash {
        return Ok(Decision::UpToDate);
    }
    if hash_at(store, &theirs.log, ours.log.last)? == Some(our_tip.hash) {
        return Ok(Decision::FastForward);
    }
    if hash_at(store, &ours.log, theirs.log.last)? == Some(their_tip.hash) {
        return Ok(Decision::Ahead);
    }
    if ours.root.map(|ptr| ptr.hash) == theirs.root.map(|ptr| ptr.hash) {
        return Ok(Decision::Equal);
    }
    Ok(Decision::Diverged)
}

fn fast_forward(ours: Option<&Head>, theirs: &Head) -> Head {
    let first = match ours.filter(|ours| ours.log.tip.is_some()) {
        Some(ours) => ours.log.first.max(theirs.log.first),
        None => theirs.log.first,
    };
    Head {
        version: theirs.version,
        root: theirs.root,
        log: Chain {
            first,
            ..theirs.log
        },
        merged: ours.map(|ours| ours.merged.clone()).unwrap_or_default(),
    }
}

fn check_remote(remote: &str, branch: &str) -> Result<()> {
    if remote == MAIN || remote == branch {
        return Err(Error::BranchInUse(remote.to_string()));
    }
    Ok(())
}

impl<T: Data> Db<T> {
    pub fn have(&self) -> Result<Have> {
        let (store, refs) = self.published_refs();
        let mut marks = Vec::new();
        for (_, head) in refs.try_entries()? {
            if let Some(tip) = head.log.tip {
                marks.push(Mark {
                    hash: tip.hash,
                    version: head.log.last,
                });
            }
        }
        let tips = marks.len();
        if let Some(head) = refs.try_get(self.branch_name())?
            && head.log.tip.is_some()
        {
            let chain = head.log;
            let (mut version, mut step) = (chain.last, 1u64);
            while version > chain.first {
                version = version.saturating_sub(step).max(chain.first);
                if let Some(ptr) = entry_at(&store, &chain, version)? {
                    marks.push(Mark {
                        hash: ptr.hash,
                        version,
                    });
                }
                step = step.saturating_mul(2);
            }
        }
        Ok(Have(Marks { marks, tips }.encode()))
    }

    pub fn bundle(&self, have: &Have) -> Result<Bundle> {
        let mut out = Vec::new();
        self.bundle_to(have, &mut out)?;
        Ok(Bundle(out))
    }

    pub fn bundle_to(&self, have: &Have, mut out: impl Write) -> Result<u64> {
        self.emitter(have)?.write_to(&mut out)
    }

    fn emitter(&self, have: &Have) -> Result<Emitter> {
        let have = Marks::decode(have.as_bytes())?;
        let (store, refs) = self.published_refs();
        plan(&store, &refs, self.branch_name(), &have)
    }

    pub fn pull<R, S: Into<String>>(
        &self,
        from: &Db<T>,
        remote: &str,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Pulled<R>> {
        self.try_pull::<R, S, Error>(from, remote, infallible(merge))
    }

    pub fn try_pull<R, S: Into<String>, E: From<Error>>(
        &self,
        from: &Db<T>,
        remote: &str,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Pulled<R>, E> {
        check_remote(remote, self.branch_name())?;
        drop(self.entering(Needs::Every)?);
        let have = self.have()?;
        let emitter = {
            let _theirs = from.entering(Needs::Tip)?;
            from.emitter(&have)?
        };
        self.step("bundled");
        let mut pulled = if emitter.order.len() < PIPE_OBJECTS {
            self.try_receive_from(remote, emitter, merge)?
        } else {
            let (sender, pipe) = mpsc::sync_channel(PIPE_CHUNKS);
            thread::scope(|scope| {
                scope.spawn(move || emitter.send(sender));
                let piped = Piped {
                    pipe,
                    chunk: Vec::new(),
                    pos: 0,
                };
                self.try_receive_from(remote, piped, merge)
            })?
        };
        pulled.sent_bytes = have.as_bytes().len() as u64;
        Ok(pulled)
    }

    pub fn push<R, S: Into<String>>(
        &self,
        to: &Db<T>,
        remote: &str,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Pulled<R>> {
        to.pull(self, remote, merge)
    }

    pub fn try_push<R, S: Into<String>, E: From<Error>>(
        &self,
        to: &Db<T>,
        remote: &str,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Pulled<R>, E> {
        to.try_pull(self, remote, merge)
    }

    pub fn receive<R, S: Into<String>>(
        &self,
        remote: &str,
        bundle: &Bundle,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Pulled<R>> {
        self.try_receive::<R, S, Error>(remote, bundle, infallible(merge))
    }

    pub fn try_receive<R, S: Into<String>, E: From<Error>>(
        &self,
        remote: &str,
        bundle: &Bundle,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Pulled<R>, E> {
        self.try_receive_from(remote, bundle.as_bytes(), merge)
    }

    pub fn receive_from<R, S: Into<String>>(
        &self,
        remote: &str,
        input: impl Read,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Pulled<R>> {
        self.try_receive_from::<R, S, Error>(remote, input, infallible(merge))
    }

    pub fn try_receive_from<R, S: Into<String>, E: From<Error>>(
        &self,
        remote: &str,
        input: impl Read,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Pulled<R>, E> {
        check_remote(remote, self.branch_name())?;
        drop(self.entering(Needs::Every)?);
        let mut input = Input::open(input, BUNDLE_MAGIC)?;
        let header = Header::decode(&mut input)?;
        let received = match header.tip {
            Some(_) => Some(self.stage_bundle(remote, &header, &mut input)?),
            None => {
                input.finish()?;
                None
            }
        };
        let received_bytes = input.read;
        drop(input);
        let (outcome, value) = match received {
            Some(received) => {
                let _ours = self.entering(Needs::Every)?;
                self.land_staged(remote, &header, received)?;
                self.integrate(remote, merge)?
            }
            None => (Outcome::UpToDate, None),
        };
        Ok(Pulled {
            outcome,
            value,
            version: self.version(),
            objects: header.count,
            sent_bytes: 0,
            received_bytes,
        })
    }

    fn stage_bundle<R: Read>(
        &self,
        remote: &str,
        header: &Header,
        input: &mut Input<R>,
    ) -> Result<Received> {
        let tip = header.tip.expect("a landing bundle has a head");
        if tip.version > MAX_VERSION {
            return Err(bad(format!(
                "its head is at v{}, past what a commit log can reach",
                tip.version
            )));
        }
        self.check_writable()?;
        let (store, refs) = self.published_refs();
        let root_kind = Kind::value::<T>();
        let anchors = anchors(&store, &refs, header, &root_kind)?;
        let mut staging = Staging::new(store.path(), SPILL_AT);
        let streamed = stream(&mut staging, header, &anchors, input)?;
        let vetted = Vetted {
            streamed,
            root_kind: &root_kind,
        };
        let sent = resolve(
            &store,
            &mut staging,
            &refs,
            remote,
            header,
            &anchors,
            Some(vetted),
        )?;
        staging.seal()?;
        self.step("received");
        Ok(Received {
            staging,
            anchors,
            commits: sent.commits,
        })
    }

    fn land_staged(&self, remote: &str, header: &Header, received: Received) -> Result<()> {
        let Received {
            staging,
            mut anchors,
            commits,
        } = received;
        let spilled = staging.spilled();
        let _cycles = spilled.then(|| self.hold_cycles()).transpose()?;
        let mut placed = HashMap::new();
        if spilled {
            let (store, refs) = self.published_refs();
            anchors = reanchor(&store, &refs, header, &anchors)?;
            drop((store, refs));
            placed = self.place_staged(&staging, &anchors, &commits)?;
            self.published_refs().0.flush()?;
            self.step("placed");
        }
        self.rewrite_refs(|store, txn, refs| {
            let anchors = reanchor(store, refs, header, &anchors)?;
            if !spilled {
                let held = anchors.held();
                let mut replay = staging.replay();
                while let Some(record) = replay.next_record()? {
                    place(txn, &record, &mut placed, &held, false)?;
                }
            }
            let mut objects = Placed {
                txn,
                earlier: &placed,
            };
            let resolved = resolve(store, &mut objects, refs, remote, header, &anchors, None)?;
            Ok((resolved.refs, ()))
        })
    }

    fn place_staged(
        &self,
        staging: &Staging,
        anchors: &Anchors,
        commits: &HashSet<Hash>,
    ) -> Result<HashMap<Hash, u64>> {
        let held = anchors.held();
        let mut placed: HashMap<Hash, u64> = HashMap::with_capacity(staging.len());
        let mut replay = staging.replay();
        let mut batch = Vec::new();
        loop {
            let mut bytes = 0;
            while bytes < PLACE_AT {
                let Some(record) = replay.next_record()? else {
                    break;
                };
                bytes += record.payload.len() + record.links.len() * (HASH_BYTES + 2);
                batch.push(record);
            }
            if batch.is_empty() {
                return Ok(placed);
            }
            self.append_frame(|store, txn| {
                if store.id() != anchors.generation {
                    return Err(Error::Corrupt(
                        "the file was replaced while a bundle was placed in it".into(),
                    ));
                }
                for record in &batch {
                    let unlisted = commits.contains(&record.hash);
                    place(txn, record, &mut placed, &held, unlisted)?;
                }
                Ok(())
            })?;
            batch.clear();
        }
    }

    fn integrate<R, S: Into<String>, E: From<Error>>(
        &self,
        remote: &str,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<(Outcome, Option<R>), E> {
        let name = self.branch_name().to_string();
        let decision = self.rewrite_refs(|store, _, refs| {
            let theirs = refs
                .try_get(remote)?
                .ok_or_else(|| Error::NoBranch(remote.to_string()))?;
            let ours = refs.try_get(&name)?;
            let decision = decide(store, ours.as_deref(), &theirs)?;
            let head = match decision {
                Decision::Diverged | Decision::Equal => return Ok((None, decision)),
                Decision::FastForward => fast_forward(ours.as_deref(), &theirs),
                Decision::UpToDate | Decision::Ahead => match ours {
                    Some(ours) => (*ours).clone(),
                    None => return Ok((None, decision)),
                },
            };
            Ok((
                recorded(refs, &name, head, remote, theirs.version)?,
                decision,
            ))
        })?;
        let outcome = match decision {
            Decision::UpToDate | Decision::Equal => Outcome::UpToDate,
            Decision::Ahead => Outcome::Ahead,
            Decision::FastForward => Outcome::FastForward,
            Decision::Diverged => return self.merge_remote(remote, merge),
        };
        Ok((outcome, None))
    }

    fn merge_remote<R, S: Into<String>, E: From<Error>>(
        &self,
        remote: &str,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<(Outcome, Option<R>), E> {
        let tracking = self.branch(remote)?;
        let seen = match self.merge_tracking(&tracking, merge)? {
            Ok(committed) => return Ok((Outcome::Merged, Some(committed.value))),
            Err(seen) => seen,
        };
        let name = self.branch_name().to_string();
        self.rewrite_refs(|_, _, refs| {
            let Some(ours) = refs.try_get(&name)? else {
                return Ok((None, ()));
            };
            Ok((recorded(refs, &name, (*ours).clone(), remote, seen)?, ()))
        })?;
        Ok((Outcome::Ahead, None))
    }
}

fn infallible<T, R, S>(
    mut merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
) -> impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> Result<(S, R)> {
    move |ours, base, theirs| Ok(merge(ours, base, theirs))
}

fn recorded(
    refs: &Refs,
    name: &str,
    mut head: Head,
    remote: &str,
    theirs: u64,
) -> Result<Option<Refs>> {
    let record = (theirs, head.version);
    if let Some(current) = refs.try_get(name)?
        && current.log == head.log
        && current.merged.try_get(remote)?.as_deref() == Some(&record)
    {
        return Ok(None);
    }
    head.merged.insert(remote.to_string(), record);
    let mut next = refs.clone();
    next.insert(name.to_string(), head);
    Ok(Some(next))
}
