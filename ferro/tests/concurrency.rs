mod common;

use std::collections::BTreeSet;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use common::{Rng, fast, quiet_ferro_panics, uncached};
use ferro::codec::{Decoder, Encoder};
use ferro::{Data, Db, Error, Map, Options, Snapshot};

const TOTAL: u64 = 1_000_000;
const ACCOUNTS: u64 = 64;

#[derive(Clone, Default)]
struct Ledger {
    a: Map<u64, u64>,
    b: Map<u64, u64>,
    transfers: u64,
}

impl Data for Ledger {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.a.encode(w)?;
        self.b.encode(w)?;
        self.transfers.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Ledger {
            a: Map::decode(r)?,
            b: Map::decode(r)?,
            transfers: u64::decode(r)?,
        })
    }
}

fn seeded(db: &Db<Ledger>) {
    db.update(|root| {
        for key in 0..ACCOUNTS {
            root.a.insert(key, TOTAL / ACCOUNTS);
            root.b.insert(key, 0);
        }
    })
    .unwrap();
}

fn transfer(root: &mut Ledger, rng: &mut Rng) {
    let from = rng.below(ACCOUNTS);
    let to = rng.below(ACCOUNTS);
    let (source, sink) = if rng.chance(50) {
        (&mut root.a, &mut root.b)
    } else {
        (&mut root.b, &mut root.a)
    };
    let available = source.get(&from).map_or(0, |v| *v);
    let amount = available.min(1 + rng.below(50));
    source.insert(from, available - amount);
    let held = sink.get(&to).map_or(0, |v| *v);
    sink.insert(to, held + amount);
    root.transfers += 1;
}

fn assert_consistent(snapshot: &Snapshot<Ledger>) {
    let left: u64 = snapshot.a.values().map(|v| *v).sum();
    let right: u64 = snapshot.b.values().map(|v| *v).sum();
    assert_eq!(left + right, (TOTAL / ACCOUNTS) * ACCOUNTS);
    assert_eq!(snapshot.a.len() as u64, ACCOUNTS);
    assert_eq!(snapshot.b.len() as u64, ACCOUNTS);
    assert_eq!(snapshot.transfers + 1, snapshot.version());
}

#[test]
fn readers_see_atomic_commits_during_heavy_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Ledger>(dir.path().join("l.ferro")).unwrap();
    seeded(&db);
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let mut readers = Vec::new();
    for reader in 0..8 {
        let db = db.clone();
        let stop = stop.clone();
        let reads = reads.clone();
        readers.push(thread::spawn(move || {
            let mut held: Vec<(Snapshot<Ledger>, u64, u64)> = Vec::new();
            let mut last = 0;
            while !stop.load(Ordering::Relaxed) {
                let snapshot = db.read();
                assert!(snapshot.version() >= last);
                last = snapshot.version();
                assert_consistent(&snapshot);
                reads.fetch_add(1, Ordering::Relaxed);
                if held.len() < 32 {
                    let sum: u64 = snapshot.a.values().map(|v| *v).sum();
                    let transfers = snapshot.transfers;
                    held.push((snapshot, sum, transfers));
                }
                if reader % 2 == 0 && last > 2 {
                    let past = db.at(last - 1).unwrap();
                    assert_consistent(&past);
                }
            }
            for (snapshot, sum, transfers) in held {
                assert_eq!(snapshot.a.values().map(|v| *v).sum::<u64>(), sum);
                assert_eq!(snapshot.transfers, transfers);
                assert_consistent(&snapshot);
            }
        }));
    }
    let mut rng = Rng::new(1);
    let mut committed = 0;
    while committed < 6000 {
        db.update(|root| transfer(root, &mut rng)).unwrap();
        committed = db.read().transfers;
    }
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }
    assert!(reads.load(Ordering::Relaxed) > 0);
    db.verify().unwrap();
}

