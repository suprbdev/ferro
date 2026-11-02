use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::codec::{Object, Ptr};
use crate::db::Refs;
use crate::error::{Error, Result};
use crate::log::Chain;
use crate::store::{LOG_START, Places, Store};

const COPY_BYTES: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reclaim {
    pub every_bytes: u64,
    pub fill: f64,
}

impl Default for Reclaim {
    fn default() -> Self {
        Reclaim {
            every_bytes: 64 << 20,
            fill: 0.5,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compacted {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub objects: u64,
    pub commits_dropped: u64,
    pub live_bytes: u64,
    pub moved_bytes: u64,
    pub punched_bytes: u64,
    pub pause: Duration,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Space {
    pub log_bytes: u64,
    pub allocated_bytes: u64,
    pub live_bytes: u64,
    pub cycles: u64,
    pub longest_pause: Duration,
    pub last_error: Option<String>,
}

#[derive(Default)]
pub(crate) struct Roots {
    pub(crate) trees: Vec<Ptr>,
    pub(crate) chains: Vec<Chain>,
    pub(crate) lenient: bool,
}

impl Roots {
    pub(crate) fn of(head: Ptr, refs: &Refs) -> Result<Roots> {
        let chains = refs.iter().map(|(_, branch)| branch.log).collect();
        Ok(Roots {
            trees: vec![head],
            chains,
            lenient: false,
        })
    }

    pub(crate) fn walk(
        &self,
        store: &Store,
        mut fresh: impl FnMut(Ptr) -> bool,
        mut visit: impl FnMut(Ptr, &Object, u32),
    ) -> Result<()> {
        let mut stack = Vec::new();
        let mut entries = HashSet::new();
        for chain in &self.chains {
            chain.visit(store, self.lenient, |ptr, entry, object, len| {
                if entries.insert(ptr.offset) && fresh(ptr) {
                    visit(ptr, object, len);
                }
                stack.extend(entry.commit.root);
                stack.extend(chain.retained_skip(entry));
                Ok(())
            })?;
        }
        stack.extend(&self.trees);
        while let Some(ptr) = stack.pop() {
            if entries.contains(&ptr.offset) || !fresh(ptr) {
                continue;
            }
            let (object, len) = match store.read_record(ptr) {
                Err(Error::Pruned(_)) if self.lenient => continue,
                other => other?,
            };
            visit(ptr, &object, len);
            stack.extend(object.links.iter().copied());
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Extent {
    pub(crate) offset: u64,
    pub(crate) place: u64,
    pub(crate) len: u32,
}

pub(crate) struct Marks {
    end: u64,
    scanned: u64,
    seen: HashSet<u64>,
    pub(crate) kept: Vec<Extent>,
    pub(crate) moved: Vec<Extent>,
}

impl Marks {
    pub(crate) fn new(end: u64) -> Marks {
        Marks {
            end,
            scanned: end,
            seen: HashSet::new(),
            kept: Vec::new(),
            moved: Vec::new(),
        }
    }

    pub(crate) fn walk(
        &mut self,
        store: &Store,
        roots: &Roots,
        stop: Option<&AtomicBool>,
    ) -> Result<Vec<Extent>> {
        let places = store.places();
        let mut found = Vec::new();
        let seen = &mut self.seen;
        let mut halted = false;
        roots.walk(
            store,
            |ptr| {
                halted |= stop.is_some_and(|stop| stop.load(Ordering::Relaxed));
                !halted && seen.insert(ptr.offset)
            },
            |ptr, _, len| {
                if let Some(place) = places.place(ptr.offset) {
                    found.push(Extent {
                        offset: ptr.offset,
                        place,
                        len,
                    });
                }
            },
        )?;
        if halted {
            return Err(Error::Interrupted);
        }
        Ok(found)
    }

    pub(crate) fn rescan(&mut self, store: &Store) -> Result<Vec<Extent>> {
        let to = store.written();
        let mut trees = Vec::new();
        let (end, seen) = (self.end, &self.seen);
        store.scan_links(self.scanned, to, |ptr| {
            if ptr.offset < end && !seen.contains(&ptr.offset) {
                trees.push(ptr);
            }
        })?;
        self.scanned = to;
        if trees.is_empty() {
            return Ok(Vec::new());
        }
        let roots = Roots {
            trees,
            ..Roots::default()
        };
        self.walk(store, &roots, None)
    }

    pub(crate) fn plan(&mut self, mut extents: Vec<Extent>, fill: f64, block: u64) {
        extents.sort_unstable_by_key(|extent| extent.place);
        let mut at = 0;
        while at < extents.len() {
            let first = extents[at].place / block;
            let mut last = (extents[at].place + u64::from(extents[at].len) - 1) / block;
            let mut bytes = u64::from(extents[at].len);
            let mut next = at + 1;
            while let Some(extent) = extents.get(next) {
                if extent.place / block > last + 1 {
                    break;
                }
                last = last.max((extent.place + u64::from(extent.len) - 1) / block);
                bytes += u64::from(extent.len);
                next += 1;
            }
            let span = (last + 1 - first) * block;
            let group = &extents[at..next];
            if (bytes as f64) < fill * span as f64 {
                self.moved.extend_from_slice(group);
            } else {
                self.kept.extend_from_slice(group);
            }
            at = next;
        }
    }

    pub(crate) fn objects(&self) -> u64 {
        (self.kept.len() + self.moved.len()) as u64
    }

    pub(crate) fn live_bytes(&self) -> u64 {
        let all = self.kept.iter().chain(&self.moved);
        all.map(|extent| u64::from(extent.len)).sum()
    }

    pub(crate) fn places(&self) -> Vec<(u64, u64)> {
        let all = self.kept.iter().chain(&self.moved);
        let mut pairs: Vec<(u64, u64)> = all.map(|extent| (extent.offset, extent.place)).collect();
        pairs.sort_unstable();
        pairs
    }

    pub(crate) fn punch(&mut self, store: &Store, stop: Option<&AtomicBool>) -> Result<u64> {
        let block = store.block_bytes()?;
        self.kept.sort_unstable_by_key(|extent| extent.place);
        let mut punched = 0;
        let mut at = LOG_START;
        for extent in &self.kept {
            if extent.place >= self.end {
                break;
            }
            if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
                return Ok(punched);
            }
            if extent.place > at {
                punched += store.punch(at, extent.place, block)?;
            }
            at = at.max(extent.place + u64::from(extent.len));
        }
        if self.end > at {
            punched += store.punch(at, self.end, block)?;
        }
        Ok(punched)
    }
}

pub(crate) fn copy_to(
    store: &Store,
    extents: &mut [Extent],
    start: u64,
    stop: Option<&AtomicBool>,
) -> Result<()> {
    let mut buf = vec![0u8; COPY_BYTES];
    let mut filled = 0usize;
    let mut base = start;
    for extent in extents.iter_mut() {
        if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
            return Err(Error::Interrupted);
        }
        let len = extent.len as usize;
        if filled + len > buf.len() {
            store.write_at(&buf[..filled], base)?;
            base += filled as u64;
            filled = 0;
            buf.resize(len.max(COPY_BYTES), 0);
        }
        store.read_place(&mut buf[filled..filled + len], extent.place)?;
        extent.place = base + filled as u64;
        filled += len;
    }
    store.write_at(&buf[..filled], base)
}

pub(crate) fn table_record(pairs: &[(u64, u64)]) -> (Vec<u8>, crate::codec::Hash) {
    let object = Object {
        body: Places::encode(pairs),
        links: Vec::new(),
    };
    let hash = object.hash();
    let mut record = Vec::new();
    crate::store::append_record(&mut record, 0, &object.body, &[]);
    (record, hash)
}
