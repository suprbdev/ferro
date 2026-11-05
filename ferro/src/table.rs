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
//! knows its length at once, and reads each row from the rows map as it yields it.
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
//! primary key and read a row only for a key that matches, as described on [`Matches`].
//!
//! # Unique indexes
//!
//! A unique index holds each value for one row at most. [`try_insert`](Table::try_insert) and
//! [`try_update`](Table::try_update) return [`Taken`] on a clash and leave the table as it was.
//! [`insert`](Table::insert), [`update`](Table::update) and [`get_mut`](Table::get_mut) have no
//! error channel, so they panic instead, which inside a commit discards the transaction.
//! [`merge`](Table::merge) passes a clash to its `resolve` closure and never panics on one.
//!
//! # Panics
//!
//! As with every collection, a read that cannot load a node from the file, or finds one damaged,
//! panics. [`Map::try_get`] on [`Table::rows`] returns that failure as an error instead.
//!
//! [The tables guide](https://github.com/archlemon/ferro/blob/main/docs/guide/tables.md) describes
//! tables and indexes at more length, and
//! [the schema and unique index internals](https://github.com/archlemon/ferro/blob/main/docs/internals/schema-and-unique-indexes.md)
//! how they are stored.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::convert::Infallible;
use std::fmt;
use std::ops::{Bound, Deref, DerefMut, RangeBounds};
use std::sync::Arc;

