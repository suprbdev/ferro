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
const BOUNDED: u8 = 64;

#[derive(Clone, Default)]
struct Head {
    version: u64,
    first: u64,
    root: Option<Ptr>,
    tip: Option<Ptr>,
}

impl Data for Head {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.version.encode(w)?;
        self.first.encode(w)?;
        for link in [self.root, self.tip] {
            w.u8(u8::from(link.is_some()));
            if let Some(link) = link {
                w.link(link);
            }
        }
        w.varint(0);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        let version = u64::decode(r)?;
        let first = u64::decode(r)?;
        let root = Option::<Ptr>::decode(r)?;
        let tip = Option::<Ptr>::decode(r)?;
        Map::<String, (u64, u64)>::decode(r)?;
        Ok(Head {
            version,
            first,
            root,
            tip,
        })
    }
}

#[derive(Default)]
struct Entry {
    version: u64,
    time_ms: u64,
    root: Option<Ptr>,
    prev: Option<Ptr>,
    skip: Option<Ptr>,
    merged: Option<(Ptr, u64, Option<u64>)>,
    graph: (u64, u64, u64),
    bounded: bool,
}

impl Entry {
    fn write(&self, txn: &mut ferro::store::Txn) -> Ptr {
        let mut w = Encoder::new(txn);
        w.varint(self.version);
        w.varint(self.time_ms);
        w.u8(0);
        match self.root {
            Some(root) => {
                w.u8(1);
                w.link(root);
            }
            None => w.u8(0),
        }
        w.u8(0);
        let wide = self.graph != (0, 0, 0);
        w.u8(u8::from(self.prev.is_some()) * HAS_PREV
            + u8::from(self.skip.is_some()) * HAS_SKIP
            + u8::from(self.merged.is_some()) * MERGED
            + u8::from(wide) * GRAPH
            + u8::from(self.bounded) * BOUNDED);
        if let Some((_, floor, after)) = self.merged {
            w.varint(floor);
            w.varint(after.map_or(0, |after| after + 1));
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

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        r.take(r.remaining())?;
        while r.link().is_ok() {}
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

struct Closing(Store);

impl Drop for Closing {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn crafted(build: impl FnOnce(&mut Txn) -> Ptr) -> (TempDir, Closing, Arc<Root>) {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = Store::open(&dir.path().join("crafted.ferro"), buffered()).unwrap();
    let mut txn = store.begin().unwrap();
    let head = build(&mut txn);
    txn.commit(head).unwrap();
    let map = store.load::<Root>(head).unwrap();
    (dir, Closing(store), map)
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
fn a_branch_named_past_its_tip_fails_to_open_before_a_commit_lands_on_it() {
    let (_dir, path) = fixture("tip-past-last");
    let past = |outcome: &ferro::Result<()>| {
        matches!(outcome, Err(Error::Corrupt(message))
            if message.contains("names v112 as its last but its tip is v48"))
    };
    for outcome in [
        Db::<Root>::open(&path).map(|_| ()),
        read_only().open::<Root>(&path).map(|_| ()),
        admin::info(&path).map(|_| ()),
    ] {
        assert!(past(&outcome), "{outcome:?}");
    }
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
        merged: Some((other, 1, Some(0))),
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
        merged: Some((merged, 1, Some(50))),
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
            tip: Some(main),
            ..Head::default()
        },
    );
    refs.insert(
        "side".into(),
        Head {
            version: 1,
            first: 1,
            tip: Some(side),
            ..Head::default()
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
fn compacting_a_null_link_outside_a_log_entry_fails_as_verify_does() {
    let (_dir, path) = fixture("compact-null-link");
    let before = fs::read(&path).unwrap();
    let wrong = admin::verify(&path).unwrap_err().to_string();
    assert!(wrong.contains("pointer 0 outside log"), "{wrong}");
    for threads in [1, 4] {
        for keep in [Keep::All, Keep::Last(1)] {
            let db: Db<Opaque> = Options::new().copy_threads(threads).open(&path).unwrap();
            let outcome = db.compact(keep);
            assert!(
                matches!(&outcome, Err(error) if error.to_string() == wrong),
                "{threads} threads, {keep:?}: {outcome:?}"
            );
            drop(db);
            assert!(
                fs::read(&path).unwrap() == before,
                "{threads} threads, {keep:?}"
            );
        }
    }
    admin::compact(&path, Keep::All).unwrap();
    admin::verify(&path).unwrap();
}

#[test]
fn compacting_a_tree_that_links_a_pruned_log_entry_fails() {
    for threads in [1, 4] {
        let (_dir, path) = fixture("compact-pruned-link");
        let db: Db<Opaque> = Options::new().copy_threads(threads).open(&path).unwrap();
        let outcome = db.compact(Keep::Last(1));
        assert!(
            matches!(&outcome, Err(Error::Corrupt(message))
                if message.starts_with("link to the pruned log entry at ")),
            "{threads} threads: {outcome:?}"
        );
        db.verify().unwrap();
        drop(db);
        admin::verify(&path).unwrap();
    }
}

#[test]
fn a_skip_link_to_another_entry_fails_verify_and_compacting_keeps_the_file() {
    let skips = |outcome: &ferro::Result<_>| {
        matches!(outcome, Err(Error::Corrupt(message))
            if message.contains("v50 skips to another entry than v48"))
    };
    for threads in [1, 4] {
        let (_dir, path) = fixture("compact-skip-link");
        let outcome = admin::verify(&path).map(|_| ());
        assert!(skips(&outcome), "{threads} threads: {outcome:?}");
        let db: Db<Opaque> = Options::new().copy_threads(threads).open(&path).unwrap();
        let outcome = db.verify().map(|_| ());
        assert!(skips(&outcome), "{threads} threads: {outcome:?}");
        let outcome = db.compact(Keep::All).map(|_| ());
        assert!(skips(&outcome), "{threads} threads: {outcome:?}");
        drop(db);
        let outcome = admin::verify(&path).map(|_| ());
        assert!(skips(&outcome), "{threads} threads: {outcome:?}");
    }
}

#[derive(Clone, Copy, Debug)]
enum Skip {
    Honest,
    WrongHash,
    OtherRecord,
}

fn skipping_file(path: &Path, skip: Skip) {
    let (store, _) = Store::open(path, buffered()).unwrap();
    let mut txn = store.begin().unwrap();
    let mut ptrs: Vec<Ptr> = Vec::new();
    for version in 1..=5 {
        let ptr = Entry {
            version,
            time_ms: 100 * version,
            prev: ptrs.last().copied(),
            ..Entry::default()
        }
        .write(&mut txn);
        ptrs.push(ptr);
    }
    let other = Entry {
        version: 4,
        time_ms: 401,
        prev: Some(ptrs[2]),
        ..Entry::default()
    }
    .write(&mut txn);
    let target = match skip {
        Skip::Honest => ptrs[3],
        Skip::WrongHash => Ptr {
            hash: [0; ferro::codec::HASH_BYTES],
            offset: ptrs[4].offset,
        },
        Skip::OtherRecord => other,
    };
    let six = Entry {
        version: 6,
        time_ms: 600,
        prev: Some(ptrs[4]),
        skip: Some(target),
        ..Entry::default()
    }
    .write(&mut txn);
    let head = write_refs(&mut txn, 6, six);
    txn.commit(head).unwrap();
    store.close();
}

#[test]
fn a_skip_link_that_names_another_entry_than_its_version_fails_verify_and_compaction() {
    for skip in [Skip::Honest, Skip::WrongHash, Skip::OtherRecord] {
        for (keep, kept) in [
            (Keep::All, true),
            (Keep::Last(3), true),
            (Keep::Last(2), false),
        ] {
            for threads in [1, 4] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("skip.ferro");
                skipping_file(&path, skip);
                let honest = matches!(skip, Skip::Honest);
                let case = format!("{skip:?} {keep:?} {threads} threads");
                assert_eq!(admin::verify(&path).is_ok(), honest, "{case}");
                let db: Db<Opaque> = Options::new().copy_threads(threads).open(&path).unwrap();
                let outcome = db.compact(keep);
                assert_eq!(outcome.is_ok(), honest || !kept, "{case}: {outcome:?}");
                drop(db);
                assert_eq!(
                    admin::verify(&path).is_ok(),
                    honest || !kept,
                    "{case}: the file after"
                );
            }
        }
    }
}

#[test]
fn a_prev_link_past_a_version_fails_verify_and_compacting_keeps_the_file() {
    let past = |outcome: &ferro::Result<_>| {
        matches!(outcome, Err(Error::Corrupt(message))
            if message.contains("v13 links v1 as the entry before it, not v12"))
    };
    for threads in [1, 4] {
        let (_dir, path) = fixture("compact-prev-link");
        let before = fs::read(&path).unwrap();
        let outcome = admin::verify(&path).map(|_| ());
        assert!(past(&outcome), "{threads} threads: {outcome:?}");
        let db: Db<Opaque> = Options::new().copy_threads(threads).open(&path).unwrap();
        let outcome = db.verify().map(|_| ());
        assert!(past(&outcome), "{threads} threads: {outcome:?}");
        let outcome = db.compact(Keep::All).map(|_| ());
        assert!(past(&outcome), "{threads} threads: {outcome:?}");
        assert!(fs::read(&path).unwrap() == before, "{threads} threads");
        let outcome = db.compact(Keep::Last(17)).map(|_| ());
        assert!(past(&outcome), "{threads} threads, pruned: {outcome:?}");
        drop(db);
        let outcome = admin::verify(&path).map(|_| ());
        assert!(past(&outcome), "{threads} threads: {outcome:?}");
    }
}

#[derive(Clone, Copy, Debug)]
enum Prev {
    Honest,
    Past,
    PastMerge,
}

fn prev_file(path: &Path, prev: Prev) {
    let (store, _) = Store::open(path, buffered()).unwrap();
    let mut txn = store.begin().unwrap();
    let mut ptrs: Vec<Ptr> = Vec::new();
    for version in 1..=4 {
        let ptr = Entry {
            version,
            time_ms: 100 * version,
            prev: ptrs.last().copied(),
            ..Entry::default()
        }
        .write(&mut txn);
        ptrs.push(ptr);
    }
    let below = match prev {
        Prev::Honest => ptrs[3],
        Prev::Past | Prev::PastMerge => ptrs[0],
    };
    let five = Entry {
        version: 5,
        time_ms: 500,
        prev: Some(below),
        ..Entry::default()
    }
    .write(&mut txn);
    let six = Entry {
        version: 6,
        time_ms: 600,
        prev: Some(five),
        skip: Some(ptrs[3]),
        ..Entry::default()
    }
    .write(&mut txn);
    let head = match prev {
        Prev::Honest | Prev::Past => write_refs(&mut txn, 6, six),
        Prev::PastMerge => {
            let base = Entry {
                version: 1,
                time_ms: 50,
                ..Entry::default()
            }
            .write(&mut txn);
            let merge = Entry {
                version: 2,
                time_ms: 700,
                prev: Some(base),
                merged: Some((six, 1, None)),
                graph: (5, 6, 2),
                ..Entry::default()
            }
            .write(&mut txn);
            write_refs(&mut txn, 2, merge)
        }
    };
    txn.commit(head).unwrap();
    store.close();
}

#[test]
fn a_prev_link_past_the_version_below_fails_verify_and_compaction() {
    for prev in [Prev::Honest, Prev::Past] {
        for (keep, kept) in [
            (Keep::All, true),
            (Keep::Last(3), true),
            (Keep::Last(2), false),
        ] {
            for threads in [1, 4] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("prev.ferro");
                prev_file(&path, prev);
                let honest = matches!(prev, Prev::Honest);
                let case = format!("{prev:?} {keep:?} {threads} threads");
                assert_eq!(admin::verify(&path).is_ok(), honest, "{case}");
                let db: Db<Opaque> = Options::new().copy_threads(threads).open(&path).unwrap();
                let outcome = db.compact(keep);
                assert_eq!(outcome.is_ok(), honest || !kept, "{case}: {outcome:?}");
                drop(db);
                assert_eq!(
                    admin::verify(&path).is_ok(),
                    honest || !kept,
                    "{case}: the file after"
                );
            }
        }
    }
}

fn write_side_named(path: &Path, named: u64, first: u64) {
    let (store, _) = Store::open(path, buffered()).unwrap();
    let mut txn = store.begin().unwrap();
    let mut refs: Map<String, Head> = Map::new();
    for (name, version, first, late) in [("main", 5, 1, 0), ("side", named, first, 1)] {
        let mut tip = None;
        for at in 1..=5 {
            let entry = Entry {
                version: at,
                time_ms: 100 * at + late,
                prev: tip,
                ..Entry::default()
            };
            tip = Some(entry.write(&mut txn));
        }
        refs.insert(
            name.into(),
            Head {
                version,
                first,
                tip,
                ..Head::default()
            },
        );
    }
    let mut encoder = Encoder::new(&mut txn);
    refs.encode(&mut encoder).unwrap();
    let head = encoder.finish().unwrap();
    txn.commit(head).unwrap();
    store.close();
}

#[test]
fn a_head_whose_last_version_is_not_its_tips_fails_verify_and_compaction() {
    for (named, threads) in [(6, 1), (6, 4), (4, 1), (4, 4)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tip.ferro");
        write_side_named(&path, named, 1);
        let wrong = |outcome: &ferro::Result<()>| {
            matches!(outcome, Err(Error::Corrupt(message))
                if message.contains(&format!("names v{named} as its last but its tip is v5")))
        };
        let case = format!("v{named} named, {threads} threads");
        let outcome = admin::verify(&path).map(|_| ());
        assert!(wrong(&outcome), "{case}: {outcome:?}");
        let db = Options::new()
            .copy_threads(threads)
            .open::<Opaque>(&path)
            .unwrap();
        let outcome = db.branch("side").map(|_| ());
        assert!(wrong(&outcome), "{case}: {outcome:?}");
        let outcome = db.compact(Keep::All).map(|_| ());
        assert!(wrong(&outcome), "{case}: {outcome:?}");
        drop(db);
        let outcome = admin::verify(&path).map(|_| ());
        assert!(wrong(&outcome), "{case}: the file after: {outcome:?}");
    }
}

#[test]
fn a_head_that_keeps_versions_past_its_tip_takes_no_commit_and_fails_pruning() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("past.ferro");
    write_side_named(&path, 8, 7);
    let past = |outcome: &ferro::Result<()>| {
        matches!(outcome, Err(Error::Corrupt(message))
            if message.contains("names v8 as its last but its tip is v5"))
    };
    let db = Db::<Root>::open(&path).unwrap();
    let outcome = db
        .branch("side")
        .and_then(|side| {
            side.update(|root| {
                root.insert(1, "one".into());
            })
        })
        .map(|_| ());
    assert!(past(&outcome), "{outcome:?}");
    for keep in [Keep::Last(2), Keep::All] {
        corrupt(db.compact(keep));
    }
    drop(db);
    corrupt(admin::compact(&path, Keep::Last(2)));
    assert!(past(&admin::info(&path).map(|_| ())));
}

#[test]
fn a_merged_chain_whose_prev_link_passes_a_version_fails_verify_and_compaction() {
    let past = |outcome: &ferro::Result<_>| {
        matches!(outcome, Err(Error::Corrupt(message))
            if message.contains("v5 links v1 as the entry before it, not v4"))
    };
    for threads in [1, 4] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prev.ferro");
        prev_file(&path, Prev::PastMerge);
        let outcome = admin::verify(&path).map(|_| ());
        assert!(past(&outcome), "{threads} threads: {outcome:?}");
        let db: Db<Opaque> = Options::new().copy_threads(threads).open(&path).unwrap();
        let outcome = db.compact(Keep::All).map(|_| ());
        assert!(past(&outcome), "{threads} threads: {outcome:?}");
        drop(db);
        let outcome = admin::verify(&path).map(|_| ());
        assert!(past(&outcome), "{threads} threads: {outcome:?}");
    }
}

#[test]
fn a_damaged_tree_handed_to_the_copy_threads_fails_before_a_later_error_of_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("order.ferro");
    let mut damaged = None;
    built(&source, |txn| {
        let mut w = Encoder::new(&mut *txn);
        w.raw(b"handed out");
        let child = w.finish().unwrap();
        let mut w = Encoder::new(&mut *txn);
        w.raw(b"read later");
        let other = w.finish().unwrap();
        damaged = Some(child.offset);
        let mut w = Encoder::new(&mut *txn);
        w.raw(b"tree");
        w.link(Ptr {
            hash: [9; ferro::codec::HASH_BYTES],
            ..child
        });
        let tree = w.finish().unwrap();
        let main = chain(txn, &[10], &[]);
        let aside = Entry {
            version: 2,
            time_ms: 20,
            root: Some(tree),
            prev: Some(Ptr {
                offset: other.offset,
                ..main[0]
            }),
            ..Entry::default()
        }
        .write(txn);
        let root = value(txn, 1);
        let heads = [
            ("aside", on(2, 2, tree, aside)),
            ("main", on(1, 1, root, main[0])),
        ];
        write_heads(txn, &heads)
    });
    let first = format!("hash mismatch for object at {}", damaged.unwrap());
    for threads in [1, 2, 4] {
        let path = dir.path().join(format!("{threads}.ferro"));
        fs::copy(&source, &path).unwrap();
        let db: Db<Opaque> = Options::new()
            .durability(Durability::Buffered)
            .copy_threads(threads)
            .open(&path)
            .unwrap();
        let outcome = db.compact(Keep::All);
        assert!(
            matches!(&outcome, Err(Error::Corrupt(message)) if *message == first),
            "{threads} threads: {outcome:?}"
        );
    }
}

#[test]
fn a_fuzzed_merged_chain_that_skips_past_where_its_prev_links_stop_fails_on_any_thread_count() {
    for (keep, message) in [
        (
            Keep::Since(0),
            "v20 skips to v16, past where the history below v18 was cut",
        ),
        (
            Keep::Last(3),
            "v20 skips past v18, where the history it keeps stops, to v16",
        ),
    ] {
        for threads in [1, 2, 4] {
            let (_dir, path) = fixture("compact-skip-past-cut");
            let db: Db<Opaque> = Options::new()
                .durability(Durability::Buffered)
                .copy_threads(threads)
                .open(&path)
                .unwrap();
            let outcome = db.compact(keep);
            assert!(
                matches!(&outcome, Err(Error::Corrupt(found)) if found.ends_with(message)),
                "{keep:?}, {threads} threads: {outcome:?}"
            );
        }
    }
}

fn write_refs(txn: &mut Txn, version: u64, tip: Ptr) -> Ptr {
    let mut refs: Map<String, Head> = Map::new();
    refs.insert(
        "main".into(),
        Head {
            version,
            first: 1,
            tip: Some(tip),
            ..Head::default()
        },
    );
    let mut encoder = Encoder::new(txn);
    refs.encode(&mut encoder).unwrap();
    encoder.finish().unwrap()
}

#[derive(Clone, Copy, PartialEq)]
enum Linked {
    Itself,
    Again,
    Cut,
}

fn entry_linked_from_a_tree(path: &Path, linked: Linked) -> Ptr {
    let at = |version, time_ms| Entry {
        version,
        time_ms,
        ..Entry::default()
    };
    let (store, _) = Store::open(path, buffered()).unwrap();
    let mut txn = store.begin().unwrap();
    let one = at(1, 100).write(&mut txn);
    let two = Entry {
        prev: Some(one),
        ..at(2, 200)
    }
    .write(&mut txn);
    let head = write_refs(&mut txn, 2, two);
    txn.commit(head).unwrap();
    store.close();
    drop(store);
    let (store, _) = Store::open(path, buffered()).unwrap();
    let mut txn = store.begin().unwrap();
    let below = match linked {
        Linked::Cut => Ptr { offset: 0, ..one },
        _ => one,
    };
    let again = match linked {
        Linked::Itself => two,
        _ => Entry {
            prev: Some(below),
            ..at(2, 200)
        }
        .write(&mut txn),
    };
    assert_eq!(again.hash, two.hash);
    assert_eq!(again.offset == two.offset, linked == Linked::Itself);
    let mut w = Encoder::new(&mut txn);
    w.raw(b"tree");
    w.link(again);
    let tree = w.finish().unwrap();
    let three = Entry {
        root: Some(tree),
        prev: Some(two),
        ..at(3, 300)
    }
    .write(&mut txn);
    let head = write_heads(&mut txn, &[("main", on(3, 1, tree, three))]);
    txn.commit(head).unwrap();
    store.close();
    drop(store);
    one
}

#[test]
fn a_tree_that_links_another_record_of_a_log_entry_is_copied_as_a_tree() {
    let mut copies = Vec::new();
    for threads in [1, 4] {
        for keep in [Keep::All, Keep::Last(2)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("again.ferro");
            let one = entry_linked_from_a_tree(&path, Linked::Again);
            let db: Db<Opaque> = Options::new().copy_threads(threads).open(&path).unwrap();
            db.verify().unwrap();
            db.verify_typed().unwrap();
            let outcome = db.compact(keep);
            drop(db);
            if keep == Keep::All {
                assert!(outcome.is_ok(), "{threads} threads: {outcome:?}");
                copies.push(fs::read(&path).unwrap());
            } else {
                let pruned = format!("link to the pruned log entry at {}", one.offset);
                assert!(
                    matches!(&outcome, Err(Error::Corrupt(message)) if *message == pruned),
                    "{threads} threads, {keep:?}: {outcome:?}"
                );
            }
            admin::verify(&path).unwrap();
        }
    }
    assert!(copies[0] == copies[1]);
}

#[test]
fn a_tree_that_links_another_record_of_a_log_entry_with_a_null_link_fails_as_verify_does() {
    for threads in [1, 4] {
        for keep in [Keep::All, Keep::Last(2)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("cut.ferro");
            entry_linked_from_a_tree(&path, Linked::Cut);
            let db: Db<Opaque> = Options::new().copy_threads(threads).open(&path).unwrap();
            let outcome = db.compact(keep);
            drop(db);
            let wrong = admin::verify(&path).unwrap_err().to_string();
            assert!(wrong.contains("pointer 0 outside log"), "{wrong}");
            assert!(
                matches!(&outcome, Err(error) if error.to_string() == wrong),
                "{threads} threads, {keep:?}: {outcome:?}"
            );
        }
    }
}

#[test]
fn a_tree_that_links_a_log_entry_copies_to_the_same_bytes_at_any_thread_count() {
    for linked in [Linked::Itself, Linked::Again] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("linked.ferro");
        entry_linked_from_a_tree(&source, linked);
        {
            let db: Db<Opaque> = Options::new().copy_threads(1).open(&source).unwrap();
            db.commit_with(|_| ("real", ())).unwrap();
        }
        let mut files = Vec::new();
        for threads in [1, 4, 8] {
            let path = dir.path().join(format!("{threads}.ferro"));
            fs::copy(&source, &path).unwrap();
            let db: Db<Opaque> = Options::new().copy_threads(threads).open(&path).unwrap();
            let report = db.compact(Keep::All).unwrap();
            drop(db);
            admin::verify(&path).unwrap();
            assert_eq!(report.handed_out > 0, threads > 1, "{threads} threads");
            files.push(fs::read(&path).unwrap());
        }
        assert!(files.iter().all(|file| *file == files[0]));
    }
}

