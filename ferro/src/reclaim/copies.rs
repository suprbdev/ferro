//! The map of copies the walk keeps: what it copied of each object, by hash, in one table of
//! slots that each fit half a cache line, so that the walk can fetch the slot of every link a
//! record holds into the cache as soon as it reads the record, before it looks any of them up.
//!
//! A slot is found by the first 8 bytes of its hash, which BLAKE3 has already mixed, and the
//! slots after it in turn until the hash or an empty slot, and the table doubles before more
//! than five eighths of its slots are taken.

use std::mem;

use super::Copied;
use crate::codec::{HASH_BYTES, Hash};
use crate::map::prefetch;

const EMPTY: u32 = 0;
const OBJECT: u32 = 1;
const ENTRY: u32 = 2;
const BOTH: u32 = 3;
const FIRST: usize = 1024;

#[derive(Clone, Copy)]
#[repr(C, align(32))]
struct Slot {
    hash: Hash,
    kind: u32,
    offset: u64,
}

impl Slot {
    const EMPTY: Slot = Slot {
        hash: [0; HASH_BYTES],
        kind: EMPTY,
        offset: 0,
    };

    fn of(hash: Hash, copied: Copied) -> Slot {
        let (kind, offset) = match copied {
            Copied::Object(offset) => (OBJECT, offset),
            Copied::Entry => (ENTRY, 0),
            Copied::Both(offset) => (BOTH, offset),
        };
        Slot { hash, kind, offset }
    }

    fn copied(&self) -> Option<Copied> {
        match self.kind {
            OBJECT => Some(Copied::Object(self.offset)),
            ENTRY => Some(Copied::Entry),
            BOTH => Some(Copied::Both(self.offset)),
            _ => None,
        }
    }
}

/// What the walk copied of each object, by hash.
#[derive(Default)]
pub(super) struct Copies {
    slots: Vec<Slot>,
    len: usize,
}

impl Copies {
    fn home(&self, hash: &Hash) -> usize {
        let word = u64::from_le_bytes(hash[..8].try_into().expect("a hash has 8 bytes"));
        word as usize & (self.slots.len() - 1)
    }

    fn find(&self, hash: &Hash) -> Result<usize, usize> {
        let mask = self.slots.len() - 1;
        let mut at = self.home(hash);
        loop {
            let slot = &self.slots[at];
            if slot.kind == EMPTY {
                return Err(at);
            }
            if slot.hash == *hash {
                return Ok(at);
            }
            at = (at + 1) & mask;
        }
    }

    fn grow(&mut self) {
        let size = (self.slots.len() * 2).max(FIRST);
        let old = mem::replace(&mut self.slots, vec![Slot::EMPTY; size]);
        for slot in old.into_iter().filter(|slot| slot.kind != EMPTY) {
            if let Err(at) = self.find(&slot.hash) {
                self.slots[at] = slot;
            }
        }
    }

    /// Starts fetching the slot of `hash` into the cache, for a lookup to come.
    pub(super) fn prefetch(&self, hash: &Hash) {
        if !self.slots.is_empty() {
            prefetch(self.slots.as_ptr().wrapping_add(self.home(hash)).addr());
        }
    }

    /// What was copied of the object with `hash`, if anything.
    pub(super) fn get(&self, hash: &Hash) -> Option<Copied> {
        if self.slots.is_empty() {
            return None;
        }
        self.find(hash).ok().and_then(|at| self.slots[at].copied())
    }

    /// Sets what was copied of the object with `hash` to what `change` makes of what was copied
    /// before, if anything.
    pub(super) fn update(&mut self, hash: &Hash, change: impl FnOnce(Option<Copied>) -> Copied) {
        if (self.len + 1) * 8 > self.slots.len() * 5 {
            self.grow();
        }
        let (at, before) = match self.find(hash) {
            Ok(at) => (at, self.slots[at].copied()),
            Err(at) => {
                self.len += 1;
                (at, None)
            }
        };
        self.slots[at] = Slot::of(*hash, change(before));
    }

    /// Each hash in the table, with what was copied of its object.
    pub(super) fn iter(&self) -> impl Iterator<Item = (&Hash, Copied)> {
        self.slots
            .iter()
            .filter_map(|slot| Some((&slot.hash, slot.copied()?)))
    }
}
