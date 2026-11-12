mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{fast, text};
use ferro::{Db, Map};
use parking_lot::Mutex;

struct Shared;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static HIGH: AtomicUsize = AtomicUsize::new(0);

fn grew(size: usize) {
    let now = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    HIGH.fetch_max(now, Ordering::Relaxed);
}

fn shrank(size: usize) {
    LIVE.fetch_sub(size, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System` with the caller's layout, pointer and size unchanged
// and returns what `System` returns, so `System`'s guarantees carry over. Besides that it only
// updates the atomics `LIVE` and `HIGH`, which neither allocate nor panic, so the heap of every
// thread is counted together.
unsafe impl GlobalAlloc for Shared {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        // SAFETY: the caller meets `GlobalAlloc::alloc`'s contract for `layout`, which is passed on
        // to `System` unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        shrank(layout.size());
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout`, and every block this allocator hands out came from `System` with that layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        // SAFETY: the caller meets `GlobalAlloc::alloc_zeroed`'s contract for `layout`, which is
        // passed on to `System` unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        grew(size);
        shrank(layout.size());
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout` and that `size` is a valid new size for it. The block came from `System` with
        // that layout, and all three are passed on unchanged.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Shared = Shared;

const MIB: usize = 1 << 20;

static SERIAL: Mutex<()> = Mutex::new(());

type Root = Map<u64, String>;

struct Reader {
    written: usize,
    pause: Duration,
}

impl Write for Reader {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.pause.is_zero() {
            std::thread::sleep(self.pause);
        }
        self.written += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn large_values(dir: &Path) -> Db<Root> {
    let a: Db<Root> = fast().open(dir.join("a.ferro")).unwrap();
    a.commit("small", |root| {
        for key in 0..100_000u64 {
            root.insert(key, text(key, 12));
        }
    })
    .unwrap();
    a.commit("large", |root| {
        for index in 0..130u64 {
            root.insert(1_000_000 + index, text(index + 77, MIB));
        }
    })
    .unwrap();
    a
}

fn small_values(dir: &Path) -> Db<Root> {
    let a: Db<Root> = fast().open(dir.join("a.ferro")).unwrap();
    for round in 0..10u64 {
        a.commit(format!("round {round}"), |root| {
            for key in round * 30_000..(round + 1) * 30_000 {
                root.insert(key, text(key, 40));
            }
        })
        .unwrap();
    }
    a
}

fn sent_with_heap(a: &Db<Root>, dir: &Path, pause: Duration) -> (usize, usize) {
    let b: Db<Root> = fast().open(dir.join("b.ferro")).unwrap();
    let have = b.have().unwrap();
    let start = LIVE.load(Ordering::Relaxed);
    HIGH.store(start, Ordering::Relaxed);
    let mut reader = Reader { written: 0, pause };
    let sent = a.bundle_to(&have, &mut reader).unwrap() as usize;
    let peak = HIGH.load(Ordering::Relaxed) - start;
    assert_eq!(sent, reader.written);
    (sent, peak)
}

#[test]
fn a_first_pull_of_large_values_to_a_slow_reader_holds_a_few_of_them_on_every_thread() {
    let _serial = SERIAL.lock();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let a = large_values(dir.path());
    let (sent, peak) = sent_with_heap(&a, dir.path(), Duration::from_micros(500));
    println!("sent {sent} bytes to a slow reader with {peak} bytes of heap on all threads");
    assert!(sent > 130 * MIB);
    assert!(
        peak < 16 * MIB,
        "sending 130 values of 1 MiB to a slow reader held {peak} bytes of heap"
    );
}

#[test]
fn a_first_pull_of_large_values_to_a_fast_reader_holds_a_few_of_them_on_every_thread() {
    let _serial = SERIAL.lock();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let a = large_values(dir.path());
    let (sent, peak) = sent_with_heap(&a, dir.path(), Duration::ZERO);
    println!("sent {sent} bytes to a fast reader with {peak} bytes of heap on all threads");
    assert!(sent > 130 * MIB);
    assert!(
        peak < 16 * MIB,
        "sending 130 values of 1 MiB to a fast reader held {peak} bytes of heap"
    );
}

#[test]
fn a_first_pull_of_small_values_to_a_slow_reader_holds_its_list_and_little_more() {
    let _serial = SERIAL.lock();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let a = small_values(dir.path());
    let (sent, peak) = sent_with_heap(&a, dir.path(), Duration::from_micros(500));
    println!("sent {sent} bytes of small values to a slow reader with {peak} bytes of heap");
    assert!(
        peak < 8 * MIB,
        "sending 300,000 small values to a slow reader held {peak} bytes of heap"
    );
}

#[cfg(target_os = "linux")]
mod one_core {
    use std::io::{self, Write};
    use std::path::Path;
    use std::time::Duration;

    use ferro::{Db, Outcome, Snapshot};

    use super::{Root, SERIAL, fast, text};

    fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
        panic!("no merge expected")
    }

    fn history(dir: &Path) -> Db<Root> {
        let a: Db<Root> = fast().open(dir.join("a.ferro")).unwrap();
        a.commit("base", |root| {
            for key in 0..60_000u64 {
                root.insert(key, text(key, 12));
            }
        })
        .unwrap();
        for round in 0..300u64 {
            a.commit(format!("round {round}"), |root| {
                for step in 0..20u64 {
                    let key = (round * 7_919 + step * 104_729) % 70_000;
                    root.insert(key, text(round * 100 + step, 30));
                }
                if round % 7 == 0 {
                    root.remove(&(round * 13));
                }
            })
            .unwrap();
            if round % 15 == 0 {
                a.commit(format!("large {round}"), |root| {
                    root.insert(1_000_000 + round, text(round, 300_000));
                    root.insert(2_000_000 + round, "z".repeat(200_000 + round as usize));
                })
                .unwrap();
            }
        }
        a
    }

    struct Paced {
        bytes: Vec<u8>,
        pause: Duration,
    }

    impl Write for Paced {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            std::thread::sleep(self.pause);
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn pin_to_one_core() {
        let mut mask = [0u64; 16];
        let size = std::mem::size_of_val(&mask);
        // SAFETY: `mask` is `size` writable bytes, as large and as aligned as a `cpu_set_t` of
        // 1,024 cores, and the kernel writes no more than `size` bytes of it.
        let got = unsafe { libc::sched_getaffinity(0, size, mask.as_mut_ptr().cast()) };
        assert_eq!(got, 0);
        let word = mask.iter().position(|bits| *bits != 0).unwrap();
        let mut one = [0u64; 16];
        one[word] = 1 << mask[word].trailing_zeros();
        // SAFETY: `one` is `size` readable bytes laid out as a `cpu_set_t`, which the kernel only
        // reads, and it names a core the thread may already run on.
        let set = unsafe { libc::sched_setaffinity(0, size, one.as_ptr().cast()) };
        assert_eq!(set, 0);
    }

    #[test]
    fn a_first_pull_is_the_same_bundle_on_one_core_and_on_several_threads() {
        let _serial = SERIAL.lock();
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let a = history(dir.path());
        let b: Db<Root> = fast().open(dir.path().join("b.ferro")).unwrap();
        let have = b.have().unwrap();
        let alone = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    pin_to_one_core();
                    assert_eq!(std::thread::available_parallelism().unwrap().get(), 1);
                    a.bundle(&have).unwrap()
                })
                .join()
                .unwrap()
        });
        let cores = std::thread::available_parallelism().map_or(1, |cores| cores.get());
        println!(
            "{} bytes on one core, compared on {cores}",
            alone.as_bytes().len()
        );
        for _ in 0..5 {
            let threaded = a.bundle(&have).unwrap();
            assert!(
                threaded.as_bytes() == alone.as_bytes(),
                "the bundle built on {cores} cores differs from the one built on one"
            );
        }
        for pause in [Duration::from_micros(200), Duration::from_millis(2)] {
            let mut paced = Paced {
                bytes: Vec::new(),
                pause,
            };
            a.bundle_to(&have, &mut paced).unwrap();
            assert!(
                paced.bytes == alone.as_bytes(),
                "the bundle sent to a reader pausing {pause:?} a write is not the one-core one"
            );
        }
        let pulled = b.receive("a", &alone, refuse).unwrap();
        assert_eq!(pulled.outcome, Outcome::FastForward);
        assert!(pulled.objects >= 4_096, "{} objects", pulled.objects);
        assert_eq!(b.head_hash(), a.head_hash());
    }
}
