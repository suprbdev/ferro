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
//! [`last`](Table::last) find one row in O(log n). [`iter`](Table::iter), [`range`](Table::range)
//! and [`prefix`](Table::prefix) return an [`Iter`] over rows, and [`keys`](Table::keys) a [`Keys`],
//! each in key order from either end. [`diff`](Table::diff) returns a [`Diff`] of the rows that
//! changed between two versions, and [`rows`](Table::rows) gives the underlying [`Map`] for
//! `(key, row)` pairs and everything else a map can do.
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
//! You choose the index: only a combination of lookups plans, picking how to read the lookups you
//! gave it. The [`Record`](derive@crate::Record) derive lists every lookup it generates, for each
//! kind of index.
//!
//! # Combining lookups
//!
//! `find()` returns a view with the equality lookups of each non-unique index, which return
//! [`Matches`] instead of an [`IndexIter`]. [`Matches::and`] and [`Matches::or`] combine them by
//! primary key and read a row only for a key that matches, as described on [`Matches`]. They also
//! take a range or prefix lookup made on the table, against which the keys of the other sides are
//! tested one by one by reading each key's row when they are at most a quarter of its keys, as
//! [`IntoMatches`] describes. A chain of `and` counts the keys of each lookup from its index and
//! is led by the smallest, whatever the order it was written in, and [`Matches::explain`] shows
//! what it will do. Lookups of different tables, or of different versions of one, combine too,
//! each row read from a lookup that holds its key.
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
//! panics. [`Map::try_get`] on [`Table::rows`] returns that failure as an error instead. A write
//! that cannot load a node, or reads a row or an entry whose `Data` impl panics, panics too, and
//! leaves the rows and every index as they were: it works out every index entry it changes and
//! loads what it will take apart before it changes anything, and takes back the entries it added
//! if a load after them fails.
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
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::convert::Infallible;
use std::fmt;
use std::ops::{Bound, Deref, DerefMut, RangeBounds};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;

use crate::codec::{
    Checkpoint, Data, Decoder, Description, Encoder, FoundTable, Hash, Pairing, Tabled,
};
use crate::error::{Error, Result};
use crate::map::{self, Change, Conflict, Cursor, Finder, Map, Val, kin, same_value};
use crate::pool::Pool;
use crate::prefix::Prefix;
use crate::store::{Store, fresh_reads};
use derive::{Delta, Differ, EveryRow, Fault, Indexed, Project, RowChange, Rows, Stage, Stored};

