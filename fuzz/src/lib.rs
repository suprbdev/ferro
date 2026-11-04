pub mod catalog;
pub mod fixup;
pub mod input;
pub mod log_entry;
pub mod map_node;
pub mod refs;
pub mod sync;
pub mod types;

use std::any::Any;
use std::cell::Cell;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};

use ferro::admin::{self, Keep};
use ferro::{Db, Durability, Options};

pub use catalog::{Catalog, CatalogV1, CatalogV3, Ints, Pairs, World};
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

pub fn scratch_base() -> PathBuf {
    match std::env::var_os("FERRO_FUZZ_DIR") {
        Some(dir) => PathBuf::from(dir),
        None if Path::new("/dev/shm").is_dir() => PathBuf::from("/dev/shm"),
        None => std::env::temp_dir(),
    }
}

static SCRATCH: OnceLock<PathBuf> = OnceLock::new();

extern "C" fn remove_scratch() {
    if let Some(dir) = SCRATCH.get() {
        let _ = fs::remove_dir_all(dir);
    }
}

pub fn scratch() -> &'static Path {
    SCRATCH.get_or_init(|| {
        let dir = scratch_base().join(format!("ferro-fuzz-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            libc::atexit(remove_scratch);
        }
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

pub const KINDS: u8 = 16;
pub const ROOT: u8 = 0;
pub const COUNTERS: u8 = 1;
pub const SHOP: u8 = 2;
pub const DOCS: u8 = 3;
pub const NESTED: u8 = 4;
pub const SUMMED: u8 = 5;
pub const CATALOG_V1: u8 = 6;
pub const CATALOG: u8 = 7;
pub const CATALOG_V3: u8 = 8;
pub const WORLD: u8 = 9;
pub const PAIRS: u8 = 10;
pub const INTS: u8 = 11;
pub const OPAQUE: u8 = 12;

pub const NO_REPAIR: u8 = 1;
pub const READ_ONLY: u8 = 2;
pub const WRITES: u8 = 0x40;
pub const SYNC_OUT: u8 = 0x80;

#[derive(Clone, Copy, Debug, Default)]
pub struct Plan {
    pub read_only: bool,
    pub writes: bool,
    pub sync_out: bool,
}

impl Plan {
    pub fn of(mode: u8) -> Plan {
        Plan {
            read_only: mode & READ_ONLY != 0,
            writes: mode & WRITES != 0,
            sync_out: mode & SYNC_OUT != 0,
        }
    }
}

pub fn mode(kind: u8, flags: u8) -> u8 {
    (kind % KINDS) << 2 | flags
}

pub fn run_open(data: &[u8]) {
    if let Some((mode, bytes)) = file_bytes(data) {
        open_kind(&stage(&bytes), Plan::of(mode), (mode >> 2) & 0x0f);
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
    if read_only {
        options.read_only(true)
    } else {
        options
    }
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
    let probes = [
        0,
        1,
        2,
        version / 2,
        version.saturating_sub(1),
        version,
        version.saturating_add(1),
        u64::MAX,
    ];
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
    for name in guarded(|| db.branches())
        .unwrap_or_default()
        .into_iter()
        .take(4)
    {
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
    open_planned::<T>(
        path,
        Plan {
            read_only,
            ..Plan::default()
        },
    );
}

pub fn open_planned<T: Probe + Default>(path: &Path, plan: Plan) {
    if let Ok(db) = options(plan.read_only).open::<T>(path) {
        exercise(&db, plan.read_only);
        let _ = db.verify_typed();
        if plan.writes && plan.read_only {
            follow(&db, path);
        }
        if plan.writes && !plan.read_only {
            writes(&db);
        }
        if plan.sync_out {
            sync_out(&db);
        }
    }
}

fn follow<T: Probe + Default>(follower: &Db<T>, path: &Path) {
    if let Ok(writer) = options(false).open::<T>(path) {
        writes(&writer);
        for step in 0..70u64 {
            if guarded(|| writer.update(|value| value.poke(step))).is_none_or(|done| done.is_err())
            {
                break;
            }
            if step == 40 {
                let _ = follower.refresh();
                let _ = writer.compact(Keep::Last(3));
            }
        }
    }
    let _ = follower.refresh();
    exercise(follower, true);
    let _ = follower.verify();
}

pub fn open_kind(path: &Path, plan: Plan, kind: u8) {
    match kind % KINDS {
        ROOT => open_planned::<Root>(path, plan),
        COUNTERS => open_planned::<Counters>(path, plan),
        SHOP => open_planned::<Shop>(path, plan),
        DOCS => open_planned::<Docs>(path, plan),
        NESTED => open_planned::<Nested>(path, plan),
        SUMMED => open_planned::<Summed>(path, plan),
        CATALOG_V1 => open_planned::<CatalogV1>(path, plan),
        CATALOG => open_planned::<Catalog>(path, plan),
        CATALOG_V3 => open_planned::<CatalogV3>(path, plan),
        WORLD => open_planned::<World>(path, plan),
        PAIRS => open_planned::<Pairs>(path, plan),
        INTS => open_planned::<Ints>(path, plan),
        _ => open_planned::<Opaque>(path, plan),
    }
}

fn writes<T: Probe>(db: &Db<T>) {
    guarded(|| db.commit("poke", |value| value.poke(1)));
    guarded(|| db.update(|value| value.poke(6)));
    let names = guarded(|| db.branches()).unwrap_or_default();
    if let Some(name) = names.iter().find(|name| name.as_str() != db.branch_name())
        && let Ok(other) = db.branch(name)
    {
        guarded(|| other.commit("other", |value| value.poke(2)));
        guarded(|| {
            db.merge_with(&other, |ours, base, theirs| {
                let _ = (base.measure(), theirs.measure());
                ours.poke(3);
                ("merge", ())
            })
        });
        guarded(|| {
            db.commit("nested", |value| {
                value.poke(4);
                other.commit("inner", |value| value.poke(5)).is_ok()
            })
        });
    }
    if let Ok(side) = db.fork("fuzz-side") {
        guarded(|| side.commit("side", |value| value.poke(7)));
        guarded(|| db.commit("main", |value| value.poke(9)));
        guarded(|| {
            db.merge_with(&side, |ours, _, theirs| {
                let _ = theirs.measure();
                ours.poke(8);
                ("merge side", ())
            })
        });
        drop(side);
        let _ = db.drop_branch("fuzz-side");
    }
    let _ = db.verify();
    if db.compact(Keep::Last(2)).is_ok() {
        guarded(|| db.update(|value| value.poke(10)));
        let _ = db.verify();
    }
}

fn sync_out<T: Probe + Default>(db: &Db<T>) {
    let path = scratch().join("out.ferro");
    let _ = fs::remove_file(&path);
    let Ok(to) = options(false).open::<T>(&path) else {
        return;
    };
    if let Ok(have) = to.have() {
        let mut bundle = Vec::new();
        if db.bundle_to(&have, &mut bundle).is_ok()
            && to
                .receive_from("in", bundle.as_slice(), |_, _, _| ("merge", ()))
                .is_ok()
        {
            let _ = to.verify();
            let _ = to.verify_typed();
            let mut budget = 400;
            guarded(|| to.read().probe(&mut budget));
        }
    }
    if let Ok(have) = db.have() {
        let _ = to.bundle(&have);
    }
    guarded(|| to.update(|value| value.poke(3)));
    let pulled = db.pull(&to, "back", |_, _, _| ("merge back", ()));
    if pulled.is_ok() {
        let _ = db.verify();
        let _ = db.verify_typed();
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
