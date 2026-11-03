mod common;

use std::collections::BTreeMap;
use ferro::Val;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{Rng, cases, fast, level_of, roomy, text, uncached};
use ferro::codec::{Decoder, Encoder};
use ferro::{Change, Data, Db, Map};
use proptest::prelude::*;

type Model = BTreeMap<u32, String>;

fn sizes() -> impl Strategy<Value = usize> {
    prop_oneof![
        4 => 0..24usize,
        4 => 500..524usize,
        1 => 524..2000usize,
    ]
}

fn contents(keys: u32, max: usize) -> impl Strategy<Value = Model> {
    prop::collection::btree_map(0..keys, (any::<u64>(), sizes()), 0..max).prop_map(|entries| {
        entries
            .into_iter()
            .map(|(key, (salt, len))| (key, text(salt, len)))
            .collect()
    })
}

fn sorted(model: &Model) -> Map<u32, String> {
    let mut map = Map::new();
    for (key, value) in model {
        map.insert(*key, value.clone());
    }
    map
}

fn reversed(model: &Model) -> Map<u32, String> {
    let mut map = Map::new();
    for (key, value) in model.iter().rev() {
        map.insert(*key, value.clone());
    }
    map
}

fn churned(model: &Model, seed: u64, keys: u32) -> Map<u32, String> {
    let mut rng = Rng::new(seed);
    let mut order: Vec<(u32, &String)> = model.iter().map(|(k, v)| (*k, v)).collect();
    rng.shuffle(&mut order);
    let mut map = Map::new();
    let mut extra = Vec::new();
    for (key, value) in order {
        if rng.chance(30) {
            let noise = rng.below(keys as u64) as u32;
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
            map.update(&key, |slot| {
                slot.push_str(&"z".repeat(600));
            });
            map.update(&key, |slot| *slot = value.clone());
        }
    }
    rng.shuffle(&mut extra);
    for key in extra {
        map.remove(&key);
    }
    map
}

fn commit_batches(
    db: &Db<Map<u32, String>>,
    model: &Model,
    seed: u64,
    keys: u32,
) -> Map<u32, String> {
    let mut rng = Rng::new(seed);
    db.update(|root| root.clear()).unwrap();
    let mut order: Vec<(u32, String)> = model.iter().map(|(k, v)| (*k, v.clone())).collect();
    rng.shuffle(&mut order);
    let mut extra = Vec::new();
    for chunk in order.chunks(1 + rng.below(9) as usize) {
        let noise = rng.below(keys as u64) as u32;
        db.update(|root| {
            for (key, value) in chunk {
                root.insert(*key, value.clone());
            }
            if !model.contains_key(&noise) {
                root.insert(noise, "noise".repeat(1 + noise as usize % 200));
                extra.push(noise);
            }
        })
        .unwrap();
    }
    db.update(|root| {
        for key in &extra {
            root.remove(key);
        }
    })
    .unwrap();
    (*db.read()).clone()
}

fn all_hashes_agree(model: &Model, seed: u64, keys: u32) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.ferro");
    let base = sorted(model);
    let hash = base.root_hash();
    assert_eq!(hash.is_none(), model.is_empty());
    assert_eq!(reversed(model).root_hash(), hash);
    for salt in 0..3 {
        let other = churned(model, seed ^ salt, keys);
        assert_eq!(other.root_hash(), hash);
        assert!(other == base);
        assert_eq!(other.len(), base.len());
    }
    let db = fast().open::<Map<u32, String>>(&path).unwrap();
    let stored = commit_batches(&db, model, seed, keys);
    assert_eq!(stored.root_hash(), hash);
    assert!(stored == base);
    assert_eq!(stored.diff(&base).count(), 0);
    assert_eq!(base.diff(&stored).count(), 0);
    let version = db.version();
    db.update(|root| *root = churned(model, seed ^ 99, keys)).unwrap();
    assert_eq!(db.version(), version);
    db.update(|root| *root = reversed(model)).unwrap();
    assert_eq!(db.version(), version);
    db.update(|root| {
        root.insert(u32::MAX, "tmp".into());
        root.remove(&u32::MAX);
    })
    .unwrap();
    assert_eq!(db.version(), version);
    drop(stored);
    drop(db);
    let db = uncached().open::<Map<u32, String>>(&path).unwrap();
    assert_eq!(db.read().root_hash(), hash);
    let mut mixed = (*db.read()).clone();
    for (key, value) in model.iter().take(16) {
        mixed.remove(key);
        mixed.insert(*key, value.clone());
    }
    assert_eq!(mixed.root_hash(), hash);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(600), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn insertion_order_never_changes_the_hash(model in contents(40, 40), seed in any::<u64>()) {
        all_hashes_agree(&model, seed, 40);
    }

    #[test]
    fn insertion_order_never_changes_the_hash_large(model in contents(100_000, 400), seed in any::<u64>()) {
        all_hashes_agree(&model, seed, 100_000);
    }

    #[test]
    fn different_contents_have_different_hashes(a in contents(64, 64), b in contents(64, 64)) {
        prop_assert_eq!(sorted(&a).root_hash() == sorted(&b).root_hash(), a == b);
    }
}

