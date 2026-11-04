mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::{Rng, cases, text};
use ferro::db::Hook;
use ferro::store::{Op, Trace};
use ferro::{Db, Durability, Map, Options, Reclaim};
use parking_lot::Mutex;
use tempfile::TempDir;

type Text = Map<u64, String>;
type Kind = (&'static str, fn() -> Options);

const WRITERS: u64 = 3;
const PERIOD: u64 = 40;
const FORK_AT: u64 = 10;
const DROP_AT: u64 = 15;
const SPREAD: u64 = 1 << 32;
const SECTOR: usize = 512;
const LOG_START: u64 = 8192;
const MANUAL: Reclaim = Reclaim { every_bytes: 0 };

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn fsync() -> Options {
    Options::new().reclaim(MANUAL)
}

fn buffered() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .reclaim(MANUAL)
        .checkpoint_bytes(8192)
}

fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_micros(200));
    }
}

fn writer(branch: u64) -> String {
    format!("w{branch}")
}

fn value(branch: u64, round: u64) -> String {
    text(branch * 1000 + round, 40)
}

fn forked(round: u64) -> bool {
    round % PERIOD == FORK_AT
}

fn dropped(round: u64) -> Option<u64> {
    (round % PERIOD == DROP_AT).then(|| round - (DROP_AT - FORK_AT))
}

struct Held {
    placed: AtomicBool,
    released: AtomicBool,
    landed: AtomicUsize,
}

fn holding() -> (Arc<Held>, Hook) {
    let held = Arc::new(Held {
        placed: AtomicBool::new(false),
        released: AtomicBool::new(false),
        landed: AtomicUsize::new(0),
    });
    let seen = held.clone();
    let hook = Hook(Arc::new(move |step| {
        match (thread::current().name(), step) {
            (Some("held"), "landing") => {
                seen.placed.store(true, Ordering::SeqCst);
                wait_until("the held frame's release", || {
                    seen.released.load(Ordering::SeqCst)
                });
            }
            (Some("ahead"), "landed") => {
                seen.landed.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }));
    (held, hook)
}

fn commit(db: &Db<Text>, branch: u64, round: u64, name: &str) -> JoinHandle<()> {
    let db = db.branch(&writer(branch)).unwrap();
    thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let done = db
                .commit_with(|root| {
                    root.insert(round, value(branch, round));
                    ("", ())
                })
                .unwrap();
            assert_eq!(done.version, round + 1);
            db.trace_op(Op::Acked(branch * SPREAD + done.version));
        })
        .unwrap()
}

struct Recording {
    base: Vec<u8>,
    ops: Vec<Op>,
    rounds: u64,
}

fn record(path: &Path, options: fn() -> Options, rounds: u64) -> Recording {
    {
        let db: Db<Text> = options().open(path).unwrap();
        db.update(|root| {
            root.insert(0, "base".to_string());
        })
        .unwrap();
        for branch in 0..WRITERS {
            db.fork(&writer(branch)).unwrap();
        }
    }
    let base = fs::read(path).unwrap();
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let (held, hook) = holding();
    {
        let db: Db<Text> = options()
            .trace(trace.clone())
            .hook(hook)
            .open(path)
            .unwrap();
        for round in 1..=rounds {
            if forked(round) {
                db.fork(&format!("extra-{round}")).unwrap();
            }
            if let Some(fork) = dropped(round) {
                db.drop_branch(&format!("extra-{fork}")).unwrap();
            }
            held.placed.store(false, Ordering::SeqCst);
            held.released.store(false, Ordering::SeqCst);
            held.landed.store(0, Ordering::SeqCst);
            let first = commit(&db, 0, round, "held");
            wait_until("the held frame to be placed", || {
                held.placed.load(Ordering::SeqCst)
            });
            let later: Vec<_> = (1..WRITERS)
                .map(|branch| commit(&db, branch, round, "ahead"))
                .collect();
            wait_until("the later frames to land", || {
                held.landed.load(Ordering::SeqCst) == later.len()
            });
            held.released.store(true, Ordering::SeqCst);
            first.join().unwrap();
            for handle in later {
                handle.join().unwrap();
            }
        }
    }
    let ops = trace.lock().clone();
    Recording { base, ops, rounds }
}

