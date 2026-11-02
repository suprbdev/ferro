use std::sync::Arc;

use crate::codec::{Data, Decoder, Encoder, Ptr, Sink};
use crate::db::Commit;
use crate::error::{Error, Result};
use crate::store::Store;

const HAS_PREV: u8 = 1;
const HAS_SKIP: u8 = 2;

#[derive(Clone)]
pub(crate) struct Entry {
    pub(crate) commit: Commit,
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
        let commit = Commit::decode(r)?;
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
            let Some(prev) = entry.prev else {
                return Ok(None);
            };
            let far = match entry.skip {
                Some(skip) => Some(store.load::<Entry>(skip)?),
                None => None,
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

    pub(crate) fn commits(&self, store: &Store) -> Result<Vec<Commit>> {
        let mut out = Vec::with_capacity(self.len());
        let Some(mut entry) = self.tip_entry(store)? else {
            return Ok(out);
        };
        loop {
            out.push(entry.commit.clone());
            match entry.prev {
                Some(prev) => {
                    let loaded = store.load::<Entry>(prev)?;
                    if loaded.commit.version >= entry.commit.version {
                        return Err(corrupt("versions do not descend"));
                    }
                    entry = loaded;
                }
                None => break,
            }
        }
        out.reverse();
        Ok(out)
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
            commit,
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

    pub(crate) fn rebuild(commits: &[Commit], sink: &mut dyn Sink) -> Result<Chain> {
        let Some(first) = commits.first().map(|commit| commit.version) else {
            return Ok(Chain::empty());
        };
        let mut pointers: Vec<Ptr> = Vec::with_capacity(commits.len());
        for commit in commits {
            let version = commit.version;
            let target = skip_version(version);
            let skip = (target >= first && target != version - 1)
                .then(|| pointers[(target - first) as usize]);
            let entry = Entry {
                commit: commit.clone(),
                prev: pointers.last().copied(),
                skip,
            };
            let mut encoder = Encoder::new(&mut *sink);
            entry.encode(&mut encoder)?;
            pointers.push(encoder.finish()?);
        }
        Ok(Chain {
            tip: pointers.last().copied(),
            first,
            last: commits[commits.len() - 1].version,
        })
    }
}

enum Step {
    Found,
    Missing,
    Skip,
    Prev,
}

#[derive(Clone)]
pub struct Log {
    pub(crate) chain: Chain,
    pub(crate) store: Store,
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
            .map(|entry| Arc::new(entry.commit.clone())))
    }

    pub fn get(&self, version: &u64) -> Option<Arc<Commit>> {
        crate::map::fatal(self.try_get(version))
    }

    pub fn first(&self) -> Option<(u64, Arc<Commit>)> {
        self.get(&self.chain.first).map(|commit| (self.chain.first, commit))
    }

    pub fn last(&self) -> Option<(u64, Arc<Commit>)> {
        let entry = crate::map::fatal(self.chain.tip_entry(&self.store))?;
        Some((self.chain.last, Arc::new(entry.commit.clone())))
    }

    pub fn iter(&self) -> std::vec::IntoIter<(u64, Arc<Commit>)> {
        let commits = crate::map::fatal(self.chain.commits(&self.store));
        commits
            .into_iter()
            .map(|commit| (commit.version, Arc::new(commit)))
            .collect::<Vec<_>>()
            .into_iter()
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = Arc<Commit>> {
        self.iter().map(|(_, commit)| commit)
    }
}