use crate::codec::{Data, Decoder, Description, Encoder, Hash};
use crate::error::Result;
use crate::map::{Conflict, Cursor, Diff, Finder, Iter, Keys, Map, Val, Values, same_value};
use crate::prefix::Prefix;

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
    /// [`try_insert`](Table::try_insert) to get the clash as a [`Taken`] instead.
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
    /// Changing the key field moves the row to the new key, replacing any row already there.
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
    /// Panics, leaving the table as it was, if the changed row clashes in a unique index. Inside
    /// a commit that discards the transaction. Use [`try_update`](Table::try_update) to get the
    /// clash as a [`Taken`] instead.
    pub fn update<Q, R>(&mut self, key: &Q, f: impl FnOnce(&mut T) -> R) -> Option<R>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut row = self.get_mut(key)?;
        Some(f(&mut row))
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
/// # Panics
///
/// Dropping it panics, and leaves the table as it was, if the changed row clashes in a unique
/// index. If its thread is already panicking it keeps the old row instead, since a second panic
/// would abort the process.
pub struct RowMut<'a, T: Record> {
    table: &'a mut Table<T>,
    old: Val<T>,
    row: Option<T>,
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
        match self.table.0.conflict(&row, Some(&self.old.key())) {
            None => self.table.replace(&self.old, row),
            Some(_) if std::thread::panicking() => {}
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

impl<K, C> EntryCursor<K, ()> for Uncovered<K, C> {
    fn forward_covered(&mut self) -> Option<(K, ())> {
        self.0.forward().map(|key| (key, ()))
    }

    fn backward_covered(&mut self) -> Option<(K, ())> {
        self.0.backward().map(|key| (key, ()))
    }
}

/// The rows of one index lookup, in index order: by indexed value, then by primary key.
///
/// Every lookup the [`Record`](derive@crate::Record) derive generates on a non-unique index, such
/// as `by_x`, `by_x_after`, `by_x_range`, `by_x_some` or `by_x_prefix`, returns one. The rows of
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
    #[inline]
    pub fn uncovered(self) -> IndexIter<T> {
        IndexIter {
            rows: self.rows,
            entries: Box::new(Uncovered(self.entries)),
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
/// key order, so the first time the combination is read its keys and covered fields are read
/// in full and sorted. A lookup of one value is in key order already and sorts in one pass.
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
        let mut entries = self.entries;
        let left = entries.left();
        let collect: Collect<T::Key, C> = Box::new(move || {
            let mut items = Vec::with_capacity(left);
            while let Some((key, covered)) = entries.forward_covered() {
                items.push((key, Some(covered)));
            }
            items
        });
        Matches {
            rows: self.rows,
            stream: Box::new(Listed::pending(left, collect)),
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
/// [`IndexIter`], by primary key. A combination reads no row until a key matches, and yields each
/// matching row once, from either end.
///
/// An index entry is `(value, key)`, so the keys of one value come in key order and two lookups
/// of similar size are walked side by side. When one side of an `and` has at least four times the
/// keys of the other, only the smaller is walked and each of its keys is looked up in the larger,
/// so a few keys against many cost a few searches. The choice is made when the combination is
/// first read. A combination reads the indexes as they were when its lookups were built.
///
/// [`keys`](Matches::keys) gives the keys alone and [`covered`](Matches::covered) the covered
/// fields of the leftmost lookup, so a combination whose first index covers what you show reads
/// no rows at all.
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
    stream: Box<dyn Stream<T::Key, C>>,
    started: bool,
}

impl<T: Record, C: Send + 'static> Matches<T, C> {
    /// The rows that both this combination and `other` match.
    ///
    /// `other` can be any [`IntoMatches`], so a range or prefix lookup on the table joins too.
    /// The result keeps this side's covered fields. If this combination has already yielded rows,
    /// what it has left is read into a sorted list first.
    pub fn and<M: IntoMatches<T>>(self, other: M) -> Matches<T, C> {
        let left = self.unstarted();
        let right = other.into_matches().unstarted();
        Matches {
            rows: left.rows,
            stream: Box::new(All::new(left.stream, Box::new(Blind(right.stream)))),
            started: false,
        }
    }

    /// The rows that this combination or `other` matches, each once.
    ///
    /// Both sides must have the same covered fields. Call [`uncovered`](Matches::uncovered), or
    /// [`IndexIter::uncovered`], on each to combine lookups of differently covered indexes. A row
    /// both sides hold takes its covered fields from this side. If this combination has already
    /// yielded rows, what it has left is read into a sorted list first.
    pub fn or<M: IntoMatches<T, Covered = C>>(self, other: M) -> Matches<T, C> {
        let left = self.unstarted();
        let right = other.into_matches().unstarted();
        Matches {
            rows: left.rows,
            stream: Box::new(Any {
                sides: vec![left.stream, right.stream],
                landed: None,
            }),
            started: false,
        }
    }

    /// The primary keys of the matching rows, in key order, without reading any row.
    pub fn keys(self) -> MatchKeys<T> {
        MatchKeys(Box::new(Blind(self.stream)))
    }

    /// `(key, fields)` pairs of the matching rows in key order, where `fields` are the covered
    /// fields of the leftmost lookup that holds the row, read from its index without reading any
    /// row.
    pub fn covered(mut self) -> MatchCovered<T, C> {
        self.stream.cover();
        MatchCovered(self.stream)
    }

    /// The same combination with the covered fields dropped from its type.
    pub fn uncovered(self) -> Matches<T> {
        Matches {
            rows: self.rows,
            stream: Box::new(Blind(self.stream)),
            started: self.started,
        }
    }

    fn unstarted(mut self) -> Self {
        if !self.started {
            return self;
        }
        self.stream.cover();
        let mut items = VecDeque::new();
        while let Some(item) = self.stream.take(End::Front) {
            items.push_back(item);
        }
        Matches {
            rows: self.rows,
            stream: Box::new(Listed::ready(items)),
            started: false,
        }
    }
}

impl<T: Record, C> Iterator for Matches<T, C> {
    type Item = Val<T>;

    fn next(&mut self) -> Option<Val<T>> {
        self.started = true;
        let (key, _) = self.stream.take(End::Front)?;
        self.rows.get(&key)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.stream.len()))
    }
}

impl<T: Record, C> DoubleEndedIterator for Matches<T, C> {
    fn next_back(&mut self) -> Option<Val<T>> {
        self.started = true;
        let (key, _) = self.stream.take(End::Back)?;
        self.rows.get(&key)
    }
}

/// The primary keys of [`Matches`], in key order, from [`Matches::keys`]. Reads no rows.
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
/// no rows.
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
}

