use ferro::codec::{Decoder, Encoder};
use ferro::{Change, Data, Db, Map, Set};

#[derive(Clone, Debug, Default, PartialEq)]
struct Root {
    counter: u64,
    names: Map<u64, String>,
    tags: Set<String>,
}

impl Data for Root {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.counter.encode(w)?;
        self.names.encode(w)?;
        self.tags.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Root {
            counter: u64::decode(r)?,
            names: Map::decode(r)?,
            tags: Set::decode(r)?,
        })
    }
}

#[test]
fn round_trip_history_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.ferro");
    {
        let db = Db::<Root>::open(&path).unwrap();
        for i in 0..2000u64 {
            db.commit(format!("add {i}"), |root| {
                root.counter += 1;
                root.names.insert(i, format!("name-{i}"));
                if i % 7 == 0 {
                    root.tags.insert(format!("tag-{i}"));
                }
            })
            .unwrap();
        }
        assert_eq!(db.version(), 2000);
    }
    let db = Db::<Root>::open(&path).unwrap();
    let latest = db.read();
    assert_eq!(latest.counter, 2000);
    assert_eq!(latest.names.len(), 2000);
    assert_eq!(*latest.names.get(&1234).unwrap(), "name-1234");
    let old = db.at(10).unwrap();
    assert_eq!(old.counter, 10);
    assert!(old.names.get(&10).is_none());
    let changes: Vec<_> = old.names.diff(&latest.names).collect();
    assert_eq!(changes.len(), 1990);
    assert!(matches!(changes[0], Change::Added(10, _)));
    let report = db.verify().unwrap();
    assert!(report.objects > 0);
}

#[test]
fn nested_maps_are_clean_after_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::<Map<u64, Map<u64, u64>>>::open(dir.path().join("n.ferro")).unwrap();
    for i in 0..200u64 {
        db.update(|root| {
            root.get_or_insert_with(i % 3, Map::new).insert(i, i);
        })
        .unwrap();
        let latest = db.read();
        for key in 0..=(i % 3) {
            if let Some(inner) = latest.get(&key) {
                assert!(!inner.is_dirty());
            }
        }
    }
}
