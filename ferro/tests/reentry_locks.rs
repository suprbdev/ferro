#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle, LocalKey};
use std::time::{Duration, Instant};

use ferro::codec::{Decoder, Encoder};
use ferro::db::Hook;
use ferro::{Bundle, Data, Db, Durability, Error, Keep, Map, Options, Reclaim, Snapshot};
use tempfile::TempDir;

const MANUAL: Reclaim = Reclaim::off();
const PATIENCE: Duration = Duration::from_secs(60);
const CACHE: u64 = 64 << 10;
const SMALL: u64 = 200;
const PAD: usize = 2 << 10;
const BIG: u64 = 5;
const BLOB: usize = 24 << 10;
const SPILLED: usize = 400 << 10;

type Slot = RefCell<Option<Box<dyn FnOnce()>>>;
type Seen = Rc<RefCell<Option<(&'static str, ferro::Result<()>)>>>;

thread_local! {
    static ON_DECODE: Slot = const { RefCell::new(None) };
    static ON_DROP: Slot = const { RefCell::new(None) };
    static ON_PLACED: Slot = const { RefCell::new(None) };
    static ARMED_AT: Cell<u64> = const { Cell::new(0) };
    static STEP: Cell<&'static str> = const { Cell::new("") };
}

static DECODED: AtomicU64 = AtomicU64::new(1);

fn arm(slot: &'static LocalKey<Slot>, run: impl FnOnce() + 'static) {
    ARMED_AT.with(|at| at.set(DECODED.load(Ordering::SeqCst)));
    slot.with(|slot| *slot.borrow_mut() = Some(Box::new(run)));
}

fn run_armed(slot: &'static LocalKey<Slot>) {
    let armed = slot
        .try_with(|slot| slot.borrow_mut().take())
        .ok()
        .flatten();
    if let Some(run) = armed {
        run();
    }
}

fn fired(slot: &'static LocalKey<Slot>) -> bool {
    slot.with(|slot| slot.borrow_mut().take().is_none())
}

fn hook() -> Hook {
    Hook(Arc::new(|step: &'static str| {
        let _ = STEP.try_with(|at| at.set(step));
        if step == "placed" {
            run_armed(&ON_PLACED);
        }
    }))
}

#[derive(Default)]
struct Probe(u64);

impl Clone for Probe {
    fn clone(&self) -> Self {
        Probe(0)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let before = ARMED_AT.try_with(Cell::get).unwrap_or(0);
        if self.0 != 0 && self.0 < before {
            run_armed(&ON_DROP);
        }
    }
}

impl Data for Probe {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.u8(0);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        r.u8()?;
        run_armed(&ON_DECODE);
        Ok(Probe(DECODED.fetch_add(1, Ordering::SeqCst)))
    }
}

