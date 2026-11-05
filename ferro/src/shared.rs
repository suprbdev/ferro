//! [`Shared<T>`], the reference count that holds tree nodes and the store's state, with the flags
//! that let readers borrow a value without a count of their own.
//!
//! A `Shared` is an `Arc` of the crate's own. Its allocation, an [`Inner<T>`], holds the strong
//! count at its front, the value right after it, and behind the value a 32-bit weak count and two
//! flags. Owners hold strong counts and, between them, one weak count, which goes with the value. A
//! [`Weak`] holds a weak count of its own, and the allocation is freed with the last weak count, so
//! a `Weak` can read the flags after the value is gone. The flags are only ever accessed
//! atomically, and no reference to them is ever exclusive: the last owner drops the value through a
//! `&mut` to the value alone, and [`Shared::get_mut`], [`Shared::make_mut`] and
//! [`Shared::unwrap_or_clone`] lend or move the value alone.
//!
//! - `cached` is set while the cache's own entry holds a count on the value, and cleared before
//!   that entry lets go of it. [`Weak::lend`] and [`Weak::lend_to`] borrow only a value whose
//!   `cached` is still set after they have set `lent`.
//! - `lent` is set by the first reader that borrows the value without a count: under an epoch
//!   guard through [`Weak::lend`], or into a hazard slot through [`Shared::lend_to`],
//!   [`Weak::lend_to`] or [`Weak::lend_held`]. Whoever lets go of a lent value's count then defers
//!   its drop. The cache, which clears `cached` before it reads `lent`, retires its count to the
//!   epoch, and the last owner of a value that a slot holds parks its count: it takes the count
//!   back and leaves it on the epoch's retired list until no slot holds the value.
//!
//! A value that a slot may hold is never changed in place: `get_mut`, `make_mut` and
//! `unwrap_or_clone` count a lent value as shared while a slot holds it, reading the records to
//! find out, and clear `lent` once none does. [`Erased`] is a `Shared` whose type is erased, as the
//! cache holds nodes, and [`Dead`] and [`Parked`] are what the epoch holds of a lent value at and
//! after its last count.
//! [Where the flags live](https://github.com/archlemon/ferro/blob/main/docs/internals/read-scaling.md#where-the-flags-live)
//! and [Iterators under hazards](https://github.com/archlemon/ferro/blob/main/docs/internals/read-scaling.md#iterators-under-hazards)
//! describe the protocol and why the flags sit behind the value.

use std::alloc::{Layout, dealloc};
use std::any::{Any, TypeId};
use std::hint;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop, offset_of};
use std::ops::Deref;
use std::process;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use crate::epoch::Guard;

/// The highest strong count. A clone, share or upgrade past it aborts the process, as `Arc`'s
/// does.
const MAX_STRONG: usize = isize::MAX as usize;
/// The weak count of a value whose uniqueness [`Shared::unique`] is checking. Downgrades wait while
/// it is set, and it is set only while no [`Weak`] exists, so no clone of one can add to it.
const LOCKED: u32 = u32::MAX;
/// The highest weak count. A downgrade or a clone of a [`Weak`] past it aborts the process.
const MAX_WEAK: u32 = LOCKED - 1;

/// The allocation behind a [`Shared`]: the strong count at the front, the value right after it,
/// and behind the value the weak count and the flags.
///
/// It is `repr(C)`, so every `Inner` starts with its strong count whatever `T` is, which is how an
/// [`Erased`], a [`Dead`] and a [`Parked`] reach the count without the type. A reader reaches the
/// flags from a [`Weak`] without making a reference to the value, and they sit apart from the
/// strong count, which clones and upgrades write.
#[repr(C)]
struct Inner<T> {
    /// The owners' count. The value is alive while it is above zero, and while it passes through
    /// zero on the way to being parked.
    strong: AtomicUsize,
    /// The value.
    data: T,
    /// One count for each [`Weak`], and one that the owners hold between them and let go with the
    /// value. [`Shared::unique`] holds it at [`LOCKED`] while it reads the strong count.
    weak: AtomicU32,
    /// Set while the cache's own entry holds a count on the value.
    cached: AtomicBool,
    /// Set once a reader has borrowed the value without a count, and cleared only by an owner
    /// holding the only count once no slot holds the value.
    lent: AtomicBool,
}

/// A strong count on a value, like `Arc<T>`, whose allocation also carries the flags of the
/// lending protocol.
///
/// Cloning adds a count and dropping lets one go. The last owner drops the value, unless it is lent
/// and a hazard slot holds it, in which case it parks the count on the epoch's retired list until
/// no slot does. [`Shared::downgrade`] makes a [`Weak`] and [`Shared::erase`] an [`Erased`].
///
/// Every `Shared` reaches a live value. One made by [`Shared::new`], a clone, [`Shared::share`],
/// [`Weak::upgrade`] or [`Erased::downcast`] holds a count. One that [`Shared::from_raw`] makes
/// without a count, such as the store an iterator borrows through a slot, is valid for as long as
/// its maker keeps the value alive; it clones only through `share`, and is never dropped.
pub struct Shared<T> {
    ptr: NonNull<Inner<T>>,
    _owns: PhantomData<Inner<T>>,
}

/// A weak count on a [`Shared`] value's allocation, which keeps the allocation and its flags alive
/// but not the value.
///
/// [`Weak::upgrade`] gives a new count while the value is alive. The tree's memos of nodes the
/// cache holds are `Weak`s, and readers borrow through them without a count while the cache holds
/// the node: [`Weak::lend`] under an epoch guard, and [`Weak::lend_to`] into a hazard slot.
pub struct Weak<T> {
    ptr: NonNull<Inner<T>>,
}

/// A [`Shared`] whose type is erased, as the cache holds nodes of every type.
///
/// It holds one strong count. It reaches the count directly, since every [`Inner`] holds it at its
/// front, and the value and the flags through `kind`, a static table of the erased type's offsets
/// and drops. [`Erased::downcast`] gives the `Shared` back.
pub struct Erased {
    ptr: NonNull<u8>,
    kind: &'static Kind,
}

/// The offsets and drops of one erased type's [`Inner`], shared by every [`Erased`] of that type.
struct Kind {
    /// The [`TypeId`] of the erased type, which the downcasts check.
    id: fn() -> TypeId,
    /// The offset of the value in the `Inner`.
    data: usize,
    /// The offset of the `cached` flag.
    cached: usize,
    /// The offset of the `lent` flag.
    lent: usize,
    /// What the owner whose decrement took the count from one to zero calls: it parks the count of
    /// a lent value that a slot holds, and drops the value otherwise.
    drop: unsafe fn(NonNull<u8>),
    /// The final drop of a value whose count is zero and that nothing can reach any more.
    last: unsafe fn(NonNull<u8>),
}

/// A lent value whose last count has just been let go, not yet dropped.
///
/// [`Erased::release_count`] and [`Parked::release`] return one to the epoch, which reads the
/// records again: if a slot still holds the value it takes the count back with [`Dead::revive`],
/// and otherwise it drops the `Dead`, which drops the value and lets the owners' weak count go.
pub struct Dead {
    ptr: NonNull<u8>,
    /// The final drop of the value's type.
    drop: unsafe fn(NonNull<u8>),
}

/// One count of a lent value that a slot holds, kept on the epoch's retired list until no slot
/// does.
///
/// The thread that takes a lent value's count to zero while a slot holds it takes the count back
/// and parks it with [`epoch::park`](crate::epoch::park), and so does the epoch when its own count
/// on a value was the last. A parked value still has a count, so a clone through a handle that
/// holds none finds one. [`Parked::release`] lets the count go; dropping a `Parked` any other way
/// leaks it.
pub struct Parked {
    ptr: NonNull<u8>,
    /// The final drop of the value's type, which the [`Dead`] that [`Parked::release`] returns
    /// takes over.
    drop: unsafe fn(NonNull<u8>),
}

