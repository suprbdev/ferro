use std::collections::VecDeque;
use std::sync::Arc;

use crate::admin::Keep;
use crate::codec::{Data, Decoder, Encoder, Hash, Object, Ptr, Sink};
use crate::db::{Commit, Held};
use crate::error::{Error, Result};
use crate::store::Store;

const HAS_PREV: u8 = 1;
const HAS_SKIP: u8 = 2;

#[derive(Clone)]
pub(crate) struct Entry {
    pub(crate) commit: Arc<Commit>,
    pub(crate) prev: Option<Ptr>,
    pub(crate) skip: Option<Ptr>,
}

impl Data for Entry {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.commit.encode(w)?;
        let flags = u8::from(self.prev.is_some()) * HAS_PREV + u8::from(self.skip.is_some()) * HAS_SKIP;
        w.u8(flags);
        if let Some(prev) = self.prev {
            w.link(prev);
        }
        if let Some(skip) = self.skip {
            w.link(skip);
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let commit = Arc::new(Commit::decode(r)?);
        let flags = r.u8()?;
        if flags & !(HAS_PREV | HAS_SKIP) != 0 {
            return Err(Error::Decode(format!("invalid log entry flags {flags}")));
        }
        let prev = if flags & HAS_PREV != 0 { Some(r.link()?) } else { None };
        let skip = if flags & HAS_SKIP != 0 { Some(r.link()?) } else { None };
        Ok(Entry { commit, prev, skip })
    }
}

fn skip_version(version: u64) -> u64 {
    version - (version & version.wrapping_neg())
}

