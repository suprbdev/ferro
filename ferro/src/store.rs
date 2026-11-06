//! The store: one generation of a database's file, the cache of what was decoded from it, and the
//! transactions that append to it.
//!
//! The file holds two superblock slots of 4 KiB and then the log, a run of frames, one per commit.
//! A frame is a 68-byte header, naming the frame's sequence number, the end of the log after it
//! and its head, then the records the commit wrote, then a table of the xxh3-64 of each 4 KiB block
//! of them. The header's xxh3 check covers the header and the table, so a frame proves by itself
//! that it reached the disk whole. A record holds one content-addressed [`Object`], which a [`Ptr`]
//! finds by its hash and offset, and every read checks the hash. A superblock names the newest
//! frame and an anchor, the newest frame known to be durable, and an open scans forward from the
//! anchor and keeps only frames that follow each other, so a torn tail is cut away. A frame that a
//! single flipped bit in a block keeps from checking reached the disk whole and was damaged since,
//! and fails the open instead.
//!
//! A [`Txn`] gathers a commit's records, skipping an object the store has already written, or read
//! if it is as large as a spilled value, and the commit places its frame at the end of the log,
//! writes it, and syncs it with one fdatasync for every frame queued meanwhile. Reads see every frame already written to the page
//! cache, so a writer reads what it has not yet published. Decoded nodes and values wait in a
//! cache bounded by an estimate of their heap, and readers borrow nodes from it without a count.
//! With [`StoreOptions::shared`], several processes write one file under byte-range locks past its
//! end.
//!
//! [Storage](https://github.com/archlemon/ferro/blob/main/docs/internals/storage.md) describes the
//! format and recovery,
//! [Writers and the commit path](https://github.com/archlemon/ferro/blob/main/docs/internals/commit-path.md)
//! how a frame is placed, written and synced, and
//! [Writers in several processes](https://github.com/archlemon/ferro/blob/main/docs/internals/processes.md)
//! the locks.

use std::any::Any;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::ErrorKind;
use std::mem;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use quick_cache::Weighter;
use quick_cache::sync::Cache;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

use crate::codec::{
    ARC_HEADER, Cached, Decoder, HASH_BYTES, Hash, Moved, Moves, Object, Ptr, Relocate, Sink,
    Staged, hash_parts, push_varint,
};
use crate::dense::{Column, Hashes};
use crate::error::{Error, Result};
use crate::map::SPILL_BYTES;
use crate::platform::{self, FileExt};
use crate::reclaim::sibling;
use crate::shared::{Erased, Shared, Weak};

/// The magic at the start of a superblock, which names the format of the file.
const MAGIC: &[u8; 8] = b"FERRODB4";
/// The magic at the start of a frame's header.
const FRAME_MAGIC: &[u8; 8] = b"FERROFRM";
/// The magic of the marker a reclamation cycle writes at the end of a log it is replacing.
const RETIRED_MAGIC: &[u8; 8] = b"FERROGEN";
/// The size of each of the two superblock slots at the front of the file.
const SLOT_BYTES: u64 = 4096;
/// Where the log starts, past both superblock slots.
pub(crate) const LOG_START: u64 = 2 * SLOT_BYTES;
/// The size of an encoded [`Mark`]: the sequence number, the log's end, and the head's offset and
/// hash.
const MARK_BYTES: usize = 24 + HASH_BYTES;
/// The bytes of a frame's header that its check covers, its magic and its mark, which a
/// superblock starts with too.
const FRAME_CHECKED: usize = 8 + MARK_BYTES;
/// Where a superblock's anchor starts, after its mark and its [`Basis`].
const ANCHOR_AT: usize = FRAME_CHECKED + 16;
/// The bytes of a superblock that its BLAKE3 check covers.
const SUPER_CHECKED: usize = ANCHOR_AT + MARK_BYTES;
/// The size of an encoded superblock, check included.
const SUPER_BYTES: usize = SUPER_CHECKED + 32;
/// The fixed-point one of a [`Basis`]'s survival.
const SURVIVAL_ONE: f64 = (1u64 << 32) as f64;
/// The size of a frame's header, its xxh3-128 check included.
const FRAME_BYTES: usize = FRAME_CHECKED + 16;
/// How many bytes of a frame's records each entry of its table covers.
const BLOCK_BYTES: usize = 4096;
/// The size of an entry of a frame's table, the xxh3-64 of one block of its records.
const BLOCK_CHECK: usize = 8;
/// The size of the sectors a device writes whole or not at all, which a torn write is made of.
const SECTOR_BYTES: u64 = 512;
/// The fewest bytes of a frame that the sector holding a bit found flipped must hold for the flip
/// to count as damage: fewer could be a sector that kept its older bytes and differs from the
/// frame's by chance in one bit.
const SECTOR_MIN: u64 = 8;
/// The most blocks of a frame's records that may fail their entries in its table for open to
/// search them for a flipped bit: more point to a torn write, and each costs a search of every bit
/// of its block.
const MEND_BLOCKS: usize = 8;
/// How many bytes past the room for a frame's header a catch-up reads, where the first record of
/// a frame that spilled before its header was written would start.
const LEFT_PEEK: usize = 16;
/// How much of a frame a check reads at a time, and the size of a record past which a read checks
/// the file's length before it allocates.
const SCAN_CHUNK: usize = 1 << 20;
/// The smallest chunk of zeroes the log grows by under [`Durability::Fsync`], after the first
/// commit of a store, which reserves nothing.
const FIRST_RESERVE: u64 = 64 << 10;
/// The largest chunk of zeroes the log grows by, each chunk doubling the last.
const MAX_RESERVE: u64 = 4 << 20;
/// How many times an open tries the writer lock again, a millisecond apart, before it fails.
const LOCK_RETRIES: u32 = 5;
/// How many bytes a read of a record takes at once, enough for most records whole.
const READ_AHEAD: usize = 1024;
/// What a cache entry weighs for itself, beside the object it holds.
const ENTRY_BYTES: usize = 96;
/// The fewest entries of the deduplication table.
const MIN_DEDUPE: u64 = 1 << 10;
/// The most entries of the deduplication table.
const MAX_DEDUPE: u64 = 1 << 16;
/// The ways of each set of the deduplication table.
const DEDUPE_WAYS: usize = 2;
/// How many sets of the deduplication table are allocated together, when an entry first lands in
/// one of them.
const DEDUPE_CHUNK: usize = 64;
/// The longest varint, which a `u64` takes.
const MAX_VARINT: usize = 10;
/// The shortest body that is LZ4 compressed.
const COMPRESS_MIN: usize = 96;
/// The most LZ4 can expand a body by, which a record's claimed raw length may not pass.
pub(crate) const MAX_RATIO: usize = 255;
/// The smallest window a replay reads from a staging's scratch file.
const REPLAY_CHUNK: usize = 256 << 10;
/// The most capacity a check's scratch body keeps between records.
const SCRATCH_KEEP: usize = 1 << 20;
/// The first byte of the byte-range locks, past any real file.
const LOCKS_AT: u64 = 1 << 62;
/// The byte every shared writer holds shared from the end of its open for the rest of its life,
/// which tells a read-only open, and a writer that opens, that one has opened and is alive.
const MULTI_AT: u64 = LOCKS_AT + 1;
/// The placement lock, held exclusive to catch up, place a frame and write it.
const COMMIT_AT: u64 = LOCKS_AT + 2;
/// The superblock lock, held exclusive to sync and write a superblock.
const SUPER_AT: u64 = LOCKS_AT + 3;
/// The cycle lock, held exclusive for a reclamation cycle and the pieces of a large receive.
const CYCLE_AT: u64 = LOCKS_AT + 4;
/// The first byte of the branch locks, each a byte at a 60-bit hash of the branch's name.
const LANES_AT: u64 = LOCKS_AT + (1 << 20);
/// How many bytes the branch locks span.
const LANES: u64 = 1 << 60;

/// The id of the next store this process opens.
static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(1);

/// How a commit reaches the disk.
///
/// [`Options::durability`](crate::Options::durability) sets it, and
/// [Durability and crash safety](https://github.com/archlemon/ferro/blob/main/docs/guide/durability.md)
/// describes what each promises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// Every commit is synced before it is acknowledged, one fdatasync covering every commit
    /// queued meanwhile, so an acknowledged commit survives a power cut. The default.
    Fsync,
    /// Commits are written to the page cache and synced only at checkpoints, every
    /// [`Options::checkpoint_bytes`](crate::Options::checkpoint_bytes) of log, and at a clean
    /// close. A power cut can lose the commits since the last checkpoint, and the file still
    /// opens.
    Buffered,
}

/// A file operation recorded in a [`Trace`], for tests that replay a crash.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// A file was created; it holds the file's inode.
    Create(u64),
    /// Bytes are about to be written to a file at an offset.
    Write(u64, u64, Vec<u8>),
    /// A file's length is about to be set.
    SetLen(u64, u64),
    /// A file was synced.
    Sync(u64),
    /// A file was renamed over the database's path.
    Rename(u64),
    /// The directory holding the database was synced.
    SyncDir,
    /// A test's marker, such as a commit acknowledged at a version.
    Acked(u64),
}

/// The operations a store's file records, shared with the test that reads them.
#[doc(hidden)]
pub type Trace = Arc<Mutex<Vec<Op>>>;

/// How a [`Store`] opens its file.
///
/// [`Options`](crate::Options) builds one for a database, and its methods of the same names say
/// more about each field.
#[derive(Clone, Debug)]
pub struct StoreOptions {
    /// How each commit reaches the disk, [`Durability::Fsync`] by default.
    pub durability: Durability,
    /// An approximate bound, in bytes, on the decoded nodes and values the cache holds, 1 GiB by
    /// default. It also sizes the deduplication table, at one entry for every 4 KiB.
    pub cache_bytes: u64,
    /// Opens the file without the writer lock, to read while another process writes.
    pub read_only: bool,
    /// Under [`Durability::Buffered`], how many bytes of log a checkpoint covers, 64 MiB by
    /// default.
    pub checkpoint_bytes: u64,
    /// Lets several processes write the file, if every one of them opens it with this, through
    /// byte-range locks that only 64-bit Linux has.
    pub shared: bool,
    /// How many threads a reclamation cycle copies and checks on, counting its own. Zero, the
    /// default, means the number of cores up to 4.
    pub copy_threads: usize,
    /// Records every write, sync and change of length to the file, for crash tests.
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
            copy_threads: 0,
            trace: None,
        }
    }
}

/// The weight of a cache entry: what the entry weighs for itself, `fixed_bytes` for the decoded
/// object's fixed parts and `body_bytes` for what its body decoded into, saturating at
/// `u32::MAX`.
pub(crate) fn heap_weight(fixed_bytes: usize, body_bytes: usize) -> u32 {
    let weight = ENTRY_BYTES + fixed_bytes + body_bytes;
    u32::try_from(weight).unwrap_or(u32::MAX)
}

/// An entry of the store's cache, keyed by the hash of the object it holds.
///
/// The cache's own copy of a node's entry is marked `owned`. It set the node's `cached` flag when
/// it took the node, and when it is dropped, by eviction, replacement or a clear, it clears the
/// flag and retires its count to the epoch if a reader borrowed the node, or lets it go otherwise.
/// The clones the cache hands out on a hit are not owners, and give their value up with
/// [`CacheEntry::take`].
struct CacheEntry(Entry);

/// What a cache entry holds.
enum Entry {
    /// A decoded object that is not a tree node, such as a root, a log entry or a spilled value.
    Value { value: Arc<dyn Cached>, weight: u32 },
    /// A tree node, `owned` in the cache's own copy, which holds the node's `cached` flag.
    Node {
        node: Erased,
        weight: u32,
        owned: bool,
    },
    /// An entry whose object was taken out, keeping its weight.
    Taken { weight: u32 },
}

impl CacheEntry {
    /// An entry for a decoded object that is not a node.
    fn value(value: Arc<dyn Cached>, weight: u32) -> CacheEntry {
        CacheEntry(Entry::Value { value, weight })
    }

    /// The cache's own entry for `node`, which sets the node's `cached` flag.
    fn node(node: Erased, weight: u32) -> CacheEntry {
        node.hold();
        CacheEntry(Entry::Node {
            node,
            weight,
            owned: true,
        })
    }

    /// What the entry weighs against the cache's bound.
    fn weight(&self) -> u32 {
        match self.0 {
            Entry::Value { weight, .. } | Entry::Node { weight, .. } | Entry::Taken { weight } => {
                weight
            }
        }
    }

    /// Takes the entry's object out, leaving its weight, so a hit hands out the count its clone
    /// took rather than taking another.
    fn take(&mut self) -> Entry {
        let weight = self.weight();
        mem::replace(&mut self.0, Entry::Taken { weight })
    }
}

impl Clone for CacheEntry {
    /// A copy with a count of its own, which is never the owner.
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
    /// The cache's own entry clears its node's `cached` flag and retires its count if a reader
    /// borrowed the node; any other entry only lets its count go.
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

/// A decoded value with the offset it was decoded from, for a type whose decoded form depends on
/// where its object sits, so that a hit at another offset decodes the object again.
struct Located<T> {
    offset: u64,
    value: Arc<T>,
}

impl<T: Send + Sync + 'static> Cached for Located<T> {}

/// What weighs a cache entry against the cache's bound.
#[derive(Clone)]
struct EntryWeight;

impl Weighter<Hash, CacheEntry> for EntryWeight {
    fn weight(&self, _key: &Hash, entry: &CacheEntry) -> u64 {
        u64::from(entry.weight())
    }
}

/// What a superblock records of how its generation began: the file's size when the cycle that
/// made it sealed it, and the share of the previous generation's bytes that survived that cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Basis {
    /// The file's size when the generation began, or zero for a file no cycle made.
    pub(crate) bytes: u64,
    /// The surviving share, as a fraction of 2^32.
    survival: u64,
}

impl Basis {
    /// A basis of `bytes`, with `survival` clamped between 0 and 1.
    pub(crate) fn new(bytes: u64, survival: f64) -> Basis {
        Basis {
            bytes,
            survival: (survival.clamp(0.0, 1.0) * SURVIVAL_ONE) as u64,
        }
    }

    /// The surviving share, between 0 and 1.
    pub(crate) fn survival(&self) -> f64 {
        self.survival as f64 / SURVIVAL_ONE
    }
}

/// A position in the log: a frame's sequence number, the log's end after it, and the head it
/// names, if any.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Mark {
    seq: u64,
    log_end: u64,
    head: Option<Ptr>,
}

impl Mark {
    /// Writes the mark's [`MARK_BYTES`] to the front of `out`, which must be zeroed: a missing head
    /// is left as zeroes, which read back as no head.
    fn fill(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.seq.to_le_bytes());
        out[8..16].copy_from_slice(&self.log_end.to_le_bytes());
        if let Some(head) = self.head {
            out[16..24].copy_from_slice(&head.offset.to_le_bytes());
            out[24..MARK_BYTES].copy_from_slice(&head.hash);
        }
    }

    /// Reads a mark from the front of `bytes`, where a head offset of zero means no head.
    fn parse(bytes: &[u8]) -> Option<Mark> {
        let seq = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let log_end = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        let offset = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
        let hash: Hash = bytes[24..MARK_BYTES].try_into().ok()?;
        let head = (offset != 0).then_some(Ptr { hash, offset });
        Some(Mark { seq, log_end, head })
    }
}

/// A superblock: the frame it names, the basis of its generation, and its anchor, the newest
/// frame known to be durable when it was written.
///
/// A frame's header holds the same mark, so a scan reads each frame as the superblock that would
/// name it.
#[derive(Clone, Copy)]
struct Super {
    seq: u64,
    log_end: u64,
    head: Option<Ptr>,
    basis: Basis,
    anchor: Mark,
}

impl Super {
    /// A superblock naming `mark`, anchored at `anchor`.
    fn new(mark: Mark, anchor: Mark, basis: Basis) -> Super {
        Super {
            seq: mark.seq,
            log_end: mark.log_end,
            head: mark.head,
            basis,
            anchor,
        }
    }