trait Stream<K, C>: Send {
    fn len(&self) -> usize;
    fn cover(&mut self);
    fn peek(&self, end: End) -> Option<&K>;
    fn fill(&mut self, end: End) -> Option<&K>;
    fn seek(&mut self, end: End, target: &K) -> Option<&K>;
    fn take(&mut self, end: End) -> Option<(K, Option<C>)>;
    fn probe(&mut self, end: End, target: &K) -> Option<&K>;
    fn found(&mut self) -> Option<C>;
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
        self.start();
        let slot = end.slot();
        if let Some((key, _)) = &self.held[slot]
            && !end.before(key, target)
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
            if !end.before(&next.0, target) {
                self.held[slot] = Some(next);
                return self.peek(end);
            }
        }
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

    fn probe(&mut self, end: End, target: &K) -> Option<&K> {
        self.start();
        let (Some(first), Some(last), Some(value)) = (&self.first, &self.last, &self.value) else {
            return None;
        };
        let target = match end {
            End::Front if target > last => return None,
            End::Back if target < first => return None,
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
        let entry = cursor.key()?;
        let inside = entry.0 == *value
            && match end {
                End::Front => entry.1 <= *last,
                End::Back => entry.1 >= *first,
            };
        inside.then_some(&entry.1)
    }

    fn found(&mut self) -> Option<C> {
        let project = self.project;
        self.cursor
            .as_ref()?
            .read(|entry, stored| project(&entry.0, stored.clone()))
    }
}

type Collect<K, C> = Box<dyn FnOnce() -> Vec<(K, Option<C>)> + Send>;

struct Listed<K, C> {
    pending: Option<(usize, Collect<K, C>)>,
    items: VecDeque<(K, Option<C>)>,
    landed: usize,
}

impl<K: Ord, C> Listed<K, C> {
    fn pending(left: usize, collect: Collect<K, C>) -> Self {
        Listed {
            pending: Some((left, collect)),
            items: VecDeque::new(),
            landed: 0,
        }
    }

    fn ready(items: VecDeque<(K, Option<C>)>) -> Self {
        Listed {
            pending: None,
            items,
            landed: 0,
        }
    }

    fn items(&mut self) -> &mut VecDeque<(K, Option<C>)> {
        if let Some((_, collect)) = self.pending.take() {
            let mut items = collect();
            items.sort_by(|a, b| a.0.cmp(&b.0));
            items.dedup_by(|a, b| a.0 == b.0);
            self.items = items.into();
        }
        &mut self.items
    }
}

impl<K: Ord + Send, C: Send> Stream<K, C> for Listed<K, C> {
    fn len(&self) -> usize {
        match &self.pending {
            Some((left, _)) => *left,
            None => self.items.len(),
        }
    }

    fn cover(&mut self) {}

    fn peek(&self, end: End) -> Option<&K> {
        match end {
            End::Front => self.items.front(),
            End::Back => self.items.back(),
        }
        .map(|(key, _)| key)
    }

    fn fill(&mut self, end: End) -> Option<&K> {
        self.items();
        self.peek(end)
    }

    fn seek(&mut self, end: End, target: &K) -> Option<&K> {
        let items = self.items();
        match end {
            End::Front => {
                let behind = items.partition_point(|(key, _)| key < target);
                items.drain(..behind);
            }
            End::Back => {
                let kept = items.partition_point(|(key, _)| key <= target);
                items.truncate(kept);
            }
        }
        self.peek(end)
    }

    fn take(&mut self, end: End) -> Option<(K, Option<C>)> {
        let items = self.items();
        match end {
            End::Front => items.pop_front(),
            End::Back => items.pop_back(),
        }
    }

    fn probe(&mut self, end: End, target: &K) -> Option<&K> {
        let items = self.items();
        let landed = match end {
            End::Front => items.partition_point(|(key, _)| key < target),
            End::Back => items
                .partition_point(|(key, _)| key <= target)
                .checked_sub(1)?,
        };
        self.landed = landed;
        self.items.get(landed).map(|(key, _)| key)
    }

