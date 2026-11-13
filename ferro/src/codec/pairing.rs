//! Finding the tables a value holds and pairing them across two versions.
//!
//! [`Tabled`] works out, once for each type, whether a value of it can hold a
//! [`Table`](crate::Table), so that a check reads only the values that can. [`Pairing`] reads two
//! versions of a value side by side through [`Data::pair`] and pairs each table of the later one
//! with the table at the same place in the earlier one, so that the check that a received table's
//! indexes agree with its rows can diff the two instead of reading every row.

use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::fmt;

use super::{Checkpoint, Data, Decoder, FoundTable, Hash, Ptr, hash_parts};
use crate::error::Result;
use crate::store::Store;

/// Whether a type's values can hold a [`Table`](crate::Table), as [`Data::tables`] adds it up.
#[derive(Default)]
pub struct Tabled {
    open: Vec<TypeId>,
    found: bool,
}

impl fmt::Debug for Tabled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tabled").finish_non_exhaustive()
    }
}

impl Tabled {
    /// Adds whether `D` can hold a table, as its [`Data::tables`] says.
    pub fn of<D: Data>(&mut self) {
        if !self.found {
            D::tables(self);
        }
    }

    /// Adds what the named type `D` holds, as `holds` adds it, unless `D` is already being looked
    /// into further out, so that a recursive type's search ends.
    ///
    /// The derive wraps every struct and enum in it.
    pub fn named<D: Data>(&mut self, holds: impl FnOnce(&mut Tabled)) {
        let id = TypeId::of::<D>();
        if self.found || self.open.contains(&id) {
            return;
        }
        self.open.push(id);
        holds(self);
        self.open.pop();
    }

    /// Marks the type as one whose values can hold a table.
    pub fn table(&mut self) {
        self.found = true;
    }
}

/// Whether values of `D` can hold a [`Table`](crate::Table), as [`Data::tables`] says, worked out
/// once for each type.
pub(crate) fn holds_tables<D: Data>() -> bool {
    static KNOWN: std::sync::OnceLock<parking_lot::RwLock<HashMap<TypeId, bool>>> =
        std::sync::OnceLock::new();
    let known = KNOWN.get_or_init(Default::default);
    let id = TypeId::of::<D>();
    if let Some(holds) = known.read().get(&id) {
        return *holds;
    }
    let mut tabled = Tabled::default();
    tabled.of::<D>();
    known.write().insert(id, tabled.found);
    tabled.found
}

/// Pairs each table of one version with the table at the same place in the version before, the
/// place being the field tags, variant tags, element positions and map keys from the root down,
/// which [`Data::pair`] reads from the two encodings side by side.
///
/// It reads through a store, the view of a receive or the file `verify_typed` checks, and skips
/// what the two versions share: a value both encode alike holds the same tables, and a map whose
/// root is the same in both holds the same entries. Of a map, it reads only the nodes the two do
/// not share, as a diff does, and pairs the values of each key both hold. The diff of one map
/// opens each node once on each side, and one that reaches a node its side opened already, which
/// no map a write builds does, is left unpaired from there. Maps that share nodes, as clones of
/// one map do, are each paired in full, opening again the nodes another map opened, as long as
/// the nodes opened again number no more than those opened once; a map that would open more is
/// left unpaired from there, so a pairing opens at most twice as many nodes as the two versions
/// hold, however the maps share them. Two maps paired once are not paired again.
///
/// A map is paired after the value that holds it, one map at a time, so that maps nested in the
/// values of maps, however deep, do not deepen the stack: only the levels of one value at a time
/// do, as [`MAX_DEPTH`](super::MAX_DEPTH) bounds them. The maps it queues so count their nodes
/// under the same bound as the others.
///
/// It keeps a table that both versions hold at a place only by its type and hash, and only when
/// asked to, since only a parent the receiver holds vouches for such a table, and it builds a
/// `FoundTable` only for the two tables of a pair that differ.
pub struct Pairing<'s> {
    store: &'s Store,
    pairs: Vec<(FoundTable, FoundTable)>,
    same: Option<Vec<(TypeId, Hash)>>,
    maps: HashSet<(Hash, Hash)>,
    opened: [HashSet<Hash>; 2],
    again: usize,
    waiting: Vec<Waiting>,
    pub(super) aligning: bool,
}