#[test]
fn a_tree_that_links_the_record_a_chain_reads_shares_its_copy() {
    for threads in [1, 4] {
        for keep in [Keep::All, Keep::Last(2)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("itself.ferro");
            entry_linked_from_a_tree(&path, Linked::Itself);
            let objects = admin::verify(&path).unwrap().objects;
            let db: Db<Opaque> = Options::new().copy_threads(threads).open(&path).unwrap();
            let compacted = db.compact(keep).unwrap();
            drop(db);
            let genesis = u64::from(matches!(keep, Keep::Last(_)));
            assert_eq!(
                compacted.objects + compacted.commits_dropped,
                objects + genesis,
                "{threads} threads, {keep:?}"
            );
            assert_eq!(admin::verify(&path).unwrap().objects, compacted.objects);
        }
    }
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

#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_crafted_map_lets_its_file_close_after_a_read_fails() {
    let overflowing = |txn: &mut Txn| {
        let low = node(txn, 0, &[1], &[]);
        let high = node(txn, 0, &[3], &[]);
        let top = node(txn, 1, &[2], &[(0, 1 << 63, low), (1, 1 << 63, high)]);
        map_root(txn, 1, top)
    };
    let miscounted = |txn: &mut Txn| {
        let leaf = node(txn, 0, &[1], &[]);
        let top = node(txn, 1, &[2], &[(0, 7, leaf)]);
        map_root(txn, 8, top)
    };
    let builds: [fn(&mut Txn) -> Ptr; 2] = [overflowing, miscounted];
    for build in builds {
        let (dir, store, map) = crafted(build);
        let found = map.try_get(&1);
        assert!(matches!(found, Err(Error::Decode(_))), "{found:?}");
        drop((store, map));
        assert!(closed(&dir.path().join("crafted.ferro")));
    }
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
                let _ = map.range_summary(1..DEPTH - 1);
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
        tip: Some(tip),
        ..Head::default()
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
    Options::new().reclaim(Reclaim::off())
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
fn both_superblocks_naming_an_older_frame_open_at_the_newest_commit_and_verify() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("older.ferro");
    let db = Db::<Root>::open(&path).unwrap();
    for key in 1..=4 {
        db.update(|root| {
            root.insert(key, key.to_string());
        })
        .unwrap();
    }
    drop(db);
    let mut bytes = fs::read(&path).unwrap();
    let newest = newest_slot(&bytes);
    let older = SLOT - newest;
    assert!(field(&bytes, older + 8) < field(&bytes, newest + 8));
    bytes.copy_within(older..older + SLOT, newest);
    fs::write(&path, &bytes).unwrap();

    admin::verify(&path).unwrap();
    let reader = read_only().open::<Root>(&path).unwrap();
    assert_eq!((reader.read().version(), reader.read().len()), (4, 4));
    reader.verify_typed().unwrap();
    drop(reader);
    let db = Db::<Root>::open(&path).unwrap();
    assert_eq!((db.read().version(), db.read().len()), (4, 4));
    db.verify_typed().unwrap();
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
            tip: Some(tip),
            ..Head::default()
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
    assert!(follower.log_bytes() <= bytes.len() as u64);
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

fn value(txn: &mut Txn, n: u64) -> Ptr {
    let mut w = Encoder::new(txn);
    n.encode(&mut w).unwrap();
    w.finish().unwrap()
}

fn write_heads(txn: &mut Txn, heads: &[(&str, Head)]) -> Ptr {
    let mut refs: Map<String, Head> = Map::new();
    for (name, head) in heads {
        refs.insert(name.to_string(), head.clone());
    }
    let mut encoder = Encoder::new(txn);
    refs.encode(&mut encoder).unwrap();
    encoder.finish().unwrap()
}

fn skip_of(version: u64) -> u64 {
    version - (version & version.wrapping_neg())
}

fn chain(txn: &mut Txn, times: &[u64], below: &[Ptr]) -> Vec<Ptr> {
    let mut ptrs: Vec<Ptr> = below.to_vec();
    for &time_ms in times {
        let version = ptrs.len() as u64 + 1;
        let target = skip_of(version);
        let skip = (target >= 1 && target != version - 1).then(|| ptrs[target as usize - 1]);
        let root = value(txn, version);
        let ptr = Entry {
            version,
            time_ms,
            root: Some(root),
            prev: ptrs.last().copied(),
            skip,
            ..Entry::default()
        }
        .write(txn);
        ptrs.push(ptr);
    }
    ptrs
}

fn built(path: &Path, build: impl FnOnce(&mut Txn) -> Ptr) {
    let (store, _) = Store::open(path, buffered()).unwrap();
    let mut txn = store.begin().unwrap();
    let head = build(&mut txn);
    txn.commit(head).unwrap();
    store.close();
}

fn on(version: u64, first: u64, root: Ptr, tip: Ptr) -> Head {
    Head {
        version,
        first,
        root: Some(root),
        tip: Some(tip),
    }
}

fn passes_everywhere(path: &Path) {
    admin::verify(path).unwrap();
    for threads in [1, 4] {
        let db: Db<u64> = Options::new().copy_threads(threads).open(path).unwrap();
        db.verify_typed().unwrap();
        db.compact(Keep::All).unwrap();
        db.verify_typed().unwrap();
    }
    admin::compact(path, Keep::All).unwrap();
    admin::verify(path).unwrap();
}

fn refused_everywhere(path: &Path, message: &str) {
    let refused = |outcome: &ferro::Result<()>| matches!(outcome, Err(Error::Corrupt(found)) if found.ends_with(message));
    let outcome = admin::verify(path).map(|_| ());
    assert!(refused(&outcome), "verify: {outcome:?}");
    for threads in [1, 4] {
        let db: Db<u64> = Options::new().copy_threads(threads).open(path).unwrap();
        let outcome = db.verify_typed().map(|_| ());
        assert!(
            refused(&outcome),
            "{threads} threads, verify_typed: {outcome:?}"
        );
        let before = fs::read(path).unwrap();
        let outcome = db.compact(Keep::All).map(|_| ());
        assert!(refused(&outcome), "{threads} threads, compact: {outcome:?}");
        assert!(fs::read(path).unwrap() == before, "{threads} threads");
    }
    let before = fs::read(path).unwrap();
    let outcome = admin::compact(path, Keep::All).map(|_| ());
    assert!(refused(&outcome), "admin::compact: {outcome:?}");
    assert!(fs::read(path).unwrap() == before);
    let outcome = admin::verify(path).map(|_| ());
    assert!(refused(&outcome), "the file after: {outcome:?}");
}

#[test]
fn a_head_whose_root_is_not_its_tips_fails_verify_and_compaction() {
    for named in [2, 7] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("root.ferro");
        built(&path, |txn| {
            let ptrs = chain(txn, &[100, 200], &[]);
            let root = value(txn, named);
            write_heads(txn, &[("main", on(2, 1, root, ptrs[1]))])
        });
        let db = read_only().open::<u64>(&path).unwrap();
        assert_eq!((*db.read(), *db.at(2).unwrap()), (named, 2));
        drop(db);
        match named {
            2 => passes_everywhere(&path),
            _ => refused_everywhere(
                &path,
                "tip v2 holds another root than its branch's head names",
            ),
        }
    }
}

