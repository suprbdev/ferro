//! What the code the [`Record`](derive@crate::Record) derive generates calls, beside the table
//! internals it reaches.
//!
//! The lookups each index's generated methods return, the sinks a write stages index entries in,
//! the fingerprints that tell a stored index's slot, and the [`Differ`] that the check that a
//! table's indexes agree with its rows reads them through. [`crate::__derive`] re-exports them,
//! which is the only path outside the crate that names them.

use std::any::{TypeId, type_name};
use std::borrow::Borrow;
use std::cell::Cell;
use std::cmp::Ordering;
use std::fmt;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

use super::{
    CoveredHolders, Entries, EqualStream, Holders, IndexEach, IndexIter, Matches, Record,
    TableData, agree, distinct, owned,
};
use crate::codec::{Data, Decoder, Description, Encoder};
use crate::error::{Error, Result};
use crate::map::{
    self, Changed, Differing, Edits, Map, Undo, Val, differ, unwound, walked, walked_until,
};
use crate::pool::{Latch, Pool};
use crate::store::Store;

/// The map of a table's rows by primary key, which [`TableData::rows`] returns.
pub type Rows<T> = Map<<T as Record>::Key, T>;

/// What [`TableData::write`] hands each changed row to, as the `old` and `new` that
/// [`reindex`](TableData::reindex) takes.
pub type Stage<'a, T> = dyn FnMut(Option<&T>, Option<&T>) + 'a;

/// A table as [`TableData::decode_stored`] reads it.
pub struct Stored<D> {
    /// The table, with an empty map for each index it does not store under a fingerprint its
    /// type knows.
    pub table: D,
    /// Whether it stores each index under the fingerprint its type gives, in the order the
    /// derive declares them.
    pub held: Vec<bool>,
    /// Whether it holds an index under a fingerprint its type does not know.
    pub unknown: bool,
}

impl<D> fmt::Debug for Stored<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stored").finish_non_exhaustive()
    }
}

/// A row that changed between two versions of a table, as it was and as it is, `None` on the
/// side that does not hold it, which [`TableData::diff_slots`] takes the entries that left and
/// came from.
pub type RowChange<T> = (Option<T>, Option<T>);

/// What [`TableData::built_slots`] reads the rows of a table with: given a function, it calls it
/// on every row, and returns the first error either gives.
pub type EveryRow<'a, T> = dyn FnMut(&mut dyn FnMut(&T) -> Result<()>) -> Result<()> + 'a;

/// What [`IndexEach::delta`] finds: the elements that left, and those that came.
pub type Delta<X> = (Vec<X>, Vec<X>);

/// Diffs the maps of two versions of a table, which the check that the later one's indexes agree
/// with its rows compares, on the threads of a pool, reading through a store.
pub struct Differ<'a, 'p> {
    pub(super) pool: &'a Pool<'p>,
    pub(super) store: &'p Store,
}

impl fmt::Debug for Differ<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Differ").finish_non_exhaustive()
    }
}

/// A diff of two versions of an index that [`Differ::start`] started.
pub struct Started<K, V>(pub(super) Differing<K, V>);

impl<K, V> fmt::Debug for Started<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Started").finish_non_exhaustive()
    }
}

