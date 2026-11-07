mod common;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{Rng, cases, fast, quiet_ferro_panics, roomy, text, uncached};
use ferro::admin::{self, Keep};
use ferro::{Change, Data, Db, Error, Map, Options, Outcome, Set, Snapshot, Summary};
use proptest::prelude::*;

type Model = BTreeMap<u32, String>;
type Probed = Map<u32, String, Probe>;
type Span = (Bound<u32>, Bound<u32>);

#[derive(Clone, Debug, PartialEq, Data)]
struct Probe {
    count: u64,
    bytes: u64,
    lowest: u32,
    highest: u32,
    shortest: u64,
    first: u32,
    last: u32,
    digest: u64,
    scale: u64,
}

impl Summary<u32, String> for Probe {
    fn of(key: &u32, value: &String) -> Self {
        let mut digest = 0xcbf2_9ce4_8422_2325 ^ u64::from(*key);
        for byte in value.bytes() {
            digest = (digest ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        Probe {
            count: 1,
            bytes: value.len() as u64,
            lowest: *key,
            highest: *key,
            shortest: value.len() as u64,
            first: *key,
            last: *key,
            digest,
            scale: 0x9e37_79b9_7f4a_7c15,
        }
    }

    fn combine(&self, other: &Self) -> Self {
        Probe {
            count: self.count + other.count,
            bytes: self.bytes + other.bytes,
            lowest: self.lowest.min(other.lowest),
            highest: self.highest.max(other.highest),
            shortest: self.shortest.min(other.shortest),
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

#[derive(Clone, Debug, PartialEq, Data)]
#[ferro(transparent)]
struct Float(f64);

impl Summary<u32, String> for Float {
    fn of(key: &u32, value: &String) -> Self {
        Float(f64::from(*key) * 0.1 + value.len() as f64 / 3.0)
    }

    fn combine(&self, other: &Self) -> Self {
        Float(self.0 + other.0)
    }
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

fn folded<S: Summary<u32, String>>(model: &Model, span: &Span) -> Option<S> {
    model
        .iter()
        .filter(|(key, _)| in_span(**key, span))
        .map(|(key, value)| S::of(key, value))
        .reduce(|total, next| total.combine(&next))
}

fn check_span(map: &Probed, model: &Model, span: Span) {
    assert_eq!(
        map.range_summary::<u32, _>(span),
        folded(model, &span),
        "{span:?}"
    );
    let by = map.summary_by(
        |key| !in_span(*key, &(span.0, Bound::Unbounded)),
        |key| in_span(*key, &(Bound::Unbounded, span.1)),
    );
    assert_eq!(by, folded(model, &span), "{span:?}");
}

fn check_full(map: &Probed, model: &Model) {
    assert_eq!(map.len(), model.len());
    let everything = (Bound::Unbounded, Bound::Unbounded);
    assert_eq!(map.summary(), folded(model, &everything));
    check_span(map, model, everything);
    for key in model.keys().step_by(1 + model.len() / 24) {
        check_span(map, model, (Bound::Unbounded, Bound::Included(*key)));
        check_span(map, model, (Bound::Unbounded, Bound::Excluded(*key)));
        check_span(map, model, (Bound::Included(*key), Bound::Unbounded));
        check_span(map, model, (Bound::Excluded(*key), Bound::Unbounded));
        check_span(map, model, (Bound::Included(*key), Bound::Included(*key)));
        check_span(map, model, (Bound::Excluded(*key), Bound::Excluded(*key)));
    }
    assert_eq!(map.root_hash(), fresh::<Probe>(model).root_hash());
}

fn fresh<S: Summary<u32, String>>(model: &Model) -> Map<u32, String, S> {
    let mut map = Map::default();
    for (key, value) in model.iter().rev() {
        map.insert(*key, value.clone());
    }
    map
}

#[derive(Clone, Debug)]
enum Op {
    Insert(u32, u64, usize),
    Remove(u32),
    GetMut(u32, u64, usize),
    Update(u32, usize),
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
    Summarize(Bound<u32>, Bound<u32>),
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
        3 => (0..keys, any::<u64>(), sizes()).prop_map(|(k, s, n)| Op::GetMut(k, s, n)),
        3 => (0..keys, sizes()).prop_map(|(k, n)| Op::Update(k, n)),
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
        10 => (bound(keys), bound(keys)).prop_map(|(a, b)| Op::Summarize(a, b)),
        commit => Just(Op::Commit),
        1 => Just(Op::Reopen),
    ]
}

fn apply(map: &mut Probed, model: &mut Model, op: &Op) {
    match op {
        Op::Insert(key, salt, len) => {
            let value = text(*salt, *len);
            map.insert(*key, value.clone());
            model.insert(*key, value);
        }
        Op::Remove(key) => {
            map.remove(key);
            model.remove(key);
        }
        Op::GetMut(key, salt, len) => {
            let value = text(*salt, *len);
            if let Some(slot) = map.get_mut(key) {
                *slot = value.clone();
            }
            if let Some(slot) = model.get_mut(key) {
                *slot = value;
            }
        }
        Op::Update(key, len) => {
            let change = |value: &mut String| value.truncate(*len);
            map.update(key, change);
            if let Some(value) = model.get_mut(key) {
                change(value);
            }
        }
        Op::PopFirst => {
            map.pop_first();
            model.pop_first();
        }
        Op::PopLast => {
            map.pop_last();
            model.pop_last();
        }
        Op::Retain(salt, modulus) => {
            let doomed = |key: &u32| (u64::from(*key) ^ *salt) % *modulus == 0;
            map.retain(|key, _| !doomed(key));
            model.retain(|key, _| !doomed(key));
        }
        Op::RemoveRange(low, high) => {
            let span = (*low, *high);
            map.remove_range::<u32, _>(span);
            model.retain(|key, _| !in_span(*key, &span));
        }
        Op::TakeRange(low, high) => {
            let span = (*low, *high);
            let wanted: Model = model
                .iter()
                .filter(|(key, _)| in_span(**key, &span))
                .map(|(key, value)| (*key, value.clone()))
                .collect();
            model.retain(|key, _| !in_span(*key, &span));
            check_full(&map.take_range::<u32, _>(span), &wanted);
        }
        Op::SplitOff(key) => {
            let wanted = model.split_off(key);
            check_full(&map.split_off(key), &wanted);
        }
        Op::SplitRejoin(key) => {
            let mut tail = map.split_off(key);
            map.append(&mut tail);
        }
        Op::AppendMap(entries) => {
            let mut other = Probed::default();
            let mut wanted = Model::new();
            for (key, salt, len) in entries {
                let value = text(*salt, *len);
                other.insert(*key, value.clone());
                wanted.insert(*key, value);
            }
            map.append(&mut other);
            model.append(&mut wanted);
        }
        Op::Extend(entries) => {
            let items: Vec<(u32, String)> = entries
                .iter()
                .map(|(key, salt, len)| (*key, text(*salt, *len)))
                .collect();
            map.extend(items.clone());
            model.extend(items);
        }
        Op::ApplyEdits(edits) => {
            let items: Vec<(u32, Option<String>)> = edits
                .iter()
                .map(|(key, edit)| (*key, edit.map(|(salt, len)| text(salt, len))))
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
        Op::Summarize(low, high) => check_span(map, model, (*low, *high)),
        Op::Commit | Op::Reopen => {}
    }
    assert_eq!(
        map.summary(),
        folded(model, &(Bound::Unbounded, Bound::Unbounded))
    );
}

fn memory_run(ops: &[Op]) {
    let mut map = Probed::default();
    let mut model = Model::new();
    for op in ops {
        apply(&mut map, &mut model, op);
    }
    check_full(&map, &model);
}

fn db_run(ops: &[Op], options: fn() -> Options) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.ferro");
    let open = || options().open::<Probed>(&path).unwrap();
    let mut db = open();
    let mut model = Model::new();
    let mut pending: Vec<&Op> = Vec::new();
    let mut settle = |db: &Db<Probed>, pending: &mut Vec<&Op>| {
        db.update(|root| {
            for op in pending.iter() {
                apply(root, &mut model, op);
            }
        })
        .unwrap();
        pending.clear();
        check_full(&db.read(), &model);
    };
    for op in ops {
        match op {
            Op::Commit => settle(&db, &mut pending),
            Op::Reopen => {
                settle(&db, &mut pending);
                drop(db);
                db = open();
            }
            other => pending.push(other),
        }
    }
    settle(&db, &mut pending);
    db.verify().unwrap();
}

fn sequence(keys: u32, durable: bool, length: usize) -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op(keys, durable), 1..length)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(500), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn memory_tiny_keys(ops in sequence(8, false, 120)) {
        memory_run(&ops);
    }

    #[test]
    fn memory_small_keys(ops in sequence(128, false, 200)) {
        memory_run(&ops);
    }

    #[test]
    fn stored_small_keys(ops in sequence(128, true, 120)) {
        db_run(&ops, fast);
    }

    #[test]
    fn stored_uncached(ops in sequence(128, true, 120)) {
        db_run(&ops, uncached);
    }

    #[test]
    fn stored_roomy_cache(ops in sequence(128, true, 120)) {
        db_run(&ops, roomy);
    }
}

fn contents(keys: u32, max: usize) -> impl Strategy<Value = Model> {
    prop::collection::btree_map(0..keys, (any::<u64>(), sizes()), 0..max).prop_map(|entries| {
        entries
            .into_iter()
            .map(|(key, (salt, len))| (key, text(salt, len)))
            .collect()
    })
}

fn churned<S: Summary<u32, String>>(model: &Model, seed: u64, keys: u32) -> Map<u32, String, S> {
    let mut rng = Rng::new(seed);
    let mut order: Vec<(u32, &String)> = model.iter().map(|(k, v)| (*k, v)).collect();
    rng.shuffle(&mut order);
    let mut map = Map::default();
    let mut extra = Vec::new();
    for (key, value) in order {
        if rng.chance(30) {
            let noise = rng.below(u64::from(keys)) as u32;
            if !model.contains_key(&noise) {
                map.insert(noise, text(rng.next(), rng.below(700) as usize));
                extra.push(noise);
            }
        }
        if rng.chance(30) {
            map.insert(key, text(rng.next(), rng.below(700) as usize));
        }
        map.insert(key, value.clone());
        if rng.chance(20) {
            map.remove(&key);
            map.insert(key, value.clone());
        }
        if rng.chance(20) {
            map.update(&key, |slot| slot.push_str(&"z".repeat(600)));
            map.update(&key, |slot| *slot = value.clone());
        }
    }
    rng.shuffle(&mut extra);
    for key in extra {
        map.remove(&key);
    }
    map
}

fn commit_batches<S: Summary<u32, String>>(db: &Db<Map<u32, String, S>>, model: &Model, seed: u64) {
    let mut rng = Rng::new(seed);
    db.update(|root| root.clear()).unwrap();
    let mut order: Vec<(u32, String)> = model.iter().map(|(k, v)| (*k, v.clone())).collect();
    rng.shuffle(&mut order);
    for chunk in order.chunks(1 + rng.below(9) as usize) {
        db.update(|root| {
            for (key, value) in chunk {
                root.insert(*key, value.clone());
            }
            root.insert(u32::MAX, "noise".repeat(1 + chunk.len() * 60));
        })
        .unwrap();
    }
    db.update(|root| {
        root.remove(&u32::MAX);
    })
    .unwrap();
}

fn all_histories_agree<S>(model: &Model, seed: u64, keys: u32, pivot: u32)
where
    S: Summary<u32, String> + PartialEq + Debug,
{
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.ferro");
    let base = fresh::<S>(model);
    let hash = base.root_hash();
    let summary = base.summary();
    let span = (Bound::Unbounded, Bound::Excluded(pivot));
    let part = base.range_summary::<u32, _>(span);
    let check = |other: &Map<u32, String, S>| {
        assert_eq!(other.root_hash(), hash);
        assert_eq!(other.summary(), summary);
        assert_eq!(other.range_summary::<u32, _>(span), part);
        assert!(*other == base);
    };
    let sorted: Map<u32, String, S> = model.iter().map(|(k, v)| (*k, v.clone())).collect();
    check(&sorted);
    for salt in 0..3 {
        check(&churned::<S>(model, seed ^ salt, keys));
    }
    let mut edited = churned::<S>(model, seed ^ 7, keys);
    edited.apply(model.keys().map(|key| (*key, None)));
    edited.apply(model.iter().map(|(k, v)| (*k, Some(v.clone()))));
    check(&edited);
    let mut parts = churned::<S>(model, seed ^ 8, keys);
    let mut tail = parts.split_off(&pivot);
    assert_eq!(parts.len() + tail.len(), model.len());
    tail.append(&mut parts);
    check(&tail);
    let db = fast().open::<Map<u32, String, S>>(&path).unwrap();
    commit_batches(&db, model, seed);
    check(&db.read());
    db.verify_typed().unwrap();
    let copy = fast()
        .open::<Map<u32, String, S>>(dir.path().join("d.ferro"))
        .unwrap();
    copy.pull(&db, "c", |_, _, _| -> (String, ()) {
        panic!("no merge expected")
    })
    .unwrap();
    check(&copy.read());
    drop(copy);
    let version = db.version();
    db.update(|root| *root = churned::<S>(model, seed ^ 99, keys))
        .unwrap();
    assert_eq!(db.version(), version);
    db.update(|root| {
        root.insert(u32::MAX - 1, "tmp".into());
        root.remove(&(u32::MAX - 1));
    })
    .unwrap();
    assert_eq!(db.version(), version);
    drop(db);
    let db = uncached().open::<Map<u32, String, S>>(&path).unwrap();
    check(&db.read());
    let mut mixed = (*db.read()).clone();
    for (key, value) in model.iter().take(16) {
        mixed.remove(key);
        mixed.insert(*key, value.clone());
    }
    check(&mixed);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(300), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn history_never_changes_the_hash_or_summary(
        model in contents(40, 40),
        seed in any::<u64>(),
        pivot in 0..40u32,
    ) {
        all_histories_agree::<Probe>(&model, seed, 40, pivot);
    }

    #[test]
    fn history_never_changes_the_hash_or_summary_large(
        model in contents(100_000, 400),
        seed in any::<u64>(),
        pivot in 0..100_000u32,
    ) {
        all_histories_agree::<Probe>(&model, seed, 100_000, pivot);
    }

    #[test]
    fn float_rounding_does_not_depend_on_history(
        model in contents(100_000, 400),
        seed in any::<u64>(),
        pivot in 0..100_000u32,
    ) {
        all_histories_agree::<Float>(&model, seed, 100_000, pivot);
    }

    #[test]
    fn different_summaries_have_different_hashes(model in contents(100_000, 400)) {
        prop_assume!(model.len() > 64);
        prop_assert_ne!(fresh::<Probe>(&model).root_hash(), fresh::<Float>(&model).root_hash());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Data)]
#[ferro(transparent)]
struct Total(i64);

impl Summary<u64, i64> for Total {
    fn of(_key: &u64, value: &i64) -> Self {
        Total(*value)
    }

    fn combine(&self, other: &Self) -> Self {
        Total(self.0 + other.0)
    }
}

#[test]
fn large_stored_tree_matches_prefix_sums() {
    let mut rng = Rng::new(5);
    let values: Vec<i64> = (0..150_000)
        .map(|_| rng.below(2001) as i64 - 1000)
        .collect();
    let mut sums = vec![0i64];
    for value in &values {
        sums.push(sums[sums.len() - 1] + value);
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("l.ferro");
    let db = fast().open::<Map<u64, i64, Total>>(&path).unwrap();
    db.update(|root| root.extend(values.iter().enumerate().map(|(i, v)| (i as u64, *v))))
        .unwrap();
    drop(db);
    let db = uncached()
        .read_only(true)
        .open::<Map<u64, i64, Total>>(&path)
        .unwrap();
    db.verify_typed().unwrap();
    let map = db.read();
    assert_eq!(map.summary(), Some(Total(sums[values.len()])));
    for _ in 0..2_000 {
        let low = rng.below(150_000) as usize;
        let high = low + rng.below(150_001 - low as u64) as usize;
        let got = map
            .range_summary((low as u64)..(high as u64))
            .map_or(0, |total| total.0);
        assert_eq!(got, sums[high] - sums[low]);
    }
}

thread_local! {
    static CALLS: Cell<u64> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Debug, PartialEq, Data)]
#[ferro(transparent)]
struct Counted(u64);

impl<V> Summary<u64, V> for Counted {
    fn of(_key: &u64, _value: &V) -> Self {
        CALLS.set(CALLS.get() + 1);
        Counted(1)
    }

    fn combine(&self, other: &Self) -> Self {
        Counted(self.0 + other.0)
    }
}

#[test]
fn a_range_folds_what_the_links_hold_not_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("o.ferro");
    let db = fast().open::<Map<u64, u64, Counted>>(&path).unwrap();
    db.update(|root| root.extend((0..200_000u64).map(|key| (key, key))))
        .unwrap();
    CALLS.set(0);
    let map = db.read();
    assert_eq!(map.range_summary(1_000..199_000u64), Some(Counted(198_000)));
    assert!(CALLS.get() < 1_000, "{} calls for 198000 rows", CALLS.get());
    CALLS.set(0);
    assert_eq!(map.summary(), Some(Counted(200_000)));
    assert_eq!(CALLS.get(), 0);
}

#[test]
fn spilled_values_are_summarized_without_being_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.ferro");
    let db = fast().open::<Map<u64, String, Counted>>(&path).unwrap();
    let big = "x".repeat(2_000);
    db.update(|root| root.extend((0..5_000u64).map(|key| (key, big.clone()))))
        .unwrap();
    drop(db);
    let db = uncached().open::<Map<u64, String, Counted>>(&path).unwrap();
    CALLS.set(0);
    assert_eq!(db.read().range_summary(10..4_990u64), Some(Counted(4_980)));
    assert_eq!(CALLS.get(), 0);
    db.update(|root| {
        root.insert(1, "small".to_string());
    })
    .unwrap();
    assert!(
        CALLS.get() <= 1,
        "a commit recomputed {} spilled values",
        CALLS.get()
    );
    assert_eq!(db.read().summary(), Some(Counted(5_000)));
}

#[test]
fn summaries_follow_compaction_foreign_stores_and_merges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ferro");
    let model: Model = (0..2_000u32)
        .map(|key| (key * 3, text(u64::from(key), 40)))
        .collect();
    let span = (Bound::Included(300), Bound::Excluded(4_200));
    {
        let db = fast().open::<Probed>(&path).unwrap();
        db.update(|root| *root = fresh(&model)).unwrap();
        db.update(|root| {
            root.insert(1, "one".to_string());
        })
        .unwrap();
        db.update(|root| {
            root.remove(&1);
        })
        .unwrap();
    }
    admin::compact(&path, Keep::All).unwrap();
    let db = fast().open::<Probed>(&path).unwrap();
    db.verify().unwrap();
    check_full(&db.read(), &model);