#[test]
fn a_branch_with_no_commit_names_version_zero_and_no_root() {
    for (version, rooted) in [(0, false), (5, false), (0, true)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.ferro");
        built(&path, |txn| {
            let ptrs = chain(txn, &[100, 200], &[]);
            let root = value(txn, 2);
            let empty = Head {
                version,
                root: rooted.then(|| value(txn, 7)),
                ..Head::default()
            };
            let heads = [("main", on(2, 1, root, ptrs[1])), ("empty", empty)];
            write_heads(txn, &heads)
        });
        if version == 0 && !rooted {
            let db = read_only().open::<u64>(&path).unwrap();
            assert_eq!(*db.branch("empty").unwrap().read(), 0);
            drop(db);
            passes_everywhere(&path);
            continue;
        }
        let case = format!("v{version}, rooted {rooted}");
        let outcome = read_only().open::<u64>(&path).map(drop);
        assert!(
            matches!(outcome, Err(Error::Corrupt(_))),
            "{case}: {outcome:?}"
        );
        corrupt(admin::info(&path));
        corrupt(admin::verify(&path));
    }
}

#[test]
fn a_commit_older_than_the_one_below_it_fails_verify_and_compaction() {
    for fourth in [400, 900] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("time.ferro");
        built(&path, |txn| {
            let ptrs = chain(txn, &[100, 200, 300, fourth, 500, 600, 700, 800], &[]);
            let root = value(txn, 8);
            write_heads(txn, &[("main", on(8, 1, root, ptrs[7]))])
        });
        let db = read_only().open::<u64>(&path).unwrap();
        let found = db.as_of(550).unwrap().version();
        assert_eq!(found, if fourth == 400 { 5 } else { 3 });
        drop(db);
        if fourth == 400 {
            passes_everywhere(&path);
            continue;
        }
        refused_everywhere(&path, "v5 is older than a commit it links to");
        for (keep, kept) in [(Keep::Last(5), true), (Keep::Last(4), false)] {
            let outcome = admin::compact(&path, keep).map(|_| ());
            assert_eq!(outcome.is_ok(), !kept, "{keep:?}: {outcome:?}");
        }
        admin::verify(&path).unwrap();
    }
}

