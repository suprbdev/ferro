use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use quick_cache::Weighter;
use quick_cache::sync::Cache;

use crate::codec::{Decoder, HASH_BYTES, Hash, Object, Ptr, Sink, push_varint};
use crate::error::{Error, Result};

const MAGIC: &[u8; 8] = b"FERRODB1";
const SLOT_BYTES: u64 = 4096;
pub(crate) const LOG_START: u64 = 2 * SLOT_BYTES;
const SUPER_BYTES: usize = 96;
const READ_AHEAD: usize = 4096;
const LINK_WEIGHT: usize = HASH_BYTES + 3;
const MAX_VARINT: usize = 10;
const COMPRESS_MIN: usize = 96;
const MAX_RATIO: usize = 255;

static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    Fsync,
    Buffered,
}

#[derive(Clone, Debug)]
pub struct StoreOptions {
    pub durability: Durability,
    pub cache_bytes: u64,
    pub read_only: bool,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            durability: Durability::Fsync,
            cache_bytes: 256 * 1024 * 1024,
            read_only: false,
        }
    }
}

#[derive(Clone)]
struct CacheEntry {
    value: Arc<dyn Any + Send + Sync>,
    weight: u32,
}

#[derive(Clone)]
struct EntryWeight;

impl Weighter<Hash, CacheEntry> for EntryWeight {
    fn weight(&self, _key: &Hash, entry: &CacheEntry) -> u64 {
        u64::from(entry.weight)
    }
}

struct Super {
    seq: u64,
    log_end: u64,
    head: Option<Ptr>,
}

impl Super {
    fn encode(&self) -> [u8; SUPER_BYTES] {
        let mut out = [0u8; SUPER_BYTES];
        out[0..8].copy_from_slice(MAGIC);
        out[8..16].copy_from_slice(&self.seq.to_le_bytes());
        out[16..24].copy_from_slice(&self.log_end.to_le_bytes());
        if let Some(head) = self.head {
            out[24..32].copy_from_slice(&head.offset.to_le_bytes());
            out[32..32 + HASH_BYTES].copy_from_slice(&head.hash);
        }
        let check = blake3::hash(&out[..64]);
        out[64..96].copy_from_slice(check.as_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<Super> {
        if bytes.len() < SUPER_BYTES || &bytes[0..8] != MAGIC {
            return None;
        }
        let check = blake3::hash(&bytes[..64]);
        if check.as_bytes() != &bytes[64..96] {
            return None;
        }
        let seq = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        let log_end = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
        let offset = u64::from_le_bytes(bytes[24..32].try_into().ok()?);
        let hash: Hash = bytes[32..32 + HASH_BYTES].try_into().ok()?;
        let head = (offset != 0).then_some(Ptr { hash, offset });
        Some(Super { seq, log_end, head })
    }
}

fn read_supers(file: &File, len: u64) -> Result<Option<Super>> {
    let mut best: Option<Super> = None;
    for slot in 0..2u64 {
        let start = slot * SLOT_BYTES;
        if len < start + SUPER_BYTES as u64 {
            continue;
        }
        let mut bytes = [0u8; SUPER_BYTES];
        file.read_exact_at(&mut bytes, start)?;
        if let Some(candidate) = Super::decode(&bytes)
            && best.as_ref().is_none_or(|b| candidate.seq > b.seq)
        {
            best = Some(candidate);
        }
    }
    Ok(best)
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
    };
    file.write_all_at(&sb.encode(), SLOT_BYTES)?;
    file.sync_all()?;
    Ok(())
}

fn open_locked(path: &Path, read_only: bool) -> Result<File> {
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
            Err(TryLockError::WouldBlock) => return Err(Error::Locked),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        if std::fs::metadata(path).is_ok_and(|on_disk| on_disk.ino() == file.metadata().map(|m| m.ino()).unwrap_or(0)) {
            return Ok(file);
        }
    }
}

