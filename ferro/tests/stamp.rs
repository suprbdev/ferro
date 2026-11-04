mod common;

use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use common::fast;
use ferro::db::Clock;
use ferro::{Committed, Data, Db, Error, Keep, Map, Options, Stamp};
use tempfile::TempDir;

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    marks: Map<u64, (u64, u64)>,
    plain: Map<u64, u64>,
}

struct Wall(Arc<AtomicU64>);

impl Wall {
    fn at(ms: u64) -> Wall {
        Wall(Arc::new(AtomicU64::new(ms)))
    }

    fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }

    fn clock(&self) -> Clock {
        let at = self.0.clone();
        Clock(Arc::new(move || at.load(Ordering::SeqCst)))
    }
}

fn open_at(path: &Path, wall: &Wall) -> Db<Root> {
    fast().clock(wall.clock()).open(path).unwrap()
}

fn mark(db: &Db<Root>, id: u64) -> Committed<(u64, u64)> {
    db.commit_stamped(|root, stamp| {
        let seen = (stamp.version(), stamp.time_ms());
        root.marks.insert(id, seen);
        (format!("mark {id}"), seen)
    })
    .unwrap()
}

fn plain(db: &Db<Root>, id: u64) -> Committed<()> {
    db.commit_with(|root| {
        root.plain.insert(id, id);
        (format!("plain {id}"), ())
    })
    .unwrap()
}

fn time_of(db: &Db<Root>, version: u64) -> u64 {
    db.log().get(&version).unwrap().time_ms
}

fn assert_lands(db: &Db<Root>, id: u64, (version, time_ms): (u64, u64)) {
    let found = db.as_of(time_ms).unwrap();
    assert_eq!(found.version(), version, "as_of({time_ms}) for mark {id}");
    assert_eq!(found.commit().time_ms, time_ms);
    assert_eq!(
        found.marks.get(&id).map(|seen| *seen),
        Some((version, time_ms))
    );
    assert!(db.at(version - 1).unwrap().marks.get(&id).is_none());
}

fn assert_times_never_go_back(db: &Db<Root>) {
    let times: Vec<u64> = db.log().values().map(|commit| commit.time_ms).collect();
    assert!(times.windows(2).all(|pair| pair[0] <= pair[1]), "{times:?}");
}

#[test]
fn a_stamp_names_the_version_and_time_its_commit_gets() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = fast().open(dir.path().join("s.ferro")).unwrap();
    plain(&db, 1);
    let done = mark(&db, 7);
    assert_eq!(done.value, (done.version, done.time_ms));
    assert_eq!(done.version, 2);
    assert!(done.changed);
    assert_eq!(time_of(&db, 2), done.time_ms);
    assert_eq!(db.read().commit().time_ms, done.time_ms);
    assert_lands(&db, 7, done.value);

    let failed = db.try_commit_stamped(|root, stamp| -> Result<(&str, ()), Error> {
        root.marks.insert(8, (stamp.version(), stamp.time_ms()));
        Ok(("mark 8", ()))
    });
    let done = failed.unwrap();
    assert_eq!(done.version, 3);
    assert_eq!(
        db.read().marks.get(&8).map(|seen| *seen),
        Some((3, done.time_ms))
    );
    assert_lands(&db, 8, (3, done.time_ms));
}

#[test]
fn committed_carries_the_time_of_every_kind_of_write() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = fast().open(dir.path().join("c.ferro")).unwrap();
    let first = plain(&db, 1);
    assert_eq!((first.version, first.time_ms), (1, time_of(&db, 1)));
    let tried = db
        .try_commit_with(|root| -> Result<(&str, ()), Error> {
            root.plain.insert(2, 2);
            Ok(("two", ()))
        })
        .unwrap();
    assert_eq!((tried.version, tried.time_ms), (2, time_of(&db, 2)));

    let idle = db.commit_with(|_| ("nothing", ())).unwrap();
    assert!(!idle.changed);
    assert_eq!((idle.version, idle.time_ms), (2, time_of(&db, 2)));

    let branch = db.fork("side").unwrap();
    plain(&branch, 3);
    plain(&db, 4);
    let merged = db
        .merge_with(&branch, |ours, base, theirs| {
            ours.plain.merge(&base.plain, &theirs.plain, |conflict| {
                conflict.theirs.map(|v| *v)
            });
            ("merge side", ())
        })
        .unwrap();
    assert_eq!((merged.version, merged.time_ms), (4, time_of(&db, 4)));
    assert!(merged.time_ms >= branch.read().commit().time_ms);

    let restored = db.restore(&db.at(1).unwrap()).unwrap();
    assert_eq!((restored.version, restored.time_ms), (5, time_of(&db, 5)));
}

