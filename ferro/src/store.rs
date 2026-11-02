use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use quick_cache::Weighter;
use quick_cache::sync::Cache;
use xxhash_rust::xxh3::Xxh3;

use crate::codec::{Decoder, HASH_BYTES, Hash, Object, Ptr, Sink, push_varint};
use crate::error::{Error, Result};
use crate::map::SPILL_BYTES;

const MAGIC: &[u8; 8] = b"FERRODB2";
const FRAME_MAGIC: &[u8; 8] = b"FERROFRM";
const GAP_MAGIC: &[u8; 8] = b"FERROGAP";
const SLOT_BYTES: u64 = 4096;
pub(crate) const LOG_START: u64 = 2 * SLOT_BYTES;
const FRAME_CHECKED: usize = 32 + HASH_BYTES;
const SUPER_CHECKED: usize = FRAME_CHECKED + 44;
const SUPER_BYTES: usize = SUPER_CHECKED + 32;
const FRAME_BYTES: usize = FRAME_CHECKED + 16;
const SCAN_CHUNK: usize = 1 << 20;
const FIRST_RESERVE: u64 = 64 << 10;
const MAX_RESERVE: u64 = 4 << 20;
const LOCK_RETRIES: u32 = 5;
const READ_AHEAD: usize = 1024;
const ENTRY_BYTES: usize = 96;
const MIN_DEDUPE: u64 = 1 << 10;
const MAX_DEDUPE: u64 = 1 << 16;
const MAX_VARINT: usize = 10;
const COMPRESS_MIN: usize = 96;
const MAX_RATIO: usize = 255;
const MIN_BLOCK: u64 = 4096;
const PUNCH_CHUNK: u64 = 64 << 20;
const OPEN_RETRIES: u32 = 64;

static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    Fsync,
    Buffered,
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Write(u64, Vec<u8>),
    SetLen(u64),
    Sync,
    Punch(u64, u64),
    Acked(u64),
}

#[doc(hidden)]
pub type Trace = Arc<Mutex<Vec<Op>>>;

#[derive(Clone, Debug)]
pub struct StoreOptions {
    pub durability: Durability,
    pub cache_bytes: u64,
    pub read_only: bool,
    #[doc(hidden)]
    pub trace: Option<Trace>,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            durability: Durability::Fsync,
            cache_bytes: 1 << 30,
            read_only: false,
            trace: None,
        }
    }
}

pub(crate) fn heap_weight(fixed_bytes: usize, body_bytes: usize) -> u32 {
    let weight = ENTRY_BYTES + fixed_bytes + body_bytes;
    u32::try_from(weight).unwrap_or(u32::MAX)
}

#[derive(Clone)]
struct CacheEntry {
    value: Arc<dyn Any + Send + Sync>,
    offset: u64,
    weight: u32,
}

#[derive(Clone)]
struct EntryWeight;

