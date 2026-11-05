mod common;

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::{Bound, ControlFlow};

use common::{Outcome, Rng, fast, guarded, quiet_ferro_panics, text, uncached};
use ferro::{Bytes, Data, Map, Options, Record, Set, Table, TableData, Val};
use proptest::prelude::*;

trait TestKey: Data + Ord + Debug {
    fn make(seed: u32) -> Self;
}

impl TestKey for u32 {
    fn make(seed: u32) -> Self {
        seed
    }
}

impl TestKey for String {
    fn make(seed: u32) -> Self {
        format!("key-{seed:06}")
    }
}

impl TestKey for (u32, u32) {
    fn make(seed: u32) -> Self {
        (seed / 16, seed % 16)
    }
}

trait TestValue: Data + PartialEq + Debug {
    fn make(seed: u64, size: usize) -> Self;
}

impl TestValue for String {
    fn make(seed: u64, size: usize) -> Self {
        text(seed, size)
    }
}

impl TestValue for u64 {
    fn make(seed: u64, _size: usize) -> Self {
        seed
    }
}

impl TestValue for () {
    fn make(_seed: u64, _size: usize) {}
}

impl TestValue for Bytes {
    fn make(seed: u64, size: usize) -> Self {
        Bytes::from(text(seed, size).into_bytes())
    }
}

#[derive(Clone, Debug)]
enum Edit {
    Put(u32, u64, usize),
    Drop(u32),
}

#[derive(Clone, Copy, Debug)]
enum Move {
    Seek(u32),
    SeekBack(u32),
    Advance,
    Rewind,
}

#[derive(Clone, Debug)]
struct Case {
    first: Vec<Edit>,
    second: Vec<Edit>,
    ranges: Vec<(Bound<u32>, Bound<u32>)>,
    moves: Vec<Move>,
    wanted: Vec<u32>,
    front: usize,
    back: usize,
}

const KEYS: u32 = 6000;

fn size() -> impl Strategy<Value = usize> {
    prop_oneof![
        8 => 0usize..24,
        1 => 400usize..620,
        1 => 700usize..1500,
    ]
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        5 => (0..KEYS, any::<u64>(), size()).prop_map(|(key, seed, len)| Edit::Put(key, seed, len)),
        1 => (0..KEYS).prop_map(Edit::Drop),
    ]
}

fn bound() -> impl Strategy<Value = Bound<u32>> {
    prop_oneof![
        Just(Bound::Unbounded),
        (0..KEYS).prop_map(Bound::Included),
        (0..KEYS).prop_map(Bound::Excluded),
    ]
}

fn movement() -> impl Strategy<Value = Move> {
    prop_oneof![
        4 => (0..KEYS).prop_map(Move::Seek),
        2 => (0..KEYS).prop_map(Move::SeekBack),
        4 => Just(Move::Advance),
        1 => Just(Move::Rewind),
    ]
}

fn case() -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(edit(), 0..1500),
        prop::collection::vec(edit(), 0..60),
        prop::collection::vec((bound(), bound()), 1..8),
        prop::collection::vec(movement(), 1..200),
        prop::collection::vec(0..KEYS, 0..200),
        0usize..40,
        0usize..40,
    )
        .prop_map(|(first, second, ranges, moves, wanted, front, back)| Case {
            first,
            second,
            ranges,
            moves,
            wanted,
            front,
            back,
        })
}

fn valid<K: Ord>(low: &Bound<K>, high: &Bound<K>) -> bool {
    match (low, high) {
        (Bound::Included(a), Bound::Included(b)) => a <= b,
        (Bound::Included(a) | Bound::Excluded(a), Bound::Excluded(b)) => a < b,
        (Bound::Excluded(a), Bound::Included(b)) => a < b,
        _ => true,
    }
}

fn apply<K: TestKey, V: TestValue>(
    map: &mut Map<K, V>,
    model: &mut BTreeMap<K, V>,
    edits: &[Edit],
) {
    for edit in edits {
        match *edit {
            Edit::Put(key, seed, len) => {
                let key = K::make(key);
                map.insert(key.clone(), V::make(seed, len));
                model.insert(key, V::make(seed, len));
            }
            Edit::Drop(key) => {
                let key = K::make(key);
                map.remove(&key);
                model.remove(&key);
            }
        }
    }
}

