//! Room on the stack for ferro's recursion over nested values.
//!
//! Encoding, decoding, visiting and pairing a value recurse once for each level it nests, up to
//! [`MAX_DEPTH`](crate::codec::MAX_DEPTH) levels, and each level takes from under a hundred bytes
//! to a few KB of stack, so a value at the limit needs far more stack than the 2 MiB a spawned
//! thread has. Each level runs through [`with_room`], which runs it where it is while at least
//! [`RED_ZONE`] bytes of stack are left below the caller, and otherwise on a segment of 16 MiB
//! mapped for the thread above a guard page, whose pages the system provides as they are first
//! touched. The levels below check against that segment in turn and move to another one when it
//! runs short, so the recursion goes as deep as the value does, and the memory it takes is the
//! stack its levels use.
//!
//! Cloning and dropping a value recurse through it too, in code that Rust generates and that
//! checks for nothing, so the clones of a root that commits and merges make run through
//! [`roomy`], which moves them to a segment unless [`DEEP_ROOM`] bytes are left, enough to clone a
//! value at the limit. The closures of commits and merges run where they are called.
//!
//! A segment is the thread's to reuse once the work on it returns, and never before, however the
//! work on its segments interleaves: a stackful coroutine may leave work suspended on one segment
//! while another coroutine on the same thread runs on a second, and either may finish first. A
//! thread keeps one idle segment, so a value whose branches cross the edge of a stack many times
//! maps it once, and unmaps the others. A segment counts as deep once a check on it finds the
//! stack below its top 256 KiB, or, for a clone, which checks nothing, once the page below them
//! has been touched; when a deep segment falls idle, the pages below its top 256 KiB are given
//! back to the system, so a thread that went deep once keeps at most that much of it.
//!
//! The check compares the address of a local with the bounds of the stack the thread is on: its
//! own, whose bounds it asks the operating system for the first time it runs short, or a segment.
//! Segments are switched to with [`psm`] on Linux, Android, macOS and iOS. On Windows, where the
//! bounds are known but ferro does not switch stacks, a level that finds no room is refused
//! instead, and so is one on a stack the thread did not start with, such as a fiber's. A stack
//! other than the thread's own and its busy segments never counts as having room, so a recursion
//! on one, such as a coroutine's, moves to a segment at its first level. Where the bounds are not
//! known and there are no segments, as on the BSDs and under Miri, nothing is checked, and
//! [`CHECKED`] keeps the limit at 128 levels there.
//!
//! [Nesting depth](https://github.com/suprbdev/ferro/blob/main/docs/internals/storage.md#nesting-depth)
//! describes what each level takes.

use std::cell::Cell;

/// The stack each level of a recursion may take before the next level checks for room: a level
/// that finds less than this left below it moves to a segment.
pub(crate) const RED_ZONE: usize = 128 << 10;

/// The stack [`roomy`] makes sure of: enough to clone or drop a value at
/// [`MAX_DEPTH`](crate::codec::MAX_DEPTH) at up to 3 KB a level, more than the code Rust generates
/// for both takes in a debug build for a tree of `BTreeMap` children that each hold a table.
pub(crate) const DEEP_ROOM: usize = 12 << 20;

/// Whether ferro checks for room on this target, which it does where it learns the bounds of a
/// thread's stack or can move to a segment. Where it does neither, a value nests at most 128
/// levels, as it did before the checks.
pub(crate) const CHECKED: bool = os::KNOWN || segments::SWITCHING;

/// The range of addresses where the stack a thread is on has room for one more level: from
/// `low` up `span` bytes. `low` is [`RED_ZONE`] above the bottom of the stack, or, on a segment
/// that is not deep, the lowest address of the part a shallow use takes.
#[derive(Clone, Copy)]
struct Room {
    low: usize,
    span: usize,
}

impl Room {
    /// Room nowhere, so that every check goes the slow way.
    const NONE: Room = Room { low: 0, span: 0 };

    /// Room everywhere, for a target that checks nothing.
    const ANY: Room = Room {
        low: 0,
        span: usize::MAX,
    };

    /// The room on a stack whose lowest address is `bottom` and that ends below `top`.
    fn of(bottom: usize, top: usize) -> Room {
        let low = bottom.saturating_add(RED_ZONE);
        Room {
            low,
            span: top.saturating_sub(low),
        }
    }
}