impl Weighter<Hash, CacheEntry> for EntryWeight {
    fn weight(&self, _key: &Hash, entry: &CacheEntry) -> u64 {
        u64::from(entry.weight)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Moved {
    pub(crate) floor: u64,
    pub(crate) table: Option<Ptr>,
    pub(crate) live: u64,
}

#[derive(Clone, Copy)]
struct Super {
    seq: u64,
    log_end: u64,
    head: Option<Ptr>,
    moved: Moved,
}

impl Super {
    fn fill(&self, magic: &[u8; 8], out: &mut [u8]) {
        out[0..8].copy_from_slice(magic);
        out[8..16].copy_from_slice(&self.seq.to_le_bytes());
        out[16..24].copy_from_slice(&self.log_end.to_le_bytes());
        if let Some(head) = self.head {
            out[24..32].copy_from_slice(&head.offset.to_le_bytes());
            out[32..32 + HASH_BYTES].copy_from_slice(&head.hash);
        }
    }

    fn fields(bytes: &[u8]) -> Option<Super> {
        let seq = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        let log_end = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
        let offset = u64::from_le_bytes(bytes[24..32].try_into().ok()?);
        let hash: Hash = bytes[32..32 + HASH_BYTES].try_into().ok()?;
        let head = (offset != 0).then_some(Ptr { hash, offset });
        Some(Super {
            seq,
            log_end,
            head,
            moved: Moved::default(),
        })
    }

    fn encode(&self) -> [u8; SUPER_BYTES] {
        let mut out = [0u8; SUPER_BYTES];
        self.fill(MAGIC, &mut out);
        let extra = &mut out[FRAME_CHECKED..SUPER_CHECKED];
        extra[..8].copy_from_slice(&self.moved.floor.to_le_bytes());
        extra[8..16].copy_from_slice(&self.moved.live.to_le_bytes());
        if let Some(table) = self.moved.table {
            extra[16..24].copy_from_slice(&table.offset.to_le_bytes());
            extra[24..24 + HASH_BYTES].copy_from_slice(&table.hash);
        }
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
        let extra = &bytes[FRAME_CHECKED..SUPER_CHECKED];
        let floor = u64::from_le_bytes(extra[..8].try_into().ok()?);
        let live = u64::from_le_bytes(extra[8..16].try_into().ok()?);
        let offset = u64::from_le_bytes(extra[16..24].try_into().ok()?);
        let hash: Hash = extra[24..24 + HASH_BYTES].try_into().ok()?;
        let table = (offset != 0).then_some(Ptr { hash, offset });
        let moved = Moved { floor, table, live };
        Some(Super {
            moved,
            ..Super::fields(bytes)?
        })
    }
}

#[derive(Default)]
pub(crate) struct Places {
    pub(crate) moved: Moved,
    offsets: Vec<u64>,
    places: Vec<u64>,
}

impl Places {
    pub(crate) fn new(moved: Moved, pairs: Vec<(u64, u64)>) -> Places {
        let (offsets, places) = pairs.into_iter().unzip();
        Places {
            moved,
            offsets,
            places,
        }
    }

    pub(crate) fn place(&self, offset: u64) -> Option<u64> {
        if offset >= self.moved.floor {
            return Some(offset);
        }
        self.offsets
            .binary_search(&offset)
            .ok()
            .map(|index| self.places[index])
    }

    fn holds(&self, offset: u64) -> bool {
        offset >= self.moved.floor || self.offsets.binary_search(&offset).is_ok()
    }

    pub(crate) fn encode(pairs: &[(u64, u64)]) -> Vec<u8> {
        let mut out = Vec::with_capacity(pairs.len() * 6 + 10);
        push_varint(&mut out, pairs.len() as u64);
        let (mut last_offset, mut last_place) = (0u64, 0u64);
        for (offset, place) in pairs {
            push_varint(&mut out, offset - last_offset);
            push_varint(&mut out, zigzag(place.wrapping_sub(last_place) as i64));
            (last_offset, last_place) = (*offset, *place);
        }
        out
    }

    fn decode(moved: Moved, body: &[u8]) -> Result<Places> {
        let mut decoder = Decoder::new(body, &[], None);
        let count = decoder.varint()? as usize;
        if count > body.len() / 2 {
            return Err(Error::Corrupt("place table claims too many entries".into()));
        }
        let mut pairs = Vec::with_capacity(count);
        let (mut offset, mut place) = (0u64, 0u64);
        for _ in 0..count {
            offset = offset.wrapping_add(decoder.varint()?);
            place = place.wrapping_add(unzigzag(decoder.varint()?) as u64);
            pairs.push((offset, place));
        }
        Ok(Places::new(moved, pairs))
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
            && best.as_ref().is_none_or(|b| candidate.seq > b.0.seq)
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
    if !read_or_end(file, &mut header, start)? {
        return Ok(None);
    }
    let gap = &header[0..8] == GAP_MAGIC;
    if !gap && &header[0..8] != FRAME_MAGIC {
        return Ok(None);
    }
    let Some(mut frame) = Super::fields(&header) else {
        return Ok(None);
    };
    frame.moved = tip.moved;
    if frame.seq != tip.seq + 1 || frame.log_end < body_start || frame.log_end > len {
        return Ok(None);
    }
    let mut hasher = Xxh3::new();
    hasher.update(&header[..FRAME_CHECKED]);
    let checked = if gap { 0 } else { frame.log_end - body_start };
    let mut chunk = vec![0u8; SCAN_CHUNK.min(checked as usize)];
    let mut at = body_start;
    while !gap && at < frame.log_end {
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
    let before = sb.seq;
    let tip = scan(file, sb, len)?;
    let moved = tip.seq != before;
    let shrink = len > tip.log_end;
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
    Ok((tip, slot))
}

fn observe(file: &File, sb: Super, len: u64) -> Result<Super> {
    if read_frame(file, &sb, len)?.is_none() || file.try_lock_shared().is_err() {
        return Ok(sb);
    }
    let before = sb.seq;
    let scanned = file
        .metadata()
        .map_err(Error::from)
        .and_then(|meta| scan(file, sb, meta.len()))
        .and_then(|tip| {
            if tip.seq != before {
                file.sync_data()?;
            }
            Ok(tip)
        });
    file.unlock()?;
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

fn compressed(body: &[u8]) -> Option<Vec<u8>> {
    if body.len() < COMPRESS_MIN {
        return None;
    }
    let packed = lz4_flex::block::compress(body);
    (packed.len() + varint_len(body.len() as u64) < body.len()).then_some(packed)
}

pub fn append_record(buf: &mut Vec<u8>, offset: u64, body: &[u8], links: &[Ptr]) {
    match compressed(body) {
        Some(packed) => {
            push_varint(buf, packed.len() as u64);
            push_varint(buf, (links.len() as u64) << 1 | 1);
            push_varint(buf, body.len() as u64);
            buf.extend_from_slice(&packed);
        }
        None => {
            push_varint(buf, body.len() as u64);
            push_varint(buf, (links.len() as u64) << 1);
            buf.extend_from_slice(body);
        }
    }
    for link in links {
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
    file: &'a File,
    start: u64,
    tail: u64,
    bytes: Vec<u8>,
    pos: usize,
}

impl RecordReader<'_> {
    fn ensure(&mut self, upto: usize) -> Result<()> {
        if upto <= self.bytes.len() {
            return Ok(());
        }
        if self.start + upto as u64 > self.tail {
            return Err(Error::Corrupt(format!(
                "record at {} runs past end of log",
                self.start
            )));
        }
        let have = self.bytes.len();
        let want = (upto - have).max(READ_AHEAD);
        let want = want.min((self.tail - self.start) as usize - have);
        self.bytes.resize(have + want, 0);
        self.file
            .read_exact_at(&mut self.bytes[have..], self.start + have as u64)?;
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

    fn prefix(&mut self) -> Result<(usize, usize, bool)> {
        let stored_len = usize::try_from(self.varint()?)
            .map_err(|_| Error::Corrupt("body length overflow".into()))?;
        let flagged = self.varint()?;
        let link_count = usize::try_from(flagged >> 1)
            .map_err(|_| Error::Corrupt("link count overflow".into()))?;
        Ok((stored_len, link_count, flagged & 1 == 1))
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

pub(crate) fn seal(file: &File, log_end: u64, head: Ptr) -> Result<()> {
    file.sync_data()?;
    let sb = Super {
        seq: 1,
        log_end,
        head: Some(head),
        moved: Moved::default(),
    };
    file.write_all_at(&sb.encode(), SLOT_BYTES)?;
    file.sync_all()?;
    Ok(())
}

fn open_locked(path: &Path, read_only: bool) -> Result<File> {
    let mut retries = 0;
    loop {
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .create(!read_only)
            .truncate(false)
            .open(path)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound if read_only => {
                    Error::NoDatabase(path.display().to_string())
                }
                _ => error.into(),
            })?;
        if read_only {
            return Ok(file);
        }
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) if retries == LOCK_RETRIES => return Err(Error::Locked),
            Err(TryLockError::WouldBlock) => {
                retries += 1;
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        if std::fs::metadata(path).is_ok_and(|on_disk| on_disk.ino() == file.metadata().map(|m| m.ino()).unwrap_or(0)) {
            return Ok(file);
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Frame {
    pub(crate) seq: u64,
    pub(crate) end: u64,
    pub(crate) head: Ptr,
}

struct Cursor {
    seq: u64,
    reserved: u64,
    reserve: u64,
}

struct Traced {
    file: File,
    trace: Option<Trace>,
}

impl Traced {
    fn log(&self, op: impl FnOnce() -> Op) {
        if let Some(trace) = &self.trace {
            trace.lock().push(op());
        }
    }

    fn write_all_at(&self, bytes: &[u8], at: u64) -> std::io::Result<()> {
        self.log(|| Op::Write(at, bytes.to_vec()));
        self.file.write_all_at(bytes, at)
    }

    fn sync_data(&self) -> std::io::Result<()> {
        self.file.sync_data()?;
        self.log(|| Op::Sync);
        Ok(())
    }

    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.log(|| Op::SetLen(len));
        self.file.set_len(len)
    }
}

impl std::ops::Deref for Traced {
    type Target = File;

    fn deref(&self) -> &File {
        &self.file
    }
}

struct Inner {
    id: u64,
    path: PathBuf,
    file: Traced,
    tail: AtomicU64,
    written: AtomicU64,
    cursor: Mutex<Cursor>,
    slot: AtomicU64,
    cache: Cache<Hash, CacheEntry, EntryWeight>,
    dedupe: Cache<Hash, u64>,
    places: ArcSwap<Places>,
    reloading: Mutex<()>,
    poisoned: AtomicBool,
    closed: AtomicBool,
    options: StoreOptions,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl Store {
    pub fn open(path: &Path, options: StoreOptions) -> Result<(Store, Option<Ptr>)> {
        let file = open_locked(path, options.read_only)?;
        let len = file.metadata()?.len();
        let existing = read_supers(&file, len)?;
        let (sb, slot) = match existing {
            Some((sb, slot)) => {
                let len = file.metadata()?.len();
                if len < sb.log_end {
                    return Err(Error::Corrupt(format!(
                        "file is {len} bytes but superblock expects {}",
                        sb.log_end
                    )));
                }
                if options.read_only {
                    (observe(&file, sb, len)?, slot)
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
                let sb = Super {
                    seq: 0,
                    log_end: LOG_START,
                    head: None,
                    moved: Moved::default(),
                };
                file.set_len(LOG_START)?;
                file.write_all_at(&sb.encode(), 0)?;
                file.sync_all()?;
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    File::open(parent)?.sync_all()?;
                }
                (sb, 0)
            }
        };
        let cache_bytes = options.cache_bytes.max(1);
        let items = ((cache_bytes / 4096).max(64)) as usize;
        let dedupe = (cache_bytes / 4096).clamp(MIN_DEDUPE, MAX_DEDUPE) as usize;
        let head = sb.head;
        let store = Store {
            inner: Arc::new(Inner {
                id: NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed),
                path: path.to_path_buf(),
                file: Traced {
                    file,
                    trace: options.trace.clone(),
                },
                tail: AtomicU64::new(sb.log_end),
                written: AtomicU64::new(sb.log_end),
                cursor: Mutex::new(Cursor {
                    seq: sb.seq,
                    reserved: sb.log_end,
                    reserve: 0,
                }),
                slot: AtomicU64::new(slot),
                cache: Cache::with_weighter(items, cache_bytes, EntryWeight),
                dedupe: Cache::new(dedupe),
                places: ArcSwap::from_pointee(Places::default()),
                reloading: Mutex::new(()),
                poisoned: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                options,
            }),
        };
        let head = store.adopt_first(sb, head)?;
        Ok((store, head))
    }

    fn adopt_first(&self, sb: Super, head: Option<Ptr>) -> Result<Option<Ptr>> {
        let mut sb = sb;
        let mut head = head;
        for _ in 0..OPEN_RETRIES {
            match self.load_places(sb.moved) {
                Ok(places) => {
                    self.inner.places.store(Arc::new(places));
                    return Ok(head);
                }
                Err(error) if !self.inner.options.read_only => return Err(error),
                Err(error) => {
                    let len = self.inner.file.metadata()?.len();
                    let newer = read_supers(&self.inner.file, len)?.map(|found| found.0);
                    match newer {
                        Some(found) if found.moved != sb.moved => {
                            self.inner.cursor.lock().seq = found.seq;
                            self.inner.tail.fetch_max(found.log_end, Ordering::AcqRel);
                            self.inner.written.fetch_max(found.log_end, Ordering::AcqRel);
                            head = found.head;
                            sb = found;
                        }
                        _ => return Err(error),
                    }
                }
            }
        }
        Err(Error::Corrupt("place table kept moving during open".into()))
    }

    fn load_places(&self, moved: Moved) -> Result<Places> {
        let Some(table) = moved.table else {
            return Ok(Places {
                moved,
                ..Places::default()
            });
        };
        let tail = self.inner.written.load(Ordering::Acquire);
        let (object, _) = self.read_at(table, table.offset, tail)?;
        Places::decode(moved, &object.body)
    }

    fn newer_places(&self, current: &Arc<Places>) -> Result<Option<Arc<Places>>> {
        let inner = &self.inner;
        let now = inner.places.load_full();
        if !Arc::ptr_eq(&now, current) {
            return Ok(Some(now));
        }
        if !inner.options.read_only {
            return Ok(None);
        }
        let _guard = inner.reloading.lock();
        let now = inner.places.load_full();
        if !Arc::ptr_eq(&now, current) {
            return Ok(Some(now));
        }
        let len = inner.file.metadata()?.len();
        let Some((sb, _)) = read_supers(&inner.file, len)? else {
            return Ok(None);
        };
        if sb.moved == current.moved {
            return Ok(None);
        }
        inner.written.fetch_max(sb.log_end, Ordering::AcqRel);
        let places = Arc::new(self.load_places(sb.moved)?);
        inner.places.store(places.clone());
        Ok(Some(places))
    }

    pub(crate) fn places(&self) -> Arc<Places> {
        self.inner.places.load_full()
    }

    pub(crate) fn written(&self) -> u64 {
        self.inner.written.load(Ordering::Acquire)
    }

    pub(crate) fn allocated(&self) -> Result<u64> {
        Ok(self.inner.file.metadata()?.blocks() * 512)
    }

    pub(crate) fn on_disk(&self) -> u64 {
        let size = self.size();
        self.allocated().map_or(size, |bytes| bytes.min(size))
    }

    #[doc(hidden)]
    pub fn trace(&self, op: Op) {
        self.inner.file.log(|| op);
    }

    pub(crate) fn block_bytes(&self) -> Result<u64> {
        Ok(self.inner.file.metadata()?.blksize().max(MIN_BLOCK))
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

    pub fn is_read_only(&self) -> bool {
        self.inner.options.read_only
    }

    pub(crate) fn last_seq(&self) -> u64 {
        self.inner.cursor.lock().seq
    }

    pub fn refresh(&self) -> Result<Option<Ptr>> {
        let mut cursor = self.inner.cursor.lock();
        let opened = self.inner.file.metadata()?;
        if std::fs::metadata(&self.inner.path)?.ino() != opened.ino() {
            return Err(Error::Replaced);
        }
        let len = opened.len();
        let (sb, _) = read_supers(&self.inner.file, len)?
            .ok_or_else(|| Error::Corrupt("both superblocks are invalid".into()))?;
        if sb.seq < cursor.seq {
            return Ok(None);
        }
        cursor.seq = sb.seq;
        self.inner.tail.fetch_max(sb.log_end, Ordering::AcqRel);
        self.inner.written.fetch_max(sb.log_end, Ordering::AcqRel);
        if sb.moved != self.inner.places.load().moved {
            let _guard = self.inner.reloading.lock();
            let places = self.load_places(sb.moved)?;
            self.inner.places.store(Arc::new(places));
        }
        Ok(sb.head)
    }

    pub(crate) fn poison(&self) {
        self.inner.poisoned.store(true, Ordering::Release);
    }

    pub(crate) fn sync(&self, frame: Frame) -> Result<()> {
        let inner = &self.inner;
        let durable = inner.options.durability == Durability::Fsync;
        let attempt = (|| -> Result<()> {
            if durable {
                inner.file.sync_data()?;
            }
            let sb = Super {
                seq: frame.seq,
                log_end: frame.end,
                head: Some(frame.head),
                moved: inner.places.load().moved,
            };
            let slot = 1 - inner.slot.load(Ordering::Relaxed);
            inner.file.write_all_at(&sb.encode(), slot * SLOT_BYTES)?;
            inner.slot.store(slot, Ordering::Relaxed);
            Ok(())
        })();
        if let Err(error) = attempt {
            self.poison();
            return Err(error);
        }
        inner.tail.store(frame.end, Ordering::Release);
        Ok(())
    }

    pub(crate) fn publish(&self, frame: Frame, places: Places) -> Result<()> {
        let inner = &self.inner;
        let moved = places.moved;
        let attempt = (|| -> Result<u64> {
            inner.file.sync_data()?;
            let sb = Super {
                seq: frame.seq,
                log_end: frame.end,
                head: Some(frame.head),
                moved,
            };
            let first = 1 - inner.slot.load(Ordering::Relaxed);
            inner.file.write_all_at(&sb.encode(), first * SLOT_BYTES)?;
            inner.file.sync_data()?;
            inner.file.write_all_at(&sb.encode(), (1 - first) * SLOT_BYTES)?;
            inner.file.sync_data()?;
            Ok(1 - first)
        })();
        match attempt {
            Ok(slot) => {
                inner.slot.store(slot, Ordering::Relaxed);
                inner.places.store(Arc::new(places));
                Ok(())
            }
            Err(error) => {
                self.poison();
                Err(error)
            }
        }
    }

    pub(crate) fn punch(&self, start: u64, end: u64, block: u64) -> Result<u64> {
        let first = start.next_multiple_of(block);
        let last = end - end % block;
        let mut at = first;
        while at < last {
            let len = (last - at).min(PUNCH_CHUNK);
            if !punch_hole(&self.inner.file, at, len)? {
                return Ok(at - first);
            }
            self.inner.file.log(|| Op::Punch(at, len));
            at += len;
        }
        Ok(last.saturating_sub(first))
    }

    pub(crate) fn write_at(&self, bytes: &[u8], at: u64) -> Result<()> {
        self.inner.file.write_all_at(bytes, at)?;
        Ok(())
    }

    pub(crate) fn read_place(&self, buf: &mut [u8], at: u64) -> Result<()> {
        self.inner.file.read_exact_at(buf, at)?;
        Ok(())
    }

    pub(crate) fn sync_data(&self) -> Result<()> {
        self.inner.file.sync_data()?;
        Ok(())
    }

    pub(crate) fn scan_links(&self, from: u64, to: u64, mut each: impl FnMut(Ptr)) -> Result<()> {
        let file = &self.inner.file;
        let mut at = from;
        while at < to {
            let mut header = [0u8; FRAME_BYTES];
            file.read_exact_at(&mut header, at)?;
            let frame = Super::fields(&header)
                .ok_or_else(|| Error::Corrupt(format!("no frame at {at}")))?;
            if &header[0..8] == GAP_MAGIC {
                at = frame.log_end;
                continue;
            }
            if &header[0..8] != FRAME_MAGIC || frame.log_end > to {
                return Err(Error::Corrupt(format!("no frame at {at}")));
            }
            let mut record = at + FRAME_BYTES as u64;
            while record < frame.log_end {
                record = links_of(file, record, frame.log_end, &mut each)?;
            }
            at = frame.log_end;
        }
        Ok(())
    }

    pub fn read_object(&self, ptr: Ptr) -> Result<Object> {
        self.read_record(ptr).map(|(object, _)| object)
    }

    pub(crate) fn read_record(&self, ptr: Ptr) -> Result<(Object, u32)> {
        let tail = self.inner.written.load(Ordering::Acquire);
        if ptr.offset < LOG_START || ptr.offset >= tail {
            return Err(Error::Corrupt(format!(
                "pointer {} outside log of {tail} bytes",
                ptr.offset
            )));
        }
        let first = self.inner.places.load();
        let found = self.read_placed(ptr, &first);
        if !stale(&found) {
            return found;
        }
        let mut places = arc_swap::Guard::into_inner(first);
        let mut found = found;
        while let Some(newer) = self.newer_places(&places)? {
            places = newer;
            found = self.read_placed(ptr, &places);
            if !stale(&found) {
                break;
            }
        }
        found
    }

    fn read_placed(&self, ptr: Ptr, places: &Places) -> Result<(Object, u32)> {
        match places.place(ptr.offset) {
            Some(place) => self.read_at(ptr, place, self.inner.written.load(Ordering::Acquire)),
            None => Err(Error::Pruned(ptr.offset)),
        }
    }

    fn read_at(&self, ptr: Ptr, place: u64, tail: u64) -> Result<(Object, u32)> {
        if place >= tail {
            return Err(Error::Corrupt(format!(
                "object at {} placed past the end of the log",
                ptr.offset
            )));
        }
        let want = (tail - place).min(READ_AHEAD as u64) as usize;
        let mut bytes = vec![0u8; want];
        self.inner.file.read_exact_at(&mut bytes, place)?;
        let mut reader = RecordReader {
            file: &self.inner.file,
            start: place,
            tail,
            bytes,
            pos: 0,
        };
        let (stored_len, link_count, packed) = reader.prefix()?;
        let body = if packed {
            let raw_len = usize::try_from(reader.varint()?)
                .map_err(|_| Error::Corrupt("body length overflow".into()))?;
            if raw_len > stored_len.saturating_mul(MAX_RATIO).saturating_add(MAX_RATIO) {
                return Err(Error::Corrupt(format!(
                    "implausible compression at {}",
                    ptr.offset
                )));
            }
            lz4_flex::block::decompress(reader.take(stored_len)?, raw_len).map_err(|error| {
                Error::Corrupt(format!("bad compressed body at {}: {error}", ptr.offset))
            })?
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
        if object.hash() != ptr.hash {
            return Err(Error::Corrupt(format!(
                "hash mismatch for object at {}",
                ptr.offset
            )));
        }
        let len = u32::try_from(reader.pos).map_err(|_| Error::Corrupt("record overflow".into()))?;
        Ok((object, len))
    }

    pub(crate) fn fetch<T: Send + Sync + 'static>(
        &self,
        ptr: Ptr,
        decode: impl FnOnce(&mut Decoder<'_>) -> Result<T>,
        fixed_bytes: impl FnOnce(&T) -> usize,
    ) -> Result<(Arc<T>, bool)> {
        if let Some(entry) = self.inner.cache.get(&ptr.hash)
            && entry.offset == ptr.offset
            && let Ok(value) = entry.value.downcast::<T>()
        {
            return Ok((value, true));
        }
        let object = self.read_object(ptr)?;
        if object.body.len() >= SPILL_BYTES && !self.inner.options.read_only {
            self.inner.dedupe.insert(ptr.hash, ptr.offset);
        }
        let mut decoder = Decoder::new(&object.body, &object.links, Some(self));
        let value = Arc::new(decode(&mut decoder)?);
        let weight = heap_weight(fixed_bytes(&value), object.body.len());
        if !self.inner.closed.load(Ordering::SeqCst) {
            self.inner.cache.insert(
                ptr.hash,
                CacheEntry {
                    value: value.clone(),
                    offset: ptr.offset,
                    weight,
                },
            );
            if self.inner.closed.load(Ordering::SeqCst) {
                self.inner.cache.clear();
            }
        }
        Ok((value, false))
    }

    pub fn close(&self) {
        let inner = &self.inner;
        inner.closed.store(true, Ordering::SeqCst);
        inner.cache.clear();
        if inner.options.read_only || inner.poisoned.load(Ordering::Acquire) {
            return;
        }
        let end = inner.written.load(Ordering::Acquire);
        if inner.file.metadata().is_ok_and(|meta| meta.len() > end) {
            let _ = inner.file.set_len(end);
        }
    }

    pub fn load<T: crate::codec::Data>(&self, ptr: Ptr) -> Result<Arc<T>> {
        let (value, _) = self.fetch(ptr, |decoder| T::decode(decoder), |_| size_of::<T>())?;
        Ok(value)
    }

    pub fn begin(&self) -> Result<Txn> {
        if self.inner.options.read_only {
            return Err(Error::ReadOnly);
        }
        if self.inner.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        Ok(Txn {
            store: self.clone(),
            start: self.inner.written.load(Ordering::Acquire),
            buf: vec![0u8; FRAME_BYTES],
            seen: HashMap::new(),
            remembered: Vec::new(),
            places: self.inner.places.load_full(),
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

pub struct Txn {
    store: Store,
    start: u64,
    buf: Vec<u8>,
    seen: HashMap<Hash, u64>,
    remembered: Vec<(Hash, CacheEntry)>,
    places: Arc<Places>,
}

impl Sink for Txn {
    fn put(&mut self, object: Object, hash: Hash) -> Result<Ptr> {
        if u32::try_from(object.body.len()).is_err() {
            return Err(Error::TooLarge(object.body.len() as u64));
        }
        if let Some(offset) = self.seen.get(&hash) {
            return Ok(Ptr {
                hash,
                offset: *offset,
            });
        }
        if let Some(offset) = self.store.inner.dedupe.get(&hash)
            && self.places.holds(offset)
        {
            return Ok(Ptr { hash, offset });
        }
        if let Some(link) = object.links.iter().find(|link| !self.places.holds(link.offset)) {
            return Err(Error::Pruned(link.offset));
        }
        let offset = self.start + self.buf.len() as u64;
        append_record(&mut self.buf, offset, &object.body, &object.links);
        self.seen.insert(hash, offset);
        Ok(Ptr { hash, offset })
    }

    fn remember(&mut self, ptr: Ptr, value: Arc<dyn Any + Send + Sync>, weight: u32) {
        if self.seen.get(&ptr.hash) == Some(&ptr.offset) {
            let entry = CacheEntry {
                value,
                offset: ptr.offset,
                weight,
            };
            self.remembered.push((ptr.hash, entry));
        }
    }

    fn store_id(&self) -> u64 {
        self.store.inner.id
    }

    fn store(&self) -> Option<Store> {
        Some(self.store.clone())
    }
}

fn stale(found: &Result<(Object, u32)>) -> bool {
    match found {
        Err(Error::Corrupt(_) | Error::Pruned(_)) => true,
        Err(Error::Io(error)) => error.kind() == ErrorKind::UnexpectedEof,
        _ => false,
    }
}

fn extend(file: &Traced, end: u64) -> std::io::Result<()> {
    if file.metadata()?.len() < end {
        file.set_len(end)?;
    }
    Ok(())
}

fn links_of(file: &File, at: u64, end: u64, each: &mut impl FnMut(Ptr)) -> Result<u64> {
    let mut reader = RecordReader {
        file,
        start: at,
        tail: end,
        bytes: Vec::new(),
        pos: 0,
    };
    let (stored_len, link_count, packed) = reader.prefix()?;
    if packed {
        reader.varint()?;
    }
    let body_end = at + reader.pos as u64 + stored_len as u64;
    let mut reader = RecordReader {
        file,
        start: body_end,
        tail: end,
        bytes: Vec::new(),
        pos: 0,
    };
    for _ in 0..link_count {
        let hash: Hash = reader
            .take(HASH_BYTES)?
            .try_into()
            .expect("slice length is HASH_BYTES");
        let delta = unzigzag(reader.varint()?);
        each(Ptr {
            hash,
            offset: at.wrapping_sub(delta as u64),
        });
    }
    Ok(body_end + reader.pos as u64)
}

#[cfg(target_os = "linux")]
fn punch_hole(file: &File, at: u64, len: u64) -> Result<bool> {
    use std::os::fd::AsRawFd;

    let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    let done = unsafe { libc::fallocate(file.as_raw_fd(), mode, at as libc::off_t, len as libc::off_t) };
    if done == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) => Ok(false),
        _ => Err(error.into()),
    }
}

#[cfg(not(target_os = "linux"))]
fn punch_hole(_file: &File, _at: u64, _len: u64) -> Result<bool> {
    Ok(false)
}

fn reserve(inner: &Inner, cursor: &mut Cursor, end: u64) {
    if inner.options.durability != Durability::Fsync || end <= cursor.reserved {
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
    pub(crate) fn write(self, head: Ptr) -> Result<Frame> {
        self.write_frame(head, FRAME_MAGIC, 0)
    }

    pub(crate) fn write_gap(self, head: Ptr, bytes: u64) -> Result<(Frame, u64)> {
        let start = self.start + FRAME_BYTES as u64;
        Ok((self.write_frame(head, GAP_MAGIC, bytes)?, start))
    }

    fn write_frame(mut self, head: Ptr, magic: &[u8; 8], gap: u64) -> Result<Frame> {
        let store = self.store.clone();
        let inner = &store.inner;
        let end = self.start + self.buf.len() as u64 + gap;
        let mut cursor = inner.cursor.lock();
        if inner.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        let frame = Frame {
            seq: cursor.seq + 1,
            end,
            head,
        };
        let header = Super {
            seq: frame.seq,
            log_end: end,
            head: Some(head),
            moved: Moved::default(),
        };
        header.fill(magic, &mut self.buf);
        let mut hasher = Xxh3::new();
        hasher.update(&self.buf[..FRAME_CHECKED]);
        hasher.update(&self.buf[FRAME_BYTES..]);
        self.buf[FRAME_CHECKED..FRAME_BYTES].copy_from_slice(&hasher.digest128().to_le_bytes());
        reserve(inner, &mut cursor, end);
        let extended = if gap > 0 { extend(&inner.file, end) } else { Ok(()) };
        if let Err(error) = extended.and(inner.file.write_all_at(&self.buf, self.start)) {
            inner.poisoned.store(true, Ordering::Release);
            return Err(error.into());
        }
        cursor.seq = frame.seq;
        inner.written.store(end, Ordering::Release);
        drop(cursor);
        for (hash, offset) in self.seen {
            inner.dedupe.insert(hash, offset);
        }
        for (hash, entry) in self.remembered {
            inner.cache.insert(hash, entry);
        }
        Ok(frame)
    }

    pub fn commit(self, head: Ptr) -> Result<()> {
        let store = self.store.clone();
        let frame = self.write(head)?;
        store.sync(frame)
    }
}