    /// The frame the superblock names.
    fn mark(&self) -> Mark {
        Mark {
            seq: self.seq,
            log_end: self.log_end,
            head: self.head,
        }
    }

    /// Whether the superblock anchors itself, so the frame it names is durable.
    fn settled(&self) -> bool {
        self.anchor == self.mark()
    }

    /// The superblock as of its anchor, which a scan starts from.
    fn at_anchor(&self) -> Super {
        Super::new(self.anchor, self.anchor, self.basis)
    }

    /// Writes `magic` and the mark to the front of `out`, the fields a frame's header shares with a
    /// superblock.
    fn fill(&self, magic: &[u8; 8], out: &mut [u8]) {
        out[0..8].copy_from_slice(magic);
        self.mark().fill(&mut out[8..]);
    }

    /// Reads the mark after a magic, as a superblock anchored at itself with no basis, as a frame's
    /// header holds it. The magic is not checked.
    fn fields(bytes: &[u8]) -> Option<Super> {
        let mark = Mark::parse(&bytes[8..])?;
        Some(Super::new(mark, mark, Basis::default()))
    }

    /// The superblock's bytes for a slot, ending in a BLAKE3 check of the rest.
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

    /// Reads a superblock, or `None` if the bytes are too short or the magic or the check is wrong.
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

/// Reads both superblock slots of a file `len` bytes long, and returns the valid superblock with
/// the highest sequence number, then anchor, and the slot it is in.
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

/// Fills `out` from the file at `at`, returning `false` if the file ends first.
fn read_or_end(file: &File, out: &mut [u8], at: u64) -> Result<bool> {
    match file.read_exact_at(out, at) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Fills `out` from the file at `at` as far as the file reaches, returning how many bytes it read.
fn read_some(file: &File, out: &mut [u8], at: u64) -> Result<usize> {
    let mut got = 0;
    while got < out.len() {
        match file.read_at(&mut out[got..], at + got as u64) {
            Ok(0) => break,
            Ok(read) => got += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(got)
}

/// [`read_supers`] in one read of both slots, for a file that holds its header.
fn newest_super(file: &File) -> Result<Option<(Super, u64)>> {
    let mut bytes = vec![0u8; SLOT_BYTES as usize + SUPER_BYTES];
    file.read_exact_at(&mut bytes, 0)?;
    let mut best: Option<(Super, u64)> = None;
    for slot in 0..2u64 {
        let at = (slot * SLOT_BYTES) as usize;
        if let Some(candidate) = Super::decode(&bytes[at..at + SUPER_BYTES])
            && best
                .as_ref()
                .is_none_or(|b| (candidate.seq, candidate.anchor.seq) > (b.0.seq, b.0.anchor.seq))
        {
            best = Some((candidate, slot));
        }
    }
    Ok(best)
}

/// What a scan finds where a frame would follow the one before.
enum Found {
    /// A whole frame that follows the one before.
    Whole(Super),
    /// No frame, or one that did not reach the disk whole, which ends the log.
    Torn,
    /// A frame that reached the disk whole and was damaged since, with the offset of the first
    /// byte found wrong.
    Damaged(u64),
}

/// The size of the table of a frame that holds `records` bytes of records: one entry for each
/// block of them, the last block shorter.
fn table_len(records: u64) -> u64 {
    records.div_ceil(BLOCK_BYTES as u64) * BLOCK_CHECK as u64
}

/// How many bytes of records a frame holds whose records and table together are `body` bytes
/// long, or `None` if no frame's are.
fn records_in(body: u64) -> Option<u64> {
    let blocks = body.div_ceil((BLOCK_BYTES + BLOCK_CHECK) as u64);
    let records = body.checked_sub(blocks * BLOCK_CHECK as u64)?;
    (table_len(records) == body - records).then_some(records)
}

/// Appends to `buf` the table of the records in `buf` from `from` on.
fn append_table(buf: &mut Vec<u8>, from: usize) {
    let end = buf.len();
    buf.reserve(table_len((end - from) as u64) as usize);
    let mut at = from;
    while at < end {
        let next = (at + BLOCK_BYTES).min(end);
        let check = xxh3_64(&buf[at..next]);
        buf.extend_from_slice(&check.to_le_bytes());
        at = next;
    }
}

/// The table of records fed to it in pieces of any size, as a frame that spilled reads its
/// records back.
struct Blocks {
    hasher: Xxh3,
    filled: usize,
    table: Vec<u8>,
}

impl Blocks {
    /// An empty table.
    fn new() -> Blocks {
        Blocks {
            hasher: Xxh3::new(),
            filled: 0,
            table: Vec::new(),
        }
    }

    /// Feeds the next `bytes` of records.
    fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let take = (BLOCK_BYTES - self.filled).min(bytes.len());
            self.hasher.update(&bytes[..take]);
            self.filled += take;
            bytes = &bytes[take..];
            if self.filled == BLOCK_BYTES {
                self.close();
            }
        }
    }

    /// Ends the block being fed with an entry for it.
    fn close(&mut self) {
        self.table
            .extend_from_slice(&self.hasher.digest().to_le_bytes());
        self.hasher.reset();
        self.filled = 0;
    }

    /// The table, with an entry for the last block if it is short.
    fn finish(mut self) -> Vec<u8> {
        if self.filled > 0 {
            self.close();
        }
        self.table
    }
}

/// The check of a frame whose header is `header` and whose table is `table`: the xxh3-128 of the
/// header's magic and mark and of the table, which holds a check of every block of records.
fn frame_check(header: &[u8], table: &[u8]) -> [u8; 16] {
    let mut hasher = Xxh3::new();
    hasher.update(&header[..FRAME_CHECKED]);
    hasher.update(table);
    hasher.digest128().to_le_bytes()
}

/// The frame `header` names if it has a frame's magic and could follow `tip` in a file `len`
/// bytes long, with the bytes of records it holds: its sequence number is the next after `tip`'s,
/// and its end lies within `len` at a length a frame can have.
fn shape(tip: &Super, header: &[u8; FRAME_BYTES], len: u64) -> Option<(Super, u64)> {
    if &header[0..8] != FRAME_MAGIC {
        return None;
    }
    let body_start = tip.log_end.checked_add(FRAME_BYTES as u64)?;
    let mut frame = Super::fields(header)?;
    frame.basis = tip.basis;
    let follows = tip.seq.checked_add(1) == Some(frame.seq);
    if !follows || frame.log_end < body_start || frame.log_end > len {
        return None;
    }
    let records = records_in(frame.log_end - body_start)?;
    Some((frame, records))
}

/// Reads the table that a frame whose records start at `at` and run for `records` bytes holds,
/// or `None` if the file ends first.
fn read_table(file: &File, at: u64, records: u64) -> Result<Option<Vec<u8>>> {
    let mut table = vec![0u8; table_len(records) as usize];
    Ok(read_or_end(file, &mut table, at + records)?.then_some(table))
}

/// Reads the body of a frame whose records start at `at` and run for `records` bytes, and returns
/// the table it holds and the table its records hash to, or `None` if the file ends first.
fn read_tables(file: &File, at: u64, records: u64) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let tables = table_len(records);
    let end = at + records + tables;
    let mut stored = Vec::with_capacity(tables as usize);
    let mut computed = Vec::with_capacity(tables as usize);
    let mut chunk = vec![0u8; SCAN_CHUNK.min((end - at) as usize)];
    let mut pos = at;
    while pos < end {
        let take = chunk.len().min((end - pos) as usize);
        if !read_or_end(file, &mut chunk[..take], pos)? {
            return Ok(None);
        }
        let split = (at + records).saturating_sub(pos).min(take as u64) as usize;
        for block in chunk[..split].chunks(BLOCK_BYTES) {
            computed.extend_from_slice(&xxh3_64(block).to_le_bytes());
        }
        stored.extend_from_slice(&chunk[split..take]);
        pos += take as u64;
    }
    Ok(Some((stored, computed)))
}

/// What follows `tip` in a file `len` bytes long, as [`examine`] judges the header there.
fn read_frame(file: &File, tip: &Super, len: u64, judge: bool) -> Result<Found> {
    let start = tip.log_end;
    if start.saturating_add(FRAME_BYTES as u64) > len {
        return Ok(Found::Torn);
    }
    let mut header = [0u8; FRAME_BYTES];
    if !read_or_end(file, &mut header, start)? {
        return Ok(Found::Torn);
    }
    examine(file, tip, &header, len, judge)
}

/// Whether `header` starts with a frame's magic, or with one that differs from it in one bit.
fn frame_like(header: &[u8]) -> bool {
    let magic = u64::from_le_bytes(header[0..8].try_into().unwrap_or_default());
    (magic ^ u64::from_le_bytes(*FRAME_MAGIC)).count_ones() <= 1
}

/// What the frame whose header, read from where `tip`'s log ends, is `header` turns out to be, in
/// a file `len` bytes long.
///
/// A frame is whole if it has the shape [`shape`] asks for, every block of its records matches
/// its entry in the table, and its check matches its header and table. Without `judge`, anything
/// else is [`Found::Torn`]. With it, a frame that is not whole goes to [`diagnose`], which tells
/// one damaged since it reached the disk from one that never reached it whole. Only a reader that
/// no write can race may judge, since one racing a write can read a frame in part.
fn examine(
    file: &File,
    tip: &Super,
    header: &[u8; FRAME_BYTES],
    len: u64,
    judge: bool,
) -> Result<Found> {
    let mut read = None;
    if let Some((frame, records)) = shape(tip, header, len) {
        let body_start = tip.log_end + FRAME_BYTES as u64;
        if let Some((stored, computed)) = read_tables(file, body_start, records)? {
            if stored == computed && frame_check(header, &stored) == header[FRAME_CHECKED..] {
                return Ok(Found::Whole(frame));
            }
            read = Some((stored, computed));
        }
    }
    if !judge || !frame_like(header) {
        return Ok(Found::Torn);
    }
    diagnose(file, tip, header, len, read)
}

/// The verdict on a frame header with a frame's magic, or one a bit from it, that does not start a
/// whole frame, given the tables [`examine`] read for it as it stands, if it could.
///
/// A frame reached the disk whole and was damaged since if putting back at most one flipped bit
/// in its header and table together, and at most one in each of at most [`MEND_BLOCKS`] blocks of
/// its records, makes it whole, and each such bit passes [`judged`]. A torn write leaves the part
/// of a sector it did not reach as it was: zeroes or ones, which [`judged`] refuses, or other
/// bytes, which differ from what the frame would have put there in many bits, so it leaves a frame
/// that no single flip mends. Anything else is [`Found::Torn`].
fn diagnose(
    file: &File,
    tip: &Super,
    header: &[u8; FRAME_BYTES],
    len: u64,
    read: Option<(Vec<u8>, Vec<u8>)>,
) -> Result<Found> {
    let start = tip.log_end;
    let body_start = start + FRAME_BYTES as u64;
    let shaped = shape(tip, header, len);
    if let (Some((frame, records)), Some((stored, computed))) = (shaped, &read) {
        let check = frame_check(header, stored);
        let differ = check
            .iter()
            .zip(&header[FRAME_CHECKED..])
            .map(|(a, b)| a ^ b);
        match differ.map(u8::count_ones).sum::<u32>() {
            0 => return mend(file, start, frame, records, stored, computed, Vec::new()),
            1 => {
                let byte = check
                    .iter()
                    .zip(&header[FRAME_CHECKED..])
                    .position(|(a, b)| a != b);
                let flipped = vec![start + (FRAME_CHECKED + byte.unwrap_or_default()) as u64];
                return mend(file, start, frame, records, stored, computed, flipped);
            }
            _ => {}
        }
        let mut near = stored
            .chunks(BLOCK_CHECK)
            .zip(computed.chunks(BLOCK_CHECK))
            .enumerate()
            .filter(|(_, (want, got))| {
                let differ = want
                    .iter()
                    .zip(got.iter())
                    .map(|(a, b)| (a ^ b).count_ones());
                differ.sum::<u32>() == 1
            });
        if let (Some((index, (want, got))), None) = (near.next(), near.next()) {
            let byte = want.iter().zip(got.iter()).position(|(a, b)| a != b);
            let at = index * BLOCK_CHECK + byte.unwrap_or_default();
            let mut table = stored.clone();
            table[at] = computed[at];
            if frame_check(header, &table) == header[FRAME_CHECKED..] {
                let flipped = vec![body_start + records + at as u64];
                return mend(file, start, frame, records, &table, computed, flipped);
            }
        }
    }
    for bit in 0..FRAME_CHECKED * 8 {
        let mut candidate = *header;
        candidate[bit / 8] ^= 1 << (bit % 8);
        let Some((frame, records)) = shape(tip, &candidate, len) else {
            continue;
        };
        let known = match (&read, shaped) {
            (Some(tables), Some((was, _))) if was.log_end == frame.log_end => Some(tables),
            _ => None,
        };
        let fetched;
        let stored = match known {
            Some((stored, _)) => stored,
            None => match read_table(file, body_start, records)? {
                Some(table) => {
                    fetched = table;
                    &fetched
                }
                None => continue,
            },
        };
        if frame_check(&candidate, stored) != candidate[FRAME_CHECKED..] {
            continue;
        }
        let computed = match known {
            Some((_, computed)) => computed.clone(),
            None => match read_tables(file, body_start, records)? {
                Some((_, computed)) => computed,
                None => return Ok(Found::Torn),
            },
        };
        let flipped = vec![start + (bit / 8) as u64];
        return mend(file, start, frame, records, stored, &computed, flipped);
    }
    Ok(Found::Torn)
}

/// Finishes [`diagnose`] for the frame from `start` with `records` bytes of records, whose header
/// and `table` check once the bits at `flipped` are put back: at most [`MEND_BLOCKS`] blocks may
/// have a check, in `computed`, that differs from their entry, each must match it once one of its
/// bits is flipped back, and every bit flipped back must pass [`judged`].
fn mend(
    file: &File,
    start: u64,
    frame: Super,
    records: u64,
    table: &[u8],
    computed: &[u8],
    mut flipped: Vec<u64>,
) -> Result<Found> {
    let body_start = start + FRAME_BYTES as u64;
    let end = frame.log_end;
    let entries = table.chunks(BLOCK_CHECK).zip(computed.chunks(BLOCK_CHECK));
    let failing: Vec<(usize, &[u8])> = entries
        .enumerate()
        .filter(|(_, (want, got))| want != got)
        .map(|(index, (want, _))| (index, want))
        .collect();
    if failing.len() > MEND_BLOCKS {
        return Ok(Found::Torn);
    }
    for at in &flipped {
        if !judged(file, *at, start, end)? {
            return Ok(Found::Torn);
        }
    }
    for (index, want) in failing {
        let from = (index * BLOCK_BYTES) as u64;
        let mut block = vec![0u8; (records - from).min(BLOCK_BYTES as u64) as usize];
        if !read_or_end(file, &mut block, body_start + from)? {
            return Ok(Found::Torn);
        }
        let want = u64::from_le_bytes(want.try_into().unwrap_or_default());
        let Some(byte) = flip_back(&mut block, want) else {
            return Ok(Found::Torn);
        };
        let at = body_start + from + byte as u64;
        if !judged(file, at, start, end)? {
            return Ok(Found::Torn);
        }
        flipped.push(at);
    }
    Ok(match flipped.into_iter().min() {
        Some(at) => Found::Damaged(at),
        None => Found::Whole(frame),
    })
}

/// Whether a bit found flipped at `at`, in the frame from `start` to `end`, counts as damage
/// rather than as a sector that a write did not reach.
///
/// The sector holding it must hold at least [`SECTOR_MIN`] of the frame's bytes, and on disk the
/// frame's bytes in that sector up to the flipped one, and those from it on, must neither be all
/// zeroes nor all ones. A write that misses part of a sector leaves it as it was, and within a
/// sector a frame's writes land in order of offset but for its header, which is written last, so
/// the part a lost write leaves runs from where that write began in the sector to the sector's
/// end, or for a header, from the sector's start to the header's end. What it leaves there is the
/// zeroes of a reservation, of a new extension or of the room past an earlier write, or the ones
/// of a block a device erased, which differ in a single bit from a frame that held little there.
fn judged(file: &File, at: u64, start: u64, end: u64) -> Result<bool> {
    let sector = at - at % SECTOR_BYTES;
    let from = sector.max(start);
    let to = (sector + SECTOR_BYTES).min(end);
    if to - from < SECTOR_MIN {
        return Ok(false);
    }
    let mut bytes = [0u8; SECTOR_BYTES as usize];
    let bytes = &mut bytes[..(to - from) as usize];
    if !read_or_end(file, bytes, from)? {
        return Ok(false);
    }
    let split = (at - from) as usize;
    Ok(!blank(&bytes[..=split]) && !blank(&bytes[split..]))
}

/// Whether `bytes` are all zeroes or all ones, as the part of a sector that a write did not reach
/// can be.
fn blank(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| *byte == 0) || bytes.iter().all(|byte| *byte == 0xff)
}

