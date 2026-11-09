use std::collections::HashSet;
use std::path::Path;

use ferro::admin::{self, Keep};
use ferro::codec::Object;
use ferro::store::{Store, StoreOptions, record_len};
use ferro::{Db, Error, Hash, Map, Ptr};

type Root = Map<u64, String>;

fn build(path: &std::path::Path, commits: u64) {
    let db = Db::<Root>::open(path).unwrap();
    for i in 0..commits {
        db.commit(format!("c{i}"), |root| {
            root.insert(i % 50, format!("value {i}"));
        })
        .unwrap();
    }
}

#[test]
fn keep_all_preserves_every_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ferro");
    build(&path, 300);
    let (expected, latest): (Vec<Vec<(u64, String)>>, u64) = {
        let db = Db::<Root>::open(&path).unwrap();
        let all = (1..=300)
            .map(|v| {
                db.at(v)
                    .unwrap()
                    .iter()
                    .map(|(k, s)| (k, (*s).clone()))
                    .collect()
            })
            .collect();
        (all, db.version())
    };
    let report = admin::compact(&path, Keep::All).unwrap();
    assert_eq!(report.commits_dropped, 0);
    assert!(report.after_bytes <= report.before_bytes);
    let db = Db::<Root>::open(&path).unwrap();
    assert_eq!(db.version(), latest);
    for (index, rows) in expected.iter().enumerate() {
        let snapshot = db.at(index as u64 + 1).unwrap();
        let found: Vec<(u64, String)> = snapshot.iter().map(|(k, s)| (k, (*s).clone())).collect();
        assert_eq!(&found, rows);
    }
    assert!(db.verify().unwrap().objects > 0);
}

#[test]
fn pruning_drops_old_versions_and_shrinks_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    build(&path, 2000);
    let head = {
        let db = Db::<Root>::open(&path).unwrap();
        db.read().root_hash()
    };
    let report = admin::compact(&path, Keep::Last(10)).unwrap();
    assert_eq!(report.commits_dropped, 1990);
    assert!(report.after_bytes * 10 < report.before_bytes);
    let db = Db::<Root>::open(&path).unwrap();
    assert_eq!(db.version(), 2000);
    assert_eq!(db.read().root_hash(), head);
    assert_eq!(db.log().len(), 10);
    assert!(db.at(1991).is_ok());
    assert!(matches!(db.at(1990), Err(Error::NoVersion(1990))));
    db.commit("after", |root| {
        root.insert(999, "more".to_string());
    })
    .unwrap();
    assert_eq!(db.version(), 2001);
    drop(db);
    let db = Db::<Root>::open(&path).unwrap();
    assert_eq!(*db.read().get(&999).unwrap(), "more");
    db.verify().unwrap();
}

#[test]
fn branches_survive_and_share_storage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.ferro");
    {
        let db = Db::<Root>::open(&path).unwrap();
        db.update(|root| {
            root.insert(1, "base".to_string());
        })
        .unwrap();
        let other = db.fork("other").unwrap();
        other
            .update(|root| {
                root.insert(2, "fork".to_string());
            })
            .unwrap();
    }
    admin::compact(&path, Keep::Last(1)).unwrap();
    let db = Db::<Root>::open(&path).unwrap();
    let other = db.branch("other").unwrap();
    assert_eq!(db.read().len(), 1);
    assert_eq!(other.read().len(), 2);
    let info = admin::info(&path).unwrap();
    assert_eq!(info.branches.len(), 2);
}

#[test]
fn compaction_refuses_a_live_writer_and_empty_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.ferro");
    let db = Db::<Root>::open(&path).unwrap();
    assert!(matches!(
        admin::compact(&path, Keep::All),
        Err(Error::Locked)
    ));
    drop(db);
    let report = admin::compact(&path, Keep::All).unwrap();
    assert_eq!(report.objects, 0);
}

#[test]
fn compacting_a_missing_path_fails_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.ferro");
    assert!(matches!(
        admin::compact(&path, Keep::All),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
    ));
    assert!(!path.exists());
}

