use std::fmt::Debug;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferro::admin::{self, Keep};
use ferro::codec::{Decoder, Encoder, Ptr};
use ferro::db::Hook;
use ferro::store::{Store, StoreOptions, Txn};
use ferro::{Data, Db, Durability, Error, Map, Options, Reclaim, Record, Table};
use tempfile::TempDir;

type Root = Map<u64, String>;
type Counters = Map<u8, i64>;

const SLOT: usize = 4096;
const LOG_START: u64 = 2 * SLOT as u64;
const ANCHOR: usize = 68;
const SUPER_CHECKED: usize = 112;

const HAS_PREV: u8 = 1;
const HAS_SKIP: u8 = 2;
const MERGED: u8 = 16;
const GRAPH: u8 = 32;

#[derive(Clone)]
struct Head {
    version: u64,
    first: u64,
    tip: Ptr,
}

impl Data for Head {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.version.encode(w)?;
        self.first.encode(w)?;
        w.u8(0);
        w.u8(1);
        w.link(self.tip);
        w.varint(0);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        let version = u64::decode(r)?;
        let first = u64::decode(r)?;
        Option::<Ptr>::decode(r)?;
        let tip = Option::<Ptr>::decode(r)?.ok_or(Error::Decode("no tip".into()))?;
        Map::<String, (u64, u64)>::decode(r)?;
        Ok(Head {
            version,
            first,
            tip,
        })
    }
}

#[derive(Default)]
struct Entry {
    version: u64,
    time_ms: u64,
    prev: Option<Ptr>,
    skip: Option<Ptr>,
    merged: Option<(Ptr, u64, u64)>,
    graph: (u64, u64, u64),
}

impl Entry {
    fn write(&self, txn: &mut ferro::store::Txn) -> Ptr {
        let mut w = Encoder::new(txn);
        w.varint(self.version);
        w.varint(self.time_ms);
        w.raw(&[0, 0, 0]);
        let wide = self.graph != (0, 0, 0);
        w.u8(u8::from(self.prev.is_some()) * HAS_PREV
            + u8::from(self.skip.is_some()) * HAS_SKIP
            + u8::from(self.merged.is_some()) * MERGED
            + u8::from(wide) * GRAPH);
        if let Some((_, floor, after)) = self.merged {
            w.varint(floor);
            w.varint(after + 1);
        }
        if wide {
            w.varint(self.graph.0);
            w.varint(self.graph.1);
            w.varint(self.graph.2);
        }
        for link in [self.prev, self.skip, self.merged.map(|(ptr, _, _)| ptr)]
            .into_iter()
            .flatten()
        {
            w.link(link);
        }
        w.finish().unwrap()
    }
}

#[derive(Clone, Default)]
struct Opaque;

impl Data for Opaque {
    fn encode(&self, _w: &mut Encoder<'_>) -> ferro::Result<()> {
        Ok(())
    }

    fn decode(_r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Opaque)
    }
}

fn fixture(name: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fuzzed.ferro");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("fuzz-{name}.db"));
    fs::copy(source, &path).unwrap();
    (dir, path)
}

fn corrupt<T: Debug>(result: ferro::Result<T>) {
    assert!(
        matches!(result, Err(Error::Corrupt(_))),
        "expected a corruption error, got {result:?}"
    );
}

fn read_only() -> Options {
    Options::new().read_only(true)
}

#[cfg(target_os = "linux")]
fn closed(path: &Path) -> bool {
    let real = fs::canonicalize(path).unwrap();
    !fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .any(|entry| fs::read_link(entry.path()).is_ok_and(|target| target == real))
}

#[cfg(windows)]
fn closed(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .is_ok()
}

