//! An ordered set that nests in your database type.
//!
//! [`Set<T>`] is a [`Map<T, ()>`](Map) with a set's methods, so it shares the map's tree: cloning
//! is O(1), `==` compares root hashes in O(1), lookups cost O(log n), and range cuts and joins
//! touch O(log n) nodes however many values move.
//!
//! [`Set::iter`], [`Set::range`] and [`Set::prefix`] return an [`Iter`], [`Set::cursor`] a
//! [`Cursor`], and [`Set::diff`] a [`Diff`] of [`Change`]s.

use std::borrow::Borrow;
use std::fmt;
use std::ops::RangeBounds;

use crate::codec::{Data, Decoder, Description, Encoder};
use crate::error::Result;
use crate::map::{self, Map};
use crate::prefix::Prefix;

/// An ordered set of values, stored as a Merkle search tree.
///
/// Values come back in order from either end, and every iterator knows its length, a
/// [`range`](Set::range) included. A set works in memory without a file, and inside a database
/// it reads the nodes it needs as it goes.
///
/// # Examples
///
/// ```
/// use ferro::Set;
///
/// let mut tags: Set<String> = Set::new();
/// assert!(tags.insert("rust".to_string()));
/// assert!(tags.insert("db".to_string()));
/// assert!(!tags.insert("db".to_string()));
///
/// assert!(tags.contains("rust"));
/// assert_eq!(tags.iter().collect::<Vec<_>>(), ["db", "rust"]);
///
/// let other: Set<String> = ["db", "web"].into_iter().map(String::from).collect();
/// assert_eq!(tags.intersection(&other).iter().collect::<Vec<_>>(), ["db"]);
/// assert_eq!(tags.union(&other).len(), 3);
/// ```
///
/// A set of tuples groups its values by their first field:
///
/// ```
/// use ferro::Set;
///
/// let mut follows: Set<(u32, u32)> = Set::new();
/// follows.extend([(1, 2), (1, 3), (2, 1)]);
///
/// assert_eq!(follows.prefix(&1).len(), 2);
/// assert_eq!(follows.range((1, 3)..).collect::<Vec<_>>(), [(1, 3), (2, 1)]);
/// ```
///
/// # Panics
///
/// As with every collection, a read that cannot load a node from the file, or finds one damaged,
/// panics.
pub struct Set<T>(Map<T, ()>);

impl<T> Clone for Set<T> {
    fn clone(&self) -> Self {
        Set(self.0.clone())
    }
}

impl<T> Default for Set<T> {
    fn default() -> Self {
        Set(Map::default())
    }
}

impl<T: Data + Ord> Set<T> {
    /// Creates an empty set.
    #[must_use]
    pub fn new() -> Self {
        Set::default()
    }

    /// The map that holds the values as its keys.
    pub(crate) fn map(&self) -> &Map<T, ()> {
        &self.0
    }

    /// The number of values, in O(1).
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set has no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Removes every value.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Adds `value`, returning whether it was absent. A value already present is left as it is.
    pub fn insert(&mut self, value: T) -> bool {
        if self.0.contains_key(&value) {
            return false;
        }
        self.0.insert(value, ());
        true
    }