// SAFETY: a `Shared<T>` hands out `&T` on whichever thread holds it, and the thread that lets go of
// its last count drops the `T` or moves it out, as with `Arc<T>`, so it is `Send` only for a
// `T: Send + Sync`. Its counts and flags are atomics, and the value changes only through `get_mut`
// and `make_mut`, which need the only count, no `Weak` and no slot holding the value.
unsafe impl<T: Send + Sync> Send for Shared<T> {}

// SAFETY: a `&Shared<T>` lets another thread read the value, clone a count it may be the last to
// drop, and downgrade or lend it, all through atomics, as a `&Arc<T>` does, so it needs a
// `T: Send + Sync` too.
unsafe impl<T: Send + Sync> Sync for Shared<T> {}

// SAFETY: a `Weak<T>` upgrades to a `Shared<T>` and lends `&T` on whichever thread holds it, and
// its own drop only frees the allocation, so it takes the bounds that `Shared<T>` has. Its counts
// and flags are atomics.
unsafe impl<T: Send + Sync> Send for Weak<T> {}

// SAFETY: as for `Send`: a `&Weak<T>` upgrades, lends and clones only through atomics.
unsafe impl<T: Send + Sync> Sync for Weak<T> {}

// SAFETY: an `Erased` is made only by `Shared::erase`, whose `T` is `Send + Sync`, so moving one
// to another thread is moving that `Shared<T>`.
unsafe impl Send for Erased {}

// SAFETY: a `&Erased` reaches the value only as a `&T`, through `downcast_ref`, and its count and
// flags through atomics, for a `T` that is `Send + Sync`, as a `&Shared<T>` does.
unsafe impl Sync for Erased {}

// SAFETY: a `Dead` value is made only for an `Erased`, whose `T` is `Send + Sync`, or for a value
// whose `lent` flag is set, which only lending functions bounded by `T: Send + Sync` do. Dropping
// it on another thread is then what dropping the last owner there would do.
unsafe impl Send for Dead {}

// SAFETY: a `Parked` value holds one count of a value whose `lent` flag is set, which only lending
// functions bounded by `T: Send + Sync` do, and such a count may be released on any thread.
unsafe impl Send for Parked {}

impl<T> Inner<T> {
    /// Lets go of one weak count, and frees the allocation if it was the last.
    ///
    /// # Safety
    ///
    /// `inner` must point to a live `Inner<T>` on which the caller holds a weak count, a `Weak`'s
    /// or the owners', and gives it up: the caller must not touch the allocation afterwards. The
    /// owners' count may go only once the value has been dropped or moved out, since the allocation
    /// is freed without dropping it.
    #[inline]
    unsafe fn release_weak(inner: *mut Inner<T>) {
        // SAFETY: the caller's weak count keeps the allocation live until this decrement.
        if unsafe { (*inner).weak.fetch_sub(1, Ordering::Release) } == 1 {
            // SAFETY: this decrement took the last weak count, so no other handle reaches the
            // allocation and it is still live. The load acquires every earlier holder's release
            // decrement, so their accesses happen before the free.
            unsafe { (*inner).weak.load(Ordering::Acquire) };
            // SAFETY: `Shared::new` allocated the `Inner<T>` through `Box` with the global
            // allocator and this layout, its value was dropped or moved out before the owners'
            // weak count went, and no weak count is left, so nothing reaches the allocation again.
            unsafe { dealloc(inner.cast(), Layout::new::<Inner<T>>()) };
        }
    }

    /// Lends the value to a hazard slot: writes the allocation's address to `hazard`, sets `lent`
    /// if it is not set, both sequentially consistent, and returns the value's address.
    ///
    /// Whoever lets go of the value's last count from then on finds `lent` set and, reading the
    /// records, the slot, so the value stays alive for as long as `hazard` holds the address, if
    /// `hazard` is an entry of a [`Hazards`](crate::epoch::Hazards) record.
    ///
    /// # Safety
    ///
    /// `inner` must point to a live `Inner<T>` whose value the caller keeps alive until this
    /// returns, by a count or by a guard pinned across the call under which it borrowed the
    /// value. `T` must be `Send + Sync`, as the bounds of every lending function make it, since a
    /// lent value's last count may be parked and let go on another thread.
    #[inline]
    unsafe fn lend_to(inner: *mut Inner<T>, hazard: &AtomicPtr<()>) -> NonNull<T> {
        hazard.store(inner.cast(), Ordering::SeqCst);
        // SAFETY: the caller keeps the allocation live until this returns, and the flag is only
        // ever accessed atomically, so a shared reference to it is sound beside other threads.
        let lent = unsafe { &(*inner).lent };
        if !lent.load(Ordering::Relaxed) {
            lent.store(true, Ordering::SeqCst);
        }
        // SAFETY: `inner` points to a live allocation, so the address of its value is not null,
        // and the projection makes no reference.
        unsafe { NonNull::new_unchecked(&raw mut (*inner).data) }
    }

    /// Drops the value once its count has reached zero, or parks the count if the value is lent
    /// and a slot holds it.
    ///
    /// # Safety
    ///
    /// The caller's release decrement must just have taken the strong count of the live `Inner<T>`
    /// at `inner` from one to zero, and the caller must not touch the allocation afterwards. The
    /// value must not have been dropped.
    #[inline(never)]
    unsafe fn drop_slow(inner: *mut Inner<T>) {
        // SAFETY: the value is not dropped yet, so the owners' weak count keeps the allocation
        // live. The load acquires every earlier owner's release decrement, so their uses of the
        // value, and any slot they wrote before letting go, happen before what follows.
        unsafe { (*inner).strong.load(Ordering::Acquire) };
        // SAFETY: the allocation is live, as above, and `lent` is only ever accessed atomically.
        if unsafe { (*inner).lent.load(Ordering::Relaxed) }
            && crate::epoch::hazarded_at(inner.addr())
        {
            // SAFETY: the caller took the count to zero, the value is not dropped and `lent` is
            // set, which is what `park` asks. A slot holds the value, so it must not be dropped.
            unsafe { Inner::park(inner) };
        } else {
            // SAFETY: the count is zero and nothing can reach the value. Either `lent` is clear,
            // so no reader borrowed it without a count, or no slot holds it, and a reader that
            // borrowed it under a guard did so while the cache held a count, which the cache
            // retired and the epoch let go only once that guard was dropped.
            unsafe { Inner::drop_final(inner) };
        }
    }

    /// Drops the value in place, through a `&mut` to the value alone, and lets the owners' weak
    /// count go.
    ///
    /// # Safety
    ///
    /// `inner` must point to a live `Inner<T>` whose strong count is zero, whose value has not been
    /// dropped or moved out, and which no owner or reader can reach again. The caller gives up the
    /// owners' weak count and must not touch the allocation afterwards.
    unsafe fn drop_final(inner: *mut Inner<T>) {
        // SAFETY: the caller guarantees that nothing reaches the value again and that it has not
        // been dropped, so it is dropped once, here. The `&mut` covers the value and not the
        // counts or flags, which a reader holding a `Weak` may still read.
        unsafe { ptr::drop_in_place(&raw mut (*inner).data) };
        // SAFETY: the value is gone, and the owners' weak count, which the caller gives up, is
        // let go once, here.
        unsafe { Inner::release_weak(inner) };
    }

    /// Parks a lent value whose count has reached zero while a slot holds it: takes the count back
    /// and hands it to the epoch's retired list, which lets it go once no slot holds the value.
    ///
    /// # Safety
    ///
    /// The caller must just have taken the strong count of the live `Inner<T>` at `inner` to zero,
    /// and must not touch the allocation afterwards. The value must not have been dropped, and
    /// `lent` must be set.
    #[cold]
    unsafe fn park(inner: *mut Inner<T>) {
        // SAFETY: the value has not been dropped, so the owners' weak count keeps the allocation
        // live.
        unsafe { (*inner).strong.fetch_add(1, Ordering::Relaxed) };
        let parked = Parked {
            // SAFETY: `inner` points to a live allocation, so it is not null.
            ptr: unsafe { NonNull::new_unchecked(inner.cast()) },
            drop: Inner::<T>::drop_last,
        };
        crate::epoch::park(parked, size_of::<Inner<T>>());
    }