struct Inner {
    id: u64,
    path: PathBuf,
    file: File,
    tail: AtomicU64,
    seq: Mutex<u64>,
    cache: Cache<Hash, CacheEntry, EntryWeight>,
    dedupe: Cache<Hash, u64>,
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
        let sb = match existing {
            Some(sb) => {
                let len = file.metadata()?.len();
                if len < sb.log_end {
                    return Err(Error::Corrupt(format!(
                        "file is {len} bytes but superblock expects {}",
                        sb.log_end
                    )));
                }
                if !options.read_only && len > sb.log_end {
                    file.set_len(sb.log_end)?;
                    file.sync_all()?;
                }
                sb
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
                };
                file.set_len(LOG_START)?;
                file.write_all_at(&sb.encode(), 0)?;
                file.sync_all()?;
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    File::open(parent)?.sync_all()?;
                }
                sb
            }
        };
        let cache_bytes = options.cache_bytes.max(1);
        let items = ((cache_bytes / 2048).max(64)) as usize;
        let head = sb.head;
        let store = Store {
            inner: Arc::new(Inner {
                id: NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed),
                path: path.to_path_buf(),
                file,
                tail: AtomicU64::new(sb.log_end),
                seq: Mutex::new(sb.seq),
                cache: Cache::with_weighter(items, cache_bytes, EntryWeight),
                dedupe: Cache::new(1 << 16),
                poisoned: AtomicBool::new(false),
                closed: AtomicBool::new(false),
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

    pub fn is_read_only(&self) -> bool {
        self.inner.options.read_only
    }

    pub fn refresh(&self) -> Result<Option<Ptr>> {
        let mut seen = self.inner.seq.lock();
        let opened = self.inner.file.metadata()?;
        if std::fs::metadata(&self.inner.path)?.ino() != opened.ino() {
            return Err(Error::Replaced);
        }
        let len = opened.len();
        let sb = read_supers(&self.inner.file, len)?
            .ok_or_else(|| Error::Corrupt("both superblocks are invalid".into()))?;
        if sb.seq < *seen {
            return Ok(None);
        }
        *seen = sb.seq;
        self.inner.tail.fetch_max(sb.log_end, Ordering::AcqRel);
        Ok(sb.head)
    }

    pub fn read_object(&self, ptr: Ptr) -> Result<Object> {
        let tail = self.inner.tail.load(Ordering::Acquire);
        if ptr.offset < LOG_START || ptr.offset >= tail {
            return Err(Error::Corrupt(format!(
                "pointer {} outside log of {tail} bytes",
                ptr.offset
            )));
        }
        let want = (tail - ptr.offset).min(READ_AHEAD as u64) as usize;
        let mut bytes = vec![0u8; want];
        self.inner.file.read_exact_at(&mut bytes, ptr.offset)?;
        let mut reader = RecordReader {
            file: &self.inner.file,
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
        let body = if flagged & 1 == 1 {
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
        Ok(object)
    }

    pub fn load_as<T: Send + Sync + 'static>(
        &self,
        ptr: Ptr,
        decode: impl FnOnce(&mut Decoder<'_>) -> Result<T>,
    ) -> Result<Arc<T>> {
        if let Some(entry) = self.inner.cache.get(&ptr.hash)
            && let Ok(value) = entry.value.downcast::<T>()
        {
            return Ok(value);
        }
        let object = self.read_object(ptr)?;
        let weight = (object.body.len() + object.links.len() * LINK_WEIGHT + 128) as u32;
        let mut decoder = Decoder::new(&object.body, &object.links, Some(self));
        let value = Arc::new(decode(&mut decoder)?);
        if !self.inner.closed.load(Ordering::SeqCst) {
            self.inner.cache.insert(
                ptr.hash,
                CacheEntry {
                    value: value.clone(),
                    weight,
                },
            );
            if self.inner.closed.load(Ordering::SeqCst) {
                self.inner.cache.clear();
            }
        }
        Ok(value)
    }

    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.cache.clear();
    }

    pub fn load<T: crate::codec::Data>(&self, ptr: Ptr) -> Result<Arc<T>> {
        self.load_as(ptr, |decoder| T::decode(decoder))
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
            base: self.inner.tail.load(Ordering::Acquire),
            buf: Vec::new(),
            seen: HashMap::new(),
            remembered: Vec::new(),
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
    base: u64,
    buf: Vec<u8>,
    seen: HashMap<Hash, u64>,
    remembered: Vec<(Hash, CacheEntry)>,
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
        if let Some(offset) = self.store.inner.dedupe.get(&hash) {
            return Ok(Ptr { hash, offset });
        }
        let offset = self.base + self.buf.len() as u64;
        append_record(&mut self.buf, offset, &object.body, &object.links);
        self.seen.insert(hash, offset);
        Ok(Ptr { hash, offset })
    }

    fn remember(&mut self, ptr: Ptr, value: Arc<dyn Any + Send + Sync>, weight: u32) {
        self.remembered.push((ptr.hash, CacheEntry { value, weight }));
    }

    fn store_id(&self) -> u64 {
        self.store.inner.id
    }

    fn store(&self) -> Option<Store> {
        Some(self.store.clone())
    }
}

impl Txn {
    pub fn commit(self, head: Ptr) -> Result<()> {
        let inner = &self.store.inner;
        let end = self.base + self.buf.len() as u64;
        let mut seq = inner.seq.lock();
        let next = *seq + 1;
        let sync = inner.options.durability == Durability::Fsync;
        let attempt = (|| -> Result<()> {
            inner.file.write_all_at(&self.buf, self.base)?;
            if sync {
                inner.file.sync_data()?;
            }
            let sb = Super {
                seq: next,
                log_end: end,
                head: Some(head),
            };
            inner.file.write_all_at(&sb.encode(), (next % 2) * SLOT_BYTES)?;
            if sync {
                inner.file.sync_data()?;
            }
            Ok(())
        })();
        if let Err(error) = attempt {
            inner.poisoned.store(true, Ordering::Release);
            return Err(error);
        }
        *seq = next;
        inner.tail.store(end, Ordering::Release);
        drop(seq);
        for (hash, offset) in self.seen {
            inner.dedupe.insert(hash, offset);
        }
        for (hash, entry) in self.remembered {
            inner.cache.insert(hash, entry);
        }
        Ok(())
    }
}