fn field(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn newest_slot(bytes: &[u8]) -> usize {
    if field(bytes, SLOT + 8) > field(bytes, 8) {
        SLOT
    } else {
        0
    }
}

fn newest_head(bytes: &[u8]) -> usize {
    field(bytes, newest_slot(bytes) + 24) as usize
}

fn reseal(bytes: &mut [u8], slot: usize, edit: impl FnOnce(&mut [u8])) {
    edit(&mut bytes[slot..slot + SUPER_CHECKED]);
    let check = blake3::hash(&bytes[slot..slot + SUPER_CHECKED]);
    bytes[slot + SUPER_CHECKED..slot + SUPER_CHECKED + 32].copy_from_slice(check.as_bytes());
}

fn buffered() -> StoreOptions {
    StoreOptions {
        durability: Durability::Buffered,
        ..StoreOptions::default()
    }
}

fn failure(f: impl FnOnce()) -> Option<String> {
    let payload = panic::catch_unwind(AssertUnwindSafe(f)).err()?;
    Some(match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map_or_else(String::new, |text| text.to_string()),
    })
}

fn fails_as_corrupt(f: impl FnOnce()) {
    let message = failure(f);
    assert!(
        message
            .as_deref()
            .is_some_and(|text| text.starts_with("ferro:")),
        "expected a ferro panic, got {message:?}"
    );
}

fn crafted(build: impl FnOnce(&mut Txn) -> Ptr) -> (TempDir, Store, Arc<Root>) {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = Store::open(&dir.path().join("crafted.ferro"), buffered()).unwrap();
    let mut txn = store.begin().unwrap();
    let head = build(&mut txn);
    txn.commit(head).unwrap();
    let map = store.load::<Root>(head).unwrap();
    (dir, store, map)
}

fn node(txn: &mut Txn, level: u8, keys: &[u64], children: &[(usize, u64, Ptr)]) -> Ptr {
    let mut w = Encoder::new(txn);
    w.u8(level);
    w.varint(keys.len() as u64);
    for key in keys {
        key.encode(&mut w).unwrap();
    }
    for key in keys {
        w.u8(0);
        key.to_string().encode(&mut w).unwrap();
    }
    for position in 0..=keys.len() {
        match children.iter().find(|(at, _, _)| *at == position) {
            Some(&(_, count, child)) => {
                w.varint(count);
                w.link(child);
            }
            None => w.varint(0),
        }
    }
    w.finish().unwrap()
}

fn map_root(txn: &mut Txn, count: u64, top: Ptr) -> Ptr {
    let mut w = Encoder::new(txn);
    w.varint(count);
    w.link(top);
    w.finish().unwrap()
}

#[test]
fn a_superblock_at_the_last_sequence_number_ends_the_scan_without_overflow() {
    let (_dir, path) = fixture("seq-overflow");
    corrupt(Db::<Root>::open(&path));
}

#[test]
fn refresh_refuses_a_record_that_runs_past_the_file() {
    let (_dir, path) = fixture("refresh-alloc");
    corrupt(read_only().open::<Root>(&path));
    let mut bytes = fs::read(&path).unwrap();
    reseal(&mut bytes, 0, |sb| {
        sb[ANCHOR + 8..ANCHOR + 16].copy_from_slice(&LOG_START.to_le_bytes());
    });
    fs::write(&path, &bytes).unwrap();
    let db = read_only().open::<Root>(&path).unwrap();
    corrupt(db.refresh());
}

#[test]
fn a_record_length_that_overflows_the_offset_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.ferro");
    let db = Db::<Root>::open(&path).unwrap();
    db.commit("one", |root| {
        root.insert(1, "one".into());
    })
    .unwrap();
    drop(db);
    let mut bytes = fs::read(&path).unwrap();
    let head = newest_head(&bytes);
    let mut header = Vec::new();
    let mut len = u64::MAX - 100;
    while len >= 0x80 {
        header.push(len as u8 | 0x80);
        len >>= 7;
    }
    header.extend([len as u8, 0]);
    bytes[head..head + header.len()].copy_from_slice(&header);
    fs::write(&path, &bytes).unwrap();
    corrupt(read_only().open::<Root>(&path));
    corrupt(Db::<Root>::open(&path));
}