    /// The final drop of a value whose count is zero: acquires the owners' decrements and drops
    /// the value.
    ///
    /// It is the type's [`Kind::last`], and the drop that a [`Parked`] of it passes on to its
    /// [`Dead`].
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live `Inner<T>` whose strong count the caller's release decrement
    /// took to zero, whose value has not been dropped, and which nothing can reach any more: it was
    /// never lent, or no slot holds it.
    unsafe fn drop_last(ptr: NonNull<u8>) {
        let inner = ptr.cast::<Inner<T>>().as_ptr();
        // SAFETY: the value is not dropped, so the owners' weak count keeps the allocation live.
        // The load acquires every owner's release decrement before the drop.
        unsafe { (*inner).strong.load(Ordering::Acquire) };
        // SAFETY: the count is zero, the value is not dropped, and, as the caller guarantees,
        // nothing can reach it, so it is dropped once, here, with the owners' weak count.
        unsafe { Inner::drop_final(inner) };
    }
}

impl<T> Shared<T> {
    /// Puts `data` in a new allocation with one count, no [`Weak`], and both flags clear.
    #[inline]
    pub(crate) fn new(data: T) -> Self {
        let inner = Box::new(Inner {
            strong: AtomicUsize::new(1),
            data,
            weak: AtomicU32::new(1),
            cached: AtomicBool::new(false),
            lent: AtomicBool::new(false),
        });
        Shared {
            ptr: NonNull::from(Box::leak(inner)),
            _owns: PhantomData,
        }
    }

    /// A [`Weak`] on the allocation of `this`, waiting while [`Shared::unique`] holds the weak
    /// count.
    pub(crate) fn downgrade(this: &Self) -> Weak<T> {
        let weak = this.weak();
        let mut count = weak.load(Ordering::Relaxed);
        loop {
            if count == LOCKED {
                hint::spin_loop();
                count = weak.load(Ordering::Relaxed);
                continue;
            }
            if count >= MAX_WEAK {
                process::abort();
            }
            match weak.compare_exchange_weak(count, count + 1, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return Weak { ptr: this.ptr },
                Err(seen) => count = seen,
            }
        }
    }

    /// A mutable reference to the value, if `this` holds its only count, no [`Weak`] exists and
    /// no slot holds it.
    ///
    /// A value whose `lent` is set counts as shared while a slot holds it, which reading the
    /// records finds out. Once none does, `lent` is cleared, so that later edits read no records.
    pub(crate) fn get_mut(this: &mut Self) -> Option<&mut T> {
        if !this.unique() || this.lent().load(Ordering::Relaxed) && Shared::in_slot(this) {
            return None;
        }
        // SAFETY: `unique` found `this` the only count with no `Weak` beside it, and either `lent`
        // was clear, so no reader borrowed the value without a count, or no slot holds it. With
        // `this` borrowed mutably nothing can clone, downgrade or lend it, so nothing else reaches
        // the value while the reference lives, and the reference covers the value alone.
        Some(unsafe { &mut (*this.ptr.as_ptr()).data })
    }

    /// A mutable reference to the value, which is first cloned or moved into a new allocation
    /// unless `this` holds its only count, no [`Weak`] exists and no slot holds it.
    ///
    /// A value with other owners, or that a slot holds, is cloned. One whose only other handles
    /// are [`Weak`]s is moved to a new allocation instead, so that they stop upgrading. A value
    /// whose `lent` is set counts as shared while a slot holds it, as for [`Shared::get_mut`].
    #[inline]
    pub(crate) fn make_mut(this: &mut Self) -> &mut T
    where
        T: Clone,
    {
        if this
            .strong()
            .compare_exchange(1, 0, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            Shared::clone_out(this);
        } else if this.lent().load(Ordering::Relaxed) && Shared::in_slot(this) {
            this.strong().store(1, Ordering::Release);
            Shared::clone_out(this);
        } else if this.weak().load(Ordering::Relaxed) != 1 {
            Shared::move_out(this);
        } else {
            this.strong().store(1, Ordering::Release);
        }
        // SAFETY: `this` holds the only count of a value that no `Weak` and no slot reaches:
        // either its count went from one to zero here with no `Weak` beside it and no slot
        // holding it, and is back at one, or `clone_out` or `move_out` put it in a new
        // allocation. With `this` borrowed mutably nothing else reaches the value while the
        // reference lives.
        unsafe { &mut (*this.ptr.as_ptr()).data }
    }

    /// Whether a slot holds the value, which reading the records finds out, clearing `lent` when
    /// none does.
    ///
    /// Only an owner holding the value's only count calls it, while nothing can lend the value
    /// anew, so a cleared `lent` stays right until the value is next lent.
    #[cold]
    #[inline(never)]
    fn in_slot(this: &Self) -> bool {
        if crate::epoch::hazarded_at(this.ptr.addr().get()) {
            return true;
        }
        this.lent().store(false, Ordering::Relaxed);
        false
    }

    /// Replaces `this` with the only count on a clone of its value, in a new allocation.
    #[cold]
    #[inline(never)]
    fn clone_out(this: &mut Self)
    where
        T: Clone,
    {
        *this = Shared::new((**this).clone());
    }

    /// Moves the value into a new allocation that `this` then holds, and leaves the old one to its
    /// [`Weak`]s, which no longer upgrade.
    ///
    /// Only [`Shared::make_mut`] calls it, once it has taken the count from one to zero with no
    /// slot holding the value.
    #[cold]
    #[inline(never)]
    fn move_out(this: &mut Self) {
        // SAFETY: `make_mut` took the count from one to zero with no slot holding the value, so no
        // owner or reader reaches it and no `Weak` can upgrade. The value is read out once, and
        // the old allocation never drops it: the old handle is forgotten and only the owners'
        // weak count is let go.
        let data = unsafe { ptr::read(&raw const (*this.ptr.as_ptr()).data) };
        let old = ManuallyDrop::new(mem::replace(this, Shared::new(data)));
        drop(Weak { ptr: old.ptr });
    }

    /// The value, moved out if `this` holds its only count and no slot holds it, and cloned
    /// otherwise.
    ///
    /// A value whose `lent` is set counts as shared while a slot holds it, as for
    /// [`Shared::get_mut`]. [`Weak`]s on a value moved out no longer upgrade.
    pub(crate) fn unwrap_or_clone(this: Self) -> T
    where
        T: Clone,
    {
        if this
            .strong()
            .compare_exchange(1, 0, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return (*this).clone();
        }
        if this.lent().load(Ordering::Relaxed) && Shared::in_slot(&this) {
            this.strong().store(1, Ordering::Release);
            return (*this).clone();
        }
        let this = ManuallyDrop::new(this);
        // SAFETY: the count went from one to zero here, so no other owner exists and no `Weak`
        // can upgrade, and either `lent` is clear or no slot holds the value, so no reader
        // borrows it. The value is read out once, and the old allocation never drops it: the
        // handle is forgotten and only the owners' weak count is let go.
        let data = unsafe { ptr::read(&raw const (*this.ptr.as_ptr()).data) };
        drop(Weak { ptr: this.ptr });
        data
    }

    /// The address of the value, without a count.
    #[inline]
    pub(crate) fn as_ptr(this: &Self) -> *const T {
        // SAFETY: `this` reaches a live allocation, as every `Shared` does, and the projection
        // makes no reference.
        unsafe { &raw const (*this.ptr.as_ptr()).data }
    }

