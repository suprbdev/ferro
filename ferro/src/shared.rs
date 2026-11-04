use std::alloc::{Layout, dealloc};
use std::any::{Any, TypeId};
use std::hint;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop, offset_of};
use std::ops::Deref;
use std::process;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

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
}

unsafe impl<T: Send + Sync> Send for Shared<T> {}

unsafe impl<T: Send + Sync> Sync for Shared<T> {}

unsafe impl<T: Send + Sync> Send for Weak<T> {}

unsafe impl<T: Send + Sync> Sync for Weak<T> {}

unsafe impl Send for Erased {}

unsafe impl Sync for Erased {}

impl<T> Inner<T> {
    #[inline]
    unsafe fn release_weak(inner: *mut Inner<T>) {
        if unsafe { (*inner).weak.fetch_sub(1, Ordering::Release) } == 1 {
            unsafe { (*inner).weak.load(Ordering::Acquire) };
            unsafe { dealloc(inner.cast(), Layout::new::<Inner<T>>()) };
        }
    }

    #[inline(never)]
    unsafe fn drop_slow(inner: *mut Inner<T>) {
        unsafe { (*inner).strong.load(Ordering::Acquire) };
        unsafe { ptr::drop_in_place(&raw mut (*inner).data) };
        unsafe { Inner::release_weak(inner) };
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
        this.unique()
            .then(|| unsafe { &mut (*this.ptr.as_ptr()).data })
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
        } else if this.weak().load(Ordering::Relaxed) != 1 {
            Shared::move_out(this);
        } else {
            this.strong().store(1, Ordering::Release);
        }
        unsafe { &mut (*this.ptr.as_ptr()).data }
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
        let this = ManuallyDrop::new(this);
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
    };

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
        if self.strong().fetch_add(1, Ordering::Relaxed) > MAX_STRONG {
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
}

impl Clone for Erased {
    #[inline]
    fn clone(&self) -> Self {
        if self.strong().fetch_add(1, Ordering::Relaxed) > MAX_STRONG {
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
    use crate::epoch;

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
}
