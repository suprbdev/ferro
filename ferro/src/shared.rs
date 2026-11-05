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

const MAX_STRONG: usize = isize::MAX as usize;
const LOCKED: u32 = u32::MAX;
const MAX_WEAK: u32 = LOCKED - 1;

#[repr(C)]
struct Inner<T> {
    strong: AtomicUsize,
    data: T,
    weak: AtomicU32,
    cached: AtomicBool,
    lent: AtomicBool,
}

pub struct Shared<T> {
    ptr: NonNull<Inner<T>>,
    _owns: PhantomData<Inner<T>>,
}

pub struct Weak<T> {
    ptr: NonNull<Inner<T>>,
}

pub struct Erased {
    ptr: NonNull<u8>,
    kind: &'static Kind,
}

struct Kind {
    id: fn() -> TypeId,
    data: usize,
    cached: usize,
    lent: usize,
    drop: unsafe fn(NonNull<u8>),
    last: unsafe fn(NonNull<u8>),
}

pub struct Dead {
    ptr: NonNull<u8>,
    drop: unsafe fn(NonNull<u8>),
}

pub struct Parked {
    ptr: NonNull<u8>,
    drop: unsafe fn(NonNull<u8>),
}

unsafe impl<T: Send + Sync> Send for Shared<T> {}

unsafe impl<T: Send + Sync> Sync for Shared<T> {}

unsafe impl<T: Send + Sync> Send for Weak<T> {}

unsafe impl<T: Send + Sync> Sync for Weak<T> {}

unsafe impl Send for Erased {}

unsafe impl Sync for Erased {}

// SAFETY: a `Dead` value is made only for an `Erased`, whose `T` is `Send + Sync`, or for a value
// whose `lent` flag is set, which only lending functions bounded by `T: Send + Sync` do. Dropping
// it on another thread is then what dropping the last owner there would do.
unsafe impl Send for Dead {}

// SAFETY: a `Parked` value holds one count of a value whose `lent` flag is set, which only lending
// functions bounded by `T: Send + Sync` do, and such a count may be released on any thread.
unsafe impl Send for Parked {}

impl<T> Inner<T> {
    #[inline]
    unsafe fn release_weak(inner: *mut Inner<T>) {
        if unsafe { (*inner).weak.fetch_sub(1, Ordering::Release) } == 1 {
            unsafe { (*inner).weak.load(Ordering::Acquire) };
            unsafe { dealloc(inner.cast(), Layout::new::<Inner<T>>()) };
        }
    }

    #[inline]
    unsafe fn lend_to(inner: *mut Inner<T>, hazard: &AtomicPtr<()>) -> NonNull<T> {
        hazard.store(inner.cast(), Ordering::SeqCst);
        // SAFETY: the caller keeps the allocation alive until the flag is set.
        let lent = unsafe { &(*inner).lent };
        if !lent.load(Ordering::Relaxed) {
            lent.store(true, Ordering::SeqCst);
        }
        // SAFETY: `inner` is a live allocation, so the address of its value is not null.
        unsafe { NonNull::new_unchecked(&raw mut (*inner).data) }
    }

    #[inline(never)]
    unsafe fn drop_slow(inner: *mut Inner<T>) {
        // SAFETY: the count reached zero but the allocation is not freed until the value is
        // dropped, which only this call may do.
        unsafe { (*inner).strong.load(Ordering::Acquire) };
        // SAFETY: as above, the allocation is live.
        if unsafe { (*inner).lent.load(Ordering::Relaxed) }
            && crate::epoch::hazarded_at(inner.addr())
        {
            // SAFETY: a slot holds the value, so it is parked rather than dropped.
            unsafe { Inner::park(inner) };
        } else {
            // SAFETY: the count is zero and no slot holds the value, so nothing can reach it.
            unsafe { Inner::drop_final(inner) };
        }
    }

