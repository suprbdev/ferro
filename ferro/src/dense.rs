//! Dense columns and hash-to-id tables for passes that track millions of objects.
//!
//! A sync or a staged receive keeps a few facts for every object it moves. [`Hashes`] gives each
//! object's hash a `u32` id, and [`Column`]s indexed by that id hold the rest, so each object costs
//! a few dozen bytes rather than a map entry per fact. [`Ascending`] keeps offsets that only grow in
//! about four bytes each and finds an offset's position without a table, and [`Marked`] keeps a set
//! of a file's objects by their offsets, finding each by its hash without holding it.
//! [The sync internals](https://github.com/suprbdev/ferro/blob/main/docs/internals/sync.md) have
//! the sizes.

use hashbrown::HashTable;

use crate::codec::Hash;

const CHUNK: usize = 1 << 12;
const SHARDS: usize = 16;

/// An append-only array of `Copy` values addressed by `u32` position.
///
/// Values live in chunks of 4,096. The first chunk grows as a `Vec` does, so a column of a few
/// values allocates only what they need, and every later chunk is allocated whole, so a large
/// column grows without copying what it already holds.
pub(crate) struct Column<T> {
    chunks: Vec<Vec<T>>,
    len: usize,
}

impl<T: Copy> Column<T> {
    /// Creates an empty column, which allocates nothing until the first push.
    pub(crate) fn new() -> Column<T> {
        Column {
            chunks: Vec::new(),
            len: 0,
        }
    }

    /// The number of values pushed.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Appends `value` at position [`len`](Column::len).
    pub(crate) fn push(&mut self, value: T) {
        match self.chunks.last_mut() {
            Some(chunk) if chunk.len() < CHUNK => chunk.push(value),
            _ => {
                let mut chunk = match self.chunks.is_empty() {
                    true => Vec::new(),
                    false => Vec::with_capacity(CHUNK),
                };
                chunk.push(value);
                self.chunks.push(chunk);
            }
        }
        self.len += 1;
    }

    /// The value at position `at`.
    ///
    /// # Panics
    ///
    /// Panics if `at` is not below [`len`](Column::len).
    pub(crate) fn get(&self, at: u32) -> T {
        let at = at as usize;
        self.chunks[at / CHUNK][at % CHUNK]
    }

    /// Overwrites the value at position `at`.
    ///
    /// # Panics
    ///
    /// Panics if `at` is not below [`len`](Column::len).
    pub(crate) fn set(&mut self, at: u32, value: T) {
        let at = at as usize;
        self.chunks[at / CHUNK][at % CHUNK] = value;
    }

    /// Every value in position order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = T> + '_ {
        self.chunks.iter().flatten().copied()
    }
}

/// A column of offsets that only grow, which finds the position of an offset by halving.
///
/// It keeps the first offset of each chunk of 4,096 in one array, and each offset as its distance
/// from that first one in four bytes, so an offset costs about four bytes. A distance that does
/// not fit, past 4 GiB from the start of its chunk, is kept whole in a table beside them. A search
/// halves the array of first offsets and then one chunk.
pub(crate) struct Ascending {
    firsts: Vec<u64>,
    distances: Column<u32>,
    far: Vec<(u32, u64)>,
}

impl Ascending {
    /// Creates an empty column.
    pub(crate) fn new() -> Ascending {
        Ascending {
            firsts: Vec::new(),
            distances: Column::new(),
            far: Vec::new(),
        }
    }

    /// The number of offsets pushed.
    pub(crate) fn len(&self) -> usize {
        self.distances.len()
    }

    /// Appends `offset`, which must be past every offset pushed before it.
    pub(crate) fn push(&mut self, offset: u64) {
        let at = self.distances.len();
        debug_assert!(at == 0 || self.get(at as u32 - 1) < offset, "offsets grow");
        if at.is_multiple_of(CHUNK) {
            self.firsts.push(offset);
        }
        let first = self.firsts[at / CHUNK];
        match u32::try_from(offset - first) {
            Ok(distance) if distance < u32::MAX => self.distances.push(distance),
            _ => {
                self.far.push((at as u32, offset));
                self.distances.push(u32::MAX);
            }
        }
    }