#[derive(Data, Clone, Default)]
struct Probed {
    rows: Map<u64, u64>,
    blob: Vec<u8>,
    probe: Probe,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Host {
    Decode,
    Drop,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Outer {
    Read,
    Attach,
    Fork,
    ReaderRefresh,
    Compact,
    Receive,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Inner {
    Branches,
    Verify,
    Have,
    Read,
    Pull,
    Push,
    Receive,
    Spilled,
    Compact,
}

#[derive(Debug, PartialEq)]
enum Expect {
    Done,
    Reentrant,
}

const INNERS: [Inner; 9] = [
    Inner::Branches,
    Inner::Verify,
    Inner::Have,
    Inner::Read,
    Inner::Pull,
    Inner::Push,
    Inner::Receive,
    Inner::Spilled,
    Inner::Compact,
];

type Kind = (&'static str, fn() -> Options);

fn kinds() -> Vec<Kind> {
    vec![
        ("plain", || {
            Options::new()
                .durability(Durability::Buffered)
                .cache_bytes(CACHE)
                .copy_threads(1)
                .reclaim(MANUAL)
                .hook(hook())
        }),
        ("shared", || {
            Options::new()
                .durability(Durability::Buffered)
                .cache_bytes(CACHE)
                .copy_threads(1)
                .reclaim(MANUAL)
                .shared(true)
                .hook(hook())
        }),
    ]
}

fn blob(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn put(db: &Db<Probed>, key: u64) -> ferro::Result<()> {
    db.update(|root| {
        root.rows.insert(key, key);
    })
}

fn grow(db: &Db<Probed>, seed: u64, len: usize) -> ferro::Result<()> {
    db.update(|root| {
        root.rows.insert(seed + 10_000, seed);
        root.blob = blob(seed, len);
    })
}

fn merge(ours: &mut Probed, _: &Snapshot<Probed>, _: &Snapshot<Probed>) -> (&'static str, ()) {
    ours.rows.insert(999, 999);
    ("merge", ())
}

fn fill(db: &Db<Probed>) {
    for version in 1..=SMALL {
        drop(db.at(version).unwrap());
    }
    db.branches();
}

struct Files {
    writer: Db<Probed>,
    reader: Db<Probed>,
    peer: Db<Probed>,
    dir: TempDir,
}

fn files(options: fn() -> Options) -> Files {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("locks.ferro");
    {
        let main: Db<Probed> = options().open(&path).unwrap();
        for seed in 0..SMALL {
            grow(&main, 1000 + seed, PAD).unwrap();
        }
        for seed in 0..BIG {
            grow(&main, seed, BLOB).unwrap();
        }
        let big = main.fork("big").unwrap();
        grow(&big, 99, BLOB).unwrap();
    }
    let writer: Db<Probed> = options().open(&path).unwrap();
    grow(&writer, 50, BLOB).unwrap();
    let reader: Db<Probed> = Options::new()
        .cache_bytes(CACHE)
        .read_only(true)
        .open(&path)
        .unwrap();
    let peer: Db<Probed> = Options::new()
        .durability(Durability::Buffered)
        .open(dir.path().join("peer.ferro"))
        .unwrap();
    for key in [7, 8] {
        put(&peer, key).unwrap();
    }
    Files {
        writer,
        reader,
        peer,
        dir,
    }
}

fn spilled(files: &Files, name: &str, into: &Db<Probed>) -> Bundle {
    let bigpeer: Db<Probed> = Options::new()
        .durability(Durability::Buffered)
        .open(files.dir.path().join(format!("{name}.ferro")))
        .unwrap();
    for seed in 0..4 {
        grow(&bigpeer, 500 + seed, SPILLED).unwrap();
    }
    bigpeer.bundle(&into.have().unwrap()).unwrap()
}

fn inner(
    on: &Db<Probed>,
    peer: &Db<Probed>,
    bundles: &[Bundle; 2],
    inner: Inner,
) -> ferro::Result<()> {
    match inner {
        Inner::Branches => {
            on.branches();
            Ok(())
        }
        Inner::Verify => on.verify().map(drop),
        Inner::Have => on.have().map(drop),
        Inner::Read => {
            on.read().rows.get(&0);
            for version in [1, 2, 3, 50, 100, 150, 199, 200] {
                drop(on.at(version)?);
            }
            Ok(())
        }
        Inner::Pull => on.pull(peer, "peer", merge).map(drop),
        Inner::Push => on.push(peer, "up", merge).map(drop),
        Inner::Receive => on.receive("peer", &bundles[0], merge).map(drop),
        Inner::Spilled => on.receive("spill", &bundles[1], merge).map(drop),
        Inner::Compact => on.compact(Keep::All).map(drop),
    }
}

fn expected(host: Host, outer: Outer, inner: Inner) -> Expect {
    let read = matches!(
        inner,
        Inner::Branches | Inner::Verify | Inner::Have | Inner::Read
    );
    match (host, outer) {
        (_, Outer::Read) | (Host::Drop, Outer::Compact) => Expect::Done,
        (_, Outer::Receive) if matches!(inner, Inner::Compact | Inner::Spilled) => {
            Expect::Reentrant
        }
        (_, Outer::Receive) => Expect::Done,
        _ if read => Expect::Done,
        _ => Expect::Reentrant,
    }
}

fn where_fired(outer: Outer) -> Option<&'static str> {
    match outer {
        Outer::Compact => Some("swapped"),
        Outer::Receive => Some("placed"),
        _ => None,
    }
}

fn got(result: &ferro::Result<()>) -> Option<Expect> {
    match result {
        Ok(()) => Some(Expect::Done),
        Err(Error::Reentrant) => Some(Expect::Reentrant),
        Err(_) => None,
    }
}

struct Outcome {
    fired: bool,
    step: &'static str,
    inner: Option<ferro::Result<()>>,
    outer: ferro::Result<()>,
    after: ferro::Result<()>,
}

fn case(options: fn() -> Options, host: Host, run: Outer, call: Inner) -> Outcome {
    let files = files(options);
    let on = match run {
        Outer::ReaderRefresh => files.reader.clone(),
        _ => files.writer.clone(),
    };
    let received = (run == Outer::Receive).then(|| spilled(&files, "bigpeer", &files.writer));
    if run == Outer::ReaderRefresh {
        grow(&files.writer, 60, BLOB).unwrap();
    }
    let bundles = [
        files.peer.bundle(&on.have().unwrap()).unwrap(),
        match call {
            Inner::Spilled => spilled(&files, "spill", &on),
            _ => Bundle::from(Vec::new()),
        },
    ];
    let kept = (files.writer.read(), files.reader.read());
    let seen: Seen = Rc::default();
    let slot = match host {
        Host::Decode => &ON_DECODE,
        Host::Drop => &ON_DROP,
    };
    STEP.with(|at| at.set(""));
    let into = seen.clone();
    let peer = files.peer.clone();
    let filled = on.clone();
    let hosted = move || {
        let step = STEP.with(Cell::get);
        *into.borrow_mut() = Some((step, inner(&on, &peer, &bundles, call)));
    };
    match run {
        Outer::Receive => {
            let writer = files.writer.clone();
            arm(&ON_PLACED, move || {
                fill(&writer);
                arm(slot, hosted);
                drop(writer.at(SMALL + 2).unwrap());
            });
        }
        _ => {
            fill(&filled);
            arm(slot, hosted);
        }
    }
    let writer = &files.writer;
    let outer = match run {
        Outer::Read => writer.at(SMALL + 1).map(drop),
        Outer::Attach => writer.branch("big").map(drop),
        Outer::Fork => writer.fork("side").map(drop),
        Outer::ReaderRefresh => files.reader.refresh().map(drop),
        Outer::Compact => writer.compact(Keep::All).map(drop),
        Outer::Receive => {
            let bundle = received.expect("a receive has a spilled bundle");
            writer.receive("bigpeer", &bundle, merge).map(drop)
        }
    };
    let fired = fired(slot) && (run != Outer::Receive || fired(&ON_PLACED));
    drop(kept);
    let after = (|| {
        put(writer, 400)?;
        writer.verify()?;
        files.reader.refresh()?;
        files.reader.verify()?;
        Ok(())
    })();
    let (step, inner) = match seen.borrow_mut().take() {
        Some((step, result)) => (step, Some(result)),
        None => ("", None),
    };
    Outcome {
        fired,
        step,
        inner,
        outer,
        after,
    }
}

fn matrix(host: Host, outers: &[Outer]) {
    let mut cases: Vec<(String, Outer, Expect, JoinHandle<Outcome>)> = Vec::new();
    for (kind, options) in kinds() {
        for &run in outers {
            for call in INNERS {
                let what = format!("{kind}: {call:?} from {host:?} inside {run:?}");
                let handle = thread::spawn(move || case(options, host, run, call));
                cases.push((what, run, expected(host, run, call), handle));
            }
        }
    }
    let deadline = Instant::now() + PATIENCE;
    let total = cases.len();
    let mut failed = Vec::new();
    for (what, run, want, handle) in cases {
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if !handle.is_finished() {
            failed.push(format!("{what}: hung"));
            continue;
        }
        let Ok(outcome) = handle.join() else {
            failed.push(format!("{what}: panicked"));
            continue;
        };
        if !outcome.fired {
            failed.push(format!("{what}: the user code never ran"));
            continue;
        }
        if host == Host::Drop
            && let Some(step) = where_fired(run)
            && outcome.step != step
        {
            failed.push(format!(
                "{what}: the drop ran after step {:?}, not {step:?}",
                outcome.step
            ));
        }
        match &outcome.inner {
            Some(result) if got(result).as_ref() == Some(&want) => {}
            Some(result) => failed.push(format!("{what}: {result:?}, wanted {want:?}")),
            None => failed.push(format!("{what}: the inner call never returned")),
        }
        if let Err(error) = &outcome.outer {
            failed.push(format!("{what}: the outer call failed with {error:?}"));
        }
        if let Err(error) = &outcome.after {
            failed.push(format!("{what}: the file failed afterwards with {error:?}"));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {total} cases failed:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

#[test]
fn a_decode_run_under_the_published_refs_cannot_deadlock_a_read() {
    matrix(
        Host::Decode,
        &[
            Outer::Read,
            Outer::Attach,
            Outer::Fork,
            Outer::ReaderRefresh,
            Outer::Compact,
        ],
    );
}

#[test]
fn a_value_the_cache_drops_under_a_lock_cannot_deadlock_a_call() {
    matrix(
        Host::Drop,
        &[
            Outer::Read,
            Outer::Attach,
            Outer::Fork,
            Outer::ReaderRefresh,
            Outer::Compact,
            Outer::Receive,
        ],
    );
}
