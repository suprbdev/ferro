mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::time::Instant;

use common::{Tripwire, fast, text};
use ferro::{
    Bundle, Data, Db, Error, Keep, Map, Outcome, Reclaim, Record, Snapshot, Summary, Table,
};

struct Peak;

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static HIGH: Cell<usize> = const { Cell::new(0) };
    static TOTAL: Cell<usize> = const { Cell::new(0) };
}

fn grew(size: usize) {
    let _ = TOTAL.try_with(|total| total.set(total.get() + size));
    let _ = LIVE.try_with(|live| {
        let now = live.get() + size;
        live.set(now);
        let _ = HIGH.try_with(|high| high.set(high.get().max(now)));
    });
}

fn shrank(size: usize) {
    let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(size)));
}

// SAFETY: every method forwards to `System` with the caller's layout, pointer and size unchanged
// and returns what `System` returns, so `System`'s guarantees carry over. Besides that it only
// updates the const-initialized thread-local cells `LIVE`, `HIGH` and `TOTAL`, through `try_with`,
// which neither allocates nor panics, even while the thread's locals are being destroyed.
unsafe impl GlobalAlloc for Peak {
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
    out.extend_from_slice(&[2, 1]);
    out.extend_from_slice(&[7; 20]);
    varint(out, 1);
    varint(out, 0);
    varint(out, 0);
    varint(out, hints);
}

fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    panic!("no merge expected")
}

fn heap_of<R>(run: impl FnOnce() -> R) -> (R, usize) {
    let start = LIVE.with(Cell::get);
    HIGH.with(|high| high.set(start));
    let out = run();
    (out, HIGH.with(Cell::get) - start)
}

fn peak_of(db: &Db<Root>, bundle: &Bundle) -> usize {
    let (received, peak) = heap_of(|| db.receive("evil", bundle, refuse));
    match received {
        Err(Error::Bundle(_)) => {}
        other => panic!("accepted a hostile bundle: {other:?}"),
    }
    peak
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
    out.resize(out.len() + 3 * count, 0);
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
        varint(&mut out, 0);
    }
    Bundle::from(out)
}

fn carries_large_objects(count: u64) -> Bundle {
    let mut out = Vec::new();
    header(&mut out, 0);
    varint(&mut out, count + 1);
    for index in 0..count {
        let mut body = vec![0u8; MIB];
        body[..8].copy_from_slice(&index.to_le_bytes());
        varint(&mut out, (body.len() as u64) << 1);
        out.extend_from_slice(&body);
        varint(&mut out, 0);
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

#[test]
fn a_hostile_stream_of_large_objects_costs_one_object_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    let db: Db<Root> = fast().open(&path).unwrap();
    db.commit("seed", |root| {
        root.insert(1, "one".to_string());
    })
    .unwrap();
    let size = db.size_on_disk();
    let bundle = carries_large_objects(48);
    let len = bundle.as_bytes().len();
    let peak = peak_of(&db, &bundle);
    println!("{len} bytes in 48 objects of 1 MiB, peak {peak}");
    assert!(
        peak < 8 * MIB,
        "a stream of {len} bytes took {peak} bytes of heap"
    );
    assert_eq!(db.size_on_disk(), size);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), size);
    assert_eq!(db.version(), 1);
}

fn hints_on_held(db: &Db<Root>, hints: &[(u64, u64)]) -> Bundle {
    let mut mark = db.head_hash().unwrap().to_vec();
    varint(&mut mark, db.version());
    let mut out = b"FERROBDL".to_vec();
    out.extend_from_slice(&[2, 1]);
    out.extend_from_slice(&mark);
    varint(&mut out, 1);
    out.extend_from_slice(&mark);
    varint(&mut out, 1);
    varint(&mut out, 0);
    varint(&mut out, hints.len() as u64);
    for (parent, slot) in hints {
        varint(&mut out, *parent);
        varint(&mut out, *slot);
    }
    varint(&mut out, 0);
    Bundle::from(out)
}

fn allocated_by<R>(run: impl FnOnce() -> R) -> (R, usize) {
    let start = TOTAL.with(Cell::get);
    let out = run();
    (out, TOTAL.with(Cell::get) - start)
}