fn entries<'a, K: 'a + Clone, V: 'a + Clone>(
    items: impl Iterator<Item = (&'a K, &'a V)>,
) -> Vec<(K, V)> {
    items
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn check_folds<K: TestKey, V: TestValue>(map: &Map<K, V>, model: &BTreeMap<K, V>, case: &Case) {
    let all = entries(model.iter());
    let visited = {
        let mut seen = Vec::new();
        map.for_each(|key, value| seen.push((key.clone(), value.clone())));
        seen
    };
    assert_eq!(visited, all);
    let mut seen = 0;
    let stop = (case.front + 1).min(all.len());
    let outcome = map.try_for_each(|_, _| {
        seen += 1;
        if seen == stop {
            ControlFlow::Break(seen)
        } else {
            ControlFlow::Continue(())
        }
    });
    assert_eq!(seen, stop);
    assert_eq!(outcome == ControlFlow::Break(stop), stop > 0);
    let front = case.front.min(all.len());
    let back = case.back.min(all.len() - front);
    let middle = &all[front..all.len() - back];

    let mut iter = map.iter();
    for (key, value) in &all[..front] {
        let (found, held) = iter.next().unwrap();
        assert_eq!((&found, &*held), (key, value));
    }
    for (key, value) in all[all.len() - back..].iter().rev() {
        let (found, held) = iter.next_back().unwrap();
        assert_eq!((&found, &*held), (key, value));
    }
    assert_eq!(iter.len(), middle.len());
    let folded = iter.fold(Vec::new(), |mut seen, (key, value)| {
        seen.push((key, (*value).clone()));
        seen
    });
    assert_eq!(folded, middle);

    let mut keys = map.keys();
    for _ in 0..front {
        keys.next();
    }
    for _ in 0..back {
        keys.next_back();
    }
    let folded = keys.fold(Vec::new(), |mut seen, key| {
        seen.push(key);
        seen
    });
    assert_eq!(
        folded,
        middle
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<K>>()
    );

    let mut values = map.values();
    for _ in 0..front {
        values.next();
    }
    for _ in 0..back {
        values.next_back();
    }
    let folded = values.fold(Vec::new(), |mut seen, value| {
        seen.push((*value).clone());
        seen
    });
    assert_eq!(
        folded,
        middle
            .iter()
            .map(|(_, value)| value.clone())
            .collect::<Vec<V>>()
    );

    assert_eq!(map.iter().count(), all.len());
    assert_eq!(map.keys().filter(|_| true).count(), all.len());

    let mut visited = Vec::new();
    map.iter()
        .visit(|key, value| visited.push((key.clone(), value.clone())));
    assert_eq!(visited, all);
    let mut visited = Vec::new();
    map.values().visit(|value| visited.push(value.clone()));
    assert!(visited.iter().eq(all.iter().map(|(_, value)| value)));

    let mut iter = map.iter();
    for (key, value) in all[all.len() - back..].iter().rev() {
        let (found, held) = iter.next_back().unwrap();
        assert_eq!((&found, &*held), (key, value));
    }
    for (key, value) in &all[..front] {
        assert_eq!(iter.next_ref(), Some((key, value)));
    }
    assert_eq!(iter.len(), middle.len());
    let mut visited = Vec::new();
    iter.visit(|key, value| visited.push((key.clone(), value.clone())));
    assert_eq!(visited, middle);

    let mut values = map.values();
    for _ in 0..front {
        values.next();
    }
    let mut lent = Vec::new();
    while let Some(value) = values.next_ref() {
        lent.push(value.clone());
    }
    assert!(lent.iter().eq(all[front..].iter().map(|(_, value)| value)));
    assert_eq!(values.next_ref(), None);
    assert!(values.next().is_none());
}

fn check_ranges<K: TestKey, V: TestValue>(map: &Map<K, V>, model: &BTreeMap<K, V>, case: &Case) {
    for (low, high) in &case.ranges {
        let low = low.map(K::make);
        let high = high.map(K::make);
        let bounds = (low.clone(), high.clone());
        let wanted = if valid(&low, &high) {
            entries(model.range::<K, _>(bounds.clone()))
        } else {
            Vec::new()
        };
        let mut seen = Vec::new();
        map.for_each_range::<K, _>(bounds.clone(), |key, value| {
            seen.push((key.clone(), value.clone()));
        });
        assert_eq!(seen, wanted);
        let iter = map.range::<K, _>(bounds.clone());
        assert_eq!(iter.len(), wanted.len());
        let folded = iter.fold(Vec::new(), |mut seen, (key, value)| {
            seen.push((key, (*value).clone()));
            seen
        });
        assert_eq!(folded, wanted);
        let reversed: Vec<K> = map
            .range::<K, _>(bounds.clone())
            .rev()
            .map(|(key, _)| key)
            .collect();
        let expected: Vec<K> = wanted.iter().rev().map(|(key, _)| key.clone()).collect();
        assert_eq!(reversed, expected);

        let limit = wanted.len() / 2 + 1;
        let mut seen = Vec::new();
        let outcome = map.try_for_each_range::<K, _, usize>(bounds, |key, _| {
            seen.push(key.clone());
            if seen.len() == limit {
                ControlFlow::Break(seen.len())
            } else {
                ControlFlow::Continue(())
            }
        });
        let stopped = limit <= wanted.len();
        assert_eq!(outcome == ControlFlow::Break(limit), stopped);
        let expected: Vec<K> = wanted
            .iter()
            .take(limit)
            .map(|(key, _)| key.clone())
            .collect();
        assert_eq!(seen, expected);
    }
}

fn check_lookups<K: TestKey, V: TestValue>(map: &Map<K, V>, model: &BTreeMap<K, V>, case: &Case) {
    let scattered: Vec<K> = case.wanted.iter().copied().map(K::make).collect();
    let mut sorted = scattered.clone();
    sorted.sort();
    for keys in [&scattered, &sorted] {
        let found: Vec<(K, Option<V>)> = map
            .get_many(keys)
            .map(|(key, value)| (key.clone(), value.map(|value| (*value).clone())))
            .collect();
        let wanted: Vec<(K, Option<V>)> = keys
            .iter()
            .map(|key| (key.clone(), model.get(key).cloned()))
            .collect();
        assert_eq!(found, wanted);
        let present: Vec<bool> = map.contains_many(keys).map(|(_, found)| found).collect();
        let wanted: Vec<bool> = keys.iter().map(|key| model.contains_key(key)).collect();
        assert_eq!(present, wanted);
        for key in keys {
            let floor = map.floor(key).map(|(key, value)| (key, (*value).clone()));
            let wanted = model.range(..=key).next_back();
            assert_eq!(
                floor,
                wanted.map(|(key, value)| (key.clone(), value.clone()))
            );
        }
    }
}

fn check_cursor<K: TestKey, V: TestValue>(map: &Map<K, V>, model: &BTreeMap<K, V>, case: &Case) {
    let mut cursor = map.cursor();
    assert_eq!(cursor.key(), None);
    assert_eq!(cursor.value(), None);
    let mut place: Option<K> = None;
    for step in &case.moves {
        match *step {
            Move::Seek(key) => {
                let key = K::make(key);
                let exact = cursor.seek(&key);
                assert_eq!(exact, model.contains_key(&key));
                place = model.range(key..).next().map(|(key, _)| key.clone());
            }
            Move::SeekBack(key) => {
                let key = K::make(key);
                let exact = cursor.seek_back(&key);
                assert_eq!(exact, model.contains_key(&key));
                place = model.range(..=key).next_back().map(|(key, _)| key.clone());
            }
            Move::Advance => {
                let moved = cursor.advance().cloned();
                if let Some(key) = &place {
                    let after = (Bound::Excluded(key.clone()), Bound::Unbounded);
                    place = model.range(after).next().map(|(key, _)| key.clone());
                }
                assert_eq!(moved, place);
            }
            Move::Rewind => {
                cursor.rewind();
                place = model.keys().next().cloned();
            }
        }
        assert_eq!(cursor.key(), place.as_ref());
        let held = place.as_ref().map(|key| model[key].clone());
        assert_eq!(cursor.value().map(|value| (*value).clone()), held);
        let read = cursor.read(|key, value| (key.clone(), value.clone()));
        assert_eq!(
            read,
            place.as_ref().map(|key| (key.clone(), model[key].clone()))
        );
    }
}

fn check_all<K: TestKey, V: TestValue>(map: &Map<K, V>, model: &BTreeMap<K, V>, case: &Case) {
    check_folds(map, model, case);
    check_ranges(map, model, case);
    check_lookups(map, model, case);
    check_cursor(map, model, case);
}

fn run<K: TestKey, V: TestValue>(case: &Case, options: fn() -> Options) {
    let mut memory = Map::new();
    let mut model = BTreeMap::new();
    apply(&mut memory, &mut model, &case.first);
    check_all(&memory, &model, case);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reads.ferro");
    let db = options().open::<Map<K, V>>(&path).unwrap();
    db.update(|root| *root = memory.clone()).unwrap();
    let snapshot = db.read();
    check_all(&snapshot, &model, case);

    let mut mixed = (*snapshot).clone();
    apply(&mut mixed, &mut model, &case.second);
    check_all(&mixed, &model, case);

    db.update(|root| *root = mixed.clone()).unwrap();
    drop((snapshot, mixed, memory));
    drop(db);
    let db = options().read_only(true).open::<Map<K, V>>(&path).unwrap();
    check_all(&db.read(), &model, case);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: common::cases(128),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn strings_with_spills_match_the_model(case in case()) {
        run::<u32, String>(&case, fast);
    }

    #[test]
    fn strings_with_spills_match_the_model_on_a_tiny_cache(case in case()) {
        run::<u32, String>(&case, uncached);
    }

    #[test]
    fn small_values_match_the_model(case in case()) {
        run::<u32, u64>(&case, fast);
    }

    #[test]
    fn small_values_match_the_model_on_a_tiny_cache(case in case()) {
        run::<(u32, u32), u64>(&case, uncached);
    }

    #[test]
    fn unit_values_match_the_model(case in case()) {
        run::<(u32, u32), ()>(&case, uncached);
    }

    #[test]
    fn string_keys_match_the_model(case in case()) {
        run::<String, Bytes>(&case, fast);
    }
}

fn large_case(seed: u64) -> Case {
    let mut rng = Rng::new(seed);
    let keys = 200_000;
    let edits = |count: usize, rng: &mut Rng| -> Vec<Edit> {
        (0..count)
            .map(|_| {
                let key = rng.below(keys) as u32;
                if rng.chance(90) {
                    Edit::Put(key, rng.next(), rng.below(32) as usize)
                } else {
                    Edit::Drop(key)
                }
            })
            .collect()
    };
    let first = edits(60_000, &mut rng);
    let second = edits(2_000, &mut rng);
    let ranges = (0..16)
        .map(|_| {
            let low = rng.below(keys) as u32;
            (
                Bound::Included(low),
                Bound::Excluded(low + rng.below(4_000) as u32),
            )
        })
        .collect();
    let moves = (0..20_000)
        .map(|_| match rng.below(4) {
            0 => Move::Seek(rng.below(keys) as u32),
            1 => Move::SeekBack(rng.below(keys) as u32),
            _ => Move::Advance,
        })
        .collect();
    let wanted = (0..20_000).map(|_| rng.below(keys) as u32).collect();
    Case {
        first,
        second,
        ranges,
        moves,
        wanted,
        front: 1_000,
        back: 700,
    }
}

#[test]
fn a_large_tree_matches_the_model() {
    run::<u32, u64>(&large_case(5), fast);
    run::<u32, String>(&large_case(6), uncached);
}

#[test]
fn unsorted_batches_cost_a_fresh_walk_and_nothing_else() {
    let map: Map<u32, u32> = (0..10_000).map(|key| (key * 2, key)).collect();
    let keys = [19_998, 0, 19_998, 7, 8, 40_000, 6, 1];
    let found: Vec<Option<u32>> = map
        .get_many(keys)
        .map(|(_, value)| value.map(|value| *value))
        .collect();
    let wanted = [
        Some(9_999),
        Some(0),
        Some(9_999),
        None,
        Some(4),
        None,
        Some(3),
        None,
    ];
    assert_eq!(found, wanted);
}

#[test]
fn a_cursor_keeps_the_map_it_was_opened_on() {
    let mut map: Map<u32, String> = (0..5_000).map(|key| (key, key.to_string())).collect();
    let mut cursor = map.cursor();
    assert!(cursor.seek(&100));
    map.remove(&101);
    map.insert(102, "changed".to_string());
    assert_eq!(cursor.advance(), Some(&101));
    assert_eq!(*cursor.value().unwrap(), "101");
    assert_eq!(cursor.advance(), Some(&102));
    assert_eq!(*cursor.value().unwrap(), "102");
    assert!(cursor.seek(&4_999));
    assert_eq!(cursor.advance(), None);
    assert_eq!(cursor.advance(), None);
    assert_eq!(cursor.key(), None);
    assert!(cursor.seek_back(&0));
    assert!(!cursor.seek(&5_000));
    assert_eq!(cursor.key(), None);
    let empty: Map<u32, String> = Map::new();
    let mut cursor = empty.cursor();
    assert!(!cursor.seek(&1));
    assert!(!cursor.seek_back(&1));
    cursor.rewind();
    assert_eq!(cursor.key(), None);
    assert_eq!(cursor.advance(), None);
}

#[test]
fn a_value_outlives_the_map_and_its_node() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("held.ferro");
    let db = uncached().open::<Map<u32, String>>(&path).unwrap();
    db.update(|root| root.extend((0..4_000).map(|key| (key, format!("value {key}")))))
        .unwrap();
    let snapshot = db.read();
    let held: Vec<Val<String>> = (0..4_000)
        .step_by(97)
        .filter_map(|key| snapshot.get(&key))
        .collect();
    drop(snapshot);
    drop(db);
    for (value, key) in held.iter().zip((0..4_000).step_by(97)) {
        assert_eq!(**value, format!("value {key}"));
    }
    let copy = held[1].clone();
    assert_eq!(copy.into_owned(), "value 97");
    assert_eq!(*held[1].clone().into_arc(), "value 97");
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Nested {
    blob: String,
    inner: Map<u64, u64>,
}

fn nested_db(path: &std::path::Path, blob: usize) -> ferro::Db<Map<u64, Nested>> {
    let db = fast().open::<Map<u64, Nested>>(path).unwrap();
    db.update(|root| {
        for outer in 0..40u64 {
            let inner = (0..4).map(|key| (key, outer * 10 + key)).collect();
            let blob = "x".repeat(blob);
            root.insert(outer, Nested { blob, inner });
        }
    })
    .unwrap();
    db
}

fn flush_elsewhere(dir: &std::path::Path) {
    drop(
        fast()
            .open::<Map<u64, u64>>(dir.join("other.ferro"))
            .unwrap(),
    );
}

#[test]
fn a_nested_map_read_after_its_store_let_go_outlives_the_iterator() {
    for blob in [8, 700] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested.ferro");
        drop(nested_db(&path, blob));
        let db = fast().open::<Map<u64, Nested>>(&path).unwrap();
        let snapshot = db.read();
        let mut walk = snapshot.iter();
        drop(snapshot);
        drop(db);
        let (first, value) = walk.next().unwrap();
        let (last, back) = walk.next_back().unwrap();
        drop(walk);
        flush_elsewhere(dir.path());
        assert_eq!((first, last), (0, 39));
        assert_eq!(value.inner.get(&1).map(|got| *got), Some(1));
        assert_eq!(back.inner.get(&3).map(|got| *got), Some(393));
        drop(value);
        flush_elsewhere(dir.path());
        assert_eq!(back.inner.iter().map(|(_, got)| *got).sum::<u64>(), 1566);
    }
}