    /// The offset at position `at`.
    ///
    /// # Panics
    ///
    /// Panics if `at` is not below [`len`](Ascending::len).
    pub(crate) fn get(&self, at: u32) -> u64 {
        match self.distances.get(at) {
            u32::MAX => {
                let found = self.far.binary_search_by_key(&at, |(held, _)| *held);
                self.far[found.expect("a far offset is kept")].1
            }
            distance => self.firsts[at as usize / CHUNK] + u64::from(distance),
        }
    }

    /// The position of `offset`, if it was pushed.
    pub(crate) fn position(&self, offset: u64) -> Option<u32> {
        let chunk = self
            .firsts
            .partition_point(|first| *first <= offset)
            .checked_sub(1)?;
        let base = (chunk * CHUNK) as u32;
        match u32::try_from(offset - self.firsts[chunk]) {
            Ok(distance) if distance < u32::MAX => self.distances.chunks[chunk]
                .binary_search(&distance)
                .ok()
                .map(|at| base + at as u32),
            _ => self
                .far
                .binary_search_by_key(&offset, |(_, held)| *held)
                .ok()
                .map(|at| self.far[at].0),
        }
    }
}

const OFFSET_BITS: u32 = 61;

/// The largest offset a [`Marked`] holds.
pub(crate) const MARKED_MOST: u64 = (1 << OFFSET_BITS) - 1;

/// One entry of a [`Marked`]: an offset in the low 61 bits of its first two words and three bits
/// of marks above them, a `u32` in the third, and the first four bytes of the hash in the last.
#[derive(Clone, Copy)]
struct Slot([u32; 4]);

impl Slot {
    fn of(offset: u64, marks: u8, value: u32, part: u32) -> Slot {
        let key = offset | u64::from(marks) << OFFSET_BITS;
        Slot([key as u32, (key >> 32) as u32, value, part])
    }

    fn key(self) -> u64 {
        u64::from(self.0[0]) | u64::from(self.0[1]) << 32
    }

    fn offset(self) -> u64 {
        self.key() & MARKED_MOST
    }

    fn mark(self) -> Mark {
        Mark {
            offset: self.offset(),
            marks: (self.key() >> OFFSET_BITS) as u8,
            value: self.0[2],
        }
    }

    fn mixed(self) -> u64 {
        mixed(self.0[3])
    }
}

fn part(hash: &Hash) -> u32 {
    u32::from_le_bytes(hash[..4].try_into().expect("a hash is longer than 4 bytes"))
}

fn mixed(part: u32) -> u64 {
    u64::from(part) | u64::from(part >> 25) << 57
}

/// What a [`Marked`] holds for an object: the offset it was added at, its marks and its value.
#[derive(Clone, Copy)]
pub(crate) struct Mark {
    /// The offset the object was added at, which may differ from the offset it was looked up by,
    /// where the file holds the object twice.
    pub(crate) offset: u64,
    /// The three bits of marks.
    pub(crate) marks: u8,
    /// The `u32` beside the object.
    pub(crate) value: u32,
}

/// A set of objects of a file, each named by its [`Hash`](type@Hash) and found at an offset, with
/// three bits of marks and a `u32` beside it, 16 bytes and a control byte a slot.
///
/// A slot keeps the offset rather than the hash, with 32 bits of the hash to tell objects apart.
/// An object looked up at the offset it was added at is found by that offset alone. One looked up
/// at another offset, where the file holds it twice, is found once the caller confirms that a slot
/// whose bits match holds the same object at its own offset, as by reading it back. The table is
/// split into 16 by a byte of the hash, so growing one copies a sixteenth of the slots.
pub(crate) struct Marked {
    shards: [HashTable<Slot>; SHARDS],
    len: usize,
}

impl Marked {
    /// Creates an empty set.
    pub(crate) fn new() -> Marked {
        Marked {
            shards: std::array::from_fn(|_| HashTable::new()),
            len: 0,
        }
    }

