mod common;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use common::{Rng, text};
use ferro::db::Hook;
use ferro::store::{Op, Trace};
use ferro::{Db, Durability, Keep, Map, Options, Reclaim};
use parking_lot::Mutex;
use tempfile::TempDir;

type Root = Map<u64, String>;
type Model = BTreeMap<u64, String>;
type Slot = Arc<Mutex<Option<Db<Root>>>>;
type Shared = Arc<Mutex<(Rng, BTreeMap<u64, Model>)>>;

const SECTOR: usize = 512;
const SLOT_BYTES: usize = 4096;
const MANUAL: Reclaim = Reclaim { every_bytes: 0 };

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn dump(root: &Root) -> Model {
    root.iter().map(|(k, v)| (k, (*v).clone())).collect()
}

struct Recording {
    base: Vec<u8>,
    inode: u64,
    ops: Vec<Op>,
    models: BTreeMap<u64, Model>,
}

fn acknowledged(db: &Db<Root>, shared: &Shared, keys: u64) {
    let (key, value) = {
        let mut state = shared.lock();
        let key = state.0.below(keys);
        let seed = state.0.next();
        let len = state.0.below(900) as usize + 1;
        (key, text(seed, len))
    };
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
    shared.lock().1.insert(db.version(), dump(&db.read()));
    db.trace_op(Op::Acked(db.version()));
}

fn record(cycles: u64, commits: u64, keys: u64, keep: usize) -> Recording {
    record_on(commits, cycles, commits, keys, keep, 40, 0)
}

fn record_on(
    filled: u64,
    cycles: u64,
    commits: u64,
    keys: u64,
    keep: usize,
    len: usize,
    threads: usize,
) -> Recording {
    let dir = dir();
    let path = dir.path().join("trace.ferro");
    let rng = Rng::new(cycles * 977 + commits + keep as u64);
    let mut models = BTreeMap::new();
    models.insert(0, Model::new());
    {
        let db: Db<Root> = Options::new()
            .durability(Durability::Buffered)
            .reclaim(MANUAL)
            .open(&path)
            .unwrap();
        for i in 0..filled {
            db.update(|root| {
                root.insert(i % keys, text(i, len));
            })
            .unwrap();
            models.insert(db.version(), dump(&db.read()));
        }
        db.compact(Keep::Last(keep)).unwrap();
    }
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let base = fs::read(&path).unwrap();
    let slot: Slot = Arc::new(Mutex::new(None));
    let shared: Shared = Arc::new(Mutex::new((rng, models)));
    let hook = {
        let (slot, shared) = (slot.clone(), shared.clone());
        move |name: &'static str| {
            if matches!(name, "created" | "copied" | "synced" | "swapped") {
                let db = slot.lock().clone().unwrap();
                acknowledged(&db, &shared, keys);
            }
        }
    };
    let db: Db<Root> = Options::new()
        .reclaim(MANUAL)
        .copy_threads(threads)
        .trace(trace.clone())
        .hook(Hook(Arc::new(hook)))
        .open(&path)
        .unwrap();
    *slot.lock() = Some(db.clone());
    for _ in 0..cycles {
        for _ in 0..commits {
            acknowledged(&db, &shared, keys);
        }
        db.compact(Keep::Last(keep)).unwrap();
    }
    slot.lock().take();
    drop(db);
    let ops = trace.lock().clone();
    let inode = ops
        .iter()
        .find_map(|op| match op {
            Op::Write(inode, ..) => Some(*inode),
            _ => None,
        })
        .unwrap();
    let models = shared.lock().1.clone();
    Recording {
        base,
        inode,
        ops,
        models,
    }
}

#[derive(Clone, Copy, Debug)]
enum Loss {
    Everything,
    Nothing,
    Some,
    Torn,
}

fn apply(image: &mut Vec<u8>, op: &Op) {
    match op {
        Op::Write(_, at, bytes) => {
            let end = *at as usize + bytes.len();
            if image.len() < end {
                image.resize(end, 0);
            }
            image[*at as usize..end].copy_from_slice(bytes);
        }
        Op::SetLen(_, len) => image.resize(*len as usize, 0),
        _ => {}
    }
}