#[test]
fn a_nested_map_dropped_before_its_iterators_leaves_the_file_readable() {
    for blob in [8, 700] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested.ferro");
        drop(nested_db(&path, blob));
        let db = fast().open::<Map<u64, Nested>>(&path).unwrap();
        let snapshot = db.read();
        let mut walk = snapshot.iter();
        let mut cursor = snapshot.cursor();
        drop(snapshot);
        drop(db);
        let (_, value) = walk.next().unwrap();
        assert!(cursor.seek(&20));
        let row = cursor.value().unwrap();
        assert_eq!(row.inner.get(&2).map(|got| *got), Some(202));
        drop(value);
        drop(row);
        drop(walk);
        drop(cursor);
        let db = fast().open::<Map<u64, Nested>>(&path).unwrap();
        assert_eq!(db.read().len(), 40);
    }
}

#[cfg(target_os = "linux")]
fn open_files(path: &std::path::Path) -> usize {
    let target = std::fs::canonicalize(path).unwrap();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|link| *link == target)
        .count()
}

#[cfg(target_os = "linux")]
#[test]
fn a_file_closes_when_the_last_iterator_over_it_goes() {
    for blob in [8, 700] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested.ferro");
        drop(nested_db(&path, blob));
        let db = fast().open::<Map<u64, Nested>>(&path).unwrap();
        let snapshot = db.read();
        let mut walk = snapshot.iter();
        let mut cursor = snapshot.cursor();
        assert_eq!(walk.next().map(|(key, _)| key), Some(0));
        assert!(cursor.seek(&20));
        drop(snapshot);
        drop(db);
        assert_eq!(
            open_files(&path),
            1,
            "blob {blob}: open while iterators read"
        );
        drop(walk);
        assert_eq!(cursor.advance(), Some(&21));
        drop(cursor);
        assert_eq!(
            open_files(&path),
            0,
            "blob {blob}: open after the last iterator"
        );
        let db = fast().open::<Map<u64, Nested>>(&path).unwrap();
        let snapshot = db.read();
        let mut walk = snapshot.iter();
        drop(snapshot);
        drop(db);
        let (_, value) = walk.next().unwrap();
        let mut inner = value.inner.iter();
        assert_eq!(inner.next().map(|(key, _)| key), Some(0));
        drop(value);
        drop(walk);
        assert_eq!(
            open_files(&path),
            1,
            "blob {blob}: open while a nested iterator reads"
        );
        drop(inner);
        assert_eq!(
            open_files(&path),
            0,
            "blob {blob}: open after the nested iterator"
        );
    }
}