    fn found(&mut self) -> Option<C> {
        self.items.get_mut(self.landed)?.1.take()
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

    fn take(&mut self, end: End) -> Option<(K, Option<()>)> {
        self.0.take(end).map(|(key, _)| (key, Some(())))
    }

    fn probe(&mut self, end: End, target: &K) -> Option<&K> {
        self.0.probe(end, target)
    }

    fn found(&mut self) -> Option<()> {
        Some(())
    }
}

#[derive(Clone, Copy)]
enum Plan {
    Merge,
    Left,
    Right,
}

const RATIO: usize = 4;

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
        let Some(found) = follow.probe(end, &key) else {
            return false;
        };
        if *found == key {
            return true;
        }
        let found = found.clone();
        let Some(next) = lead.seek(end, &found) else {
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
            Plan::Left
        } else {
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

    fn probe(&mut self, end: End, target: &K) -> Option<&K> {
        let mut key = target.clone();
        loop {
            let found = self.left.probe(end, &key)?.clone();
            let again = self.right.probe(end, &found)?;
            if *again == found {
                self.landed = Some(found);
                return self.landed.as_ref();
            }
            key = again.clone();
        }
    }

    fn found(&mut self) -> Option<C> {
        self.left.found()
    }
}

struct Any<K, C> {
    sides: Vec<Box<dyn Stream<K, C>>>,
    landed: Option<(usize, K)>,
}

impl<K: Data + Ord, C> Any<K, C> {
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

    fn take(&mut self, end: End) -> Option<(K, Option<C>)> {
        let key = self.fill(end)?.clone();
        let mut taken = None;
        for side in &mut self.sides {
            if side.peek(end) == Some(&key) {
                let item = side.take(end);
                if taken.is_none() {
                    taken = item;
                }
            }
        }
        taken
    }

    fn probe(&mut self, end: End, target: &K) -> Option<&K> {
        let mut best: Option<(usize, K)> = None;
        for (side, stream) in self.sides.iter_mut().enumerate() {
            if let Some(key) = stream.probe(end, target)
                && best.as_ref().is_none_or(|(_, held)| end.before(key, held))
            {
                best = Some((side, key.clone()));
            }
        }
        self.landed = best;
        self.landed.as_ref().map(|(_, key)| key)
    }

    fn found(&mut self) -> Option<C> {
        let side = self.landed.as_ref()?.0;
        self.sides[side].found()
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
/// open instead.
pub fn stored_rows<T: Record>(rows: &Map<T::Key, T>) -> Result<Vec<Val<T>>> {
    rows.try_values()
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

/// The rows of the index entries whose value lies in the span `below` and `within` mark out, as
/// [`Map::range_by`] takes them. Backs a composite index's generated `by_a_b_prefix`.
#[inline(always)]
pub fn index_where<I, T, S, C>(
    rows: &Map<T::Key, T>,
    index: &Map<(I, T::Key), S>,
    project: Project<I, S, C>,
    below: impl Fn(&I) -> bool,
    within: impl Fn(&I) -> bool,
) -> IndexIter<T, C>
where
    I: Data + Ord,
    T: Record,
    S: Data,
    C: 'static,
{
    IndexIter::over(
        rows,
        index.range_by(|entry| below(&entry.0), |entry| within(&entry.0)),
        project,
    )
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
#[inline(always)]
pub fn index_range<I, T, S, C, Q, R>(
    rows: &Map<T::Key, T>,
    index: &Map<(I, T::Key), S>,
    project: Project<I, S, C>,
    range: R,
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
    IndexIter::over(
        rows,
        index.range_by(
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
    )
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

    /// The elements to index.
    fn values(&self) -> Vec<Self::Value>;
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

    fn values(&self) -> Vec<X> {
        self.iter().collect()
    }
}

impl<X: Data + Ord> IndexEach for Option<X> {
    type Value = X;

    fn values(&self) -> Vec<X> {
        self.iter().cloned().collect()
    }
}
