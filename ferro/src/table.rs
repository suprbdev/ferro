use std::borrow::Borrow;
use std::fmt;
use std::ops::{Bound, Deref, DerefMut, RangeBounds};
use std::sync::Arc;

use crate::codec::{Data, Decoder, Encoder};
use crate::error::Result;
use crate::map::{Conflict, Diff, Iter, Map, Values, three_way};

pub trait Record: Data {
    type Key: Data + Ord;
    type Table: TableData<Self>;

    fn key(&self) -> Self::Key;
}

pub trait TableData<T: Record>: Data + Default {
    fn rows(&self) -> &Map<T::Key, T>;
    fn rows_mut(&mut self) -> &mut Map<T::Key, T>;
    fn reindex(&mut self, old: Option<&T>, new: Option<&T>);
}

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
    pub fn new() -> Self {
        Table::default()
    }

    pub fn len(&self) -> usize {
        self.0.rows().len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.rows().is_empty()
    }

    pub fn get<Q>(&self, key: &Q) -> Option<Arc<T>>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.rows().get(key)
    }

    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.rows().contains_key(key)
    }

    pub fn iter(&self) -> Values<T::Key, T> {
        self.0.rows().values()
    }

    pub fn range<Q, R>(&self, range: R) -> Iter<T::Key, T>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        self.0.rows().range::<Q, R>(range)
    }

    pub fn first(&self) -> Option<Arc<T>> {
        self.0.rows().first().map(|(_, row)| row)
    }

    pub fn last(&self) -> Option<Arc<T>> {
        self.0.rows().last().map(|(_, row)| row)
    }

    pub fn insert(&mut self, row: T) -> Option<Arc<T>> {
        let key = row.key();
        let old = self.0.rows().get(&key);
        self.0.reindex(old.as_deref(), Some(&row));
        self.0.rows_mut().insert(key, row);
        old
    }

    pub fn remove<Q>(&mut self, key: &Q) -> Option<Arc<T>>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let old = self.0.rows_mut().remove(key)?;
        self.0.reindex(Some(&old), None);
        Some(old)
    }

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

    pub fn update<Q>(&mut self, key: &Q, f: impl FnOnce(&mut T)) -> bool
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        match self.get_mut(key) {
            Some(mut row) => {
                f(&mut row);
                true
            }
            None => false,
        }
    }

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

    pub fn diff(&self, newer: &Table<T>) -> Diff<T::Key, T>
    where
        T: PartialEq,
    {
        self.0.rows().diff(newer.0.rows())
    }

    pub fn merge(
        &mut self,
        base: &Table<T>,
        theirs: &Table<T>,
        resolve: impl FnMut(Conflict<T::Key, T>) -> Option<T>,
    ) -> usize
    where
        T: PartialEq,
    {
        three_way(
            self,
            base.0.rows(),
            theirs.0.rows(),
            |table, key| table.get(key),
            |table, key, value| match value {
                Some(row) => {
                    table.insert(row);
                }
                None => {
                    table.remove(&key);
                }
            },
            resolve,
        )
    }
}

pub struct RowMut<'a, T: Record> {
    table: &'a mut Table<T>,
    old: Arc<T>,
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
        let old_key = self.old.key();
        let new_key = row.key();
        if old_key == new_key {
            self.table.0.reindex(Some(&self.old), Some(&row));
            self.table.0.rows_mut().insert(new_key, row);
        } else {
            self.table.0.rows_mut().remove(&old_key);
            self.table.0.reindex(Some(&self.old), None);
            self.table.insert(row);
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

    fn missing() -> Option<Self> {
        Some(Table::default())
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

struct Entries<I, K>(Iter<(I, K), ()>);

impl<I: Data + Ord, K: Data + Ord> KeyCursor<K> for Entries<I, K> {
    fn forward(&mut self) -> Option<K> {
        self.0.next().map(|((_, key), _)| key)
    }

    fn backward(&mut self) -> Option<K> {
        self.0.next_back().map(|((_, key), _)| key)
    }

    fn left(&self) -> usize {
        self.0.len()
    }
}

pub struct IndexIter<T: Record> {
    rows: Map<T::Key, T>,
    keys: Box<dyn KeyCursor<T::Key>>,
}

impl<T: Record> IndexIter<T> {
    fn over<I: Data + Ord>(rows: &Map<T::Key, T>, entries: Iter<(I, T::Key), ()>) -> Self {
        IndexIter {
            rows: rows.clone(),
            keys: Box::new(Entries(entries)),
        }
    }
}

impl<T: Record> Iterator for IndexIter<T> {
    type Item = Arc<T>;

    fn next(&mut self) -> Option<Arc<T>> {
        let key = self.keys.forward()?;
        self.rows.get(&key)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.keys.left(), Some(self.keys.left()))
    }
}

impl<T: Record> DoubleEndedIterator for IndexIter<T> {
    fn next_back(&mut self) -> Option<Arc<T>> {
        let key = self.keys.backward()?;
        self.rows.get(&key)
    }
}

impl<T: Record> ExactSizeIterator for IndexIter<T> {}

pub fn index_eq<I, T, Q>(
    rows: &Map<T::Key, T>,
    index: &Map<(I, T::Key), ()>,
    value: &Q,
) -> IndexIter<T>
where
    I: Data + Ord + Borrow<Q>,
    T: Record,
    Q: Ord + ?Sized,
{
    IndexIter::over(
        rows,
        index.range_by(
            |entry| entry.0.borrow() < value,
            |entry| entry.0.borrow() <= value,
        ),
    )
}

pub fn index_eq_some<X, T, Q>(
    rows: &Map<T::Key, T>,
    index: &Map<(Option<X>, T::Key), ()>,
    value: &Q,
) -> IndexIter<T>
where
    X: Data + Ord + Borrow<Q>,
    T: Record,
    Q: Ord + ?Sized,
{
    IndexIter::over(
        rows,
        index.range_by(
            |entry| entry.0.as_ref().is_none_or(|x| x.borrow() < value),
            |entry| entry.0.as_ref().is_none_or(|x| x.borrow() <= value),
        ),
    )
}

pub fn index_range<I, T, Q, R>(
    rows: &Map<T::Key, T>,
    index: &Map<(I, T::Key), ()>,
    range: R,
) -> IndexIter<T>
where
    I: Data + Ord + Borrow<Q>,
    T: Record,
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
    )
}

pub fn index_counts<I: Data + Ord, K: Data + Ord>(index: &Map<(I, K), ()>) -> Vec<(I, usize)> {
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

pub trait IndexEach {
    type Value: Data + Ord;

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
