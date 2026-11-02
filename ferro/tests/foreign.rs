mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};

use common::{Rng, cases, fast, roomy, text, uncached};
use ferro::codec::{Decoder, Encoder};
use ferro::{Data, Db, Error, List, Map, Options, Set};

type Flat = Map<u64, String>;
type Nested = Map<u64, Map<u64, String>>;

fn fill(db: &Db<Flat>, count: u64) -> BTreeMap<u64, String> {
    let mut model = BTreeMap::new();
    db.update(|root| {
        for key in 0..count {
            let value = text(key, if key % 5 == 0 { 900 } else { 16 });
            root.insert(key, value.clone());
            model.insert(key, value);
        }
    })
    .unwrap();
    model
}

fn assert_flat(map: &Flat, model: &BTreeMap<u64, String>) {
    assert_eq!(map.len(), model.len());
    for ((a, b), (c, d)) in map.iter().zip(model.iter()) {
        assert_eq!(a, *c);
        assert_eq!(*b, *d);
    }
}

fn assert_unlocked<T: Data + Default>(path: &Path) {
    match fast().open::<T>(path) {
        Ok(_) => {}
        Err(Error::Locked) => panic!("{} is still locked", path.display()),
        Err(other) => panic!("open failed: {other}"),
    }
}

#[test]
fn map_moved_between_databases_survives_loss_of_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("a.ferro");
    let target = dir.path().join("b.ferro");
    let a = fast().open::<Flat>(&source).unwrap();
    let model = fill(&a, 2000);
    let b = fast().open::<Flat>(&target).unwrap();
    let moved = a.read();
    b.update(|root| *root = (*moved).clone()).unwrap();
    assert_eq!(b.version(), 1);
    assert_eq!(b.read().root_hash(), moved.root_hash());
    b.update(|root| *root = (*moved).clone()).unwrap();
    assert_eq!(b.version(), 1);
    drop(moved);
    drop(a);
    fs::remove_file(&source).unwrap();
    drop(b);
    let b = uncached().open::<Flat>(&target).unwrap();
    assert_flat(&b.read(), &model);
    b.verify().unwrap();
}

#[test]
fn partially_edited_foreign_map_is_copied_whole() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("a.ferro");
    let target = dir.path().join("b.ferro");
    let a = fast().open::<Flat>(&source).unwrap();
    let mut model = fill(&a, 2000);
    let b = fast().open::<Flat>(&target).unwrap();
    let mut edited = (*a.read()).clone();
    edited.insert(5000, "new".repeat(400));
    edited.remove(&10);
    edited.update(&15, |value| value.push_str(&"p".repeat(800)));
    model.insert(5000, "new".repeat(400));
    model.remove(&10);
    model.get_mut(&15).unwrap().push_str(&"p".repeat(800));
    b.update(|root| *root = edited.clone()).unwrap();
    drop(edited);
    drop(a);
    fs::remove_file(&source).unwrap();
    drop(b);
    let b = uncached().open::<Flat>(&target).unwrap();
    assert_flat(&b.read(), &model);
    b.verify().unwrap();
}

#[test]
fn nested_maps_are_rebuilt_in_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("a.ferro");
    let target = dir.path().join("b.ferro");
    let a = fast().open::<Nested>(&source).unwrap();
    a.update(|root| {
        for outer in 0..50 {
            let mut inner = Map::new();
            for key in 0..200 {
                inner.insert(key, text(key, if key % 7 == 0 { 800 } else { 10 }));
            }
            root.insert(outer, inner);
        }
    })
    .unwrap();
    let b = fast().open::<Nested>(&target).unwrap();
    let snapshot = a.read();
    b.update(|root| {
        root.insert(0, (*snapshot.get(&3).unwrap()).clone());
        let mut edited = (*snapshot.get(&4).unwrap()).clone();
        edited.insert(10_000, "edited".into());
        root.insert(1, edited);
    })
    .unwrap();
    drop(snapshot);
    drop(a);
    fs::remove_file(&source).unwrap();
    drop(b);
    let b = uncached().open::<Nested>(&target).unwrap();
    let snapshot = b.read();
    assert_eq!(snapshot.get(&0).unwrap().len(), 200);
    assert_eq!(snapshot.get(&1).unwrap().len(), 201);
    assert_eq!(
        *snapshot.get(&0).unwrap().get(&14).unwrap(),
        text(14, 800)
    );
    b.verify().unwrap();
}

