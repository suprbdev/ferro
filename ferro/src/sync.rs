//! Sync between files: exchanging commits, so that each device works offline and catches up
//! later.
//!
//! Two files that share a history, or have none yet, exchange only the commits the other lacks.
//! [`Db::pull`](crate::Db::pull) does it in one call between two open files. Over a network the
//! same pull is three calls on bytes: the pulling side sends its [`Have`], the other answers with a
//! [`Bundle`], and the pulling side lands it with [`Db::receive`](crate::Db::receive). Either way
//! the other side's branch lands in this file as a remote branch, and this branch is brought up to
//! it by a fast-forward or a merge, as the [`Outcome`] in the returned [`Pulled`] says. Every
//! object received is checked by hash, and as the type that will read it, before any of it can be
//! reached from the file. See [Sync between files](docs/guide/sync.md).

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::mem;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use crate::codec::{
    Checked, Data, Decoder, HASH_BYTES, Hash, Kind, Object, Ptr, Shape, push_varint,
};
use crate::db::{Db, Head, Refs, Snapshot, now_ms};
use crate::dense::{Column, Hashes, Index};
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
const MOVED_BUDGET: usize = 1 << 16;
const UNRESOLVED: Ptr = Ptr {
    hash: [0; HASH_BYTES],
    offset: 0,
};

/// What a file holds of the commit graph, sent to another file so that it can bundle what this
/// one lacks.
///
/// [`Db::have`](crate::Db::have) makes one, a few hundred bytes. It crosses a network as bytes:
/// [`Have::as_bytes`] or `Vec::from` on one side, `Have::from` on the other. Bytes that are not a
/// have are refused when [`Db::bundle`](crate::Db::bundle) reads them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Have(Vec<u8>);

