use std::any::Any;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::ErrorKind;
use std::mem;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::thread;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use quick_cache::Weighter;
use quick_cache::sync::Cache;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

use crate::codec::{
    ARC_HEADER, Cached, Decoder, HASH_BYTES, Hash, Moved, Moves, Object, Ptr, Relocate, Sink,
    Staged, push_varint,
};
use crate::error::{Error, Result};
use crate::map::SPILL_BYTES;
use crate::platform::{self, FileExt};
use crate::reclaim::sibling;
use crate::shared::{Erased, Shared};

const MAGIC: &[u8; 8] = b"FERRODB3";
const FRAME_MAGIC: &[u8; 8] = b"FERROFRM";
const RETIRED_MAGIC: &[u8; 8] = b"FERROGEN";
const SLOT_BYTES: u64 = 4096;
pub(crate) const LOG_START: u64 = 2 * SLOT_BYTES;
const MARK_BYTES: usize = 24 + HASH_BYTES;
const FRAME_CHECKED: usize = 8 + MARK_BYTES;
const ANCHOR_AT: usize = FRAME_CHECKED + 16;
const SUPER_CHECKED: usize = ANCHOR_AT + MARK_BYTES;
const SUPER_BYTES: usize = SUPER_CHECKED + 32;
const SURVIVAL_ONE: f64 = (1u64 << 32) as f64;
const FRAME_BYTES: usize = FRAME_CHECKED + 16;
const SCAN_CHUNK: usize = 1 << 20;
const FIRST_RESERVE: u64 = 64 << 10;
const MAX_RESERVE: u64 = 4 << 20;
const LOCK_RETRIES: u32 = 5;
const READ_AHEAD: usize = 1024;
const ENTRY_BYTES: usize = 96;
const MIN_DEDUPE: u64 = 1 << 10;
const MAX_DEDUPE: u64 = 1 << 16;
const DEDUPE_WAYS: usize = 2;
const DEDUPE_CHUNK: usize = 64;
const MAX_VARINT: usize = 10;
const COMPRESS_MIN: usize = 96;
pub(crate) const MAX_RATIO: usize = 255;
const REPLAY_CHUNK: usize = 256 << 10;
const LOCKS_AT: u64 = 1 << 62;
const MULTI_AT: u64 = LOCKS_AT + 1;
const COMMIT_AT: u64 = LOCKS_AT + 2;
const SUPER_AT: u64 = LOCKS_AT + 3;
const CYCLE_AT: u64 = LOCKS_AT + 4;
const LANES_AT: u64 = LOCKS_AT + (1 << 20);
const LANES: u64 = 1 << 60;

static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    Fsync,
    Buffered,
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Create(u64),
    Write(u64, u64, Vec<u8>),
    SetLen(u64, u64),
    Sync(u64),
    Rename(u64),
    SyncDir,
    Acked(u64),
}

#[doc(hidden)]
pub type Trace = Arc<Mutex<Vec<Op>>>;

#[derive(Clone, Debug)]
pub struct StoreOptions {
    pub durability: Durability,
    pub cache_bytes: u64,
    pub read_only: bool,
    pub checkpoint_bytes: u64,
    pub shared: bool,
    #[doc(hidden)]
    pub trace: Option<Trace>,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            durability: Durability::Fsync,
            cache_bytes: 1 << 30,
            read_only: false,
            checkpoint_bytes: 64 << 20,
            shared: false,
            trace: None,
        }
    }
}

pub(crate) fn heap_weight(fixed_bytes: usize, body_bytes: usize) -> u32 {
    let weight = ENTRY_BYTES + fixed_bytes + body_bytes;
    u32::try_from(weight).unwrap_or(u32::MAX)
}

struct CacheEntry(Entry);

enum Entry {
    Value {
        value: Arc<dyn Cached>,
        weight: u32,
    },
    Node {
        node: Erased,
        weight: u32,
        owned: bool,
    },
    Taken {
        weight: u32,
    },
}

impl CacheEntry {
    fn value(value: Arc<dyn Cached>, weight: u32) -> CacheEntry {
        CacheEntry(Entry::Value { value, weight })
    }

    fn node(node: Erased, weight: u32) -> CacheEntry {
        node.hold();
        CacheEntry(Entry::Node {
            node,
            weight,
            owned: true,
        })
    }

    fn weight(&self) -> u32 {
        match self.0 {
            Entry::Value { weight, .. } | Entry::Node { weight, .. } | Entry::Taken { weight } => {
                weight
            }
        }
    }

    fn take(&mut self) -> Entry {
        let weight = self.weight();
        mem::replace(&mut self.0, Entry::Taken { weight })
    }
}

impl Clone for CacheEntry {
    fn clone(&self) -> Self {
        CacheEntry(match &self.0 {
            Entry::Value { value, weight } => Entry::Value {
                value: value.clone(),
                weight: *weight,
            },
            Entry::Node { node, weight, .. } => Entry::Node {
                node: node.clone(),
                weight: *weight,
                owned: false,
            },
            Entry::Taken { weight } => Entry::Taken { weight: *weight },
        })
    }
}

impl Drop for CacheEntry {
    fn drop(&mut self) {
        if let Entry::Node {
            node,
            weight,
            owned: true,
        } = self.take()
            && node.release()
        {
            crate::epoch::retire(node, weight as usize);
        }
    }
}

struct Located<T> {
    offset: u64,
    value: Arc<T>,
}

impl<T: Send + Sync + 'static> Cached for Located<T> {}

#[derive(Clone)]
struct EntryWeight;

