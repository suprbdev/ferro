use std::mem;
use std::sync::Arc;

use crate::codec::Data;
use crate::db::{Commit, Held};
use crate::error::{Error, Result};
use crate::log::Walk;
use crate::map::fatal;
use crate::store::Store;

pub struct History<T, V, F> {
    walk: Walk,
    store: Store,
    genesis: Option<Arc<T>>,
    project: F,
    run: Option<(Arc<Commit>, V)>,
    started: bool,
    _pin: Arc<Held>,
}

impl<T: Data, V: PartialEq, F: Fn(&T) -> V> History<T, V, F> {
    pub(crate) fn new(
        walk: Walk,
        store: Store,
        genesis: Option<Arc<T>>,
        project: F,
        pin: Arc<Held>,
    ) -> Self {
        History {
            walk,
            store,
            genesis,
            project,
            run: None,
            started: false,
            _pin: pin,
        }
    }

    fn older(&mut self) -> Result<Option<(Arc<Commit>, V)>> {
        let Some(entry) = self.walk.step()? else {
            return Ok(None);
        };
        let version = entry.commit.version;
        let root = entry.commit.root.ok_or(Error::NoVersion(version))?;
        let value = self.store.load::<T>(root)?;
        Ok(Some((entry.commit.clone(), (self.project)(&value))))
    }

    pub fn try_next(&mut self) -> Result<Option<(Commit, V)>> {
        if !mem::replace(&mut self.started, true) {
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
        let unchanged = match &self.genesis {
            Some(genesis) => (self.project)(genesis) == value,
            None => false,
        };
        Ok((!unchanged).then(|| (Arc::unwrap_or_clone(commit), value)))
    }

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

impl<T: Data, V: PartialEq, F: Fn(&T) -> V> Iterator for History<T, V, F> {
    type Item = (Commit, V);

    fn next(&mut self) -> Option<Self::Item> {
        fatal(self.try_next())
    }
}

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
