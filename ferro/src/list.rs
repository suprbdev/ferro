//! An append-mostly sequence with indexed access that nests in your database type.
//!
//! [`List<T>`] keeps its values in a [`Map<u64, T>`](Map) keyed by position, so it shares the
//! map's tree: cloning is O(1), `==` compares root hashes in O(1), and reading or writing a
//! position costs O(log n). [`List::iter`] returns an [`Iter`].

use std::fmt;

use crate::codec::{Data, Decoder, Description, Encoder, Pairing, Tabled};
use crate::error::Result;
use crate::map::{self, Map, Val};

/// A sequence of values that grows and shrinks at the end, with indexed access.
///
/// Values are addressed by position from 0. [`push`](List::push), [`pop`](List::pop) and
/// [`truncate`](List::truncate) change the end, and [`set`](List::set) and
/// [`get_mut`](List::get_mut) change a value in place. Values cannot be inserted or removed in
/// the middle. A list works in memory without a file.
///
/// # Examples
///
/// ```
/// use ferro::List;
///
/// let mut events: List<String> = List::new();
/// events.push("opened".to_string());
/// events.push("paid".to_string());
/// events.extend(["shipped".to_string()]);
///
/// assert_eq!(events.len(), 3);
/// assert_eq!(*events.get(1).unwrap(), "paid");
/// assert_eq!(events.last().unwrap().as_str(), "shipped");
///
/// events.set(1, "refunded".to_string());
/// events.truncate(2);
/// let kept: Vec<String> = events.iter().map(|event| event.into_owned()).collect();
/// assert_eq!(kept, ["opened", "refunded"]);
/// ```
///
/// # Panics
///
/// As with every collection, a read that cannot load a node from the file, or finds one damaged,
/// panics.
pub struct List<T>(Map<u64, T>);

impl<T> Clone for List<T> {
    fn clone(&self) -> Self {
        List(self.0.clone())
    }
}

impl<T> Default for List<T> {
    fn default() -> Self {
        List(Map::default())
    }
}

impl<T: Data> List<T> {
    /// Creates an empty list.
    #[must_use]
    pub fn new() -> Self {
        List::default()
    }

    /// The number of values, in O(1).
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the list has no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Removes every value.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Appends `value` at the end, in O(log n).
    pub fn push(&mut self, value: T) {
        self.0.insert(self.0.len() as u64, value);
    }

    /// Removes and returns the last value.
    pub fn pop(&mut self) -> Option<Val<T>> {
        let last = self.0.len().checked_sub(1)? as u64;
        self.0.remove(&last)
    }

    /// The value at `index`, in O(log n), or `None` past the end.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<Val<T>> {
        self.0.get(&(index as u64))
    }

    /// A mutable reference to the value at `index`, or `None` past the end.
    ///
    /// As with [`Map::get_mut`](crate::Map::get_mut), a panic while the reference is held keeps
    /// what was changed through it.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.0.get_mut(&(index as u64))
    }

    /// Replaces the value at `index` and returns the old one.
    ///
    /// # Panics
    ///
    /// Panics if `index` is not below [`len`](List::len).
    pub fn set(&mut self, index: usize, value: T) -> Option<Val<T>> {
        let key = index as u64;
        assert!(
            index < self.len(),
            "ferro: list index {index} out of bounds for length {}",
            self.len()
        );
        self.0.insert(key, value)
    }

    /// The first value.
    #[must_use]
    pub fn first(&self) -> Option<Val<T>> {
        self.get(0)
    }

    /// The last value.
    #[must_use]
    pub fn last(&self) -> Option<Val<T>> {
        self.get(self.len().checked_sub(1)?)
    }

    /// Marks every value changed, so that the next commit writes it again as its type encodes it
    /// now, as [`Map::rewrite`] does for a map.
    pub fn rewrite(&mut self) {
        self.0.rewrite();
    }

    /// Keeps the first `len` values and removes the rest.
    ///
    /// Touches O(log n) nodes however many values go, and does nothing when the list is no longer
    /// than `len`.
    pub fn truncate(&mut self, len: usize) {
        self.0.remove_range(len as u64..);
    }

    /// Every value, in order, from either end.
    pub fn iter(&self) -> Iter<T> {
        Iter(self.0.values())
    }
}

impl<T: Data> Data for List<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        Ok(List(Map::decode_below(r, Some(|len| len))?))
    }

    fn missing() -> Option<Self> {
        Some(List::new())
    }

    fn describe(d: &mut Description) {
        d.word("list");
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.0.same_encoding(&other.0)
    }

    fn tables(t: &mut Tabled) {
        t.of::<T>();
    }

    fn pair(old: &mut Decoder<'_>, new: &mut Decoder<'_>, pairing: &mut Pairing<'_>) -> Result<()> {
        Map::<u64, T>::pair(old, new, pairing)
    }
}

impl<T: Data> PartialEq for List<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T: Data> Eq for List<T> {}

impl<T: Data + fmt::Debug> fmt::Debug for List<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: Data> Extend<T> for List<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        let start = self.0.len() as u64;
        self.0.extend(
            items
                .into_iter()
                .enumerate()
                .map(|(i, item)| (start + i as u64, item)),
        );
    }
}

impl<T: Data> FromIterator<T> for List<T> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
        let mut list = List::new();
        list.extend(items);
        list
    }
}

impl<T: Data> IntoIterator for &List<T> {
    type Item = Val<T>;
    type IntoIter = Iter<T>;

    fn into_iter(self) -> Iter<T> {
        self.iter()
    }
}

/// An iterator over the values of a [`List`], in order, from either end, made by [`List::iter`].
///
/// It knows its length from the start, and holds the list as it was when it was made, so changes
/// to the list afterwards do not reach it. Each value comes as a [`Val`], and
/// [`next_ref`](Iter::next_ref) and [`visit`](Iter::visit) lend it instead.
///
/// # Panics
///
/// A step that cannot load a node or value from the file, or finds one damaged, panics.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Iter<T>(map::Values<u64, T>);

impl<T> fmt::Debug for Iter<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Iter").finish_non_exhaustive()
    }
}

impl<T: Data> Iter<T> {
    /// Steps to the next value from the front and lends it, where `next` hands out a [`Val`].
    ///
    /// The value is read in place, without the reference count a [`Val`] takes on its node's
    /// column, and stays lent until the iterator moves again.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::List;
    ///
    /// let words: List<String> = ["one", "three", "five"].into_iter().map(String::from).collect();
    ///
    /// let mut values = words.iter();
    /// let mut letters = 0;
    /// while let Some(word) = values.next_ref() {
    ///     letters += word.len();
    /// }
    /// assert_eq!(letters, 12);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if a node or value cannot be read, as `next` does.
    pub fn next_ref(&mut self) -> Option<&T> {
        self.0.next_ref()
    }

    /// Calls `f` on each value the iterator has left, in order from the front, lending it as
    /// [`next_ref`](Iter::next_ref) does.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferro::List;
    ///
    /// let words: List<String> = ["one", "three", "five"].into_iter().map(String::from).collect();
    ///
    /// let mut letters = 0;
    /// words.iter().visit(|word| letters += word.len());
    /// assert_eq!(letters, 12);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if a node or value cannot be read, as `next` does.
    pub fn visit(self, f: impl FnMut(&T)) {
        self.0.visit(f);
    }
}

impl<T: Data> Iterator for Iter<T> {
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

impl<T: Data> DoubleEndedIterator for Iter<T> {
    #[inline]
    fn next_back(&mut self) -> Option<Val<T>> {
        self.0.next_back()
    }
}

impl<T: Data> ExactSizeIterator for Iter<T> {}