#[test]
fn target_cache_does_not_pin_the_source_database() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("a.ferro");
    let target = dir.path().join("b.ferro");
    let a = roomy().open::<Nested>(&source).unwrap();
    a.update(|root| {
        let mut inner = Map::new();
        inner.insert(1, "inner".to_string());
        root.insert(1, inner);
    })
    .unwrap();
    drop(a);
    let a = roomy().open::<Nested>(&source).unwrap();
    let b = roomy().open::<Nested>(&target).unwrap();
    let snapshot = a.read();
    let inner = (*snapshot.get(&1).unwrap()).clone();
    b.update(|root| {
        root.insert(7, inner);
    })
    .unwrap();
    let copied = b.read();
    assert_eq!(*copied.get(&7).unwrap().get(&1).unwrap(), "inner");
    drop(copied);
    drop(snapshot);
    drop(a);
    assert_unlocked::<Nested>(&source);
    assert_eq!(*b.read().get(&7).unwrap().get(&1).unwrap(), "inner");
}

#[test]
fn map_with_nested_maps_moved_whole_keeps_every_level() {
    let dir = tempfile::tempdir().unwrap();
    let a = fast().open::<Nested>(dir.path().join("a.ferro")).unwrap();
    let b = fast().open::<Nested>(dir.path().join("b.ferro")).unwrap();
    a.update(|root| {
        for outer in 0..40 {
            let inner: Map<u64, String> = (0..30).map(|k| (k, k.to_string())).collect();
            root.insert(outer, inner);
        }
    })
    .unwrap();
    b.update(|root| *root = (*a.read()).clone()).unwrap();
    assert_eq!(a.read().root_hash(), b.read().root_hash());
    let bp = b.path();
    drop(b);
    drop(a);
    let b = uncached().open::<Nested>(bp).unwrap();
    assert_eq!(*b.read().get(&39).unwrap().get(&29).unwrap(), "29");
}

#[test]
fn set_and_list_move_between_databases() {
    let dir = tempfile::tempdir().unwrap();
    type Root = (Set<u64>, List<String>);
    let a = fast().open::<Root>(dir.path().join("a.ferro")).unwrap();
    let b = fast().open::<Root>(dir.path().join("b.ferro")).unwrap();
    a.update(|root| {
        for n in 0..500 {
            root.0.insert(n);
            root.1.push(text(n, 600));
        }
    })
    .unwrap();
    b.update(|root| *root = (*a.read()).clone()).unwrap();
    let path = b.path();
    drop(b);
    fs::remove_file(a.path()).unwrap();
    drop(a);
    let b = uncached().open::<Root>(path).unwrap();
    let snapshot = b.read();
    assert_eq!(snapshot.0.len(), 500);
    assert_eq!(*snapshot.1.get(499).unwrap(), text(499, 600));
    b.verify().unwrap();
}

#[test]
fn map_retained_across_a_no_op_commit_stays_valid() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n.ferro");
    let db = fast().open::<Flat>(&path).unwrap();
    let model = fill(&db, 1000);
    let mut stash: Option<Flat> = None;
    db.update(|root| {
        let retained = root.clone();
        root.insert(1, "temp".repeat(300));
        root.insert(1, model[&1].clone());
        root.insert(5000, "x".repeat(900));
        root.remove(&5000);
        stash = Some(retained);
    })
    .unwrap();
    assert_eq!(db.version(), 1);
    let retained = stash.take().unwrap();
    assert_flat(&retained, &model);

    let mut built = (*db.read()).clone();
    built.insert(6000, "y".repeat(900));
    let mut unused = built.clone();
    unused.remove(&6000);
    db.update(|root| *root = unused.clone()).unwrap();
    assert_eq!(db.version(), 1);
    db.update(|root| *root = built.clone()).unwrap();
    assert_eq!(db.version(), 2);
    assert_eq!(db.read().get(&6000).unwrap().len(), 900);

    db.update(|root| *root = retained.clone()).unwrap();
    assert_eq!(db.version(), 3);
    assert_flat(&db.read(), &model);
    drop((retained, built, unused));
    drop(db);
    let db = uncached().open::<Flat>(&path).unwrap();
    assert_flat(&db.read(), &model);
    db.verify().unwrap();
}

