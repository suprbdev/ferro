mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::fast;
use ferro::{Bundle, Db, Error, Map, Snapshot};

struct Peak;

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static HIGH: Cell<usize> = const { Cell::new(0) };
}

fn grew(size: usize) {
    let _ = LIVE.try_with(|live| {
        let now = live.get() + size;
        live.set(now);
        let _ = HIGH.try_with(|high| high.set(high.get().max(now)));
    });
}

fn shrank(size: usize) {
    let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(size)));
}

unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        shrank(layout.size());
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        grew(size);
        shrank(layout.size());
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Peak = Peak;

const MIB: usize = 1 << 20;

type Root = Map<u64, String>;

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn header(out: &mut Vec<u8>, hints: u64) {
    out.extend_from_slice(b"FERROBDL");
    out.extend_from_slice(&[1, 1]);
    out.extend_from_slice(&[7; 20]);
    varint(out, 1);
    varint(out, 0);
    varint(out, 0);
    varint(out, hints);
}

fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    panic!("no merge expected")
}

fn peak_of(db: &Db<Root>, bundle: &Bundle) -> usize {
    let start = LIVE.with(Cell::get);
    HIGH.with(|high| high.set(start));
    match db.receive("evil", bundle, refuse) {
        Err(Error::Bundle(_)) => {}
        other => panic!("accepted a hostile bundle: {other:?}"),
    }
    HIGH.with(Cell::get) - start
}

fn claims_many_objects() -> Bundle {
    let mut out = Vec::new();
    header(&mut out, 0);
    let padding = 4 * MIB;
    varint(&mut out, padding as u64 / 2);
    varint(&mut out, (padding as u64) << 1);
    out.resize(out.len() + padding, 0);
    out.push(0);
    Bundle::from(out)
}

fn carries_many_objects() -> Bundle {
    let mut out = Vec::new();
    header(&mut out, 0);
    let count = 2 * MIB;
    varint(&mut out, count as u64);
    out.resize(out.len() + 2 * count, 0);
    Bundle::from(out)
}

fn claims_many_hints() -> Bundle {
    let mut out = Vec::new();
    header(&mut out, 4 * MIB as u64 / 2);
    out.resize(out.len() + 4 * MIB, 0x80);
    Bundle::from(out)
}

fn expands_far() -> Bundle {
    let mut out = Vec::new();
    header(&mut out, 0);
    let count = 96;
    varint(&mut out, count);
    for index in 0..count {
        let mut body = vec![0u8; MIB];
        body[0] = index as u8;
        let packed = lz4_flex::block::compress(&body);
        varint(&mut out, (packed.len() as u64) << 1 | 1);
        varint(&mut out, body.len() as u64);
        out.extend_from_slice(&packed);
        varint(&mut out, 0);
    }
    Bundle::from(out)
}

fn with_huge_claimed_size() -> Bundle {
    let mut out = Vec::new();
    header(&mut out, 0);
    varint(&mut out, 1);
    let packed = vec![0u8; 256 << 10];
    varint(&mut out, (packed.len() as u64) << 1 | 1);
    varint(&mut out, packed.len() as u64 * 255);
    out.extend_from_slice(&packed);
    varint(&mut out, 0);
    Bundle::from(out)
}

#[test]
fn hostile_bundles_cost_memory_in_proportion_to_what_they_carry() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = fast().open(dir.path().join("b.ferro")).unwrap();
    db.commit("seed", |root| {
        root.insert(1, "one".to_string());
    })
    .unwrap();
    let cases = [
        ("many objects", claims_many_objects(), 3),
        ("many repeated objects", carries_many_objects(), 3),
        ("many hints", claims_many_hints(), 3),
        ("far expansion", expands_far(), 64),
        ("huge claimed size", with_huge_claimed_size(), 3),
    ];
    for (name, bundle, ratio) in cases {
        let len = bundle.as_bytes().len();
        let peak = peak_of(&db, &bundle);
        println!("{name}: {len} bytes, peak {peak}");
        assert!(
            peak <= ratio * len + 20 * MIB,
            "{name}: a bundle of {len} bytes took {peak} bytes of heap"
        );
    }
    assert_eq!(db.version(), 1);
    assert_eq!(db.branches(), vec!["main".to_string()]);
}