    /// Whether `a` and `b` are counts on one allocation.
    #[inline]
    pub(crate) fn ptr_eq(a: &Self, b: &Self) -> bool {
        a.ptr == b.ptr
    }

    /// The address of the value, keeping the count of `this` for [`Shared::from_raw`] to take
    /// back.
    #[inline]
    pub(crate) fn into_raw(this: Self) -> *const T {
        let this = ManuallyDrop::new(this);
        Shared::as_ptr(&this)
    }

    /// A handle on the value at `data`, an address that [`Shared::as_ptr`], [`Shared::into_raw`]
    /// or a lending function gave.
    ///
    /// The handle takes no count of its own. Either it takes over one the caller gives up, as
    /// from `into_raw`, or it holds none, as the store an iterator borrows through a slot does,
    /// and then it must clone only through [`Shared::share`] and never be dropped.
    ///
    /// # Safety
    ///
    /// `data` must be the address of the value in a live `Shared<T>` allocation. If the caller
    /// gives the handle a count, it may be used and dropped like any other. If not, the caller
    /// must keep the value alive for as long as the handle is used, by a count held elsewhere or a
    /// slot that holds the value, and must never drop the handle.
    #[inline]
    pub(crate) unsafe fn from_raw(data: *const T) -> Self {
        // SAFETY: `data` is the address of the value inside a live `Inner<T>`, which starts the
        // value's offset before it, in the same allocation.
        let inner = unsafe { data.byte_sub(offset_of!(Inner<T>, data)) }.cast::<Inner<T>>();
        Shared {
            // SAFETY: `inner` is the start of a live allocation, so it is not null.
            ptr: unsafe { NonNull::new_unchecked(inner.cast_mut()) },
            _owns: PhantomData,
        }
    }

    /// Copies a loan into a second slot: writes the allocation's address to `hazard`, then reads
    /// `cached` again, both sequentially consistent, and returns whether the copy holds.
    ///
    /// If `cached` is still set, the cache clears it only after the new slot was written, so any
    /// free of the value, which comes after that, reads the new slot. If it was cleared, this
    /// clears `hazard` through [`epoch::retract`](crate::epoch::retract), as [`Weak::lend_to`]
    /// does, and returns `false`, and the caller takes a count instead, through
    /// [`Shared::share`], since the count may be passing through zero.
    ///
    /// # Safety
    ///
    /// `data` must be the address of the value in a live `Shared<T>` allocation, and the caller
    /// must keep the value alive across the call, as a slot of its own that holds the value does.
    #[inline]
    pub(crate) unsafe fn lend_again(data: *const T, hazard: &AtomicPtr<()>) -> bool {
        // SAFETY: `data` is the address of the value inside a live `Inner<T>`, which starts the
        // value's offset before it, in the same allocation.
        let inner = unsafe { data.byte_sub(offset_of!(Inner<T>, data)) }.cast::<Inner<T>>();
        hazard.store(inner.cast_mut().cast(), Ordering::SeqCst);
        #[cfg(test)]
        between();
        // SAFETY: the caller keeps the allocation live across the call, and `cached` is only ever
        // accessed atomically.
        if unsafe { (*inner).cached.load(Ordering::SeqCst) } {
            return true;
        }
        crate::epoch::retract(hazard);
        false
    }

    /// Adds a count to the value at `data`, as a clone that is then forgotten would.
    ///
    /// # Safety
    ///
    /// `data` must be the address of the value in a `Shared<T>` allocation on which the caller
    /// holds a count across the call, as a counted frame of an iterator does.
    #[inline]
    pub(crate) unsafe fn increment_strong_count(data: *const T) {
        // SAFETY: the caller holds a count on the allocation at `data`, so the handle reaches a
        // live value. It is never dropped, so it takes no count of its own, and the clone adds to
        // the caller's count, which is not zero.
        let this = ManuallyDrop::new(unsafe { Shared::from_raw(data) });
        mem::forget(Shared::clone(&this));
    }

    /// The strong count, for tests.
    #[cfg(test)]
    pub(crate) fn strong_count(this: &Self) -> usize {
        this.strong().load(Ordering::Relaxed)
    }

    /// Whether `cached` is set, for tests.
    #[cfg(test)]
    pub(crate) fn held(&self) -> bool {
        // SAFETY: `self` reaches a live allocation, as every `Shared` does, and `cached` is only
        // ever accessed atomically.
        unsafe { (*self.ptr.as_ptr()).cached.load(Ordering::SeqCst) }
    }

    /// Sets `cached` as the cache's own entry does, for tests.
    #[cfg(test)]
    pub(crate) fn hold(&self) {
        // SAFETY: `self` reaches a live allocation, as every `Shared` does, and `cached` is only
        // ever accessed atomically.
        unsafe { (*self.ptr.as_ptr()).cached.store(true, Ordering::SeqCst) };
    }

    /// Clears `cached` and reads `lent` as [`Erased::release`] does, for tests.
    #[cfg(test)]
    pub(crate) fn release(&self) -> bool {
        let inner = self.ptr.as_ptr();
        // SAFETY: `self` reaches a live allocation, as every `Shared` does, and both flags are
        // only ever accessed atomically.
        unsafe {
            (*inner).cached.store(false, Ordering::SeqCst);
            (*inner).lent.load(Ordering::SeqCst)
        }
    }

    /// The strong count.
    #[inline]
    fn strong(&self) -> &AtomicUsize {
        // SAFETY: `self` reaches a live allocation, as every `Shared` does, for as long as it is
        // borrowed, and the count is an atomic, so a shared reference to it is sound beside other
        // threads.
        unsafe { &(*self.ptr.as_ptr()).strong }
    }

    /// The weak count.
    #[inline]
    fn weak(&self) -> &AtomicU32 {
        // SAFETY: `self` reaches a live allocation, as every `Shared` does, for as long as it is
        // borrowed, and the count is an atomic, so a shared reference to it is sound beside other
        // threads.
        unsafe { &(*self.ptr.as_ptr()).weak }
    }

    /// The `lent` flag.
    #[inline]
    fn lent(&self) -> &AtomicBool {
        // SAFETY: `self` reaches a live allocation, as every `Shared` does, for as long as it is
        // borrowed, and the flag is only ever accessed atomically.
        unsafe { &(*self.ptr.as_ptr()).lent }
    }

    /// A new count on the value of `this`, which waits out a count that is passing through zero.
    ///
    /// A handle that [`Shared::from_raw`] made without a count, such as the store an iterator
    /// borrows through a slot, may find the count at zero between a last owner's decrement and
    /// its parking of the value, where a plain clone would add to zero. `share` waits until the
    /// count is back, which the slot holding the value makes sure of, and then adds to it. Every
    /// other clone adds to a count its handle holds, through [`Clone`], with one atomic add.
    pub(crate) fn share(this: &Self) -> Self {
        let strong = this.strong();
        let mut count = strong.load(Ordering::Relaxed);
        let mut spins = 0u32;
        loop {
            if count == 0 {
                spins = spins.wrapping_add(1);
                if spins.is_multiple_of(64) {
                    std::thread::yield_now();
                } else {
                    hint::spin_loop();
                }
                count = strong.load(Ordering::Relaxed);
                continue;
            }
            if count > MAX_STRONG {
                process::abort();
            }
            match strong.compare_exchange_weak(
                count,
                count + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Shared {
                        ptr: this.ptr,
                        _owns: PhantomData,
                    };
                }
                Err(seen) => count = seen,
            }
        }
    }

    /// Whether `self` holds the only count and no [`Weak`] exists, as `Arc::get_mut` checks.
    ///
    /// It holds the weak count at [`LOCKED`] while it reads the strong count, so that no
    /// downgrade can come between the two reads.
    fn unique(&self) -> bool {
        if self
            .weak()
            .compare_exchange(1, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        let unique = self.strong().load(Ordering::Acquire) == 1;
        self.weak().store(1, Ordering::Release);
        unique
    }
}

