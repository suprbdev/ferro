#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::os::unix::fs::FileExt;

use ferro::{Data, Db, Durability, Options};

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const F_OFD_SETLK: c_int = 37;
const F_RDLCK: i16 = 0;
const ENOLCK: c_int = 37;
const PLACEMENT: i64 = (1 << 62) + 2;

#[repr(C)]
struct Flock {
    l_type: i16,
    l_whence: i16,
    l_start: i64,
    l_len: i64,
    l_pid: i32,
}

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn __errno_location() -> *mut c_int;
}

thread_local! {
    static FAIL_PLACEMENT: Cell<bool> = const { Cell::new(false) };
    static FAILED: Cell<u32> = const { Cell::new(0) };
}

/// Fails a shared test of the placement lock with `ENOLCK` on a thread that set
/// `FAIL_PLACEMENT`, and passes every other call to glibc's `fcntl`.
///
/// # Safety
///
/// The caller keeps `fcntl`'s contract: `arg` is the argument `command` takes, a pointer to a
/// `struct flock` for `F_OFD_SETLK`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fcntl(fd: c_int, command: c_int, arg: *mut c_void) -> c_int {
    if command == F_OFD_SETLK && FAIL_PLACEMENT.with(Cell::get) {
        // SAFETY: for `F_OFD_SETLK` the caller passes a pointer to a live `struct flock`, whose
        // layout on 64-bit Linux `Flock` repeats.
        let lock = unsafe { &*(arg as *const Flock) };
        if lock.l_type == F_RDLCK && lock.l_start == PLACEMENT {
            FAILED.with(|failed| failed.set(failed.get() + 1));
            // SAFETY: `__errno_location` returns a non-null pointer to the calling thread's
            // `errno`, valid for writes for as long as the thread runs.
            unsafe { *__errno_location() = ENOLCK };
            return -1;
        }
    }
    // SAFETY: `RTLD_NEXT` is glibc's pseudo-handle for the next definition after this one in lookup
    // order, and `c"fcntl"` is a NUL-terminated string that outlives the call.
    let symbol = unsafe { dlsym(RTLD_NEXT, c"fcntl".as_ptr()) };
    // SAFETY: glibc defines `fcntl`, so `symbol` is the non-null address of that function, and its
    // C prototype `int fcntl(int, int, ...)` matches this function pointer type.
    let real: unsafe extern "C" fn(c_int, c_int, ...) -> c_int =
        unsafe { std::mem::transmute(symbol) };
    // SAFETY: the arguments are the caller's, passed on unchanged under the same contract.
    unsafe { real(fd, command, arg) }
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    n: u64,
}

fn read_only_failing_the_placement_lock(path: &std::path::Path) -> (ferro::Result<u64>, u32) {
    FAIL_PLACEMENT.with(|fail| fail.set(true));
    FAILED.with(|failed| failed.set(0));
    let opened = Options::new()
        .read_only(true)
        .open::<Root>(path)
        .map(|db| db.read().n);
    FAIL_PLACEMENT.with(|fail| fail.set(false));
    (opened, FAILED.with(Cell::get))
}

#[test]
fn a_read_only_open_whose_test_of_the_placement_lock_fails_scans_past_the_superblock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enolck.ferro");
    let db: Db<Root> = Options::new()
        .durability(Durability::Fsync)
        .open(&path)
        .unwrap();
    db.update(|root| root.n += 1).unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut slots = vec![0u8; 8192];
    file.read_exact_at(&mut slots, 0).unwrap();
    db.update(|root| root.n += 1).unwrap();
    drop(db);
    file.write_all_at(&slots, 0).unwrap();
    let (opened, failed) = read_only_failing_the_placement_lock(&path);
    assert_eq!(failed, 1, "the test of the placement lock was not failed");
    assert_eq!(
        opened.unwrap(),
        2,
        "the open trusted a superblock a lost write left one commit behind"
    );
}