thread_local! {
    static ROOM: Cell<Room> = const { Cell::new(Room::NONE) };
    static STACK: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

/// Runs `f`, on a segment of its own if the stack has less than [`RED_ZONE`] bytes left, and
/// returns what it returns, or `None` without running it if the stack is short and no segment can
/// be had: on Windows, or when a segment cannot be mapped.
///
/// A panic in `f` unwinds on through the caller.
#[inline(always)]
pub(crate) fn with_room<R>(f: impl FnOnce() -> R) -> Option<R> {
    if room() { Some(f()) } else { elsewhere(f) }
}

/// Whether the stack the thread is on has [`RED_ZONE`] bytes left below the caller, so that one
/// more level can run where it is; [`elsewhere`] runs it when not.
#[inline(always)]
pub(crate) fn room() -> bool {
    has_room(0)
}

/// Runs `f`, on a segment of its own if the stack has less than [`DEEP_ROOM`] bytes left, or where
/// it is if no segment can be had, and returns what it returns.
///
/// It is for ferro's own clones of values, never for a caller's closure, whose stack overflow
/// should be reported as one, which it is not on a segment.
///
/// A panic in `f` unwinds on through the caller.
pub(crate) fn roomy<R>(f: impl FnOnce() -> R) -> R {
    if has_room(DEEP_ROOM - RED_ZONE) {
        return f();
    }
    let mut f = Some(f);
    let mut out = None;
    run_elsewhere(DEEP_ROOM - RED_ZONE, false, &mut || {
        out = f.take().map(|f| f())
    });
    match (out, f) {
        (Some(out), _) => out,
        (None, Some(f)) => f(),
        (None, None) => unreachable!("a closure that ran returned"),
    }
}

/// Whether the stack the thread is on has `more` bytes beyond [`RED_ZONE`] left below the caller.
#[inline(always)]
fn has_room(more: usize) -> bool {
    let room = ROOM.get();
    let above = here().wrapping_sub(room.low);
    above < room.span && above >= more
}

/// The address of a local of the caller's frame, which stands for where the stack is.
#[inline(always)]
fn here() -> usize {
    let marker = 0u8;
    std::ptr::addr_of!(marker).addr()
}

/// Runs `f` once [`room`] found the stack short: where it is, if the stack it is on proves to have
/// room after all, and otherwise on a segment. Returns what `f` returned, or `None` without
/// running it if no segment can be had.
#[cold]
#[inline(never)]
pub(crate) fn elsewhere<R>(f: impl FnOnce() -> R) -> Option<R> {
    let mut f = Some(f);
    let mut out = None;
    run_elsewhere(0, true, &mut || out = f.take().map(|f| f()));
    out
}

/// The bounds of the thread's own stack, asked of the system the first time, or `(0, 0)` if they
/// cannot be learned.
fn thread_stack() -> (usize, usize) {
    if let Some(bounds) = STACK.get() {
        return bounds;
    }
    let bounds = os::bounds().unwrap_or((0, 0));
    STACK.set(Some(bounds));
    bounds
}

/// The room of the stack that `at` lies on: the thread's own, or a segment that work on this
/// thread is running on, or `None` for any other stack.
fn room_at(at: usize) -> Option<Room> {
    let (bottom, top) = thread_stack();
    if bottom < top {
        if (bottom..top).contains(&at) {
            return Some(Room::of(bottom, top));
        }
    } else if !segments::SWITCHING {
        return Some(Room::ANY);
    }
    segments::busy_room_at(at)
}

/// Runs `f` where it is if the stack it is on has `more` bytes beyond [`RED_ZONE`] left, which
/// [`ROOM`] may not have said because it named another stack, or because the segment it is on was
/// not yet deep, and otherwise on a segment. `checked` says whether the work in `f` checks for
/// room as it goes. Returns whether `f` ran, which it does not where no segment can be had.
fn run_elsewhere(more: usize, checked: bool, f: &mut dyn FnMut()) -> bool {
    let at = here();
    if let Some(room) = room_at(at) {
        ROOM.set(room);
        if has_room(more) {
            f();
            return true;
        }
    }
    if let Some(room) = segments::deepen(at) {
        ROOM.set(room);
        if has_room(more) {
            f();
            return true;
        }
    }
    segments::run(checked, f)
}

#[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
mod os {
    use std::mem::MaybeUninit;

    /// Whether [`bounds`] knows the stack's bounds on this target.
    pub(super) const KNOWN: bool = true;

    /// The lowest and one past the highest address of the calling thread's stack.
    pub(super) fn bounds() -> Option<(usize, usize)> {
        let mut attr = MaybeUninit::<libc::pthread_attr_t>::zeroed();
        // SAFETY: `pthread_getattr_np` fills the zeroed attribute object for the calling thread,
        // which is destroyed below once it succeeded.
        if unsafe { libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) } != 0 {
            return None;
        }
        let mut addr = std::ptr::null_mut();
        let mut size = 0;
        // SAFETY: the attribute object was initialized by `pthread_getattr_np` above, and the
        // out-pointers are to locals.
        let read = unsafe { libc::pthread_attr_getstack(attr.as_ptr(), &mut addr, &mut size) };
        // SAFETY: the attribute object is initialized and destroyed only here.
        unsafe { libc::pthread_attr_destroy(attr.as_mut_ptr()) };
        let low = addr.addr();
        (read == 0).then(|| (low, low.saturating_add(size)))
    }
}