fn bulk_forms_agree(model: &Model, seed: u64) {
    let base = sorted(model);
    let hash = base.root_hash();
    let ascending: Map<u32, String> = model.iter().map(|(k, v)| (*k, v.clone())).collect();
    assert_eq!(ascending.root_hash(), hash);
    assert_eq!(ascending.len(), model.len());
    let mut rng = Rng::new(seed);
    let mut order: Vec<(u32, String)> = model.iter().map(|(k, v)| (*k, v.clone())).collect();
    rng.shuffle(&mut order);
    let shuffled: Map<u32, String> = order.iter().cloned().collect();
    assert_eq!(shuffled.root_hash(), hash);
    assert!(shuffled == base);
    let mut doubled = order.clone();
    for (key, value) in &order {
        if rng.chance(40) {
            doubled.push((*key, format!("{value}-stale")));
        }
    }
    let mut stale_first: Vec<(u32, String)> = doubled
        .iter()
        .filter(|(_, value)| value.ends_with("-stale"))
        .cloned()
        .collect();
    stale_first.extend(order.iter().cloned());
    let deduped: Map<u32, String> = stale_first.into_iter().collect();
    assert_eq!(deduped.root_hash(), hash);
    assert_eq!(deduped.len(), model.len());
    for (key, value) in model {
        assert_eq!(deduped.get(key).as_deref(), Some(value));
    }
    let split = order.len() / 2;
    let mut partial: Map<u32, String> = order[..split].iter().cloned().collect();
    partial.extend(order[split..].iter().cloned());
    assert_eq!(partial.root_hash(), hash);
    assert_eq!(partial.len(), model.len());
    let reread: Vec<(u32, String)> = ascending.iter().map(|(k, v)| (k, (*v).clone())).collect();
    assert_eq!(reread, model.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(600), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn bulk_build_matches_incremental_insertion(model in contents(40, 40), seed in any::<u64>()) {
        bulk_forms_agree(&model, seed);
    }

    #[test]
    fn bulk_build_matches_incremental_insertion_large(model in contents(100_000, 600), seed in any::<u64>()) {
        bulk_forms_agree(&model, seed);
    }
}

fn range_forms_agree(model: &Model, keys: u32, low: u32, span: u32, pivot: u32) {
    let high = low.saturating_add(span);
    let pivot = pivot % keys;
    let trimmed: Model = model
        .iter()
        .filter(|(key, _)| !(low..high).contains(*key))
        .map(|(key, value)| (*key, value.clone()))
        .collect();
    let mut removed = sorted(model);
    assert_eq!(removed.remove_range(low..high), model.len() - trimmed.len());
    assert_eq!(removed.root_hash(), sorted(&trimmed).root_hash());
    let dir = tempfile::tempdir().unwrap();
    let db = uncached()
        .open::<Map<u32, String>>(dir.path().join("r.ferro"))
        .unwrap();
    db.update(|root| *root = sorted(model)).unwrap();
    db.update(|root| {
        root.remove_range(low..high);
    })
    .unwrap();
    assert_eq!(db.read().root_hash(), sorted(&trimmed).root_hash());
    let mut head = reversed(model);
    let tail = head.split_off(&pivot);
    let mut left = model.clone();
    let right = left.split_off(&pivot);
    assert_eq!(head.root_hash(), sorted(&left).root_hash());
    assert_eq!(tail.root_hash(), sorted(&right).root_hash());
    let mut swapped = tail.clone();
    swapped.append(&mut head);
    assert_eq!(swapped.root_hash(), sorted(model).root_hash());
    let mut parts = (*db.read()).clone();
    let mut rest = parts.split_off(&pivot);
    parts.append(&mut rest);
    assert_eq!(parts.root_hash(), sorted(&trimmed).root_hash());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(600), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn range_removal_matches_incremental_insertion(
        model in contents(40, 40),
        low in 0..50u32,
        span in 0..50u32,
        pivot in any::<u32>(),
    ) {
        range_forms_agree(&model, 40, low, span, pivot);
    }

    #[test]
    fn range_removal_matches_incremental_insertion_large(
        model in contents(100_000, 600),
        low in 0..100_000u32,
        span in 0..100_000u32,
        pivot in any::<u32>(),
    ) {
        range_forms_agree(&model, 100_000, low, span, pivot);
    }
}

fn edited(model: &Model, edits: &[(u32, Option<String>)]) -> Model {
    let mut out = model.clone();
    for (key, value) in edits {
        match value {
            Some(value) => {
                out.insert(*key, value.clone());
            }
            None => {
                out.remove(key);
            }
        }
    }
    out
}

fn batch_forms_agree(model: &Model, edits: &[(u32, Option<String>)]) {
    let wanted = edited(model, edits);
    let hash = sorted(&wanted).root_hash();
    let mut memory = reversed(model);
    memory.apply(edits.iter().cloned());
    assert_eq!(memory.len(), wanted.len());
    assert_eq!(memory.root_hash(), hash);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    let db = uncached().open::<Map<u32, String>>(&path).unwrap();
    db.update(|root| *root = sorted(model)).unwrap();
    db.update(|root| root.apply(edits.iter().cloned())).unwrap();
    assert_eq!(db.read().root_hash(), hash);
    assert_eq!(db.read().len(), wanted.len());
    let puts: Vec<(u32, String)> = edits
        .iter()
        .filter_map(|(key, value)| Some((*key, value.clone()?)))
        .collect();
    let mut grown = sorted(model);
    grown.extend(puts.iter().cloned());
    let mut wanted_puts = model.clone();
    wanted_puts.extend(puts.iter().cloned());
    assert_eq!(grown.root_hash(), sorted(&wanted_puts).root_hash());
    drop(db);
    let db = uncached().open::<Map<u32, String>>(&path).unwrap();
    assert_eq!(db.read().root_hash(), hash);
    db.verify().unwrap();
}

fn batch(keys: u32, max: usize) -> impl Strategy<Value = Vec<(u32, Option<String>)>> {
    let edit = (0..keys, prop::option::weighted(0.7, (any::<u64>(), sizes())));
    prop::collection::vec(edit, 0..max).prop_map(|edits| {
        edits
            .into_iter()
            .map(|(key, value)| (key, value.map(|(salt, len)| text(salt, len))))
            .collect()
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(600), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn batches_match_incremental_insertion(model in contents(40, 40), edits in batch(50, 40)) {
        batch_forms_agree(&model, &edits);
    }

    #[test]
    fn batches_match_incremental_insertion_large(model in contents(100_000, 600), edits in batch(100_000, 600)) {
        batch_forms_agree(&model, &edits);
    }

    #[test]
    fn dense_batches_match_incremental_insertion(model in contents(800, 500), edits in batch(800, 900)) {
        batch_forms_agree(&model, &edits);
    }
}

#[test]
fn batch_onto_a_large_tree_matches_incremental_insertion() {
    let mut rng = Rng::new(31);
    let mut map: Map<u64, u64> = (0..150_000u64).map(|key| (key * 3, key)).collect();
    let mut model: BTreeMap<u64, u64> = (0..150_000u64).map(|key| (key * 3, key)).collect();
    for round in 0..40u64 {
        let size = 1 << (round % 14);
        let edits: Vec<(u64, Option<u64>)> = (0..size)
            .map(|_| {
                let key = rng.below(600_000);
                (key, rng.chance(70).then_some(round))
            })
            .collect();
        for (key, value) in &edits {
            match value {
                Some(value) => {
                    model.insert(*key, *value);
                }
                None => {
                    model.remove(key);
                }
            }
        }
        map.apply(edits);
        assert_eq!(map.len(), model.len());
    }
    let direct: Map<u64, u64> = model.iter().map(|(k, v)| (*k, *v)).collect();
    assert_eq!(map.root_hash(), direct.root_hash());
    let got: Vec<(u64, u64)> = map.iter().map(|(k, v)| (k, *v)).collect();
    assert_eq!(got, model.into_iter().collect::<Vec<_>>());
}

#[test]
fn bulk_build_matches_incremental_insertion_at_scale() {
    let mut rng = Rng::new(7);
    let entries: Vec<(u64, u64)> = (0..120_000).map(|i| (rng.next(), i)).collect();
    let mut incremental = Map::new();
    for (key, value) in &entries {
        incremental.insert(*key, *value);
    }
    let bulk: Map<u64, u64> = entries.iter().cloned().collect();
    assert_eq!(bulk.len(), incremental.len());
    assert_eq!(bulk.root_hash(), incremental.root_hash());
    let sequential: Map<u64, u64> = (0..120_000u64).map(|i| (i, i)).collect();
    let mut stepwise = Map::new();
    for i in 0..120_000u64 {
        stepwise.insert(i, i);
    }
    assert_eq!(sequential.root_hash(), stepwise.root_hash());
}

fn expected_changes(old: &Model, new: &Model) -> Vec<Change<u32, String>> {
    let mut changes: BTreeMap<u32, Change<u32, String>> = BTreeMap::new();
    for (key, value) in old {
        match new.get(key) {
            None => {
                changes.insert(*key, Change::Removed(*key, Val::new(value.clone())));
            }
            Some(other) if other != value => {
                let change = Change::Changed(*key, Val::new(value.clone()), Val::new(other.clone()));
                changes.insert(*key, change);
            }
            Some(_) => {}
        }
    }
    for (key, value) in new {
        if !old.contains_key(key) {
            changes.insert(*key, Change::Added(*key, Val::new(value.clone())));
        }
    }
    changes.into_values().collect()
}

fn mutated(base: &Model, seed: u64, keys: u32, edits: u64) -> Model {
    let mut rng = Rng::new(seed);
    let mut out = base.clone();
    for _ in 0..edits {
        let key = rng.below(keys as u64) as u32;
        match rng.below(4) {
            0 => {
                out.remove(&key);
            }
            1 => {
                out.insert(key, text(rng.next(), rng.below(800) as usize));
            }
            2 => {
                if let Some(value) = out.get_mut(&key) {
                    value.push('!');
                }
            }
            _ => {
                if let Some(value) = out.get_mut(&key) {
                    value.push_str(&"g".repeat(520));
                }
            }
        }
    }
    out
}

fn assert_diff(old: &Map<u32, String>, new: &Map<u32, String>, wanted: &[Change<u32, String>]) {
    let got: Vec<Change<u32, String>> = old.diff(new).collect();
    assert_eq!(got, wanted);
}

fn check_diff_all_forms(old: &Model, new: &Model) {
    let wanted = expected_changes(old, new);
    let reverse = expected_changes(new, old);
    let dir = tempfile::tempdir().unwrap();
    let db_old = fast()
        .open::<Map<u32, String>>(dir.path().join("a.ferro"))
        .unwrap();
    let db_new = uncached()
        .open::<Map<u32, String>>(dir.path().join("b.ferro"))
        .unwrap();
    db_old.update(|root| *root = sorted(old)).unwrap();
    db_new.update(|root| *root = reversed(new)).unwrap();
    let stored_old = (*db_old.read()).clone();
    let stored_new = (*db_new.read()).clone();
    assert_diff(&sorted(old), &sorted(new), &wanted);
    assert_diff(&stored_old, &sorted(new), &wanted);
    assert_diff(&sorted(old), &stored_new, &wanted);
    assert_diff(&stored_old, &stored_new, &wanted);
    assert_diff(&stored_new, &stored_old, &reverse);
    let same_db = uncached()
        .open::<Map<u32, String>>(dir.path().join("c.ferro"))
        .unwrap();
    same_db.update(|root| *root = sorted(old)).unwrap();
    let first = same_db.read();
    same_db.update(|root| *root = sorted(new)).unwrap();
    let second = same_db.read();
    assert_diff(&first, &second, &wanted);
    assert_diff(&second, &first, &reverse);
    let mut mixed = (*first).clone();
    for key in old.keys().take(4) {
        mixed.remove(key);
    }
    for (key, value) in old.iter().take(4) {
        mixed.insert(*key, value.clone());
    }
    assert_diff(&mixed, &second, &wanted);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(600), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn diff_of_unrelated_maps_matches_model(old in contents(64, 64), new in contents(64, 64)) {
        check_diff_all_forms(&old, &new);
    }

    #[test]
    fn diff_of_related_maps_matches_model(old in contents(2000, 300), seed in any::<u64>(), edits in 0..40u64) {
        let new = mutated(&old, seed, 2000, edits);
        check_diff_all_forms(&old, &new);
    }

    #[test]
    fn diff_of_sparse_multilevel_maps_matches_model(old in contents(1_000_000, 300), new in contents(1_000_000, 300)) {
        check_diff_all_forms(&old, &new);
    }
}

#[test]
fn diff_is_empty_for_identical_and_both_empty() {
    let empty: Map<u32, String> = Map::new();
    assert_eq!(empty.diff(&Map::new()).count(), 0);
    let map = sorted(&(0..100).map(|n| (n, n.to_string())).collect());
    assert_eq!(map.diff(&map.clone()).count(), 0);
    assert_eq!(empty.diff(&map).count(), 100);
    assert_eq!(map.diff(&empty).count(), 100);
}

#[test]
fn diff_detects_change_inside_spilled_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast()
        .open::<Map<u32, String>>(dir.path().join("s.ferro"))
        .unwrap();
    db.update(|root| {
        for n in 0..200 {
            root.insert(n, "v".repeat(1000));
        }
    })
    .unwrap();
    let before = db.read();
    db.update(|root| {
        root.update(&77, |value| value.push('!'));
    })
    .unwrap();
    let after = db.read();
    let changes: Vec<_> = before.diff(&after).collect();
    assert_eq!(changes.len(), 1);
    assert_eq!(*changes[0].key(), 77);
    assert_eq!(changes[0].before().unwrap().len(), 1000);
    assert_eq!(changes[0].after().unwrap().len(), 1001);
}

static DECODES: AtomicU64 = AtomicU64::new(0);
static ENCODES: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq)]
struct Counted(u64);