    unsafe fn drop_final(inner: *mut Inner<T>) {
        // SAFETY: the caller guarantees nothing reaches the value again.
        unsafe { ptr::drop_in_place(&raw mut (*inner).data) };
        // SAFETY: the owners' weak count is still held and is let go once.
        unsafe { Inner::release_weak(inner) };
    }

    #[cold]
    unsafe fn park(inner: *mut Inner<T>) {
        // SAFETY: the value has not been dropped, so its allocation is live.
        unsafe { (*inner).strong.fetch_add(1, Ordering::Relaxed) };
        let parked = Parked {
            // SAFETY: `inner` points to a live allocation, so it is not null.
            ptr: unsafe { NonNull::new_unchecked(inner.cast()) },
            drop: Inner::<T>::drop_last,
        };
        crate::epoch::park(parked, size_of::<Inner<T>>());
    }

    unsafe fn drop_last(ptr: NonNull<u8>) {
        let inner = ptr.cast::<Inner<T>>().as_ptr();
        // SAFETY: the caller guarantees the allocation is live and unreachable.
        unsafe { (*inner).strong.load(Ordering::Acquire) };
        // SAFETY: as above.
        unsafe { Inner::drop_final(inner) };
    }
}

impl<T> Shared<T> {
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

    pub(crate) fn get_mut(this: &mut Self) -> Option<&mut T> {
        if !this.unique() || this.lent().load(Ordering::Relaxed) && Shared::in_slot(this) {
            return None;
        }
        // SAFETY: this is the only count, no weak or slot reaches the value, and `this` stays
        // borrowed mutably for as long as the reference lives.
        Some(unsafe { &mut (*this.ptr.as_ptr()).data })
    }

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
        // SAFETY: `this` now holds the only count of a value no weak or slot reaches, and stays
        // borrowed mutably for as long as the reference lives.
        unsafe { &mut (*this.ptr.as_ptr()).data }
    }

    #[cold]
    #[inline(never)]
    fn in_slot(this: &Self) -> bool {
        if crate::epoch::hazarded_at(this.ptr.addr().get()) {
            return true;
        }
        this.lent().store(false, Ordering::Relaxed);
        false
    }

    #[cold]
    #[inline(never)]
    fn clone_out(this: &mut Self)
    where
        T: Clone,
    {
        *this = Shared::new((**this).clone());
    }

    #[cold]
    #[inline(never)]
    fn move_out(this: &mut Self) {
        let data = unsafe { ptr::read(&raw const (*this.ptr.as_ptr()).data) };
        let old = ManuallyDrop::new(mem::replace(this, Shared::new(data)));
        drop(Weak { ptr: old.ptr });
    }

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
        // SAFETY: the count went from one to zero here and no slot holds the value, so nothing else
        // reads it, and the allocation is let go without dropping the value read out.
        let data = unsafe { ptr::read(&raw const (*this.ptr.as_ptr()).data) };
        drop(Weak { ptr: this.ptr });
        data
    }

    #[inline]
    pub(crate) fn as_ptr(this: &Self) -> *const T {
        unsafe { &raw const (*this.ptr.as_ptr()).data }
    }

    #[inline]
    pub(crate) fn ptr_eq(a: &Self, b: &Self) -> bool {
        a.ptr == b.ptr
    }

    #[inline]
    pub(crate) fn into_raw(this: Self) -> *const T {
        let this = ManuallyDrop::new(this);
        Shared::as_ptr(&this)
    }

    #[inline]
    pub(crate) unsafe fn from_raw(data: *const T) -> Self {
        let inner = unsafe { data.byte_sub(offset_of!(Inner<T>, data)) }.cast::<Inner<T>>();
        Shared {
            ptr: unsafe { NonNull::new_unchecked(inner.cast_mut()) },
            _owns: PhantomData,
        }
    }

    #[inline]
    pub(crate) unsafe fn lend_again(data: *const T, hazard: &AtomicPtr<()>) -> bool {
        // SAFETY: `data` points to the value of a live `Inner`, which starts that far before it.
        let inner = unsafe { data.byte_sub(offset_of!(Inner<T>, data)) }.cast::<Inner<T>>();
        hazard.store(inner.cast_mut().cast(), Ordering::SeqCst);
        // SAFETY: the caller keeps the allocation alive.
        if unsafe { (*inner).cached.load(Ordering::SeqCst) } {
            return true;
        }
        hazard.store(ptr::null_mut(), Ordering::Release);
        false
    }

    #[inline]
    pub(crate) unsafe fn increment_strong_count(data: *const T) {
        let this = ManuallyDrop::new(unsafe { Shared::from_raw(data) });
        mem::forget(Shared::clone(&this));
    }

    #[cfg(test)]
    pub(crate) fn strong_count(this: &Self) -> usize {
        this.strong().load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn held(&self) -> bool {
        unsafe { (*self.ptr.as_ptr()).cached.load(Ordering::SeqCst) }
    }

    #[cfg(test)]
    pub(crate) fn hold(&self) {
        unsafe { (*self.ptr.as_ptr()).cached.store(true, Ordering::SeqCst) };
    }

    #[cfg(test)]
    pub(crate) fn release(&self) -> bool {
        let inner = self.ptr.as_ptr();
        unsafe {
            (*inner).cached.store(false, Ordering::SeqCst);
            (*inner).lent.load(Ordering::SeqCst)
        }
    }

    #[inline]
    fn strong(&self) -> &AtomicUsize {
        unsafe { &(*self.ptr.as_ptr()).strong }
    }

    #[inline]
    fn weak(&self) -> &AtomicU32 {
        unsafe { &(*self.ptr.as_ptr()).weak }
    }

    #[inline]
    fn lent(&self) -> &AtomicBool {
        // SAFETY: a `Shared` holds a count, so its allocation is live.
        unsafe { &(*self.ptr.as_ptr()).lent }
    }

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
    const KIND: Kind = Kind {
        id: TypeId::of::<T>,
        data: offset_of!(Inner<T>, data),
        cached: offset_of!(Inner<T>, cached),
        lent: offset_of!(Inner<T>, lent),
        drop: Shared::<T>::drop_erased,
        last: Inner::<T>::drop_last,
    };

    #[inline]
    pub(crate) fn lend_to(this: &Self, hazard: &AtomicPtr<()>) -> NonNull<T> {
        // SAFETY: `this` holds a count, so the value is alive while the flag is set.
        unsafe { Inner::lend_to(this.ptr.as_ptr(), hazard) }
    }

    pub(crate) fn erase(this: Self) -> Erased {
        let this = ManuallyDrop::new(this);
        Erased {
            ptr: this.ptr.cast(),
            kind: &Self::KIND,
        }
    }

    unsafe fn drop_erased(ptr: NonNull<u8>) {
        unsafe { Inner::<T>::drop_slow(ptr.cast().as_ptr()) };
    }
}

