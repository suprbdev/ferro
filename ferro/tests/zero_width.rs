mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::sync::Arc;

use common::{fast, level_of};
use ferro::codec::{Decoder, Encoder, HashSink, Object, try_hash_of};
use ferro::{Data, Db, Error, Map, Snapshot, Summary};

const LIMIT: usize = 1 << 24;

type Units = Map<u64, (Vec<()>, String)>;

#[derive(Data, Clone, Debug, PartialEq)]
struct Pair {
    left: Vec<()>,
    right: Vec<()>,
}

#[derive(Data, Clone, Debug, PartialEq)]
#[ferro(transparent)]
struct First(Vec<()>);

impl Summary<u64, Vec<()>> for First {
    fn of(_key: &u64, value: &Vec<()>) -> Self {
        First(value.clone())
    }

    fn combine(&self, _other: &Self) -> Self {
        self.clone()
    }
}

type Firsts = Map<u64, Vec<()>, First>;

fn keys_of_one_node(count: usize) -> Vec<u64> {
    (0..).filter(|key| level_of(key) == 0).take(count).collect()
}

fn units(count: usize) -> (Vec<()>, String) {
    (vec![(); count], String::new())
}

fn refuse(_: &mut Units, _: &Snapshot<Units>, _: &Snapshot<Units>) -> (String, ()) {
    panic!("no merge expected");
}

fn lengths(root: &Units, keys: &[u64]) -> Vec<Option<(usize, usize)>> {
    keys.iter()
        .map(|key| root.get(key).map(|value| (value.0.len(), value.1.len())))
        .collect()
}

fn hand_made(write: impl FnOnce(&mut Encoder<'_>)) -> Object {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    write(&mut encoder);
    encoder.into_object()
}

fn decodes<D: Data>(object: &Object) -> ferro::Result<D> {
    D::decode(&mut Decoder::new(&object.body, &object.links))
}

fn fits<D: Data + PartialEq + Debug>(value: D) {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder).unwrap();
    let object = encoder.into_object();
    assert_eq!(decodes::<D>(&object).unwrap(), value);
}

fn refused<D: Data>(value: D) {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    assert!(matches!(
        value.encode(&mut encoder),
        Err(Error::TooManyZeroWidth)
    ));
    assert!(matches!(try_hash_of(&value), Err(Error::TooManyZeroWidth)));
}

#[test]
fn a_map_node_refuses_inline_values_whose_zero_width_items_overflow_it_and_stays_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("units.ferro");
    let keys = keys_of_one_node(5);
    let db: Db<Units> = fast().open(&path).unwrap();
    for key in &keys[..2] {
        db.update(|root| {
            root.insert(*key, units(6_000_000));
        })
        .unwrap();
    }
    let spilled = (vec![(); LIMIT], "x".repeat(600));
    db.update(|root| {
        root.insert(keys[2], spilled.clone());
    })
    .unwrap();
    let too_many = db.update(|root| {
        root.insert(keys[3], units(LIMIT - 12_000_000 + 1));
    });
    assert!(
        matches!(too_many, Err(Error::TooManyZeroWidth)),
        "{too_many:?}"
    );
    db.update(|root| {
        root.insert(keys[3], units(LIMIT - 12_000_000));
    })
    .unwrap();
    let one_more = db.update(|root| {
        root.insert(keys[4], units(1));
    });
    assert!(
        matches!(one_more, Err(Error::TooManyZeroWidth)),
        "{one_more:?}"
    );
    db.update(|root| {
        root.insert(keys[4], units(0));
    })
    .unwrap();
    let mut expected = vec![
        Some((6_000_000, 0)),
        Some((6_000_000, 0)),
        Some((LIMIT, 600)),
        Some((LIMIT - 12_000_000, 0)),
        Some((0, 0)),
    ];
    assert_eq!(lengths(&db.read(), &keys), expected);
    db.verify_typed().unwrap();
    drop(db);

    let db: Db<Units> = fast().open(&path).unwrap();
    db.verify_typed().unwrap();
    assert_eq!(lengths(&db.read(), &keys), expected);
    db.update(|root| {
        root.insert(keys[0], units(5_000_000));
    })
    .unwrap();
    expected[0] = Some((5_000_000, 0));
    assert_eq!(lengths(&db.read(), &keys), expected);

    let copy: Db<Units> = fast().open(dir.path().join("copy.ferro")).unwrap();
    copy.pull(&db, "origin", refuse).unwrap();
    copy.verify_typed().unwrap();
    assert_eq!(lengths(&copy.read(), &keys), expected);
}

#[test]
fn a_root_value_with_more_zero_width_items_than_decode_reads_is_refused_when_it_encodes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("root.ferro");
    let db: Db<Vec<()>> = fast().open(&path).unwrap();
    db.update(|root| *root = vec![(); LIMIT]).unwrap();
    let over = vec![(); LIMIT + 1];
    assert!(matches!(try_hash_of(&over), Err(Error::TooManyZeroWidth)));
    let outcome = db.update(|root| *root = over.clone());
    assert!(
        matches!(outcome, Err(Error::TooManyZeroWidth)),
        "{outcome:?}"
    );
    assert_eq!(db.version(), 1);
    assert_eq!(db.read().len(), LIMIT);
    drop(db);
    let db: Db<Vec<()>> = fast().open(&path).unwrap();
    db.verify_typed().unwrap();
    assert_eq!(db.read().len(), LIMIT);
}