    /// Removes `value`, returning whether it was present.
    pub fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.remove(value).is_some()
    }

    /// Whether the set holds `value`, in O(log n).
    #[must_use]
    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.contains_key(value)
    }

    /// Pairs each of `items` with whether the set holds it.
    ///
    /// One cursor answers them all, so items in ascending order cost the distance between them
    /// rather than a search from the root each. Items out of order still get the right answer, at
    /// the cost of a fresh search.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn contains_many<I>(&self, items: I) -> impl Iterator<Item = (I::Item, bool)> + use<I, T>
    where
        I: IntoIterator,
        I::Item: Borrow<T>,
    {
        self.0.contains_many(items)
    }

    /// A [`Cursor`] over the values, whose seeks cost the distance moved.
    ///
    /// The cursor starts with no position: [`Cursor::seek`] or [`Cursor::rewind`] gives it one.
    #[must_use]
    pub fn cursor(&self) -> Cursor<T> {
        Cursor(self.0.cursor())
    }

    /// Every value, in order, from either end.
    pub fn iter(&self) -> Iter<T> {
        Iter(self.0.keys())
    }

    /// The values that lie in `range`, in order, from either end.
    ///
    /// Finding the start costs O(log n), and the iterator knows its length at once.
    pub fn range<Q, R>(&self, range: R) -> Iter<T>
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        Iter(self.0.range::<Q, R>(range).into_keys())
    }

    /// The tuple values whose first field is `head`, in order, from either end.
    ///
    /// The values must be tuples of two to four fields, or a type of your own that implements
    /// [`Prefix`]. It costs what the same [`range`](Set::range) does.
    pub fn prefix<Q>(&self, head: &Q) -> Iter<T>
    where
        T: Prefix<Q>,
        Q: Ord + ?Sized,
    {
        Iter(self.0.prefix(head).into_keys())
    }

    /// The smallest value.
    #[must_use]
    pub fn first(&self) -> Option<T> {
        self.0.first().map(|(key, _)| key)
    }

    /// The largest value.
    #[must_use]
    pub fn last(&self) -> Option<T> {
        self.0.last().map(|(key, _)| key)
    }

    /// Removes and returns the smallest value.
    pub fn pop_first(&mut self) -> Option<T> {
        self.0.pop_first().map(|(key, _)| key)
    }

    /// Removes and returns the largest value.
    pub fn pop_last(&mut self) -> Option<T> {
        self.0.pop_last().map(|(key, _)| key)
    }

    /// Applies a batch of edits in one descent of the tree: `(value, true)` adds a value and
    /// `(value, false)` removes one.
    ///
    /// The edits need not be sorted. When a value appears more than once, its last edit wins.
    pub fn apply(&mut self, edits: impl IntoIterator<Item = (T, bool)>) {
        self.0.apply(
            edits
                .into_iter()
                .map(|(item, keep)| (item, keep.then_some(()))),
        );
    }

    /// Removes the values that lie in `range` and returns them as a set of their own.
    ///
    /// The cut touches O(log n) nodes however many values move.
    #[must_use = "use `remove_range` if the removed entries are not needed"]
    pub fn take_range<Q, R>(&mut self, range: R) -> Set<T>
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        Set(self.0.take_range::<Q, R>(range))
    }

    /// Removes the values that lie in `range` and returns how many there were.
    ///
    /// The cut touches O(log n) nodes and reads none of the removed subtrees.
    pub fn remove_range<Q, R>(&mut self, range: R) -> usize
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        self.0.remove_range::<Q, R>(range)
    }

    /// Splits the set at `value`: returns the values from `value` up and keeps the rest.
    ///
    /// Touches O(log n) nodes however many values move.
    #[must_use = "use `remove_range` if the entries split off are not needed"]
    pub fn split_off<Q>(&mut self, value: &Q) -> Set<T>
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        Set(self.0.split_off(value))
    }

    /// Moves every value of `other` into this set, leaving `other` empty.
    ///
    /// When the two sets do not overlap and belong to the same database, the trees are joined in
    /// O(log n) nodes. Otherwise the values of `other` are copied in as one batch.
    pub fn append(&mut self, other: &mut Set<T>) {
        self.0.append(&mut other.0);
    }

    /// Removes every value for which `keep` returns `false`.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.0.retain(|key, _| keep(key));
    }

    /// The values added and removed between this set and `newer`, in order, as
    /// [`Change::Added`] and [`Change::Removed`].
    ///
    /// Subtrees with the same hash are skipped, so a diff costs O(changes).
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::Set;
    /// use ferro::set::Change;
    ///
    /// let old: Set<u32> = [1, 2, 3].into_iter().collect();
    /// let new: Set<u32> = [2, 3, 4].into_iter().collect();
    ///
    /// let changes: Vec<Change<u32>> = old.diff(&new).collect();
    /// assert_eq!(changes, [Change::Removed(1), Change::Added(4)]);
    /// ```
    pub fn diff(&self, newer: &Set<T>) -> Diff<T> {
        Diff(self.0.diff(&newer.0))
    }

    /// Whether every value of this set is in `other`.
    ///
    /// Looks each value up in `other` through one cursor, after an O(1) check of the lengths.
    #[must_use]
    pub fn is_subset(&self, other: &Set<T>) -> bool {
        self.len() <= other.len() && other.contains_many(self.iter()).all(|(_, found)| found)
    }

    /// A new set with the values of both sets.
    #[must_use]
    pub fn union(&self, other: &Set<T>) -> Set<T> {
        let mut out = self.clone();
        out.extend(other.iter());
        out
    }

    /// A new set with the values of this set that `other` holds too.
    ///
    /// Looks each value of this set up in `other` through one cursor.
    #[must_use]
    pub fn intersection(&self, other: &Set<T>) -> Set<T> {
        other
            .contains_many(self.iter())
            .filter(|(_, found)| *found)
            .map(|(item, _)| item)
            .collect()
    }

    /// A new set with the values of this set that `other` lacks.
    ///
    /// Looks each value of this set up in `other` through one cursor.
    #[must_use]
    pub fn difference(&self, other: &Set<T>) -> Set<T> {
        other
            .contains_many(self.iter())
            .filter(|(_, found)| !found)
            .map(|(item, _)| item)
            .collect()
    }
}

impl<T: Data + Ord> Data for Set<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        Ok(Set(Map::decode(r)?))
    }

    fn missing() -> Option<Self> {
        Some(Set::new())
    }

    fn describe(d: &mut Description) {
        d.word("set");
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.0.same_encoding(&other.0)
    }
}

impl<T: Data + Ord> PartialEq for Set<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T: Data + Ord> Eq for Set<T> {}

impl<T: Data + Ord + fmt::Debug> fmt::Debug for Set<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<T: Data + Ord> Extend<T> for Set<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        self.0.extend(items.into_iter().map(|item| (item, ())));
    }
}

impl<T: Data + Ord> FromIterator<T> for Set<T> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
        let mut set = Set::new();
        set.extend(items);
        set
    }
}