#[test]
fn offsets_from_an_abandoned_transaction_are_never_reused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.ferro");
    let db = fast().open::<Flat>(&path).unwrap();
    fill(&db, 100);
    let mut retained = (*db.read()).clone();
    let body = "z".repeat(2000);
    retained.insert(200, body.clone());
    let attempt: Result<(), Error> = db.try_update(|root| {
        *root = retained.clone();
        Err(Error::NoVersion(1))
    });
    assert!(attempt.is_err());
    let unchanged = (*db.read()).clone();
    db.update(|root| *root = unchanged.clone()).unwrap();
    assert_eq!(db.version(), 1);
    db.update(|root| {
        root.insert(201, "other".into());
    })
    .unwrap();
    db.update(|root| *root = retained.clone()).unwrap();
    assert_eq!(*db.read().get(&200).unwrap(), body);
    drop((retained, unchanged));
    drop(db);
    let db = uncached().open::<Flat>(&path).unwrap();
    assert_eq!(*db.read().get(&200).unwrap(), body);
    db.verify().unwrap();
}

#[test]
fn retained_map_works_after_its_database_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.ferro");
    let db = fast().open::<Flat>(&path).unwrap();
    let model = fill(&db, 1500);
    let snapshot = db.read();
    let map = (*snapshot).clone();
    let mut iter = snapshot.iter();
    let mut changes = map.diff(&Map::new());
    assert_eq!(iter.next().map(|p| p.0), Some(0));
    assert_eq!(changes.next().map(|c| *c.key()), Some(0));
    drop(snapshot);
    drop(db);
    assert_eq!(iter.next_back().map(|p| p.0), Some(1499));
    assert_eq!(iter.len(), 1498);
    assert_eq!(iter.count(), 1498);
    assert_eq!(changes.count(), 1499);
    assert_flat(&map, &model);
    assert_eq!(*map.get(&15).unwrap(), model[&15]);
    assert_eq!(map.range(10..20).count(), 10);
    let mut edited = map.clone();
    edited.insert(9999, "late".into());
    edited.remove(&0);
    edited.update(&5, |value| value.push('!'));
    assert_eq!(map.diff(&edited).count(), 3);
    assert!(matches!(
        fast().open::<Flat>(&path),
        Err(Error::Locked)
    ));
    drop(map);
    drop(edited);
    let db = fast().open::<Flat>(&path).unwrap();
    assert_flat(&db.read(), &model);
}

#[test]
fn lock_is_released_after_every_handle_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("l.ferro");
    let db = fast().open::<Flat>(&path).unwrap();
    fill(&db, 800);
    let snapshot = db.read();
    let old = db.at(1).unwrap();
    let inner = (*snapshot).clone();
    let log = db.log();
    let events = db.watch();
    db.update(|root| {
        root.insert(1, "w".repeat(1000));
    })
    .unwrap();
    assert!(inner.get(&5).is_some());
    drop(db);
    assert!(snapshot.get(&5).is_some());
    assert_eq!(log.len(), 1);
    assert!(matches!(fast().open::<Flat>(&path), Err(Error::Locked)));
    drop(snapshot);
    assert!(matches!(fast().open::<Flat>(&path), Err(Error::Locked)));
    assert!(old.get(&5).is_some());
    drop(old);
    drop(inner);
    assert!(events.try_recv().is_err());
    drop(log);
    assert_unlocked::<Flat>(&path);
}

#[test]
fn lock_is_released_after_reads_through_a_closed_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.ferro");
    let db = roomy().open::<Nested>(&path).unwrap();
    db.update(|root| {
        for outer in 0..64 {
            root.insert(outer, (0..64).map(|k| (k, k.to_string())).collect());
        }
    })
    .unwrap();
    let snapshot = db.read();
    drop(db);
    for outer in 0..64 {
        assert_eq!(snapshot.get(&outer).unwrap().len(), 64);
        assert_eq!(*snapshot.get(&outer).unwrap().get(&3).unwrap(), "3");
    }
    drop(snapshot);
    assert_unlocked::<Nested>(&path);
}

