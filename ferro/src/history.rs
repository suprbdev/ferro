//! Lazy walks over a branch's history.
//!
//! [`Db::history_of`](crate::Db::history_of) and
//! [`Db::history_of_range`](crate::Db::history_of_range) return a [`History`], which walks the
//! commit log newest first and yields each commit that changed a projection of the value.
//! [`History::take_while`] and [`History::while_some`] stop the walk early without reading past
//! where they stop.

use std::sync::Arc;

use crate::codec::Data;
use crate::db::Commit;
use crate::error::{Error, Result};
use crate::log::{Chain, Walk};
use crate::map::fatal;
use crate::store::Store;

/// A walk over a branch's history, newest first, yielding each commit that changed a projection.
///
/// Each item is a [`Commit`] and the projection's value at it. The walk reads nothing until it is
/// asked for an item, and each item costs the commits walked to find it. It holds the file it walks
/// open, and goes on reading it after a reclamation cycle replaces the file or prunes what it has
/// not reached yet.
pub struct History<T, V, F> {
    chain: Chain,
    pending: Option<(u64, u64)>,
    floor: u64,
    walk: Option<Walk>,
    store: Store,
    genesis: Option<Arc<T>>,
    project: F,
    run: Option<(Arc<Commit>, V)>,
}

impl<T: Data, V: PartialEq, F: Fn(&T) -> V> History<T, V, F> {
    /// A walk over `chain` within `bounds`, comparing the oldest commit with `genesis` when the
    /// chain still retains version 1.
    pub(crate) fn new(
        chain: Chain,
        bounds: Option<(u64, u64)>,
        store: Store,
        genesis: Option<Arc<T>>,
        project: F,
    ) -> Self {
        History {
            chain,
            pending: bounds,
            floor: bounds.map_or(0, |(from, _)| from),
            walk: None,
            store,
            genesis,
            project,
            run: None,
        }
    }

    fn older(&mut self) -> Result<Option<(Arc<Commit>, V)>> {
        let Some(walk) = &mut self.walk else {
            return Ok(None);
        };
        let Some(entry) = walk.step()? else {
            return Ok(None);
        };
        let version = entry.commit.version;
        let root = entry.commit.root.ok_or(Error::NoVersion(version))?;
        let value = self.store.load::<T>(root)?;
        Ok(Some((entry.commit.clone(), (self.project)(&value))))
    }

    /// The next commit that changed the projection, or `None` at the end of the walk.
    ///
    /// The iterator's `next` is this with the error turned into a panic.
    ///
    /// # Errors
    ///
    /// A read error from the log or from loading a version's value, such as [`Error::Corrupt`].
    pub fn try_next(&mut self) -> Result<Option<(Commit, V)>> {
        if let Some((from, to)) = self.pending.take() {
            let baseline = from.saturating_sub(1).max(self.chain.first);
            self.walk = Some(self.chain.walk_down(&self.store, to, baseline)?);
            self.run = self.older()?;
        }
        let Some((mut commit, value)) = self.run.take() else {
            return Ok(None);
        };
        while let Some((older, before)) = self.older()? {
            if before != value {
                self.run = Some((older, before));
                return Ok(Some((Arc::unwrap_or_clone(commit), value)));
            }
            commit = older;
        }
        if commit.version < self.floor {
            return Ok(None);
        }
        let unchanged = match &self.genesis {
            Some(genesis) => (self.project)(genesis) == value,
            None => false,
        };
        Ok((!unchanged).then(|| (Arc::unwrap_or_clone(commit), value)))
    }

