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
        write!(
            f,
            "unique index {} already holds this value for {:?}",
            self.index, self.holder
        )
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
    panic!(
        "ferro: unique index {} already holds this value",
        taken.index
    )
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

    pub fn keys(&self) -> Keys<T::Key, T> {
        self.0.rows().keys()
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

    pub fn range<Q, R>(&self, range: R) -> Values<T::Key, T>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        self.0.rows().range::<Q, R>(range).into_values()
    }

    pub fn prefix<Q>(&self, head: &Q) -> Values<T::Key, T>
    where
        T::Key: Prefix<Q>,
        Q: Ord + ?Sized,
    {
        self.0.rows().prefix(head).into_values()
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

    pub fn update<Q, R>(&mut self, key: &Q, f: impl FnOnce(&mut T) -> R) -> Option<R>
    where
        T::Key: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut row = self.get_mut(key)?;
        Some(f(&mut row))
    }

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

    pub fn diff(&self, newer: &Table<T>) -> Diff<T::Key, T> {
        self.0.rows().diff(newer.0.rows())
    }

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

pub type Project<I, S, C> = fn(&I, S) -> C;

pub fn plain<I>(_value: &I, _stored: ()) {}

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

    #[inline]
    pub fn keys(self) -> IndexKeys<T> {
        IndexKeys(self.entries)
    }

    #[inline]
    pub fn covered(self) -> IndexCovered<T, C> {
        IndexCovered(self.entries)
    }

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

pub trait IntoMatches<T: Record> {
    type Covered: Send + 'static;

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

pub struct Matches<T: Record, C = ()> {
    rows: Finder<T::Key, T>,
    stream: Box<dyn Stream<T::Key, C>>,
    started: bool,
}

impl<T: Record, C: Send + 'static> Matches<T, C> {
    pub fn and<M: IntoMatches<T>>(self, other: M) -> Matches<T, C> {
        let left = self.unstarted();
        let right = other.into_matches().unstarted();
        Matches {
            rows: left.rows,
            stream: Box::new(All::new(left.stream, Box::new(Blind(right.stream)))),
            started: false,
        }
    }

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

    pub fn keys(self) -> MatchKeys<T> {
        MatchKeys(Box::new(Blind(self.stream)))
    }

    pub fn covered(mut self) -> MatchCovered<T, C> {
        self.stream.cover();
        MatchCovered(self.stream)
    }

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

pub fn index_fingerprint(describe: impl FnOnce(&mut Description)) -> u64 {
    let mut d = Description::new();
    d.word("index");
    describe(&mut d);
    d.fingerprint()
}

pub struct Fingerprints<const N: usize> {
    values: [u64; N],
    order: [usize; N],
}

impl<const N: usize> Fingerprints<N> {
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

    pub fn get(&self, slot: usize) -> u64 {
        self.values[slot]
    }

    pub fn order(&self) -> &[usize; N] {
        &self.order
    }

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

pub fn encode_index<M: Data>(fingerprint: u64, index: &M, w: &mut Encoder<'_>) -> Result<()> {
    w.raw(&fingerprint.to_le_bytes());
    index.encode(w)
}

pub fn stored_rows<T: Record>(rows: &Map<T::Key, T>) -> Result<Vec<Val<T>>> {
    rows.try_values()
}

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