    /// The number of objects in the set.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// What the set holds for the object `hash` names, found at `offset`, if it holds it.
    ///
    /// A slot added at `offset` is the object. Otherwise `same` is asked of each slot whose bits
    /// match `hash`, by its offset, whether the object there is the one `hash` names.
    ///
    /// # Errors
    ///
    /// The first error `same` returns.
    pub(crate) fn find<E>(
        &self,
        hash: &Hash,
        offset: u64,
        mut same: impl FnMut(u64) -> std::result::Result<bool, E>,
    ) -> std::result::Result<Option<Mark>, E> {
        let (table, part) = (&self.shards[shard(hash)], part(hash));
        let matching = || {
            table
                .iter_hash(mixed(part))
                .filter(move |slot| slot.0[3] == part)
        };
        let mut elsewhere = false;
        for slot in matching() {
            if slot.offset() == offset {
                return Ok(Some(slot.mark()));
            }
            elsewhere = true;
        }
        if elsewhere {
            for slot in matching() {
                if same(slot.offset())? {
                    return Ok(Some(slot.mark()));
                }
            }
        }
        Ok(None)
    }

    /// Adds the object `hash` names at `offset`, at most [`MARKED_MOST`], with `marks` below 8
    /// and `value` beside it.
    ///
    /// The caller must know the object is absent, from [`find`](Marked::find).
    pub(crate) fn insert(&mut self, hash: &Hash, offset: u64, marks: u8, value: u32) {
        debug_assert!(offset <= MARKED_MOST && marks < 8);
        let part = part(hash);
        let slot = Slot::of(offset, marks, value, part);
        self.shards[shard(hash)].insert_unique(mixed(part), slot, |slot| slot.mixed());
        self.len += 1;
    }

    /// Replaces the marks and value of the object `hash` names that was added at `offset`, as
    /// [`find`](Marked::find) gives it, with what `change` makes of them, and returns what they
    /// were.
    ///
    /// # Errors
    ///
    /// If no object was added at `offset`, whether a slot added elsewhere has bits that match
    /// `hash`, so that it may hold the same object.
    pub(crate) fn update(
        &mut self,
        hash: &Hash,
        offset: u64,
        change: impl FnOnce(Mark) -> (u8, u32),
    ) -> std::result::Result<Mark, bool> {
        let part = part(hash);
        let mut elsewhere = false;
        for slot in self.shards[shard(hash)].iter_hash_mut(mixed(part)) {
            if slot.0[3] != part {
                continue;
            }
            if slot.offset() != offset {
                elsewhere = true;
                continue;
            }
            let mark = slot.mark();
            let (marks, value) = change(mark);
            *slot = Slot::of(offset, marks, value, part);
            return Ok(mark);
        }
        Err(elsewhere)
    }
}

fn spread(hash: &Hash) -> u64 {
    u64::from_le_bytes(hash[..8].try_into().expect("a hash is longer than 8 bytes"))
}

fn shard(hash: &Hash) -> usize {
    usize::from(hash[8]) % SHARDS
}

/// A hash table of `u32` ids keyed by a [`Hash`](type@Hash) that it does not store.
///
/// Each slot holds only an id, and the caller passes `hash_of` to read the hash an id stands for,
/// usually from a [`Column`], so a slot costs four bytes and a control byte.
#[derive(Default)]
pub(crate) struct Index(HashTable<u32>);

impl Index {
    /// The id whose hash is `hash`, where `hash_of` gives the hash of each id.
    pub(crate) fn find(&self, hash: &Hash, hash_of: impl Fn(u32) -> Hash) -> Option<u32> {
        self.0
            .find(spread(hash), |id| hash_of(*id) == *hash)
            .copied()
    }

    /// Adds `id` under `hash`, where `hash_of` gives the hash of each id already present.
    ///
    /// The caller must know `hash` is absent, from [`find`](Index::find) or by construction: a
    /// second id under one hash is kept beside the first, and `find` returns either.
    pub(crate) fn insert(&mut self, hash: &Hash, id: u32, hash_of: impl Fn(u32) -> Hash) {
        self.0
            .insert_unique(spread(hash), id, |id| spread(&hash_of(*id)));
    }
}