impl<'p> Differ<'_, 'p> {
    /// Starts the diff of `old` and `new`, two versions of one index, on the threads of the pool.
    pub fn start<K: Data + Ord, V: Data>(&self, old: &Map<K, V>, new: &Map<K, V>) -> Started<K, V> {
        Started(differ(self.pool, self.store, old, new))
    }

    /// Waits for the diff `started`, of an index that holds exactly the entries its rows give
    /// and a later version of it, and checks that it is exactly what taking `gone`, the entries
    /// of the rows that left or changed, out and putting `come`, those of the rows that came or
    /// changed, in changes: keys compared by `Ord` and values by their encoding. An entry that
    /// `gone` and `come` both hold with the same encoding stays.
    ///
    /// Returns [`Fault::Shared`] when two entries of `come` fall on one key, or one falls on a
    /// key that the earlier version holds and `gone` does not take out, as two rows that share a
    /// value of a unique index give, and [`Fault::Differs`] for any other difference.
    ///
    /// # Errors
    ///
    /// The error of a node of either version that cannot be read or decoded.
    pub fn index<K: Data + Ord, V: Data>(
        &self,
        started: Started<K, V>,
        gone: Vec<(K, V)>,
        come: Vec<(K, V)>,
    ) -> Result<Option<Fault>> {
        Ok(diff_index(started.0.wait(self.pool)?, gone, come))
    }

    /// The keys whose values differ between `old` and `new`, two versions of a map, with the
    /// value each held and holds, as [`Differ::start`] diffs an index, waited for.
    pub(crate) fn changed<K: Data + Ord, V: Data>(
        &self,
        old: &Map<K, V>,
        new: &Map<K, V>,
    ) -> Result<Changed<K, V>> {
        differ(self.pool, self.store, old, new).wait(self.pool)
    }

    /// Every key of `map`, in order, read as [`walked`] reads it.
    pub(crate) fn keys<K: Data + Ord, V: Data>(&self, map: &Map<K, V>) -> Result<Vec<K>> {
        let mut keys = Vec::with_capacity(map.len().min(1 << 16));
        walked(self.pool, self.store, map, |key, _| {
            keys.push(key);
            Ok(())
        })?;
        Ok(keys)
    }

    /// Calls `f` on every key and value of `map` in key order, read as [`walked`] reads them.
    pub(super) fn walked<K: Data + Ord, V: Data>(
        &self,
        map: &Map<K, V>,
        f: impl FnMut(K, V) -> Result<()>,
    ) -> Result<()> {
        walked(self.pool, self.store, map, f)
    }

    /// Queues on the threads of the pool the check that `index` holds exactly `entries`: the same
    /// keys, by `Ord`, with values that encode the same. With fewer than 4,096 entries it makes
    /// the check here, which costs less than handing it to another thread.
    pub fn verify<K: Data + Ord, V: Data>(
        &self,
        index: &Map<K, V>,
        entries: Vec<(K, V)>,
    ) -> Verifying {
        if entries.len() < QUEUED_ENTRIES {
            return Verifying(Arc::new(Told {
                latch: Latch::new(0),
                found: parking_lot::Mutex::new(Some(matched(
                    self.pool, self.store, index, entries,
                ))),
            }));
        }
        let told = Arc::new(Told {
            latch: Latch::new(1),
            found: parking_lot::Mutex::new(None),
        });
        let (index, store, answer) = (index.clone(), self.store, told.clone());
        self.pool.queue(Box::new(move |pool: &Pool<'p>| {
            let found = unwound("checking an index", || {
                matched(pool, store, &index, entries)
            })
            .and_then(|found| found);
            *answer.found.lock() = Some(found);
            answer.latch.done(pool);
        }));
        Verifying(told)
    }

    /// Waits for the check `verifying`, running the work queued on the pool meanwhile, and
    /// returns how the index differs from its entries: [`Fault::Shared`] when two of the entries
    /// fall on one key, as two rows that share a value of a unique index give, and
    /// [`Fault::Differs`] for any other difference.
    ///
    /// # Errors
    ///
    /// The error of a node of the index that cannot be read or decoded.
    pub fn verified(&self, verifying: Verifying) -> Result<Option<Fault>> {
        self.pool.help_until(false, || verifying.0.latch.is_open());
        verifying
            .0
            .found
            .lock()
            .take()
            .unwrap_or(Err(Error::Detached))
    }
}

/// The entries past which [`Differ::verify`] hands its check to the threads of the pool.
const QUEUED_ENTRIES: usize = 1 << 12;

/// A check that an index holds exactly the entries of its rows, which [`Differ::verify`] queued.
pub struct Verifying(Arc<Told>);

impl fmt::Debug for Verifying {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Verifying").finish_non_exhaustive()
    }
}

struct Told {
    latch: Latch,
    found: parking_lot::Mutex<Option<Result<Option<Fault>>>>,
}

/// The check [`Differ::verify`] makes.
fn matched<K: Data + Ord, V: Data>(
    pool: &Pool<'_>,
    store: &Store,
    index: &Map<K, V>,
    mut entries: Vec<(K, V)>,
) -> Result<Option<Fault>> {
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    if entries
        .windows(2)
        .any(|pair| pair[0].0.cmp(&pair[1].0).is_eq())
    {
        return Ok(Some(Fault::Shared));
    }
    if entries.len() != index.len() {
        return Ok(Some(Fault::Differs));
    }
    let mut expected = entries.into_iter();
    let mut differs = false;
    let walked = walked(pool, store, index, |key, value| match expected.next() {
        Some((held, should)) if held.cmp(&key).is_eq() && V::same_encoding(&value, &should) => {
            Ok(())
        }
        _ => {
            differs = true;
            Err(Error::Detached)
        }
    });
    if differs {
        return Ok(Some(Fault::Differs));
    }
    walked?;
    Ok(expected.next().map(|_| Fault::Differs))
}

/// How an index differs from what the rows that changed give, as [`Differ::index`] finds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Two rows share a value of a unique index: an entry of the rows that came falls on a key
    /// that another of them, or a row that did not change, holds.
    Shared,
    /// The index changed by anything else than the entries of the rows that left and came.
    Differs,
}