#[test]
fn a_map_edited_beside_its_iterators_leaves_what_they_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edited.ferro");
    let db = uncached().open::<Map<u32, String>>(&path).unwrap();
    db.update(|root| root.extend((0..4_000).map(|key| (key, format!("value {key}")))))
        .unwrap();
    let mut map = (*db.read()).clone();
    drop(db);
    let mut walk = map.iter();
    let mut cursor = map.cursor();
    assert!(cursor.seek(&2_000));
    assert_eq!(walk.next().map(|(key, _)| key), Some(0));
    for key in 0..4_000 {
        map.insert(key, format!("changed {key}"));
    }
    map.remove(&1);
    map.insert(4_000, "added".to_string());
    let rest: Vec<(u32, String)> = walk.map(|(key, value)| (key, (*value).clone())).collect();
    assert_eq!(rest.len(), 3_999);
    assert!(
        rest.iter()
            .all(|(key, value)| *value == format!("value {key}"))
    );
    assert_eq!(cursor.advance(), Some(&2_001));
    assert_eq!(
        cursor.value().map(|value| (*value).clone()),
        Some("value 2001".to_string())
    );
    drop(cursor);
    assert_eq!(map.len(), 4_000);
    assert_eq!(
        map.get(&2).map(|value| (*value).clone()),
        Some("changed 2".to_string())
    );
}

