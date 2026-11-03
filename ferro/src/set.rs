use std::borrow::Borrow;
use std::fmt;
use std::ops::RangeBounds;

use crate::codec::{Data, Decoder, Encoder};
use crate::error::Result;
use crate::map::{Cursor, Diff, Keys, Map};

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
    pub fn new() -> Self {
        Set::default()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn insert(&mut self, value: T) -> bool {
        if self.0.contains_key(&value) {
            return false;
        }
        self.0.insert(value, ());
        true
    }

    pub fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.remove(value).is_some()
    }

    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.contains_key(value)
    }

    pub fn contains_many<I>(&self, items: I) -> impl Iterator<Item = (I::Item, bool)> + use<I, T>
    where
        I: IntoIterator,
        I::Item: Borrow<T>,
    {
        self.0.contains_many(items)
    }

    pub fn cursor(&self) -> Cursor<T, ()> {
        self.0.cursor()
    }

    pub fn iter(&self) -> Keys<T, ()> {
        self.0.keys()
    }

    pub fn range<Q, R>(&self, range: R) -> Keys<T, ()>
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        self.0.range::<Q, R>(range).into_keys()
    }

    pub fn first(&self) -> Option<T> {
        self.0.first().map(|(key, _)| key)
    }

    pub fn last(&self) -> Option<T> {
        self.0.last().map(|(key, _)| key)
    }

    pub fn pop_first(&mut self) -> Option<T> {
        self.0.pop_first().map(|(key, _)| key)
    }

    pub fn pop_last(&mut self) -> Option<T> {
        self.0.pop_last().map(|(key, _)| key)
    }

    pub fn apply(&mut self, edits: impl IntoIterator<Item = (T, bool)>) {
        self.0.apply(edits.into_iter().map(|(item, keep)| (item, keep.then_some(()))));
    }

    pub fn take_range<Q, R>(&mut self, range: R) -> Set<T>
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        Set(self.0.take_range::<Q, R>(range))
    }

    pub fn remove_range<Q, R>(&mut self, range: R) -> usize
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        self.0.remove_range::<Q, R>(range)
    }

    pub fn split_off<Q>(&mut self, value: &Q) -> Set<T>
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        Set(self.0.split_off(value))
    }

    pub fn append(&mut self, other: &mut Set<T>) {
        self.0.append(&mut other.0);
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.0.retain(|key, _| keep(key));
    }

    pub fn diff(&self, newer: &Set<T>) -> Diff<T, ()> {
        self.0.diff(&newer.0)
    }

    pub fn is_subset(&self, other: &Set<T>) -> bool {
        self.len() <= other.len() && other.contains_many(self.iter()).all(|(_, found)| found)
    }

    pub fn union(&self, other: &Set<T>) -> Set<T> {
        let mut out = self.clone();
        out.extend(other.iter());
        out
    }

    pub fn intersection(&self, other: &Set<T>) -> Set<T> {
        other.contains_many(self.iter()).filter(|(_, found)| *found).map(|(item, _)| item).collect()
    }

    pub fn difference(&self, other: &Set<T>) -> Set<T> {
        other.contains_many(self.iter()).filter(|(_, found)| !found).map(|(item, _)| item).collect()
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
    type IntoIter = Keys<T, ()>;

    fn into_iter(self) -> Keys<T, ()> {
        self.iter()
    }
}
