//! Edits staged against one map, made so that a write that changes several maps changes all of
//! them or none.
//!
//! A [`Table`](crate::Table) stages the entries each of its indexes gains and loses in an
//! [`Edits`], loads what they will load before it changes anything, and makes them once nothing
//! is left that can fail.

use std::fmt;
use std::sync::Arc;

use super::{
    Edit, Map, Summary, Undo, Writes, absorb, fatal, insert_at, level_of, ready, ready_remove,
    rekeyed, remove_at,
};
use crate::codec::Data;
use crate::error::Result;

/// Edits to one map that a write changing several maps stages, so that a load that fails or
/// panics partway leaves every map with the entries it had.
///
/// [`ready`](Edits::ready) loads in place what the edits will load, changing no entry, except for
/// a lone insert, alone or beside a lone removal. [`insert_now`](Edits::insert_now) then makes
/// that insert, which loads what it needs and fails as a whole, and loads what the removal beside
/// it will load. [`take_back`](Edits::take_back) undoes the insert while nothing else has changed
/// the map, which loads nothing, and [`apply`](Edits::apply) makes the rest, which has nothing
/// left to load. A removal and an insert of keys that compare equal but encode otherwise are
/// made in that order by `apply`, the removal loaded for down both edges beside its key, along
/// which the insert then cuts. A [`Table`](crate::Table) writes its indexes this way.
pub struct Edits<K, V, S = ()> {
    staged: Pending<K, V, S>,
}

impl<K, V, S> fmt::Debug for Edits<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Edits").finish_non_exhaustive()
    }
}

enum Pending<K, V, S> {
    Empty,
    One(K, Option<V>),
    Pair(K, K, V),
    Swap(K, K, V),
    Many(Writes<K, V>),
    Remove(K),
    Inserted(Option<K>, K, Undo<V, S>),
    Swapped(K, K, u8, Arc<V>),
    Batch(Vec<Edit<K, V>>, Vec<Edit<K, V>>),
}

impl<K, V, S> Pending<K, V, S> {
    fn into_items(self) -> Writes<K, V> {
        match self {
            Pending::Empty | Pending::Inserted(..) => Vec::new(),
            Pending::One(key, value) => vec![(key, value.map(Arc::new))],
            Pending::Pair(gone, come, value) | Pending::Swap(gone, come, value) => {
                vec![(gone, None), (come, Some(Arc::new(value)))]
            }
            Pending::Swapped(gone, come, _, value) => vec![(gone, None), (come, Some(value))],
            Pending::Many(items) => items,
            Pending::Remove(key) => vec![(key, None)],
            Pending::Batch(first, rest) => first
                .into_iter()
                .chain(rest)
                .map(|edit| (edit.key, edit.value))
                .collect(),
        }
    }
}

impl<K, V, S> Default for Edits<K, V, S> {
    fn default() -> Self {
        Edits {
            staged: Pending::Empty,
        }
    }
}

impl<K: Data + Ord, V: Data, S: Summary<K, V>> Edits<K, V, S> {
    /// No edits.
    pub fn new() -> Self {
        Edits::default()
    }

    /// Stages the removal of `key`.
    pub fn remove(&mut self, key: K) {
        self.push(key, None);
    }

    /// Stages storing `value` under `key`.
    ///
    /// After the removal of a key equal to `key` that encodes otherwise, it stores `key` itself
    /// rather than the value alone, as the removal and an insert would.
    pub fn insert(&mut self, key: K, value: V) {
        self.push(key, Some(value));
    }

    #[inline]
    fn push(&mut self, key: K, value: Option<V>) {
        if let Pending::Empty = self.staged {
            self.staged = Pending::One(key, value);
            return;
        }
        self.staged = match (std::mem::replace(&mut self.staged, Pending::Empty), value) {
            (Pending::One(gone, None), Some(value)) if gone == key => {
                if gone.same_encoding(&key) {
                    Pending::One(key, Some(value))
                } else {
                    Pending::Swap(gone, key, value)
                }
            }
            (Pending::One(first, _), value) if first == key => Pending::One(key, value),
            (Pending::One(gone, None), Some(value)) => Pending::Pair(gone, key, value),
            (Pending::One(come, Some(value)), None) => Pending::Pair(key, come, value),
            (staged, value) => {
                let mut items = staged.into_items();
                items.push((key, value.map(Arc::new)));
                Pending::Many(items)
            }
        };
    }

    /// Loads in place every node and value of `map` that the edits will load, changing no entry,
    /// except for a lone insert, which [`insert_now`](Edits::insert_now) makes.
    ///
    /// # Panics
    ///
    /// Panics if a node or value cannot be loaded, as a write does, leaving `map` with the
    /// entries it had.
    #[inline]
    pub fn ready(&mut self, map: &mut Map<K, V, S>) {
        if let Pending::One(_, None) | Pending::Swap(..) | Pending::Many(_) = self.staged {
            fatal(self.try_ready(map));
        }
    }

