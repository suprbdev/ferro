//! Checking the hashes of the records a copy reads many at once.
//!
//! BLAKE3 compresses each 64-byte block of an input after the one before, so a record checked on
//! its own waits on every block in turn. [`Checks`] queues the bytes the hash of each record is
//! taken over, and once it holds enough it compresses the blocks of many records together through
//! `blake3::platform::Platform::hash_many`, as BLAKE3 compresses the chunks of one long input: the
//! chunks of every queued record that share a chunk counter and a number of blocks go through it
//! 16 at a time, the last block of each chunk follows on its own, and each record's chunks are then
//! joined into its hash through the parent nodes BLAKE3 would build. Each check gives the hash
//! [`hash_parts`](crate::codec::hash_parts) gives.
//!
//! A caller that queues a record goes on before its hash is checked, and may follow its links.
//! [`Checks::check`] reports the records in the order they were queued, and its error names the
//! first whose hash differs, so a caller that checks what it holds before it returns any error of
//! its own reports the error it would have met checking each record as it read it.

use blake3::IncrementCounter;
use blake3::platform::Platform;

use crate::codec::{HASH_BYTES, Ptr, hash_filled};
use crate::error::{Error, Result};
use crate::store::{Stored, fill_body};

/// The words BLAKE3 starts the chaining value of every chunk and parent from, in unkeyed hashing.
const IV: [u32; 8] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
];
/// BLAKE3's flag for the first block of a chunk.
const CHUNK_START: u8 = 1 << 0;
/// BLAKE3's flag for the last block of a chunk.
const CHUNK_END: u8 = 1 << 1;
/// BLAKE3's flag for a parent node, which joins the chaining values of two subtrees.
const PARENT: u8 = 1 << 2;
/// BLAKE3's flag for the compression that gives the hash of the whole input.
const ROOT: u8 = 1 << 3;
/// The most inputs one call compresses at once, the widest SIMD degree BLAKE3 has.
const WIDE: usize = 16;
/// The blocks of a BLAKE3 chunk.
const BLOCKS: usize = blake3::CHUNK_LEN / blake3::BLOCK_LEN;
/// The most chunks a queued record's hashed bytes may take; a larger record is hashed at once.
const CHUNKS: usize = 16;
/// The kinds of chunk compressed together under one counter: the last chunk of a record by its
/// number of blocks, and a whole chunk that is not a record's last.
const KINDS: usize = BLOCKS + 1;
/// The most bytes [`Checks`] keeps for the body of a record hashed at once.
const SPILL_KEEP: usize = 1 << 20;

/// The hashes of records a copy has read, queued to be checked many at once.
///
/// [`Checks::queue`] fills in the bytes a record's hash is taken over and hashes at once a record
/// whose bytes take more than 16 chunks, 16 KiB. [`Checks::check`] checks every record still
/// queued, and [`Checks::full`] says when the queued bytes have reached the capacity it was made
/// with.
///
/// `blake3::platform` is public for the crate's benchmarks and not part of its documented API,
/// so ferro depends on one exact version of `blake3`, and the unit tests check the hashes against
/// [`hash_parts`](crate::codec::hash_parts) on every SIMD level the machine has.
pub(super) struct Checks {
    platform: Platform,
    capacity: usize,
    bytes: Vec<u8>,
    queued: Vec<Queued>,
    spill: Vec<u8>,
    cvs: Vec<[u32; 8]>,
    tasks: Vec<Vec<Task>>,
}

/// A record queued in [`Checks`]: where its hashed bytes start, how many there are, and the
/// pointer it was read by.
struct Queued {
    at: usize,
    total: usize,
    ptr: Ptr,
}

/// One chunk of a queued record to compress: where it starts, the chaining value it gives, and
/// the length of its last block.
#[derive(Clone, Copy)]
struct Task {
    at: usize,
    cv: usize,
    len: u8,
}

impl Checks {
    /// An empty queue for the CPU this runs on, full once it holds `capacity` bytes.
    pub(super) fn new(capacity: usize) -> Checks {
        Checks::on(Platform::detect(), capacity)
    }