#[test]
fn compacting_an_empty_file_is_not_ferro_and_leaves_it_empty() {
    let dir = tempfile::tempdir().unwrap();
    for (name, bytes) in [("empty.ferro", 0), ("zeros.ferro", 100)] {
        let path = dir.path().join(name);
        std::fs::write(&path, vec![0u8; bytes]).unwrap();
        assert!(matches!(admin::info(&path), Err(Error::NotFerro(_))));
        assert!(matches!(
            admin::compact(&path, Keep::All),
            Err(Error::NotFerro(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), vec![0u8; bytes]);
        assert!(!dir.path().join(format!("{name}.compacting")).exists());
    }
}

#[test]
fn read_only_handle_follows_a_replaced_file_on_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.ferro");
    build(&path, 5);
    let reader = ferro::Options::new()
        .read_only(true)
        .open::<Root>(&path)
        .unwrap();
    let held = reader.read();
    admin::compact(&path, Keep::Last(1)).unwrap();
    assert_eq!(reader.log().len(), 5);
    assert!(reader.refresh().unwrap());
    assert_eq!(reader.log().len(), 1);
    assert_eq!(reader.read().len(), 5);
    assert_eq!(held.len(), 5);
    assert!(!reader.refresh().unwrap());
}

type Heads = Vec<(String, u64, Vec<(u64, String)>)>;

fn branches_of(db: &Db<Root>) -> Heads {
    db.branches()
        .into_iter()
        .map(|name| {
            let snapshot = db.branch(&name).unwrap().read();
            let values = snapshot.iter().map(|(k, v)| (k, (*v).clone())).collect();
            (name, snapshot.version(), values)
        })
        .collect()
}

fn merge_ours(db: &Db<Root>, other: &Db<Root>) {
    db.merge_with(other, |ours, base, theirs| {
        let applied = ours.merge(base, theirs, |conflict| conflict.keep_ours());
        ("merge", applied)
    })
    .unwrap();
}

#[test]
fn a_compacted_file_opens_for_shared_writers_and_read_only_handles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.ferro");
    build(&path, 200);
    let want = {
        let db = Db::<Root>::open(&path).unwrap();
        let side = db.fork("side").unwrap();
        for i in 0..50 {
            side.update(|root| {
                root.insert(i, format!("side {i}"));
            })
            .unwrap();
        }
        merge_ours(&db, &side);
        merge_ours(&side, &db);
        branches_of(&db)
    };
    admin::compact(&path, Keep::Last(3)).unwrap();
    admin::verify(&path).unwrap();
    let reader = ferro::Options::new()
        .read_only(true)
        .open::<Root>(&path)
        .unwrap();
    assert_eq!(branches_of(&reader), want);
    reader.verify_typed().unwrap();
    let shared = || {
        ferro::Options::new()
            .shared(true)
            .open::<Root>(&path)
            .unwrap()
    };
    let one = shared();
    assert_eq!(branches_of(&one), want);
    one.verify_typed().unwrap();
    one.update(|root| {
        root.insert(1000, "shared".into());
    })
    .unwrap();
    merge_ours(&one.branch("side").unwrap(), &one);
    let want = branches_of(&one);
    one.compact(Keep::Last(2)).unwrap();
    one.verify_typed().unwrap();
    let other = shared();
    assert_eq!(branches_of(&other), want);
    assert!(reader.refresh().unwrap());
    assert_eq!(branches_of(&reader), want);
    drop((one, other));
    let again = ferro::Options::new()
        .read_only(true)
        .open::<Root>(&path)
        .unwrap();
    assert_eq!(branches_of(&again), want);
    again.verify_typed().unwrap();
}

#[test]
fn nested_collections_copy_with_their_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.ferro");
    {
        let db = Db::<Map<u64, Map<u64, Vec<u8>>>>::open(&path).unwrap();
        db.update(|root| {
            let inner = root.get_or_insert_with(1, Map::new);
            inner.insert(1, vec![7u8; 4096]);
            inner.insert(2, vec![8u8; 10]);
        })
        .unwrap();
    }
    admin::compact(&path, Keep::All).unwrap();
    let db = Db::<Map<u64, Map<u64, Vec<u8>>>>::open(&path).unwrap();
    let snapshot = db.read();
    let inner = snapshot.get(&1).unwrap();
    assert_eq!(inner.get(&1).unwrap().len(), 4096);
    assert_eq!(inner.get(&2).unwrap().len(), 10);
}