impl Weighter<Hash, CacheEntry> for EntryWeight {
    fn weight(&self, _key: &Hash, entry: &CacheEntry) -> u64 {
        u64::from(entry.weight())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Basis {
    pub(crate) bytes: u64,
    survival: u64,
}

impl Basis {
    pub(crate) fn new(bytes: u64, survival: f64) -> Basis {
        Basis {
            bytes,
            survival: (survival.clamp(0.0, 1.0) * SURVIVAL_ONE) as u64,
        }
    }

    pub(crate) fn survival(&self) -> f64 {
        self.survival as f64 / SURVIVAL_ONE
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Mark {
    seq: u64,
    log_end: u64,
    head: Option<Ptr>,
}

impl Mark {
    fn fill(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.seq.to_le_bytes());
        out[8..16].copy_from_slice(&self.log_end.to_le_bytes());
        if let Some(head) = self.head {
            out[16..24].copy_from_slice(&head.offset.to_le_bytes());
            out[24..MARK_BYTES].copy_from_slice(&head.hash);
        }
    }

    fn parse(bytes: &[u8]) -> Option<Mark> {
        let seq = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let log_end = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        let offset = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
        let hash: Hash = bytes[24..MARK_BYTES].try_into().ok()?;
        let head = (offset != 0).then_some(Ptr { hash, offset });
        Some(Mark { seq, log_end, head })
    }
}

#[derive(Clone, Copy)]
struct Super {
    seq: u64,
    log_end: u64,
    head: Option<Ptr>,
    basis: Basis,
    anchor: Mark,
}

impl Super {
    fn new(mark: Mark, anchor: Mark, basis: Basis) -> Super {
        Super {
            seq: mark.seq,
            log_end: mark.log_end,
            head: mark.head,
            basis,
            anchor,
        }
    }

    fn mark(&self) -> Mark {
        Mark {
            seq: self.seq,
            log_end: self.log_end,
            head: self.head,
        }
    }

    fn settled(&self) -> bool {
        self.anchor == self.mark()
    }

    fn at_anchor(&self) -> Super {
        Super::new(self.anchor, self.anchor, self.basis)
    }

    fn fill(&self, magic: &[u8; 8], out: &mut [u8]) {
        out[0..8].copy_from_slice(magic);
        self.mark().fill(&mut out[8..]);
    }

    fn fields(bytes: &[u8]) -> Option<Super> {
        let mark = Mark::parse(&bytes[8..])?;
        Some(Super::new(mark, mark, Basis::default()))
    }

    fn encode(&self) -> [u8; SUPER_BYTES] {
        let mut out = [0u8; SUPER_BYTES];
        self.fill(MAGIC, &mut out);
        out[FRAME_CHECKED..FRAME_CHECKED + 8].copy_from_slice(&self.basis.bytes.to_le_bytes());
        out[FRAME_CHECKED + 8..ANCHOR_AT].copy_from_slice(&self.basis.survival.to_le_bytes());
        self.anchor.fill(&mut out[ANCHOR_AT..]);
        let check = blake3::hash(&out[..SUPER_CHECKED]);
        out[SUPER_CHECKED..SUPER_BYTES].copy_from_slice(check.as_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<Super> {
        if bytes.len() < SUPER_BYTES || &bytes[0..8] != MAGIC {
            return None;
        }
        let check = blake3::hash(&bytes[..SUPER_CHECKED]);
        if check.as_bytes() != &bytes[SUPER_CHECKED..SUPER_BYTES] {
            return None;
        }
        let bytes_at = bytes[FRAME_CHECKED..FRAME_CHECKED + 8].try_into().ok()?;
        let survival = bytes[FRAME_CHECKED + 8..ANCHOR_AT].try_into().ok()?;
        let basis = Basis {
            bytes: u64::from_le_bytes(bytes_at),
            survival: u64::from_le_bytes(survival),
        };
        Some(Super {
            basis,
            anchor: Mark::parse(&bytes[ANCHOR_AT..])?,
            ..Super::fields(bytes)?
        })
    }
}

fn read_supers(file: &File, len: u64) -> Result<Option<(Super, u64)>> {
    let mut best: Option<(Super, u64)> = None;
    for slot in 0..2u64 {
        let start = slot * SLOT_BYTES;
        if len < start + SUPER_BYTES as u64 {
            continue;
        }
        let mut bytes = [0u8; SUPER_BYTES];
        file.read_exact_at(&mut bytes, start)?;
        if let Some(candidate) = Super::decode(&bytes)
            && best
                .as_ref()
                .is_none_or(|b| (candidate.seq, candidate.anchor.seq) > (b.0.seq, b.0.anchor.seq))
        {
            best = Some((candidate, slot));
        }
    }
    Ok(best)
}

fn read_or_end(file: &File, out: &mut [u8], at: u64) -> Result<bool> {
    match file.read_exact_at(out, at) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn read_frame(file: &File, tip: &Super, len: u64) -> Result<Option<Super>> {
    let start = tip.log_end;
    let body_start = start.saturating_add(FRAME_BYTES as u64);
    if body_start > len {
        return Ok(None);
    }
    let mut header = [0u8; FRAME_BYTES];
    if !read_or_end(file, &mut header, start)? || &header[0..8] != FRAME_MAGIC {
        return Ok(None);
    }
    let Some(mut frame) = Super::fields(&header) else {
        return Ok(None);
    };
    frame.basis = tip.basis;
    let follows = tip.seq.checked_add(1) == Some(frame.seq);
    if !follows || frame.log_end < body_start || frame.log_end > len {
        return Ok(None);
    }
    let mut hasher = Xxh3::new();
    hasher.update(&header[..FRAME_CHECKED]);
    let mut chunk = vec![0u8; SCAN_CHUNK.min((frame.log_end - body_start) as usize)];
    let mut at = body_start;
    while at < frame.log_end {
        let take = chunk.len().min((frame.log_end - at) as usize);
        if !read_or_end(file, &mut chunk[..take], at)? {
            return Ok(None);
        }
        hasher.update(&chunk[..take]);
        at += take as u64;
    }
    if hasher.digest128().to_le_bytes() != header[FRAME_CHECKED..FRAME_BYTES] {
        return Ok(None);
    }
    Ok(Some(frame))
}

fn scan(file: &File, sb: Super, len: u64) -> Result<Super> {
    let mut tip = sb;
    while let Some(next) = read_frame(file, &tip, len)? {
        tip = next;
    }
    Ok(tip)
}

fn recover(file: &File, sb: Super, slot: u64, len: u64) -> Result<(Super, u64)> {
    let mut tip = scan(file, sb.at_anchor(), len)?;
    let moved = tip.seq != sb.seq || !sb.settled();
    let rolled_back = tip.seq < sb.seq;
    let shrink = len > tip.log_end;
    tip.anchor = tip.mark();
    if moved {
        file.sync_data()?;
    }
    if shrink {
        file.set_len(tip.log_end)?;
    }
    let slot = if moved { 1 - slot } else { slot };
    if moved {
        file.write_all_at(&tip.encode(), slot * SLOT_BYTES)?;
    }
    if moved || shrink {
        file.sync_all()?;
    }
    if rolled_back {
        file.write_all_at(&tip.encode(), (1 - slot) * SLOT_BYTES)?;
        file.sync_all()?;
    }
    Ok((tip, slot))
}

fn observe(file: &File, path: &Path, sb: Super, len: u64) -> Result<Super> {
    let settled = sb.settled() && read_frame(file, &sb, len)?.is_none();
    if settled || platform::try_lock_shared(file).is_err() {
        return Ok(sb);
    }
    if platform::range_held(file, MULTI_AT, 1).unwrap_or(true) {
        platform::unlock(file)?;
        return Ok(sb);
    }
    let before = sb.anchor.seq;
    let scanned = file
        .metadata()
        .map_err(Error::from)
        .and_then(|meta| scan(file, sb.at_anchor(), meta.len()))
        .and_then(|tip| {
            if tip.seq != before {
                platform::sync_reader(file, path)?;
            }
            Ok(tip)
        });
    platform::unlock(file)?;
    scanned
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

fn varint_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

type Raw = (Object, u32, Option<Vec<u8>>);

pub(crate) fn compressed(body: &[u8]) -> Option<Vec<u8>> {
    if body.len() < COMPRESS_MIN {
        return None;
    }
    let packed = lz4_flex::block::compress(body);
    (packed.len() + varint_len(body.len() as u64) < body.len()).then_some(packed)
}

pub fn append_record(buf: &mut Vec<u8>, offset: u64, body: &[u8], links: &[Ptr]) {
    let packed = compressed(body);
    append_staged(buf, offset, staged(body, packed.as_deref(), links));
}

pub(crate) fn staged<'a>(body: &'a [u8], packed: Option<&'a [u8]>, links: &'a [Ptr]) -> Staged<'a> {
    Staged {
        raw: body.len(),
        payload: packed.unwrap_or(body),
        packed: packed.is_some(),
        links,
    }
}

fn append_staged(buf: &mut Vec<u8>, offset: u64, staged: Staged<'_>) {
    push_varint(buf, staged.payload.len() as u64);
    if staged.packed {
        push_varint(buf, (staged.links.len() as u64) << 1 | 1);
        push_varint(buf, staged.raw as u64);
    } else {
        push_varint(buf, (staged.links.len() as u64) << 1);
    }
    buf.extend_from_slice(staged.payload);
    for link in staged.links {
        buf.extend_from_slice(&link.hash);
        push_varint(buf, zigzag(offset.wrapping_sub(link.offset) as i64));
    }
}

pub fn record_len(offset: u64, object: &Object) -> usize {
    let mut buf = Vec::new();
    append_record(&mut buf, offset, &object.body, &object.links);
    buf.len()
}

struct RecordReader<'a> {
    file: Option<&'a File>,
    start: u64,
    tail: u64,
    bytes: Cow<'a, [u8]>,
    pos: usize,
}

impl RecordReader<'_> {
    fn ensure(&mut self, upto: usize) -> Result<()> {
        if upto <= self.bytes.len() {
            return Ok(());
        }
        let past = (upto as u64)
            .checked_add(self.start)
            .is_none_or(|end| end > self.tail);
        let Some(file) = self.file.filter(|_| !past) else {
            return Err(Error::Corrupt(format!(
                "record at {} runs past end of log",
                self.start
            )));
        };
        let have = self.bytes.len();
        let want = (upto - have).max(READ_AHEAD);
        let want = want.min((self.tail - self.start) as usize - have);
        if want > SCAN_CHUNK && self.start + (have + want) as u64 > file.metadata()?.len() {
            return Err(Error::Corrupt(format!(
                "record at {} runs past end of file",
                self.start
            )));
        }
        let bytes = self.bytes.to_mut();
        bytes.resize(have + want, 0);
        file.read_exact_at(&mut bytes[have..], self.start + have as u64)?;
        Ok(())
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for index in 0..MAX_VARINT {
            self.ensure(self.pos + 1)?;
            let byte = self.bytes[self.pos];
            self.pos += 1;
            if index == MAX_VARINT - 1 && byte > 1 {
                break;
            }
            value |= u64::from(byte & 0x7f) << (7 * index);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::Corrupt("varint overflow in record".into()))
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| Error::Corrupt("record size overflow".into()))?;
        self.ensure(end)?;
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }
}

pub(crate) fn seal(file: &Traced, log_end: u64, head: Ptr, seq: u64, basis: Basis) -> Result<()> {
    file.sync_data()?;
    let mark = Mark {
        seq,
        log_end,
        head: Some(head),
    };
    let sb = Super::new(mark, mark, basis);
    file.write_all_at(&sb.encode(), 0)?;
    file.write_all_at(&sb.encode(), SLOT_BYTES)?;
    file.sync_all()?;
    Ok(())
}

fn try_lock_shared_writer(file: &File) -> std::result::Result<(), TryLockError> {
    platform::lock_range(file, MULTI_AT, 1, false, false)?;
    platform::try_lock_shared(file)
}

fn open_locked(path: &Path, read_only: bool, shared: bool) -> Result<File> {
    if shared && !read_only && !platform::RANGES {
        return Err(Error::Io(std::io::Error::new(
            ErrorKind::Unsupported,
            "writers in several processes need open file description locks",
        )));
    }
    let mut retries = 0;
    loop {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(!read_only)
            .create(!read_only)
            .truncate(false);
        let file = platform::open(&options, path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound if read_only => {
                Error::NoDatabase(path.display().to_string())
            }
            _ => error.into(),
        })?;
        if read_only {
            return Ok(file);
        }
        let locked = match shared {
            true => try_lock_shared_writer(&file),
            false => platform::try_lock(&file),
        };
        match locked {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) if retries == LOCK_RETRIES => return Err(Error::Locked),
            Err(TryLockError::WouldBlock) => {
                retries += 1;
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        if platform::path_id(path)
            .is_ok_and(|on_disk| platform::file_id(&file).is_ok_and(|id| id == on_disk))
        {
            return Ok(file);
        }
    }
}

struct Range<'a> {
    file: &'a File,
    at: u64,
}

impl<'a> Range<'a> {
    fn hold(file: &'a File, at: u64, shared: bool) -> Result<Option<Range<'a>>> {
        if !shared {
            return Ok(None);
        }
        platform::lock_range(file, at, 1, true, true).map_err(|error| match error {
            TryLockError::Error(error) => Error::Io(error),
            TryLockError::WouldBlock => Error::Locked,
        })?;
        Ok(Some(Range { file, at }))
    }
}

impl Drop for Range<'_> {
    fn drop(&mut self) {
        let _ = platform::unlock_range(self.file, self.at, 1);
    }
}

pub(crate) fn lane_of(name: &str) -> u64 {
    LANES_AT + (xxh3_64(name.as_bytes()) & (LANES - 1))
}

pub(crate) struct Held {
    store: Store,
    at: u64,
    len: u64,
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = platform::unlock_range(&self.store.inner.file, self.at, self.len);
    }
}