/// The check [`Differ::index`] makes of `found`, the diff it waited for.
fn diff_index<K: Data + Ord, V: Data>(
    found: Changed<K, V>,
    mut gone: Vec<(K, V)>,
    mut come: Vec<(K, V)>,
) -> Option<Fault> {
    gone.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    come.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let repeats = |entries: &[(K, V)]| entries.windows(2).any(|pair| pair[0].0 == pair[1].0);
    if repeats(&come) {
        return Some(Fault::Shared);
    }
    if repeats(&gone) {
        return Some(Fault::Differs);
    }
    let same = |found: &Option<V>, expected: Option<&V>| match (found, expected) {
        (None, None) => true,
        (Some(found), Some(expected)) => V::same_encoding(found, expected),
        _ => false,
    };
    let mut changes = expected(&gone, &come);
    let agrees = found.iter().all(|(key, before, after)| {
        changes.next().is_some_and(|(other, out, into)| {
            other == key && same(before, out) && same(after, into)
        })
    });
    if agrees && changes.next().is_none() {
        return None;
    }
    let mut changes = expected(&gone, &come).peekable();
    for (key, before, _) in &found {
        while changes.next_if(|(other, ..)| *other < key).is_some() {}
        if let Some((_, None, Some(_))) = changes.next_if(|(other, ..)| *other == key)
            && before.is_some()
        {
            return Some(Fault::Shared);
        }
    }
    Some(Fault::Differs)
}

/// The changes that taking `gone` out of an index and putting `come` in make, both sorted by key,
/// in key order: each key with the value it loses and the one it gains, leaving out a key that
/// both hold with the same encoding.
fn expected<'e, K: Ord, V: Data>(
    gone: &'e [(K, V)],
    come: &'e [(K, V)],
) -> impl Iterator<Item = (&'e K, Option<&'e V>, Option<&'e V>)> + 'e {
    let (mut gone, mut come) = (gone.iter().peekable(), come.iter().peekable());
    std::iter::from_fn(move || {
        loop {
            let order = match (gone.peek(), come.peek()) {
                (Some(out), Some(into)) => out.0.cmp(&into.0),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => return None,
            };
            match order {
                Ordering::Less => return gone.next().map(|(key, out)| (key, Some(out), None)),
                Ordering::Greater => {
                    return come.next().map(|(key, into)| (key, None, Some(into)));
                }
                Ordering::Equal => {
                    let ((_, out), (key, into)) = (gone.next()?, come.next()?);
                    if !V::same_encoding(out, into) {
                        return Some((key, Some(out), Some(into)));
                    }
                }
            }
        }
    })
}

/// Reads a table value of `T` with `read` and, when `r` belongs to a typed check that collects
/// tables, notes it for the check that its indexes agree with its rows.
///
/// The [`Record`](derive@crate::Record) derive calls it from the `decode` and `visit` of every
/// table, with or without indexes.
///
/// # Errors
///
/// Returns the error of `read`.
pub fn noted<'a, T: Record, R>(
    r: &mut Decoder<'a>,
    read: impl FnOnce(&mut Decoder<'a>) -> Result<R>,
) -> Result<R> {
    let start = r.checkpoint();
    let value = read(r)?;
    r.found_table(
        start,
        TypeId::of::<T::Table>(),
        type_name::<T::Table>(),
        agree::<T>,
        !T::Table::indexing_reads(),
    );
    Ok(value)
}

/// Builds an index entry's covered fields `C` from its indexed value `I` and the fields `S` it
/// stores.
///
/// The [`Record`](derive@crate::Record) derive generates one for every index with a `cover(...)`
/// list, so that covered fields the index is keyed on come from the entry rather than being stored
/// twice.
pub type Project<I, S, C> = fn(&I, S) -> C;

/// The [`Project`] of an index that covers nothing.
pub fn plain<I>(_value: &I, _stored: ()) {}

/// Calls `f` on each value an index holds for the row, until `f` returns `true`, and returns
/// whether it did.
///
/// The [`Record`](derive@crate::Record) derive generates one for every index and passes it to
/// the range and prefix lookups, so that [`Matches::and`] can test a row against a lookup without
/// reading the lookup's entries: one value for most indexes, each element for an
/// `#[index(each)]`, and none for a unique `Option` field that holds `None`. It is for the code
/// the derive generates, and changes with it.
pub type Indexed<T, I> = fn(&T, &mut dyn FnMut(&I) -> bool) -> bool;

/// Whether a covered field is unchanged and its index entry can stay as it is.
///
/// A field counts as unchanged only when `==` holds and both values encode to the same bytes, so
/// a float that turns from `0.0` to `-0.0` is rewritten.
pub fn same_stored<D: Data + PartialEq>(old: &D, new: &D) -> bool {
    old == new && old.same_encoding(new)
}

/// Where the code the [`Record`](derive@crate::Record) derive generates writes the entries of one
/// index for a changed row: the index's map itself, for
/// [`reindex`](TableData::reindex), the [`Edits`] that [`write`](TableData::write) stages, or
/// the [`Fresh`] that [`add`](TableData::add) puts them in through.
pub trait IndexSink<K, V> {
    /// Removes the entry under `key`.
    fn remove(&mut self, key: K);
    /// Removes the entry under `key`, borrowed.
    fn remove_ref(&mut self, key: &K);
    /// Stores `value` under `key`.
    fn insert(&mut self, key: K, value: V);
}

impl<K: Data + Ord, V: Data> IndexSink<K, V> for Map<K, V> {
    #[inline]
    fn remove(&mut self, key: K) {
        Map::remove(self, &key);
    }

    #[inline]
    fn remove_ref(&mut self, key: &K) {
        Map::remove(self, key);
    }

    #[inline]
    fn insert(&mut self, key: K, value: V) {
        Map::insert(self, key, value);
    }
}

impl<K: Data + Ord, V: Data> IndexSink<K, V> for Edits<K, V> {
    #[inline]
    fn remove(&mut self, key: K) {
        Edits::remove(self, key);
    }

    #[inline]
    fn remove_ref(&mut self, key: &K) {
        Edits::remove(self, key.clone());
    }

    #[inline]
    fn insert(&mut self, key: K, value: V) {
        Edits::insert(self, key, value);
    }
}

/// Where the code the [`Record`](derive@crate::Record) derive generates for
/// [`add`](TableData::add) puts the entry of one index for a row new to the table: straight into
/// the index, keeping what [`take_back`](Fresh::take_back) needs to take it out again.
///
/// It takes one entry, and no removals, as an index that gives a row at most one entry hands it
/// for a row that changes from `None`.
pub struct Fresh<'a, K, V> {
    map: &'a mut Map<K, V>,
    undo: Option<Undo<V, ()>>,
    back: bool,
}