pub(crate) mod derive;

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
    ///
    /// Each index is written in turn, so a load that fails partway leaves the indexes that came
    /// before it changed. The writes of [`Table`] leave the rows and every index as they were.
    fn reindex(&mut self, old: Option<&T>, new: Option<&T>);
    /// Changes the rows with `rows`, and every index for each row that changes from `old` to
    /// `new` as `changes` hands the pair to the [`Stage`] it is given, as
    /// [`reindex`](TableData::reindex) does for one.
    ///
    /// `changes` is called once, before `rows`, so a row read for it can be dropped as soon as it
    /// is handed over. The derive stages every index entry in an
    /// [`Edits`](crate::map::edits::Edits) and loads what the removals and batches of entries will
    /// load. Then it adds each lone entry and calls `rows`, either of which can fail, taking the
    /// entries it added back if one does, and last makes the removals and batches, which have
    /// nothing left to load. So a load that fails leaves the rows and every index as they were, as
    /// long as `rows` changes nothing when it fails. The default calls
    /// [`reindex`](TableData::reindex) for each change and then `rows`.
    #[doc(hidden)]
    fn write(
        &mut self,
        changes: &mut dyn FnMut(&mut Stage<'_, T>),
        rows: &mut dyn FnMut(&mut Rows<T>),
    ) {
        changes(&mut |old, new| self.reindex(old, new));
        rows(self.rows_mut());
    }
    /// Adds the index entries of `row`, which no row's key equals, as
    /// [`write`](TableData::write) does for a row that changes from `None`, leaving every index
    /// as it was if a load fails or a `Data` impl panics.
    ///
    /// The derive, for a table whose every index gives a row at most one entry, puts each entry
    /// straight into its index through a [`Fresh`](derive::Fresh), and if a later one fails, takes
    /// back those it put in, which loads nothing. The default calls [`write`](TableData::write).
    #[doc(hidden)]
    fn add(&mut self, row: &T) {
        self.write(&mut |stage| stage(None, Some(row)), &mut |_| {});
    }
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
    #[must_use]
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

/// The table value of `T` that `r` read since `start`, as [`noted`](derive::noted) notes it.
fn found<T: Record>(r: &Decoder<'_>, start: Checkpoint) -> FoundTable {
    r.table_since(
        start,
        TypeId::of::<T::Table>(),
        type_name::<T::Table>(),
        agree::<T>,
        !T::Table::indexing_reads(),
    )
}

/// Checks that each index `table` stores under a fingerprint `T` knows holds exactly the entries
/// its rows give, and that each row is stored under its own key, reading both through `store`.
/// With `strict`, a table that holds an index under a fingerprint `T` does not know fails too,
/// since that index would be trusted unchecked once `T` gains an index with that fingerprint.
///
/// With `base`, a table of the same type whose indexes agree with its rows and which stores every
/// index `table` does, it diffs the rows of the two and each index of the two on the threads of
/// `pool`, and checks that each index changed by exactly the entries of the rows that changed,
/// which costs what changed. Without one, or with one whose rows share no node with those of
/// `table` near their roots, as [`kin`] tells, so that the diff would read every row of both, it
/// builds the indexes from every row.
fn agree<'p, T: Record>(
    table: &FoundTable,
    base: Option<&FoundTable>,
    store: &'p Store,
    pool: &Pool<'p>,
    strict: bool,
) -> Result<()> {
    let read = |found: &FoundTable| {
        T::Table::decode_stored(&mut Decoder::with_store(
            &found.body,
            &found.links,
            Some(store),
        ))
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
        if slots.iter().zip(&held).all(|(slot, held)| !slot || *held)
            && kin(base.rows(), table.rows(), store)?
        {
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
/// collection, and so does a write that has to load one, which leaves the rows and every index as
/// they were. Writes that would break a unique index panic too, as described on
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
    #[must_use]
    pub fn new() -> Self {
        Table::default()
    }

    /// The rows as a [`Map`] from key to row, for `(key, row)` pairs, cursors, folds, diffs and
    /// the other reads a map has.
    #[must_use]
    pub fn rows(&self) -> &Map<T::Key, T> {
        self.0.rows()
    }

    /// The hash of the rows' tree, or `None` for an empty table.
    ///
    /// Two tables with the same rows have the same root hash, whatever order the rows went in,
    /// so it can serve as an HTTP `ETag` for the whole table. The indexes are left out, since
    /// they follow from the rows.
    #[must_use]
    pub fn root_hash(&self) -> Option<Hash> {
        self.0.rows().root_hash()
    }

    /// The number of rows, in O(1).
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.rows().len()
    }

    /// Whether the table has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.rows().is_empty()
    }

    /// The row with primary key `key`, in O(log n).
    #[must_use]
    pub fn get<Q>(&self, key: &Q) -> Option<Val<T>>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.rows().get(key)
    }

    /// Whether a row has primary key `key`, in O(log n).
    #[must_use]
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.rows().contains_key(key)
    }

    /// Every row, in key order, from either end.
    pub fn iter(&self) -> Iter<T> {
        Iter(self.0.rows().values())
    }

    /// Every primary key, in order, from either end.
    pub fn keys(&self) -> Keys<T> {
        Keys(self.0.rows().keys())
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
    pub fn range<Q, R>(&self, range: R) -> Iter<T>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        Iter(self.0.rows().range::<Q, R>(range).into_values())
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
    pub fn prefix<Q>(&self, head: &Q) -> Iter<T>
    where
        T::Key: Prefix<Q>,
        Q: Ord + ?Sized,
    {
        Iter(self.0.rows().prefix(head).into_values())
    }

    /// The row with the smallest key.
    #[must_use]
    pub fn first(&self) -> Option<Val<T>> {
        self.0.rows().first().map(|(_, row)| row)
    }

    /// The row with the largest key.
    #[must_use]
    pub fn last(&self) -> Option<Val<T>> {
        self.0.rows().last().map(|(_, row)| row)
    }

    /// Marks every row changed, so that the next commit writes every node of the rows again, with
    /// each row encoded as its type encodes it now, as [`Map::rewrite`] does for a map.
    ///
    /// A table is written by a commit as it is held, its indexes as its type now declares them,
    /// but its rows only in the nodes the commit changes. A [`Table`] inside a row of a node the
    /// commit does not touch keeps its indexes under the fingerprints its type gave when the row
    /// was written, and a peer whose type gives another fingerprint for one of them refuses it in a
    /// pull. After a table type held in the rows of another table gains, drops or changes an
    /// index, rewrite the outer table in a commit, then prune the versions before it with
    /// [`Keep`](crate::Keep). Reading each row rebuilds the indexes a table in it stores under
    /// another fingerprint, and drops those its type no longer declares. The rows encode as they
    /// did apart from those tables, so the indexes of this table are left as they are.
    ///
    /// It reads every row and holds them all, with the nodes, until the commit. A collection inside
    /// a row is written as it was, so a table whose rows hold tables whose rows hold tables is
    /// rewritten a level at a time, by rewriting each inner table too.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Data, Record, Table};
    ///
    /// #[derive(Data, Record, Clone, Debug, PartialEq)]
    /// struct Member {
    ///     #[key]
    ///     id: u64,
    ///     #[index]
    ///     team: u32,
    /// }
    ///
    /// #[derive(Data, Record, Clone, Debug, PartialEq)]
    /// struct Team {
    ///     #[key]
    ///     id: u32,
    ///     members: Table<Member>,
    /// }
    ///
    /// let mut members: Table<Member> = Table::new();
    /// members.insert(Member { id: 1, team: 7 });
    /// let mut teams: Table<Team> = Table::new();
    /// teams.insert(Team { id: 7, members });
    ///
    /// teams.rewrite();
    /// assert_eq!(teams.get(&7).unwrap().members.by_team(&7).len(), 1);
    /// ```
    pub fn rewrite(&mut self) {
        self.0.rows_mut().rewrite();
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
        let row = Arc::new(row);
        let (old, undo) = self.0.rows_mut().insert_undoable(row.key(), row.clone());
        let indexed = panic::catch_unwind(AssertUnwindSafe(|| match old.as_deref() {
            None => self.0.add(&row),
            Some(old) => self
                .0
                .write(&mut |stage| stage(Some(old), Some(&row)), &mut |_| {}),
        }));
        if let Err(payload) = indexed {
            self.0.rows_mut().undo_insert(&row.key(), undo);
            panic::resume_unwind(payload);
        }
        old
    }

    /// [`put`](Table::put) for a caller that puts the whole table back if it fails, which writes
    /// each index in turn.
    fn put_guarded(&mut self, row: T) {
        let key = row.key();
        let row = Arc::new(row);
        let old = self.0.rows_mut().insert_shared(key, row.clone());
        self.0.reindex(old.as_deref(), Some(&row));
    }

    /// [`remove`](Table::remove) for a caller that puts the whole table back if it fails, which
    /// writes each index in turn.
    fn remove_guarded(&mut self, key: &T::Key) {
        if let Some(old) = self.0.rows_mut().remove(key) {
            self.0.reindex(Some(&old), None);
        }
    }

    fn replace(&mut self, old: &T, row: T) {
        let old_key = old.key();
        let new_key = row.key();
        let row = Arc::new(row);
        if old_key == new_key {
            let mut entry = Some((new_key, row.clone()));
            self.0
                .write(&mut |stage| stage(Some(old), Some(&row)), &mut |rows| {
                    if let Some((key, row)) = entry.take() {
                        rows.put_shared(key, row);
                    }
                });
        } else {
            let displaced = self.0.rows().get(&new_key);
            let mut moved = Some(vec![(old_key, None), (new_key, Some(row.clone()))]);
            self.0.write(
                &mut |stage| {
                    stage(Some(old), None);
                    stage(displaced.as_deref(), Some(&row));
                },
                &mut |rows| {
                    if let Some(moved) = moved.take() {
                        rows.apply_shared(moved);
                    }
                },
            );
        }
    }

    /// Removes the row with primary key `key` and its index entries, returning the row.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<Val<T>>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let old = self.0.rows().get(key)?;
        self.0
            .write(&mut |stage| stage(Some(&old), None), &mut |rows| {
                rows.discard(key)
            });
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
    /// Reads every row once, in key order, then removes the rows as one batch, and their entries
    /// as one batch for each index. Until then it holds the keys and index entries of the rows it
    /// removes, not the rows.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        let mut read = Some(self.0.rows().clone());
        let gone = RefCell::new(Vec::new());
        self.0.write(
            &mut |stage| {
                let Some(read) = read.take() else {
                    return;
                };
                for (key, row) in read.iter() {
                    if !keep(&row) {
                        stage(Some(&row), None);
                        gone.borrow_mut().push((key, None));
                    }
                }
            },
            &mut |rows| {
                let gone = gone.take();
                if !gone.is_empty() {
                    rows.apply_shared(gone);
                }
            },
        );
    }

    /// Removes the rows whose key lies in `range` and returns them as a table of their own, with
    /// every index built.
    ///
    /// The rows' tree is cut along the two ends of the range, which touches O(log n) nodes, but
    /// every row that moves is read to move its index entries, as one batch for each index of
    /// either table, so the whole costs O(rows moved). A range that holds no key leaves the table
    /// as it was.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::{Data, Record, Table};
    ///
    /// #[derive(Data, Record, Clone)]
    /// struct Event {
    ///     #[key]
    ///     at: u64,
    ///     #[index]
    ///     kind: String,
    /// }
    ///
    /// let mut events: Table<Event> = Table::new();
    /// for at in 0..6 {
    ///     let kind = if at % 2 == 0 { "click" } else { "view" };
    ///     events.insert(Event { at, kind: kind.into() });
    /// }
    ///
    /// let old = events.take_range(..3);
    /// assert_eq!(old.keys().collect::<Vec<_>>(), [0, 1, 2]);
    /// assert_eq!(old.by_kind("click").len(), 2);
    /// assert_eq!(events.by_kind("click").len(), 1);
    /// assert_eq!(events.remove_range(4..), 2);
    /// assert_eq!(events.keys().collect::<Vec<_>>(), [3]);
    /// ```
    #[must_use = "use `remove_range` if the removed entries are not needed"]
    pub fn take_range<Q, R>(&mut self, range: R) -> Table<T>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        let mut kept = self.0.rows().clone();
        let moved = kept.take_range::<Q, R>(range);
        let mut taken = Table::new();
        if moved.is_empty() {
            return taken;
        }
        taken.0.write(
            &mut |stage| moved.for_each(|_, row| stage(None, Some(row))),
            &mut |rows| *rows = moved.clone(),
        );
        self.cut(kept, &moved);
        taken
    }

    /// Removes the rows whose key lies in `range` and returns how many there were.
    ///
    /// The rows' tree is cut along the two ends of the range, which touches O(log n) nodes, but
    /// every removed row is read to remove its index entries, as one batch for each index, so the
    /// whole costs O(rows removed).
    pub fn remove_range<Q, R>(&mut self, range: R) -> usize
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        let mut kept = self.0.rows().clone();
        let moved = kept.take_range::<Q, R>(range);
        if moved.is_empty() {
            return 0;
        }
        self.cut(kept, &moved);
        moved.len()
    }

    fn cut(&mut self, kept: Rows<T>, moved: &Rows<T>) {
        self.0.write(
            &mut |stage| moved.for_each(|_, row| stage(Some(row), None)),
            &mut |rows| *rows = kept.clone(),
        );
    }

    /// The rows added, removed and changed between this table and `newer`, in key order, as a
    /// [`Change`] each.
    ///
    /// Subtrees with the same hash are skipped, so a diff costs O(changes), not O(rows). Rows are
    /// compared by their encoding, not with `==`.
    pub fn diff(&self, newer: &Table<T>) -> Diff<T> {
        Diff(self.0.rows().diff(newer.0.rows()))
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
    /// # Panics
    ///
    /// Panics, as a write does, if a node or row cannot be loaded, and leaves the table as it was
    /// before the merge.
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
    ///
    /// # Panics
    ///
    /// Panics, as a write does, if a node or row cannot be loaded, and leaves the table as it was
    /// before the merge.
    pub fn try_merge<E>(
        &mut self,
        base: &Table<T>,
        theirs: &Table<T>,
        resolve: impl FnMut(Conflict<T::Key, T>) -> std::result::Result<Option<T>, E>,
    ) -> std::result::Result<usize, E> {
        let mut guard = Restore {
            kept: Some(self.clone()),
            table: self,
        };
        let merged = guard.table.merge_rows(base, theirs, resolve);
        if merged.is_ok() {
            guard.kept = None;
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
                    self.remove_guarded(&key);
                }
                Some(row) if row.key() != key => {}
                Some(row) if self.0.conflict(&row, None).is_none() => {
                    self.put_guarded(row);
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
                    self.remove_guarded(&key);
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
                        self.put_guarded(row);
                    }
                    held.freed(&key);
                }
            }
        }
    }
}