fn corrupt(what: &str) -> Error {
    Error::Corrupt(format!("commit log {what}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chain {
    pub(crate) tip: Option<Ptr>,
    pub(crate) first: u64,
    pub(crate) last: u64,
}

impl Chain {
    pub(crate) fn empty() -> Self {
        Chain {
            tip: None,
            first: 0,
            last: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self.tip {
            Some(_) => (self.last - self.first + 1) as usize,
            None => 0,
        }
    }

    fn descend(
        &self,
        store: &Store,
        mut step: impl FnMut(&Entry) -> Step,
    ) -> Result<Option<Arc<Entry>>> {
        let Some(mut ptr) = self.tip else {
            return Ok(None);
        };
        let mut entry = store.load::<Entry>(ptr)?;
        loop {
            let next = match step(&entry) {
                Step::Found => return Ok(Some(entry)),
                Step::Missing => return Ok(None),
                Step::Skip => entry.skip,
                Step::Prev => entry.prev,
            };
            ptr = next.ok_or_else(|| corrupt("chain ends early"))?;
            let loaded = store.load::<Entry>(ptr)?;
            if loaded.commit.version >= entry.commit.version {
                return Err(corrupt("versions do not descend"));
            }
            entry = loaded;
        }
    }

    pub(crate) fn tip_entry(&self, store: &Store) -> Result<Option<Arc<Entry>>> {
        match self.tip {
            Some(ptr) => Ok(Some(store.load::<Entry>(ptr)?)),
            None => Ok(None),
        }
    }

    pub(crate) fn get(&self, store: &Store, version: u64) -> Result<Option<Arc<Entry>>> {
        if self.tip.is_none() || version < self.first || version > self.last {
            return Ok(None);
        }
        self.descend(store, |entry| {
            let at = entry.commit.version;
            if at == version {
                Step::Found
            } else if at < version {
                Step::Missing
            } else if entry.skip.is_some() && skip_version(at) >= version {
                Step::Skip
            } else {
                Step::Prev
            }
        })
    }

    pub(crate) fn at_time(&self, store: &Store, time_ms: u64) -> Result<Option<Arc<Entry>>> {
        let Some(tip) = self.tip else {
            return Ok(None);
        };
        let mut entry = store.load::<Entry>(tip)?;
        while entry.commit.time_ms > time_ms {
            let at = entry.commit.version;
            let Some(prev) = entry.prev.filter(|_| at > self.first) else {
                return Ok(None);
            };
            let far = match entry.skip {
                Some(skip) if skip_version(at) >= self.first => Some(store.load::<Entry>(skip)?),
                _ => None,
            };
            let loaded = match far {
                Some(far) if far.commit.time_ms > time_ms => far,
                _ => store.load::<Entry>(prev)?,
            };
            if loaded.commit.version >= entry.commit.version {
                return Err(corrupt("versions do not descend"));
            }
            entry = loaded;
        }
        Ok(Some(entry))
    }

    pub(crate) fn walk(&self, store: &Store) -> Walk {
        Walk {
            store: store.clone(),
            next: self.tip,
            above: u64::MAX,
            first: self.first,
        }
    }

    pub(crate) fn retained_skip(&self, entry: &Entry) -> Option<Ptr> {
        entry.skip.filter(|_| skip_version(entry.commit.version) >= self.first)
    }

    pub(crate) fn pruned(&self, store: &Store, keep: Keep) -> Result<Chain> {
        if self.tip.is_none() {
            return Ok(*self);
        }
        let first = match keep {
            Keep::All => self.first,
            Keep::Last(count) => self.last.saturating_add(1).saturating_sub(count.max(1) as u64),
            Keep::Since(time_ms) => match self.at_time(store, time_ms.saturating_sub(1))? {
                Some(entry) => entry.commit.version + 1,
                None => self.first,
            },
        };
        Ok(Chain {
            first: first.clamp(self.first, self.last),
            ..*self
        })
    }

    pub(crate) fn visit(
        &self,
        store: &Store,
        lenient: bool,
        mut each: impl FnMut(Ptr, &Entry, &Object, u32) -> Result<()>,
    ) -> Result<()> {
        let mut next = self.tip;
        let mut above = u64::MAX;
        while let Some(ptr) = next {
            let (object, len) = match store.read_record(ptr) {
                Err(Error::Pruned(_)) if lenient => return Ok(()),
                other => other?,
            };
            let entry = Entry::decode(&mut Decoder::new(&object.body, &object.links, None))?;
            let at = entry.commit.version;
            if at >= above {
                return Err(corrupt("versions do not descend"));
            }
            above = at;
            each(ptr, &entry, &object, len)?;
            next = entry.prev.filter(|_| at > self.first);
        }
        Ok(())
    }

    pub(crate) fn append(&self, store: &Store, sink: &mut dyn Sink, commit: Commit) -> Result<Chain> {
        let version = commit.version;
        let first = if self.tip.is_some() { self.first } else { version };
        let target = skip_version(version);
        let skip = if target >= first && target != version - 1 {
            self.pointer_to(store, target)?
        } else {
            None
        };
        let entry = Entry {
            commit: Arc::new(commit),
            prev: self.tip,
            skip,
        };
        let mut encoder = Encoder::new(sink);
        entry.encode(&mut encoder)?;
        let tip = encoder.finish()?;
        Ok(Chain {
            tip: Some(tip),
            first,
            last: version,
        })
    }

    fn pointer_to(&self, store: &Store, version: u64) -> Result<Option<Ptr>> {
        let mut ptr = self.tip.ok_or_else(|| corrupt("is empty"))?;
        loop {
            let entry = store.load::<Entry>(ptr)?;
            let at = entry.commit.version;
            if at == version {
                return Ok(Some(ptr));
            }
            let next = if at > version && entry.skip.is_some() && skip_version(at) >= version {
                entry.skip
            } else if at > version {
                entry.prev
            } else {
                return Err(corrupt("skip target missing"));
            };
            ptr = next.ok_or_else(|| corrupt("chain ends early"))?;
        }
    }
}

enum Step {
    Found,
    Missing,
    Skip,
    Prev,
}

pub(crate) struct Walk {
    store: Store,
    next: Option<Ptr>,
    above: u64,
    first: u64,
}

impl Walk {
    pub(crate) fn step(&mut self) -> Result<Option<Arc<Entry>>> {
        let Some(ptr) = self.next else {
            return Ok(None);
        };
        let entry = self.store.load::<Entry>(ptr)?;
        if entry.commit.version >= self.above {
            return Err(corrupt("versions do not descend"));
        }
        self.above = entry.commit.version;
        self.next = entry.prev.filter(|_| entry.commit.version > self.first);
        Ok(Some(entry))
    }
}

#[derive(Clone)]
pub struct Log {
    pub(crate) chain: Chain,
    pub(crate) store: Store,
    pub(crate) pin: Arc<Held>,
}

impl Log {
    pub fn len(&self) -> usize {
        self.chain.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chain.tip.is_none()
    }

    pub fn try_get(&self, version: &u64) -> Result<Option<Arc<Commit>>> {
        Ok(self
            .chain
            .get(&self.store, *version)?
            .map(|entry| entry.commit.clone()))
    }

    pub fn get(&self, version: &u64) -> Option<Arc<Commit>> {
        crate::map::fatal(self.try_get(version))
    }

    pub fn first(&self) -> Option<(u64, Arc<Commit>)> {
        self.get(&self.chain.first).map(|commit| (self.chain.first, commit))
    }

    pub fn last(&self) -> Option<(u64, Arc<Commit>)> {
        let entry = crate::map::fatal(self.chain.tip_entry(&self.store))?;
        Some((self.chain.last, entry.commit.clone()))
    }

    pub fn hash_at(&self, version: &u64) -> Option<Hash> {
        if self.is_empty() || *version < self.chain.first || *version > self.chain.last {
            return None;
        }
        let ptr = crate::map::fatal(self.chain.pointer_to(&self.store, *version));
        ptr.map(|ptr| ptr.hash)
    }

    pub fn iter(&self) -> Iter {
        Iter {
            walk: self.chain.walk(&self.store),
            front: VecDeque::new(),
            remaining: self.chain.len(),
            _pin: self.pin.clone(),
        }
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = Arc<Commit>> + ExactSizeIterator {
        self.iter().map(|(_, commit)| commit)
    }
}

pub struct Iter {
    walk: Walk,
    front: VecDeque<Arc<Commit>>,
    remaining: usize,
    _pin: Arc<Held>,
}

impl Iter {
    fn older(&mut self) -> Result<Arc<Commit>> {
        let entry = self.walk.step()?;
        entry
            .map(|entry| entry.commit.clone())
            .ok_or_else(|| corrupt("chain ends early"))
    }

    fn try_next(&mut self) -> Result<Option<Arc<Commit>>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        if self.front.is_empty() {
            for _ in 0..self.remaining {
                let commit = self.older()?;
                self.front.push_front(commit);
            }
        }
        self.remaining -= 1;
        Ok(self.front.pop_front())
    }

    fn try_next_back(&mut self) -> Result<Option<Arc<Commit>>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let commit = match self.front.pop_back() {
            Some(commit) => commit,
            None => self.older()?,
        };
        self.remaining -= 1;
        Ok(Some(commit))
    }
}

impl Iterator for Iter {
    type Item = (u64, Arc<Commit>);

    fn next(&mut self) -> Option<Self::Item> {
        let commit = crate::map::fatal(self.try_next())?;
        Some((commit.version, commit))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl DoubleEndedIterator for Iter {
    fn next_back(&mut self) -> Option<Self::Item> {
        let commit = crate::map::fatal(self.try_next_back())?;
        Some((commit.version, commit))
    }
}

impl ExactSizeIterator for Iter {}
