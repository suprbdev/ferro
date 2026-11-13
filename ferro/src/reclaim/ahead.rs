//! Reading a branch's chain of log entries ahead of the walk, on the copy's other threads.
//!
//! The walk down a chain reads each entry from the one above it, so on one thread it waits for
//! every read in turn. [`Ahead::begin`] splits the chain's versions into stretches, and a thread
//! reads each stretch down its `prev` links, from the tip or from the entry its skip links lead to,
//! checking each record's hash and keeping its bytes, those of each batch it hands over in one
//! buffer. The walk then takes each entry it reads from the stretch that holds it, in order, and
//! reads an entry itself where no stretch has it next: one that no thread has reached, that a
//! damaged link sent elsewhere, or that the walk reaches some other way, such as through a merge.
//! So the walk sees the same records in the same order as when it reads them all itself, and meets
//! any error on its own read.

use std::collections::VecDeque;
use std::mem;
use std::ops::Range;
use std::sync::Arc;
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
/// the pointer it was read by, kept with the other records of its batch in one buffer.
pub(super) struct Fetched {
    pub(super) ptr: Ptr,
    pub(super) len: u32,
    bytes: Arc<Vec<u8>>,
    at: Range<usize>,
}

impl Fetched {
    /// The record's bytes.
    pub(super) fn record(&self) -> &[u8] {
        &self.bytes[self.at.clone()]
    }

    fn weight(&self) -> usize {
        self.at.len() + size_of::<Fetched>()
    }
}

/// The records `found` names, read into `bytes`, as one batch that shares the buffer.
fn seal(found: &mut Vec<(Ptr, u32, Range<usize>)>, bytes: &mut Vec<u8>) -> Vec<Fetched> {
    bytes.shrink_to_fit();
    let bytes = Arc::new(mem::take(bytes));
    found
        .drain(..)
        .map(|(ptr, len, at)| Fetched {
            ptr,
            len,
            bytes: bytes.clone(),
            at,
        })
        .collect()
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
        let versions = chain.len() as u64;
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
        if !self.publish(read, Some(start), &mut Vec::new()) {
            return self.finish(read);
        }
        let mut found = Vec::new();
        let mut bytes = Vec::new();
        let mut next = start;
        let mut expect = read.top;
        while let Some(ptr) = next.filter(|ptr| ptr.offset != 0) {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let from = bytes.len();
            let Ok((object, len)) = source.read_record_into(ptr, Some(&mut bytes)) else {
                bytes.truncate(from);
                break;
            };
            let decoded = Entry::decode(&mut Decoder::new(&object.body, &object.links));
            let Ok(entry) = decoded else {
                break;
            };
            if entry.commit.version != expect {
                break;
            }
            next = entry.prev;
            found.push((ptr, len, from..bytes.len()));
            if expect == read.bottom {
                break;
            }
            expect -= 1;
            let size = if read.top - expect < 8 { 4 } else { BATCH };
            if found.len() >= size
                && !self.publish(read, Some(next), &mut seal(&mut found, &mut bytes))
            {
                return self.finish(read);
            }
        }
        self.publish(read, None, &mut seal(&mut found, &mut bytes));
    }

    /// Hands `batch` to the walk as the next records of the stretch `read` names, with `next`
    /// the entry its thread reads after them, or, with `next` `None`, as the stretch's last
    /// records, marking it read as far as its thread got under the same lock.
    ///
    /// Returns `false`, handing over nothing, once the chain it belongs to is ended or `batch`
    /// would take the stretch past its share of [`HOLD_BYTES`].
    fn publish(&self, read: &Read, next: Option<Option<Ptr>>, batch: &mut Vec<Fetched>) -> bool {
        let mut state = self.state.lock();
        if state.generation != read.generation {
            return false;
        }
        let share = state.share;
        let stretch = &mut state.stretches[read.index];
        let weight: usize = batch.iter().map(Fetched::weight).sum();
        let fits = stretch.bytes + weight <= share;
        if fits {
            stretch.bytes += weight;
            stretch.started = true;
            stretch.fetched.extend(mem::take(batch));
        }
        match next {
            Some(next) if fits => stretch.next = next,
            Some(_) => return false,
            None => {
                stretch.started = true;
                stretch.ended = true;
            }
        }
        let waiting = state.waiting;
        drop(state);
        if waiting {
            self.fetched.notify_all();
        }
        fits
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
        let entry = Entry::decode(&mut Decoder::new(&object.body, &object.links)).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::HASH_BYTES;

    fn ptr(at: u64) -> Ptr {
        Ptr {
            hash: [at as u8; HASH_BYTES],
            offset: at,
        }
    }

    fn fetched(at: u64) -> Fetched {
        whole(ptr(at), vec![0])
    }

    fn whole(ptr: Ptr, record: Vec<u8>) -> Fetched {
        Fetched {
            ptr,
            len: 1,
            at: 0..record.len(),
            bytes: Arc::new(record),
        }
    }

    #[test]
    fn the_walk_passes_to_the_next_stretch_once_a_stretch_hands_over_its_last_records() {
        let ahead = Ahead::default();
        let chain = Chain {
            tip: Some(ptr(4096)),
            first: 1,
            last: 4096,
        };
        let reads = ahead.begin(&chain, 2);
        assert_eq!(reads.len(), 2);
        assert!(ahead.publish(&reads[1], Some(Some(ptr(2048))), &mut Vec::new()));
        let mut batch = vec![fetched(2048), fetched(2047)];
        assert!(ahead.publish(&reads[1], Some(Some(ptr(2046))), &mut batch));
        assert!(ahead.publish(&reads[0], Some(Some(ptr(4096))), &mut Vec::new()));
        let mut last = vec![fetched(4096), fetched(4095)];
        assert!(ahead.publish(&reads[0], None, &mut last));
        for at in [4096, 4095, 2048, 2047] {
            let taken = ahead.take(ptr(at)).map(|fetched| fetched.ptr);
            assert_eq!(taken, Some(ptr(at)), "the entry at {at}");
        }
    }

    #[test]
    fn a_last_batch_past_the_share_still_ends_the_stretch() {
        let ahead = Ahead::default();
        let chain = Chain {
            tip: Some(ptr(4096)),
            first: 1,
            last: 4096,
        };
        let reads = ahead.begin(&chain, 2);
        assert!(ahead.publish(&reads[1], Some(Some(ptr(2048))), &mut Vec::new()));
        assert!(ahead.publish(&reads[0], Some(Some(ptr(4096))), &mut Vec::new()));
        let mut huge = vec![whole(ptr(4096), vec![0; HOLD_BYTES])];
        assert!(!ahead.publish(&reads[0], None, &mut huge));
        let state = ahead.state.lock();
        assert!(state.stretches[0].ended);
        assert!(state.stretches[0].fetched.is_empty());
        assert_eq!(state.stretches[0].bytes, 0);
    }
}