impl Data for Counted {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        ENCODES.fetch_add(1, Ordering::Relaxed);
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        DECODES.fetch_add(1, Ordering::Relaxed);
        Ok(Counted(u64::decode(r)?))
    }
}

#[test]
fn diff_cost_tracks_changes_not_size() {
    let dir = tempfile::tempdir().unwrap();
    let db = uncached()
        .open::<Map<u64, Counted>>(dir.path().join("d.ferro"))
        .unwrap();
    db.update(|root| {
        for n in 0..100_000 {
            root.insert(n, Counted(n));
        }
    })
    .unwrap();
    let before = db.read();
    db.update(|root| {
        root.insert(50_000, Counted(0));
    })
    .unwrap();
    let after = db.read();
    let start = DECODES.load(Ordering::Relaxed);
    let changes: Vec<_> = before.diff(&after).collect();
    let cost = DECODES.load(Ordering::Relaxed) - start;
    assert_eq!(changes.len(), 1);
    assert!(cost < 5_000, "diff decoded {cost} values for one change");
}

#[test]
fn levels_of_test_keys_are_spread() {
    let high = (0..100_000u64).filter(|key| level_of(key) >= 2).count();
    assert!((100..2000).contains(&high));
}

#[test]
fn commit_cost_does_not_grow_with_nested_map_size() {
    let dir = tempfile::tempdir().unwrap();
    let db = roomy()
        .open::<Map<u64, Map<u64, Counted>>>(dir.path().join("n.ferro"))
        .unwrap();
    let mut costs = Vec::new();
    for round in 0..3000u64 {
        let start = ENCODES.load(Ordering::Relaxed);
        db.update(|root| {
            root.get_or_insert_with(0, Map::new)
                .insert(round, Counted(round));
        })
        .unwrap();
        costs.push(ENCODES.load(Ordering::Relaxed) - start);
    }
    let late: u64 = costs[2900..].iter().sum::<u64>() / 100;
    assert!(late < 400, "commit encoded {late} values on average");
}

