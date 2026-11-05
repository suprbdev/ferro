use ferro::admin::{self, Keep};
use ferro::{Db, Error, Map};

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
