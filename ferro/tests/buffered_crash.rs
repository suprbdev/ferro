mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use common::{Rng, assert_damaged, frames_past_the_anchor, judged, text};
use ferro::store::{Op, Trace};
use ferro::{Db, Durability, Map, Options, Reclaim, Snapshot};
use parking_lot::Mutex;
use tempfile::TempDir;

type Root = Map<u64, String>;
type Model = BTreeMap<u64, String>;

const SECTOR: usize = 512;
const LOG_START: u64 = 8192;
const MANUAL: Reclaim = Reclaim { every_bytes: 0 };

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn dump(root: &Root) -> Model {
    root.iter().map(|(k, v)| (k, (*v).clone())).collect()
}

fn buffered() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .reclaim(MANUAL)
}

struct Recording {
    base: Vec<u8>,
    start: u64,
    ops: Vec<Op>,
    models: BTreeMap<u64, Model>,
}

fn commit(db: &Db<Root>, models: &mut BTreeMap<u64, Model>, key: u64, seed: u64) {
    commit_value(db, models, key, text(seed, 60));
}

fn commit_value(db: &Db<Root>, models: &mut BTreeMap<u64, Model>, key: u64, value: String) {
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
    models.insert(db.version(), dump(&db.read()));
    db.trace_op(Op::Acked(db.version()));
}

const WINDOW: usize = 1024;

fn sparse(seed: u64, len: usize) -> String {
    let mut rng = Rng::new(seed);
    let mut value: Vec<u8> = (0..len).map(|_| b' ' + rng.below(95) as u8).collect();
    let window = len / 2;
    value[window..window + WINDOW].fill(0);
    value[window + WINDOW / 2] = 1 << rng.below(7);
    String::from_utf8(value).unwrap()
}

fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    panic!("no merge expected")
}

fn record_sparse(dir: &Path, options: impl Fn() -> Options) -> Recording {
    let path = dir.join("sparse.ferro");
    let source = dir.join("source.ferro");
    let mut models = BTreeMap::new();
    models.insert(0, Model::new());
    {
        let db: Db<Root> = options().open(&path).unwrap();
        for i in 0..2 {
            commit(&db, &mut models, i, i);
        }
    }
    let base = fs::read(&path).unwrap();
    let start = 2;
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let db: Db<Root> = options().trace(trace.clone()).open(&path).unwrap();
    fs::copy(&path, &source).unwrap();
    {
        let sender: Db<Root> = options().open(&source).unwrap();
        sender
            .update(|root| {
                for key in 100..103 {
                    root.insert(key, sparse(key, 400_000));
                }
            })
            .unwrap();
    }
    let sender: Db<Root> = options().open(&source).unwrap();
    db.pull(&sender, "source", refuse).unwrap();
    models.insert(db.version(), dump(&db.read()));
    db.trace_op(Op::Acked(db.version()));
    for key in 0..3 {
        commit_value(&db, &mut models, 200 + key, sparse(200 + key, 300_000));
    }
    drop(db);
    let written = fs::read(&path).unwrap();
    for key in [100, 200] {
        let value = sparse(key, if key == 100 { 400_000 } else { 300_000 });
        let window = &value.as_bytes()[value.len() / 2 - 8..value.len() / 2 + WINDOW + 8];
        assert!(
            written.windows(window.len()).any(|bytes| bytes == window),
            "the value at {key} is stored uncompressed"
        );
    }
    let ops = trace.lock().clone();
    Recording {
        base,
        start,
        ops,
        models,
    }
}