    let other_path = dir.path().join("b.ferro");
    let other = fast().open::<Probed>(&other_path).unwrap();
    let foreign = (*db.read()).clone();
    other.update(|root| *root = foreign).unwrap();
    check_full(&other.read(), &model);
    drop(other);
    let other = fast().open::<Probed>(&other_path).unwrap();
    check_full(&other.read(), &model);

    let theirs = db.fork("theirs").unwrap();
    theirs
        .update(|root| {
            root.insert(301, "theirs".to_string());
            root.remove(&600);
        })
        .unwrap();
    db.update(|root| {
        root.insert(302, "ours".to_string());
    })
    .unwrap();
    let merged = db
        .merge_with(&theirs, |root, base, incoming| {
            let written = root.merge(base, incoming, |conflict| {
                conflict.theirs.map(|v| (*v).clone())
            });
            ("merge", written)
        })
        .unwrap();
    assert!(merged.version > 0);
    let mut wanted = model.clone();
    wanted.insert(301, "theirs".to_string());
    wanted.insert(302, "ours".to_string());
    wanted.remove(&600);
    check_full(&db.read(), &wanted);
    check_span(&db.read(), &wanted, span);
    let changes: Vec<Change<u32, String>> = theirs.read().diff(&db.read()).collect();
    assert_eq!(changes.len(), 1);
}