#[cfg(all(target_vendor = "apple", not(miri)))]
mod os {
    /// Whether [`bounds`] knows the stack's bounds on this target.
    pub(super) const KNOWN: bool = true;

    /// The lowest and one past the highest address of the calling thread's stack.
    pub(super) fn bounds() -> Option<(usize, usize)> {
        // SAFETY: both calls only read the description of the calling thread.
        let (high, size) = unsafe {
            let thread = libc::pthread_self();
            (
                libc::pthread_get_stackaddr_np(thread).addr(),
                libc::pthread_get_stacksize_np(thread),
            )
        };
        Some((high.checked_sub(size)?, high))
    }
}

#[cfg(all(windows, not(miri)))]
mod os {
    use windows_sys::Win32::System::Threading::GetCurrentThreadStackLimits;

    /// Whether [`bounds`] knows the stack's bounds on this target.
    pub(super) const KNOWN: bool = true;

    /// The lowest and one past the highest address of the calling thread's stack.
    pub(super) fn bounds() -> Option<(usize, usize)> {
        let (mut low, mut high) = (0, 0);
        // SAFETY: the call writes the calling thread's stack limits to the two locals.
        unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
        (high > low).then_some((low, high))
    }
}

#[cfg(any(
    miri,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    ))
))]
mod os {
    /// Whether [`bounds`] knows the stack's bounds on this target.
    pub(super) const KNOWN: bool = false;

