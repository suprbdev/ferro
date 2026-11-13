mod common;

use std::fs;

use common::fast;
use ferro::admin::{self, Keep};
use ferro::store::{Store, StoreOptions};
use ferro::{Db, Error, Map, Reclaim, Snapshot};

type Root = Map<u64, String>;

fn value(key: u64) -> String {
    format!(
        "customer-{key:08} lives at {} Example Street, Exampleville",
        key % 97
    )
    .repeat(1 + (key % 5) as usize)
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
        assert!(db.log_bytes() < raw / 2, "{} vs {raw}", db.log_bytes());
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

fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    panic!("no merge expected")
}

#[test]
fn a_value_pulled_uncompressed_is_stored_compressed_and_a_power_cut_over_it_is_torn() {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Root> = fast().open(dir.path().join("a.ferro")).unwrap();
    let path = dir.path().join("b.ferro");
    let b: Db<Root> = fast().open(&path).unwrap();
    let len = 32 << 20;
    let mut value = vec![0u8; len];
    for at in (100..len).step_by(8192) {
        value[at] = 1;
    }
    let value = String::from_utf8(value).unwrap();
    a.commit("sparse", |root| {
        root.insert(1, value.clone());
    })
    .unwrap();
    let sent = a.log_bytes();
    let before = fs::read(&path).unwrap();
    b.pull(&a, "a", refuse).unwrap();
    assert!(
        b.log_bytes() < 2 * sent,
        "{} bytes pulled from a file of {sent}",
        b.log_bytes()
    );
    drop(b);
    let whole = fs::read(&path).unwrap();
    let mut image = whole.clone();
    image[..before.len()].copy_from_slice(&before);
    for sector in (before.len()..image.len()).step_by(512).skip(5).step_by(7) {
        image[sector..(sector + 512).min(whole.len())].fill(0);
    }
    fs::write(&path, &image).unwrap();
    let readonly = fast().read_only(true).open::<Root>(&path).unwrap();
    assert_eq!(readonly.version(), 0);
    drop(readonly);
    let torn = fast().open::<Root>(&path).unwrap();
    assert_eq!(torn.version(), 0);
    drop(torn);
    fs::write(&path, &whole).unwrap();
    let b = fast().cache_bytes(1).open::<Root>(&path).unwrap();
    assert_eq!(*b.read().get(&1).unwrap(), value);
    b.verify().unwrap();
}

fn varint_at(bytes: &[u8], at: &mut usize) -> u64 {
    let mut value = 0;
    let mut shift = 0;
    loop {
        let byte = bytes[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return value;
        }
    }
}

fn largest_record(path: &std::path::Path) -> u64 {
    let options = StoreOptions {
        read_only: true,
        ..StoreOptions::default()
    };
    let (store, head) = Store::open(path, options).unwrap();
    let mut largest = (0, 0);
    store
        .walk(head.unwrap(), |ptr, object| {
            if object.body.len() > largest.1 {
                largest = (ptr.offset, object.body.len());
            }
        })
        .unwrap();
    store.close();
    largest.0
}

fn inflate_raw_length(path: &std::path::Path, offset: u64, by: u64) {
    let mut bytes = fs::read(path).unwrap();
    let mut at = offset as usize;
    varint_at(&bytes, &mut at);
    let flagged = varint_at(&bytes, &mut at);
    assert_eq!(flagged & 1, 1, "the record at {offset} is not compressed");
    let start = at;
    let raw = varint_at(&bytes, &mut at);
    let len = at - start;
    let mut value = raw + by;
    for (index, byte) in bytes[start..at].iter_mut().enumerate() {
        let more = if index + 1 < len { 0x80 } else { 0 };
        *byte = (value & 0x7f) as u8 | more;
        value >>= 7;
    }
    assert_eq!(value, 0, "{raw} + {by} needs a longer varint");
    fs::write(path, bytes).unwrap();
}

#[test]
fn a_damaged_raw_length_fails_verify_a_read_and_a_cycle() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("a.ferro");
    let mut rng = common::Rng::new(5);
    let big: String = (0..40_000)
        .map(|_| format!("w{:02} ", rng.below(64)))
        .collect();
    {
        let db = fast().open::<Root>(&path).unwrap();
        db.update(|root| {
            root.insert(1, big.clone());
            for key in 10..3000 {
                root.insert(key, common::text(key, 300));
            }
        })
        .unwrap();
        db.update(|root| {
            root.insert(3, "more".into());
        })
        .unwrap();
    }
    let offset = largest_record(&path);
    inflate_raw_length(&path, offset, 100_000);
    let damaged = fs::read(&path).unwrap();
    let wrong = format!("compressed body at {offset} has the wrong length");
    for threads in [1, 4] {
        let db = fast()
            .cache_bytes(1)
            .reclaim(Reclaim::off())
            .copy_threads(threads)
            .open::<Root>(&path)
            .unwrap();
        match db.verify() {
            Err(Error::Corrupt(message)) => assert_eq!(message, wrong),
            other => panic!("verify passed a damaged raw length: {other:?}"),
        }
        match db.read().try_get(&1) {
            Err(Error::Corrupt(message)) => assert_eq!(message, wrong),
            other => panic!("a read passed a damaged raw length: {other:?}"),
        }
        assert_eq!(*db.read().get(&3).unwrap(), "more");
        match db.compact(Keep::All) {
            Err(Error::Corrupt(message)) => assert_eq!(message, wrong, "{threads} threads"),
            other => panic!("{threads} threads: a cycle kept a damaged raw length: {other:?}"),
        }
        drop(db);
        assert_eq!(fs::read(&path).unwrap(), damaged, "{threads} threads");
    }
    let outcome = admin::compact(&path, Keep::All);
    assert!(
        matches!(&outcome, Err(Error::Corrupt(message)) if *message == wrong),
        "{outcome:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), damaged);
}