#[test]
fn hints_that_name_one_held_object_many_ways_read_it_once() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = fast()
        .cache_bytes(1)
        .open(dir.path().join("b.ferro"))
        .unwrap();
    db.commit("seed", |root| {
        for key in 0..20_000 {
            root.insert(key, text(key, 16));
        }
    })
    .unwrap();
    let count = 200_000;
    let flat = vec![(0, 0); 2 * count as usize];
    let many_ways: Vec<(u64, u64)> = (0..count)
        .map(|_| (0, 0))
        .chain((0..count).map(|at| (1 + at, 0)))
        .collect();
    let mut costs = Vec::new();
    for (name, hints) in [("one parent", flat), ("one parent many ways", many_ways)] {
        let bundle = hints_on_held(&db, &hints);
        let started = Instant::now();
        let (outcome, allocated) = allocated_by(|| db.receive("evil", &bundle, refuse));
        let took = started.elapsed();
        let outcome = outcome.map(|pulled| pulled.outcome);
        let len = bundle.as_bytes().len();
        println!("{name}: {len} bytes, {allocated} bytes allocated in {took:?}, {outcome:?}");
        costs.push(allocated);
    }
    assert!(
        costs[1] < costs[0] + costs[0] / 2 + 8 * MIB,
        "hints that reach one held object through many hints allocated {} bytes, against {} through one",
        costs[1],
        costs[0]
    );
    assert_eq!(db.version(), 1);
}

#[test]
fn a_streamed_bundle_costs_a_buffer_and_its_objects_not_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Root> = fast().open(dir.path().join("a.ferro")).unwrap();
    let b: Db<Root> = fast().open(dir.path().join("b.ferro")).unwrap();
    for round in 0..8u64 {
        a.commit(format!("round {round}"), |root| {
            for key in round * 500..(round + 1) * 500 {
                root.insert(key, text(key, 8 << 10));
            }
        })
        .unwrap();
    }
    let have = b.have().unwrap();
    let path = dir.path().join("bundle");
    let (written, sent) = heap_of(|| {
        a.bundle_to(&have, BufWriter::new(File::create(&path).unwrap()))
            .unwrap()
    });
    let (pulled, received) = heap_of(|| {
        let input = BufReader::new(File::open(&path).unwrap());
        b.receive_from("a", input, refuse).unwrap()
    });
    let objects = pulled.objects as usize;
    println!(
        "{written} bytes in {objects} objects, sent with {sent} bytes of heap, received with {received}"
    );
    assert!(written > 30 * MIB as u64, "{written}");
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(b.head_hash(), a.head_hash());
    assert!(
        sent < 4 * MIB,
        "sending {written} bytes took {sent} bytes of heap"
    );
    assert!(
        received < 6 * MIB + objects * 1024,
        "receiving {written} bytes in {objects} objects took {received} bytes of heap"
    );
}

