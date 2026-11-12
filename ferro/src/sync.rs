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
//! reached from the file. See
//! [Sync between files](https://github.com/archlemon/ferro/blob/main/docs/guide/sync.md).

use std::any::{Any, TypeId};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BinaryHeap, HashMap, HashSet, hash_map};
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::mem;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use hashbrown::HashTable;
use hashbrown::hash_table as table;

use crate::codec::{
    Checks, Claim, Data, Decoder, Finding, FoundTable, HASH_BYTES, Hash, Kind, Object, PairRoots,
    Paired, Pairing, Plans, Ptr, Recalled, Shape, Tables, Walks, pair_roots, push_varint,
};
use crate::db::{Db, Head, Refs, Snapshot, now_ms};
use crate::dense::{self, Column, Hashes, Index, MARKED_MOST, Marked};
use crate::error::{Error, Result};
use crate::log::{Chain, Entry, Facts, Merged, Node, disagreement};
use crate::map::Map;
use crate::pool::{self, Pool};
use crate::reentry::{Crossing, Needs};
use crate::stack;
use crate::store::{
    LinkTo, Opened, Record, Room, StagedLinks, Staging, Store, Together, Txn, compressed,
    stored_within,
};

const HAVE_MAGIC: &[u8; 8] = b"FERROHAV";
const BUNDLE_MAGIC: &[u8; 8] = b"FERROBDL";
const FORMAT: u8 = 2;
const COMPRESS_MIN: usize = 96;
const MAX_RATIO: usize = 255;
const RECHECK_SLACK: usize = 16 << 20;
const RERUN_FACTOR: usize = 8;
const RERUN_SLACK: usize = 256 << 20;
const RERUN_PER_LINK: usize = 4 << 10;
const CREDIT_BATCH: u64 = 1 << 10;
const ZERO_WIDTH_SLACK: u64 = 1 << 28;
const ZERO_WIDTH_PER_BYTE: u64 = 16;
const CONFIRMED_KEPT: usize = 1 << 16;
const RANGES_KEPT: usize = 16;
const LIVE_CHUNK: usize = 1 << 12;
const LIVE_SETTLE: usize = 1 << 6;
const LIVE_SCAN: usize = 1 << 4;
const LINKS_HELD: usize = 1 << 10;
const LIVE_SLACK: u64 = 4 << 20;
const HELD_SLOTS: usize = 1 << 18;
const HELD_KEPT: usize = HELD_SLOTS / 8 * 7;
const HELD_FIRST: usize = 1 << 10;
const HELD_EPOCH: usize = HELD_KEPT / 16;
const HELD_AGES: usize = 64;
const HINT_KINDS: usize = 4;
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
const PULL_ATTEMPTS: usize = 3;
const CACHED_LINKS: usize = 1 << 16;
const MOVED_BUDGET: usize = 1 << 16;
const SWEPT_RUN: usize = 64;
const SWEPT_PAGE: usize = 64 << 10;
const SWEPT_WINDOW: u64 = 1 << 20;
const SWEPT_TAIL: u64 = 64 << 10;
const WAITING: u8 = 0;
const OPEN: u8 = 1;
const SENT: u8 = 2;
const BARE: u8 = 4;
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
    /// The remote branch brought nothing this branch lacks: this branch already had the remote
    /// head, or has the remote head's value and every remote commit it lacks holds that value too,
    /// or the remote branch has no commits. This branch did not change.
    UpToDate,
    /// This branch already holds everything the remote branch has, and more of its own. This
    /// branch did not change.
    Ahead,
    /// This branch had nothing the remote branch lacks, so it now points at the remote head, and
    /// [`Db::head_hash`](crate::Db::head_hash) is the same in both files.
    FastForward,
    /// Each side had commits the other lacked, so the merge closure ran, and its merge commit is
    /// this branch's new head.
    ///
    /// A pull merges even when both sides have the same value, if a remote commit this branch
    /// lacked holds another one, as when the remote changed a field and changed it back, so that
    /// the merge commit links those commits and a later merge finds its base among them.
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
        if len > READ_STEP {
            return self.bytes_in_steps(len);
        }
        let mut out = Vec::with_capacity(len);
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

    fn bytes_in_steps(&mut self, len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        while out.len() < len {
            let step = (len - out.len()).min(out.capacity().max(READ_STEP));
            out.reserve_exact(step);
            let got = (&mut self.source)
                .take(step as u64)
                .read_to_end(&mut out)
                .map_err(from_io)?;
            self.read += got as u64;
            if got != step {
                return Err(bad("truncated"));
            }
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
            let packed = compressed(&bytes);
            return Ok((bytes, packed));
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

struct Ready {
    ptr: Ptr,
    object: Object,
    packed: Option<Vec<u8>>,
    uses: u64,
}

fn take_varint(bytes: &[u8], pos: &mut usize) -> u64 {
    let mut value = 0u64;
    for index in 0..10 {
        let byte = bytes[*pos];
        *pos += 1;
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            break;
        }
    }
    value
}

struct Swept {
    heads: Vec<u64>,
    places: Vec<(u32, u32)>,
    pages: Vec<Vec<u8>>,
    len: usize,
    last: u64,
    run: Option<usize>,
    found: Vec<(u64, u32, u64)>,
}

fn part_of(hash: &Hash) -> u32 {
    u32::from_le_bytes(hash[..4].try_into().expect("a hash is longer than 4 bytes"))
}

impl Swept {
    fn new() -> Swept {
        Swept {
            heads: Vec::new(),
            places: Vec::new(),
            pages: Vec::new(),
            len: 0,
            last: u64::MAX,
            run: None,
            found: Vec::new(),
        }
    }

    fn push(&mut self, offset: u64, part: u32, uses: u64) -> Result<()> {
        if self.len >= u32::MAX as usize {
            return Err(too_many());
        }
        if self.len.is_multiple_of(SWEPT_RUN) {
            if self
                .pages
                .last()
                .is_none_or(|page| page.len() + SWEPT_RUN * 24 > SWEPT_PAGE)
            {
                self.pages.push(Vec::with_capacity(SWEPT_PAGE));
            }
            let page = self.pages.len() - 1;
            self.heads.push(offset);
            self.places
                .push((page as u32, self.pages[page].len() as u32));
        } else {
            let page = self.pages.last_mut().expect("a run has a page");
            push_varint(page, self.last - offset);
        }
        let page = self.pages.last_mut().expect("a run has a page");
        page.extend_from_slice(&part.to_le_bytes());
        push_varint(page, uses);
        self.last = offset;
        self.len += 1;
        Ok(())
    }

    fn runs(&self) -> usize {
        self.heads.len()
    }

    fn decode(&self, run: usize, out: &mut Vec<(u64, u32, u64)>) {
        out.clear();
        let count = (self.len - run * SWEPT_RUN).min(SWEPT_RUN);
        let (page, start) = self.places[run];
        let bytes = &self.pages[page as usize][start as usize..];
        let (mut offset, mut pos) = (self.heads[run], 0);
        for at in 0..count {
            if at > 0 {
                offset -= take_varint(bytes, &mut pos);
            }
            let part = u32::from_le_bytes(bytes[pos..pos + 4].try_into().expect("four bytes"));
            pos += 4;
            out.push((offset, part, take_varint(bytes, &mut pos)));
        }
    }

    fn position(&mut self, offset: u64, from: usize) -> Option<u32> {
        let heads = &self.heads;
        if heads.get(from).is_none_or(|head| *head < offset) {
            return None;
        }
        let (mut low, mut step) = (from, 1);
        while low + step < heads.len() && heads[low + step] >= offset {
            low += step;
            step *= 2;
        }
        let high = (low + step).min(heads.len());
        let run = low + heads[low..high].partition_point(|head| *head >= offset) - 1;
        if self.run != Some(run) {
            let mut found = mem::take(&mut self.found);
            self.decode(run, &mut found);
            self.found = found;
            self.run = Some(run);
        }
        let within = self
            .found
            .binary_search_by(|(held, _, _)| offset.cmp(held))
            .ok()?;
        Some((self.len - 1 - (run * SWEPT_RUN + within)) as u32)
    }
}

fn sweep(store: &Store, entries: &[Sending]) -> Result<Swept> {
    let mut commits: Vec<u64> = entries.iter().map(|sending| sending.ptr.offset).collect();
    commits.sort_unstable();
    let mut frontier = BinaryHeap::with_capacity(2 * entries.len());
    for sending in entries {
        frontier.push((sending.ptr.offset, part_of(&sending.ptr.hash)));
    }
    let mut reached = Swept::new();
    let mut current: Option<(u64, u32, u64)> = None;
    let (mut window, mut window_at, mut found) = (Vec::new(), 0u64, Vec::new());
    while let Some((offset, part)) = frontier.pop() {
        if let Some((at, _, uses)) = current.as_mut()
            && *at == offset
        {
            *uses += 1;
            continue;
        }
        if let Some((at, part, uses)) = current.take() {
            reached.push(at, part, uses)?;
        }
        if offset == 0 {
            return Err(bad(
                "the branch links to pruned history outside its commit log",
            ));
        }
        let commit = commits.binary_search(&offset).is_ok();
        current = Some((offset, part, u64::from(!commit)));
        let links = match commit {
            true => {
                let (object, _) = store.read_packed_at(offset)?;
                let entry = read_entry(&object)?;
                let chained = [entry.prev, entry.skip, entry.merged.map(|link| link.ptr)];
                let chained = chained
                    .into_iter()
                    .flatten()
                    .filter(|link| commits.binary_search(&link.offset).is_ok());
                entry.commit.root.into_iter().chain(chained).collect()
            }
            false => {
                if offset < window_at || offset >= window_at + window.len() as u64 {
                    window_at = offset.saturating_sub(SWEPT_WINDOW - SWEPT_TAIL);
                    store.read_span(window_at, offset.saturating_add(SWEPT_TAIL), &mut window)?;
                }
                let held = window
                    .get((offset - window_at) as usize..)
                    .unwrap_or_default();
                match stored_within(held, offset, &mut found) {
                    Some(_) => mem::take(&mut found),
                    None => store.read_links(Ptr {
                        hash: [0; HASH_BYTES],
                        offset,
                    })?,
                }
            }
        };
        for link in links {
            if link.offset >= offset {
                return Err(Error::Corrupt(format!(
                    "the record at {offset} links to one that is not before it"
                )));
            }
            frontier.push((link.offset, part_of(&link.hash)));
        }
    }
    if let Some((at, part, uses)) = current {
        reached.push(at, part, uses)?;
    }
    Ok(reached)
}

struct Walk {
    send: Sendable,
    order: Vec<u64>,
    next: usize,
}

impl Walk {
    fn next(&mut self, store: &Store) -> Result<Option<Ready>> {
        let Some(offset) = self.order.get(self.next).copied() else {
            return Ok(None);
        };
        self.next += 1;
        let (object, packed) = store.read_packed_at(offset)?;
        Ok(Some(Ready {
            ptr: Ptr {
                hash: object.hash(),
                offset,
            },
            object,
            packed,
            uses: 0,
        }))
    }
}

struct Sweep {
    reached: Swept,
    run: usize,
    offsets: Vec<(u64, u32, u64)>,
}

impl Sweep {
    fn new(reached: Swept) -> Sweep {
        Sweep {
            run: reached.runs(),
            reached,
            offsets: Vec::new(),
        }
    }

    fn next(&mut self, store: &Store) -> Result<Option<Ready>> {
        if self.offsets.is_empty() {
            let Some(run) = self.run.checked_sub(1) else {
                return Ok(None);
            };
            self.run = run;
            self.reached.decode(run, &mut self.offsets);
        }
        let (offset, part, uses) = self.offsets.pop().expect("a run holds an offset");
        let (object, packed) = store.read_packed_at(offset)?;
        let hash = object.hash();
        if part_of(&hash) != part {
            return Err(Error::Corrupt(format!(
                "hash mismatch for object at {offset}"
            )));
        }
        Ok(Some(Ready {
            ptr: Ptr { hash, offset },
            object,
            packed,
            uses,
        }))
    }
}

enum Order {
    Walk(Box<Walk>),
    Sweep(Sweep),
}

struct Emitter {
    store: Store,
    header: Vec<u8>,
    order: Order,
    count: u64,
    position: u32,
    references: HashMap<Hash, usize>,
    held: usize,
    bodies: usize,
    sent: usize,
    out: Vec<u8>,
    pos: usize,
}

impl Emitter {
    fn new(
        store: &Store,
        header: &Header,
        order: Order,
        references: HashMap<Hash, usize>,
    ) -> Emitter {
        Emitter {
            store: store.clone(),
            header: header.encode(),
            order,
            count: header.count,
            position: 0,
            references,
            held: header.roots.len() + header.hints.len(),
            bodies: 0,
            sent: 0,
            out: Vec::new(),
            pos: 0,
        }
    }

    fn sent_at(&mut self, link: Ptr) -> Result<Option<u32>> {
        if link.offset == 0 {
            return Ok(None);
        }
        Ok(match &mut self.order {
            Order::Walk(walk) => walk
                .send
                .get(link)?
                .filter(|place| place.state == SENT)
                .map(|place| place.position),
            Order::Sweep(sweep) => sweep.reached.position(link.offset, sweep.run),
        })
    }

    fn emit(&mut self, mut ready: Ready) -> Result<()> {
        if let Order::Walk(walk) = &mut self.order {
            let position = self.position;
            let place = walk.send.update(ready.ptr, |place| Place {
                state: SENT,
                position,
                ..place
            })?;
            let place = place.ok_or_else(|| {
                Error::Corrupt(format!("hash mismatch for object at {}", ready.ptr.offset))
            })?;
            ready.uses = u64::from(place.position);
        }
        let before = self.sent;
        put_body(
            &mut self.out,
            &ready.object.body,
            ready.packed,
            &mut self.bodies,
            before,
        );
        push_varint(&mut self.out, ready.object.links.len() as u64);
        for link in &ready.object.links {
            if let Some(reference) = self.references.get(&link.hash) {
                push_varint(&mut self.out, *reference as u64);
                self.out.extend_from_slice(&link.hash);
                continue;
            }
            match self.sent_at(*link)? {
                Some(position) if position < self.position => {
                    let back = u64::from(self.position - 1 - position);
                    push_varint(&mut self.out, self.held as u64 + 1 + back);
                }
                _ => {
                    push_varint(&mut self.out, 0);
                    self.out.extend_from_slice(&link.hash);
                }
            }
        }
        push_varint(&mut self.out, ready.uses);
        self.position += 1;
        Ok(())
    }

    fn fill(&mut self) -> Result<bool> {
        self.out.clear();
        self.pos = 0;
        if !self.header.is_empty() {
            mem::swap(&mut self.out, &mut self.header);
            self.sent += self.out.len();
            return Ok(true);
        }
        while self.out.len() < WRITE_CHUNK {
            let ready = match &mut self.order {
                Order::Walk(walk) => walk.next(&self.store)?,
                Order::Sweep(sweep) => sweep.next(&self.store)?,
            };
            let Some(ready) = ready else {
                break;
            };
            self.emit(ready)?;
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

#[derive(Clone, Copy)]
struct Place {
    at: u64,
    state: u8,
    bare: bool,
    position: u32,
}

impl Place {
    fn of(at: u64, mark: dense::Mark) -> Place {
        Place {
            at,
            state: mark.marks & !BARE,
            bare: mark.marks & BARE != 0,
            position: mark.value,
        }
    }
}

struct Sendable {
    marked: Marked,
    store: Store,
    aliases: RefCell<HashMap<u64, u64>>,
}

impl Sendable {
    fn new(store: &Store) -> Sendable {
        Sendable {
            marked: Marked::new(),
            store: store.clone(),
            aliases: RefCell::new(HashMap::new()),
        }
    }

    fn len(&self) -> usize {
        self.marked.len()
    }

    fn alias(&self, ptr: Ptr) -> Option<u64> {
        let aliases = self.aliases.borrow();
        match aliases.is_empty() {
            true => None,
            false => aliases.get(&ptr.offset).copied(),
        }
    }

    fn get(&self, ptr: Ptr) -> Result<Option<Place>> {
        let offset = self.alias(ptr);
        let found = self
            .marked
            .find(&ptr.hash, offset.unwrap_or(ptr.offset), |at| {
                self.store.holds(Ptr {
                    hash: ptr.hash,
                    offset: at,
                })
            })?;
        let Some(mark) = found else {
            return Ok(None);
        };
        if mark.offset != ptr.offset && ptr.offset != 0 && offset.is_none() {
            self.aliases.borrow_mut().insert(ptr.offset, mark.offset);
        }
        Ok(Some(Place::of(mark.offset, mark)))
    }

    fn contains(&self, ptr: Ptr) -> Result<bool> {
        Ok(self.get(ptr)?.is_some())
    }

    fn insert(&mut self, ptr: Ptr, bare: bool) -> Result<()> {
        if self.marked.len() >= u32::MAX as usize {
            return Err(too_many());
        }
        if ptr.offset > MARKED_MOST {
            return Err(Error::Corrupt(format!(
                "pointer {} past what a sync can send",
                ptr.offset
            )));
        }
        let marks = if bare { BARE } else { WAITING };
        self.marked.insert(&ptr.hash, ptr.offset, marks, 0);
        Ok(())
    }

    fn update(&mut self, ptr: Ptr, change: impl FnOnce(Place) -> Place) -> Result<Option<Place>> {
        let mut change = Some(change);
        let mut apply = |at: u64, mark: dense::Mark| {
            let change = change.take().expect("a slot is changed once");
            let place = change(Place::of(at, mark));
            let bare = if place.bare { BARE } else { 0 };
            (place.state | bare, place.position)
        };
        let at = self.alias(ptr).unwrap_or(ptr.offset);
        match self.marked.update(&ptr.hash, at, |mark| apply(at, mark)) {
            Ok(was) => return Ok(Some(Place::of(at, was))),
            Err(false) => return Ok(None),
            Err(true) => {}
        }
        let Some(place) = self.get(ptr)? else {
            return Ok(None);
        };
        let was = self
            .marked
            .update(&ptr.hash, place.at, |mark| apply(place.at, mark));
        Ok(was.ok().map(|mark| Place::of(place.at, mark)))
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

#[derive(Clone, Copy)]
struct Reached {
    hash: Hash,
    offset: u64,
    times: i32,
}

impl Reached {
    fn of(ptr: Ptr, counted: bool) -> Reached {
        Reached {
            hash: ptr.hash,
            offset: ptr.offset,
            times: i32::from(counted),
        }
    }

    fn ptr(&self) -> Ptr {
        Ptr {
            hash: self.hash,
            offset: self.offset,
        }
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
    counting: usize,
    new_roots: impl IntoIterator<Item = Ptr>,
) -> Result<Difference> {
    let mut found = Difference {
        known: Known {
            hashes: Hashes::new(),
            origins: Column::new(),
        },
        send: Sendable::new(store),
        links: HashMap::new(),
        cached: 0,
        moved: MOVED_BUDGET,
    };
    for (slot, root) in old_roots.iter().enumerate() {
        if found.known.find(&root.hash).is_none() {
            found.known.insert(root.hash, Origin::Root(slot as u32))?;
        }
    }
    let mut previous = old_roots
        .iter()
        .enumerate()
        .map(|(slot, root)| Reached::of(*root, slot == counting))
        .collect();
    for root in new_roots {
        found.step(store, previous, root)?;
        previous = vec![Reached::of(root, true)];
    }
    Ok(found)
}

impl Difference {
    fn classified(&self, ptr: Ptr) -> Result<bool> {
        Ok(self.known.find(&ptr.hash).is_some() || self.send.contains(ptr)?)
    }

    fn step(&mut self, store: &Store, mut old: Vec<Reached>, root: Ptr) -> Result<()> {
        let mut seen = HashMap::from([(root.hash, 1i32)]);
        let mut expanded = HashSet::new();
        let mut fresh = vec![root];
        let mut moved: Vec<Ptr> = Vec::new();
        let mut children = Vec::new();
        loop {
            let mut kept = 0;
            for at in 0..fresh.len() {
                if !self.classified(fresh[at])? {
                    fresh[kept] = fresh[at];
                    kept += 1;
                }
            }
            fresh.truncate(kept);
            if fresh.is_empty() {
                return Ok(());
            }
            if old.is_empty() {
                moved.reverse();
                while self.moved > 0
                    && let Some(ptr) = moved.pop()
                {
                    if !seen.contains_key(&ptr.hash) && expanded.insert(ptr.hash) {
                        self.expand(store, ptr, &mut moved, true)?;
                    }
                }
                return self.reach(store, fresh);
            }
            old.sort_unstable_by_key(|reached| reached.hash);
            old.dedup_by(|reached, kept| {
                let same = reached.hash == kept.hash;
                if same {
                    kept.times = kept.times.saturating_add(reached.times);
                }
                same
            });
            let mut linked = vec![false; old.len()];
            let mut starts = Vec::with_capacity(fresh.len() + 1);
            let mut next = Vec::new();
            for ptr in &fresh {
                starts.push(next.len());
                if ptr.offset == 0 {
                    continue;
                }
                for link in store.read_links(*ptr)? {
                    if let Ok(at) = old.binary_search_by_key(&link.hash, |reached| reached.hash) {
                        linked[at] = true;
                    }
                    next.push(link);
                }
            }
            starts.push(next.len());
            let mut deeper = Vec::new();
            for (at, reached) in old.drain(..).enumerate() {
                let common = seen.get_mut(&reached.hash).is_some_and(|count| {
                    *count = count.saturating_sub(reached.times);
                    *count >= 0
                });
                if linked[at] {
                    moved.push(reached.ptr());
                } else if !common && expanded.insert(reached.hash) {
                    self.expand(store, reached.ptr(), &mut children, false)?;
                    let counted = reached.times > 0;
                    deeper.extend(children.drain(..).map(|link| Reached::of(link, counted)));
                }
            }
            let mut below = Vec::new();
            for ptr in mem::take(&mut moved) {
                if self.moved == 0 {
                    break;
                }
                if !seen.contains_key(&ptr.hash) && expanded.insert(ptr.hash) {
                    self.expand(store, ptr, &mut below, true)?;
                }
            }
            moved = below;
            old = deeper;
            let tracking = !old.is_empty();
            let mut kept = 0;
            for (at, ptr) in fresh.drain(..).enumerate() {
                let (start, end) = (starts[at], starts[at + 1]);
                if self.classified(ptr)? {
                    continue;
                }
                self.send_object(ptr, &next[start..end])?;
                for index in start..end {
                    let link = next[index];
                    if tracking {
                        match seen.entry(link.hash) {
                            hash_map::Entry::Occupied(mut count) => {
                                *count.get_mut() = count.get().saturating_add(1);
                                continue;
                            }
                            hash_map::Entry::Vacant(count) => {
                                count.insert(1);
                            }
                        }
                    }
                    next[kept] = link;
                    kept += 1;
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
            if !self.classified(link)? {
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
            if self.classified(ptr)? {
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

struct Ordered {
    order: Vec<u64>,
    ids: HashMap<u32, usize>,
    hints: Vec<(usize, usize)>,
}

fn post_order(
    store: &Store,
    starts: &[Ptr],
    send: &mut Sendable,
    mut cached: HashMap<Hash, Vec<Ptr>>,
    old: &Known,
    anchors: usize,
) -> Result<Ordered> {
    let mut links_of = |ptr: Ptr, place: Place| {
        if place.bare {
            return Ok(Vec::new());
        }
        match cached.remove(&ptr.hash) {
            Some(links) => Ok(links),
            None => store.read_links(ptr),
        }
    };
    let mut found = Ordered {
        order: Vec::new(),
        ids: HashMap::new(),
        hints: Vec::new(),
    };
    let open = |place: Place| Place {
        state: if place.state == WAITING {
            OPEN
        } else {
            place.state
        },
        ..place
    };
    for start in starts {
        let Some(place) = send
            .update(*start, open)?
            .filter(|place| place.state == WAITING)
        else {
            continue;
        };
        let mut stack = vec![(*start, links_of(*start, place)?, 0usize)];
        while let Some((_, links, next)) = stack.last_mut() {
            if let Some(link) = links.get(*next).copied() {
                *next += 1;
                if link.offset == 0 {
                    continue;
                }
                let counted = |place: Place| Place {
                    position: place.position.saturating_add(1),
                    ..open(place)
                };
                if let Some(place) = send.update(link, counted)?
                    && place.state == WAITING
                {
                    stack.push((link, links_of(link, place)?, 0));
                }
                continue;
            }
            let (ptr, links, _) = stack.pop().expect("the stack has a top");
            for link in &links {
                if let Some(child) = old.child(&link.hash) {
                    hint_for(child, old, &mut found.ids, &mut found.hints, anchors);
                }
            }
            found.order.push(ptr.offset);
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
            Order::Sweep(Sweep::new(Swept::new())),
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
    let chosen = prefix.map(|(_, chosen)| chosen);
    let mut counting = 0;
    for (slot, base) in known.iter().enumerate() {
        if !base.tip && chosen != Some(slot) {
            continue;
        }
        let entry = Entry::load(store, base.ptr)?;
        let Some(root) = entry.commit.root else {
            continue;
        };
        let at = old_roots.iter().position(|old| old.hash == root.hash);
        if chosen == Some(slot) {
            counting = at.unwrap_or(old_roots.len());
        }
        if at.is_some() {
            continue;
        }
        header.roots.push(base_slot(slot, &mut header.bases));
        old_roots.push(root);
    }
    if let Some(chosen) = chosen {
        base_slot(chosen, &mut header.bases);
    }
    let extra = closures(store, &chain, &entries, merges, &known, &mut header.bases)?;
    if old_roots.is_empty() {
        entries.extend(extra);
        let reached = sweep(store, &entries)?;
        drop(entries);
        header.count = reached.len as u64;
        let order = Order::Sweep(Sweep::new(reached));
        return Ok(Emitter::new(store, &header, order, HashMap::new()));
    }
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
    } = difference(store, &old_roots, counting, new_roots)?;
    entries.extend(extra);
    for sending in &entries {
        if !send.contains(sending.ptr)? {
            send.insert(sending.ptr, false)?;
        }
    }
    entries.sort_by_key(|sending| (sending.generation, sending.ptr.offset));
    let starts: Vec<Ptr> = entries.iter().map(|sending| sending.ptr).collect();
    drop(entries);
    let Ordered { order, ids, hints } =
        post_order(store, &starts, &mut send, links, &old, old_roots.len())?;
    drop(starts);
    let count = order.len() as u64;
    if count != send.len() as u64 {
        return Err(Error::Corrupt(
            "sync found objects its head does not reach".into(),
        ));
    }
    header.hints = hints;
    header.count = count;
    let mut references = HashMap::with_capacity(old_roots.len() + ids.len());
    for (slot, root) in old_roots.iter().enumerate() {
        references.entry(root.hash).or_insert(1 + slot);
    }
    for (id, hint) in ids {
        references.insert(old.hashes.hash(id), 1 + old_roots.len() + hint);
    }
    drop(old);
    let order = Order::Walk(Box::new(Walk {
        send,
        order,
        next: 0,
    }));
    Ok(Emitter::new(store, &header, order, references))
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
        let mut held: Option<(u64, usize)> = None;
        for (slot, base) in known.iter().enumerate() {
            if let Some(shared) = shared_prefix(store, &reach, &base.chain)?
                && held.is_none_or(|(best, _)| shared > best)
            {
                held = Some((shared, slot));
            }
        }
        if let Some((shared, slot)) = held
            && shared + 1 >= low
            && !marked(&known[slot].mark.hash)
        {
            bases.push(known[slot].mark);
        }
        let stop = held.map_or(low, |(held, _)| low.max(held + 1)).min(version);
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

fn check_graph(
    own: Facts,
    link: Option<Merged>,
    prev: Option<Option<Facts>>,
    merged: Option<Option<Facts>>,
) -> Result<()> {
    if disagreement(own, link, prev, merged).is_some() {
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
    hash: Hash,
    segment: u32,
    facts: Facts,
    root: Option<Ptr>,
}

impl Sent {
    fn of(id: u32, hash: Hash, entry: &Entry, segment: u32) -> Sent {
        Sent {
            id,
            hash,
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

    fn found(&self) -> Vec<(Hash, u32)> {
        let mut found: Vec<(Hash, u32)> = self.iter().map(|sent| (sent.hash, sent.id)).collect();
        found.sort_unstable();
        found
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
    let mut decoder = Decoder::new(&object.body, &object.links, None);
    let entry = Entry::decode(&mut decoder).map_err(|error| {
        bad(format!(
            "an object it links as a commit is not one: {error}"
        ))
    })?;
    if decoder.remaining() != 0 {
        return Err(bad("a commit carries bytes past its end"));
    }
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
    fn find(&self, ptr: Ptr) -> Option<u32>;
    fn head(&self, hash: &Hash) -> Option<u32>;
    fn at(&self, id: u32, hash: Hash) -> Ptr;
    fn read_object(&self, ptr: Ptr) -> Result<Object>;
    fn relink(&self, object: Object, hash: Hash) -> Result<()>;

    fn staging(&self) -> Option<&Staging> {
        None
    }
}

impl Objects for Staging {
    fn find(&self, ptr: Ptr) -> Option<u32> {
        Staging::find(self, ptr)
    }

    fn head(&self, hash: &Hash) -> Option<u32> {
        let last = u32::try_from(self.len()).ok()?.checked_sub(1)?;
        (self.last() == Some(*hash)).then_some(last)
    }

    fn at(&self, id: u32, hash: Hash) -> Ptr {
        Ptr {
            hash,
            offset: self.offset(id),
        }
    }

    fn read_object(&self, ptr: Ptr) -> Result<Object> {
        Staging::read_object(self, ptr)
    }

    fn relink(&self, _: Object, _: Hash) -> Result<()> {
        Ok(())
    }

    fn staging(&self) -> Option<&Staging> {
        Some(self)
    }
}

struct Placed<'a> {
    txn: RefCell<&'a mut Txn>,
    earlier: Earlier<'a>,
    commits: &'a [(Hash, u32)],
}

#[derive(Clone, Copy)]
struct Earlier<'a> {
    staging: &'a Staging,
    placed: Option<&'a Column<u64>>,
}

impl Earlier<'_> {
    fn offset(&self, id: u32) -> Option<u64> {
        match self.placed {
            None => Some(self.staging.offset(id)),
            Some(placed) => ((id as usize) < placed.len()).then(|| placed.get(id)),
        }
    }
}

impl Objects for Placed<'_> {
    fn find(&self, ptr: Ptr) -> Option<u32> {
        let at = self
            .commits
            .binary_search_by(|(hash, _)| hash.cmp(&ptr.hash))
            .ok()?;
        Some(self.commits[at].1)
    }

    fn head(&self, hash: &Hash) -> Option<u32> {
        self.earlier.staging.head(hash)
    }

    fn at(&self, id: u32, hash: Hash) -> Ptr {
        let offset = self
            .txn
            .borrow()
            .offset_of(&hash)
            .or_else(|| self.earlier.offset(id));
        Ptr {
            hash,
            offset: offset.expect("a sent object has an offset"),
        }
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
}

fn read_commit(objects: &impl Objects, id: u32, hash: Hash) -> Result<Entry> {
    decode_commit(&objects.read_object(objects.at(id, hash))?)
}

fn mark_linked(linked: &mut Option<Column<u8>>, id: u32) {
    if let Some(links) = linked.as_mut() {
        links.set(id, links.get(id).max(1));
    }
}

struct Landing<'a> {
    store: &'a Store,
    low: u64,
    segments: Vec<Segment>,
    commits: Commits,
}

impl Landing<'_> {
    fn sent(&self, objects: &impl Objects, ptr: Ptr) -> Option<Sent> {
        self.commits.get(objects.find(ptr)?)
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

    fn facts(&self, objects: &impl Objects, ptr: Ptr) -> Result<Option<Facts>> {
        if ptr.offset == 0 {
            return Ok(None);
        }
        match self.sent(objects, ptr) {
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
            Found::Sent(id)
                if self
                    .commits
                    .get(id)
                    .is_some_and(|sent| sent.hash == want.hash) =>
            {
                mark_linked(linked, id);
                Ok(objects.at(id, want.hash))
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

    fn segment(
        &mut self,
        refs: &Refs,
        objects: &impl Objects,
        (top, hash): (u32, Hash),
    ) -> Result<Vec<Ptr>> {
        let segment = self.segments.len() as u32;
        let mut versions = Vec::new();
        let mut bottom;
        let mut merged = Vec::new();
        let (mut at, mut expected, mut hash) = (top, None, hash);
        let below = loop {
            let entry = read_commit(objects, at, hash)?;
            let version = entry.commit.version;
            let skipped = expected.is_some_and(|want| want != version);
            if version == 0 || version > MAX_VERSION || skipped {
                return Err(bad(
                    "a merged chain's versions do not descend one at a time",
                ));
            }
            let id = at;
            versions.push(id);
            bottom = version;
            merged.extend(entry.merged.map(|link| link.ptr));
            let prev = entry.prev;
            self.commits.insert(Sent::of(id, hash, &entry, segment));
            let Some(prev) = prev else {
                if version != 1 {
                    return Err(bad(format!("v{version} has no parent")));
                }
                break Below::Gone;
            };
            if let Some(sent) = self.sent(objects, prev) {
                if sent.facts.version != version - 1 {
                    return Err(bad("a merged chain joins another at the wrong version"));
                }
                break Below::Joins(sent.id);
            }
            if let Some(found) = objects.find(prev) {
                (at, expected, hash) = (found, Some(version - 1), prev.hash);
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
    shape: Shape,
    kinds: Vec<Kind>,
}

enum Beneath {
    Check(Ptr, Option<Kind>, Kind),
    Settle(Ptr, Kind),
}

#[derive(Clone, Copy)]
struct Walked {
    at: u64,
    count: u64,
    used: u32,
    ty: u16,
    level: u8,
}

fn walked_at(at: u64, ty: u16) -> u64 {
    let product = u128::from(at ^ (u64::from(ty) << 48)) * 0x9e37_79b9_7f4a_7c15;
    product as u64 ^ (product >> 64) as u64
}

#[derive(Default)]
struct Retyped {
    walked: HashTable<Walked>,
    types: Vec<TypeId>,
    full: usize,
    epoch: u32,
    since: usize,
}

impl Retyped {
    fn get(&mut self, ptr: Ptr, kind: &Kind) -> Option<Shape> {
        let ty = u16::try_from(self.types.iter().position(|id| *id == kind.id)?).ok()?;
        let epoch = self.epoch;
        let walked = self.walked.find_mut(walked_at(ptr.offset, ty), |walked| {
            walked.at == ptr.offset && walked.ty == ty
        })?;
        walked.used = epoch;
        Some(Shape {
            count: walked.count,
            level: walked.level,
            deferred: false,
        })
    }

    fn insert(&mut self, ptr: Ptr, kind: &Kind, shape: Shape) {
        if ptr.offset == 0 {
            return;
        }
        let ty = match self.types.iter().position(|id| *id == kind.id) {
            Some(at) => at,
            None => {
                self.types.push(kind.id);
                self.types.len() - 1
            }
        };
        let Ok(ty) = u16::try_from(ty) else {
            return;
        };
        self.room();
        self.since += 1;
        if self.since >= HELD_EPOCH {
            self.since = 0;
            self.epoch = self.epoch.wrapping_add(1);
        }
        let walked = Walked {
            at: ptr.offset,
            count: shape.count,
            used: self.epoch,
            ty,
            level: shape.level,
        };
        match self.walked.entry(
            walked_at(walked.at, ty),
            |seen| seen.at == walked.at && seen.ty == ty,
            |seen| walked_at(seen.at, seen.ty),
        ) {
            table::Entry::Occupied(mut seen) => *seen.get_mut() = walked,
            table::Entry::Vacant(free) => {
                free.insert(walked);
            }
        }
    }

    fn room(&mut self) {
        if self.walked.len() < self.walked.capacity() {
            return;
        }
        if self.full < HELD_KEPT {
            self.full = (self.full * 2).clamp(HELD_FIRST / 8 * 7, HELD_KEPT);
            let more = self.full.saturating_sub(self.walked.len());
            self.walked
                .reserve(more, |seen| walked_at(seen.at, seen.ty));
        } else if self.walked.len() >= HELD_KEPT / 2 {
            self.evict();
        }
    }

    fn evict(&mut self) {
        let epoch = self.epoch;
        let class = |walked: &Walked| {
            let age = epoch.wrapping_sub(walked.used) as usize;
            (
                usize::from(walked.level).min(HELD_AGES - 1),
                age.min(HELD_AGES - 1),
            )
        };
        let mut counts = vec![[0usize; HELD_AGES]; HELD_AGES];
        for walked in &self.walked {
            let (level, age) = class(walked);
            counts[level][age] += 1;
        }
        let mut left = self.walked.len() / 2 + 1;
        let mut cut = (HELD_AGES, 0);
        'levels: for (level, ages) in counts.iter().enumerate() {
            for age in (0..HELD_AGES).rev() {
                left = left.saturating_sub(ages[age]);
                if left == 0 {
                    cut = (level, age);
                    break 'levels;
                }
            }
        }
        self.walked.retain(|walked| {
            let (level, age) = class(walked);
            level > cut.0 || (level == cut.0 && age < cut.1)
        });
    }
}

struct Vetting<'a> {
    store: &'a Store,
    staging: &'a Staging,
    links: &'a Column<u8>,
    anchors: &'a Anchors,
    held_at: &'a Index,
    root_kind: &'a Kind,
    pair_roots: PairRoots,
    kept: HashMap<u32, Vec<Kept>>,
    held: Retyped,
    beneath: Vec<Beneath>,
    spent: usize,
    budget: usize,
    queue: Vec<(Ptr, Kind, Option<Kind>)>,
    deferred: Vec<(Ptr, Kind)>,
    tables: Tables,
    confirmed: HashSet<(Hash, TypeId, TypeId, Hash)>,
    carried: usize,
    rerun_budget: usize,
    zero_width_budget: u64,
    view: OnceCell<Store>,
    views: RefCell<Vec<Store>>,
    walked: Vec<u64>,
    credited: u64,
    flushed: Cell<u64>,
    recalled: Option<Recalled>,
}

struct Walking<'w, 'a> {
    vetting: RefCell<&'w mut Vetting<'a>>,
    links: &'w StagedLinks<'w>,
    floor: usize,
    failed: RefCell<Option<Error>>,
}

impl Walking<'_, '_> {
    fn check(&self, found: &mut Vec<(usize, Kind)>) -> Result<()> {
        if found.is_empty() {
            return Ok(());
        }
        let mut vetting = self.vetting.borrow_mut();
        for (index, kind) in found.drain(..) {
            let link = self.links.link(index)?;
            vetting.queue.push((link, kind, None));
        }
        stack::with_room(|| vetting.run_above(self.floor))
            .unwrap_or_else(|| Err(bad("the objects it links nest too deep to check")))
    }
}

impl Walks for Walking<'_, '_> {
    fn walk(&self, found: &mut Vec<(usize, Kind)>) -> Result<()> {
        let checked = match self.failed.borrow().is_some() {
            true => Err(bad("checking what it links stopped")),
            false => self.check(found),
        };
        found.clear();
        if let Err(error) = checked {
            self.failed.borrow_mut().get_or_insert(error);
            return Err(bad("checking what it links stopped"));
        }
        Ok(())
    }
}

fn check_staged(
    kind: &Kind,
    body: &[u8],
    links: &StagedLinks<'_>,
    tables: Option<&RefCell<Vec<FoundTable>>>,
) -> Result<(Shape, u64)> {
    let checked = kind.check_reading(body, &Finding::reading(links, None), tables);
    links.failure().map_or(checked, Err)
}

fn rejected(error: Error) -> Error {
    match error {
        Error::Decode(message) => bad(message),
        other => other,
    }
}

fn retyped(kind: &Kind, error: Error) -> Error {
    match error {
        Error::Decode(message) => bad(format!(
            "it links as a {} to an object this database holds as another type, which does not \
             check as one: {message}",
            kind.name
        )),
        other => other,
    }
}

fn overread() -> Error {
    bad(
        "checking that the indexes of the tables it carries agree with their rows reads more than \
         its size allows",
    )
}

fn disagreeing(table: &FoundTable, error: Error) -> Error {
    match error {
        Error::Decode(message) => bad(format!(
            "a table it carries ({}) has indexes that do not agree with its rows: {message}",
            table.name
        )),
        other => other,
    }
}

fn undecodable(kind: &Kind, error: Error) -> Error {
    match error {
        Error::Io(_)
        | Error::Poisoned
        | Error::Replaced
        | Error::Interrupted
        | Error::Bundle(_) => error,
        error => bad(format!(
            "a {} it carries does not decode: {error}",
            kind.name
        )),
    }
}

impl Vetting<'_> {
    fn run(&mut self) -> Result<()> {
        self.run_above(0)
    }

    fn run_above(&mut self, floor: usize) -> Result<()> {
        while self.queue.len() > floor
            && let Some((ptr, kind, before)) = self.queue.pop()
        {
            let shape = match self.staging.find(ptr) {
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

    fn sent(
        &mut self,
        id: u32,
        hash: Hash,
        kind: &Kind,
        mut before: Option<Kind>,
    ) -> Result<Shape> {
        let links = self.links.get(id);
        let pinned = links == u8::MAX || self.anchors.find(self.held_at, &hash).is_some();
        let shared = links >= 2 || pinned;
        let counted = before.is_none();
        if shared
            && let Some(checks) = self.kept.get_mut(&id)
            && let Some(at) = checks.iter().position(|kept| kept.kinds[0].id == kind.id)
        {
            let kept = &mut checks[at];
            if counted && kept.left != u8::MAX {
                kept.left = kept.left.saturating_sub(1);
            }
            let hit = kept
                .kinds
                .iter()
                .any(|seen| (seen.within)(seen, kind))
                .then(|| {
                    (
                        kept.shape,
                        kept.kinds.iter().any(|seen| kind.vouched_by(seen)),
                    )
                });
            if let Some((shape, vouched)) = hit {
                if kept.left == 0 {
                    checks.swap_remove(at);
                    if checks.is_empty() {
                        self.kept.remove(&id);
                    }
                }
                if !vouched {
                    self.confirm(id, hash, kind)?;
                }
                return Ok(shape);
            }
            if before.is_none() {
                before = kept.kinds.last().cloned();
            }
        }
        let ptr = Ptr {
            hash,
            offset: self.staging.offset(id),
        };
        let object = match self.staging.open(ptr, LINKS_HELD)? {
            Opened::Whole(object) => object,
            Opened::Body(body, staged) => {
                let shape = self.dense(id, ptr, kind, &body, &staged)?;
                self.keep(id, kind, shape, (links, pinned, counted));
                return Ok(shape);
            }
        };
        self.spend(object.body.len())?;
        let tables = RefCell::new(Vec::new());
        let (shape, found, zero_width) = kind
            .check_tables(&object.body, &object.links, &tables)
            .map_err(|error| undecodable(kind, error))?;
        self.credit(id, object.links.len());
        self.tables.note(tables.into_inner());
        self.tally(zero_width)?;
        if shape.deferred {
            self.deferred.push((ptr, kind.clone()));
        }
        let earlier = match before.filter(|_| !found.is_empty()) {
            Some(old) => {
                self.spend(object.body.len())?;
                let (_, earlier, zero_width) = old
                    .check(&object.body, &object.links, None)
                    .map_err(|error| undecodable(&old, error))?;
                self.tally(zero_width)?;
                earlier
            }
            None => Vec::new(),
        };
        let mut earlier = earlier.into_iter().peekable();
        for (index, child) in found {
            while earlier.next_if(|(at, _)| *at < index).is_some() {}
            let covered = earlier
                .next_if(|(at, _)| *at == index)
                .map(|(_, kind)| kind);
            if covered.as_ref().is_some_and(|old| old.implies(&child)) {
                continue;
            }
            self.queue.push((object.links[index], child, covered));
        }
        self.keep(id, kind, shape, (links, pinned, counted));
        Ok(shape)
    }

    fn keep(
        &mut self,
        id: u32,
        kind: &Kind,
        shape: Shape,
        (links, pinned, counted): (u8, bool, bool),
    ) {
        if links < 2 && !pinned {
            return;
        }
        let left = if pinned {
            u8::MAX
        } else {
            links - u8::from(counted)
        };
        let checks = self.kept.entry(id).or_default();
        match checks.iter().position(|kept| kept.kinds[0].id == kind.id) {
            Some(at) if checks[at].left == 0 => {
                checks.swap_remove(at);
            }
            Some(at) => {
                let kinds = &mut checks[at].kinds;
                if kinds.len() >= RANGES_KEPT {
                    kinds.remove(0);
                }
                kinds.push(kind.clone());
            }
            None => checks.push(Kept {
                left,
                shape,
                kinds: vec![kind.clone()],
            }),
        }
        if checks.is_empty() {
            self.kept.remove(&id);
        }
    }

    fn dense(
        &mut self,
        id: u32,
        ptr: Ptr,
        kind: &Kind,
        body: &[u8],
        links: &StagedLinks<'_>,
    ) -> Result<Shape> {
        self.spend(body.len())?;
        self.credit(id, links.count());
        let tables = RefCell::new(Vec::new());
        let floor = self.queue.len();
        let walking = Walking {
            vetting: RefCell::new(self),
            links,
            floor,
            failed: RefCell::new(None),
        };
        let found = Finding::reading(links, Some(&walking));
        let checked = kind.check_reading(body, &found, Some(&tables));
        let failure = links.failure();
        let mut rest = found.into_found();
        let checked = checked.and_then(|checked| walking.walk(&mut rest).map(|()| checked));
        if let Some(error) = walking.failed.into_inner().or(failure) {
            return Err(error);
        }
        let (shape, zero_width) = checked.map_err(|error| undecodable(kind, error))?;
        self.tables.note(tables.into_inner());
        self.tally(zero_width)?;
        if shape.deferred {
            self.deferred.push((ptr, kind.clone()));
        }
        Ok(shape)
    }

    fn spend(&mut self, bytes: usize) -> Result<()> {
        self.spent = self.spent.saturating_add(bytes);
        if self.spent > self.budget {
            return Err(bad("it reuses its objects under too many key ranges"));
        }
        Ok(())
    }

    fn confirm(&mut self, id: u32, hash: Hash, kind: &Kind) -> Result<()> {
        if !self.remember(hash, kind) {
            return Ok(());
        }
        let ptr = Ptr {
            hash,
            offset: self.staging.offset(id),
        };
        let checked = match self.staging.open(ptr, LINKS_HELD)? {
            Opened::Whole(object) => {
                self.rerun(object.body.len())?;
                kind.check(&object.body, &object.links, None)
                    .map(|(shape, _, zero_width)| (shape, zero_width))
            }
            Opened::Body(body, links) => {
                self.rerun(body.len())?;
                check_staged(kind, &body, &links, None)
            }
        };
        let (shape, zero_width) = checked.map_err(|error| undecodable(kind, error))?;
        self.tally(zero_width)?;
        if shape.deferred {
            self.deferred.push((ptr, kind.clone()));
        }
        Ok(())
    }

    fn remember(&mut self, hash: Hash, kind: &Kind) -> bool {
        let Some(claim) = kind.claim.as_deref() else {
            return false;
        };
        let Some(print) = claim.fingerprint() else {
            return true;
        };
        let memo = (hash, kind.id, (claim as &dyn Any).type_id(), print);
        if self.confirmed.contains(&memo) {
            return false;
        }
        if self.confirmed.len() < CONFIRMED_KEPT {
            self.confirmed.insert(memo);
        }
        true
    }

    fn view(&self) -> &Store {
        self.view.get_or_init(|| self.fresh_view())
    }

    fn fresh_view(&self) -> Store {
        self.flush();
        let allowance = (self.rerun_budget as u64)
            .saturating_add(self.credited.saturating_mul(RERUN_PER_LINK as u64));
        let view = self
            .store
            .view(self.carried as u64, allowance, self.zero_width_budget);
        self.views.borrow_mut().push(view.clone());
        view
    }

    fn credit(&mut self, id: u32, links: usize) {
        let (word, bit) = (id as usize / 64, 1u64 << (id % 64));
        if self.walked[word] & bit != 0 {
            return;
        }
        self.walked[word] |= bit;
        self.credited = self.credited.saturating_add(links as u64);
        if self.credited - self.flushed.get() >= CREDIT_BATCH {
            self.flush();
        }
    }

    fn flush(&self) {
        let links = self.credited - self.flushed.get();
        if links == 0 {
            return;
        }
        self.flushed.set(self.credited);
        let bytes = links.saturating_mul(RERUN_PER_LINK as u64);
        for view in self.views.borrow().iter() {
            view.credit(bytes);
        }
    }

    fn tally(&self, zero_width: u64) -> Result<()> {
        if zero_width == 0 {
            return Ok(());
        }
        self.view().count_zero_width(zero_width)
    }

    fn rerun(&self, bytes: usize) -> Result<()> {
        self.view()
            .charge(bytes)
            .map_err(|_| bad("it spills values under too many keys to recompute their summaries"))
    }

    fn recompute_held(&self, kind: &Kind, ptr: Ptr) -> Result<Shape> {
        let view = self.view();
        let shape = (kind.held)(kind, view, ptr);
        if let Some(error) = view.overdrawn() {
            return Err(error);
        }
        shape.map_err(rejected)
    }

    fn settle(&mut self) -> Result<()> {
        let deferred = mem::take(&mut self.deferred);
        if deferred.is_empty() {
            return Ok(());
        }
        let (view, recalled, staging) = (self.view(), self.recalled.as_ref(), self.staging);
        staging.overlaid(view, || {
            for (ptr, kind) in &deferred {
                let checked = match staging.open(*ptr, LINKS_HELD)? {
                    Opened::Whole(object) => {
                        kind.recheck(&object.body, &object.links, view, recalled)
                    }
                    Opened::Body(body, links) => {
                        let found = Finding::reading(&links, None);
                        kind.recheck_reading(&body, &found, view, recalled)
                    }
                };
                if let Some(error) = view.overdrawn() {
                    return Err(error);
                }
                let (_, zero_width) = checked.map_err(|error| undecodable(kind, error))?;
                view.count_zero_width(zero_width)?;
            }
            Ok(())
        })
    }

    fn tables_of(&self, root: Ptr) -> Result<(Vec<FoundTable>, bool)> {
        let (tables, held, zero_width) = self.root_tables(root)?;
        self.tally(zero_width)?;
        Ok((tables, held))
    }

    fn root_tables(&self, root: Ptr) -> Result<(Vec<FoundTable>, bool, u64)> {
        let tables = RefCell::new(Vec::new());
        let (held, zero_width) = match self.staging.find(root) {
            Some(_) => {
                let checked = match self.staging.open(root, LINKS_HELD)? {
                    Opened::Whole(object) => self
                        .root_kind
                        .check_tables(&object.body, &object.links, &tables)
                        .map(|(shape, _, zero_width)| (shape, zero_width)),
                    Opened::Body(body, links) => {
                        check_staged(self.root_kind, &body, &links, Some(&tables))
                    }
                };
                let (_, zero_width) =
                    checked.map_err(|error| undecodable(self.root_kind, error))?;
                (false, zero_width)
            }
            None => {
                let object = self.store.read_object(root)?;
                self.root_kind
                    .check_tables(&object.body, &object.links, &tables)
                    .map_err(rejected)?;
                (true, 0)
            }
        };
        Ok((tables.into_inner(), held, zero_width))
    }

    fn plan(
        &self,
        plans: &mut Plans,
        listed: &mut HashSet<Hash>,
        (root, parent): &Descent,
        tables_of: impl Fn(Ptr) -> Result<(Vec<FoundTable>, bool)>,
        paired: &mut HashMap<Hash, Paired>,
        view: &Store,
    ) -> Result<Vec<FoundTable>> {
        if self.staging.find(*root).is_none() || !listed.insert(root.hash) {
            return Ok(Vec::new());
        }
        let Some((parent, kept)) = parent else {
            return Ok(Vec::new());
        };
        let (base, held) = tables_of(*parent)?;
        for table in base.iter().filter(|_| held) {
            self.vouch(plans, table, *kept);
        }
        let found = match paired.entry(root.hash) {
            hash_map::Entry::Occupied(found) => found.into_mut(),
            hash_map::Entry::Vacant(slot) => {
                let mut pairing = Pairing::new(view, *kept);
                (self.pair_roots)(&mut pairing, *parent, *root)?;
                slot.insert(pairing.into_paired())
            }
        };
        if *kept {
            for identity in &found.same {
                plans.trust(*identity);
            }
        }
        let mut planned = Vec::new();
        for (old, new) in &found.pairs {
            self.vouch(plans, old, *kept);
            if plans.from(new, old) {
                planned.push(new.clone());
            }
        }
        Ok(planned)
    }

    fn vouch(&self, plans: &mut Plans, table: &FoundTable, kept: bool) {
        if kept || !self.tables.holds(table) {
            plans.trust(table.identity());
        } else {
            plans.build(table);
        }
    }

    fn walk(&mut self, roots: &[Ptr], mut before: impl FnMut(&Self, usize)) -> Result<()> {
        for (at, root) in roots.iter().enumerate() {
            before(self, at);
            self.queue.push((*root, self.root_kind.clone(), None));
            self.run()?;
            self.flush();
        }
        self.settle()
    }

    fn early<'p>(&self, early: &mut Early, descent: &Descent, pool: &Pool<'p>, view: &'p Store) {
        if !early.on {
            return;
        }
        let planned = self.plan(
            &mut early.plans,
            &mut early.listed,
            descent,
            |root| {
                self.root_tables(root)
                    .map(|(tables, held, _)| (tables, held))
            },
            &mut early.paired,
            view,
        );
        match planned {
            Ok(tables) => {
                for table in &tables {
                    if table.beside
                        && let Some(Some(base)) = early.plans.base(table)
                    {
                        early.checks.start(pool, view, table, Some(base));
                    }
                }
            }
            Err(_) => early.on = false,
        }
    }

    fn agree<'p>(
        &mut self,
        lineage: &[Descent],
        (checks, mut paired): (Checks, HashMap<Hash, Paired>),
        pool: &Pool<'p>,
        view: &'p Store,
    ) -> Result<()> {
        let mut plans = Plans::default();
        let mut listed = HashSet::new();
        for descent in lineage {
            let planned = self.plan(
                &mut plans,
                &mut listed,
                descent,
                |root| self.tables_of(root),
                &mut paired,
                view,
            );
            if let Err(error) = planned {
                return Err(if view.out_of_reads() {
                    overread()
                } else {
                    rejected(error)
                });
            }
        }
        let found = mem::take(&mut self.tables).into_found();
        let failed = checks.finish(pool, view, &plans, &found);
        if view.out_of_reads() {
            return Err(overread());
        }
        if let Some(error) = view.overdrawn() {
            return Err(error);
        }
        match failed {
            Some((table, error)) => Err(disagreeing(table, error)),
            None => Ok(()),
        }
    }

    fn held(&mut self, hash: Hash, reference: usize, kind: &Kind) -> Result<Option<Shape>> {
        let roots = self.anchors.roots.len();
        let (ptr, first, also) = if reference <= roots {
            (
                self.anchors.roots[reference - 1],
                Some(self.root_kind.clone()),
                Vec::new(),
            )
        } else {
            let id = reference - 1 - roots;
            let kinds = &self.anchors.hint_kinds;
            (self.anchors.hinted[id], kinds.get(id), kinds.also(id))
        };
        if ptr.hash != hash {
            return Err(bad("a link does not match the object it names"));
        }
        if first.iter().chain(&also).any(|known| known.implies(kind)) {
            return Ok(None);
        }
        let known = first
            .iter()
            .chain(&also)
            .find(|known| known.id == kind.id)
            .or(first.as_ref())
            .or(also.first());
        let shape = self.held_as(ptr, known, kind)?;
        while let Some(step) = self.beneath.pop() {
            match step {
                Beneath::Check(ptr, known, kind) => {
                    if let Some(shape) = self.held_as(ptr, known.as_ref(), &kind)?
                        && !kind.fits(shape)
                    {
                        return Err(bad(format!(
                            "a {} it links to holds {} entries at level {}, not what the link claims",
                            kind.name, shape.count, shape.level
                        )));
                    }
                }
                Beneath::Settle(ptr, kind) => {
                    let object = self.store.read_object(ptr)?;
                    let view = self.view();
                    let checked =
                        kind.recheck(&object.body, &object.links, view, self.recalled.as_ref());
                    if let Some(error) = view.overdrawn() {
                        return Err(error);
                    }
                    let (_, zero_width) = checked.map_err(|error| retyped(&kind, error))?;
                    self.tally(zero_width)?;
                }
            }
        }
        Ok(shape)
    }

    fn walking(&self, error: Error) -> Error {
        if self.view().out_of_reads() {
            return bad(
                "checking held data it links as another type reads more than its size allows",
            );
        }
        error
    }

    fn held_as(&mut self, ptr: Ptr, known: Option<&Kind>, kind: &Kind) -> Result<Option<Shape>> {
        let hash = ptr.hash;
        if known.is_some_and(|known| known.implies(kind)) {
            return Ok(None);
        }
        if let Some(known) = known.filter(|known| known.id == kind.id) {
            if !kind.vouched_by(known) && self.remember(hash, kind) {
                return self.recompute_held(kind, ptr).map(Some);
            }
            let unclaimed = kind.unclaimed();
            return (unclaimed.held)(&unclaimed, self.store, ptr)
                .map(Some)
                .map_err(rejected);
        }
        if let Some(shape) = self.held.get(ptr, kind) {
            let claimed = kind.claim.is_some() && self.remember(hash, kind);
            let ranged =
                kind.range.is_some() && !known.is_some_and(|known| (kind.within)(known, kind));
            if claimed || ranged {
                let kind = if claimed {
                    kind.clone()
                } else {
                    kind.unclaimed()
                };
                let view = self.view();
                let checked = (kind.held)(&kind, view, ptr);
                if let Some(error) = view.overdrawn() {
                    return Err(if claimed { error } else { self.walking(error) });
                }
                checked.map_err(|error| retyped(&kind, error))?;
            }
            return Ok(Some(shape));
        }
        let (object, charged) = self
            .view()
            .read_fresh(ptr)
            .map_err(|error| self.walking(rejected(error)))?;
        let tables = RefCell::new(Vec::new());
        let (shape, found, zero_width) = kind
            .check_tables(&object.body, &object.links, &tables)
            .map_err(|error| retyped(kind, error))?;
        self.tables.note(tables.into_inner());
        self.tally(zero_width)?;
        if kind.claim.is_some() {
            self.remember(hash, kind);
        }
        if shape.deferred {
            self.beneath.push(Beneath::Settle(ptr, kind.clone()));
        }
        if !found.is_empty() {
            let (held, zero_width) = known
                .and_then(|known| {
                    known
                        .unclaimed()
                        .check(&object.body, &object.links, None)
                        .ok()
                })
                .map_or_else(
                    || (Vec::new(), 0),
                    |(_, found, zero_width)| (found, zero_width),
                );
            if charged {
                self.tally(zero_width)?;
            }
            let mut held = held.into_iter().peekable();
            for (index, child) in found {
                while held.next_if(|(at, _)| *at < index).is_some() {}
                let known = held.next_if(|(at, _)| *at == index).map(|(_, kind)| kind);
                self.beneath
                    .push(Beneath::Check(object.links[index], known, child));
            }
        }
        self.held.insert(ptr, kind, shape);
        Ok(Some(shape))
    }
}

#[derive(Clone)]
struct Held {
    template: u16,
    count: Option<u64>,
    below: Option<u8>,
    range: Option<Arc<dyn Any + Send + Sync>>,
    claim: Option<Arc<dyn Claim>>,
}

#[derive(Default)]
struct Kinds {
    templates: Vec<Kind>,
    held: Vec<Held>,
    also: HashMap<usize, Vec<Held>>,
}

impl Kinds {
    fn new(len: usize) -> Kinds {
        let none = Held {
            template: u16::MAX,
            count: None,
            below: None,
            range: None,
            claim: None,
        };
        Kinds {
            templates: Vec::new(),
            held: vec![none; len],
            also: HashMap::new(),
        }
    }

    fn keep(&mut self, kind: Kind) -> Held {
        let template = match self.templates.iter().position(|seen| seen.id == kind.id) {
            Some(at) => at,
            None => {
                let mut template = kind.unclaimed();
                (template.count, template.below, template.range) = (None, None, None);
                self.templates.push(template);
                self.templates.len() - 1
            }
        };
        Held {
            template: u16::try_from(template).unwrap_or(u16::MAX),
            count: kind.count,
            below: kind.below,
            range: kind.range,
            claim: kind.claim,
        }
    }

    fn set(&mut self, id: usize, kind: Kind) {
        self.held[id] = self.keep(kind);
    }

    fn add(&mut self, id: usize, kind: Kind) {
        let held = self.keep(kind);
        self.also.entry(id).or_default().push(held);
    }

    fn get(&self, id: usize) -> Option<Kind> {
        self.kind(&self.held[id])
    }

    fn also(&self, id: usize) -> Vec<Kind> {
        if self.also.is_empty() {
            return Vec::new();
        }
        self.also.get(&id).map_or_else(Vec::new, |also| {
            also.iter().filter_map(|held| self.kind(held)).collect()
        })
    }

    #[inline(never)]
    fn also_from(
        &mut self,
        id: usize,
        hash: Hash,
        primary: Option<&Kind>,
        found: &[(usize, Kind)],
        more: &[Vec<(usize, Kind)>],
        links: &[Ptr],
    ) {
        let mut types: Vec<TypeId> = Vec::new();
        for (at, kind) in found.iter().chain(more.iter().flatten()) {
            if links[*at].hash != hash
                || primary.is_some_and(|primary| primary.id == kind.id)
                || types.contains(&kind.id)
            {
                continue;
            }
            if types.len() + usize::from(primary.is_some()) >= HINT_KINDS {
                break;
            }
            types.push(kind.id);
            self.add(id, kind.clone());
        }
    }

    fn identity(&self, id: usize) -> (u16, Option<u64>, Option<u8>, usize) {
        let held = &self.held[id];
        let range = held
            .range
            .as_ref()
            .map_or(0, |range| Arc::as_ptr(range).cast::<()>() as usize);
        (held.template, held.count, held.below, range)
    }

    fn kind(&self, held: &Held) -> Option<Kind> {
        let mut kind = self.templates.get(usize::from(held.template))?.clone();
        kind.count = held.count;
        kind.below = held.below;
        kind.range = held.range.clone();
        kind.claim = held.claim.clone();
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

fn linked_twice(found: &[(usize, Kind)], links: &[Ptr]) -> bool {
    let Some((_, first)) = found.first() else {
        return false;
    };
    if found.iter().all(|(_, kind)| kind.id == first.id) {
        return false;
    }
    let mut typed: Vec<(&Hash, TypeId)> = found
        .iter()
        .map(|(at, kind)| (&links[*at].hash, kind.id))
        .collect();
    typed.sort_unstable_by_key(|(hash, _)| *hash);
    typed
        .windows(2)
        .any(|pair| pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1)
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
            let (first, also) = match header.hints[order[at] as usize].0.checked_sub(roots.len()) {
                None => (Some(root_kind.clone()), Vec::new()),
                Some(parent) => (hint_kinds.get(parent), hint_kinds.also(parent)),
            };
            let found_as = |kind: &Kind| {
                kind.unclaimed()
                    .check(&object.body, &object.links, None)
                    .map(|(_, found, _)| found)
                    .unwrap_or_default()
            };
            let found = first.as_ref().map(found_as).unwrap_or_default();
            let more: Vec<Vec<(usize, Kind)>> = also.iter().map(found_as).collect();
            let twice = !more.is_empty() || linked_twice(&found, &object.links);
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
                let primary = found
                    .iter()
                    .find(|(at, _)| *at == slot)
                    .map(|(_, kind)| kind);
                if let Some(kind) = primary {
                    hint_kinds.set(id, kind.clone());
                }
                if twice {
                    hint_kinds.also_from(id, link.hash, primary, &found, &more, &object.links);
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

#[derive(Clone, Copy)]
struct Pending {
    id: u32,
    hash: Hash,
    left: u64,
}

#[derive(Default)]
struct Live {
    chunks: Vec<Vec<Pending>>,
    len: usize,
    dead: usize,
}

impl Live {
    fn bytes(&self) -> u64 {
        ((self.len - self.dead) * mem::size_of::<Pending>()) as u64
    }

    fn is_empty(&self) -> bool {
        self.len == self.dead
    }

    fn push(&mut self, id: u32, hash: Hash, left: u64) {
        if self
            .chunks
            .last()
            .is_none_or(|chunk| chunk.len() == LIVE_CHUNK)
        {
            self.chunks.push(Vec::with_capacity(LIVE_CHUNK));
        }
        let chunk = self.chunks.last_mut().expect("a chunk was just made");
        chunk.push(Pending { id, hash, left });
        self.len += 1;
    }

    fn find(&self, id: u32) -> Option<(usize, usize)> {
        let last = self.chunks.len().checked_sub(1)?;
        let tail = &self.chunks[last];
        for at in (tail.len().saturating_sub(LIVE_SCAN)..tail.len()).rev() {
            match tail[at].id.cmp(&id) {
                std::cmp::Ordering::Equal => return Some((last, at)),
                std::cmp::Ordering::Less => return None,
                std::cmp::Ordering::Greater => {}
            }
        }
        let chunk = match self.chunks[last][0].id <= id {
            true => last,
            false => self.chunks[..last]
                .partition_point(|chunk| chunk[0].id <= id)
                .checked_sub(1)?,
        };
        let entries = &self.chunks[chunk];
        let (mut low, mut high, mut step) = (entries.len() - 1, entries.len(), 1);
        while low > 0 && entries[low].id > id {
            high = low;
            low = low.saturating_sub(step);
            step *= 2;
        }
        let at = low
            + entries[low..high]
                .binary_search_by_key(&id, |pending| pending.id)
                .ok()?;
        Some((chunk, at))
    }

    fn get(&self, id: u32) -> Option<&Pending> {
        self.find(id).map(|(chunk, at)| &self.chunks[chunk][at])
    }

    fn take(&mut self, id: u32) -> Option<Hash> {
        let (chunk, at) = self.find(id)?;
        let pending = &mut self.chunks[chunk][at];
        if pending.left == 0 {
            return None;
        }
        pending.left -= 1;
        if pending.left == 0 {
            self.dead += 1;
        }
        Some(pending.hash)
    }

    fn settle(&mut self) {
        while let Some(chunk) = self.chunks.last_mut()
            && chunk.last().is_some_and(|pending| pending.left == 0)
        {
            chunk.pop();
            if chunk.is_empty() {
                self.chunks.pop();
            }
            self.len -= 1;
            self.dead -= 1;
        }
        if self.dead * 4 <= self.len || self.len < LIVE_SETTLE {
            return;
        }
        let (mut to, mut at) = (0, 0);
        for from in 0..self.chunks.len() {
            for index in 0..self.chunks[from].len() {
                let pending = self.chunks[from][index];
                if pending.left == 0 {
                    continue;
                }
                self.chunks[to][at] = pending;
                at += 1;
                if at == LIVE_CHUNK {
                    (to, at) = (to + 1, 0);
                }
            }
        }
        self.chunks.truncate(to + 1);
        self.chunks[to].truncate(at);
        if at == 0 {
            self.chunks.pop();
        }
        self.len -= self.dead;
        self.dead = 0;
    }
}

struct Linked<'a> {
    bytes: &'a [u8],
    pos: usize,
    left: usize,
    held: u64,
    position: u64,
    live: &'a Live,
    anchors: &'a Anchors,
}

impl Iterator for Linked<'_> {
    type Item = LinkTo;

    fn next(&mut self) -> Option<LinkTo> {
        self.left = self.left.checked_sub(1)?;
        let reference = take_varint(self.bytes, &mut self.pos);
        if let Some(back) = reference.checked_sub(self.held + 1) {
            let id = (self.position - back - 1) as u32;
            let pending = self
                .live
                .get(id)
                .expect("a link checked as it arrived names an object still held");
            return Some(LinkTo::Put(pending.hash, id));
        }
        let hash: Hash = self.bytes[self.pos..self.pos + HASH_BYTES]
            .try_into()
            .expect("a link that is not by place carries its hash");
        self.pos += HASH_BYTES;
        Some(LinkTo::At(match reference.checked_sub(1) {
            None => Ptr { hash, offset: 0 },
            Some(reference) => self.anchors.ptr(reference as u32),
        }))
    }
}

fn stream<R: Read>(
    staging: &mut Staging,
    header: &Header,
    anchors: &Anchors,
    input: &mut Input<R>,
    room: &mut Room<'_>,
) -> Result<Streamed> {
    let held = (anchors.roots.len() + anchors.hinted.len()) as u64;
    let mut found = Streamed {
        links: Column::new(),
        loose: Vec::new(),
        carried: 0,
    };
    let mut live = Live::default();
    let (mut raw, mut near) = (Vec::new(), Vec::new());
    for _ in 0..header.count {
        let (body, packed) = input.body()?;
        let position = staging.len() as u64;
        let count = input.varint()?;
        let few = count <= LINKS_HELD as u64;
        let mut open = false;
        raw.clear();
        near.clear();
        for _ in 0..count {
            let reference = input.varint()?;
            if !few {
                push_varint(&mut raw, reference);
            }
            if let Some(back) = reference.checked_sub(held + 1) {
                let id = back
                    .checked_add(1)
                    .and_then(|back| position.checked_sub(back))
                    .ok_or_else(|| bad("a link leads past the start of the bundle"))?
                    as u32;
                let Some(hash) = live.take(id) else {
                    return Err(bad("it links an object more often than it said it would"));
                };
                if few {
                    near.push(LinkTo::Put(hash, id));
                }
                continue;
            }
            let hash = input.hash()?;
            let ptr = match reference.checked_sub(1) {
                None => {
                    open = true;
                    Ptr { hash, offset: 0 }
                }
                Some(reference) => anchors.ptr(reference as u32),
            };
            if ptr.hash != hash {
                return Err(bad("a link does not match the object it names"));
            }
            match few {
                true => near.push(LinkTo::At(ptr)),
                false => raw.extend_from_slice(&hash),
            }
        }
        let uses = input.varint()?;
        let count = count as usize;
        let (id, hash) = match few {
            true => staging.put(&body, packed.as_deref(), count, near.drain(..), room)?,
            false => {
                let links = Linked {
                    bytes: &raw,
                    pos: 0,
                    left: count,
                    held,
                    position,
                    live: &live,
                    anchors,
                };
                staging.put(&body, packed.as_deref(), count, links, room)?
            }
        };
        live.settle();
        if raw.capacity() > READ_BUFFER {
            raw = Vec::new();
        }
        found.carried = found.carried.saturating_add(body.len());
        found.links.push(u8::try_from(uses).unwrap_or(u8::MAX));
        if uses > 0 {
            live.push(id, hash, uses);
            if live.bytes() > input.read.saturating_add(LIVE_SLACK) {
                return Err(bad(
                    "it says later objects link more of its objects than its size allows",
                ));
            }
        }
        if open {
            found.loose.push(id);
        }
    }
    input.finish()?;
    if !live.is_empty() {
        return Err(bad("it says it links objects more often than it does"));
    }
    Ok(found)
}

fn place(
    txn: &mut Txn,
    record: &Record,
    staging: &Staging,
    placed: &mut Column<u64>,
    anchors: &Anchors,
    held: &Index,
    unlisted: bool,
) -> Result<()> {
    let earlier = Earlier {
        staging,
        placed: Some(&*placed),
    };
    let mut links = Vec::with_capacity(record.links.len());
    for link in &record.links {
        let sent = staging.find(*link).and_then(|id| earlier.offset(id));
        links.push(match sent {
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
    pair_roots: PairRoots,
}

struct Resolved {
    refs: Option<Refs>,
    commits: Vec<u32>,
    found: Vec<(Hash, u32)>,
}

struct Received {
    staging: Staging,
    anchors: Anchors,
    commits: Vec<u32>,
    found: Vec<(Hash, u32)>,
}

struct Arrival {
    landed: Option<Option<Crossing>>,
    together: Option<Together>,
    objects: u64,
    received_bytes: u64,
}

type Descent = (Ptr, Option<(Ptr, bool)>);

/// The checks that the tables of each version agree with their rows, planned and queued before the
/// walk reaches the version, so that they run beside the walk: those against the table at the
/// same place in the version before, as the pairing of the two roots finds it, whose diffs read
/// what the walk has not checked yet as data that may not decode. A table built from its rows, or
/// one whose rows give entries only by reading the file, as an `each` index over a `Set` does,
/// waits for the walk, since that reads as a map reads, which takes a node that does not decode
/// for a damaged file. The pairing of each version is kept, by its root, for the plan made once
/// the walk is done.
struct Early {
    on: bool,
    plans: Plans,
    listed: HashSet<Hash>,
    checks: Checks,
    paired: HashMap<Hash, Paired>,
}

fn lineage(
    store: &Store,
    objects: &impl Objects,
    landing: &Landing<'_>,
    order: &[(u64, u64, u32)],
) -> Result<Vec<Descent>> {
    let mut lineage = Vec::with_capacity(order.len());
    for (_, _, id) in order {
        let sent = landing
            .commits
            .get(*id)
            .expect("an ordered commit was walked");
        let Some(root) = sent.root else {
            continue;
        };
        let entry = read_commit(objects, *id, sent.hash)?;
        let parent = match entry.prev {
            None => None,
            Some(prev) => match landing.sent(objects, prev) {
                Some(parent) => parent.root.map(|root| (root, false)),
                None => match landing
                    .find(sent.segment as usize, sent.facts.version.saturating_sub(1))
                {
                    Ok(Found::Held(ptr)) if ptr.hash == prev.hash => Entry::load(store, ptr)?
                        .commit
                        .root
                        .map(|root| (root, true)),
                    _ => None,
                },
            },
        };
        lineage.push((root, parent));
    }
    Ok(lineage)
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
    let mut pending: Vec<Ptr> = Vec::new();
    let mut below = Some(tip);
    let mut walking = objects.head(&tip.hash);
    let tip_id = walking;
    while let Some(mark) = below {
        let Some(id) = walking else {
            break;
        };
        let entry = read_commit(objects, id, mark.hash)
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
        walking = entry.prev.and_then(|prev| objects.find(prev));
        versions.push(id);
        pending.extend(entry.merged.map(|link| link.ptr));
        commits.insert(Sent::of(id, mark.hash, &entry, 0));
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
        low,
        segments: vec![Segment {
            versions,
            bottom: low,
            below: prefix.map_or(Below::Gone, Below::Chain),
        }],
        commits,
    };
    while let Some(top) = pending.pop() {
        if landing.sent(objects, top).is_some() {
            continue;
        }
        if let Some(id) = objects.find(top) {
            pending.extend(landing.segment(refs, objects, (id, top.hash))?);
        }
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
        pair_roots,
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
            pair_roots,
            kept: HashMap::new(),
            held: Retyped::default(),
            beneath: Vec::new(),
            spent: 0,
            budget: streamed
                .carried
                .saturating_mul(2)
                .saturating_add(RECHECK_SLACK),
            queue: Vec::new(),
            deferred: Vec::new(),
            tables: Tables::default(),
            confirmed: HashSet::new(),
            carried: streamed.carried,
            rerun_budget: streamed
                .carried
                .saturating_mul(RERUN_FACTOR)
                .saturating_add(RERUN_SLACK),
            zero_width_budget: (streamed.carried as u64)
                .saturating_mul(ZERO_WIDTH_PER_BYTE)
                .saturating_add(ZERO_WIDTH_SLACK),
            view: OnceCell::new(),
            views: RefCell::new(Vec::new()),
            walked: vec![0; staging.len().div_ceil(64)],
            credited: 0,
            flushed: Cell::new(0),
            recalled: (order.len() > 1).then(Recalled::default),
        };
        let roots: Vec<Ptr> = order
            .iter()
            .filter_map(|(_, _, id)| {
                landing
                    .commits
                    .get(*id)
                    .expect("an ordered commit was walked")
                    .root
            })
            .collect();
        let tabled = roots.first().is_some_and(|root| {
            vetting
                .root_tables(*root)
                .is_ok_and(|(tables, ..)| !tables.is_empty())
        });
        if tabled {
            let descents = lineage(store, objects, &landing, &order);
            let agreeing = vetting.fresh_view();
            staging.overlaid(&agreeing, || {
                pool::scope(|pool| {
                    let mut early = Early {
                        on: descents.is_ok(),
                        plans: Plans::default(),
                        listed: HashSet::new(),
                        checks: Checks::strict(),
                        paired: HashMap::new(),
                    };
                    let walked = vetting.walk(&roots, |vetting, at| {
                        if let Ok(descents) = &descents
                            && let Some(descent) = descents.get(at)
                        {
                            vetting.early(&mut early, descent, pool, &agreeing);
                        }
                    });
                    if walked.is_err() || vetting.tables.is_empty() {
                        pool.stop();
                        return walked;
                    }
                    match descents {
                        Ok(lineage) => {
                            vetting.agree(&lineage, (early.checks, early.paired), pool, &agreeing)
                        }
                        Err(error) => {
                            pool.stop();
                            Err(error)
                        }
                    }
                })
            })?;
        } else {
            vetting.walk(&roots, |_, _| {})?;
            if !vetting.tables.is_empty() {
                let lineage = lineage(store, objects, &landing, &order)?;
                let agreeing = vetting.fresh_view();
                staging.overlaid(&agreeing, || {
                    pool::scope(|pool| {
                        vetting.agree(
                            &lineage,
                            (Checks::strict(), HashMap::new()),
                            pool,
                            &agreeing,
                        )
                    })
                })?;
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
        let hash = landing
            .commits
            .get(id)
            .expect("an ordered commit was walked")
            .hash;
        let object = objects.read_object(objects.at(id, hash))?;
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
            prev_facts = Some(landing.facts(objects, ptr)?);
            links.push(ptr);
        }
        if let Some(skip) = entry.skip {
            let at = skip_version(version);
            links.push(landing.chained(objects, &mut linked, segment, skip, at)?);
        }
        let mut merged_facts = None;
        if let Some(link) = entry.merged {
            let merged = link.ptr.hash;
            let ptr = if let Some(sent) = landing.sent(objects, link.ptr) {
                mark_linked(&mut linked, sent.id);
                objects.at(sent.id, merged)
            } else {
                based.get(&merged).copied().unwrap_or(Ptr {
                    hash: merged,
                    offset: 0,
                })
            };
            merged_facts = Some(landing.facts(objects, ptr)?);
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
    if let Some(id) = tip_id.filter(|_| landed > 0) {
        mark_linked(&mut linked, id);
    }
    if checking && linked.is_some_and(|links| links.iter().any(|count| count == 0)) {
        return Err(bad("it carries objects nothing links to"));
    }
    let tip_ptr = match (tip_ptr, tip_id) {
        (Some(ptr), _) => ptr,
        (None, Some(id)) => objects.at(id, tip.hash),
        (None, None) => return Err(bad("its head is neither in it nor in this database")),
    };
    let root = if tip_id.is_some_and(|id| landing.commits.contains(id)) {
        tip_root
    } else {
        Entry::load(store, tip_ptr)?.commit.root
    };
    let existing = refs.try_get(remote)?;
    let head = Head {
        version: tip.version,
        root,
        log: Chain {
            tip: Some(tip_ptr),
            first,
            last: tip.version,
        },
        merged: Map::new(),
        born: existing
            .as_ref()
            .map_or_else(|| Head::born_next(store), |old| old.born),
    };
    if let Some(old) = &existing
        && (old.log == head.log || hash_at(store, &old.log, tip.version)? == Some(tip.hash))
    {
        return Ok(Resolved {
            refs: None,
            commits: landing.commits.ids(),
            found: landing.commits.found(),
        });
    }
    let mut next = refs.clone();
    let continues = match existing
        .as_ref()
        .and_then(|old| old.log.tip.map(|ptr| (ptr.hash, old.log.last)))
    {
        None => false,
        Some((hash, version)) => match landing.find(0, version) {
            Ok(Found::Sent(found)) => landing
                .commits
                .get(found)
                .is_some_and(|sent| sent.hash == hash),
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
        found: landing.commits.found(),
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
        born: ours.map_or(0, |ours| ours.born),
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
    /// a few hundred bytes. With [`Options::shared`](crate::Options::shared) it first catches up
    /// with commits from other processes, so it names no commit that another process has dropped
    /// or pruned since this one last caught up, except from a `decode` or a `Drop` that runs while
    /// this handle holds the whole file, where it names what was last published.
    ///
    /// # Errors
    ///
    /// Read errors from the branch table or the log, and with
    /// [`Options::shared`](crate::Options::shared) the errors of [`Db::refresh`].
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
        self.caught_up()?;
        let (store, refs) = self.published_refs()?;
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
    /// file that shares no commit with this one carries everything. With
    /// [`Options::shared`](crate::Options::shared) it first catches up with commits from other
    /// processes, as [`Db::have`] does, so it sends the newest head this file holds. It then reads
    /// the branch as last published and holds no writer lock while it builds the bundle.
    ///
    /// # Errors
    ///
    /// [`Error::Bundle`] if `have` is not a have, read errors, and with
    /// [`Options::shared`](crate::Options::shared) the errors of [`Db::refresh`].
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
        self.caught_up()?;
        self.emitter(have)?.write_to(&mut out)
    }

    fn emitter(&self, have: &Have) -> Result<Emitter> {
        let have = Marks::decode(have.as_bytes())?;
        let (store, refs) = self.published_refs()?;
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
    /// brings new commits replaces the branch `remote`, so do not commit to it. Under
    /// [`Options::shared`](crate::Options::shared) each file first catches up with commits from
    /// other processes, so the pull sends the newest head the file of `from` holds.
    ///
    /// If this file prunes or drops what the bundle builds on while it is sent, nothing lands, and
    /// the pull sends a fresh have and tries again, up to three attempts in all. That is found
    /// before `merge` can run, so `merge` never runs for an attempt that is tried again, only for
    /// the one that landed, as often as [`Db::merge_with`] runs it, and the [`Pulled`] describes
    /// that attempt.
    ///
    /// # Errors
    ///
    /// - [`Error::BranchInUse`] if `remote` is `main` or this handle's branch.
    /// - [`Error::StaleHave`] if this file pruned or dropped what the bundle builds on while it was
    ///   sent, on each of the three attempts. Nothing landed and `merge` did not run.
    /// - [`Error::NoCommonAncestor`] when the merge base could lie in pruned history. The remote
    ///   branch has landed, and this branch is as it was.
    /// - [`Error::NoBranch`] if this handle's branch is dropped while the pull runs, or the branch
    ///   `remote` is dropped between its landing and bringing this branch up to it, as another
    ///   handle or process can do. This branch is as it was.
    /// - [`Error::Bundle`] if what `from` sends fails the checks, which for an open file means it
    ///   is damaged, or if checking it would decode or read more again than its size allows, as
    ///   one large value shared by many keys, or one large node reached under many key ranges,
    ///   can, or would visit more items that encode to nothing, such as the `()`s of a `Vec<()>`,
    ///   than its size allows. A table whose index, stored under a fingerprint this side knows,
    ///   holds other entries than its rows give, a table that holds an index under a fingerprint
    ///   this side does not know, a row stored under a key other than its own, or two rows
    ///   sharing a value of a unique index fail the checks too.
    /// - [`Error::Decode`] if `from` sends a head that `T` cannot read, such as a table whose
    ///   unique index, rebuilt on this side, finds two rows sharing a value. Nothing lands and
    ///   `merge` does not run: every branch, the remote one and handles on it included, is as it
    ///   was. A merge whose base `T` cannot read returns it too, with the remote branch landed and
    ///   this branch as it was.
    /// - [`Error::Reentrant`] from inside a closure on this file, for a bundle large enough to be
    ///   staged in the log also from code that runs while this thread holds the file's cycle lock,
    ///   or where waiting on `from`'s file could otherwise deadlock, [`Error::ReadOnly`] on a
    ///   read-only handle, and the errors of [`Db::commit`].
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
    /// merges it again. The errors of [`Db::pull`] come back through `E::from`. A stale have is
    /// retried as [`Db::pull`] retries it, before anything reaches `E::from`, so a pull that
    /// succeeds on a later attempt returns `Ok`, and [`Error::StaleHave`] comes back only after
    /// the third attempt.
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
        let mut attempts = 1;
        let (arrival, sent_bytes) = loop {
            match self.fetch(from, remote) {
                Err(Error::StaleHave(_)) if attempts < PULL_ATTEMPTS => attempts += 1,
                fetched => break fetched?,
            }
        };
        let mut pulled = self.bring_up(remote, arrival, merge)?;
        pulled.sent_bytes = sent_bytes;
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
    /// As for [`Db::pull`], for the branch of `to`: [`Error::StaleHave`] comes back only once the
    /// file of `to` has pruned or dropped what the bundle builds on in each of three attempts.
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
    /// As for [`Db::try_pull`], for the branch of `to`, so a stale have is retried before
    /// anything reaches `E::from`.
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
    /// type that will read it, recomputing each map summary from the entries below it with this
    /// side's [`Summary`](crate::Summary), and it refuses commits dated more than a day past this
    /// machine's clock. Objects are staged as they arrive, in memory up to 1 MiB and past that in
    /// staging frames at the end of the file that nothing links, under no writer lock but for the
    /// moment each frame takes to be placed, so commits on every branch go on meanwhile. Once the
    /// whole bundle has passed, one frame points the branch `remote` at them, so a crash leaves the
    /// previous head. A bundle that arrives late or twice changes nothing. With
    /// [`Options::shared`](crate::Options::shared) it first catches up with commits from other
    /// processes, as [`Db::have`] does, so a bundle built from a have that another handle or
    /// process on this file sent finds what it builds on.
    ///
    /// # Errors
    ///
    /// - [`Error::Bundle`] if the bundle is malformed, damaged or forged, which includes a value
    ///   written in a form no encoder writes, such as a varint longer than its value needs or bytes
    ///   left after it, a summary that is not the fold of its entries and one whose `of` or
    ///   `combine` panics to recompute, and a table whose index, stored under a fingerprint this
    ///   side knows, does not hold exactly the entries its rows give, as a peer whose schema sets
    ///   index maps apart from the rows can send, or that holds an index under a fingerprint this
    ///   side does not know, as a peer whose type has an index this side's lacks or declares
    ///   otherwise sends, or if checking it would decode or read more again than its size allows,
    ///   as one large value shared by many keys, or one large node reached under many key ranges,
    ///   can, or would visit more items that encode to nothing, such as the `()`s of a
    ///   `Vec<()>`, than its size allows. Nothing it carries is linked, and on Linux the disk its
    ///   staging frames took is given back.
    /// - [`Error::StaleHave`] if this file does not hold the commit the bundle builds on, because
    ///   it was built for another file, this one pruned or dropped that commit after sending its
    ///   have, or the part of the bundle that names that commit was damaged on the way. Nothing
    ///   lands and `merge` does not run; build a new bundle from a fresh have, which is the way on
    ///   from a damaged one too. Only the caller can fetch one, so this is not retried as
    ///   [`Db::pull`] retries it.
    /// - The other errors of [`Db::pull`], and with [`Options::shared`](crate::Options::shared)
    ///   those of [`Db::refresh`].
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
    /// An `Err` from `merge` comes back as for [`Db::try_pull`], and the errors of
    /// [`Db::receive`] through `E::from`, [`Error::StaleHave`] among them without a retry.
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
    /// As for [`Db::try_receive`], and [`Error::Io`] if reading `input` fails.
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
        self.caught_up()?;
        let arrival = self.land_from(remote, input)?;
        self.bring_up(remote, arrival, merge)
    }

    fn fetch(&self, from: &Db<T>, remote: &str) -> Result<(Arrival, u64)> {
        drop(self.entering(Needs::Every)?);
        let have = self.have()?;
        from.caught_up()?;
        let emitter = {
            let _theirs = from.entering(Needs::Tip)?;
            from.emitter(&have)?
        };
        self.step("bundled");
        let arrival = if emitter.count < PIPE_OBJECTS as u64 {
            self.land_from(remote, emitter)?
        } else {
            let (sender, pipe) = mpsc::sync_channel(PIPE_CHUNKS);
            thread::scope(|scope| {
                scope.spawn(move || emitter.send(sender));
                let piped = Piped {
                    pipe,
                    chunk: Vec::new(),
                    pos: 0,
                };
                self.land_from(remote, piped)
            })?
        };
        Ok((arrival, have.as_bytes().len() as u64))
    }

    fn land_from(&self, remote: &str, input: impl Read) -> Result<Arrival> {
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
        let mut bound = None;
        let landed = match received {
            Some(received) => {
                let ours = match self.entering(Needs::Every) {
                    Ok(ours) => ours,
                    Err(error) => {
                        received.staging.discard();
                        return Err(error);
                    }
                };
                bound = self.published_store().together();
                self.land_staged(remote, &header, received)?;
                Some(ours)
            }
            None => None,
        };
        Ok(Arrival {
            landed,
            together: bound,
            objects: header.count,
            received_bytes,
        })
    }

    fn bring_up<R, S: Into<String>, E: From<Error>>(
        &self,
        remote: &str,
        arrival: Arrival,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Pulled<R>, E> {
        let (outcome, value) = match arrival.landed {
            Some(_ours) => self.integrate(remote, merge, arrival.together)?,
            None => (Outcome::UpToDate, None),
        };
        Ok(Pulled {
            outcome,
            value,
            version: self.version(),
            objects: arrival.objects,
            sent_bytes: 0,
            received_bytes: arrival.received_bytes,
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
        let (store, refs) = self.published_refs()?;
        let root_kind = Kind::value::<T>();
        let mut anchors = anchors(&store, &refs, header, &root_kind)?;
        let mut staging = Staging::new(&store, SPILL_AT);
        let generation = store.id();
        let mut room = |bytes| self.stage_room(generation, bytes);
        let checked = stream(&mut staging, header, &anchors, input, &mut room)
            .and_then(|streamed| {
                let vetted = Vetted {
                    streamed,
                    root_kind: &root_kind,
                    pair_roots: pair_roots::<T>,
                };
                resolve(
                    &store,
                    &staging,
                    &refs,
                    remote,
                    header,
                    &anchors,
                    Some(vetted),
                )
            })
            .and_then(|sent| staging.finish().map(|()| sent));
        let sent = match checked {
            Ok(sent) => sent,
            Err(error) => {
                staging.discard();
                return Err(error);
            }
        };
        anchors.hint_kinds = Kinds::default();
        self.step("received");
        Ok(Received {
            staging,
            anchors,
            commits: sent.commits,
            found: sent.found,
        })
    }

    fn stage_room(&self, generation: u64, bytes: u64) -> Result<Option<(u64, u64)>> {
        let room = self.place_staging(generation, bytes)?;
        if room.is_some() {
            self.step("staging");
        }
        Ok(room)
    }

    fn land_staged(&self, remote: &str, header: &Header, received: Received) -> Result<()> {
        let Received {
            staging,
            anchors,
            commits,
            found,
        } = received;
        let mut learned = false;
        let landed = self.land_records(
            remote,
            header,
            &staging,
            anchors,
            (&commits, &found),
            &mut learned,
        );
        let failed = landed.is_err() && self.published_store().check_healthy().is_err();
        if !learned && !failed {
            staging.discard();
        }
        landed
    }

    fn land_records(
        &self,
        remote: &str,
        header: &Header,
        staging: &Staging,
        mut anchors: Anchors,
        (commits, found): (&[u32], &[(Hash, u32)]),
        learned: &mut bool,
    ) -> Result<()> {
        let spilled = staging.spilled();
        if spilled {
            drop(self.entering(Needs::Cycle)?);
        }
        let _cycles = spilled.then(|| self.hold_cycles()).transpose()?;
        let mut moved = None;
        if spilled {
            let (store, refs) = self.published_refs()?;
            if staging.in_place(&store) {
                drop((store, refs));
                staging.sync()?;
                staging.learn(commits);
                *learned = true;
            } else {
                anchors = reanchor(&store, &refs, header, &anchors)?;
                drop((store, refs));
                moved = Some(self.place_staged(staging, &anchors, commits)?);
                self.published_store().flush()?;
            }
            self.step("placed");
        }
        let mut held_here = Column::new();
        self.rewrite_refs(|store, txn, refs| {
            let anchors = reanchor(store, refs, header, &anchors)?;
            let placed = match (&moved, spilled) {
                (Some(placed), _) => Some(placed),
                (None, true) => None,
                (None, false) => {
                    let held = anchors.held();
                    let mut replay = staging.replay();
                    while let Some(record) = replay.next_record()? {
                        place(
                            txn,
                            &record,
                            staging,
                            &mut held_here,
                            &anchors,
                            &held,
                            false,
                        )?;
                    }
                    Some(&held_here)
                }
            };
            let objects = Placed {
                txn: RefCell::new(txn),
                earlier: Earlier { staging, placed },
                commits: found,
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
                    place(txn, record, staging, &mut placed, anchors, &held, unlisted)?;
                }
                Ok(())
            })?;
            batch.clear();
        }
    }

    /// Brings this branch up to the landed `remote`, as [`Db::pull`] describes.
    ///
    /// `together` binds the landing's hold of the gate to the decision's, and is let go once the
    /// decision is made, before `merge` runs.
    fn integrate<R, S: Into<String>, E: From<Error>>(
        &self,
        remote: &str,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
        together: Option<Together>,
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
            let changed = recorded(refs, &name, head, remote, theirs.version)?;
            let mut next = changed.clone().unwrap_or_else(|| refs.clone());
            let pruned = self.keep_last(store, &mut next, &[&name, remote])?;
            Ok(((changed.is_some() || pruned).then_some(next), decision))
        })?;
        drop(together);
        let outcome = match decision {
            Decision::UpToDate => Outcome::UpToDate,
            Decision::Ahead => Outcome::Ahead,
            Decision::FastForward => Outcome::FastForward,
            Decision::Equal => return self.merge_remote(remote, merge, Outcome::UpToDate),
            Decision::Diverged => return self.merge_remote(remote, merge, Outcome::Ahead),
        };
        Ok((outcome, None))
    }

    fn merge_remote<R, S: Into<String>, E: From<Error>>(
        &self,
        remote: &str,
        merge: impl FnMut(&mut T, &Snapshot<T>, &Snapshot<T>) -> std::result::Result<(S, R), E>,
        held: Outcome,
    ) -> std::result::Result<(Outcome, Option<R>), E> {
        let tracking = self.branch(remote)?;
        let seen = match self.merge_tracking(&tracking, merge)? {
            Ok(committed) => {
                self.keep_last_now(&[remote])?;
                return Ok((Outcome::Merged, Some(committed.value)));
            }
            Err(Some(seen)) => seen,
            Err(None) => return Ok((Outcome::UpToDate, None)),
        };
        let name = self.branch_name().to_string();
        self.rewrite_refs(|store, _, refs| {
            let Some(ours) = refs.try_get(&name)? else {
                return Ok((None, ()));
            };
            if !tracking.held_in(refs)? {
                return Ok((None, ()));
            }
            let changed = recorded(refs, &name, (*ours).clone(), remote, seen)?;
            let mut next = changed.clone().unwrap_or_else(|| refs.clone());
            let pruned = self.keep_last(store, &mut next, &[remote])?;
            Ok(((changed.is_some() || pruned).then_some(next), ()))
        })?;
        Ok((held, None))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_finds_each_object_until_its_last_link_and_settles_what_is_spent() {
        let mut live = Live::default();
        let count = 3 * LIVE_CHUNK as u32 + 17;
        for id in 0..count {
            let mut hash = [0u8; HASH_BYTES];
            hash[..4].copy_from_slice(&id.to_le_bytes());
            live.push(id * 2, hash, u64::from(id % 3) + 1);
        }
        assert_eq!(live.bytes(), u64::from(count) * 32);
        assert!(live.get(1).is_none() && live.take(1).is_none());
        for id in (0..count).filter(|id| id % 3 == 0) {
            assert!(live.take(id * 2).is_some());
            assert!(live.take(id * 2).is_none());
            live.settle();
        }
        let left = count - count.div_ceil(3);
        assert_eq!(live.bytes(), u64::from(left) * 32);
        assert!(live.len - live.dead <= live.chunks.len() * LIVE_CHUNK);
        for id in (0..count).filter(|id| id % 3 != 0) {
            let pending = live
                .get(id * 2)
                .expect("an object with links to come is held");
            assert_eq!(pending.hash[..4], id.to_le_bytes());
            let pending_hash = pending.hash;
            for _ in 0..id % 3 + 1 {
                assert_eq!(live.take(id * 2), Some(pending_hash));
            }
            assert!(live.take(id * 2).is_none());
            live.settle();
        }
        assert!(live.is_empty());
        assert_eq!(live.bytes(), 0);
    }
}