#[test]
fn a_composite_prefix_can_be_summarized_by_predicate() {
    let mut map: Map<(u8, u32), i64, Pair> = Map::default();
    let mut model: BTreeMap<(u8, u32), i64> = BTreeMap::new();
    let mut rng = Rng::new(9);
    for key in 0..4_000u32 {
        let group = rng.below(5) as u8;
        let value = rng.below(1_000) as i64;
        map.insert((group, key), value);
        model.insert((group, key), value);
    }
    for group in 0..6u8 {
        let wanted: i64 = model
            .range((group, 0)..=(group, u32::MAX))
            .map(|(_, v)| *v)
            .sum();
        let got = map.summary_by(|key| key.0 < group, |key| key.0 <= group);
        assert_eq!(got.map_or(0, |pair| pair.0), wanted);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Data)]
#[ferro(transparent)]
struct Pair(i64);

impl Summary<(u8, u32), i64> for Pair {
    fn of(_key: &(u8, u32), value: &i64) -> Self {
        Pair(*value)
    }

    fn combine(&self, other: &Self) -> Self {
        Pair(self.0 + other.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Data)]
#[ferro(transparent)]
struct Fragile(u64);

impl Summary<u64, u64> for Fragile {
    fn of(_key: &u64, value: &u64) -> Self {
        assert!(*value != u64::MAX, "ferro: test summary refused a value");
        Fragile(*value)
    }