#[test]
fn a_stamped_commit_keeps_its_millisecond_while_the_clock_stands_still() {
    let dir = tempfile::tempdir().unwrap();
    let wall = Wall::at(10_000);
    let db = open_at(&dir.path().join("still.ferro"), &wall);
    plain(&db, 1);
    plain(&db, 2);
    assert_eq!((time_of(&db, 1), time_of(&db, 2)), (10_000, 10_000));
    let stamped = mark(&db, 1);
    assert_eq!(stamped.value, (3, 10_000));
    assert_eq!(plain(&db, 3).time_ms, 10_001);
    assert_eq!(plain(&db, 4).time_ms, 10_001);
    assert_lands(&db, 1, stamped.value);
    assert_eq!(db.as_of(10_001).unwrap().version(), 5);
    assert_eq!(db.as_of(9_999).unwrap().version(), 0);

    let again = mark(&db, 2);
    assert_eq!(again.value, (6, 10_001));
    let next = mark(&db, 3);
    assert_eq!(next.value, (7, 10_002));
    assert_lands(&db, 2, again.value);
    assert_lands(&db, 3, next.value);

    let version_only = db
        .commit_stamped(|root, stamp| {
            root.plain.insert(100, stamp.version());
            ("version only", stamp.version())
        })
        .unwrap();
    assert_eq!((version_only.value, version_only.time_ms), (8, 10_003));
    assert_eq!(plain(&db, 5).time_ms, 10_003);
    assert_eq!(db.as_of(10_003).unwrap().version(), 9);
    assert_times_never_go_back(&db);
}

#[test]
fn commit_times_hold_their_order_when_the_clock_goes_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let wall = Wall::at(50_000);
    let db = open_at(&dir.path().join("back.ferro"), &wall);
    plain(&db, 1);
    wall.set(20_000);
    let behind = mark(&db, 1);
    assert_eq!(behind.value, (2, 50_000));
    assert_eq!(plain(&db, 2).time_ms, 50_001);
    wall.set(1_000);
    assert_eq!(plain(&db, 3).time_ms, 50_001);
    let second = mark(&db, 2);
    assert_eq!(second.value, (5, 50_001));
    assert_eq!(mark(&db, 3).value, (6, 50_002));
    wall.set(0);
    assert_eq!(plain(&db, 4).time_ms, 50_003);
    wall.set(90_000);
    let ahead = mark(&db, 4);
    assert_eq!(ahead.value, (8, 90_000));
    wall.set(60_000);
    assert_eq!(plain(&db, 5).time_ms, 90_001);

    for (id, seen) in [
        (1, behind.value),
        (2, second.value),
        (3, (6, 50_002)),
        (4, ahead.value),
    ] {
        assert_lands(&db, id, seen);
    }
    assert_eq!(db.as_of(49_999).unwrap().version(), 0);
    assert_eq!(db.as_of(50_000).unwrap().version(), 2);
    assert_eq!(db.as_of(89_999).unwrap().version(), 7);
    assert_times_never_go_back(&db);
}

