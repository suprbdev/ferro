#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle, LocalKey};
use std::time::{Duration, Instant};

use ferro::codec::{Decoder, Encoder};
use ferro::{Data, Db, Durability, Error, Keep, Map, Options, Reclaim};
use tempfile::TempDir;

const MANUAL: Reclaim = Reclaim { every_bytes: 0 };
const PATIENCE: Duration = Duration::from_secs(60);

type Slot = RefCell<Option<Box<dyn FnOnce()>>>;

thread_local! {
    static ON_DECODE: Slot = const { RefCell::new(None) };
    static ON_DROP: Slot = const { RefCell::new(None) };
    static ARMED_AT: Cell<u64> = const { Cell::new(0) };
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
    probe: Probe,
}

#[derive(Clone, Copy, PartialEq)]
enum Host {
    Decode,
    Drop,
}

#[derive(Clone, Copy)]
enum Trigger {
    Commit,
    Compact,
    DropBranch,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Outer {
    Update,
    Fork,
    DropBranch,
    Compact,
    Branch,
    Refresh,
    ReaderRefresh,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Inner {
    Refresh,
    Branch,
    Later,
    Earlier,
    Fork,
    DropBranch,
    Read,
}

#[derive(Debug, PartialEq)]
enum Expect {
    Done,
    Reentrant,
    ReadOnly,
}

const OUTERS: [Outer; 7] = [
    Outer::Update,
    Outer::Fork,
    Outer::DropBranch,
    Outer::Compact,
    Outer::Branch,
    Outer::Refresh,
    Outer::ReaderRefresh,
];

const INNERS: [Inner; 7] = [
    Inner::Refresh,
    Inner::Branch,
    Inner::Later,
    Inner::Earlier,
    Inner::Fork,
    Inner::DropBranch,
    Inner::Read,
];

type Kind = (&'static str, fn() -> Options);

fn kinds() -> Vec<Kind> {
    vec![
        ("shared", || {
            Options::new()
                .durability(Durability::Buffered)
                .cache_bytes(1 << 20)
                .copy_threads(1)
                .reclaim(MANUAL)
                .shared(true)
        }),
        ("shared coalesced", || {
            Options::new()
                .durability(Durability::Buffered)
                .cache_bytes(1 << 20)
                .copy_threads(1)
                .reclaim(MANUAL)
                .shared(true)
                .coalesce(true)
        }),
    ]
}

#[derive(Clone)]
struct Handles {
    main: Db<Probed>,
    zz: Db<Probed>,
    aa: Db<Probed>,
}

struct Files {
    writer: Handles,
    reader: Handles,
    other: Db<Probed>,
    _dir: TempDir,
}

fn put(db: &Db<Probed>, key: u64) -> ferro::Result<()> {
    db.update(|root| {
        root.rows.insert(key, key);
    })
}

fn files(options: fn() -> Options) -> Files {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("shared.ferro");
    let main: Db<Probed> = options().open(&path).unwrap();
    for key in 0..3 {
        put(&main, key).unwrap();
    }
    let zz = main.fork("zz").unwrap();
    let aa = main.fork("aa").unwrap();
    for name in ["gone", "spare", "victim"] {
        put(&main.fork(name).unwrap(), 50).unwrap();
    }
    let reader: Db<Probed> = Options::new()
        .cache_bytes(1 << 20)
        .read_only(true)
        .open(&path)
        .unwrap();
    let reader = Handles {
        zz: reader.branch("zz").unwrap(),
        aa: reader.branch("aa").unwrap(),
        main: reader,
    };
    let other: Db<Probed> = options().open(&path).unwrap();
    Files {
        writer: Handles { main, zz, aa },
        reader,
        other,
        _dir: dir,
    }
}

fn trigger(files: &Files, trigger: Trigger) {
    match trigger {
        Trigger::Commit => put(&files.other, 100).unwrap(),
        Trigger::Compact => {
            put(&files.other, 100).unwrap();
            files.other.compact(Keep::All).unwrap();
        }
        Trigger::DropBranch => files.other.drop_branch("gone").unwrap(),
    }
}

fn outer(files: &Files, outer: Outer) -> ferro::Result<()> {
    let main = &files.writer.main;
    match outer {
        Outer::Update => put(main, 200),
        Outer::Fork => main.fork("side").map(drop),
        Outer::DropBranch => main.drop_branch("spare"),
        Outer::Compact => main.compact(Keep::All).map(drop),
        Outer::Branch => main.branch("aa").map(drop),
        Outer::Refresh => main.refresh().map(drop),
        Outer::ReaderRefresh => files.reader.main.refresh().map(drop),
    }
}

fn inner(on: &Handles, inner: Inner) -> ferro::Result<()> {
    match inner {
        Inner::Refresh => on.main.refresh().map(drop),
        Inner::Branch => on.main.branch("main").map(drop),
        Inner::Later => put(&on.zz, 300),
        Inner::Earlier => put(&on.aa, 300),
        Inner::Fork => on.main.fork("inner").map(drop),
        Inner::DropBranch => on.main.drop_branch("victim"),
        Inner::Read => {
            on.main.read().rows.get(&0);
            Ok(())
        }
    }
}

fn expected(host: Host, outer: Outer, inner: Inner) -> Expect {
    match (host, inner) {
        (_, Inner::Read) => Expect::Done,
        (Host::Decode, _) => Expect::Reentrant,
        (Host::Drop, Inner::Refresh | Inner::Branch) => Expect::Done,
        (Host::Drop, _) if outer == Outer::ReaderRefresh => Expect::ReadOnly,
        (Host::Drop, _) => Expect::Done,
    }
}

fn got(result: &ferro::Result<()>) -> Option<Expect> {
    match result {
        Ok(()) => Some(Expect::Done),
        Err(Error::Reentrant) => Some(Expect::Reentrant),
        Err(Error::ReadOnly) => Some(Expect::ReadOnly),
        Err(_) => None,
    }
}

struct Outcome {
    fired: bool,
    inner: Option<ferro::Result<()>>,
    outer: ferro::Result<()>,
    after: ferro::Result<()>,
}

fn case(options: fn() -> Options, host: Host, cause: Trigger, run: Outer, call: Inner) -> Outcome {
    let files = files(options);
    trigger(&files, cause);
    let seen: Rc<RefCell<Option<ferro::Result<()>>>> = Rc::default();
    let on = match run {
        Outer::ReaderRefresh => files.reader.clone(),
        _ => files.writer.clone(),
    };
    let slot = match host {
        Host::Decode => &ON_DECODE,
        Host::Drop => &ON_DROP,
    };
    let into = seen.clone();
    arm(slot, move || {
        *into.borrow_mut() = Some(inner(&on, call));
    });
    let outer = outer(&files, run);
    let fired = fired(slot);
    let after = (|| {
        put(&files.writer.main, 400)?;
        put(&files.other, 500)?;
        files.writer.main.verify()?;
        files.other.verify()?;
        files.reader.main.refresh()?;
        Ok(())
    })();
    let inner = seen.borrow_mut().take();
    Outcome {
        fired,
        inner,
        outer,
        after,
    }
}

fn matrix(host: Host, cause: Trigger, outers: &[Outer]) {
    let mut cases: Vec<(String, Expect, JoinHandle<Outcome>)> = Vec::new();
    for (kind, options) in kinds() {
        for &run in outers {
            for call in INNERS {
                let what = format!("{kind}: {call:?} inside {run:?}");
                let handle = thread::spawn(move || case(options, host, cause, run, call));
                cases.push((what, expected(host, run, call), handle));
            }
        }
    }
    let deadline = Instant::now() + PATIENCE;
    let total = cases.len();
    let mut failed = Vec::new();
    for (what, want, handle) in cases {
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
fn a_decode_run_while_catching_up_with_a_commit_cannot_deadlock() {
    matrix(Host::Decode, Trigger::Commit, &OUTERS);
}

#[test]
fn a_decode_run_while_following_a_cycle_cannot_deadlock() {
    matrix(Host::Decode, Trigger::Compact, &OUTERS);
}

#[test]
fn a_value_a_followed_cycle_replaces_is_dropped_after_the_locks() {
    matrix(Host::Drop, Trigger::Compact, &OUTERS);
}

#[test]
fn a_branch_another_process_dropped_is_dropped_after_the_locks() {
    let writers = OUTERS
        .into_iter()
        .filter(|outer| *outer != Outer::ReaderRefresh);
    let writers: Vec<Outer> = writers.collect();
    matrix(Host::Drop, Trigger::DropBranch, &writers);
}