    /// The items of this walk while `keep` holds for them, stopping at the first that fails.
    ///
    /// It yields what [`Iterator::take_while`] would, but stops without walking the run of
    /// commits that fails: before walking further it tries `keep` on the next value paired with
    /// the commit last yielded, and ends if that fails. So a test on the value, such as whether a
    /// row exists, costs the commits up to where it fails, not the rest of the log. That shortcut
    /// gives the same items for any test that, once it fails for a value, fails for that value at
    /// every older commit too, as a test on the value alone does.
    pub fn take_while<P>(self, keep: P) -> TakeWhile<T, V, F, P>
    where
        P: Fn(&(Commit, V)) -> bool,
    {
        TakeWhile {
            history: self,
            keep,
            last: None,
            done: false,
        }
    }
}

impl<T: Data, X: PartialEq, F: Fn(&T) -> Option<X>> History<T, Option<X>, F> {
    /// The items of a walk whose projection returns an `Option`, without the `Option`, while it is
    /// `Some`.
    ///
    /// For a projection such as a row by key, this follows the row back to the commit that created
    /// it and stops there, without walking past its creation into the rest of the log.
    pub fn while_some(self) -> WhileSome<T, X, F> {
        WhileSome(self.take_while(present))
    }
}

impl<T: Data, V: PartialEq, F: Fn(&T) -> V> Iterator for History<T, V, F> {
    type Item = (Commit, V);

    fn next(&mut self) -> Option<Self::Item> {
        fatal(self.try_next())
    }
}

type Present<X> = fn(&(Commit, Option<X>)) -> bool;

fn present<X>(item: &(Commit, Option<X>)) -> bool {
    item.1.is_some()
}

/// A walk over a projection's history while it is `Some`, from [`History::while_some`].
pub struct WhileSome<T, X, F>(TakeWhile<T, Option<X>, F, Present<X>>);

impl<T: Data, X: PartialEq, F: Fn(&T) -> Option<X>> WhileSome<T, X, F> {
    /// The next commit that changed the projection while it is `Some`, or `None` at the end.
    ///
    /// # Errors
    ///
    /// As for [`History::try_next`].
    pub fn try_next(&mut self) -> Result<Option<(Commit, X)>> {
        let next = self.0.try_next()?;
        Ok(next.and_then(|(commit, value)| value.map(|value| (commit, value))))
    }
}

impl<T: Data, X: PartialEq, F: Fn(&T) -> Option<X>> Iterator for WhileSome<T, X, F> {
    type Item = (Commit, X);

    fn next(&mut self) -> Option<Self::Item> {
        fatal(self.try_next())
    }
}

/// A walk over a projection's history while a test holds, from [`History::take_while`].
pub struct TakeWhile<T, V, F, P> {
    history: History<T, V, F>,
    keep: P,
    last: Option<Commit>,
    done: bool,
}

impl<T: Data, V: PartialEq, F: Fn(&T) -> V, P: Fn(&(Commit, V)) -> bool> TakeWhile<T, V, F, P> {
    fn older_rejected(&mut self) -> bool {
        let Some(last) = self.last.take() else {
            return false;
        };
        let Some((commit, value)) = self.history.run.take() else {
            return false;
        };
        let probe = (last, value);
        let rejected = !(self.keep)(&probe);
        let (last, value) = probe;
        self.last = Some(last);
        self.history.run = Some((commit, value));
        rejected
    }

    /// The next commit that changed the projection while the test holds, or `None` at the end.
    ///
    /// # Errors
    ///
    /// As for [`History::try_next`].
    pub fn try_next(&mut self) -> Result<Option<(Commit, V)>> {
        if self.done || self.older_rejected() {
            self.done = true;
            return Ok(None);
        }
        match self.history.try_next()? {
            Some(item) if (self.keep)(&item) => {
                self.last = Some(item.0.clone());
                Ok(Some(item))
            }
            _ => {
                self.done = true;
                Ok(None)
            }
        }
    }
}

impl<T: Data, V: PartialEq, F: Fn(&T) -> V, P: Fn(&(Commit, V)) -> bool> Iterator
    for TakeWhile<T, V, F, P>
{
    type Item = (Commit, V);

    fn next(&mut self) -> Option<Self::Item> {
        fatal(self.try_next())
    }
}
