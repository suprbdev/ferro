#![cfg(unix)]

mod common;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_void};

use common::{Rng, fast};
use ferro::{Db, Error, Keep, Map};

type Root = Map<u64, u64>;

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const EIO: c_int = 5;

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn __errno_location() -> *mut c_int;
}

thread_local! {
    static FAIL_READS_AT: RefCell<Vec<i64>> = const { RefCell::new(Vec::new()) };
    static FAILED_READS: Cell<u32> = const { Cell::new(0) };
}

#[unsafe(no_mangle)]
pub extern "C" fn pread64(fd: c_int, buf: *mut c_void, count: usize, offset: i64) -> isize {
    let end = offset.saturating_add(count as i64);
    if FAIL_READS_AT.with_borrow(|at| at.iter().any(|at| (offset..end).contains(at))) {
        FAILED_READS.with(|failed| failed.set(failed.get() + 1));
        // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's `errno`,
        // valid for writes for as long as the thread runs.
        unsafe { *__errno_location() = EIO };
        return -1;
    }
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"pread64"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"pread64".as_ptr()) };
    // SAFETY: glibc defines `pread64`, so `symbol` is the non-null address of that function, and
    // its C prototype `ssize_t pread64(int, void *, size_t, off64_t)` matches this function pointer
    // type.
    let real: extern "C" fn(c_int, *mut c_void, usize, i64) -> isize =
        unsafe { std::mem::transmute(symbol) };
    real(fd, buf, count, offset)
}

fn put(db: &Db<Root>, key: u64, value: u64) {
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
}

fn root_offset(db: &Db<Root>) -> i64 {
    db.read().commit().root.unwrap().offset as i64
}

fn refresh_failing_at(reader: &Db<Root>, offset: i64) -> ferro::Result<bool> {
    FAIL_READS_AT.with_borrow_mut(|at| *at = vec![offset]);
    let refreshed = reader.refresh();
    FAIL_READS_AT.with_borrow_mut(Vec::clear);
    refreshed
}

fn followed(compact: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refresh.ferro");
    let db = fast().open::<Root>(&path).unwrap();
    put(&db, 1, 1);
    let side = db.fork("side").unwrap();
    let reader = fast()
        .cache_bytes(1)
        .read_only(true)
        .open::<Root>(&path)
        .unwrap();
    let reader_side = reader.branch("side").unwrap();
    put(&db, 2, 2);
    put(&side, 3, 3);
    if compact {
        db.compact(Keep::All).unwrap();
    }
    for target in [root_offset(&side), root_offset(&db)] {
        let failed = refresh_failing_at(&reader, target);
        assert!(matches!(failed, Err(Error::Io(_))), "{failed:?}");
    }
    assert!(reader.refresh().unwrap());
    assert_eq!(reader.version(), db.version());
    assert_eq!(reader_side.version(), side.version());
    assert_eq!(*reader.read().get(&2).unwrap(), 2);
    assert_eq!(*reader_side.read().get(&3).unwrap(), 3);
    assert!(!reader.refresh().unwrap());
    put(&side, 4, 4);
    assert!(reader.refresh().unwrap());
    assert_eq!(reader_side.version(), side.version());
    assert_eq!(reader.version(), db.version());
}

#[test]
fn a_refresh_that_fails_partway_is_finished_by_the_next() {
    followed(false);
}

#[test]
fn a_refresh_that_fails_partway_through_a_replaced_file_is_finished_by_the_next() {
    followed(true);
}

#[test]
fn a_scan_reads_past_bytes_beside_its_records_that_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scan.ferro");
    let db = fast().open::<Map<u64, String>>(&path).unwrap();
    let mut rng = Rng::new(7);
    let mut model = BTreeMap::new();
    let mut overwritten = Vec::new();
    for round in 0..40u64 {
        let text: String = (0..4096)
            .map(|_| char::from(b'0' + rng.below(64) as u8))
            .collect();
        db.update(|root| {
            root.insert(round * 1_000, text.clone());
        })
        .unwrap();
        let batch: Vec<(u64, String)> = (0..120)
            .map(|i| (round * 1_000 + i, format!("value-{round}-{i}")))
            .collect();
        db.update(|root| root.extend(batch.clone())).unwrap();
        model.extend(batch);
        overwritten.push(text);
    }
    drop(db);
    let bytes = std::fs::read(&path).unwrap();
    let failing: Vec<i64> = overwritten
        .iter()
        .map(|text| {
            let head = &text.as_bytes()[..64];
            let at = bytes.windows(64).position(|window| window == head).unwrap();
            at as i64 + 2048
        })
        .collect();
    let reader = fast()
        .cache_bytes(1)
        .read_only(true)
        .open::<Map<u64, String>>(&path)
        .unwrap();
    let snapshot = reader.read();
    FAILED_READS.with(|failed| failed.set(0));
    FAIL_READS_AT.with_borrow_mut(|at| *at = failing);
    let seen: Vec<(u64, String)> = snapshot
        .iter()
        .map(|(key, value)| (key, value.to_string()))
        .collect();
    FAIL_READS_AT.with_borrow_mut(Vec::clear);
    assert_eq!(seen, model.into_iter().collect::<Vec<_>>());
    assert!(FAILED_READS.with(Cell::get) > 0);
}
