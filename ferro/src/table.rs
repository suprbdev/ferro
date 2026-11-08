//! Tables: keyed rows with secondary indexes that every write keeps up to date.
//!
//! A [`Table<T>`] holds rows of a struct that derives [`Data`](derive@crate::Data) and
//! [`Record`](derive@crate::Record). The field marked `#[key]` is the primary key. Every field
//! marked `#[index]` or `#[unique]`, and every combination of fields named by `#[index(a, b)]` or
//! `#[unique(a, b)]` on the struct, gets a secondary index, which [`insert`](Table::insert),
//! [`remove`](Table::remove), [`update`](Table::update) and the rest keep in step with the rows.
//! The derive generates the indexes and their lookups on a `{Name}Table` type that [`Table`]
//! derefs to, so the lookups are called on the table itself.
//!
//! A table is a value like the other collections: it nests in your database type, clones in O(1),
//! compares with `==` in O(1) by root hash, and works in memory without a file.
//!
//! ```
//! use ferro::{Data, Record, Table};
//!
//! #[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
//! enum Status {
//!     Pending,
//!     Shipped,
//! }
//!
//! #[derive(Data, Record, Clone, Debug, PartialEq)]
//! struct Order {
//!     #[key]
//!     id: u64,
//!     #[index]
//!     customer: String,
//!     #[index]
//!     status: Status,
//!     total: u64,
//! }
//!
//! let mut orders: Table<Order> = Table::new();
//! orders.insert(Order { id: 1, customer: "wile".into(), status: Status::Pending, total: 4999 });
//! orders.insert(Order { id: 2, customer: "road".into(), status: Status::Pending, total: 150 });
//! orders.insert(Order { id: 3, customer: "wile".into(), status: Status::Shipped, total: 900 });
//!
//! assert_eq!(orders.get(&2).unwrap().customer, "road");
//!
//! let wile: Vec<u64> = orders.by_customer("wile").map(|order| order.id).collect();
//! assert_eq!(wile, [1, 3]);
//!
//! orders.update(&1, |order| order.status = Status::Shipped);
//! assert_eq!(orders.by_status(&Status::Shipped).len(), 2);
//! assert_eq!(orders.by_status_counts(), [(Status::Pending, 1), (Status::Shipped, 2)]);
//! ```
//!
//! # Reading rows
//!
//! [`get`](Table::get), [`contains`](Table::contains), [`first`](Table::first) and
//! [`last`](Table::last) find one row in O(log n). [`iter`](Table::iter), [`range`](Table::range),
//! [`keys`](Table::keys) and [`prefix`](Table::prefix) walk rows in key order from either end, and
//! [`rows`](Table::rows) gives the underlying [`Map`] for `(key, row)` pairs and everything else a
//! map can do.
//!
//! # Index lookups
//!
//! A lookup such as `by_status(&value)`, `by_status_range(range)` or
//! `by_status_after(&value, &key)` returns an [`IndexIter`]. It finds its first entry in O(log n),
//! knows its length at once, and reads each row from the rows map as it yields it, as a [`Val`]
//! from `next` or lent from the node that holds it by [`IndexIter::next_ref`] and
//! [`IndexIter::visit`], which is cheaper when many threads read the same rows.
//! [`IndexIter::keys`] reads no rows at all, and on an index with `cover(...)`
//! [`IndexIter::covered`] returns the fields the index stores beside each entry, also without
//! reading a row. `by_x_counts()` counts the rows of each value with one seek per distinct value.
//! No query planning happens: you choose the index. The [`Record`](derive@crate::Record) derive
//! lists every lookup it generates, for each kind of index.
//!
//! # Combining lookups
//!
//! `find()` returns a view with the equality lookups of each non-unique index, which return
//! [`Matches`] instead of an [`IndexIter`]. [`Matches::and`] and [`Matches::or`] combine them by
//! primary key and read a row only for a key that matches, as described on [`Matches`]. They also
//! take a range or prefix lookup made on the table, against which a side with at most a quarter
//! of its keys tests each of its keys by reading that key's row, as [`IntoMatches`] describes.
//! Lookups of different tables, or of different versions of one, combine too, each row read from
//! a lookup that holds its key.
//!
//! # Unique indexes
//!
//! A unique index holds each value for one row at most. [`try_insert`](Table::try_insert) and
//! [`try_update`](Table::try_update) return [`Taken`] on a clash and leave the table as it was.
//! [`insert`](Table::insert), [`update`](Table::update) and [`get_mut`](Table::get_mut) have no
//! error channel, so they panic instead, which inside a commit discards the transaction.
//! [`merge`](Table::merge) passes a clash to its `resolve` closure and never panics on one.
//!
//! `by_x(&value)` returns the one row that holds a value. `by_x_range(range)` and, on a composite
//! `#[unique(a, b)]`, `by_a_b_prefix(&a)` return an [`IndexIter`] over the rows in value order, as
//! the lookups of a non-unique index do:
//!
//! ```
//! use ferro::{Data, Record, Table};
//!
//! #[derive(Data, Record, Clone)]
//! #[unique(room, night, cover(guest))]
//! struct Stay {
//!     #[key]
//!     id: u64,
//!     #[unique]
//!     reference: String,
//!     room: u16,
//!     night: u32,
//!     guest: String,
//! }
//!
//! let mut stays: Table<Stay> = Table::new();
//! for (id, room, night) in [(1, 101, 3), (2, 102, 1), (3, 101, 1), (4, 101, 2)] {
//!     let guest = format!("guest {id}");
//!     stays.insert(Stay { id, reference: format!("S{id}"), room, night, guest });
//! }
//!
//! let room: Vec<u64> = stays.by_room_night_prefix(&101).map(|stay| stay.id).collect();
//! assert_eq!(room, [3, 4, 1]);
//!
//! let guests: Vec<(u64, String)> = stays.by_room_night_range((101, 2)..).covered().collect();
//! assert_eq!(guests, [(4, "guest 4".into()), (1, "guest 1".into()), (2, "guest 2".into())]);
//!
//! let newest = stays.by_reference_range(String::from("S2")..).next_back();
//! assert_eq!(newest.map(|stay| stay.id), Some(4));
//! ```
//!
//! # Panics
//!
//! As with every collection, a read that cannot load a node from the file, or finds one damaged,
//! panics. [`Map::try_get`] on [`Table::rows`] returns that failure as an error instead.
//!
//! A write trusts that each index holds exactly the entries the rows give. Every write keeps it
//! so, and [`Db::receive`](crate::Db::receive) and [`Db::verify_typed`](crate::Db::verify_typed)
//! check it, so only a damaged file can break it. In such a table, lookups miss rows or return
//! others, and [`insert`](Table::insert) or [`update`](Table::update) can panic on a value that
//! no row holds, as they do on a clash, where `try_insert` returns that clash as a [`Taken`].
//!
//! [The tables guide](https://github.com/archlemon/ferro/blob/main/docs/guide/tables.md) describes
//! tables and indexes at more length, and
//! [the schema and unique index internals](https://github.com/archlemon/ferro/blob/main/docs/internals/schema-and-unique-indexes.md)
//! how they are stored.

use std::any::{TypeId, type_name};
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::convert::Infallible;
use std::fmt;
use std::ops::{Bound, Deref, DerefMut, RangeBounds};
use std::sync::Arc;

use crate::codec::{Data, Decoder, Description, Encoder, FoundTable, Hash};
use crate::error::{Error, Result};
use crate::map::{
    Changed, Conflict, Cursor, Diff, Differing, Finder, Iter, Keys, Map, Val, Values, differ,
    same_value, unwound, walked,
};
use crate::pool::{Latch, Pool};
use crate::prefix::Prefix;
use crate::store::{Store, fresh_reads};

/// A row type that can be stored in a [`Table`].
///
/// Derive it with [`Record`](derive@crate::Record) rather than implementing it by hand: the
/// derive also generates the [`TableData`] type that holds the rows and keeps the indexes.
pub trait Record: Data {
    /// The primary key, the type of the field marked `#[key]`.
    type Key: Data + Ord;
    /// The generated `{Name}Table` that holds the rows and every index of this row type.
    type Table: TableData<Self>;

    /// Returns a clone of this row's primary key.
    fn key(&self) -> Self::Key;
}

/// A unique index already holds a value for another row.
///
/// Returned by [`Table::try_insert`] and [`Table::try_update`], which leave the table unchanged
/// when they return it.
///
/// # Examples
///
/// ```
/// use ferro::{Data, Record, Table, Taken};
///
/// #[derive(Data, Record, Clone, Debug)]
/// struct User {
///     #[key]
///     id: u32,
///     #[unique]
///     email: String,
/// }
///
/// let mut users: Table<User> = Table::new();
/// users.insert(User { id: 1, email: "a@example.com".into() });
///
/// let clash = users.try_insert(User { id: 2, email: "a@example.com".into() });
/// assert_eq!(clash.unwrap_err(), Taken { index: "email", holder: 1 });
/// assert_eq!(users.len(), 1);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Taken<K> {
    /// The name of the unique index: the field's name, or the field names of a struct-level
    /// `#[unique(a, b)]` joined by `_`, as in `a_b`.
    pub index: &'static str,
    /// The key of the row that holds the value.
    pub holder: K,
}

impl<K: fmt::Debug> fmt::Display for Taken<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unique index {} already holds this value for {:?}",
            self.index, self.holder
        )
    }
}

impl<K: fmt::Debug> std::error::Error for Taken<K> {}

/// The storage behind a [`Table`]: its rows and the indexes kept beside them.
///
/// [`Record`](derive@crate::Record) implements this for the `{Name}Table` type it generates, which
/// also carries the lookup methods that [`Table`] reaches through `Deref`. [`Table`] calls these
/// methods so that the rows and every index change together, so code outside a table has no need
/// to call them.
pub trait TableData<T: Record>: Data + Default {
    /// The rows by primary key.
    fn rows(&self) -> &Map<T::Key, T>;
    /// The rows by primary key, for writing. A change made here bypasses the indexes, so it must
    /// be followed by the matching [`reindex`](TableData::reindex).
    fn rows_mut(&mut self) -> &mut Map<T::Key, T>;
    /// Updates every index for a row that changes from `old` to `new`, where `None` means the row
    /// is absent on that side: `(None, Some(row))` adds a row, `(Some(row), None)` removes one.
    fn reindex(&mut self, old: Option<&T>, new: Option<&T>);
    /// Returns the clash `row` would cause in a unique index, if any.
    ///
    /// A value held by `row`'s own key does not clash, and neither does one held by `moving`, the
    /// key of a row that `row` replaces under a new key. The default, for a table without unique
    /// indexes, never clashes.
    fn conflict(&self, _row: &T, _moving: Option<&T::Key>) -> Option<Taken<T::Key>> {
        None
    }
    /// Reads a table as [`Data::decode`] does, without rebuilding an index it does not store
    /// under a fingerprint this type knows, and returns it with whether it stores each index, in
    /// the order the derive declares them, and whether it holds an index under a fingerprint
    /// this type does not know. The default reads it with [`Data::decode`] and lists none.
    ///
    /// # Errors
    ///
    /// As for [`Data::decode`].
    #[doc(hidden)]
    fn decode_stored(r: &mut Decoder<'_>) -> Result<Stored<Self>> {
        Ok(Stored {
            table: Self::decode(r)?,
            held: Vec::new(),
            unknown: false,
        })
    }
    /// Whether working out the entries a row gives reads the file, as an `each` index over a
    /// [`Set`](crate::Set) does. The default, for a table without such an index, is `false`.
    #[doc(hidden)]
    fn indexing_reads() -> bool {
        false
    }
    /// Checks that each index of `later` whose place in `slots` is `true` differs from the same
    /// index of `self` by exactly what the rows that `rows` returns as changed, each as it was
    /// and as it is, `None` where it is absent, change in its entries.
    ///
    /// It starts the diff of each such index with [`Differ::start`], then calls `rows`, then
    /// checks each index in turn with [`Differ::index`], so the diffs of the indexes and of the
    /// rows share the threads of `differ`. A row whose fields the index names, and stores, encode
    /// as they did changes nothing in it, and of an `each` field that changed only the elements
    /// that came and went, as [`IndexEach::delta`] finds them, change its entries.
    ///
    /// Returns the name of the first index that differs by anything else, with how.
    ///
    /// # Errors
    ///
    /// The error of `rows`, and of a node of either index, or of an `each` field, that cannot be
    /// read or decoded.
    #[doc(hidden)]
    fn diff_slots(
        &self,
        _later: &Self,
        _slots: &[bool],
        _differ: &Differ<'_, '_>,
        rows: &mut dyn FnMut() -> Result<Vec<RowChange<T>>>,
    ) -> Result<Option<(&'static str, Fault)>> {
        rows().map(|_| None)
    }
    /// Checks that each index of this table whose place in `slots` is `true` holds exactly the
    /// entries the rows give, where `rows` calls the function it is given on every row.
    ///
    /// It gathers the entries of each such index from the rows, then compares each index with
    /// them, sorted, on the threads of `differ`, as [`Differ::verify`] does.
    ///
    /// Returns the name of the first index that holds anything else, with how.
    ///
    /// # Errors
    ///
    /// The error of `rows`, and of a node of an index, or of an `each` field, that cannot be read
    /// or decoded.
    #[doc(hidden)]
    fn built_slots(
        &self,
        _slots: &[bool],
        _differ: &Differ<'_, '_>,
        rows: &mut EveryRow<'_, T>,
    ) -> Result<Option<(&'static str, Fault)>> {
        rows(&mut |_| Ok(())).map(|()| None)
    }
}

/// A table as [`TableData::decode_stored`] reads it.
#[doc(hidden)]
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

/// A row that changed between two versions of a table, as it was and as it is, `None` on the
/// side that does not hold it, which [`TableData::diff_slots`] takes the entries that left and
/// came from.
#[doc(hidden)]
pub type RowChange<T> = (Option<T>, Option<T>);

/// What [`TableData::built_slots`] reads the rows of a table with: given a function, it calls it
/// on every row, and returns the first error either gives.
#[doc(hidden)]
pub type EveryRow<'a, T> = dyn FnMut(&mut dyn FnMut(&T) -> Result<()>) -> Result<()> + 'a;

/// What [`IndexEach::delta`] finds: the elements that left, and those that came.
#[doc(hidden)]
pub type Delta<X> = (Vec<X>, Vec<X>);

/// Diffs the maps of two versions of a table, which the check that the later one's indexes agree
/// with its rows compares, on the threads of a pool, reading through a store.
#[doc(hidden)]
pub struct Differ<'a, 'p> {
    pool: &'a Pool<'p>,
    store: &'p Store,
}