#[test]
fn a_stream_of_commits_with_long_messages_costs_a_buffer_not_its_messages() {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Root> = fast().open(dir.path().join("a.ferro")).unwrap();
    let b: Db<Root> = fast().open(dir.path().join("b.ferro")).unwrap();
    for round in 0..64u64 {
        a.commit(format!("short {round}"), |root| {
            root.insert(round, text(round, 16));
        })
        .unwrap();
    }
    assert_eq!(
        b.pull(&a, "a", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    for round in 64..320u64 {
        a.commit(text(round, 64 << 10), |root| {
            root.insert(round, text(round, 16));
        })
        .unwrap();
    }
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    let len = bundle.as_bytes().len();
    let (pulled, received) = heap_of(|| b.receive("a", &bundle, refuse).unwrap());
    let objects = pulled.objects as usize;
    println!("{len} bytes in {objects} objects, received with {received} bytes of heap");
    assert!(len > 16 * MIB, "{len}");
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(b.head_hash(), a.head_hash());
    assert!(
        received < 6 * MIB + objects * 1024,
        "receiving {len} bytes of commits took {received} bytes of heap"
    );
    b.verify_typed().unwrap();
}

fn clone_heap(a: &Db<Root>, dir: &std::path::Path) -> (usize, usize, usize) {
    let b: Db<Root> = fast().open(dir.join("b.ferro")).unwrap();
    let have = b.have().unwrap();
    let path = dir.join("bundle");
    let (written, sent) = heap_of(|| {
        a.bundle_to(&have, BufWriter::new(File::create(&path).unwrap()))
            .unwrap()
    });
    let (pulled, received) = heap_of(|| {
        let input = BufReader::new(File::open(&path).unwrap());
        b.receive_from("a", input, refuse).unwrap()
    });
    let objects = pulled.objects as usize;
    println!(
        "{written} bytes in {objects} objects, sent with {sent} bytes of heap, received with {received}"
    );
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(b.head_hash(), a.head_hash());
    (objects, sent, received)
}

#[test]
fn a_clone_of_a_large_tree_holds_a_few_bytes_for_each_object() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let a: Db<Root> = fast().open(dir.path().join("a.ferro")).unwrap();
    for round in 0..8u64 {
        a.commit(format!("round {round}"), |root| {
            for key in round * 500_000..(round + 1) * 500_000 {
                root.insert(key, text(key, 8));
            }
        })
        .unwrap();
    }
    let (objects, sent, received) = clone_heap(&a, dir.path());
    assert!(objects > 480_000, "{objects}");
    assert!(
        sent < 2 * MIB + 8 * objects,
        "sending {objects} objects took {sent} bytes of heap"
    );
    assert!(
        received < 4 * MIB + 8 * objects,
        "receiving {objects} objects took {received} bytes of heap"
    );
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct Len(u64);

impl Summary<u64, String> for Len {
    fn of(_key: &u64, value: &String) -> Self {
        Len(value.len() as u64)
    }

    fn combine(&self, other: &Self) -> Self {
        Len(self.0.wrapping_add(other.0))
    }
}

fn spread_heap(dir: &std::path::Path, copies: u64) -> usize {
    let a: Db<Map<u64, String, Len>> = fast().open(dir.join(format!("a{copies}.ferro"))).unwrap();
    a.update(|root| {
        for index in 0..100u64 {
            let value = text(index * 7_919 + 1, 256 << 10);
            for copy in 0..copies {
                root.insert(index + copy * 1_000_000, value.clone());
            }
            for filler in 0..20u64 {
                root.insert(500_000 + index * 20 + filler, format!("f{filler}"));
            }
        }
    })
    .unwrap();
    let b: Db<Map<u64, String, Len>> = fast().open(dir.join(format!("b{copies}.ferro"))).unwrap();
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    let (pulled, received) = heap_of(|| {
        b.receive("a", &bundle, |_, _, _| -> (String, ()) {
            panic!("no merge expected")
        })
        .unwrap()
    });
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(b.read().summary(), a.read().summary());
    received
}

#[test]
fn a_value_spilled_under_several_keys_is_not_kept_while_its_links_arrive() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let once = spread_heap(dir.path(), 1);
    let thrice = spread_heap(dir.path(), 3);
    println!("received with {once} bytes of heap under one key, {thrice} under three");
    assert!(
        thrice < once + 8 * MIB,
        "100 values of 256 KiB under three keys took {thrice} bytes of heap, under one {once}"
    );
}

#[test]
fn verifying_one_value_spilled_under_many_keys_holds_little() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let db: Db<Map<u64, String, Len>> = fast().open(dir.path().join("v.ferro")).unwrap();
    let shared = "v".repeat(600);
    for chunk in 0..10u64 {
        db.update(|root| {
            for key in chunk * 30_000..(chunk + 1) * 30_000 {
                root.insert(key, shared.clone());
            }
        })
        .unwrap();
    }
    let (verified, heap) = heap_of(|| db.verify_typed().unwrap());
    println!(
        "verified {} objects with {heap} bytes of heap",
        verified.objects
    );
    assert!(
        heap < 16 * MIB,
        "verifying 300,000 keys that share one value took {heap} bytes of heap"
    );
}

#[test]
fn a_clone_of_a_long_history_holds_a_few_bytes_for_each_object_and_more_for_each_commit() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let a: Db<Root> = fast().open(dir.path().join("a.ferro")).unwrap();
    for round in 0..50_000u64 {
        a.commit(format!("round {round}"), |root| {
            for key in round * 8..(round + 1) * 8 {
                root.insert(key, text(key, 8));
            }
        })
        .unwrap();
    }
    let (objects, sent, received) = clone_heap(&a, dir.path());
    assert!(objects > 380_000, "{objects}");
    assert!(
        sent < 4 * MIB + 8 * objects + 200 * 50_000,
        "sending {objects} objects in 50,000 commits took {sent} bytes of heap"
    );
    assert!(
        received < 4 * MIB + 8 * objects + 250 * 50_000,
        "receiving {objects} objects in 50,000 commits took {received} bytes of heap"
    );
}

#[derive(Data, Clone, Default)]
struct Widened {
    counts: Map<String, u32>,
    wide: Map<String, u64>,
}

fn widen(_: &mut Widened, _: &Snapshot<Widened>, _: &Snapshot<Widened>) -> (String, ()) {
    panic!("no merge expected")
}

fn widened_heap(dir: &std::path::Path, keys: u64, pad: usize) -> usize {
    let open = |name: &str| -> Db<Widened> {
        fast()
            .cache_bytes(1 << 20)
            .open(dir.join(format!("{name}{pad}.ferro")))
            .unwrap()
    };
    let (laptop, phone) = (open("laptop"), open("phone"));
    let filler = "k".repeat(pad);
    laptop
        .commit("count", |root| {
            for key in 0..keys {
                root.counts.insert(format!("{key:012}{filler}"), key as u32);
            }
        })
        .unwrap();
    phone.pull(&laptop, "laptop", widen).unwrap();
    laptop
        .commit("widen", |root| {
            root.wide = root
                .counts
                .iter()
                .map(|(key, value)| (key, u64::from(*value)))
                .collect();
            root.counts = Map::new();
        })
        .unwrap();
    let bundle = laptop.bundle(&phone.have().unwrap()).unwrap();
    let (pulled, heap) = heap_of(|| phone.receive("laptop", &bundle, widen).unwrap());
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(phone.read().wide.len() as u64, keys);
    heap
}