impl<K, V> fmt::Debug for Fresh<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fresh").finish_non_exhaustive()
    }
}

impl<'a, K: Data + Ord, V: Data> Fresh<'a, K, V> {
    /// Puts the entry it is handed into `map`.
    #[inline]
    pub fn new(map: &'a mut Map<K, V>) -> Self {
        Fresh {
            map,
            undo: None,
            back: false,
        }
    }

    /// Takes back the entry put in, if any, while nothing else has changed the index since,
    /// which loads nothing. `entries` hands the same entry again, as it did to put it in.
    pub fn take_back(&mut self, entries: impl FnOnce(&mut Self)) {
        if self.undo.is_some() {
            self.back = true;
            entries(self);
        }
    }
}

impl<K: Data + Ord, V: Data> IndexSink<K, V> for Fresh<'_, K, V> {
    fn remove(&mut self, _: K) {
        unreachable!("a row new to the table removes no index entry");
    }

    fn remove_ref(&mut self, _: &K) {
        unreachable!("a row new to the table removes no index entry");
    }

    #[inline]
    fn insert(&mut self, key: K, value: V) {
        if !self.back {
            debug_assert!(self.undo.is_none(), "a Fresh takes one entry");
            self.undo = Some(self.map.put_undoable(key, Arc::new(value)));
        } else if let Some(undo) = self.undo.take() {
            self.map.undo_insert(&key, undo);
        }
    }
}

const FINGERPRINT_BYTES: usize = 8;

/// The fingerprint of an index, from a description of what decides its encoding and meaning.
///
/// The [`Record`](derive@crate::Record) derive describes each index by its kind, the key's type,
/// the stored names and types of the fields it is keyed on and of the fields it stores, and the
/// defaults of those fields. A stored index whose fingerprint differs from the struct's is rebuilt
/// from the rows when the table is read, as
/// [the schema evolution guide](https://github.com/suprbdev/ferro/blob/main/docs/guide/schema-evolution.md)
/// describes.
pub fn index_fingerprint(describe: impl FnOnce(&mut Description)) -> u64 {
    let mut d = Description::new();
    d.word("index");
    describe(&mut d);
    d.fingerprint()
}

/// The fingerprints of a table's `N` indexes, which tell a stored index's slot when a table is
/// decoded.
///
/// The [`Record`](derive@crate::Record) derive builds one per table type, once per process.
pub struct Fingerprints<const N: usize> {
    values: [u64; N],
    order: [usize; N],
}

impl<const N: usize> fmt::Debug for Fingerprints<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fingerprints").finish_non_exhaustive()
    }
}

impl<const N: usize> Fingerprints<N> {
    /// Collects the fingerprints of `table`'s indexes, one per slot.
    ///
    /// # Panics
    ///
    /// Panics, naming `table`, if two indexes have the same fingerprint.
    #[must_use]
    pub fn new(table: &str, values: [u64; N]) -> Self {
        let mut order: [usize; N] = std::array::from_fn(|slot| slot);
        order.sort_unstable_by_key(|slot| values[*slot]);
        assert!(
            order
                .windows(2)
                .all(|pair| values[pair[0]] != values[pair[1]]),
            "ferro: two indexes of {table} have the same fingerprint"
        );
        Fingerprints { values, order }
    }