/// A diff of two versions of an index that [`Differ::start`] started.
#[doc(hidden)]
pub struct Started<K, V>(Differing<K, V>);

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
    fn walked<K: Data + Ord, V: Data>(
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
#[doc(hidden)]
pub struct Verifying(Arc<Told>);

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
#[doc(hidden)]
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
#[doc(hidden)]
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

/// Checks that each index `table` stores under a fingerprint `T` knows holds exactly the entries
/// its rows give, and that each row is stored under its own key, reading both through `store`.
/// With `strict`, a table that holds an index under a fingerprint `T` does not know fails too,
/// since that index would be trusted unchecked once `T` gains an index with that fingerprint.
///
/// With `base`, a table of the same type whose indexes agree with its rows and which stores every
/// index `table` does, it diffs the rows of the two and each index of the two on the threads of
/// `pool`, and checks that each index changed by exactly the entries of the rows that changed,
/// which costs what changed. Without one it builds the indexes from every row.
fn agree<'p, T: Record>(
    table: &FoundTable,
    base: Option<&FoundTable>,
    store: &'p Store,
    pool: &Pool<'p>,
    strict: bool,
) -> Result<()> {
    let read = |found: &FoundTable| {
        T::Table::decode_stored(&mut Decoder::new(&found.body, &found.links, Some(store)))
    };
    let Stored {
        table,
        held: slots,
        unknown,
    } = read(table)?;
    if strict && unknown {
        return Err(Error::Bundle(format!(
            "a table it carries ({}) holds an index under a fingerprint this type does not know, \
             which nothing here can check",
            type_name::<T::Table>()
        )));
    }
    let differ = Differ { pool, store };
    if let Some(base) = base {
        let Stored {
            table: base, held, ..
        } = read(base)?;
        if slots.iter().zip(&held).all(|(slot, held)| !slot || *held) {
            return faulted(diffed::<T>(&base, &table, &slots, &differ)?);
        }
    }
    let found = table.built_slots(&slots, &differ, &mut |each| {
        differ.walked(table.rows(), |key, row| {
            owned::<T>(&key, &row)?;
            each(&row)
        })
    })?;
    faulted(found)
}

fn diffed<T: Record>(
    base: &T::Table,
    table: &T::Table,
    slots: &[bool],
    differ: &Differ<'_, '_>,
) -> Result<Option<(&'static str, Fault)>> {
    let mut rows = Some(differ.start(base.rows(), table.rows()));
    let mut moved = || -> Result<Vec<RowChange<T>>> {
        let started = rows.take().ok_or(Error::Detached)?;
        let changed = started.0.wait(differ.pool)?;
        let mut moved = Vec::with_capacity(changed.len());
        for (key, before, after) in changed {
            if let Some(after) = &after {
                owned::<T>(&key, after)?;
            }
            moved.push((before, after));
        }
        Ok(moved)
    };
    base.diff_slots(table, slots, differ, &mut moved)
}

fn faulted(found: Option<(&'static str, Fault)>) -> Result<()> {
    match found {
        None => Ok(()),
        Some((index, Fault::Shared)) => Err(Error::Decode(format!(
            "two rows share a value of unique index {index}"
        ))),
        Some((index, Fault::Differs)) => Err(Error::Decode(format!(
            "index {index} does not hold exactly the entries its rows give"
        ))),
    }
}

fn owned<T: Record>(key: &T::Key, row: &T) -> Result<()> {
    if row.key() != *key {
        return Err(Error::Decode(
            "a row is stored under a key other than its own".into(),
        ));
    }
    Ok(())
}

fn clash<K>(taken: Taken<K>) -> ! {
    panic!(
        "ferro: unique index {} already holds this value",
        taken.index
    )
}

/// Rows keyed by their `#[key]` field, with the secondary indexes their
/// [`Record`](derive@crate::Record) derive declares.
///
/// `Table<T>` derefs to the generated `{Name}Table`, so the index lookups the derive generates,
/// such as `by_status(&value)` and `find()`, are called on the table directly. Rows come back as
/// [`Val<T>`], which derefs to `T`.
///
/// Cloning a table is O(1) and `==` compares root hashes in O(1). An empty table needs no file, so
/// a table can be built and queried in memory as well as inside a database.
///
/// # Examples
///
/// ```
/// use ferro::{Data, Record, Table};
///
/// #[derive(Data, Record, Clone, Debug, PartialEq)]
/// struct Task {
///     #[key]
///     id: u32,
///     #[index]
///     owner: String,
///     done: bool,
/// }
///
/// let mut tasks: Table<Task> = Table::new();
/// tasks.insert(Task { id: 1, owner: "ana".into(), done: false });
/// tasks.insert(Task { id: 2, owner: "ben".into(), done: false });
///
/// let done = tasks.update(&2, |task| {
///     task.done = true;
///     task.owner.clone()
/// });
/// assert_eq!(done.as_deref(), Some("ben"));
/// assert_eq!(tasks.update(&9, |task| task.done = true), None);
///
/// let open: Vec<u32> = tasks.iter().filter(|task| !task.done).map(|task| task.id).collect();
/// assert_eq!(open, [1]);
/// assert_eq!(tasks.by_owner("ben").next().map(|task| task.id), Some(2));
/// ```
///
/// # Panics
///
/// A read that cannot load a node from the file, or finds one damaged, panics, as with every
/// collection. Writes that would break a unique index panic too, as described on
/// [`insert`](Table::insert).
pub struct Table<T: Record>(T::Table);

impl<T: Record> Clone for Table<T> {
    fn clone(&self) -> Self {
        Table(self.0.clone())
    }
}

impl<T: Record> Default for Table<T> {
    fn default() -> Self {
        Table(T::Table::default())
    }
}

impl<T: Record> Deref for Table<T> {
    type Target = T::Table;

    fn deref(&self) -> &T::Table {
        &self.0
    }
}

impl<T: Record> Table<T> {
    /// Creates an empty table.
    pub fn new() -> Self {
        Table::default()
    }

    /// The rows as a [`Map`] from key to row, for `(key, row)` pairs, cursors, folds, diffs and
    /// the other reads a map has.
    pub fn rows(&self) -> &Map<T::Key, T> {
        self.0.rows()
    }

    /// The hash of the rows' tree, or `None` for an empty table.
    ///
    /// Two tables with the same rows have the same root hash, whatever order the rows went in,
    /// so it can serve as an HTTP `ETag` for the whole table. The indexes are left out, since
    /// they follow from the rows.
    pub fn root_hash(&self) -> Option<Hash> {
        self.0.rows().root_hash()
    }

    /// The number of rows, in O(1).
    pub fn len(&self) -> usize {
        self.0.rows().len()
    }

    /// Whether the table has no rows.
    pub fn is_empty(&self) -> bool {
        self.0.rows().is_empty()
    }

    /// The row with primary key `key`, in O(log n).
    pub fn get<Q>(&self, key: &Q) -> Option<Val<T>>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.rows().get(key)
    }

    /// Whether a row has primary key `key`, in O(log n).
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.rows().contains_key(key)
    }

    /// Every row, in key order, from either end.
    pub fn iter(&self) -> Values<T::Key, T> {
        self.0.rows().values()
    }

    /// Every primary key, in order, from either end.
    pub fn keys(&self) -> Keys<T::Key, T> {
        self.0.rows().keys()
    }

    /// Calls `f` on every row in key order, lending each row instead of cloning a [`Val`].
    pub fn for_each(&self, mut f: impl FnMut(&T)) {
        self.0.rows().for_each(|_, row| f(row));
    }

    /// Calls `f` on every row whose key lies in `range`, in key order, lending each row.
    pub fn for_each_range<Q, R>(&self, range: R, mut f: impl FnMut(&T))
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        self.0.rows().for_each_range::<Q, R>(range, |_, row| f(row));
    }

    /// The rows whose key lies in `range`, in key order, from either end.
    ///
    /// Finding the start costs O(log n), and the iterator knows its length at once.
    pub fn range<Q, R>(&self, range: R) -> Values<T::Key, T>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        self.0.rows().range::<Q, R>(range).into_values()
    }

    /// The rows whose tuple key starts with `head`, in key order, from either end.
    ///
    /// The key must be a tuple of two to four fields, or a type of your own that implements
    /// [`Prefix`]. `head` can be borrowed, as `&str` for a `String` field. It costs what the same
    /// [`range`](Table::range) does.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Data, Record, Table};
    ///
    /// #[derive(Data, Record, Clone)]
    /// struct Posting {
    ///     #[key]
    ///     id: (String, u64),
    ///     amount: i64,
    /// }
    ///
    /// let mut postings: Table<Posting> = Table::new();
    /// postings.insert(Posting { id: ("cash".into(), 1), amount: 50 });
    /// postings.insert(Posting { id: ("bank".into(), 2), amount: -20 });
    /// postings.insert(Posting { id: ("cash".into(), 3), amount: -5 });
    ///
    /// let cash: i64 = postings.prefix("cash").map(|posting| posting.amount).sum();
    /// assert_eq!(cash, 45);
    /// ```
    pub fn prefix<Q>(&self, head: &Q) -> Values<T::Key, T>
    where
        T::Key: Prefix<Q>,
        Q: Ord + ?Sized,
    {
        self.0.rows().prefix(head).into_values()
    }

    /// The row with the smallest key.
    pub fn first(&self) -> Option<Val<T>> {
        self.0.rows().first().map(|(_, row)| row)
    }

    /// The row with the largest key.
    pub fn last(&self) -> Option<Val<T>> {
        self.0.rows().last().map(|(_, row)| row)
    }

    /// Inserts `row` under its key and updates every index, returning the row it replaced.
    ///
    /// # Panics
    ///
    /// Panics, before changing anything, if a unique index already holds one of `row`'s values
    /// for another key. Inside a commit that discards the transaction. Use
    /// [`try_insert`](Table::try_insert) to get the clash as a [`Taken`] instead. The index is
    /// trusted, so in a damaged file whose index holds a value that no row does, it panics for
    /// that value too, as the [module docs](crate::table#panics) describe.
    pub fn insert(&mut self, row: T) -> Option<Val<T>> {
        self.try_insert(row).unwrap_or_else(|taken| clash(taken))
    }

    /// Inserts `row` under its key and updates every index, returning the row it replaced.
    ///
    /// A row that replaces one with the same key may keep that row's unique values.
    ///
    /// # Errors
    ///
    /// Returns [`Taken`], and leaves the table as it was, if a unique index already holds one of
    /// `row`'s values for another key.
    pub fn try_insert(&mut self, row: T) -> std::result::Result<Option<Val<T>>, Taken<T::Key>> {
        if let Some(taken) = self.0.conflict(&row, None) {
            return Err(taken);
        }
        Ok(self.put(row))
    }

    fn put(&mut self, row: T) -> Option<Val<T>> {
        let key = row.key();
        let row = Arc::new(row);
        let old = self.0.rows_mut().insert_shared(key, row.clone());
        self.0.reindex(old.as_deref(), Some(&row));
        old
    }

    fn replace(&mut self, old: &T, row: T) {
        let old_key = old.key();
        let new_key = row.key();
        if old_key == new_key {
            self.0.reindex(Some(old), Some(&row));
            self.0.rows_mut().insert(new_key, row);
        } else {
            self.0.rows_mut().remove(&old_key);
            self.0.reindex(Some(old), None);
            self.put(row);
        }
    }

    /// Removes the row with primary key `key` and its index entries, returning the row.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<Val<T>>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let old = self.0.rows_mut().remove(key)?;
        self.0.reindex(Some(&old), None);
        Some(old)
    }

    /// A copy of the row with primary key `key` to change in place, written back with its
    /// indexes when the [`RowMut`] drops.
    ///
    /// Changing the key field moves the row to the new key, replacing any row already there. A panic
    /// while the [`RowMut`] is held writes nothing back.
    ///
    /// # Panics
    ///
    /// The [`RowMut`] panics when it drops if the changed row clashes in a unique index, and
    /// leaves the table as it was. Use [`try_update`](Table::try_update) to get the clash as a
    /// [`Taken`] instead.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<RowMut<'_, T>>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let old = self.0.rows().get(key)?;
        let row = (*old).clone();
        Some(RowMut {
            table: self,
            old,
            row: Some(row),
            made_panicking: std::thread::panicking(),
        })
    }

    /// Runs `f` on the row with primary key `key`, writes it back with its indexes, and returns
    /// what `f` returns, or `None` when no row has that key.
    ///
    /// `.ok_or(..)?` turns a missing row into an error of your own. Changing the key field moves
    /// the row to the new key, replacing any row already there.
    ///
    /// # Panics
    ///
    /// If `f` panics, the panic carries on and the table keeps the row as it was, whatever `f`
    /// changed before it. Panics, leaving the table as it was, if the changed row clashes in a
    /// unique index. Inside a commit that discards the transaction. Use
    /// [`try_update`](Table::try_update) to get the clash as a [`Taken`] instead.
    pub fn update<Q, R>(&mut self, key: &Q, f: impl FnOnce(&mut T) -> R) -> Option<R>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.try_update(key, f).unwrap_or_else(|taken| clash(taken))
    }

    /// Runs `f` on the row with primary key `key` and writes it back with its indexes, as
    /// [`update`](Table::update) does, returning `Ok(None)` when no row has that key.
    ///
    /// # Errors
    ///
    /// Returns [`Taken`] if the changed row clashes in a unique index. The table is left as it
    /// was and the changes `f` made are dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Data, Record, Table};
    ///
    /// #[derive(Data, Record, Clone)]
    /// struct Page {
    ///     #[key]
    ///     id: u32,
    ///     #[unique]
    ///     slug: String,
    /// }
    ///
    /// let mut pages: Table<Page> = Table::new();
    /// pages.insert(Page { id: 1, slug: "home".into() });
    /// pages.insert(Page { id: 2, slug: "about".into() });
    ///
    /// let renamed = pages.try_update(&2, |page| page.slug = "home".into());
    /// assert_eq!(renamed.unwrap_err().holder, 1);
    /// assert_eq!(pages.by_slug("about").map(|page| page.id), Some(2));
    /// ```
    pub fn try_update<Q, R>(
        &mut self,
        key: &Q,
        f: impl FnOnce(&mut T) -> R,
    ) -> std::result::Result<Option<R>, Taken<T::Key>>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let Some(old) = self.0.rows().get(key) else {
            return Ok(None);
        };
        let mut row = (*old).clone();
        let value = f(&mut row);
        if let Some(taken) = self.0.conflict(&row, Some(&old.key())) {
            return Err(taken);
        }
        self.replace(&old, row);
        Ok(Some(value))
    }

    /// Removes every row for which `keep` returns `false`, with its index entries.
    ///
    /// Reads every row once, in key order.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        let doomed: Vec<T::Key> = self
            .0
            .rows()
            .iter()
            .filter(|(_, row)| !keep(row))
            .map(|(key, _)| key)
            .collect();
        for key in doomed {
            self.remove(&key);
        }
    }

    /// Removes the rows whose key lies in `range` and returns them in key order.
    ///
    /// Cutting the rows out takes O(log n) nodes, but every removed row's index entries are
    /// removed one by one, so the whole costs O(rows removed).
    pub fn drain_range<Q, R>(&mut self, range: R) -> Vec<Val<T>>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        let drained: Vec<Val<T>> = self
            .0
            .rows_mut()
            .take_range::<Q, R>(range)
            .values()
            .collect();
        for row in &drained {
            self.0.reindex(Some(row), None);
        }
        drained
    }

    /// The rows added, removed and changed between this table and `newer`, in key order.
    ///
    /// Subtrees with the same hash are skipped, so a diff costs O(changes), not O(rows). Rows are
    /// compared by their encoding, not with `==`.
    pub fn diff(&self, newer: &Table<T>) -> Diff<T::Key, T> {
        self.0.rows().diff(newer.0.rows())
    }

    /// Merges into this table the changes `theirs` made since `base`, three-way, and returns how
    /// many keys it wrote or passed to `resolve`.
    ///
    /// A row that only `theirs` changed is taken from `theirs`. A key that both sides changed in
    /// different ways goes to `resolve` as a [`Conflict`] holding the `base`, `ours` and `theirs`
    /// rows, `None` where the row is absent, and `resolve` returns the row to keep, or `None` to
    /// remove it. A returned row with a key other than the conflict's keeps our row for that key.
    /// The cost is O(changes `theirs` made).
    ///
    /// A row that would clash in a unique index is held back until the rest of the diff is in,
    /// since a chain of renames clashes only on the way through. If it still clashes, it goes to
    /// `resolve` with `ours` set to the row holding the value. `resolve` is called at most once
    /// for each key, and a row that still clashes after that keeps our row for its key, so a merge
    /// never panics on a clash and the indexes always match the rows.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Data, Record, Table};
    ///
    /// #[derive(Data, Record, Clone, Debug, PartialEq)]
    /// struct Item {
    ///     #[key]
    ///     id: u32,
    ///     price: u32,
    /// }
    ///
    /// let mut base: Table<Item> = Table::new();
    /// base.insert(Item { id: 1, price: 10 });
    /// base.insert(Item { id: 2, price: 20 });
    ///
    /// let mut ours = base.clone();
    /// let mut theirs = base.clone();
    /// ours.update(&1, |item| item.price = 11);
    /// theirs.update(&1, |item| item.price = 12);
    /// theirs.remove(&2);
    ///
    /// let merged = ours.merge(&base, &theirs, |conflict| conflict.keep_ours());
    /// assert_eq!(merged, 2);
    /// assert_eq!(ours.get(&1).unwrap().price, 11);
    /// assert!(!ours.contains(&2));
    /// ```
    pub fn merge(
        &mut self,
        base: &Table<T>,
        theirs: &Table<T>,
        mut resolve: impl FnMut(Conflict<T::Key, T>) -> Option<T>,
    ) -> usize {
        let Ok(written) = self.try_merge(base, theirs, |conflict| {
            Ok::<_, Infallible>(resolve(conflict))
        });
        written
    }

    /// Merges as [`merge`](Table::merge) does, with a `resolve` that can fail.
    ///
    /// # Errors
    ///
    /// Returns the first error `resolve` returns, and leaves the table as it was before the
    /// merge.
    pub fn try_merge<E>(
        &mut self,
        base: &Table<T>,
        theirs: &Table<T>,
        resolve: impl FnMut(Conflict<T::Key, T>) -> std::result::Result<Option<T>, E>,
    ) -> std::result::Result<usize, E> {
        let kept = self.clone();
        let merged = self.merge_rows(base, theirs, resolve);
        if merged.is_err() {
            *self = kept;
        }
        merged
    }

    fn merge_rows<E>(
        &mut self,
        base: &Table<T>,
        theirs: &Table<T>,
        mut resolve: impl FnMut(Conflict<T::Key, T>) -> std::result::Result<Option<T>, E>,
    ) -> std::result::Result<usize, E> {
        let mut held = Pending::default();
        let mut written = 0;
        for change in base.0.rows().diff(theirs.0.rows()) {
            let key = change.key().clone();
            let before = change.before().cloned();
            let after = change.after().cloned();
            let ours = self.get(&key);
            if same_value(ours.as_ref(), after.as_ref()) {
                continue;
            }
            written += 1;
            let (row, asked) = if same_value(ours.as_ref(), before.as_ref()) {
                (after.map(|row| (*row).clone()), false)
            } else {
                let conflict = Conflict {
                    key: key.clone(),
                    base: before,
                    ours,
                    theirs: after,
                };
                (resolve(conflict)?, true)
            };
            match row {
                None => {
                    self.remove(&key);
                }
                Some(row) if row.key() != key => {}
                Some(row) if self.0.conflict(&row, None).is_none() => {
                    self.put(row);
                }
                Some(row) => held.hold(key, row, asked),
            }
        }
        held.queue.extend(held.rows.keys().cloned());
        loop {
            self.settle(&mut held);
            let Some((key, row)) = held.next_unasked() else {
                break;
            };
            let ours = self
                .0
                .conflict(&row, None)
                .and_then(|taken| self.get(&taken.holder));
            let resolved = resolve(Conflict {
                key: key.clone(),
                base: base.get(&key),
                ours,
                theirs: Some(Val::new(row)),
            })?;
            match resolved {
                None => {
                    self.remove(&key);
                    held.freed(&key);
                }
                Some(row) if row.key() == key => {
                    held.hold(key.clone(), row, true);
                    held.queue.insert(key);
                }
                Some(_) => {}
            }
        }
        Ok(written)
    }

    fn settle(&mut self, held: &mut Pending<T>) {
        while let Some(key) = held.queue.pop_first() {
            let Some((row, _)) = held.rows.get(&key) else {
                continue;
            };
            match self.0.conflict(row, None) {
                Some(taken) => held.waiting.entry(taken.holder).or_default().push(key),
                None => {
                    if let Some((row, _)) = held.rows.remove(&key) {
                        self.put(row);
                    }
                    held.freed(&key);
                }
            }
        }
    }
}