#[test]
fn a_stamped_millisecond_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.ferro");
    let wall = Wall::at(7_000);
    let db = open_at(&path, &wall);
    plain(&db, 1);
    let stamped = mark(&db, 1);
    assert_eq!(stamped.value, (2, 7_000));
    drop(db);

    let db = open_at(&path, &wall);
    assert_lands(&db, 1, stamped.value);
    assert_eq!(plain(&db, 2).time_ms, 7_001);
    assert_eq!(plain(&db, 3).time_ms, 7_001);
    assert_lands(&db, 1, stamped.value);
    drop(db);

    let db = open_at(&path, &wall);
    assert_eq!(plain(&db, 4).time_ms, 7_001);
    db.verify().unwrap();
    let reader: Db<Root> = Options::new().read_only(true).open(&path).unwrap();
    assert_eq!(reader.as_of(7_000).unwrap().version(), 2);
    assert_eq!(reader.as_of(7_001).unwrap().version(), 5);
}

#[test]
fn a_stamped_millisecond_survives_a_reclamation_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cycle.ferro");
    let wall = Wall::at(5_000);
    let db = open_at(&path, &wall);
    plain(&db, 1);
    plain(&db, 2);
    let stamped = mark(&db, 1);
    assert_eq!(stamped.value, (3, 5_000));
    db.compact(Keep::Last(2)).unwrap();
    assert!(matches!(db.at(1), Err(Error::NoVersion(1))));
    assert_lands(&db, 1, stamped.value);
    assert_eq!(plain(&db, 3).time_ms, 5_001);
    let second = mark(&db, 2);
    assert_eq!(second.value, (5, 5_001));
    db.compact(Keep::Last(1)).unwrap();
    assert!(matches!(db.as_of(5_000), Err(Error::NoVersion(_))));
    drop(db);
    let db = open_at(&path, &wall);
    assert_eq!(db.as_of(5_001).unwrap().version(), 5);
    assert_eq!(plain(&db, 4).time_ms, 5_002);
    assert_eq!(db.as_of(5_001).unwrap().version(), 5);
    db.verify().unwrap();
}