impl<T> Clone for Shared<T> {
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
        unsafe { &(*self.ptr.as_ptr()).data }
    }
}

impl<T> Drop for Shared<T> {
    #[inline]
    fn drop(&mut self) {
        if self.strong().fetch_sub(1, Ordering::Release) == 1 {
            unsafe { Inner::drop_slow(self.ptr.as_ptr()) };
        }
    }
}

impl Erased {
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

    pub(crate) fn downcast_ref<T: Any + Send + Sync>(&self) -> Option<&T> {
        ((self.kind.id)() == TypeId::of::<T>())
            .then(|| unsafe { &(*self.ptr.cast::<Inner<T>>().as_ptr()).data })
    }

    #[inline]
    pub(crate) fn as_ptr(&self) -> *const () {
        unsafe { self.ptr.as_ptr().add(self.kind.data) }
            .cast_const()
            .cast()
    }

    pub(crate) fn hold(&self) {
        self.flag(self.kind.cached).store(true, Ordering::SeqCst);
    }

    pub(crate) fn release(&self) -> bool {
        self.flag(self.kind.cached).store(false, Ordering::SeqCst);
        self.flag(self.kind.lent).load(Ordering::SeqCst)
    }

    fn flag(&self, offset: usize) -> &AtomicBool {
        unsafe { &*self.ptr.as_ptr().add(offset).cast::<AtomicBool>() }
    }