#[test]
fn panic_inside_update_discards_the_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast()
        .open::<Map<u64, String>>(dir.path().join("p.ferro"))
        .unwrap();
    db.update(|root| {
        root.insert(1, "keep".into());
    })
    .unwrap();
    let size = db.size_on_disk();
    for round in 0..50 {
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            db.update(|root| {
                root.insert(1, "lost".repeat(400));
                root.insert(2, "lost".into());
                root.update(&1, |value| value.push('!'));
                if round >= 0 {
                    panic::panic_any(round);
                }
            })
            .unwrap();
        }));
        assert_eq!(result.unwrap_err().downcast_ref::<i32>(), Some(&round));
        assert_eq!(db.version(), 1);
        assert_eq!(*db.read().get(&1).unwrap(), "keep");
        assert!(db.read().get(&2).is_none());
        assert_eq!(db.size_on_disk(), size);
    }
    db.update(|root| {
        root.insert(3, "next".into());
    })
    .unwrap();
    assert_eq!(db.version(), 2);
    assert_eq!(db.read().len(), 2);
}

#[derive(Clone, Default)]
struct Touchy {
    panic_on_encode: bool,
    fail_on_encode: bool,
    fail_on_decode: bool,
    payload: u64,
}

impl Data for Touchy {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        assert!(!self.panic_on_encode, "ferro: test panic in encode");
        if self.fail_on_encode {
            return Err(Error::Decode("refused to encode".into()));
        }
        self.fail_on_decode.encode(w)?;
        self.payload.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        let fail_on_decode = bool::decode(r)?;
        if fail_on_decode {
            return Err(Error::Decode("refused to decode".into()));
        }
        Ok(Touchy {
            panic_on_encode: false,
            fail_on_encode: false,
            fail_on_decode,
            payload: u64::decode(r)?,
        })
    }
}

fn touchy(payload: u64) -> Touchy {
    Touchy {
        payload,
        ..Touchy::default()
    }
}

#[test]
fn panic_or_error_from_encode_leaves_the_database_usable() {
    quiet_ferro_panics();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.ferro");
    let db = fast().open::<Map<u64, Touchy>>(&path).unwrap();
    db.update(|root| {
        root.insert(1, touchy(1));
    })
    .unwrap();
    let size = db.size_on_disk();
    let panicked = panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = db.update(|root| {
            let mut bad = touchy(2);
            bad.panic_on_encode = true;
            root.insert(2, bad);
        });
    }));
    assert!(panicked.is_err());
    assert_eq!(db.version(), 1);
    let refused = db.update(|root| {
        let mut bad = touchy(3);
        bad.fail_on_encode = true;
        root.insert(3, bad);
    });
    assert!(matches!(refused, Err(Error::Decode(_))));
    assert_eq!(db.version(), 1);
    assert_eq!(db.size_on_disk(), size);
    db.update(|root| {
        root.insert(4, touchy(4));
    })
    .unwrap();
    assert_eq!(db.version(), 2);
    assert_eq!(db.read().len(), 2);
    db.verify().unwrap();
}

#[test]
fn value_that_cannot_decode_is_refused_before_it_becomes_durable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.ferro");
    let db = fast().open::<Map<u64, Touchy>>(&path).unwrap();
    db.update(|root| {
        root.insert(1, touchy(1));
    })
    .unwrap();
    let size = db.size_on_disk();
    let refused = db.update(|root| {
        let mut bad = touchy(2);
        bad.fail_on_decode = true;
        root.insert(2, bad);
    });
    assert!(matches!(refused, Err(Error::Decode(_))));
    assert_eq!(db.version(), 1);
    assert_eq!(db.size_on_disk(), size);
    db.update(|root| {
        root.insert(3, touchy(3));
    })
    .unwrap();
    assert_eq!(db.version(), 2);
    drop(db);
    let db = fast().open::<Map<u64, Touchy>>(&path).unwrap();
    assert_eq!(db.version(), 2);
    assert_eq!(db.read().len(), 2);
}

#[derive(Debug)]
enum AppError {
    Rejected,
    Db,
}

impl From<Error> for AppError {
    fn from(_: Error) -> Self {
        AppError::Db
    }
}