    /// The fingerprint of the index in `slot`.
    ///
    /// # Panics
    ///
    /// Panics if `slot` is not below `N`.
    #[must_use]
    pub fn get(&self, slot: usize) -> u64 {
        self.values[slot]
    }

    /// The slots in ascending order of fingerprint, which is the order a table encodes its
    /// indexes in.
    #[must_use]
    pub fn order(&self) -> &[usize; N] {
        &self.order
    }

    /// Reads the fingerprint a stored index starts with and returns the slot it belongs to.
    ///
    /// Returns `None` when fewer than eight bytes remain, or when no index of this table has that
    /// fingerprint, which marks the stored index as stale.
    pub fn slot(&self, r: &mut Decoder<'_>) -> Option<usize> {
        if r.remaining() < FINGERPRINT_BYTES {
            return None;
        }
        let mut stored = [0u8; FINGERPRINT_BYTES];
        stored.copy_from_slice(r.take(FINGERPRINT_BYTES).ok()?);
        let stored = u64::from_le_bytes(stored);
        self.values.iter().position(|value| *value == stored)
    }
}

/// Writes an index as its eight-byte fingerprint followed by its map.
///
/// # Errors
///
/// Returns the error encoding the map returns.
pub fn encode_index<M: Data>(fingerprint: u64, index: &M, w: &mut Encoder<'_>) -> Result<()> {
    w.raw(&fingerprint.to_le_bytes());
    index.encode(w)
}

thread_local! {
    static REBUILDING: Cell<bool> = const { Cell::new(false) };
}

/// Marks this thread as rebuilding the indexes of a table until it drops.
struct Rebuilding;

impl Rebuilding {
    fn start() -> Rebuilding {
        REBUILDING.set(true);
        Rebuilding
    }
}

impl Drop for Rebuilding {
    fn drop(&mut self) {
        REBUILDING.set(false);
    }
}

/// Every row in key order, for rebuilding a stale index on decode.
///
/// The rows are read through the cache, where they stay. A table that decodes while its thread
/// already reads rows for a rebuild, because a row holds it, reads its own rows from the file
/// without caching them and without rebuilding the indexes of the tables they hold, which its
/// index entries do not need, so that tables nested in rows, however deep, rebuild one level at a
/// time rather than all within one decode.
///
/// # Errors
///
/// Returns a read or decode error where a plain read would panic, so a damaged row node fails the
/// open instead, and [`Error::Decode`] for a row stored under a key other than its own, which an
/// index built from it would file under the wrong key.
pub fn stored_rows<T: Record>(rows: &Map<T::Key, T>) -> Result<Vec<Val<T>>> {
    if REBUILDING.get() {
        let store = rows.store().ok_or(Error::Detached)?;
        let mut out = Vec::with_capacity(rows.len().min(1 << 16));
        walked_until(
            || false,
            store,
            rows,
            |key, row| {
                owned::<T>(&key, &row)?;
                out.push(Val::from(Arc::new(row)));
                Ok(())
            },
        )?;
        return Ok(out);
    }
    let _rebuilding = Rebuilding::start();
    rows.try_keyed_values()?
        .into_iter()
        .map(|(key, row)| {
            owned::<T>(&key, &row)?;
            Ok(row)
        })
        .collect()
}

/// The row whose unique index entry is `value`. Backs a unique index's generated `by_x`.
pub fn unique_eq<I, T, Q>(
    rows: &Map<T::Key, T>,
    index: &Map<I, T::Key>,
    value: &Q,
) -> Option<Val<T>>
where
    I: Data + Ord + Borrow<Q>,
    T: Record,
    Q: Ord + ?Sized,
{
    rows.get(&*index.get(value)?)
}

/// The value a unique index holds for a row's field, or `None` for a value that stands for no
/// value, as `None` does, which the index leaves out.
///
/// It is for the code the [`Record`](derive@crate::Record) derive generates.
#[inline]
pub fn present<V: Data>(value: &V) -> Option<V> {
    (!value.absent()).then(|| value.clone())
}

/// The entry of a unique index on a field written `Option<X>` whose value is `Some(value)`, where
/// `value` may be borrowed as `X` is. Backs that index's generated `by_x`, `key_by_x` and
/// `covered_by_x`.
///
/// It is for the code the [`Record`](derive@crate::Record) derive generates.
pub fn unique_some<X, V, Q>(index: &Map<Option<X>, V>, value: &Q) -> Option<Val<V>>
where
    X: Data + Ord + Borrow<Q>,
    V: Data,
    Q: Ord + ?Sized,
{
    index
        .range_by(
            |entry| entry.as_ref().is_none_or(|x| x.borrow() < value),
            |entry| entry.as_ref().is_none_or(|x| x.borrow() <= value),
        )
        .next()
        .map(|(_, held)| held)
}