#[test]
fn a_branch_that_keeps_versions_out_of_order_is_corrupt() {
    let (_dir, path) = fixture("chain-len");
    corrupt(read_only().open::<Opaque>(&path));
    corrupt(admin::info(&path));
}

#[test]
fn a_branch_whose_first_version_is_past_its_last_is_corrupt() {
    let (_dir, path) = fixture("chain-bounds");
    corrupt(read_only().open::<Root>(&path));
    corrupt(Db::<Root>::open(&path));
    corrupt(admin::layout(&path, Keep::Last(2)));
    corrupt(admin::info(&path));
}

#[test]
fn keeping_history_since_a_time_reaches_the_last_version_without_overflow() {
    let (_dir, path) = fixture("first-since");
    corrupt(admin::layout(&path, Keep::Since(1)));
}

#[test]
fn verify_reports_a_damaged_branch_table() {
    let (_dir, path) = fixture("verify-refs");
    let db = Db::<Opaque>::open(&path).unwrap();
    corrupt(db.verify());
}

#[test]
fn info_reports_a_damaged_branch_table() {
    let (_dir, path) = fixture("info-refs");
    corrupt(admin::info(&path));
    corrupt(admin::verify(&path));
    corrupt(admin::layout(&path, Keep::Last(2)));
}

#[test]
fn common_ancestor_reports_a_damaged_merge_record() {
    let (_dir, path) = fixture("ancestor-merged");
    let db = read_only().open::<Counters>(&path).unwrap();
    let mut failures = 0;
    for name in db.branches() {
        let Ok(branch) = db.branch(&name) else {
            continue;
        };
        for result in [db.common_ancestor(&branch), branch.common_ancestor(&db)] {
            if let Err(error) = result {
                assert!(matches!(error, Error::Corrupt(_)), "{error}");
                failures += 1;
            }
        }
    }
    assert!(failures > 0);
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn admin_reads_close_the_file_when_they_fail() {
    let (_dir, path) = fixture("info-refs");
    corrupt(admin::info(&path));
    corrupt(admin::layout(&path, Keep::All));
    corrupt(admin::layout(&path, Keep::Last(2)));
    assert!(closed(&path));
}

#[test]
fn info_rejects_a_branch_table_deeper_than_its_levels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deep.ferro");
    let options = StoreOptions {
        durability: Durability::Buffered,
        ..StoreOptions::default()
    };
    let (store, _) = Store::open(&path, options).unwrap();
    let mut txn = store.begin().unwrap();
    let depth = 50_000;
    let mut below: Option<Ptr> = None;
    for at in 0..depth {
        let name = format!("b{at}");
        let mut node = Encoder::new(&mut txn);
        node.u8(0);
        node.varint(1);
        node.varint(name.len() as u64);
        node.raw(name.as_bytes());
        node.u8(0);
        node.raw(&[0; 5]);
        match below {
            Some(ptr) => {
                node.varint(at);
                node.link(ptr);
            }
            None => node.varint(0),
        }
        node.varint(0);
        below = Some(node.finish().unwrap());
    }
    let mut refs = Encoder::new(&mut txn);
    refs.varint(depth);
    refs.link(below.unwrap());
    let head = refs.finish().unwrap();
    txn.commit(head).unwrap();
    store.close();
    drop(store);
    let info = admin::info(&path).map(|info| info.branches.len());
    assert!(matches!(info, Err(Error::Decode(_))), "{info:?}");
}