impl Have {
    /// The have's bytes, to send to the other file.
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

/// The commits a file lacks, and the objects they reach that it does not hold, built for it by
/// another file.
///
/// [`Db::bundle`](crate::Db::bundle) builds one from the receiver's [`Have`], and
/// [`Db::receive`](crate::Db::receive) lands it. It crosses a network as bytes, as a [`Have`]
/// does. [`Db::bundle_to`](crate::Db::bundle_to) and [`Db::receive_from`](crate::Db::receive_from)
/// stream it instead, so that neither side holds it whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle(Vec<u8>);

impl Bundle {
    /// The bundle's bytes, to send to the receiving file.
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

/// How a pull or receive brought this branch up to the remote one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// This branch already had the remote head or a value equal to it, or the remote branch has
    /// no commits. This branch did not change.
    UpToDate,
    /// This branch already holds everything the remote branch has, and more of its own. This
    /// branch did not change.
    Ahead,
    /// This branch had nothing the remote branch lacks, so it now points at the remote head, and
    /// [`Db::head_hash`](crate::Db::head_hash) is the same in both files.
    FastForward,
    /// Each side had commits the other lacked, so the merge closure ran, and its merge commit is
    /// this branch's new head.
    Merged,
}

/// What a pull or receive did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pulled<R> {
    /// How this branch was brought up to the remote one.
    pub outcome: Outcome,
    /// What the merge closure returned, if the outcome is [`Outcome::Merged`].
    pub value: Option<R>,
    /// This branch's version afterwards.
    pub version: u64,
    /// The number of objects the bundle carried.
    pub objects: u64,
    /// The bytes of the [`Have`] sent to the other side, which only a pull sends.
    pub sent_bytes: u64,
    /// The bytes of the bundle read.
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
            if roots.len() + hints.len() >= u32::MAX as usize {
                return Err(bad("it names more hints than one receive can hold"));
            }
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
    hashes: Column<Hash>,
    offsets: Column<u64>,
    order: Vec<u32>,
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
        sending: Sendable,
        order: Vec<u32>,
        references: HashMap<Hash, usize>,
    ) -> Emitter {
        Emitter {
            store: store.clone(),
            header: header.encode(),
            hashes: sending.hashes.into_column(),
            offsets: sending.offsets,
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
            let id = self.order[self.next];
            let ptr = Ptr {
                hash: self.hashes.get(id),
                offset: self.offsets.get(id),
            };
            let (object, packed) = self.store.read_packed(ptr)?;
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
    Root(u32),
    Child(u32, u32),
}

fn too_many() -> Error {
    bad("it would carry more objects than one receive can hold")
}

struct Sendable {
    hashes: Hashes,
    offsets: Column<u64>,
    bare: Column<bool>,
}

impl Sendable {
    fn new() -> Sendable {
        Sendable {
            hashes: Hashes::new(),
            offsets: Column::new(),
            bare: Column::new(),
        }
    }

    fn len(&self) -> usize {
        self.offsets.len()
    }

    fn find(&self, hash: &Hash) -> Option<u32> {
        self.hashes.find(hash)
    }

    fn insert(&mut self, ptr: Ptr, bare: bool) -> Result<u32> {
        let id = self.hashes.insert(ptr.hash).ok_or_else(too_many)?;
        self.offsets.push(ptr.offset);
        self.bare.push(bare);
        Ok(id)
    }
}

struct Known {
    hashes: Hashes,
    origins: Column<Origin>,
}

impl Known {
    fn find(&self, hash: &Hash) -> Option<u32> {
        self.hashes.find(hash)
    }

    fn insert(&mut self, hash: Hash, origin: Origin) -> Result<u32> {
        let id = self.hashes.insert(hash).ok_or_else(too_many)?;
        self.origins.push(origin);
        Ok(id)
    }

    fn child(&self, hash: &Hash) -> Option<u32> {
        self.find(hash)
            .filter(|id| matches!(self.origins.get(*id), Origin::Child(..)))
    }
}

struct Difference {
    known: Known,
    send: Sendable,
    links: HashMap<Hash, Vec<Ptr>>,
    cached: usize,
    moved: usize,
}

fn difference(
    store: &Store,
    old_roots: &[Ptr],
    new_roots: impl IntoIterator<Item = Ptr>,
) -> Result<Difference> {
    let mut found = Difference {
        known: Known {
            hashes: Hashes::new(),
            origins: Column::new(),
        },
        send: Sendable::new(),
        links: HashMap::new(),
        cached: 0,
        moved: MOVED_BUDGET,
    };
    for (slot, root) in old_roots.iter().enumerate() {
        if found.known.find(&root.hash).is_none() {
            found.known.insert(root.hash, Origin::Root(slot as u32))?;
        }
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
        self.known.find(hash).is_some() || self.send.find(hash).is_some()
    }

    fn step(&mut self, store: &Store, mut old: Vec<Ptr>, root: Ptr) -> Result<()> {
        let mut seen = HashSet::from([root.hash]);
        let mut expanded = HashSet::new();
        let mut fresh = vec![root];
        let mut moved: Vec<Ptr> = Vec::new();
        loop {
            fresh.retain(|ptr| !self.classified(&ptr.hash));
            if fresh.is_empty() {
                return Ok(());
            }
            if old.is_empty() {
                moved.reverse();
                while self.moved > 0
                    && let Some(ptr) = moved.pop()
                {
                    if !seen.contains(&ptr.hash) && expanded.insert(ptr.hash) {
                        self.expand(store, ptr, &mut moved, true)?;
                    }
                }
                return self.reach(store, fresh);
            }
            old.sort_unstable_by_key(|ptr| ptr.hash);
            old.dedup_by_key(|ptr| ptr.hash);
            let mut linked = vec![false; old.len()];
            let mut starts = Vec::with_capacity(fresh.len() + 1);
            let mut next = Vec::new();
            for ptr in &fresh {
                starts.push(next.len());
                if ptr.offset == 0 {
                    continue;
                }
                for link in store.read_links(*ptr)? {
                    if let Ok(at) = old.binary_search_by_key(&link.hash, |ptr| ptr.hash) {
                        linked[at] = true;
                    }
                    next.push(link);
                }
            }
            starts.push(next.len());
            let mut deeper = Vec::new();
            for (at, ptr) in old.drain(..).enumerate() {
                if linked[at] {
                    moved.push(ptr);
                } else if !seen.contains(&ptr.hash) && expanded.insert(ptr.hash) {
                    self.expand(store, ptr, &mut deeper, false)?;
                }
            }
            let mut below = Vec::new();
            for ptr in mem::take(&mut moved) {
                if self.moved == 0 {
                    break;
                }
                if !seen.contains(&ptr.hash) && expanded.insert(ptr.hash) {
                    self.expand(store, ptr, &mut below, true)?;
                }
            }
            moved = below;
            old = deeper;
            let tracking = !old.is_empty();
            let mut kept = 0;
            for (at, ptr) in fresh.drain(..).enumerate() {
                let (start, end) = (starts[at], starts[at + 1]);
                if self.classified(&ptr.hash) {
                    continue;
                }
                self.send_object(ptr, &next[start..end])?;
                for index in start..end {
                    let link = next[index];
                    if !tracking || seen.insert(link.hash) {
                        next[kept] = link;
                        kept += 1;
                    }
                }
            }
            next.truncate(kept);
            fresh = next;
        }
    }

    fn expand(&mut self, store: &Store, ptr: Ptr, out: &mut Vec<Ptr>, moved: bool) -> Result<()> {
        let links = match self.links.get(&ptr.hash) {
            Some(links) => links.clone(),
            None => store.read_links(ptr)?,
        };
        let mut parent = None;
        for (index, link) in links.into_iter().enumerate() {
            if link.offset == 0 {
                continue;
            }
            if moved {
                if self.moved == 0 {
                    break;
                }
                self.moved -= 1;
            }
            if !self.classified(&link.hash) {
                let parent = match parent {
                    Some(parent) => parent,
                    None => *parent.insert(self.known.find(&ptr.hash).ok_or_else(|| {
                        Error::Corrupt("sync expanded an object it had not classified".into())
                    })?),
                };
                self.known
                    .insert(link.hash, Origin::Child(parent, index as u32))?;
            }
            out.push(link);
        }
        Ok(())
    }

    fn reach(&mut self, store: &Store, mut stack: Vec<Ptr>) -> Result<()> {
        while let Some(ptr) = stack.pop() {
            if self.classified(&ptr.hash) {
                continue;
            }
            let links = match ptr.offset {
                0 => Vec::new(),
                _ => store.read_links(ptr)?,
            };
            self.send_object(ptr, &links)?;
            stack.extend(links);
        }
        Ok(())
    }

    fn send_object(&mut self, ptr: Ptr, links: &[Ptr]) -> Result<()> {
        if ptr.offset == 0 {
            return Err(bad(
                "the branch links to pruned history outside its commit log",
            ));
        }
        self.send.insert(ptr, links.is_empty())?;
        if !links.is_empty() && self.cached + 1 + links.len() <= CACHED_LINKS {
            self.cached += 1 + links.len();
            self.links.insert(ptr.hash, links.to_vec());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    Waiting,
    Open,
    Done,
}

struct Ordered {
    order: Vec<u32>,
    ids: HashMap<u32, usize>,
    hints: Vec<(usize, usize)>,
}

fn post_order(
    store: &Store,
    starts: impl IntoIterator<Item = Ptr>,
    send: &Sendable,
    mut cached: HashMap<Hash, Vec<Ptr>>,
    old: &Known,
    anchors: usize,
) -> Result<Ordered> {
    let mut links_of = |id: u32, ptr: Ptr| {
        if send.bare.get(id) {
            return Ok(Vec::new());
        }
        match cached.remove(&ptr.hash) {
            Some(links) => Ok(links),
            None => store.read_links(ptr),
        }
    };
    let mut state = Column::new();
    for _ in 0..send.len() {
        state.push(Visit::Waiting);
    }
    let mut found = Ordered {
        order: Vec::with_capacity(send.len()),
        ids: HashMap::new(),
        hints: Vec::new(),
    };
    for start in starts {
        let Some(id) = send.find(&start.hash) else {
            continue;
        };
        if state.get(id) != Visit::Waiting {
            continue;
        }
        state.set(id, Visit::Open);
        let mut stack = vec![(id, links_of(id, start)?, 0usize)];
        while let Some((_, links, next)) = stack.last_mut() {
            if let Some(link) = links.get(*next).copied() {
                *next += 1;
                if link.offset != 0
                    && let Some(id) = send.find(&link.hash)
                    && state.get(id) == Visit::Waiting
                {
                    state.set(id, Visit::Open);
                    stack.push((id, links_of(id, link)?, 0));
                }
                continue;
            }
            let (id, links, _) = stack.pop().expect("the stack has a top");
            for link in &links {
                if send.find(&link.hash).is_none()
                    && let Some(child) = old.child(&link.hash)
                {
                    hint_for(child, old, &mut found.ids, &mut found.hints, anchors);
                }
            }
            state.set(id, Visit::Done);
            found.order.push(id);
        }
    }
    Ok(found)
}

struct Recognised {
    mark: Mark,
    ptr: Ptr,
    chain: Chain,
    tip: bool,
}

struct Sending {
    ptr: Ptr,
    root: Option<Ptr>,
    generation: u64,
}

impl Sending {
    fn of(ptr: Ptr, entry: &Entry) -> Sending {
        Sending {
            ptr,
            root: entry.commit.root,
            generation: entry.generation(),
        }
    }
}

fn plan(store: &Store, refs: &Refs, name: &str, have: &Marks) -> Result<Emitter> {
    let mut header = Header::default();
    let chain = refs.try_get(name)?.map(|head| head.log);
    let Some(chain) = chain.filter(|chain| chain.tip.is_some()) else {
        return Ok(Emitter::new(
            store,
            &header,
            Sendable::new(),
            Vec::new(),
            HashMap::new(),
        ));
    };
    let tip = chain.tip.expect("chain has a tip");
    let mut known = Vec::new();
    for (index, mark) in have.marks.iter().enumerate() {
        if let Some((ptr, base)) = locate(store, refs, *mark)? {
            known.push(Recognised {
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
    let mut entries = Vec::with_capacity((chain.last - floor) as usize);
    let mut merges = Vec::new();
    let mut next = Some(tip);
    while let Some(ptr) = next {
        let entry = read_entry(&store.read_object(ptr)?)?;
        let at = entry.commit.version;
        if at <= floor {
            break;
        }
        next = entry.prev.filter(|_| at > chain.first);
        merges.extend(entry.merged);
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
    let extra = closures(store, &chain, &entries, merges, &known, &mut header.bases)?;
    let new_roots = entries
        .iter()
        .rev()
        .chain(&extra)
        .filter_map(|sending| sending.root);
    let Difference {
        known: old,
        mut send,
        links,
        ..
    } = difference(store, &old_roots, new_roots)?;
    entries.extend(extra);
    for sending in &entries {
        if send.find(&sending.ptr.hash).is_none() {
            send.insert(sending.ptr, false)?;
        }
    }
    entries.sort_by_key(|sending| (sending.generation, sending.ptr.offset));
    let starts = entries.iter().map(|sending| sending.ptr);
    let Ordered { order, ids, hints } =
        post_order(store, starts, &send, links, &old, old_roots.len())?;
    drop(entries);
    if order.len() != send.len() {
        return Err(Error::Corrupt(
            "sync found objects its head does not reach".into(),
        ));
    }
    header.hints = hints;
    header.count = order.len() as u64;
    let mut references = HashMap::with_capacity(old_roots.len() + ids.len());
    for (slot, root) in old_roots.iter().enumerate() {
        references.entry(root.hash).or_insert(1 + slot);
    }
    for (id, hint) in ids {
        references.insert(old.hashes.hash(id), 1 + old_roots.len() + hint);
    }
    drop(old);
    Ok(Emitter::new(store, &header, send, order, references))
}

fn closures(
    store: &Store,
    chain: &Chain,
    entries: &[Sending],
    merges: Vec<Merged>,
    known: &[Recognised],
    bases: &mut Vec<Mark>,
) -> Result<Vec<Sending>> {
    let after = chain.after(store)?;
    let mut extra = Vec::new();
    if merges.is_empty() {
        return Ok(extra);
    }
    let mut sending: HashSet<Hash> = entries.iter().map(|sending| sending.ptr.hash).collect();
    let mut seeds: Vec<(Merged, Option<u64>)> =
        merges.into_iter().map(|merged| (merged, after)).collect();
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
    child: u32,
    old: &Known,
    ids: &mut HashMap<u32, usize>,
    hints: &mut Vec<(usize, usize)>,
    roots: usize,
) {
    let mut path = Vec::new();
    let mut at = child;
    let parent = loop {
        if let Some(id) = ids.get(&at) {
            break roots + id;
        }
        match old.origins.get(at) {
            Origin::Root(slot) => break slot as usize,
            Origin::Child(parent, index) => {
                path.push((at, index as usize));
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
    Joins(u32),
    Chain(Chain),
    Gone,
}

struct Segment {
    versions: Vec<u32>,
    bottom: u64,
    below: Below,
}

enum Found {
    Sent(u32),
    Held(Ptr),
    Gone,
}

#[derive(Clone, Copy)]
struct Sent {
    id: u32,
    segment: u32,
    facts: Facts,
    root: Option<Ptr>,
}

impl Sent {
    fn of(id: u32, entry: &Entry, segment: u32) -> Sent {
        Sent {
            id,
            segment,
            facts: Facts::of(entry),
            root: entry.commit.root,
        }
    }
}

struct Commits {
    index: HashMap<u32, u32>,
    sent: Column<Sent>,
}

impl Commits {
    fn new() -> Commits {
        Commits {
            index: HashMap::new(),
            sent: Column::new(),
        }
    }

    fn len(&self) -> usize {
        self.sent.len()
    }

    fn insert(&mut self, sent: Sent) {
        self.index.insert(sent.id, self.sent.len() as u32);
        self.sent.push(sent);
    }

    fn get(&self, id: u32) -> Option<Sent> {
        self.index.get(&id).map(|at| self.sent.get(*at))
    }

    fn contains(&self, id: u32) -> bool {
        self.index.contains_key(&id)
    }

    fn iter(&self) -> impl Iterator<Item = Sent> + '_ {
        self.sent.iter()
    }

    fn ids(&self) -> Vec<u32> {
        let mut ids = Vec::with_capacity(self.len());
        ids.extend(self.iter().map(|sent| sent.id));
        ids.sort_unstable();
        ids
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
    fn relink(&self, object: Object, hash: Hash) -> Result<()>;
    fn hashes(&self) -> &Hashes;

    fn staging(&self) -> Option<&Staging> {
        None
    }
}

impl Objects for Staging {
    fn offset_of(&self, hash: &Hash) -> Option<u64> {
        Staging::offset_of(self, hash)
    }

    fn read_object(&self, ptr: Ptr) -> Result<Object> {
        Staging::read_object(self, ptr)
    }

    fn relink(&self, _: Object, _: Hash) -> Result<()> {
        Ok(())
    }

    fn hashes(&self) -> &Hashes {
        Staging::hashes(self)
    }

    fn staging(&self) -> Option<&Staging> {
        Some(self)
    }
}

struct Placed<'a> {
    txn: RefCell<&'a mut Txn>,
    earlier: Earlier<'a>,
}

#[derive(Clone, Copy)]
struct Earlier<'a> {
    hashes: &'a Hashes,
    placed: &'a Column<u64>,
}

impl Earlier<'_> {
    fn offset_of(&self, hash: &Hash) -> Option<u64> {
        let id = self.hashes.find(hash)?;
        ((id as usize) < self.placed.len()).then(|| self.placed.get(id))
    }
}

impl Objects for Placed<'_> {
    fn offset_of(&self, hash: &Hash) -> Option<u64> {
        self.txn
            .borrow()
            .offset_of(hash)
            .or_else(|| self.earlier.offset_of(hash))
    }

    fn read_object(&self, ptr: Ptr) -> Result<Object> {
        self.txn.borrow().read_object(ptr)
    }

    fn relink(&self, object: Object, hash: Hash) -> Result<()> {
        let mut txn = self.txn.borrow_mut();
        txn.put_fresh(object, hash)?;
        if txn.buffered() >= SPILL_AT {
            txn.spill()?;
        }
        Ok(())
    }

    fn hashes(&self) -> &Hashes {
        self.earlier.hashes
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

fn mark_linked(objects: &impl Objects, linked: &mut Option<Column<u8>>, hash: &Hash) {
    if let (Some(links), Some(id)) = (linked.as_mut(), objects.hashes().find(hash)) {
        links.set(id, links.get(id).max(1));
    }
}

fn staged_id(objects: &impl Objects, hash: &Hash) -> u32 {
    objects
        .hashes()
        .find(hash)
        .expect("a sent object is staged")
}

struct Landing<'a> {
    store: &'a Store,
    hashes: &'a Hashes,
    low: u64,
    segments: Vec<Segment>,
    commits: Commits,
}

impl Landing<'_> {
    fn sent(&self, hash: &Hash) -> Option<Sent> {
        self.commits.get(self.hashes.find(hash)?)
    }

    fn joined(&self, id: u32) -> usize {
        self.commits
            .get(id)
            .expect("a chain joins a commit it walked")
            .segment as usize
    }

    fn find(&self, mut segment: usize, version: u64) -> Result<Found> {
        loop {
            let seg = &self.segments[segment];
            if version >= seg.bottom {
                return usize::try_from(version - seg.bottom)
                    .ok()
                    .and_then(|at| seg.versions.get(at))
                    .map(|id| Found::Sent(*id))
                    .ok_or_else(|| bad("a commit links past the top of its chain"));
            }
            match &seg.below {
                Below::Joins(id) => segment = self.joined(*id),
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
        match self.sent(&ptr.hash) {
            Some(sent) => Ok(Some(sent.facts)),
            None => Ok(Some(Facts::of(&*Entry::load(self.store, ptr)?))),
        }
    }

    fn chained(
        &self,
        objects: &impl Objects,
        linked: &mut Option<Column<u8>>,
        segment: usize,
        want: Ptr,
        at: u64,
    ) -> Result<Ptr> {
        match self.find(segment, at)? {
            Found::Sent(id) if self.hashes.hash(id) == want.hash => {
                mark_linked(objects, linked, &want.hash);
                Ok(sent_ptr(objects, want.hash))
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
        let segment = self.segments.len() as u32;
        let mut versions = Vec::new();
        let mut bottom;
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
            let id = staged_id(objects, &at);
            versions.push(id);
            bottom = version;
            merged.extend(entry.merged.map(|link| link.ptr.hash));
            let prev = entry.prev;
            self.commits.insert(Sent::of(id, &entry, segment));
            let Some(prev) = prev else {
                if version != 1 {
                    return Err(bad(format!("v{version} has no parent")));
                }
                break Below::Gone;
            };
            if let Some(sent) = self.sent(&prev.hash) {
                if sent.facts.version != version - 1 {
                    return Err(bad("a merged chain joins another at the wrong version"));
                }
                break Below::Joins(sent.id);
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
        versions.reverse();
        self.segments.push(Segment {
            versions,
            bottom,
            below,
        });
        Ok(merged)
    }
}

struct Kept {
    left: u8,
    checks: Vec<(Shape, Vec<Kind>)>,
}

struct Vetting<'a> {
    store: &'a Store,
    staging: &'a Staging,
    links: &'a Column<u8>,
    anchors: &'a Anchors,
    held_at: &'a Index,
    root_kind: &'a Kind,
    kept: HashMap<u32, Kept>,
    held: Checked,
    spent: usize,
    budget: usize,
    queue: Vec<(Ptr, Kind, Vec<Kind>)>,
}

fn rejected(error: Error) -> Error {
    match error {
        Error::Decode(message) => bad(message),
        other => other,
    }
}

impl Vetting<'_> {
    fn run(&mut self) -> Result<()> {
        while let Some((ptr, kind, before)) = self.queue.pop() {
            let shape = match self.staging.id_of(&ptr.hash) {
                Some(id) => self.sent(id, ptr.hash, &kind, before)?,
                None => {
                    let reference = self.anchors.find(self.held_at, &ptr.hash).ok_or_else(|| {
                        bad("an object links to one that was neither sent nor known")
                    })? as usize
                        + 1;
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

    fn sent(&mut self, id: u32, hash: Hash, kind: &Kind, mut before: Vec<Kind>) -> Result<Shape> {
        let links = self.links.get(id);
        let pinned = links == u8::MAX || self.anchors.find(self.held_at, &hash).is_some();
        let shared = links >= 2 || pinned;
        let counted = before.is_empty();
        if shared && let Some(kept) = self.kept.get_mut(&id) {
            let same = kept
                .checks
                .iter()
                .position(|(_, kinds)| kinds[0].id == kind.id);
            let hit = same.and_then(|at| {
                let (shape, kinds) = &kept.checks[at];
                kinds
                    .iter()
                    .any(|seen| (seen.within)(seen, kind))
                    .then_some(*shape)
            });
            if counted && kept.left != u8::MAX {
                kept.left = kept.left.saturating_sub(1);
            }
            if let Some(shape) = hit {
                if kept.left == 0 {
                    self.kept.remove(&id);
                }
                return Ok(shape);
            }
            if let Some(at) = same {
                before.extend(kept.checks[at].1.iter().cloned());
            }
        }
        let ptr = Ptr {
            hash,
            offset: self.staging.offset(id),
        };
        let object = self.staging.read_object(ptr)?;
        let checks = 1 + before.len();
        self.spent = self
            .spent
            .saturating_add(object.body.len().saturating_mul(checks));
        if self.spent > self.budget {
            return Err(bad("it reuses its objects under too many key ranges"));
        }
        let undecodable = |kind: &Kind, error: Error| {
            bad(format!(
                "a {} it carries does not decode: {error}",
                kind.name
            ))
        };
        let (shape, found) = kind
            .check(&object.body, &object.links)
            .map_err(|error| undecodable(kind, error))?;
        let mut earlier = Vec::with_capacity(before.len());
        for old in &before {
            let (_, found) = old
                .check(&object.body, &object.links)
                .map_err(|error| undecodable(old, error))?;
            earlier.push(found);
        }
        for (index, child) in found {
            let covered: Vec<Kind> = earlier
                .iter()
                .filter_map(|found| {
                    found
                        .iter()
                        .find(|(at, _)| *at == index)
                        .map(|(_, kind)| kind.clone())
                })
                .collect();
            if covered.iter().any(|old| old.implies(&child)) {
                continue;
            }
            self.queue.push((object.links[index], child, covered));
        }
        if shared {
            let left = if pinned {
                u8::MAX
            } else {
                links - u8::from(counted)
            };
            let kept = self.kept.entry(id).or_insert_with(|| Kept {
                left,
                checks: Vec::new(),
            });
            match kept
                .checks
                .iter_mut()
                .find(|(_, kinds)| kinds[0].id == kind.id)
            {
                Some((_, kinds)) => kinds.push(kind.clone()),
                None => kept.checks.push((shape, vec![kind.clone()])),
            }
            if kept.left == 0 {
                self.kept.remove(&id);
            }
        }
        Ok(shape)
    }

    fn held(&mut self, hash: Hash, reference: usize, kind: &Kind) -> Result<Option<Shape>> {
        let roots = self.anchors.roots.len();
        let (ptr, known) = if reference <= roots {
            (
                self.anchors.roots[reference - 1],
                Some(self.root_kind.clone()),
            )
        } else {
            let id = reference - 1 - roots;
            (self.anchors.hinted[id], self.anchors.hint_kinds.get(id))
        };
        if ptr.hash != hash {
            return Err(bad("a link does not match the object it names"));
        }
        let known = known.as_ref();
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

#[derive(Clone)]
struct Held {
    template: u16,
    count: Option<u64>,
    below: Option<u8>,
    range: Option<Arc<dyn Any + Send + Sync>>,
}

#[derive(Default)]
struct Kinds {
    templates: Vec<Kind>,
    held: Vec<Held>,
}

impl Kinds {
    fn new(len: usize) -> Kinds {
        let none = Held {
            template: u16::MAX,
            count: None,
            below: None,
            range: None,
        };
        Kinds {
            templates: Vec::new(),
            held: vec![none; len],
        }
    }

    fn set(&mut self, id: usize, kind: Kind) {
        let template = match self.templates.iter().position(|seen| seen.id == kind.id) {
            Some(at) => at,
            None => {
                let mut template = kind.clone();
                (template.count, template.below, template.range) = (None, None, None);
                self.templates.push(template);
                self.templates.len() - 1
            }
        };
        self.held[id] = Held {
            template: u16::try_from(template).unwrap_or(u16::MAX),
            count: kind.count,
            below: kind.below,
            range: kind.range,
        };
    }

    fn identity(&self, id: usize) -> (u16, Option<u64>, Option<u8>, usize) {
        let held = &self.held[id];
        let range = held
            .range
            .as_ref()
            .map_or(0, |range| Arc::as_ptr(range).cast::<()>() as usize);
        (held.template, held.count, held.below, range)
    }

    fn get(&self, id: usize) -> Option<Kind> {
        let held = &self.held[id];
        let mut kind = self.templates.get(usize::from(held.template))?.clone();
        kind.count = held.count;
        kind.below = held.below;
        kind.range = held.range.clone();
        Some(kind)
    }
}

struct Anchors {
    generation: u64,
    bases: Vec<(Ptr, Chain)>,
    roots: Vec<Ptr>,
    hinted: Vec<Ptr>,
    hint_kinds: Kinds,
}

impl Anchors {
    fn ptr(&self, at: u32) -> Ptr {
        let at = at as usize;
        match at.checked_sub(self.roots.len()) {
            None => self.roots[at],
            Some(id) => self.hinted[id],
        }
    }

    fn held(&self) -> Index {
        let mut index = Index::default();
        for at in 0..(self.roots.len() + self.hinted.len()) as u32 {
            let hash = self.ptr(at).hash;
            if index.find(&hash, |at| self.ptr(at).hash).is_none() {
                index.insert(&hash, at, |at| self.ptr(at).hash);
            }
        }
        index
    }

    fn find(&self, held: &Index, hash: &Hash) -> Option<u32> {
        held.find(hash, |at| self.ptr(at).hash)
    }
}

fn by_depth(hints: &[(usize, usize)], roots: usize) -> (Vec<u32>, Vec<u32>) {
    let mut depth: Vec<u32> = Vec::with_capacity(hints.len());
    for (parent, _) in hints {
        let below = parent.checked_sub(roots).map_or(0, |id| depth[id] + 1);
        depth.push(below);
    }
    let mut order: Vec<u32> = (0..hints.len() as u32).collect();
    order.sort_by_key(|id| depth[*id as usize]);
    (order, depth)
}

fn level_end(order: &[u32], depth: &[u32], start: usize) -> usize {
    let level = depth[order[start] as usize];
    start
        + order[start..]
            .iter()
            .take_while(|id| depth[**id as usize] == level)
            .count()
}

fn parent_of(header: &Header, roots: &[Ptr], hinted: &[Ptr], id: u32) -> Ptr {
    let parent = header.hints[id as usize].0;
    match parent.checked_sub(roots.len()) {
        None => roots[parent],
        Some(at) => hinted[at],
    }
}

type ParentKey = (u64, Hash, Option<(u16, Option<u64>, Option<u8>, usize)>);

fn parent_key(header: &Header, roots: &[Ptr], hinted: &[Ptr], kinds: &Kinds, id: u32) -> ParentKey {
    let from = parent_of(header, roots, hinted, id);
    let parent = header.hints[id as usize].0;
    let kind = parent.checked_sub(roots.len()).map(|at| kinds.identity(at));
    (from.offset, from.hash, kind)
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
    let mut hinted = vec![UNRESOLVED; header.hints.len()];
    let mut hint_kinds = Kinds::new(header.hints.len());
    let (mut order, depth) = by_depth(&header.hints, roots.len());
    let mut start = 0;
    while start < order.len() {
        let end = level_end(&order, &depth, start);
        order[start..end]
            .sort_unstable_by_key(|id| parent_key(header, &roots, &hinted, &hint_kinds, *id));
        let mut read: Option<(Ptr, Object)> = None;
        let mut at = start;
        while at < end {
            let key = parent_key(header, &roots, &hinted, &hint_kinds, order[at]);
            let from = parent_of(header, &roots, &hinted, order[at]);
            if read.as_ref().is_none_or(|(ptr, _)| *ptr != from) {
                read = Some((from, store.read_object(from)?));
            }
            let (_, object) = read.as_ref().expect("the parent was read");
            let kind = match header.hints[order[at] as usize].0.checked_sub(roots.len()) {
                None => Some(root_kind.clone()),
                Some(parent) => hint_kinds.get(parent),
            };
            let kinds: Vec<(usize, Kind)> = kind
                .and_then(|kind| kind.check(&object.body, &object.links).ok())
                .map(|(_, found)| found)
                .unwrap_or_default();
            while at < end && parent_key(header, &roots, &hinted, &hint_kinds, order[at]) == key {
                let id = order[at] as usize;
                let slot = header.hints[id].1;
                let link = *object
                    .links
                    .get(slot)
                    .ok_or_else(|| bad("a hint names a link that does not exist"))?;
                if link.offset == 0 {
                    return Err(bad("a hint leads into pruned history"));
                }
                hinted[id] = link;
                if let Some((_, kind)) = kinds.iter().find(|(at, _)| *at == slot) {
                    hint_kinds.set(id, kind.clone());
                }
                at += 1;
            }
        }
        start = end;
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
        let mut hinted = vec![UNRESOLVED; before.hinted.len()];
        let (mut order, depth) = by_depth(&header.hints, roots.len());
        let mut start = 0;
        while start < order.len() {
            let end = level_end(&order, &depth, start);
            order[start..end].sort_unstable_by_key(|id| {
                let from = parent_of(header, &roots, &hinted, *id);
                (from.offset, from.hash)
            });
            let mut at = start;
            while at < end {
                let from = parent_of(header, &roots, &hinted, order[at]);
                let links = store.read_links(from)?;
                while at < end && parent_of(header, &roots, &hinted, order[at]) == from {
                    let id = order[at] as usize;
                    let (slot, was) = (header.hints[id].1, before.hinted[id]);
                    let link = links
                        .get(slot)
                        .copied()
                        .filter(|link| link.hash == was.hash && link.offset != 0)
                        .ok_or_else(|| {
                            Error::StaleHave(
                                "an object it builds on is no longer in this database".into(),
                            )
                        })?;
                    hinted[id] = link;
                    at += 1;
                }
            }
            start = end;
        }
        hinted
    };
    Ok(Anchors {
        generation: store.id(),
        bases,
        roots,
        hinted,
        hint_kinds: Kinds::default(),
    })
}

struct Streamed {
    links: Column<u8>,
    loose: Vec<u32>,
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
        links: Column::new(),
        loose: Vec::new(),
        carried: 0,
    };
    for _ in 0..header.count {
        let (body, packed) = input.body()?;
        let mut links = Vec::new();
        let mut open = false;
        for _ in 0..input.varint()? {
            let hash = input.hash()?;
            let ptr = match input.below(references)? {
                0 => match staging.id_of(&hash) {
                    Some(id) => {
                        found.links.set(id, found.links.get(id).saturating_add(1));
                        Ptr {
                            hash,
                            offset: staging.offset(id),
                        }
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
        if staging.id_of(&hash).is_some() {
            return Err(bad("it repeats an object"));
        }
        found.carried = found.carried.saturating_add(object.body.len());
        let id = staging.put(hash, staged(&object.body, packed.as_deref(), &object.links))?;
        found.links.push(0);
        if open {
            found.loose.push(id);
        }
    }
    input.finish()?;
    Ok(found)
}

fn place(
    txn: &mut Txn,
    record: &Record,
    hashes: &Hashes,
    placed: &mut Column<u64>,
    anchors: &Anchors,
    held: &Index,
    unlisted: bool,
) -> Result<()> {
    let earlier = Earlier {
        hashes,
        placed: &*placed,
    };
    let mut links = Vec::with_capacity(record.links.len());
    for link in &record.links {
        links.push(match earlier.offset_of(&link.hash) {
            _ if link.offset == 0 => *link,
            Some(offset) => Ptr {
                hash: link.hash,
                offset,
            },
            None => anchors
                .find(held, &link.hash)
                .map(|at| anchors.ptr(at))
                .ok_or_else(|| {
                    Error::Corrupt("a received object links to one that was not placed".into())
                })?,
        });
    }
    let staged = record.staged(&links);
    let ptr = match unlisted {
        true => txn.put_unlisted(record.hash, staged)?,
        false => txn.put_record(record.hash, staged)?,
    };
    placed.push(ptr.offset);
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
    commits: Vec<u32>,
}

struct Received {
    staging: Staging,
    anchors: Anchors,
    commits: Vec<u32>,
}

fn resolve<O: Objects>(
    store: &Store,
    objects: &O,
    refs: &Refs,
    remote: &str,
    header: &Header,
    anchors: &Anchors,
    vetted: Option<Vetted<'_>>,
) -> Result<Resolved> {
    let tip = header.tip.expect("a landing bundle has a head");
    let checking = vetted.is_some();
    let mut commits = Commits::new();
    let mut versions = Vec::new();
    let mut pending: Vec<Hash> = Vec::new();
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
        let id = staged_id(objects, &mark.hash);
        versions.push(id);
        pending.extend(entry.merged.map(|link| link.ptr.hash));
        commits.insert(Sent::of(id, &entry, 0));
    }
    versions.reverse();
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
        hashes: objects.hashes(),
        low,
        segments: vec![Segment {
            versions,
            bottom: low,
            below: prefix.map_or(Below::Gone, Below::Chain),
        }],
        commits,
    };
    while let Some(top) = pending.pop() {
        if landing.sent(&top).is_some() || objects.offset_of(&top).is_none() {
            continue;
        }
        pending.extend(landing.segment(refs, objects, top)?);
    }
    let mut order: Vec<(u64, u64, u32)> = Vec::with_capacity(landing.commits.len());
    order.extend(
        landing
            .commits
            .iter()
            .map(|sent| (sent.facts.generation, sent.facts.version, sent.id)),
    );
    order.sort_unstable();
    let mut linked = None;
    if let Some(Vetted {
        streamed,
        root_kind,
    }) = vetted
    {
        let staging = objects
            .staging()
            .expect("a bundle is checked where it is staged");
        let now = now_ms();
        let horizon = now.saturating_add(MAX_AHEAD_MS);
        if let Some(sent) = landing
            .commits
            .iter()
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
            .any(|id| !landing.commits.contains(*id))
            || landing
                .commits
                .iter()
                .any(|sent| sent.root.is_some_and(|root| root.offset == 0));
        if unresolved {
            return Err(bad(
                "an object links to one that was neither sent nor known",
            ));
        }
        let held_at = anchors.held();
        let mut vetting = Vetting {
            store,
            staging,
            links: &streamed.links,
            anchors,
            held_at: &held_at,
            root_kind,
            kept: HashMap::new(),
            held: Checked::default(),
            spent: 0,
            budget: streamed
                .carried
                .saturating_mul(2)
                .saturating_add(RECHECK_SLACK),
            queue: Vec::new(),
        };
        for (_, _, id) in &order {
            let sent = landing
                .commits
                .get(*id)
                .expect("an ordered commit was walked");
            if let Some(root) = sent.root {
                vetting.queue.push((root, root_kind.clone(), Vec::new()));
                vetting.run()?;
            }
        }
        drop(vetting);
        linked = Some(streamed.links);
    }
    let based: HashMap<Hash, Ptr> = anchors
        .bases
        .iter()
        .map(|(ptr, _)| (ptr.hash, *ptr))
        .collect();
    let mut tip_root = None;
    for (_, _, id) in order {
        let hash = landing.hashes.hash(id);
        let object = objects.read_object(sent_ptr(objects, hash))?;
        let entry = decode_commit(&object)?;
        let sent = landing
            .commits
            .get(id)
            .expect("an ordered commit was walked");
        let version = sent.facts.version;
        let segment = sent.segment as usize;
        let mut links = Vec::with_capacity(object.links.len());
        links.extend(sent.root);
        let mut prev_facts = None;
        if let Some(prev) = entry.prev {
            let ptr = landing.chained(objects, &mut linked, segment, prev, version - 1)?;
            prev_facts = Some(landing.facts(ptr)?);
            links.push(ptr);
        }
        if let Some(skip) = entry.skip {
            let at = skip_version(version);
            links.push(landing.chained(objects, &mut linked, segment, skip, at)?);
        }
        let mut merged_facts = None;
        if let Some(link) = entry.merged {
            let merged = link.ptr.hash;
            let ptr = if landing.sent(&merged).is_some() {
                mark_linked(objects, &mut linked, &merged);
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
        check_graph(sent.facts, entry.merged, prev_facts, merged_facts)?;
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
    mark_linked(objects, &mut linked, &tip.hash);
    if checking && linked.is_some_and(|links| links.iter().any(|count| count == 0)) {
        return Err(bad("it carries objects nothing links to"));
    }
    let tip_ptr = match tip_ptr {
        Some(ptr) => ptr,
        None => sent_ptr(objects, tip.hash),
    };
    let root = if landing.sent(&tip.hash).is_some() {
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
            commits: landing.commits.ids(),
        });
    }
    let mut next = refs.clone();
    let continues = match existing
        .as_ref()
        .and_then(|old| old.log.tip.map(|ptr| (ptr.hash, old.log.last)))
    {
        None => false,
        Some((hash, version)) => match landing.find(0, version) {
            Ok(Found::Sent(found)) => landing.hashes.hash(found) == hash,
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
        commits: landing.commits.ids(),
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
    /// What this file holds of the commit graph, for another file to build a [`Bundle`] from.
    ///
    /// It names the head of every branch in the file, and commits along this branch at doubling
    /// distances back from its head, so the other side can find the newest commit both hold. It is
    /// a few hundred bytes.
    ///
    /// # Errors
    ///
    /// Read errors from the branch table or the log.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Bundle, Db, Have, Map, Outcome, Snapshot};
    ///
    /// type Stock = Map<String, u32>;
    ///
    /// fn merge(
    ///     stock: &mut Stock,
    ///     base: &Snapshot<Stock>,
    ///     theirs: &Snapshot<Stock>,
    /// ) -> (&'static str, usize) {
    ///     ("sync", stock.merge(base, theirs, |conflict| conflict.keep_theirs()))
    /// }
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// let laptop = Db::<Stock>::open(dir.path().join("laptop.ferro"))?;
    /// let phone = Db::<Stock>::open(dir.path().join("phone.ferro"))?;
    /// laptop.commit("receive stock", |stock| {
    ///     stock.insert("anvil".to_string(), 5);
    /// })?;
    ///
    /// let have: Vec<u8> = phone.have()?.into();
    /// let bundle: Vec<u8> = laptop.bundle(&Have::from(have))?.into();
    /// let pulled = phone.receive("laptop", &Bundle::from(bundle), merge)?;
    ///
    /// assert_eq!(pulled.outcome, Outcome::FastForward);
    /// assert_eq!(phone.read().get("anvil").as_deref(), Some(&5));
    /// # Ok(())
    /// # }
    /// ```
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

    /// The commits of this branch that the file which sent `have` lacks, with the objects they
    /// reach that it does not hold.
    ///
    /// It costs what changed since the newest commit both files hold, and the first bundle for a
    /// file that shares no commit with this one carries everything. It reads the branch as last
    /// published and holds no writer lock.
    ///
    /// # Errors
    ///
    /// [`Error::Bundle`] if `have` is not a have, and read errors.
    pub fn bundle(&self, have: &Have) -> Result<Bundle> {
        let mut out = Vec::new();
        self.bundle_to(have, &mut out)?;
        Ok(Bundle(out))
    }

    /// Writes the bundle [`Db::bundle`] would build to `out` as it goes, and returns the bytes
    /// written.
    ///
    /// Neither side holds the bundle whole, so `out` can be a socket or a response body.
    ///
    /// # Errors
    ///
    /// As for [`Db::bundle`], and [`Error::Io`] if writing to `out` fails.
    pub fn bundle_to(&self, have: &Have, mut out: impl Write) -> Result<u64> {
        self.emitter(have)?.write_to(&mut out)
    }

    fn emitter(&self, have: &Have) -> Result<Emitter> {
        let have = Marks::decode(have.as_bytes())?;
        let (store, refs) = self.published_refs();
        plan(&store, &refs, self.branch_name(), &have)
    }

    /// Copies the branch of `from`, usually a handle on another file, into this file as the branch
    /// `remote`, then brings this branch up to it.
    ///
    /// Only the commits this file lacks are sent, streamed between the two files. If this branch
    /// has nothing the remote one lacks, it fast-forwards to the remote head; if each has commits
    /// the other lacks, `merge` runs as in [`Db::merge_with`] and its merge commit links the
    /// remote head. The base is a best common ancestor over the merge graph of every file the
    /// commits passed through, so a change that reaches this file through a third device is
    /// merged once. A pulled commit keeps the version, time, message and hash it had, while a merge
    /// commits at this branch's next version, so compare commits across files by hash. A pull that
    /// brings new commits replaces the branch `remote`, so do not commit to it.
    ///
    /// # Errors
    ///
    /// - [`Error::BranchInUse`] if `remote` is `main` or this handle's branch.
    /// - [`Error::StaleHave`] if this file pruned or dropped what the bundle builds on while it was
    ///   sent. Nothing landed, and calling again sends a fresh have.
    /// - [`Error::NoCommonAncestor`] when the merge base could lie in pruned history. The remote
    ///   branch has landed, and this branch is as it was.
    /// - [`Error::Bundle`] if what `from` sends fails the checks, which for an open file means it
    ///   is damaged.
    /// - [`Error::Reentrant`] from inside a closure on this file, or where waiting on `from`'s
    ///   file could otherwise deadlock, [`Error::ReadOnly`] on a read-only handle, and the errors
    ///   of [`Db::commit`].
    ///
    /// # Panics
    ///
    /// A panic in `merge` propagates, and this branch is as it was.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Map, Outcome, Snapshot};
    ///
    /// type Stock = Map<String, u32>;
    ///
    /// fn merge(
    ///     stock: &mut Stock,
    ///     base: &Snapshot<Stock>,
    ///     theirs: &Snapshot<Stock>,
    /// ) -> (&'static str, usize) {
    ///     ("sync", stock.merge(base, theirs, |conflict| conflict.keep_theirs()))
    /// }
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// let laptop = Db::<Stock>::open(dir.path().join("laptop.ferro"))?;
    /// let phone = Db::<Stock>::open(dir.path().join("phone.ferro"))?;
    /// laptop.commit("receive anvils", |stock| {
    ///     stock.insert("anvil".to_string(), 5);
    /// })?;
    ///
    /// let pulled = phone.pull(&laptop, "laptop", merge)?;
    /// assert_eq!(pulled.outcome, Outcome::FastForward);
    /// assert_eq!(phone.head_hash(), laptop.head_hash());
    ///
    /// phone.commit("receive rope", |stock| {
    ///     stock.insert("rope".to_string(), 2);
    /// })?;
    /// laptop.commit("sell the anvils", |stock| {
    ///     stock.remove("anvil");
    /// })?;
    /// let pulled = phone.pull(&laptop, "laptop", merge)?;
    /// assert_eq!(pulled.outcome, Outcome::Merged);
    ///
    /// let stock = phone.read();
    /// assert!(stock.get("anvil").is_none());
    /// assert_eq!(stock.get("rope").as_deref(), Some(&2));
    /// # Ok(())
    /// # }
    /// ```
    pub fn pull<R, S: Into<String>>(
        &self,
        from: &Db<T>,
        remote: &str,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Pulled<R>> {
        self.try_pull::<R, S, Error>(from, remote, infallible(merge))
    }

    /// Pulls like [`Db::pull`], with a merge closure that can fail.
    ///
    /// # Errors
    ///
    /// An `Err` from `merge` leaves this branch and its merge records as they were and comes back
    /// as it was, while the remote branch stays landed, so the next pull sends nothing new and
    /// merges it again. The errors of [`Db::pull`] come back through `E::from`.
    ///
    /// # Panics
    ///
    /// As for [`Db::pull`].
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

    /// Copies this handle's branch into the file of `to` as the branch `remote`, and brings the
    /// branch of `to` up to it.
    ///
    /// It is [`Db::pull`] from the other side: `to.pull(self, remote, merge)`, so `merge` gets the
    /// value of `to`'s branch as `ours`.
    ///
    /// # Errors
    ///
    /// As for [`Db::pull`], for the branch of `to`.
    ///
    /// # Panics
    ///
    /// As for [`Db::pull`].
    pub fn push<R, S: Into<String>>(
        &self,
        to: &Db<T>,
        remote: &str,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Pulled<R>> {
        to.pull(self, remote, merge)
    }

    /// Pushes like [`Db::push`], with a merge closure that can fail.
    ///
    /// # Errors
    ///
    /// As for [`Db::try_pull`], for the branch of `to`.
    ///
    /// # Panics
    ///
    /// As for [`Db::pull`].
    pub fn try_push<R, S: Into<String>, E: From<Error>>(
        &self,
        to: &Db<T>,
        remote: &str,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Pulled<R>, E> {
        to.try_pull(self, remote, merge)
    }

    /// Lands `bundle`, which another file built with [`Db::bundle`] from this file's
    /// [`Db::have`], as the branch `remote`, then brings this branch up to it as [`Db::pull`] does.
    ///
    /// It hashes every object and checks every link as it reads, and checks every object as the
    /// type that will read it, and it refuses commits dated more than a day past this machine's
    /// clock. Objects are staged as they arrive, in memory up to 1 MiB and past that in a scratch
    /// file beside the database, under no writer lock, so commits on every branch go on meanwhile.
    /// Once the whole bundle has passed, its objects are copied into the file, and one frame
    /// points the branch `remote` at them, so a crash leaves the previous head. A bundle that
    /// arrives late or twice changes nothing.
    ///
    /// # Errors
    ///
    /// - [`Error::Bundle`] if the bundle is malformed, damaged or forged. Nothing reaches the file.
    /// - [`Error::StaleHave`] if this file does not hold the commit the bundle builds on, because
    ///   it was built for another file, or this one pruned or dropped that commit after sending
    ///   its have. Nothing lands; build a new bundle from a fresh have.
    /// - The other errors of [`Db::pull`].
    ///
    /// # Panics
    ///
    /// As for [`Db::pull`].
    pub fn receive<R, S: Into<String>>(
        &self,
        remote: &str,
        bundle: &Bundle,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Pulled<R>> {
        self.try_receive::<R, S, Error>(remote, bundle, infallible(merge))
    }

    /// Receives like [`Db::receive`], with a merge closure that can fail.
    ///
    /// # Errors
    ///
    /// As for [`Db::try_pull`].
    ///
    /// # Panics
    ///
    /// As for [`Db::pull`].
    pub fn try_receive<R, S: Into<String>, E: From<Error>>(
        &self,
        remote: &str,
        bundle: &Bundle,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Pulled<R>, E> {
        self.try_receive_from(remote, bundle.as_bytes(), merge)
    }

    /// Receives a bundle like [`Db::receive`], reading it from `input` to its end.
    ///
    /// `input` can be a socket or a download, so the bundle is never held whole: this uses a fixed
    /// buffer, the object being read, and about 40 bytes for each object the bundle carries.
    ///
    /// # Errors
    ///
    /// As for [`Db::receive`], and [`Error::Io`] if reading `input` fails.
    ///
    /// # Panics
    ///
    /// As for [`Db::pull`].
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Db, Map, Outcome};
    ///
    /// # fn main() -> ferro::Result<()> {
    /// # let dir = tempfile::tempdir()?;
    /// let laptop = Db::<Map<u64, u64>>::open(dir.path().join("laptop.ferro"))?;
    /// let phone = Db::<Map<u64, u64>>::open(dir.path().join("phone.ferro"))?;
    /// laptop.update(|readings| {
    ///     readings.insert(1, 20);
    /// })?;
    ///
    /// let mut wire = Vec::new();
    /// let sent = laptop.bundle_to(&phone.have()?, &mut wire)?;
    /// let pulled = phone.receive_from("laptop", wire.as_slice(), |readings, base, theirs| {
    ///     ("sync", readings.merge(base, theirs, |conflict| conflict.keep_theirs()))
    /// })?;
    ///
    /// assert_eq!(pulled.received_bytes, sent);
    /// assert_eq!(pulled.outcome, Outcome::FastForward);
    /// # Ok(())
    /// # }
    /// ```
    pub fn receive_from<R, S: Into<String>>(
        &self,
        remote: &str,
        input: impl Read,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Pulled<R>> {
        self.try_receive_from::<R, S, Error>(remote, input, infallible(merge))
    }

    /// Receives from `input` like [`Db::receive_from`], with a merge closure that can fail.
    ///
    /// # Errors
    ///
    /// As for [`Db::try_pull`], and [`Error::Io`] if reading `input` fails.
    ///
    /// # Panics
    ///
    /// As for [`Db::pull`].
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
        let mut anchors = anchors(&store, &refs, header, &root_kind)?;
        let mut staging = Staging::new(store.path(), SPILL_AT);
        let streamed = stream(&mut staging, header, &anchors, input)?;
        let vetted = Vetted {
            streamed,
            root_kind: &root_kind,
        };
        let sent = resolve(
            &store,
            &staging,
            &refs,
            remote,
            header,
            &anchors,
            Some(vetted),
        )?;
        anchors.hint_kinds = Kinds::default();
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
        let mut placed = Column::new();
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
                    place(
                        txn,
                        &record,
                        staging.hashes(),
                        &mut placed,
                        &anchors,
                        &held,
                        false,
                    )?;
                }
            }
            let objects = Placed {
                txn: RefCell::new(txn),
                earlier: Earlier {
                    hashes: staging.hashes(),
                    placed: &placed,
                },
            };
            let resolved = resolve(store, &objects, refs, remote, header, &anchors, None)?;
            Ok((resolved.refs, ()))
        })
    }

    fn place_staged(
        &self,
        staging: &Staging,
        anchors: &Anchors,
        commits: &[u32],
    ) -> Result<Column<u64>> {
        let held = anchors.held();
        let mut placed = Column::new();
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
                    let unlisted = commits.binary_search(&(placed.len() as u32)).is_ok();
                    place(
                        txn,
                        record,
                        staging.hashes(),
                        &mut placed,
                        anchors,
                        &held,
                        unlisted,
                    )?;
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
