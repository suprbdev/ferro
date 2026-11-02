mod common;

use common::fast;
use ferro::admin::{self, Keep};
use ferro::{Db, Map};

type Root = Map<u64, String>;

fn value(key: u64) -> String {
    format!("customer-{key:08} lives at {} Example Street, Exampleville", key % 97).repeat(1 + (key % 5) as usize)
}

#[test]
fn compressible_values_round_trip_and_shrink_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ferro");
    let raw: u64 = (0..2000u64).map(|key| value(key).len() as u64).sum();
    {
        let db = fast().open::<Root>(&path).unwrap();
        db.update(|root| {
            for key in 0..2000 {
                root.insert(key, value(key));
            }
        })
        .unwrap();
        assert!(db.size_on_disk() < raw / 2, "{} vs {raw}", db.size_on_disk());
    }
    let check = |db: &Db<Root>| {
        let snapshot = db.read();
        for key in 0..2000 {
            assert_eq!(*snapshot.get(&key).unwrap(), value(key));
        }
        db.verify().unwrap();
    };
    check(&fast().cache_bytes(1).open::<Root>(&path).unwrap());
    admin::compact(&path, Keep::All).unwrap();
    check(&fast().cache_bytes(1).open::<Root>(&path).unwrap());
}

#[test]
fn incompressible_values_are_stored_raw() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ferro");
    let db = fast().cache_bytes(1).open::<Root>(&path).unwrap();
    db.update(|root| {
        for key in 0..300 {
            root.insert(key, common::text(key, 200));
        }
    })
    .unwrap();
    let snapshot = db.read();
    for key in 0..300 {
        assert_eq!(*snapshot.get(&key).unwrap(), common::text(key, 200));
    }
    db.verify().unwrap();
}