    fn combine(&self, other: &Self) -> Self {
        Fragile(self.0 + other.0)
    }
}

#[test]
fn a_panicking_summary_discards_the_commit() {
    quiet_ferro_panics();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.ferro");
    let db = fast().open::<Map<u64, u64, Fragile>>(&path).unwrap();
    db.update(|root| root.extend((0..1_000u64).map(|key| (key, 1))))
        .unwrap();
    let version = db.version();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        db.update(|root| {
            root.insert(5_000, u64::MAX);
        })
    }));
    assert!(failed.is_err());
    assert_eq!(db.version(), version);
    assert_eq!(db.read().summary(), Some(Fragile(1_000)));
    db.update(|root| {
        root.insert(5_000, 7);
    })
    .unwrap();
    assert_eq!(db.read().summary(), Some(Fragile(1_007)));
    drop(db);
    let db = fast().open::<Map<u64, u64, Fragile>>(&path).unwrap();
    assert_eq!(db.read().summary(), Some(Fragile(1_007)));
    db.verify().unwrap();
}

#[test]
fn an_empty_or_inverted_range_has_no_summary() {
    let map: Map<u64, i64, Total> = (0..1_000u64).map(|key| (key * 2, 1)).collect();
    assert_eq!(Map::<u64, i64, Total>::default().summary(), None);
    assert_eq!(map.range_summary(5..5u64), None);
    assert_eq!(map.range_summary(11..11u64), None);
    assert_eq!(map.range_summary(5_000..u64::MAX), None);
    assert_eq!(map.summary_by(|key| *key < 900, |key| *key <= 100), None);
}

