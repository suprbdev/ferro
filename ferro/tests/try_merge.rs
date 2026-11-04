mod common;

use std::path::Path;

use common::fast;
use ferro::{Bundle, Data, Db, Error, Have, Map, Options, Outcome, Record, Table};

#[derive(Debug)]
enum Refusal {
    Ferro(Error),
    Clash(u64),
}

impl From<Error> for Refusal {
    fn from(error: Error) -> Self {
        Refusal::Ferro(error)
    }
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Seat {
    #[key]
    id: u64,
    #[unique]
    holder: String,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    counts: Map<u64, i64>,
    seats: Table<Seat>,
}

fn seat(id: u64, holder: &str) -> Seat {
    Seat {
        id,
        holder: holder.to_string(),
    }
}

fn open(path: &Path) -> Db<Root> {
    fast().open(path).unwrap()
}

fn count(db: &Db<Root>, key: u64, value: i64) {
    db.commit(format!("count {key}"), |root| {
        root.counts.insert(key, value);
    })
    .unwrap();
}

fn take_theirs(ours: &mut Root, base: &Root, theirs: &Root) -> usize {
    ours.counts.merge(&base.counts, &theirs.counts, |conflict| {
        conflict.theirs.map(|v| *v)
    })
}

fn diverged(db: &Db<Root>) -> (Db<Root>, u64) {
    count(db, 1, 1);
    let side = db.fork("side").unwrap();
    let forked = side.version();
    count(&side, 1, 5);
    count(&side, 2, 2);
    count(db, 3, 3);
    (side, forked)
}

#[test]
fn an_err_from_try_merge_with_discards_the_merge_and_returns_the_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("merge.ferro");
    let db = open(&path);
    let (side, forked) = diverged(&db);
    let (version, head, before) = (db.version(), db.head_hash(), db.read());
    let size = db.size_on_disk();

    let mut ran = 0;
    let refused = db.try_merge_with(&side, |ours, base, theirs| {
        ran += 1;
        assert_eq!(base.version(), forked);
        take_theirs(ours, base, theirs);
        ours.counts.insert(9, 9);
        Err::<(&str, ()), _>(Refusal::Clash(7))
    });
    assert!(matches!(refused, Err(Refusal::Clash(7))), "{refused:?}");
    assert_eq!(ran, 1);
    assert_eq!((db.version(), db.head_hash()), (version, head));
    assert_eq!(*db.read(), *before);
    assert_eq!(db.size_on_disk(), size);
    assert_eq!(db.common_ancestor(&side).unwrap().version(), forked);

    let done = db
        .try_merge_with(&side, |ours, base, theirs| -> Result<_, Refusal> {
            assert_eq!(base.version(), forked);
            let taken = take_theirs(ours, base, theirs);
            Ok(("merge side", taken))
        })
        .unwrap();
    assert_eq!(
        (done.value, done.version, done.changed),
        (2, version + 1, true)
    );
    assert_eq!(done.time_ms, db.log().get(&done.version).unwrap().time_ms);
    assert_eq!(db.common_ancestor(&side).unwrap().version(), side.version());
    let merged: Vec<(u64, i64)> = db.read().counts.iter().map(|(k, v)| (k, *v)).collect();
    assert_eq!(merged, vec![(1, 5), (2, 2), (3, 3)]);
    drop((db, side));

    let db = open(&path);
    assert_eq!(db.version(), version + 1);
    db.verify().unwrap();
}

#[test]
fn try_merge_with_hands_ferro_errors_back_in_the_callers_type() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("gone.ferro"));
    let (side, _) = diverged(&db);
    db.drop_branch("side").unwrap();
    let mut ran = false;
    let gone = db.try_merge_with(&side, |_, _, _| {
        ran = true;
        Ok::<_, Refusal>(("never", ()))
    });
    assert!(matches!(gone, Err(Refusal::Ferro(Error::NoBranch(name))) if name == "side"));
    assert!(!ran);
}