    /// An empty queue that hashes through `platform`, full once it holds `capacity` bytes.
    fn on(platform: Platform, capacity: usize) -> Checks {
        Checks {
            platform,
            capacity,
            bytes: Vec::new(),
            queued: Vec::new(),
            spill: Vec::new(),
            cvs: Vec::new(),
            tasks: (0..CHUNKS * KINDS).map(|_| Vec::new()).collect(),
        }
    }

    /// Whether no record is queued.
    pub(super) fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    /// Whether the queued bytes have reached the capacity, so that the caller should check them.
    pub(super) fn full(&self) -> bool {
        self.bytes.len() >= self.capacity
    }

    /// Whether [`Checks::queue`] queues a record whose body takes `raw` bytes once decompressed
    /// and which has `links` links, rather than checking it at once.
    ///
    /// A record it does not queue is decompressed into a buffer as long as its body, so a caller
    /// that may have reached the record through a damaged link checks what it holds first.
    pub(super) fn queues(raw: usize, links: usize) -> bool {
        hashed(raw, links) <= CHUNKS * blake3::CHUNK_LEN
    }

    /// Queues the check that `record`, the bytes of the record at `ptr` found to hold `stored`
    /// and `links`, holds an object whose hash is `ptr`'s, decompressing a compressed body
    /// straight into the bytes the hash is taken over.
    ///
    /// Returns `true` for a record too large to queue, which it checked at once, and `false` for
    /// one it queued.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] if the body does not decompress to its length, which queues nothing,
    /// and for a record checked at once whose hash differs.
    pub(super) fn queue(
        &mut self,
        record: &[u8],
        stored: &Stored,
        links: &[Ptr],
        ptr: Ptr,
    ) -> Result<bool> {
        let total = hashed(stored.raw, links.len());
        if !Checks::queues(stored.raw, links.len()) {
            let hashes = links.iter().map(|link| &link.hash);
            let hash = hash_filled(stored.raw, hashes, &mut self.spill, |body| {
                fill_body(record, stored, ptr.offset, body)
            });
            self.spill.clear();
            self.spill.shrink_to(SPILL_KEEP);
            return matches(hash?, ptr).map(|()| true);
        }
        let at = self.bytes.len();
        let padded = total.div_ceil(blake3::BLOCK_LEN) * blake3::BLOCK_LEN;
        if self.bytes.capacity() < at + padded {
            self.bytes
                .reserve_exact(self.capacity.max(at) + CHUNKS * blake3::CHUNK_LEN - at);
        }
        self.bytes.resize(at + padded, 0);
        let slot = &mut self.bytes[at..at + total];
        slot[..4].copy_from_slice(&(stored.raw as u32).to_le_bytes());
        slot[4..8].copy_from_slice(&(links.len() as u32).to_le_bytes());
        if let Err(error) = fill_body(record, stored, ptr.offset, &mut slot[8..8 + stored.raw]) {
            self.bytes.truncate(at);
            return Err(error);
        }
        for (index, link) in links.iter().enumerate() {
            let from = 8 + stored.raw + index * HASH_BYTES;
            slot[from..from + HASH_BYTES].copy_from_slice(&link.hash);
        }
        self.queued.push(Queued { at, total, ptr });
        Ok(false)
    }

