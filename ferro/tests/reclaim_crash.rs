mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::{Rng, cases, text};
use ferro::db::Hook;
use ferro::store::{Op, Trace};
use ferro::{Db, Durability, Keep, Map, Options, Reclaim};
use parking_lot::Mutex;
use tempfile::TempDir;

type Root = Map<u64, String>;
type Model = BTreeMap<u64, String>;

const SECTOR: usize = 512;
const LOG_START: usize = 8192;

const MANUAL: Reclaim = Reclaim {
    every_bytes: 0,
    fill: 0.5,
};

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn dump(root: &Root) -> Model {
    root.iter().map(|(k, v)| (k, (*v).clone())).collect()
}

struct Recording {
    base: Vec<u8>,
    ops: Vec<Op>,
    models: BTreeMap<u64, Model>,
}

fn record(cycles: u64, commits: u64, keys: u64, keep: usize, fill_at: f64, torn_len: usize) -> Recording {
    let dir = dir();
    let path = dir.path().join("trace.ferro");
    let mut rng = Rng::new(cycles * 977 + commits + torn_len as u64);
    let mut models = BTreeMap::new();
    models.insert(0, Model::new());
    {
        let db: Db<Root> = Options::new()
            .durability(Durability::Buffered)
            .reclaim(MANUAL)
            .open(&path)
            .unwrap();
        for i in 0..commits {
            db.update(|root| {
                root.insert(i % keys, text(i, 40));
            })
            .unwrap();
            models.insert(db.version(), dump(&db.read()));
        }
        db.compact(Keep::Last(keep)).unwrap();
    }
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let mut base = fs::read(&path).unwrap();
    let db: Db<Root> = Options::new()
        .reclaim(Reclaim {
            every_bytes: 0,
            fill: fill_at,
        })
        .trace(trace.clone())
        .open(&path)
        .unwrap();
    base.truncate(db.size_log() as usize);
    for _ in 0..cycles {
        for _ in 0..commits {
            let key = rng.below(keys);
            let value = text(rng.next(), rng.below(900) as usize + 1);
            db.update(|root| {
                root.insert(key, value);
            })
            .unwrap();
            models.insert(db.version(), dump(&db.read()));
            db.trace_op(Op::Acked(db.version()));
        }
        db.compact(Keep::Last(keep)).unwrap();
    }
    drop(db);
    let ops = trace.lock().clone();
    Recording { base, ops, models }
}

fn apply(image: &mut Vec<u8>, op: &Op) {
    match op {
        Op::Write(at, bytes) => {
            let end = *at as usize + bytes.len();
            if image.len() < end {
                image.resize(end, 0);
            }
            image[*at as usize..end].copy_from_slice(bytes);
        }
        Op::SetLen(len) => image.resize(*len as usize, 0),
        Op::Punch(at, len) => {
            let end = ((*at + *len) as usize).min(image.len());
            image[*at as usize..end].fill(0);
        }
        Op::Sync | Op::Acked(_) => {}
    }
}

