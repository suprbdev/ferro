pub mod fixup;
pub mod input;
pub mod log_entry;
pub mod map_node;
pub mod types;

use std::any::Any;
use std::cell::Cell;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};

use ferro::admin::{self, Keep};
use ferro::{Db, Durability, Options};

pub use types::{Counters, Docs, Nested, Opaque, Probe, Root, Shop, Summed};

pub fn panic_text(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = payload.downcast_ref::<&str>() {
        text.to_string()
    } else {
        String::new()
    }
}

thread_local! {
    static GUARDS: Cell<usize> = const { Cell::new(0) };
}

pub fn install_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let inherited = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let expected = GUARDS.get() > 0 && panic_text(info.payload()).starts_with("ferro:");
            if !expected {
                inherited(info);
            }
        }));
    });
}

pub fn guarded<R>(f: impl FnOnce() -> R) -> Option<R> {
    GUARDS.set(GUARDS.get() + 1);
    let result = panic::catch_unwind(AssertUnwindSafe(f));
    GUARDS.set(GUARDS.get() - 1);
    result.ok()
}

pub fn scratch() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let base = match std::env::var_os("FERRO_FUZZ_DIR") {
            Some(dir) => PathBuf::from(dir),
            None if Path::new("/dev/shm").is_dir() => PathBuf::from("/dev/shm"),
            None => std::env::temp_dir(),
        };
        let dir = base.join(format!("ferro-fuzz-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    })
}

pub fn fresh(name: &str) -> PathBuf {
    let path = scratch().join(name);
    for entry in fs::read_dir(scratch()).into_iter().flatten().flatten() {
        let _ = fs::remove_file(entry.path());
    }
    path
}

pub fn stage(bytes: &[u8]) -> PathBuf {
    let path = fresh("db.ferro");
    fs::write(&path, bytes).unwrap();
    path
}

pub fn file_bytes(data: &[u8]) -> Option<(u8, Vec<u8>)> {
    let (&mode, rest) = data.split_first()?;
    let mut bytes = rest.to_vec();
    if mode & 1 == 0 {
        fixup::fixup(&mut bytes);
    }
    Some((mode, bytes))
}

pub fn run_open(data: &[u8]) {
    if let Some((mode, bytes)) = file_bytes(data) {
        open_kind(&stage(&bytes), mode & 2 != 0, mode >> 2);
    }
}

pub fn run_admin_input(data: &[u8]) {
    if let Some((mode, bytes)) = file_bytes(data) {
        run_admin(&stage(&bytes), mode & 0x0e == 0x0e);
    }
}

pub fn options(read_only: bool) -> Options {
    let options = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20);
    if read_only { options.read_only() } else { options }
}

pub fn exercise<T: Probe>(db: &Db<T>, read_only: bool) {
    let mut budget = 4000;
    let snapshot = db.read();
    guarded(|| snapshot.probe(&mut budget));
    let _ = db.head_hash();
    let _ = db.verify();
    let _ = db.space();
    let log = db.log();
    let _ = log.len();
    let _ = log.is_empty();
    guarded(|| {
        let mut walk = log.iter();
        for _ in 0..4 {
            if walk.next().is_none() {
                break;
            }
        }
        let _ = walk.next_back();
    });
    guarded(|| log.last());
    guarded(|| log.first());
    let version = db.version();
    guarded(|| log.hash_at(&version));
    let probes = [0, 1, 2, version / 2, version.saturating_sub(1), version, version.saturating_add(1), u64::MAX];
    for at in probes {
        let _ = log.try_get(&at);
        if let Ok(old) = db.at(at) {
            let mut small = 200;
            guarded(|| old.probe(&mut small));
        }
    }
    let mut history = db.history_of(|value: &T| value.measure());
    for _ in 0..32 {
        if !matches!(history.try_next(), Ok(Some(_))) {
            break;
        }
    }
    let mut ranged = db.history_of_range(version / 2..=version, |value: &T| value.measure());
    for _ in 0..8 {
        if !matches!(ranged.try_next(), Ok(Some(_))) {
            break;
        }
    }
    let time = snapshot.commit().time_ms;
    for at in [0, 1, time / 2, time, time.saturating_add(1), u64::MAX] {
        let _ = db.as_of(at);
    }
    let _ = db.bisect(.., |value| value.measure() > 0);
    let _ = db.bisect_changes(.., |value| value.measure());
    for name in guarded(|| db.branches()).unwrap_or_default().into_iter().take(4) {
        let Ok(branch) = db.branch(&name) else {
            continue;
        };
        let mut small = 200;
        guarded(|| branch.read().probe(&mut small));
        let _ = branch.log().len();
        let _ = db.common_ancestor(&branch);
        let _ = branch.common_ancestor(db);
    }
    if read_only {
        let _ = db.refresh();
    }
}

pub fn open_as<T: Probe + Default>(path: &Path, read_only: bool) {
    if let Ok(db) = options(read_only).open::<T>(path) {
        exercise(&db, read_only);
    }
}

pub fn open_kind(path: &Path, read_only: bool, kind: u8) {
    match kind % 8 {
        0 => open_as::<Root>(path, read_only),
        1 => open_as::<Counters>(path, read_only),
        2 => open_as::<Shop>(path, read_only),
        3 => open_as::<Docs>(path, read_only),
        4 => open_as::<Nested>(path, read_only),
        5 => open_as::<Summed>(path, read_only),
        _ => open_as::<Opaque>(path, read_only),
    }
}

pub fn run_admin(path: &Path, compact: bool) {
    if let Ok(info) = admin::info(path) {
        for branch in info.branches.iter().take(4) {
            let _ = admin::log(path, &branch.name, 16);
        }
    }
    let _ = admin::log(path, "main", 64);
    let _ = admin::verify(path);
    let _ = admin::space(path);
    let _ = admin::layout(path, Keep::All);
    let _ = admin::layout(path, Keep::Last(2));
    let _ = admin::layout(path, Keep::Since(1));
    if compact {
        let _ = admin::compact(path, Keep::Last(3));
        let _ = admin::verify(path);
    }
}
