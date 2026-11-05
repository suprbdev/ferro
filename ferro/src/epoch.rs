use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{self, AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::shared::{Dead, Erased, Parked};

const BATCH: usize = 128;
const BATCH_BYTES: usize = 64 << 10;
const FLUSH_WAIT: Duration = Duration::from_secs(1);
pub(crate) const HAZARDS: usize = 8;
pub(crate) const STORE: usize = 0;
pub(crate) const ROOT: usize = 1;
const SPARE_RECORDS: usize = 8;
const MAX_RECORDS: usize = 4096;
const HELD: u64 = 0;

static EPOCH: AtomicU64 = AtomicU64::new(1);
static SLOTS: Mutex<Vec<&'static Slot>> = Mutex::new(Vec::new());
static FREEING: AtomicUsize = AtomicUsize::new(0);
static RECORDS: Mutex<Vec<&'static Record>> = Mutex::new(Vec::new());
static SPARE: Mutex<Vec<&'static Record>> = Mutex::new(Vec::new());

enum Value {
    Counted(ManuallyDrop<Erased>),
    Parked(Parked),
}

struct Retired {
    epoch: u64,
    bytes: usize,
    value: Value,
}

impl Retired {
    fn addr(&self) -> usize {
        match &self.value {
            Value::Counted(value) => value.addr(),
            Value::Parked(value) => value.addr(),
        }
    }

    fn held_at(&self, addr: usize) -> bool {
        self.epoch == HELD && self.addr() == addr
    }

    fn release(self) -> Option<(Dead, usize)> {
        let dead = match self.value {
            Value::Counted(value) => ManuallyDrop::into_inner(value).release_count(),
            Value::Parked(value) => value.release(),
        };
        dead.map(|dead| (dead, self.bytes))
    }
}

#[derive(Default)]
struct Pending {
    items: Vec<Retired>,
    bytes: usize,
    kept: usize,
    kept_bytes: usize,
}

impl Pending {
    fn take(&mut self, pick: impl FnMut(&mut Retired) -> bool) -> Vec<Retired> {
        let freed: Vec<Retired> = self.items.extract_if(.., pick).collect();
        self.bytes -= freed.iter().map(|item| item.bytes).sum::<usize>();
        self.kept = self.items.len();
        self.kept_bytes = self.bytes;
        freed
    }

    fn due(&self) -> bool {
        self.items.len() >= self.kept + BATCH || self.bytes >= self.kept_bytes + BATCH_BYTES
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

#[repr(align(128))]
struct Record {
    entries: [AtomicPtr<()>; HAZARDS],
    hint: AtomicBool,
}

struct Spares(RefCell<Vec<&'static Record>>);

impl Drop for Spares {
    fn drop(&mut self) {
        SPARE.lock().append(self.0.get_mut());
    }
}

thread_local! {
    static LOCAL: Local = Local {
        slot: claim(),
        depth: Cell::new(0),
    };
    static FREEING_HERE: Cell<usize> = const { Cell::new(0) };
    static SPARES: Spares = const { Spares(RefCell::new(Vec::new())) };
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
                local
                    .slot
                    .epoch
                    .swap(EPOCH.load(Ordering::SeqCst), Ordering::SeqCst);
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

pub(crate) struct Hazards {
    record: &'static Record,
}

impl Hazards {
    pub(crate) fn new() -> Option<Hazards> {
        let spare = SPARES.try_with(|spares| spares.0.borrow_mut().pop()).ok()?;
        let record = match spare.or_else(|| SPARE.lock().pop()) {
            Some(record) => record,
            None => {
                let mut records = RECORDS.lock();
                if records.len() >= MAX_RECORDS {
                    return None;
                }
                let record: &'static Record = Box::leak(Box::new(Record {
                    entries: [const { AtomicPtr::new(ptr::null_mut()) }; HAZARDS],
                    hint: AtomicBool::new(false),
                }));
                records.push(record);
                record
            }
        };
        Some(Hazards { record })
    }

    #[inline]
    pub(crate) fn entry(&self, index: usize) -> &AtomicPtr<()> {
        &self.record.entries[index]
    }
}

impl Drop for Hazards {
    fn drop(&mut self) {
        let record = self.record;
        let held = record
            .entries
            .iter()
            .any(|entry| !entry.load(Ordering::Relaxed).is_null());
        for entry in &record.entries {
            entry.store(ptr::null_mut(), Ordering::Release);
        }
        let hinted = held && {
            atomic::fence(Ordering::SeqCst);
            record.hint.load(Ordering::Relaxed)
        };
        if hinted {
            record.hint.store(false, Ordering::Relaxed);
        }
        let kept = SPARES
            .try_with(|spares| {
                let mut spares = spares.0.borrow_mut();
                let room = spares.len() < SPARE_RECORDS;
                if room {
                    spares.push(record);
                }
                room
            })
            .unwrap_or(false);
        if !kept {
            SPARE.lock().push(record);
        }
        if hinted {
            flush();
        }
    }
}

pub(crate) fn hazarded_at(addr: usize) -> bool {
    RECORDS
        .lock()
        .iter()
        .flat_map(|record| &record.entries)
        .any(|entry| entry.load(Ordering::SeqCst).addr() == addr)
}

fn hazarded() -> Vec<usize> {
    let records = RECORDS.lock();
    let mut held: Vec<usize> = records
        .iter()
        .flat_map(|record| &record.entries)
        .map(|entry| entry.load(Ordering::SeqCst).addr())
        .filter(|addr| *addr != 0)
        .collect();
    drop(records);
    held.sort_unstable();
    held
}

pub(crate) fn retire(value: Erased, bytes: usize) {
    let epoch = EPOCH.load(Ordering::SeqCst);
    push(Value::Counted(ManuallyDrop::new(value)), bytes, epoch);
}

pub(crate) fn park(value: Parked, bytes: usize) {
    let addr = value.addr();
    let slot = push(Value::Parked(value), bytes, HELD);
    if !waits(addr) {
        release(slot, &mut |item| item.held_at(addr));
    }
}

fn waits(addr: usize) -> bool {
    let records = RECORDS.lock();
    let mut held = false;
    for record in records.iter() {
        for entry in &record.entries {
            if entry.load(Ordering::SeqCst).addr() != addr {
                continue;
            }
            record.hint.store(true, Ordering::SeqCst);
            held |= entry.load(Ordering::SeqCst).addr() == addr;
        }
    }
    held
}

fn push(value: Value, bytes: usize, epoch: u64) -> &'static Slot {
    let item = Retired {
        epoch,
        bytes,
        value,
    };
    let Ok(slot) = LOCAL.try_with(|local| local.slot) else {
        let slot = claim();
        let mut pending = slot.pending.lock();
        pending.bytes += item.bytes;
        pending.items.push(item);
        drop(pending);
        slot.taken.store(false, Ordering::Release);
        return slot;
    };
    let due = {
        let mut pending = slot.pending.lock();
        pending.bytes += item.bytes;
        pending.items.push(item);
        pending.due()
    };
    if due {
        let oldest = oldest(None);
        free(slot, oldest, &hazarded());
    }
    slot
}

fn free(slot: &Slot, oldest: u64, hazards: &[usize]) {
    let mut held = Vec::new();
    release(slot, &mut |item| {
        if item.epoch >= oldest {
            return false;
        }
        if hazards.binary_search(&item.addr()).is_err() {
            return true;
        }
        item.epoch = HELD;
        held.push(item.addr());
        false
    });
    for addr in held {
        if !waits(addr) {
            release(slot, &mut |item| item.held_at(addr));
        }
    }
}

fn release(slot: &Slot, pick: &mut dyn FnMut(&mut Retired) -> bool) {
    let freed = {
        let mut pending = slot.pending.lock();
        let freed = pending.take(pick);
        FREEING.fetch_add(freed.len(), Ordering::SeqCst);
        freed
    };
    let count = freed.len();
    let counted = FREEING_HERE
        .try_with(|here| here.set(here.get() + count))
        .is_ok();
    let dying: Vec<(Dead, usize)> = freed.into_iter().filter_map(Retired::release).collect();
    let mut loose = Vec::new();
    if !dying.is_empty() {
        let held = hazarded();
        let (kept, gone): (Vec<_>, Vec<_>) = dying
            .into_iter()
            .partition(|(dead, _)| held.binary_search(&dead.addr()).is_ok());
        if !kept.is_empty() {
            let addrs: Vec<usize> = kept.iter().map(|(dead, _)| dead.addr()).collect();
            let mut pending = slot.pending.lock();
            for (value, bytes) in kept {
                pending.bytes += bytes;
                pending.kept_bytes += bytes;
                pending.kept += 1;
                pending.items.push(Retired {
                    epoch: HELD,
                    bytes,
                    value: Value::Parked(value.revive()),
                });
            }
            drop(pending);
            drop(gone);
            loose = addrs.into_iter().filter(|addr| !waits(*addr)).collect();
        } else {
            drop(gone);
        }
    }
    if counted {
        let _ = FREEING_HERE.try_with(|here| here.set(here.get() - count));
    }
    FREEING.fetch_sub(count, Ordering::SeqCst);
    for addr in loose {
        release(slot, &mut |item| item.held_at(addr));
    }
}

fn oldest(skip: Option<&'static Slot>) -> u64 {
    let bumped = EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    SLOTS
        .lock()
        .iter()
        .filter(|slot| skip.is_none_or(|skip| !std::ptr::eq(**slot, skip)))
        .map(|slot| slot.epoch.load(Ordering::SeqCst))
        .filter(|epoch| *epoch != 0)
        .fold(bumped, u64::min)
}

#[cfg(test)]
pub(crate) fn free_after(between: impl FnOnce()) {
    let oldest = oldest(None);
    let hazards = hazarded();
    between();
    let slots: Vec<&'static Slot> = SLOTS.lock().clone();
    for slot in slots {
        free(slot, oldest, &hazards);
    }
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
    let hazards = hazarded();
    let slots: Vec<&'static Slot> = SLOTS.lock().clone();
    for slot in slots {
        free(slot, oldest, &hazards);
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

    #[test]
    fn epoch_hazards_keep_what_they_hold_through_a_flush() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let hazards = Hazards::new().unwrap();
        let record = std::ptr::from_ref(hazards.record);
        let value = Shared::erase(Shared::new(Counted(dropped.clone())));
        let held = std::ptr::without_provenance_mut(value.addr());
        hazards.entry(HAZARDS - 1).store(held, Ordering::SeqCst);
        retire(value, 1);
        flush();
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        let other = thread::spawn(flush);
        other.join().unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        drop(hazards);
        flush();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        let again = Hazards::new().unwrap();
        assert!(std::ptr::eq(again.record, record));
        assert!(
            again
                .record
                .entries
                .iter()
                .all(|entry| entry.load(Ordering::SeqCst).is_null())
        );
    }

    #[test]
    fn epoch_hazards_move_between_threads() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let value = Shared::erase(Shared::new(Counted(dropped.clone())));
        let held = value.addr();
        let hazards = thread::spawn(move || {
            let hazards = Hazards::new().unwrap();
            let held = std::ptr::without_provenance_mut(held);
            hazards.entry(HAZARDS - 1).store(held, Ordering::SeqCst);
            hazards
        })
        .join()
        .unwrap();
        retire(value, 1);
        flush();
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        thread::spawn(move || drop(hazards)).join().unwrap();
        flush();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}