struct Pending<T: Record> {
    rows: BTreeMap<T::Key, (T, bool)>,
    unasked: BTreeSet<T::Key>,
    queue: BTreeSet<T::Key>,
    waiting: BTreeMap<T::Key, Vec<T::Key>>,
}

impl<T: Record> Default for Pending<T> {
    fn default() -> Self {
        Pending {
            rows: BTreeMap::new(),
            unasked: BTreeSet::new(),
            queue: BTreeSet::new(),
            waiting: BTreeMap::new(),
        }
    }
}

impl<T: Record> Pending<T> {
    fn hold(&mut self, key: T::Key, row: T, asked: bool) {
        if !asked {
            self.unasked.insert(key.clone());
        }
        self.rows.insert(key, (row, asked));
    }

    fn freed(&mut self, key: &T::Key) {
        if let Some(blocked) = self.waiting.remove(key) {
            self.queue.extend(blocked);
        }
    }

    fn next_unasked(&mut self) -> Option<(T::Key, T)> {
        while let Some(key) = self.unasked.pop_first() {
            if let Some((_, false)) = self.rows.get(&key)
                && let Some((row, _)) = self.rows.remove(&key)
            {
                return Some((key, row));
            }
        }
        None
    }
}

/// A row taken out of a [`Table`] by [`Table::get_mut`] to change in place.
///
/// It derefs to the row, mutably as well. When it drops it writes the row back and updates every
/// index the change touched. A changed key field moves the row to the new key, replacing any row
/// already there.
///
/// A panic that starts while it is held drops it without writing anything, so the table keeps the
/// old row rather than one changed half way. One made while its thread was already panicking,
/// inside a `Drop` that runs during unwinding, writes the row back as usual.
///
/// # Panics
///
/// Dropping it panics, and leaves the table as it was, if the changed row clashes in a unique
/// index. If its thread is already panicking it keeps the old row instead, since a second panic
/// would abort the process.
pub struct RowMut<'a, T: Record> {
    table: &'a mut Table<T>,
    old: Val<T>,
    row: Option<T>,
    made_panicking: bool,
}

impl<T: Record> Deref for RowMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.row.as_ref().expect("row present until drop")
    }
}

impl<T: Record> DerefMut for RowMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.row.as_mut().expect("row present until drop")
    }
}

impl<T: Record> Drop for RowMut<'_, T> {
    fn drop(&mut self) {
        let Some(row) = self.row.take() else {
            return;
        };
        let panicking = std::thread::panicking();
        if panicking && !self.made_panicking {
            return;
        }
        match self.table.0.conflict(&row, Some(&self.old.key())) {
            None => self.table.replace(&self.old, row),
            Some(_) if panicking => {}
            Some(taken) => clash(taken),
        }
    }
}

impl<T: Record> Data for Table<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        Ok(Table(T::Table::decode(r)?))
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        T::Table::visit(r)
    }

    fn missing() -> Option<Self> {
        Some(Table::default())
    }

    fn describe(d: &mut Description) {
        d.word("table");
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.0.same_encoding(&other.0)
    }
}

impl<T: Record> PartialEq for Table<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0.rows() == other.0.rows()
    }
}

impl<T: Record + fmt::Debug> fmt::Debug for Table<T>
where
    T::Key: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.rows().fmt(f)
    }
}

trait KeyCursor<K>: Send {
    fn forward(&mut self) -> Option<K>;
    fn backward(&mut self) -> Option<K>;
    fn left(&self) -> usize;
}

trait EntryCursor<K, C>: KeyCursor<K> {
    fn forward_covered(&mut self) -> Option<(K, C)>;
    fn backward_covered(&mut self) -> Option<(K, C)>;
    fn into_stream(self: Box<Self>) -> Box<dyn Stream<K, C>>
    where
        K: Ord + Send + 'static,
        C: Send + 'static;
}