static TALLIED: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq)]
struct Tallied(u64);

impl Data for Tallied {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        TALLIED.fetch_add(1, Ordering::Relaxed);
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Tallied(u64::decode(r)?))
    }
}

#[test]
fn a_commit_encodes_only_the_values_it_changed() {
    let dir = tempfile::tempdir().unwrap();
    let db = roomy()
        .open::<Map<u64, Tallied>>(dir.path().join("t.ferro"))
        .unwrap();
    db.update(|root| root.extend((0..20_000u64).map(|key| (key * 2, Tallied(key)))))
        .unwrap();
    let mut rng = Rng::new(5);
    for _ in 0..50 {
        let key = rng.below(40_000) | 1;
        db.update(|root| root.insert(key, Tallied(key))).unwrap();
    }
    let start = TALLIED.load(Ordering::Relaxed);
    for _ in 0..200 {
        let key = rng.below(40_000) | 1;
        db.update(|root| root.insert(key, Tallied(key))).unwrap();
    }
    let each = (TALLIED.load(Ordering::Relaxed) - start) / 200;
    assert!(each < 24, "a single insert encoded {each} values");
}

#[test]
fn commits_along_cached_paths_hash_as_a_fresh_build() {
    type Nested = BTreeMap<u32, BTreeMap<u32, String>>;
    fn build(model: &Nested) -> Map<u32, Map<u32, String>> {
        model
            .iter()
            .map(|(key, inner)| (*key, inner.iter().map(|(k, v)| (*k, v.clone())).collect()))
            .collect()
    }
    for options in [roomy, uncached] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.ferro");
        let mut db = options().open::<Map<u32, Map<u32, String>>>(&path).unwrap();
        let mut model = Nested::new();
        let mut rng = Rng::new(11);
        for round in 0..400u64 {
            let edits = 1 + rng.below(3);
            let mut planned = Vec::new();
            for _ in 0..edits {
                let outer = rng.below(300) as u32;
                let inner = rng.below(40) as u32;
                let len = if rng.chance(10) { 600 } else { rng.below(20) as usize };
                planned.push((rng.below(6), outer, inner, text(rng.next(), len)));
            }
            db.update(|root| {
                for (kind, outer, inner, value) in &planned {
                    match kind {
                        0 => {
                            root.remove(outer);
                            model.remove(outer);
                        }
                        1 => {
                            let fresh: Map<u32, String> = [(*inner, value.clone())].into_iter().collect();
                            root.insert(*outer, fresh);
                            model.insert(*outer, [(*inner, value.clone())].into_iter().collect());
                        }
                        2 => {
                            if let Some(nested) = root.get_mut(outer) {
                                nested.remove(inner);
                                model.get_mut(outer).unwrap().remove(inner);
                            }
                        }
                        3 => {
                            if root.get_mut(outer).is_some() {
                                model.get_mut(outer).unwrap();
                            }
                        }
                        _ => {
                            root.get_or_insert_with(*outer, Map::new).insert(*inner, value.clone());
                            model.entry(*outer).or_default().insert(*inner, value.clone());
                        }
                    }
                }
            })
            .unwrap();
            let snapshot = db.read();
            assert_eq!(snapshot.root_hash(), build(&model).root_hash(), "round {round}");
            if round % 100 == 99 {
                drop(snapshot);
                drop(db);
                db = options().open(&path).unwrap();
            }
        }
        db.verify().unwrap();
        assert_eq!(db.read().root_hash(), build(&model).root_hash());
    }
}