#[test]
fn an_err_merge_under_coalesce_writes_nothing_and_leaves_writers_running() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = Options::new()
        .coalesce(true)
        .open(dir.path().join("c.ferro"))
        .unwrap();
    let (side, forked) = diverged(&db);
    let version = db.version();
    let refused = db.try_merge_with(&side, |ours, base, theirs| {
        take_theirs(ours, base, theirs);
        Err::<(&str, ()), _>(Refusal::Clash(1))
    });
    assert!(matches!(refused, Err(Refusal::Clash(1))));
    assert_eq!(db.version(), version);
    count(&db, 4, 4);
    assert_eq!(db.version(), version + 1);
    assert_eq!(db.common_ancestor(&side).unwrap().version(), forked);
}

#[test]
fn map_try_merge_leaves_the_map_as_it_was_on_err() {
    let base: Map<u64, i64> = (0..10).map(|key| (key, 0)).collect();
    let mut ours = base.clone();
    let mut theirs = base.clone();
    for key in [2, 4, 6] {
        ours.insert(key, 1);
        theirs.insert(key, 2);
    }
    theirs.insert(20, 20);
    theirs.remove(&0);
    let before = ours.clone();
    let mut asked = Vec::new();
    let refused = ours.try_merge(&base, &theirs, |conflict| {
        asked.push(conflict.key);
        if conflict.key == 4 {
            return Err(conflict.key);
        }
        Ok(conflict.theirs.map(|v| *v))
    });
    assert_eq!(refused, Err(4));
    assert_eq!(asked, vec![2, 4]);
    assert!(ours == before);
    assert_eq!(ours.get(&2).map(|v| *v), Some(1));
    assert!(ours.contains_key(&0) && !ours.contains_key(&20));

    let mut plain = ours.clone();
    let merged = ours.try_merge(&base, &theirs, |conflict| {
        Ok::<_, ()>(conflict.theirs.map(|v| *v))
    });
    let written = plain.merge(&base, &theirs, |conflict| conflict.theirs.map(|v| *v));
    assert_eq!(merged, Ok(written));
    assert!(ours == plain);
}

#[test]
fn table_try_merge_leaves_the_table_as_it_was_on_err_in_either_pass() {
    let mut base: Table<Seat> = Table::new();
    base.insert(seat(1, "ann"));
    base.insert(seat(2, "bob"));
    let mut ours = base.clone();
    ours.insert(seat(3, "cat"));
    ours.update(&1, |row| row.holder = "amy".to_string());
    let mut theirs = base.clone();
    theirs.insert(seat(4, "cat"));
    theirs.update(&1, |row| row.holder = "abe".to_string());
    theirs.remove(&2);
    let before = ours.clone();

    let first = ours.try_merge(&base, &theirs, |conflict| {
        if conflict.key == 1 {
            return Err(Refusal::Clash(1));
        }
        Ok(conflict.theirs.map(|row| (*row).clone()))
    });
    assert!(matches!(first, Err(Refusal::Clash(1))));
    assert!(ours == before);

    let mut asked = Vec::new();
    let clash = ours.try_merge(&base, &theirs, |conflict| {
        asked.push((conflict.key, conflict.ours.map(|row| row.id)));
        if conflict.key == 4 {
            return Err(Refusal::Clash(4));
        }
        Ok(conflict.theirs.map(|row| (*row).clone()))
    });
    assert!(matches!(clash, Err(Refusal::Clash(4))));
    assert_eq!(asked, vec![(1, Some(1)), (4, Some(3))]);
    assert!(ours == before);
    assert!(
        ours.by_holder(&"cat".to_string())
            .is_some_and(|row| row.id == 3)
    );
    assert!(ours.contains(&2));

    let done = ours.try_merge(&base, &theirs, |conflict| {
        Ok::<_, Refusal>(match conflict.key {
            4 => None,
            _ => conflict.theirs.map(|row| (*row).clone()),
        })
    });
    assert!(matches!(done, Ok(3)));
    let rows: Vec<Seat> = ours.iter().map(|row| (*row).clone()).collect();
    assert_eq!(rows, vec![seat(1, "abe"), seat(3, "cat")]);
}