fn merging(txn: &mut Txn, side: &[u64], merge: Entry, floor: u64, heads: &[(&str, u64)]) -> Ptr {
    let theirs = chain(txn, side, &[]);
    let base = chain(txn, &[1], &[]);
    let tip = theirs[theirs.len() - 1];
    let root = value(txn, 102);
    let merged = Entry {
        version: 2,
        root: Some(root),
        prev: Some(base[0]),
        merged: Some((tip, floor, None)),
        ..merge
    }
    .write(txn);
    let mut named = vec![("main", on(2, 1, root, merged))];
    for &(name, version) in heads {
        let root = value(txn, version);
        named.push((name, on(version, 1, root, theirs[version as usize - 1])));
    }
    write_heads(txn, &named)
}

#[test]
fn a_merge_older_than_the_commit_it_merged_fails_verify_and_compaction() {
    for time_ms in [40, 20] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("merge.ferro");
        built(&path, |txn| {
            let merge = Entry {
                time_ms,
                graph: (2, 3, 2),
                ..Entry::default()
            };
            merging(txn, &[10, 20, 30], merge, 1, &[("side", 3)])
        });
        match time_ms {
            40 => passes_everywhere(&path),
            _ => refused_everywhere(&path, "v2 is older than a commit it links to"),
        }
    }
}

