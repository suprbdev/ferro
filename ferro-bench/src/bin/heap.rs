//! Measures the live heap that reads and commits hold at each cache size.
//!
//! A counting global allocator tracks live bytes as `malloc` chunks: each request plus 8 bytes,
//! rounded up to a multiple of 16 and at least 32. In a temporary directory under the directory
//! given as the first argument (default `target/bench`) it builds maps of 8,000,000 short strings,
//! of 1,200,000 strings of 300 bytes and of 200,000 strings of 2,000 bytes, and a table of
//! 1,500,000 accounts with two indexes. With caches of 1, 4, 16, 64 and 256 MiB it opens each file
//! read-only and reports how much the heap grew over two full scans of one snapshot, each followed
//! on the table by a count of one country's rows through its index. Two more rows measure a fresh
//! `Durability::Buffered` map of 500,000 keys taking 30,000 single-key commits with two `get`s
//! after each, and a `history_of` walk over that file. It prints a table of MiB with a row for
//! each workload and a column for each cache size.
//!
//! ```sh
//! make bench-heap
//! cargo run --release -p ferro-bench --bin heap -- target/bench
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferro::{Data, Db, Map, Options, Record, Table};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

fn chunk(size: usize) -> usize {
    (size + 8).next_multiple_of(16).max(32)
}

// SAFETY: every method forwards to `System` with the caller's layout, pointer and size unchanged
// and returns what `System` returns, so `System`'s guarantees carry over. Besides that it only
// adjusts the atomic counter `LIVE`, which neither allocates nor panics: a layout's size is at
// most `isize::MAX`, so `chunk` cannot overflow.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(chunk(layout.size()), Ordering::Relaxed);
        // SAFETY: the caller meets `GlobalAlloc::alloc`'s contract for `layout`, which is passed
        // on to `System` unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(chunk(layout.size()), Ordering::Relaxed);
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout`, and every block this allocator hands out came from `System` with that layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        LIVE.fetch_add(chunk(size), Ordering::Relaxed);
        LIVE.fetch_sub(chunk(layout.size()), Ordering::Relaxed);
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout` and that `size` is a valid new size for it. The block came from `System` with
        // that layout, and all three are passed on unchanged.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const MIB: usize = 1 << 20;

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Free,
    Pro,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Account {
    #[key]
    id: u64,
    #[index]
    tier: Tier,
    #[index]
    country: String,
    email: String,
    name: String,
    balance: i64,
    tags: Vec<String>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Bank {
    accounts: Table<Account>,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x ^ (x >> 27)
}

fn filler(seed: u64, len: usize) -> String {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = mix(state);
            char::from(b'a' + (state % 26) as u8)
        })
        .collect()
}

fn account(i: u64) -> Account {
    let seed = mix(i);
    Account {
        id: i,
        tier: if seed.is_multiple_of(3) {
            Tier::Pro
        } else {
            Tier::Free
        },
        country: ["nz", "au", "de", "us"][(seed >> 8) as usize % 4].to_string(),
        email: format!("user{i}@example.com"),
        name: format!("Account Holder {}", seed % 100_000),
        balance: (seed % 1_000_000) as i64,
        tags: vec!["beta".to_string(), format!("cohort-{}", seed % 12)],
    }
}

fn settle<T: Data>(db: &Db<T>, read: impl Fn(&T)) -> f64 {
    let before = LIVE.load(Ordering::Relaxed);
    let snapshot = db.read();
    read(&snapshot);
    read(&snapshot);
    let grown = LIVE.load(Ordering::Relaxed).saturating_sub(before);
    grown as f64 / MIB as f64
}

fn row(name: &str, sizes: &[usize], measure: impl Fn(usize) -> f64) {
    print!("{name:<20}");
    for mib in sizes {
        let heap = measure(mib * MIB);
        print!(" {heap:>9.1}");
    }
    println!();
}

fn built<T: Data + Default>(path: &Path, fill: impl FnOnce(&Db<T>)) {
    let db = Options::new().open::<T>(path).unwrap();
    fill(&db);
}