fn shared_pair(dir: &Path) -> (Db<Root>, Db<Root>, u64) {
    let laptop = open(&dir.join("laptop.ferro"));
    let phone = open(&dir.join("phone.ferro"));
    count(&laptop, 1, 1);
    let pulled = phone
        .pull(&laptop, "laptop", |_, _, _| ("never", ()))
        .unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    let shared = laptop.version();
    count(&laptop, 2, 2);
    count(&phone, 3, 3);
    count(&phone, 1, 7);
    (laptop, phone, shared)
}

#[test]
fn a_refused_pull_merges_nothing_and_a_retry_merges_against_the_same_base() {
    let dir = tempfile::tempdir().unwrap();
    let (laptop, phone, shared) = shared_pair(dir.path());
    let (version, head, before) = (laptop.version(), laptop.head_hash(), laptop.read());

    let refused = laptop.try_pull(&phone, "phone", |ours, base, theirs| {
        assert_eq!(base.version(), shared);
        take_theirs(ours, base, theirs);
        Err::<(&str, ()), _>(Refusal::Clash(3))
    });
    assert!(matches!(refused, Err(Refusal::Clash(3))));
    assert_eq!((laptop.version(), laptop.head_hash()), (version, head));
    assert_eq!(*laptop.read(), *before);
    assert_eq!(
        laptop.branch("phone").unwrap().head_hash(),
        phone.head_hash()
    );

    let pulled = laptop
        .try_pull(
            &phone,
            "phone",
            |ours, base, theirs| -> Result<_, Refusal> {
                assert_eq!(base.version(), shared);
                Ok(("sync from the phone", take_theirs(ours, base, theirs)))
            },
        )
        .unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    assert_eq!(pulled.value, Some(2));
    assert_eq!(pulled.version, version + 1);
    let merged: Vec<(u64, i64)> = laptop.read().counts.iter().map(|(k, v)| (k, *v)).collect();
    assert_eq!(merged, vec![(1, 7), (2, 2), (3, 3)]);

    let again = laptop.try_pull(&phone, "phone", |_, _, _| {
        Err::<(&str, ()), _>(Refusal::Clash(0))
    });
    assert_eq!(again.unwrap().outcome, Outcome::Ahead);
}

#[test]
fn a_refused_receive_or_push_merges_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (laptop, phone, shared) = shared_pair(dir.path());
    let version = phone.version();

    let have: Vec<u8> = phone.have().unwrap().into();
    let bundle: Vec<u8> = laptop.bundle(&Have::from(have)).unwrap().into();
    let refused = phone.try_receive("laptop", &Bundle::from(bundle.clone()), |_, base, _| {
        assert_eq!(base.version(), shared);
        Err::<(&str, ()), _>(Refusal::Clash(5))
    });
    assert!(matches!(refused, Err(Refusal::Clash(5))));
    assert_eq!(phone.version(), version);

    let pushed = laptop.try_push(&phone, "laptop", |_, base, _| {
        assert_eq!(base.version(), shared);
        Err::<(&str, ()), _>(Refusal::Clash(6))
    });
    assert!(matches!(pushed, Err(Refusal::Clash(6))));
    assert_eq!(phone.version(), version);

    let bad = phone.try_receive("laptop", &Bundle::from(vec![1, 2, 3]), |_, _, _| {
        Ok::<(&str, ()), Refusal>(("never", ()))
    });
    assert!(
        matches!(bad, Err(Refusal::Ferro(Error::Bundle(_)))),
        "{bad:?}"
    );

    let received = phone
        .try_receive(
            "laptop",
            &Bundle::from(bundle),
            |ours, base, theirs| -> Result<_, Refusal> {
                Ok(("from the laptop", take_theirs(ours, base, theirs)))
            },
        )
        .unwrap();
    assert_eq!(received.outcome, Outcome::Merged);
    assert_eq!(phone.version(), version + 1);
}
