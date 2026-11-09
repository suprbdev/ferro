mod common;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Debug;
use std::ops::Bound;
use std::sync::OnceLock;

use common::{Rng, fast, level_of, text, uncached};
use ferro::{Data, Db, List, Map, Options, Set, Val};
use proptest::prelude::*;

trait TestKey: Data + Ord + Debug {
    fn make(seed: u32) -> Self;
}

impl TestKey for u32 {
    fn make(seed: u32) -> Self {
        seed
    }
}

impl TestKey for u64 {
    fn make(seed: u32) -> Self {
        u64::from(seed) * 0x1_0000_0001
    }
}

impl TestKey for String {
    fn make(seed: u32) -> Self {
        format!("key-{seed:06}")
    }
}

impl TestKey for (u8, String) {
    fn make(seed: u32) -> Self {
        ((seed % 3) as u8, format!("{seed:05}"))
    }
}

impl TestKey for Vec<u8> {
    fn make(seed: u32) -> Self {
        seed.to_be_bytes().to_vec()
    }
}

#[derive(Clone, Copy, Debug)]
enum Step {
    Front,
    Back,
    Nth(usize),
    NthBack(usize),
}

#[derive(Clone, Debug)]
enum Op {
    Insert(u32, u64, usize),
    Remove(u32),
    Get(u32),
    GetMut(u32, u64, usize),
    Update(u32, usize),
    GetOrInsert(u32, u64, usize),
    PopFirst,
    PopLast,
    Retain(u64, u64),
    RemoveRange(Bound<u32>, Bound<u32>),
    TakeRange(Bound<u32>, Bound<u32>),
    SplitOff(u32),
    SplitRejoin(u32),
    AppendMap(Vec<(u32, u64, usize)>),
    Extend(Vec<(u32, u64, usize)>),
    ApplyEdits(Vec<(u32, Option<(u64, usize)>)>),
    Range(Bound<u32>, Bound<u32>, Vec<Step>),
    Walk(Vec<Step>),
    KeysWalk(Vec<Step>),
    ValuesWalk(Vec<Step>),
    Nth(usize),
    Commit,
    Reopen,
}

fn sizes() -> impl Strategy<Value = usize> {
    prop_oneof![
        5 => 0..32usize,
        5 => 500..524usize,
        1 => 524..3000usize,
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => Just(Step::Front),
        4 => Just(Step::Back),
        1 => (0..6usize).prop_map(Step::Nth),
        1 => (0..6usize).prop_map(Step::NthBack),
    ]
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    prop::collection::vec(step(), 0..24)
}

fn bound(keys: u32) -> impl Strategy<Value = Bound<u32>> {
    prop_oneof![
        Just(Bound::Unbounded),
        (0..keys).prop_map(Bound::Included),
        (0..keys).prop_map(Bound::Excluded),
    ]
}

fn op(keys: u32, durable: bool) -> impl Strategy<Value = Op> {
    let commit = if durable { 6 } else { 1 };
    prop_oneof![
        12 => (0..keys, any::<u64>(), sizes()).prop_map(|(k, s, n)| Op::Insert(k, s, n)),
        8 => (0..keys).prop_map(Op::Remove),
        3 => (0..keys).prop_map(Op::Get),
        3 => (0..keys, any::<u64>(), sizes()).prop_map(|(k, s, n)| Op::GetMut(k, s, n)),
        3 => (0..keys, sizes()).prop_map(|(k, n)| Op::Update(k, n)),
        2 => (0..keys, any::<u64>(), sizes()).prop_map(|(k, s, n)| Op::GetOrInsert(k, s, n)),
        2 => Just(Op::PopFirst),
        2 => Just(Op::PopLast),
        1 => (any::<u64>(), 2..7u64).prop_map(|(s, m)| Op::Retain(s, m)),
        2 => (bound(keys), bound(keys)).prop_map(|(a, b)| Op::RemoveRange(a, b)),
        1 => (bound(keys), bound(keys)).prop_map(|(a, b)| Op::TakeRange(a, b)),
        1 => (0..keys).prop_map(Op::SplitOff),
        2 => (0..keys).prop_map(Op::SplitRejoin),
        2 => prop::collection::vec((0..keys, any::<u64>(), sizes()), 0..12).prop_map(Op::AppendMap),
        3 => prop::collection::vec((0..keys, any::<u64>(), sizes()), 0..40).prop_map(Op::Extend),
        3 => prop::collection::vec((0..keys, prop::option::of((any::<u64>(), sizes()))), 0..40)
            .prop_map(Op::ApplyEdits),
        4 => (bound(keys), bound(keys), steps()).prop_map(|(a, b, s)| Op::Range(a, b, s)),
        2 => steps().prop_map(Op::Walk),
        1 => steps().prop_map(Op::KeysWalk),
        1 => steps().prop_map(Op::ValuesWalk),
        1 => (0..keys as usize + 2).prop_map(Op::Nth),
        commit => Just(Op::Commit),
        1 => Just(Op::Reopen),
    ]
}

fn map_bound<K: TestKey>(bound: Bound<u32>, resolve: &impl Fn(u32) -> K) -> Bound<K> {
    match bound {
        Bound::Included(seed) => Bound::Included(resolve(seed)),
        Bound::Excluded(seed) => Bound::Excluded(resolve(seed)),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn in_range<K: Ord>(key: &K, low: &Bound<K>, high: &Bound<K>) -> bool {
    let above = match low {
        Bound::Included(bound) => key >= bound,
        Bound::Excluded(bound) => key > bound,
        Bound::Unbounded => true,
    };
    let below = match high {
        Bound::Included(bound) => key <= bound,
        Bound::Excluded(bound) => key < bound,
        Bound::Unbounded => true,
    };
    above && below
}

fn walk<I, T>(mut iter: I, mut expected: VecDeque<T>, steps: &[Step])
where
    I: DoubleEndedIterator<Item = T> + ExactSizeIterator,
    T: PartialEq + Debug,
{
    assert_eq!(iter.len(), expected.len());
    for step in steps {
        match step {
            Step::Front => assert_eq!(iter.next(), expected.pop_front()),
            Step::Back => assert_eq!(iter.next_back(), expected.pop_back()),
            Step::Nth(n) => {
                let wanted = if *n < expected.len() {
                    expected.drain(..*n).for_each(drop);
                    expected.pop_front()
                } else {
                    expected.clear();
                    None
                };
                assert_eq!(iter.nth(*n), wanted);
            }
            Step::NthBack(n) => {
                let wanted = if *n < expected.len() {
                    let keep = expected.len() - *n;
                    expected.truncate(keep);
                    expected.pop_back()
                } else {
                    expected.clear();
                    None
                };
                assert_eq!(iter.nth_back(*n), wanted);
            }
        }
        assert_eq!(iter.len(), expected.len());
    }
    let rest: Vec<T> = iter.by_ref().collect();
    assert_eq!(rest, Vec::from(expected));
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.len(), 0);
}

fn pairs<K: Clone>(model: &BTreeMap<K, String>) -> VecDeque<(K, Val<String>)> {
    model
        .iter()
        .map(|(key, value)| (key.clone(), Val::new(value.clone())))
        .collect()
}

fn fresh<K: TestKey>(model: &BTreeMap<K, String>) -> Map<K, String> {
    let mut map = Map::new();
    for (key, value) in model.iter().rev() {
        map.insert(key.clone(), value.clone());
    }
    map
}

fn apply<K: TestKey>(
    map: &mut Map<K, String>,
    model: &mut BTreeMap<K, String>,
    op: &Op,
    resolve: &impl Fn(u32) -> K,
) {
    match op {
        Op::Insert(seed, salt, len) => {
            let key = resolve(*seed);
            let value = text(*salt, *len);
            let old = map.insert(key.clone(), value.clone());
            assert_eq!(old.map(|v| (*v).clone()), model.insert(key, value));
        }
        Op::Remove(seed) => {
            let key = resolve(*seed);
            let old = map.remove(&key);
            assert_eq!(old.map(|v| (*v).clone()), model.remove(&key));
        }
        Op::Get(seed) => {
            let key = resolve(*seed);
            assert_eq!(
                map.get(&key).map(|v| (*v).clone()),
                model.get(&key).cloned()
            );
            assert_eq!(
                map.try_get(&key).unwrap().is_some(),
                model.contains_key(&key)
            );
            assert_eq!(map.contains_key(&key), model.contains_key(&key));
        }
        Op::GetMut(seed, salt, len) => {
            let key = resolve(*seed);
            let value = text(*salt, *len);
            match (map.get_mut(&key), model.get_mut(&key)) {
                (Some(a), Some(b)) => {
                    assert_eq!(a, b);
                    *a = value.clone();
                    *b = value;
                }
                (None, None) => {}
                (a, b) => panic!("get_mut mismatch {a:?} {b:?}"),
            }
        }
        Op::Update(seed, len) => {
            let key = resolve(*seed);
            let change = |value: &mut String| {
                if value.len() > *len {
                    value.truncate(*len);
                } else {
                    while value.len() < *len {
                        value.push('x');
                    }
                }
            };
            let touched = map.update(&key, change).is_some();
            assert_eq!(touched, model.contains_key(&key));
            if let Some(value) = model.get_mut(&key) {
                change(value);
            }
        }
        Op::GetOrInsert(seed, salt, len) => {
            let key = resolve(*seed);
            let value = text(*salt, *len);
            let a = map
                .get_or_insert_with(key.clone(), || value.clone())
                .clone();
            let b = model.entry(key).or_insert(value).clone();
            assert_eq!(a, b);
        }
        Op::PopFirst => {
            let got = map.pop_first().map(|(k, v)| (k, (*v).clone()));
            assert_eq!(got, model.pop_first());
        }
        Op::PopLast => {
            let got = map.pop_last().map(|(k, v)| (k, (*v).clone()));
            assert_eq!(got, model.pop_last());
        }
        Op::Retain(salt, modulus) => {
            let mut rng = Rng::new(*salt);
            let doomed: BTreeSet<K> = model
                .keys()
                .filter(|_| rng.below(*modulus) == 0)
                .cloned()
                .collect();
            let mut visited = Vec::new();
            map.retain(|key, value| {
                assert_eq!(Some(value), model.get(key));
                visited.push(key.clone());
                !doomed.contains(key)
            });
            let all: Vec<K> = model.keys().cloned().collect();
            assert_eq!(visited, all);
            model.retain(|key, _| !doomed.contains(key));
        }
        Op::RemoveRange(low, high) => {
            let low = map_bound(*low, resolve);
            let high = map_bound(*high, resolve);
            let before = model.len();
            model.retain(|key, _| !in_range(key, &low, &high));
            assert_eq!(map.remove_range::<K, _>((low, high)), before - model.len());
        }
        Op::TakeRange(low, high) => {
            let low = map_bound(*low, resolve);
            let high = map_bound(*high, resolve);
            let wanted: BTreeMap<K, String> = model
                .iter()
                .filter(|(key, _)| in_range(*key, &low, &high))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            model.retain(|key, _| !in_range(key, &low, &high));
            let taken = map.take_range::<K, _>((low, high));
            check_full(&taken, &wanted);
            assert_eq!(taken.root_hash(), fresh(&wanted).root_hash());
        }
        Op::SplitOff(seed) => {
            let key = resolve(*seed);
            let wanted = model.split_off(&key);
            let tail = map.split_off(&key);
            check_full(&tail, &wanted);
            assert_eq!(tail.root_hash(), fresh(&wanted).root_hash());
        }
        Op::SplitRejoin(seed) => {
            let key = resolve(*seed);
            let mut tail = map.split_off(&key);
            map.append(&mut tail);
            assert!(tail.is_empty());
        }
        Op::AppendMap(entries) => {
            let mut other = Map::new();
            let mut wanted = BTreeMap::new();
            for (seed, salt, len) in entries {
                let key = resolve(*seed);
                let value = text(*salt, *len);
                other.insert(key.clone(), value.clone());
                wanted.insert(key, value);
            }
            map.append(&mut other);
            assert!(other.is_empty());
            model.append(&mut wanted);
        }
        Op::Extend(entries) => {
            let items: Vec<(K, String)> = entries
                .iter()
                .map(|(seed, salt, len)| (resolve(*seed), text(*salt, *len)))
                .collect();
            map.extend(items.clone());
            model.extend(items);
        }
        Op::ApplyEdits(edits) => {
            let items: Vec<(K, Option<String>)> = edits
                .iter()
                .map(|(seed, edit)| {
                    let value = edit.map(|(salt, len)| text(salt, len));
                    (resolve(*seed), value)
                })
                .collect();
            map.apply(items.clone());
            for (key, value) in items {
                match value {
                    Some(value) => {
                        model.insert(key, value);
                    }
                    None => {
                        model.remove(&key);
                    }
                }
            }
        }
        Op::Range(low, high, steps) => {
            let low = map_bound(*low, resolve);
            let high = map_bound(*high, resolve);
            let expected: VecDeque<(K, Val<String>)> = model
                .iter()
                .filter(|(key, _)| in_range(*key, &low, &high))
                .map(|(key, value)| (key.clone(), Val::new(value.clone())))
                .collect();
            let reversed: Vec<(K, Val<String>)> = expected.iter().rev().cloned().collect();
            let bounds = (low, high);
            let got: Vec<(K, Val<String>)> = map.range::<K, _>(bounds.clone()).rev().collect();
            assert_eq!(got, reversed);
            walk(map.range::<K, _>(bounds), expected, steps);
        }
        Op::Walk(steps) => walk(map.iter(), pairs(model), steps),
        Op::KeysWalk(steps) => walk(map.keys(), model.keys().cloned().collect(), steps),
        Op::ValuesWalk(steps) => walk(
            map.values(),
            model.values().map(|v| Val::new(v.clone())).collect(),
            steps,
        ),
        Op::Nth(index) => {
            let got = map.nth(*index).map(|(k, v)| (k, (*v).clone()));
            let wanted = model
                .iter()
                .nth(*index)
                .map(|(k, v)| (k.clone(), v.clone()));
            assert_eq!(got, wanted);
        }
        Op::Commit | Op::Reopen => {}
    }
    assert_eq!(map.len(), model.len());
    assert_eq!(map.is_empty(), model.is_empty());
}

fn check_full<K: TestKey>(map: &Map<K, String>, model: &BTreeMap<K, String>) {
    assert_eq!(map.len(), model.len());
    let forward: Vec<(K, Val<String>)> = map.iter().collect();
    assert_eq!(forward, Vec::from(pairs(model)));
    let mut backward: Vec<(K, Val<String>)> = map.iter().rev().collect();
    backward.reverse();
    assert_eq!(backward, forward);
    let keys: Vec<K> = map.keys().collect();
    assert_eq!(keys, model.keys().cloned().collect::<Vec<K>>());
    let values: Vec<Val<String>> = map.values().rev().collect();
    let wanted: Vec<Val<String>> = model.values().rev().map(|v| Val::new(v.clone())).collect();
    assert_eq!(values, wanted);
    assert_eq!(map.first().map(|p| p.0), model.keys().next().cloned());
    assert_eq!(map.last().map(|p| p.0), model.keys().next_back().cloned());
    for (index, (key, value)) in model.iter().enumerate() {
        assert_eq!(map.get(key).as_deref(), Some(value));
        assert_eq!(map.nth(index).map(|p| p.0).as_ref(), Some(key));
    }
    assert_eq!(map.nth(model.len()).map(|p| p.0), None);
    assert_eq!(map.root_hash(), fresh(model).root_hash());
}

fn memory_run<K: TestKey>(ops: &[Op], resolve: impl Fn(u32) -> K) {
    let mut map = Map::new();
    let mut model = BTreeMap::new();
    for op in ops {
        apply(&mut map, &mut model, op, &resolve);
    }
    check_full(&map, &model);
    map.clear();
    assert!(map.is_empty());
    assert_eq!(map.iter().next(), None);
}

fn db_run<K: TestKey>(ops: &[Op], options: fn() -> Options, resolve: impl Fn(u32) -> K) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.ferro");
    let open = || options().open::<Map<K, String>>(&path).unwrap();
    let mut db = open();
    let mut model: BTreeMap<K, String> = BTreeMap::new();
    let mut committed = model.clone();
    let mut pending: Vec<&Op> = Vec::new();
    let mut settle = |db: &Db<Map<K, String>>, pending: &mut Vec<&Op>| {
        let before = db.version();
        db.update(|root| {
            for op in pending.iter() {
                apply(root, &mut model, op, &resolve);
            }
        })
        .unwrap();
        pending.clear();
        let changed = model != committed;
        assert_eq!(db.version(), before + u64::from(changed));
        committed = model.clone();
        check_full(&*db.read(), &model);
    };
    for op in ops {
        match op {
            Op::Commit => settle(&db, &mut pending),
            Op::Reopen => {
                settle(&db, &mut pending);
                let version = db.version();
                drop(db);
                db = open();
                assert_eq!(db.version(), version);
            }
            other => pending.push(other),
        }
    }
    settle(&db, &mut pending);
    db.verify().unwrap();
}

