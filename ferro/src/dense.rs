use hashbrown::HashTable;

use crate::codec::Hash;

const CHUNK: usize = 1 << 12;
const SHARDS: usize = 16;

pub(crate) struct Column<T> {
    chunks: Vec<Vec<T>>,
    len: usize,
}

impl<T: Copy> Column<T> {
    pub(crate) fn new() -> Column<T> {
        Column {
            chunks: Vec::new(),
            len: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

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

    pub(crate) fn get(&self, at: u32) -> T {
        let at = at as usize;
        self.chunks[at / CHUNK][at % CHUNK]
    }

    pub(crate) fn set(&mut self, at: u32, value: T) {
        let at = at as usize;
        self.chunks[at / CHUNK][at % CHUNK] = value;
    }

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

#[derive(Default)]
pub(crate) struct Index(HashTable<u32>);

impl Index {
    pub(crate) fn find(&self, hash: &Hash, hash_of: impl Fn(u32) -> Hash) -> Option<u32> {
        self.0
            .find(spread(hash), |id| hash_of(*id) == *hash)
            .copied()
    }

    pub(crate) fn insert(&mut self, hash: &Hash, id: u32, hash_of: impl Fn(u32) -> Hash) {
        self.0
            .insert_unique(spread(hash), id, |id| spread(&hash_of(*id)));
    }
}

pub(crate) struct Hashes {
    hashes: Column<Hash>,
    shards: [Index; SHARDS],
}

impl Hashes {
    pub(crate) fn new() -> Hashes {
        Hashes {
            hashes: Column::new(),
            shards: std::array::from_fn(|_| Index::default()),
        }
    }

    pub(crate) fn find(&self, hash: &Hash) -> Option<u32> {
        let hashes = &self.hashes;
        self.shards[shard(hash)].find(hash, |id| hashes.get(id))
    }

    pub(crate) fn insert(&mut self, hash: Hash) -> Option<u32> {
        let id = u32::try_from(self.hashes.len()).ok()?;
        self.hashes.push(hash);
        let hashes = &self.hashes;
        self.shards[shard(&hash)].insert(&hash, id, |id| hashes.get(id));
        Some(id)
    }

    pub(crate) fn hash(&self, id: u32) -> Hash {
        self.hashes.get(id)
    }
}
