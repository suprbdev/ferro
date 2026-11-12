//! The number each hash the copy's threads meet is given, in tables that any thread reads without
//! a lock.
//!
//! The hashes are split into 256 shards by one of their bytes, and each shard has an
//! open-addressed table of 24-byte slots, found from the first 8 bytes of the hash and the slots
//! after it in turn until the hash or an empty slot. A slot is three words a thread reads with
//! atomic loads: the first two hold the first 16 bytes of the hash, and the third the last 4 with
//! the number plus one, which is written last, so that a thread that reads a number reads the hash
//! written before it. A thread that does not find a hash adds it under its shard's lock, which
//! first doubles the shard's table into a new one if that would take more than three quarters of
//! its slots. A table a shard outgrew is kept until the copy ends, since a thread may still be
//! reading it, and a hash missing from it is looked for again under the lock in the newest.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::codec::Hash;
use crate::map::prefetch;

const SHARDS: usize = 256;
const FIRST: usize = 64;
const TABLES: usize = 27;

#[derive(Default)]
struct Slot([AtomicU64; 3]);

struct Words(u64, u64, u64);

impl Words {
    fn of(hash: &Hash) -> Words {
        let word = |at: usize| u64::from_le_bytes(hash[at..at + 8].try_into().expect("8 bytes"));
        let last = u32::from_le_bytes(hash[16..20].try_into().expect("4 bytes"));
        Words(word(0), word(8), u64::from(last))
    }
}

struct Shard {
    newest: AtomicUsize,
    tables: [OnceLock<Box<[Slot]>>; TABLES],
    len: Mutex<usize>,
}

impl Shard {
    fn new() -> Shard {
        Shard {
            newest: AtomicUsize::new(0),
            tables: std::array::from_fn(|_| OnceLock::new()),
            len: Mutex::new(0),
        }
    }

    fn newest(&self) -> Option<&[Slot]> {
        let newest = self.newest.load(Ordering::Acquire).checked_sub(1)?;
        self.tables[newest].get().map(|table| &**table)
    }

    fn grow(&self) -> &[Slot] {
        let next = self.newest.load(Ordering::Acquire);
        assert!(next < TABLES, "a copy of more objects than its tables hold");
        let old = self.newest().unwrap_or(&[]);
        let size = (old.len() * 2).max(FIRST);
        let table: Box<[Slot]> = std::iter::repeat_with(Slot::default).take(size).collect();
        for slot in old {
            let last = slot.0[2].load(Ordering::Relaxed);
            if last >> 32 == 0 {
                continue;
            }
            let words = Words(
                slot.0[0].load(Ordering::Relaxed),
                slot.0[1].load(Ordering::Relaxed),
                last & u64::from(u32::MAX),
            );
            if let Err(at) = find(&table, &words) {
                write(&table[at], &words, last >> 32);
            }
        }
        let table = self.tables[next].get_or_init(|| table);
        self.newest.store(next + 1, Ordering::Release);
        table
    }
}

fn find(table: &[Slot], words: &Words) -> Result<u32, usize> {
    let mask = table.len() - 1;
    let mut at = words.0 as usize & mask;
    loop {
        let slot = &table[at];
        let last = slot.0[2].load(Ordering::Acquire);
        let id = last >> 32;
        if id == 0 {
            return Err(at);
        }
        if last & u64::from(u32::MAX) == words.2
            && slot.0[0].load(Ordering::Relaxed) == words.0
            && slot.0[1].load(Ordering::Relaxed) == words.1
        {
            return Ok((id - 1) as u32);
        }
        at = (at + 1) & mask;
    }
}

fn write(slot: &Slot, words: &Words, number: u64) {
    slot.0[0].store(words.0, Ordering::Relaxed);
    slot.0[1].store(words.1, Ordering::Relaxed);
    slot.0[2].store(words.2 | number << 32, Ordering::Release);
}

/// The number given to each hash, by hash.
pub(super) struct Ids {
    shards: Box<[Shard]>,
}

impl Ids {
    /// An empty table, which allocates each shard's slots as its first hash comes.
    pub(super) fn new() -> Ids {
        Ids {
            shards: (0..SHARDS).map(|_| Shard::new()).collect(),
        }
    }

    fn shard(&self, hash: &Hash) -> &Shard {
        &self.shards[usize::from(hash[8]) % SHARDS]
    }

    /// Starts fetching the slot `hash` would be found from into the cache, for a lookup to come.
    pub(super) fn prefetch(&self, hash: &Hash) {
        if let Some(table) = self.shard(hash).newest() {
            let home = Words::of(hash).0 as usize & (table.len() - 1);
            prefetch(table.as_ptr().wrapping_add(home).addr());
        }
    }

    /// The number given to `hash`, if it has one.
    pub(super) fn get(&self, hash: &Hash) -> Option<u32> {
        find(self.shard(hash).newest()?, &Words::of(hash)).ok()
    }