fn retained_run<K: TestKey>(ops: &[Op], options: fn() -> Options, resolve: impl Fn(u32) -> K) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.ferro");
    let open = || options().open::<Map<K, String>>(&path).unwrap();
    let mut db = open();
    let mut live: Map<K, String> = (*db.read()).clone();
    let mut model: BTreeMap<K, String> = BTreeMap::new();
    let mut flip = false;
    for op in ops {
        match op {
            Op::Commit => {
                db.update(|root| *root = live.clone()).unwrap();
                check_full(&*db.read(), &model);
                check_full(&live, &model);
                flip = !flip;
                if flip {
                    live = (*db.read()).clone();
                }
            }
            Op::Reopen => {
                db.update(|root| *root = live.clone()).unwrap();
                drop(live);
                drop(db);
                db = open();
                live = (*db.read()).clone();
                check_full(&live, &model);
            }
            other => apply(&mut live, &mut model, other, &resolve),
        }
    }
    check_full(&live, &model);
    db.update(|root| *root = live.clone()).unwrap();
    check_full(&*db.read(), &model);
}

fn universe() -> &'static Vec<u32> {
    static KEYS: OnceLock<Vec<u32>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let mut high = Vec::new();
        let mut one = Vec::new();
        let mut zero = Vec::new();
        for key in 0..40_000u32 {
            match level_of(&key) {
                0 => {
                    if zero.len() < 60 || key % 400 == 0 {
                        zero.push(key);
                    }
                }
                1 => {
                    if one.len() < 80 {
                        one.push(key);
                    }
                }
                _ => high.push(key),
            }
        }
        high.extend(one);
        high.extend(zero);
        high.sort_unstable();
        high
    })
}

fn sequence(keys: u32, durable: bool, length: usize) -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op(keys, durable), 1..length)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: common::cases(1000), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn memory_tiny_keys(ops in sequence(8, false, 200)) {
        memory_run::<u32>(&ops, |seed| seed);
    }

    #[test]
    fn memory_small_keys(ops in sequence(64, false, 300)) {
        memory_run::<u64>(&ops, u64::make);
    }

    #[test]
    fn memory_string_keys(ops in sequence(128, false, 300)) {
        memory_run::<String>(&ops, String::make);
    }

    #[test]
    fn memory_tuple_keys(ops in sequence(64, false, 200)) {
        memory_run::<(u8, String)>(&ops, <(u8, String)>::make);
    }

    #[test]
    fn memory_bytes_keys(ops in sequence(64, false, 200)) {
        memory_run::<Vec<u8>>(&ops, Vec::<u8>::make);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: common::cases(500), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn memory_multilevel_keys(ops in sequence(4000, false, 800)) {
        let keys = universe();
        memory_run::<u32>(&ops, |seed| keys[seed as usize % keys.len()]);
    }

    #[test]
    fn stored_tiny_keys(ops in sequence(8, true, 200)) {
        db_run::<u32>(&ops, fast, |seed| seed);
    }

    #[test]
    fn stored_small_keys_uncached(ops in sequence(64, true, 300)) {
        db_run::<u64>(&ops, uncached, u64::make);
    }

    #[test]
    fn stored_string_keys(ops in sequence(128, true, 300)) {
        db_run::<String>(&ops, fast, String::make);
    }

    #[test]
    fn stored_multilevel_keys(ops in sequence(4000, true, 600)) {
        let keys = universe();
        db_run::<u32>(&ops, fast, |seed| keys[seed as usize % keys.len()]);
    }

    #[test]
    fn stored_multilevel_keys_uncached(ops in sequence(4000, true, 400)) {
        let keys = universe();
        db_run::<u32>(&ops, uncached, |seed| keys[seed as usize % keys.len()]);
    }

    #[test]
    fn retained_small_keys(ops in sequence(64, true, 300)) {
        retained_run::<u32>(&ops, fast, |seed| seed);
    }

    #[test]
    fn retained_multilevel_keys_uncached(ops in sequence(4000, true, 400)) {
        let keys = universe();
        retained_run::<u32>(&ops, uncached, |seed| keys[seed as usize % keys.len()]);
    }
}

#[test]
fn large_random_workload_matches_model() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.ferro");
    let db = fast().open::<Map<u32, String>>(&path).unwrap();
    let mut model: BTreeMap<u32, String> = BTreeMap::new();
    let mut rng = Rng::new(7);
    for _ in 0..30 {
        let batch: Vec<Op> = (0..3000)
            .map(|_| {
                let key = rng.below(20_000) as u32;
                match rng.below(10) {
                    0..=5 => Op::Insert(key, rng.next(), rng.below(40) as usize),
                    6..=7 => Op::Remove(key),
                    8 => Op::Update(key, 505 + rng.below(16) as usize),
                    _ => Op::GetMut(key, rng.next(), rng.below(1024) as usize),
                }
            })
            .collect();
        db.update(|root| {
            for op in &batch {
                apply(root, &mut model, op, &|seed| seed);
            }
        })
        .unwrap();
        let snapshot = db.read();
        assert_eq!(snapshot.len(), model.len());
        for (index, (key, value)) in model.iter().enumerate().step_by(97) {
            assert_eq!(snapshot.get(key).as_deref(), Some(value));
            assert_eq!(snapshot.nth(index).unwrap().0, *key);
        }
    }
    check_full(&*db.read(), &model);
    db.verify().unwrap();
}