#[test]
fn a_plain_map_keeps_its_format() {
    let mut plain = Map::new();
    for key in 0..1000u64 {
        plain.insert(key, format!("value {key}"));
    }
    let golden = [
        222, 75, 3, 187, 160, 118, 64, 237, 45, 55, 16, 54, 185, 12, 37, 68, 193, 180, 206, 107,
    ];
    assert_eq!(plain.root_hash(), Some(golden));
    assert_eq!(plain.range_summary(10..20u64), Some(()));
    assert_eq!(plain.range_summary(5_000..6_000u64), None);
}

#[derive(Clone, Default, Data)]
#[ferro(transparent)]
struct Distinct(Set<u32>);

impl Distinct {
    fn values(&self) -> Vec<u32> {
        self.0.iter().collect()
    }
}

impl Summary<u64, u32> for Distinct {
    fn of(_key: &u64, value: &u32) -> Self {
        Distinct([*value].into_iter().collect())
    }

    fn combine(&self, other: &Self) -> Self {
        Distinct(self.0.union(&other.0))
    }
}

impl Summary<u64, String> for Distinct {
    fn of(_key: &u64, value: &String) -> Self {
        Distinct([value.len() as u32].into_iter().collect())
    }

    fn combine(&self, other: &Self) -> Self {
        Distinct(self.0.union(&other.0))
    }
}