fn torn(image: &mut Vec<u8>, op: &Op, rng: &mut Rng) {
    let Op::Write(_, at, bytes) = op else {
        return;
    };
    let start = *at as usize;
    let end = start + bytes.len();
    if image.len() < end {
        image.resize(end, 0);
    }
    let mut offset = start;
    while offset < end {
        let stop = ((offset / SECTOR + 1) * SECTOR).min(end);
        if rng.chance(50) {
            image[offset..stop].copy_from_slice(&bytes[offset - start..stop - start]);
        }
        offset = stop;
    }
}

struct Crash {
    named: Vec<u8>,
    leftover: Option<Vec<u8>>,
    acked: u64,
}

fn crash(recording: &Recording, cut: usize, loss: Loss, rng: &mut Rng) -> Crash {
    let mut durable: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut pending: HashMap<u64, Vec<&Op>> = HashMap::new();
    durable.insert(recording.inode, recording.base.clone());
    let mut named = recording.inode;
    let mut renamed = None;
    let mut temp = None;
    let mut acked = 0;
    for op in &recording.ops[..cut] {
        match op {
            Op::Create(inode) => {
                durable.insert(*inode, Vec::new());
                pending.insert(*inode, Vec::new());
                temp = Some(*inode);
            }
            Op::Write(inode, ..) | Op::SetLen(inode, _) => {
                pending.entry(*inode).or_default().push(op);
            }
            Op::Sync(inode) => {
                let image = durable.entry(*inode).or_default();
                for done in pending.entry(*inode).or_default().drain(..) {
                    apply(image, done);
                }
            }
            Op::Rename(inode) => {
                renamed = Some(*inode);
                temp = None;
            }
            Op::SyncDir => {
                if let Some(inode) = renamed.take() {
                    named = inode;
                }
            }
            Op::Acked(version) => acked = (*version).max(acked),
        }
    }
    let image_of = |inode: u64, rng: &mut Rng| -> Vec<u8> {
        let mut image = durable.get(&inode).cloned().unwrap_or_default();
        for op in pending.get(&inode).into_iter().flatten() {
            match loss {
                Loss::Everything => {}
                Loss::Nothing => apply(&mut image, op),
                Loss::Some => {
                    if rng.chance(50) {
                        apply(&mut image, op);
                    }
                }
                Loss::Torn => {
                    if matches!(op, Op::Write(..)) {
                        torn(&mut image, op, rng);
                    } else if rng.chance(50) {
                        apply(&mut image, op);
                    }
                }
            }
        }
        image
    };
    let chosen = match (renamed, loss) {
        (Some(inode), Loss::Nothing) => inode,
        (Some(inode), Loss::Some | Loss::Torn) if rng.chance(50) => inode,
        _ => named,
    };
    let leftover = temp.map(|inode| image_of(inode, rng));
    Crash {
        named: image_of(chosen, rng),
        leftover,
        acked,
    }
}

fn check(path: &Path, recording: &Recording, acked: u64, context: &str) {
    let reader: Db<Root> = Options::new()
        .read_only(true)
        .cache_bytes(1)
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: {error}"));
    assert!(
        reader.version() >= acked,
        "{context}: read-only lost {acked}"
    );
    assert_eq!(
        dump(&reader.read()),
        recording.models[&reader.version()],
        "{context}"
    );
    drop(reader);
    let db: Db<Root> = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1)
        .reclaim(MANUAL)
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: {error}"));
    assert!(
        db.version() >= acked,
        "{context}: lost {acked}, at {}",
        db.version()
    );
    assert_eq!(
        dump(&db.read()),
        recording.models[&db.version()],
        "{context}"
    );
    let log: Vec<u64> = db.log().iter().map(|(v, _)| v).collect();
    for version in log {
        assert_eq!(
            dump(&db.at(version).unwrap()),
            recording.models[&version],
            "{context} v{version}"
        );
    }
    db.verify()
        .unwrap_or_else(|error| panic!("{context}: {error}"));
    db.update(|root| {
        root.insert(u64::MAX, "after".into());
    })
    .unwrap();
    db.compact(Keep::Last(1))
        .unwrap_or_else(|error| panic!("{context}: {error}"));
    db.verify()
        .unwrap_or_else(|error| panic!("{context}: {error}"));
}