#[derive(Clone, Copy, Debug)]
enum Loss {
    Everything,
    Nothing,
    Some,
    Torn,
    SuperblocksOnly,
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

fn crash(recording: &Recording, cut: usize, loss: Loss, durable: bool, rng: &mut Rng) -> Crash {
    let mut image = recording.base.clone();
    let mut pending: Vec<&Op> = Vec::new();
    let mut floors = vec![1; WRITERS as usize];
    let mut newest = floors.clone();
    for op in &recording.ops[..cut] {
        match op {
            Op::Write(..) | Op::SetLen(..) => pending.push(op),
            Op::Sync(_) => {
                for done in pending.drain(..) {
                    apply(&mut image, done);
                }
                floors.clone_from(&newest);
            }
            Op::Acked(marker) => {
                let branch = (marker / SPREAD) as usize;
                newest[branch] = newest[branch].max(marker % SPREAD);
                if durable {
                    floors[branch] = floors[branch].max(marker % SPREAD);
                }
            }
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
            Loss::FramesOnly if !is_superblock(op) => apply(&mut image, op),
            Loss::SuperblocksOnly | Loss::FramesOnly => {}
        }
    }
    Crash { image, floors }
}

struct Crash {
    image: Vec<u8>,
    floors: Vec<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Landed {
    Yes,
    Maybe,
    No,
}

fn landed(op_round: u64, last: u64, whole: bool) -> Landed {
    match op_round.cmp(&(last + 1)) {
        std::cmp::Ordering::Less => Landed::Yes,
        std::cmp::Ordering::Equal if whole => Landed::Maybe,
        _ => Landed::No,
    }
}

fn check_heads(db: &Db<Text>, recording: &Recording, floors: &[u64], context: &str) {
    let versions: Vec<u64> = (0..WRITERS)
        .map(|branch| {
            let handle = db
                .branch(&writer(branch))
                .unwrap_or_else(|error| panic!("{context}: w{branch}: {error}"));
            let snapshot = handle.read();
            let version = snapshot.version();
            assert!(
                version >= floors[branch as usize],
                "{context}: w{branch} at v{version} lost v{}",
                floors[branch as usize]
            );
            let mut expected: BTreeMap<u64, String> = BTreeMap::new();
            expected.insert(0, "base".to_string());
            for round in 1..version {
                expected.insert(round, value(branch, round));
            }
            let found: BTreeMap<u64, String> = snapshot
                .iter()
                .map(|(key, value)| (key, (*value).clone()))
                .collect();
            assert_eq!(found, expected, "{context}: w{branch} at v{version}");
            version
        })
        .collect();
    let lead = versions[0];
    let low = *versions.iter().min().unwrap();
    assert!(
        versions.iter().all(|version| *version <= lead) && lead <= low + 1,
        "{context}: heads {versions:?} name no prefix of the frames"
    );
    let last = lead - 1;
    let whole = low == lead;
    let names = db.branches();
    for round in 1..=recording.rounds {
        if !forked(round) {
            continue;
        }
        let fork = landed(round, last, whole);
        let drop = landed(round + DROP_AT - FORK_AT, last, whole);
        let exists = names.contains(&format!("extra-{round}"));
        let allowed = match (fork, drop) {
            (Landed::No, _) | (Landed::Yes, Landed::Yes) => [false, false],
            (Landed::Yes, Landed::No) => [true, true],
            _ => [false, true],
        };
        assert!(
            allowed.contains(&exists),
            "{context}: extra-{round} {} with heads {versions:?}",
            if exists { "exists" } else { "is missing" }
        );
        if exists {
            let extra = db.branch(&format!("extra-{round}")).unwrap();
            assert_eq!(extra.version(), 1, "{context}: extra-{round}");
        }
    }
}

fn check(
    path: &Path,
    recording: &Recording,
    state: &Crash,
    options: fn() -> Options,
    context: &str,
) {
    let reader: Db<Text> = Options::new()
        .read_only(true)
        .cache_bytes(1)
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: read-only open: {error}"));
    check_heads(&reader, recording, &state.floors, context);
    drop(reader);
    let db: Db<Text> = options()
        .cache_bytes(1)
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: open: {error}"));
    check_heads(&db, recording, &state.floors, context);
    db.verify()
        .unwrap_or_else(|error| panic!("{context}: verify: {error}"));
    let info = ferro::admin::info(path).unwrap();
    for branch in &info.branches {
        let handle = db.branch(&branch.name).unwrap();
        assert_eq!(branch.version, handle.version(), "{context}: admin info");
    }
    let first = db.branch(&writer(1)).unwrap();
    first
        .update(|root| {
            root.insert(u64::MAX, "after".to_string());
        })
        .unwrap_or_else(|error| panic!("{context}: commit: {error}"));
    let after = first.version();
    drop((db, first));
    let again: Db<Text> = options()
        .open(path)
        .unwrap_or_else(|error| panic!("{context}: reopen: {error}"));
    assert_eq!(
        again.branch(&writer(1)).unwrap().version(),
        after,
        "{context}"
    );
    again
        .verify()
        .unwrap_or_else(|error| panic!("{context}: verify again: {error}"));
}

fn frames(ops: &[Op]) -> usize {
    ops.iter()
        .filter(|op| matches!(op, Op::Write(_, at, bytes) if *at >= LOG_START && bytes.starts_with(b"FERROFRM")))
        .count()
}

#[test]
fn a_power_cut_across_steps_tables_and_frames_out_of_order_keeps_one_prefix() {
    let modes: [Kind; 2] = [("fsync", fsync), ("buffered", buffered)];
    for (seed, (kind, options)) in modes.into_iter().enumerate() {
        let dir = dir();
        let rounds = 60;
        let recording = record(&dir.path().join("trace.ferro"), options, rounds);
        assert!(frames(&recording.ops) as u64 >= rounds * WRITERS);
        let image = dir.path().join("image.ferro");
        let mut rng = Rng::new(90 + seed as u64);
        let stride = (recording.ops.len() / (40 * cases(1) as usize)).max(1);
        let mut images = 0;
        for cut in (0..=recording.ops.len()).step_by(stride) {
            for loss in [
                Loss::Everything,
                Loss::Nothing,
                Loss::Some,
                Loss::Torn,
                Loss::SuperblocksOnly,
                Loss::FramesOnly,
            ] {
                let durable = kind == "fsync";
                let state = crash(&recording, cut, loss, durable, &mut rng);
                fs::write(&image, &state.image).unwrap();
                let context = format!("{kind}: cut {cut} of {} {loss:?}", recording.ops.len());
                check(&image, &recording, &state, options, &context);
                images += 1;
            }
        }
        assert!(images > 200, "{kind}: only {images} images");
    }
}