#[test]
fn empty_and_reversed_ranges_are_empty() {
    let map: Map<u32, u32> = (0..100).map(|n| (n * 2, n)).collect();
    let reversed = (Bound::Included(50), Bound::Excluded(10));
    assert_eq!(map.range::<u32, _>(reversed).count(), 0);
    assert_eq!(map.range(50..50).count(), 0);
    assert_eq!(map.range(50..=50).count(), 1);
    assert_eq!(map.range(51..52).len(), 0);
    assert_eq!(map.range(51..52).next_back(), None);
    let excluded = (Bound::Excluded(50), Bound::Excluded(50));
    assert_eq!(map.range::<u32, _>(excluded).count(), 0);
    assert_eq!(map.range(500..).count(), 0);
    assert_eq!(map.range(..0).count(), 0);
    let empty: Map<u32, u32> = Map::new();
    assert_eq!(empty.range(1..5).next(), None);
    assert_eq!(empty.iter().next_back(), None);
    assert_eq!(empty.first(), None);
    assert_eq!(empty.nth(0), None);
}

#[test]
fn borrowed_string_lookups() {
    let mut map: Map<String, u32> = Map::new();
    for n in 0..500u32 {
        map.insert(format!("{n:03}"), n);
    }
    assert_eq!(map.get("123").as_deref(), Some(&123));
    assert!(map.contains_key("499"));
    assert_eq!(map.remove("123").as_deref(), Some(&123));
    assert_eq!(map.get("123"), None);
    let bounds: (Bound<&str>, Bound<&str>) = (Bound::Excluded("100"), Bound::Included("105"));
    let keys: Vec<String> = map.range::<str, _>(bounds).map(|p| p.0).collect();
    assert_eq!(keys, ["101", "102", "103", "104", "105"]);
}

#[test]
fn spill_boundary_round_trips_at_every_length() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast()
        .open::<Map<u32, String>>(dir.path().join("s.ferro"))
        .unwrap();
    for len in 500..530usize {
        db.update(|root| {
            root.insert(len as u32, "x".repeat(len));
        })
        .unwrap();
        let snapshot = db.read();
        assert_eq!(snapshot.get(&(len as u32)).unwrap().len(), len);
    }
    for len in 500..530usize {
        db.update(|root| {
            root.update(&(len as u32), |value| value.push('y'));
        })
        .unwrap();
    }
    for len in 500..530usize {
        db.update(|root| {
            root.update(&(len as u32), |value| value.truncate(len - 20));
        })
        .unwrap();
    }
    let snapshot = db.read();
    for len in 500..530usize {
        assert_eq!(snapshot.get(&(len as u32)).unwrap().len(), len - 20);
    }
    db.verify().unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig { cases: common::cases(500), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn set_algebra_matches_model(
        a in prop::collection::btree_set(0..200u32, 0..120),
        b in prop::collection::btree_set(0..200u32, 0..120),
    ) {
        let left: Set<u32> = a.iter().copied().collect();
        let right: Set<u32> = b.iter().copied().collect();
        let union: Vec<u32> = left.union(&right).iter().collect();
        let both: Vec<u32> = left.intersection(&right).iter().collect();
        let only: Vec<u32> = left.difference(&right).iter().collect();
        prop_assert_eq!(union, a.union(&b).copied().collect::<Vec<u32>>());
        prop_assert_eq!(both, a.intersection(&b).copied().collect::<Vec<u32>>());
        prop_assert_eq!(only, a.difference(&b).copied().collect::<Vec<u32>>());
        prop_assert_eq!(left.is_subset(&right), a.is_subset(&b));
        prop_assert_eq!(left == right, a == b);
        prop_assert_eq!(left.diff(&right).count(), a.symmetric_difference(&b).count());
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: common::cases(1000), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn range_by_with_arbitrary_predicates_stays_bounded(
        keys in prop::collection::btree_set(0..500u32, 0..200),
        low in prop::collection::vec(any::<bool>(), 64),
        high in prop::collection::vec(any::<bool>(), 64),
        monotone in any::<bool>(),
        steps in steps(),
    ) {
        let map: Map<u32, u32> = keys.iter().map(|k| (*k, *k)).collect();
        let below = |key: &u32| {
            if monotone { *key < low.len() as u32 * 7 } else { low[*key as usize % low.len()] }
        };
        let within = |key: &u32| {
            if monotone { *key < high.iter().filter(|b| **b).count() as u32 * 8 } else { high[*key as usize % high.len()] }
        };
        let forward: Vec<u32> = map.range_by(below, within).map(|p| p.0).collect();
        let mut backward: Vec<u32> = map.range_by(below, within).rev().map(|p| p.0).collect();
        backward.reverse();
        prop_assert_eq!(&forward, &backward);
        prop_assert!(forward.windows(2).all(|pair| pair[0] < pair[1]));
        prop_assert_eq!(map.range_by(below, within).len(), forward.len());
        let expected: VecDeque<u32> = forward.iter().copied().collect();
        walk(map.range_by(below, within).map(|p| p.0), expected, &steps);
        if monotone {
            let wanted: Vec<u32> = keys.iter().copied().filter(|k| !below(k) && within(k)).collect();
            prop_assert_eq!(forward, wanted);
        }
    }
}

#[test]
fn list_preserves_order_and_equality() {
    let items: Vec<String> = (0..300).map(|n| text(n, (n as usize * 7) % 700)).collect();
    let list: List<String> = items.iter().cloned().collect();
    assert_eq!(list.len(), 300);
    assert_eq!(list.first().as_deref(), items.first());
    assert_eq!(list.last().as_deref(), items.last());
    let got: Vec<String> = (&list).into_iter().map(|v| (*v).clone()).collect();
    assert_eq!(got, items);
    let mut other: List<String> = List::new();
    other.extend(items.iter().cloned());
    assert!(list == other);
    other.get_mut(5).unwrap().push('!');
    assert!(list != other);
    assert!(List::<u8>::new().last().is_none());
    assert!(List::<u8>::new().pop().is_none());
}

#[test]
fn set_and_list_wrappers_match_models() {
    let mut rng = Rng::new(11);
    let dir = tempfile::tempdir().unwrap();
    let db = fast()
        .open::<(Set<u32>, List<String>)>(dir.path().join("w.ferro"))
        .unwrap();
    let mut set_model = BTreeSet::new();
    let mut list_model: Vec<String> = Vec::new();
    for _ in 0..300 {
        db.update(|(set, list)| {
            for _ in 0..30 {
                let key = rng.below(200) as u32;
                match rng.below(9) {
                    0..=2 => assert_eq!(set.insert(key), set_model.insert(key)),
                    3 => assert_eq!(set.remove(&key), set_model.remove(&key)),
                    4 => assert_eq!(set.pop_first(), set_model.pop_first()),
                    5 => {
                        let value = text(rng.next(), rng.below(600) as usize);
                        list.push(value.clone());
                        list_model.push(value);
                    }
                    6 => assert_eq!(list.pop().map(|v| (*v).clone()), list_model.pop()),
                    7 => {
                        let keep = rng.below(list_model.len() as u64 + 2) as usize;
                        list.truncate(keep);
                        list_model.truncate(keep);
                    }
                    _ => {
                        if !list_model.is_empty() {
                            let index = rng.below(list_model.len() as u64) as usize;
                            let value = text(rng.next(), rng.below(600) as usize);
                            list.set(index, value.clone());
                            list_model[index] = value;
                        }
                    }
                }
            }
        })
        .unwrap();
        let snapshot = db.read();
        let items: Vec<u32> = snapshot.0.iter().collect();
        assert_eq!(items, set_model.iter().copied().collect::<Vec<u32>>());
        let values: Vec<String> = snapshot.1.iter().map(|v| (*v).clone()).collect();
        assert_eq!(values, list_model);
        let low = rng.below(200) as u32;
        let high = low + rng.below(50) as u32;
        let ranged: Vec<u32> = snapshot.0.range(low..high).collect();
        let wanted: Vec<u32> = set_model.range(low..high).copied().collect();
        assert_eq!(ranged, wanted);
    }
}

mod interrupted {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::ops::Bound;
    use std::panic::{self, AssertUnwindSafe};

    use ferro::codec::{Decoder, Encoder};
    use ferro::{Data, Error, Map, Options, Summary};

    use crate::common::{Rng, cases, level_of, roomy, text, uncached};

    thread_local! {
        static FUSE: Cell<Option<(u32, bool)>> = const { Cell::new(None) };
    }

    #[derive(Clone, Debug, PartialEq)]
    struct Fragile(u64, String);

    impl Data for Fragile {
        fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
            self.0.encode(w)?;
            self.1.encode(w)
        }

        fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
            let value = Fragile(u64::decode(r)?, String::decode(r)?);
            match FUSE.get() {
                Some((0, fails)) => {
                    FUSE.set(None);
                    if fails {
                        return Err(Error::Decode("the fuse blew".into()));
                    }
                    panic!("the fuse blew");
                }
                Some((left, fails)) => FUSE.set(Some((left - 1, fails))),
                None => {}
            }
            Ok(value)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Data)]
    #[ferro(transparent)]
    struct Weight(u64);

    impl Summary<u64, Fragile> for Weight {
        fn of(key: &u64, value: &Fragile) -> Self {
            Weight(key.wrapping_mul(31) ^ value.0 ^ value.1.len() as u64)
        }

        fn combine(&self, other: &Self) -> Self {
            Weight(self.0.wrapping_add(other.0))
        }
    }

    type Fragiles = Map<u64, Fragile, Weight>;
    type Model = BTreeMap<u64, Fragile>;
    type Span = (Bound<u64>, Bound<u64>);

    fn pool() -> Vec<u64> {
        let mut keys: Vec<u64> = (0..1600).collect();
        keys.extend((1600..).filter(|key| level_of(key) >= 2).take(24));
        keys.extend((1600..).filter(|key| level_of(key) >= 3).take(6));
        keys
    }

    fn pick(pool: &[u64], rng: &mut Rng) -> u64 {
        pool[rng.below(pool.len() as u64) as usize]
    }

    fn value(rng: &mut Rng) -> Fragile {
        let len = if rng.chance(10) { 600 } else { rng.below(16) };
        Fragile(rng.next(), text(rng.next(), len as usize))
    }

    fn span(pool: &[u64], rng: &mut Rng) -> Span {
        let bound = |rng: &mut Rng| match rng.below(3) {
            0 => Bound::Unbounded,
            1 => Bound::Included(pick(pool, rng)),
            _ => Bound::Excluded(pick(pool, rng)),
        };
        (bound(rng), bound(rng))
    }

    fn inside(key: u64, span: &Span) -> bool {
        let above = match span.0 {
            Bound::Included(low) => key >= low,
            Bound::Excluded(low) => key > low,
            Bound::Unbounded => true,
        };
        let below = match span.1 {
            Bound::Included(high) => key <= high,
            Bound::Excluded(high) => key < high,
            Bound::Unbounded => true,
        };
        above && below
    }

    fn fresh(model: &Model) -> Fragiles {
        let mut map = Map::default();
        for (key, value) in model.iter().rev() {
            map.insert(*key, value.clone());
        }
        map
    }

    fn same(map: &Fragiles, model: &Model, what: &str) {
        assert_eq!(map.len(), model.len(), "{what}");
        let got: Vec<(u64, Fragile)> = map.iter().map(|(k, v)| (k, (*v).clone())).collect();
        let wanted: Vec<(u64, Fragile)> = model.iter().map(|(k, v)| (*k, v.clone())).collect();
        assert_eq!(got, wanted, "{what}");
        let built = fresh(model);
        assert_eq!(map.root_hash(), built.root_hash(), "{what}");
        assert_eq!(map.summary(), built.summary(), "{what}");
    }

    fn interrupted<R>(rng: &mut Rng, op: impl FnOnce() -> R) -> Option<R> {
        if rng.chance(80) {
            let decodes = if rng.chance(50) { 48 } else { 3000 };
            FUSE.set(Some((rng.below(decodes) as u32, rng.chance(50))));
        }
        let outcome = panic::catch_unwind(AssertUnwindSafe(op));
        FUSE.set(None);
        outcome.ok()
    }

    fn rejoin(
        root: &mut Fragiles,
        model: &mut Model,
        mut tail: Fragiles,
        wanted: Model,
        rng: &mut Rng,
    ) {
        let upward = rng.chance(50);
        let joined = interrupted(rng, || {
            if upward {
                root.append(&mut tail);
            } else {
                tail.append(root);
                std::mem::swap(root, &mut tail);
            }
        });
        if joined.is_some() {
            model.extend(wanted);
            return;
        }
        same(root, model, "a failed join changed the lower map");
        same(&tail, &wanted, "a failed join changed the upper map");
    }

    fn step(root: &mut Fragiles, model: &mut Model, pool: &[u64], rng: &mut Rng) -> &'static str {
        let before = model.clone();
        let (name, done) = match rng.below(14) {
            0..=2 => {
                let (key, value) = (pick(pool, rng), value(rng));
                let done = interrupted(rng, || root.insert(key, value.clone())).is_some();
                if done {
                    model.insert(key, value);
                }
                ("insert", done)
            }
            3 | 4 => {
                let key = pick(pool, rng);
                let done = interrupted(rng, || root.remove(&key)).is_some();
                if done {
                    model.remove(&key);
                }
                ("remove", done)
            }
            5 => {
                let edits: Vec<(u64, Option<Fragile>)> = (0..rng.below(40))
                    .map(|_| (pick(pool, rng), rng.chance(50).then(|| value(rng))))
                    .collect();
                let done = interrupted(rng, || root.apply(edits.clone())).is_some();
                if done {
                    for (key, edit) in edits {
                        match edit {
                            Some(value) => model.insert(key, value),
                            None => model.remove(&key),
                        };
                    }
                }
                ("apply", done)
            }
            6 => {
                let items: Vec<(u64, Fragile)> = (0..rng.below(40))
                    .map(|_| (pick(pool, rng), value(rng)))
                    .collect();
                let done = interrupted(rng, || root.extend(items.clone())).is_some();
                if done {
                    model.extend(items);
                }
                ("extend", done)
            }
            7 => {
                let salt = rng.next() | 1;
                let doomed = |key: &u64| (key ^ salt).is_multiple_of(5);
                let done = interrupted(rng, || root.retain(|key, _| !doomed(key))).is_some();
                if done {
                    model.retain(|key, _| !doomed(key));
                }
                ("retain", done)
            }
            8 => {
                let span = span(pool, rng);
                let taken = interrupted(rng, || root.take_range::<u64, _>(span));
                let done = taken.is_some();
                if let Some(taken) = taken {
                    let wanted: Model = model
                        .iter()
                        .filter(|(key, _)| inside(**key, &span))
                        .map(|(key, value)| (*key, value.clone()))
                        .collect();
                    model.retain(|key, _| !inside(*key, &span));
                    same(&taken, &wanted, "take_range");
                }
                ("take_range", done)
            }
            9 => {
                let key = pick(pool, rng);
                let tail = interrupted(rng, || root.split_off(&key));
                let done = tail.is_some();
                if let Some(tail) = tail {
                    let wanted = model.split_off(&key);
                    same(&tail, &wanted, "split_off");
                    if rng.chance(80) {
                        rejoin(root, model, tail, wanted, rng);
                    }
                }
                ("split_off", done)
            }
            10 => {
                let mut other = Fragiles::default();
                let mut wanted = Model::new();
                for _ in 0..rng.below(30) {
                    let (key, value) = (pick(pool, rng), value(rng));
                    other.insert(key, value.clone());
                    wanted.insert(key, value);
                }
                let done = interrupted(rng, || root.append(&mut other)).is_some();
                if done {
                    model.extend(wanted);
                } else {
                    same(&other, &wanted, "a failed append changed the map appended");
                }
                ("append", done)
            }
            11 => {
                let done = interrupted(rng, || root.rewrite()).is_some();
                ("rewrite", done)
            }
            12 => {
                let first = rng.chance(50);
                let popped = interrupted(rng, || {
                    if first {
                        root.pop_first()
                    } else {
                        root.pop_last()
                    }
                });
                let done = popped.is_some();
                if done {
                    let _ = if first {
                        model.pop_first()
                    } else {
                        model.pop_last()
                    };
                }
                ("pop", done)
            }
            _ => {
                let key = pick(pool, rng);
                let grown = text(rng.next(), rng.below(700) as usize);
                let done = interrupted(rng, || root.update(&key, |value| value.1 = grown.clone()))
                    .is_some();
                if done && let Some(value) = model.get_mut(&key) {
                    value.1 = grown;
                }
                ("update", done)
            }
        };
        if !done {
            same(root, &before, &format!("a failed {name} changed the map"));
        }
        assert_eq!(root.len(), model.len(), "{name}");
        name
    }

    fn run(seed: u64, options: fn() -> Options) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interrupted.ferro");
        let open = || options().open::<Fragiles>(&path).unwrap();
        let mut db = open();
        let pool = pool();
        let mut rng = Rng::new(seed);
        let mut model = Model::new();
        db.update(|root| {
            for _ in 0..1500 {
                let (key, value) = (pick(&pool, &mut rng), value(&mut rng));
                root.insert(key, value.clone());
                model.insert(key, value);
            }
        })
        .unwrap();
        for commit in 0..12 {
            let mut working = model.clone();
            let mut trail = Vec::new();
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                db.update(|root| {
                    for _ in 0..12 {
                        trail.push(step(root, &mut working, &pool, &mut rng));
                    }
                })
                .unwrap();
            }));
            if let Err(payload) = outcome {
                eprintln!("seed {seed} commit {commit} after {trail:?}");
                panic::resume_unwind(payload);
            }
            model = working;
            same(&db.read(), &model, "the commit after failed edits");
            if commit % 4 == 3 {
                db.verify_typed().unwrap();
                drop(db);
                db = open();
                same(&db.read(), &model, "the reopened file");
            }
        }
        db.verify_typed().unwrap();
    }

    #[test]
    fn an_edit_whose_load_fails_or_panics_leaves_the_map_as_it_was() {
        for seed in 0..u64::from(cases(4)) {
            run(seed, uncached);
            run(seed, roomy);
        }
    }

    const FILLED: u64 = 4000;

    fn top() -> u32 {
        (0..FILLED).map(|key| level_of(&key)).max().unwrap()
    }

    fn blown(fails: bool, edit: impl FnOnce(&mut Fragiles)) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blown.ferro");
        let model: Model = (0..FILLED)
            .map(|key| (key, Fragile(key, String::new())))
            .collect();
        let db = roomy().open::<Fragiles>(&path).unwrap();
        db.update(|root| root.extend(model.clone())).unwrap();
        drop(db);
        let db = roomy().open::<Fragiles>(&path).unwrap();
        db.update(|root| {
            assert!(root.get(&0).is_some());
            FUSE.set(Some((0, fails)));
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| edit(root)));
            FUSE.set(None);
            assert!(outcome.is_err(), "the edit loaded no node");
            same(root, &model, "the edit whose load failed");
        })
        .unwrap();
        drop(db);
        let db = roomy().open::<Fragiles>(&path).unwrap();
        db.verify_typed().unwrap();
        same(&db.read(), &model, "the commit after the failed edit");
    }

    #[test]
    fn a_failed_load_while_an_insert_cuts_a_subtree_keeps_every_entry() {
        let key = (FILLED..).find(|key| level_of(key) == top()).unwrap();
        for fails in [false, true] {
            blown(fails, |root| {
                root.insert(key, Fragile(key, String::new()));
            });
        }
    }

    #[test]
    fn a_failed_load_while_an_insert_above_the_root_cuts_the_tree_keeps_every_entry() {
        let key = (FILLED..).find(|key| level_of(key) > top()).unwrap();
        for fails in [false, true] {
            blown(fails, |root| {
                root.insert(key, Fragile(key, String::new()));
            });
        }
    }

    #[test]
    fn a_failed_load_while_a_remove_joins_two_subtrees_keeps_every_entry() {
        let key = (1..FILLED).find(|key| level_of(key) == top()).unwrap();
        for fails in [false, true] {
            blown(fails, |root| {
                root.remove(&key);
            });
        }
    }
}