/// Puts back the table it holds when it drops, on an error or a panic, unless the work it guards
/// went through and took `kept`.
struct Restore<'a, T: Record> {
    table: &'a mut Table<T>,
    kept: Option<Table<T>>,
}

impl<T: Record> Drop for Restore<'_, T> {
    fn drop(&mut self) {
        if let Some(kept) = self.kept.take() {
            *self.table = kept;
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

impl<T: Record + fmt::Debug> fmt::Debug for RowMut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
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

    fn tables(t: &mut Tabled) {
        t.table();
    }

    fn pair(old: &mut Decoder<'_>, new: &mut Decoder<'_>, pairing: &mut Pairing<'_>) -> Result<()> {
        let (from_old, from_new) = (old.checkpoint(), new.checkpoint());
        T::Table::pair(old, new, pairing)?;
        pairing.table(
            TypeId::of::<T::Table>(),
            (old, from_old),
            (new, from_new),
            found::<T>,
        );
        Ok(())
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

/// An iterator over the rows of a [`Table`], in key order, from either end, made by
/// [`Table::iter`], [`Table::range`] and [`Table::prefix`].
///
/// It knows its length from the start, and holds the table as it was when it was made, so
/// changes to the table afterwards do not reach it. Each row comes as a [`Val`], and
/// [`next_ref`](Iter::next_ref) and [`visit`](Iter::visit) lend it instead, as [Reading rows in
/// place](https://github.com/archlemon/ferro/blob/main/docs/guide/tables.md#reading-rows-in-place)
/// describes.
///
/// # Panics
///
/// A step that cannot load a node or row from the file, or finds one damaged, panics.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Iter<T: Record>(map::Values<T::Key, T>);

impl<T: Record> fmt::Debug for Iter<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Iter").finish_non_exhaustive()
    }
}

impl<T: Record> Iter<T> {
    /// Steps to the next row from the front and lends it, where `next` hands out a [`Val`].
    ///
    /// The row is read in place, without the reference count a [`Val`] takes on its node's
    /// column, and stays lent until the iterator moves again.
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
    ///     total: u64,
    /// }
    ///
    /// let mut orders: Table<Order> = Table::new();
    /// for id in 1..=10 {
    ///     orders.insert(Order { id, total: id * 100 });
    /// }
    ///
    /// let mut rows = orders.range(3..);
    /// let mut page = Vec::new();
    /// while page.len() < 3
    ///     && let Some(order) = rows.next_ref()
    /// {
    ///     page.push(order.total);
    /// }
    /// assert_eq!(page, [300, 400, 500]);
    /// assert_eq!(rows.len(), 5);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if a node or row cannot be read, as `next` does.
    pub fn next_ref(&mut self) -> Option<&T> {
        self.0.next_ref()
    }

    /// Calls `f` on each row the iterator has left, in key order from the front, lending it as
    /// [`next_ref`](Iter::next_ref) does.
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
    ///     total: u64,
    /// }
    ///
    /// let mut orders: Table<Order> = Table::new();
    /// for id in 1..=10 {
    ///     orders.insert(Order { id, total: id * 100 });
    /// }
    ///
    /// let mut total = 0;
    /// orders.range(3..=5).visit(|order| total += order.total);
    /// assert_eq!(total, 1200);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if a node or row cannot be read, as `next` does.
    pub fn visit(self, f: impl FnMut(&T)) {
        self.0.visit(f);
    }
}

impl<T: Record> Iterator for Iter<T> {
    type Item = Val<T>;

    #[inline]
    fn next(&mut self) -> Option<Val<T>> {
        self.0.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    #[inline]
    fn fold<B, F: FnMut(B, Val<T>) -> B>(self, init: B, f: F) -> B {
        self.0.fold(init, f)
    }
}

impl<T: Record> DoubleEndedIterator for Iter<T> {
    #[inline]
    fn next_back(&mut self) -> Option<Val<T>> {
        self.0.next_back()
    }
}

impl<T: Record> ExactSizeIterator for Iter<T> {}

/// An iterator over the primary keys of a [`Table`], in order, from either end, made by
/// [`Table::keys`].
///
/// It reads no rows, knows its length from the start, and holds the table as it was when it was
/// made.
///
/// # Panics
///
/// A step that cannot load a node from the file, or finds one damaged, panics.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Keys<T: Record>(map::Keys<T::Key, T>);

impl<T: Record> fmt::Debug for Keys<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keys").finish_non_exhaustive()
    }
}

impl<T: Record> Iterator for Keys<T> {
    type Item = T::Key;

    #[inline]
    fn next(&mut self) -> Option<T::Key> {
        self.0.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    #[inline]
    fn fold<B, F: FnMut(B, T::Key) -> B>(self, init: B, f: F) -> B {
        self.0.fold(init, f)
    }
}

impl<T: Record> DoubleEndedIterator for Keys<T> {
    #[inline]
    fn next_back(&mut self) -> Option<T::Key> {
        self.0.next_back()
    }
}

impl<T: Record> ExactSizeIterator for Keys<T> {}

/// An iterator over the [`Change`]s between two versions of a [`Table`], in key order, made by
/// [`Table::diff`].
///
/// Each change carries the primary key and the row before, after or both. It is lazy: each step
/// expands only the subtrees the two versions do not share, until it finds the next change, so
/// stopping early costs only what was read. It holds both versions, and changes to either table
/// afterwards do not reach it.
///
/// # Examples
///
/// ```
/// use ferro::{Change, Data, Record, Table};
///
/// #[derive(Data, Record, Clone)]
/// struct Order {
///     #[key]
///     id: u64,
///     total: u64,
/// }
///
/// let mut old: Table<Order> = Table::new();
/// old.insert(Order { id: 1, total: 100 });
/// old.insert(Order { id: 2, total: 200 });
/// let mut new = old.clone();
/// new.remove(&1);
/// new.insert(Order { id: 2, total: 250 });
///
/// let changed: Vec<String> = old
///     .diff(&new)
///     .map(|change| match change {
///         Change::Added(id, order) => format!("+{id} {}", order.total),
///         Change::Removed(id, order) => format!("-{id} {}", order.total),
///         Change::Changed(id, before, after) => format!("~{id} {} {}", before.total, after.total),
///     })
///     .collect();
/// assert_eq!(changed, ["-1 100", "~2 200 250"]);
/// ```
///
/// # Panics
///
/// A step that cannot load a node or row from the file, or finds one damaged, panics.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Diff<T: Record>(map::Diff<T::Key, T>);

impl<T: Record> fmt::Debug for Diff<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Diff").finish_non_exhaustive()
    }
}

impl<T: Record> Iterator for Diff<T> {
    type Item = Change<T::Key, T>;

    #[inline]
    fn next(&mut self) -> Option<Change<T::Key, T>> {
        self.0.next()
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

struct Entries<I, K, S, C> {
    iter: map::Iter<(I, K), S>,
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
    iter: map::Iter<I, K>,
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
    iter: map::Iter<I, (K, S)>,
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
            parked: None,
            side: End::Front,
            quit: false,
            rowless: false,
            budget: 0,
            spent: 0,
            tests: 0,
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
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct IndexIter<T: Record, C = ()> {
    rows: Finder<T::Key, T>,
    entries: Box<dyn EntryCursor<T::Key, C>>,
}

impl<T: Record, C> fmt::Debug for IndexIter<T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexIter").finish_non_exhaustive()
    }
}

impl<T: Record, C: 'static> IndexIter<T, C> {
    #[inline(always)]
    fn over<I: Data + Ord, S: Data>(
        rows: &Map<T::Key, T>,
        entries: map::Iter<(I, T::Key), S>,
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
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct IndexCovered<T: Record, C = ()>(Box<dyn EntryCursor<T::Key, C>>);

impl<T: Record, C> fmt::Debug for IndexCovered<T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexCovered").finish_non_exhaustive()
    }
}

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
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct IndexKeys<T: Record>(Box<dyn KeyCursor<T::Key>>);