    /// Checks every queued record, calls `passed` with the place in the queue of each whose hash
    /// matches, in the order they were queued, and empties the queue.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] for the first record queued whose hash differs.
    pub(super) fn check(&mut self, mut passed: impl FnMut(usize)) -> Result<()> {
        if self.queued.is_empty() {
            return Ok(());
        }
        self.cvs.clear();
        for queued in &self.queued {
            let base = self.cvs.len();
            let chunks = queued.total.div_ceil(blake3::CHUNK_LEN);
            self.cvs.resize(base + chunks, IV);
            for chunk in 0..chunks - 1 {
                self.tasks[chunk * KINDS + BLOCKS].push(Task {
                    at: queued.at + chunk * blake3::CHUNK_LEN,
                    cv: base + chunk,
                    len: blake3::BLOCK_LEN as u8,
                });
            }
            let last = chunks - 1;
            let rest = queued.total - last * blake3::CHUNK_LEN;
            let blocks = rest.div_ceil(blake3::BLOCK_LEN);
            self.tasks[last * KINDS + blocks - 1].push(Task {
                at: queued.at + last * blake3::CHUNK_LEN,
                cv: base + last,
                len: (rest - (blocks - 1) * blake3::BLOCK_LEN) as u8,
            });
        }
        for (index, tasks) in self.tasks.iter_mut().enumerate() {
            if tasks.is_empty() {
                continue;
            }
            let counter = (index / KINDS) as u64;
            let kind = index % KINDS;
            compress(
                &self.platform,
                counter,
                kind,
                tasks,
                &self.bytes,
                &mut self.cvs,
            );
            tasks.clear();
        }
        let mut found = Ok(());
        let mut base = 0;
        for (index, queued) in self.queued.iter().enumerate() {
            let chunks = queued.total.div_ceil(blake3::CHUNK_LEN);
            let root = subtree(&self.platform, &self.cvs[base..base + chunks], ROOT);
            base += chunks;
            let mut hash = [0u8; HASH_BYTES];
            for (bytes, word) in hash.chunks_mut(4).zip(root) {
                bytes.copy_from_slice(&word.to_le_bytes());
            }
            match matches(hash, queued.ptr) {
                Ok(()) => passed(index),
                Err(error) if found.is_ok() => found = Err(error),
                Err(_) => {}
            }
        }
        self.bytes.clear();
        self.queued.clear();
        found
    }

    /// Checks every queued record and empties the queue.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] for the first record queued whose hash differs.
    pub(super) fn finish(&mut self) -> Result<()> {
        self.check(|_| {})
    }

    /// The error to report for `error`, met after everything queued was read: the first failure
    /// among the queued records if there is one, and `error` otherwise. It empties the queue.
    pub(super) fn first(&mut self, error: Error) -> Error {
        match self.finish() {
            Err(earlier) => earlier,
            Ok(()) => error,
        }
    }
}

/// The bytes the hash of an object with a body of `raw` bytes and `links` links is taken over.
fn hashed(raw: usize, links: usize) -> usize {
    links
        .saturating_mul(HASH_BYTES)
        .saturating_add(raw.saturating_add(8))
}

/// Fails with [`Error::Corrupt`] unless `hash` is the hash `ptr` names.
fn matches(hash: [u8; HASH_BYTES], ptr: Ptr) -> Result<()> {
    match hash == ptr.hash {
        true => Ok(()),
        false => Err(Error::Corrupt(format!(
            "hash mismatch for object at {}",
            ptr.offset
        ))),
    }
}

/// The words of a chaining value BLAKE3 wrote out as bytes.
fn words(bytes: &[u8]) -> [u32; 8] {
    std::array::from_fn(|index| {
        let word = &bytes[index * 4..index * 4 + 4];
        u32::from_le_bytes(word.try_into().expect("a word is 4 bytes"))
    })
}

/// Compresses the chunks `tasks` names in `bytes`, all of them at chunk counter `counter`, into
/// the chaining values `cvs` holds for them: whole chunks that are not a record's last for `kind`
/// [`BLOCKS`], and otherwise last chunks of `kind` blocks before their last, whose last block has
/// the root flag when the chunk is the record's only one.
fn compress(
    platform: &Platform,
    counter: u64,
    kind: usize,
    tasks: &[Task],
    bytes: &[u8],
    cvs: &mut [[u32; 8]],
) {
    let mut out = [0u8; WIDE * blake3::OUT_LEN];
    for part in tasks.chunks(WIDE) {
        let out = &mut out[..part.len() * blake3::OUT_LEN];
        if kind == BLOCKS {
            many::<{ blake3::CHUNK_LEN }>(platform, part, bytes, counter, CHUNK_END, out);
            for (task, cv) in part.iter().zip(out.chunks_exact(blake3::OUT_LEN)) {
                cvs[task.cv] = words(cv);
            }
            continue;
        }
        leading(platform, kind, part, bytes, counter, out);
        for (index, task) in part.iter().enumerate() {
            let mut cv = match kind {
                0 => IV,
                _ => words(&out[index * blake3::OUT_LEN..(index + 1) * blake3::OUT_LEN]),
            };
            let from = task.at + kind * blake3::BLOCK_LEN;
            let last: &[u8; blake3::BLOCK_LEN] = bytes[from..from + blake3::BLOCK_LEN]
                .try_into()
                .expect("a record's bytes end in a whole block");
            let start = if kind == 0 { CHUNK_START } else { 0 };
            let root = if counter == 0 { ROOT } else { 0 };
            platform.compress_in_place(&mut cv, last, task.len, counter, CHUNK_END | start | root);
            cvs[task.cv] = cv;
        }
    }
}