#[test]
fn err_from_try_update_discards_the_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.ferro");
    let db = fast().open::<Map<u64, String>>(&path).unwrap();
    db.update(|root| {
        root.insert(1, "keep".into());
    })
    .unwrap();
    for _ in 0..20 {
        let result: Result<(), AppError> = db.try_update(|root| {
            root.clear();
            root.insert(9, "x".repeat(4000));
            Err(AppError::Rejected)
        });
        assert!(matches!(result, Err(AppError::Rejected)));
        assert_eq!(db.version(), 1);
        assert_eq!(db.read().len(), 1);
    }
    let good: Result<u32, AppError> = db.try_commit("fine", |root| {
        root.insert(2, "two".into());
        Ok(7)
    });
    assert_eq!(good.unwrap(), 7);
    assert_eq!(db.read().commit().message, "fine");
    drop(db);
    let db = fast().open::<Map<u64, String>>(&path).unwrap();
    assert_eq!(db.version(), 2);
    assert_eq!(db.read().len(), 2);
}

#[test]
fn contending_writers_lose_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.ferro");
    let db = fast().open::<Map<u64, u64>>(&path).unwrap();
    let branch = db.fork("side").unwrap();
    let threads = 8;
    let rounds = 300;
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();
    for thread in 0..threads {
        let target = if thread % 4 == 3 {
            branch.clone()
        } else {
            db.clone()
        };
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for round in 0..rounds {
                target
                    .commit(format!("{thread}:{round}"), |root| {
                        let count = root.get(&0).map_or(0, |v| *v);
                        root.insert(0, count + 1);
                        root.insert(1 + thread as u64 * 1000 + round, count);
                    })
                    .unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let main_commits = rounds * 6;
    let side_commits = rounds * 2;
    assert_eq!(db.version(), main_commits);
    assert_eq!(branch.version(), side_commits);
    assert_eq!(*db.read().get(&0).unwrap(), main_commits);
    assert_eq!(*branch.read().get(&0).unwrap(), side_commits);
    let messages: BTreeSet<String> = db.log().values().map(|c| c.message.clone()).collect();
    assert_eq!(messages.len() as u64, main_commits);
    db.verify().unwrap();
    drop((db, branch));
    let db = fast().open::<Map<u64, u64>>(&path).unwrap();
    assert_eq!(db.version(), main_commits);
}

#[test]
fn restore_reports_its_own_version_under_contention() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast()
        .open::<Map<u64, u64>>(dir.path().join("r.ferro"))
        .unwrap();
    let threads = 8;
    for n in 0..threads {
        db.update(|root| {
            root.insert(0, n as u64);
        })
        .unwrap();
    }
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();
    for n in 0..threads {
        let db = db.clone();
        let barrier = barrier.clone();
        let origin = db.at(n as u64 + 1).unwrap();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..300 {
                let version = db.restore(&origin).unwrap().version;
                let restored = db.at(version).unwrap();
                assert_eq!(
                    restored.root_hash(),
                    origin.root_hash(),
                    "restore returned v{version}, which holds another thread's state"
                );
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
}

#[test]
fn watchers_see_newer_commits_in_order_and_end_with_the_last_handle() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast()
        .open::<Map<u64, u64>>(dir.path().join("w.ferro"))
        .unwrap();
    let early = db.watch();
    let dropped = db.watch();
    drop(dropped);
    let fork = db.fork("f").unwrap();
    let side = fork.watch();
    let writer = {
        let db = db.clone();
        thread::spawn(move || {
            for n in 1..=500u64 {
                db.update(|root| {
                    root.insert(n, n);
                })
                .unwrap();
                db.update(|_| {}).unwrap();
            }
        })
    };
    let mut last = 0;
    while last < 500 {
        let snapshot = early.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(snapshot.version() > last);
        assert_eq!(snapshot.len() as u64, snapshot.version());
        last = snapshot.version();
    }
    writer.join().unwrap();
    assert!(early.try_recv().is_err());
    assert!(side.try_recv().is_err());
    fork.update(|root| {
        root.insert(1, 1);
    })
    .unwrap();
    assert_eq!(
        side.recv_timeout(Duration::from_secs(10))
            .unwrap()
            .version(),
        1
    );
    drop(db);
    drop(fork);
    assert!(matches!(
        early.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn read_only_open_never_races_a_writer_into_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("o.ferro");
    let db = fast().open::<Ledger>(&path).unwrap();
    seeded(&db);
    let stop = Arc::new(AtomicBool::new(false));
    let mut openers = Vec::new();
    for _ in 0..4 {
        let path = path.clone();
        let stop = stop.clone();
        openers.push(thread::spawn(move || {
            let mut opens = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let reader = fast().read_only().open::<Ledger>(&path).unwrap();
                assert_consistent(&reader.read());
                opens += 1;
            }
            opens
        }));
    }
    let mut rng = Rng::new(3);
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        db.update(|root| transfer(root, &mut rng)).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let opens: u64 = openers.into_iter().map(|t| t.join().unwrap()).sum();
    assert!(opens > 10);
}

#[test]
fn read_only_open_never_races_a_durable_writer_closing() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("c.ferro");
    let open = |path: &std::path::Path| Options::new().open::<Map<u64, u64>>(path);
    open(&path)
        .unwrap()
        .update(|root| {
            root.insert(0, 0);
        })
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let (path, stop) = (path.clone(), stop.clone());
            thread::spawn(move || {
                let mut last = 0;
                while !stop.load(Ordering::Relaxed) {
                    let reader = fast().read_only().open::<Map<u64, u64>>(&path).unwrap();
                    assert!(reader.version() >= last);
                    last = reader.version();
                }
            })
        })
        .collect();
    for n in 1..=200u64 {
        let db = open(&path).unwrap();
        db.update(|root| {
            root.insert(0, n);
        })
        .unwrap();
        db.update(|root| {
            root.insert(1, n);
        })
        .unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }
    assert_eq!(open(&path).unwrap().version(), 401);
}

