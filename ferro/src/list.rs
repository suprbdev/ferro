use std::fmt;

use crate::codec::{Data, Decoder, Encoder};
use crate::error::Result;
use crate::map::{Map, Val, Values};

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
    pub fn new() -> Self {
        List::default()
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

    pub fn push(&mut self, value: T) {
        self.0.insert(self.0.len() as u64, value);
    }

    pub fn pop(&mut self) -> Option<Val<T>> {
        let last = self.0.len().checked_sub(1)? as u64;
        self.0.remove(&last)
    }

    pub fn get(&self, index: usize) -> Option<Val<T>> {
        self.0.get(&(index as u64))
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.0.get_mut(&(index as u64))
    }

    pub fn set(&mut self, index: usize, value: T) -> Option<Val<T>> {
        let key = index as u64;
        assert!(
            index < self.len(),
            "ferro: list index {index} out of bounds for length {}",
            self.len()
        );
        self.0.insert(key, value)
    }

    pub fn first(&self) -> Option<Val<T>> {
        self.get(0)
    }

    pub fn last(&self) -> Option<Val<T>> {
        self.get(self.len().checked_sub(1)?)
    }

    pub fn truncate(&mut self, len: usize) {
        self.0.remove_range(len as u64..);
    }

    pub fn iter(&self) -> Values<u64, T> {
        self.0.values()
    }
}

impl<T: Data> Data for List<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        Ok(List(Map::decode(r)?))
    }

    fn missing() -> Option<Self> {
        Some(List::new())
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
        self.0.extend(items.into_iter().enumerate().map(|(i, item)| (start + i as u64, item)));
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
    type IntoIter = Values<u64, T>;

    fn into_iter(self) -> Values<u64, T> {
        self.iter()
    }
}