fn main() {
    let parent = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/bench".to_string());
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let sizes = [1, 4, 16, 64, 256];
    print!("{:<20}", "live heap MiB at");
    for mib in sizes {
        print!(" {:>8}M", mib);
    }
    println!();

    let short = dir.path().join("short.ferro");
    let keys = 8_000_000u64;
    built::<Map<u64, String>>(&short, |db| {
        db.update(|root| {
            root.extend((0..keys).map(|i| (mix(i), format!("value-{i}"))));
        })
        .unwrap();
    });
    row("short strings", &sizes, |bytes| {
        let db = Options::new().read_only(true).cache_bytes(bytes as u64);
        let db = db.open::<Map<u64, String>>(&short).unwrap();
        settle(&db, |root| {
            for (key, value) in root.iter() {
                std::hint::black_box((key, value));
            }
        })
    });

    let medium = dir.path().join("medium.ferro");
    let rows = 1_200_000u64;
    built::<Map<u64, String>>(&medium, |db| {
        db.update(|root| {
            root.extend((0..rows).map(|i| (mix(i), filler(i, 300))));
        })
        .unwrap();
    });
    row("300 byte strings", &sizes, |bytes| {
        let db = Options::new().read_only(true).cache_bytes(bytes as u64);
        let db = db.open::<Map<u64, String>>(&medium).unwrap();
        settle(&db, |root| {
            for (key, value) in root.iter() {
                std::hint::black_box((key, value));
            }
        })
    });

    let large = dir.path().join("large.ferro");
    let blobs = 200_000u64;
    built::<Map<u64, String>>(&large, |db| {
        db.update(|root| {
            root.extend((0..blobs).map(|i| (i, filler(i, 2000))));
        })
        .unwrap();
    });
    row("2 KiB spilled", &sizes, |bytes| {
        let db = Options::new().read_only(true).cache_bytes(bytes as u64);
        let db = db.open::<Map<u64, String>>(&large).unwrap();
        settle(&db, |root| {
            for (key, value) in root.iter() {
                std::hint::black_box((key, value));
            }
        })
    });

    let churn = dir.path().join("churn.ferro");
    row("commits and reads", &sizes, |bytes| {
        let _ = std::fs::remove_file(&churn);
        let before = LIVE.load(Ordering::Relaxed);
        let db = Options::new()
            .durability(ferro::Durability::Buffered)
            .cache_bytes(bytes as u64)
            .open::<Map<u64, String>>(&churn)
            .unwrap();
        db.update(|root| {
            root.extend((0..500_000u64).map(|i| (mix(i), format!("value-{i}"))));
        })
        .unwrap();
        for i in 0..30_000u64 {
            db.update(|root| {
                root.insert(mix(i), format!("again-{i}"));
            })
            .unwrap();
            let snapshot = db.read();
            std::hint::black_box(snapshot.get(&mix(i + 7)));
            std::hint::black_box(snapshot.get(&mix(i * 31)));
        }
        LIVE.load(Ordering::Relaxed).saturating_sub(before) as f64 / MIB as f64
    });
    row("history walk", &sizes, |bytes| {
        let db = Options::new()
            .read_only(true)
            .cache_bytes(bytes as u64)
            .open::<Map<u64, String>>(&churn)
            .unwrap();
        let before = LIVE.load(Ordering::Relaxed);
        let key = mix(29_000);
        let changes = db.history_of(|root| root.get(&key)).count();
        std::hint::black_box(changes);
        LIVE.load(Ordering::Relaxed).saturating_sub(before) as f64 / MIB as f64
    });

    let table = dir.path().join("table.ferro");
    let accounts = 1_500_000u64;
    built::<Bank>(&table, |db| {
        for chunk in 0..accounts.div_ceil(100_000) {
            db.update(|bank| {
                for i in chunk * 100_000..(chunk + 1) * 100_000 {
                    bank.accounts.insert(account(i));
                }
            })
            .unwrap();
        }
    });
    row("table rows", &sizes, |bytes| {
        let db = Options::new().read_only(true).cache_bytes(bytes as u64);
        let db = db.open::<Bank>(&table).unwrap();
        settle(&db, |bank| {
            for row in bank.accounts.iter() {
                std::hint::black_box(row);
            }
            std::hint::black_box(bank.accounts.by_country("nz").count());
        })
    });
}