#[test]
fn refresh_tracks_a_writer_in_the_same_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.ferro");
    let db = fast().open::<Ledger>(&path).unwrap();
    seeded(&db);
    let target = 3000;
    let opened = Arc::new(Barrier::new(5));
    let mut readers = Vec::new();
    for cache in 0..4 {
        let (path, opened) = (path.clone(), opened.clone());
        readers.push(thread::spawn(move || {
            let options = if cache % 2 == 0 { fast() } else { uncached() };
            let reader = options.read_only().open::<Ledger>(&path).unwrap();
            opened.wait();
            let mut last = reader.version();
            let mut changes = 0;
            while last < target {
                let changed = reader.refresh().unwrap();
                let snapshot = reader.read();
                assert!(snapshot.version() >= last);
                assert_eq!(changed, snapshot.version() != last);
                changes += u64::from(changed);
                last = snapshot.version();
                assert_consistent(&snapshot);
                assert_consistent(&reader.at(last).unwrap());
            }
            reader.verify().unwrap();
            changes
        }));
    }
    opened.wait();
    let mut rng = Rng::new(5);
    for _ in 0..target {
        db.update(|root| transfer(root, &mut rng)).unwrap();
    }
    for reader in readers {
        assert!(reader.join().unwrap() > 0);
    }
}

#[test]
fn refresh_on_a_new_branch_and_read_only_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    let db = fast().open::<Map<u64, u64>>(&path).unwrap();
    let reader = fast().read_only().open::<Map<u64, u64>>(&path).unwrap();
    assert!(!reader.refresh().unwrap());
    assert_eq!(reader.version(), 0);
    let fork = db.fork("x").unwrap();
    fork.update(|root| {
        root.insert(1, 1);
    })
    .unwrap();
    assert!(matches!(reader.branch("x"), Err(Error::NoBranch(_))));
    assert!(reader.refresh().unwrap());
    let side = reader.branch("x").unwrap();
    assert_eq!(side.version(), 1);
    assert_eq!(reader.version(), 0);
    db.update(|root| {
        root.insert(2, 2);
    })
    .unwrap();
    fork.update(|root| {
        root.insert(3, 3);
    })
    .unwrap();
    assert!(reader.refresh().unwrap());
    assert!(!reader.refresh().unwrap());
    assert_eq!(side.version(), 2);
    assert_eq!(reader.version(), 1);
    assert!(matches!(
        reader.update(|root| {
            root.insert(9, 9);
        }),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(reader.fork("y"), Err(Error::ReadOnly)));
    assert_eq!(reader.version(), 1);
    assert!(!db.refresh().unwrap());
}