/// The entries of a unique index on a field written `Option<X>` whose value is `Some(x)` for an
/// `x` in `range`, in value order, where the bounds may be borrowed as `X` is. Backs that index's
/// generated `by_x_range`.
///
/// It is for the code the [`Record`](derive@crate::Record) derive generates.
pub fn unique_some_range<X, V, Q, R>(index: &Map<Option<X>, V>, range: R) -> map::Iter<Option<X>, V>
where
    X: Data + Ord + Borrow<Q>,
    V: Data,
    Q: Ord + ?Sized,
    R: RangeBounds<Q>,
{
    let start = range.start_bound();
    let end = range.end_bound();
    index.range_by(
        |entry| match entry {
            None => true,
            Some(x) => match start {
                Bound::Included(bound) => x.borrow() < bound,
                Bound::Excluded(bound) => x.borrow() <= bound,
                Bound::Unbounded => false,
            },
        },
        |entry| match entry {
            None => true,
            Some(x) => match end {
                Bound::Included(bound) => x.borrow() <= bound,
                Bound::Excluded(bound) => x.borrow() < bound,
                Bound::Unbounded => true,
            },
        },
    )
}

/// The rows of the unique index entries that `entries` walks, in index order, from either end.
/// Backs a unique index's generated `by_x_range` and, for a composite, `by_a_b_prefix`.
///
/// Each entry of a unique index without a cover list holds the key of the row that holds its
/// value, which the lookup reads in place to find the row. `indexed` gives the value a row holds,
/// so that [`Matches::and`] can test a row against the lookup's span.
///
/// It is for the code the [`Record`](derive@crate::Record) derive generates, and its signature
/// changes with the derive's, so call the generated lookup rather than this.
#[inline(always)]
pub fn unique_rows<I, T>(
    rows: &Map<T::Key, T>,
    entries: map::Iter<I, T::Key>,
    indexed: Indexed<T, I>,
) -> IndexIter<T>
where
    I: Data + Ord,
    T: Record,
{
    IndexIter::spanned(rows, Holders { iter: entries }, indexed)
}

/// [`unique_rows`] for a unique index with a cover list, whose entries hold the key beside the
/// stored fields that `project` turns into the covered ones.
///
/// It is for the code the [`Record`](derive@crate::Record) derive generates, and its signature
/// changes with the derive's, so call the generated lookup rather than this.
#[inline(always)]
pub fn unique_rows_covered<I, T, S, C>(
    rows: &Map<T::Key, T>,
    entries: map::Iter<I, (T::Key, S)>,
    project: fn(S) -> C,
    indexed: Indexed<T, I>,
) -> IndexIter<T, C>
where
    I: Data + Ord,
    T: Record,
    S: Data,
    C: 'static,
{
    let cursor = CoveredHolders {
        iter: entries,
        project,
    };
    IndexIter::spanned(rows, cursor, indexed)
}

/// The rows of the index entries whose value lies in the span `below` and `within` mark out, as
/// [`Map::range_by`] takes them. Backs a composite index's generated `by_a_b_prefix`.
///
/// `indexed` gives the value a row holds, so that [`Matches::and`] can test a row against the
/// lookup's span.
///
/// It is for the code the [`Record`](derive@crate::Record) derive generates, and its signature
/// changes with the derive's, so call the generated lookup rather than this.
#[inline(always)]
pub fn index_where<I, T, S, C>(
    rows: &Map<T::Key, T>,
    index: &Map<(I, T::Key), S>,
    project: Project<I, S, C>,
    below: impl Fn(&I) -> bool,
    within: impl Fn(&I) -> bool,
    indexed: Indexed<T, I>,
) -> IndexIter<T, C>
where
    I: Data + Ord,
    T: Record,
    S: Data,
    C: 'static,
{
    let cursor = Entries {
        iter: index.range_by(|entry| below(&entry.0), |entry| within(&entry.0)),
        project,
    };
    IndexIter::spanned(rows, cursor, indexed)
}

/// The rows whose indexed value equals `value`, in key order. Backs the generated `by_x`.
#[inline(always)]
pub fn index_eq<I, T, S, C, Q>(
    rows: &Map<T::Key, T>,
    index: &Map<(I, T::Key), S>,
    project: Project<I, S, C>,
    value: &Q,
) -> IndexIter<T, C>
where
    I: Data + Ord + Borrow<Q>,
    T: Record,
    S: Data,
    C: 'static,
    Q: Ord + ?Sized,
{
    IndexIter::over(
        rows,
        index.range_by(
            |entry| entry.0.borrow() < value,
            |entry| entry.0.borrow() <= value,
        ),
        project,
    )
}