impl<T: Record> fmt::Debug for IndexKeys<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexKeys").finish_non_exhaustive()
    }
}

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
/// key order. A range or prefix lookup passed to `and` or `or`, or leading a chain of `and`
/// through [`into_matches`](IntoMatches::into_matches), is tested rather than read when the
/// combination only looks keys up in it, because the keys the other sides of the `and` lead to it
/// are at most a quarter of its own: each such key is tested by reading its row from the table the
/// lookup was made on and checking the indexed value against the span of entries the lookup has
/// left. Otherwise the first time the combination is read the lookup's keys and covered fields are
/// read in full and sorted, as they are for one that leads its `and`, one whose covered fields the
/// combination yields, one passed through [`IndexIter::uncovered`], and every other
/// [`IndexIter`]. A lookup of one value is in key order already and sorts in one pass. A range on
/// the left side of an `and` is tested through the finder of a lookup on the other side of the
/// same table at the same version, since its own reads the combination's rows.
///
/// A row found in the cache costs about as much to test as four entries do to read, but one read
/// from the file costs as much as dozens. So tests count the rows they had to read from the file:
/// those of every key under [`Matches::keys`] and [`Matches::covered`], which yield no rows, and
/// otherwise those of keys the lookup does not hold, since a row that matches is read anyway. Once
/// they come to more than about a 64th of the entries the lookup has, or the keys tested to more
/// than a quarter of them, which only a chain of `and` that estimated too few keys can reach, the
/// lookup is read in full for the keys left. So a combination whose rows are not cached costs at
/// most about twice what reading the lookup would.
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
/// so a few keys against many cost a few searches. A chain such as `a.and(b).and(c)` is one `and`
/// of up to 64 sides, however its `and`s nest, as in `a.and(b.and(c))`: the side with the fewest
/// keys leads, those within four times its keys are walked beside it, and the rest are searched,
/// fewest keys first, each only for a key every side before it holds. The order the chain was
/// written in decides only which side's rows and covered fields it yields, and which of two sides
/// with as many keys leads. An `or`, and a combination already read from, is one side of a chain,
/// and a chain of 64 sides given another becomes one side of a new chain. A range or prefix lookup
/// on the table is not in key order, so it is searched by reading the row of each key and testing
/// it against the range, and when it has less than four times the keys led to it it is read in
/// full and sorted instead, as [`IntoMatches`] describes. In a chain, the keys led to a range are
/// estimated from the counts of the sides walked before it, as if they matched independently. The
/// choices are made when the combination is first read, from the number of keys each lookup counts
/// in its index, and [`explain`](Matches::explain) shows them. A combination reads the indexes and
/// rows as they were when its lookups were built.
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
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Matches<T: Record, C = ()> {
    rows: Finder<T::Key, T>,
    more: More<T>,
    stream: Box<dyn Stream<T::Key, C>>,
    started: bool,
}

impl<T: Record, C> fmt::Debug for Matches<T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Matches").finish_non_exhaustive()
    }
}

type More<T> = Option<Box<Vec<Finder<<T as Record>::Key, T>>>>;

type Adopted<T, C> = (
    Box<dyn Stream<<T as Record>::Key, C>>,
    Option<Finder<<T as Record>::Key, T>>,
);

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
    /// tested key by key when the keys led to it are at most a quarter of its own and read in full
    /// and sorted otherwise. When this combination is itself an `and` not yet read, `other` joins
    /// it as one more side, and when `other` is an `and` not yet read, each of its sides joins, up
    /// to 64 sides in all, so `a.and(b.and(c))` is the chain `a.and(b).and(c)`, and a chain of
    /// `and` is led by its smallest side whatever the order and nesting it was written in, as
    /// [`Matches`] describes. An `or` joins as one side, and a chain of 64 sides given another
    /// becomes one side of a new chain. The result keeps this side's covered fields, and reads each
    /// row from this side. `other` may be a lookup of another table, or of another version of this
    /// one: it matches the keys its own table holds, and decides only which of this side's rows are
    /// yielded. If either combination has already yielded rows, what it has left is read into a
    /// sorted list first, which joins as one side.
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
        let mut left = self.unstarted();
        let right = other.into_matches().unstarted();
        let alone = left.more.is_none();
        let same = alone
            && right.more.is_none()
            && (left.stream.ranges() || right.stream.ranges())
            && left.rows.same_map(&right.rows);
        let (mut right, spare) = right.adopted();
        if same
            && let Some(rows) = spare
            && left.stream.wants()
        {
            left.stream.adopt(&mut Spare(Some(Box::new(rows))));
        }
        let total = left.rows.len();
        let stream = match right.split() {
            None => chained(left.stream, Box::new(Blind(right)), alone, same, total),
            Some(sides) => sides
                .into_iter()
                .fold(left.stream, |stream, (side, shared)| {
                    chained(stream, side, alone, same && shared, total)
                }),
        };
        Matches {
            rows: left.rows,
            more: left.more,
            stream,
            started: false,
        }
    }

    /// The rows that this combination or `other` matches, each once.
    ///
    /// Both sides must have the same covered fields, or the call does not compile, since a key only
    /// `other` holds comes with `other`'s covered fields; [`and`](Matches::and) has no such bound.
    /// Call [`uncovered`](Matches::uncovered), or [`IndexIter::uncovered`], on each side to combine
    /// lookups of differently covered indexes. A key both sides hold takes its row and covered
    /// fields from this side, and a key only `other` holds takes them from `other`. So when `other`
    /// is a lookup of another table, or of another version of this one, the combination yields
    /// every row either holds, each from a table whose lookup matched it. If this combination has
    /// already yielded rows, what it has left is read into a sorted list first.
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

    fn adopted(self) -> Adopted<T, C> {
        let mut stream = self.stream;
        match self.more {
            None => {
                let mut rows = Some(self.rows);
                stream.adopt(&mut rows);
                (stream, rows)
            }
            Some(more) => {
                stream.adopt(&mut Finders::<T>::of(Some(self.rows), *more));
                (stream, None)
            }
        }
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
    /// How the combination reads its lookups: which side of each `and` leads, which is searched,
    /// which range or prefix lookup is tested key by key and which is read in full first.
    ///
    /// It reads no entry and changes nothing. The choices come from the number of keys of each
    /// lookup, which an index counts from its nodes without reading its entries, and are the ones
    /// the combination makes when it is first read, as rows or as [`keys`](Matches::keys).
    /// [`covered`](Matches::covered) reads in full, rather than tests, a range or prefix lookup it
    /// takes covered fields from: the left side of an `and`, and each side of an `or` that gives
    /// the combination its fields. It tests other ranges as shown, though in a chain the estimate
    /// that decides them changes when such a range is walked instead. A combination already read
    /// from reports what it is doing. A tested range whose rows turn out to be read from the file
    /// can still be read in full partway, as [`IntoMatches`] describes.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::table::{IntoMatches, Read, Step};
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
    /// let late = orders.by_day_range(5..).into_matches().and(orders.find().by_customer("wile"));
    /// let plan = late.explain();
    /// assert_eq!((plan.step, plan.sides[0].step), (Step::And, Step::Range));
    /// assert_eq!((plan.sides[0].read, plan.sides[1].read), (Read::Tested, Read::Walked));
    /// assert_eq!(
    ///     plan.to_string(),
    ///     "and, at most 4 keys, walked\n  range, 331 keys, tested\n  value, 4 keys, walked"
    /// );
    /// assert_eq!(late.keys().collect::<Vec<_>>(), [100, 200, 400]);
    /// ```
    #[must_use]
    pub fn explain(&self) -> Explain {
        self.stream.explain(Role::Walked, false)
    }

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
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct MatchKeys<T: Record>(Box<dyn Stream<T::Key, ()>>);

impl<T: Record> fmt::Debug for MatchKeys<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MatchKeys").finish_non_exhaustive()
    }
}

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
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct MatchCovered<T: Record, C = ()>(Box<dyn Stream<T::Key, C>>);

impl<T: Record, C> fmt::Debug for MatchCovered<T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MatchCovered").finish_non_exhaustive()
    }
}

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

/// How [`Matches`] reads one of its lookups, or one `and` or `or` of them, from
/// [`Matches::explain`].
///
/// A lookup has no [`sides`](Explain::sides), and an `and` or an `or` has one for each lookup or
/// combination it was given, left first, where an `and` not yet read given to an `and` counts as
/// its own sides. `Display` prints the tree one step a line, each side indented below the step it
/// belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Explain {
    /// What the step is.
    pub step: Step,
    /// How the step is read.
    pub read: Read,
    /// The keys the step holds: exactly for a lookup, counted from the index without reading its
    /// entries, and at most for an `and` or an `or`.
    pub keys: usize,
    /// The sides of an `and` or an `or`, left first, and none for a lookup.
    pub sides: Vec<Explain>,
}

impl Explain {
    fn of(step: Step, keys: usize, read: Read) -> Self {
        Explain {
            step,
            read,
            keys,
            sides: Vec::new(),
        }
    }