#[test]
fn checking_held_data_linked_as_another_type_holds_no_more_for_long_keys() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let short = widened_heap(dir.path(), 40_000, 8);
    let long = widened_heap(dir.path(), 40_000, 2_048);
    println!(
        "checked a copy of 40,000 keys with {short} bytes of heap for 20-byte keys, {long} for 2 KiB"
    );
    assert!(
        long < short + 8 * MIB,
        "checking a copy of 40,000 keys of 2 KiB took {long} bytes of heap, of 20 bytes {short}"
    );
}

#[test]
fn a_receive_whose_file_a_cycle_replaced_before_it_spilled_holds_a_buffer_not_the_bundle() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let a: Db<Root> = fast().open(dir.path().join("a.ferro")).unwrap();
    let b: Db<Root> = fast().open(dir.path().join("b.ferro")).unwrap();
    a.update(|root| {
        for key in 0..10 {
            root.insert(key, text(key, 1_500));
        }
    })
    .unwrap();
    b.pull(&a, "a", refuse).unwrap();
    a.update(|root| {
        for key in 10..12_000 {
            root.insert(key, text(key, 1_500));
        }
    })
    .unwrap();
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    drop(b);
    let source = std::fs::read(dir.path().join("b.ferro")).unwrap();
    let mut peaks = Vec::new();
    for swap in [false, true] {
        let path = dir.path().join(format!("swap-{swap}.ferro"));
        std::fs::write(&path, &source).unwrap();
        let b: Db<Root> = fast()
            .reclaim(Reclaim { every_bytes: 0 })
            .open(&path)
            .unwrap();
        let base = Cell::new(0);
        let input = Tripwire::new(bundle.as_bytes(), 256 << 10, || {
            if swap {
                let db = b.clone();
                std::thread::spawn(move || db.compact(Keep::All))
                    .join()
                    .unwrap()
                    .unwrap();
            }
            let live = LIVE.with(Cell::get);
            HIGH.with(|high| high.set(live));
            base.set(live);
        });
        let pulled = b.receive_from("a", input, refuse).unwrap();
        peaks.push(HIGH.with(Cell::get) - base.get());
        assert_eq!(pulled.outcome, Outcome::FastForward);
        assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
        b.verify_typed().unwrap();
    }
    let len = bundle.as_bytes().len();
    println!(
        "{len} bytes received with {} bytes of heap in place, {} once a cycle replaced the file",
        peaks[0], peaks[1]
    );
    assert!(len > 16 * MIB, "{len}");
    assert!(
        peaks[1] < 8 * MIB,
        "a receive whose file a cycle replaced took {} bytes of heap for {len} bytes, against {} \
         in place",
        peaks[1],
        peaks[0]
    );
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u64,
    #[index]
    n: u32,
}

#[derive(Data, Clone, Default)]
struct Shelves {
    tables: Vec<Table<Row>>,
}

fn shelved(_: &mut Shelves, _: &Snapshot<Shelves>, _: &Snapshot<Shelves>) -> (String, ()) {
    panic!("no merge expected")
}

#[test]
fn a_pull_of_many_versions_holds_the_tables_that_changed_between_them() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let open = |name: &str| -> Db<Shelves> { fast().open(dir.path().join(name)).unwrap() };
    let (sender, receiver) = (open("sender.ferro"), open("receiver.ferro"));
    let tables = 20_000u64;
    sender
        .commit("fill", |root| {
            root.tables = (0..tables).map(|_| Table::new()).collect();
        })
        .unwrap();
    receiver.pull(&sender, "sender", shelved).unwrap();
    for at in 0..50u64 {
        sender
            .commit(format!("edit {at}"), |root| {
                root.tables[(at * 7_919 % tables) as usize].insert(Row {
                    id: at,
                    n: at as u32,
                });
            })
            .unwrap();
    }
    let bundle = sender.bundle(&receiver.have().unwrap()).unwrap();
    let len = bundle.as_bytes().len();
    let (pulled, heap) = heap_of(|| receiver.receive("sender", &bundle, shelved).unwrap());
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(receiver.head_hash(), sender.head_hash());
    println!("a pull of 50 versions of {tables} tables, {len} bytes, took {heap} bytes of heap");
    assert!(
        heap < 8 * len + 16 * MIB,
        "a pull of 50 versions of {tables} tables, {len} bytes, took {heap} bytes of heap"
    );
}