#[test]
fn a_merge_that_reaches_a_version_zero_entry_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zero.ferro");
    let options = StoreOptions {
        durability: Durability::Buffered,
        ..StoreOptions::default()
    };
    let (store, _) = Store::open(&path, options).unwrap();
    let mut txn = store.begin().unwrap();
    let at = |version, time_ms| Entry {
        version,
        time_ms,
        ..Entry::default()
    };
    let other = at(1, 300).write(&mut txn);
    let zero = Entry {
        merged: Some((other, 1, 0)),
        graph: (1, 0, 0),
        ..at(0, 100)
    }
    .write(&mut txn);
    let one = Entry {
        prev: Some(zero),
        ..at(1, 100)
    }
    .write(&mut txn);
    let old = Entry {
        prev: Some(one),
        ..at(2, 10)
    }
    .write(&mut txn);
    let merged = Entry {
        prev: Some(zero),
        skip: Some(old),
        ..at(3, 100)
    }
    .write(&mut txn);
    let base = at(1, 100).write(&mut txn);
    let main = Entry {
        prev: Some(base),
        merged: Some((merged, 1, 50)),
        graph: (2, 3, 2),
        ..at(2, 100)
    }
    .write(&mut txn);
    let side = at(1, 200).write(&mut txn);
    let mut refs: Map<String, Head> = Map::new();
    refs.insert(
        "main".into(),
        Head {
            version: 2,
            first: 1,
            tip: main,
        },
    );
    refs.insert(
        "side".into(),
        Head {
            version: 1,
            first: 1,
            tip: side,
        },
    );
    let mut encoder = Encoder::new(&mut txn);
    refs.encode(&mut encoder).unwrap();
    let head = encoder.finish().unwrap();
    txn.commit(head).unwrap();
    store.close();
    drop(store);
    let db = read_only().open::<Root>(&path).unwrap();
    let side = db.branch("side").unwrap();
    let found = db.common_ancestor(&side);
    assert!(matches!(found, Err(Error::Decode(_))), "{found:?}");
}

#[test]
fn compact_counts_the_commits_it_drops_without_overflow() {
    let (_dir, path) = fixture("compact-dropped");
    corrupt(admin::compact(&path, Keep::Last(3)));
}

#[test]
fn a_node_whose_child_counts_overflow_is_rejected() {
    let (_dir, _store, map) = crafted(|txn| {
        let low = node(txn, 0, &[1], &[]);
        let high = node(txn, 0, &[3], &[]);
        let half = 1 << 63;
        let top = node(txn, 1, &[2], &[(0, half, low), (1, half, high)]);
        map_root(txn, 1, top)
    });
    let found = map.try_get(&2);
    assert!(matches!(found, Err(Error::Decode(_))), "{found:?}");
}

#[test]
fn inserting_under_a_crafted_count_does_not_overflow() {
    let (_dir, _store, map) = crafted(|txn| {
        let low = node(txn, 0, &[1], &[]);
        let top = node(txn, 1, &[2], &[(0, u64::MAX - 1, low)]);
        map_root(txn, u64::MAX, top)
    });
    let mut edited = (*map).clone();
    fails_as_corrupt(|| {
        edited.insert(0, "zero".into());
    });
}

#[test]
fn a_link_whose_count_disagrees_with_its_node_is_rejected() {
    let (_dir, _store, map) = crafted(|txn| {
        let leaf = node(txn, 0, &[1, 2], &[]);
        map_root(txn, 5, leaf)
    });
    let found = map.try_get(&1);
    assert!(matches!(found, Err(Error::Decode(_))), "{found:?}");
    let (_dir, _store, map) = crafted(|txn| {
        let leaf = node(txn, 0, &[1], &[]);
        let top = node(txn, 1, &[2], &[(0, 7, leaf)]);
        map_root(txn, 8, top)
    });
    assert_eq!(map.try_get(&2).unwrap().as_deref(), Some(&"2".to_string()));
    let found = map.try_get(&1);
    assert!(matches!(found, Err(Error::Decode(_))), "{found:?}");
}