#[test]
fn lock_is_released_when_threads_keep_reading_during_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.ferro");
    let open = || fast().cache_bytes(1 << 24).open::<Nested>(&path).unwrap();
    open()
        .update(|root| {
            for outer in 0..256 {
                root.insert(outer, (0..4).map(|k| (k, k.to_string())).collect());
            }
        })
        .unwrap();
    for _ in 0..cases(400) {
        let db = open();
        let snapshot = db.read();
        let start = Arc::new(Barrier::new(5));
        let mut readers = Vec::new();
        for offset in 0..4u64 {
            let snapshot = snapshot.clone();
            let start = start.clone();
            readers.push(std::thread::spawn(move || {
                start.wait();
                for outer in 0..256 {
                    let key = (outer + offset * 64) % 256;
                    assert_eq!(snapshot.get(&key).unwrap().len(), 4);
                }
            }));
        }
        drop(snapshot);
        start.wait();
        drop(db);
        for reader in readers {
            reader.join().unwrap();
        }
        assert_unlocked::<Nested>(&path);
    }
}

#[derive(Clone, Default)]
struct Wrapper {
    child: Map<u64, String>,
    blob: Vec<Map<u64, String>>,
}

impl Data for Wrapper {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.child.encode(w)?;
        self.blob.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Wrapper {
            child: Map::decode(r)?,
            blob: Vec::decode(r)?,
        })
    }
}

#[test]
fn spilled_value_holding_maps_moves_between_databases() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("a.ferro");
    let target = dir.path().join("b.ferro");
    type Root = Map<u64, Wrapper>;
    let a = fast().open::<Root>(&source).unwrap();
    a.update(|root| {
        let child: Map<u64, String> = (0..100).map(|k| (k, k.to_string())).collect();
        let blob: Vec<Map<u64, String>> = (0..80)
            .map(|n| (0..20).map(|k| (k, format!("{n}-{k}"))).collect())
            .collect();
        root.insert(1, Wrapper { child, blob });
    })
    .unwrap();
    let b = fast().open::<Root>(&target).unwrap();
    b.update(|root| *root = (*a.read()).clone()).unwrap();
    drop(a);
    fs::remove_file(&source).unwrap();
    drop(b);
    let b = uncached().open::<Root>(&target).unwrap();
    let snapshot = b.read();
    let value = snapshot.get(&1).unwrap();
    assert_eq!(value.child.len(), 100);
    assert_eq!(*value.blob[79].get(&19).unwrap(), "79-19");
    b.verify().unwrap();
}

#[test]
fn nested_map_edits_round_trip_at_every_cache_size() {
    let options: [fn() -> Options; 3] = [
        fast,
        uncached,
        || Options::new().durability(ferro::Durability::Buffered),
    ];
    for open in options {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.ferro");
        let db = open().open::<Nested>(&path).unwrap();
        let mut model: BTreeMap<u64, BTreeMap<u64, String>> = BTreeMap::new();
        let mut rng = Rng::new(21);
        for _ in 0..400 {
            let outer = rng.below(5);
            let key = rng.below(64);
            let value = text(rng.next(), rng.below(900) as usize);
            let remove = rng.chance(25);
            db.update(|root| {
                let inner = root.get_or_insert_with(outer, Map::new);
                if remove {
                    inner.remove(&key);
                } else {
                    inner.insert(key, value.clone());
                }
            })
            .unwrap();
            let slot = model.entry(outer).or_default();
            if remove {
                slot.remove(&key);
            } else {
                slot.insert(key, value);
            }
            let snapshot = db.read();
            let inner = snapshot.get(&outer).unwrap();
            assert_eq!(inner.len(), slot.len());
            assert!(
                inner
                    .iter()
                    .map(|(k, v)| (k, (*v).clone()))
                    .eq(slot.iter().map(|(k, v)| (*k, v.clone())))
            );
        }
        drop(db);
        let db = open().open::<Nested>(&path).unwrap();
        let snapshot = db.read();
        assert_eq!(snapshot.len(), model.len());
        for (outer, wanted) in &model {
            assert_eq!(snapshot.get(outer).unwrap().len(), wanted.len());
        }
        db.verify().unwrap();
    }
}
