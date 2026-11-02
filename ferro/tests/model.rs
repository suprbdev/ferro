mod common;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Debug;
use std::ops::Bound;
use std::sync::{Arc, OnceLock};

use common::{Rng, fast, level_of, text, uncached};
use ferro::{Data, Db, List, Map, Options, Set};
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

fn pairs<K: Clone>(model: &BTreeMap<K, String>) -> VecDeque<(K, Arc<String>)> {
    model
        .iter()
        .map(|(key, value)| (key.clone(), Arc::new(value.clone())))
        .collect()
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
            assert_eq!(map.get(&key).map(|v| (*v).clone()), model.get(&key).cloned());
            assert_eq!(map.try_get(&key).unwrap().is_some(), model.contains_key(&key));
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
            let touched = map.update(&key, change);
            assert_eq!(touched, model.contains_key(&key));
            if let Some(value) = model.get_mut(&key) {
                change(value);
            }
        }
        Op::GetOrInsert(seed, salt, len) => {
            let key = resolve(*seed);
            let value = text(*salt, *len);
            let a = map.get_or_insert_with(key.clone(), || value.clone()).clone();
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
        Op::Range(low, high, steps) => {
            let low = map_bound(*low, resolve);
            let high = map_bound(*high, resolve);
            let expected: VecDeque<(K, Arc<String>)> = model
                .iter()
                .filter(|(key, _)| in_range(*key, &low, &high))
                .map(|(key, value)| (key.clone(), Arc::new(value.clone())))
                .collect();
            let reversed: Vec<(K, Arc<String>)> = expected.iter().rev().cloned().collect();
            let bounds = (low, high);
            let got: Vec<(K, Arc<String>)> = map.range::<K, _>(bounds.clone()).rev().collect();
            assert_eq!(got, reversed);
            walk(map.range::<K, _>(bounds), expected, steps);
        }
        Op::Walk(steps) => walk(map.iter(), pairs(model), steps),
        Op::KeysWalk(steps) => walk(map.keys(), model.keys().cloned().collect(), steps),
        Op::ValuesWalk(steps) => walk(
            map.values(),
            model.values().map(|v| Arc::new(v.clone())).collect(),
            steps,
        ),
        Op::Nth(index) => {
            let got = map.nth(*index).map(|(k, v)| (k, (*v).clone()));
            let wanted = model.iter().nth(*index).map(|(k, v)| (k.clone(), v.clone()));
            assert_eq!(got, wanted);
        }
        Op::Commit | Op::Reopen => {}
    }
    assert_eq!(map.len(), model.len());
    assert_eq!(map.is_empty(), model.is_empty());
}

fn check_full<K: TestKey>(map: &Map<K, String>, model: &BTreeMap<K, String>) {
    assert_eq!(map.len(), model.len());
    let forward: Vec<(K, Arc<String>)> = map.iter().collect();
    assert_eq!(forward, Vec::from(pairs(model)));
    let mut backward: Vec<(K, Arc<String>)> = map.iter().rev().collect();
    backward.reverse();
    assert_eq!(backward, forward);
    let keys: Vec<K> = map.keys().collect();
    assert_eq!(keys, model.keys().cloned().collect::<Vec<K>>());
    let values: Vec<Arc<String>> = map.values().rev().collect();
    let wanted: Vec<Arc<String>> = model.values().rev().map(|v| Arc::new(v.clone())).collect();
    assert_eq!(values, wanted);
    assert_eq!(map.first().map(|p| p.0), model.keys().next().cloned());
    assert_eq!(map.last().map(|p| p.0), model.keys().next_back().cloned());
    for (index, (key, value)) in model.iter().enumerate() {
        assert_eq!(map.get(key).as_deref(), Some(value));
        assert_eq!(map.nth(index).map(|p| p.0).as_ref(), Some(key));
    }
    assert_eq!(map.nth(model.len()).map(|p| p.0), None);
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
