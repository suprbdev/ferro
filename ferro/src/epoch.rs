use std::any::Any;
use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const BATCH: usize = 128;
const BATCH_BYTES: usize = 64 << 10;
const FLUSH_WAIT: Duration = Duration::from_secs(1);

static EPOCH: AtomicU64 = AtomicU64::new(1);
static SLOTS: Mutex<Vec<&'static Slot>> = Mutex::new(Vec::new());

struct Retired {
    epoch: u64,
    bytes: usize,
    _value: Arc<dyn Any + Send + Sync>,
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

pub(crate) fn retire(value: Arc<dyn Any + Send + Sync>, bytes: usize) {
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
        let oldest = oldest(None);
        let freed = slot.pending.lock().take(oldest);
        drop(freed);
    }
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
        let freed = slot.pending.lock().take(oldest);
        drop(freed);
    }
}