fn replay(recording: &Recording, stride: usize, seed: u64) -> usize {
    let dir = dir();
    let path = dir.path().join("image.ferro");
    let temp = dir.path().join("image.ferro.compacting");
    let mut rng = Rng::new(seed);
    let mut images = 0;
    let mut cut = 0;
    while cut <= recording.ops.len() {
        for loss in [Loss::Everything, Loss::Nothing, Loss::Some, Loss::Torn] {
            let state = crash(recording, cut, loss, &mut rng);
            fs::write(&path, &state.named).unwrap();
            match state.leftover {
                Some(bytes) => fs::write(&temp, bytes).unwrap(),
                None => {
                    let _ = fs::remove_file(&temp);
                }
            }
            let context = format!("cut {cut} of {} {loss:?}", recording.ops.len());
            check(&path, recording, state.acked, &context);
            assert!(
                !temp.exists(),
                "{context}: temp file survived a writer open"
            );
            images += 1;
        }
        cut += stride.max(1);
    }
    images
}

#[test]
fn power_loss_at_any_operation_keeps_every_acknowledged_commit() {
    let recording = record(3, 12, 30, 2);
    assert!(recording.ops.iter().any(|op| matches!(op, Op::Rename(_))));
    assert!(replay(&recording, 3, 11) > 100);
}

#[test]
fn power_loss_keeping_a_single_version_keeps_every_acknowledged_commit() {
    let recording = record(3, 10, 24, 1);
    assert!(replay(&recording, 2, 12) > 100);
}

#[test]
fn power_loss_while_threads_verify_a_copy_of_many_batches_keeps_every_acknowledged_commit() {
    let recording = record_on(400, 2, 8, 400, 2, 2000, 4);
    let temp = recording
        .ops
        .iter()
        .find_map(|op| match op {
            Op::Create(inode) => Some(*inode),
            _ => None,
        })
        .unwrap();
    let copied: usize = recording
        .ops
        .iter()
        .map(|op| match op {
            Op::Write(inode, _, bytes) if *inode == temp => bytes.len(),
            _ => 0,
        })
        .sum();
    assert!(copied > 512 << 10, "{copied}");
    assert!(replay(&recording, 3, 13) > 150);
}

#[test]
fn the_new_file_is_durable_before_it_is_named_and_named_before_it_is_used() {
    let recording = record(3, 10, 20, 1);
    let mut unsynced: HashMap<u64, usize> = HashMap::new();
    let mut unnamed: Option<u64> = None;
    let mut renames = 0;
    for op in &recording.ops {
        match op {
            Op::Write(inode, ..) | Op::SetLen(inode, _) => {
                assert_ne!(
                    Some(*inode),
                    unnamed,
                    "written before the rename was durable"
                );
                *unsynced.entry(*inode).or_default() += 1;
            }
            Op::Sync(inode) => {
                unsynced.insert(*inode, 0);
            }
            Op::Rename(inode) => {
                assert_eq!(
                    unsynced.get(inode).copied().unwrap_or(0),
                    0,
                    "named before it was durable"
                );
                unnamed = Some(*inode);
                renames += 1;
            }
            Op::SyncDir => unnamed = None,
            Op::Acked(_) => assert_eq!(unnamed, None, "acknowledged before the rename was durable"),
            Op::Create(_) => {}
        }
    }
    assert_eq!(renames, 3);
}

#[test]
fn either_superblock_of_a_new_generation_can_be_damaged() {
    let recording = record(3, 10, 24, 1);
    let dir = dir();
    let path = dir.path().join("image.ferro");
    let mut rng = Rng::new(21);
    let mut checked = 0;
    for cut in 0..=recording.ops.len() {
        if !matches!(
            cut.checked_sub(1).map(|at| &recording.ops[at]),
            Some(Op::SyncDir)
        ) {
            continue;
        }
        let state = crash(&recording, cut, Loss::Nothing, &mut rng);
        for slot in [0, SLOT_BYTES] {
            let mut damaged = state.named.clone();
            damaged[slot + 8] ^= 0x40;
            fs::write(&path, &damaged).unwrap();
            let reader = Options::new()
                .read_only(true)
                .cache_bytes(1)
                .open::<Root>(&path);
            let db = reader.unwrap_or_else(|error| panic!("cut {cut} slot {slot}: {error}"));
            assert_eq!(
                dump(&db.read()),
                recording.models[&db.version()],
                "cut {cut}"
            );
            db.verify()
                .unwrap_or_else(|error| panic!("cut {cut} slot {slot}: {error}"));
            checked += 1;
        }
    }
    assert!(checked >= 6, "{checked}");
}
