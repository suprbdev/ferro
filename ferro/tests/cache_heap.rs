use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ferro::store::StoreOptions;
use ferro::{Data, Map, Options, Record, Table};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

fn chunk(size: usize) -> usize {
    (size + 8).next_multiple_of(16).max(32)
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(chunk(layout.size()), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(chunk(layout.size()), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        LIVE.fetch_add(chunk(size), Ordering::Relaxed);
        LIVE.fetch_sub(chunk(layout.size()), Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const MIB: usize = 1 << 20;

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u64,
    #[index]
    city: String,
    name: String,
    tags: Vec<String>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Rows {
    rows: Table<Row>,
}

fn grown_by(read: impl FnOnce()) -> usize {
    let before = LIVE.load(Ordering::Relaxed);
    read();
    LIVE.load(Ordering::Relaxed).saturating_sub(before)
}

fn check(name: &str, setting: usize, heap: usize) {
    assert!(
        heap < setting * 3 / 2,
        "{name}: {heap} heap at a setting of {setting}"
    );
    assert!(
        heap > setting / 2,
        "{name}: {heap} heap at a setting of {setting}"
    );
}

#[test]
fn cache_bytes_bounds_the_decoded_heap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("map.ferro");
    let keys = 300_000u64;
    {
        let db = Options::new().open::<Map<u64, String>>(&path).unwrap();
        db.update(|root| {
            root.extend((0..keys).map(|i| (i * 7919, format!("value-{i}"))));
        })
        .unwrap();
    }
    let rows_path = dir.path().join("rows.ferro");
    {
        let db = Options::new().open::<Rows>(&rows_path).unwrap();
        db.update(|root| {
            for i in 0..120_000u64 {
                root.rows.insert(Row {
                    id: i,
                    city: format!("city-{}", i % 16),
                    name: format!("person number {i}"),
                    tags: vec!["one".to_string(), format!("group-{}", i % 40)],
                });
            }
        })
        .unwrap();
    }
    for setting in [2 * MIB, 8 * MIB] {
        let db = Options::new()
            .read_only()
            .cache_bytes(setting as u64)
            .open::<Map<u64, String>>(&path)
            .unwrap();
        let snapshot = db.read();
        let heap = grown_by(|| {
            for pair in snapshot.iter() {
                std::hint::black_box(pair);
            }
        });
        check("map", setting, heap);
        let db = Options::new()
            .read_only()
            .cache_bytes(setting as u64)
            .open::<Rows>(&rows_path)
            .unwrap();
        let snapshot = db.read();
        let heap = grown_by(|| {
            for row in snapshot.rows.iter() {
                std::hint::black_box(row);
            }
        });
        check("table", setting, heap);
    }
}

#[test]
fn the_default_budget_keeps_what_the_old_setting_really_held() {
    assert_eq!(StoreOptions::default().cache_bytes, 1 << 30);
}