static SKEWED_KEY: AtomicU64 = AtomicU64::new(u64::MAX);

#[derive(Clone, Default, Data)]
#[ferro(transparent)]
struct Skewed(Set<u32>);

impl Skewed {
    fn of_value(key: u64, value: u32) -> Self {
        let value = match key == SKEWED_KEY.load(Ordering::Relaxed) {
            true => value + 1_000,
            false => value,
        };
        Skewed([value].into_iter().collect())
    }
}

impl Summary<u64, u32> for Skewed {
    fn of(key: &u64, value: &u32) -> Self {
        Skewed::of_value(*key, *value)
    }

    fn combine(&self, other: &Self) -> Self {
        Skewed(self.0.union(&other.0))
    }
}

impl Summary<u64, String> for Skewed {
    fn of(key: &u64, value: &String) -> Self {
        Skewed::of_value(*key, value.len() as u32)
    }

    fn combine(&self, other: &Self) -> Self {
        Skewed(self.0.union(&other.0))
    }
}

#[derive(Clone, Data)]
struct Tallies<S: Summary<u64, u32> + Summary<u64, String>> {
    inline: Map<u64, u32, S>,
    spilled: Map<u64, String, S>,
}

impl<S: Summary<u64, u32> + Summary<u64, String>> Default for Tallies<S> {
    fn default() -> Self {
        Tallies {
            inline: Map::default(),
            spilled: Map::default(),
        }
    }
}

