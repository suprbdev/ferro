mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::sync::Arc;

use common::{Rng, fast};
use ferro::{
    Bytes, Change, Conflict, Data, Db, Map, Outcome, Record, Snapshot, Table, Val, hash_of,
};
use tempfile::TempDir;

const OTHER_NAN: u64 = 0x7ff8_0000_0000_0001;

#[derive(Data, Clone, Debug)]
struct Label {
    text: String,
    revision: u64,
}

impl PartialEq for Label {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Reading {
    #[key]
    id: u64,
    value: f64,
    label: Label,
    blends: u32,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Field {
    readings: Table<Reading>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Sides {
    floats: Map<u8, Map<u64, f64>>,
    labels: Map<u8, Map<u64, Label>>,
    tables: Map<u8, Table<Reading>>,
}

fn label(text: &str, revision: u64) -> Label {
    Label {
        text: text.to_string(),
        revision,
    }
}

fn reading(id: u64, value: f64) -> Reading {
    Reading {
        id,
        value,
        label: label("plain", 0),
        blends: 0,
    }
}

fn bits(map: &Map<u64, f64>) -> Vec<(u64, u64)> {
    map.iter()
        .map(|(key, value)| (key, value.to_bits()))
        .collect()
}

fn refuse<K, V>(_: Conflict<K, V>) -> Option<V> {
    panic!("no conflict expected")
}

fn stored<T: Data + Default>(dir: &TempDir, value: T) -> (Db<T>, Snapshot<T>) {
    let db: Db<T> = fast().open(dir.path().join("stored.ferro")).unwrap();
    db.update(|root| *root = value).unwrap();
    let read = db.read();
    (db, read)
}

fn float_sides(base: Map<u64, f64>, ours: Map<u64, f64>, theirs: Map<u64, f64>) -> Sides {
    Sides {
        floats: [(0, base), (1, ours), (2, theirs)].into_iter().collect(),
        ..Sides::default()
    }
}

fn both_ways(
    base: Map<u64, f64>,
    ours: Map<u64, f64>,
    theirs: Map<u64, f64>,
    check: impl Fn(Map<u64, f64>, Map<u64, f64>, Map<u64, f64>),
) {
    let dir = tempfile::tempdir().unwrap();
    let (_db, read) = stored(
        &dir,
        float_sides(base.clone(), ours.clone(), theirs.clone()),
    );
    check(base, ours, theirs);
    let side = |at: u8| (*read.floats.get(&at).unwrap()).clone();
    check(side(0), side(1), side(2));
}

fn neighbours() -> Map<u64, f64> {
    (0..64).map(|key| (key, key as f64 + 0.5)).collect()
}

#[test]
fn an_unchanged_nan_is_neither_a_change_nor_a_conflict() {
    let mut base = neighbours();
    base.insert(10, f64::NAN);
    base.insert(11, f64::from_bits(OTHER_NAN));
    let mut ours = base.clone();
    ours.insert(12, -1.0);
    let mut theirs = base.clone();
    theirs.insert(9, -2.0);
    both_ways(base, ours, theirs, |base, mut ours, theirs| {
        let changes: Vec<u64> = base.diff(&theirs).map(|change| *change.key()).collect();
        assert_eq!(changes, vec![9]);
        assert_eq!(ours.merge(&base, &theirs, refuse), 1);
        assert_eq!(ours.get(&10).unwrap().to_bits(), f64::NAN.to_bits());
        assert_eq!(ours.get(&11).unwrap().to_bits(), OTHER_NAN);
        assert_eq!(*ours.get(&9).unwrap(), -2.0);
        assert_eq!(*ours.get(&12).unwrap(), -1.0);
    });
}

#[test]
fn a_nan_with_another_payload_is_a_change() {
    let mut base = neighbours();
    base.insert(10, f64::NAN);
    let mut theirs = base.clone();
    theirs.insert(10, f64::from_bits(OTHER_NAN));
    both_ways(base.clone(), base, theirs, |base, mut ours, theirs| {
        let changes: Vec<u64> = base.diff(&theirs).map(|change| *change.key()).collect();
        assert_eq!(changes, vec![10]);
        assert_eq!(ours.merge(&base, &theirs, refuse), 1);
        assert_eq!(bits(&ours), bits(&theirs));
    });
}

#[test]
fn a_change_of_sign_of_zero_is_a_change() {
    let mut base = neighbours();
    base.insert(20, 0.0);
    let mut theirs = base.clone();
    theirs.insert(20, -0.0);
    both_ways(base.clone(), base, theirs, |base, mut ours, theirs| {
        let changes: Vec<Change<u64, f64>> = base.diff(&theirs).collect();
        assert_eq!(changes.len(), 1);
        assert_eq!(*changes[0].key(), 20);
        assert_eq!(changes[0].after().unwrap().to_bits(), (-0.0f64).to_bits());
        assert_eq!(ours.merge(&base, &theirs, refuse), 1);
        assert_eq!(bits(&ours), bits(&theirs));
        assert_ne!(ours.root_hash(), base.root_hash());
    });
}

#[test]
fn zeros_of_opposite_sign_on_the_two_sides_conflict() {
    let mut base = neighbours();
    base.insert(20, 1.0);
    let mut ours = base.clone();
    ours.insert(20, -0.0);
    let mut theirs = base.clone();
    theirs.insert(20, 0.0);
    both_ways(base, ours, theirs, |base, mut ours, theirs| {
        let mut asked = Vec::new();
        let written = ours.merge(&base, &theirs, |conflict| {
            asked.push((
                conflict.key,
                conflict.ours.as_ref().map(|value| value.to_bits()),
                conflict.theirs.as_ref().map(|value| value.to_bits()),
            ));
            conflict.theirs.map(Val::into_owned)
        });
        assert_eq!(written, 1);
        assert_eq!(
            asked,
            vec![(20, Some((-0.0f64).to_bits()), Some(0.0f64.to_bits()))]
        );
        assert_eq!(ours.get(&20).unwrap().to_bits(), 0.0f64.to_bits());
    });
}

#[test]
fn the_same_change_of_sign_on_both_sides_is_left_alone() {
    let mut base = neighbours();
    base.insert(20, 0.0);
    let mut ours = base.clone();
    ours.insert(20, -0.0);
    let theirs = ours.clone();
    both_ways(base, ours, theirs, |base, mut ours, theirs| {
        assert_eq!(ours.merge(&base, &theirs, refuse), 0);
        assert_eq!(ours.get(&20).unwrap().to_bits(), (-0.0f64).to_bits());
    });
}

#[test]
fn a_change_that_partial_eq_ignores_is_merged_and_conflicts() {
    let base: Map<u64, Label> = (0..40).map(|key| (key, label("note", key))).collect();
    let mut theirs = base.clone();
    theirs.insert(7, label("note", 100));
    theirs.insert(8, label("note", 200));
    let mut ours = base.clone();
    ours.insert(8, label("note", 300));
    let dir = tempfile::tempdir().unwrap();
    let (_db, read) = stored(
        &dir,
        Sides {
            labels: [(0, base.clone()), (1, ours.clone()), (2, theirs.clone())]
                .into_iter()
                .collect(),
            ..Sides::default()
        },
    );
    let side = |at: u8| (*read.labels.get(&at).unwrap()).clone();
    for [base, mut ours, theirs] in [[base, ours, theirs], [side(0), side(1), side(2)]] {
        let changes: Vec<u64> = base.diff(&theirs).map(|change| *change.key()).collect();
        assert_eq!(changes, vec![7, 8]);
        let mut asked = Vec::new();
        let written = ours.merge(&base, &theirs, |conflict| {
            asked.push(conflict.key);
            let ours = conflict.ours.unwrap();
            let theirs = conflict.theirs.unwrap();
            Some(label("note", ours.revision.max(theirs.revision)))
        });
        assert_eq!(written, 2);
        assert_eq!(asked, vec![8]);
        assert_eq!(ours.get(&7).unwrap().revision, 100);
        assert_eq!(ours.get(&8).unwrap().revision, 300);
    }
}

fn readings(rows: impl IntoIterator<Item = Reading>) -> Table<Reading> {
    let mut table: Table<Reading> = Table::new();
    for row in rows {
        table.insert(row);
    }
    table
}

fn row_bits(table: &Table<Reading>) -> Vec<(u64, u64, String, u64, u32)> {
    table
        .iter()
        .map(|row| {
            (
                row.id,
                row.value.to_bits(),
                row.label.text.clone(),
                row.label.revision,
                row.blends,
            )
        })
        .collect()
}

#[test]
fn table_merges_compare_rows_as_stored() {
    let base = readings((0..40).map(|id| match id % 4 {
        0 => reading(id, f64::NAN),
        1 => reading(id, 0.0),
        _ => reading(id, id as f64),
    }));
    let mut ours = base.clone();
    ours.insert(reading(2, -2.0));
    let mut theirs = base.clone();
    theirs.insert(reading(1, -0.0));
    theirs.update(&3, |row| row.label.revision = 9);
    theirs.insert(reading(6, -6.0));
    let dir = tempfile::tempdir().unwrap();
    let (_db, read) = stored(
        &dir,
        Sides {
            tables: [(0, base.clone()), (1, ours.clone()), (2, theirs.clone())]
                .into_iter()
                .collect(),
            ..Sides::default()
        },
    );
    let side = |at: u8| (*read.tables.get(&at).unwrap()).clone();
    for [base, ours, theirs] in [[base, ours, theirs], [side(0), side(1), side(2)]] {
        let changes: Vec<u64> = base.diff(&theirs).map(|change| *change.key()).collect();
        assert_eq!(changes, vec![1, 3, 6]);
        let mut merged = ours.clone();
        assert_eq!(merged.merge(&base, &theirs, refuse), 3);
        let mut tried = ours.clone();
        let written = tried.try_merge(&base, &theirs, |_| -> Result<Option<Reading>, Infallible> {
            panic!("no conflict expected")
        });
        assert_eq!(written, Ok(3));
        let mut expected = theirs.clone();
        expected.insert(reading(2, -2.0));
        assert_eq!(row_bits(&merged), row_bits(&expected));
        assert_eq!(row_bits(&tried), row_bits(&expected));
        assert_eq!(merged.root_hash(), expected.root_hash());
    }
}

fn blend(root: &mut Field, base: &Snapshot<Field>, theirs: &Snapshot<Field>) -> (String, usize) {
    let written = root
        .readings
        .merge(&base.readings, &theirs.readings, |conflict| {
            match (conflict.ours, conflict.theirs) {
                (Some(ours), Some(theirs)) => {
                    let pick = if ours.value.to_bits() >= theirs.value.to_bits() {
                        ours.value
                    } else {
                        theirs.value
                    };
                    let text = (&ours.label.text).max(&theirs.label.text).clone();
                    let revision = ours.label.revision.max(theirs.label.revision);
                    Some(Reading {
                        id: ours.id,
                        value: pick,
                        label: Label { text, revision },
                        blends: ours.blends.max(theirs.blends) + 1,
                    })
                }
                (ours, theirs) => ours.or(theirs).map(Val::into_owned),
            }
        });
    ("blend".to_string(), written)
}

fn devices(dir: &TempDir, count: usize) -> Vec<Db<Field>> {
    let devices: Vec<Db<Field>> = (0..count)
        .map(|at| {
            fast()
                .open(dir.path().join(format!("d{at}.ferro")))
                .unwrap()
        })
        .collect();
    devices[0]
        .update(|field| {
            for id in 0..48 {
                field.readings.insert(match id % 4 {
                    0 => reading(id, f64::NAN),
                    1 => reading(id, 0.0),
                    _ => reading(id, id as f64),
                });
            }
        })
        .unwrap();
    for to in 1..count {
        let pulled = devices[to].pull(&devices[0], "d0", blend).unwrap();
        assert_eq!(pulled.outcome, Outcome::FastForward);
    }
    devices
}

fn exchange(devices: &[Db<Field>], from: usize, to: usize) -> Outcome {
    devices[to]
        .pull(&devices[from], &format!("d{from}"), blend)
        .unwrap()
        .outcome
}

fn settle(devices: &[Db<Field>]) {
    let mut quiet = false;
    for _ in 0..6 {
        let mut moved = false;
        for to in 0..devices.len() {
            for from in 0..devices.len() {
                if from != to {
                    let before = devices[to].log_bytes();
                    let outcome = exchange(devices, from, to);
                    moved |= outcome != Outcome::UpToDate || devices[to].log_bytes() != before;
                }
            }
        }
        if !moved {
            quiet = true;
            break;
        }
    }
    assert!(quiet, "the devices kept merging");
    let first = devices[0].read();
    for device in devices {
        let read = device.read();
        assert_eq!(row_bits(&read.readings), row_bits(&first.readings));
        assert_eq!(read.readings.root_hash(), first.readings.root_hash());
        device.verify().unwrap();
    }
    for row in first.readings.iter().filter(|row| row.id % 4 == 0) {
        assert_eq!(row.value.to_bits(), f64::NAN.to_bits());
        assert_eq!(
            row.blends, 0,
            "row {} blended though no side changed it",
            row.id
        );
    }
}

#[test]
fn two_devices_with_nan_rows_converge_under_a_blending_merge() {
    for first in [0, 1] {
        let dir = tempfile::tempdir().unwrap();
        let devices = devices(&dir, 2);
        devices[0]
            .update(|field| {
                field.readings.insert(reading(1, -0.0));
                field.readings.update(&2, |row| row.label.revision = 5);
                field.readings.insert(reading(3, 30.0));
            })
            .unwrap();
        devices[1]
            .update(|field| {
                field.readings.insert(reading(3, 31.0));
                field.readings.insert(reading(6, 60.0));
            })
            .unwrap();
        assert_eq!(exchange(&devices, 1 - first, first), Outcome::Merged);
        settle(&devices);
        let read = devices[0].read();
        let row = |id: u64| read.readings.get(&id).unwrap();
        assert_eq!(row(1).value.to_bits(), (-0.0f64).to_bits());
        assert_eq!(row(2).label.revision, 5);
        assert_eq!((row(3).value, row(3).blends), (31.0, 1));
        assert_eq!(row(6).value, 60.0);
        assert_eq!(
            read.readings.iter().map(|row| row.blends).sum::<u32>(),
            1,
            "only the row both sides changed blends"
        );
    }
}

fn edit(rng: &mut Rng, field: &mut Field) {
    let id = 1 + 4 * rng.below(12) + rng.below(3);
    match rng.below(4) {
        0 => {
            field.readings.update(&id, |row| row.value = -row.value);
        }
        1 => {
            field.readings.update(&id, |row| row.label.revision += 1);
        }
        2 => {
            field.readings.update(&id, |row| {
                row.value = if rng.chance(50) { 0.0 } else { -0.0 }
            });
        }
        _ => {
            field
                .readings
                .update(&id, |row| row.value = rng.below(1000) as f64 / 8.0);
        }
    }
}

#[test]
fn three_devices_with_nan_rows_converge_under_a_blending_merge() {
    let mut merged = 0;
    for seed in 0..common::cases(8) as u64 {
        let dir = tempfile::tempdir().unwrap();
        let devices = devices(&dir, 3);
        let mut rng = Rng::new(seed + 700);
        for _ in 0..30 {
            let device = rng.below(3) as usize;
            if rng.chance(60) {
                devices[device]
                    .update(|field| {
                        for _ in 0..rng.below(3) + 1 {
                            edit(&mut rng, field);
                        }
                    })
                    .unwrap();
            } else {
                let other = (device + 1 + rng.below(2) as usize) % 3;
                merged += usize::from(exchange(&devices, other, device) == Outcome::Merged);
            }
        }
        settle(&devices);
    }
    assert!(merged > 0);
}

#[derive(Data, Clone, Debug)]
enum Shape {
    Dot,
    Circle(f64),
    Named {
        name: String,
        #[ferro(skip)]
        cache: u32,
        sides: Option<u8>,
    },
}

#[derive(Data, Clone, Debug)]
#[ferro(transparent)]
struct Meters(f64);

#[derive(Data, Clone, Debug)]
struct Mixed {
    shape: Shape,
    pair: (i32, f32),
    list: Vec<f64>,
    tags: BTreeMap<String, Option<f64>>,
    set: BTreeSet<i64>,
    boxed: Box<Label>,
    shared: Arc<String>,
    bytes: Bytes,
    #[ferro(skip)]
    scratch: u64,
    #[ferro(default)]
    extra: u16,
    nested: Map<u64, Meters>,
    array: [u8; 2],
    big: u128,
    signed: i128,
    letter: char,
    flag: bool,
    small: i8,
}

fn float(rng: &mut Rng) -> f64 {
    match rng.below(5) {
        0 => 0.0,
        1 => -0.0,
        2 => f64::NAN,
        3 => f64::from_bits(OTHER_NAN),
        _ => 1.5,
    }
}

fn word(rng: &mut Rng) -> String {
    ["", "a", "b"][rng.below(3) as usize].to_string()
}

fn shape(rng: &mut Rng) -> Shape {
    match rng.below(3) {
        0 => Shape::Dot,
        1 => Shape::Circle(float(rng)),
        _ => Shape::Named {
            name: word(rng),
            cache: rng.below(3) as u32,
            sides: [None, Some(0), Some(3)][rng.below(3) as usize],
        },
    }
}

fn redraw(rng: &mut Rng, value: &mut Mixed, field: u64) {
    match field {
        0 => value.shape = shape(rng),
        1 => value.pair = (rng.below(2) as i32 - 1, float(rng) as f32),
        2 => value.list = (0..rng.below(3)).map(|_| float(rng)).collect(),
        3 => {
            value.tags = (0..rng.below(3))
                .map(|_| (word(rng), rng.chance(50).then(|| float(rng))))
                .collect()
        }
        4 => value.set = (0..rng.below(3)).map(|_| rng.below(3) as i64 - 1).collect(),
        5 => *value.boxed = label(&word(rng), rng.below(2)),
        6 => value.shared = Arc::new(word(rng)),
        7 => value.bytes = Bytes::from(word(rng).into_bytes()),
        8 => value.scratch = rng.next(),
        9 => value.extra = rng.below(2) as u16,
        10 => {
            value.nested = (0..rng.below(3))
                .map(|key| (key, Meters(float(rng))))
                .collect()
        }
        11 => value.array = [rng.below(2) as u8, rng.below(2) as u8],
        12 => value.big = u128::from(rng.below(2)) << 100,
        13 => value.signed = -i128::from(rng.below(2)) << 90,
        14 => value.letter = ['a', 'é'][rng.below(2) as usize],
        15 => value.flag = rng.chance(50),
        _ => value.small = rng.below(3) as i8 - 1,
    }
}

fn mixed(rng: &mut Rng) -> Mixed {
    let mut value = Mixed {
        shape: Shape::Dot,
        pair: (0, 0.0),
        list: Vec::new(),
        tags: BTreeMap::new(),
        set: BTreeSet::new(),
        boxed: Box::new(label("", 0)),
        shared: Arc::new(String::new()),
        bytes: Bytes::new(),
        scratch: 0,
        extra: 0,
        nested: Map::new(),
        array: [0, 0],
        big: 0,
        signed: 0,
        letter: 'a',
        flag: false,
        small: 0,
    };
    for field in 0..17 {
        redraw(rng, &mut value, field);
    }
    value
}

#[test]
fn same_encoding_agrees_with_the_bytes_a_value_encodes_to() {
    let mut rng = Rng::new(52);
    let (mut same, mut differ) = (0, 0);
    for _ in 0..common::cases(4000) {
        let a = mixed(&mut rng);
        let mut b = a.clone();
        for _ in 0..rng.below(3) {
            let field = rng.below(17);
            redraw(&mut rng, &mut b, field);
        }
        let encoded = hash_of(&a) == hash_of(&b);
        assert_eq!(a.same_encoding(&b), encoded, "{a:?} against {b:?}");
        assert_eq!(b.same_encoding(&a), encoded);
        assert!(a.same_encoding(&a.clone()));
        if encoded {
            same += 1;
        } else {
            differ += 1;
        }
    }
    assert!(
        same > 100 && differ > 100,
        "{same} same and {differ} differing"
    );
}

#[test]
fn tables_compare_by_what_they_store() {
    let rows: Vec<Reading> = (0..30)
        .map(|id| reading(id, if id % 3 == 0 { f64::NAN } else { id as f64 }))
        .collect();
    let forward = readings(rows.iter().cloned());
    let backward = readings(rows.iter().rev().cloned());
    assert!(forward.same_encoding(&backward));
    let mut signed = forward.clone();
    signed.insert(reading(1, -1.0));
    signed.insert(reading(1, 1.0));
    assert!(forward.same_encoding(&signed));
    signed.insert(reading(1, -0.0));
    assert!(!forward.same_encoding(&signed));
    assert_ne!(hash_of(&forward), hash_of(&signed));
}

#[test]
fn fields_that_are_not_stored_do_not_count() {
    let named = |cache| Shape::Named {
        name: "a".to_string(),
        cache,
        sides: None,
    };
    let (one, two) = (named(1), named(2));
    assert!(one.same_encoding(&two));
    assert!(matches!(
        (one, two),
        (Shape::Named { cache: 1, .. }, Shape::Named { cache: 2, .. })
    ));
}