/// The rows whose `Option` index holds `Some(value)`, in key order, where `value` may be
/// borrowed. Backs the generated `by_x_some`.
#[inline(always)]
pub fn index_eq_some<X, T, S, C, Q>(
    rows: &Map<T::Key, T>,
    index: &Map<(Option<X>, T::Key), S>,
    project: Project<Option<X>, S, C>,
    value: &Q,
) -> IndexIter<T, C>
where
    X: Data + Ord + Borrow<Q>,
    T: Record,
    S: Data,
    C: 'static,
    Q: Ord + ?Sized,
{
    IndexIter::over(
        rows,
        index.range_by(
            |entry| entry.0.as_ref().is_none_or(|x| x.borrow() < value),
            |entry| entry.0.as_ref().is_none_or(|x| x.borrow() <= value),
        ),
        project,
    )
}

/// The rows whose indexed value lies in `range`, by value and then by key. Backs the generated
/// `by_x_range`.
///
/// `indexed` gives the values a row holds, so that [`Matches::and`] can test a row against the
/// range.
///
/// It is for the code the [`Record`](derive@crate::Record) derive generates, and its signature
/// changes with the derive's, so call the generated lookup rather than this.
#[inline(always)]
pub fn index_range<I, T, S, C, Q, R>(
    rows: &Map<T::Key, T>,
    index: &Map<(I, T::Key), S>,
    project: Project<I, S, C>,
    range: R,
    indexed: Indexed<T, I>,
) -> IndexIter<T, C>
where
    I: Data + Ord + Borrow<Q>,
    T: Record,
    S: Data,
    C: 'static,
    Q: Ord + ?Sized,
    R: RangeBounds<Q>,
{
    let start = range.start_bound();
    let end = range.end_bound();
    let cursor = Entries {
        iter: index.range_by(
            |entry| match start {
                Bound::Included(bound) => entry.0.borrow() < bound,
                Bound::Excluded(bound) => entry.0.borrow() <= bound,
                Bound::Unbounded => false,
            },
            |entry| match end {
                Bound::Included(bound) => entry.0.borrow() <= bound,
                Bound::Excluded(bound) => entry.0.borrow() < bound,
                Bound::Unbounded => true,
            },
        ),
        project,
    };
    IndexIter::spanned(rows, cursor, indexed)
}

fn past_cursor<K, A>(key: &K, after: &A) -> bool
where
    K: Borrow<A>,
    A: Ord + ?Sized,
{
    key.borrow() <= after
}

/// The rows whose indexed value equals `value` and whose key is greater than `after`, in key
/// order. Backs the generated `by_x_after`.
#[inline(always)]
pub fn index_after<I, T, S, C, Q, A>(
    rows: &Map<T::Key, T>,
    index: &Map<(I, T::Key), S>,
    project: Project<I, S, C>,
    value: &Q,
    after: &A,
) -> IndexIter<T, C>
where
    I: Data + Ord + Borrow<Q>,
    T: Record,
    T::Key: Borrow<A>,
    S: Data,
    C: 'static,
    Q: Ord + ?Sized,
    A: Ord + ?Sized,
{
    IndexIter::over(
        rows,
        index.range_by(
            |entry| match entry.0.borrow().cmp(value) {
                Ordering::Less => true,
                Ordering::Equal => past_cursor(&entry.1, after),
                Ordering::Greater => false,
            },
            |entry| entry.0.borrow() <= value,
        ),
        project,
    )
}

/// The rows whose `Option` index holds `Some(value)` and whose key is greater than `after`, in
/// key order. Backs the generated `by_x_some_after`.
#[inline(always)]
pub fn index_after_some<X, T, S, C, Q, A>(
    rows: &Map<T::Key, T>,
    index: &Map<(Option<X>, T::Key), S>,
    project: Project<Option<X>, S, C>,
    value: &Q,
    after: &A,
) -> IndexIter<T, C>
where
    X: Data + Ord + Borrow<Q>,
    T: Record,
    T::Key: Borrow<A>,
    S: Data,
    C: 'static,
    Q: Ord + ?Sized,
    A: Ord + ?Sized,
{
    IndexIter::over(
        rows,
        index.range_by(
            |entry| match entry.0.as_ref().map(|x| x.borrow().cmp(value)) {
                None | Some(Ordering::Less) => true,
                Some(Ordering::Equal) => past_cursor(&entry.1, after),
                Some(Ordering::Greater) => false,
            },
            |entry| entry.0.as_ref().is_none_or(|x| x.borrow() <= value),
        ),
        project,
    )
}

fn seeking<I, T, S, C>(
    rows: &Map<T::Key, T>,
    index: &Map<(I, T::Key), S>,
    entries: map::Iter<(I, T::Key), S>,
    project: Project<I, S, C>,
) -> Matches<T, C>
where
    I: Data + Ord,
    T: Record,
    S: Data,
    C: Send + 'static,
{
    Matches {
        rows: rows.finder(),
        more: None,
        stream: Box::new(EqualStream::new(index.clone(), entries, project)),
        started: false,
    }
}