    fn write(&self, f: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
        let step = match self.step {
            Step::Value => "value",
            Step::Range => "range",
            Step::List => "list",
            Step::And => "and",
            Step::Or => "or",
        };
        let read = match self.read {
            Read::Walked => "walked",
            Read::Searched => "searched",
            Read::Tested => "tested",
            Read::Collected => "collected",
        };
        let most = match self.step {
            Step::And | Step::Or => "at most ",
            Step::Value | Step::Range | Step::List => "",
        };
        let indent = 2 * depth;
        write!(f, "{:indent$}{step}, {most}{} keys, {read}", "", self.keys)?;
        for side in &self.sides {
            writeln!(f)?;
            side.write(f, depth + 1)?;
        }
        Ok(())
    }
}

impl fmt::Display for Explain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, 0)
    }
}

/// What one step of an [`Explain`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Step {
    /// A lookup of one value of a non-unique index from the `find()` view, in key order.
    Value,
    /// A range or prefix lookup made on the table, which is not in key order.
    Range,
    /// A list of keys sorted in memory: any other [`IndexIter`], a range or prefix lookup passed
    /// through [`IndexIter::uncovered`], or what a combination had left when it was combined
    /// again.
    List,
    /// The rows both sides match.
    And,
    /// The rows either side matches.
    Or,
}

/// How one step of an [`Explain`] is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Read {
    /// Stepped through in key order. The walked sides of an `and` lead it, and when both are
    /// walked they leapfrog.
    Walked,
    /// Searched for each key that the walked side of the `and` it belongs to offers, which skips
    /// the keys in between.
    Searched,
    /// A range or prefix tested against each key that the walked side of its `and` offers, by
    /// reading that key's row and checking the indexed value against the range.
    Tested,
    /// Read in full and sorted the first time the combination is read, then walked or searched as
    /// a list.
    Collected,
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
    fn spare(&mut self) -> Option<Box<dyn std::any::Any + Send>> {
        None
    }
    fn spares(&self) -> bool {
        false
    }
    fn wants(&self) -> bool {
        false
    }
    fn testable(&self) -> bool {
        false
    }
    fn ranges(&self) -> bool {
        false
    }
    fn join(
        &mut self,
        right: Box<dyn Stream<K, ()>>,
        _same: bool,
        _total: usize,
    ) -> Option<Box<dyn Stream<K, ()>>> {
        Some(right)
    }
    fn split(&mut self) -> Option<Vec<Side<K>>> {
        None
    }
    fn explain(&self, role: Role, offered: bool) -> Explain;
}

struct Spare(Option<Box<dyn std::any::Any + Send>>);

#[derive(Clone, Copy)]
enum Role {
    Walked,
    Probed(usize),
}

impl Role {
    fn read(self) -> Read {
        match self {
            Role::Walked => Read::Walked,
            Role::Probed(_) => Read::Searched,
        }
    }
}

const STEPS: usize = 16;