/// Compresses the `kind` blocks before the last of each chunk in `tasks`, at chunk counter
/// `counter`, writing each chunk's chaining value to `out`. It writes nothing for `kind` 0.
fn leading(
    platform: &Platform,
    kind: usize,
    tasks: &[Task],
    bytes: &[u8],
    counter: u64,
    out: &mut [u8],
) {
    match kind {
        1 => many::<64>(platform, tasks, bytes, counter, 0, out),
        2 => many::<128>(platform, tasks, bytes, counter, 0, out),
        3 => many::<192>(platform, tasks, bytes, counter, 0, out),
        4 => many::<256>(platform, tasks, bytes, counter, 0, out),
        5 => many::<320>(platform, tasks, bytes, counter, 0, out),
        6 => many::<384>(platform, tasks, bytes, counter, 0, out),
        7 => many::<448>(platform, tasks, bytes, counter, 0, out),
        8 => many::<512>(platform, tasks, bytes, counter, 0, out),
        9 => many::<576>(platform, tasks, bytes, counter, 0, out),
        10 => many::<640>(platform, tasks, bytes, counter, 0, out),
        11 => many::<704>(platform, tasks, bytes, counter, 0, out),
        12 => many::<768>(platform, tasks, bytes, counter, 0, out),
        13 => many::<832>(platform, tasks, bytes, counter, 0, out),
        14 => many::<896>(platform, tasks, bytes, counter, 0, out),
        15 => many::<960>(platform, tasks, bytes, counter, 0, out),
        _ => {}
    }
}

/// Compresses the first `N` bytes of each chunk in `tasks`, at most [`WIDE`] of them, as the start
/// of a chunk at counter `counter`, with `end` on the last block, writing each chaining value to
/// `out`.
fn many<const N: usize>(
    platform: &Platform,
    tasks: &[Task],
    bytes: &[u8],
    counter: u64,
    end: u8,
    out: &mut [u8],
) {
    let count = tasks.len();
    let inputs: [&[u8; N]; WIDE] = std::array::from_fn(|index| {
        let from = tasks[index.min(count - 1)].at;
        bytes[from..from + N]
            .try_into()
            .expect("a chunk holds its blocks")
    });
    platform.hash_many(
        &inputs[..count],
        &IV,
        counter,
        IncrementCounter::No,
        0,
        CHUNK_START,
        end,
        out,
    );
}