#[test]
fn merge_numbers_that_do_not_follow_from_the_parents_fail_verify_and_compaction() {
    let honest = ((2, 3, 2), false);
    let cases = [
        (honest, Some(3)),
        (((2, 3, 0), false), Some(0)),
        (((2, 0, 2), false), Some(0)),
        (((9, 3, 2), false), Some(3)),
        (((2, 3, 2), true), Some(3)),
    ];
    for ((graph, bounded), ancestor) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.ferro");
        built(&path, |txn| {
            let merge = Entry {
                time_ms: 40,
                graph: (2, 3, 2),
                ..Entry::default()
            };
            let theirs = chain(txn, &[10, 20, 30], &[]);
            let base = chain(txn, &[1], &[]);
            let two = value(txn, 102);
            let merged = Entry {
                version: 2,
                root: Some(two),
                prev: Some(base[0]),
                merged: Some((theirs[2], 1, None)),
                ..merge
            }
            .write(txn);
            let three = value(txn, 103);
            let tip = Entry {
                version: 3,
                time_ms: 50,
                root: Some(three),
                prev: Some(merged),
                graph,
                bounded,
                ..Entry::default()
            }
            .write(txn);
            let side = value(txn, 3);
            let heads = [
                ("main", on(3, 1, three, tip)),
                ("side", on(3, 1, side, theirs[2])),
            ];
            write_heads(txn, &heads)
        });
        let case = format!("{graph:?} bounded {bounded}");
        let db = read_only().open::<u64>(&path).unwrap();
        let side = db.branch("side").unwrap();
        let found = db.common_ancestor(&side).map(|base| base.version());
        assert_eq!(found.ok(), ancestor, "{case}");
        let dir = tempfile::tempdir().unwrap();
        let copy = Db::<u64>::open(dir.path().join("copy.ferro")).unwrap();
        let pulled = copy.pull(&db, "remote", |_, _, _| ("merge", ()));
        assert_eq!(
            pulled.is_ok(),
            (graph, bounded) == honest,
            "{case}: {pulled:?}"
        );
        drop((side, db));
        match (graph, bounded) == honest {
            true => passes_everywhere(&path),
            false => refused_everywhere(&path, "v3 does not agree with the commits it links to"),
        }
    }
}