#[test]
fn a_chain_of_nodes_at_one_level_is_rejected_before_it_overflows_the_stack() {
    const DEPTH: u64 = 100_000;
    let (_dir, _store, map) = crafted(|txn| {
        let mut below = node(txn, 0, &[DEPTH - 1], &[]);
        for key in (0..DEPTH - 1).rev() {
            below = node(txn, 0, &[key], &[(1, DEPTH - 1 - key, below)]);
        }
        map_root(txn, DEPTH, below)
    });
    let found = map.try_get(&(DEPTH - 1));
    assert!(matches!(found, Err(Error::Decode(_))), "{found:?}");
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            fails_as_corrupt(|| map.for_each(|_, _| {}));
            fails_as_corrupt(|| {
                map.range_summary(1..DEPTH - 1);
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn shared_children_cannot_claim_more_entries_than_the_file_holds() {
    let (_dir, _store, map) = crafted(|txn| {
        let mut below = node(txn, 0, &[1, 2], &[]);
        let mut count = 2u64;
        for level in 1..=20u8 {
            let low = u64::from(level) * 10;
            let shared = [(0, count, below), (1, count, below), (2, count, below)];
            below = node(txn, level, &[low, low + 1], &shared);
            count = 2 + 3 * count;
        }
        map_root(txn, count, below)
    });
    assert!(map.len() as u64 > 1 << 33);
    let found = map.try_get(&200);
    assert!(matches!(found, Err(Error::Decode(_))), "{found:?}");
    fails_as_corrupt(|| {
        map.iter().next();
    });
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Plain {
    #[key]
    id: u32,
    slug: String,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Indexed {
    #[key]
    id: u32,
    #[index]
    slug: String,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct PlainRows {
    rows: Table<Plain>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct IndexedRows {
    rows: Table<Indexed>,
}

#[test]
fn rebuilding_an_index_over_a_damaged_row_fails_the_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rows.ferro");
    let marker = "Qv8-damaged-row-Zk3";
    let db = Db::<PlainRows>::open(&path).unwrap();
    db.update(|root| {
        root.rows.insert(Plain {
            id: 1,
            slug: marker.to_string(),
        });
    })
    .unwrap();
    drop(db);
    let mut bytes = fs::read(&path).unwrap();
    let found: Vec<usize> = bytes
        .windows(marker.len())
        .enumerate()
        .filter(|(_, window)| *window == marker.as_bytes())
        .map(|(at, _)| at)
        .collect();
    assert_eq!(found.len(), 1);
    bytes[found[0]] ^= 1;
    fs::write(&path, &bytes).unwrap();
    assert!(Db::<PlainRows>::open(&path).is_ok());
    let opened = panic::catch_unwind(|| Db::<IndexedRows>::open(&path).map(drop));
    assert!(matches!(opened, Ok(Err(Error::Corrupt(_)))), "{opened:?}");
}

fn damaged_branch_table(path: &Path, aside: Option<&str>) {
    let (store, _) = Store::open(path, buffered()).unwrap();
    let mut txn = store.begin().unwrap();
    let tip = Entry {
        version: 1,
        time_ms: 100,
        ..Entry::default()
    }
    .write(&mut txn);
    let head = Head {
        version: 1,
        first: 1,
        tip,
    };
    let leaf = aside.map(|name| {
        let mut w = Encoder::new(&mut txn);
        w.u8(0);
        w.varint(1);
        name.to_string().encode(&mut w).unwrap();
        w.u8(0);
        head.encode(&mut w).unwrap();
        w.varint(0);
        w.varint(0);
        w.finish().unwrap()
    });
    let mut w = Encoder::new(&mut txn);
    w.u8(1);
    w.varint(1);
    "main".to_string().encode(&mut w).unwrap();
    w.u8(0);
    head.encode(&mut w).unwrap();
    match leaf {
        Some(leaf) => {
            w.varint(1);
            w.link(leaf);
        }
        None => w.varint(0),
    }
    w.varint(1);
    w.link(Ptr {
        hash: [7; 20],
        offset: tip.offset,
    });
    let top = w.finish().unwrap();
    let count = if aside.is_some() { 3 } else { 2 };
    let head = map_root(&mut txn, count, top);
    txn.commit(head).unwrap();
    store.close();
    drop(store);
}

fn manual() -> Options {
    Options::new().reclaim(Reclaim { every_bytes: 0 })
}

#[test]
fn sync_reports_a_damaged_branch_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refs.ferro");
    damaged_branch_table(&path, None);
    let honest = Db::<Root>::open(dir.path().join("honest.ferro")).unwrap();
    honest
        .commit("one", |root| {
            root.insert(1, "one".into());
        })
        .unwrap();
    let have = honest.have().unwrap();
    let db = read_only().open::<Root>(&path).unwrap();
    corrupt(db.have().map(drop));
    corrupt(db.bundle(&have).map(drop));
}

#[test]
fn pruning_reports_a_damaged_branch_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prune.ferro");
    damaged_branch_table(&path, None);
    let db = manual().open::<Root>(&path).unwrap();
    corrupt(db.compact(Keep::Last(1)));
}

#[test]
fn copying_reports_a_damaged_branch_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("copy.ferro");
    damaged_branch_table(&path, None);
    let db = manual().open::<Root>(&path).unwrap();
    corrupt(db.compact(Keep::All));
}

#[test]
fn dropping_a_branch_reports_a_damaged_branch_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drop.ferro");
    damaged_branch_table(&path, Some("aside"));
    let db = manual().open::<Root>(&path).unwrap();
    db.branch("aside").unwrap();
    corrupt(db.drop_branch("aside"));
}

#[test]
fn swapping_in_a_compacted_file_reports_a_damaged_branch_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swap.ferro");
    let temp = dir.path().join("swap.ferro.compacting");
    let hook = move |step: &'static str| {
        if step == "synced" {
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&temp)
                .unwrap();
            let mut bytes = Vec::new();
            (&file).read_to_end(&mut bytes).unwrap();
            let at = bytes
                .windows(4)
                .position(|window| window == b"main")
                .unwrap();
            (&file).seek(SeekFrom::Start(at as u64)).unwrap();
            (&file).write_all(&[bytes[at] ^ 1]).unwrap();
        }
    };
    let db = manual()
        .hook(Hook(Arc::new(hook)))
        .open::<Root>(&path)
        .unwrap();
    db.commit("one", |root| {
        root.insert(1, "one".into());
    })
    .unwrap();
    corrupt(db.compact(Keep::All));
}

