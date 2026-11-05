//! The epoch scheme and the hazard records, which let readers borrow nodes without a count.
//!
//! A guarded get pins its thread for the length of one descent with [`pin`], which stores the
//! global epoch in the thread's [`Slot`], and borrows nodes through their memos with
//! [`Weak::lend`](crate::shared::Weak::lend). An iterator, cursor or row finder lives across the
//! caller's code, so it borrows into the slots of a [`Hazards`] record instead. Either way the
//! reader sets the node's `lent` flag and then still sees `cached`, so the cache, which clears
//! `cached` before it reads `lent`, finds the loan when it lets the node go.
//!
//! - **Retiring.** The cache hands its count on a lent node to [`retire`], which puts it on the
//!   thread's retired list with the current epoch. Once the list has grown by a batch since its
//!   last check, a free bumps the epoch, reads every pinned slot and then every record, and lets
//!   go of each item that was retired before the oldest pin and that no record's slot holds. An
//!   item a slot holds has passed its epoch, so from then on it waits only for slots, and a free
//!   lets it go only once it has marked the records and read their slots again, never on the
//!   reading it began with, which may be older than the slot that holds the item.
//! - **Parking.** The last owner of a lent value that a slot holds takes its count back and hands
//!   it to [`park`], which keeps it on the retired list until no slot holds the value, so a clone
//!   through a handle without a count always finds one. When the retired list's own count is the
//!   last on a lent value, it reads the records again and parks what a slot holds.
//! - **Marks.** Whoever parks a value, or skips an item because a slot holds it, marks each record
//!   that holds it and then reads that slot again. A record's owner clears its slots and then reads
//!   its mark, so either the owner sees the mark and flushes when it drops the record, or the
//!   marker finds the slot cleared and lets the item go itself. A borrow that writes a slot and
//!   then finds it cannot lend clears the slot through [`retract`], and the owner reads the mark
//!   at its drop whether or not a slot is still set.
//! - **Flushing.** [`flush`] waits for the threads pinned before it, then frees everything the pins
//!   and slots allow and waits for other threads still dropping what they took from a list, so that
//!   [`Store::close`](crate::store::Store::close) and a marked record's drop let go of what they
//!   kept.
//!
//! So a value retired while a thread is pinned is not let go until that thread unpins, a value
//! lent into a slot is not let go while the slot holds it, and every value is let go once, by
//! whichever thread finds both done. Slots and records live as long as the process and are reused.
//! [Guarded gets and the epoch scheme](https://github.com/archlemon/ferro/blob/main/docs/internals/read-scaling.md#guarded-gets-and-the-epoch-scheme)
//! and [Iterators under hazards](https://github.com/archlemon/ferro/blob/main/docs/internals/read-scaling.md#iterators-under-hazards)
//! describe the protocol, and
//! [How the protocol is checked](https://github.com/archlemon/ferro/blob/main/docs/internals/read-scaling.md#how-the-protocol-is-checked)
//! how it is tested.

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{self, AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[cfg(not(miri))]
use parking_lot::Mutex;

/// `std`'s mutex behind the `lock` of `parking_lot`'s, for Miri, which cannot run the futex call
/// `parking_lot` makes when two threads contend for a lock.
#[cfg(miri)]
struct Mutex<T>(std::sync::Mutex<T>);

#[cfg(miri)]
impl<T> Mutex<T> {
    /// A mutex holding `value`.
    const fn new(value: T) -> Self {
        Mutex(std::sync::Mutex::new(value))
    }

    /// Locks the mutex, ignoring poison, as `parking_lot`'s does.
    fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

use crate::shared::{Dead, Erased, Parked};

/// How many items a retired list gains past what it kept at its last check before it frees a
/// batch.
const BATCH: usize = 128;
/// How many bytes a retired list gains past what it kept at its last check before it frees a
/// batch.
const BATCH_BYTES: usize = 64 << 10;
/// How long [`flush`] waits for the threads pinned before it, and for other threads to finish
/// dropping what they took from a list.
const FLUSH_WAIT: Duration = Duration::from_secs(1);
/// The number of slots in a hazard record.
pub(crate) const HAZARDS: usize = 8;
/// The slot of a record that holds the store an iterator, cursor or row finder borrows.
pub(crate) const STORE: usize = 0;
/// The slot of a record that holds the root an iterator, cursor or row finder borrows.
pub(crate) const ROOT: usize = 1;
/// How many spare records each thread keeps for itself.
const SPARE_RECORDS: usize = 8;
/// The most records there can be. With every one in use, [`Hazards::new`] returns `None`.
const MAX_RECORDS: usize = 4096;
/// The epoch of a retired item that waits only for slots: one that has passed its epoch, or a
/// parked count, which never had one. The epoch starts at 1, so no item retired at an epoch reads
/// as one.
const HELD: u64 = 0;

/// The global epoch. A pin records it in its slot, and every free bumps it before it reads the
/// slots.
static EPOCH: AtomicU64 = AtomicU64::new(1);
/// Every slot ever claimed. Slots are never freed: a thread that exits gives its slot up, retired
/// list and all, for the next thread to claim.
static SLOTS: Mutex<Vec<&'static Slot>> = Mutex::new(Vec::new());
/// How many retired items threads have taken out of a list and not yet finished letting go, which
/// [`flush`] waits for.
static FREEING: AtomicUsize = AtomicUsize::new(0);
/// Every hazard record ever made, which frees and last owners read. Records are never freed.
static RECORDS: Mutex<Vec<&'static Record>> = Mutex::new(Vec::new());
/// The records that no [`Hazards`] holds and no thread keeps as a spare.
static SPARE: Mutex<Vec<&'static Record>> = Mutex::new(Vec::new());

/// The count a retired item holds.
enum Value {
    /// The cache's count on a lent value it let go of, kept until every thread pinned before the
    /// retire has unpinned and no slot holds the value.
    Counted(ManuallyDrop<Erased>),
    /// A count taken back by the thread that let go of a lent value's last count while a slot held
    /// it, kept until no slot does.
    Parked(Parked),
}

/// An item on a thread's retired list.
struct Retired {
    /// The epoch it was retired at, or [`HELD`] once it waits only for slots.
    epoch: u64,
    /// What it weighs, counted against its list's batch.
    bytes: usize,
    /// The count it holds.
    value: Value,
}

impl Retired {
    /// The address of the value's allocation, as the slots hold it.
    fn addr(&self) -> usize {
        match &self.value {
            Value::Counted(value) => value.addr(),
            Value::Parked(value) => value.addr(),
        }
    }

    /// Whether the item waits only for slots and holds the value at one of `addrs`, which are
    /// sorted.
    fn held_in(&self, addrs: &[usize]) -> bool {
        self.epoch == HELD && addrs.binary_search(&self.addr()).is_ok()
    }

    /// Lets go of the item's count. If it was the last on a lent value, it returns the value and
    /// its weight, for the caller to drop or park once it has read the records again; the last
    /// count on a value never lent drops it at once.
    fn release(self) -> Option<(Dead, usize)> {
        let dead = match self.value {
            Value::Counted(value) => ManuallyDrop::into_inner(value).release_count(),
            Value::Parked(value) => value.release(),
        };
        dead.map(|dead| (dead, self.bytes))
    }
}

/// A thread's retired list.
#[derive(Default)]
struct Pending {
    /// The items.
    items: Vec<Retired>,
    /// What the items weigh in all.
    bytes: usize,
    /// How many items the list kept at its last check.
    kept: usize,
    /// What the items it kept at its last check weighed.
    kept_bytes: usize,
}

impl Pending {
    /// Takes out every item that `pick` returns `true` for, letting it change the others, and
    /// counts what is left as what the list kept at this check.
    fn take(&mut self, pick: impl FnMut(&mut Retired) -> bool) -> Vec<Retired> {
        let freed: Vec<Retired> = self.items.extract_if(.., pick).collect();
        self.bytes -= freed.iter().map(|item| item.bytes).sum::<usize>();
        self.kept = self.items.len();
        self.kept_bytes = self.bytes;
        freed
    }

    /// Whether the list has gained [`BATCH`] items or [`BATCH_BYTES`] bytes since its last check.
    fn due(&self) -> bool {
        self.items.len() >= self.kept + BATCH || self.bytes >= self.kept_bytes + BATCH_BYTES
    }
}

/// A thread's epoch slot and retired list.
///
/// It is aligned to 128 bytes, so that no two slots share the pair of cache lines that the
/// adjacent-line prefetcher fetches together. A thread claims one for its life and gives it up,
/// retired list and all, when it exits.
#[repr(align(128))]
struct Slot {
    /// The epoch the thread saw when it pinned, or zero while it is not pinned.
    epoch: AtomicU64,
    /// Whether a live thread has claimed the slot.
    taken: AtomicBool,
    /// The thread's retired list.
    pending: Mutex<Pending>,
}

/// This thread's claim on its [`Slot`], and how deeply its pins are nested.
///
/// Its drop, as the thread exits, unpins the slot and gives it up for another thread to claim.
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

/// A hazard record: [`HAZARDS`] pointer slots and a mark, aligned to 128 bytes as the epoch slots
/// are.
#[repr(align(128))]
struct Record {
    /// The slots, each null or the address of the allocation of a value the owner borrows.
    entries: [AtomicPtr<()>; HAZARDS],
    /// Set by a thread that parks a value or skips an item because a slot here holds it, so that
    /// the owner flushes when it gives the record up.
    hint: AtomicBool,
}

/// This thread's spare records, which go to [`SPARE`] when the thread exits.
struct Spares(RefCell<Vec<&'static Record>>);

impl Drop for Spares {
    fn drop(&mut self) {
        SPARE.lock().append(self.0.get_mut());
    }
}

thread_local! {
    /// This thread's slot, claimed on first use.
    static LOCAL: Local = Local {
        slot: claim(),
        depth: Cell::new(0),
    };
    /// How many of the items counted in [`FREEING`] this thread is letting go, which a flush on
    /// this thread does not wait for.
    static FREEING_HERE: Cell<usize> = const { Cell::new(0) };
    /// This thread's spare records.
    static SPARES: Spares = const { Spares(RefCell::new(Vec::new())) };
}

/// Claims a slot that no live thread holds, or makes a new one.
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

/// A pin of this thread's epoch slot, from [`pin`].
///
/// While it lives, the epoch lets go of nothing retired since it was taken, and the cache retires
/// a lent node only after it has cleared `cached`, so every node borrowed under the guard through
/// [`Weak::lend`](crate::shared::Weak::lend) stays alive for as long as the reference, which the
/// guard's lifetime bounds. Pins nest, and the slot is cleared when the outermost guard is
/// dropped. A guard stands for this thread's slot, so it is neither `Send` nor `Sync`.
pub(crate) struct Guard {
    _thread: PhantomData<*const ()>,
}

/// Pins this thread's slot at the current epoch, or nests in a pin the thread already holds.
///
/// The outermost pin exchanges the epoch into the slot, sequentially consistent, so a free that
/// reads the slot later in that order sees the pin. It returns `None` once the thread's slot is
/// gone because the thread is exiting, and the caller then reads with counts.
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
    /// Leaves the pin, and clears the slot with a release store once the outermost pin is gone,
    /// so a free that reads the cleared slot comes after every borrow made under it.
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

/// A hazard record held by an iterator, cursor or row finder, whose slots keep what they hold from
/// being let go.
///
/// [`Hazards::new`] takes one, and dropping it clears its slots and hands it back, on whichever
/// thread drops it. Only the owner writes the slots, through the lending functions of
/// [`Shared`](crate::shared::Shared) and [`Weak`](crate::shared::Weak). A value lent into a slot
/// is not let go by the epoch, and its last owner parks its count rather than drop it, until the
/// slot is written again or the record dropped, so the owner may read the value through the
/// address until then.
pub(crate) struct Hazards {
    record: &'static Record,
}

impl Hazards {
    /// Takes a record with every slot clear, from this thread's spares, the shared spares or a new
    /// allocation.
    ///
    /// It returns `None` once this thread's spares are gone because the thread is exiting, or
    /// when [`MAX_RECORDS`] records are in use, and the caller then reads with counts.
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

    /// The slot at `index`: [`STORE`], [`ROOT`], or one the iterator chooses by level.
    ///
    /// # Panics
    ///
    /// If `index` is [`HAZARDS`] or more.
    #[inline]
    pub(crate) fn entry(&self, index: usize) -> &AtomicPtr<()> {
        &self.record.entries[index]
    }
}

impl Drop for Hazards {
    /// Clears the slots, hands the record back, and flushes the epoch if a thread marked the
    /// record while a slot held something, so that what the slots kept goes now, a store's file
    /// included.
    ///
    /// The slots are cleared and the mark read behind a sequentially consistent fence, and a
    /// marker sets the mark and then reads the slot again, so either this sees the mark or the
    /// marker sees the slot cleared and lets the item go itself. A slot cleared before the drop,
    /// by a borrow that found it could not lend, went through [`retract`], which has a fence of
    /// its own, so the mark is read even when no slot is set, and then without a fence.
    fn drop(&mut self) {
        let record = self.record;
        let held = record
            .entries
            .iter()
            .any(|entry| !entry.load(Ordering::Relaxed).is_null());
        for entry in &record.entries {
            entry.store(ptr::null_mut(), Ordering::Release);
        }
        if held {
            atomic::fence(Ordering::SeqCst);
        }
        let hinted = record.hint.load(Ordering::Relaxed);
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

/// Clears `hazard`, a slot that a borrow wrote and then found it could not lend through, with a
/// release store and then a sequentially consistent fence.
///
/// A free that marks the record while the slot holds the value reads the slot again after the
/// mark. Either that reading comes after the fence in the single order of sequentially consistent
/// operations and finds the slot cleared, so the free lets the item go itself, or the mark comes
/// before the fence, and the drop of the record, which reads the mark after this whether or not a
/// slot is still set, sees it and flushes. `hazard` must be an entry of a [`Hazards`] record.
#[inline]
pub(crate) fn retract(hazard: &AtomicPtr<()>) {
    hazard.store(ptr::null_mut(), Ordering::Release);
    atomic::fence(Ordering::SeqCst);
}

/// Whether any slot of any record holds `addr`, read sequentially consistent.
///
/// The last owner of a lent value calls it after the acquire that follows its decrement, so it
/// sees every slot written before an earlier owner let go, and parks the value if one holds it.
/// An owner holding the only count calls it before it changes a lent value in place.
pub(crate) fn hazarded_at(addr: usize) -> bool {
    RECORDS
        .lock()
        .iter()
        .flat_map(|record| &record.entries)
        .any(|entry| entry.load(Ordering::SeqCst).addr() == addr)
}

/// The addresses that every slot of every record holds, sorted, read sequentially consistent.
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

/// Retires `value`, the cache's count on a lent node, weighing `bytes`, at the current epoch, and
/// frees a batch from this thread's list if one is due.
///
/// The caller must have cleared the node's `cached` flag and then found `lent` set, both
/// sequentially consistent, so a reader that borrowed the node pinned at an epoch no later than the
/// one read here, or lent it into a slot before the flag was cleared. The count is kept until every
/// thread pinned at that epoch or an earlier one has unpinned and no slot holds the node, and let
/// go by the first free after that.
pub(crate) fn retire(value: Erased, bytes: usize) {
    let epoch = EPOCH.load(Ordering::SeqCst);
    push(Value::Counted(ManuallyDrop::new(value)), bytes, epoch);
}

/// Parks `value`, weighing `bytes`: a count that a lent value's last owner took back because a
/// slot held the value, kept on this thread's retired list until no slot does.
///
/// It marks each record that holds the value and reads the slot again. If none still holds it,
/// it lets the count go at once, and otherwise the owner of each record that does sees the mark
/// and flushes once it drops the record.
pub(crate) fn park(value: Parked, bytes: usize) {
    let addr = [value.addr()];
    let slot = push(Value::Parked(value), bytes, HELD);
    let loose = waits(&addr);
    if !loose.is_empty() {
        release(slot, &mut |item| item.held_in(&loose));
    }
}

/// Marks each record with a slot that holds one of `addrs`, which are sorted, then reads that slot
/// again, both sequentially consistent, and returns, sorted, the addresses that no slot still
/// holds.
///
/// A record's owner clears its slots, when it drops the record or earlier through [`retract`], and
/// then reads its mark behind a sequentially consistent fence. So the caller lets go of each value
/// whose address this returns, which no slot holds, and the owner of a record that still holds
/// any other sees the mark once it clears the slot, and flushes.
fn waits(addrs: &[usize]) -> Vec<usize> {
    let records = RECORDS.lock();
    let mut held = Vec::new();
    for record in records.iter() {
        for entry in &record.entries {
            let addr = entry.load(Ordering::SeqCst).addr();
            if addrs.binary_search(&addr).is_err() {
                continue;
            }
            record.hint.store(true, Ordering::SeqCst);
            if entry.load(Ordering::SeqCst).addr() == addr {
                held.push(addr);
            }
        }
    }
    drop(records);
    held.sort_unstable();
    addrs
        .iter()
        .copied()
        .filter(|addr| held.binary_search(addr).is_err())
        .collect()
}

/// Puts `value`, weighing `bytes`, on this thread's retired list at `epoch`, frees a batch if one
/// is due, and returns the slot whose list holds the item.
///
/// A thread whose slot is gone because it is exiting claims a free slot for the item and gives it
/// straight back, so the next thread to claim that slot, or a flush, lets the item go.
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

/// Lets go of every item on `slot`'s list retired before `oldest` whose address is not in
/// `hazards`, marks the others retired before then as waiting only for slots, and lets go of each
/// item waiting only for slots that no slot holds once it has read the slots again.
///
/// `hazards` must have been read after the bump of the epoch that returned `oldest`. An item
/// retired before that bump was retired after the cache cleared `cached`, which came after every
/// slot a reader wrote to borrow the value, so `hazards` shows each such slot. An item already
/// waiting only for slots may have been found held by another free, or parked by the value's last
/// owner, through a slot written after `hazards` was read, so `hazards` decides only for items
/// still waiting on their epoch. For every item waiting only for slots, those it marks so here and
/// those marked before, it marks the records that hold it and reads them again, as [`park`] does,
/// and lets go of those that no slot still holds.
fn free(slot: &Slot, oldest: u64, hazards: &[usize]) {
    let mut held = Vec::new();
    release(slot, &mut |item| {
        if item.epoch != HELD {
            if item.epoch >= oldest {
                return false;
            }
            if hazards.binary_search(&item.addr()).is_err() {
                return true;
            }
            item.epoch = HELD;
        }
        held.push(item.addr());
        false
    });
    if held.is_empty() {
        return;
    }
    held.sort_unstable();
    held.dedup();
    let loose = waits(&held);
    if !loose.is_empty() {
        release(slot, &mut |item| item.held_in(&loose));
    }
}

/// Takes the items `pick` chooses out of `slot`'s list and lets go of their counts.
///
/// When an item's count was the last on a lent value, it reads the records again. That reading
/// comes after the decrement, which orders it behind every owner that let go before and any slot
/// such an owner wrote, so a value lent through a count that has since gone is seen. What a slot
/// holds is revived, put back on the list as waiting only for slots and marked as [`park`] does,
/// and what none holds is dropped. Items count in [`FREEING`] from when they leave the list until
/// they are dropped, or revived with their slots read again and those that no slot still holds
/// let go, so that a flush that missed them on the list waits for them.
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
    if !dying.is_empty() {
        let held = hazarded();
        let (kept, gone): (Vec<_>, Vec<_>) = dying
            .into_iter()
            .partition(|(dead, _)| held.binary_search(&dead.addr()).is_ok());
        if !kept.is_empty() {
            let mut addrs: Vec<usize> = kept.iter().map(|(dead, _)| dead.addr()).collect();
            addrs.sort_unstable();
            addrs.dedup();
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
            let loose = waits(&addrs);
            if !loose.is_empty() {
                release(slot, &mut |item| item.held_in(&loose));
            }
        } else {
            drop(gone);
        }
    }
    if counted {
        let _ = FREEING_HERE.try_with(|here| here.set(here.get() - count));
    }
    FREEING.fetch_sub(count, Ordering::SeqCst);
}

/// Bumps the epoch, then returns the oldest epoch that a pinned slot other than `skip` holds, or
/// the bumped epoch if none holds an older one.
///
/// No thread pinned in a slot it reads borrows anything retired before the epoch it returns.
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

/// Reads the epochs and the records as a free does, runs `between`, then frees every list with
/// what it read, so that tests can change slots between a free's reading and its release.
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

/// Lets go of everything that the pins and slots allow, after waiting for the threads pinned when
/// it began.
///
/// It waits up to [`FLUSH_WAIT`] for every other thread pinned when it began to unpin or pin again
/// later, then frees, from every thread's list, what was retired before then and no slot holds,
/// though nothing this thread's own pin still protects. Within the same time it then waits for
/// other threads to finish letting go of items they took from a list. A thread pinned for longer
/// than that only delays what it protects to a later free.
/// [`Store::close`](crate::store::Store::close) calls it so that retired nodes holding nested maps
/// do not keep a file open, and the drop of a marked record so that what its slots kept goes.
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
    fn epoch_a_free_that_read_the_records_early_keeps_what_another_found_held() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let hazards = Hazards::new().unwrap();
        let value = Shared::erase(Shared::new(Counted(dropped.clone())));
        let held = std::ptr::without_provenance_mut(value.addr());
        free_after(|| {
            hazards.entry(HAZARDS - 1).store(held, Ordering::SeqCst);
            retire(value, 1);
            flush();
        });
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        drop(hazards);
        flush();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let owner = Shared::new(Counted(dropped.clone()));
        let cache = Shared::erase(owner.clone());
        cache.hold();
        let hazards = Hazards::new().unwrap();
        free_after(|| {
            Shared::lend_to(&owner, hazards.entry(HAZARDS - 1));
            assert!(cache.release());
            retire(cache, 1);
            flush();
        });
        assert_eq!(Shared::strong_count(&owner), 2);
        drop(hazards);
        flush();
        assert_eq!(Shared::strong_count(&owner), 1);
        drop(owner);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
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