fn listed<K, C>(mut entries: Box<dyn EntryCursor<K, C>>) -> Box<dyn Stream<K, C>>
where
    K: Ord + Send + 'static,
    C: Send + 'static,
{
    let left = entries.left();
    let collect: Collect<K, C> = Box::new(move || {
        let mut items = Vec::with_capacity(left);
        while let Some((key, covered)) = entries.forward_covered() {
            items.push((key, Some(covered)));
        }
        items
    });
    Box::new(Listed::pending(left, collect))
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

struct Entries<I, K, S, C> {
    iter: Iter<(I, K), S>,
    project: Project<I, S, C>,
}

impl<I: Data + Ord, K: Data + Ord, S: Data, C> KeyCursor<K> for Entries<I, K, S, C> {
    #[inline]
    fn forward(&mut self) -> Option<K> {
        self.iter.next_key_with(|entry| entry.1.clone())
    }

    #[inline]
    fn backward(&mut self) -> Option<K> {
        self.iter.next_back_key_with(|entry| entry.1.clone())
    }

    fn left(&self) -> usize {
        self.iter.len()
    }
}

impl<I: Data + Ord, K: Data + Ord, S: Data, C> EntryCursor<K, C> for Entries<I, K, S, C> {
    #[inline]
    fn forward_covered(&mut self) -> Option<(K, C)> {
        let project = self.project;
        self.iter
            .next_with(|entry, stored| (entry.1.clone(), project(&entry.0, stored.clone())))
    }

    #[inline]
    fn backward_covered(&mut self) -> Option<(K, C)> {
        let project = self.project;
        self.iter
            .next_back_with(|entry, stored| (entry.1.clone(), project(&entry.0, stored.clone())))
    }

    fn into_stream(self: Box<Self>) -> Box<dyn Stream<K, C>>
    where
        K: Ord + Send + 'static,
        C: Send + 'static,
    {
        listed(self)
    }
}

trait Span<I, K> {
    type Edge: Send;
    fn edges(&mut self) -> Option<[Self::Edge; 2]>;
    fn holds(edges: &[Self::Edge; 2], value: &I, key: &K) -> bool;
}

impl<I: Data + Ord, K: Data + Ord, S: Data, C> Span<I, K> for Entries<I, K, S, C> {
    type Edge = (I, K);

    fn edges(&mut self) -> Option<[(I, K); 2]> {
        let low = self.iter.front_key_with(Clone::clone)?;
        let high = self.iter.back_key_with(Clone::clone)?;
        Some([low, high])
    }

    fn holds([low, high]: &[(I, K); 2], value: &I, key: &K) -> bool {
        let above = match value.cmp(&low.0) {
            Ordering::Less => false,
            Ordering::Equal => *key >= low.1,
            Ordering::Greater => true,
        };
        above
            && match value.cmp(&high.0) {
                Ordering::Less => true,
                Ordering::Equal => *key <= high.1,
                Ordering::Greater => false,
            }
    }
}

struct Holders<I, K> {
    iter: Iter<I, K>,
}

impl<I: Data + Ord, K: Data + Ord> KeyCursor<K> for Holders<I, K> {
    #[inline]
    fn forward(&mut self) -> Option<K> {
        self.iter.next_with(|_, key| key.clone())
    }

    #[inline]
    fn backward(&mut self) -> Option<K> {
        self.iter.next_back_with(|_, key| key.clone())
    }

    fn left(&self) -> usize {
        self.iter.len()
    }
}

impl<I: Data + Ord, K: Data + Ord> EntryCursor<K, ()> for Holders<I, K> {
    #[inline]
    fn forward_covered(&mut self) -> Option<(K, ())> {
        self.forward().map(|key| (key, ()))
    }

    #[inline]
    fn backward_covered(&mut self) -> Option<(K, ())> {
        self.backward().map(|key| (key, ()))
    }

    fn into_stream(self: Box<Self>) -> Box<dyn Stream<K, ()>>
    where
        K: Ord + Send + 'static,
    {
        listed(self)
    }
}

impl<I: Data + Ord, K: Data + Ord> Span<I, K> for Holders<I, K> {
    type Edge = I;

    fn edges(&mut self) -> Option<[I; 2]> {
        let low = self.iter.front_key_with(Clone::clone)?;
        let high = self.iter.back_key_with(Clone::clone)?;
        Some([low, high])
    }

    fn holds([low, high]: &[I; 2], value: &I, _key: &K) -> bool {
        low <= value && value <= high
    }
}

struct CoveredHolders<I, K, S, C> {
    iter: Iter<I, (K, S)>,
    project: fn(S) -> C,
}

impl<I: Data + Ord, K: Data + Ord, S: Data, C> KeyCursor<K> for CoveredHolders<I, K, S, C> {
    #[inline]
    fn forward(&mut self) -> Option<K> {
        self.iter.next_with(|_, held| held.0.clone())
    }

    #[inline]
    fn backward(&mut self) -> Option<K> {
        self.iter.next_back_with(|_, held| held.0.clone())
    }

    fn left(&self) -> usize {
        self.iter.len()
    }
}

impl<I: Data + Ord, K: Data + Ord, S: Data, C> EntryCursor<K, C> for CoveredHolders<I, K, S, C> {
    #[inline]
    fn forward_covered(&mut self) -> Option<(K, C)> {
        let project = self.project;
        self.iter
            .next_with(|_, held| (held.0.clone(), project(held.1.clone())))
    }

    #[inline]
    fn backward_covered(&mut self) -> Option<(K, C)> {
        let project = self.project;
        self.iter
            .next_back_with(|_, held| (held.0.clone(), project(held.1.clone())))
    }

    fn into_stream(self: Box<Self>) -> Box<dyn Stream<K, C>>
    where
        K: Ord + Send + 'static,
        C: Send + 'static,
    {
        listed(self)
    }
}

impl<I: Data + Ord, K: Data + Ord, S: Data, C> Span<I, K> for CoveredHolders<I, K, S, C> {
    type Edge = I;

    fn edges(&mut self) -> Option<[I; 2]> {
        let low = self.iter.front_key_with(Clone::clone)?;
        let high = self.iter.back_key_with(Clone::clone)?;
        Some([low, high])
    }

    fn holds([low, high]: &[I; 2], value: &I, _key: &K) -> bool {
        low <= value && value <= high
    }
}

struct Spanned<T: Record, I, X> {
    cursor: X,
    indexed: Indexed<T, I>,
}

impl<T: Record, I, X: KeyCursor<T::Key>> KeyCursor<T::Key> for Spanned<T, I, X> {
    #[inline(always)]
    fn forward(&mut self) -> Option<T::Key> {
        self.cursor.forward()
    }

    #[inline(always)]
    fn backward(&mut self) -> Option<T::Key> {
        self.cursor.backward()
    }

    fn left(&self) -> usize {
        self.cursor.left()
    }
}

impl<T, I, X, C> EntryCursor<T::Key, C> for Spanned<T, I, X>
where
    T: Record,
    I: 'static,
    X: EntryCursor<T::Key, C> + Span<I, T::Key> + 'static,
{
    #[inline(always)]
    fn forward_covered(&mut self) -> Option<(T::Key, C)> {
        self.cursor.forward_covered()
    }

    #[inline(always)]
    fn backward_covered(&mut self) -> Option<(T::Key, C)> {
        self.cursor.backward_covered()
    }

    fn into_stream(self: Box<Self>) -> Box<dyn Stream<T::Key, C>>
    where
        T::Key: Ord + Send + 'static,
        C: Send + 'static,
    {
        Box::new(Ranged {
            spanned: Some(*self),
            rows: None,
            edges: None,
            listed: Listed::ready(VecDeque::new()),
            covering: false,
            lead: None,
            landed: None,
            cold: false,
            rowless: false,
            budget: 0,
            spent: 0,
            source: 0,
        })
    }
}

struct Uncovered<K, C>(Box<dyn EntryCursor<K, C>>);

impl<K, C> KeyCursor<K> for Uncovered<K, C> {
    fn forward(&mut self) -> Option<K> {
        self.0.forward()
    }

    fn backward(&mut self) -> Option<K> {
        self.0.backward()
    }

    fn left(&self) -> usize {
        self.0.left()
    }
}

impl<K, C: 'static> EntryCursor<K, ()> for Uncovered<K, C> {
    fn forward_covered(&mut self) -> Option<(K, ())> {
        self.0.forward().map(|key| (key, ()))
    }

    fn backward_covered(&mut self) -> Option<(K, ())> {
        self.0.backward().map(|key| (key, ()))
    }

    fn into_stream(self: Box<Self>) -> Box<dyn Stream<K, ()>>
    where
        K: Ord + Send + 'static,
    {
        listed(self)
    }
}

/// The rows of one index lookup, in index order: by indexed value, then by primary key.
///
/// Every lookup the [`Record`](derive@crate::Record) derive generates on a non-unique index, such
/// as `by_x`, `by_x_after`, `by_x_range`, `by_x_some` or `by_x_prefix`, returns one, and so do
/// `by_x_range` and `by_x_prefix` on a unique index, which holds one row per value. The rows of
/// one value come in key order, and the iterator runs from either end and knows its length as
/// soon as it is built. Each row is read from the rows map when it is yielded, through a finder
/// that keeps the last leaf it found, so rows close together in key order cost less.
///
/// A lookup reads the table as it was when the lookup was built. `C` holds the fields the index
/// covers, `()` for an index without `cover(...)`. [`keys`](IndexIter::keys) and
/// [`covered`](IndexIter::covered) read the index alone, and no row.
///
/// An `#[index(each)]` lookup over a range of elements yields a row once for each element of the
/// row that falls in the range.
///
/// # Examples
///
/// ```
/// use ferro::{Data, Record, Table};
///
/// #[derive(Data, Record, Clone)]
/// struct Booking {
///     #[key]
///     id: u64,
///     #[index(cover(nights, total))]
///     customer: u32,
///     nights: u32,
///     total: i64,
///     notes: String,
/// }
///
/// let mut bookings: Table<Booking> = Table::new();
/// bookings.insert(Booking { id: 1, customer: 7, nights: 2, total: 300, notes: String::new() });
/// bookings.insert(Booking { id: 2, customer: 9, nights: 3, total: 450, notes: String::new() });
/// bookings.insert(Booking { id: 3, customer: 7, nights: 1, total: 120, notes: String::new() });
///
/// assert_eq!(bookings.by_customer(&7).len(), 2);
/// assert_eq!(bookings.by_customer(&7).keys().collect::<Vec<_>>(), [1, 3]);
///
/// let newest: Vec<(u64, (u32, i64))> = bookings.by_customer(&7).covered().rev().collect();
/// assert_eq!(newest, [(3, (1, 120)), (1, (2, 300))]);
///
/// let busy: Vec<u64> = bookings.by_customer_range(8..).map(|booking| booking.id).collect();
/// assert_eq!(busy, [2]);
/// ```
pub struct IndexIter<T: Record, C = ()> {
    rows: Finder<T::Key, T>,
    entries: Box<dyn EntryCursor<T::Key, C>>,
}

impl<T: Record, C: 'static> IndexIter<T, C> {
    #[inline(always)]
    fn over<I: Data + Ord, S: Data>(
        rows: &Map<T::Key, T>,
        entries: Iter<(I, T::Key), S>,
        project: Project<I, S, C>,
    ) -> Self {
        IndexIter {
            rows: rows.finder(),
            entries: Box::new(Entries {
                iter: entries,
                project,
            }),
        }
    }

    #[inline(always)]
    fn held(rows: &Map<T::Key, T>, entries: impl EntryCursor<T::Key, C> + 'static) -> Self {
        IndexIter {
            rows: rows.finder(),
            entries: Box::new(entries),
        }
    }

    #[inline(always)]
    fn spanned<I: Data + Ord, X>(rows: &Map<T::Key, T>, cursor: X, indexed: Indexed<T, I>) -> Self
    where
        X: EntryCursor<T::Key, C> + Span<I, T::Key> + 'static,
    {
        IndexIter::held(rows, Spanned { cursor, indexed })
    }

    /// The primary keys of the rows this lookup would yield, in the same order, without reading
    /// any row.
    #[inline]
    pub fn keys(self) -> IndexKeys<T> {
        IndexKeys(self.entries)
    }

    /// `(key, fields)` pairs in the same order, where `fields` are the fields the index's
    /// `cover(...)` list names, read from the index without reading any row.
    ///
    /// One covered field comes back as itself and several as a tuple in the order listed.
    #[inline]
    pub fn covered(self) -> IndexCovered<T, C> {
        IndexCovered(self.entries)
    }

    /// The same lookup with the covered fields dropped from its type, so that lookups of
    /// differently covered indexes fit in one collection or one [`Matches::or`].
    ///
    /// A range or prefix lookup passed through it to [`Matches::and`] or [`Matches::or`] is read in
    /// full and sorted rather than tested key by key, as [`IntoMatches`] describes, so a
    /// combination read through [`Matches::keys`] reads no row for it.
    #[inline]
    pub fn uncovered(self) -> IndexIter<T> {
        IndexIter {
            rows: self.rows,
            entries: Box::new(Uncovered(self.entries)),
        }
    }
}

impl<T: Record, C> IndexIter<T, C> {
    /// Steps to the next row and lends it, where `next` hands out a [`Val`].
    ///
    /// The row is borrowed from the node the lookup holds, so it lasts until the lookup moves
    /// again. A [`Val`] of a row with a destructor takes a reference count on its node's column,
    /// and many threads reading the same rows all write those counts' cache lines, where a lent
    /// row writes nothing shared. So this is the form for hot pages of rows that the caller reads
    /// in place: a `while let` loop over it can stop after a page, return or use `?` as it likes,
    /// and the lookup goes on from there. [`visit`](IndexIter::visit) lends every row left to a
    /// closure.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Data, Record, Table};
    ///
    /// #[derive(Data, Record, Clone)]
    /// struct Order {
    ///     #[key]
    ///     id: u64,
    ///     #[index]
    ///     customer: String,
    ///     total: u64,
    /// }
    ///
    /// let mut orders: Table<Order> = Table::new();
    /// for id in 1..=5 {
    ///     orders.insert(Order { id, customer: "wile".into(), total: id * 100 });
    /// }
    ///
    /// let mut rows = orders.by_customer("wile");
    /// let mut page = Vec::new();
    /// while page.len() < 2
    ///     && let Some(order) = rows.next_ref()
    /// {
    ///     page.push(order.total);
    /// }
    /// assert_eq!(page, [100, 200]);
    /// assert_eq!(rows.keys().collect::<Vec<_>>(), [3, 4, 5]);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if a row cannot be read, as `next` does.
    #[inline]
    pub fn next_ref(&mut self) -> Option<&T> {
        let key = self.entries.forward()?;
        self.rows.lend(&key)
    }

    /// Calls `f` on each row the lookup has left, in the order `next` yields them, lending each
    /// as [`next_ref`](IndexIter::next_ref) does.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Data, Record, Table};
    ///
    /// #[derive(Data, Record, Clone)]
    /// struct Order {
    ///     #[key]
    ///     id: u64,
    ///     #[index]
    ///     customer: String,
    ///     total: u64,
    /// }
    ///
    /// let mut orders: Table<Order> = Table::new();
    /// orders.insert(Order { id: 1, customer: "wile".into(), total: 4999 });
    /// orders.insert(Order { id: 2, customer: "road".into(), total: 150 });
    /// orders.insert(Order { id: 3, customer: "wile".into(), total: 900 });
    ///
    /// let mut spent = 0;
    /// orders.by_customer("wile").visit(|order| spent += order.total);
    /// assert_eq!(spent, 5899);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if a row cannot be read, as `next` does.
    pub fn visit(mut self, mut f: impl FnMut(&T)) {
        while let Some(row) = self.next_ref() {
            f(row);
        }
    }
}

impl<T: Record, C> Iterator for IndexIter<T, C> {
    type Item = Val<T>;

    #[inline]
    fn next(&mut self) -> Option<Val<T>> {
        let key = self.entries.forward()?;
        self.rows.get(&key)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.entries.left(), Some(self.entries.left()))
    }
}

impl<T: Record, C> DoubleEndedIterator for IndexIter<T, C> {
    #[inline]
    fn next_back(&mut self) -> Option<Val<T>> {
        let key = self.entries.backward()?;
        self.rows.get(&key)
    }
}

impl<T: Record, C> ExactSizeIterator for IndexIter<T, C> {}

/// `(key, covered fields)` pairs of an index lookup, in index order, from
/// [`IndexIter::covered`]. Reads no rows.
pub struct IndexCovered<T: Record, C = ()>(Box<dyn EntryCursor<T::Key, C>>);

impl<T: Record, C> Iterator for IndexCovered<T, C> {
    type Item = (T::Key, C);

    #[inline]
    fn next(&mut self) -> Option<(T::Key, C)> {
        self.0.forward_covered()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.0.left(), Some(self.0.left()))
    }
}

impl<T: Record, C> DoubleEndedIterator for IndexCovered<T, C> {
    #[inline]
    fn next_back(&mut self) -> Option<(T::Key, C)> {
        self.0.backward_covered()
    }
}

impl<T: Record, C> ExactSizeIterator for IndexCovered<T, C> {}

/// The primary keys of an index lookup, in index order, from [`IndexIter::keys`]. Reads no rows.
pub struct IndexKeys<T: Record>(Box<dyn KeyCursor<T::Key>>);

impl<T: Record> Iterator for IndexKeys<T> {
    type Item = T::Key;

    #[inline]
    fn next(&mut self) -> Option<T::Key> {
        self.0.forward()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.0.left(), Some(self.0.left()))
    }
}

impl<T: Record> DoubleEndedIterator for IndexKeys<T> {
    #[inline]
    fn next_back(&mut self) -> Option<T::Key> {
        self.0.backward()
    }
}

impl<T: Record> ExactSizeIterator for IndexKeys<T> {}