    fn try_ready(&mut self, map: &mut Map<K, V, S>) -> Result<()> {
        let store = map.store.as_ref();
        self.staged = match std::mem::replace(&mut self.staged, Pending::Empty) {
            Pending::One(key, None) => {
                if store.is_some() {
                    ready_remove(store, &mut map.root, &key)?;
                }
                Pending::Remove(key)
            }
            Pending::Swap(gone, come, value) => {
                let level = level_of(&come)?;
                let gone = Edit {
                    key: gone,
                    level: 0,
                    value: None,
                };
                if store.is_some() {
                    ready(store, &mut map.root, std::slice::from_ref(&gone))?;
                }
                Pending::Swapped(gone.key, come, level, Arc::new(value))
            }
            Pending::Many(items) => {
                let (first, rest) = rekeyed(items);
                let edit = |(key, value): (K, Option<Arc<V>>)| -> Result<Edit<K, V>> {
                    let level = level_of(&key)?;
                    Ok(Edit { key, level, value })
                };
                let first = first.into_iter().map(edit).collect::<Result<Vec<_>>>()?;
                let rest = rest.into_iter().map(edit).collect::<Result<Vec<_>>>()?;
                if store.is_some() {
                    ready(store, &mut map.root, &first)?;
                    ready(store, &mut map.root, &rest)?;
                }
                Pending::Batch(first, rest)
            }
            staged => staged,
        };
        Ok(())
    }

    /// Makes a lone insert, which loads what it needs and fails, or panics, as a whole, leaving
    /// `map` with the entries it had, and then loads in place what a lone removal staged beside it
    /// will load.
    #[inline]
    pub fn insert_now(&mut self, map: &mut Map<K, V, S>) {
        if let Pending::One(_, Some(_)) | Pending::Pair(..) = self.staged {
            self.insert_lone(map);
        }
    }

    fn insert_lone(&mut self, map: &mut Map<K, V, S>) {
        let (gone, key, value) = match std::mem::replace(&mut self.staged, Pending::Empty) {
            Pending::One(key, Some(value)) => (None, key, value),
            Pending::Pair(gone, key, value) => (Some(gone), key, value),
            staged => {
                self.staged = staged;
                return;
            }
        };
        let store = map.store.as_ref();
        let level = fatal(level_of(&key));
        let replaced = fatal(insert_at(
            store,
            &mut map.root,
            key.clone(),
            level,
            Arc::new(value),
            false,
        ));
        self.staged = Pending::Inserted(gone, key, Undo(replaced));
        if let Pending::Inserted(Some(gone), ..) = &self.staged
            && store.is_some()
        {
            fatal(ready_remove(store, &mut map.root, gone));
        }
    }

    /// Takes back the insert [`insert_now`](Edits::insert_now) made, while nothing but its own
    /// loads has changed `map` since, which loads nothing.
    #[inline]
    pub fn take_back(&mut self, map: &mut Map<K, V, S>) {
        if let Pending::Inserted(..) = self.staged
            && let Pending::Inserted(_, key, undo) =
                std::mem::replace(&mut self.staged, Pending::Empty)
        {
            map.undo_insert(&key, undo);
        }
    }

    /// Makes the edits that [`ready`](Edits::ready) and [`insert_now`](Edits::insert_now) loaded
    /// for, which load nothing and cannot fail, and any that they have not been given.
    #[inline]
    pub fn apply(&mut self, map: &mut Map<K, V, S>) {
        if let Pending::Empty | Pending::Inserted(None, ..) = self.staged {
            return;
        }
        self.finish(map);
    }

    fn finish(&mut self, map: &mut Map<K, V, S>) {
        self.ready(map);
        self.insert_now(map);
        let store = map.store.as_ref();
        match std::mem::replace(&mut self.staged, Pending::Empty) {
            Pending::Remove(key) | Pending::Inserted(Some(key), ..) => {
                fatal(remove_at(store, &mut map.root, &key, false));
            }
            Pending::Swapped(gone, come, level, value) => {
                fatal(remove_at(store, &mut map.root, &gone, false));
                fatal(insert_at(store, &mut map.root, come, level, value, false));
            }
            Pending::Batch(first, rest) => {
                for edits in [first, rest] {
                    let count = edits.len();
                    fatal(absorb(
                        store,
                        &mut map.root,
                        &mut edits.into_iter(),
                        count,
                        false,
                    ));
                }
            }
            _ => {}
        }
    }
}