mod summarized {
    use std::collections::BTreeMap;
    use std::ops::{Bound, ControlFlow};
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::OnceLock;

    use ferro::{Change, Conflict, Data, Keep, Map, Options, Summary, Val, hash_of};

    use crate::common::{Rng, cases, fast, level_of, roomy, text, uncached};

    #[derive(Clone, Debug, PartialEq, Data)]
    struct Tally {
        count: u64,
        bytes: u64,
        first: u32,
        last: u32,
        digest: u64,
        scale: u64,
    }

    impl Summary<u32, String> for Tally {
        fn of(key: &u32, value: &String) -> Self {
            let mut digest = 0xcbf2_9ce4_8422_2325 ^ u64::from(*key);
            for byte in value.bytes() {
                digest = (digest ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
            }
            Tally {
                count: 1,
                bytes: value.len() as u64,
                first: *key,
                last: *key,
                digest,
                scale: 0x9e37_79b9_7f4a_7c15,
            }
        }

        fn combine(&self, other: &Self) -> Self {
            Tally {
                count: self.count + other.count,
                bytes: self.bytes + other.bytes,
                first: self.first,
                last: other.last,
                digest: self
                    .digest
                    .wrapping_mul(other.scale)
                    .wrapping_add(other.digest),
                scale: self.scale.wrapping_mul(other.scale),
            }
        }
    }

    type Tallied = Map<u32, String, Tally>;
    type Model = BTreeMap<u32, String>;
    type Span = (Bound<u32>, Bound<u32>);

    fn deep_keys() -> &'static Vec<u32> {
        static KEYS: OnceLock<Vec<u32>> = OnceLock::new();
        KEYS.get_or_init(|| {
            let mut picked = Vec::new();
            let (mut zero, mut one) = (0, 0);
            for key in 0..60_000u32 {
                match level_of(&key) {
                    0 if zero < 120 || key % 500 == 0 => {
                        zero += 1;
                        picked.push(key);
                    }
                    1 if one < 120 => {
                        one += 1;
                        picked.push(key);
                    }
                    0 | 1 => {}
                    _ => picked.push(key),
                }
            }
            picked.sort_unstable();
            picked
        })
    }

    struct Pool {
        keys: Vec<u32>,
    }

    impl Pool {
        fn dense(n: u32) -> Self {
            Pool {
                keys: (0..n).map(|k| k * 2 + 1).collect(),
            }
        }