#[test]
fn a_superblock_whose_log_ends_inside_the_header_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("short.ferro");
    let db = Db::<Root>::open(&path).unwrap();
    db.commit("one", |root| {
        root.insert(1, "one".into());
    })
    .unwrap();
    drop(db);
    let mut bytes = fs::read(&path).unwrap();
    let short = (LOG_START / 2).to_le_bytes();
    let slot = newest_slot(&bytes);
    reseal(&mut bytes, slot, |sb| {
        for mark in [8, ANCHOR] {
            sb[mark + 8..mark + 16].copy_from_slice(&short);
            sb[mark + 16..mark + 24].fill(0);
        }
    });
    fs::write(&path, &bytes).unwrap();
    corrupt(read_only().open::<Root>(&path));
    corrupt(Db::<Root>::open(&path));
    assert!(fs::read(&path).unwrap() == bytes, "open rewrote the file");
}

#[test]
fn a_commit_after_the_last_version_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("last.ferro");
    let (store, _) = Store::open(&path, buffered()).unwrap();
    let mut txn = store.begin().unwrap();
    let tip = Entry {
        version: u64::MAX,
        time_ms: 100,
        ..Entry::default()
    }
    .write(&mut txn);
    let mut refs: Map<String, Head> = Map::new();
    refs.insert(
        "main".into(),
        Head {
            version: u64::MAX,
            first: u64::MAX,
            tip,
        },
    );
    let mut encoder = Encoder::new(&mut txn);
    refs.encode(&mut encoder).unwrap();
    let head = encoder.finish().unwrap();
    txn.commit(head).unwrap();
    store.close();
    drop(store);
    let db = Db::<Root>::open(&path).unwrap();
    assert_eq!(db.version(), u64::MAX);
    corrupt(db.commit("next", |root| {
        root.insert(1, "one".into());
    }));
    corrupt(db.update(|root| {
        root.insert(2, "two".into());
    }));
    drop(db);
    let db = read_only().open::<Root>(&path).unwrap();
    assert_eq!(db.version(), u64::MAX);
    assert!(db.read().is_empty());
}