impl<T: Data + Ord> IntoIterator for &Set<T> {
    type Item = T;
    type IntoIter = Iter<T>;

    fn into_iter(self) -> Iter<T> {
        self.iter()
    }
}

/// An iterator over the values of a [`Set`], in order, from either end, made by [`Set::iter`],
/// [`Set::range`] and [`Set::prefix`].
///
/// It knows its length from the start, for a range as for the whole set, and holds the set as it
/// was when it was made, so changes to the set afterwards do not reach it.
///
/// # Panics
///
/// A step that cannot load a node from the file, or finds one damaged, panics.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Iter<T>(map::Keys<T, ()>);

impl<T> fmt::Debug for Iter<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Iter").finish_non_exhaustive()
    }
}

impl<T: Data + Ord> Iterator for Iter<T> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        self.0.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    #[inline]
    fn fold<B, F: FnMut(B, T) -> B>(self, init: B, f: F) -> B {
        self.0.fold(init, f)
    }
}

impl<T: Data + Ord> DoubleEndedIterator for Iter<T> {
    #[inline]
    fn next_back(&mut self) -> Option<T> {
        self.0.next_back()
    }
}

impl<T: Data + Ord> ExactSizeIterator for Iter<T> {}

/// A position among the values of a [`Set`], made by [`Set::cursor`], that keeps its path through
/// the tree, so a seek costs the distance it moves rather than a walk from the root.
///
/// A new cursor has no position until [`seek`](Cursor::seek), [`seek_back`](Cursor::seek_back) or
/// [`rewind`](Cursor::rewind) gives it one, and it loses its position when a move goes past
/// either end. It reads the set as it was when it was made, so changes to the set afterwards do
/// not reach it.
///
/// # Examples
///
/// ```
/// use ferro::Set;
///
/// let evens: Set<u32> = (0..100).step_by(2).collect();
/// let mut cursor = evens.cursor();
/// assert_eq!(cursor.value(), None);
///
/// assert!(!cursor.seek(&41));
/// assert_eq!(cursor.value(), Some(&42));
/// assert_eq!(cursor.advance(), Some(&44));
///
/// assert!(!cursor.seek_back(&41));
/// assert_eq!(cursor.value(), Some(&40));
///
/// cursor.rewind();
/// assert_eq!(cursor.value(), Some(&0));
/// ```
///
/// # Panics
///
/// A move that cannot load a node from the file, or finds one damaged, panics.
pub struct Cursor<T>(map::Cursor<T, ()>);

impl<T> fmt::Debug for Cursor<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cursor").finish_non_exhaustive()
    }
}

impl<T: Data + Ord> Cursor<T> {
    /// Moves to the smallest value, or to no position if the set is empty.
    pub fn rewind(&mut self) {
        self.0.rewind();
    }

    /// Moves to `value` if the set holds it, and otherwise to the first value after it, returning
    /// whether it found `value`.
    ///
    /// With no value after it, the cursor is left with no position.
    pub fn seek<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.seek(value)
    }

    /// Moves to `value` if the set holds it, and otherwise to the last value before it, returning
    /// whether it found `value`.
    ///
    /// With no value before it, the cursor is left with no position.
    pub fn seek_back<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.seek_back(value)
    }

    /// Moves to the next value in order and returns it.
    ///
    /// Past the largest value it returns `None` and leaves the cursor with no position, and a
    /// cursor with no position stays where it is and returns `None`.
    pub fn advance(&mut self) -> Option<&T> {
        self.0.advance()
    }

    /// The value at the cursor, or `None` if it has no position.
    #[must_use]
    pub fn value(&self) -> Option<&T> {
        self.0.key()
    }
}

/// One difference between two versions of a [`Set`], from the older to the newer, as [`Diff`]
/// yields them.
///
/// See [`Set::diff`] for an example.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Change<T> {
    /// The newer set holds this value, and the older one does not.
    Added(T),
    /// The older set held this value, and the newer one does not.
    Removed(T),
}

impl<T> Change<T> {
    /// The value that was added or removed.
    #[must_use]
    pub fn value(&self) -> &T {
        match self {
            Change::Added(value) | Change::Removed(value) => value,
        }
    }
}

/// An iterator over the [`Change`]s between two versions of a [`Set`], in order, made by
/// [`Set::diff`].
///
/// It is lazy: each step expands only the subtrees the two versions do not share, until it finds
/// the next change, so stopping early costs only what was read. It holds both versions, and
/// changes to either set afterwards do not reach it.
///
/// # Panics
///
/// A step that cannot load a node from the file, or finds one damaged, panics.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Diff<T>(map::Diff<T, ()>);

impl<T> fmt::Debug for Diff<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Diff").finish_non_exhaustive()
    }
}

impl<T: Data + Ord> Iterator for Diff<T> {
    type Item = Change<T>;

    #[inline]
    fn next(&mut self) -> Option<Change<T>> {
        loop {
            match self.0.next()? {
                map::Change::Added(value, _) => return Some(Change::Added(value)),
                map::Change::Removed(value, _) => return Some(Change::Removed(value)),
                map::Change::Changed(..) => {}
            }
        }
    }
}