        fn deep() -> Self {
            Pool {
                keys: deep_keys().clone(),
            }
        }

        fn key(&self, rng: &mut Rng) -> u32 {
            self.keys[rng.below(self.keys.len() as u64) as usize]
        }

        fn probe(&self, rng: &mut Rng) -> u32 {
            let key = self.key(rng);
            match rng.below(6) {
                0 => key.saturating_sub(1),
                1 => key.saturating_add(1),
                2 => 0,
                3 => u32::MAX,
                _ => key,
            }
        }

        fn bound(&self, rng: &mut Rng) -> Bound<u32> {
            match rng.below(5) {
                0 => Bound::Unbounded,
                1 | 2 => Bound::Included(self.probe(rng)),
                _ => Bound::Excluded(self.probe(rng)),
            }
        }

        fn span(&self, rng: &mut Rng) -> Span {
            (self.bound(rng), self.bound(rng))
        }
    }

    fn value(rng: &mut Rng) -> String {
        let len = match rng.below(20) {
            0..=11 => rng.below(24),
            12..=16 => 500 + rng.below(24),
            _ => 524 + rng.below(2000),
        };
        text(rng.next(), len as usize)
    }

    fn in_span(key: u32, span: &Span) -> bool {
        let above = match span.0 {
            Bound::Included(bound) => key >= bound,
            Bound::Excluded(bound) => key > bound,
            Bound::Unbounded => true,
        };
        let below = match span.1 {
            Bound::Included(bound) => key <= bound,
            Bound::Excluded(bound) => key < bound,
            Bound::Unbounded => true,
        };
        above && below
    }

    fn spanned(model: &Model, span: &Span) -> Vec<(u32, String)> {
        model
            .iter()
            .filter(|(key, _)| in_span(**key, span))
            .map(|(key, value)| (*key, value.clone()))
            .collect()
    }

    fn folded(model: &Model, span: &Span) -> Option<Tally> {
        model
            .iter()
            .filter(|(key, _)| in_span(**key, span))
            .map(|(key, value)| Tally::of(key, value))
            .reduce(|total, next| total.combine(&next))
    }

    fn owned(pair: Option<(u32, Val<String>)>) -> Option<(u32, String)> {
        pair.map(|(key, value)| (key, (*value).clone()))
    }

    fn fresh(model: &Model) -> Tallied {
        let mut map = Map::default();
        for (key, value) in model.iter().rev() {
            map.insert(*key, value.clone());
        }
        map
    }

    fn check_range(map: &Tallied, model: &Model, span: Span, rng: &mut Rng) {
        let want = spanned(model, &span);
        let got: Vec<(u32, String)> = map
            .range::<u32, _>(span)
            .map(|(key, value)| (key, (*value).clone()))
            .collect();
        assert_eq!(got, want, "range {span:?}");
        let back: Vec<u32> = map.range::<u32, _>(span).rev().map(|pair| pair.0).collect();
        let wanted_back: Vec<u32> = want.iter().rev().map(|pair| pair.0).collect();
        assert_eq!(back, wanted_back, "reversed range {span:?}");
        let mut iter = map.range::<u32, _>(span);
        let mut left: std::collections::VecDeque<(u32, String)> = want.iter().cloned().collect();
        assert_eq!(iter.len(), left.len());
        for _ in 0..rng.below(12) {
            match rng.below(6) {
                0 => assert_eq!(owned(iter.next()), left.pop_front(), "{span:?}"),
                1 => assert_eq!(owned(iter.next_back()), left.pop_back(), "{span:?}"),
                2 => {
                    let got = iter.next_ref().map(|(key, value)| (*key, value.clone()));
                    assert_eq!(got, left.pop_front(), "{span:?}");
                }
                3 => {
                    let n = rng.below(4) as usize;
                    let wanted = if n < left.len() {
                        left.drain(..n).for_each(drop);
                        left.pop_front()
                    } else {
                        left.clear();
                        None
                    };
                    assert_eq!(owned(iter.nth(n)), wanted, "{span:?}");
                }
                4 => {
                    let n = rng.below(4) as usize;
                    let wanted = if n < left.len() {
                        left.truncate(left.len() - n);
                        left.pop_back()
                    } else {
                        left.clear();
                        None
                    };
                    assert_eq!(owned(iter.nth_back(n)), wanted, "{span:?}");
                }
                _ => {
                    let got = iter.next_back().map(|(key, _)| key);
                    assert_eq!(got, left.pop_back().map(|pair| pair.0), "{span:?}");
                }
            }
            assert_eq!(iter.len(), left.len(), "{span:?}");
        }
        match rng.below(3) {
            0 => {
                let mut seen = Vec::new();
                iter.visit(|key, value| seen.push((*key, value.clone())));
                assert_eq!(seen, Vec::from(left), "{span:?}");
            }
            1 => {
                let rest = iter.fold(Vec::new(), |mut rest, (key, value)| {
                    rest.push((key, (*value).clone()));
                    rest
                });
                assert_eq!(rest, Vec::from(left), "{span:?}");
            }
            _ => {
                let rest: Vec<(u32, String)> = iter.map(|(k, v)| (k, (*v).clone())).collect();
                assert_eq!(rest, Vec::from(left), "{span:?}");
            }
        }
        let stop = rng.below(want.len() as u64 + 2) as usize;
        let mut seen = Vec::new();
        let flow = map.try_for_each_range::<u32, _, u32>(span, |key, value| {
            if seen.len() == stop {
                return ControlFlow::Break(*key);
            }
            seen.push((*key, value.clone()));
            ControlFlow::Continue(())
        });
        let expected_flow = match want.get(stop) {
            Some((key, _)) => ControlFlow::Break(*key),
            None => ControlFlow::Continue(()),
        };
        assert_eq!(flow, expected_flow, "try_for_each_range {span:?}");
        assert_eq!(seen, want[..stop.min(want.len())], "{span:?}");
        assert_eq!(
            map.range_summary::<u32, _>(span),
            folded(model, &span),
            "range_summary {span:?}"
        );
        let by = map.summary_by(
            |key| !in_span(*key, &(span.0, Bound::Unbounded)),
            |key| in_span(*key, &(Bound::Unbounded, span.1)),
        );
        assert_eq!(by, folded(model, &span), "summary_by {span:?}");
    }

    fn check_cursor(map: &Tallied, model: &Model, pool: &Pool, rng: &mut Rng) {
        let mut cursor = map.cursor();
        let mut at: Option<u32> = None;
        for _ in 0..rng.below(30) {
            match rng.below(5) {
                0 => {
                    let key = pool.probe(rng);
                    let found = cursor.seek(&key);
                    assert_eq!(found, model.contains_key(&key), "seek {key}");
                    at = model.range(key..).next().map(|pair| *pair.0);
                }
                1 => {
                    let key = pool.probe(rng);
                    let found = cursor.seek_back(&key);
                    assert_eq!(found, model.contains_key(&key), "seek_back {key}");
                    at = model.range(..=key).next_back().map(|pair| *pair.0);
                }
                2 | 3 => {
                    let moved = cursor.advance().copied();
                    if let Some(key) = at {
                        at = model
                            .range((Bound::Excluded(key), Bound::Unbounded))
                            .next()
                            .map(|pair| *pair.0);
                    }
                    assert_eq!(moved, at, "advance");
                }
                _ => {
                    cursor.rewind();
                    at = model.keys().next().copied();
                }
            }
            assert_eq!(cursor.key().copied(), at, "cursor key");
            assert_eq!(
                cursor.value().map(|value| (*value).clone()),
                at.map(|key| model[&key].clone()),
                "cursor value"
            );
            assert_eq!(
                cursor.read(|key, value| (*key, value.len())),
                at.map(|key| (key, model[&key].len())),
                "cursor read"
            );
        }
    }

    fn check_reads(map: &Tallied, model: &Model, pool: &Pool, rng: &mut Rng) {
        assert_eq!(map.len(), model.len());
        assert_eq!(map.is_empty(), model.is_empty());
        for _ in 0..6 {
            let key = pool.probe(rng);
            assert_eq!(
                map.get(&key).map(|value| (*value).clone()),
                model.get(&key).cloned(),
                "get {key}"
            );
            assert_eq!(map.contains_key(&key), model.contains_key(&key));
            assert_eq!(
                map.value_hash(&key),
                model.get(&key).map(hash_of),
                "value_hash {key}"
            );
            assert_eq!(
                owned(map.floor(&key)),
                model
                    .range(..=key)
                    .next_back()
                    .map(|(key, value)| (*key, value.clone())),
                "floor {key}"
            );
        }
        let probes: Vec<u32> = (0..rng.below(12)).map(|_| pool.probe(rng)).collect();
        let many: Vec<(u32, Option<String>)> = map
            .get_many(probes.iter().copied())
            .map(|(key, value)| (key, value.map(|value| (*value).clone())))
            .collect();
        let wanted: Vec<(u32, Option<String>)> = probes
            .iter()
            .map(|key| (*key, model.get(key).cloned()))
            .collect();
        assert_eq!(many, wanted, "get_many");
        let found: Vec<bool> = map.contains_many(&probes).map(|pair| pair.1).collect();
        let wanted: Vec<bool> = probes.iter().map(|key| model.contains_key(key)).collect();
        assert_eq!(found, wanted, "contains_many");
        assert_eq!(
            owned(map.first()),
            model.first_key_value().map(|(k, v)| (*k, v.clone()))
        );
        assert_eq!(
            owned(map.last()),
            model.last_key_value().map(|(k, v)| (*k, v.clone()))
        );
        let index = rng.below(model.len() as u64 + 2) as usize;
        assert_eq!(
            owned(map.nth(index)),
            model.iter().nth(index).map(|(k, v)| (*k, v.clone())),
            "nth {index}"
        );
        let everything = (Bound::Unbounded, Bound::Unbounded);
        assert_eq!(map.summary(), folded(model, &everything), "summary");
        for _ in 0..3 {
            let span = pool.span(rng);
            check_range(map, model, span, rng);
        }
        check_cursor(map, model, pool, rng);
    }

    fn check_full(map: &Tallied, model: &Model) {
        let all: Vec<(u32, String)> = map.iter().map(|(k, v)| (k, (*v).clone())).collect();
        let wanted: Vec<(u32, String)> = model.iter().map(|(k, v)| (*k, v.clone())).collect();
        assert_eq!(all, wanted);
        let mut lent = Vec::new();
        map.for_each(|key, value| lent.push((*key, value.clone())));
        assert_eq!(lent, wanted);
        let built = fresh(model);
        assert_eq!(map.root_hash(), built.root_hash(), "canonical hash");
        assert!(*map == built);
        let collected: Tallied = model.iter().map(|(k, v)| (*k, v.clone())).collect();
        assert_eq!(collected.root_hash(), built.root_hash(), "collected hash");
    }

    fn model_diff(old: &Model, new: &Model) -> Vec<(u32, Option<String>, Option<String>)> {
        let mut keys: Vec<u32> = old.keys().chain(new.keys()).copied().collect();
        keys.sort_unstable();
        keys.dedup();
        keys.into_iter()
            .filter_map(|key| {
                let (a, b) = (old.get(&key).cloned(), new.get(&key).cloned());
                (a != b).then_some((key, a, b))
            })
            .collect()
    }