impl<T: Any + Send + Sync> Shared<T> {
    /// The table that every [`Erased`] of a `T` points to.
    const KIND: Kind = Kind {
        id: TypeId::of::<T>,
        data: offset_of!(Inner<T>, data),
        cached: offset_of!(Inner<T>, cached),
        lent: offset_of!(Inner<T>, lent),
        drop: Shared::<T>::drop_erased,
        last: Inner::<T>::drop_last,
    };

    /// Lends the value of `this` to a hazard slot and returns its address, which stays valid
    /// without a count for as long as `hazard` holds it.
    ///
    /// It writes the allocation's address to `hazard` and sets `lent`, both sequentially
    /// consistent, while `this` keeps the value alive, so whoever lets go of the last count later
    /// finds the flag and the slot and parks the value. `hazard` must be an entry of a
    /// [`Hazards`](crate::epoch::Hazards) record, whose slots every free and last drop reads. The
    /// value is not changed in place meanwhile, since [`Shared::get_mut`], [`Shared::make_mut`]
    /// and [`Shared::unwrap_or_clone`] copy a value a slot holds.
    #[inline]
    pub(crate) fn lend_to(this: &Self, hazard: &AtomicPtr<()>) -> NonNull<T> {
        // SAFETY: `this` reaches a live value across the call, as every `Shared` does, so the
        // value is alive until the flag is set, and `T` is `Send + Sync`.
        unsafe { Inner::lend_to(this.ptr.as_ptr(), hazard) }
    }

    /// Erases the type of `this`, keeping its count.
    pub(crate) fn erase(this: Self) -> Erased {
        let this = ManuallyDrop::new(this);
        Erased {
            ptr: this.ptr.cast(),
            kind: &Self::KIND,
        }
    }

    /// The type's [`Kind::drop`]: what an [`Erased`] whose decrement took the count from one to
    /// zero calls.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live `Inner<T>` whose strong count the caller's release decrement
    /// just took from one to zero, whose value has not been dropped, and which the caller does not
    /// touch afterwards.
    unsafe fn drop_erased(ptr: NonNull<u8>) {
        // SAFETY: `ptr` points to an `Inner<T>` whose count the caller just took from one to zero,
        // with its value not yet dropped, which is what `drop_slow` asks.
        unsafe { Inner::<T>::drop_slow(ptr.cast().as_ptr()) };
    }
}

impl<T> Clone for Shared<T> {
    /// Adds a count to the one `self` holds.
    ///
    /// The count is never zero here: a handle that may hold no count of its own clones through
    /// [`Shared::share`] instead, and debug builds assert it. A count past [`MAX_STRONG`] aborts
    /// the process.
    #[inline]
    fn clone(&self) -> Self {
        let old = self.strong().fetch_add(1, Ordering::Relaxed);
        debug_assert_ne!(old, 0);
        if old > MAX_STRONG {
            process::abort();
        }
        Shared {
            ptr: self.ptr,
            _owns: PhantomData,
        }
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `self` reaches a live value for as long as it is borrowed, as every `Shared`
        // does, and the value changes in place only through `get_mut` and `make_mut`, which need
        // the only count, no `Weak` and no slot holding it, so never while another handle can
        // read it.
        unsafe { &(*self.ptr.as_ptr()).data }
    }
}

impl<T> Drop for Shared<T> {
    #[inline]
    fn drop(&mut self) {
        if self.strong().fetch_sub(1, Ordering::Release) == 1 {
            // SAFETY: this handle's release decrement took the count from one to zero, so this
            // call alone may drop the value, which is still alive, and a handle that holds no count
            // of its own is never dropped.
            unsafe { Inner::drop_slow(self.ptr.as_ptr()) };
        }
    }
}

impl Erased {
    /// The [`Shared`] back, keeping the count, if `T` is the erased type, and `self` if it is not.
    pub(crate) fn downcast<T: Any + Send + Sync>(self) -> Result<Shared<T>, Erased> {
        if (self.kind.id)() != TypeId::of::<T>() {
            return Err(self);
        }
        let this = ManuallyDrop::new(self);
        Ok(Shared {
            ptr: this.ptr.cast(),
            _owns: PhantomData,
        })
    }

    /// A reference to the value, if `T` is the erased type.
    pub(crate) fn downcast_ref<T: Any + Send + Sync>(&self) -> Option<&T> {
        ((self.kind.id)() == TypeId::of::<T>())
            // SAFETY: the type ids match, so `ptr` points to an `Inner<T>`, and `self` holds a
            // count, so the value is alive and unchanged for as long as `self` is borrowed.
            .then(|| unsafe { &(*self.ptr.cast::<Inner<T>>().as_ptr()).data })
    }

    /// The address of the value, without a count, as [`Shared::as_ptr`] gives it.
    #[inline]
    pub(crate) fn as_ptr(&self) -> *const () {
        // SAFETY: `kind.data` is the value's offset in the `Inner` this was erased from, so the
        // address stays inside that allocation, which the count `self` holds keeps live.
        unsafe { self.ptr.as_ptr().add(self.kind.data) }
            .cast_const()
            .cast()
    }

    /// Sets `cached`, as the cache's own entry does when it takes the node.
    pub(crate) fn hold(&self) {
        self.flag(self.kind.cached).store(true, Ordering::SeqCst);
    }

    /// Clears `cached`, then reads `lent`, both sequentially consistent, and returns it.
    ///
    /// The cache's own entry calls it before it lets go of its count. If it returns `true` a
    /// reader may have borrowed the value without a count, and the entry must retire its count to
    /// the epoch rather than drop it: a reader sets `lent` before it reads `cached` again, so
    /// either the reader sees `cached` cleared and takes no loan, or this sees `lent`.
    pub(crate) fn release(&self) -> bool {
        self.flag(self.kind.cached).store(false, Ordering::SeqCst);
        self.flag(self.kind.lent).load(Ordering::SeqCst)
    }

    /// The flag at `offset`, `kind.cached` or `kind.lent`.
    fn flag(&self, offset: usize) -> &AtomicBool {
        // SAFETY: `offset` is the offset of an `AtomicBool` in the `Inner` this was erased from,
        // and the allocation is live: `self` holds a count, or, in `release_count`, has just let
        // go of the last one without dropping the value, so the owners' weak count still holds
        // it. The flags are only ever accessed atomically.
        unsafe { &*self.ptr.as_ptr().add(offset).cast::<AtomicBool>() }
    }

    /// The strong count.
    fn strong(&self) -> &AtomicUsize {
        // SAFETY: every `Inner` is `repr(C)` and starts with its strong count, and the allocation
        // is live, as for `flag`. The count is an atomic, so a shared reference to it is sound
        // beside other threads.
        unsafe { &*self.ptr.as_ptr().cast::<AtomicUsize>() }
    }

    /// The address of the allocation, as the hazard slots hold it.
    pub(crate) fn addr(&self) -> usize {
        self.ptr.addr().get()
    }

    /// Lets go of the count, as the epoch's retired list does.
    ///
    /// If it was the last count and the value is lent, it returns the [`Dead`] value, for the
    /// caller to drop or revive once it has read the records again. If it was the last and the
    /// value was never lent, it drops the value at once.
    pub(crate) fn release_count(self) -> Option<Dead> {
        let this = ManuallyDrop::new(self);
        if this.strong().fetch_sub(1, Ordering::Release) != 1 {
            return None;
        }
        this.strong().load(Ordering::Acquire);
        let dead = Dead {
            ptr: this.ptr,
            drop: this.kind.last,
        };
        if this.flag(this.kind.lent).load(Ordering::Relaxed) {
            return Some(dead);
        }
        drop(dead);
        None
    }
}