/// The chaining value of the subtree over the chunks whose chaining values `cvs` holds, built as
/// BLAKE3 builds it, its left side holding the largest power of two chunks short of all of them,
/// with `flags` on its top parent node. A single chunk is its own subtree.
fn subtree(platform: &Platform, cvs: &[[u32; 8]], flags: u8) -> [u32; 8] {
    if cvs.len() == 1 {
        return cvs[0];
    }
    let left = 1 << (usize::BITS - 1 - (cvs.len() - 1).leading_zeros());
    let joined = [
        subtree(platform, &cvs[..left], 0),
        subtree(platform, &cvs[left..], 0),
    ];
    let mut block = [0u8; blake3::BLOCK_LEN];
    for (bytes, word) in block.chunks_exact_mut(4).zip(joined.iter().flatten()) {
        bytes.copy_from_slice(&word.to_le_bytes());
    }
    let mut cv = IV;
    platform.compress_in_place(&mut cv, &block, blake3::BLOCK_LEN as u8, 0, PARENT | flags);
    cv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Hash, hash_parts};

    fn object(seed: u64, raw: usize, links: usize) -> (Vec<u8>, Vec<Ptr>) {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        };
        let body = (0..raw).map(|_| next()).collect();
        let links = (0..links)
            .map(|offset| Ptr {
                hash: std::array::from_fn(|_| next()),
                offset: offset as u64,
            })
            .collect();
        (body, links)
    }

    fn stored(body: &[u8]) -> Stored {
        Stored {
            raw: body.len(),
            packed: false,
            payload: 0..body.len(),
            len: body.len(),
        }
    }

    fn ptr(body: &[u8], links: &[Ptr], offset: u64) -> Ptr {
        let hash: Hash = hash_parts(body, links.iter().map(|link| &link.hash));
        Ptr { hash, offset }
    }

    fn platforms() -> Vec<Platform> {
        let mut platforms = vec![Platform::portable(), Platform::detect()];
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        platforms.extend(
            [Platform::sse2(), Platform::sse41(), Platform::avx2()]
                .into_iter()
                .flatten(),
        );
        platforms
    }

    #[test]
    fn checks_give_the_hash_hash_parts_gives_at_every_length_past_sixteen_chunks() {
        let lengths: Vec<usize> = (0..2200)
            .chain((2200..17_500).step_by(37))
            .chain([3071, 3072, 3073, 4095, 4096, 4097, 16_375, 16_376, 16_377])
            .collect();
        for platform in platforms() {
            let mut checks = Checks::on(platform, 256 << 10);
            let mut offset = 0;
            for &raw in &lengths {
                for count in [0, 1, 7] {
                    let (body, links) = object(offset, raw, count);
                    offset += 1;
                    let ptr = ptr(&body, &links, offset);
                    checks.queue(&body, &stored(&body), &links, ptr).unwrap();
                    if checks.full() {
                        checks.finish().unwrap();
                    }
                }
            }
            checks.finish().unwrap();
        }
    }

    #[test]
    fn a_check_names_the_first_record_queued_whose_hash_differs() {
        for raw in [
            0, 40, 56, 57, 300, 1000, 1017, 1500, 2100, 5000, 16_000, 17_000,
        ] {
            let mut checks = Checks::new(1 << 20);
            let mut passed = Vec::new();
            let mut immediate = None;
            for offset in 0..40u64 {
                let (body, links) = object(offset + 1, raw + offset as usize % 3, 1);
                let mut ptr = ptr(&body, &links, offset);
                if offset == 21 || offset == 33 {
                    ptr.hash[HASH_BYTES - 1] ^= 1;
                }
                match checks.queue(&body, &stored(&body), &links, ptr) {
                    Ok(true) => passed.push(offset as usize),
                    Ok(false) => {}
                    Err(error) => {
                        immediate.get_or_insert(error);
                    }
                }
            }
            let error = match immediate {
                Some(error) => checks.first(error),
                None => checks.check(|index| passed.push(index)).unwrap_err(),
            };
            match error {
                Error::Corrupt(message) => {
                    assert_eq!(message, "hash mismatch for object at 21", "{raw} bytes")
                }
                other => panic!("{raw} bytes: {other:?}"),
            }
            let expected: Vec<usize> = (0..40).filter(|at| *at != 21 && *at != 33).collect();
            assert_eq!(passed, expected, "{raw} bytes");
        }
    }

    #[test]
    fn a_body_that_fails_to_decompress_queues_nothing() {
        let mut checks = Checks::new(1 << 20);
        let (body, links) = object(1, 100, 2);
        let good = ptr(&body, &links, 1);
        checks.queue(&body, &stored(&body), &links, good).unwrap();
        let packed = Stored {
            raw: 100,
            packed: true,
            payload: 0..4,
            len: 4,
        };
        let refused = checks.queue(&[0xff; 4], &packed, &links, ptr(&body, &links, 2));
        assert!(matches!(refused, Err(Error::Corrupt(_))));
        let mut passed = Vec::new();
        checks.check(|index| passed.push(index)).unwrap();
        assert_eq!(passed, vec![0]);
        assert!(checks.is_empty());
    }
}