/// Dense `u32` ids for a set of hashes: the [`Hash`](type@Hash)es in a [`Column`] in the order
/// they came, and a table from hash to id.
///
/// The table is split into 16 [`Index`]es by a byte of the hash, so growing one copies a
/// sixteenth of the ids.
pub(crate) struct Hashes {
    hashes: Column<Hash>,
    shards: [Index; SHARDS],
}

impl Hashes {
    /// Creates an empty set of hashes.
    pub(crate) fn new() -> Hashes {
        Hashes {
            hashes: Column::new(),
            shards: std::array::from_fn(|_| Index::default()),
        }
    }

    /// The id of `hash`, if it has one.
    pub(crate) fn find(&self, hash: &Hash) -> Option<u32> {
        let hashes = &self.hashes;
        self.shards[shard(hash)].find(hash, |id| hashes.get(id))
    }

    /// Gives `hash` the next id and returns it, or `None` once every `u32` id is taken.
    ///
    /// The caller must know `hash` has no id yet, from [`find`](Hashes::find) or by
    /// construction, as [`Index::insert`] requires.
    pub(crate) fn insert(&mut self, hash: Hash) -> Option<u32> {
        let id = u32::try_from(self.hashes.len()).ok()?;
        self.hashes.push(hash);
        let hashes = &self.hashes;
        self.shards[shard(&hash)].insert(&hash, id, |id| hashes.get(id));
        Some(id)
    }

    /// The hash with id `id`.
    ///
    /// # Panics
    ///
    /// Panics if no hash has that id.
    pub(crate) fn hash(&self, id: u32) -> Hash {
        self.hashes.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascending_finds_every_offset_it_holds_and_no_other() {
        let mut column = Ascending::new();
        let mut offsets = Vec::new();
        let mut offset = 16u64;
        for at in 0..3 * CHUNK as u64 {
            offset += match at % 1000 {
                999 => 5 << 30,
                _ => 2 + at % 7,
            };
            offsets.push(offset);
            column.push(offset);
        }
        assert_eq!(column.len(), offsets.len());
        for (at, offset) in offsets.iter().enumerate() {
            assert_eq!(column.get(at as u32), *offset);
            assert_eq!(column.position(*offset), Some(at as u32));
            assert_eq!(column.position(offset + 1), None);
        }
        assert_eq!(column.position(15), None);
    }

    #[test]
    fn marked_finds_an_object_at_another_offset_once_confirmed() {
        let mut marked = Marked::new();
        let hashes: Vec<Hash> = (0..10_000u32)
            .map(|at| {
                let mut hash = [0u8; 20];
                hash[..4].copy_from_slice(&(at % 64).to_le_bytes());
                hash[4..8].copy_from_slice(&at.to_le_bytes());
                hash[8] = at as u8;
                hash
            })
            .collect();
        for (at, hash) in hashes.iter().enumerate() {
            marked.insert(hash, 100 + at as u64, (at % 8) as u8, at as u32);
        }
        assert_eq!(marked.len(), hashes.len());
        let never = |_| Ok::<bool, ()>(false);
        for (at, hash) in hashes.iter().enumerate() {
            let mark = marked.find(hash, 100 + at as u64, never).unwrap().unwrap();
            assert_eq!(
                (mark.offset, mark.marks, mark.value),
                (100 + at as u64, (at % 8) as u8, at as u32)
            );
            assert!(marked.find(hash, 7, never).unwrap().is_none());
            let same = |offset| Ok::<bool, ()>(offset == 100 + at as u64);
            assert_eq!(
                marked.find(hash, 7, same).unwrap().unwrap().offset,
                100 + at as u64
            );
        }
        let was = marked.update(&hashes[5], 105, |_| (3, 99)).unwrap();
        assert_eq!((was.marks, was.value), (5, 5));
        let mark = marked.find(&hashes[5], 105, never).unwrap().unwrap();
        assert_eq!((mark.marks, mark.value), (3, 99));
        assert_eq!(marked.update(&hashes[5], 7, |_| (0, 0)).err(), Some(true));
        let mut absent = hashes[5];
        absent[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(marked.update(&absent, 105, |_| (0, 0)).err(), Some(false));
    }
}