fn torn(image: &mut Vec<u8>, op: &Op, rng: &mut Rng) {
    let Op::Write(at, bytes) = op else {
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

#[derive(Clone, Copy)]
enum Loss {
    Everything,
    Nothing,
    Some,
    Torn,
}

fn crash_image(recording: &Recording, cut: usize, loss: Loss, rng: &mut Rng) -> (Vec<u8>, u64) {
    let synced = recording.ops[..cut]
        .iter()
        .rposition(|op| *op == Op::Sync)
        .map_or(0, |index| index + 1);
    let mut image = recording.base.clone();
    for op in &recording.ops[..synced] {
        apply(&mut image, op);
    }
    for op in &recording.ops[synced..cut] {
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
    let acked = recording.ops[..cut]
        .iter()
        .filter_map(|op| match op {
            Op::Acked(version) => Some(*version),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    (image, acked)
}

fn check(path: &Path, recording: &Recording, acked: u64, context: &str) {
    let reader: Db<Root> = Options::new().read_only().cache_bytes(1).open(path).unwrap_or_else(|error| panic!("{context}: {error}"));
    assert!(reader.version() >= acked, "{context}: read-only lost {acked}");
    assert_eq!(dump(&reader.read()), recording.models[&reader.version()], "{context}");
    drop(reader);
    let db: Db<Root> = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1)
        .reclaim(MANUAL)
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: {error}"));
    assert!(db.version() >= acked, "{context}: lost {acked}, at {}", db.version());
    assert_eq!(dump(&db.read()), recording.models[&db.version()], "{context}");
    let log: Vec<u64> = db.log().iter().map(|(v, _)| v).collect();
    for version in log {
        assert_eq!(
            dump(&db.at(version).unwrap()),
            recording.models[&version],
            "{context} v{version}"
        );
    }
    db.verify().unwrap_or_else(|error| panic!("{context}: {error}"));
    db.update(|root| {
        root.insert(u64::MAX, "after".into());
    })
    .unwrap();
    db.compact(Keep::Last(1)).unwrap_or_else(|error| panic!("{context}: {error}"));
    db.verify().unwrap_or_else(|error| panic!("{context}: {error}"));
}

fn replay(recording: &Recording, stride: usize, seed: u64) -> usize {
    let dir = dir();
    let path = dir.path().join("image.ferro");
    let mut rng = Rng::new(seed);
    let mut images = 0;
    let mut cut = 0;
    while cut <= recording.ops.len() {
        for loss in [Loss::Everything, Loss::Nothing, Loss::Some, Loss::Torn] {
            let (image, acked) = crash_image(recording, cut, loss, &mut rng);
            fs::write(&path, &image).unwrap();
            check(&path, recording, acked, &format!("cut {cut} of {}", recording.ops.len()));
            images += 1;
        }
        cut += stride.max(1);
    }
    images
}

#[test]
fn power_loss_at_any_operation_keeps_every_acknowledged_commit() {
    let recording = record(3, 12, 30, 2, 0.5, 0);
    assert!(recording.ops.iter().any(|op| matches!(op, Op::Punch(..))));
    assert!(replay(&recording, 3, 11) > 100);
}

#[test]
fn power_loss_with_everything_relocated_keeps_every_acknowledged_commit() {
    let recording = record(3, 12, 30, 1, 2.0, 1);
    assert!(replay(&recording, 3, 12) > 100);
}

#[test]
fn power_loss_with_nothing_relocated_keeps_every_acknowledged_commit() {
    let recording = record(2, 12, 30, 1, 0.0, 2);
    assert!(replay(&recording, 3, 13) > 50);
}

#[test]
fn a_damaged_newest_superblock_falls_back_without_losing_acknowledged_data() {
    let recording = record(3, 10, 24, 1, 2.0, 3);
    let dir = dir();
    let path = dir.path().join("image.ferro");
    let mut rng = Rng::new(21);
    let mut checked = 0;
    for cut in 0..=recording.ops.len() {
        let Some(Op::Sync) = recording.ops.get(cut) else {
            continue;
        };
        let (mut image, _) = crash_image(&recording, cut + 1, Loss::Nothing, &mut rng);
        let (first, second) = (superblock_seq(&image, 0), superblock_seq(&image, 4096));
        if first == second {
            for slot in [0, 4096] {
                let mut damaged = image.clone();
                damaged[slot + 8] ^= 0x40;
                fs::write(&path, &damaged).unwrap();
                let reader = Options::new().read_only().cache_bytes(1).open::<Root>(&path);
                let db = reader.unwrap_or_else(|error| panic!("cut {cut} slot {slot}: {error}"));
                assert_eq!(dump(&db.read()), recording.models[&db.version()], "cut {cut}");
                db.verify().unwrap_or_else(|error| panic!("cut {cut} slot {slot}: {error}"));
                checked += 1;
            }
        }
        image.clear();
    }
    assert!(checked >= 6, "{checked}");
}

fn superblock_seq(image: &[u8], slot: usize) -> u64 {
    u64::from_le_bytes(image[slot + 8..slot + 16].try_into().unwrap())
}

#[test]
fn nothing_below_the_log_start_is_ever_punched() {
    let recording = record(2, 10, 20, 1, 2.0, 4);
    for op in &recording.ops {
        if let Op::Punch(at, len) = op {
            assert!(*at as usize >= LOG_START);
            assert_eq!(at % 4096, 0);
            assert_eq!(len % 4096, 0);
        }
    }
}

#[test]
fn every_punch_follows_a_durable_table_in_both_slots() {
    let recording = record(3, 10, 20, 1, 2.0, 5);
    let mut image = recording.base.clone();
    let mut synced = image.clone();
    for op in &recording.ops {
        if let Op::Punch(at, len) = op {
            let floor = |image: &[u8], slot: usize| {
                u64::from_le_bytes(image[slot + 52..slot + 60].try_into().unwrap())
            };
            let end = at + len;
            assert!(floor(&synced, 0) >= end, "slot 0 floor below {end}");
            assert!(floor(&synced, 4096) >= end, "slot 1 floor below {end}");
        }
        apply(&mut image, op);
        if *op == Op::Sync {
            synced = image.clone();
        }
    }
}

fn child_writer(path: &str, stage: &'static str) {
    let db: Db<Root> = Options::new()
        .reclaim(MANUAL)
        .hook(Hook(Arc::new(move |name| {
            if name == stage {
                std::process::abort();
            }
        })))
        .open(path)
        .unwrap();
    for round in 0..4u64 {
        for i in 0..40u64 {
            let key = i % 12;
            let value = format!("{round}-{i}-{}", text(i, 30));
            db.update(|root| {
                root.insert(key, value.clone());
            })
            .unwrap();
            println!("ACK {} {key} {value}", db.version());
        }
        if round == 2 {
            db.compact(Keep::Last(3)).unwrap();
        }
    }
}

#[test]
fn process_child() {
    let Ok(path) = std::env::var("FERRO_RECLAIM_PATH") else {
        return;
    };
    let stage = std::env::var("FERRO_RECLAIM_STAGE").unwrap();
    child_writer(&path, Box::leak(stage.into_boxed_str()));
}

#[test]
fn killing_the_process_at_every_stage_keeps_every_acknowledged_commit() {
    for stage in ["marked", "copied", "tabled", "published", "punched"] {
        for attempt in 0..cases(3) {
            let dir = dir();
            let path = dir.path().join("killed.ferro");
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "process_child", "--nocapture", "--test-threads=1"])
                .env("FERRO_RECLAIM_PATH", &path)
                .env("FERRO_RECLAIM_STAGE", stage)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut acked = BTreeMap::new();
            for line in BufReader::new(child.stdout.take().unwrap()).lines() {
                let line = line.unwrap();
                let Some((_, rest)) = line.split_once("ACK ") else {
                    continue;
                };
                let mut fields = rest.splitn(3, ' ');
                let version: u64 = fields.next().unwrap().parse().unwrap();
                let key: u64 = fields.next().unwrap().parse().unwrap();
                acked.insert(version, (key, fields.next().unwrap().to_string()));
            }
            assert!(!child.wait().unwrap().success(), "{stage}: the hook never fired");
            assert!(acked.len() >= 120, "{stage} {attempt}: {}", acked.len());
            let last = *acked.keys().max().unwrap();
            let db: Db<Root> = Options::new().reclaim(MANUAL).open(&path).unwrap();
            assert!(db.version() >= last, "{stage}: {} below {last}", db.version());
            let want: Model = acked.values().cloned().collect::<BTreeMap<_, _>>();
            let root = db.at(last).unwrap();
            assert_eq!(dump(&root), want, "{stage}");
            db.verify().unwrap();
            db.update(|root| {
                root.insert(999, "after".into());
            })
            .unwrap();
            db.compact(Keep::Last(1)).unwrap();
            assert_eq!(dump(&db.read()).len(), want.len() + 1);
            db.verify().unwrap();
        }
    }
}
