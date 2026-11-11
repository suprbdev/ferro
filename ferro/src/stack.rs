//! Room on the stack for ferro's recursion over nested values.
//!
//! Encoding, decoding, visiting and pairing a value recurse once for each level it nests, up to
//! [`MAX_DEPTH`](crate::codec::MAX_DEPTH) levels, and each level takes from under a hundred bytes
//! to a few KB of stack, so a value at the limit needs far more stack than the 2 MiB a spawned
//! thread has. Each level runs through [`with_room`], which runs it where it is while at least
//! [`RED_ZONE`] bytes of stack are left below the caller, and otherwise on a segment of 16 MiB
//! mapped for the thread above a guard page, whose pages the system provides as they are first
//! touched. The levels below check against that segment in turn and move to the next one when it
//! runs short, so the recursion goes as deep as the value does, and the memory it takes is the
//! stack its levels use.
//!
//! Cloning and dropping a value recurse through it too, in code that Rust generates and that
//! checks for nothing, so a commit, which clones the root, and a merge run through [`roomy`],
//! which moves them to a segment unless [`DEEP_ROOM`] bytes are left, enough to clone or drop a
//! value at the limit.
//!
//! A thread keeps the segments it is running on, and the first one after the recursion returns to
//! the thread's own stack, so a value whose branches cross the edge of a stack many times maps
//! each segment once, and a thread that went deep holds one segment after it, with the pages its
//! deepest use touched, as a thread's own stack does.
//!
//! The check compares the address of a local with the bounds of the stack the thread is on, which
//! it asks the operating system for the first time it runs short. Segments are switched to with
//! [`psm`] on Linux, Android, macOS and iOS. On Windows, where the bounds are known but ferro does
//! not switch stacks, a level that finds no room is refused instead, and so is one on a stack the
//! thread did not start with, such as a fiber's. Elsewhere the bounds are not known and nothing is
//! checked. A stack other than the thread's own and its segments never counts as having room, so
//! a recursion on one, such as a coroutine's, moves to a segment at its first level.
//!
//! [Nesting depth](https://github.com/archlemon/ferro/blob/main/docs/internals/storage.md#nesting-depth)
//! describes what each level takes.

use std::cell::Cell;

/// The stack each level of a recursion may take before the next level checks for room: a level
/// that finds less than this left below it moves to a segment.
pub(crate) const RED_ZONE: usize = 128 << 10;

/// The stack [`roomy`] makes sure of: enough to clone or drop a value at
/// [`MAX_DEPTH`](crate::codec::MAX_DEPTH) at up to 3 KB a level, more than the code Rust generates
/// for both takes in a debug build for a tree of `BTreeMap` children that each hold a table.
pub(crate) const DEEP_ROOM: usize = 12 << 20;

/// The range of addresses where the stack a thread is on has room for one more level: from
/// `low`, [`RED_ZONE`] above the bottom of the stack, up `span` bytes to its top.
#[derive(Clone, Copy)]
struct Room {
    low: usize,
    span: usize,
}