/// The byte of `block` in which flipping one bit makes its xxh3-64 `want`, if there is one. The
/// block is left as it was.
fn flip_back(block: &mut [u8], want: u64) -> Option<usize> {
    for byte in 0..block.len() {
        for bit in 0..8 {
            block[byte] ^= 1 << bit;
            let mended = xxh3_64(block) == want;
            block[byte] ^= 1 << bit;
            if mended {
                return Some(byte);
            }
        }
    }
    None
}

/// The error for the frame after `tip`, which reached the disk whole and is damaged at `at`.
fn damaged(tip: &Super, at: u64) -> Error {
    Error::Corrupt(format!(
        "frame {} at {} reached the disk whole and is damaged at byte {at}",
        tip.seq.saturating_add(1),
        tip.log_end
    ))
}

/// Fails with [`Error::Corrupt`] if `tip`, where a scan or a catch-up stopped, lies before the end
/// of the log that `named`, the newest superblock, names, so that a frame it names does not check.
fn reaches(tip: &Super, named: &Super) -> Result<()> {
    if tip.seq >= named.seq && tip.log_end >= named.log_end {
        return Ok(());
    }
    Err(Error::Corrupt(format!(
        "frame {} at {} does not check, inside the log the newest superblock names to {}",
        tip.seq.saturating_add(1),
        tip.log_end,
        named.log_end
    )))
}

/// Follows frames from `sb` while each follows the one before and is whole, and returns the last.
///
/// With `judge`, a frame that reached the disk whole and was damaged since fails the scan with
/// [`Error::Corrupt`], naming its offset, where it would otherwise end the log. Only a scan that
/// no write can race may judge, as [`examine`] says.
fn scan(file: &File, sb: Super, len: u64, judge: bool) -> Result<Super> {
    let mut tip = sb;
    loop {
        match read_frame(file, &tip, len, judge)? {
            Found::Whole(next) => tip = next,
            Found::Torn => return Ok(tip),
            Found::Damaged(at) => return Err(damaged(&tip, at)),
        }
    }
}

/// Recovers a writer's file at open from `sb`, the newest superblock, found in `slot`: scans from
/// its anchor and makes the frames it keeps the file, returning the superblock that names the last
/// of them and its slot.
///
/// If the log moved or the superblock did not anchor itself, it syncs the frames it kept and
/// writes a superblock anchoring the last of them to the other slot. It truncates anything past
/// that frame, durably, before any new frame is written, so a frame stranded past a torn one can
/// never line up with the log later. If the scan ended before the frame the superblock named, it
/// writes the new superblock to both slots, syncing between them, so no slot names a lost frame.
/// A frame that reached the disk whole and was damaged since fails it with [`Error::Corrupt`]
/// before it writes anything.
///
/// With `live` set, another shared writer that has opened is alive, so no crash can have taken
/// back a frame `sb` names since that writer recovered the file, and the scan ending before the
/// end of the log `sb` names fails it with [`Error::Corrupt`] too, before it writes anything.
/// Without it, only frames past the anchor can be truncated, which under
/// [`Durability::Buffered`] a crash may have lost.
fn recover(file: &File, sb: Super, slot: u64, len: u64, live: bool) -> Result<(Super, u64)> {
    let mut tip = scan(file, sb.at_anchor(), len, true)?;
    if live {
        reaches(&tip, &sb)?;
    }
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

/// The superblock a read-only open trusts: `sb`, unless no writer is alive and a scan from its
/// anchor finds otherwise.
///
/// A superblock that anchors itself with nothing like a frame after it is used as it is.
/// Otherwise the open tries the writer lock shared, and where there are byte-range locks the
/// placement lock shared too, and if it gets them and no shared writer holds [`MULTI_AT`], no
/// writer is alive: it scans from the anchor under the locks, judging damage as a writer's open
/// does, and syncs the frames it kept, since a dead writer's frames may be only in the page cache.
/// A shared writer that opens meanwhile waits for the placement lock before it recovers or writes,
/// so nothing it writes races the scan. With a writer alive, `sb` is current. A placement lock
/// held elsewhere means a writer is placing a frame, so `sb` is used as it is, but a test of the
/// lock that fails with an error says nothing about writers, and the open goes on to test
/// [`MULTI_AT`] and scans without the placement lock, as it would where there are no byte-range
/// locks.
fn observe(file: &File, path: &Path, sb: Super, len: u64) -> Result<Super> {
    let mut header = [0u8; FRAME_BYTES];
    let follows = sb.log_end.saturating_add(FRAME_BYTES as u64) <= len
        && read_or_end(file, &mut header, sb.log_end)?
        && frame_like(&header);
    if (sb.settled() && !follows) || platform::try_lock_shared(file).is_err() {
        return Ok(sb);
    }
    let placement = match platform::RANGES {
        true => match platform::lock_range(file, COMMIT_AT, 1, false, false) {
            Ok(()) => Some(Range {
                file,
                at: COMMIT_AT,
            }),
            Err(TryLockError::WouldBlock) => {
                platform::unlock(file)?;
                return Ok(sb);
            }
            Err(TryLockError::Error(_)) => None,
        },
        false => None,
    };
    if platform::range_held(file, MULTI_AT, 1).unwrap_or(false) {
        drop(placement);
        platform::unlock(file)?;
        return Ok(sb);
    }
    let before = sb.anchor.seq;
    let scanned = file
        .metadata()
        .map_err(Error::from)
        .and_then(|meta| scan(file, sb.at_anchor(), meta.len(), true))
        .and_then(|tip| {
            if tip.seq != before {
                platform::sync_reader(file, path)?;
            }
            Ok(tip)
        });
    drop(placement);
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

/// A record read back: its object, its length in the log, and its compressed body if it has one
/// and the reader asked to keep it.
type Raw = (Object, u32, Option<Vec<u8>>);

/// The LZ4 block of `body`, if the body is long enough to try and the block makes its record
/// shorter.
pub(crate) fn compressed(body: &[u8]) -> Option<Vec<u8>> {
    if body.len() < COMPRESS_MIN {
        return None;
    }
    let packed = lz4_flex::block::compress(body);
    (packed.len() + varint_len(body.len() as u64) < body.len()).then_some(packed)
}

/// Appends to `buf` the record of an object with `body` and `links`, as it would sit at `offset`
/// in the log.
///
/// The body is LZ4 compressed when that makes the record shorter, and each link is stored as its
/// hash and the distance back from `offset` to the link's own offset.
pub fn append_record(buf: &mut Vec<u8>, offset: u64, body: &[u8], links: &[Ptr]) {
    let packed = compressed(body);
    append_staged(buf, offset, staged(body, packed.as_deref(), links));
}

/// The parts of a record to append: `body`, its compressed form if it has one, and `links`.
pub(crate) fn staged<'a>(body: &'a [u8], packed: Option<&'a [u8]>, links: &'a [Ptr]) -> Staged<'a> {
    Staged {
        raw: body.len(),
        payload: packed.unwrap_or(body),
        packed: packed.is_some(),
        links,
    }
}

/// Appends to `buf` the record of `staged`, as it would sit at `offset` in the log.
pub(crate) fn append_staged(buf: &mut Vec<u8>, offset: u64, staged: Staged<'_>) {
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

/// The length in bytes of the record that `object` would take at `offset` in the log.
pub fn record_len(offset: u64, object: &Object) -> usize {
    let mut buf = Vec::new();
    append_record(&mut buf, offset, &object.body, &object.links);
    buf.len()
}

/// A reader of one record, from the bytes held of it, reading more from the file as it needs, at
/// least [`READ_AHEAD`] at a time, and never past `tail`.
struct RecordReader<'a> {
    /// The file to read more from, or `None` when `bytes` must hold the whole record.
    file: Option<&'a File>,
    /// The record's offset in the log.
    start: u64,
    /// The end of what may be read.
    tail: u64,
    /// The record's bytes read so far.
    bytes: Cow<'a, [u8]>,
    /// Where the next read starts in `bytes`.
    pos: usize,
}

impl RecordReader<'_> {
    /// Makes sure `bytes` holds the record up to `upto`, failing with `Error::Corrupt` if that runs
    /// past `tail` or, for a read of more than [`SCAN_CHUNK`], past the end of the file.
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

    /// Reads a varint, failing with `Error::Corrupt` on one that does not fit a `u64`.
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

    /// Reads the next `n` bytes.
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

    /// Reads a record's header, failing with `Error::Corrupt` on a compressed body that claims
    /// more than LZ4 can expand it to.
    fn header(&mut self) -> Result<Header> {
        let stored = usize::try_from(self.varint()?)
            .map_err(|_| Error::Corrupt("body length overflow".into()))?;
        let flagged = self.varint()?;
        let links = usize::try_from(flagged >> 1)
            .map_err(|_| Error::Corrupt("link count overflow".into()))?;
        if flagged & 1 == 0 {
            return Ok(Header {
                stored,
                links,
                raw: None,
            });
        }
        let raw = usize::try_from(self.varint()?)
            .map_err(|_| Error::Corrupt("body length overflow".into()))?;
        if raw > stored.saturating_mul(MAX_RATIO).saturating_add(MAX_RATIO) {
            return Err(Error::Corrupt(format!(
                "implausible compression at {}",
                self.start
            )));
        }
        Ok(Header {
            stored,
            links,
            raw: Some(raw),
        })
    }

    /// Reads one link: its hash and the distance back from the record to its target.
    fn link(&mut self) -> Result<Ptr> {
        let hash: Hash = self
            .take(HASH_BYTES)?
            .try_into()
            .expect("slice length is HASH_BYTES");
        let delta = unzigzag(self.varint()?);
        Ok(Ptr {
            hash,
            offset: self.start.wrapping_sub(delta as u64),
        })
    }
}

/// A record's header: the stored body's length, the number of links, and the body's length once
/// uncompressed if it is compressed.
struct Header {
    stored: usize,
    links: usize,
    raw: Option<usize>,
}

/// Where the parts of a record sit in its bytes.
pub(crate) struct Stored {
    /// The body's length once uncompressed.
    pub(crate) raw: usize,
    /// Whether the body is LZ4 compressed.
    pub(crate) packed: bool,
    /// Where the stored body sits in the record's bytes.
    pub(crate) payload: std::ops::Range<usize>,
    /// The record's length, links included.
    pub(crate) len: usize,
}

/// Decompresses `payload`, a body stored at `at`, into `out`, which it sizes to `raw`, failing
/// with `Error::Corrupt` if the payload is damaged or decompresses to another length.
fn unpack_into(payload: &[u8], raw: usize, at: u64, out: &mut Vec<u8>) -> Result<()> {
    out.clear();
    if out.capacity() < raw {
        *out = vec![0; raw];
    } else {
        out.resize(raw, 0);
    }
    let len = lz4_flex::block::decompress_into(payload, out)
        .map_err(|error| Error::Corrupt(format!("bad compressed body at {at}: {error}")))?;
    if len != raw {
        return Err(wrong_length(at));
    }
    Ok(())
}

/// The error for a compressed body at `at` that decompresses to the wrong length.
fn wrong_length(at: u64) -> Error {
    Error::Corrupt(format!("compressed body at {at} has the wrong length"))
}

/// The body of the record at `at` whose bytes are `bytes`: its payload as it is, or decompressed
/// into `out`.
pub(crate) fn body_of<'a>(
    bytes: &'a [u8],
    stored: &Stored,
    at: u64,
    out: &'a mut Vec<u8>,
) -> Result<&'a [u8]> {
    let payload = &bytes[stored.payload.clone()];
    if !stored.packed {
        return Ok(payload);
    }
    unpack_into(payload, stored.raw, at, out)?;
    Ok(out)
}