#[test]
fn refresh_never_moves_a_reader_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.ferro");
    let db = fast().open::<Map<u64, u64>>(&path).unwrap();
    for n in 0..4u64 {
        db.update(|root| {
            root.insert(n, n);
        })
        .unwrap();
    }
    drop(db);
    let copy = dir.path().join("copy.ferro");
    fs::copy(&path, &copy).unwrap();
    let reader = fast().read_only().open::<Map<u64, u64>>(&copy).unwrap();
    assert_eq!(reader.version(), 4);
    let mut image = fs::read(&copy).unwrap();
    image[20] ^= 0xFF;
    fs::write(&copy, &image).unwrap();
    assert!(!reader.refresh().unwrap());
    assert_eq!(reader.version(), 4);
}

#[test]
fn watchers_on_a_read_only_handle_fire_on_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.ferro");
    let db = fast().open::<Map<u64, u64>>(&path).unwrap();
    let reader = fast().read_only().open::<Map<u64, u64>>(&path).unwrap();
    let events = reader.watch();
    db.update(|root| {
        root.insert(1, 1);
    })
    .unwrap();
    db.update(|root| {
        root.insert(2, 2);
    })
    .unwrap();
    assert!(events.try_recv().is_err());
    assert!(reader.refresh().unwrap());
    let snapshot = events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(snapshot.version(), 2);
    assert!(events.try_recv().is_err());
    db.fork("other").unwrap();
    assert!(reader.refresh().unwrap());
    assert!(events.try_recv().is_err());
}

#[test]
fn second_writer_is_locked_out_and_reader_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.ferro");
    let db = fast().open::<Map<u64, u64>>(&path).unwrap();
    assert!(matches!(
        fast().open::<Map<u64, u64>>(&path),
        Err(Error::Locked)
    ));
    assert!(fast().read_only().open::<Map<u64, u64>>(&path).is_ok());
    let clone = db.clone();
    drop(db);
    assert!(matches!(
        fast().open::<Map<u64, u64>>(&path),
        Err(Error::Locked)
    ));
    drop(clone);
    fast().open::<Map<u64, u64>>(&path).unwrap();
}

#[test]
fn memoized_reads_stay_correct_while_a_tiny_cache_evicts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiny.ferro");
    let db = fast()
        .cache_bytes(2048)
        .open::<Map<u64, String>>(&path)
        .unwrap();
    db.update(|root| {
        for key in 0..4000 {
            root.insert(key, format!("value-{key}"));
        }
    })
    .unwrap();
    let snapshot = db.read();
    thread::scope(|scope| {
        for shift in 0..4u64 {
            let snapshot = &snapshot;
            scope.spawn(move || {
                for round in 0..3 {
                    for step in 0..4000u64 {
                        let key = (step * 7 + shift * 13 + round) % 4000;
                        assert_eq!(*snapshot.get(&key).unwrap(), format!("value-{key}"));
                    }
                    let keys: Vec<u64> = snapshot.keys().collect();
                    assert_eq!(keys, (0..4000).collect::<Vec<u64>>());
                }
            });
        }
    });
}

#[test]
fn pinned_nested_maps_do_not_hold_the_writer_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested.ferro");
    {
        let db = fast().open::<Map<u64, Map<u64, u64>>>(&path).unwrap();
        db.update(|root| {
            for outer in 0..500 {
                let mut inner = Map::new();
                for key in 0..20 {
                    inner.insert(key, outer + key);
                }
                root.insert(outer, inner);
            }
        })
        .unwrap();
        let snapshot = db.read();
        for outer in 0..500 {
            assert_eq!(*snapshot.get(&outer).unwrap().get(&3).unwrap(), outer + 3);
        }
        let held = snapshot.get(&7).unwrap();
        drop(snapshot);
        assert_eq!(*held.get(&1).unwrap(), 8);
    }
    let db = fast().open::<Map<u64, Map<u64, u64>>>(&path).unwrap();
    assert_eq!(db.read().len(), 500);
}