fn build_branched(path: &Path, commits: u64) {
    let db = Db::<Root>::open(path).unwrap();
    let mut side = None;
    for i in 0..commits {
        db.commit(format!("c{i}"), |root| {
            root.insert(i % 50, format!("{i}").repeat(100));
        })
        .unwrap();
        if i == commits / 4 {
            side = Some(db.fork("side").unwrap());
        }
        if let Some(side) = side.as_ref().filter(|_| i % 3 == 0) {
            side.commit(format!("s{i}"), |root| {
                root.insert(1_000 + i, format!("side {i}"));
            })
            .unwrap();
        }
        if i == commits / 2 {
            let side = side.as_ref().unwrap();
            db.merge_with(side, |root, base, theirs| {
                let applied = root.merge(base, theirs, |conflict| conflict.keep_ours());
                ("merge side", applied)
            })
            .unwrap();
        }
    }
}

fn records(path: &Path, mut each: impl FnMut(Ptr, &Object, bool)) {
    let options = StoreOptions {
        read_only: true,
        ..StoreOptions::default()
    };
    let (store, head) = Store::open(path, options).unwrap();
    let head = head.unwrap();
    let mut seen = HashSet::new();
    let mut stack = vec![head];
    while let Some(ptr) = stack.pop() {
        if ptr.offset == 0 || !seen.insert(ptr) {
            continue;
        }
        let object = store.read_object(ptr).unwrap();
        each(ptr, &object, ptr == head);
        stack.extend(object.links.iter().copied());
    }
    store.close();
}

#[test]
fn layout_lists_exactly_the_records_a_compaction_copies() {
    let dir = tempfile::tempdir().unwrap();
    for commits in [20u64, 200] {
        let path = dir.path().join(format!("{commits}.ferro"));
        build_branched(&path, commits);
        for keep in [
            Keep::All,
            Keep::Last(1),
            Keep::Last(5),
            Keep::Last(100),
            Keep::Since(u64::MAX),
        ] {
            let layout = admin::layout(&path, keep).unwrap();
            let copy = dir.path().join("copy.ferro");
            std::fs::copy(&path, &copy).unwrap();
            let compacted = admin::compact(&copy, keep).unwrap();
            let verified = admin::verify(&copy).unwrap();
            assert_eq!(verified.objects, compacted.objects, "{commits} {keep:?}");
            let mut source: HashSet<Hash> = HashSet::new();
            records(&path, |ptr, _, _| {
                source.insert(ptr.hash);
            });
            let mut copied: HashSet<Hash> = HashSet::new();
            let (mut fresh, mut fresh_bytes) = (0, 0);
            records(&copy, |ptr, object, head| {
                if head || !source.contains(&ptr.hash) {
                    fresh += 1;
                    fresh_bytes += record_len(ptr.offset, object) as u64;
                } else {
                    copied.insert(ptr.hash);
                }
            });
            assert_eq!(fresh, 2, "a branch table of one node under its head");
            let mut kept = Vec::new();
            records(&path, |ptr, object, _| {
                if copied.contains(&ptr.hash) {
                    kept.push((ptr.offset, record_len(ptr.offset, object) as u32));
                }
            });
            kept.sort_unstable();
            assert_eq!(layout.extents, kept, "{commits} commits, {keep:?}");
            assert_eq!(
                layout.extents.len() as u64 + fresh,
                compacted.objects,
                "{commits} commits, {keep:?}"
            );
            let bytes: u64 = layout
                .extents
                .iter()
                .map(|extent| u64::from(extent.1))
                .sum();
            let copied_bytes = verified.bytes - fresh_bytes;
            assert!(
                copied_bytes.abs_diff(bytes) * 100 <= bytes,
                "{commits} commits, {keep:?}: {bytes} bytes here, {copied_bytes} in the copy"
            );
        }
    }
}