impl Dead {
    /// The address of the allocation, as the hazard slots hold it.
    pub(crate) fn addr(&self) -> usize {
        self.ptr.addr().get()
    }

    /// Takes the count back from zero, for a value a slot still holds, to park it.
    pub(crate) fn revive(self) -> Parked {
        let this = ManuallyDrop::new(self);
        // SAFETY: a `Dead` value has not been dropped, so its allocation is live, and every
        // `Inner` starts with its strong count.
        unsafe { (*this.ptr.as_ptr().cast::<AtomicUsize>()).fetch_add(1, Ordering::Relaxed) };
        Parked {
            ptr: this.ptr,
            drop: this.drop,
        }
    }
}

impl Drop for Dead {
    fn drop(&mut self) {
        // SAFETY: a `Dead` is made by the thread whose decrement took the count to zero, after an
        // acquire, and dropped only where nothing can reach the value: by `release_count` when it
        // was never lent, or by the epoch once its reading of the records after the decrement
        // found no slot holding it. No guard can still borrow it, since the cache's count on a
        // lent value waits out every guard. `drop` is its type's final drop, run once, here.
        unsafe { (self.drop)(self.ptr) };
    }
}

impl Parked {
    /// The address of the allocation, as the hazard slots hold it.
    pub(crate) fn addr(&self) -> usize {
        self.ptr.addr().get()
    }

    /// Lets go of the parked count, returning the [`Dead`] value if it was the last, for the
    /// caller to drop or revive once it has read the records again.
    pub(crate) fn release(self) -> Option<Dead> {
        // SAFETY: a `Parked` value holds a count, so its allocation is live, and every `Inner`
        // starts with its strong count.
        let strong = unsafe { &*self.ptr.as_ptr().cast::<AtomicUsize>() };
        if strong.fetch_sub(1, Ordering::Release) != 1 {
            return None;
        }
        strong.load(Ordering::Acquire);
        Some(Dead {
            ptr: self.ptr,
            drop: self.drop,
        })
    }
}

impl Clone for Erased {
    /// Adds a count to the one `self` holds, which is never zero, as debug builds assert. A count
    /// past [`MAX_STRONG`] aborts the process.
    #[inline]
    fn clone(&self) -> Self {
        let old = self.strong().fetch_add(1, Ordering::Relaxed);
        debug_assert_ne!(old, 0);
        if old > MAX_STRONG {
            process::abort();
        }
        Erased {
            ptr: self.ptr,
            kind: self.kind,
        }
    }
}

impl Drop for Erased {
    #[inline]
    fn drop(&mut self) {
        if self.strong().fetch_sub(1, Ordering::Release) == 1 {
            // SAFETY: this handle's release decrement took the count from one to zero with the
            // value not yet dropped, and `kind.drop` is `drop_erased` for the type this was erased
            // from, which asks exactly that.
            unsafe { (self.kind.drop)(self.ptr) };
        }
    }
}

impl<T> Weak<T> {
    /// The address of the value, which may already be dropped, so only a caller that knows it is
    /// alive may read through it.
    #[inline]
    pub(crate) fn as_ptr(&self) -> *const T {
        // SAFETY: the weak count keeps the allocation live, and the projection makes no reference
        // to the value, which may already be dropped.
        unsafe { &raw const (*self.ptr.as_ptr()).data }
    }

    /// The address of the flags, for a prefetch before a borrow.
    #[inline]
    pub(crate) fn flags_addr(&self) -> usize {
        self.ptr.addr().get() + offset_of!(Inner<T>, cached)
    }

    /// A new count on the value, if its count is not zero.
    ///
    /// A parked value still has a count, so it upgrades.
    pub(crate) fn upgrade(&self) -> Option<Shared<T>> {
        // SAFETY: the weak count keeps the allocation, and so the strong count, live for as long
        // as `self` is borrowed, and the count is an atomic.
        let strong = unsafe { &(*self.ptr.as_ptr()).strong };
        let mut count = strong.load(Ordering::Relaxed);
        loop {
            if count == 0 {
                return None;
            }
            if count > MAX_STRONG {
                process::abort();
            }
            match strong.compare_exchange_weak(
                count,
                count + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(Shared {
                        ptr: self.ptr,
                        _owns: PhantomData,
                    });
                }
                Err(seen) => count = seen,
            }
        }
    }

    /// The strong count, for tests.
    #[cfg(test)]
    pub(crate) fn strong_count(&self) -> usize {
        // SAFETY: the weak count keeps the allocation, and so the strong count, live.
        unsafe { (*self.ptr.as_ptr()).strong.load(Ordering::Relaxed) }
    }
}

impl<T: Send + Sync + 'static> Weak<T> {
    /// Borrows the value under `guard`, without a count, if the cache holds it.
    ///
    /// It reads `cached`, sets `lent` if it is not set and reads `cached` again, all sequentially
    /// consistent, and lends the value only if both reads saw it set. The cache then had not yet
    /// cleared `cached`, so it held a count, and its release, which clears `cached` before it
    /// reads `lent`, sees the loan and retires that count to the epoch, which keeps it until
    /// `guard` is dropped. A reader that gets `None` borrows nothing and takes a count instead.
    #[inline]
    pub(crate) fn lend<'g>(&self, _guard: &'g Guard) -> Option<&'g T> {
        let inner = self.ptr.as_ptr();
        // SAFETY: the weak count keeps the allocation and its flags live. The value is lent only
        // if `cached` is still set after `lent` is stored, all sequentially consistent, so the
        // cache held a count then and its later load of `lent` sees the loan. It retires that
        // count, which the epoch lets go only once `guard`, pinned before that release, is
        // dropped, and the reference lives no longer than `guard`. The value is not changed
        // meanwhile, since an edit in place needs the only count.
        unsafe {
            if !(*inner).cached.load(Ordering::SeqCst) {
                return None;
            }
            if !(*inner).lent.load(Ordering::SeqCst) {
                (*inner).lent.store(true, Ordering::SeqCst);
            }
            (*inner)
                .cached
                .load(Ordering::SeqCst)
                .then(|| &(*inner).data)
        }
    }

    /// Borrows the value into a hazard slot, without a count, if the cache holds it, and returns
    /// its address, which stays valid for as long as `hazard` holds it.
    ///
    /// It reads `cached`, writes the allocation's address to `hazard`, sets `lent` if it is not
    /// set and reads `cached` again, all sequentially consistent. If `cached` is still set, the
    /// cache had not cleared it when the slot was written, so its release sees `lent` and retires
    /// the value, and the epoch frees a retired value only once no slot holds it. Otherwise it
    /// clears `hazard` through [`epoch::retract`](crate::epoch::retract), so that a free that
    /// marked the record meanwhile either finds the slot cleared or has the record's drop flush,
    /// and returns `None`, and the caller takes a count instead. `hazard` must be an entry of a
    /// [`Hazards`](crate::epoch::Hazards) record, whose slots every free reads.
    #[inline]
    pub(crate) fn lend_to(&self, hazard: &AtomicPtr<()>) -> Option<NonNull<T>> {
        let inner = self.ptr.as_ptr();
        // SAFETY: the weak count keeps the allocation and its flags live. The value is handed out
        // only if `cached` is still set after the slot and `lent` are written, all sequentially
        // consistent, so the cache held a count then, and its later load of `lent` sees the loan
        // and retires that count. A free lets a retired value go only after it has bumped the
        // epoch past the retire and then read the records, which show this slot, and a last owner
        // that later finds `lent` set reads them too and parks the value. The address of a field
        // of the live allocation is not null.
        unsafe {
            if !(*inner).cached.load(Ordering::SeqCst) {
                return None;
            }
            hazard.store(inner.cast(), Ordering::SeqCst);
            if !(*inner).lent.load(Ordering::SeqCst) {
                (*inner).lent.store(true, Ordering::SeqCst);
            }
            #[cfg(test)]
            between();
            if (*inner).cached.load(Ordering::SeqCst) {
                return Some(NonNull::new_unchecked(&raw mut (*inner).data));
            }
        }
        crate::epoch::retract(hazard);
        None
    }

    /// Lends a value already borrowed under a guard to a hazard slot, so that it stays borrowed
    /// once the guard is dropped, and returns its address.
    ///
    /// It writes the slot and sets `lent` as [`Shared::lend_to`] does. Unlike [`Weak::lend_to`] it
    /// does not read `cached`: the guard keeps the value alive until the slot is written, and a
    /// free that comes after the guard is dropped reads the slot. `hazard` must be an entry of a
    /// [`Hazards`](crate::epoch::Hazards) record.
    ///
    /// # Safety
    ///
    /// The caller must keep the value alive until this returns, as a guard does that is pinned
    /// across the call and under which the caller borrowed the value.
    #[inline]
    pub(crate) unsafe fn lend_held(&self, hazard: &AtomicPtr<()>) -> NonNull<T> {
        // SAFETY: the caller keeps the value alive until this returns, and `T` is `Send + Sync`,
        // which is what `Inner::lend_to` asks.
        unsafe { Inner::lend_to(self.ptr.as_ptr(), hazard) }
    }
}