#[test]
fn set_ranges_know_their_length_from_both_ends() {
    let set: Set<(u32, u32)> = (0..1_000)
        .flat_map(|a| (0..8).map(move |b| (a, b * 3)))
        .collect();
    let mut range = set.range((5, 0)..=(9, u32::MAX));
    assert_eq!(range.len(), 40);
    assert_eq!(range.next(), Some((5, 0)));
    assert_eq!(range.next_back(), Some((9, 21)));
    assert_eq!(range.len(), 38);
    assert_eq!(range.rev().count(), 38);
    assert_eq!(set.range((1_000, 0)..).len(), 0);
    let probes = [(5, 3), (5, 4), (2_000, 0)];
    let found: Vec<bool> = set.contains_many(probes).map(|(_, found)| found).collect();
    assert_eq!(found, [true, false, false]);
    let other: Set<(u32, u32)> = set.iter().filter(|item| item.0 % 2 == 0).collect();
    assert_eq!(set.intersection(&other).len(), other.len());
    assert_eq!(set.difference(&other).len(), set.len() - other.len());
    assert!(other.is_subset(&set));
    assert!(!set.is_subset(&other));
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
struct Row {
    #[key]
    id: u32,
    #[index]
    group: u32,
    note: String,
}

#[derive(Clone, Default, Data)]
struct Rows {
    rows: Table<Row>,
}

#[test]
fn index_keys_match_the_rows_without_reading_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rows.ferro");
    let db = fast().open::<Rows>(&path).unwrap();
    db.update(|root| {
        for id in 0..2_000u32 {
            root.rows.insert(Row {
                id,
                group: id.wrapping_mul(2_654_435_761) % 7,
                note: text(u64::from(id), (id % 700) as usize),
            });
        }
    })
    .unwrap();
    let snapshot = db.read();
    let rows = &snapshot.rows;
    for group in 0..8 {
        let wanted: Vec<u32> = rows.by_group(&group).map(|row| row.id).collect();
        let mut keys = rows.by_group(&group).keys();
        assert_eq!(keys.len(), wanted.len());
        assert_eq!(keys.by_ref().collect::<Vec<u32>>(), wanted);
        let back: Vec<u32> = rows.by_group(&group).keys().rev().collect();
        assert!(back.iter().rev().eq(wanted.iter()));
        let reversed: Vec<u32> = rows.by_group(&group).rev().map(|row| row.id).collect();
        assert_eq!(reversed, back);
        for id in &wanted {
            assert_eq!(rows.get(id).unwrap().group, group);
        }
        let mut visited = Vec::new();
        rows.by_group(&group).visit(|row| visited.push(row.clone()));
        assert!(visited.iter().map(|row| row.id).eq(wanted.iter().copied()));
        for row in &visited {
            assert_eq!(*row, *rows.get(&row.id).unwrap());
        }
        let mut lookup = rows.by_group(&group);
        let mut lent = Vec::new();
        while lent.len() < wanted.len() / 2
            && let Some(row) = lookup.next_ref()
        {
            lent.push(row.clone());
        }
        lent.extend(lookup.map(|row| (*row).clone()));
        assert_eq!(lent, visited);
        let mut matches = rows.find().by_group(&group);
        let mut lent = Vec::new();
        while let Some(row) = matches.next_ref() {
            lent.push(row.clone());
        }
        assert_eq!(lent, visited);
        let mut visited = Vec::new();
        rows.find()
            .by_group(&group)
            .visit(|row| visited.push(row.id));
        assert_eq!(visited, wanted);
    }
    let mut visited = Vec::new();
    rows.range(100..1_200)
        .visit(|row| visited.push(row.clone()));
    assert!(visited.iter().map(|row| row.id).eq(100..1_200));
    let mut range = rows.range(100..1_200);
    let mut lent = Vec::new();
    while let Some(row) = range.next_ref() {
        lent.push(row.clone());
    }
    assert_eq!(lent, visited);
    for row in &visited {
        assert_eq!(*row, *rows.get(&row.id).unwrap());
    }
    let mut seen = Vec::new();
    rows.for_each(|row| seen.push(row.id));
    assert_eq!(seen, (0..2_000).collect::<Vec<u32>>());
    let mut seen = Vec::new();
    rows.for_each_range(100..120, |row| seen.push(row.id));
    assert_eq!(seen, (100..120).collect::<Vec<u32>>());
    assert_eq!(
        rows.iter().map(|row| u64::from(row.id)).sum::<u64>(),
        1_999_000
    );
}