    /// The bounds of the calling thread's stack, which are not known on this target.
    pub(super) fn bounds() -> Option<(usize, usize)> {
        None
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
psm::psm_stack_manipulation! {
    yes {
        mod segments {
            use std::any::Any;
            use std::cell::{Cell, RefCell};
            use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

            use super::{ROOM, Room, here, room_at};

            /// Whether a recursion can move to a segment on this target.
            pub(super) const SWITCHING: bool = true;

            /// The usable bytes of each segment, more than [`DEEP_ROOM`](super::DEEP_ROOM).
            const SEGMENT_BYTES: usize = 16 << 20;

            /// The top of a segment that shallow work stays within, whose pages a segment keeps
            /// when it falls idle; work that goes below it makes the segment deep.
            const SHALLOW: usize = 256 << 10;

            /// How many idle segments a thread keeps.
            const KEPT: usize = 1;

            #[cfg(any(target_os = "linux", target_os = "android"))]
            const MAP_FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_STACK;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            const MAP_FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANON;

            #[cfg(any(target_os = "linux", target_os = "android"))]
            const GIVE_BACK: libc::c_int = libc::MADV_DONTNEED;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            const GIVE_BACK: libc::c_int = libc::MADV_FREE;

            thread_local! {
                static SEGMENTS: Segments = const { Segments(RefCell::new(Vec::new())) };
            }

            /// The segments a thread has mapped, busy or idle.
            struct Segments(RefCell<Vec<Segment>>);

            impl Drop for Segments {
                fn drop(&mut self) {
                    for segment in self.0.get_mut().drain(..) {
                        if segment.busy.get() {
                            std::mem::forget(segment);
                        }
                    }
                }
            }

            /// A stack segment: [`SEGMENT_BYTES`] of memory mapped for reading and writing above
            /// a guard page that is mapped for neither, unmapped when dropped.
            struct Segment {
                map: *mut libc::c_void,
                len: usize,
                page: usize,
                busy: Cell<bool>,
                deep: Cell<bool>,
            }

            impl Segment {
                /// Maps a segment, or returns `None` if the system refuses.
                fn new() -> Option<Segment> {
                    // SAFETY: `sysconf` only reads a system setting.
                    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
                        .ok()
                        .filter(|page| page.is_power_of_two() && *page < SHALLOW)?;
                    let len = SEGMENT_BYTES.next_multiple_of(page) + page;
                    // SAFETY: a private anonymous mapping at an address the system picks aliases
                    // no memory, and its result is checked before use.
                    let map = unsafe {
                        libc::mmap(std::ptr::null_mut(), len, libc::PROT_NONE, MAP_FLAGS, -1, 0)
                    };
                    if map == libc::MAP_FAILED {
                        return None;
                    }
                    let segment = Segment {
                        map,
                        len,
                        page,
                        busy: Cell::new(false),
                        deep: Cell::new(false),
                    };
                    // SAFETY: the range starts one page into the mapping just made and runs to
                    // its end.
                    let opened = unsafe {
                        libc::mprotect(
                            map.byte_add(page),
                            len - page,
                            libc::PROT_READ | libc::PROT_WRITE,
                        )
                    };
                    no_huge_pages(map, len);
                    (opened == 0).then_some(segment)
                }

                /// The lowest address of the segment's usable part.
                fn bottom(&self) -> usize {
                    self.map.addr() + self.page
                }

                /// One past the highest address of the segment.
                fn top(&self) -> usize {
                    self.map.addr() + self.len
                }

                /// Whether `at` lies on the segment.
                fn holds(&self, at: usize) -> bool {
                    (self.bottom()..self.top()).contains(&at)
                }

                /// The segment's room: all of it once deep, and its top [`SHALLOW`] bytes before.
                fn room(&self) -> Room {
                    match self.deep.get() {
                        true => Room::of(self.bottom(), self.top()),
                        false => Room {
                            low: self.top() - SHALLOW,
                            span: SHALLOW,
                        },
                    }
                }

                /// Whether the page just below the segment's top [`SHALLOW`] bytes is in memory,
                /// which it is once work on the segment went below them, as every page above the
                /// deepest frame is written to on the way down.
                fn touched_deep(&self) -> bool {
                    let mut held = 0u8;
                    let at = self.top() - SHALLOW - self.page;
                    // SAFETY: the page lies within the mapping, and the call writes one byte for
                    // it to a local.
                    let asked = unsafe {
                        libc::mincore(
                            at as *mut libc::c_void as _,
                            self.page,
                            &mut held as *mut u8 as _,
                        )
                    };
                    asked != 0 || held & 1 == 1
                }

                /// Gives back to the system the pages below the segment's top [`SHALLOW`] bytes,
                /// once no work runs on it, and counts it shallow again.
                fn give_back(&self) {
                    let below = self.top() - SHALLOW - self.bottom();
                    // SAFETY: the range lies within the mapping, below the top that work left
                    // when it returned, and nothing runs on the segment while it is idle.
                    unsafe { libc::madvise(self.bottom() as *mut libc::c_void, below, GIVE_BACK) };
                    self.deep.set(false);
                }
            }

            /// Keeps the system from backing the mapping at `map` of `len` bytes with huge pages,
            /// which would make a shallow use of a segment hold megabytes.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            fn no_huge_pages(map: *mut libc::c_void, len: usize) {
                // SAFETY: the range is a mapping that `Segment::new` just made, and the advice
                // changes only how the system backs it.
                unsafe { libc::madvise(map, len, libc::MADV_NOHUGEPAGE) };
            }

            /// Does nothing, as the system backs a stack segment with small pages here.
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            fn no_huge_pages(_map: *mut libc::c_void, _len: usize) {}

            impl Drop for Segment {
                fn drop(&mut self) {
                    // SAFETY: the mapping was made in `Segment::new` and is unmapped only here,
                    // for a segment that no work is running on.
                    unsafe { libc::munmap(self.map, self.len) };
                }
            }

            /// The room of the busy segment that `at` lies on, if it lies on one.
            pub(super) fn busy_room_at(at: usize) -> Option<Room> {
                SEGMENTS
                    .try_with(|segments| {
                        let segments = segments.0.borrow();
                        segments
                            .iter()
                            .find(|segment| segment.busy.get() && segment.holds(at))
                            .map(Segment::room)
                    })
                    .ok()
                    .flatten()
            }

            /// Makes the busy segment that `at` lies on deep, if it lies on one that is not, and
            /// returns its room, which then reaches down to [`RED_ZONE`](super::RED_ZONE) above
            /// its bottom.
            pub(super) fn deepen(at: usize) -> Option<Room> {
                SEGMENTS
                    .try_with(|segments| {
                        let segments = segments.0.borrow();
                        let segment = segments.iter().find(|segment| {
                            segment.busy.get() && !segment.deep.get() && segment.holds(at)
                        })?;
                        segment.deep.set(true);
                        Some(segment.room())
                    })
                    .ok()
                    .flatten()
            }

            /// Takes an idle segment for work, mapping one if the thread has none, and returns
            /// its usable range and its room, or `None` if no segment can be mapped.
            fn claim() -> Option<(usize, usize, Room)> {
                SEGMENTS
                    .try_with(|segments| {
                        let mut segments = segments.0.borrow_mut();
                        let index = match segments.iter().position(|segment| !segment.busy.get()) {
                            Some(index) => index,
                            None => {
                                segments.push(Segment::new()?);
                                segments.len() - 1
                            }
                        };
                        let segment = &segments[index];
                        segment.busy.set(true);
                        Some((segment.bottom(), segment.top() - segment.bottom(), segment.room()))
                    })
                    .ok()
                    .flatten()
            }

            /// Marks idle the segment whose usable part starts at `bottom`, once the work on it
            /// has returned, unmaps the idle segments past [`KEPT`], and gives back the deep pages
            /// of those it keeps. Work that does not check for room, as `checked` says, made the
            /// segment deep if it touched the page below the shallow top.
            fn release(bottom: usize, checked: bool) {
                let _ = SEGMENTS.try_with(|segments| {
                    let mut segments = segments.0.borrow_mut();
                    let released = segments.iter().find(|segment| segment.bottom() == bottom);
                    if let Some(segment) = released {
                        if !checked && !segment.deep.get() && segment.touched_deep() {
                            segment.deep.set(true);
                        }
                        segment.busy.set(false);
                    }
                    let mut idle = 0;
                    segments.retain(|segment| {
                        segment.busy.get() || {
                            idle += 1;
                            idle <= KEPT
                        }
                    });
                    for segment in segments.iter() {
                        if !segment.busy.get() && segment.deep.get() {
                            segment.give_back();
                        }
                    }
                });
            }

            /// Runs `f` on an idle segment of the thread and returns whether it ran, which it does
            /// not if no segment can be mapped. `checked` says whether the work in `f` checks for
            /// room as it goes.
            ///
            /// The segment is busy from before the switch until after the switch back, and no
            /// other work takes or unmaps a busy segment, so it stays the work's own however the
            /// thread interleaves work on other segments in between, as coroutines do. A panic in
            /// `f` is caught on the segment and resumed once back on the stack that switched, so
            /// that nothing unwinds across the switch.
            pub(super) fn run(checked: bool, f: &mut dyn FnMut()) -> bool {
                let Some((bottom, size, room)) = claim() else {
                    return false;
                };
                ROOM.set(room);
                let panic = switch(bottom, size, f);
                release(bottom, checked);
                ROOM.set(room_at(here()).unwrap_or(Room::NONE));
                if let Some(payload) = panic {
                    resume_unwind(payload);
                }
                true
            }

            /// Runs `f` on the stack from `bottom` up `size` bytes and returns what it panicked
            /// with, if it did.
            fn switch(
                bottom: usize,
                size: usize,
                f: &mut dyn FnMut(),
            ) -> Option<Box<dyn Any + Send>> {
                let saved = asan::leave(bottom, size);
                // SAFETY: `bottom` is page-aligned and `size` a whole number of pages of memory
                // mapped for reading and writing above a guard page. `claim` marked the segment
                // busy, so no other work on this thread takes or unmaps it until `release` marks
                // it idle after this returns, and segments are never shared between threads. The
                // callback catches every panic, so nothing unwinds out of it.
                let panic = unsafe {
                    psm::on_stack(bottom as *mut u8, size, || {
                        let from = asan::enter();
                        let panic = catch_unwind(AssertUnwindSafe(f)).err();
                        asan::exit(from);
                        panic
                    })
                };
                asan::back(saved);
                panic
            }

            #[cfg(ferro_asan)]
            mod asan {
                use std::ffi::c_void;

                unsafe extern "C" {
                    fn __sanitizer_start_switch_fiber(
                        fake_stack_save: *mut *mut c_void,
                        bottom: *const c_void,
                        size: usize,
                    );
                    fn __sanitizer_finish_switch_fiber(
                        fake_stack_save: *mut c_void,
                        bottom_old: *mut *const c_void,
                        size_old: *mut usize,
                    );
                }

                /// Tells AddressSanitizer that the thread is about to switch to the stack from
                /// `bottom` up `size` bytes, and returns what it saved of the stack it leaves.
                pub(super) fn leave(bottom: usize, size: usize) -> *mut c_void {
                    let mut saved = std::ptr::null_mut();
                    // SAFETY: the call records the stack switched to and writes to a local.
                    unsafe {
                        __sanitizer_start_switch_fiber(&mut saved, bottom as *const c_void, size)
                    };
                    saved
                }

                /// Tells it that the switch to a segment is done, and returns the bounds of the
                /// stack switched from.
                pub(super) fn enter() -> (usize, usize) {
                    let (mut bottom, mut size) = (std::ptr::null(), 0);
                    // SAFETY: the segment is entered afresh, which a null save says, and the
                    // call writes to locals.
                    unsafe {
                        __sanitizer_finish_switch_fiber(
                            std::ptr::null_mut(),
                            &mut bottom,
                            &mut size,
                        )
                    };
                    (bottom.addr(), size)
                }

                /// Tells it that the thread is about to leave the segment for good, back to the
                /// stack `from` names.
                pub(super) fn exit(from: (usize, usize)) {
                    // SAFETY: a null save says the segment's work is done, and `from` is what
                    // `enter` was told of the stack switched from.
                    unsafe {
                        __sanitizer_start_switch_fiber(
                            std::ptr::null_mut(),
                            from.0 as *const c_void,
                            from.1,
                        )
                    };
                }

                /// Tells it that the switch back is done, with what `leave` saved.
                pub(super) fn back(saved: *mut c_void) {
                    // SAFETY: `saved` is what `leave` returned before the switch.
                    unsafe {
                        __sanitizer_finish_switch_fiber(
                            saved,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                        )
                    };
                }
            }

            #[cfg(not(ferro_asan))]
            mod asan {
                /// Does nothing, as nothing watches the switch without AddressSanitizer.
                #[inline(always)]
                pub(super) fn leave(_bottom: usize, _size: usize) -> *mut std::ffi::c_void {
                    std::ptr::null_mut()
                }

                /// Does nothing, as nothing watches the switch without AddressSanitizer.
                #[inline(always)]
                pub(super) fn enter() -> (usize, usize) {
                    (0, 0)
                }

                /// Does nothing, as nothing watches the switch without AddressSanitizer.
                #[inline(always)]
                pub(super) fn exit(_from: (usize, usize)) {}

                /// Does nothing, as nothing watches the switch without AddressSanitizer.
                #[inline(always)]
                pub(super) fn back(_saved: *mut std::ffi::c_void) {}
            }
        }
    }
    no {
        mod segments {
            use super::Room;

            /// Whether a recursion can move to a segment on this target.
            pub(super) const SWITCHING: bool = false;

            /// Returns `None`, as there are no segments on this target.
            pub(super) fn busy_room_at(_at: usize) -> Option<Room> {
                None
            }

            /// Returns `None`, as there are no segments on this target.
            pub(super) fn deepen(_at: usize) -> Option<Room> {
                None
            }

            /// Returns `false` without running `f`, as no segment can be had on this target.
            pub(super) fn run(_checked: bool, _f: &mut dyn FnMut()) -> bool {
                false
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
mod segments {
    use super::Room;

    /// Whether a recursion can move to a segment on this target.
    pub(super) const SWITCHING: bool = false;

    /// Returns `None`, as there are no segments on this target.
    pub(super) fn busy_room_at(_at: usize) -> Option<Room> {
        None
    }

    /// Returns `None`, as there are no segments on this target.
    pub(super) fn deepen(_at: usize) -> Option<Room> {
        None
    }

    /// Returns `false` without running `f`, as no segment can be had on this target.
    pub(super) fn run(_checked: bool, _f: &mut dyn FnMut()) -> bool {
        false
    }
}