impl<T> Clone for Weak<T> {
    /// Adds a weak count to the one `self` holds. A count past [`MAX_WEAK`] aborts the process.
    #[inline]
    fn clone(&self) -> Self {
        // SAFETY: the weak count `self` holds keeps the allocation live. `unique` locks the count
        // only while no `Weak` exists, so this never adds to `LOCKED`.
        if unsafe { (*self.ptr.as_ptr()).weak.fetch_add(1, Ordering::Relaxed) } >= MAX_WEAK {
            process::abort();
        }
        Weak { ptr: self.ptr }
    }
}

impl<T> Drop for Weak<T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: this handle holds a weak count, which it gives up once, here, and it never
        // touches the allocation again.
        unsafe { Inner::release_weak(self.ptr.as_ptr()) };
    }
}

#[cfg(test)]
thread_local! {
    /// What the next [`Weak::lend_to`] or [`Shared::lend_again`] on this thread runs between
    /// writing its slot and reading `cached` again, set by [`lend_between`].
    static BETWEEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Has the next [`Weak::lend_to`] or [`Shared::lend_again`] on this thread run `between` once it
/// has written its slot and before it reads `cached` again, so that tests can let the cache go of
/// the value and run a free inside the borrow.
#[cfg(test)]
pub(crate) fn lend_between(between: impl FnOnce() + 'static) {
    BETWEEN.with(|hook| *hook.borrow_mut() = Some(Box::new(between)));
}

/// Runs what [`lend_between`] set, if anything.
#[cfg(test)]
fn between() {
    if let Some(between) = BETWEEN.with(|hook| hook.borrow_mut().take()) {
        between();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    use super::*;
    use crate::epoch::{self, HAZARDS, Hazards, ROOT};

    #[derive(Debug)]
    struct Counted {
        value: u64,
        clones: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl Counted {
        fn new(value: u64) -> (Counted, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let clones = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let counted = Counted {
                value,
                clones: clones.clone(),
                drops: drops.clone(),
            };
            (counted, clones, drops)
        }
    }

    impl Clone for Counted {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::SeqCst);
            Counted {
                value: self.value,
                clones: self.clones.clone(),
                drops: self.drops.clone(),
            }
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn shared_keeps_the_flags_after_the_value_in_an_allocation_of_the_same_size() {
        assert_eq!(size_of::<Inner<[u64; 17]>>(), 152);
        assert_eq!(offset_of!(Inner<[u64; 17]>, data), 8);
        assert_eq!(offset_of!(Inner<[u64; 17]>, cached), 148);
    }

    #[test]
    fn shared_drops_the_value_with_its_last_owner_and_frees_with_its_last_weak() {
        let (counted, _, drops) = Counted::new(1);
        let first = Shared::new(counted);
        let second = first.clone();
        let weak = Shared::downgrade(&first);
        let spare = weak.clone();
        assert_eq!(weak.upgrade().map(|owner| owner.value), Some(1));
        drop(first);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(weak.strong_count(), 1);
        drop(second);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(weak.upgrade().is_none());
        drop(weak);
        assert!(spare.upgrade().is_none());
    }

    #[test]
    fn shared_moves_out_beside_weaks_and_clones_beside_owners() {
        let (counted, clones, drops) = Counted::new(2);
        let mut owner = Shared::new(counted);
        let weak = Shared::downgrade(&owner);
        assert!(Shared::get_mut(&mut owner).is_none());
        Shared::make_mut(&mut owner).value = 3;
        assert_eq!(clones.load(Ordering::SeqCst), 0);
        assert!(weak.upgrade().is_none());
        assert_eq!(
            Shared::get_mut(&mut owner).map(|counted| counted.value),
            Some(3)
        );
        let other = owner.clone();
        Shared::make_mut(&mut owner).value = 4;
        assert_eq!(clones.load(Ordering::SeqCst), 1);
        assert_eq!((owner.value, other.value), (4, 3));
        let weak = Shared::downgrade(&other);
        let moved = Shared::unwrap_or_clone(other);
        assert_eq!(clones.load(Ordering::SeqCst), 1);
        assert!(weak.upgrade().is_none());
        let kept = owner.clone();
        let copied = Shared::unwrap_or_clone(owner);
        assert_eq!(clones.load(Ordering::SeqCst), 2);
        assert_eq!((moved.value, copied.value, kept.value), (3, 4, 4));
        drop((moved, copied, kept));
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn shared_erases_and_round_trips_through_raw_pointers() {
        let owner = Shared::new(5u64);
        let raw = Shared::into_raw(owner.clone());
        // SAFETY: `raw` came from `into_raw`, whose count it still holds, and `owner` holds
        // another.
        unsafe { Shared::increment_strong_count(raw) };
        // SAFETY: `raw` came from `into_raw`, and this handle takes back the count it kept.
        let back = unsafe { Shared::from_raw(raw) };
        // SAFETY: `raw` came from `into_raw`, and this handle takes the count that
        // `increment_strong_count` added.
        let again = unsafe { Shared::from_raw(raw) };
        assert!(Shared::ptr_eq(&back, &owner));
        assert_eq!(Shared::strong_count(&owner), 3);
        drop((back, again));
        let erased = Shared::erase(owner);
        let erased = erased.downcast::<u32>().err().unwrap();
        assert_eq!(erased.downcast_ref::<u32>(), None);
        assert_eq!(erased.downcast_ref::<u64>(), Some(&5));
        let weak = {
            let owner = erased.clone().downcast::<u64>().ok().unwrap();
            assert_eq!(erased.as_ptr(), Shared::as_ptr(&owner).cast());
            Shared::downgrade(&owner)
        };
        let guard = epoch::pin().unwrap();
        erased.hold();
        assert_eq!(weak.lend(&guard), Some(&5));
        assert!(erased.release());
        assert_eq!(weak.lend(&guard), None);
        drop(guard);
        let owner = erased.downcast::<u64>().ok().unwrap();
        assert_eq!(*owner, 5);
        drop(owner);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn shared_flags_outlive_the_value_they_describe() {
        let (counted, _, drops) = Counted::new(6);
        let owner = Shared::new(counted);
        let weak = Shared::downgrade(&owner);
        let guard = epoch::pin().unwrap();
        assert!(weak.lend(&guard).is_none());
        owner.hold();
        assert_eq!(weak.lend(&guard).map(|counted| counted.value), Some(6));
        drop(guard);
        assert!(owner.release());
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let weak = weak.clone();
                thread::spawn(move || {
                    (0..8).all(|_| epoch::pin().is_some_and(|guard| weak.lend(&guard).is_none()))
                })
            })
            .collect();
        drop(owner);
        for reader in readers {
            assert!(reader.join().unwrap());
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let guard = epoch::pin().unwrap();
        assert!(weak.lend(&guard).is_none());
    }

    #[test]
    fn shared_a_lent_owner_waits_for_the_hazard_on_it() {
        let (counted, _, drops) = Counted::new(9);
        let owner = Shared::new(counted);
        let hazards = Hazards::new().unwrap();
        let lent = Shared::lend_to(&owner, hazards.entry(ROOT));
        drop(owner);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        epoch::flush();
        // SAFETY: a slot still holds the value, so it has not been dropped.
        assert_eq!(unsafe { lent.as_ref() }.value, 9);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(hazards);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let (counted, _, drops) = Counted::new(10);
        let owner = Shared::new(counted);
        let hazards = Hazards::new().unwrap();
        Shared::lend_to(&owner, hazards.entry(ROOT));
        drop(hazards);
        drop(owner);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn shared_a_weak_lends_to_a_hazard_only_what_is_cached() {
        let (counted, _, drops) = Counted::new(11);
        let owner = Shared::new(counted);
        let weak = Shared::downgrade(&owner);
        let hazards = Hazards::new().unwrap();
        let entry = hazards.entry(HAZARDS - 1);
        assert!(weak.lend_to(entry).is_none());
        assert!(entry.load(Ordering::SeqCst).is_null());
        let cache = Shared::erase(owner.clone());
        cache.hold();
        let lent = weak.lend_to(entry).unwrap();
        assert_eq!(entry.load(Ordering::SeqCst).addr(), cache.addr());
        drop(owner);
        assert!(cache.release());
        epoch::retire(cache, 1);
        let reader = {
            let weak = weak.clone();
            thread::spawn(move || {
                let hazards = Hazards::new().unwrap();
                weak.lend_to(hazards.entry(HAZARDS - 1)).is_none()
            })
        };
        epoch::flush();
        assert!(reader.join().unwrap());
        // SAFETY: a slot still holds the value, so it has not been dropped.
        assert_eq!(unsafe { lent.as_ref() }.value, 11);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(hazards);
        epoch::flush();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn shared_a_borrow_that_clears_its_slot_after_a_free_marked_it_flushes_at_drop() {
        for again in [false, true] {
            let (counted, _, drops) = Counted::new(17);
            let owner = Shared::new(counted);
            let weak = Shared::downgrade(&owner);
            let cache = Shared::erase(owner.clone());
            cache.hold();
            let first = Hazards::new().unwrap();
            Shared::lend_to(&owner, first.entry(ROOT));
            drop(first);
            let hazards = Hazards::new().unwrap();
            let kept = weak.clone();
            lend_between(move || {
                assert!(cache.release());
                epoch::retire(cache, 1);
                epoch::flush();
                assert_eq!(kept.strong_count(), 2);
            });
            let entry = hazards.entry(HAZARDS - 1);
            let lent = if again {
                // SAFETY: `owner` holds a count on the value across the call.
                unsafe { Shared::lend_again(Shared::as_ptr(&owner), entry) }
            } else {
                weak.lend_to(entry).is_some()
            };
            assert!(!lent, "again {again}");
            assert!(entry.load(Ordering::SeqCst).is_null(), "again {again}");
            drop(hazards);
            assert_eq!(weak.strong_count(), 1, "again {again}");
            drop(owner);
            assert_eq!(drops.load(Ordering::SeqCst), 1, "again {again}");
        }
    }

    #[test]
    fn shared_a_value_in_a_slot_is_copied_rather_than_changed() {
        let (counted, clones, drops) = Counted::new(12);
        let mut owner = Shared::new(counted);
        let hazards = Hazards::new().unwrap();
        let lent = Shared::lend_to(&owner, hazards.entry(ROOT));
        assert!(Shared::get_mut(&mut owner).is_none());
        Shared::make_mut(&mut owner).value = 13;
        assert_eq!(clones.load(Ordering::SeqCst), 1);
        let (counted, copies, taken_drops) = Counted::new(15);
        let moved = Shared::new(counted);
        let also = Shared::lend_to(&moved, hazards.entry(HAZARDS - 1));
        let taken = Shared::unwrap_or_clone(moved);
        assert_eq!(copies.load(Ordering::SeqCst), 1);
        // SAFETY: a slot still holds each value, so neither has been dropped.
        let read = unsafe { (lent.as_ref().value, also.as_ref().value) };
        assert_eq!(read, (12, 15));
        assert_eq!((owner.value, taken.value), (13, 15));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(taken_drops.load(Ordering::SeqCst), 0);
        drop(hazards);
        epoch::flush();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(taken_drops.load(Ordering::SeqCst), 1);
        let hazards = Hazards::new().unwrap();
        Shared::lend_to(&owner, hazards.entry(ROOT));
        drop(hazards);
        let before = Shared::as_ptr(&owner);
        Shared::make_mut(&mut owner).value = 16;
        assert_eq!(Shared::as_ptr(&owner), before);
        assert_eq!(clones.load(Ordering::SeqCst), 1);
        assert!(Shared::get_mut(&mut owner).is_some());
    }

    #[test]
    fn shared_a_lent_owner_let_go_beside_its_slot_is_never_left_waiting() {
        let rounds = if cfg!(miri) { 4 } else { 400 };
        for round in 0..rounds {
            let (counted, _, drops) = Counted::new(round);
            let owner = Shared::new(counted);
            let kept = Shared::downgrade(&owner);
            let hazards = Hazards::new().unwrap();
            Shared::lend_to(&owner, hazards.entry(ROOT));
            let start = Arc::new(std::sync::Barrier::new(2));
            let other = {
                let start = start.clone();
                thread::spawn(move || {
                    start.wait();
                    drop(hazards);
                })
            };
            start.wait();
            drop(owner);
            other.join().unwrap();
            let begun = std::time::Instant::now();
            while epoch::waiting(kept.ptr.addr().get()) && begun.elapsed().as_secs() < 10 {
                thread::yield_now();
            }
            assert!(!epoch::waiting(kept.ptr.addr().get()), "round {round}");
            while drops.load(Ordering::SeqCst) == 0 && begun.elapsed().as_secs() < 10 {
                thread::yield_now();
            }
            assert_eq!(drops.load(Ordering::SeqCst), 1, "round {round}");
        }
    }

    #[test]
    fn shared_a_share_through_a_slot_never_brings_a_value_back() {
        struct Sent(*const Counted);

        // SAFETY: the pointer is only read on the thread it moves to, while a slot holds the
        // value it points to, and `Counted` is `Send` and `Sync`.
        unsafe impl Send for Sent {}

        impl Sent {
            fn into_ptr(self) -> *const Counted {
                self.0
            }
        }

        let rounds = if cfg!(miri) { 4 } else { 400 };
        for round in 0..rounds {
            let (counted, _, drops) = Counted::new(round);
            let owner = Shared::new(counted);
            let hazards = Hazards::new().unwrap();
            let lent = Sent(Shared::lend_to(&owner, hazards.entry(ROOT)).as_ptr());
            let start = Arc::new(std::sync::Barrier::new(2));
            let other = {
                let start = start.clone();
                thread::spawn(move || {
                    let ptr = lent.into_ptr();
                    // SAFETY: the slot holds the value, so the pointer reaches a live value, and
                    // the handle made from it takes no count and is never dropped.
                    let borrowed = ManuallyDrop::new(unsafe { Shared::from_raw(ptr) });
                    start.wait();
                    let copy = Shared::share(&borrowed);
                    assert_eq!(copy.value, round);
                    drop(copy);
                    drop(hazards);
                })
            };
            start.wait();
            drop(owner);
            other.join().unwrap();
            epoch::flush();
            assert_eq!(drops.load(Ordering::SeqCst), 1, "round {round}");
        }
    }
}