#[test]
fn an_index_entry_without_its_row_ends_the_walk_there() {
    let mut rows: Table<Row> = Table::new();
    for id in 0..200 {
        rows.insert(Row {
            id,
            group: id % 2,
            note: String::new(),
        });
    }
    let mut torn = (*rows).clone();
    torn.rows_mut().remove(&100);
    let seen: Vec<u32> = torn.by_group(&0).map(|row| row.id).collect();
    assert_eq!(seen, (0..100).step_by(2).collect::<Vec<u32>>());
    let mut visited = Vec::new();
    torn.by_group(&0).visit(|row| visited.push(row.id));
    assert_eq!(visited, seen);
    let mut lookup = torn.by_group(&0);
    let mut lent = Vec::new();
    while let Some(row) = lookup.next_ref() {
        lent.push(row.id);
    }
    assert_eq!(lent, seen);
    assert_eq!(torn.by_group(&0).keys().count(), 100);
}

fn damage(bytes: &mut [u8], rng: &mut Rng) {
    let start = 8192 + rng.below(bytes.len() as u64 - 8192) as usize;
    let end = (start + 1 + rng.below(64) as usize).min(bytes.len());
    for byte in &mut bytes[start..end] {
        *byte ^= 1 + rng.below(255) as u8;
    }
}