    /// The number given to `hash`, which is `id` unless another thread gave it one first.
    ///
    /// # Panics
    ///
    /// If `id` is `u32::MAX`, or a shard would need a table of more than 2^32 slots.
    pub(super) fn insert(&self, hash: &Hash, id: u32) -> u32 {
        assert!(id < u32::MAX, "a copy of more than 2^32 objects");
        let shard = self.shard(hash);
        let words = Words::of(hash);
        let mut len = shard.len.lock();
        let table = match shard.newest() {
            Some(table) => match find(table, &words) {
                Ok(found) => return found,
                Err(_) if (*len + 1) * 4 > table.len() * 3 => shard.grow(),
                Err(_) => table,
            },
            None => shard.grow(),
        };
        if let Err(at) = find(table, &words) {
            write(&table[at], &words, u64::from(id) + 1);
            *len += 1;
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u32) -> Hash {
        let mut hash = [0u8; 20];
        hash[..4].copy_from_slice(&seed.to_le_bytes());
        hash[8] = (seed % 3) as u8;
        hash[16..].copy_from_slice(&seed.wrapping_mul(0x9e37_79b9).to_le_bytes());
        hash
    }

    #[test]
    fn numbers_stay_as_tables_grow() {
        let ids = Ids::new();
        for seed in 0..10_000 {
            assert_eq!(ids.get(&hash(seed)), None);
            assert_eq!(ids.insert(&hash(seed), seed * 2), seed * 2);
            assert_eq!(ids.insert(&hash(seed), 1), seed * 2);
        }
        for seed in 0..10_000 {
            assert_eq!(ids.get(&hash(seed)), Some(seed * 2));
        }
        assert_eq!(ids.get(&hash(10_000)), None);
    }

    fn crowded(seed: u32) -> Hash {
        let mut hash = [0u8; 20];
        let word = u64::from(seed / 4).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        hash[..8].copy_from_slice(&word.to_le_bytes());
        hash[8] = 7;
        hash[12] = (seed % 4 / 2) as u8;
        hash[16] = (seed % 2) as u8;
        hash
    }

    #[test]
    fn readers_racing_grows_find_every_published_hash_and_no_other() {
        use std::sync::atomic::AtomicU32;
        const WRITERS: u32 = 2;
        const EACH: u32 = 20_000;
        for _ in 0..3 {
            let ids = Ids::new();
            let published: Vec<AtomicU32> = (0..WRITERS).map(|_| AtomicU32::new(0)).collect();
            let given: Vec<[AtomicU32; 2]> = (0..WRITERS * EACH)
                .map(|_| [AtomicU32::new(u32::MAX), AtomicU32::new(u32::MAX)])
                .collect();
            let done = std::sync::atomic::AtomicBool::new(false);
            std::thread::scope(|scope| {
                for writer in 0..WRITERS {
                    let (ids, published, given) = (&ids, &published, &given);
                    scope.spawn(move || {
                        for at in 0..EACH {
                            let seed = writer * EACH + at;
                            let got = ids.insert(&crowded(seed), seed);
                            given[seed as usize][0].store(got, Ordering::Relaxed);
                            let twin = (seed + EACH) % (WRITERS * EACH);
                            let other = ids.insert(&crowded(twin), twin + 1_000_000);
                            given[twin as usize][1].store(other, Ordering::Relaxed);
                            published[writer as usize].store(at + 1, Ordering::Release);
                        }
                    });
                }
                for reader in 0..2u32 {
                    let (ids, published, done) = (&ids, &published, &done);
                    scope.spawn(move || {
                        let mut state = 0x9e37_79b9u32.wrapping_mul(reader + 1);
                        while !done.load(Ordering::Relaxed) {
                            state ^= state << 13;
                            state ^= state >> 17;
                            state ^= state << 5;
                            let writer = state % WRITERS;
                            let count = published[writer as usize].load(Ordering::Acquire);
                            let at = (state >> 8) % EACH;
                            let seed = writer * EACH + at;
                            let got = ids.get(&crowded(seed));
                            if at < count {
                                assert!(
                                    got == Some(seed) || got == Some(seed + 1_000_000),
                                    "published seed {seed} read as {got:?}"
                                );
                            } else {
                                assert!(
                                    got.is_none()
                                        || got == Some(seed)
                                        || got == Some(seed + 1_000_000),
                                    "unpublished seed {seed} read as {got:?}"
                                );
                            }
                            let absent = crowded(WRITERS * EACH + (state >> 4) % 1000);
                            assert_eq!(ids.get(&absent), None);
                        }
                    });
                }
                scope.spawn(|| {
                    while published
                        .iter()
                        .any(|count| count.load(Ordering::Acquire) < EACH)
                    {
                        std::thread::yield_now();
                    }
                    done.store(true, Ordering::Relaxed);
                });
            });
            for seed in 0..WRITERS * EACH {
                let got = ids.get(&crowded(seed)).unwrap();
                assert!(got == seed || got == seed + 1_000_000);
                let [first, second] = &given[seed as usize];
                assert_eq!(first.load(Ordering::Relaxed), got);
                assert_eq!(second.load(Ordering::Relaxed), got);
            }
        }
    }

    #[test]
    fn threads_agree_on_each_number() {
        let ids = Ids::new();
        let numbers: Vec<Vec<u32>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4u32)
                .map(|thread| {
                    let ids = &ids;
                    scope.spawn(move || {
                        (0..5_000)
                            .map(|seed| ids.insert(&hash(seed), thread * 10_000 + seed))
                            .collect()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        for seed in 0..5_000 {
            let first = ids.get(&hash(seed as u32)).unwrap();
            assert!(numbers.iter().all(|numbers| numbers[seed] == first));
        }
    }
}