fn record(path: &Path, sessions: u64, commits: u64, options: impl Fn() -> Options) -> Recording {
    let mut models = BTreeMap::new();
    models.insert(0, Model::new());
    {
        let db: Db<Root> = options().open(path).unwrap();
        for i in 0..commits {
            commit(&db, &mut models, i % 7, i);
        }
    }
    let base = fs::read(path).unwrap();
    let start = commits;
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    for session in 0..sessions {
        let db: Db<Root> = options().trace(trace.clone()).open(path).unwrap();
        for i in 0..commits {
            commit(&db, &mut models, i % 7, (session + 1) * 1000 + i);
        }
    }
    let ops = trace.lock().clone();
    Recording {
        base,
        start,
        ops,
        models,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Loss {
    Everything,
    Nothing,
    Some,
    Torn,
    SuperblocksOnly,
    SuperblocksOverZeroes,
    FramesOnly,
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

fn is_superblock(op: &Op) -> bool {
    matches!(op, Op::Write(_, at, _) if *at < LOG_START)
}

fn hollow(image: &mut Vec<u8>, op: &Op) {
    if let Op::Write(_, at, bytes) = op {
        let end = *at as usize + bytes.len();
        if image.len() < end {
            image.resize(end, 0);
        }
    }
}

struct Crash {
    image: Vec<u8>,
    floor: u64,
    newest: u64,
}

fn crash(recording: &Recording, cut: usize, loss: Loss, rng: &mut Rng) -> Crash {
    let mut image = recording.base.clone();
    let mut pending: Vec<&Op> = Vec::new();
    let (mut floor, mut newest) = (recording.start, recording.start);
    for op in &recording.ops[..cut] {
        match op {
            Op::Write(..) | Op::SetLen(..) => pending.push(op),
            Op::Sync(_) => {
                for done in pending.drain(..) {
                    apply(&mut image, done);
                }
                floor = newest;
            }
            Op::Acked(version) => newest = *version,
            _ => {}
        }
    }
    for op in pending {
        match loss {
            Loss::Everything => {}
            Loss::Nothing => apply(&mut image, op),
            Loss::Some => {
                if rng.chance(50) {
                    apply(&mut image, op);
                }
            }
            Loss::Torn => torn(&mut image, op, rng),
            Loss::SuperblocksOnly if is_superblock(op) => apply(&mut image, op),
            Loss::SuperblocksOverZeroes if is_superblock(op) => apply(&mut image, op),
            Loss::SuperblocksOverZeroes => hollow(&mut image, op),
            Loss::FramesOnly if !is_superblock(op) => apply(&mut image, op),
            Loss::SuperblocksOnly | Loss::FramesOnly => {}
        }
    }
    Crash {
        image,
        floor,
        newest,
    }
}

fn check(path: &Path, recording: &Recording, state: &Crash, context: &str) {
    let reader: Db<Root> = Options::new()
        .read_only(true)
        .cache_bytes(1)
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: read-only open: {error}"));
    assert!(
        reader.version() >= state.floor,
        "{context}: read-only lost {}",
        state.floor
    );
    assert!(
        reader.version() <= state.newest + 1,
        "{context}: read-only is ahead"
    );
    assert_eq!(
        dump(&reader.read()),
        recording.models[&reader.version()],
        "{context}"
    );
    let seen = reader.version();
    reader
        .refresh()
        .unwrap_or_else(|error| panic!("{context}: refresh: {error}"));
    assert_eq!(
        reader.version(),
        seen,
        "{context}: refresh moved a stopped database"
    );
    let db: Db<Root> = buffered()
        .cache_bytes(1)
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: open: {error}"));
    assert_eq!(db.version(), seen, "{context}: writer and reader disagree");
    assert_eq!(
        dump(&db.read()),
        recording.models[&db.version()],
        "{context}"
    );
    let follower: Db<Root> = Options::new()
        .read_only(true)
        .cache_bytes(1)
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: follower open: {error}"));
    assert_eq!(
        follower.version(),
        seen,
        "{context}: a follower saw lost commits"
    );
    drop(follower);
    db.verify()
        .unwrap_or_else(|error| panic!("{context}: verify: {error}"));
    db.update(|root| {
        root.insert(u64::MAX, "after".into());
    })
    .unwrap_or_else(|error| panic!("{context}: commit: {error}"));
    let after = db.version();
    reader
        .refresh()
        .unwrap_or_else(|error| panic!("{context}: refresh after: {error}"));
    assert_eq!(
        reader.version(),
        after,
        "{context}: reader did not follow the recovery"
    );
    drop((db, reader));
    let again: Db<Root> = buffered()
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: reopen: {error}"));
    assert_eq!(again.version(), after, "{context}");
    again
        .verify()
        .unwrap_or_else(|error| panic!("{context}: verify again: {error}"));
}

fn replay(recording: &Recording, stride: usize, seed: u64) -> usize {
    let dir = dir();
    let path = dir.path().join("image.ferro");
    let mut rng = Rng::new(seed);
    let mut images = 0;
    let mut cut = 0;
    while cut <= recording.ops.len() {
        for loss in [
            Loss::Everything,
            Loss::Nothing,
            Loss::Some,
            Loss::Torn,
            Loss::SuperblocksOnly,
            Loss::SuperblocksOverZeroes,
            Loss::FramesOnly,
        ] {
            let state = crash(recording, cut, loss, &mut rng);
            fs::write(&path, &state.image).unwrap();
            let context = format!("cut {cut} of {} {loss:?}", recording.ops.len());
            check(&path, recording, &state, &context);
            images += 1;
        }
        cut += stride.max(1);
    }
    images
}

fn damage(recording: &Recording, losses: &[Loss], seed: u64, name: &str) -> (usize, usize) {
    let dir = dir();
    let path = dir.path().join("image.ferro");
    let mut rng = Rng::new(seed);
    let (mut reported, mut acknowledged) = (0, 0);
    for cut in 0..=recording.ops.len() {
        for &loss in losses {
            let state = crash(recording, cut, loss, &mut rng);
            let frames = frames_past_the_anchor(&state.image);
            let Some(&(start, end)) = frames.get(rng.below(frames.len().max(1) as u64) as usize)
            else {
                continue;
            };
            let at = start + rng.below((end - start) as u64) as usize;
            let mut image = state.image.clone();
            image[at] ^= 1 << rng.below(8);
            if !judged(&image, at, start, end) {
                continue;
            }
            fs::write(&path, &state.image).unwrap();
            let whole: Db<Root> = Options::new().read_only(true).open(&path).unwrap();
            let kept = whole.version();
            drop(whole);
            fs::write(&path, &image).unwrap();
            let context = format!("{name}: cut {cut} {loss:?}, byte {at} of the frame at {start}");
            let readonly = Options::new().read_only(true).open::<Root>(&path);
            assert_damaged(readonly, start, at, &context);
            assert_damaged(buffered().open::<Root>(&path), start, at, &context);
            assert_eq!(
                fs::read(&path).unwrap(),
                image,
                "{context}: the file changed"
            );
            reported += 1;
            let index = frames.iter().position(|frame| frame.0 == start).unwrap();
            if kept as i64 - (frames.len() - 1 - index) as i64 <= state.newest as i64 {
                acknowledged += 1;
            }
        }
    }
    (reported, acknowledged)
}

#[test]
fn a_flipped_bit_in_a_whole_frame_past_the_anchor_is_reported_after_any_cut() {
    let dir = dir();
    let recording = record(&dir.path().join("trace.ferro"), 2, 12, buffered);
    let losses = [Loss::Nothing, Loss::Some, Loss::Torn, Loss::FramesOnly];
    let (reported, acknowledged) = damage(&recording, &losses, 45, "buffered");
    assert!(reported > 100, "only {reported} frames damaged");
    assert!(
        acknowledged > 50,
        "only {acknowledged} acknowledged frames damaged"
    );
}

#[test]
fn a_flipped_bit_in_an_acknowledged_frame_whose_superblock_was_lost_is_reported() {
    let dir = dir();
    let fsync = || Options::new().reclaim(MANUAL);
    let recording = record(&dir.path().join("trace.ferro"), 2, 8, fsync);
    let (reported, acknowledged) = damage(&recording, &[Loss::FramesOnly], 46, "fsync");
    assert!(reported > 20, "only {reported} frames damaged");
    assert!(
        acknowledged > 10,
        "only {acknowledged} acknowledged frames damaged"
    );
}

fn sweep(recording: &Recording, name: &str) -> usize {
    let dir = dir();
    let path = dir.path().join("image.ferro");
    let mut image = recording.base.clone();
    let mut swept = 0;
    for op in &recording.ops {
        if let Op::Write(_, at, bytes) = op
            && *at >= LOG_START
        {
            let start = *at as usize;
            let end = start + bytes.len();
            let mut sector = start - start % SECTOR;
            while sector < end {
                let (from, to) = (sector.max(start), (sector + SECTOR).min(end));
                let set: u32 = bytes[from - start..to - start]
                    .iter()
                    .map(|byte| byte.count_ones())
                    .sum();
                if (set > 0 && set <= 8) || from == start || to == end {
                    let mut torn = image.clone();
                    if torn.len() < end {
                        torn.resize(end, 0);
                    }
                    torn[start..from].copy_from_slice(&bytes[..from - start]);
                    torn[to..end].copy_from_slice(&bytes[to - start..]);
                    fs::write(&path, &torn).unwrap();
                    let context = format!("{name}: sector {sector} of the write at {start} lost");
                    let reader: Db<Root> = Options::new()
                        .read_only(true)
                        .open(&path)
                        .unwrap_or_else(|error| panic!("{context}: read-only open: {error}"));
                    let seen = reader.version();
                    assert_eq!(dump(&reader.read()), recording.models[&seen], "{context}");
                    drop(reader);
                    let db: Db<Root> = buffered()
                        .open(&path)
                        .unwrap_or_else(|error| panic!("{context}: open: {error}"));
                    assert_eq!(db.version(), seen, "{context}");
                    db.verify()
                        .unwrap_or_else(|error| panic!("{context}: verify: {error}"));
                    swept += 1;
                }
                sector += SECTOR;
            }
        }
        apply(&mut image, op);
    }
    swept
}

#[test]
fn a_power_cut_never_leaves_a_database_of_sparse_values_that_will_not_open() {
    let fsync = || Options::new().reclaim(MANUAL);
    for (name, options, seed) in [
        ("buffered", buffered as fn() -> Options, 47),
        ("fsync", fsync, 48),
    ] {
        let dir = dir();
        let recording = record_sparse(dir.path(), options);
        let spilled = recording
            .ops
            .iter()
            .filter(
                |op| matches!(op, Op::Write(_, at, bytes) if *at >= LOG_START && bytes.len() == 68),
            )
            .count();
        assert!(spilled > 2, "{name}: {spilled} frames spilled");
        assert!(replay(&recording, 1, seed) > 100, "{name}");
        let swept = sweep(&recording, name);
        assert!(swept > 20, "{name}: only {swept} sectors swept");
    }
}

#[test]
fn a_power_cut_never_leaves_a_buffered_database_that_will_not_open() {
    let dir = dir();
    let recording = record(&dir.path().join("trace.ferro"), 2, 12, buffered);
    assert!(replay(&recording, 3, 41) > 100);
}

#[test]
fn fsync_commits_survive_the_same_cuts() {
    let dir = dir();
    let fsync = || Options::new().reclaim(MANUAL);
    let recording = record(&dir.path().join("trace.ferro"), 2, 8, fsync);
    assert!(replay(&recording, 3, 42) > 100);
}

#[test]
fn a_clean_close_leaves_nothing_to_lose() {
    let dir = dir();
    let recording = record(&dir.path().join("trace.ferro"), 1, 12, buffered);
    let mut rng = Rng::new(43);
    let state = crash(&recording, recording.ops.len(), Loss::Everything, &mut rng);
    assert_eq!(state.floor, 24);
    let path = dir.path().join("image.ferro");
    fs::write(&path, &state.image).unwrap();
    check(&path, &recording, &state, "after close");
}

#[test]
fn checkpoints_bound_what_a_power_cut_can_lose() {
    let dir = dir();
    let small = || buffered().checkpoint_bytes(4096);
    let recording = record(&dir.path().join("trace.ferro"), 1, 40, small);
    let syncs = recording
        .ops
        .iter()
        .filter(|op| matches!(op, Op::Sync(_)))
        .count();
    assert!(
        syncs >= 4,
        "only {syncs} syncs in {} ops",
        recording.ops.len()
    );
    assert!(replay(&recording, 4, 44) > 100);
}

#[test]
fn nothing_before_a_checkpoint_is_rolled_back() {
    let dir = dir();
    let path = dir.path().join("live.ferro");
    let copy = dir.path().join("image.ferro");
    let db: Db<Root> = buffered().checkpoint_bytes(4096).open(&path).unwrap();
    let start = fs::metadata(&path).unwrap().len() as usize;
    let mut models = BTreeMap::new();
    for i in 0..40 {
        commit(&db, &mut models, i % 7, i);
    }
    let mut image = fs::read(&path).unwrap();
    image[start] ^= 0x40;
    fs::write(&copy, &image).unwrap();
    let reader: Db<Root> = Options::new()
        .read_only(true)
        .cache_bytes(1)
        .open(&copy)
        .unwrap();
    assert!(
        reader.version() > 30,
        "rolled back to v{}",
        reader.version()
    );
    drop(reader);
    let writer: Db<Root> = buffered().cache_bytes(1).open(&copy).unwrap();
    assert!(
        writer.version() > 30,
        "rolled back to v{}",
        writer.version()
    );
    assert_eq!(dump(&writer.read()), models[&writer.version()]);
}

#[test]
fn recovery_settles_either_slot_and_never_scans_old_frames_again() {
    for commits in [7, 8] {
        let dir = dir();
        let path = dir.path().join("live.ferro");
        let copy = dir.path().join("image.ferro");
        let db: Db<Root> = buffered().open(&path).unwrap();
        let start = fs::metadata(&path).unwrap().len() as usize;
        let mut models = BTreeMap::new();
        for i in 0..commits {
            commit(&db, &mut models, i % 7, i);
        }
        fs::copy(&path, &copy).unwrap();
        drop(db);
        drop(buffered().open::<Root>(&copy).unwrap());
        let mut image = fs::read(&copy).unwrap();
        image[start] ^= 0x40;
        fs::write(&copy, &image).unwrap();
        let reopened: Db<Root> = buffered().cache_bytes(1).open(&copy).unwrap();
        assert_eq!(reopened.version(), commits, "rolled back");
    }
}