/// Reads a varint at `pos` in `bytes`, or `None` if it runs past them or is too long for this
/// quick path.
fn varint_within(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for index in 0..MAX_VARINT - 1 {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Reads a whole record from `bytes`, which start at `start` in the log, filling `links`, or
/// `None` if it runs past them or does not parse, for the caller to read it the slow way.
fn stored_within(bytes: &[u8], start: u64, links: &mut Vec<Ptr>) -> Option<Stored> {
    let mut pos = 0;
    let stored = usize::try_from(varint_within(bytes, &mut pos)?).ok()?;
    let flagged = varint_within(bytes, &mut pos)?;
    let count = usize::try_from(flagged >> 1).ok()?;
    let raw = match flagged & 1 {
        0 => stored,
        _ => {
            let raw = usize::try_from(varint_within(bytes, &mut pos)?).ok()?;
            if raw > stored.saturating_mul(MAX_RATIO).saturating_add(MAX_RATIO) {
                return None;
            }
            raw
        }
    };
    let payload = pos..pos.checked_add(stored)?;
    if payload.end > bytes.len() || count > (bytes.len() - payload.end) / (HASH_BYTES + 1) {
        return None;
    }
    pos = payload.end;
    links.clear();
    for _ in 0..count {
        let hash: Hash = bytes.get(pos..pos + HASH_BYTES)?.try_into().ok()?;
        pos += HASH_BYTES;
        let delta = unzigzag(varint_within(bytes, &mut pos)?);
        links.push(Ptr {
            hash,
            offset: start.wrapping_sub(delta as u64),
        });
    }
    Some(Stored {
        raw,
        packed: flagged & 1 == 1,
        payload,
        len: pos,
    })
}

/// The scratch buffers that [`verify_stored`] reuses from one record to the next.
#[derive(Default)]
pub(crate) struct Unpacked {
    body: Vec<u8>,
    hashes: Vec<Hash>,
}

/// Checks that `record`, the bytes of the record at `ptr`, holds an object whose hash is `ptr`'s,
/// and lets the scratch body shrink back to [`SCRATCH_KEEP`].
///
/// # Errors
///
/// `Error::Corrupt` if the record does not parse, its body does not decompress, or the hash
/// differs.
pub(crate) fn verify_stored(record: &[u8], ptr: Ptr, scratch: &mut Unpacked) -> Result<()> {
    let checked = check_stored(record, ptr, scratch);
    scratch.body.clear();
    scratch.body.shrink_to(SCRATCH_KEEP);
    checked
}

/// The check that [`verify_stored`] makes.
fn check_stored(record: &[u8], ptr: Ptr, scratch: &mut Unpacked) -> Result<()> {
    let mut reader = RecordReader {
        file: None,
        start: ptr.offset,
        tail: ptr.offset.saturating_add(record.len() as u64),
        bytes: Cow::Borrowed(record),
        pos: 0,
    };
    let header = reader.header()?;
    let start = reader.pos;
    reader.take(header.stored)?;
    let payload = start..reader.pos;
    scratch.hashes.clear();
    for _ in 0..header.links {
        let link = reader.link()?;
        scratch.hashes.push(link.hash);
    }
    let payload = &record[payload];
    let body = match header.raw {
        Some(raw) => {
            unpack_into(payload, raw, ptr.offset, &mut scratch.body)?;
            &scratch.body
        }
        None => payload,
    };
    let hash = hash_parts(body, scratch.hashes.iter());
    if hash != ptr.hash {
        return Err(Error::Corrupt(format!(
            "hash mismatch for object at {}",
            ptr.offset
        )));
    }
    Ok(())
}

/// Seals the file of a new generation: syncs its log, writes a superblock naming `head` at `seq`
/// with the log ending at `log_end`, anchored at itself with `basis`, to both slots, and syncs
/// again.
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

/// Opens the file at `path` and, for a writer, takes the writer lock: exclusive, or shared with
/// `shared`.
///
/// A writer tries a held lock again [`LOCK_RETRIES`] times, a millisecond apart, before it fails
/// with `Error::Locked`, and opens the path again if it no longer names the file it locked. A
/// read-only open takes no lock and fails with `Error::NoDatabase` if the file is missing. A
/// shared writer fails with an `Unsupported` I/O error where there are no byte-range locks.
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
            true => platform::try_lock_shared(&file),
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

/// A byte-range lock that an open holds on its file while it recovers, before any other thread
/// can reach the store, released when dropped.
struct Range<'a> {
    file: &'a File,
    at: u64,
}

/// The error for a lock that could not be taken: `Error::Locked` if another holds it, and the I/O
/// error otherwise.
fn locked(error: TryLockError) -> Error {
    match error {
        TryLockError::Error(error) => Error::Io(error),
        TryLockError::WouldBlock => Error::Locked,
    }
}

impl<'a> Range<'a> {
    /// Takes the lock at `at` exclusive, waiting for it, if the open is `shared`, and nothing
    /// otherwise.
    fn hold(file: &'a File, at: u64, shared: bool) -> Result<Option<Range<'a>>> {
        if !shared {
            return Ok(None);
        }
        platform::lock_range(file, at, 1, true, true).map_err(locked)?;
        Ok(Some(Range { file, at }))
    }
}

impl Drop for Range<'_> {
    fn drop(&mut self) {
        let _ = platform::unlock_range(self.file, self.at, 1);
    }
}

/// The byte of the branch lock of the branch called `name`: [`LANES_AT`] plus 60 bits of an xxh3
/// of the name. Two names can share a byte, which costs only parallelism.
pub(crate) fn lane_of(name: &str) -> u64 {
    LANES_AT + (xxh3_64(name.as_bytes()) & (LANES - 1))
}

/// A byte-range lock that one thread of this process holds, released when dropped.
///
/// Every thread of a process locks through one open file description, which grants a range it
/// already holds again at once, so [`Store::hold`] keeps a table of the ranges its threads hold. A
/// thread waits until no other thread of this process holds the range before it takes it from the
/// kernel, and the drop unlocks it in the kernel before it frees the table's entry.
pub(crate) struct Held {
    store: Store,
    at: u64,
    len: u64,
}

impl Drop for Held {
    fn drop(&mut self) {
        let inner = &self.store.inner;
        let mut taken = inner.taken.lock();
        let _ = platform::unlock_range(&inner.file, self.at, self.len);
        taken.retain(|at| *at != self.at);
        inner.freed.notify_all();
    }
}

/// The branch locks this process holds.
#[derive(Default)]
struct Lanes {
    /// For each byte, how many of this process's lanes hold it, or `None` while a thread is taking
    /// it from the kernel.
    held: HashMap<u64, Option<usize>>,
    /// Set while a thread wants or holds every byte at once, so that no thread takes one.
    whole: bool,
}

/// Every branch lock at once, held by one thread of this process and released when dropped.
pub(crate) struct AllLanes {
    store: Store,
}

impl Drop for AllLanes {
    fn drop(&mut self) {
        let inner = &self.store.inner;
        let mut lanes = inner.lanes.lock();
        let _ = platform::unlock_range(&inner.file, LANES_AT, LANES);
        lanes.whole = false;
        inner.lanes_moved.notify_all();
    }
}

/// What catching up with other processes found past the end of the log this process knew.
pub(crate) enum Followed {
    /// No new frame.
    Still,
    /// New whole frames, of which this is the last.
    Moved(Frame),
    /// The marker of a cycle that replaced the file, which the path no longer names.
    Retired,
}

/// A frame placed in the log: its sequence number, the end of the log after it, and its head.
#[derive(Clone, Copy)]
pub(crate) struct Frame {
    pub(crate) seq: u64,
    pub(crate) end: u64,
    pub(crate) head: Ptr,
}

impl Frame {
    /// The frame as a position in the log.
    fn mark(&self) -> Mark {
        Mark {
            seq: self.seq,
            log_end: self.end,
            head: Some(self.head),
        }
    }

    /// The frame's header, with its check left zero for the writer to fill in.
    fn header(&self) -> [u8; FRAME_BYTES] {
        let mut header = [0u8; FRAME_BYTES];
        header[0..8].copy_from_slice(FRAME_MAGIC);
        self.mark().fill(&mut header[8..]);
        header
    }
}

/// What placing frames keeps under its lock.
struct Cursor {
    /// The sequence number of the last frame placed, followed or refreshed.
    seq: u64,
    /// How far the zeroes of the [`Durability::Fsync`] preallocation reach, or `u64::MAX` once a
    /// write of them failed and no more are tried.
    reserved: u64,
    /// The next chunk of zeroes to add.
    reserve: u64,
}

/// The newest frame a superblock names, and the anchor, the newest frame known to be durable.
#[derive(Clone, Copy)]
struct Marks {
    latest: Mark,
    anchor: Mark,
}

/// The frames written to the page cache ahead of one before them, and the readers waiting for
/// `written` to move.
#[derive(Default)]
struct Arrivals {
    /// The start and end of each frame written ahead.
    early: Vec<(u64, u64)>,
    /// How many readers wait on `landed`.
    waiters: usize,
}

/// The store's file, recording every write, sync and change of length when a trace is given, for
/// tests that replay a crash.
///
/// A write or a change of length is recorded before it is made, since it may land in part, and a
/// sync once it has succeeded, since only then does it count.
pub(crate) struct Traced {
    file: File,
    trace: Option<Trace>,
    /// The file's identity, which the trace names it by and which tells whether a path still
    /// names it.
    inode: u64,
}

impl Traced {
    /// Wraps `file`, recording to `trace` if one is given.
    pub(crate) fn new(file: File, trace: Option<Trace>) -> Result<Traced> {
        let inode = platform::file_id(&file)?;
        Ok(Traced { file, trace, inode })
    }

    /// Records the operation that `op` makes from the file's identity, if there is a trace.
    fn log(&self, op: impl FnOnce(u64) -> Op) {
        if let Some(trace) = &self.trace {
            trace.lock().push(op(self.inode));
        }
    }

    /// Records that the file was created.
    pub(crate) fn created(&self) {
        self.log(Op::Create);
    }

    /// Writes `bytes` at `at`.
    pub(crate) fn write_all_at(&self, bytes: &[u8], at: u64) -> std::io::Result<()> {
        self.log(|inode| Op::Write(inode, at, bytes.to_vec()));
        self.file.write_all_at(bytes, at)
    }

    /// Syncs the file's data.
    pub(crate) fn sync_data(&self) -> std::io::Result<()> {
        self.file.sync_data()?;
        self.log(Op::Sync);
        Ok(())
    }

    /// Syncs the file's data and metadata.
    fn sync_all(&self) -> std::io::Result<()> {
        self.file.sync_all()?;
        self.log(Op::Sync);
        Ok(())
    }

    /// Sets the file's length.
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

/// A handle on a generation that a cycle replaced, which does not keep its store.
pub(crate) struct Retired(Weak<Inner>);

impl Retired {
    /// The old file's size, while some handle still counts its store.
    pub(crate) fn bytes(&self) -> Option<u64> {
        self.0
            .upgrade()
            .map(|inner| inner.tail.load(Ordering::Acquire))
    }
}

/// What every clone of a [`Store`] shares.
struct Inner {
    /// The store's id, unique in this process.
    id: u64,
    /// The database's path: canonical for a shared writer, as given otherwise.
    path: PathBuf,
    /// The file.
    file: Traced,
    /// The end of the log as of the last published frame, below which every published pointer
    /// lies.
    tail: AtomicU64,
    /// The end of the run of frames already in the page cache, which bounds reads.
    written: AtomicU64,
    /// The end of the last frame placed, where the next one starts.
    placed: AtomicU64,
    /// Frames written ahead of `written`, and the readers waiting for it.
    arrivals: Mutex<Arrivals>,
    /// Woken when `written` moves or the store poisons.
    landed: Condvar,
    /// What placing frames keeps under its lock.
    cursor: Mutex<Cursor>,
    /// The superblock slot written last.
    slot: AtomicU64,
    /// The newest frame a superblock names, and the anchor.
    marks: Mutex<Marks>,
    /// Set while a checkpoint syncs, so that only one does at a time.
    checkpointing: AtomicBool,
    /// The newer superblock the open found naming frames its scan did not keep, which
    /// [`Store::refresh`] ignores until a writer replaces it.
    ignored: Option<Mark>,
    /// The decoded nodes and values, by the hash of their object.
    cache: Cache<Hash, CacheEntry, EntryWeight>,
    /// Where objects of this file sit, by hash, for transactions to skip writing them again.
    dedupe: Dedupe,
    /// What the superblock records of how this generation began.
    basis: Basis,
    /// Set once a write or a sync has failed, after which nothing more is acknowledged.
    poisoned: AtomicBool,
    /// The start of the first frame whose write failed, which `written` never passes.
    lost_at: AtomicU64,
    /// Set once an fdatasync has failed, after which no superblock is written and no checkpoint
    /// moves the anchor.
    unsynced: AtomicBool,
    /// Set once the store is closed, after which nothing more enters the cache.
    closed: AtomicBool,
    /// The branch locks this process holds.
    lanes: Mutex<Lanes>,
    /// Woken when a branch lock is taken or let go.
    lanes_moved: Condvar,
    /// The offsets of the byte-range locks that threads of this process hold through [`Held`].
    taken: Mutex<Vec<u64>>,
    /// Woken when one of them is let go.
    freed: Condvar,
    /// How the store was opened.
    options: StoreOptions,
}

/// An open file of a database: one generation of its log, with the cache of what was decoded from
/// it.
///
/// Clones share the file, the cache and the deduplication table, and the file closes with the last
/// of them. [`Store::open`] opens or creates a file, [`Store::begin`] starts a [`Txn`] that
/// appends to it, and [`Store::read_object`] and [`Store::load`] read objects back, checking each
/// one's hash. [`Store::close`] lets the writer lock go while clones still read.
///
/// # Examples
///
/// ```
/// use ferro::codec::{Data, Encoder};
/// use ferro::store::{Store, StoreOptions};
///
/// # fn main() -> ferro::Result<()> {
/// # let dir = tempfile::tempdir()?;
/// # let path = dir.path().join("raw.ferro");
/// let (store, head) = Store::open(&path, StoreOptions::default())?;
/// assert!(head.is_none());
///
/// let mut txn = store.begin()?;
/// let mut encoder = Encoder::new(&mut txn);
/// "hello".to_string().encode(&mut encoder)?;
/// let ptr = encoder.finish()?;
/// txn.commit(ptr)?;
/// assert_eq!(*store.load::<String>(ptr)?, "hello");
/// store.close();
///
/// let (store, head) = Store::open(&path, StoreOptions::default())?;
/// assert_eq!(head, Some(ptr));
/// store.close();
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Store {
    inner: Shared<Inner>,
}

/// The store of an iterator, cursor or row finder: borrowed through a hazard slot without a count,
/// or a clone it owns when it has no slot.
///
/// It derefs to the store. A borrowed one is valid for as long as its slot holds the store, and is
/// never dropped as a store. Clones through it go through [`Store::share`], since the store's count
/// may be passing through zero.
pub(crate) struct Lent {
    store: ManuallyDrop<Store>,
    /// Whether `store` is a clone this owns, which its drop lets go.
    owned: bool,
}

impl Deref for Lent {
    type Target = Store;

    #[inline]
    fn deref(&self) -> &Store {
        &self.store
    }
}

impl Drop for Lent {
    #[inline]
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: an owned `Lent` holds a store it cloned, with a count of its own, which is
            // dropped once, here, and never used again. A borrowed one holds no count and is never
            // dropped.
            unsafe { ManuallyDrop::drop(&mut self.store) };
        }
    }
}

