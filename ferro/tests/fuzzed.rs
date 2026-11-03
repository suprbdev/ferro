use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};

use ferro::admin::{self, Keep};
use ferro::codec::{Decoder, Encoder};
use ferro::{Data, Db, Error, Map, Options};
use tempfile::TempDir;

type Root = Map<u64, String>;
type Counters = Map<u8, i64>;

const SLOT: usize = 4096;

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
