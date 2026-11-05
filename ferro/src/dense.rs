//! Dense columns and hash-to-id tables for passes that track millions of objects.
//!
//! A sync or a staged receive keeps a few facts for every object it moves. [`Hashes`] gives each
//! object's hash a `u32` id, and [`Column`]s indexed by that id hold the rest, so each object costs
//! a few dozen bytes rather than a map entry per fact.
//! [The sync internals](https://github.com/archlemon/ferro/blob/main/docs/internals/sync.md) have
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

    /// The hashes in id order, dropping the table.
    pub(crate) fn into_column(self) -> Column<Hash> {
        self.hashes
    }
}
