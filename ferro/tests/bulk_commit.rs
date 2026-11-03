mod common;

use std::collections::BTreeMap;

use common::roomy;
use ferro::codec::{Decoder, Encoder};
use ferro::{Data, Error, Hash, Map, hash_of};

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn hex(hash: Hash) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn value(i: u64) -> String {
    if i.is_multiple_of(97) {
        "x".repeat(700 + i as usize % 50)
    } else {
        format!("value-{i}")
    }
}

#[test]
fn large_flushes_keep_the_hashes_of_the_sequential_build() {
    let plain: Map<u64, String> = (0..50_000u64).map(|i| (mix(i), format!("value-{i}"))).collect();
    assert_eq!(hex(plain.root_hash().unwrap()), "89e2a9af3d4f46b37eadc6b97d0180ab10adb2b0");
    let mut spilled = Map::new();
    for i in 0..50_000u64 {
        spilled.insert(mix(i), value(i));
    }
    assert_eq!(hex(spilled.root_hash().unwrap()), "cd8c5f4b224b31d751409f40923d01c13d1161f1");
    let mut nested: Map<u64, Map<u64, u64>> = Map::new();
    for i in 0..6_000u64 {
        nested.insert(mix(i), (0..i % 5).map(|j| (j, i * j)).collect());
    }
    assert_eq!(hex(nested.root_hash().unwrap()), "313fb44173ed046971e3fb69e847a45b5dccc9c6");
    let dir = tempfile::tempdir().unwrap();
    let db = roomy().open::<Map<u64, String>>(dir.path().join("pins.ferro")).unwrap();
    db.update(|root| *root = spilled.clone()).unwrap();
    db.update(|root| {
        for i in 0..20_000u64 {
            root.insert(mix(i * 3), format!("again-{i}"));
        }
        for i in 0..3_000u64 {
            root.remove(&mix(i * 5 + 1));
        }
    })
    .unwrap();
    assert_eq!(hex(hash_of(&*db.read())), "1139214c59d0ddcd434fc84be10c8e1f4ecc18ea");
    assert_eq!(hex(db.read().root_hash().unwrap()), "8b8911642884ac9a14e77e496581ab49cdc6830f");
    let nested_db = roomy()
        .open::<Map<u64, Map<u64, u64>>>(dir.path().join("nested.ferro"))
        .unwrap();
    nested_db.update(|root| *root = nested.clone()).unwrap();
    assert_eq!(nested_db.read().root_hash(), nested.root_hash());
    nested_db.verify().unwrap();
}

#[test]
fn a_large_commit_reads_back_from_memory_and_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.ferro");
    let model: BTreeMap<u64, String> = (0..60_000u64).map(|i| (mix(i), value(i))).collect();
    {
        let db = roomy().open::<Map<u64, String>>(&path).unwrap();
        db.update(|root| {
            for i in 0..60_000u64 {
                root.insert(mix(i), value(i));
            }
        })
        .unwrap();
        let snapshot = db.read();
        for (key, value) in &model {
            assert_eq!(snapshot.get(key).as_deref(), Some(value));
        }
        assert!(snapshot.iter().map(|(key, value)| (key, (*value).clone())).eq(model.clone()));
        db.verify().unwrap();
    }
    let db = roomy().open::<Map<u64, String>>(&path).unwrap();
    let snapshot = db.read();
    assert!(snapshot.iter().map(|(key, value)| (key, (*value).clone())).eq(model.clone()));
    assert_eq!(snapshot.get(&mix(97)).as_deref(), model.get(&mix(97)));
}

#[derive(Clone, Debug, PartialEq)]
struct Picky(u8);

impl Data for Picky {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.u8(self.0);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        match r.u8()? {
            255 => Err(Error::Decode("picky refuses 255".into())),
            byte => Ok(Picky(byte)),
        }
    }
}

#[test]
fn a_large_commit_refuses_a_value_that_does_not_decode() {
    let dir = tempfile::tempdir().unwrap();
    let db = roomy().open::<Map<u64, Picky>>(dir.path().join("picky.ferro")).unwrap();
    db.update(|root| root.insert(1, Picky(1))).unwrap();
    let refused = db.try_update(|root| {
        for i in 0..40_000u64 {
            root.insert(mix(i), Picky(if i == 31_337 { 255 } else { i as u8 % 255 }));
        }
        Ok::<_, Error>(())
    });
    assert!(matches!(refused, Err(Error::Decode(_))), "{refused:?}");
    assert_eq!(db.read().len(), 1);
    db.update(|root| {
        for i in 0..40_000u64 {
            root.insert(mix(i), Picky(i as u8 % 255));
        }
    })
    .unwrap();
    assert_eq!(db.read().len(), 40_001);
    db.verify().unwrap();
}