fn agree<T: PartialEq + Debug>(outcome: Outcome<T>, wanted: T) -> bool {
    match outcome {
        Outcome::Value(value) => {
            assert_eq!(value, wanted);
            false
        }
        Outcome::FerroPanic(_) => true,
    }
}

#[test]
fn damage_is_never_read_as_data() {
    quiet_ferro_panics();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("whole.ferro");
    let keys: Vec<u32> = (0..3_000).map(|key| key * 3).collect();
    let value = |key: u32| {
        let len = if key.is_multiple_of(90) {
            800
        } else {
            (key % 40) as usize
        };
        text(u64::from(key), len)
    };
    let model: BTreeMap<u32, String> = keys.iter().map(|&key| (key, value(key))).collect();
    {
        let db = fast().open::<Map<u32, String>>(&path).unwrap();
        db.update(|root| root.extend(model.clone())).unwrap();
    }
    let whole = std::fs::read(&path).unwrap();
    let all: Vec<(u32, String)> = model.clone().into_iter().collect();
    let tail: Vec<(u32, String)> = model
        .range(4_500..4_800)
        .map(|(k, v)| (*k, v.clone()))
        .collect();
    let floors: Vec<Option<u32>> = keys
        .iter()
        .map(|key| model.range(..=key + 1).next_back().map(|(k, _)| *k))
        .collect();
    let mut rng = Rng::new(11);
    let mut caught = 0;
    for round in 0..300 {
        let mut bytes = whole.clone();
        damage(&mut bytes, &mut rng);
        let damaged = dir.path().join(format!("damaged-{round}.ferro"));
        std::fs::write(&damaged, &bytes).unwrap();
        let cache = 1 + rng.below(1 << 16);
        let opened = Options::new()
            .read_only(true)
            .cache_bytes(cache)
            .open::<Map<u32, String>>(&damaged);
        let Ok(db) = opened else {
            std::fs::remove_file(&damaged).unwrap();
            continue;
        };
        let snapshot = match guarded(|| db.read()) {
            Outcome::Value(snapshot) => snapshot,
            Outcome::FerroPanic(_) => continue,
        };
        let map: &Map<u32, String> = &snapshot;
        caught += usize::from(agree(
            guarded(|| {
                let mut seen = Vec::new();
                map.for_each(|key, value| seen.push((*key, value.clone())));
                seen
            }),
            all.clone(),
        ));
        caught += usize::from(agree(
            guarded(|| {
                map.iter().fold(Vec::new(), |mut seen, (key, value)| {
                    seen.push((key, (*value).clone()));
                    seen
                })
            }),
            all.clone(),
        ));
        caught += usize::from(agree(
            guarded(|| map.keys().fold(0u64, |total, key| total + u64::from(key))),
            keys.iter().map(|key| u64::from(*key)).sum(),
        ));
        caught += usize::from(agree(
            guarded(|| {
                let mut seen = Vec::new();
                for (key, value) in map.iter() {
                    seen.push((key, (*value).clone()));
                }
                seen
            }),
            all.clone(),
        ));
        caught += usize::from(agree(
            guarded(|| {
                let mut seen: Vec<(u32, String)> = map
                    .iter()
                    .rev()
                    .map(|(key, value)| (key, value.into_owned()))
                    .collect();
                seen.reverse();
                seen
            }),
            all.clone(),
        ));
        caught += usize::from(agree(
            guarded(|| {
                let mut cursor = map.cursor();
                cursor.rewind();
                let mut seen = Vec::new();
                while let Some(pair) = cursor.read(|key, value| (*key, value.clone())) {
                    seen.push(pair);
                    cursor.advance();
                }
                seen
            }),
            all.clone(),
        ));
        caught += usize::from(agree(
            guarded(|| {
                let mut seen = Vec::new();
                map.for_each_range(4_500..4_800, |key, value| seen.push((*key, value.clone())));
                seen
            }),
            tail.clone(),
        ));
        caught += usize::from(agree(
            guarded(|| {
                map.get_many(&keys)
                    .map(|(key, value)| (*key, value.map_or(String::new(), Val::into_owned)))
                    .collect::<Vec<_>>()
            }),
            all.clone(),
        ));
        caught += usize::from(agree(
            guarded(|| {
                let mut cursor = map.cursor();
                keys.iter()
                    .map(|key| {
                        cursor.seek_back(&(key + 1));
                        cursor.key().copied()
                    })
                    .collect::<Vec<_>>()
            }),
            floors.clone(),
        ));
        drop(snapshot);
        drop(db);
        std::fs::remove_file(&damaged).unwrap();
    }
    assert!(caught > 100, "only {caught} reads met the damage");
}
