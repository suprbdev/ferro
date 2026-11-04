use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::shared::Erased;

const BATCH: usize = 128;
const BATCH_BYTES: usize = 64 << 10;
const FLUSH_WAIT: Duration = Duration::from_secs(1);

static EPOCH: AtomicU64 = AtomicU64::new(1);
static SLOTS: Mutex<Vec<&'static Slot>> = Mutex::new(Vec::new());
static FREEING: AtomicUsize = AtomicUsize::new(0);

struct Retired {
    epoch: u64,
    bytes: usize,
    _value: Erased,
}

#[derive(Default)]
struct Pending {
    items: Vec<Retired>,
    bytes: usize,
}

impl Pending {
    fn take(&mut self, oldest: u64) -> Vec<Retired> {
        let freed: Vec<Retired> = self.items.extract_if(.., |item| item.epoch < oldest).collect();
        self.bytes -= freed.iter().map(|item| item.bytes).sum::<usize>();
        freed
    }
}

#[repr(align(128))]
struct Slot {
    epoch: AtomicU64,
    taken: AtomicBool,
    pending: Mutex<Pending>,
}

struct Local {
    slot: &'static Slot,
    depth: Cell<usize>,
}

impl Drop for Local {
    fn drop(&mut self) {
        self.slot.epoch.store(0, Ordering::SeqCst);
        self.slot.taken.store(false, Ordering::Release);
    }
}

thread_local! {
    static LOCAL: Local = Local {
        slot: claim(),
        depth: Cell::new(0),
    };
    static FREEING_HERE: Cell<usize> = const { Cell::new(0) };
}

fn claim() -> &'static Slot {
    let mut slots = SLOTS.lock();
    for slot in slots.iter() {
        if slot
            .taken
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return slot;
        }
    }
    let slot: &'static Slot = Box::leak(Box::new(Slot {
        epoch: AtomicU64::new(0),
        taken: AtomicBool::new(true),
        pending: Mutex::new(Pending::default()),
    }));
    slots.push(slot);
    slot
}

pub(crate) struct Guard {
    _thread: PhantomData<*const ()>,
}

pub(crate) fn pin() -> Option<Guard> {
    LOCAL
        .try_with(|local| {
            let depth = local.depth.get();
            if depth == 0 {
                local.slot.epoch.swap(EPOCH.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            local.depth.set(depth + 1);
        })
        .ok()
        .map(|()| Guard {
            _thread: PhantomData,
        })
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = LOCAL.try_with(|local| {
            let depth = local.depth.get() - 1;
            local.depth.set(depth);
            if depth == 0 {
                local.slot.epoch.store(0, Ordering::Release);
            }
        });
    }
}

pub(crate) fn retire(value: Erased, bytes: usize) {
    let item = Retired {
        epoch: EPOCH.load(Ordering::SeqCst),
        bytes,
        _value: value,
    };
    let Ok(slot) = LOCAL.try_with(|local| local.slot) else {
        let slot = claim();
        let mut pending = slot.pending.lock();
        pending.bytes += item.bytes;
        pending.items.push(item);
        drop(pending);
        slot.taken.store(false, Ordering::Release);
        return;
    };
    let due = {
        let mut pending = slot.pending.lock();
        pending.bytes += item.bytes;
        pending.items.push(item);
        pending.items.len() >= BATCH || pending.bytes >= BATCH_BYTES
    };
    if due {
        free(slot, oldest(None));
    }
}

fn free(slot: &Slot, oldest: u64) {
    let freed = {
        let mut pending = slot.pending.lock();
        let freed = pending.take(oldest);
        FREEING.fetch_add(freed.len(), Ordering::SeqCst);
        freed
    };
    let count = freed.len();
    let counted = FREEING_HERE.try_with(|here| here.set(here.get() + count)).is_ok();
    drop(freed);
    if counted {
        let _ = FREEING_HERE.try_with(|here| here.set(here.get() - count));
    }
    FREEING.fetch_sub(count, Ordering::SeqCst);
}

fn oldest(skip: Option<&'static Slot>) -> u64 {
    EPOCH.fetch_add(1, Ordering::SeqCst);
    SLOTS
        .lock()
        .iter()
        .filter(|slot| skip.is_none_or(|skip| !std::ptr::eq(**slot, skip)))
        .map(|slot| slot.epoch.load(Ordering::SeqCst))
        .filter(|epoch| *epoch != 0)
        .min()
        .unwrap_or(u64::MAX)
}

pub(crate) fn flush() {
    let target = EPOCH.load(Ordering::SeqCst);
    let pinned = LOCAL
        .try_with(|local| (local.depth.get() > 0).then_some(local.slot))
        .ok()
        .flatten();
    let own = pinned.map_or(u64::MAX, |slot| slot.epoch.load(Ordering::SeqCst));
    let start = Instant::now();
    let oldest = loop {
        let oldest = oldest(pinned);
        if oldest > target || start.elapsed() > FLUSH_WAIT {
            break oldest.min(target + 1).min(own);
        }
        std::thread::yield_now();
    };
    let slots: Vec<&'static Slot> = SLOTS.lock().clone();
    for slot in slots {
        free(slot, oldest);
    }
    let here = FREEING_HERE.try_with(Cell::get).unwrap_or(0);
    while FREEING.load(Ordering::SeqCst) > here && start.elapsed() <= FLUSH_WAIT {
        std::thread::yield_now();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    use super::*;
    use crate::shared::Shared;

    struct Counted(Arc<AtomicUsize>);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn epoch_retired_values_wait_for_every_earlier_pin() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let outer = pin().unwrap();
        let inner = pin().unwrap();
        drop(inner);
        let pinned = thread::spawn(|| {
            let guard = pin().unwrap();
            thread::yield_now();
            drop(guard);
        });
        retire(Shared::erase(Shared::new(Counted(dropped.clone()))), 1);
        let flusher = thread::spawn(flush);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        drop(outer);
        flusher.join().unwrap();
        pinned.join().unwrap();
        flush();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn epoch_a_full_batch_frees_what_no_pin_can_see() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let dropped = dropped.clone();
                thread::spawn(move || {
                    for _ in 0..BATCH + 1 {
                        let guard = pin().unwrap();
                        retire(Shared::erase(Shared::new(Counted(dropped.clone()))), 1);
                        drop(guard);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        flush();
        assert_eq!(dropped.load(Ordering::SeqCst), 2 * (BATCH + 1));
    }
}