/// A lookup that [`Matches::and`] and [`Matches::or`] can combine: a [`Matches`] or any
/// [`IndexIter`].
///
/// An [`IndexIter`] from a lookup on the table, such as `by_x_range` or `by_x_prefix`, is not in
/// key order. A range or prefix lookup passed to `and` or `or` is tested rather than read when
/// the combination only looks keys up in it, because the other side of an `and` has at most a
/// quarter of its keys: each key that side offers is tested by reading its row from the table the
/// lookup was made on and checking the indexed value against the span of entries the lookup has
/// left. Otherwise the first time the combination is read the lookup's keys and covered fields are
/// read in full and sorted, as they are for one that leads the combination through
/// [`into_matches`](IntoMatches::into_matches), one whose covered fields the combination yields,
/// one passed through [`IndexIter::uncovered`], and every other [`IndexIter`]. A lookup of one
/// value is in key order already and sorts in one pass.
///
/// A row found in the cache costs about as much to test as a few entries do to read, but one read
/// from the file costs as much as dozens. So tests count the rows they had to read from the file:
/// those of every key under [`Matches::keys`] and [`Matches::covered`], which yield no rows, and
/// otherwise those of keys the lookup does not hold, since a row that matches is read anyway. Once
/// they come to more than about a 64th of the entries the lookup has, the lookup is read in full
/// for the keys left, so a combination whose rows are not cached costs at most about twice what
/// reading the lookup would.
///
/// # Examples
///
/// ```
/// use ferro::{Data, Record, Table};
///
/// #[derive(Data, Record, Clone)]
/// struct Order {
///     #[key]
///     id: u64,
///     #[index]
///     customer: String,
///     #[index]
///     total: u64,
/// }
///
/// let mut orders: Table<Order> = Table::new();
/// orders.insert(Order { id: 1, customer: "wile".into(), total: 4999 });
/// orders.insert(Order { id: 2, customer: "wile".into(), total: 150 });
/// orders.insert(Order { id: 3, customer: "road".into(), total: 7000 });
///
/// let big: Vec<u64> = orders
///     .find()
///     .by_customer("wile")
///     .and(orders.by_total_range(1000..))
///     .keys()
///     .collect();
/// assert_eq!(big, [1]);
/// ```
pub trait IntoMatches<T: Record> {
    /// The covered fields of the lookup, `()` for one without `cover(...)`.
    type Covered: Send + 'static;

    /// Turns the lookup into [`Matches`] in key order.
    fn into_matches(self) -> Matches<T, Self::Covered>;
}

impl<T: Record, C: Send + 'static> IntoMatches<T> for IndexIter<T, C> {
    type Covered = C;

    fn into_matches(self) -> Matches<T, C> {
        Matches {
            rows: self.rows,
            more: None,
            stream: self.entries.into_stream(),
            started: false,
        }
    }
}

impl<T: Record, C: Send + 'static> IntoMatches<T> for Matches<T, C> {
    type Covered = C;

    fn into_matches(self) -> Matches<T, C> {
        self
    }
}

/// The rows matching a combination of index lookups, in primary key order.
///
/// The `find()` view the [`Record`](derive@crate::Record) derive generates returns one for each
/// equality lookup, `by_x(&value)`, `by_x_after`, `by_x_some`, `by_x_some_after` and a composite's
/// `by_a_b(&(a, b))`. [`and`](Matches::and) and [`or`](Matches::or) combine them, and any
/// [`IndexIter`], by primary key. A combination reads a row only for a key that matches or that
/// it tests against a range or prefix lookup, and yields each matching row once, from either end.
///
/// An index entry is `(value, key)`, so the keys of one value come in key order and two lookups
/// of similar size are walked side by side. When one side of an `and` has at least four times the
/// keys of the other, only the smaller is walked and each of its keys is looked up in the larger,
/// so a few keys against many cost a few searches. A range or prefix lookup on the table is not
/// in key order, so it is looked up by reading the row of each key and testing it against the
/// range, and when it has less than four times the keys of the other side it is read in full and
/// sorted instead, as [`IntoMatches`] describes. The choice is made when the combination is first
/// read. A combination reads the indexes and rows as they were when its lookups were built.
///
/// [`keys`](Matches::keys) gives the keys alone and [`covered`](Matches::covered) the covered
/// fields of the leftmost lookup, so a combination whose first index covers what you show reads
/// no rows, but those it tests against a range or prefix lookup.
///
/// # Lookups of several tables
///
/// Lookups made on different tables of one row type, or on different versions of one table, such
/// as a snapshot's and the one inside a commit, combine as well. Each lookup answers for the table
/// it was made on, as it was when the lookup was built. Each key's row comes from a lookup that
/// holds the key: from the left side of an `and`, and from the leftmost side of an `or` that holds
/// it, the lookup whose fields [`covered`](Matches::covered) yields. So rows, lent rows, keys and
/// covered fields, read from either end, all give the same keys, and an `or` of two tables yields
/// every row either holds, a key both hold with the row of the left one. Lookups of one table at
/// one version read their rows through one finder, as a single lookup does.
///
/// # Examples
///
/// ```
/// use ferro::{Data, Record, Table};
///
/// #[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// enum Status {
///     Pending,
///     Shipped,
/// }
///
/// #[derive(Data, Record, Clone)]
/// struct Order {
///     #[key]
///     id: u64,
///     #[index]
///     customer: String,
///     #[index]
///     status: Status,
/// }
///
/// let mut orders: Table<Order> = Table::new();
/// orders.insert(Order { id: 1, customer: "wile".into(), status: Status::Pending });
/// orders.insert(Order { id: 2, customer: "wile".into(), status: Status::Shipped });
/// orders.insert(Order { id: 3, customer: "road".into(), status: Status::Pending });
/// orders.insert(Order { id: 4, customer: "wile".into(), status: Status::Pending });
///
/// let find = orders.find();
/// let pending: Vec<u64> = find
///     .by_customer("wile")
///     .and(find.by_status(&Status::Pending))
///     .rev()
///     .map(|order| order.id)
///     .collect();
/// assert_eq!(pending, [4, 1]);
///
/// let either: Vec<u64> = find
///     .by_customer("road")
///     .or(find.by_status(&Status::Shipped))
///     .keys()
///     .collect();
/// assert_eq!(either, [2, 3]);
/// ```
pub struct Matches<T: Record, C = ()> {
    rows: Finder<T::Key, T>,
    more: More<T>,
    stream: Box<dyn Stream<T::Key, C>>,
    started: bool,
}

type More<T> = Option<Box<Vec<Finder<<T as Record>::Key, T>>>>;

struct Finders<T: Record> {
    first: Option<Finder<T::Key, T>>,
    more: Vec<Option<Finder<T::Key, T>>>,
}

impl<T: Record> Finders<T> {
    fn of(first: Option<Finder<T::Key, T>>, more: Vec<Finder<T::Key, T>>) -> Self {
        Finders {
            first,
            more: more.into_iter().map(Some).collect(),
        }
    }

    fn take(&mut self, source: usize) -> Option<Finder<T::Key, T>> {
        match source.checked_sub(1) {
            None => self.first.take(),
            Some(at) => self.more.get_mut(at)?.take(),
        }
    }
}

impl<T: Record, C: Send + 'static> Matches<T, C> {
    /// The rows that both this combination and `other` match.
    ///
    /// `other` can be any [`IntoMatches`], so a range or prefix lookup on the table joins too,
    /// tested key by key when this side has at most a quarter of its keys and read in full and
    /// sorted otherwise. The result keeps this side's covered fields, and reads each row from this
    /// side. `other` may be a lookup of another table, or of another version of this one: it
    /// matches the keys its own table holds, and decides only which of this side's rows are
    /// yielded. If this combination has already yielded rows, what it has left is read into a
    /// sorted list first.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Data, Record, Table};
    ///
    /// #[derive(Data, Record, Clone)]
    /// struct Order {
    ///     #[key]
    ///     id: u64,
    ///     #[index]
    ///     customer: String,
    ///     #[index]
    ///     day: u32,
    /// }
    ///
    /// let mut orders: Table<Order> = Table::new();
    /// for id in 1..=400 {
    ///     let customer = if id % 100 == 0 { "wile" } else { "road" };
    ///     orders.insert(Order { id, customer: customer.into(), day: id as u32 % 30 });
    /// }
    ///
    /// let late: Vec<u64> =
    ///     orders.find().by_customer("wile").and(orders.by_day_range(5..)).keys().collect();
    /// assert_eq!(late, [100, 200, 400]);
    /// ```
    pub fn and<M: IntoMatches<T>>(self, other: M) -> Matches<T, C> {
        let left = self.unstarted();
        let right = other.into_matches().unstarted().adopted();
        Matches {
            rows: left.rows,
            more: left.more,
            stream: Box::new(All::new(left.stream, Box::new(Blind(right)))),
            started: false,
        }
    }

    /// The rows that this combination or `other` matches, each once.
    ///
    /// Both sides must have the same covered fields. Call [`uncovered`](Matches::uncovered), or
    /// [`IndexIter::uncovered`], on each to combine lookups of differently covered indexes. A key
    /// both sides hold takes its row and covered fields from this side, and a key only `other`
    /// holds takes them from `other`. So when `other` is a lookup of another table, or of another
    /// version of this one, the combination yields every row either holds, each from a table whose
    /// lookup matched it. If this combination has already yielded rows, what it has left is read
    /// into a sorted list first.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Data, Record, Table};
    ///
    /// #[derive(Data, Record, Clone)]
    /// struct Order {
    ///     #[key]
    ///     id: u64,
    ///     #[index]
    ///     customer: String,
    ///     shipped: bool,
    /// }
    ///
    /// let mut archive: Table<Order> = Table::new();
    /// archive.insert(Order { id: 1, customer: "wile".into(), shipped: true });
    /// archive.insert(Order { id: 2, customer: "wile".into(), shipped: false });
    /// let mut today = archive.clone();
    /// today.update(&2, |order| order.shipped = true);
    /// today.insert(Order { id: 3, customer: "wile".into(), shipped: false });
    /// today.remove(&1);
    ///
    /// let wile: Vec<(u64, bool)> = today
    ///     .find()
    ///     .by_customer("wile")
    ///     .or(archive.find().by_customer("wile"))
    ///     .map(|order| (order.id, order.shipped))
    ///     .collect();
    /// assert_eq!(wile, [(1, true), (2, true), (3, false)]);
    /// ```
    pub fn or<M: IntoMatches<T, Covered = C>>(self, other: M) -> Matches<T, C> {
        let mut left = self.unstarted();
        let right = left.joined(other.into_matches().unstarted());
        Matches {
            rows: left.rows,
            more: left.more,
            stream: Box::new(Any::new(vec![left.stream, right])),
            started: false,
        }
    }

    /// The primary keys of the matching rows, in key order, reading no row but those tested
    /// against a range or prefix lookup, as [`IntoMatches`] describes. A range passed through
    /// [`IndexIter::uncovered`] is read in full instead, and reads no row.
    pub fn keys(mut self) -> MatchKeys<T> {
        self.stream.rowless();
        self.lend_more();
        MatchKeys(Box::new(Blind(self.stream)))
    }

    /// `(key, fields)` pairs of the matching rows in key order, where `fields` are the covered
    /// fields of the leftmost lookup that holds the row, read from its index. It reads no row but
    /// those tested against a range or prefix lookup, as [`IntoMatches`] describes, and none for a
    /// range passed through [`IndexIter::uncovered`].
    pub fn covered(mut self) -> MatchCovered<T, C> {
        self.stream.cover();
        self.stream.rowless();
        self.lend_more();
        MatchCovered(self.stream)
    }

    /// The same combination with the covered fields dropped from its type.
    pub fn uncovered(self) -> Matches<T> {
        Matches {
            rows: self.rows,
            more: self.more,
            stream: Box::new(Blind(self.stream)),
            started: self.started,
        }
    }

    fn unstarted(mut self) -> Self {
        if !self.started {
            return self;
        }
        self.stream.cover();
        let stream: Box<dyn Stream<T::Key, C>> = if let Some(more) = &self.more {
            let mut lists: Vec<VecDeque<_>> = (0..=more.len()).map(|_| VecDeque::new()).collect();
            while let Some(item) = self.stream.take(End::Front) {
                lists[self.stream.source(End::Front)].push_back(item);
            }
            let sides = lists.into_iter().enumerate().map(|(source, items)| {
                let mut listed = Listed::ready(items);
                listed.source = source;
                Box::new(listed) as Box<dyn Stream<T::Key, C>>
            });
            Box::new(Any::new(sides.collect()))
        } else {
            let mut items = VecDeque::new();
            while let Some(item) = self.stream.take(End::Front) {
                items.push_back(item);
            }
            Box::new(Listed::ready(items))
        };
        Matches {
            rows: self.rows,
            more: self.more,
            stream,
            started: false,
        }
    }

    fn adopted(self) -> Box<dyn Stream<T::Key, C>> {
        let mut stream = self.stream;
        match self.more {
            None => stream.adopt(&mut Some(self.rows)),
            Some(more) => stream.adopt(&mut Finders::<T>::of(Some(self.rows), *more)),
        }
        stream
    }

    fn lend_more(&mut self) {
        if let Some(more) = self.more.take() {
            self.stream.adopt(&mut Finders::<T>::of(None, *more));
        }
    }

    fn joined(&mut self, right: Matches<T, C>) -> Box<dyn Stream<T::Key, C>> {
        let Matches {
            rows, more, stream, ..
        } = right;
        let mut stream = stream;
        if more.is_none() && self.more.is_none() && self.rows.same_map(&rows) {
            stream.adopt(&mut Some(rows));
            return stream;
        }
        let more = more.map_or_else(Vec::new, |more| *more);
        let mut spare = Finders::<T> {
            first: None,
            more: Vec::with_capacity(more.len()),
        };
        let mut sources = Vec::with_capacity(1 + more.len());
        for (at, finder) in std::iter::once(rows).chain(more).enumerate() {
            let held = std::iter::once(&self.rows)
                .chain(self.more.iter().flat_map(|more| more.iter()))
                .position(|kept| kept.same_map(&finder));
            let finder = match held {
                Some(source) => {
                    sources.push(source);
                    Some(finder)
                }
                None => {
                    let kept = self.more.get_or_insert_default();
                    kept.push(finder);
                    sources.push(kept.len());
                    None
                }
            };
            match at {
                0 => spare.first = finder,
                _ => spare.more.push(finder),
            }
        }
        stream.adopt(&mut spare);
        if sources.iter().enumerate().any(|(at, source)| at != *source) {
            stream.renumber(&sources);
        }
        stream
    }
}

impl<T: Record, C> Matches<T, C> {
    /// Steps to the next matching row and lends it, where `next` hands out a [`Val`], as
    /// [`IndexIter::next_ref`] does.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Data, Record, Table};
    ///
    /// #[derive(Data, Record, Clone)]
    /// struct Order {
    ///     #[key]
    ///     id: u64,
    ///     #[index]
    ///     customer: String,
    ///     #[index]
    ///     shipped: bool,
    /// }
    ///
    /// let mut orders: Table<Order> = Table::new();
    /// for id in 1..=6 {
    ///     orders.insert(Order { id, customer: "wile".into(), shipped: id % 3 == 0 });
    /// }
    ///
    /// let find = orders.find();
    /// let mut pending = find.by_customer("wile").and(find.by_shipped(&false));
    /// assert_eq!(pending.next_ref().map(|order| order.id), Some(1));
    /// assert_eq!(pending.keys().collect::<Vec<_>>(), [2, 4, 5]);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if a row cannot be read, as `next` does.
    pub fn next_ref(&mut self) -> Option<&T> {
        self.started = true;
        let (key, _) = self.stream.take(End::Front)?;
        match self.more {
            None => self.rows.lend(&key),
            Some(_) => self.lend_from(End::Front, &key),
        }
    }