pub(crate) struct AllLanes {
    store: Store,
}

impl Drop for AllLanes {
    fn drop(&mut self) {
        let inner = &self.store.inner;
        let lanes = inner.lanes.lock();
        let mut held: Vec<u64> = lanes.keys().copied().collect();
        if cfg!(windows) {
            held.clear();
        }
        held.sort_unstable();
        let mut at = LANES_AT;
        for byte in held {
            if byte > at {
                let _ = platform::unlock_range(&inner.file, at, byte - at);
            }
            at = byte + 1;
        }
        if at < LANES_AT + LANES {
            let _ = platform::unlock_range(&inner.file, at, LANES_AT + LANES - at);
        }
    }
}

pub(crate) enum Followed {
    Still,
    Moved(Frame),
    Retired,
}

#[derive(Clone, Copy)]
pub(crate) struct Frame {
    pub(crate) seq: u64,
    pub(crate) end: u64,
    pub(crate) head: Ptr,
}

impl Frame {
    fn mark(&self) -> Mark {
        Mark {
            seq: self.seq,
            log_end: self.end,
            head: Some(self.head),
        }
    }

    fn header(&self) -> [u8; FRAME_BYTES] {
        let mut header = [0u8; FRAME_BYTES];
        header[0..8].copy_from_slice(FRAME_MAGIC);
        self.mark().fill(&mut header[8..]);
        header
    }
}

struct Cursor {
    seq: u64,
    reserved: u64,
    reserve: u64,
}

#[derive(Clone, Copy)]
struct Marks {
    latest: Mark,
    anchor: Mark,
}

#[derive(Default)]
struct Arrivals {
    early: Vec<(u64, u64)>,
    waiters: usize,
}

pub(crate) struct Traced {
    file: File,
    trace: Option<Trace>,
    inode: u64,
}

impl Traced {
    pub(crate) fn new(file: File, trace: Option<Trace>) -> Result<Traced> {
        let inode = platform::file_id(&file)?;
        Ok(Traced { file, trace, inode })
    }

    fn log(&self, op: impl FnOnce(u64) -> Op) {
        if let Some(trace) = &self.trace {
            trace.lock().push(op(self.inode));
        }
    }

    pub(crate) fn created(&self) {
        self.log(Op::Create);
    }

    pub(crate) fn write_all_at(&self, bytes: &[u8], at: u64) -> std::io::Result<()> {
        self.log(|inode| Op::Write(inode, at, bytes.to_vec()));
        self.file.write_all_at(bytes, at)
    }

    pub(crate) fn sync_data(&self) -> std::io::Result<()> {
        self.file.sync_data()?;
        self.log(Op::Sync);
        Ok(())
    }

    fn sync_all(&self) -> std::io::Result<()> {
        self.file.sync_all()?;
        self.log(Op::Sync);
        Ok(())
    }

    pub(crate) fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.log(|inode| Op::SetLen(inode, len));
        self.file.set_len(len)
    }
}

impl Deref for Traced {
    type Target = File;

    fn deref(&self) -> &File {
        &self.file
    }
}

pub(crate) struct Retired(Weak<Inner>);

impl Retired {
    pub(crate) fn bytes(&self) -> Option<u64> {
        self.0
            .upgrade()
            .map(|inner| inner.tail.load(Ordering::Acquire))
    }
}

struct Inner {
    id: u64,
    path: PathBuf,
    file: Traced,
    tail: AtomicU64,
    written: AtomicU64,
    placed: AtomicU64,
    arrivals: Mutex<Arrivals>,
    landed: Condvar,
    cursor: Mutex<Cursor>,
    slot: AtomicU64,
    marks: Mutex<Marks>,
    checkpointing: AtomicBool,
    ignored: Option<Mark>,
    cache: Cache<Hash, CacheEntry, EntryWeight>,
    dedupe: Dedupe,
    basis: Basis,
    poisoned: AtomicBool,
    lost_at: AtomicU64,
    unsynced: AtomicBool,
    closed: AtomicBool,
    lanes: Mutex<HashMap<u64, Option<usize>>>,
    lanes_moved: Condvar,
    options: StoreOptions,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

pub(crate) struct Closing(pub(crate) Store);

impl Deref for Closing {
    type Target = Store;