type Tallied = Tallies<Distinct>;
type TallyModel = (BTreeMap<u64, u32>, BTreeMap<u64, String>);

fn distinct<V>(
    model: &BTreeMap<u64, V>,
    (low, high): (u64, u64),
    of: impl Fn(&V) -> u32,
) -> Option<Vec<u32>> {
    let mut found: Vec<u32> = model.range(low..high).map(|(_, value)| of(value)).collect();
    found.sort_unstable();
    found.dedup();
    (!found.is_empty()).then_some(found)
}

fn check_tallies(tallies: &Tallied, model: &TallyModel, rng: &mut Rng) {
    let length = |value: &String| value.len() as u32;
    assert_eq!(tallies.inline.len(), model.0.len());
    assert_eq!(tallies.spilled.len(), model.1.len());
    assert_eq!(
        tallies.inline.summary().map(|found| found.values()),
        distinct(&model.0, (0, u64::MAX), |value| *value)
    );
    assert_eq!(
        tallies.spilled.summary().map(|found| found.values()),
        distinct(&model.1, (0, u64::MAX), length)
    );
    for _ in 0..60 {
        let (a, b) = (rng.below(600), rng.below(600));
        let span = (a.min(b), a.max(b));
        assert_eq!(
            tallies
                .inline
                .range_summary(span.0..span.1)
                .map(|found| found.values()),
            distinct(&model.0, span, |value| *value),
            "{span:?}"
        );
        assert_eq!(
            tallies
                .spilled
                .range_summary(span.0..span.1)
                .map(|found| found.values()),
            distinct(&model.1, span, length),
            "{span:?}"
        );
    }
}