    /// Calls `f` on each matching row left, in key order, lending each as
    /// [`next_ref`](Matches::next_ref) does.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Data, Record, Table};
    ///
    /// #[derive(Data, Record, Clone)]
    /// struct Order {
    ///     #[key]
    ///     id: u64,
    ///     #[index]
    ///     customer: String,
    ///     #[index]
    ///     shipped: bool,
    ///     total: u64,
    /// }
    ///
    /// let mut orders: Table<Order> = Table::new();
    /// orders.insert(Order { id: 1, customer: "wile".into(), shipped: false, total: 4999 });
    /// orders.insert(Order { id: 2, customer: "wile".into(), shipped: true, total: 150 });
    /// orders.insert(Order { id: 3, customer: "wile".into(), shipped: false, total: 900 });
    ///
    /// let find = orders.find();
    /// let mut owed = 0;
    /// find.by_customer("wile").and(find.by_shipped(&false)).visit(|order| owed += order.total);
    /// assert_eq!(owed, 5899);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if a row cannot be read, as `next` does.
    pub fn visit(mut self, mut f: impl FnMut(&T)) {
        while let Some(row) = self.next_ref() {
            f(row);
        }
    }

    fn finder(&mut self, end: End) -> &mut Finder<T::Key, T> {
        match (self.stream.source(end).checked_sub(1), &mut self.more) {
            (Some(at), Some(more)) => &mut more[at],
            _ => &mut self.rows,
        }
    }

    #[inline(never)]
    fn get_from(&mut self, end: End, key: &T::Key) -> Option<Val<T>> {
        self.finder(end).get(key)
    }

    #[inline(never)]
    fn lend_from(&mut self, end: End, key: &T::Key) -> Option<&T> {
        self.finder(end).lend(key)
    }
}

impl<T: Record, C> Iterator for Matches<T, C> {
    type Item = Val<T>;

    fn next(&mut self) -> Option<Val<T>> {
        self.started = true;
        let (key, _) = self.stream.take(End::Front)?;
        match self.more {
            None => self.rows.get(&key),
            Some(_) => self.get_from(End::Front, &key),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.stream.len()))
    }
}

impl<T: Record, C> DoubleEndedIterator for Matches<T, C> {
    fn next_back(&mut self) -> Option<Val<T>> {
        self.started = true;
        let (key, _) = self.stream.take(End::Back)?;
        match self.more {
            None => self.rows.get(&key),
            Some(_) => self.get_from(End::Back, &key),
        }
    }
}

/// The primary keys of [`Matches`], in key order, from [`Matches::keys`]. Reads no rows but those
/// tested against a range or prefix lookup.
pub struct MatchKeys<T: Record>(Box<dyn Stream<T::Key, ()>>);

impl<T: Record> Iterator for MatchKeys<T> {
    type Item = T::Key;

    fn next(&mut self) -> Option<T::Key> {
        self.0.take(End::Front).map(|(key, _)| key)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.0.len()))
    }
}

impl<T: Record> DoubleEndedIterator for MatchKeys<T> {
    fn next_back(&mut self) -> Option<T::Key> {
        self.0.take(End::Back).map(|(key, _)| key)
    }
}

/// `(key, covered fields)` pairs of [`Matches`], in key order, from [`Matches::covered`]. Reads
/// no rows but those tested against a range or prefix lookup.
pub struct MatchCovered<T: Record, C = ()>(Box<dyn Stream<T::Key, C>>);

impl<T: Record, C> Iterator for MatchCovered<T, C> {
    type Item = (T::Key, C);

    fn next(&mut self) -> Option<(T::Key, C)> {
        let (key, covered) = self.0.take(End::Front)?;
        Some((key, covered.expect("covered fields kept once covered")))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.0.len()))
    }
}

impl<T: Record, C> DoubleEndedIterator for MatchCovered<T, C> {
    fn next_back(&mut self) -> Option<(T::Key, C)> {
        let (key, covered) = self.0.take(End::Back)?;
        Some((key, covered.expect("covered fields kept once covered")))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Front,
    Back,
}

impl End {
    fn slot(self) -> usize {
        match self {
            End::Front => 0,
            End::Back => 1,
        }
    }

    fn other(self) -> End {
        match self {
            End::Front => End::Back,
            End::Back => End::Front,
        }
    }

    fn before<K: Ord>(self, key: &K, target: &K) -> bool {
        match self {
            End::Front => key < target,
            End::Back => key > target,
        }
    }

    fn short<const PAST: bool, K: Ord>(self, key: &K, target: &K) -> bool {
        self.before(key, target) || PAST && key == target
    }
}

enum Probe<'a, K> {
    At(&'a K),
    Past(&'a K),
    Gone,
}

impl<'a, K> Probe<'a, K> {
    fn at(found: Option<&'a K>) -> Self {
        match found {
            Some(found) => Probe::At(found),
            None => Probe::Gone,
        }
    }
}

trait Stream<K, C>: Send {
    fn len(&self) -> usize;
    fn cover(&mut self);
    fn peek(&self, end: End) -> Option<&K>;
    fn fill(&mut self, end: End) -> Option<&K>;
    fn seek(&mut self, end: End, target: &K) -> Option<&K>;
    fn pass(&mut self, end: End, target: &K) -> Option<&K>;
    fn take(&mut self, end: End) -> Option<(K, Option<C>)>;
    fn probe(&mut self, end: End, target: &K) -> Probe<'_, K>;
    fn found(&mut self) -> Option<C>;
    fn source(&self, end: End) -> usize;
    fn found_source(&self) -> usize;
    fn renumber(&mut self, sources: &[usize]);
    fn follow(&mut self, _lead: usize) {}
    fn adopt(&mut self, _rows: &mut dyn std::any::Any) {}
    fn rowless(&mut self) {}
}

const STEPS: usize = 16;

struct EqualStream<I, K, S, C> {
    index: Map<(I, K), S>,
    iter: Iter<(I, K), S>,
    project: Project<I, S, C>,
    covering: bool,
    started: bool,
    value: Option<I>,
    first: Option<K>,
    last: Option<K>,
    held: [Option<(K, Option<C>)>; 2],
    edge: [Bound<K>; 2],
    cursor: Option<Cursor<(I, K), S>>,
    probe: Option<(I, K)>,
    source: usize,
}

impl<I: Data + Ord, K: Data + Ord, S: Data, C: Send> EqualStream<I, K, S, C> {
    fn new(index: Map<(I, K), S>, iter: Iter<(I, K), S>, project: Project<I, S, C>) -> Self {
        EqualStream {
            index,
            iter,
            project,
            covering: false,
            started: false,
            value: None,
            first: None,
            last: None,
            held: [None, None],
            edge: [Bound::Unbounded, Bound::Unbounded],
            cursor: None,
            probe: None,
            source: 0,
        }
    }

    #[inline]
    fn read(&mut self, end: End) -> Option<(K, Option<C>)> {
        let project = self.project;
        if self.covering {
            let read = |entry: &(I, K), stored: &S| {
                (entry.1.clone(), Some(project(&entry.0, stored.clone())))
            };
            match end {
                End::Front => self.iter.next_with(read),
                End::Back => self.iter.next_back_with(read),
            }
        } else {
            let read = |entry: &(I, K)| (entry.1.clone(), None);
            match end {
                End::Front => self.iter.next_key_with(read),
                End::Back => self.iter.next_back_key_with(read),
            }
        }
    }

    #[inline]
    fn start(&mut self) {
        if !self.started {
            self.begin();
        }
    }

    fn begin(&mut self) {
        self.started = true;
        let project = self.project;
        let covering = self.covering;
        let mut value = None;
        let first = self.iter.next_with(|entry, stored| {
            value = Some(entry.0.clone());
            (
                entry.1.clone(),
                covering.then(|| project(&entry.0, stored.clone())),
            )
        });
        self.value = value;
        let last = self.read(End::Back);
        self.first = first.as_ref().map(|(key, _)| key.clone());
        self.last = last.as_ref().or(first.as_ref()).map(|(key, _)| key.clone());
        if let (Some(first), Some(last)) = (&self.first, &self.last) {
            self.edge = [
                Bound::Included(first.clone()),
                Bound::Included(last.clone()),
            ];
        }
        self.held = [first, last];
    }

    #[inline]
    fn pull(&mut self, end: End) {
        let next = self.read(end);
        self.held[end.slot()] = match next {
            Some(entry) => Some(entry),
            None => self.held[end.other().slot()].take(),
        };
    }

    fn rebuild(&mut self, end: End, target: &K) {
        let Some(value) = self.value.as_ref() else {
            return;
        };
        let other = end.other();
        let far = match &self.held[other.slot()] {
            Some((key, _)) => Bound::Excluded(key.clone()),
            None => self.edge[other.slot()].clone(),
        };
        let near = Bound::Included(target.clone());
        let (low, high) = match end {
            End::Front => (near, far),
            End::Back => (far, near),
        };
        self.iter = self.index.range_by(
            |entry| match entry.0.cmp(value) {
                Ordering::Less => true,
                Ordering::Greater => false,
                Ordering::Equal => match &low {
                    Bound::Included(bound) => entry.1 < *bound,
                    Bound::Excluded(bound) => entry.1 <= *bound,
                    Bound::Unbounded => false,
                },
            },
            |entry| match entry.0.cmp(value) {
                Ordering::Less => true,
                Ordering::Greater => false,
                Ordering::Equal => match &high {
                    Bound::Included(bound) => entry.1 <= *bound,
                    Bound::Excluded(bound) => entry.1 < *bound,
                    Bound::Unbounded => true,
                },
            },
        );
        self.edge[end.slot()] = Bound::Included(target.clone());
    }

    fn lookup(&self, key: &K) -> Option<C> {
        let value = self.value.as_ref()?;
        let stored = self.index.get(&(value.clone(), key.clone()))?;
        Some((self.project)(value, (*stored).clone()))
    }

    fn reach<const PAST: bool>(&mut self, end: End, target: &K) -> Option<&K> {
        self.start();
        let slot = end.slot();
        if let Some((key, _)) = &self.held[slot]
            && !end.short::<PAST, K>(key, target)
        {
            return self.peek(end);
        }
        self.held[slot] = None;
        let mut steps = 0;
        loop {
            if steps == STEPS && self.iter.len() > 0 {
                self.rebuild(end, target);
            }
            steps += 1;
            let next = match self.read(end) {
                Some(next) => next,
                None => self.held[end.other().slot()].take()?,
            };
            if !end.short::<PAST, K>(&next.0, target) {
                self.held[slot] = Some(next);
                return self.peek(end);
            }
        }
    }
}

impl<I: Data + Ord, K: Data + Ord, S: Data, C: Send> Stream<K, C> for EqualStream<I, K, S, C> {
    fn len(&self) -> usize {
        self.iter.len() + self.held.iter().filter(|held| held.is_some()).count()
    }

    fn cover(&mut self) {
        self.covering = true;
    }

    fn peek(&self, end: End) -> Option<&K> {
        self.held[end.slot()].as_ref().map(|(key, _)| key)
    }

    fn fill(&mut self, end: End) -> Option<&K> {
        self.start();
        if self.held[end.slot()].is_none() {
            self.pull(end);
        }
        self.peek(end)
    }

    fn seek(&mut self, end: End, target: &K) -> Option<&K> {
        self.reach::<false>(end, target)
    }

    fn pass(&mut self, end: End, target: &K) -> Option<&K> {
        self.reach::<true>(end, target)
    }

    fn take(&mut self, end: End) -> Option<(K, Option<C>)> {
        self.fill(end)?;
        let (key, covered) = self.held[end.slot()].take()?;
        self.edge[end.slot()] = Bound::Excluded(key.clone());
        let covered = match covered {
            None if self.covering => self.lookup(&key),
            covered => covered,
        };
        Some((key, covered))
    }

    fn probe(&mut self, end: End, target: &K) -> Probe<'_, K> {
        self.start();
        let (Some(first), Some(last), Some(value)) = (&self.first, &self.last, &self.value) else {
            return Probe::Gone;
        };
        let target = match end {
            End::Front if target > last => return Probe::Gone,
            End::Back if target < first => return Probe::Gone,
            End::Front if target < first => first,
            End::Back if target > last => last,
            _ => target,
        };
        let probe = match &mut self.probe {
            Some(probe) => {
                probe.1 = target.clone();
                probe
            }
            None => self.probe.insert((value.clone(), target.clone())),
        };
        let cursor = self.cursor.get_or_insert_with(|| self.index.cursor());
        match end {
            End::Front => cursor.seek(probe),
            End::Back => cursor.seek_back(probe),
        };
        let Some(entry) = cursor.key() else {
            return Probe::Gone;
        };
        let inside = entry.0 == *value
            && match end {
                End::Front => entry.1 <= *last,
                End::Back => entry.1 >= *first,
            };
        Probe::at(inside.then_some(&entry.1))
    }

    fn found(&mut self) -> Option<C> {
        let project = self.project;
        self.cursor
            .as_ref()?
            .read(|entry, stored| project(&entry.0, stored.clone()))
    }

    fn source(&self, _end: End) -> usize {
        self.source
    }

    fn found_source(&self) -> usize {
        self.source
    }

    fn renumber(&mut self, sources: &[usize]) {
        self.source = sources[self.source];
    }
}

type Collect<K, C> = Box<dyn FnOnce() -> Vec<(K, Option<C>)> + Send>;

struct Listed<K, C> {
    pending: Option<(usize, Collect<K, C>)>,
    items: VecDeque<(K, Option<C>)>,
    landed: usize,
    source: usize,
}

impl<K: Ord, C> Listed<K, C> {
    fn pending(left: usize, collect: Collect<K, C>) -> Self {
        Listed {
            pending: Some((left, collect)),
            items: VecDeque::new(),
            landed: 0,
            source: 0,
        }
    }

    fn ready(items: VecDeque<(K, Option<C>)>) -> Self {
        Listed {
            pending: None,
            items,
            landed: 0,
            source: 0,
        }
    }

