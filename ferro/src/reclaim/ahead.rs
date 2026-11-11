//! Reading a branch's chain of log entries ahead of the walk, on the copy's other threads.
//!
//! The walk down a chain reads each entry from the one above it, so on one thread it waits for
//! every read in turn. [`Ahead::begin`] splits the chain's versions into stretches, and a thread
//! reads each stretch down its `prev` links, from the tip or from the entry its skip links lead to,
//! checking each record's hash and keeping its bytes. The walk then takes each entry it reads from
//! the stretch that holds it, in order, and reads an entry itself where no stretch has it next: one
//! that no thread has reached, that a damaged link sent elsewhere, or that the walk reaches some
//! other way, such as through a merge. So the walk sees the same records in the same order as when
//! it reads them all itself, and meets any error on its own read.

use std::collections::VecDeque;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::codec::{Data, Decoder, Ptr};
use crate::log::{Chain, Entry, skip_version};
use crate::store::Store;

/// The fewest entries a chain needs before threads read it ahead.
const LEAST: u64 = 2048;
/// The most stretches a chain is split into.
const STRETCHES: usize = 4;
/// The bytes the stretches of one chain may hold at once, shared among them evenly.
const HOLD_BYTES: usize = 16 << 20;
/// The most entries a thread reads before handing them to the walk, after its first few.
const BATCH: usize = 32;
const WAIT: Duration = Duration::from_millis(10);

/// The record of an entry a thread read ahead of the walk, as stored, whose hash checked against
/// the pointer it was read by.
pub(super) struct Fetched {
    pub(super) ptr: Ptr,
    pub(super) len: u32,
    pub(super) record: Vec<u8>,
}

impl Fetched {
    fn weight(&self) -> usize {
        self.record.capacity() + size_of::<Fetched>()
    }
}

/// One stretch of a chain for a thread to read: the versions from `top` down to `bottom`.
pub(super) struct Read {
    generation: u64,
    index: usize,
    chain: Chain,
    top: u64,
    bottom: u64,
}

#[derive(Default)]
struct Stretch {
    fetched: VecDeque<Fetched>,
    next: Option<Ptr>,
    bytes: usize,
    started: bool,
    ended: bool,
}

#[derive(Default)]
struct State {
    generation: u64,
    stretches: Vec<Stretch>,
    current: usize,
    share: usize,
    waiting: bool,
}

/// The stretches of the chain the walk is going down, as threads read them.
#[derive(Default)]
pub(super) struct Ahead {
    state: Mutex<State>,
    fetched: Condvar,
}

impl Ahead {
    /// Splits `chain` into stretches for up to `threads` threads to read, and returns what each
    /// is to read, or nothing for a chain too short to be worth it.
    pub(super) fn begin(&self, chain: &Chain, threads: usize) -> Vec<Read> {
        let mut state = self.state.lock();
        state.generation += 1;
        state.stretches.clear();
        state.current = 0;
        let Some(tip) = chain.tip.filter(|_| threads > 0) else {
            return Vec::new();
        };
        let versions = chain.last - chain.first + 1;
        if versions < LEAST {
            return Vec::new();
        }
        let count = threads.min(STRETCHES) as u64;
        let each = versions.div_ceil(count);
        let generation = state.generation;
        let reads: Vec<Read> = (0..count)
            .map(|index| Read {
                generation,
                index: index as usize,
                chain: *chain,
                top: chain.last - index * each,
                bottom: (chain.last + 1)
                    .saturating_sub((index + 1) * each)
                    .max(chain.first),
            })
            .filter(|read| read.top >= read.bottom && read.top >= chain.first)
            .collect();
        state.share = HOLD_BYTES / reads.len().max(1);
        state.stretches = reads.iter().map(|_| Stretch::default()).collect();
        state.stretches[0].next = Some(tip);
        reads
    }

    /// Drops whatever the threads read of the chain begun last, and stops them.
    pub(super) fn end(&self) {
        let mut state = self.state.lock();
        state.generation += 1;
        state.stretches.clear();
    }