    fn deref(&self) -> &Store {
        &self.0
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl Store {
    pub fn open(path: &Path, options: StoreOptions) -> Result<(Store, Option<Ptr>)> {
        Store::open_as(path, path, options)
    }

    pub(crate) fn open_as(
        actual: &Path,
        path: &Path,
        options: StoreOptions,
    ) -> Result<(Store, Option<Ptr>)> {
        let shared = options.shared && !options.read_only;
        let file = open_locked(actual, options.read_only, shared)?;
        let commit = Range::hold(&file, COMMIT_AT, shared)?;
        let tops = Range::hold(&file, SUPER_AT, shared)?;
        let idle = !shared || platform::lock_range(&file, CYCLE_AT, 1, true, false).is_ok();
        if !options.read_only && actual == path && idle {
            let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let _ = std::fs::remove_file(sibling(&real));
        }
        if shared && idle {
            platform::unlock_range(&file, CYCLE_AT, 1)?;
        }
        let len = file.metadata()?.len();
        let existing = read_supers(&file, len)?;
        let named = existing.map(|(sb, _)| sb.mark());
        let (sb, slot) = match existing {
            Some((sb, slot)) => {
                let len = file.metadata()?.len();
                if sb.anchor.log_end < LOG_START {
                    return Err(Error::Corrupt(format!(
                        "superblock ends the log at {}, inside the {LOG_START} byte header",
                        sb.anchor.log_end
                    )));
                }
                if len < sb.anchor.log_end {
                    return Err(Error::Corrupt(format!(
                        "file is {len} bytes but superblock expects {}",
                        sb.anchor.log_end
                    )));
                }
                if options.read_only {
                    (observe(&file, actual, sb, len)?, slot)
                } else {
                    recover(&file, sb, slot, len)?
                }
            }
            None => {
                if options.read_only {
                    return Err(Error::NotFerro(path.display().to_string()));
                }
                let mut probe = vec![0u8; len.min(LOG_START) as usize];
                file.read_exact_at(&mut probe, 0)?;
                if probe.starts_with(MAGIC) {
                    return Err(Error::Corrupt("both superblocks are invalid".into()));
                }
                if len > LOG_START || probe.iter().any(|b| *b != 0) {
                    return Err(Error::NotFerro(path.display().to_string()));
                }
                let empty = Mark {
                    seq: 0,
                    log_end: LOG_START,
                    head: None,
                };
                let sb = Super::new(empty, empty, Basis::default());
                file.set_len(LOG_START)?;
                file.write_all_at(&sb.encode(), 0)?;
                file.sync_all()?;
                platform::sync_parent(path)?;
                (sb, 0)
            }
        };
        drop((tops, commit));
        let cache_bytes = options.cache_bytes.max(1);
        let items = ((cache_bytes / 4096).max(64)) as usize;
        let dedupe = (cache_bytes / 4096).clamp(MIN_DEDUPE, MAX_DEDUPE) as usize;
        let head = sb.head;
        let file = Traced::new(file, options.trace.clone())?;
        let store = Store {
            inner: Arc::new(Inner {
                id: NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed),
                path: path.to_path_buf(),
                file,
                tail: AtomicU64::new(sb.log_end),
                written: AtomicU64::new(sb.log_end),
                placed: AtomicU64::new(sb.log_end),
                arrivals: Mutex::new(Arrivals::default()),
                landed: Condvar::new(),
                cursor: Mutex::new(Cursor {
                    seq: sb.seq,
                    reserved: sb.log_end,
                    reserve: 0,
                }),
                slot: AtomicU64::new(slot),
                marks: Mutex::new(Marks {
                    latest: sb.mark(),
                    anchor: sb.mark(),
                }),
                checkpointing: AtomicBool::new(false),
                ignored: named.filter(|mark| mark.seq > sb.seq),
                cache: Cache::with_weighter(items, cache_bytes, EntryWeight),
                dedupe: Dedupe::new(dedupe),
                basis: sb.basis,
                poisoned: AtomicBool::new(false),
                lost_at: AtomicU64::new(u64::MAX),
                unsynced: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                lanes: Mutex::new(HashMap::new()),
                lanes_moved: Condvar::new(),
                options,
            }),
        };
        Ok((store, head))
    }

    pub fn id(&self) -> u64 {
        self.inner.id
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub fn size(&self) -> u64 {
        self.inner.tail.load(Ordering::Acquire)
    }

    pub(crate) fn written(&self) -> u64 {
        self.inner.written.load(Ordering::Acquire)
    }

    pub fn is_read_only(&self) -> bool {
        self.inner.options.read_only
    }

    pub(crate) fn last_seq(&self) -> u64 {
        self.inner.cursor.lock().seq
    }

    pub(crate) fn basis(&self) -> Basis {
        self.inner.basis
    }

    pub(crate) fn metadata(&self) -> Result<std::fs::Metadata> {
        Ok(self.inner.file.metadata()?)
    }

    pub(crate) fn retired(&self) -> Retired {
        Retired(Arc::downgrade(&self.inner))
    }

    #[doc(hidden)]
    pub fn trace(&self, op: Op) {
        self.inner.file.log(|_| op);
    }

    pub(crate) fn trace_rename(&self) {
        self.inner.file.log(Op::Rename);
    }

    pub fn refresh(&self) -> Result<Option<Ptr>> {
        let mut cursor = self.inner.cursor.lock();
        let opened = self.inner.file.metadata()?;
        if platform::path_id(&self.inner.path)? != self.inner.file.inode {
            return Err(Error::Replaced);
        }
        let len = opened.len();
        let (sb, _) = read_supers(&self.inner.file, len)?
            .ok_or_else(|| Error::Corrupt("both superblocks are invalid".into()))?;
        if sb.seq < cursor.seq || self.inner.ignored == Some(sb.mark()) {
            return Ok(None);
        }
        let len = self.inner.file.metadata()?.len();
        if len < sb.log_end {
            return Err(Error::Corrupt(format!(
                "file is {len} bytes but superblock expects {}",
                sb.log_end
            )));
        }
        cursor.seq = sb.seq;
        self.inner.tail.fetch_max(sb.log_end, Ordering::AcqRel);
        self.inner.placed.fetch_max(sb.log_end, Ordering::AcqRel);
        self.inner.written.fetch_max(sb.log_end, Ordering::AcqRel);
        Ok(sb.head)
    }

    pub(crate) fn poison(&self) {
        self.inner.poisoned.store(true, Ordering::Release);
        let _arrivals = self.inner.arrivals.lock();
        self.inner.landed.notify_all();
    }

    fn poison_unsynced(&self) {
        self.inner.unsynced.store(true, Ordering::Release);
        self.poison();
    }

    fn poison_at(&self, at: u64) {
        self.inner.lost_at.fetch_min(at, Ordering::AcqRel);
        self.poison();
    }

    fn arrived(&self, start: u64, end: u64) {
        let inner = &self.inner;
        let mut arrivals = inner.arrivals.lock();
        if inner.written.load(Ordering::Relaxed) != start {
            arrivals.early.push((start, end));
            return;
        }
        let mut written = end;
        while let Some(index) = arrivals.early.iter().position(|(at, _)| *at == written) {
            written = arrivals.early.swap_remove(index).1;
        }
        inner.written.store(written, Ordering::Release);
        if arrivals.waiters > 0 {
            inner.landed.notify_all();
        }
    }

    fn landed_past(&self, offset: u64) -> u64 {
        let inner = &self.inner;
        let written = inner.written.load(Ordering::Acquire);
        if offset < written || offset >= inner.placed.load(Ordering::Acquire) {
            return written;
        }
        let mut arrivals = inner.arrivals.lock();
        loop {
            let written = inner.written.load(Ordering::Acquire);
            if offset < written
                || offset >= inner.placed.load(Ordering::Acquire)
                || written >= inner.lost_at.load(Ordering::Acquire)
            {
                return written;
            }
            arrivals.waiters += 1;
            inner.landed.wait(&mut arrivals);
            arrivals.waiters -= 1;
        }
    }

    pub(crate) fn landed_to(&self, end: u64) -> u64 {
        self.landed_past(end.saturating_sub(1))
    }

    fn readable_past(&self, offset: u64) -> Result<u64> {
        let inner = &self.inner;
        let written = self.landed_past(offset);
        if offset >= written
            && inner.poisoned.load(Ordering::Acquire)
            && offset < inner.placed.load(Ordering::Acquire)
        {
            return Err(Error::Poisoned);
        }
        Ok(written)
    }

    pub(crate) fn check_healthy(&self) -> Result<()> {
        if self.inner.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        Ok(())
    }

    pub(crate) fn sync(&self, frame: Frame) -> Result<()> {
        let inner = &self.inner;
        if self.landed_to(frame.end) < frame.end || inner.unsynced.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        let durable = inner.options.durability == Durability::Fsync;
        let attempt = (|| -> Result<()> {
            if inner.options.shared {
                return self.sync_shared(frame, durable);
            }
            if durable {
                inner.file.sync_data()?;
            }
            let mark = frame.mark();
            let anchor = {
                let mut marks = inner.marks.lock();
                marks.latest = mark;
                if durable {
                    marks.anchor = mark;
                }
                marks.anchor
            };
            self.write_super(&Super::new(mark, anchor, inner.basis))
        })();
        if let Err(error) = attempt {
            self.poison_unsynced();
            return Err(error);
        }
        inner.tail.store(frame.end, Ordering::Release);
        Ok(())
    }

    fn sync_shared(&self, frame: Frame, durable: bool) -> Result<()> {
        let inner = &self.inner;
        let _held = Range::hold(&inner.file, SUPER_AT, true)?;
        let newest = read_supers(&inner.file, LOG_START)?;
        let mark = frame.mark();
        if durable && newest.is_some_and(|(found, _)| found.anchor.seq >= frame.seq) {
            let mut marks = inner.marks.lock();
            marks.latest = mark;
            marks.anchor = mark;
            return Ok(());
        }
        let last = match durable {
            true => {
                let len = inner.file.metadata()?.len();
                scan(&inner.file, Super::new(mark, mark, inner.basis), len)?.mark()
            }
            false => mark,
        };
        if durable {
            inner.file.sync_data()?;
        }
        let anchor = {
            let mut marks = inner.marks.lock();
            marks.latest = mark;
            if durable {
                marks.anchor = mark;
            }
            if durable { last } else { marks.anchor }
        };
        let sb = Super::new(last, anchor, inner.basis);
        let slot = match newest {
            Some((found, _)) if (found.seq, found.anchor.seq) >= (sb.seq, sb.anchor.seq) => {
                return Ok(());
            }
            Some((_, slot)) => 1 - slot,
            None => 1 - inner.slot.load(Ordering::Relaxed),
        };
        inner.file.write_all_at(&sb.encode(), slot * SLOT_BYTES)?;
        inner.slot.store(slot, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn flush(&self) -> Result<()> {
        if self.inner.options.durability != Durability::Fsync {
            return Ok(());
        }
        if let Err(error) = self.inner.file.sync_data() {
            self.poison_unsynced();
            return Err(error.into());
        }
        Ok(())
    }

    pub(crate) fn checkpoint(&self, frame: Frame) {
        let inner = &self.inner;
        let anchored = inner.marks.lock().anchor.log_end;
        if frame.end.saturating_sub(anchored) < inner.options.checkpoint_bytes.max(1)
            || inner.checkpointing.swap(true, Ordering::AcqRel)
        {
            return;
        }
        if !inner.unsynced.load(Ordering::Acquire) {
            match inner.file.sync_data() {
                Ok(()) => inner.marks.lock().anchor = frame.mark(),
                Err(_) => self.poison_unsynced(),
            }
        }
        inner.checkpointing.store(false, Ordering::Release);
    }

    pub(crate) fn settle_anchor(&self) -> Result<()> {
        let inner = &self.inner;
        let marks = *inner.marks.lock();
        if inner.options.read_only || marks.anchor == marks.latest || self.check_healthy().is_err()
        {
            return Ok(());
        }
        inner.file.sync_data()?;
        let sb = Super::new(marks.latest, marks.latest, inner.basis);
        if inner.options.shared {
            self.write_super(&sb)?;
        } else {
            let slot = inner.slot.load(Ordering::Relaxed);
            inner.file.write_all_at(&sb.encode(), slot * SLOT_BYTES)?;
        }
        inner.marks.lock().anchor = marks.latest;
        Ok(())
    }

    fn write_super(&self, sb: &Super) -> Result<()> {
        let inner = &self.inner;
        let _held = Range::hold(&inner.file, SUPER_AT, inner.options.shared)?;
        let slot = match inner.options.shared {
            false => 1 - inner.slot.load(Ordering::Relaxed),
            true => match read_supers(&inner.file, LOG_START)? {
                Some((found, _)) if (found.seq, found.anchor.seq) >= (sb.seq, sb.anchor.seq) => {
                    return Ok(());
                }
                Some((_, slot)) => 1 - slot,
                None => 1 - inner.slot.load(Ordering::Relaxed),
            },
        };
        inner.file.write_all_at(&sb.encode(), slot * SLOT_BYTES)?;
        inner.slot.store(slot, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn is_shared(&self) -> bool {
        self.inner.options.shared
    }

    fn hold(&self, at: u64, len: u64, wait: bool) -> Result<Option<Held>> {
        match platform::lock_range(&self.inner.file, at, len, true, wait) {
            Ok(()) => Ok(Some(Held {
                store: self.clone(),
                at,
                len,
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub(crate) fn lock_commit(&self) -> Result<Held> {
        self.hold(COMMIT_AT, 1, true)?.ok_or(Error::Locked)
    }

    pub(crate) fn lock_lanes(&self) -> Result<AllLanes> {
        platform::lock_range(&self.inner.file, LANES_AT, LANES, true, true).map_err(|error| {
            match error {
                TryLockError::Error(error) => Error::Io(error),
                TryLockError::WouldBlock => Error::Locked,
            }
        })?;
        Ok(AllLanes {
            store: self.clone(),
        })
    }

    pub(crate) fn lock_cycle(&self, wait: bool) -> Result<Option<Held>> {
        self.hold(CYCLE_AT, 1, wait)
    }

    pub(crate) fn lock_lane(&self, at: u64) -> Result<()> {
        self.take_lane(at, true).map(|_| ())
    }

    pub(crate) fn try_lock_lane(&self, at: u64) -> Result<bool> {
        self.take_lane(at, false)
    }

    fn take_lane(&self, at: u64, wait: bool) -> Result<bool> {
        let inner = &self.inner;
        let mut lanes = inner.lanes.lock();
        loop {
            match lanes.get_mut(&at) {
                Some(Some(count)) => {
                    *count += 1;
                    return Ok(true);
                }
                Some(None) if wait => inner.lanes_moved.wait(&mut lanes),
                Some(None) => return Ok(false),
                None => break,
            }
        }
        lanes.insert(at, None);
        drop(lanes);
        let locked = platform::lock_range(&inner.file, at, 1, true, wait);
        let mut lanes = inner.lanes.lock();
        match locked {
            Ok(()) => lanes.insert(at, Some(1)),
            Err(_) => lanes.remove(&at),
        };
        inner.lanes_moved.notify_all();
        match locked {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub(crate) fn unlock_lane(&self, at: u64) {
        let inner = &self.inner;
        let mut lanes = inner.lanes.lock();
        match lanes.get_mut(&at) {
            Some(Some(count)) if *count > 1 => *count -= 1,
            _ => {
                lanes.remove(&at);
                let _ = platform::unlock_range(&inner.file, at, 1);
            }
        }
    }

    pub(crate) fn follow(&self) -> Result<Followed> {
        let inner = &self.inner;
        let cursor = inner.cursor.lock();
        let start = Mark {
            seq: cursor.seq,
            log_end: inner.placed.load(Ordering::Acquire),
            head: None,
        };
        let mut at = Super::new(start, start, Basis::default());
        let mut last = None;
        let mut len = None;
        loop {
            let mut header = [0u8; FRAME_BYTES];
            if !read_or_end(&inner.file, &mut header, at.log_end)? {
                break;
            }
            if &header[0..8] == RETIRED_MAGIC {
                if platform::path_id(&inner.path).is_ok_and(|id| id != inner.file.inode) {
                    return Ok(Followed::Retired);
                }
                break;
            }
            if &header[0..8] != FRAME_MAGIC {
                break;
            }
            let len = match len {
                Some(len) => len,
                None => *len.insert(inner.file.metadata()?.len()),
            };
            match read_frame(&inner.file, &at, len)? {
                Some(next) if next.head.is_some() => {
                    at = next;
                    last = Some(next);
                }
                _ => break,
            }
        }
        let Some(last) = last else {
            return Ok(Followed::Still);
        };
        inner.written.fetch_max(last.log_end, Ordering::AcqRel);
        Ok(Followed::Moved(Frame {
            seq: last.seq,
            end: last.log_end,
            head: last.head.expect("a followed frame names its head"),
        }))
    }

    pub(crate) fn advance(&self, frame: Frame) {
        let inner = &self.inner;
        let mut cursor = inner.cursor.lock();
        if frame.seq <= cursor.seq {
            return;
        }
        cursor.seq = frame.seq;
        inner.placed.fetch_max(frame.end, Ordering::AcqRel);
        inner.written.fetch_max(frame.end, Ordering::AcqRel);
    }

    pub(crate) fn mark_retired(&self) -> Result<()> {
        let mut header = [0u8; FRAME_BYTES];
        header[0..8].copy_from_slice(RETIRED_MAGIC);
        let at = self.inner.placed.load(Ordering::Acquire);
        self.inner.file.write_all_at(&header, at)?;
        Ok(())
    }

    pub fn read_object(&self, ptr: Ptr) -> Result<Object> {
        self.read_record(ptr).map(|(object, _)| object)
    }

    pub(crate) fn read_record(&self, ptr: Ptr) -> Result<(Object, u32)> {
        let tail = self.readable_past(ptr.offset)?;
        let file = Some(&*self.inner.file);
        let (object, len, _) = Store::read_within(file, ptr, tail, &[], true, false)?;
        Ok((object, len))
    }

    pub(crate) fn read_packed(&self, ptr: Ptr) -> Result<(Object, Option<Vec<u8>>)> {
        let tail = self.readable_past(ptr.offset)?;
        let file = Some(&*self.inner.file);
        let (object, _, packed) = Store::read_within(file, ptr, tail, &[], true, true)?;
        Ok((object, packed))
    }

    pub(crate) fn read_links(&self, ptr: Ptr) -> Result<Vec<Ptr>> {
        let tail = self.readable_past(ptr.offset)?;
        if ptr.offset < LOG_START || ptr.offset >= tail {
            return Err(Error::Corrupt(format!(
                "pointer {} outside log of {tail} bytes",
                ptr.offset
            )));
        }
        let mut reader = RecordReader {
            file: Some(&*self.inner.file),
            start: ptr.offset,
            tail,
            bytes: Cow::Borrowed(&[]),
            pos: 0,
        };
        let stored_len = usize::try_from(reader.varint()?)
            .map_err(|_| Error::Corrupt("body length overflow".into()))?;
        let flagged = reader.varint()?;
        if flagged & 1 == 1 {
            reader.varint()?;
        }
        reader.take(stored_len)?;
        let mut links = Vec::with_capacity(((flagged >> 1) as usize).min(1024));
        for _ in 0..flagged >> 1 {
            let hash: Hash = reader
                .take(HASH_BYTES)?
                .try_into()
                .expect("slice length is HASH_BYTES");
            let delta = unzigzag(reader.varint()?);
            links.push(Ptr {
                hash,
                offset: ptr.offset.wrapping_sub(delta as u64),
            });
        }
        Ok(links)
    }

    fn read_within(
        file: Option<&File>,
        ptr: Ptr,
        tail: u64,
        held: &[u8],
        verify: bool,
        keep: bool,
    ) -> Result<Raw> {
        if ptr.offset < LOG_START || ptr.offset >= tail {
            return Err(Error::Corrupt(format!(
                "pointer {} outside log of {tail} bytes",
                ptr.offset
            )));
        }
        let bytes = match file {
            Some(file) if held.is_empty() => {
                let want = (tail - ptr.offset).min(READ_AHEAD as u64) as usize;
                let mut bytes = vec![0u8; want];
                file.read_exact_at(&mut bytes, ptr.offset)?;
                Cow::Owned(bytes)
            }
            _ => Cow::Borrowed(held),
        };
        let mut reader = RecordReader {
            file,
            start: ptr.offset,
            tail,
            bytes,
            pos: 0,
        };
        let stored_len = usize::try_from(reader.varint()?)
            .map_err(|_| Error::Corrupt("body length overflow".into()))?;
        let flagged = reader.varint()?;
        let link_count = usize::try_from(flagged >> 1)
            .map_err(|_| Error::Corrupt("link count overflow".into()))?;
        let mut packed = None;
        let body = if flagged & 1 == 1 {
            let raw_len = usize::try_from(reader.varint()?)
                .map_err(|_| Error::Corrupt("body length overflow".into()))?;
            if raw_len
                > stored_len
                    .saturating_mul(MAX_RATIO)
                    .saturating_add(MAX_RATIO)
            {
                return Err(Error::Corrupt(format!(
                    "implausible compression at {}",
                    ptr.offset
                )));
            }
            let stored = reader.take(stored_len)?;
            let body = lz4_flex::block::decompress(stored, raw_len).map_err(|error| {
                Error::Corrupt(format!("bad compressed body at {}: {error}", ptr.offset))
            })?;
            packed = keep.then(|| stored.to_vec());
            body
        } else {
            reader.take(stored_len)?.to_vec()
        };
        let mut links = Vec::with_capacity(link_count.min(1024));
        for _ in 0..link_count {
            let hash: Hash = reader
                .take(HASH_BYTES)?
                .try_into()
                .expect("slice length is HASH_BYTES");
            let delta = unzigzag(reader.varint()?);
            let offset = ptr.offset.wrapping_sub(delta as u64);
            links.push(Ptr { hash, offset });
        }
        let object = Object { body, links };
        if verify && object.hash() != ptr.hash {
            return Err(Error::Corrupt(format!(
                "hash mismatch for object at {}",
                ptr.offset
            )));
        }
        let len =
            u32::try_from(reader.pos).map_err(|_| Error::Corrupt("record overflow".into()))?;
        Ok((object, len, packed))
    }

    pub(crate) fn fetch<T: Cached>(
        &self,
        ptr: Ptr,
        decode: impl FnOnce(&mut Decoder<'_>) -> Result<T>,
        weigh: impl FnOnce(&T, usize, usize) -> u32,
    ) -> Result<(Arc<T>, bool)> {
        if let Some(mut entry) = self.inner.cache.get(&ptr.hash)
            && let Entry::Value { value, .. } = entry.take()
            && let Ok(value) = (value as Arc<dyn Any + Send + Sync>).downcast::<T>()
        {
            return Ok((value, true));
        }
        let (value, weight) = self.decode_fresh(ptr, decode, weigh)?;
        let value = Arc::new(value);
        self.admit(ptr, || CacheEntry::value(value.clone(), weight));
        Ok((value, false))
    }

    pub(crate) fn fetch_node<T: Any + Send + Sync>(
        &self,
        ptr: Ptr,
        decode: impl FnOnce(&mut Decoder<'_>) -> Result<T>,
        weigh: impl FnOnce(&T, usize, usize) -> u32,
    ) -> Result<(Shared<T>, bool)> {
        if let Some(mut entry) = self.inner.cache.get(&ptr.hash)
            && let Entry::Node { node, .. } = entry.take()
            && let Ok(node) = node.downcast::<T>()
        {
            return Ok((node, true));
        }
        let (node, weight) = self.decode_fresh(ptr, decode, weigh)?;
        let node = Shared::new(node);
        self.admit(ptr, || {
            CacheEntry::node(Shared::erase(node.clone()), weight)
        });
        Ok((node, false))
    }

    fn decode_fresh<T>(
        &self,
        ptr: Ptr,
        decode: impl FnOnce(&mut Decoder<'_>) -> Result<T>,
        weigh: impl FnOnce(&T, usize, usize) -> u32,
    ) -> Result<(T, u32)> {
        let object = self.read_object(ptr)?;
        if object.body.len() >= SPILL_BYTES && !self.inner.options.read_only {
            self.inner.dedupe.insert(ptr.hash, ptr.offset);
        }
        let mut decoder = Decoder::new(&object.body, &object.links, Some(self));
        let value = decode(&mut decoder)?;
        let weight = weigh(&value, object.body.len(), decoder.heap());
        Ok((value, weight))
    }

    fn admit(&self, ptr: Ptr, entry: impl FnOnce() -> CacheEntry) {
        if !self.inner.closed.load(Ordering::SeqCst) {
            self.inner.cache.insert(ptr.hash, entry());
            if self.inner.closed.load(Ordering::SeqCst) {
                self.inner.cache.clear();
            }
        }
    }

    pub fn close(&self) {
        let inner = &self.inner;
        let closed = inner.closed.swap(true, Ordering::SeqCst);
        inner.cache.clear();
        crate::epoch::flush();
        if closed || inner.options.read_only {
            return;
        }
        if inner.options.shared {
            let _ = platform::unlock_range(&inner.file, MULTI_AT, 1);
            let _ = platform::unlock(&inner.file);
            return;
        }
        let end = inner.written.load(Ordering::Acquire);
        if !inner.poisoned.load(Ordering::Acquire)
            && inner.file.metadata().is_ok_and(|meta| meta.len() > end)
        {
            let _ = inner.file.set_len(end);
        }
        let _ = platform::unlock(&inner.file);
    }

    pub fn load<T: crate::codec::Data>(&self, ptr: Ptr) -> Result<Arc<T>> {
        if T::LOCATED {
            return self.load_located(ptr);
        }
        let (value, _) = self.fetch(
            ptr,
            |decoder| T::decode(decoder),
            |_, body, heap| heap_weight(ARC_HEADER + size_of::<T>(), body.max(heap)),
        )?;
        Ok(value)
    }

    fn load_located<T: crate::codec::Data>(&self, ptr: Ptr) -> Result<Arc<T>> {
        let (found, _) = self.fetch(
            ptr,
            |decoder| {
                Ok(Located {
                    offset: ptr.offset,
                    value: Arc::new(T::decode(decoder)?),
                })
            },
            |_, body, heap| {
                let fixed = 2 * ARC_HEADER + size_of::<Located<T>>() + size_of::<T>();
                heap_weight(fixed, body.max(heap))
            },
        )?;
        if found.offset == ptr.offset {
            return Ok(found.value.clone());
        }
        Ok(Arc::new(self.decode_at(ptr, T::decode)?))
    }

    pub(crate) fn decode_at<T>(
        &self,
        ptr: Ptr,
        decode: impl FnOnce(&mut Decoder<'_>) -> Result<T>,
    ) -> Result<T> {
        let object = self.read_object(ptr)?;
        decode(&mut Decoder::new(&object.body, &object.links, Some(self)))
    }

    pub fn begin(&self) -> Result<Txn> {
        if self.inner.options.read_only {
            return Err(Error::ReadOnly);
        }
        if self.inner.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        let start = self.inner.placed.load(Ordering::Acquire);
        Ok(Txn {
            store: self.clone(),
            origin: start,
            start,
            buf: vec![0u8; FRAME_BYTES],
            seen: HashMap::default(),
            remembered: Vec::new(),
            unshared: false,
            shared_end: u64::MAX,
            reserve: true,
        })
    }

    pub fn walk(&self, head: Ptr, mut visit: impl FnMut(Ptr, &Object)) -> Result<()> {
        let mut seen = HashSet::new();
        let mut stack = vec![head];
        while let Some(ptr) = stack.pop() {
            if !seen.insert(ptr) {
                continue;
            }
            let object = self.read_object(ptr)?;
            visit(ptr, &object);
            stack.extend(object.links.iter().copied());
        }
        Ok(())
    }
}

#[derive(Default)]
struct Prehashed(u64);

impl Hasher for Prehashed {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut word = [0u8; 8];
        let take = bytes.len().min(8);
        word[..take].copy_from_slice(&bytes[..take]);
        self.0 ^= u64::from_le_bytes(word);
    }
}

#[derive(Clone, Copy, Default)]
struct Way {
    hash: Hash,
    offset: [u8; 8],
}

#[derive(Default)]
#[repr(align(64))]
struct Set(Mutex<[Way; DEDUPE_WAYS]>);

struct Dedupe {
    chunks: Box<[OnceLock<Box<[Set]>>]>,
    mask: usize,
}

impl Dedupe {
    fn new(entries: usize) -> Dedupe {
        let sets = (entries / DEDUPE_WAYS)
            .next_power_of_two()
            .max(DEDUPE_CHUNK);
        Dedupe {
            chunks: (0..sets / DEDUPE_CHUNK).map(|_| OnceLock::new()).collect(),
            mask: sets - 1,
        }
    }

    fn index(&self, hash: &Hash) -> (usize, usize) {
        let mut word = [0u8; 8];
        word.copy_from_slice(&hash[..8]);
        let set = u64::from_le_bytes(word) as usize & self.mask;
        (set / DEDUPE_CHUNK, set % DEDUPE_CHUNK)
    }

    fn get(&self, hash: &Hash) -> Option<u64> {
        let (chunk, set) = self.index(hash);
        let mut ways = self.chunks[chunk].get()?[set].0.lock();
        let at = ways.iter().position(|way| way.hash == *hash)?;
        ways[..=at].rotate_right(1);
        Some(u64::from_le_bytes(ways[0].offset)).filter(|offset| *offset != 0)
    }

    fn insert(&self, hash: Hash, offset: u64) {
        let (chunk, set) = self.index(&hash);
        let chunk =
            self.chunks[chunk].get_or_init(|| (0..DEDUPE_CHUNK).map(|_| Set::default()).collect());
        let mut ways = chunk[set].0.lock();
        let at = ways
            .iter()
            .position(|way| way.hash == hash)
            .unwrap_or(DEDUPE_WAYS - 1);
        ways.copy_within(0..at, 1);
        ways[0] = Way {
            hash,
            offset: offset.to_le_bytes(),
        };
    }
}

pub struct Txn {
    store: Store,
    origin: u64,
    start: u64,
    buf: Vec<u8>,
    seen: Seen,
    remembered: Vec<Remembered>,
    unshared: bool,
    shared_end: u64,
    reserve: bool,
}

type Seen = HashMap<Hash, u64, BuildHasherDefault<Prehashed>>;

type Remembered = (Hash, CacheEntry, Option<(Relocate, usize)>);

pub(crate) struct Settling {
    moves: Option<Moves>,
    remembered: Vec<Remembered>,
    seen: Seen,
}

impl Settling {
    pub(crate) fn moves(&self) -> Option<&Moves> {
        self.moves.as_ref()
    }

    pub(crate) fn take_seen(&mut self, txn: &mut Txn) {
        self.seen = mem::take(&mut txn.seen);
    }

    pub(crate) fn finish(self, store: &Store) {
        let cache = &store.inner.cache;
        for (hash, offset) in self.seen {
            store.inner.dedupe.insert(hash, offset);
        }
        let Some(moves) = &self.moves else {
            for (hash, entry, _) in self.remembered {
                cache.insert(hash, entry);
            }
            return;
        };
        let mut moved = Moved::new();
        for (hash, entry, relocate) in self.remembered {
            let (Some((relocate, links)), Entry::Node { node: old, .. }) = (relocate, &entry.0)
            else {
                continue;
            };
            let Some(value) = relocate(old, links, moves, &moved) else {
                continue;
            };
            moved.insert(old.as_ptr().addr(), value.clone());
            cache.insert(hash, CacheEntry::node(value, entry.weight()));
        }
    }
}

fn take_varint(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for index in 0..MAX_VARINT {
        let byte = *bytes
            .get(*at)
            .ok_or_else(|| Error::Corrupt("a drafted record runs past its frame".into()))?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Error::Corrupt("varint overflow in a drafted record".into()))
}

fn relayout(
    old: &[u8],
    from: u64,
    to: u64,
    hashes: &[(u64, Hash)],
    known: impl Fn(u64, &Hash) -> Option<u64>,
) -> Result<(Vec<u8>, Moves)> {
    let mut out = Vec::with_capacity(old.len() + old.len() / 32);
    out.extend_from_slice(&old[..FRAME_BYTES]);
    let mut olds = Vec::with_capacity(hashes.len());
    let mut news = Vec::with_capacity(hashes.len());
    let mut at = FRAME_BYTES;
    let missing = || Error::Corrupt("a drafted record links one its frame does not hold".into());
    while at < old.len() {
        let was = from + at as u64;
        let now = to + out.len() as u64;
        let begun = at;
        let stored = usize::try_from(take_varint(old, &mut at)?)
            .map_err(|_| Error::Corrupt("body length overflow".into()))?;
        let flagged = take_varint(old, &mut at)?;
        if flagged & 1 == 1 {
            take_varint(old, &mut at)?;
        }
        at += stored;
        let hash = match hashes.get(olds.len()) {
            Some((offset, hash)) if *offset == was => hash,
            _ => return Err(Error::Corrupt("a drafted record has no hash".into())),
        };
        if let Some(found) = known(was, hash) {
            for _ in 0..flagged >> 1 {
                at += HASH_BYTES;
                take_varint(old, &mut at)?;
            }
            olds.push(was);
            news.push(found);
            continue;
        }
        out.extend_from_slice(old.get(begun..at).ok_or_else(missing)?);
        for _ in 0..flagged >> 1 {
            out.extend_from_slice(old.get(at..at + HASH_BYTES).ok_or_else(missing)?);
            at += HASH_BYTES;
            let target = was.wrapping_sub(unzigzag(take_varint(old, &mut at)?) as u64);
            let target = match target >= from {
                true => news[olds.binary_search(&target).map_err(|_| missing())?],
                false => target,
            };
            push_varint(&mut out, zigzag(now.wrapping_sub(target) as i64));
        }
        olds.push(was);
        news.push(now);
    }
    Ok((out, Moves::new(from, olds, news)))
}

impl Drop for Txn {
    fn drop(&mut self) {
        if self.start == self.origin {
            return;
        }
        let inner = &self.store.inner;
        let mut cursor = inner.cursor.lock();
        cursor.reserved = cursor.reserved.min(self.origin);
        if inner.file.set_len(self.origin).is_err() {
            self.store.poison();
        }
    }
}

impl Sink for Txn {
    fn check_link(&self, ptr: Ptr) -> Result<()> {
        if self.seen.get(&ptr.hash) == Some(&ptr.offset) {
            return Ok(());
        }
        match self.store.read_object(ptr) {
            Ok(_) => Ok(()),
            Err(Error::Corrupt(_)) => Err(Error::StalePointer(ptr.offset)),
            Err(error) => Err(error),
        }
    }

    fn put(&mut self, object: Object, hash: Hash) -> Result<Ptr> {
        self.put_ref(&object, hash)
    }

    fn packs(&self) -> bool {
        true
    }

    fn put_staged(&mut self, hash: Hash, staged: Staged<'_>) -> Result<Ptr> {
        if let Some(ptr) = self.known(hash, staged.raw)? {
            return Ok(ptr);
        }
        Ok(self.record(hash, staged))
    }

    fn remember(&mut self, ptr: Ptr, value: Erased, weight: u32) {
        self.remembered
            .push((ptr.hash, CacheEntry::node(value, weight), None));
    }

    fn remember_moving(
        &mut self,
        ptr: Ptr,
        value: Erased,
        weight: u32,
        relocate: (Relocate, usize),
    ) {
        self.remembered
            .push((ptr.hash, CacheEntry::node(value, weight), Some(relocate)));
    }

    fn store_id(&self) -> u64 {
        self.store.inner.id
    }

    fn store(&self) -> Option<Store> {
        Some(self.store.clone())
    }
}

fn reserve(inner: &Inner, cursor: &mut Cursor, end: u64) {
    if inner.options.durability != Durability::Fsync || end <= cursor.reserved {
        return;
    }
    if inner.options.shared
        && let Ok(meta) = inner.file.metadata()
        && meta.len() > end
    {
        cursor.reserved = meta.len();
        return;
    }
    let zeros = vec![0u8; cursor.reserve as usize];
    if inner.file.write_all_at(&zeros, end).is_ok() {
        cursor.reserved = end + cursor.reserve;
        cursor.reserve = (cursor.reserve * 2).clamp(FIRST_RESERVE, MAX_RESERVE);
    } else {
        cursor.reserved = u64::MAX;
    }
}

impl Txn {
    fn known(&self, hash: Hash, raw: usize) -> Result<Option<Ptr>> {
        if u32::try_from(raw).is_err() {
            return Err(Error::TooLarge(raw as u64));
        }
        let offset = self
            .seen
            .get(&hash)
            .copied()
            .or_else(|| match self.unshared {
                true => None,
                false => self
                    .store
                    .inner
                    .dedupe
                    .get(&hash)
                    .filter(|offset| *offset < self.origin),
            });
        Ok(offset.map(|offset| Ptr { hash, offset }))
    }

    pub(crate) fn unshared(&mut self) {
        if !self.unshared {
            self.unshared = true;
            self.shared_end = self.start + self.buf.len() as u64;
        }
    }

    pub(crate) fn unreserved(&mut self) {
        self.reserve = false;
    }

    pub(crate) fn put_ref(&mut self, object: &Object, hash: Hash) -> Result<Ptr> {
        if let Some(ptr) = self.known(hash, object.body.len())? {
            return Ok(ptr);
        }
        let packed = compressed(&object.body);
        Ok(self.record(hash, staged(&object.body, packed.as_deref(), &object.links)))
    }

    pub(crate) fn rebase(&mut self) -> Result<Settling> {
        let remembered = mem::take(&mut self.remembered);
        let now = self.store.inner.placed.load(Ordering::Acquire);
        if now != self.origin && self.start != self.origin {
            return Err(Error::Corrupt("a draft that spilled cannot move".into()));
        }
        let moves = match now == self.origin {
            true => None,
            false => {
                let mut hashes: Vec<(u64, Hash)> = self
                    .seen
                    .iter()
                    .filter(|(_, offset)| **offset >= self.origin)
                    .map(|(hash, offset)| (*offset, *hash))
                    .collect();
                hashes.sort_unstable_by_key(|(offset, _)| *offset);
                let (dedupe, shared_end) = (&self.store.inner.dedupe, self.shared_end);
                let known = |was: u64, hash: &Hash| match was < shared_end {
                    true => dedupe.get(hash).filter(|offset| *offset < now),
                    false => None,
                };
                let (buf, moves) = relayout(&self.buf, self.origin, now, &hashes, known)?;
                for offset in self.seen.values_mut() {
                    *offset = moves
                        .offset(*offset)
                        .ok_or_else(|| Error::Corrupt("a drafted object has no record".into()))?;
                }
                self.buf = buf;
                self.origin = now;
                self.start = now;
                Some(moves)
            }
        };
        Ok(Settling {
            moves,
            remembered,
            seen: Seen::default(),
        })
    }

    fn record(&mut self, hash: Hash, staged: Staged<'_>) -> Ptr {
        let offset = self.start + self.buf.len() as u64;
        append_staged(&mut self.buf, offset, staged);
        self.seen.insert(hash, offset);
        Ptr { hash, offset }
    }

    pub(crate) fn put_record(&mut self, hash: Hash, staged: Staged<'_>) -> Result<Ptr> {
        if let Some(ptr) = self.known(hash, staged.raw)? {
            self.seen.insert(hash, ptr.offset);
            return Ok(ptr);
        }
        Ok(self.record(hash, staged))
    }

    pub(crate) fn put_unlisted(&mut self, hash: Hash, staged: Staged<'_>) -> Result<Ptr> {
        if let Some(ptr) = self.known(hash, staged.raw)? {
            return Ok(ptr);
        }
        let offset = self.start + self.buf.len() as u64;
        append_staged(&mut self.buf, offset, staged);
        Ok(Ptr { hash, offset })
    }

    pub(crate) fn put_fresh(&mut self, object: Object, hash: Hash) -> Result<Ptr> {
        let packed = compressed(&object.body);
        Ok(self.record(hash, staged(&object.body, packed.as_deref(), &object.links)))
    }

    pub(crate) fn buffered(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn offset_of(&self, hash: &Hash) -> Option<u64> {
        self.seen.get(hash).copied()
    }

    pub(crate) fn spill(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.store.inner.file.write_all_at(&self.buf, self.start)?;
        self.start += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }

    pub(crate) fn read_object(&self, ptr: Ptr) -> Result<Object> {
        let end = self.start + self.buf.len() as u64;
        let (held, tail) = match ptr.offset.checked_sub(self.start) {
            Some(at) if ptr.offset < end => (&self.buf[at as usize..], end),
            _ if ptr.offset < self.origin => (
                &[][..],
                self.store.readable_past(ptr.offset)?.min(self.start),
            ),
            _ => (&[][..], self.start),
        };
        let verify = ptr.offset < self.origin;
        let file = Some(&*self.store.inner.file);
        let (object, _, _) = Store::read_within(file, ptr, tail, held, verify, false)?;
        Ok(object)
    }

    fn hash_spilled(&self, hasher: &mut Xxh3) -> Result<()> {
        let mut at = self.origin + FRAME_BYTES as u64;
        let mut chunk = vec![0u8; SCAN_CHUNK.min((self.start - at) as usize)];
        while at < self.start {
            let take = chunk.len().min((self.start - at) as usize);
            self.store
                .inner
                .file
                .read_exact_at(&mut chunk[..take], at)?;
            hasher.update(&chunk[..take]);
            at += take as u64;
        }
        Ok(())
    }

    pub(crate) fn place(mut self, head: Ptr) -> Result<Landing> {
        let store = self.store.clone();
        let inner = &store.inner;
        let end = self.start + self.buf.len() as u64;
        let mut cursor = inner.cursor.lock();
        if inner.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        if self.origin != inner.placed.load(Ordering::Acquire) {
            return Err(Error::Corrupt(
                "a frame was placed away from the end of the log".into(),
            ));
        }
        let seq = cursor
            .seq
            .checked_add(1)
            .ok_or_else(|| Error::Corrupt("the log has used its last sequence number".into()))?;
        let frame = Frame { seq, end, head };
        let mut landing = Landing {
            store: store.clone(),
            frame,
            at: self.origin,
            buf: Vec::new(),
            pending: false,
        };
        if self.start == self.origin {
            if self.reserve {
                reserve(inner, &mut cursor, end);
            }
            landing.buf = mem::take(&mut self.buf);
            landing.pending = true;
        } else {
            let mut header = frame.header();
            let mut hasher = Xxh3::new();
            hasher.update(&header[..FRAME_CHECKED]);
            self.hash_spilled(&mut hasher)?;
            hasher.update(&self.buf);
            header[FRAME_CHECKED..FRAME_BYTES].copy_from_slice(&hasher.digest128().to_le_bytes());
            if self.reserve {
                reserve(inner, &mut cursor, end);
            }
            let written = inner
                .file
                .write_all_at(&self.buf, self.start)
                .and_then(|()| inner.file.write_all_at(&header, self.origin));
            if let Err(error) = written {
                store.poison();
                return Err(error.into());
            }
            self.origin = self.start;
        }
        cursor.seq = frame.seq;
        inner.placed.store(end, Ordering::Release);
        drop(cursor);
        if !landing.pending {
            store.arrived(landing.at, end);
        }
        for (hash, offset) in mem::take(&mut self.seen) {
            inner.dedupe.insert(hash, offset);
        }
        for (hash, entry, _) in mem::take(&mut self.remembered) {
            inner.cache.insert(hash, entry);
        }
        Ok(landing)
    }

    pub(crate) fn write(self, head: Ptr) -> Result<Frame> {
        let landing = self.place(head)?;
        let frame = landing.frame;
        landing.land()?;
        Ok(frame)
    }

    pub fn commit(self, head: Ptr) -> Result<()> {
        let store = self.store.clone();
        let frame = self.write(head)?;
        store.sync(frame)
    }
}

pub(crate) struct Landing {
    store: Store,
    frame: Frame,
    at: u64,
    buf: Vec<u8>,
    pending: bool,
}

impl Landing {
    pub(crate) fn frame(&self) -> Frame {
        self.frame
    }

    pub(crate) fn land(mut self) -> Result<()> {
        if !self.pending {
            return Ok(());
        }
        self.pending = false;
        let mut header = self.frame.header();
        let mut hasher = Xxh3::new();
        hasher.update(&header[..FRAME_CHECKED]);
        hasher.update(&self.buf[FRAME_BYTES..]);
        header[FRAME_CHECKED..FRAME_BYTES].copy_from_slice(&hasher.digest128().to_le_bytes());
        self.buf[..FRAME_BYTES].copy_from_slice(&header);
        if let Err(error) = self.store.inner.file.write_all_at(&self.buf, self.at) {
            self.store.poison_at(self.at);
            return Err(error.into());
        }
        self.store.arrived(self.at, self.frame.end);
        Ok(())
    }
}

impl Drop for Landing {
    fn drop(&mut self) {
        if self.pending {
            self.store.poison_at(self.at);
        }
    }
}

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

fn scratch(near: &Path) -> Result<File> {
    loop {
        let mut name = near.as_os_str().to_os_string();
        let id = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        name.push(format!(".receiving.{}.{id}", std::process::id()));
        match platform::scratch(Path::new(&name)) {
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            opened => return Ok(opened?),
        }
    }
}

pub(crate) struct Staging {
    near: PathBuf,
    limit: usize,
    file: Option<File>,
    start: u64,
    buf: Vec<u8>,
    seen: Seen,
    count: usize,
}

pub(crate) struct Record {
    pub(crate) hash: Hash,
    pub(crate) raw: usize,
    pub(crate) packed: bool,
    pub(crate) payload: Vec<u8>,
    pub(crate) links: Vec<Ptr>,
}

impl Record {
    pub(crate) fn staged<'a>(&'a self, links: &'a [Ptr]) -> Staged<'a> {
        Staged {
            raw: self.raw,
            payload: &self.payload,
            packed: self.packed,
            links,
        }
    }
}

fn parse_varint(bytes: &[u8], pos: &mut usize) -> Result<Option<u64>> {
    let mut value = 0u64;
    for index in 0..MAX_VARINT {
        let Some(&byte) = bytes.get(*pos) else {
            return Ok(None);
        };
        *pos += 1;
        if index == MAX_VARINT - 1 && byte > 1 {
            break;
        }
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok(Some(value));
        }
    }
    Err(Error::Corrupt("varint overflow in a staged record".into()))
}

fn parse_record(bytes: &[u8], at: u64) -> Result<Option<(Record, usize)>> {
    let Some(hash) = bytes.get(..HASH_BYTES) else {
        return Ok(None);
    };
    let hash: Hash = hash.try_into().expect("slice length is HASH_BYTES");
    let offset = at + HASH_BYTES as u64;
    let mut pos = HASH_BYTES;
    let size = |value: u64| {
        usize::try_from(value).map_err(|_| Error::Corrupt("staged record overflow".into()))
    };
    let Some(stored) = parse_varint(bytes, &mut pos)? else {
        return Ok(None);
    };
    let stored = size(stored)?;
    let Some(flagged) = parse_varint(bytes, &mut pos)? else {
        return Ok(None);
    };
    let packed = flagged & 1 == 1;
    let raw = match packed {
        true => match parse_varint(bytes, &mut pos)? {
            Some(raw) => size(raw)?,
            None => return Ok(None),
        },
        false => stored,
    };
    let Some(payload) = bytes.get(pos..pos.saturating_add(stored)) else {
        return Ok(None);
    };
    let payload = payload.to_vec();
    pos += stored;
    let count = size(flagged >> 1)?;
    let mut links = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let Some(found) = bytes.get(pos..pos + HASH_BYTES) else {
            return Ok(None);
        };
        let link: Hash = found.try_into().expect("slice length is HASH_BYTES");
        pos += HASH_BYTES;
        let Some(delta) = parse_varint(bytes, &mut pos)? else {
            return Ok(None);
        };
        links.push(Ptr {
            hash: link,
            offset: offset.wrapping_sub(unzigzag(delta) as u64),
        });
    }
    let record = Record {
        hash,
        raw,
        packed,
        payload,
        links,
    };
    Ok(Some((record, pos)))
}

impl Staging {
    pub(crate) fn new(near: &Path, limit: usize) -> Staging {
        Staging {
            near: near.to_path_buf(),
            limit,
            file: None,
            start: LOG_START,
            buf: Vec::new(),
            seen: HashMap::default(),
            count: 0,
        }
    }

    pub(crate) fn put(&mut self, hash: Hash, staged: Staged<'_>) -> Result<Ptr> {
        let links = staged.links.len() * (HASH_BYTES + MAX_VARINT);
        let most = HASH_BYTES + staged.payload.len() + 3 * MAX_VARINT + links;
        if !self.buf.is_empty() && self.buf.len() + most > self.limit {
            self.spill()?;
        }
        self.buf.extend_from_slice(&hash);
        let offset = self.start + self.buf.len() as u64;
        append_staged(&mut self.buf, offset, staged);
        self.seen.insert(hash, offset);
        self.count += 1;
        Ok(Ptr { hash, offset })
    }

    pub(crate) fn offset_of(&self, hash: &Hash) -> Option<u64> {
        self.seen.get(hash).copied()
    }

    pub(crate) fn spilled(&self) -> bool {
        self.file.is_some()
    }

    pub(crate) fn len(&self) -> usize {
        self.count
    }

    pub(crate) fn spill(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let file = match &self.file {
            Some(file) => file,
            None => self.file.insert(scratch(&self.near)?),
        };
        file.write_all_at(&self.buf, self.start)?;
        self.start += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }

    pub(crate) fn read_object(&self, ptr: Ptr) -> Result<Object> {
        let end = self.start + self.buf.len() as u64;
        let (held, tail) = match ptr.offset.checked_sub(self.start) {
            Some(at) if ptr.offset < end => (&self.buf[at as usize..], end),
            _ => (&[][..], self.start),
        };
        let (object, _, _) = Store::read_within(self.file.as_ref(), ptr, tail, held, false, false)?;
        Ok(object)
    }

    pub(crate) fn seal(&mut self) -> Result<()> {
        self.seen = HashMap::default();
        if self.spilled() {
            self.spill()?;
            self.buf = Vec::new();
        }
        Ok(())
    }

    pub(crate) fn replay(&self) -> Replay<'_> {
        Replay {
            staging: self,
            at: LOG_START,
            window: Vec::new(),
            window_at: LOG_START,
        }
    }
}

pub(crate) struct Replay<'a> {
    staging: &'a Staging,
    at: u64,
    window: Vec<u8>,
    window_at: u64,
}

impl Replay<'_> {
    pub(crate) fn next_record(&mut self) -> Result<Option<Record>> {
        let staging = self.staging;
        if self.at == staging.start + staging.buf.len() as u64 {
            return Ok(None);
        }
        let parsed = match self.at.checked_sub(staging.start) {
            Some(at) => parse_record(&staging.buf[at as usize..], self.at)?,
            None => self.spilled_record()?,
        };
        let (record, len) =
            parsed.ok_or_else(|| Error::Corrupt("a staged record runs past its end".into()))?;
        self.at += len as u64;
        Ok(Some(record))
    }

    fn spilled_record(&mut self) -> Result<Option<(Record, usize)>> {
        let file = self
            .staging
            .file
            .as_ref()
            .ok_or_else(|| Error::Corrupt("a staged record was never written".into()))?;
        loop {
            let from = usize::try_from(self.at - self.window_at).unwrap_or(usize::MAX);
            let have = self.window.len().saturating_sub(from);
            if have > 0
                && let Some(found) = parse_record(&self.window[from..], self.at)?
            {
                return Ok(Some(found));
            }
            let left = usize::try_from(self.staging.start - self.at).unwrap_or(usize::MAX);
            let want = have.saturating_mul(2).max(REPLAY_CHUNK).min(left);
            if want <= have {
                return Ok(None);
            }
            self.window.clear();
            self.window.resize(want, 0);
            file.read_exact_at(&mut self.window, self.at)?;
            self.window_at = self.at;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_only_the_cached_entry_releases_its_value() {
        for lent in [false, true] {
            let value = Shared::new(7u64);
            let weak = Shared::downgrade(&value);
            let entry = CacheEntry::node(Shared::erase(value.clone()), 64);
            assert!(value.held());
            if lent {
                let guard = crate::epoch::pin().unwrap();
                assert_eq!(weak.lend(&guard), Some(&7));
            }
            let mut copy = entry.clone();
            assert!(matches!(copy.take(), Entry::Node { owned: false, .. }));
            drop(copy);
            drop(entry.clone());
            assert!(value.held());
            assert_eq!(Shared::strong_count(&value), 2);
            drop(entry);
            assert!(!value.held());
            assert_eq!(Shared::strong_count(&value), if lent { 2 } else { 1 });
            crate::epoch::flush();
            assert_eq!(Shared::strong_count(&value), 1);
        }
    }
}