    fn check_diff(old: &Tallied, old_model: &Model, new: &Tallied, new_model: &Model) {
        let got: Vec<(u32, Option<String>, Option<String>)> = old
            .diff(new)
            .map(|change| {
                let before = change.before().map(|value| (**value).clone());
                let after = change.after().map(|value| (**value).clone());
                match &change {
                    Change::Added(..) => assert!(before.is_none() && after.is_some()),
                    Change::Removed(..) => assert!(before.is_some() && after.is_none()),
                    Change::Changed(..) => assert!(before.is_some() && after.is_some()),
                }
                (*change.key(), before, after)
            })
            .collect();
        assert_eq!(got, model_diff(old_model, new_model), "diff");
    }

    fn resolve(conflict: &Conflict<u32, String>) -> Option<String> {
        match conflict.key % 4 {
            0 => conflict.ours.as_deref().cloned(),
            1 => conflict.theirs.as_deref().cloned(),
            2 => None,
            _ => Some(format!(
                "{}|{}|{}",
                conflict.base.as_deref().map_or(0, String::len),
                conflict.ours.as_deref().map_or(0, String::len),
                conflict.theirs.as_deref().map_or(0, String::len),
            )),
        }
    }

    fn model_merge(ours: &mut Model, base: &Model, theirs: &Model) -> usize {
        let mut written = 0;
        for (key, before, after) in model_diff(base, theirs) {
            let mine = ours.get(&key).cloned();
            if mine == after {
                continue;
            }
            let value = if mine == before {
                after
            } else {
                let conflict = Conflict {
                    key,
                    base: before.map(Val::new),
                    ours: mine.map(Val::new),
                    theirs: after.map(Val::new),
                };
                resolve(&conflict)
            };
            match value {
                Some(value) => ours.insert(key, value),
                None => ours.remove(&key),
            };
            written += 1;
        }
        written
    }

    fn edit_copy(map: &mut Tallied, model: &mut Model, pool: &Pool, rng: &mut Rng) {
        for _ in 0..rng.below(20) {
            let key = pool.key(rng);
            if rng.chance(60) {
                let value = value(rng);
                map.insert(key, value.clone());
                model.insert(key, value);
            } else {
                map.remove(&key);
                model.remove(&key);
            }
        }
    }

