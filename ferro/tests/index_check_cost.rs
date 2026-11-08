mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use common::fast;
use ferro::codec::{Decoder, Encoder, HashSink, Object, field_tag};
use ferro::{Bundle, Data, Db, Map, Ptr, Record, Snapshot, Table};

struct Counted;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to `System` with the caller's layout, pointer and size unchanged
// and returns what `System` returns, so `System`'s guarantees carry over. Besides that it only adds
// to the atomic `ALLOCATED`, which neither allocates nor panics.
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller meets `GlobalAlloc::alloc`'s contract for `layout`, which is passed on
        // to `System` unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout`, and every block this allocator hands out came from `System` with that layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller meets `GlobalAlloc::alloc_zeroed`'s contract for `layout`, which is
        // passed on to `System` unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(size, Ordering::Relaxed);
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout` and that `size` is a valid new size for it. The block came from `System` with
        // that layout, and all three are passed on unchanged.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counted = Counted;

const MIB: usize = 1 << 20;

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u64,
    #[index]
    n: u32,
}

#[derive(Data, Clone, Default)]
struct Mine {
    blob: Vec<u64>,
    rows: Table<Row>,
}

#[derive(Data, Clone, Default)]
struct MineMap {
    blob: Vec<u64>,
    rows: Map<u64, Row>,
}

#[derive(Clone)]
struct RawMap {
    count: u64,
    ptr: Ptr,
}

impl Data for RawMap {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.varint(self.count);
        w.link(self.ptr);
        Ok(())
    }

    fn decode(_: &mut Decoder<'_>) -> ferro::Result<Self> {
        Err(ferro::Error::Decode("a raw map is only written".into()))
    }
}

#[derive(Clone)]
struct ShadowTable {
    rows: RawMap,
}

impl Data for ShadowTable {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        let print = RowTable::__fingerprints().get(0);
        let empty: Map<(u32, u64), ()> = Map::new();
        let indexes = |w: &mut Encoder<'_>| -> ferro::Result<()> {
            w.field(field_tag("indexes"), |w| {
                w.raw(&print.to_le_bytes());
                empty.encode(w)
            })
        };
        let rows = |w: &mut Encoder<'_>| w.field(field_tag("rows"), |w| self.rows.encode(w));
        w.varint(2);
        if field_tag("rows") < field_tag("indexes") {
            rows(w)?;
            indexes(w)
        } else {
            indexes(w)?;
            rows(w)
        }
    }

    fn decode(_: &mut Decoder<'_>) -> ferro::Result<Self> {
        Err(ferro::Error::Decode(
            "a shadow table is only written".into(),
        ))
    }
}

#[derive(Clone)]
struct PeerTable {
    blob: Vec<u64>,
    rows: ShadowTable,
}

impl Data for PeerTable {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.varint(2);
        let blob = |w: &mut Encoder<'_>| w.field(field_tag("blob"), |w| self.blob.encode(w));
        let rows = |w: &mut Encoder<'_>| w.field(field_tag("rows"), |w| self.rows.encode(w));
        if field_tag("blob") < field_tag("rows") {
            blob(w)?;
            rows(w)
        } else {
            rows(w)?;
            blob(w)
        }
    }

    fn decode(_: &mut Decoder<'_>) -> ferro::Result<Self> {
        Err(ferro::Error::Decode("a peer table is only written".into()))
    }
}

#[derive(Clone)]
struct PeerMap {
    blob: Vec<u64>,
    rows: RawMap,
}

impl Data for PeerMap {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.varint(2);
        let blob = |w: &mut Encoder<'_>| w.field(field_tag("blob"), |w| self.blob.encode(w));
        let rows = |w: &mut Encoder<'_>| w.field(field_tag("rows"), |w| self.rows.encode(w));
        if field_tag("blob") < field_tag("rows") {
            blob(w)?;
            rows(w)
        } else {
            rows(w)?;
            blob(w)
        }
    }

    fn decode(_: &mut Decoder<'_>) -> ferro::Result<Self> {
        Err(ferro::Error::Decode("a peer map is only written".into()))
    }
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn object_of<D: Data>(value: &D) -> Object {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder).unwrap();
    encoder.into_object()
}

fn link_to(object: &Object) -> Ptr {
    Ptr {
        hash: object.hash(),
        offset: 0,
    }
}

fn node(first: u64, keys: u64, child: Option<(&Object, u64)>) -> Object {
    let mut body = vec![0u8];
    varint(&mut body, keys);
    for key in first..first + keys {
        varint(&mut body, key);
    }
    for key in first..first + keys {
        body.push(0);
        body.extend_from_slice(
            &object_of(&Row {
                id: key,
                n: key as u32,
            })
            .body,
        );
    }
    let mut links = Vec::new();
    for _ in 0..=keys {
        match child {
            Some((child, count)) => {
                varint(&mut body, count);
                links.push(link_to(child));
            }
            None => body.push(0),
        }
    }
    Object { body, links }
}

fn entry(root: &Object, version: u64, prev: Ptr) -> Object {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    version.encode(&mut encoder).unwrap();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    (now.as_millis() as u64).encode(&mut encoder).unwrap();
    String::new().encode(&mut encoder).unwrap();
    Some(link_to(root)).encode(&mut encoder).unwrap();
    None::<u64>.encode(&mut encoder).unwrap();
    encoder.u8(1);
    encoder.link(prev);
    encoder.into_object()
}

fn bundle(tip: &Object, base: [u8; 20], objects: &[(&Object, &[u64])]) -> Bundle {
    let mut out = b"FERROBDL".to_vec();
    out.extend_from_slice(&[1, 1]);
    out.extend_from_slice(&tip.hash());
    varint(&mut out, 2);
    varint(&mut out, 1);
    out.extend_from_slice(&base);
    varint(&mut out, 1);
    varint(&mut out, 0);
    varint(&mut out, 0);
    varint(&mut out, objects.len() as u64);
    for (object, references) in objects {
        varint(&mut out, (object.body.len() as u64) << 1);
        out.extend_from_slice(&object.body);
        varint(&mut out, object.links.len() as u64);
        for (linked, reference) in object.links.iter().zip(references.iter()) {
            out.extend_from_slice(&linked.hash);
            varint(&mut out, *reference);
        }
    }
    Bundle::from(out)
}

fn refuse<T>(_: &mut T, _: &Snapshot<T>, _: &Snapshot<T>) -> (String, ()) {
    ("merge".to_string(), ())
}

fn forged(table: bool, dir: &std::path::Path) -> usize {
    let leaf = node(1, 10, None);
    let middle = node(100, 99, Some((&leaf, 1 << 20)));
    let top = node(1000, 999, Some((&middle, 1 << 30)));
    let rows = RawMap {
        count: 1 << 40,
        ptr: link_to(&top),
    };
    let blob: Vec<u64> = (0..100_000u64)
        .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .collect();
    let path = dir.join(if table { "table.ferro" } else { "map.ferro" });
    let root = if table {
        let mine: Db<Mine> = fast().open(&path).unwrap();
        mine.update(|root| {
            root.rows.insert(Row { id: 1, n: 5 });
        })
        .unwrap();
        object_of(&PeerTable {
            blob,
            rows: ShadowTable { rows },
        })
    } else {
        let mine: Db<MineMap> = fast().open(&path).unwrap();
        mine.update(|root| {
            root.rows.insert(1, Row { id: 1, n: 5 });
        })
        .unwrap();
        object_of(&PeerMap { blob, rows })
    };
    let base = if table {
        fast()
            .open::<Mine>(&path)
            .unwrap()
            .log()
            .hash_at(&1)
            .unwrap()
    } else {
        fast()
            .open::<MineMap>(&path)
            .unwrap()
            .log()
            .hash_at(&1)
            .unwrap()
    };
    let tip = entry(
        &root,
        2,
        Ptr {
            hash: base,
            offset: 0,
        },
    );
    let top_refs = vec![0u64; top.links.len()];
    let middle_refs = vec![0u64; middle.links.len()];
    let sent = bundle(
        &tip,
        base,
        &[
            (&leaf, &[]),
            (&middle, &middle_refs),
            (&top, &top_refs),
            (&root, &[0]),
            (&tip, &[0, 0]),
        ],
    );
    let before = ALLOCATED.load(Ordering::Relaxed);
    let refused = if table {
        let mine: Db<Mine> = fast().open(&path).unwrap();
        mine.receive("evil", &sent, refuse)
            .map(|pulled| pulled.outcome)
    } else {
        let mine: Db<MineMap> = fast().open(&path).unwrap();
        mine.receive("evil", &sent, refuse)
            .map(|pulled| pulled.outcome)
    };
    let allocated = ALLOCATED.load(Ordering::Relaxed) - before;
    match refused {
        Err(ferro::Error::Bundle(message)) => {
            assert!(message.contains("does not decode"), "{message}")
        }
        other => panic!("a forged map of rows landed: {other:?}"),
    }
    allocated
}

#[test]
fn a_forged_table_whose_rows_link_one_node_many_times_costs_what_a_map_does() {
    let dir = tempfile::tempdir().unwrap();
    let map = forged(false, dir.path());
    let table = forged(true, dir.path());
    assert!(
        table < 4 * map + 16 * MIB,
        "the receive allocated {} MiB for the table and {} MiB for the map",
        table / MIB,
        map / MIB
    );
}