/// [`index_eq`] as [`Matches`] that can seek within the value. Backs `by_x` on the `find()` view.
pub fn match_eq<I, T, S, C, Q>(
    rows: &Map<T::Key, T>,
    index: &Map<(I, T::Key), S>,
    project: Project<I, S, C>,
    value: &Q,
) -> Matches<T, C>
where
    I: Data + Ord + Borrow<Q>,
    T: Record,
    S: Data,
    C: Send + 'static,
    Q: Ord + ?Sized,
{
    let entries = index.range_by(
        |entry| entry.0.borrow() < value,
        |entry| entry.0.borrow() <= value,
    );
    seeking(rows, index, entries, project)
}

/// [`index_eq_some`] as [`Matches`]. Backs `by_x_some` on the `find()` view.
pub fn match_eq_some<X, T, S, C, Q>(
    rows: &Map<T::Key, T>,
    index: &Map<(Option<X>, T::Key), S>,
    project: Project<Option<X>, S, C>,
    value: &Q,
) -> Matches<T, C>
where
    X: Data + Ord + Borrow<Q>,
    T: Record,
    S: Data,
    C: Send + 'static,
    Q: Ord + ?Sized,
{
    let entries = index.range_by(
        |entry| entry.0.as_ref().is_none_or(|x| x.borrow() < value),
        |entry| entry.0.as_ref().is_none_or(|x| x.borrow() <= value),
    );
    seeking(rows, index, entries, project)
}

/// [`index_after`] as [`Matches`]. Backs `by_x_after` on the `find()` view.
pub fn match_after<I, T, S, C, Q, A>(
    rows: &Map<T::Key, T>,
    index: &Map<(I, T::Key), S>,
    project: Project<I, S, C>,
    value: &Q,
    after: &A,
) -> Matches<T, C>
where
    I: Data + Ord + Borrow<Q>,
    T: Record,
    T::Key: Borrow<A>,
    S: Data,
    C: Send + 'static,
    Q: Ord + ?Sized,
    A: Ord + ?Sized,
{
    let entries = index.range_by(
        |entry| match entry.0.borrow().cmp(value) {
            Ordering::Less => true,
            Ordering::Equal => past_cursor(&entry.1, after),
            Ordering::Greater => false,
        },
        |entry| entry.0.borrow() <= value,
    );
    seeking(rows, index, entries, project)
}

/// [`index_after_some`] as [`Matches`]. Backs `by_x_some_after` on the `find()` view.
pub fn match_after_some<X, T, S, C, Q, A>(
    rows: &Map<T::Key, T>,
    index: &Map<(Option<X>, T::Key), S>,
    project: Project<Option<X>, S, C>,
    value: &Q,
    after: &A,
) -> Matches<T, C>
where
    X: Data + Ord + Borrow<Q>,
    T: Record,
    T::Key: Borrow<A>,
    S: Data,
    C: Send + 'static,
    Q: Ord + ?Sized,
    A: Ord + ?Sized,
{
    let entries = index.range_by(
        |entry| match entry.0.as_ref().map(|x| x.borrow().cmp(value)) {
            None | Some(Ordering::Less) => true,
            Some(Ordering::Equal) => past_cursor(&entry.1, after),
            Some(Ordering::Greater) => false,
        },
        |entry| entry.0.as_ref().is_none_or(|x| x.borrow() <= value),
    );
    seeking(rows, index, entries, project)
}

/// Each distinct value of an index with the number of rows that hold it, in value order.
///
/// Takes one seek per distinct value and reads no rows. Backs the generated `by_x_counts`.
pub fn index_counts<I: Data + Ord, K: Data + Ord, S: Data>(
    index: &Map<(I, K), S>,
) -> Vec<(I, usize)> {
    let mut counts = Vec::new();
    let mut next = index.first();
    while let Some(((value, _), _)) = next {
        let count = index
            .range_by(|entry| entry.0 < value, |entry| entry.0 <= value)
            .len();
        next = index.range_by(|entry| entry.0 <= value, |_| true).next();
        counts.push((value, count));
    }
    counts
}

/// The elements of `field` that an `each` index holds an entry for, sorted and each once, read
/// through `differ` with [`IndexEach::read_values`] where the check that a table's indexes agree
/// with its rows reads them, and with [`IndexEach::try_values`] otherwise.
///
/// It is for the code the [`Record`](derive@crate::Record) derive generates.
///
/// # Errors
///
/// The error of a node of `field` that cannot be read or decoded.
pub fn each_values<E: IndexEach>(
    field: &E,
    differ: Option<&Differ<'_, '_>>,
) -> Result<Vec<E::Value>> {
    let values = match differ {
        Some(differ) => field.read_values(differ)?,
        None => field.try_values()?,
    };
    Ok(distinct(values))
}