#[test]
fn a_commit_after_the_last_sequence_number_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seq.ferro");
    let db = Db::<Root>::open(&path).unwrap();
    db.commit("one", |root| {
        root.insert(1, "one".into());
    })
    .unwrap();
    drop(db);
    let mut bytes = fs::read(&path).unwrap();
    let last = u64::MAX.to_le_bytes();
    let slot = newest_slot(&bytes);
    reseal(&mut bytes, slot, |sb| {
        sb[8..16].copy_from_slice(&last);
        sb[ANCHOR..ANCHOR + 8].copy_from_slice(&last);
    });
    fs::write(&path, &bytes).unwrap();
    let db = Db::<Root>::open(&path).unwrap();
    assert!(db.read().contains_key(&1));
    corrupt(db.commit("two", |root| {
        root.insert(2, "two".into());
    }));
    drop(db);
    let db = read_only().open::<Root>(&path).unwrap();
    assert_eq!(db.read().len(), 1);
}

#[test]
fn space_estimates_no_more_live_bytes_than_the_file_holds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("basis.ferro");
    let db = Db::<Root>::open(&path).unwrap();
    db.commit("one", |root| {
        root.insert(1, "one".into());
    })
    .unwrap();
    drop(db);
    let honest = fs::read(&path).unwrap();
    let slot = newest_slot(&honest);
    for basis in [LOG_START, u64::MAX] {
        let mut bytes = honest.clone();
        reseal(&mut bytes, slot, |sb| {
            sb[ANCHOR - 16..ANCHOR - 8].copy_from_slice(&basis.to_le_bytes());
            sb[ANCHOR - 8..ANCHOR].copy_from_slice(&u64::MAX.to_le_bytes());
        });
        fs::write(&path, &bytes).unwrap();
        let space = read_only().open::<Root>(&path).unwrap().space().unwrap();
        assert!(space.live_bytes <= space.file_bytes, "{space:?}");
    }
}

#[test]
fn refresh_refuses_a_superblock_whose_log_ends_past_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("follow.ferro");
    let db = Db::<Root>::open(&path).unwrap();
    db.commit("one", |root| {
        root.insert(1, "one".into());
    })
    .unwrap();
    drop(db);
    let mut bytes = fs::read(&path).unwrap();
    let slot = newest_slot(&bytes);
    reseal(&mut bytes, slot, |sb| {
        sb[ANCHOR - 16..ANCHOR - 8].copy_from_slice(&LOG_START.to_le_bytes());
        sb[ANCHOR - 8..ANCHOR].copy_from_slice(&u64::MAX.to_le_bytes());
    });
    fs::write(&path, &bytes).unwrap();
    let follower = read_only().open::<Root>(&path).unwrap();
    reseal(&mut bytes, slot, |sb| {
        sb[16..24].copy_from_slice(&(u64::MAX >> 4).to_le_bytes());
    });
    fs::write(&path, &bytes).unwrap();
    corrupt(follower.refresh());
    assert!(follower.size_on_disk() <= bytes.len() as u64);
    let space = follower.space().unwrap();
    assert!(space.live_bytes <= space.file_bytes, "{space:?}");
    assert!(follower.read().contains_key(&1));
}

#[test]
fn a_cycle_reports_a_branch_table_it_cannot_edit() {
    let (_dir, path) = fixture("prune-insert");
    let db = manual().open::<Opaque>(&path).unwrap();
    corrupt(db.compact(Keep::Last(2)));
}

#[test]
fn a_cycle_counts_the_commits_it_prunes_without_overflow() {
    let (_dir, path) = fixture("prune-dropped");
    let db = manual().open::<Opaque>(&path).unwrap();
    let compacted = db.compact(Keep::Last(2));
    assert!(
        matches!(compacted, Ok(_) | Err(Error::Corrupt(_))),
        "{compacted:?}"
    );
}