    fn sorted(mut items: Vec<(K, Option<C>)>) -> Self {
        items.sort_by(|a, b| a.0.cmp(&b.0));
        items.dedup_by(|a, b| a.0 == b.0);
        Listed::ready(items.into())
    }

    fn items(&mut self) -> &mut VecDeque<(K, Option<C>)> {
        if let Some((_, collect)) = self.pending.take() {
            let source = self.source;
            *self = Listed::sorted(collect());
            self.source = source;
        }
        &mut self.items
    }

    fn reach<const PAST: bool>(&mut self, end: End, target: &K) -> Option<&K> {
        let items = self.items();
        match end {
            End::Front => {
                let behind = match PAST {
                    false => items.partition_point(|(key, _)| key < target),
                    true => items.partition_point(|(key, _)| key <= target),
                };
                items.drain(..behind);
            }
            End::Back => {
                let kept = match PAST {
                    false => items.partition_point(|(key, _)| key <= target),
                    true => items.partition_point(|(key, _)| key < target),
                };
                items.truncate(kept);
            }
        }
        match end {
            End::Front => self.items.front(),
            End::Back => self.items.back(),
        }
        .map(|(key, _)| key)
    }
}

impl<K: Ord + Send, C: Send> Stream<K, C> for Listed<K, C> {
    #[inline]
    fn len(&self) -> usize {
        match &self.pending {
            Some((left, _)) => *left,
            None => self.items.len(),
        }
    }

    #[inline]
    fn cover(&mut self) {}

    #[inline]
    fn peek(&self, end: End) -> Option<&K> {
        match end {
            End::Front => self.items.front(),
            End::Back => self.items.back(),
        }
        .map(|(key, _)| key)
    }

    #[inline]
    fn fill(&mut self, end: End) -> Option<&K> {
        self.items();
        self.peek(end)
    }

    #[inline]
    fn seek(&mut self, end: End, target: &K) -> Option<&K> {
        self.reach::<false>(end, target)
    }

    #[inline]
    fn pass(&mut self, end: End, target: &K) -> Option<&K> {
        self.reach::<true>(end, target)
    }

    #[inline]
    fn take(&mut self, end: End) -> Option<(K, Option<C>)> {
        let items = self.items();
        match end {
            End::Front => items.pop_front(),
            End::Back => items.pop_back(),
        }
    }

    #[inline]
    fn probe(&mut self, end: End, target: &K) -> Probe<'_, K> {
        let items = self.items();
        let landed = match end {
            End::Front => items.partition_point(|(key, _)| key < target),
            End::Back => match items.partition_point(|(key, _)| key <= target) {
                0 => return Probe::Gone,
                kept => kept - 1,
            },
        };
        self.landed = landed;
        Probe::at(self.items.get(landed).map(|(key, _)| key))
    }

    #[inline]
    fn found(&mut self) -> Option<C> {
        self.items.get_mut(self.landed)?.1.take()
    }

    fn source(&self, _end: End) -> usize {
        self.source
    }

    fn found_source(&self) -> usize {
        self.source
    }

    fn renumber(&mut self, sources: &[usize]) {
        self.source = sources[self.source];
    }
}

struct Ranged<T: Record, I, X: Span<I, T::Key>, C> {
    spanned: Option<Spanned<T, I, X>>,
    rows: Option<Finder<T::Key, T>>,
    edges: Option<[X::Edge; 2]>,
    listed: Listed<T::Key, C>,
    covering: bool,
    lead: Option<usize>,
    landed: Option<(T::Key, bool)>,
    cold: bool,
    rowless: bool,
    budget: u64,
    spent: u64,
    source: usize,
}

impl<T, I, X, C> Ranged<T, I, X, C>
where
    T: Record,
    X: EntryCursor<T::Key, C> + Span<I, T::Key>,
{
    #[inline]
    fn listed(&mut self) -> &mut Listed<T::Key, C> {
        if self.spanned.is_some() {
            self.collect();
        }
        &mut self.listed
    }

    #[inline(never)]
    fn collect(&mut self) {
        if let Some(spanned) = &mut self.spanned {
            let mut items = Vec::with_capacity(spanned.cursor.left());
            while let Some((key, covered)) = spanned.cursor.forward_covered() {
                items.push((key, Some(covered)));
            }
            self.listed = Listed::sorted(items);
            self.spanned = None;
            self.edges = None;
            self.landed = None;
        }
    }

    fn testing(&mut self) -> bool {
        if self.cold {
            return false;
        }
        if self.edges.is_some() {
            return true;
        }
        let (Some(spanned), Some(lead)) = (&mut self.spanned, self.lead) else {
            return false;
        };
        if self.covering
            || self.rows.is_none()
            || lead.saturating_mul(RATIO) > spanned.cursor.left()
        {
            return false;
        }
        self.budget = spanned.cursor.left() as u64;
        self.edges = spanned.cursor.edges();
        self.edges.is_some()
    }

    fn holds(&mut self, key: &T::Key) -> bool {
        let (Some(spanned), Some(edges), Some(rows)) = (&self.spanned, &self.edges, &mut self.rows)
        else {
            return false;
        };
        let indexed = spanned.indexed;
        rows.lend(key)
            .is_some_and(|row| indexed(row, &mut |value| X::holds(edges, value, key)))
    }
}

impl<T, I, X, C> Stream<T::Key, C> for Ranged<T, I, X, C>
where
    T: Record,
    I: 'static,
    X: EntryCursor<T::Key, C> + Span<I, T::Key> + 'static,
    C: Send + 'static,
{
    fn len(&self) -> usize {
        match &self.spanned {
            Some(spanned) => spanned.cursor.left(),
            None => self.listed.len(),
        }
    }

    fn cover(&mut self) {
        self.covering = true;
        self.edges = None;
        self.landed = None;
    }

    fn peek(&self, end: End) -> Option<&T::Key> {
        self.listed.peek(end)
    }

    fn fill(&mut self, end: End) -> Option<&T::Key> {
        self.listed().fill(end)
    }

    fn seek(&mut self, end: End, target: &T::Key) -> Option<&T::Key> {
        self.listed().seek(end, target)
    }

    fn pass(&mut self, end: End, target: &T::Key) -> Option<&T::Key> {
        self.listed().pass(end, target)
    }

    fn take(&mut self, end: End) -> Option<(T::Key, Option<C>)> {
        self.listed().take(end)
    }

    fn probe(&mut self, end: End, target: &T::Key) -> Probe<'_, T::Key> {
        if !self.testing() {
            return self.listed().probe(end, target);
        }
        let known = matches!(&self.landed, Some((key, _)) if key == target);
        if !known {
            let before = fresh_reads();
            let holds = self.holds(target);
            self.landed = Some((target.clone(), holds));
            if !holds || self.rowless {
                self.spent += fresh_reads().wrapping_sub(before);
            }
            if self.spent.saturating_mul(FRESH) > self.budget {
                self.cold = true;
                self.edges = None;
            }
        }
        match &self.landed {
            Some((key, true)) => Probe::At(key),
            Some((key, false)) => Probe::Past(key),
            None => Probe::Gone,
        }
    }

    fn found(&mut self) -> Option<C> {
        match self.edges {
            Some(_) => None,
            None => self.listed.found(),
        }
    }

    fn source(&self, _end: End) -> usize {
        self.source
    }

    fn found_source(&self) -> usize {
        self.source
    }

    fn renumber(&mut self, sources: &[usize]) {
        self.source = sources[self.source];
    }

    fn follow(&mut self, lead: usize) {
        self.lead = Some(lead);
    }

    fn rowless(&mut self) {
        self.rowless = true;
    }

    fn adopt(&mut self, rows: &mut dyn std::any::Any) {
        if self.rows.is_some() {
            return;
        }
        if let Some(rows) = rows.downcast_mut::<Option<Finder<T::Key, T>>>() {
            if self.source == 0 {
                self.rows = rows.take();
            }
        } else if let Some(rows) = rows.downcast_mut::<Finders<T>>() {
            self.rows = rows.take(self.source);
        }
    }
}

struct Blind<K, X>(Box<dyn Stream<K, X>>);

impl<K: 'static, X: 'static> Stream<K, ()> for Blind<K, X> {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn cover(&mut self) {}

    fn peek(&self, end: End) -> Option<&K> {
        self.0.peek(end)
    }

    fn fill(&mut self, end: End) -> Option<&K> {
        self.0.fill(end)
    }

    fn seek(&mut self, end: End, target: &K) -> Option<&K> {
        self.0.seek(end, target)
    }

    fn pass(&mut self, end: End, target: &K) -> Option<&K> {
        self.0.pass(end, target)
    }

    fn take(&mut self, end: End) -> Option<(K, Option<()>)> {
        self.0.take(end).map(|(key, _)| (key, Some(())))
    }

    fn probe(&mut self, end: End, target: &K) -> Probe<'_, K> {
        self.0.probe(end, target)
    }

    fn found(&mut self) -> Option<()> {
        Some(())
    }

    fn source(&self, end: End) -> usize {
        self.0.source(end)
    }

    fn found_source(&self) -> usize {
        self.0.found_source()
    }

    fn renumber(&mut self, sources: &[usize]) {
        self.0.renumber(sources);
    }

    fn follow(&mut self, lead: usize) {
        self.0.follow(lead);
    }

    fn adopt(&mut self, rows: &mut dyn std::any::Any) {
        self.0.adopt(rows);
    }

    fn rowless(&mut self) {
        self.0.rowless();
    }
}

#[derive(Clone, Copy)]
enum Plan {
    Merge,
    Left,
    Right,
}

const RATIO: usize = 4;

const FRESH: u64 = 64;

struct All<K, C> {
    left: Box<dyn Stream<K, C>>,
    right: Box<dyn Stream<K, ()>>,
    plan: Option<Plan>,
    settled: [bool; 2],
    covering: bool,
    landed: Option<K>,
}

fn merged<K: Ord + Clone, X, Y>(
    a: &mut dyn Stream<K, X>,
    b: &mut dyn Stream<K, Y>,
    end: End,
) -> bool {
    let Some(key) = a.fill(end) else {
        return false;
    };
    let mut key = key.clone();
    loop {
        let Some(found) = b.seek(end, &key) else {
            return false;
        };
        if *found == key {
            return true;
        }
        key = found.clone();
        let Some(found) = a.seek(end, &key) else {
            return false;
        };
        if *found == key {
            return true;
        }
        key = found.clone();
    }
}

fn led<K: Ord + Clone, X, Y>(
    lead: &mut dyn Stream<K, X>,
    follow: &mut dyn Stream<K, Y>,
    end: End,
) -> bool {
    let Some(key) = lead.fill(end) else {
        return false;
    };
    let mut key = key.clone();
    loop {
        let next = match follow.probe(end, &key) {
            Probe::At(found) if *found == key => return true,
            Probe::At(found) => {
                let found = found.clone();
                lead.seek(end, &found)
            }
            Probe::Past(found) => {
                let found = found.clone();
                lead.pass(end, &found)
            }
            Probe::Gone => return false,
        };
        let Some(next) = next else {
            return false;
        };
        key = next.clone();
    }
}

impl<K: Data + Ord, C> All<K, C> {
    fn new(left: Box<dyn Stream<K, C>>, right: Box<dyn Stream<K, ()>>) -> Self {
        All {
            left,
            right,
            plan: None,
            settled: [false, false],
            covering: false,
            landed: None,
        }
    }

    fn plan(&mut self) -> Plan {
        if let Some(plan) = self.plan {
            return plan;
        }
        let (left, right) = (self.left.len(), self.right.len());
        let plan = if left.max(right) < left.min(right).saturating_mul(RATIO) {
            Plan::Merge
        } else if left <= right {
            self.right.follow(left);
            Plan::Left
        } else {
            self.left.follow(right);
            Plan::Right
        };
        self.plan = Some(plan);
        plan
    }

    fn settle(&mut self, end: End) -> bool {
        let settled = match self.plan() {
            Plan::Merge => merged(&mut *self.left, &mut *self.right, end),
            Plan::Left => led(&mut *self.left, &mut *self.right, end),
            Plan::Right => led(&mut *self.right, &mut *self.left, end),
        };
        self.settled[end.slot()] = settled;
        settled
    }
}

impl<K: Data + Ord, C: Send> Stream<K, C> for All<K, C> {
    fn len(&self) -> usize {
        self.left.len().min(self.right.len())
    }

    fn cover(&mut self) {
        self.covering = true;
        self.left.cover();
    }

    fn peek(&self, end: End) -> Option<&K> {
        if !self.settled[end.slot()] {
            return None;
        }
        match self.plan? {
            Plan::Merge | Plan::Left => self.left.peek(end),
            Plan::Right => self.right.peek(end),
        }
    }

    fn fill(&mut self, end: End) -> Option<&K> {
        if !self.settle(end) {
            return None;
        }
        self.peek(end)
    }

    fn seek(&mut self, end: End, target: &K) -> Option<&K> {
        match self.plan() {
            Plan::Merge | Plan::Left => self.left.seek(end, target)?,
            Plan::Right => self.right.seek(end, target)?,
        };
        self.fill(end)
    }

    fn pass(&mut self, end: End, target: &K) -> Option<&K> {
        match self.plan() {
            Plan::Merge | Plan::Left => self.left.pass(end, target)?,
            Plan::Right => self.right.pass(end, target)?,
        };
        self.fill(end)
    }

    fn take(&mut self, end: End) -> Option<(K, Option<C>)> {
        if !self.settle(end) {
            return None;
        }
        self.settled[end.slot()] = false;
        match self.plan() {
            Plan::Merge => {
                self.right.take(end)?;
                self.left.take(end)
            }
            Plan::Left => self.left.take(end),
            Plan::Right => {
                let (key, _) = self.right.take(end)?;
                let covered = if self.covering {
                    self.left.found()
                } else {
                    None
                };
                Some((key, covered))
            }
        }
    }

