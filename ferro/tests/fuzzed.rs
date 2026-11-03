use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};

use ferro::admin::{self, Keep};
use ferro::codec::{Decoder, Encoder, Ptr};
use ferro::store::{Store, StoreOptions};
use ferro::{Data, Db, Durability, Error, Map, Options};
use tempfile::TempDir;

type Root = Map<u64, String>;
type Counters = Map<u8, i64>;

const SLOT: usize = 4096;

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
        Ok(Head { version, first, tip })
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
        w.u8(
            u8::from(self.prev.is_some()) * HAS_PREV
                | u8::from(self.skip.is_some()) * HAS_SKIP
                | u8::from(self.merged.is_some()) * MERGED
                | u8::from(wide) * GRAPH,
        );
        if let Some((_, floor, after)) = self.merged {
            w.varint(floor);
            w.varint(after + 1);
        }
        if wide {
            w.varint(self.graph.0);
            w.varint(self.graph.1);
            w.varint(self.graph.2);
        }
        for link in [self.prev, self.skip, self.merged.map(|(ptr, _, _)| ptr)].into_iter().flatten() {
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
    Options::new().read_only()
}

fn open_handles(path: &Path) -> usize {
    let real = fs::canonicalize(path).unwrap();
    fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .filter(|entry| fs::read_link(entry.path()).is_ok_and(|target| target == real))
        .count()
}

fn newest_head(bytes: &[u8]) -> usize {
    let field = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let slot = if field(SLOT + 8) > field(8) { SLOT } else { 0 };
    field(slot + 24) as usize
}

#[test]
fn a_superblock_at_the_last_sequence_number_ends_the_scan_without_overflow() {
    let (_dir, path) = fixture("seq-overflow");
    corrupt(Db::<Root>::open(&path));
}

#[test]
fn refresh_refuses_a_record_that_runs_past_the_file() {
    let (_dir, path) = fixture("refresh-alloc");
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

#[test]
fn admin_reads_close_the_file_when_they_fail() {
    let (_dir, path) = fixture("info-refs");
    corrupt(admin::info(&path));
    corrupt(admin::layout(&path, Keep::All));
    corrupt(admin::layout(&path, Keep::Last(2)));
    assert_eq!(open_handles(&path), 0);
}

#[test]
fn info_walks_a_deep_branch_table_without_recursing() {
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
    assert_eq!(admin::info(&path).unwrap().branches.len(), depth as usize);
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
    refs.insert("main".into(), Head { version: 2, first: 1, tip: main });
    refs.insert("side".into(), Head { version: 1, first: 1, tip: side });
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
