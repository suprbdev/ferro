use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::fmt;
use std::ops::{Bound, Deref, DerefMut, RangeBounds};
use std::sync::Arc;

use crate::codec::{Data, Decoder, Encoder, Hash};
use crate::error::Result;
use crate::map::{Conflict, Diff, Finder, Iter, Map, Val, Values};

pub trait Record: Data {
    type Key: Data + Ord;
    type Table: TableData<Self>;

    fn key(&self) -> Self::Key;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Taken<K> {
    pub index: &'static str,
    pub holder: K,
}

impl<K: fmt::Debug> fmt::Display for Taken<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unique index {} already holds this value for {:?}", self.index, self.holder)
    }
}

impl<K: fmt::Debug> std::error::Error for Taken<K> {}

pub trait TableData<T: Record>: Data + Default {
    fn rows(&self) -> &Map<T::Key, T>;
    fn rows_mut(&mut self) -> &mut Map<T::Key, T>;
    fn reindex(&mut self, old: Option<&T>, new: Option<&T>);
    fn conflict(&self, _row: &T, _moving: Option<&T::Key>) -> Option<Taken<T::Key>> {
        None
    }
}

fn clash<K>(taken: Taken<K>) -> ! {
    panic!("ferro: unique index {} already holds this value", taken.index)
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

    pub fn rows(&self) -> &Map<T::Key, T> {
        self.0.rows()
    }

    pub fn root_hash(&self) -> Option<Hash> {
        self.0.rows().root_hash()
    }

    pub fn len(&self) -> usize {
        self.0.rows().len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.rows().is_empty()
    }

    pub fn get<Q>(&self, key: &Q) -> Option<Val<T>>
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

    pub fn for_each(&self, mut f: impl FnMut(&T)) {
        self.0.rows().for_each(|_, row| f(row));
    }

    pub fn for_each_range<Q, R>(&self, range: R, mut f: impl FnMut(&T))
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        self.0.rows().for_each_range::<Q, R>(range, |_, row| f(row));
    }

    pub fn range<Q, R>(&self, range: R) -> Iter<T::Key, T>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        self.0.rows().range::<Q, R>(range)
    }

    pub fn first(&self) -> Option<Val<T>> {
        self.0.rows().first().map(|(_, row)| row)
    }

    pub fn last(&self) -> Option<Val<T>> {
        self.0.rows().last().map(|(_, row)| row)
    }

    pub fn insert(&mut self, row: T) -> Option<Val<T>> {
        self.try_insert(row).unwrap_or_else(|taken| clash(taken))
    }

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

    pub fn remove<Q>(&mut self, key: &Q) -> Option<Val<T>>
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

    pub fn try_update<Q>(
        &mut self,
        key: &Q,
        f: impl FnOnce(&mut T),
    ) -> std::result::Result<bool, Taken<T::Key>>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let Some(old) = self.0.rows().get(key) else {
            return Ok(false);
        };
        let mut row = (*old).clone();
        f(&mut row);
        if let Some(taken) = self.0.conflict(&row, Some(&old.key())) {
            return Err(taken);
        }
        self.replace(&old, row);
        Ok(true)
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

    pub fn drain_range<Q, R>(&mut self, range: R) -> Vec<Val<T>>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        let drained: Vec<Val<T>> = self.0.rows_mut().take_range::<Q, R>(range).values().collect();
        for row in &drained {
            self.0.reindex(Some(row), None);
        }
        drained
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
        mut resolve: impl FnMut(Conflict<T::Key, T>) -> Option<T>,
    ) -> usize
    where
        T: PartialEq,
    {
        let Ok(written) = self.try_merge(base, theirs, |conflict| {
            Ok::<_, Infallible>(resolve(conflict))
        });
        written
    }

    pub fn try_merge<E>(
        &mut self,
        base: &Table<T>,
        theirs: &Table<T>,
        resolve: impl FnMut(Conflict<T::Key, T>) -> std::result::Result<Option<T>, E>,
    ) -> std::result::Result<usize, E>
    where
        T: PartialEq,
    {
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
    ) -> std::result::Result<usize, E>
    where
        T: PartialEq,
    {
        let mut held = Pending::default();
        let mut written = 0;
        for change in base.0.rows().diff(theirs.0.rows()) {
            let key = change.key().clone();
            let before = change.before().cloned();
            let after = change.after().cloned();
            let ours = self.get(&key);
            if ours == after {
                continue;
            }
            written += 1;
            let (row, asked) = if ours == before {
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
            let ours = self.0.conflict(&row, None).and_then(|taken| self.get(&taken.holder));
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

pub type Project<I, S, C> = fn(&I, S) -> C;

pub fn plain<I>(_value: &I, _stored: ()) {}

struct Entries<I, K, S, C> {
    iter: Iter<(I, K), S>,
    project: Project<I, S, C>,
}

impl<I: Data + Ord, K: Data + Ord, S: Data, C> KeyCursor<K> for Entries<I, K, S, C> {
    fn forward(&mut self) -> Option<K> {
        self.iter.next_key_with(|entry| entry.1.clone())
    }

    fn backward(&mut self) -> Option<K> {
        self.iter.next_back_key_with(|entry| entry.1.clone())
    }

    fn left(&self) -> usize {
        self.iter.len()
    }
}

impl<I: Data + Ord, K: Data + Ord, S: Data, C> EntryCursor<K, C> for Entries<I, K, S, C> {
    fn forward_covered(&mut self) -> Option<(K, C)> {
        let project = self.project;
        self.iter
            .next_with(|entry, stored| (entry.1.clone(), project(&entry.0, stored.clone())))
    }

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

pub struct IndexIter<T: Record, C = ()> {
    rows: Finder<T::Key, T>,
    entries: Box<dyn EntryCursor<T::Key, C>>,
}

impl<T: Record, C: 'static> IndexIter<T, C> {
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

    pub fn keys(self) -> IndexKeys<T> {
        IndexKeys(self.entries)
    }

    pub fn covered(self) -> IndexCovered<T, C> {
        IndexCovered(self.entries)
    }

    pub fn uncovered(self) -> IndexIter<T> {
        IndexIter {
            rows: self.rows,
            entries: Box::new(Uncovered(self.entries)),
        }
    }
}

impl<T: Record, C> Iterator for IndexIter<T, C> {
    type Item = Val<T>;

    fn next(&mut self) -> Option<Val<T>> {
        let key = self.entries.forward()?;
        self.rows.get(&key)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.entries.left(), Some(self.entries.left()))
    }
}

impl<T: Record, C> DoubleEndedIterator for IndexIter<T, C> {
    fn next_back(&mut self) -> Option<Val<T>> {
        let key = self.entries.backward()?;
        self.rows.get(&key)
    }
}

impl<T: Record, C> ExactSizeIterator for IndexIter<T, C> {}

pub struct IndexCovered<T: Record, C = ()>(Box<dyn EntryCursor<T::Key, C>>);

impl<T: Record, C> Iterator for IndexCovered<T, C> {
    type Item = (T::Key, C);

    fn next(&mut self) -> Option<(T::Key, C)> {
        self.0.forward_covered()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.0.left(), Some(self.0.left()))
    }
}

impl<T: Record, C> DoubleEndedIterator for IndexCovered<T, C> {
    fn next_back(&mut self) -> Option<(T::Key, C)> {
        self.0.backward_covered()
    }
}

impl<T: Record, C> ExactSizeIterator for IndexCovered<T, C> {}

pub struct IndexKeys<T: Record>(Box<dyn KeyCursor<T::Key>>);

impl<T: Record> Iterator for IndexKeys<T> {
    type Item = T::Key;

    fn next(&mut self) -> Option<T::Key> {
        self.0.forward()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.0.left(), Some(self.0.left()))
    }
}

impl<T: Record> DoubleEndedIterator for IndexKeys<T> {
    fn next_back(&mut self) -> Option<T::Key> {
        self.0.backward()
    }
}

impl<T: Record> ExactSizeIterator for IndexKeys<T> {}

pub fn stored_rows<T: Record>(rows: &Map<T::Key, T>) -> Result<Vec<Val<T>>> {
    rows.try_values()
}

pub fn unique_eq<I, T, Q>(rows: &Map<T::Key, T>, index: &Map<I, T::Key>, value: &Q) -> Option<Val<T>>
where
    I: Data + Ord + Borrow<Q>,
    T: Record,
    Q: Ord + ?Sized,
{
    rows.get(&*index.get(value)?)
}

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