#[test]
fn a_merge_floor_past_the_merged_commit_fails_verify_and_compaction() {
    for floor in [3, 4] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("floor.ferro");
        built(&path, |txn| {
            let merge = Entry {
                time_ms: 40,
                graph: (2, 3, 2),
                bounded: true,
                ..Entry::default()
            };
            merging(txn, &[10, 20, 30], merge, floor, &[("side", 3)])
        });
        let db = read_only().open::<u64>(&path).unwrap();
        let side = db.branch("side").unwrap();
        let found = db.common_ancestor(&side).map(|base| base.version());
        drop((side, db));
        if floor == 3 {
            assert_eq!(found.unwrap(), 3);
            passes_everywhere(&path);
            continue;
        }
        assert!(
            matches!(&found, Err(Error::Corrupt(message))
                if message == "merge link below its floor 4"),
            "{found:?}"
        );
        refused_everywhere(
            &path,
            "v2 keeps the history it merged from past the merged commit",
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum MergedSkip {
    Honest,
    OtherEntry,
    PastCovered,
    PastMerge,
    PastTime,
    PastCut,
}

fn merged_history(path: &Path, merged: MergedSkip) {
    built(path, |txn| {
        let times: Vec<u64> = match merged {
            MergedSkip::PastTime => vec![1, 2, 3, 4, 5, 60],
            _ => (1..=12).map(|version| 10 * version).collect(),
        };
        let mut side = chain(txn, &times[..times.len() - 1], &[]);
        let top = times.len() as u64;
        let target = skip_of(top);
        let other = Entry {
            version: target,
            time_ms: times[top as usize - 1] - 5,
            root: Some(value(txn, 100 + target)),
            prev: Some(side[target as usize - 2]),
            ..Entry::default()
        }
        .write(txn);
        let below = side[top as usize - 2];
        if merged == MergedSkip::PastCut {
            let cut = Entry {
                version: top - 1,
                time_ms: times[top as usize - 2] + 1,
                root: Some(value(txn, top - 1)),
                prev: Some(Ptr {
                    offset: 0,
                    ..side[top as usize - 3]
                }),
                ..Entry::default()
            }
            .write(txn);
            side[top as usize - 2] = cut;
        }
        let skip = match merged {
            MergedSkip::Honest | MergedSkip::PastCut => side[target as usize - 1],
            _ => other,
        };
        let tip = Entry {
            version: top,
            time_ms: times[top as usize - 1],
            root: Some(value(txn, top)),
            prev: Some(match merged {
                MergedSkip::PastCut => side[top as usize - 2],
                _ => below,
            }),
            skip: Some(skip),
            ..Entry::default()
        }
        .write(txn);
        let base = chain(txn, &[1], &[]);
        let (mut below, mut reach) = (base[0], 0);
        if merged == MergedSkip::PastMerge {
            below = Entry {
                version: 2,
                time_ms: 999,
                root: Some(value(txn, 102)),
                prev: Some(below),
                merged: Some((side[top as usize - 3], 1, None)),
                graph: (top - 3, top - 2, 2),
                ..Entry::default()
            }
            .write(txn);
            reach = top - 2;
        }
        let version = 2 + u64::from(reach != 0);
        let root = value(txn, 100 + version);
        let after = (merged == MergedSkip::PastTime).then_some(50);
        let floor = 1 + u64::from(reach != 0);
        let merge = Entry {
            version,
            time_ms: 1000,
            root: Some(root),
            prev: Some(below),
            merged: Some((tip, floor, after)),
            graph: (top + 1 - version, top.max(reach), version),
            bounded: after.is_some() || floor > 1,
            ..Entry::default()
        }
        .write(txn);
        let mut heads = vec![("main", on(version, 1, root, merge))];
        let other_root = value(txn, 100 + target);
        heads.push(("other", on(target, 1, other_root, other)));
        if merged == MergedSkip::PastCovered {
            let low = top - 2;
            let low_root = value(txn, low);
            heads.push(("low", on(low, 1, low_root, side[low as usize - 1])));
        }
        write_heads(txn, &heads)
    });
}

#[test]
fn a_skip_link_in_merged_history_that_names_another_entry_fails_verify_and_compaction() {
    let cases = [
        MergedSkip::Honest,
        MergedSkip::OtherEntry,
        MergedSkip::PastCovered,
        MergedSkip::PastMerge,
    ];
    for merged in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("merged.ferro");
        merged_history(&path, merged);
        let db = read_only().open::<u64>(&path).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let copy = Db::<u64>::open(dir.path().join("copy.ferro")).unwrap();
        let pulled = copy.pull(&db, "remote", |_, _, _| ("merge", ()));
        assert_eq!(
            pulled.is_ok(),
            merged == MergedSkip::Honest,
            "{merged:?}: {pulled:?}"
        );
        drop(db);
        match merged {
            MergedSkip::Honest => passes_everywhere(&path),
            _ => refused_everywhere(&path, "v12 skips to another entry than v8"),
        }
    }
}

#[test]
fn a_skip_link_past_where_merged_history_stops_fails_verify_and_compaction() {
    for (merged, message) in [
        (
            MergedSkip::PastTime,
            "v6 skips past v5, where the history it keeps stops, to v4",
        ),
        (
            MergedSkip::PastCut,
            "v12 skips to v8, past where the history below v11 was cut",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("merged.ferro");
        merged_history(&path, merged);
        refused_everywhere(&path, message);
    }
}

#[test]
fn a_fork_whose_skip_link_names_another_entry_in_shared_history_fails_compaction() {
    for skip in [Skip::Honest, Skip::OtherRecord] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fork.ferro");
        built(&path, |txn| {
            let ptrs = chain(txn, &[10, 20, 30, 40, 50], &[]);
            let other = Entry {
                version: 4,
                time_ms: 41,
                root: Some(value(txn, 44)),
                prev: Some(ptrs[2]),
                ..Entry::default()
            }
            .write(txn);
            let six = value(txn, 6);
            let fork = Entry {
                version: 6,
                time_ms: 60,
                root: Some(six),
                prev: Some(ptrs[4]),
                skip: Some(match skip {
                    Skip::OtherRecord => other,
                    _ => ptrs[3],
                }),
                ..Entry::default()
            }
            .write(txn);
            let five = value(txn, 5);
            let heads = [("a", on(5, 1, five, ptrs[4])), ("b", on(6, 1, six, fork))];
            write_heads(txn, &heads)
        });
        match skip {
            Skip::OtherRecord => refused_everywhere(&path, "v6 skips to another entry than v4"),
            _ => passes_everywhere(&path),
        }
    }
}