    fn strong(&self) -> &AtomicUsize {
        unsafe { &*self.ptr.as_ptr().cast::<AtomicUsize>() }
    }

    pub(crate) fn addr(&self) -> usize {
        self.ptr.addr().get()
    }

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
    pub(crate) fn addr(&self) -> usize {
        self.ptr.addr().get()
    }

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
        // SAFETY: a `Dead` value's count is zero and it is dropped only once no slot holds it, so
        // nothing reaches it, and `drop` is its type's final drop.
        unsafe { (self.drop)(self.ptr) };
    }
}

impl Parked {
    pub(crate) fn addr(&self) -> usize {
        self.ptr.addr().get()
    }

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
            unsafe { (self.kind.drop)(self.ptr) };
        }
    }
}

impl<T> Weak<T> {
    #[inline]
    pub(crate) fn as_ptr(&self) -> *const T {
        unsafe { &raw const (*self.ptr.as_ptr()).data }
    }

    #[inline]
    pub(crate) fn flags_addr(&self) -> usize {
        self.ptr.addr().get() + offset_of!(Inner<T>, cached)
    }

    pub(crate) fn upgrade(&self) -> Option<Shared<T>> {
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

    #[cfg(test)]
    pub(crate) fn strong_count(&self) -> usize {
        unsafe { (*self.ptr.as_ptr()).strong.load(Ordering::Relaxed) }
    }
}

impl<T: Send + Sync + 'static> Weak<T> {
    #[inline]
    pub(crate) fn lend<'g>(&self, _guard: &'g Guard) -> Option<&'g T> {
        let inner = self.ptr.as_ptr();
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

    #[inline]
    pub(crate) fn lend_to(&self, hazard: &AtomicPtr<()>) -> Option<NonNull<T>> {
        let inner = self.ptr.as_ptr();
        // SAFETY: the weak count keeps the allocation and its flags alive. The value is handed
        // out only if `cached` is still set after the slot and `lent` are written, so the cache
        // held a count then, and whoever lets that count go reads `lent` and finds the slot.
        unsafe {
            if !(*inner).cached.load(Ordering::SeqCst) {
                return None;
            }
            hazard.store(inner.cast(), Ordering::SeqCst);
            if !(*inner).lent.load(Ordering::SeqCst) {
                (*inner).lent.store(true, Ordering::SeqCst);
            }
            if (*inner).cached.load(Ordering::SeqCst) {
                return Some(NonNull::new_unchecked(&raw mut (*inner).data));
            }
        }
        hazard.store(ptr::null_mut(), Ordering::Release);
        None
    }

    #[inline]
    pub(crate) unsafe fn lend_held(&self, hazard: &AtomicPtr<()>) -> NonNull<T> {
        // SAFETY: the caller keeps the value alive until the flag is set.
        unsafe { Inner::lend_to(self.ptr.as_ptr(), hazard) }
    }
}

impl<T> Clone for Weak<T> {
    #[inline]
    fn clone(&self) -> Self {
        if unsafe { (*self.ptr.as_ptr()).weak.fetch_add(1, Ordering::Relaxed) } >= MAX_WEAK {
            process::abort();
        }
        Weak { ptr: self.ptr }
    }
}

impl<T> Drop for Weak<T> {
    #[inline]
    fn drop(&mut self) {
        unsafe { Inner::release_weak(self.ptr.as_ptr()) };
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
        unsafe { Shared::increment_strong_count(raw) };
        let back = unsafe { Shared::from_raw(raw) };
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
    fn shared_a_lent_owner_let_go_beside_its_slot_is_dropped_by_one_of_them() {
        let rounds = if cfg!(miri) { 4 } else { 400 };
        for round in 0..rounds {
            let (counted, _, drops) = Counted::new(round);
            let owner = Shared::new(counted);
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