    fn mutate(
        map: &mut Tallied,
        model: &mut Model,
        pool: &Pool,
        rng: &mut Rng,
        earlier: &[(Tallied, Model)],
    ) -> &'static str {
        match rng.below(100) {
            0..=24 => {
                let key = pool.key(rng);
                let value = value(rng);
                let old = map.insert(key, value.clone()).map(|v| (*v).clone());
                assert_eq!(old, model.insert(key, value));
                "insert"
            }
            25..=37 => {
                let key = pool.probe(rng);
                let old = map.remove(&key).map(|v| (*v).clone());
                assert_eq!(old, model.remove(&key));
                "remove"
            }
            38..=42 => {
                let key = pool.probe(rng);
                let grow = value(rng);
                match (map.get_mut(&key), model.get_mut(&key)) {
                    (Some(a), Some(b)) => {
                        assert_eq!(a, b);
                        if grow.len().is_multiple_of(2) {
                            a.push_str(&grow);
                            b.push_str(&grow);
                        } else {
                            a.truncate(grow.len() % 7);
                            b.truncate(grow.len() % 7);
                        }
                    }
                    (None, None) => {}
                    (a, b) => panic!("get_mut {key}: {a:?} {b:?}"),
                }
                "get_mut"
            }
            43..=48 => {
                let key = pool.probe(rng);
                let target = match rng.below(3) {
                    0 => rng.below(20) as usize,
                    1 => 505 + rng.below(16) as usize,
                    _ => 600 + rng.below(1000) as usize,
                };
                let change = |value: &mut String| {
                    if value.len() > target {
                        value.truncate(target);
                    } else {
                        while value.len() < target {
                            value.push('u');
                        }
                    }
                    value.len()
                };
                let got = map.update(&key, change);
                let wanted = model.get_mut(&key).map(change);
                assert_eq!(got, wanted);
                "update"
            }
            49..=51 => {
                let key = pool.key(rng);
                let value = value(rng);
                let got = map.get_or_insert_with(key, || value.clone()).clone();
                let wanted = model.entry(key).or_insert(value).clone();
                assert_eq!(got, wanted);
                "get_or_insert_with"
            }
            52..=53 => {
                if rng.chance(50) {
                    assert_eq!(owned(map.pop_first()), model.pop_first());
                } else {
                    assert_eq!(owned(map.pop_last()), model.pop_last());
                }
                "pop"
            }
            54..=55 => {
                let modulus = 2 + rng.below(6);
                let salt = rng.next();
                let doomed = |key: &u32| (u64::from(*key) ^ salt).is_multiple_of(modulus);
                map.retain(|key, _| !doomed(key));
                model.retain(|key, _| !doomed(key));
                "retain"
            }
            56..=59 => {
                let span = pool.span(rng);
                let wanted: Model = spanned(model, &span).into_iter().collect();
                model.retain(|key, _| !in_span(*key, &span));
                if rng.chance(50) {
                    assert_eq!(map.remove_range::<u32, _>(span), wanted.len());
                } else {
                    let taken = map.take_range::<u32, _>(span);
                    assert_eq!(taken.len(), wanted.len());
                    check_full(&taken, &wanted);
                }
                "remove_range"
            }
            60..=63 => {
                let key = pool.probe(rng);
                let mut tail = map.split_off(&key);
                let mut wanted = model.split_off(&key);
                check_full(&tail, &wanted);
                if rng.chance(70) {
                    map.append(&mut tail);
                    model.append(&mut wanted);
                    assert!(tail.is_empty());
                }
                "split_off"
            }
            64..=67 => {
                let mut other: Tallied = Map::default();
                let mut wanted = Model::new();
                let window = pool.span(rng);
                for _ in 0..rng.below(30) {
                    let key = pool.key(rng);
                    if in_span(key, &window) {
                        let value = value(rng);
                        other.insert(key, value.clone());
                        wanted.insert(key, value);
                    }
                }
                if rng.chance(30)
                    && let Some((source, _)) = earlier.last()
                {
                    let mut piece = source.clone().take_range::<u32, _>(window);
                    let piece_model: Model = piece.iter().map(|(k, v)| (k, (*v).clone())).collect();
                    other.append(&mut piece);
                    for (key, value) in piece_model {
                        wanted.insert(key, value);
                    }
                }
                map.append(&mut other);
                assert!(other.is_empty());
                model.append(&mut wanted);
                "append"
            }
            68..=73 => {
                let items: Vec<(u32, String)> = (0..rng.below(60))
                    .map(|_| (pool.key(rng), value(rng)))
                    .collect();
                map.extend(items.clone());
                model.extend(items);
                "extend"
            }
            74..=81 => {
                let edits: Vec<(u32, Option<String>)> = (0..rng.below(60))
                    .map(|_| {
                        let key = if rng.chance(70) {
                            pool.key(rng)
                        } else {
                            pool.probe(rng)
                        };
                        (key, rng.chance(60).then(|| value(rng)))
                    })
                    .collect();
                map.apply(edits.clone());
                for (key, edit) in edits {
                    match edit {
                        Some(value) => model.insert(key, value),
                        None => model.remove(&key),
                    };
                }
                "apply"
            }
            82..=83 => {
                map.rewrite();
                "rewrite"
            }
            84..=89 => {
                if earlier.is_empty() {
                    return "merge skipped";
                }
                let (base, base_model) = &earlier[rng.below(earlier.len() as u64) as usize];
                let mut theirs = base.clone();
                let mut theirs_model = base_model.clone();
                edit_copy(&mut theirs, &mut theirs_model, pool, rng);
                let written = map.merge(base, &theirs, |conflict| resolve(&conflict));
                let wanted = model_merge(model, base_model, &theirs_model);
                assert_eq!(written, wanted, "merge written");
                "merge"
            }
            90..=91 => {
                if earlier.is_empty() {
                    return "try_merge skipped";
                }
                let (base, base_model) = &earlier[rng.below(earlier.len() as u64) as usize];
                let mut theirs = base.clone();
                let mut theirs_model = base_model.clone();
                edit_copy(&mut theirs, &mut theirs_model, pool, rng);
                let before = map.root_hash();
                let refuse = rng.chance(50);
                let mut conflicts = 0;
                let outcome = map.try_merge(base, &theirs, |conflict| {
                    conflicts += 1;
                    if refuse {
                        Err(conflict.key)
                    } else {
                        Ok(resolve(&conflict))
                    }
                });
                match outcome {
                    Ok(written) => {
                        let wanted = model_merge(model, base_model, &theirs_model);
                        assert_eq!(written, wanted, "try_merge written");
                    }
                    Err(_) => {
                        assert!(refuse && conflicts == 1);
                        assert_eq!(map.root_hash(), before, "refused merge left changes");
                    }
                }
                "try_merge"
            }
            92 => {
                if rng.chance(20) {
                    map.clear();
                    model.clear();
                }
                "clear"
            }
            _ => {
                let key = pool.probe(rng);
                assert_eq!(map.update(&key, |_| ()).is_some(), model.contains_key(&key));
                "update noop"
            }
        }
    }

    fn memory_run(seed: u64, pool: &Pool, steps: usize) {
        let mut rng = Rng::new(seed);
        let mut map: Tallied = Map::default();
        let mut model = Model::new();
        let mut earlier: Vec<(Tallied, Model)> = Vec::new();
        for step in 0..steps {
            let op = mutate(&mut map, &mut model, pool, &mut rng, &earlier);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                check_reads(&map, &model, pool, &mut Rng::new(seed ^ step as u64));
            }));
            if let Err(payload) = result {
                eprintln!("seed {seed} step {step} after {op}");
                std::panic::resume_unwind(payload);
            }
            if rng.chance(8) {
                check_full(&map, &model);
                if let Some((old, old_model)) =
                    earlier.get(rng.below(earlier.len() as u64 + 1) as usize)
                {
                    check_diff(old, old_model, &map, &model);
                    check_diff(&map, &model, old, old_model);
                }
                earlier.push((map.clone(), model.clone()));
                if earlier.len() > 8 {
                    earlier.remove(0);
                }
            }
        }
        check_full(&map, &model);
    }

    fn stored_run(seed: u64, pool: &Pool, options: fn() -> Options, commits: usize) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summarized.ferro");
        let open = || options().open::<Tallied>(&path).unwrap();
        let mut db = open();
        let mut rng = Rng::new(seed);
        let mut model = Model::new();
        let mut earlier: Vec<(Tallied, Model)> = Vec::new();
        let mut versions: Vec<(u64, Model)> = Vec::new();
        for commit in 0..commits {
            let ops = rng.below(25) as usize;
            let mut working = model.clone();
            let mut trail = Vec::new();
            db.update(|root| {
                for _ in 0..ops {
                    let op = mutate(root, &mut working, pool, &mut rng, &earlier);
                    trail.push(op);
                    if rng.chance(15) {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            check_reads(root, &working, pool, &mut Rng::new(seed ^ commit as u64));
                        }));
                        if let Err(payload) = result {
                            eprintln!("seed {seed} commit {commit} dirty after {trail:?}");
                            std::panic::resume_unwind(payload);
                        }
                    }
                }
            })
            .unwrap();
            model = working;
            let snapshot = db.read();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                check_reads(&snapshot, &model, pool, &mut Rng::new(seed ^ commit as u64));
                if commit % 5 == 0 {
                    check_full(&snapshot, &model);
                }
                if let Some((old, old_model)) =
                    earlier.get(rng.below(earlier.len() as u64 + 1) as usize)
                {
                    check_diff(old, old_model, &snapshot, &model);
                    check_diff(&snapshot, &model, old, old_model);
                }
            }));
            if let Err(payload) = result {
                eprintln!("seed {seed} commit {commit} after {trail:?}");
                std::panic::resume_unwind(payload);
            }
            earlier.push(((*snapshot).clone(), model.clone()));
            if earlier.len() > 6 {
                earlier.remove(0);
            }
            drop(snapshot);
            versions.push((db.version(), model.clone()));
            match rng.below(40) {
                0 => {
                    let version = db.version();
                    drop(db);
                    db = open();
                    assert_eq!(db.version(), version);
                    check_full(&db.read(), &model);
                }
                1 => {
                    db.verify_typed().unwrap();
                }
                2 => {
                    db.compact(Keep::Last(4)).unwrap();
                    versions.retain(|(version, _)| *version + 4 > db.version());
                    check_full(&db.read(), &model);
                }
                3 => {
                    for (version, wanted) in versions.iter().rev().take(4) {
                        if let Ok(old) = db.at(*version) {
                            check_full(&old, wanted);
                            check_diff(&old, wanted, &db.read(), &model);
                        }
                    }
                }
                _ => {}
            }
        }
        db.verify_typed().unwrap();
        db.compact(Keep::Last(2)).unwrap();
        db.verify_typed().unwrap();
        check_full(&db.read(), &model);
        drop(db);
        let db = open();
        check_full(&db.read(), &model);
        db.verify_typed().unwrap();
    }

    fn seeds(base: u64, count: u32) -> impl Iterator<Item = u64> {
        (0..u64::from(cases(count))).map(move |n| base + n)
    }

    #[test]
    fn maps_in_memory_match_the_model() {
        for seed in seeds(1000, 40) {
            memory_run(seed, &Pool::dense(24), 300);
        }
    }

    #[test]
    fn deep_maps_in_memory_match_the_model() {
        for seed in seeds(2000, 12) {
            memory_run(seed, &Pool::deep(), 500);
        }
    }

    #[test]
    fn stored_maps_match_the_model() {
        for seed in seeds(3000, 12) {
            stored_run(seed, &Pool::dense(40), fast, 60);
        }
    }

    #[test]
    fn deep_stored_maps_match_the_model_uncached() {
        for seed in seeds(4000, 6) {
            stored_run(seed, &Pool::deep(), uncached, 60);
        }
    }

    #[test]
    fn deep_stored_maps_match_the_model_in_a_roomy_cache() {
        for seed in seeds(5000, 6) {
            stored_run(seed, &Pool::deep(), roomy, 60);
        }
    }

    fn large_edit(root: &mut Tallied, working: &mut Model, pool: &Pool, rng: &mut Rng) {
        let key = pool.key(rng);
        match rng.below(10) {
            0..=4 => {
                let value = value(rng);
                root.insert(key, value.clone());
                working.insert(key, value);
            }
            5..=6 => {
                root.remove(&key);
                working.remove(&key);
            }
            7 => {
                let grow = rng.below(900) as usize;
                let change = |value: &mut String| {
                    if grow.is_multiple_of(2) {
                        value.push_str(&"g".repeat(grow));
                    } else {
                        value.truncate(grow % 9);
                    }
                };
                root.update(&key, change);
                if let Some(value) = working.get_mut(&key) {
                    change(value);
                }
            }
            8 => {
                if let (Some(a), Some(b)) = (root.get_mut(&key), working.get_mut(&key)) {
                    a.push('m');
                    b.push('m');
                }
            }
            _ => {
                let items: Vec<(u32, Option<String>)> = (0..rng.below(30))
                    .map(|_| (pool.key(rng), rng.chance(50).then(|| value(rng))))
                    .collect();
                root.apply(items.clone());
                for (key, edit) in items {
                    match edit {
                        Some(value) => working.insert(key, value),
                        None => working.remove(&key),
                    };
                }
            }
        }
    }

    fn large_run(seed: u64, options: fn() -> Options, keys: u32, rounds: usize) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.ferro");
        let open = || options().open::<Tallied>(&path).unwrap();
        let mut db = open();
        let mut rng = Rng::new(seed);
        let mut model = Model::new();
        let pool = Pool::dense(keys);
        let mut earlier: Vec<(Tallied, Model)> = Vec::new();
        for round in 0..rounds {
            let mut working = model.clone();
            let edits = match rng.below(4) {
                0 => u64::from(keys) / 2,
                1 => u64::from(keys) / 8,
                2 => 40,
                _ => 3,
            };
            db.update(|root| {
                for _ in 0..edits {
                    large_edit(root, &mut working, &pool, &mut rng);
                }
                if rng.chance(30) {
                    check_reads(root, &working, &pool, &mut Rng::new(seed ^ round as u64));
                }
            })
            .unwrap();
            model = working;
            let snapshot = db.read();
            check_reads(&snapshot, &model, &pool, &mut Rng::new(seed ^ round as u64));
            if rng.chance(30) {
                check_full(&snapshot, &model);
            }
            if let Some((old, old_model)) = earlier.last() {
                check_diff(old, old_model, &snapshot, &model);
            }
            earlier.push(((*snapshot).clone(), model.clone()));
            if earlier.len() > 3 {
                earlier.remove(0);
            }
            drop(snapshot);
            match rng.below(8) {
                0 => {
                    drop(db);
                    db = open();
                    check_reads(&db.read(), &model, &pool, &mut rng);
                }
                1 => {
                    db.verify_typed().unwrap();
                }
                2 => {
                    db.compact(Keep::Last(2)).unwrap();
                    check_reads(&db.read(), &model, &pool, &mut rng);
                }
                _ => {}
            }
        }
        db.verify_typed().unwrap();
        check_full(&db.read(), &model);
        drop(db);
        let db = uncached().open::<Tallied>(&path).unwrap();
        check_full(&db.read(), &model);
        db.verify_typed().unwrap();
    }

    #[test]
    fn commits_flushed_on_many_threads_match_the_model() {
        for seed in seeds(6000, 2) {
            large_run(seed, fast, 30_000, 12);
        }
    }

    #[test]
    fn commits_flushed_on_many_threads_match_the_model_in_a_roomy_cache() {
        for seed in seeds(7000, 2) {
            large_run(seed, roomy, 30_000, 12);
        }
    }

    enum Reader {
        Walk(
            ferro::map::Iter<u32, String, Tally>,
            std::collections::VecDeque<(u32, String)>,
        ),
        Seek(ferro::Cursor<u32, String, Tally>, Model, Option<u32>),
        Held(Val<String>, String),
        Changes(
            ferro::map::Diff<u32, String, Tally>,
            Vec<(u32, Option<String>, Option<String>)>,
        ),
    }

    fn reader(
        map: &Tallied,
        model: &Model,
        pool: &Pool,
        rng: &mut Rng,
        older: Option<&(Tallied, Model)>,
    ) -> Option<Reader> {
        Some(match rng.below(4) {
            0 => {
                let span = pool.span(rng);
                Reader::Walk(
                    map.range::<u32, _>(span),
                    spanned(model, &span).into_iter().collect(),
                )
            }
            1 => {
                let mut cursor = map.cursor();
                let key = pool.probe(rng);
                cursor.seek(&key);
                let at = model.range(key..).next().map(|pair| *pair.0);
                Reader::Seek(cursor, model.clone(), at)
            }
            2 => {
                let key = pool.key(rng);
                let value = map.get(&key)?;
                Reader::Held(value, model[&key].clone())
            }
            _ => {
                let (old, old_model) = older?;
                Reader::Changes(old.diff(map), model_diff(old_model, model))
            }
        })
    }

    fn poke(reader: &mut Reader, pool: &Pool, rng: &mut Rng) -> bool {
        match reader {
            Reader::Walk(iter, left) => {
                for _ in 0..rng.below(4) {
                    if rng.chance(50) {
                        assert_eq!(owned(iter.next()), left.pop_front(), "held walk");
                    } else {
                        assert_eq!(owned(iter.next_back()), left.pop_back(), "held walk back");
                    }
                    assert_eq!(iter.len(), left.len());
                }
                left.is_empty()
            }
            Reader::Seek(cursor, model, at) => {
                if rng.chance(30) {
                    let key = pool.probe(rng);
                    assert_eq!(cursor.seek_back(&key), model.contains_key(&key));
                    *at = model.range(..=key).next_back().map(|pair| *pair.0);
                } else {
                    let moved = cursor.advance().copied();
                    if let Some(key) = *at {
                        *at = model
                            .range((Bound::Excluded(key), Bound::Unbounded))
                            .next()
                            .map(|pair| *pair.0);
                    }
                    assert_eq!(moved, *at, "held cursor");
                }
                assert_eq!(
                    cursor.value().map(|value| (*value).clone()),
                    at.map(|key| model[&key].clone()),
                    "held cursor value"
                );
                at.is_none() && rng.chance(50)
            }
            Reader::Held(value, wanted) => {
                assert_eq!(**value, *wanted, "held value");
                rng.chance(20)
            }
            Reader::Changes(diff, wanted) => {
                let next = diff.next().map(|change| {
                    (
                        *change.key(),
                        change.before().map(|value| (**value).clone()),
                        change.after().map(|value| (**value).clone()),
                    )
                });
                let expected = (!wanted.is_empty()).then(|| wanted.remove(0));
                assert_eq!(next, expected, "held diff");
                expected.is_none()
            }
        }
    }

    fn poke_all(readers: &mut Vec<Reader>, pool: &Pool, rng: &mut Rng, percent: u64) {
        let mut index = 0;
        while index < readers.len() {
            if rng.chance(percent) && poke(&mut readers[index], pool, rng) {
                readers.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn readers_run(seed: u64, pool: &Pool, steps: usize) {
        let mut rng = Rng::new(seed);
        let mut map: Tallied = Map::default();
        let mut model = Model::new();
        let mut earlier: Vec<(Tallied, Model)> = Vec::new();
        let mut readers: Vec<Reader> = Vec::new();
        for _ in 0..steps {
            mutate(&mut map, &mut model, pool, &mut rng, &earlier);
            if rng.chance(40)
                && let Some(made) = reader(&map, &model, pool, &mut rng, earlier.last())
            {
                readers.push(made);
            }
            poke_all(&mut readers, pool, &mut rng, 50);
            if rng.chance(10) {
                earlier.push((map.clone(), model.clone()));
                if earlier.len() > 4 {
                    earlier.remove(0);
                }
            }
        }
    }

    fn readers_stored_run(seed: u64, pool: &Pool, options: fn() -> Options, commits: usize) {
        let dir = tempfile::tempdir().unwrap();
        let db = options()
            .open::<Tallied>(dir.path().join("readers.ferro"))
            .unwrap();
        let mut rng = Rng::new(seed);
        let mut model = Model::new();
        let mut earlier: Vec<(Tallied, Model)> = Vec::new();
        let mut readers: Vec<Reader> = Vec::new();
        for _ in 0..commits {
            let mut working = model.clone();
            db.update(|root| {
                for _ in 0..rng.below(20) {
                    mutate(root, &mut working, pool, &mut rng, &earlier);
                    if rng.chance(30)
                        && let Some(made) = reader(root, &working, pool, &mut rng, earlier.last())
                    {
                        readers.push(made);
                    }
                    poke_all(&mut readers, pool, &mut rng, 30);
                }
            })
            .unwrap();
            model = working;
            let snapshot = db.read();
            if let Some(made) = reader(&snapshot, &model, pool, &mut rng, earlier.last()) {
                readers.push(made);
            }
            earlier.push(((*snapshot).clone(), model.clone()));
            if earlier.len() > 4 {
                earlier.remove(0);
            }
        }
        for mut reader in readers {
            for _ in 0..50 {
                if poke(&mut reader, pool, &mut rng) {
                    break;
                }
            }
        }
    }

    #[test]
    fn readers_keep_the_version_they_were_made_on() {
        for seed in seeds(8000, 30) {
            readers_run(seed, &Pool::dense(30), 300);
            readers_run(seed, &Pool::deep(), 300);
        }
    }

    #[test]
    fn readers_keep_the_committed_version_they_were_made_on() {
        for seed in seeds(9000, 8) {
            readers_stored_run(seed, &Pool::deep(), uncached, 40);
            readers_stored_run(seed, &Pool::dense(40), fast, 40);
        }
    }

    type Outer = Map<u32, Tallied>;
    type OuterModel = BTreeMap<u32, Model>;

    fn outer_fresh(model: &OuterModel) -> Outer {
        let mut outer = Map::new();
        for (key, inner) in model.iter().rev() {
            outer.insert(*key, fresh(inner));
        }
        outer
    }

    fn check_outer(outer: &Outer, model: &OuterModel) {
        assert_eq!(outer.len(), model.len());
        let keys: Vec<u32> = outer.keys().collect();
        assert_eq!(keys, model.keys().copied().collect::<Vec<u32>>());
        for (key, inner) in model {
            let got = outer.get(key).unwrap();
            check_full(&got, inner);
            assert_eq!(
                got.summary(),
                folded(inner, &(Bound::Unbounded, Bound::Unbounded))
            );
        }
        assert_eq!(
            outer.root_hash(),
            outer_fresh(model).root_hash(),
            "outer hash"
        );
    }

    fn check_outer_diff(old: &Outer, old_model: &OuterModel, new: &Outer, new_model: &OuterModel) {
        let got: Vec<(u32, bool, bool)> = old
            .diff(new)
            .map(|change| {
                (
                    *change.key(),
                    change.before().is_some(),
                    change.after().is_some(),
                )
            })
            .collect();
        let mut keys: Vec<u32> = old_model.keys().chain(new_model.keys()).copied().collect();
        keys.sort_unstable();
        keys.dedup();
        let wanted: Vec<(u32, bool, bool)> = keys
            .into_iter()
            .filter(|key| old_model.get(key) != new_model.get(key))
            .map(|key| {
                (
                    key,
                    old_model.contains_key(&key),
                    new_model.contains_key(&key),
                )
            })
            .collect();
        assert_eq!(got, wanted, "outer diff");
    }

    fn nested_edit(
        root: &mut Outer,
        model: &mut OuterModel,
        pool: &Pool,
        rng: &mut Rng,
        foreign: &(Tallied, Model),
        earlier: &[(Outer, OuterModel)],
    ) {
        let key = rng.below(24) as u32;
        match rng.below(12) {
            0 => {
                let mut inner: Tallied = Map::default();
                let mut wanted = Model::new();
                for _ in 0..rng.below(30) {
                    let k = pool.key(rng);
                    let v = value(rng);
                    inner.insert(k, v.clone());
                    wanted.insert(k, v);
                }
                root.insert(key, inner);
                model.insert(key, wanted);
            }
            1 => {
                root.remove(&key);
                model.remove(&key);
            }
            2 => {
                if let Some(inner) = root.get_mut(&key) {
                    let wanted = model.get_mut(&key).unwrap();
                    mutate(inner, wanted, pool, rng, &[]);
                }
            }
            3 => {
                let salt = rng.next();
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                    root.update(&key, |inner| {
                        let mut local = Rng::new(salt);
                        let mut shadow = Model::new();
                        for _ in 0..1 + local.below(6) {
                            let (k, v) = (pool.key(&mut local), value(&mut local));
                            inner.insert(k, v.clone());
                            shadow.insert(k, v);
                        }
                        panic::panic_any(7u8);
                    })
                }));
                match outcome {
                    Ok(None) => assert!(!model.contains_key(&key)),
                    Ok(Some(())) => unreachable!(),
                    Err(payload) => assert_eq!(payload.downcast_ref::<u8>(), Some(&7)),
                }
            }
            4 => {
                let salt = rng.next();
                let got = root.update(&key, |inner| {
                    let mut local = Rng::new(salt);
                    let mut shadow: Model = inner.iter().map(|(k, v)| (k, (*v).clone())).collect();
                    for _ in 0..local.below(8) {
                        mutate(inner, &mut shadow, pool, &mut local, &[]);
                    }
                    shadow
                });
                match got {
                    Some(shadow) => {
                        model.insert(key, shadow);
                    }
                    None => assert!(!model.contains_key(&key)),
                }
            }
            5 => {
                let entry = root.get_or_insert_with(key, Map::default);
                let wanted = model.entry(key).or_default();
                let k = pool.key(rng);
                let v = value(rng);
                entry.insert(k, v.clone());
                wanted.insert(k, v);
            }
            6 => {
                let to = rng.below(24) as u32;
                if let Some(inner) = root.get(&key) {
                    root.insert(to, inner.into_owned());
                    let copy = model[&key].clone();
                    model.insert(to, copy);
                }
            }
            7 => {
                root.insert(key, foreign.0.clone());
                model.insert(key, foreign.1.clone());
            }
            8 => {
                let to = rng.below(24) as u32;
                if key != to
                    && let (Some(_), Some(_)) = (root.get(&key), root.get(&to))
                {
                    let span = pool.span(rng);
                    let mut moved = root.get_mut(&key).unwrap().take_range::<u32, _>(span);
                    root.get_mut(&to).unwrap().append(&mut moved);
                    let taken: Model = spanned(&model[&key], &span).into_iter().collect();
                    model
                        .get_mut(&key)
                        .unwrap()
                        .retain(|k, _| !in_span(*k, &span));
                    model.get_mut(&to).unwrap().extend(taken);
                }
            }
            9 => {
                root.rewrite();
                if rng.chance(50)
                    && let Some(inner) = root.get_mut(&key)
                {
                    inner.rewrite();
                }
            }
            10 => {
                if let Some((base, base_model)) = earlier.last() {
                    let mut theirs = base.clone();
                    let mut theirs_model = base_model.clone();
                    let k = rng.below(24) as u32;
                    if let Some(inner) = theirs.get_mut(&k) {
                        let wanted = theirs_model.get_mut(&k).unwrap();
                        let v = value(rng);
                        let at = pool.key(rng);
                        inner.insert(at, v.clone());
                        wanted.insert(at, v);
                    } else {
                        theirs.remove(&k);
                        theirs_model.remove(&k);
                    }
                    let written = root.merge(base, &theirs, Conflict::keep_theirs);
                    let mut wanted_written = 0;
                    let mut keys: Vec<u32> = base_model
                        .keys()
                        .chain(theirs_model.keys())
                        .copied()
                        .collect();
                    keys.sort_unstable();
                    keys.dedup();
                    for k in keys {
                        let (before, after) = (base_model.get(&k), theirs_model.get(&k));
                        if before == after {
                            continue;
                        }
                        let ours = model.get(&k);
                        if ours == after {
                            continue;
                        }
                        match after {
                            Some(inner) => model.insert(k, inner.clone()),
                            None => model.remove(&k),
                        };
                        wanted_written += 1;
                    }
                    assert_eq!(written, wanted_written, "nested merge written");
                }
            }
            _ => {
                let items: Vec<(u32, Option<Tallied>)> = (0..rng.below(6))
                    .map(|_| {
                        let k = rng.below(24) as u32;
                        let edit = rng.chance(60).then(|| {
                            let inner: Tallied = (0..rng.below(10))
                                .map(|_| (pool.key(rng), value(rng)))
                                .collect();
                            inner
                        });
                        (k, edit)
                    })
                    .collect();
                for (k, edit) in &items {
                    match edit {
                        Some(inner) => {
                            let shadow: Model =
                                inner.iter().map(|(a, b)| (a, (*b).clone())).collect();
                            model.insert(*k, shadow);
                        }
                        None => {
                            model.remove(k);
                        }
                    }
                }
                root.apply(items);
            }
        }
    }

    fn nested_run(seed: u64, options: fn() -> Options, commits: usize) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested.ferro");
        let open = || options().open::<Outer>(&path).unwrap();
        let mut db = open();
        let other = fast()
            .open::<Tallied>(dir.path().join("other.ferro"))
            .unwrap();
        let pool = Pool::dense(60);
        let mut rng = Rng::new(seed);
        let mut foreign_model = Model::new();
        other
            .update(|root| {
                for _ in 0..40 {
                    let (k, v) = (pool.key(&mut rng), value(&mut rng));
                    root.insert(k, v.clone());
                    foreign_model.insert(k, v);
                }
            })
            .unwrap();
        let foreign = ((*other.read()).clone(), foreign_model);
        let mut model = OuterModel::new();
        let mut earlier: Vec<(Outer, OuterModel)> = Vec::new();
        for commit in 0..commits {
            let mut working = model.clone();
            db.update(|root| {
                for _ in 0..rng.below(10) {
                    nested_edit(root, &mut working, &pool, &mut rng, &foreign, &earlier);
                }
                if rng.chance(20) {
                    check_outer(root, &working);
                }
            })
            .unwrap();
            model = working;
            let snapshot = db.read();
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                check_outer(&snapshot, &model);
                if let Some((old, old_model)) = earlier.first() {
                    check_outer_diff(old, old_model, &snapshot, &model);
                }
            }));
            if let Err(payload) = outcome {
                eprintln!("seed {seed} commit {commit}");
                panic::resume_unwind(payload);
            }
            earlier.push(((*snapshot).clone(), model.clone()));
            if earlier.len() > 3 {
                earlier.remove(0);
            }
            drop(snapshot);
            match rng.below(10) {
                0 => {
                    drop(db);
                    db = open();
                    check_outer(&db.read(), &model);
                }
                1 => {
                    db.verify_typed().unwrap();
                }
                2 => {
                    db.compact(Keep::Last(3)).unwrap();
                    check_outer(&db.read(), &model);
                }
                _ => {}
            }
        }
        drop(other);
        db.verify_typed().unwrap();
        drop(db);
        let db = uncached().open::<Outer>(&path).unwrap();
        check_outer(&db.read(), &model);
        db.verify_typed().unwrap();
    }

    #[test]
    fn nested_summarized_maps_match_the_model() {
        for seed in seeds(10_000, 6) {
            nested_run(seed, fast, 40);
            nested_run(seed, uncached, 30);
            nested_run(seed, roomy, 30);
        }
    }
}