    fn probe(&mut self, end: End, target: &K) -> Probe<'_, K> {
        let mut key = target.clone();
        loop {
            let found = match self.left.probe(end, &key) {
                Probe::At(found) => found.clone(),
                Probe::Past(past) => {
                    let past = past.clone();
                    return Probe::Past(self.landed.insert(past));
                }
                Probe::Gone => return Probe::Gone,
            };
            match self.right.probe(end, &found) {
                Probe::At(again) if *again == found => {
                    return Probe::At(self.landed.insert(found));
                }
                Probe::At(again) => key = again.clone(),
                Probe::Past(past) => {
                    let past = past.clone();
                    return Probe::Past(self.landed.insert(past));
                }
                Probe::Gone => return Probe::Gone,
            }
        }
    }

    fn found(&mut self) -> Option<C> {
        self.left.found()
    }

    fn source(&self, end: End) -> usize {
        match self.plan {
            Some(Plan::Right) => self.left.found_source(),
            _ => self.left.source(end),
        }
    }

    fn found_source(&self) -> usize {
        self.left.found_source()
    }

    fn renumber(&mut self, sources: &[usize]) {
        self.left.renumber(sources);
    }

    fn follow(&mut self, lead: usize) {
        self.left.follow(lead);
        self.right.follow(lead);
    }

    fn adopt(&mut self, rows: &mut dyn std::any::Any) {
        self.left.adopt(rows);
    }

    fn rowless(&mut self) {
        self.left.rowless();
        self.right.rowless();
    }
}

struct Any<K, C> {
    sides: Vec<Box<dyn Stream<K, C>>>,
    took: [usize; 2],
    landed: Option<(usize, K)>,
    passed: Option<K>,
}

impl<K: Data + Ord, C> Any<K, C> {
    fn new(sides: Vec<Box<dyn Stream<K, C>>>) -> Self {
        Any {
            sides,
            took: [0, 0],
            landed: None,
            passed: None,
        }
    }

    fn best(&self, end: End) -> Option<usize> {
        let mut best: Option<(usize, &K)> = None;
        for (side, stream) in self.sides.iter().enumerate() {
            if let Some(key) = stream.peek(end)
                && best.is_none_or(|(_, held)| end.before(key, held))
            {
                best = Some((side, key));
            }
        }
        best.map(|(side, _)| side)
    }
}

impl<K: Data + Ord, C: Send> Stream<K, C> for Any<K, C> {
    fn len(&self) -> usize {
        self.sides
            .iter()
            .fold(0usize, |total, side| total.saturating_add(side.len()))
    }

    fn cover(&mut self) {
        for side in &mut self.sides {
            side.cover();
        }
    }

    fn peek(&self, end: End) -> Option<&K> {
        self.sides[self.best(end)?].peek(end)
    }

    fn fill(&mut self, end: End) -> Option<&K> {
        for side in &mut self.sides {
            side.fill(end);
        }
        self.peek(end)
    }

    fn seek(&mut self, end: End, target: &K) -> Option<&K> {
        for side in &mut self.sides {
            side.seek(end, target);
        }
        self.peek(end)
    }

    fn pass(&mut self, end: End, target: &K) -> Option<&K> {
        for side in &mut self.sides {
            side.pass(end, target);
        }
        self.peek(end)
    }

    fn take(&mut self, end: End) -> Option<(K, Option<C>)> {
        for side in &mut self.sides {
            side.fill(end);
        }
        let first = self.best(end)?;
        self.took[end.slot()] = first;
        let (side, after) = self.sides[first..].split_first_mut()?;
        let taken = side.take(end)?;
        for side in after {
            if side.peek(end) == Some(&taken.0) {
                side.take(end);
            }
        }
        Some(taken)
    }

    fn probe(&mut self, end: End, target: &K) -> Probe<'_, K> {
        let mut best: Option<(usize, K)> = None;
        let mut short: Option<K> = None;
        for (side, stream) in self.sides.iter_mut().enumerate() {
            match stream.probe(end, target) {
                Probe::At(key) if best.as_ref().is_none_or(|(_, held)| end.before(key, held)) => {
                    best = Some((side, key.clone()));
                }
                Probe::Past(key) if short.as_ref().is_none_or(|held| end.before(key, held)) => {
                    short = Some(key.clone());
                }
                _ => {}
            }
        }
        self.landed = None;
        match (best, short) {
            (Some((_, key)), Some(short)) if end.before(&short, &key) => {
                Probe::Past(self.passed.insert(short))
            }
            (Some(best), _) => Probe::At(&self.landed.insert(best).1),
            (None, Some(short)) => Probe::Past(self.passed.insert(short)),
            (None, None) => Probe::Gone,
        }
    }

    fn found(&mut self) -> Option<C> {
        let side = self.landed.as_ref()?.0;
        self.sides[side].found()
    }

    fn source(&self, end: End) -> usize {
        self.sides[self.took[end.slot()]].source(end)
    }

    fn found_source(&self) -> usize {
        match &self.landed {
            Some((side, _)) => self.sides[*side].found_source(),
            None => 0,
        }
    }

    fn renumber(&mut self, sources: &[usize]) {
        for side in &mut self.sides {
            side.renumber(sources);
        }
    }

    fn follow(&mut self, lead: usize) {
        for side in &mut self.sides {
            side.follow(lead);
        }
    }

    fn adopt(&mut self, rows: &mut dyn std::any::Any) {
        for side in &mut self.sides {
            side.adopt(rows);
        }
    }

    fn rowless(&mut self) {
        for side in &mut self.sides {
            side.rowless();
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
/// [the schema evolution guide](https://github.com/archlemon/ferro/blob/main/docs/guide/schema-evolution.md)
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

impl<const N: usize> Fingerprints<N> {
    /// Collects the fingerprints of `table`'s indexes, one per slot.
    ///
    /// # Panics
    ///
    /// Panics, naming `table`, if two indexes have the same fingerprint.
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
    pub fn get(&self, slot: usize) -> u64 {
        self.values[slot]
    }

    /// The slots in ascending order of fingerprint, which is the order a table encodes its
    /// indexes in.
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

/// Every row in key order, for rebuilding a stale index on decode.
///
/// # Errors
///
/// Returns a read or decode error where a plain read would panic, so a damaged row node fails the
/// open instead, and [`Error::Decode`] for a row stored under a key other than its own, which an
/// index built from it would file under the wrong key.
pub fn stored_rows<T: Record>(rows: &Map<T::Key, T>) -> Result<Vec<Val<T>>> {
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
    entries: Iter<I, T::Key>,
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
    entries: Iter<I, (T::Key, S)>,
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
    entries: Iter<(I, T::Key), S>,
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

/// A collection field whose elements `#[index(each)]` indexes one by one.
///
/// Implemented for `Vec<X>`, `BTreeSet<X>`, [`Set<X>`](crate::Set) and `Option<X>`. A row holding
/// an element more than once gets one entry for it.
pub trait IndexEach {
    /// The element type, which the index is keyed on.
    type Value: Data + Ord;
    /// Whether [`values`](IndexEach::values) reads the file, as the nodes of a
    /// [`Set`](crate::Set) are read.
    #[doc(hidden)]
    const READS: bool = false;

    /// The elements to index.
    fn values(&self) -> Vec<Self::Value>;

    /// The elements to index, or the error of a read that [`values`](IndexEach::values) would
    /// panic on, as a damaged node of a [`Set`](crate::Set) gives. The default returns
    /// [`values`](IndexEach::values).
    ///
    /// # Errors
    ///
    /// The error of a node that cannot be read or decoded.
    #[doc(hidden)]
    fn try_values(&self) -> Result<Vec<Self::Value>> {
        Ok(self.values())
    }

    /// The elements to index, read through `differ` as the check that a table's indexes agree
    /// with its rows reads, so that a read that fails returns its error. The default returns
    /// [`values`](IndexEach::values).
    ///
    /// # Errors
    ///
    /// The error of a node that cannot be read or decoded.
    #[doc(hidden)]
    fn read_values(&self, _differ: &Differ<'_, '_>) -> Result<Vec<Self::Value>> {
        Ok(self.values())
    }

    /// The elements `self` holds that `newer` does not, and those `newer` holds that `self` does
    /// not, compared by `Ord`, each once and in order, read through `differ`. The default reads
    /// both whole with [`read_values`](IndexEach::read_values); a [`Set`](crate::Set) diffs its
    /// two versions, which reads only the nodes they do not share.
    ///
    /// # Errors
    ///
    /// The error of a node that cannot be read or decoded.
    #[doc(hidden)]
    fn delta(&self, newer: &Self, differ: &Differ<'_, '_>) -> Result<Delta<Self::Value>> {
        let (old, new) = (
            distinct(self.read_values(differ)?),
            distinct(newer.read_values(differ)?),
        );
        let (mut gone, mut come) = (Vec::new(), Vec::new());
        let (mut old, mut new) = (old.into_iter().peekable(), new.into_iter().peekable());
        loop {
            let order = match (old.peek(), new.peek()) {
                (Some(left), Some(came)) => left.cmp(came),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => return Ok((gone, come)),
            };
            match order {
                Ordering::Less => gone.extend(old.next()),
                Ordering::Greater => come.extend(new.next()),
                Ordering::Equal => {
                    old.next();
                    new.next();
                }
            }
        }
    }
}

/// `values` sorted, each once, as an index keeps them.
fn distinct<X: Ord>(mut values: Vec<X>) -> Vec<X> {
    values.sort_unstable();
    values.dedup_by(|a, b| Ord::cmp(&*a, &*b).is_eq());
    values
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
#[doc(hidden)]
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

impl<X: Data + Ord> IndexEach for Vec<X> {
    type Value = X;

    fn values(&self) -> Vec<X> {
        self.to_vec()
    }
}

impl<X: Data + Ord> IndexEach for std::collections::BTreeSet<X> {
    type Value = X;

    fn values(&self) -> Vec<X> {
        self.iter().cloned().collect()
    }
}

impl<X: Data + Ord> IndexEach for crate::set::Set<X> {
    type Value = X;
    const READS: bool = true;

    fn values(&self) -> Vec<X> {
        self.iter().collect()
    }

    fn try_values(&self) -> Result<Vec<X>> {
        Ok(self
            .map()
            .try_entries()?
            .into_iter()
            .map(|(value, ())| value)
            .collect())
    }

    fn read_values(&self, differ: &Differ<'_, '_>) -> Result<Vec<X>> {
        differ.keys(self.map())
    }

    fn delta(&self, newer: &Self, differ: &Differ<'_, '_>) -> Result<Delta<X>> {
        let (mut gone, mut come) = (Vec::new(), Vec::new());
        for (value, before, after) in differ.changed(self.map(), newer.map())? {
            match (before, after) {
                (Some(_), None) => gone.push(value),
                (None, Some(_)) => come.push(value),
                _ => {}
            }
        }
        Ok((gone, come))
    }
}

impl<X: Data + Ord> IndexEach for Option<X> {
    type Value = X;

    fn values(&self) -> Vec<X> {
        self.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::HashSink;
    use crate::{Db, Durability, Options, Set, pool};

    #[derive(crate::Data, crate::Record, Clone, Debug, PartialEq)]
    struct Tagged {
        #[key]
        id: u64,
        #[index(each)]
        tags: Set<u32>,
        #[index]
        group: u8,
        note: u64,
    }

    #[derive(crate::Data, Clone, Default)]
    struct Shelf {
        rows: Table<Tagged>,
    }

    #[derive(crate::Data, Clone, Default)]
    struct Shelves {
        many: Map<u64, Table<Tagged>>,
    }

    #[test]
    fn many_small_tables_are_checked_in_a_few_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let db: Db<Shelves> = Options::new()
            .durability(Durability::Buffered)
            .open(dir.path().join("shelves.ferro"))
            .unwrap();
        db.update(|shelves| {
            for at in 0..2_000u64 {
                let mut table: Table<Tagged> = Table::new();
                table.insert(Tagged {
                    id: at,
                    tags: (0..3).collect(),
                    group: (at % 7) as u8,
                    note: 0,
                });
                shelves.many.insert(at, table);
            }
        })
        .unwrap();
        let read = db.read();
        let found: Vec<FoundTable> = read.many.values().map(|table| found(&table)).collect();
        let mut plans = crate::codec::Plans::default();
        for table in &found {
            plans.build(table);
        }
        let view = read.many.store().unwrap().view(0, 1 << 40, u64::MAX);
        let (failed, jobs) = pool::scope(|pool| {
            let failed = crate::codec::Checks::default().finish(pool, &view, &plans, &found);
            (failed.map(|(_, error)| error.to_string()), pool.jobs())
        });
        assert_eq!(failed, None);
        assert!(jobs <= 200, "2,000 small tables took {jobs} jobs to check");
    }

    fn found(table: &Table<Tagged>) -> FoundTable {
        let mut sink = HashSink;
        let mut encoder = Encoder::new(&mut sink);
        table.encode(&mut encoder).unwrap();
        let object = encoder.into_object();
        FoundTable {
            id: TypeId::of::<TaggedTable>(),
            name: "TaggedTable",
            agree: agree::<Tagged>,
            beside: true,
            hash: object.hash(),
            body: object.body.into(),
            links: object.links.into(),
        }
    }

    fn checked(db: &Db<Shelf>, old: u64, new: u64) -> (Result<()>, u64, u32) {
        let allowance = 1 << 40;
        let (old, new) = (db.at(old).unwrap(), db.at(new).unwrap());
        let view = new
            .rows
            .rows()
            .store()
            .unwrap()
            .view(0, allowance, u64::MAX);
        let agreed = pool::scope(|pool| {
            agree::<Tagged>(
                &found(&new.rows),
                Some(&found(&old.rows)),
                &view,
                pool,
                true,
            )
        });
        let (left, records) = view.reads_left().unwrap();
        (agreed, allowance - left, records)
    }

    #[test]
    fn an_edit_reads_of_a_set_an_each_index_names_only_what_changed() {
        let dir = tempfile::tempdir().unwrap();
        let db: Db<Shelf> = Options::new()
            .durability(Durability::Buffered)
            .open(dir.path().join("shelf.ferro"))
            .unwrap();
        db.update(|shelf| {
            for id in 0..200 {
                let tags = (0..if id == 7 { 40_000 } else { 3 }).collect();
                shelf.rows.insert(Tagged {
                    id,
                    tags,
                    group: (id % 5) as u8,
                    note: 0,
                });
            }
        })
        .unwrap();
        db.update(|shelf| {
            shelf.rows.update(&7, |row| row.note = 1);
        })
        .unwrap();
        db.update(|shelf| {
            shelf.rows.update(&7, |row| {
                row.tags.remove(&20_000);
                row.tags.insert(50_000);
                row.group = 9;
            });
        })
        .unwrap();

        let (agreed, charged, records) = checked(&db, 1, 2);
        agreed.unwrap();
        assert_eq!(
            charged, 0,
            "an edit of a field no index names read the set again"
        );
        assert!(
            records < 12,
            "an edit of a field no index names read {records} records"
        );

        let (agreed, charged, records) = checked(&db, 2, 3);
        agreed.unwrap();
        assert_eq!(charged, 0, "an edit of the set read it again");
        assert!(records < 100, "an edit of two tags read {records} records");
    }
}