    /// The record of the entry at `ptr` and its length, if the stretch the walk is in, or the one
    /// after it, holds it next, waiting for the thread that reads it there if it is on its way.
    pub(super) fn take(&self, ptr: Ptr) -> Option<Fetched> {
        let mut state = self.state.lock();
        loop {
            let current = state.current;
            let stretch = state.stretches.get_mut(current)?;
            if let Some(front) = stretch.fetched.front() {
                if front.ptr != ptr {
                    return None;
                }
                let fetched = stretch.fetched.pop_front()?;
                stretch.bytes -= fetched.weight();
                return Some(fetched);
            }
            if !stretch.ended {
                let coming = match stretch.next {
                    Some(next) => next == ptr,
                    None => !stretch.started,
                };
                if !coming {
                    return None;
                }
            } else {
                let following = state.stretches.get(current + 1)?;
                if following.started {
                    let first = following.fetched.front().map(|front| front.ptr);
                    if first.or(following.next) != Some(ptr) {
                        return None;
                    }
                    state.current += 1;
                    continue;
                }
            }
            state.waiting = true;
            self.fetched.wait_for(&mut state, WAIT);
            state.waiting = false;
        }
    }

    /// Reads the stretch `read` names from `source`, stopping early at anything the walk should
    /// meet on its own read, once its share of [`HOLD_BYTES`] is full, once the chain it belongs
    /// to is ended, or once `cancel` is set.
    pub(super) fn read(&self, source: &Store, read: &Read, cancel: &AtomicBool) {
        let start = match read.index {
            0 => read.chain.tip,
            _ => find(source, &read.chain, read.top),
        };
        let mut batch = Vec::new();
        if !self.publish(read, Some(start), &mut batch) {
            return self.finish(read);
        }
        let mut next = start;
        let mut expect = read.top;
        while let Some(ptr) = next.filter(|ptr| ptr.offset != 0) {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let mut record = Vec::new();
            let Ok((object, len)) = source.read_record_into(ptr, Some(&mut record)) else {
                break;
            };
            let decoded = Entry::decode(&mut Decoder::new(&object.body, &object.links, None));
            let Ok(entry) = decoded else {
                break;
            };
            if entry.commit.version != expect {
                break;
            }
            next = entry.prev;
            batch.push(Fetched { ptr, len, record });
            if expect == read.bottom {
                break;
            }
            expect -= 1;
            let size = if read.top - expect < 8 { 4 } else { BATCH };
            if batch.len() >= size && !self.publish(read, Some(next), &mut batch) {
                return self.finish(read);
            }
        }
        self.publish(read, None, &mut batch);
        self.finish(read);
    }

    fn publish(&self, read: &Read, next: Option<Option<Ptr>>, batch: &mut Vec<Fetched>) -> bool {
        let mut state = self.state.lock();
        if state.generation != read.generation {
            return false;
        }
        let share = state.share;
        let stretch = &mut state.stretches[read.index];
        let weight: usize = batch.iter().map(Fetched::weight).sum();
        if stretch.bytes + weight > share {
            return false;
        }
        stretch.bytes += weight;
        stretch.started = true;
        stretch.fetched.extend(mem::take(batch));
        if let Some(next) = next {
            stretch.next = next;
        }
        let waiting = state.waiting;
        drop(state);
        if waiting {
            self.fetched.notify_all();
        }
        true
    }

    /// Marks the stretch `read` names as read as far as its thread got.
    pub(super) fn finish(&self, read: &Read) {
        let mut state = self.state.lock();
        if state.generation != read.generation {
            return;
        }
        let stretch = &mut state.stretches[read.index];
        stretch.started = true;
        stretch.ended = true;
        drop(state);
        self.fetched.notify_all();
    }
}

/// The entry of `chain` at `version`, found down its skip links from the tip, or `None` where a
/// read fails or the links lead elsewhere.
fn find(source: &Store, chain: &Chain, version: u64) -> Option<Ptr> {
    let mut ptr = chain.tip?;
    loop {
        if ptr.offset == 0 {
            return None;
        }
        let (object, _) = source.read_record(ptr).ok()?;
        let entry = Entry::decode(&mut Decoder::new(&object.body, &object.links, None)).ok()?;
        let at = entry.commit.version;
        if at == version {
            return Some(ptr);
        }
        if at < version {
            return None;
        }
        ptr = match entry.skip {
            Some(skip) if skip_version(at) >= version => skip,
            _ => entry.prev?,
        };
    }
}