#[test]
fn an_error_a_panic_or_a_no_op_claims_no_millisecond() {
    common::quiet_ferro_panics();
    let dir = tempfile::tempdir().unwrap();
    let wall = Wall::at(3_000);
    let db = open_at(&dir.path().join("claims.ferro"), &wall);
    plain(&db, 1);
    let head = db.head_hash();

    let refused = db.try_commit_stamped(|root, stamp| -> Result<(&str, ()), Error> {
        root.marks.insert(1, (stamp.version(), stamp.time_ms()));
        Err(Error::Decode(format!(
            "refused v{} at {}",
            stamp.version(),
            stamp.time_ms()
        )))
    });
    match refused {
        Err(Error::Decode(message)) => assert_eq!(message, "refused v2 at 3000"),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!((db.version(), db.head_hash()), (1, head));

    let panicked = panic::catch_unwind(AssertUnwindSafe(|| {
        db.commit_stamped(|root, stamp| -> (&str, ()) {
            root.marks.insert(1, (stamp.version(), stamp.time_ms()));
            panic!("ferro: test panic after reading the stamp");
        })
    }));
    assert!(panicked.is_err());
    assert_eq!((db.version(), db.head_hash()), (1, head));

    let idle = db
        .commit_stamped(|_, stamp| ("idle", (stamp.version(), stamp.time_ms())))
        .unwrap();
    assert_eq!(idle.value, (2, 3_000));
    assert!(!idle.changed);
    assert_eq!((idle.version, idle.time_ms), (1, 3_000));

    assert_eq!(plain(&db, 2).time_ms, 3_000);
    assert_eq!(db.as_of(3_000).unwrap().version(), 2);
}

#[test]
fn forks_merges_and_pulls_keep_a_stamped_millisecond() {
    let dir = tempfile::tempdir().unwrap();
    let wall = Wall::at(4_000);
    let db = open_at(&dir.path().join("a.ferro"), &wall);
    plain(&db, 1);
    let stamped = mark(&db, 1);
    assert_eq!(stamped.value, (2, 4_000));

    let side = db.fork("side").unwrap();
    assert_eq!(plain(&side, 2).time_ms, 4_001);
    assert_eq!(side.as_of(4_000).unwrap().version(), 2);

    let merged = db
        .merge_with(&side, |ours, base, theirs| {
            ours.plain.merge(&base.plain, &theirs.plain, |conflict| {
                conflict.theirs.map(|v| *v)
            });
            ("merge side", ())
        })
        .unwrap();
    assert_eq!((merged.version, merged.time_ms), (3, 4_001));
    assert_lands(&db, 1, stamped.value);

    let other = open_at(&dir.path().join("b.ferro"), &wall);
    let pulled = other.pull(&db, "a", |_, _, _| ("never", ())).unwrap();
    assert_eq!(pulled.outcome, ferro::Outcome::FastForward);
    assert_lands(&other, 1, stamped.value);
    let late = mark(&other, 2);
    assert_eq!(late.value, (4, 4_001));
    assert_eq!(plain(&other, 3).time_ms, 4_002);
    assert_lands(&other, 2, late.value);

    let back = db.pull(&other, "b", |_, _, _| ("never", ())).unwrap();
    assert_eq!(back.outcome, ferro::Outcome::FastForward);
    assert_eq!(plain(&db, 4).time_ms, 4_002);
    assert_lands(&db, 2, late.value);
    assert_times_never_go_back(&db);
}

fn race(db: &Db<Root>, coalesce: bool) {
    let threads = 8u64;
    let each = 150u64;
    let writers: Vec<_> = (0..threads)
        .map(|thread| {
            let db = db.clone();
            thread::spawn(move || {
                let mut seen = Vec::new();
                for i in 0..each {
                    let id = thread * 1_000 + i;
                    if i % 3 == 0 {
                        plain(&db, id);
                        continue;
                    }
                    let done = db
                        .try_commit_stamped(|root, stamp: &Stamp<'_>| -> Result<_, Error> {
                            let at = (stamp.version(), stamp.time_ms());
                            root.marks.insert(id, at);
                            Ok((format!("mark {id}"), at))
                        })
                        .unwrap();
                    assert_eq!(done.value, (done.version, done.time_ms));
                    assert!(done.changed);
                    seen.push((id, done.value));
                }
                seen
            })
        })
        .collect();
    let seen: Vec<(u64, (u64, u64))> = writers
        .into_iter()
        .flat_map(|writer| writer.join().unwrap())
        .collect();
    assert_eq!(seen.len() as u64, threads * (each - each.div_ceil(3)));
    let tip = db.read();
    for (id, at) in &seen {
        assert_eq!(tip.marks.get(id).map(|seen| *seen), Some(*at));
        assert_lands(db, *id, *at);
        assert_eq!(time_of(db, at.0), at.1);
    }
    let mut versions: Vec<u64> = seen.iter().map(|(_, at)| at.0).collect();
    versions.sort_unstable();
    let shared = versions
        .windows(2)
        .filter(|pair| pair[0] == pair[1])
        .count();
    if !coalesce {
        assert_eq!(shared, 0);
    }
    for pair in seen.iter().flat_map(|a| seen.iter().map(move |b| (a, b))) {
        let ((_, a), (_, b)) = pair;
        assert_eq!(a.0 == b.0, a.1 == b.1, "{a:?} and {b:?}");
    }
    assert_times_never_go_back(db);
}

#[test]
fn concurrent_writers_each_land_on_the_commit_their_stamp_named() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = Options::new().open(dir.path().join("race.ferro")).unwrap();
    race(&db, false);
}

#[test]
fn coalesced_writers_each_land_on_their_groups_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = Options::new()
        .coalesce(true)
        .open(dir.path().join("group.ferro"))
        .unwrap();
    race(&db, true);
}

#[test]
fn a_test_clock_drives_commit_times() {
    let dir: TempDir = tempfile::tempdir().unwrap();
    let wall = Wall::at(123_456);
    let db = open_at(&dir.path().join("clock.ferro"), &wall);
    assert_eq!(plain(&db, 1).time_ms, 123_456);
    wall.set(123_999);
    assert_eq!(mark(&db, 1).value, (2, 123_999));
    assert_eq!(db.as_of(123_998).unwrap().version(), 1);
}
