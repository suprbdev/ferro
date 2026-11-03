use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferro::{Data, Db, Durability, Map, Options, Record, Table};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn chunk(size: usize) -> usize {
    (size + 8).next_multiple_of(16).max(32)
}

fn grew(by: usize) {
    let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(chunk(layout.size()));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(chunk(layout.size()), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        grew(chunk(size));
        LIVE.fetch_sub(chunk(layout.size()), Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const MIB: usize = 1 << 20;
const COMMITS: u64 = 20_000;

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u64,
    #[index]
    city: String,
    name: String,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Shop {
    rows: Table<Row>,
    names: Vec<String>,
    counts: Map<u64, u64>,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x ^ (x >> 27)
}

fn row(i: u64) -> Row {
    Row {
        id: mix(i),
        city: format!("city-{}", i % 16),
        name: format!("person number {i}"),
    }
}

fn build(path: &Path) {
    let db: Db<Shop> = Options::new().durability(Durability::Buffered).open(path).unwrap();
    db.update(|shop| {
        for i in 0..60_000u64 {
            shop.rows.insert(row(i));
        }
        shop.counts.extend((0..60_000u64).map(|i| (mix(i), i)));
        shop.names = (0..200).map(|i| format!("name-{i}")).collect();
    })
    .unwrap();
    for i in 0..COMMITS {
        db.update(|shop| {
            shop.rows.insert(row(i * 7));
            shop.counts.insert(mix(i * 11), i);
            shop.names[(i % 200) as usize] = format!("renamed-{i}");
        })
        .unwrap();
    }
}

struct Walked {
    retained: usize,
    peak: usize,
}

fn walk(path: &Path, setting: usize, step: impl Fn(&Db<Shop>)) -> Walked {
    let db: Db<Shop> = Options::new()
        .read_only()
        .cache_bytes(setting as u64)
        .open(path)
        .unwrap();
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    step(&db);
    Walked {
        retained: LIVE.load(Ordering::Relaxed).saturating_sub(before),
        peak: PEAK.load(Ordering::Relaxed).saturating_sub(before),
    }
}

fn report(name: &str, setting: usize, walked: &Walked) {
    eprintln!(
        "{name} at {} MiB: retained {:.2} MiB, peak {:.2} MiB",
        setting / MIB,
        walked.retained as f64 / MIB as f64,
        walked.peak as f64 / MIB as f64,
    );
}

#[test]
fn walking_history_stays_near_cache_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.ferro");
    build(&path);
    let probe = mix(0);
    for setting in [2 * MIB, 8 * MIB] {
        let versions = walk(&path, setting, |db| {
            for version in 1..=db.version() {
                let snapshot = db.at(version).unwrap();
                std::hint::black_box(snapshot.rows.get(&probe));
                std::hint::black_box(snapshot.rows.by_city("city-3").next());
                std::hint::black_box(snapshot.counts.get(&probe));
            }
        });
        report("at every version", setting, &versions);
        let projected = walk(&path, setting, |db| {
            let changes = db
                .history_of(|shop| (shop.rows.get(&probe).map(|row| row.name.clone()), shop.counts.len()))
                .count();
            std::hint::black_box(changes);
        });
        report("history_of", setting, &projected);
        for (name, walked) in [("at every version", &versions), ("history_of", &projected)] {
            assert!(
                walked.retained < setting * 3 / 2,
                "{name}: retained {} at a setting of {setting}",
                walked.retained
            );
            assert!(
                walked.peak < setting * 2,
                "{name}: peak {} at a setting of {setting}",
                walked.peak
            );
        }
    }
}