impl fmt::Debug for Pairing<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pairing").finish_non_exhaustive()
    }
}

/// What pairs the tables of two versions of a map, given their roots with the entry counts their
/// links record.
pub(crate) type PairMaps = for<'s> fn(&mut Pairing<'s>, (Ptr, u64), (Ptr, u64)) -> Result<()>;

/// The pairing of two versions of a map that a [`Pairing`] has queued: what pairs them, and the
/// root of each with the entry count its link records.
type Waiting = (PairMaps, (Ptr, u64), (Ptr, u64));

/// The tables a [`Pairing`] paired: each table of the later version with the one at the same
/// place in the earlier version, when the two differ, and each table that is the same at its
/// place in both, if the pairing kept them.
#[derive(Default)]
pub(crate) struct Paired {
    /// Each table of the earlier version, with the table at its place in the later one.
    pub(crate) pairs: Vec<(FoundTable, FoundTable)>,
    /// The type and hash of each table that both versions hold at the same place, as
    /// [`FoundTable::identity`] gives them, if the pairing kept them.
    pub(crate) same: Vec<(TypeId, Hash)>,
}

impl<'s> Pairing<'s> {
    /// A pairing that reads through `store`, which with `keep_same` keeps the tables that both
    /// versions hold at the same place as well as those it pairs.
    pub(crate) fn new(store: &'s Store, keep_same: bool) -> Pairing<'s> {
        Pairing {
            store,
            pairs: Vec::new(),
            same: keep_same.then(Vec::new),
            maps: HashSet::new(),
            opened: [HashSet::new(), HashSet::new()],
            again: 0,
            waiting: Vec::new(),
            aligning: false,
        }
    }

    /// The store the pairing reads through.
    pub(crate) fn store(&self) -> &'s Store {
        self.store
    }

    /// What the pairing paired.
    pub(crate) fn into_paired(self) -> Paired {
        Paired {
            pairs: self.pairs,
            same: self.same.unwrap_or_default(),
        }
    }