struct EqualStream<I, K, S, C> {
    index: Map<(I, K), S>,
    iter: map::Iter<(I, K), S>,
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
    fn new(index: Map<(I, K), S>, iter: map::Iter<(I, K), S>, project: Project<I, S, C>) -> Self {
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

    fn explain(&self, role: Role, _offered: bool) -> Explain {
        Explain::of(Step::Value, self.len(), role.read())
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

    fn explain(&self, role: Role, _offered: bool) -> Explain {
        let read = match self.pending {
            Some(_) => Read::Collected,
            None => role.read(),
        };
        Explain::of(Step::List, self.len(), read)
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
    parked: Option<(T::Key, bool)>,
    side: End,
    quit: bool,
    rowless: bool,
    budget: u64,
    spent: u64,
    tests: u64,
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
            self.parked = None;
        }
    }

    fn testing(&mut self) -> bool {
        if self.quit {
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
        self.tests = self.budget / RATIO as u64;
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

    fn test(&mut self, key: &T::Key) -> bool {
        let before = fresh_reads();
        let holds = self.holds(key);
        if !holds || self.rowless {
            self.spent += fresh_reads().wrapping_sub(before);
        }
        match self.tests.checked_sub(1) {
            Some(tests) if self.spent.saturating_mul(FRESH) <= self.budget => self.tests = tests,
            _ => {
                self.quit = true;
                self.edges = None;
            }
        }
        holds
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
        self.parked = None;
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
        if !matches!(&self.landed, Some((key, _)) if key == target) {
            if matches!(&self.parked, Some((key, _)) if key == target) {
                std::mem::swap(&mut self.landed, &mut self.parked);
                self.side = self.side.other();
            } else {
                let holds = self.test(target);
                if end != self.side {
                    self.parked = self.landed.take();
                    self.side = end;
                }
                self.landed = Some((target.clone(), holds));
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
        } else if let Some(Spare(rows)) = rows.downcast_mut::<Spare>()
            && self.spanned.is_some()
            && let Some(rows) = rows.take_if(|rows| rows.is::<Finder<T::Key, T>>())
        {
            self.rows = rows.downcast().ok().map(|rows| *rows);
        }
    }

    fn spare(&mut self) -> Option<Box<dyn std::any::Any + Send>> {
        let rows = self.rows.take()?;
        Some(Box::new(rows))
    }

    fn spares(&self) -> bool {
        self.rows.is_some()
    }

    fn wants(&self) -> bool {
        self.rows.is_none() && self.spanned.is_some()
    }

    fn testable(&self) -> bool {
        self.spanned.is_some() && !self.covering && !self.quit
    }

    fn ranges(&self) -> bool {
        self.spanned.is_some()
    }

    fn explain(&self, role: Role, offered: bool) -> Explain {
        let read = match (&self.spanned, role) {
            (None, role) => role.read(),
            (Some(_), Role::Walked) => Read::Collected,
            (Some(spanned), Role::Probed(lead)) => {
                let left = spanned.cursor.left();
                let tests = !self.quit
                    && (self.edges.is_some()
                        || !self.covering
                            && left > 0
                            && (self.rows.is_some() || offered)
                            && self.lead.unwrap_or(lead).saturating_mul(RATIO) <= left);
                if tests { Read::Tested } else { Read::Collected }
            }
        };
        Explain::of(Step::Range, self.len(), read)
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

    fn spare(&mut self) -> Option<Box<dyn std::any::Any + Send>> {
        self.0.spare()
    }

    fn spares(&self) -> bool {
        self.0.spares()
    }

    fn wants(&self) -> bool {
        self.0.wants()
    }

    fn testable(&self) -> bool {
        self.0.testable()
    }

    fn ranges(&self) -> bool {
        self.0.ranges()
    }

    fn join(
        &mut self,
        right: Box<dyn Stream<K, ()>>,
        same: bool,
        total: usize,
    ) -> Option<Box<dyn Stream<K, ()>>> {
        self.0.join(right, same, total)
    }

    fn split(&mut self) -> Option<Vec<Side<K>>> {
        self.0.split()
    }

    fn explain(&self, role: Role, offered: bool) -> Explain {
        self.0.explain(role, offered)
    }
}

#[derive(Clone, Copy)]
enum Plan {
    Merge,
    Left,
    Right,
    Many,
}

const RATIO: usize = 4;

const FRESH: u64 = 64;

const SIDES: usize = 64;

type Side<K> = (Box<dyn Stream<K, ()>>, bool);

struct All<K, C> {
    left: Box<dyn Stream<K, C>>,
    right: Box<dyn Stream<K, ()>>,
    more: Vec<Box<dyn Stream<K, ()>>>,
    plan: Option<Plan>,
    settled: [bool; 2],
    covering: bool,
    landed: Option<K>,
    shared: u64,
    ordered: bool,
    order: [u8; SIDES],
    walks: usize,
    total: usize,
}

macro_rules! side {
    ($all:expr, $at:expr, |$side:ident| $body:expr) => {
        match $at {
            0 => {
                let $side = &mut *$all.left;
                $body
            }
            1 => {
                let $side = &mut *$all.right;
                $body
            }
            at => {
                let $side = &mut *$all.more[at - 2];
                $body
            }
        }
    };
}

macro_rules! seen {
    ($all:expr, $at:expr, |$side:ident| $body:expr) => {
        match $at {
            0 => {
                let $side = &*$all.left;
                $body
            }
            1 => {
                let $side = &*$all.right;
                $body
            }
            at => {
                let $side = &*$all.more[at - 2];
                $body
            }
        }
    };
}

#[inline]
fn chained<K: Data + Ord, C: Send + 'static>(
    mut left: Box<dyn Stream<K, C>>,
    right: Box<dyn Stream<K, ()>>,
    alone: bool,
    same: bool,
    total: usize,
) -> Box<dyn Stream<K, C>> {
    match left.join(right, same, total) {
        None => left,
        Some(right) => Box::new(All::new(left, right, alone, same)),
    }
}

fn estimate(candidates: usize, len: usize, total: usize) -> usize {
    let estimate = match total {
        0 => candidates,
        total => (candidates as u128 * len as u128).div_ceil(total as u128) as usize,
    };
    estimate.min(candidates).min(len)
}

fn choose(left: usize, right: usize) -> Plan {
    if left.max(right) < left.min(right).saturating_mul(RATIO) {
        Plan::Merge
    } else if left <= right {
        Plan::Left
    } else {
        Plan::Right
    }
}

enum Met<K> {
    At(K),
    Past(K),
    Gone,
}

fn met<K: Clone>(probe: Probe<'_, K>) -> Met<K> {
    match probe {
        Probe::At(key) => Met::At(key.clone()),
        Probe::Past(key) => Met::Past(key.clone()),
        Probe::Gone => Met::Gone,
    }
}

fn joined<K: Ord + Clone, X, Y>(
    a: &mut dyn Stream<K, X>,
    b: &mut dyn Stream<K, Y>,
    end: End,
    leads: bool,
) -> bool {
    match leads {
        true => led(a, b, end),
        false => merged(a, b, end),
    }
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
    fn new(
        left: Box<dyn Stream<K, C>>,
        right: Box<dyn Stream<K, ()>>,
        alone: bool,
        same: bool,
    ) -> Self {
        All {
            left,
            right,
            more: Vec::new(),
            plan: None,
            settled: [false, false],
            covering: false,
            landed: None,
            shared: u64::from(alone) | u64::from(same) << 1,
            ordered: false,
            order: [0; SIDES],
            walks: 0,
            total: 0,
        }
    }

    fn sides(&self) -> usize {
        2 + self.more.len()
    }

    fn len_at(&self, at: usize) -> usize {
        seen!(self, at, |side| side.len())
    }

    fn right_at(&mut self, at: usize) -> &mut dyn Stream<K, ()> {
        match at {
            1 => &mut *self.right,
            at => &mut *self.more[at - 2],
        }
    }

    fn shares(&self, at: usize) -> bool {
        self.shared & 1 << at != 0
    }

    fn by_length(&self) -> [u8; SIDES] {
        let sides = self.sides();
        let mut lens = [0; SIDES];
        let mut order = [0; SIDES];
        for at in 0..sides {
            lens[at] = self.len_at(at);
            order[at] = at as u8;
        }
        order[..sides].sort_by_key(|at| lens[usize::from(*at)]);
        order
    }

    fn ordered(&mut self) {
        if !self.ordered {
            self.order = self.by_length();
            self.ordered = true;
        }
    }

    fn walked(&self) -> ([u8; SIDES], usize, usize) {
        let sides = self.sides();
        let by_length = self.by_length();
        let lead = self.len_at(usize::from(by_length[0]));
        let (mut order, mut walks, mut candidates) = ([0; SIDES], 0, lead);
        let (mut probed, mut probes) = ([0; SIDES], 0);
        for &at in &by_length[..sides] {
            let side = usize::from(at);
            let len = self.len_at(side);
            let bound = match seen!(self, side, |side| side.testable()) {
                true => candidates,
                false => lead,
            };
            if walks == 0 || len < bound.saturating_mul(RATIO) {
                if walks > 0 {
                    candidates = estimate(candidates, len, self.total);
                }
                order[walks] = at;
                walks += 1;
            } else {
                probed[probes] = at;
                probes += 1;
            }
        }
        order[walks..sides].copy_from_slice(&probed[..probes]);
        (order, walks, candidates)
    }

    fn walking(&self) -> ([u8; SIDES], usize) {
        let mut pair = [0; SIDES];
        match self.plan {
            Some(Plan::Merge) => {
                pair[1] = 1;
                (pair, 2)
            }
            Some(Plan::Left) => {
                pair[1] = 1;
                (pair, 1)
            }
            Some(Plan::Right) => {
                pair[0] = 1;
                (pair, 1)
            }
            Some(Plan::Many) => (self.order, self.walks),
            None => {
                let (order, walks, _) = self.walked();
                (order, walks)
            }
        }
    }

    fn lend(&mut self, from: usize, to: usize) {
        if !self.shares(from) || !self.shares(to) || !seen!(self, to, |side| side.wants()) {
            return;
        }
        if let Some(rows) = side!(self, from, |side| side.spare()) {
            side!(self, to, |side| side.adopt(&mut Spare(Some(rows))));
        }
    }

    #[inline]
    fn plan(&mut self) -> Plan {
        match self.plan {
            Some(plan) => plan,
            None => self.planned(),
        }
    }

    #[inline(never)]
    fn planned(&mut self) -> Plan {
        let plan = if self.more.is_empty() {
            let (left, right) = (self.left.len(), self.right.len());
            let plan = choose(left, right);
            match plan {
                Plan::Left => {
                    self.right.follow(left);
                    self.lend(0, 1);
                }
                Plan::Right => {
                    self.left.follow(right);
                    self.lend(1, 0);
                }
                Plan::Merge | Plan::Many => {}
            }
            plan
        } else {
            let (order, walks, candidates) = self.walked();
            (self.order, self.walks) = (order, walks);
            for &probed in &order[walks..self.sides()] {
                let probed = usize::from(probed);
                side!(self, probed, |side| side.follow(candidates));
                for &walked in &order[..walks] {
                    self.lend(usize::from(walked), probed);
                }
            }
            Plan::Many
        };
        self.plan = Some(plan);
        plan
    }

    fn pair(&mut self, a: usize, b: usize, end: End, leads: bool) -> bool {
        let All {
            left, right, more, ..
        } = self;
        let mut rest = std::iter::once(&mut **right)
            .chain(more.iter_mut().map(|side| &mut **side))
            .enumerate()
            .filter(|(at, _)| *at + 1 == a || *at + 1 == b)
            .map(|(_, side)| side);
        match (a, b) {
            (0, _) => joined(&mut **left, rest.next().expect("a side"), end, leads),
            (_, 0) => joined(rest.next().expect("a side"), &mut **left, end, leads),
            _ => {
                let (first, second) = (rest.next().expect("a side"), rest.next().expect("a side"));
                match a < b {
                    true => joined(first, second, end, leads),
                    false => joined(second, first, end, leads),
                }
            }
        }
    }

    fn agree(&mut self, end: End) -> Option<K> {
        let lead = usize::from(self.order[0]);
        if self.walks == 2 {
            if !self.pair(lead, usize::from(self.order[1]), end, false) {
                return None;
            }
            return side!(self, lead, |side| side.peek(end)).cloned();
        }
        let mut key = side!(self, lead, |side| side.fill(end))?.clone();
        let (mut held, mut at) = (1, 0);
        while held < self.walks {
            at += 1;
            if at == self.walks {
                at = 0;
            }
            let walked = usize::from(self.order[at]);
            let found = side!(self, walked, |side| side.seek(end, &key))?;
            if *found == key {
                held += 1;
            } else {
                key = found.clone();
                held = 1;
            }
        }
        Some(key)
    }

    #[inline(never)]
    fn many(&mut self, end: End) -> bool {
        let lead = usize::from(self.order[0]);
        let checked = self.walks.max(2);
        loop {
            let key = if self.walks == 1 {
                if !self.pair(lead, usize::from(self.order[1]), end, true) {
                    return false;
                }
                side!(self, lead, |side| side.peek(end)).cloned()
            } else {
                self.agree(end)
            };
            let Some(key) = key else {
                return false;
            };
            let mut answer = None;
            for at in checked..self.sides() {
                let probed = usize::from(self.order[at]);
                answer = side!(self, probed, |side| match side.probe(end, &key) {
                    Probe::At(found) if *found == key => None,
                    other => Some(met(other)),
                });
                if answer.is_some() {
                    break;
                }
            }
            let moved = match answer {
                None => return true,
                Some(Met::At(found)) => side!(self, lead, |side| side.seek(end, &found)).is_some(),
                Some(Met::Past(found)) => {
                    side!(self, lead, |side| side.pass(end, &found)).is_some()
                }
                Some(Met::Gone) => false,
            };
            if !moved {
                return false;
            }
        }
    }

    fn settle(&mut self, end: End) -> bool {
        let settled = match self.plan() {
            Plan::Merge => merged(&mut *self.left, &mut *self.right, end),
            Plan::Left => led(&mut *self.left, &mut *self.right, end),
            Plan::Right => led(&mut *self.right, &mut *self.left, end),
            Plan::Many => self.many(end),
        };
        self.settled[end.slot()] = settled;
        settled
    }

    fn lead(&self) -> usize {
        match self.plan {
            Some(Plan::Right) => 1,
            Some(Plan::Many) => usize::from(self.order[0]),
            _ => 0,
        }
    }

    fn reads_left(&self) -> bool {
        match self.plan {
            Some(Plan::Right) => false,
            Some(Plan::Many) => self.order[..self.walks].contains(&0),
            _ => true,
        }
    }

    #[inline(never)]
    fn take_many(&mut self, end: End) -> Option<(K, Option<C>)> {
        let mut taken = None;
        for walked in 0..self.walks {
            let at = usize::from(self.order[walked]);
            let (key, covered) = match at {
                0 => self.left.take(end)?,
                at => (self.right_at(at).take(end)?.0, None),
            };
            if at == 0 || taken.is_none() {
                taken = Some((key, covered));
            }
        }
        let (key, covered) = taken?;
        let covered = match covered {
            None if self.covering && !self.reads_left() => self.left.found(),
            covered => covered,
        };
        Some((key, covered))
    }

    #[inline(never)]
    fn probe_many(&mut self, end: End, target: &K) -> Option<Met<K>> {
        self.ordered();
        for at in 0..self.sides() {
            let at = usize::from(self.order[at]);
            let answer = side!(self, at, |side| match side.probe(end, target) {
                Probe::At(found) if found == target => None,
                other => Some(met(other)),
            });
            if answer.is_some() {
                return answer;
            }
        }
        None
    }
}

impl<K: Data + Ord, C: Send + 'static> Stream<K, C> for All<K, C> {
    fn len(&self) -> usize {
        self.more
            .iter()
            .fold(self.left.len().min(self.right.len()), |len, side| {
                len.min(side.len())
            })
    }

    fn cover(&mut self) {
        self.covering = true;
        self.left.cover();
    }

    fn peek(&self, end: End) -> Option<&K> {
        if !self.settled[end.slot()] {
            return None;
        }
        seen!(self, self.lead(), |side| side.peek(end))
    }

    fn fill(&mut self, end: End) -> Option<&K> {
        if !self.settle(end) {
            return None;
        }
        self.peek(end)
    }

    fn seek(&mut self, end: End, target: &K) -> Option<&K> {
        self.plan();
        side!(self, self.lead(), |side| side.seek(end, target))?;
        self.fill(end)
    }

    fn pass(&mut self, end: End, target: &K) -> Option<&K> {
        self.plan();
        side!(self, self.lead(), |side| side.pass(end, target))?;
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
            Plan::Many => self.take_many(end),
        }
    }

    fn probe(&mut self, end: End, target: &K) -> Probe<'_, K> {
        match self.probe_many(end, target) {
            None => Probe::At(self.landed.insert(target.clone())),
            Some(Met::At(key)) => Probe::At(self.landed.insert(key)),
            Some(Met::Past(key)) => Probe::Past(self.landed.insert(key)),
            Some(Met::Gone) => Probe::Gone,
        }
    }

    fn found(&mut self) -> Option<C> {
        self.left.found()
    }

    fn source(&self, end: End) -> usize {
        if self.reads_left() {
            self.left.source(end)
        } else {
            self.left.found_source()
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
        for side in &mut self.more {
            side.follow(lead);
        }
    }

    fn adopt(&mut self, rows: &mut dyn std::any::Any) {
        self.left.adopt(rows);
        if let Some(spare) = rows.downcast_mut::<Spare>() {
            for at in 1..self.sides() {
                if spare.0.is_none() {
                    break;
                }
                if self.shares(at) {
                    side!(self, at, |side| side.adopt(spare));
                }
            }
        }
    }

    fn rowless(&mut self) {
        self.left.rowless();
        self.right.rowless();
        for side in &mut self.more {
            side.rowless();
        }
    }

    fn spare(&mut self) -> Option<Box<dyn std::any::Any + Send>> {
        self.plan();
        let (order, walks) = self.walking();
        for &at in &order[..walks] {
            let at = usize::from(at);
            if self.shares(at)
                && let Some(rows) = side!(self, at, |side| side.spare())
            {
                return Some(rows);
            }
        }
        None
    }

    fn spares(&self) -> bool {
        let (order, walks) = self.walking();
        let given = order[..walks]
            .iter()
            .map(|at| usize::from(*at))
            .filter(|at| self.shares(*at) && seen!(self, *at, |side| side.spares()))
            .count();
        let taken = match self.plan {
            Some(_) => 0,
            None => order[walks..self.sides()]
                .iter()
                .map(|at| usize::from(*at))
                .filter(|at| self.shares(*at) && seen!(self, *at, |side| side.wants()))
                .count(),
        };
        given > taken
    }

    fn wants(&self) -> bool {
        (0..self.sides())
            .any(|at| (at == 0 || self.shares(at)) && seen!(self, at, |side| side.wants()))
    }

    fn ranges(&self) -> bool {
        (0..self.sides()).any(|at| seen!(self, at, |side| side.ranges()))
    }

    fn join(
        &mut self,
        right: Box<dyn Stream<K, ()>>,
        same: bool,
        total: usize,
    ) -> Option<Box<dyn Stream<K, ()>>> {
        let sides = self.sides();
        if self.plan.is_some() || self.ordered || sides == SIDES {
            return Some(right);
        }
        self.shared |= u64::from(same) << sides;
        self.more.push(right);
        self.total = total;
        None
    }

    fn split(&mut self) -> Option<Vec<Side<K>>> {
        if self.plan.is_some() || self.ordered {
            return None;
        }
        let left: Box<dyn Stream<K, C>> = Box::new(Listed::ready(VecDeque::new()));
        let right: Box<dyn Stream<K, ()>> = Box::new(Listed::ready(VecDeque::new()));
        let left = std::mem::replace(&mut self.left, left);
        let right = std::mem::replace(&mut self.right, right);
        let mut sides = Vec::with_capacity(self.sides());
        sides.push((
            Box::new(Blind(left)) as Box<dyn Stream<K, ()>>,
            self.shares(0),
        ));
        sides.push((right, self.shares(1)));
        let more = std::mem::take(&mut self.more);
        for (at, side) in more.into_iter().enumerate() {
            sides.push((side, self.shares(at + 2)));
        }
        Some(sides)
    }

    fn explain(&self, role: Role, offered: bool) -> Explain {
        let sides = self.sides();
        let mut roles = [role; SIDES];
        let mut lent = [false; SIDES];
        match role {
            Role::Probed(_) => {
                let mut offered = offered;
                for (at, lent) in lent.iter_mut().enumerate().take(sides) {
                    *lent = offered
                        && (at == 0 || self.shares(at))
                        && seen!(self, at, |side| side.wants());
                    offered &= !*lent;
                }
            }
            Role::Walked => {
                let (order, walks) = self.walking();
                let lead = match self.plan {
                    Some(Plan::Many) | None if self.sides() > 2 => self.walked().2,
                    _ => self.len_at(usize::from(order[0])),
                };
                let mut spares = [false; SIDES];
                for &walked in &order[..walks] {
                    let walked = usize::from(walked);
                    spares[walked] = self.plan.is_none()
                        && self.shares(walked)
                        && seen!(self, walked, |side| side.spares());
                }
                for &probed in &order[walks..sides] {
                    let probed = usize::from(probed);
                    roles[probed] = Role::Probed(lead);
                    if self.shares(probed)
                        && seen!(self, probed, |side| side.wants())
                        && let Some(giver) = spares.iter().position(|spares| *spares)
                    {
                        spares[giver] = false;
                        lent[probed] = true;
                    }
                }
            }
        }
        Explain {
            step: Step::And,
            read: role.read(),
            keys: self.len(),
            sides: (0..sides)
                .map(|at| seen!(self, at, |side| side.explain(roles[at], lent[at])))
                .collect(),
        }
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

    fn spare(&mut self) -> Option<Box<dyn std::any::Any + Send>> {
        self.sides.iter_mut().find_map(|side| side.spare())
    }

    fn spares(&self) -> bool {
        self.sides.iter().any(|side| side.spares())
    }

    fn wants(&self) -> bool {
        self.sides.iter().any(|side| side.wants())
    }

    fn ranges(&self) -> bool {
        self.sides.iter().any(|side| side.ranges())
    }

    fn explain(&self, role: Role, offered: bool) -> Explain {
        let mut offered = offered;
        let sides = self
            .sides
            .iter()
            .map(|side| {
                let taken = offered && side.wants();
                offered &= !taken;
                side.explain(role, taken)
            })
            .collect();
        Explain {
            step: Step::Or,
            read: role.read(),
            keys: self.len(),
            sides,
        }
    }
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
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::codec::{HashSink, Object, Paired, Pairing, field_tag};
    use crate::store::Store;
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
        checked_from(db, Some(old), new)
    }

    fn checked_from(db: &Db<Shelf>, old: Option<u64>, new: u64) -> (Result<()>, u64, u32) {
        let allowance = 1 << 40;
        let old = old.map(|old| found(&db.at(old).unwrap().rows));
        let new = db.at(new).unwrap();
        let view = new
            .rows
            .rows()
            .store()
            .unwrap()
            .view(0, allowance, u64::MAX);
        let agreed =
            pool::scope(|pool| agree::<Tagged>(&found(&new.rows), old.as_ref(), &view, pool, true));
        let (left, records) = view.reads_left().unwrap();
        (agreed, allowance - left, records)
    }

    #[test]
    fn a_table_that_shares_no_node_with_its_base_is_built_rather_than_diffed() {
        let dir = tempfile::tempdir().unwrap();
        let db: Db<Shelf> = Options::new()
            .durability(Durability::Buffered)
            .open(dir.path().join("shelf.ferro"))
            .unwrap();
        let fill = |shelf: &mut Shelf, first: u64| {
            shelf.rows = Table::new();
            for id in first..first + 20_000 {
                shelf.rows.insert(Tagged {
                    id,
                    tags: Set::new(),
                    group: (id % 5) as u8,
                    note: 0,
                });
            }
        };
        db.update(|shelf| fill(shelf, 0)).unwrap();
        db.update(|shelf| fill(shelf, 1_000_000)).unwrap();
        db.update(|shelf| {
            shelf.rows.update(&1_000_007, |row| row.group = 9);
        })
        .unwrap();

        let (built, _, whole) = checked_from(&db, None, 2);
        built.unwrap();
        let (agreed, _, records) = checked(&db, 1, 2);
        agreed.unwrap();
        assert!(
            records < whole + whole / 8,
            "a table checked against one it shares no row with read {records} records, built \
             from its rows {whole}"
        );
        let (agreed, _, records) = checked(&db, 2, 3);
        agreed.unwrap();
        assert!(records < 64, "an edit of one row read {records} records");
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

    #[derive(crate::Data, Clone, Default)]
    struct Wide {
        t: Vec<[Table<Tagged>; 0]>,
    }

    #[derive(crate::Data, Clone, Default)]
    struct Wides {
        v: Vec<Wide>,
    }

    #[derive(crate::Data, Clone, Default)]
    struct Keyed {
        by: BTreeMap<u64, Table<Tagged>>,
    }

    #[derive(crate::Data, Clone, Default)]
    struct Racks {
        racks: Vec<Table<Tagged>>,
    }

    fn scratch() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = Store::open(
            &dir.path().join("pairing.ferro"),
            crate::store::StoreOptions::default(),
        )
        .unwrap();
        (dir, store)
    }

    fn object_of<D: Data>(value: &D) -> Object {
        let mut sink = HashSink;
        let mut encoder = Encoder::new(&mut sink);
        value.encode(&mut encoder).unwrap();
        encoder.into_object()
    }

    fn paired<D: Data>(store: &Store, old: &Object, new: &Object) -> Result<Paired> {
        let mut pairing = Pairing::new(store, true);
        D::pair(
            &mut Decoder::with_store(&old.body, &old.links, Some(store)),
            &mut Decoder::with_store(&new.body, &new.links, Some(store)),
            &mut pairing,
        )?;
        pairing.settle()?;
        Ok(pairing.into_paired())
    }

    fn wides(each: u64) -> Object {
        let mut sink = HashSink;
        let mut w = Encoder::new(&mut sink);
        w.varint(1);
        w.field(field_tag("v"), |w| {
            w.varint(2);
            for _ in 0..2 {
                w.varint(1);
                w.field(field_tag("t"), |w| {
                    w.varint(each);
                    Ok(())
                })?;
            }
            Ok(())
        })
        .unwrap();
        w.into_object()
    }

    #[test]
    fn the_fields_of_one_object_share_its_zero_width_items_when_paired() {
        let (_dir, store) = scratch();
        let wide = |result: Result<()>| matches!(result, Err(Error::Decode(message)) if message.contains("zero-width"));
        let (none, half, over) = (wides(0), wides(1 << 23), wides(1 << 24));
        assert!(wide(Wides::visit(&mut Decoder::new(
            &over.body,
            &over.links
        ))));
        assert!(wide(paired::<Wides>(&store, &none, &over).map(drop)));
        assert!(wide(paired::<Wides>(&store, &over, &none).map(drop)));
        Wides::visit(&mut Decoder::new(&half.body, &half.links)).unwrap();
        paired::<Wides>(&store, &none, &half).unwrap();
    }

    fn junk(count: u64, tag: u16) -> Object {
        let mut sink = HashSink;
        let mut w = Encoder::new(&mut sink);
        w.varint(1 + count);
        w.field(field_tag("rows"), |w| Table::<Tagged>::new().encode(w))
            .unwrap();
        for _ in 0..count {
            w.field(tag, |_| Ok(())).unwrap();
        }
        w.into_object()
    }

    #[test]
    fn a_pairing_matches_the_fields_of_two_versions_in_one_pass() {
        let (_dir, store) = scratch();
        let rows = field_tag("rows");
        let (old, new) = (junk(200_000, rows ^ 1), junk(200_000, rows ^ 2));
        let started = Instant::now();
        let many = paired::<Shelf>(&store, &old, &new).unwrap();
        let took = started.elapsed();
        assert_eq!(many.same.len(), 1);
        assert!(
            took < Duration::from_secs(5),
            "pairing two roots of 200,000 unknown fields took {took:?}"
        );
        let mixed = paired::<Shelf>(&store, &junk(3, rows ^ 1), &junk(2, rows ^ 1)).unwrap();
        assert_eq!(mixed.same.len(), 1);
    }

    #[test]
    fn a_sequence_that_gained_or_lost_items_pairs_the_rest_with_the_items_they_came_from() {
        let (_dir, store) = scratch();
        let table = |first: u64, note: u64| {
            let mut table: Table<Tagged> = Table::new();
            for id in first..first + 3 {
                table.insert(Tagged {
                    id,
                    tags: Set::new(),
                    group: id as u8,
                    note,
                });
            }
            table
        };
        let (a, b, c) = (table(0, 0), table(10, 0), table(20, 0));
        let (b2, c2) = (table(10, 1), table(20, 1));
        let racks = |racks: &[&Table<Tagged>]| {
            object_of(&Racks {
                racks: racks.iter().map(|&table| table.clone()).collect(),
            })
        };
        let pairs = |old: &[&Table<Tagged>], new: &[&Table<Tagged>]| {
            let paired = paired::<Racks>(&store, &racks(old), &racks(new)).unwrap();
            let pairs: Vec<_> = paired
                .pairs
                .iter()
                .map(|(old, new)| (old.hash, new.hash))
                .collect();
            let same: Vec<_> = paired.same.iter().map(|(_, hash)| *hash).collect();
            (pairs, same)
        };
        let hash = |table: &Table<Tagged>| found(table).hash;

        assert_eq!(
            pairs(&[&a, &b], &[&b2]),
            (vec![(hash(&b), hash(&b2))], vec![])
        );
        assert_eq!(
            pairs(&[&a, &b, &c], &[&a, &c2]),
            (vec![(hash(&c), hash(&c2))], vec![hash(&a)])
        );
        assert_eq!(
            pairs(&[&b, &c], &[&a, &b2, &c]),
            (vec![(hash(&b), hash(&b2))], vec![hash(&c)])
        );
        assert_eq!(
            pairs(&[&a, &b], &[&c2]),
            (vec![(hash(&a), hash(&c2))], vec![])
        );
        assert_eq!(
            pairs(&[&a, &b], &[&a, &b2]),
            (vec![(hash(&b), hash(&b2))], vec![hash(&a)])
        );
    }

    #[test]
    fn a_btreemap_pairs_the_table_under_each_key_both_versions_hold() {
        let (_dir, store) = scratch();
        let table = |rows: u64| {
            let mut table: Table<Tagged> = Table::new();
            for id in 0..rows {
                table.insert(Tagged {
                    id,
                    tags: Set::new(),
                    group: 0,
                    note: rows,
                });
            }
            table
        };
        let old = Keyed {
            by: BTreeMap::from([(1, table(1)), (2, table(2)), (3, table(3)), (5, table(5))]),
        };
        let new = Keyed {
            by: BTreeMap::from([(2, table(4)), (3, table(3)), (4, table(6)), (6, table(7))]),
        };
        let paired = paired::<Keyed>(&store, &object_of(&old), &object_of(&new)).unwrap();
        let hashes: Vec<_> = paired
            .pairs
            .iter()
            .map(|(old, new)| (old.hash, new.hash))
            .collect();
        assert_eq!(hashes, [(found(&table(2)).hash, found(&table(4)).hash)]);
        let same: Vec<_> = paired.same.iter().map(|(_, hash)| *hash).collect();
        assert_eq!(same, [found(&table(3)).hash]);
    }
}