thread_local! {
    static ROOM: Cell<Room> = const { Cell::new(Room { low: 0, span: 0 }) };
    static PROBED: Cell<bool> = const { Cell::new(false) };
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
/// A panic in `f` unwinds on through the caller.
pub(crate) fn roomy<R>(f: impl FnOnce() -> R) -> R {
    if has_room(DEEP_ROOM - RED_ZONE) {
        return f();
    }
    let mut f = Some(f);
    let mut out = None;
    run_elsewhere(DEEP_ROOM - RED_ZONE, &mut || out = f.take().map(|f| f()));
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

/// Runs `f` once [`room`] found the stack short, on a segment unless the thread's bounds were not
/// yet known and show room after all, and returns what it returned, or `None` without running it
/// if no segment can be had.
#[cold]
#[inline(never)]
pub(crate) fn elsewhere<R>(f: impl FnOnce() -> R) -> Option<R> {
    let mut f = Some(f);
    let mut out = None;
    run_elsewhere(0, &mut || out = f.take().map(|f| f()));
    out
}

/// Learns the bounds of the thread's stack the first time it is asked, then runs `f` where it is
/// if they show `more` bytes beyond [`RED_ZONE`] left, and otherwise on a segment. Returns whether
/// `f` ran, which it does not where no segment can be had.
fn run_elsewhere(more: usize, f: &mut dyn FnMut()) -> bool {
    if !PROBED.replace(true) {
        ROOM.set(match os::bounds() {
            Some((low, high)) => {
                let low = low.saturating_add(RED_ZONE);
                Room {
                    low,
                    span: high.saturating_sub(low),
                }
            }
            None if segments::SWITCHING => Room { low: 0, span: 0 },
            None => Room {
                low: 0,
                span: usize::MAX,
            },
        });
        if has_room(more) {
            f();
            return true;
        }
    }
    segments::run(f)
}

#[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
mod os {
    use std::mem::MaybeUninit;

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
    /// The bounds of the calling thread's stack, which are not known on this target.
    pub(super) fn bounds() -> Option<(usize, usize)> {
        None
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
psm::psm_stack_manipulation! {
    yes {
        mod segments {
            use std::cell::{Cell, RefCell};
            use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

            use super::{RED_ZONE, ROOM, Room};

            /// Whether a recursion can move to a segment on this target.
            pub(super) const SWITCHING: bool = true;

            /// The usable bytes of each segment, more than [`DEEP_ROOM`](super::DEEP_ROOM).
            const SEGMENT_BYTES: usize = 16 << 20;

            /// How many segments a thread keeps once its recursion is back on its own stack.
            const KEPT: usize = 1;

            thread_local! {
                static SEGMENTS: RefCell<Vec<Segment>> = const { RefCell::new(Vec::new()) };
                static IN_USE: Cell<usize> = const { Cell::new(0) };
            }

            /// A stack segment: [`SEGMENT_BYTES`] of memory mapped for reading and writing above
            /// a guard page that is mapped for neither, unmapped when dropped.
            struct Segment {
                map: *mut libc::c_void,
                len: usize,
                guard: usize,
            }

            impl Segment {
                /// Maps a segment, or returns `None` if the system refuses.
                fn new() -> Option<Segment> {
                    // SAFETY: `sysconf` only reads a system setting.
                    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
                        .ok()
                        .filter(|page| page.is_power_of_two())?;
                    let len = SEGMENT_BYTES.next_multiple_of(page) + page;
                    // SAFETY: a private anonymous mapping at an address the system picks aliases
                    // no memory, and its result is checked before use.
                    let map = unsafe {
                        libc::mmap(
                            std::ptr::null_mut(),
                            len,
                            libc::PROT_NONE,
                            libc::MAP_PRIVATE | libc::MAP_ANON,
                            -1,
                            0,
                        )
                    };
                    if map == libc::MAP_FAILED {
                        return None;
                    }
                    let segment = Segment {
                        map,
                        len,
                        guard: page,
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
                    (opened == 0).then_some(segment)
                }

                /// The lowest address and the length of the segment's usable part.
                fn area(&self) -> (usize, usize) {
                    (self.map.addr() + self.guard, self.len - self.guard)
                }
            }

            impl Drop for Segment {
                fn drop(&mut self) {
                    // SAFETY: the mapping was made in `Segment::new` and is unmapped only here,
                    // once no recursion runs on it.
                    unsafe { libc::munmap(self.map, self.len) };
                }
            }

            /// Runs `f` on the next segment of the thread, mapping it if the thread has none to
            /// spare, and returns whether it ran, which it does not if no segment can be mapped.
            ///
            /// A panic in `f` is caught on the segment and resumed once back on the stack that
            /// called, so that nothing unwinds across the switch.
            pub(super) fn run(f: &mut dyn FnMut()) -> bool {
                let index = IN_USE.get();
                let area = SEGMENTS.try_with(|segments| {
                    let mut segments = segments.borrow_mut();
                    if segments.len() <= index {
                        segments.push(Segment::new()?);
                    }
                    segments.get(index).map(Segment::area)
                });
                let Ok(Some((base, size))) = area else {
                    return false;
                };
                let low = base + RED_ZONE;
                let outer = ROOM.replace(Room {
                    low,
                    span: base + size - low,
                });
                IN_USE.set(index + 1);
                // SAFETY: `base` is page-aligned and `size` a whole number of pages of memory
                // mapped for reading and writing, above a guard page, which this thread alone
                // uses and keeps mapped until `IN_USE` falls back to `index` below. The callback
                // catches every panic, so nothing unwinds out of it.
                let panic = unsafe {
                    psm::on_stack(base as *mut u8, size, || {
                        catch_unwind(AssertUnwindSafe(f)).err()
                    })
                };
                IN_USE.set(index);
                ROOM.set(outer);
                if index == 0 {
                    let _ = SEGMENTS.try_with(|segments| segments.borrow_mut().truncate(KEPT));
                }
                if let Some(payload) = panic {
                    resume_unwind(payload);
                }
                true
            }
        }
    }
    no {
        mod segments {
            /// Whether a recursion can move to a segment on this target.
            pub(super) const SWITCHING: bool = false;

            /// Returns `false` without running `f`, as no segment can be had on this target.
            pub(super) fn run(_f: &mut dyn FnMut()) -> bool {
                false
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
mod segments {
    /// Whether a recursion can move to a segment on this target.
    pub(super) const SWITCHING: bool = false;

    /// Returns `false` without running `f`, as no segment can be had on this target.
    pub(super) fn run(_f: &mut dyn FnMut()) -> bool {
        false
    }
}