#[test]
fn nested_zero_width_items_count_together_when_encoded_as_when_decoded() {
    let half = LIMIT / 2;
    fits(vec![vec![(); half]; 2]);
    refused(vec![vec![(); half], vec![(); half + 1]]);
    fits(BTreeMap::from([(0u8, vec![(); half]), (1, vec![(); half])]));
    refused(BTreeMap::from([
        (0u8, vec![(); half]),
        (1, vec![(); half + 1]),
    ]));
    fits(BTreeSet::from([vec![(); half - 1], vec![(); half + 1]]));
    refused(BTreeSet::from([vec![(); half], vec![(); half + 1]]));
    fits((vec![(); half], vec![(); half]));
    refused((vec![(); half], vec![(); half + 1]));
    fits(Some(vec![(); LIMIT]));
    refused(Some(vec![(); LIMIT + 1]));
    fits([vec![(); half], vec![(); half]]);
    refused([vec![(); half], vec![(); half + 1]]);
    fits(Box::new(vec![(); LIMIT]));
    refused(Arc::new(vec![(); LIMIT + 1]));
    fits(Pair {
        left: vec![(); half],
        right: vec![(); half],
    });
    refused(Pair {
        left: vec![(); half],
        right: vec![(); half + 1],
    });
    fits([[(); (1 << 12) - 1]; 1 << 12]);
    refused([[(); 1 << 12]; 1 << 12]);
    fits(vec![((), ()); LIMIT]);
    refused(vec![Box::new(()); LIMIT + 1]);

    let nested = |last: usize| {
        hand_made(|w| {
            w.varint(2);
            w.varint(half as u64);
            w.varint(last as u64);
        })
    };
    assert_eq!(decodes::<Vec<Vec<()>>>(&nested(half)).unwrap().len(), 2);
    assert!(matches!(
        decodes::<Vec<Vec<()>>>(&nested(half + 1)),
        Err(Error::Decode(_))
    ));
    let pair = |right: usize| {
        hand_made(|w| {
            w.varint(half as u64);
            w.varint(right as u64);
        })
    };
    assert!(decodes::<(Vec<()>, Vec<()>)>(&pair(half)).is_ok());
    assert!(matches!(
        decodes::<(Vec<()>, Vec<()>)>(&pair(half + 1)),
        Err(Error::Decode(_))
    ));
}

#[test]
fn a_map_node_counts_the_zero_width_items_of_its_child_summaries_with_its_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("firsts.ferro");
    let top = (1u64 << 20..).find(|key| level_of(key) == 1).unwrap();
    let mut below = (0..top).rev().filter(|key| level_of(key) == 0);
    let (last, first) = (below.next().unwrap(), below.next().unwrap());
    let db: Db<Firsts> = fast().open(&path).unwrap();
    db.update(|root| {
        root.insert(top, vec![(); 10_000_000]);
        root.insert(last, Vec::new());
    })
    .unwrap();
    let outcome = db.update(|root| {
        root.insert(first, vec![(); 7_000_000]);
    });
    assert!(
        matches!(outcome, Err(Error::TooManyZeroWidth)),
        "{outcome:?}"
    );
    db.update(|root| {
        root.insert(first, vec![(); 6_000_000]);
    })
    .unwrap();
    db.verify_typed().unwrap();
    drop(db);
    let db: Db<Firsts> = fast().open(&path).unwrap();
    db.verify_typed().unwrap();
    let root = db.read();
    assert_eq!(root.get(&top).map(|value| value.len()), Some(10_000_000));
    assert_eq!(root.get(&first).map(|value| value.len()), Some(6_000_000));
    assert_eq!(root.summary().map(|first| first.0.len()), Some(6_000_000));
}

#[test]
fn a_commit_that_stages_a_large_map_counts_zero_width_items_by_node() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("staged.ferro");
    let start = (0u64..)
        .find(|key| (*key..key + 3).all(|key| level_of(&key) == 0))
        .unwrap();
    let run = [start, start + 1, start + 2];
    let fill = |sizes: [usize; 3]| {
        move |root: &mut Map<u64, Vec<()>>| {
            for key in 0..5000 {
                root.insert(key, Vec::new());
            }
            for (key, size) in run.iter().zip(sizes) {
                root.insert(*key, vec![(); size]);
            }
        }
    };
    let db: Db<Map<u64, Vec<()>>> = fast().open(&path).unwrap();
    let outcome = db.update(fill([6_000_000; 3]));
    assert!(
        matches!(outcome, Err(Error::TooManyZeroWidth)),
        "{outcome:?}"
    );
    db.update(fill([6_000_000, 6_000_000, 4_000_000])).unwrap();
    db.verify_typed().unwrap();
    drop(db);
    let db: Db<Map<u64, Vec<()>>> = fast().open(&path).unwrap();
    let root = db.read();
    assert_eq!(root.len(), 5000);
    let lengths: Vec<_> = run
        .iter()
        .map(|key| root.get(key).map(|value| value.len()))
        .collect();
    assert_eq!(lengths, [Some(6_000_000), Some(6_000_000), Some(4_000_000)]);
}