    /// Reads the fields of a struct from `old` and `new`, as [`Decoder::fields`] does, and calls
    /// `f` on each tag both hold, with a decoder over that field on each side, in the order of
    /// their tags.
    ///
    /// It merges the two lists in the order they are stored, which the derive writes sorted by
    /// tag, so it reads each field of either once and holds none it is not pairing. A tag stored
    /// more than once is paired in the order stored, and of a list stored out of order it pairs
    /// only the fields the merge meets.
    ///
    /// The zero-width items `f` reads count as those of the object each side's field lies in, as
    /// [`Decoder::item`] counts them, so the fields of one object share the limit.
    ///
    /// # Errors
    ///
    /// The error of a field list that does not decode, and the first error of `f`, such as
    /// [`Error::Decode`] once more zero-width items than one object may hold have been read.
    pub fn fields<'a, 'b>(
        &mut self,
        old: &mut Decoder<'a>,
        new: &mut Decoder<'b>,
        mut f: impl FnMut(&mut Self, u16, &mut Decoder<'a>, &mut Decoder<'b>) -> Result<()>,
    ) -> Result<()> {
        let (mut olds, news) = (old.varint()?, new.varint()?);
        let mut ahead: Option<(u16, Decoder<'a>)> = None;
        for _ in 0..news {
            let (tag, mut new_field) = new.next_field()?;
            while olds > 0 && ahead.as_ref().is_none_or(|(other, _)| *other < tag) {
                olds -= 1;
                ahead = Some(old.next_field()?);
            }
            if let Some((_, mut old_field)) = ahead.take_if(|(other, _)| *other == tag) {
                old_field.empty_items = old.empty_items;
                new_field.empty_items = new.empty_items;
                f(self, tag, &mut old_field, &mut new_field)?;
                old.empty_items = old_field.empty_items;
                new.empty_items = new_field.empty_items;
            }
        }
        for _ in 0..olds {
            old.next_field()?;
        }
        Ok(())
    }

    /// Pairs the tables of a value of `D` that `old` and `new` each hold to their end, as a field
    /// does, unless `D` cannot hold a table, when it reads neither.
    ///
    /// # Errors
    ///
    /// As for [`Data::pair`].
    pub fn field<D: Data>(&mut self, old: &mut Decoder<'_>, new: &mut Decoder<'_>) -> Result<()> {
        if !holds_tables::<D>() {
            return Ok(());
        }
        D::pair(old, new, self)
    }

    /// Reads the variant tag of an enum `D` from `old` and `new`, and calls `same` with it when
    /// the two match, to pair what follows, or otherwise visits the rest of each as a `D`.
    ///
    /// # Errors
    ///
    /// The error of a tag or value that does not decode, and that of `same`.
    pub fn variant<'a, 'b, D: Data>(
        &mut self,
        old: &mut Decoder<'a>,
        new: &mut Decoder<'b>,
        same: impl FnOnce(&mut Self, u16, &mut Decoder<'a>, &mut Decoder<'b>) -> Result<()>,
    ) -> Result<()> {
        let (from_old, from_new) = (old.checkpoint(), new.checkpoint());
        let (tag, other) = (old.u16()?, new.u16()?);
        if tag == other {
            return same(self, tag, old, new);
        }
        old.rewind(from_old);
        new.rewind(from_new);
        D::visit(old)?;
        D::visit(new)
    }

    /// Records the table of type `id` that `new` read since `from_new`, a table of the later
    /// version, as paired with the one `old` read since `from_old`, the table at its place in the
    /// earlier one, each as `found` makes a [`FoundTable`] of it, or as the same table when the two
    /// encode alike.
    pub(crate) fn table(
        &mut self,
        id: TypeId,
        (old, from_old): (&Decoder<'_>, Checkpoint),
        (new, from_new): (&Decoder<'_>, Checkpoint),
        found: impl Fn(&Decoder<'_>, Checkpoint) -> FoundTable,
    ) {
        let (old_body, old_links) = old.read_since(from_old);
        let (new_body, new_links) = new.read_since(from_new);
        let alike = old_body == new_body
            && old_links
                .iter()
                .map(|link| link.hash)
                .eq(new_links.iter().map(|link| link.hash));
        if !alike {
            self.pairs
                .push((found(old, from_old), found(new, from_new)));
        } else if let Some(same) = &mut self.same {
            let hash = hash_parts(new_body, new_links.iter().map(|link| &link.hash));
            same.push((id, hash));
        }
    }

    /// Whether the maps with roots `old` and `new` are yet to be paired, which it notes they no
    /// longer are.
    pub(crate) fn unpaired_maps(&mut self, old: Hash, new: Hash) -> bool {
        self.maps.insert((old, new))
    }

    /// Whether the pairing of one map may open the node `hash` of the earlier version, or with
    /// `later` of the later one, which it then notes as opened in `map`, the nodes that map's
    /// pairing opened on each side: a node is opened once in each map, and a node another map
    /// opened is opened again only while the nodes opened again number no more than those opened
    /// once.
    pub(crate) fn may_open(
        &mut self,
        map: &mut [HashSet<Hash>; 2],
        later: bool,
        hash: Hash,
    ) -> bool {
        let side = usize::from(later);
        if !map[side].insert(hash) {
            return false;
        }
        if self.opened[side].insert(hash) {
            return true;
        }
        if self.again >= self.opened[0].len() + self.opened[1].len() {
            return false;
        }
        self.again += 1;
        true
    }

    /// Queues the pairing of two versions of a map, with roots `old` and `new`, by `pair`, for
    /// [`settle`](Pairing::settle) to make once the value that holds them is paired.
    pub(crate) fn defer(&mut self, pair: PairMaps, old: (Ptr, u64), new: (Ptr, u64)) {
        self.waiting.push((pair, old, new));
    }

    /// Pairs the maps queued by [`defer`](Pairing::defer), and those their values queue in turn,
    /// until none is left.
    ///
    /// # Errors
    ///
    /// The first error of a map's pairing.
    pub(crate) fn settle(&mut self) -> Result<()> {
        while let Some((pair, old, new)) = self.waiting.pop() {
            pair(self, old, new)?;
        }
        Ok(())
    }
}