fn tally_round(db: &Db<Tallied>, model: &mut TallyModel, rng: &mut Rng) {
    db.update(|root| {
        for _ in 0..150 {
            let key = rng.below(500);
            if rng.chance(25) {
                root.inline.remove(&key);
                model.0.remove(&key);
                root.spilled.remove(&key);
                model.1.remove(&key);
                continue;
            }
            let value = rng.below(300) as u32;
            root.inline.insert(key, value);
            model.0.insert(key, value);
            let note = text(key ^ u64::from(value), 520 + rng.below(40) as usize);
            root.spilled.insert(key, note.clone());
            model.1.insert(key, note);
        }
    })
    .unwrap();
}

fn unmerged<T>(_: &mut T, _: &Snapshot<T>, _: &Snapshot<T>) -> (String, ()) {
    panic!("no merge expected")
}

fn key_at_level(tallies: &Tallied, level: u32) -> u64 {
    tallies
        .inline
        .iter()
        .map(|(key, _)| key)
        .find(|key| common::level_of(key) == level)
        .unwrap()
}

#[test]
fn a_summary_that_holds_a_collection_verifies_syncs_and_refuses_a_forged_one() {
    quiet_ferro_panics();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ferro");
    let mut rng = Rng::new(7);
    let mut model = TallyModel::default();
    let a: Db<Tallied> = fast().open(&path).unwrap();
    for _ in 0..8 {
        tally_round(&a, &mut model, &mut rng);
        check_tallies(&a.read(), &model, &mut rng);
    }
    a.verify_typed().unwrap();
    drop(a);
    let a: Db<Tallied> = uncached().open(&path).unwrap();
    check_tallies(&a.read(), &model, &mut rng);
    a.verify_typed().unwrap();
    a.compact(Keep::Last(3)).unwrap();
    check_tallies(&a.read(), &model, &mut rng);
    a.verify_typed().unwrap();

    let b: Db<Tallied> = fast().open(dir.path().join("b.ferro")).unwrap();
    assert_eq!(
        b.pull(&a, "a", unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    check_tallies(&b.read(), &model, &mut rng);
    b.verify_typed().unwrap();
    for _ in 0..3 {
        tally_round(&b, &mut model, &mut rng);
    }
    assert_eq!(
        a.pull(&b, "b", unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    check_tallies(&a.read(), &model, &mut rng);
    a.verify_typed().unwrap();
    tally_round(&a, &mut model, &mut rng);
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    assert_eq!(
        b.receive("a", &bundle, unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    check_tallies(&b.read(), &model, &mut rng);
    b.verify_typed().unwrap();

    let read = a.read();
    let skews = [key_at_level(&read, 0), key_at_level(&read, 1)];
    drop((read, a, b));
    for (round, skew) in skews.into_iter().enumerate() {
        SKEWED_KEY.store(skew, Ordering::Relaxed);
        let forged: Db<Tallies<Skewed>> = fast().read_only(true).open(&path).unwrap();
        forged.verify().unwrap();
        match forged.verify_typed() {
            Err(Error::Corrupt(message)) => assert!(message.contains("summary"), "{message}"),
            other => panic!("typed verify passed a summary another `of` made: {other:?}"),
        }
        let c: Db<Tallies<Skewed>> = fast()
            .open(dir.path().join(format!("c{round}.ferro")))
            .unwrap();
        let theirs: Db<Tallied> = fast().read_only(true).open(&path).unwrap();
        let bundle = theirs.bundle(&c.have().unwrap()).unwrap();
        match c.receive("a", &bundle, unmerged) {
            Err(Error::Bundle(message)) => assert!(message.contains("summary"), "{message}"),
            other => panic!("accepted a summary another `of` made: {other:?}"),
        }
        assert_eq!(c.version(), 0);
    }
}
