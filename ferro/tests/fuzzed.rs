use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};

use ferro::{Db, Error, Map, Options};
use tempfile::TempDir;

type Root = Map<u64, String>;

const SLOT: usize = 4096;

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

fn newest_head(bytes: &[u8]) -> usize {
    let field = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let slot = if field(SLOT + 8) > field(8) { SLOT } else { 0 };
    field(slot + 24) as usize
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