/// A store that closes itself when dropped, for a store opened outside a database, so that an
/// error on the way out still lets the writer lock go and drops the cache.
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
    /// Opens the database file at `path`, creating it if it is missing and the store may write,
    /// and returns the store and the head that the newest frame names, `None` for a new file.
    ///
    /// A writer takes the writer lock, recovers the log as the module's docs describe, keeping
    /// only frames that follow each other from the anchor and truncating the rest, and removes a
    /// copy that a cycle left beside the file. A read-only store takes no lock, and scans past the
    /// newest superblock only when no writer is alive.
    ///
    /// # Errors
    ///
    /// - [`Error::Locked`] if another writer has the file open.
    /// - [`Error::NoDatabase`] if a read-only open finds no file.
    /// - [`Error::NotFerro`] if the file holds something else, or is empty and opened read-only.
    /// - [`Error::Corrupt`] if both superblocks are invalid, the newest names a log the file does
    ///   not hold, or a frame past its anchor reached the disk whole and was damaged since, or,
    ///   for a shared writer that opens beside another, any frame the newest superblock names
    ///   does not check, each of which leaves the file as it was.
    /// - [`Error::Io`] if the file cannot be opened, read, written or synced, or, with
    ///   [`StoreOptions::shared`], where the byte-range locks it needs do not exist.
    pub fn open(path: &Path, options: StoreOptions) -> Result<(Store, Option<Ptr>)> {
        Store::open_as(path, path, options)
    }

    /// Opens the file at `actual` as the database at `path`, as a cycle opens its copy before it
    /// renames the copy over the path.
    ///
    /// A shared writer takes the placement lock and then checks that `path` still names the file
    /// it opened, opening it again if a cycle renamed another over it meanwhile, and recovers under
    /// the placement and superblock locks. If another shared writer holds [`MULTI_AT`], it has
    /// opened and no crash can have taken back what the newest superblock names, so recovery
    /// truncates nothing that superblock names, and only once recovered does this writer take
    /// [`MULTI_AT`] itself, under the same locks. A stale copy is removed only by an open of the
    /// path itself, and for a shared writer only if no cycle runs.
    pub(crate) fn open_as(
        actual: &Path,
        path: &Path,
        options: StoreOptions,
    ) -> Result<(Store, Option<Ptr>)> {
        let shared = options.shared && !options.read_only;
        let renamed = actual != path;
        let file = loop {
            let file = open_locked(actual, options.read_only, shared)?;
            if !shared {
                break file;
            }
            platform::lock_range(&file, COMMIT_AT, 1, true, true).map_err(locked)?;
            if renamed || platform::path_id(actual)? == platform::file_id(&file)? {
                break file;
            }
        };
        let kept = match shared {
            true => platform::canonicalize(path)?,
            false => path.to_path_buf(),
        };
        let commit = shared.then_some(Range {
            file: &file,
            at: COMMIT_AT,
        });
        let tops = Range::hold(&file, SUPER_AT, shared)?;
        let idle = !shared || platform::lock_range(&file, CYCLE_AT, 1, true, false).is_ok();
        if !options.read_only && !renamed && idle {
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
                    let live = shared && platform::range_held(&file, MULTI_AT, 1)?;
                    recover(&file, sb, slot, len, live)?
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
        if shared {
            platform::lock_range(&file, MULTI_AT, 1, false, false).map_err(locked)?;
        }
        drop((tops, commit));
        let cache_bytes = options.cache_bytes.max(1);
        let items = ((cache_bytes / 4096).max(64)) as usize;
        let dedupe = (cache_bytes / 4096).clamp(MIN_DEDUPE, MAX_DEDUPE) as usize;
        let head = sb.head;
        let file = Traced::new(file, options.trace.clone())?;
        let store = Store {
            inner: Shared::new(Inner {
                id: NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed),
                path: kept,
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
                lanes: Mutex::new(Lanes::default()),
                lanes_moved: Condvar::new(),
                taken: Mutex::new(Vec::new()),
                freed: Condvar::new(),
                options,
            }),
        };
        Ok((store, head))
    }

    /// The store's id, which no other store this process opens shares, so that a collection can
    /// tell whether it was read from this store.
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    /// The database's path, as the store was opened by, or made canonical for a shared writer.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The end of the log as of the last published frame, in bytes, which leaves out any zeroes
    /// preallocated past it.
    pub fn size(&self) -> u64 {
        self.inner.tail.load(Ordering::Acquire)
    }

    /// The end of the run of frames already in the page cache, below which every object can be
    /// read.
    pub(crate) fn written(&self) -> u64 {
        self.inner.written.load(Ordering::Acquire)
    }

    /// Whether the store was opened read-only.
    pub fn is_read_only(&self) -> bool {
        self.inner.options.read_only
    }

    /// The sequence number of the last frame placed, followed or refreshed.
    pub(crate) fn last_seq(&self) -> u64 {
        self.inner.cursor.lock().seq
    }

    /// What the superblock records of how this generation began.
    pub(crate) fn basis(&self) -> Basis {
        self.inner.basis
    }

    /// The file's metadata.
    pub(crate) fn metadata(&self) -> Result<std::fs::Metadata> {
        Ok(self.inner.file.metadata()?)
    }

    /// A clone that waits out a count passing through zero, for a handle that may hold no count of
    /// its own.
    ///
    /// The store that [`Store::lend`] lends into a slot holds no count, and the count may be at
    /// zero between a last owner's decrement and its parking of the count. A nested map decoded
    /// through such a store clones it in the decoder, which uses this; every other clone adds to a
    /// count its handle holds.
    pub(crate) fn share(&self) -> Store {
        Store {
            inner: Shared::share(&self.inner),
        }
    }

    /// A handle on this generation that does not keep its store, for reporting the old file's
    /// size once a cycle has replaced it.
    pub(crate) fn retired(&self) -> Retired {
        Retired(Shared::downgrade(&self.inner))
    }

    /// Lends the store to `hazard`, so that an iterator, cursor or row finder holds it without a
    /// count, or clones it when there is no slot.
    ///
    /// With a slot, it writes the store's address there and sets its `lent` flag, and returns a
    /// handle that takes no count and is valid for as long as the slot holds the address: whoever
    /// lets go of the store's last count meanwhile finds the slot and parks the count. `hazard`
    /// must be an entry of a [`Hazards`](crate::epoch::Hazards) record.
    #[inline]
    pub(crate) fn lend(&self, hazard: Option<&AtomicPtr<()>>) -> Lent {
        let Some(hazard) = hazard else {
            return Lent {
                store: ManuallyDrop::new(self.clone()),
                owned: true,
            };
        };
        Shared::lend_to(&self.inner, hazard);
        Lent {
            store: ManuallyDrop::new(Store {
                // SAFETY: the pointer is the value's address in the live allocation that
                // `self.inner` reaches, and the copy takes no count: it sits in a `ManuallyDrop`
                // that `Lent::drop` leaves alone, since `owned` is false. The slot just written
                // keeps the store alive until it is cleared, since whoever lets go of its last
                // count meanwhile finds `lent` and the slot and parks that count.
                inner: unsafe { Shared::from_raw(Shared::as_ptr(&self.inner)) },
            }),
            owned: false,
        }
    }

    /// Records `op` in the store's trace, for tests.
    #[doc(hidden)]
    pub fn trace(&self, op: Op) {
        self.inner.file.log(|_| op);
    }

    /// Records that this store's file was renamed over the database's path.
    pub(crate) fn trace_rename(&self) {
        self.inner.file.log(Op::Rename);
    }

    /// Reads the newest superblock of a read-only store, moves the store's end of the log to it,
    /// and returns the head it names.
    ///
    /// It returns `None`, changing nothing, for a superblock older than the last frame the store
    /// knew, or for the superblock that the open refused, until a writer replaces it. It reads
    /// only the superblocks and never scans, since a frame past the superblock may not be durable
    /// while a writer is alive.
    ///
    /// # Errors
    ///
    /// - [`Error::Replaced`] if the path no longer names this file, since a cycle replaced it.
    /// - [`Error::Corrupt`] if both superblocks are invalid, or the file is shorter than the log
    ///   the newest one names.
    /// - [`Error::Io`] if the file cannot be read.
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

    /// Poisons the store, so that nothing more is acknowledged until it is opened again, and wakes
    /// every reader waiting for `written`.
    pub(crate) fn poison(&self) {
        self.inner.poisoned.store(true, Ordering::Release);
        let _arrivals = self.inner.arrivals.lock();
        self.inner.landed.notify_all();
    }

    /// Poisons the store after a failed fdatasync, after which no superblock is written and no
    /// checkpoint moves the anchor, since a later sync can succeed without the pages this one
    /// dropped.
    fn poison_unsynced(&self) {
        self.inner.unsynced.store(true, Ordering::Release);
        self.poison();
    }

    /// Poisons the store after the write of the frame starting at `at` failed, so that `written`
    /// stops there and readers waiting past it give up.
    fn poison_at(&self, at: u64) {
        self.inner.lost_at.fetch_min(at, Ordering::AcqRel);
        self.poison();
    }

    /// Notes that the frame from `start` to `end` is in the page cache, and moves `written` over
    /// it and over any frames written ahead of it that now follow on.
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

    /// `written`, once it has passed `offset`: waits while `offset` lies in a frame that is placed
    /// but still being written, returns at once for an offset past every placed frame, and stops
    /// waiting at a frame whose write failed.
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

    /// `written`, once every frame before `end` is in the page cache or one of them failed, as the
    /// leader of a sync waits for before it syncs.
    pub(crate) fn landed_to(&self, end: u64) -> u64 {
        self.landed_past(end.saturating_sub(1))
    }

    /// The end below which the object at `offset` can be read, once its frame is written.
    ///
    /// # Errors
    ///
    /// [`Error::Poisoned`] if the frame holding `offset` will never be written, since a write
    /// before it failed.
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

    /// Fails with [`Error::Poisoned`] once a write or a sync has failed.
    pub(crate) fn check_healthy(&self) -> Result<()> {
        if self.inner.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        Ok(())
    }

    /// Makes `frame` and every frame before it durable and published: syncs the file under
    /// [`Durability::Fsync`], writes a superblock naming the frame to the other slot, and moves the
    /// published end of the log to the frame's end.
    ///
    /// The leader of a group commit calls it for the last frame of its batch, once every frame in
    /// the batch is in the page cache. Under [`Durability::Buffered`] it writes the superblock
    /// without syncing, keeping the last checkpoint's anchor. With [`StoreOptions::shared`] it
    /// syncs under the superblock lock, as [`Store::sync_shared`] describes.
    ///
    /// # Errors
    ///
    /// [`Error::Poisoned`] if a frame before the end of `frame` is not in the page cache or an
    /// earlier fdatasync failed, and the I/O error of a failed sync or superblock write, which
    /// poisons the store.
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

    /// [`Store::sync`] for a shared writer, under the superblock lock.
    ///
    /// Under [`Durability::Fsync`], if the newest superblock already anchors at or past `frame`,
    /// another process's sync covered it and nothing is written. Otherwise it reads on past `frame`
    /// to the newest whole frame of any process, syncs, and names that frame, so that the
    /// processes that queued frames meanwhile find them covered. The superblock goes to the slot
    /// that does not hold the newest, unless the newest is already as new.
    fn sync_shared(&self, frame: Frame, durable: bool) -> Result<()> {
        let inner = &self.inner;
        let _held = self.hold(SUPER_AT, 1, true)?;
        let newest = newest_super(&inner.file)?;
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
                scan(&inner.file, Super::new(mark, mark, inner.basis), len, false)?.mark()
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

    /// Syncs the file under [`Durability::Fsync`], poisoning the store if that fails, as a large
    /// receive does once it has placed its pieces and before it lands them. It does nothing under
    /// `Buffered`.
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

    /// Syncs the file and moves the anchor to `frame` once `frame` takes the log
    /// [`StoreOptions::checkpoint_bytes`] past the anchor, which bounds what a power cut loses
    /// under [`Durability::Buffered`].
    ///
    /// One checkpoint runs at a time, and a failed sync poisons the store. The leader of a sync
    /// calls it once it has published and woken its batch, so that only its own commit waits.
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

    /// Syncs and writes a superblock that anchors itself at the newest frame named, if the anchor
    /// lags behind it, as a writer does as it closes. It does nothing for a read-only or poisoned
    /// store.
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

    /// Writes `sb` to the slot other than the one written last, or, for a shared writer, under the
    /// superblock lock to the slot that does not hold the newest superblock, unless that one is
    /// already as new.
    fn write_super(&self, sb: &Super) -> Result<()> {
        let inner = &self.inner;
        let _held = match inner.options.shared {
            true => self.hold(SUPER_AT, 1, true)?,
            false => None,
        };
        let slot = match inner.options.shared {
            false => 1 - inner.slot.load(Ordering::Relaxed),
            true => match newest_super(&inner.file)? {
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

    /// Whether the store was opened with [`StoreOptions::shared`].
    pub(crate) fn is_shared(&self) -> bool {
        self.inner.options.shared
    }

    /// Takes the `len` bytes from `at` exclusive for this thread: waits until no other thread of
    /// this process holds them, then locks them in the kernel, waiting there too if `wait` is set.
    ///
    /// It returns `None` if `wait` is not set and the range is held, here or in another process.
    fn hold(&self, at: u64, len: u64, wait: bool) -> Result<Option<Held>> {
        let inner = &self.inner;
        let mut taken = inner.taken.lock();
        while taken.contains(&at) {
            if !wait {
                return Ok(None);
            }
            inner.freed.wait(&mut taken);
        }
        taken.push(at);
        drop(taken);
        let locked = platform::lock_range(&inner.file, at, len, true, wait);
        if locked.is_ok() {
            return Ok(Some(Held {
                store: self.clone(),
                at,
                len,
            }));
        }
        let mut taken = inner.taken.lock();
        taken.retain(|held| *held != at);
        inner.freed.notify_all();
        match locked {
            Err(TryLockError::Error(error)) => Err(error.into()),
            _ => Ok(None),
        }
    }

    /// Takes the placement lock, waiting for it.
    pub(crate) fn lock_commit(&self) -> Result<Held> {
        self.hold(COMMIT_AT, 1, true)?.ok_or(Error::Locked)
    }

    /// Takes every branch lock at once, as a cycle's prune and pause and a landing receive do.
    ///
    /// It marks the whole range wanted, so that no thread of this process takes a branch byte,
    /// waits until none holds one, and then locks the range in the kernel. While it waits it calls
    /// `stale` every millisecond and gives up with `None` once that returns `true`, since a thread
    /// holding a byte for a branch attached after the caller listed its lanes may be waiting for
    /// one of those lanes.
    pub(crate) fn lock_lanes(&self, stale: impl Fn() -> bool) -> Result<Option<AllLanes>> {
        let inner = &self.inner;
        let mut lanes = inner.lanes.lock();
        while lanes.whole {
            inner.lanes_moved.wait(&mut lanes);
        }
        lanes.whole = true;
        while !lanes.held.is_empty() {
            inner
                .lanes_moved
                .wait_for(&mut lanes, Duration::from_millis(1));
            drop(lanes);
            let stale = stale();
            lanes = inner.lanes.lock();
            if stale {
                lanes.whole = false;
                inner.lanes_moved.notify_all();
                return Ok(None);
            }
        }
        drop(lanes);
        let all = AllLanes {
            store: self.clone(),
        };
        platform::lock_range(&inner.file, LANES_AT, LANES, true, true).map_err(
            |error| match error {
                TryLockError::Error(error) => Error::Io(error),
                TryLockError::WouldBlock => Error::Locked,
            },
        )?;
        Ok(Some(all))
    }

    /// Takes the cycle lock, waiting for it if `wait` is set, and returns `None` if it is not and
    /// the lock is held.
    pub(crate) fn lock_cycle(&self, wait: bool) -> Result<Option<Held>> {
        self.hold(CYCLE_AT, 1, wait)
    }

    /// Takes the branch lock at `at`, from [`lane_of`], waiting for it.
    pub(crate) fn lock_lane(&self, at: u64) -> Result<()> {
        self.take_lane(at, true).map(|_| ())
    }

    /// Takes the branch lock at `at` if it can without waiting, and returns whether it did.
    pub(crate) fn try_lock_lane(&self, at: u64) -> Result<bool> {
        self.take_lane(at, false)
    }

    /// Takes the branch lock at `at`: shares the byte with any lane of this process that holds it,
    /// and otherwise locks it in the kernel. Unless `wait` is set, it returns `false` rather than
    /// wait for the whole range, for another thread taking the byte, or for another process.
    fn take_lane(&self, at: u64, wait: bool) -> Result<bool> {
        let inner = &self.inner;
        let mut lanes = inner.lanes.lock();
        loop {
            if lanes.whole {
                match wait {
                    true => inner.lanes_moved.wait(&mut lanes),
                    false => return Ok(false),
                }
                continue;
            }
            match lanes.held.get_mut(&at) {
                Some(Some(count)) => {
                    *count += 1;
                    return Ok(true);
                }
                Some(None) if wait => inner.lanes_moved.wait(&mut lanes),
                Some(None) => return Ok(false),
                None => break,
            }
        }
        lanes.held.insert(at, None);
        drop(lanes);
        let locked = platform::lock_range(&inner.file, at, 1, true, wait);
        let mut lanes = inner.lanes.lock();
        match locked {
            Ok(()) => lanes.held.insert(at, Some(1)),
            Err(_) => lanes.held.remove(&at),
        };
        inner.lanes_moved.notify_all();
        match locked {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    /// Lets go of one lane's hold on the branch lock at `at`, unlocking the byte in the kernel once
    /// the last lane of this process lets go of it.
    pub(crate) fn unlock_lane(&self, at: u64) {
        let inner = &self.inner;
        let mut lanes = inner.lanes.lock();
        match lanes.held.get_mut(&at) {
            Some(Some(count)) if *count > 1 => *count -= 1,
            _ => {
                lanes.held.remove(&at);
                let _ = platform::unlock_range(&inner.file, at, 1);
                inner.lanes_moved.notify_all();
            }
        }
    }

    /// Catches up with the frames other processes placed past `placed`, the end of the log this
    /// process knew, reading each header and checking each frame, and moves `written` past them.
    ///
    /// A process that is alone reads one header. It meets a cycle's marker as the end of the log,
    /// or, once the path names another file, as [`Followed::Retired`].
    ///
    /// A caller that `holds` the placement lock knows that no live process is writing a frame, so
    /// a frame that reached the disk whole and was damaged since fails it with [`Error::Corrupt`]
    /// rather than end the log for the next frame to be written over. Without the lock such a
    /// frame may be one still being written, and ends the log.
    ///
    /// Under the lock it also reads the newest superblock, and if the frames it followed end
    /// before the log that superblock names, a frame other processes acknowledged does not check,
    /// whatever the judgement made of it, since no crash can have taken it back while this process
    /// is alive: that fails it with [`Error::Corrupt`] too, and nothing is truncated or written
    /// there. Past that log, what a dead process left, part of a frame or the records a frame
    /// spilled before its header, behind a stale marker or not, is truncated and the truncation
    /// synced, as [`recover`] does, so that the next frame is written over nothing a power cut
    /// could bring back under it. A failed truncation or sync poisons the store.
    pub(crate) fn follow(&self, holds: bool) -> Result<Followed> {
        let inner = &self.inner;
        let mut cursor = inner.cursor.lock();
        let start = Mark {
            seq: cursor.seq,
            log_end: inner.placed.load(Ordering::Acquire),
            head: None,
        };
        let mut at = Super::new(start, start, Basis::default());
        let mut last = None;
        let mut len = None;
        let left = loop {
            let mut peek = [0u8; FRAME_BYTES + LEFT_PEEK];
            let got = read_some(&inner.file, &mut peek, at.log_end)?;
            let written = peek[..got].iter().any(|byte| *byte != 0);
            let Some(header) = peek
                .first_chunk::<FRAME_BYTES>()
                .filter(|_| got >= FRAME_BYTES)
            else {
                break written;
            };
            if &header[0..8] == RETIRED_MAGIC {
                if platform::path_id(&inner.path)? != inner.file.inode {
                    return Ok(Followed::Retired);
                }
                break peek[FRAME_BYTES..got].iter().any(|byte| *byte != 0);
            }
            if !frame_like(header) {
                break written;
            }
            let len = match len {
                Some(len) => len,
                None => *len.insert(inner.file.metadata()?.len()),
            };
            match examine(&inner.file, &at, header, len, holds)? {
                Found::Whole(next) if next.head.is_some() => {
                    at = next;
                    last = Some(next);
                }
                Found::Damaged(offset) => return Err(damaged(&at, offset)),
                Found::Torn => break true,
                Found::Whole(_) => break false,
            }
        };
        if holds {
            let (named, _) = newest_super(&inner.file)?
                .ok_or_else(|| Error::Corrupt("both superblocks are invalid".into()))?;
            reaches(&at, &named)?;
        }
        if holds && left {
            let end = at.log_end;
            let cut = inner.file.set_len(end).and_then(|()| inner.file.sync_all());
            if let Err(error) = cut {
                self.poison_unsynced();
                return Err(error.into());
            }
            cursor.reserved = cursor.reserved.min(end);
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

    /// Moves this process's end of the log past `frame`, a frame another process placed, once the
    /// caller has loaded its refs.
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

    /// Writes a cycle's marker at the end of the placed log, so that a process catching up learns
    /// that the file is being replaced.
    pub(crate) fn mark_retired(&self) -> Result<()> {
        let mut header = [0u8; FRAME_BYTES];
        header[0..8].copy_from_slice(RETIRED_MAGIC);
        let at = self.inner.placed.load(Ordering::Acquire);
        self.inner.file.write_all_at(&header, at)?;
        Ok(())
    }

    /// Reads the object at `ptr` and checks its hash.
    ///
    /// An object in a frame that is placed but still being written is waited for.
    ///
    /// # Errors
    ///
    /// - [`Error::Corrupt`] if `ptr` lies outside the log, or the record there does not parse,
    ///   does not decompress or does not match the hash.
    /// - [`Error::Poisoned`] if the object's frame will never be written, since a write before it
    ///   failed.
    /// - [`Error::Io`] if the read fails.
    pub fn read_object(&self, ptr: Ptr) -> Result<Object> {
        self.read_record(ptr).map(|(object, _)| object)
    }

    /// [`Store::read_object`], with the record's length in the log.
    pub(crate) fn read_record(&self, ptr: Ptr) -> Result<(Object, u32)> {
        let tail = self.readable_past(ptr.offset)?;
        let file = Some(&*self.inner.file);
        let (object, len, _) = Store::read_within(file, ptr, tail, &[], true, false)?;
        Ok((object, len))
    }

    /// [`Store::read_object`], with the record's compressed body if it has one, for a sync to send
    /// as it is.
    pub(crate) fn read_packed(&self, ptr: Ptr) -> Result<(Object, Option<Vec<u8>>)> {
        let tail = self.readable_past(ptr.offset)?;
        let file = Some(&*self.inner.file);
        let (object, _, packed) = Store::read_within(file, ptr, tail, &[], true, true)?;
        Ok((object, packed))
    }

    /// Reads the record at `ptr` into `bytes` as it is stored, without decompressing it or checking
    /// its hash, fills `links`, and returns the record's bytes and where its parts sit.
    ///
    /// A record longer than the first read is read on only once `claims` accepts its length as its
    /// header gives it, so the caller can make room or refuse.
    pub(crate) fn read_stored<'a>(
        &self,
        ptr: Ptr,
        bytes: &'a mut Vec<u8>,
        links: &mut Vec<Ptr>,
        claims: impl FnOnce(usize) -> Result<()>,
    ) -> Result<(&'a [u8], Stored)> {
        let tail = self.readable_past(ptr.offset)?;
        if ptr.offset < LOG_START || ptr.offset >= tail {
            return Err(Error::Corrupt(format!(
                "pointer {} outside log of {tail} bytes",
                ptr.offset
            )));
        }
        let want = (tail - ptr.offset).min(READ_AHEAD as u64) as usize;
        if bytes.len() < want {
            bytes.resize(want, 0);
        }
        bytes.truncate(want);
        self.inner.file.read_exact_at(bytes, ptr.offset)?;
        if let Some(stored) = stored_within(bytes, ptr.offset, links) {
            return Ok((&bytes[..stored.len], stored));
        }
        let mut reader = RecordReader {
            file: Some(&*self.inner.file),
            start: ptr.offset,
            tail,
            bytes: Cow::Owned(mem::take(bytes)),
            pos: 0,
        };
        let stored = Self::stored_in(&mut reader, links, claims);
        *bytes = reader.bytes.into_owned();
        let stored = stored?;
        Ok((&bytes[..stored.len], stored))
    }

    /// Reads a record that did not fit the first read, once `claims` accepts its length.
    fn stored_in(
        reader: &mut RecordReader<'_>,
        links: &mut Vec<Ptr>,
        claims: impl FnOnce(usize) -> Result<()>,
    ) -> Result<Stored> {
        let header = reader.header()?;
        let least = header.links.saturating_mul(HASH_BYTES + 1);
        claims(
            reader
                .pos
                .saturating_add(header.stored)
                .saturating_add(least),
        )?;
        let start = reader.pos;
        reader.take(header.stored)?;
        let payload = start..reader.pos;
        links.clear();
        links.reserve(header.links.min(1024));
        for _ in 0..header.links {
            links.push(reader.link()?);
        }
        Ok(Stored {
            raw: header.raw.unwrap_or(header.stored),
            packed: header.raw.is_some(),
            payload,
            len: reader.pos,
        })
    }

    /// The links of the record at `ptr`, read without decompressing its body or checking its hash.
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

    /// Reads the record at `ptr`, never past `tail`, from `held`, bytes that start at the record,
    /// or from `file` when `held` is empty, checking its hash if `verify` is set and keeping its
    /// compressed body if `keep` is.
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
        let header = reader.header()?;
        let mut packed = None;
        let body = match header.raw {
            Some(raw_len) => {
                let stored = reader.take(header.stored)?;
                let body = lz4_flex::block::decompress(stored, raw_len).map_err(|error| {
                    Error::Corrupt(format!("bad compressed body at {}: {error}", ptr.offset))
                })?;
                if body.len() != raw_len {
                    return Err(wrong_length(ptr.offset));
                }
                packed = keep.then(|| stored.to_vec());
                body
            }
            None => reader.take(header.stored)?.to_vec(),
        };
        let mut links = Vec::with_capacity(header.links.min(1024));
        for _ in 0..header.links {
            links.push(reader.link()?);
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

    /// The value at `ptr`, from the cache or decoded with `decode` and cached, and whether it was
    /// in the cache.
    ///
    /// `weigh` gives a decoded value's weight from the value, its body's length and the heap its
    /// decode counted.
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

    /// [`Store::fetch`] for a tree node, which the cache holds as an [`Erased`] in its own entry,
    /// lending it to readers while it does, and which a hit hands out with the count its entry's
    /// clone took.
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

    /// Reads and decodes the object at `ptr`, registers it for deduplication if it is as large as a
    /// spilled value and the store writes, and weighs it.
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

    /// Puts the entry that `entry` makes in the cache unless the store is closed, and clears the
    /// cache again if a close came meanwhile, so that a closed store's cache stays empty.
    fn admit(&self, ptr: Ptr, entry: impl FnOnce() -> CacheEntry) {
        if !self.inner.closed.load(Ordering::SeqCst) {
            self.inner.cache.insert(ptr.hash, entry());
            if self.inner.closed.load(Ordering::SeqCst) {
                self.inner.cache.clear();
            }
        }
    }

    /// Closes the store: drops the cache, flushes the epoch, and for a writer truncates the zeroes
    /// preallocated past the log and lets the writer lock go.
    ///
    /// Clones of the store go on reading what they read before, and a new writer may open the
    /// file meanwhile. Cached and retired nodes that hold nested maps hold the store too, so
    /// dropping the cache and flushing the epoch is what lets the file close with the last clone.
    /// A shared writer truncates nothing, since other writers may be placing frames in the
    /// preallocation, and neither does a poisoned store. A second close does nothing to the file,
    /// so it never truncates under a new writer.
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

    /// The value of type `T` stored at `ptr`, from the cache or read, decoded and cached.
    ///
    /// # Errors
    ///
    /// The errors of [`Store::read_object`], and those of decoding a `T`, such as [`Error::Decode`]
    /// for an object that does not decode as one.
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

    /// [`Store::load`] for a type whose decoded form depends on its object's offset, which decodes
    /// again when the cached copy came from another offset.
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

    /// Reads the object at `ptr` and decodes it with `decode`, past the cache.
    pub(crate) fn decode_at<T>(
        &self,
        ptr: Ptr,
        decode: impl FnOnce(&mut Decoder<'_>) -> Result<T>,
    ) -> Result<T> {
        let object = self.read_object(ptr)?;
        decode(&mut Decoder::new(&object.body, &object.links, Some(self)))
    }

    /// Starts a transaction that appends to the end of the log.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] for a read-only store, and [`Error::Poisoned`] once a write or a sync
    /// has failed.
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

    /// Calls `visit` on every object reachable from `head`, once each, reading each one and
    /// checking its hash.
    ///
    /// # Errors
    ///
    /// The first error of [`Store::read_object`] on the way.
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

/// A hasher for keys that are hashes already: it folds the first 8 bytes of each write into its
/// state.
#[derive(Default)]
pub(crate) struct Prehashed(u64);

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

/// One way of a deduplication set: an object's hash and its offset, which is zero in an empty way.
#[derive(Clone, Copy, Default)]
struct Way {
    hash: Hash,
    offset: [u8; 8],
}

/// A set of the deduplication table, newest way first, behind a lock, on a line of its own.
#[derive(Default)]
#[repr(align(64))]
struct Set(Mutex<[Way; DEDUPE_WAYS]>);

/// The deduplication table: a bounded, two-way set associative map from an object's hash to its
/// offset in the file.
///
/// It holds what this process wrote, and what it read of objects as large as a spilled value, so
/// that a transaction can skip an object the file already holds. Its sets are allocated in chunks
/// as entries first land in them.
struct Dedupe {
    chunks: Box<[OnceLock<Box<[Set]>>]>,
    mask: usize,
}

impl Dedupe {
    /// A table of about `entries` entries, in a power of two sets, and at least one chunk of them.
    fn new(entries: usize) -> Dedupe {
        let sets = (entries / DEDUPE_WAYS)
            .next_power_of_two()
            .max(DEDUPE_CHUNK);
        Dedupe {
            chunks: (0..sets / DEDUPE_CHUNK).map(|_| OnceLock::new()).collect(),
            mask: sets - 1,
        }
    }

    /// The chunk and the set within it of `hash`.
    fn index(&self, hash: &Hash) -> (usize, usize) {
        let mut word = [0u8; 8];
        word.copy_from_slice(&hash[..8]);
        let set = u64::from_le_bytes(word) as usize & self.mask;
        (set / DEDUPE_CHUNK, set % DEDUPE_CHUNK)
    }

    /// The offset recorded for `hash`, which moves to the front of its set.
    fn get(&self, hash: &Hash) -> Option<u64> {
        let (chunk, set) = self.index(hash);
        let mut ways = self.chunks[chunk].get()?[set].0.lock();
        let at = ways.iter().position(|way| way.hash == *hash)?;
        ways[..=at].rotate_right(1);
        Some(u64::from_le_bytes(ways[0].offset)).filter(|offset| *offset != 0)
    }

    /// Records `hash` at `offset` at the front of its set, pushing the older way out of a full one.
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

/// A transaction: the records of one commit, gathered before its frame is placed.
///
/// It lays its records out as if its frame began where the log ended when [`Store::begin`]
/// started it, and skips an object it already holds or that the store's deduplication table finds
/// below that start. Its records wait in memory, or for a large commit are written past the end
/// of the log as they go. [`Txn::commit`] places the frame, writes it and syncs it. A transaction
/// dropped before its frame is placed truncates what it wrote past the log and syncs the file, so
/// that a power cut cannot bring those records back under the next frame written there.
pub struct Txn {
    store: Store,
    /// Where the frame begins.
    origin: u64,
    /// Where the records still in `buf` begin, which is `origin` until the transaction spills.
    start: u64,
    /// The records not yet spilled, after room for the frame's header if none has spilled.
    buf: Vec<u8>,
    /// The offset of every object this transaction wrote or found, by hash.
    seen: Seen,
    /// The cached copies of the nodes this transaction wrote, which go into the cache once its
    /// frame is placed.
    remembered: Vec<Remembered>,
    /// Set once the transaction stops taking offsets from the deduplication table.
    unshared: bool,
    /// The end of the records written before it stopped, which a moved draft looks up again.
    shared_end: u64,
    /// Whether placing the frame grows the [`Durability::Fsync`] preallocation.
    reserve: bool,
}

/// The offset of every object a transaction wrote or found, by hash.
type Seen = HashMap<Hash, u64, BuildHasherDefault<Prehashed>>;

/// A cached copy of a node a transaction wrote: its hash, the cache's entry for it, and, for a
/// node that can be moved with its frame, how to move it and how many links its record has.
type Remembered = (Hash, CacheEntry, Option<(Relocate, usize)>);

/// What a draft carries from its rebase until its frame is placed: how its records moved, the
/// cached copies of the nodes it wrote, and the objects it wrote, for the cache and the
/// deduplication table.
pub(crate) struct Settling {
    moves: Option<Moves>,
    remembered: Vec<Remembered>,
    seen: Seen,
}

impl Settling {
    /// How the draft's records moved, if another frame was placed first.
    pub(crate) fn moves(&self) -> Option<&Moves> {
        self.moves.as_ref()
    }

    /// Takes over the record of what `txn` wrote, for [`Settling::finish`] to register.
    pub(crate) fn take_seen(&mut self, txn: &mut Txn) {
        self.seen = mem::take(&mut txn.seen);
    }

    /// Registers the objects written for deduplication and puts the cached copies in the cache,
    /// moving each node of a moved draft with its records; a copy that cannot be moved is left
    /// out.
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

/// Reads a varint at `at` in a drafted frame.
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

/// Moves `old`, a frame drafted to start at `from`, to start at `to`, record by record, and returns
/// it with the table of where each record went.
///
/// A link to an older object grows by the distance moved, and a link into the draft goes through
/// the table. A record whose object `known` finds already in the file, which a frame placed since
/// may have written, is dropped, and its entry in the table points at that copy. `hashes` gives
/// the hash of every drafted record, in offset order.
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
        } else if inner.file.sync_all().is_err() {
            self.store.poison_unsynced();
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

/// Grows the [`Durability::Fsync`] preallocation past `end` by its next chunk of zeroes, under
/// the placement state's lock, so that frames overwrite blocks already written.
///
/// The first call reserves nothing, and each chunk after it doubles the last, from
/// [`FIRST_RESERVE`] to [`MAX_RESERVE`]. A shared writer takes another's longer file as reserved,
/// and a failed write stops further tries. It does nothing under [`Durability::Buffered`].
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
    /// The pointer of an object with `hash` that is already written: by this transaction, or,
    /// unless it is unshared, below its start in the deduplication table.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] for a body of `raw` bytes that is 4 GiB or more.
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

    /// Stops taking offsets from the deduplication table, as a commit does before it writes its log
    /// entry, and remembers where the records that took them end, so that a moved draft looks up
    /// again only those.
    pub(crate) fn unshared(&mut self) {
        if !self.unshared {
            self.unshared = true;
            self.shared_end = self.start + self.buf.len() as u64;
        }
    }

    /// Leaves the [`Durability::Fsync`] preallocation as it is when the frame is placed.
    pub(crate) fn unreserved(&mut self) {
        self.reserve = false;
    }

    /// Adds `object`, whose hash is `hash`, unless it is already written, and returns its pointer.
    pub(crate) fn put_ref(&mut self, object: &Object, hash: Hash) -> Result<Ptr> {
        if let Some(ptr) = self.known(hash, object.body.len())? {
            return Ok(ptr);
        }
        let packed = compressed(&object.body);
        Ok(self.record(hash, staged(&object.body, packed.as_deref(), &object.links)))
    }

    /// Moves the draft to the end of the log if another frame was placed since it began, and
    /// returns what its placement must settle.
    ///
    /// The caller holds `tip`, so no frame is placed meanwhile.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] if the draft spilled and would have to move, or a drafted record does
    /// not parse.
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

    /// Appends the record of `staged` to the draft and registers its hash.
    fn record(&mut self, hash: Hash, staged: Staged<'_>) -> Ptr {
        let offset = self.start + self.buf.len() as u64;
        append_staged(&mut self.buf, offset, staged);
        self.seen.insert(hash, offset);
        Ptr { hash, offset }
    }

    /// Adds a received record unless its object is already written, and registers where the
    /// object is either way.
    pub(crate) fn put_record(&mut self, hash: Hash, staged: Staged<'_>) -> Result<Ptr> {
        if let Some(ptr) = self.known(hash, staged.raw)? {
            self.seen.insert(hash, ptr.offset);
            return Ok(ptr);
        }
        Ok(self.record(hash, staged))
    }

    /// Adds a received record unless its object is already written, without registering its hash,
    /// as a received commit is placed, so that no later record of this transaction finds it.
    pub(crate) fn put_unlisted(&mut self, hash: Hash, staged: Staged<'_>) -> Result<Ptr> {
        if let Some(ptr) = self.known(hash, staged.raw)? {
            return Ok(ptr);
        }
        let offset = self.start + self.buf.len() as u64;
        append_staged(&mut self.buf, offset, staged);
        Ok(Ptr { hash, offset })
    }

    /// Adds `object`, whose hash is `hash`, without looking for a copy already written.
    pub(crate) fn put_fresh(&mut self, object: Object, hash: Hash) -> Result<Ptr> {
        let packed = compressed(&object.body);
        Ok(self.record(hash, staged(&object.body, packed.as_deref(), &object.links)))
    }

    /// How many bytes of the draft wait in memory.
    pub(crate) fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The offset of the object with `hash` that this transaction wrote or found.
    pub(crate) fn offset_of(&self, hash: &Hash) -> Option<u64> {
        self.seen.get(hash).copied()
    }

    /// Writes the records waiting in memory past the end of the log, so that a large commit need
    /// not hold them all, and goes on from there. A draft that has spilled cannot move.
    pub(crate) fn spill(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.store.inner.file.write_all_at(&self.buf, self.start)?;
        self.start += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }

    /// Reads the object at `ptr` from this transaction's records or the log. It checks the hash
    /// only of an object below the transaction's start, since its own records were hashed as they
    /// were added.
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

    /// Feeds the records spilled so far, past the room for the frame's header, to `blocks`.
    fn hash_spilled(&self, blocks: &mut Blocks) -> Result<()> {
        let mut at = self.origin + FRAME_BYTES as u64;
        let mut chunk = vec![0u8; SCAN_CHUNK.min((self.start - at) as usize)];
        while at < self.start {
            let take = chunk.len().min((self.start - at) as usize);
            self.store
                .inner
                .file
                .read_exact_at(&mut chunk[..take], at)?;
            blocks.feed(&chunk[..take]);
            at += take as u64;
        }
        Ok(())
    }

    /// Places the frame at the end of the log, naming `head`: gives it the next sequence number,
    /// moves `placed` past it, registers the objects it still holds for deduplication and puts the
    /// nodes it still holds in the cache, those a [`Settling`] has not taken, and returns the
    /// [`Landing`] that writes it.
    ///
    /// A transaction that spilled writes the rest of its frame and its header here, and one that
    /// did not is written by [`Landing::land`] once the caller has let `tip` go. The caller holds
    /// `tip` and has rebased the draft.
    ///
    /// # Errors
    ///
    /// [`Error::Poisoned`] once a write has failed, [`Error::Corrupt`] if the frame no longer
    /// starts at the end of the log or the log has used its last sequence number, and the I/O
    /// error of a failed write of a spilled frame, which poisons the store.
    pub(crate) fn place(mut self, head: Ptr) -> Result<Landing> {
        let store = self.store.clone();
        let inner = &store.inner;
        let body_start = self.origin + FRAME_BYTES as u64;
        let records = self.start + self.buf.len() as u64 - body_start;
        let end = body_start + records + table_len(records);
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
            let mut blocks = Blocks::new();
            self.hash_spilled(&mut blocks)?;
            blocks.feed(&self.buf);
            let table = blocks.finish();
            let mut header = frame.header();
            let check = frame_check(&header, &table);
            header[FRAME_CHECKED..FRAME_BYTES].copy_from_slice(&check);
            self.buf.extend_from_slice(&table);
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

    /// Places the frame, naming `head`, and writes it at once, as the frames written under `tip`
    /// are.
    pub(crate) fn write(self, head: Ptr) -> Result<Frame> {
        let landing = self.place(head)?;
        let frame = landing.frame;
        landing.land()?;
        Ok(frame)
    }

    /// Places, writes and syncs the frame, naming `head` as the head the next open returns, and
    /// publishes it.
    ///
    /// # Errors
    ///
    /// [`Error::Poisoned`] once a write or a sync has failed, and the I/O error of a failed write
    /// or sync, which poisons the store.
    pub fn commit(self, head: Ptr) -> Result<()> {
        let store = self.store.clone();
        let frame = self.write(head)?;
        store.sync(frame)
    }
}

/// A placed frame not yet written, which a commit writes once it has let `tip` go.
///
/// Dropping one that was never written poisons the store at the frame, since no frame placed
/// after it could be kept.
pub(crate) struct Landing {
    store: Store,
    frame: Frame,
    /// Where the frame starts.
    at: u64,
    /// The frame, header first, when it is written here.
    buf: Vec<u8>,
    /// Set until the frame is written, when it is written here.
    pending: bool,
}

impl Landing {
    /// The placed frame.
    pub(crate) fn frame(&self) -> Frame {
        self.frame
    }

    /// Writes the frame, with its table and check, to the page cache and moves `written` over it,
    /// or does nothing for a frame [`Txn::place`] wrote. A failed write poisons the store at the
    /// frame.
    pub(crate) fn land(mut self) -> Result<()> {
        if !self.pending {
            return Ok(());
        }
        self.pending = false;
        let records = self.buf.len();
        append_table(&mut self.buf, FRAME_BYTES);
        let mut header = self.frame.header();
        let check = frame_check(&header, &self.buf[records..]);
        header[FRAME_CHECKED..FRAME_BYTES].copy_from_slice(&check);
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

/// The counter that names scratch files.
static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

/// Opens a new scratch file beside `near`, named for this process and a counter, that deletes
/// itself.
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

/// The records of a received bundle, held until it lands: in memory up to `limit` bytes, and past
/// that in a scratch file beside the database that deletes itself.
///
/// Each record sits behind its hash, in the log's record format, with its links resolved to
/// offsets, within the staging for objects sent earlier in the bundle. Each object gets a `u32`
/// id in the order it was put, with its offset in the staging beside it. Offsets start at
/// [`LOG_START`], as in a log.
pub(crate) struct Staging {
    near: PathBuf,
    limit: usize,
    file: Option<File>,
    /// Where the records still in `buf` begin, past those already spilled.
    start: u64,
    buf: Vec<u8>,
    hashes: Hashes,
    offsets: Column<u64>,
}

/// A record of a staging, read back for placing.
pub(crate) struct Record {
    /// The object's hash.
    pub(crate) hash: Hash,
    /// The body's length once uncompressed.
    pub(crate) raw: usize,
    /// Whether the payload is LZ4 compressed.
    pub(crate) packed: bool,
    /// The body as stored.
    pub(crate) payload: Vec<u8>,
    /// The links, as resolved when the record was staged.
    pub(crate) links: Vec<Ptr>,
}

impl Record {
    /// The record's parts to append, with `links` in place of its own.
    pub(crate) fn staged<'a>(&'a self, links: &'a [Ptr]) -> Staged<'a> {
        Staged {
            raw: self.raw,
            payload: &self.payload,
            packed: self.packed,
            links,
        }
    }
}

/// Reads a varint at `pos` in `bytes`, or `None` if it runs past them, failing with
/// `Error::Corrupt` on one that does not fit a `u64`.
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

/// Reads the staged record at the front of `bytes`, which start at `at` in the staging, and its
/// length, or `None` if it runs past them.
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
    /// An empty staging, which spills to a scratch file beside `near` once it holds `limit` bytes.
    pub(crate) fn new(near: &Path, limit: usize) -> Staging {
        Staging {
            near: near.to_path_buf(),
            limit,
            file: None,
            start: LOG_START,
            buf: Vec::new(),
            hashes: Hashes::new(),
            offsets: Column::new(),
        }
    }

    /// Adds the record of `staged` behind `hash`, spilling what is in memory first if the record
    /// could take it past the limit, and returns the object's id.
    ///
    /// The caller must know that `hash` is not staged yet, as [`Hashes::insert`] requires.
    ///
    /// # Errors
    ///
    /// [`Error::Bundle`] once the staging holds as many objects as it has ids, and the I/O error
    /// of a failed spill.
    pub(crate) fn put(&mut self, hash: Hash, staged: Staged<'_>) -> Result<u32> {
        let links = staged.links.len() * (HASH_BYTES + MAX_VARINT);
        let most = HASH_BYTES + staged.payload.len() + 3 * MAX_VARINT + links;
        if !self.buf.is_empty() && self.buf.len() + most > self.limit {
            self.spill()?;
        }
        let id = self.hashes.insert(hash).ok_or_else(|| {
            Error::Bundle("it carries more objects than one receive can hold".into())
        })?;
        self.buf.extend_from_slice(&hash);
        let offset = self.start + self.buf.len() as u64;
        append_staged(&mut self.buf, offset, staged);
        self.offsets.push(offset);
        Ok(id)
    }

    /// The id of the object with `hash`.
    pub(crate) fn id_of(&self, hash: &Hash) -> Option<u32> {
        self.hashes.find(hash)
    }

    /// The offset in the staging of the object with the id `id`.
    ///
    /// # Panics
    ///
    /// If no object has that id, or the staging is sealed.
    pub(crate) fn offset(&self, id: u32) -> u64 {
        self.offsets.get(id)
    }

    /// The offset in the staging of the object with `hash`.
    ///
    /// # Panics
    ///
    /// If the staging is sealed and holds the object.
    pub(crate) fn offset_of(&self, hash: &Hash) -> Option<u64> {
        self.id_of(hash).map(|id| self.offset(id))
    }

    /// The table of every object's hash and id.
    pub(crate) fn hashes(&self) -> &Hashes {
        &self.hashes
    }

    /// Whether any records went to the scratch file.
    pub(crate) fn spilled(&self) -> bool {
        self.file.is_some()
    }

    /// Writes the records in memory to the scratch file, creating it on the first spill.
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

    /// Reads the object at `ptr`, an offset in the staging, without checking its hash, which was
    /// checked as it arrived.
    pub(crate) fn read_object(&self, ptr: Ptr) -> Result<Object> {
        let end = self.start + self.buf.len() as u64;
        let (held, tail) = match ptr.offset.checked_sub(self.start) {
            Some(at) if ptr.offset < end => (&self.buf[at as usize..], end),
            _ => (&[][..], self.start),
        };
        let (object, _, _) = Store::read_within(self.file.as_ref(), ptr, tail, held, false, false)?;
        Ok(object)
    }

    /// Drops the offsets, which placing does not need, and if the staging spilled, writes the rest
    /// to the scratch file and frees the buffer, so that a replay reads only the file.
    pub(crate) fn seal(&mut self) -> Result<()> {
        self.offsets = Column::new();
        if self.spilled() {
            self.spill()?;
            self.buf = Vec::new();
        }
        Ok(())
    }

    /// A reader of the records in the order they were put.
    pub(crate) fn replay(&self) -> Replay<'_> {
        Replay {
            staging: self,
            at: LOG_START,
            window: Vec::new(),
            window_at: LOG_START,
        }
    }
}

/// A reader of a staging's records in the order they were put.
pub(crate) struct Replay<'a> {
    staging: &'a Staging,
    /// The offset of the next record.
    at: u64,
    /// The bytes read of the scratch file.
    window: Vec<u8>,
    /// Where `window` starts.
    window_at: u64,
}

impl Replay<'_> {
    /// The next record, or `None` after the last.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] if a record runs past the end of the staging or does not parse, and the
    /// I/O error of a failed read of the scratch file.
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

    /// Reads the record at `at` from the scratch file, reading a window at least [`REPLAY_CHUNK`]
    /// long and doubling it until the record fits.
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

    #[test]
    fn a_frames_length_gives_back_the_records_it_holds() {
        for records in (0..20_000u64).chain([1 << 20, (1 << 30) + 1]) {
            let body = records + table_len(records);
            assert_eq!(
                records_in(body),
                Some(records),
                "{records} bytes of records"
            );
        }
        let block = (BLOCK_BYTES + BLOCK_CHECK) as u64;
        for blocks in 0..5u64 {
            for over in 1..=BLOCK_CHECK as u64 {
                assert_eq!(records_in(blocks * block + over), None);
            }
        }
    }

    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn written(objects: &[Object], spill_after: Option<usize>) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.ferro");
        let options = StoreOptions {
            durability: Durability::Buffered,
            ..StoreOptions::default()
        };
        let (store, _) = Store::open(&path, options).unwrap();
        let mut txn = store.begin().unwrap();
        let mut head = None;
        for (index, object) in objects.iter().enumerate() {
            head = Some(txn.put_ref(object, object.hash()).unwrap());
            if spill_after == Some(index) {
                txn.spill().unwrap();
            }
        }
        txn.commit(head.unwrap()).unwrap();
        store.close();
        std::fs::read(&path).unwrap()
    }

    #[test]
    fn a_frame_that_spilled_is_the_frame_written_whole() {
        let objects: Vec<Object> = (0..40)
            .map(|seed| Object {
                body: noise(seed, 700 + seed as usize * 13),
                links: Vec::new(),
            })
            .collect();
        let whole = written(&objects, None);
        assert!(whole.len() > LOG_START as usize + 4 * BLOCK_BYTES);
        for spill_after in [0, 6, 17, 38] {
            assert_eq!(
                written(&objects, Some(spill_after)),
                whole,
                "spilled after {spill_after}"
            );
        }
    }

    #[test]
    fn a_flipped_bit_counts_as_damage_only_in_a_sector_that_holds_enough_of_the_frame() {
        let object = Object {
            body: noise(3, 436),
            links: Vec::new(),
        };
        let bytes = written(&[object], None);
        let start = LOG_START;
        let end = bytes.len() as u64;
        assert_eq!(
            end % SECTOR_BYTES,
            3,
            "the frame ends 3 bytes into a sector"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("judged.ferro");
        let empty = Mark {
            seq: 0,
            log_end: start,
            head: None,
        };
        let tip = Super::new(empty, empty, Basis::default());
        let judge = |at: u64| {
            let mut image = bytes.clone();
            image[at as usize] ^= 0x20;
            std::fs::write(&path, &image).unwrap();
            let file = File::open(&path).unwrap();
            let lenient = read_frame(&file, &tip, end, false).unwrap();
            assert!(matches!(lenient, Found::Torn), "byte {at} without judging");
            read_frame(&file, &tip, end, true).unwrap()
        };
        std::fs::write(&path, &bytes).unwrap();
        let file = File::open(&path).unwrap();
        assert!(matches!(
            read_frame(&file, &tip, end, true).unwrap(),
            Found::Whole(_)
        ));
        let table = end - BLOCK_CHECK as u64;
        for at in [start, start + 20, start + 60, start + 100, table, end - 4] {
            assert!(
                matches!(judge(at), Found::Damaged(found) if found == at),
                "byte {at}"
            );
        }
        for at in end - 3..end {
            assert!(matches!(judge(at), Found::Torn), "byte {at}");
        }
    }

    /// An object to write in a frame: stored raw whatever compression would make of it, and
    /// followed by a spill.
    struct Put {
        object: Object,
        raw: bool,
        spill: bool,
    }

    fn put(body: Vec<u8>) -> Put {
        Put {
            object: Object {
                body,
                links: Vec::new(),
            },
            raw: false,
            spill: false,
        }
    }

    fn raw(body: Vec<u8>) -> Put {
        Put {
            raw: true,
            ..put(body)
        }
    }

    fn spilled(put: Put) -> Put {
        Put { spill: true, ..put }
    }

    /// The file a fresh store held and the writes it made, one frame for each of `frames`.
    fn traced(frames: &[Vec<Put>], durability: Durability) -> (Vec<u8>, Vec<Op>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("traced.ferro");
        drop(Store::open(&path, StoreOptions::default()).unwrap());
        let base = std::fs::read(&path).unwrap();
        let trace: Trace = Arc::new(Mutex::new(Vec::new()));
        let options = StoreOptions {
            durability,
            trace: Some(trace.clone()),
            ..StoreOptions::default()
        };
        let (store, _) = Store::open(&path, options).unwrap();
        for frame in frames {
            let mut txn = store.begin().unwrap();
            let mut head = None;
            for put in frame {
                let hash = put.object.hash();
                let ptr = match put.raw {
                    true => txn.record(hash, staged(&put.object.body, None, &put.object.links)),
                    false => txn.put_ref(&put.object, hash).unwrap(),
                };
                head = Some(ptr);
                if put.spill {
                    txn.spill().unwrap();
                }
            }
            txn.commit(head.unwrap()).unwrap();
        }
        store.close();
        let ops = trace.lock().clone();
        (base, ops)
    }

    /// The image of `base` once `ops` are applied, every write but the bytes `lost` picks from
    /// it, which keep what they held or become `older`.
    fn applied(
        base: &[u8],
        ops: &[Op],
        mut lost: impl FnMut(usize, u64) -> bool,
        older: Option<u8>,
    ) -> Vec<u8> {
        let mut image = base.to_vec();
        for (index, op) in ops.iter().enumerate() {
            match op {
                Op::Write(_, at, bytes) if *at >= LOG_START => {
                    let end = *at as usize + bytes.len();
                    if image.len() < end {
                        image.resize(end, 0);
                    }
                    let mut from = *at;
                    while from < end as u64 {
                        let to = (from - from % SECTOR_BYTES + SECTOR_BYTES).min(end as u64);
                        let (a, b) = (from as usize, to as usize);
                        match (lost(index, from - from % SECTOR_BYTES), older) {
                            (false, _) => image[a..b]
                                .copy_from_slice(&bytes[a - *at as usize..b - *at as usize]),
                            (true, Some(fill)) => image[a..b].fill(fill),
                            (true, None) => {}
                        }
                        from = to;
                    }
                }
                Op::SetLen(_, len) => image.resize(*len as usize, 0),
                _ => {}
            }
        }
        image
    }

    /// What a judging scan from the start of the log makes of `image`.
    fn scanned(path: &Path, image: &[u8]) -> Result<Super> {
        std::fs::write(path, image).unwrap();
        let file = File::open(path).unwrap();
        let empty = Mark {
            seq: 0,
            log_end: LOG_START,
            head: None,
        };
        scan(
            &file,
            Super::new(empty, empty, Basis::default()),
            image.len() as u64,
            true,
        )
    }

    /// Checks that no image of `frames` torn by a power cut reads as damaged: with each sector
    /// of each write lost in turn, and with each kept by chance, the lost bytes keeping what they
    /// held or becoming `older`. Returns how many images ended the log early.
    fn never_damaged(frames: &[Vec<Put>], older: Option<u8>) -> usize {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("torn.ferro");
        let (base, ops) = traced(frames, Durability::Buffered);
        let whole = applied(&base, &ops, |_, _| false, older);
        let last = scanned(&path, &whole).unwrap().seq;
        assert_eq!(last, frames.len() as u64);
        let mut cuts = Vec::new();
        for (index, op) in ops.iter().enumerate() {
            if let Op::Write(_, at, bytes) = op
                && *at >= LOG_START
            {
                let mut sector = at - at % SECTOR_BYTES;
                while sector < at + bytes.len() as u64 {
                    cuts.push((index, sector));
                    sector += SECTOR_BYTES;
                }
            }
        }
        let mut short = 0;
        let mut judge = |image: Vec<u8>, what: &str| match scanned(&path, &image) {
            Ok(tip) => short += usize::from(tip.seq < last),
            Err(error) => panic!("{what}: {error}"),
        };
        for &(index, sector) in &cuts {
            let image = applied(&base, &ops, |at, from| (at, from) == (index, sector), older);
            judge(image, &format!("sector {sector} of write {index} lost"));
        }
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for round in 0..200 {
            let image = applied(
                &base,
                &ops,
                |_, _| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed.is_multiple_of(2)
                },
                older,
            );
            judge(image, &format!("round {round} of sectors kept by chance"));
        }
        short
    }

    /// A frame's puts by name, with the byte a lost sector turns to, if not the one it held.
    type Case = (&'static str, Vec<Vec<Put>>, Option<u8>);

    /// `len` bytes of `fill`, but for one bit flipped in every `every` bytes, from `first` on.
    fn sparse(len: usize, fill: u8, first: usize, every: usize) -> Vec<u8> {
        let mut body = vec![fill; len];
        for (index, at) in (first..len).step_by(every).enumerate() {
            body[at] ^= 1 << (index % 8);
        }
        body
    }

    #[test]
    fn a_sector_left_with_the_zeroes_past_a_spill_reads_as_torn() {
        let mut frame = vec![spilled(put(noise(1, 4536))), put(noise(2, 64))];
        frame.extend((3..8).map(|seed| put(noise(seed, 3000))));
        assert!(compressed(&frame[0].object.body).is_none());
        let spill_end = LOG_START + FRAME_BYTES as u64 + 4539;
        assert_eq!(spill_end % SECTOR_BYTES, SECTOR_BYTES - 1);
        let (base, ops) = traced(&[frame], Durability::Buffered);
        let whole = applied(&base, &ops, |_, _| false, None);
        assert_eq!(
            whole[spill_end as usize], 0x40,
            "a record whose first byte has one bit"
        );
        let next = ops
            .iter()
            .position(|op| matches!(op, Op::Write(_, at, _) if *at == spill_end))
            .unwrap();
        let torn = applied(
            &base,
            &ops,
            |at, sector| at == next && sector == spill_end - spill_end % SECTOR_BYTES,
            None,
        );
        assert_eq!(torn[spill_end as usize], 0);
        let dir = tempfile::tempdir().unwrap();
        let tip = scanned(&dir.path().join("spill.ferro"), &torn).unwrap();
        assert_eq!(tip.seq, 0);
    }

    #[test]
    fn no_torn_frame_of_zeroes_or_ones_with_a_bit_set_here_and_there_reads_as_damaged() {
        let cases: [Case; 6] = [
            (
                "noise",
                vec![(1..9).map(|seed| put(noise(seed, 1500))).collect()],
                None,
            ),
            (
                "a spill ending a sector",
                vec![{
                    let mut frame = vec![spilled(put(noise(1, 4536))), put(noise(2, 64))];
                    frame.extend((3..8).map(|seed| put(noise(seed, 3000))));
                    frame
                }],
                None,
            ),
            (
                "zeroes",
                vec![vec![
                    raw(sparse(9000, 0, 17, 701)),
                    spilled(raw(sparse(5000, 0, 3, 523))),
                    raw(sparse(7000, 0, 300, 1111)),
                ]],
                None,
            ),
            (
                "zeroes over ones",
                vec![vec![
                    raw(sparse(9000, 0, 17, 701)),
                    spilled(raw(sparse(5000, 0, 3, 523))),
                ]],
                Some(0xff),
            ),
            (
                "ones",
                vec![vec![
                    raw(sparse(9000, 0xff, 5, 613)),
                    spilled(raw(sparse(6000, 0xff, 91, 997))),
                ]],
                Some(0xff),
            ),
            (
                "ones over zeroes",
                vec![vec![
                    raw(sparse(9000, 0xff, 5, 613)),
                    spilled(raw(sparse(6000, 0xff, 91, 997))),
                ]],
                None,
            ),
        ];
        for (name, frames, older) in &cases {
            let short = never_damaged(frames, *older);
            assert!(
                short > 100,
                "{name}: only {short} torn images ended the log early"
            );
        }
    }

    #[test]
    fn a_header_written_after_its_records_and_lost_past_a_sector_reads_as_torn() {
        let before = || vec![put(noise(0, 366))];
        let mut found = None;
        for seed in 1..2000 {
            let frames = [
                before(),
                vec![spilled(put(noise(seed, 3000))), put(noise(seed + 1, 900))],
            ];
            let (base, ops) = traced(&frames, Durability::Buffered);
            let whole = applied(&base, &ops, |_, _| false, None);
            let start = LOG_START as usize + 445;
            assert_eq!((start + FRAME_BYTES - 1) % SECTOR_BYTES as usize, 0);
            if whole[start + FRAME_BYTES - 1].count_ones() == 1 {
                found = Some((frames, base, ops, start));
                break;
            }
        }
        let (frames, base, ops, start) = found.expect("a check whose last byte has one bit");
        let header = ops
            .iter()
            .position(|op| matches!(op, Op::Write(_, at, bytes) if *at == start as u64 && bytes.len() == FRAME_BYTES))
            .expect("the header is written on its own, after the records");
        let last = (start + FRAME_BYTES - 1) as u64;
        let torn = applied(
            &base,
            &ops,
            |at, sector| at == header && sector == last,
            None,
        );
        assert_eq!(torn[last as usize], 0);
        let dir = tempfile::tempdir().unwrap();
        let tip = scanned(&dir.path().join("header.ferro"), &torn).unwrap();
        assert_eq!(tip.seq, 1);
        never_damaged(&frames, None);
    }

    #[test]
    fn a_frame_failing_more_blocks_than_open_searches_reads_as_torn() {
        let frame: Vec<Put> = (0..12).map(|seed| put(noise(seed, BLOCK_BYTES))).collect();
        let (base, ops) = traced(&[frame], Durability::Buffered);
        let whole = applied(&base, &ops, |_, _| false, None);
        let body = LOG_START as usize + FRAME_BYTES;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cap.ferro");
        for blocks in [1, MEND_BLOCKS, MEND_BLOCKS + 1, 12] {
            let mut image = whole.clone();
            for block in 0..blocks {
                image[body + block * BLOCK_BYTES + 1000] ^= 0x04;
            }
            match scanned(&path, &image) {
                Err(Error::Corrupt(message)) if blocks <= MEND_BLOCKS => {
                    assert!(
                        message.ends_with(&format!("byte {}", body + 1000)),
                        "{message}"
                    );
                }
                Ok(tip) if blocks > MEND_BLOCKS => assert_eq!(tip.seq, 0),
                other => panic!("{blocks} blocks flipped: {:?}", other.map(|tip| tip.seq)),
            }
        }
    }

    #[test]
    fn a_draft_dropped_after_it_spilled_truncates_and_syncs_what_it_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dropped.ferro");
        let trace: Trace = Arc::new(Mutex::new(Vec::new()));
        let options = StoreOptions {
            trace: Some(trace.clone()),
            ..StoreOptions::default()
        };
        let (store, _) = Store::open(&path, options).unwrap();
        let mut txn = store.begin().unwrap();
        let object = Object {
            body: noise(9, 5000),
            links: Vec::new(),
        };
        txn.put_ref(&object, object.hash()).unwrap();
        txn.spill().unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > LOG_START);
        drop(txn);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), LOG_START);
        let ops = trace.lock().clone();
        let cut = ops
            .iter()
            .position(|op| matches!(op, Op::SetLen(_, len) if *len == LOG_START))
            .expect("the spilled records are truncated");
        assert!(
            matches!(ops.get(cut + 1), Some(Op::Sync(_))),
            "the truncation is synced"
        );
    }

    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[test]
    fn a_read_only_open_trusts_the_superblock_while_the_placement_lock_is_held() {
        let (base, ops) = traced(&[vec![put(noise(4, 2000))]], Durability::Buffered);
        let mut image = applied(&base, &ops, |_, _| false, None);
        image[LOG_START as usize + FRAME_BYTES + 1000] ^= 0x10;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("placing.ferro");
        std::fs::write(&path, &image).unwrap();
        let read_only = || {
            let options = StoreOptions {
                read_only: true,
                ..StoreOptions::default()
            };
            Store::open(&path, options).map(|(store, head)| (store.last_seq(), head))
        };
        assert!(matches!(read_only(), Err(Error::Corrupt(_))));
        let writer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        platform::lock_range(&writer, COMMIT_AT, 1, true, false).unwrap();
        assert!(matches!(read_only(), Ok((0, None))));
        platform::unlock_range(&writer, COMMIT_AT, 1).unwrap();
        assert!(matches!(read_only(), Err(Error::Corrupt(_))));
        assert_eq!(std::fs::read(&path).unwrap(), image);
    }

    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[test]
    fn shared_writers_that_open_together_after_a_crash_roll_a_buffered_log_back_past_its_anchor() {
        let frames: Vec<Vec<Put>> = (0..3).map(|seed| vec![put(noise(seed, 3000))]).collect();
        let (base, ops) = traced(&frames, Durability::Buffered);
        let mut image = applied(&base, &ops, |_, _| false, None);
        let (at, slot) = ops
            .iter()
            .rev()
            .find_map(|op| match op {
                Op::Write(_, at, bytes) if *at < LOG_START => Some((*at as usize, bytes)),
                _ => None,
            })
            .unwrap();
        image[at..at + slot.len()].copy_from_slice(slot);
        let named = Super::decode(&image[at..]).unwrap();
        assert_eq!((named.seq, named.anchor.seq), (3, 0));
        let second = LOG_START as usize + 16;
        let second = u64::from_le_bytes(image[second..second + 8].try_into().unwrap()) as usize;
        image[second + FRAME_BYTES + 200] ^= 0x01;
        image[second + FRAME_BYTES + 201] ^= 0x01;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("together.ferro");
        std::fs::write(&path, &image).unwrap();
        let placing = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        platform::lock_range(&placing, COMMIT_AT, 1, true, false).unwrap();
        let options = StoreOptions {
            durability: Durability::Buffered,
            shared: true,
            ..StoreOptions::default()
        };
        let openers: Vec<_> = (0..2)
            .map(|_| {
                let (path, options) = (path.clone(), options.clone());
                thread::spawn(move || Store::open(&path, options).map(|(store, _)| store))
            })
            .collect();
        thread::sleep(Duration::from_millis(100));
        platform::unlock_range(&placing, COMMIT_AT, 1).unwrap();
        for opener in openers {
            let store = opener.join().unwrap().unwrap();
            assert_eq!(
                store.last_seq(),
                1,
                "the log was not rolled back to its first frame"
            );
            store.close();
        }
    }
}
