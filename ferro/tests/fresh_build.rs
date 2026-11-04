mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use common::{Rng, cases, fast, roomy, text, uncached};
use ferro::store::{Store, StoreOptions};
use ferro::{
    Data, Db, Hash, Keep, Map, Options, Reclaim, Record, Set, Snapshot, Summary, Table, hash_of,
};

#[derive(Clone, Copy, Debug, PartialEq, Data)]
struct Tally {
    count: u64,
    bytes: u64,
    last: u32,
}

impl Summary<u32, String> for Tally {
    fn of(key: &u32, value: &String) -> Self {
        Tally {
            count: 1,
            bytes: value.len() as u64,
            last: *key,
        }
    }

    fn combine(&self, other: &Self) -> Self {
        Tally {
            count: self.count + other.count,
            bytes: self.bytes + other.bytes,
            last: other.last,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
struct Row {
    #[key]
    id: u32,
    #[index]
    group: u8,
    #[index(each)]
    tags: Vec<u8>,
    note: String,
}

#[derive(Clone, Default, Data)]
struct Root {
    plain: Map<u32, String>,
    summed: Map<u32, String, Tally>,
    nested: Map<u16, Map<u32, String>>,
    set: Set<u64>,
    rows: Table<Row>,
}

type Entries = BTreeMap<u32, String>;

#[derive(Clone, Default, Debug, PartialEq)]
struct Model {
    plain: Entries,
    summed: Entries,
    nested: BTreeMap<u16, Entries>,
    set: BTreeSet<u64>,
    rows: BTreeMap<u32, Row>,
}

fn build<S: Summary<u32, String>>(entries: &Entries, rng: &mut Rng) -> Map<u32, String, S> {
    if rng.chance(50) {
        return entries
            .iter()
            .map(|(key, value)| (*key, value.clone()))
            .collect();
    }
    let mut order: Vec<(&u32, &String)> = entries.iter().collect();
    rng.shuffle(&mut order);
    let mut map = Map::default();
    for (key, value) in order {
        map.insert(*key, value.clone());
    }
    map
}

fn fresh(model: &Model, seed: u64) -> Root {
    let mut rng = Rng::new(seed);
    let mut nested = Map::new();
    for (outer, inner) in &model.nested {
        nested.insert(*outer, build::<()>(inner, &mut rng));
    }
    let mut rows: Vec<&Row> = model.rows.values().collect();
    rng.shuffle(&mut rows);
    let mut table: Table<Row> = Table::new();
    for row in rows {
        table.insert(row.clone());
    }
    Root {
        plain: build(&model.plain, &mut rng),
        summed: build(&model.summed, &mut rng),
        nested,
        set: model.set.iter().copied().collect(),
        rows: table,
    }
}

fn row(rng: &mut Rng, id: u32) -> Row {
    Row {
        id,
        group: rng.below(6) as u8,
        tags: (0..rng.below(4)).map(|_| rng.below(10) as u8).collect(),
        note: value(rng),
    }
}

fn size(rng: &mut Rng) -> usize {
    match rng.below(10) {
        0..=4 => rng.below(24) as usize,
        5..=7 => 500 + rng.below(24) as usize,
        _ => 524 + rng.below(1500) as usize,
    }
}

fn value(rng: &mut Rng) -> String {
    let len = size(rng);
    text(rng.next(), len)
}

fn pick(rng: &mut Rng, entries: &Entries, keys: u32) -> u32 {
    if !entries.is_empty() && rng.chance(70) {
        let index = rng.below(entries.len() as u64) as usize;
        if let Some(key) = entries.keys().nth(index) {
            return *key;
        }
    }
    rng.below(u64::from(keys)) as u32
}

fn span(rng: &mut Rng, keys: u32) -> (u32, u32) {
    let low = rng.below(u64::from(keys)) as u32;
    let width = 1 + rng.below(u64::from(keys / 64).max(4)) as u32;
    (low, low.saturating_add(width))
}

fn ranged(entries: &Entries, low: u32, high: u32) -> Entries {
    entries
        .range(low..high)
        .map(|(key, value)| (*key, value.clone()))
        .collect()
}

fn same<S: Summary<u32, String>>(map: &Map<u32, String, S>, entries: &Entries, rng: &mut Rng) {
    assert_eq!(map.len(), entries.len());
    assert_eq!(map.root_hash(), build::<S>(entries, rng).root_hash());
}

fn edit_map<S: Summary<u32, String>>(
    map: &mut Map<u32, String, S>,
    entries: &mut Entries,
    rng: &mut Rng,
    keys: u32,
) {
    match rng.below(20) {
        0..=4 => {
            let key = pick(rng, entries, keys);
            let value = value(rng);
            map.insert(key, value.clone());
            entries.insert(key, value);
        }
        5..=6 => {
            let key = pick(rng, entries, keys);
            assert_eq!(map.remove(&key).map(|v| (*v).clone()), entries.remove(&key));
        }
        7 => {
            let key = pick(rng, entries, keys);
            let value = value(rng);
            if let Some(slot) = map.get_mut(&key) {
                *slot = value.clone();
            }
            if let Some(slot) = entries.get_mut(&key) {
                *slot = value;
            }
        }
        8 => {
            let key = pick(rng, entries, keys);
            let len = size(rng);
            let change = |slot: &mut String| {
                slot.truncate(len);
                while slot.len() < len {
                    slot.push('q');
                }
            };
            assert_eq!(
                map.update(&key, change).is_some(),
                entries.contains_key(&key)
            );
            if let Some(slot) = entries.get_mut(&key) {
                change(slot);
            }
        }
        9 => {
            let key = pick(rng, entries, keys);
            let _ = map.get_mut(&key);
        }
        10 => {
            let (low, high) = span(rng, keys / 4);
            let removed = map.remove_range(low..high);
            let before = entries.len();
            entries.retain(|key, _| !(low..high).contains(key));
            assert_eq!(removed, before - entries.len());
        }
        11 => {
            let (low, high) = span(rng, keys);
            let mut taken = map.take_range(low..high);
            let wanted = ranged(entries, low, high);
            same(&taken, &wanted, rng);
            if rng.chance(50) {
                map.append(&mut taken);
            } else {
                entries.retain(|key, _| !(low..high).contains(key));
            }
        }
        12 => {
            let key = pick(rng, entries, keys);
            let mut tail = map.split_off(&key);
            let wanted = entries.split_off(&key);
            same(&tail, &wanted, rng);
            if wanted.len() > 300 || rng.chance(50) {
                map.append(&mut tail);
                entries.extend(wanted);
            }
        }
        13 => {
            let base = entries.keys().next_back().map_or(0, |last| last + 1);
            let count = rng.below(30);
            let mut other: Map<u32, String, S> = Map::default();
            for step in 0..count {
                let key = base.saturating_add(step as u32 * (1 + rng.below(3) as u32));
                let value = value(rng);
                other.insert(key, value.clone());
                entries.insert(key, value);
            }
            map.append(&mut other);
        }
        14 => {
            let mut other: Map<u32, String, S> = Map::default();
            for _ in 0..rng.below(20) {
                let key = pick(rng, entries, keys);
                let value = value(rng);
                other.insert(key, value.clone());
                entries.insert(key, value);
            }
            map.append(&mut other);
        }
        15 => {
            let mut edits = Vec::new();
            for _ in 0..rng.below(60) {
                let key = pick(rng, entries, keys);
                let edit = rng.chance(70).then(|| value(rng));
                edits.push((key, edit));
            }
            for (key, edit) in &edits {
                match edit {
                    Some(value) => {
                        entries.insert(*key, value.clone());
                    }
                    None => {
                        entries.remove(key);
                    }
                }
            }
            map.apply(edits);
        }
        16 => {
            let items: Vec<(u32, String)> = (0..rng.below(40))
                .map(|_| (pick(rng, entries, keys), value(rng)))
                .collect();
            entries.extend(items.clone());
            map.extend(items);
        }
        17 => {
            let modulus = 16 + rng.below(48);
            let salt = rng.next();
            let doomed = |key: &u32| (u64::from(*key) ^ salt).is_multiple_of(modulus);
            map.retain(|key, _| !doomed(key));
            entries.retain(|key, _| !doomed(key));
        }
        18 => {
            if rng.chance(50) {
                assert_eq!(
                    map.pop_first().map(|(k, v)| (k, (*v).clone())),
                    entries.pop_first()
                );
            } else {
                assert_eq!(
                    map.pop_last().map(|(k, v)| (k, (*v).clone())),
                    entries.pop_last()
                );
            }
        }
        _ => {
            if rng.chance(20) {
                *map = map
                    .iter()
                    .map(|(key, value)| (key, (*value).clone()))
                    .collect();
            } else if rng.chance(2) {
                map.clear();
                entries.clear();
            }
        }
    }
}

fn bulk_edits(rng: &mut Rng, entries: &Entries, keys: u32) -> Vec<(u32, Option<String>)> {
    let count = 2_000 + rng.below(6_000);
    let mut edits = Vec::new();
    for _ in 0..count {
        let key = pick(rng, entries, keys);
        let edit = rng.chance(92).then(|| {
            let len = if rng.chance(5) {
                600
            } else {
                rng.below(20) as usize
            };
            text(rng.next(), len)
        });
        edits.push((key, edit));
    }
    edits
}

fn bulk_map<S: Summary<u32, String>>(
    map: &mut Map<u32, String, S>,
    entries: &mut Entries,
    rng: &mut Rng,
    keys: u32,
) {
    let edits = bulk_edits(rng, entries, keys);
    if rng.chance(50) {
        for (key, edit) in &edits {
            match edit {
                Some(value) => {
                    map.insert(*key, value.clone());
                }
                None => {
                    map.remove(key);
                }
            }
        }
    } else {
        map.apply(edits.clone());
    }
    for (key, edit) in edits {
        match edit {
            Some(value) => {
                entries.insert(key, value);
            }
            None => {
                entries.remove(&key);
            }
        }
    }
}

fn bulk_edit(root: &mut Root, model: &mut Model, rng: &mut Rng) {
    match rng.below(8) {
        0..=2 => bulk_map(&mut root.plain, &mut model.plain, rng, 60_000),
        3 | 4 => bulk_map(&mut root.summed, &mut model.summed, rng, 30_000),
        5 | 6 => {
            let outer = BIG_OUTER + rng.below(3) as u16;
            let inner = root.nested.get_or_insert_with(outer, Map::new);
            bulk_map(inner, model.nested.entry(outer).or_default(), rng, 40_000);
        }
        _ => {
            for _ in 0..2_000 + rng.below(4_000) {
                let id = rng.below(12_000) as u32;
                if rng.chance(90) {
                    let row = row(rng, id);
                    root.rows.insert(row.clone());
                    model.rows.insert(id, row);
                } else {
                    root.rows.remove(&id);
                    model.rows.remove(&id);
                }
            }
        }
    }
}

const BIG_OUTER: u16 = 1_000;

fn edit_nested(root: &mut Root, model: &mut Model, rng: &mut Rng) {
    let outer = if rng.chance(10) {
        BIG_OUTER + rng.below(3) as u16
    } else {
        rng.below(120) as u16
    };
    match rng.below(8) {
        0..=2 => {
            let key = rng.below(48) as u32;
            let value = value(rng);
            root.nested
                .get_or_insert_with(outer, Map::new)
                .insert(key, value.clone());
            model.nested.entry(outer).or_default().insert(key, value);
        }
        3 => {
            let key = rng.below(48) as u32;
            if let Some(inner) = root.nested.get_mut(&outer) {
                inner.remove(&key);
            }
            if let Some(inner) = model.nested.get_mut(&outer) {
                inner.remove(&key);
            }
        }
        4 => {
            root.nested.remove(&outer);
            model.nested.remove(&outer);
        }
        5 => {
            let entries: Entries = (0..rng.below(12))
                .map(|_| (rng.below(48) as u32, value(rng)))
                .collect();
            root.nested.insert(outer, build::<()>(&entries, rng));
            model.nested.insert(outer, entries);
        }
        6 => {
            let _ = root.nested.get_mut(&outer);
        }
        _ => {
            if let Some(inner) = root.nested.get_mut(&outer) {
                let entries = model
                    .nested
                    .get_mut(&outer)
                    .expect("model holds the same outer keys");
                edit_map(inner, entries, rng, 48);
            }
        }
    }
}

fn edit_set(root: &mut Root, model: &mut Model, rng: &mut Rng) {
    match rng.below(4) {
        0 | 1 => {
            let value = rng.below(5_000);
            assert_eq!(root.set.insert(value), model.set.insert(value));
        }
        2 => {
            let value = rng.below(5_000);
            assert_eq!(root.set.remove(&value), model.set.remove(&value));
        }
        _ => {
            let low = rng.below(5_000);
            let high = low + rng.below(300);
            root.set.remove_range(low..high);
            model.set.retain(|value| !(low..high).contains(value));
        }
    }
}

fn edit_rows(root: &mut Root, model: &mut Model, rng: &mut Rng) {
    let id = rng.below(2_000) as u32;
    match rng.below(8) {
        0..=2 => {
            let row = row(rng, id);
            root.rows.insert(row.clone());
            model.rows.insert(id, row);
        }
        3 => {
            root.rows.remove(&id);
            model.rows.remove(&id);
        }
        4 | 5 => {
            let group = rng.below(6) as u8;
            let tag = rng.below(10) as u8;
            let change = |row: &mut Row| {
                row.group = group;
                row.tags.push(tag);
            };
            assert_eq!(
                root.rows.update(&id, change).is_some(),
                model.rows.contains_key(&id)
            );
            if let Some(row) = model.rows.get_mut(&id) {
                change(row);
            }
        }
        6 => {
            let high = id + rng.below(40) as u32;
            let drained = root.rows.drain_range(id..high);
            let before = model.rows.len();
            model.rows.retain(|key, _| !(id..high).contains(key));
            assert_eq!(drained.len(), before - model.rows.len());
        }
        _ => {
            let group = rng.below(6) as u8;
            let modulus = 4 + rng.below(8) as u32;
            let doomed = |row: &Row| row.group == group && row.id.is_multiple_of(modulus);
            root.rows.retain(|row| !doomed(row));
            model.rows.retain(|_, row| !doomed(row));
        }
    }
}

type Kept = Vec<(Snapshot<Root>, Model)>;

fn edit_from(root: &mut Root, model: &mut Model, rng: &mut Rng, kept: &Kept) {
    let Some((base, base_model)) = kept.get(rng.below(kept.len().max(1) as u64) as usize) else {
        return;
    };
    if rng.chance(30) {
        root.summed = base.summed.clone();
        model.summed = base_model.summed.clone();
        return;
    }
    let mut theirs = base.plain.clone();
    let mut wanted = base_model.plain.clone();
    for _ in 0..1 + rng.below(30) {
        edit_map(&mut theirs, &mut wanted, rng, 60_000);
    }
    root.plain.merge(&base.plain, &theirs, |conflict| {
        conflict.theirs.map(|value| (*value).clone())
    });
    let keys: BTreeSet<u32> = base_model
        .plain
        .keys()
        .chain(wanted.keys())
        .copied()
        .collect();
    for key in keys {
        let after = wanted.get(&key);
        if base_model.plain.get(&key) != after {
            match after {
                Some(value) => {
                    model.plain.insert(key, value.clone());
                }
                None => {
                    model.plain.remove(&key);
                }
            }
        }
    }
}

fn edit(root: &mut Root, model: &mut Model, rng: &mut Rng, kept: &Kept) {
    match rng.below(24) {
        0..=7 => edit_map(&mut root.plain, &mut model.plain, rng, 60_000),
        8..=11 => edit_map(&mut root.summed, &mut model.summed, rng, 30_000),
        12..=16 => edit_nested(root, model, rng),
        17..=18 => edit_set(root, model, rng),
        19..=22 => edit_rows(root, model, rng),
        _ => edit_from(root, model, rng, kept),
    }
}

fn objects(path: &Path, root: ferro::Ptr) -> BTreeMap<Hash, (Vec<u8>, Vec<Hash>)> {
    let options = StoreOptions {
        read_only: true,
        cache_bytes: 1,
        ..StoreOptions::default()
    };
    let (store, _) = Store::open(path, options).unwrap();
    let mut found = BTreeMap::new();
    store
        .walk(root, |ptr, object| {
            let links = object.links.iter().map(|link| link.hash).collect();
            found.insert(ptr.hash, (object.body.clone(), links));
        })
        .unwrap();
    store.close();
    found
}

fn same_objects(db: &Db<Root>, model: &Model, seed: u64) {
    let root = db.read().commit().root.expect("a committed root");
    let held = objects(&db.path(), root);
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("fresh.ferro");
    let built = fast().open::<Root>(&path).unwrap();
    let value = fresh(model, seed);
    built.update(|root| *root = value).unwrap();
    let built_root = built.read().commit().root.expect("a committed root");
    assert_eq!(built_root.hash, root.hash);
    let rebuilt = objects(&path, built_root);
    assert_eq!(held.len(), rebuilt.len());
    assert!(
        held == rebuilt,
        "the file holds objects a fresh build does not"
    );
}

fn options(choice: u64) -> Options {
    let options = match choice % 3 {
        0 => roomy(),
        1 => fast(),
        _ => uncached(),
    };
    options.reclaim(Reclaim { every_bytes: 0 })
}

fn check(db: &Db<Root>, model: &Model, seed: u64) {
    let snapshot = db.read();
    let built = fresh(model, seed);
    assert_eq!(snapshot.plain.root_hash(), built.plain.root_hash());
    assert_eq!(snapshot.summed.root_hash(), built.summed.root_hash());
    assert_eq!(snapshot.summed.summary(), built.summed.summary());
    assert_eq!(snapshot.nested.root_hash(), built.nested.root_hash());
    assert_eq!(snapshot.set.len(), model.set.len());
    assert_eq!(hash_of(&snapshot.rows), hash_of(&built.rows));
    assert_eq!(hash_of(&*snapshot), hash_of(&built));
}

fn take(mirror: &mut Db<Root>, db: &Db<Root>, dir: &Path) {
    let pull = |mirror: &Db<Root>| {
        mirror.pull(db, "origin", |root, _, theirs| {
            *root = (**theirs).clone();
            ("", ())
        })
    };
    match pull(mirror) {
        Ok(_) => {}
        Err(ferro::Error::NoCommonAncestor) => {
            let path = dir.join(format!("mirror-{}.ferro", mirror.version()));
            *mirror = fast().open::<Root>(path).unwrap();
            pull(mirror).unwrap();
        }
        Err(error) => panic!("pull failed: {error}"),
    }
}

fn run(seed: u64, rounds: u64) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("edits.ferro");
    let mut rng = Rng::new(seed);
    let mut db = options(seed).open::<Root>(&path).unwrap();
    let mut mirror = options(seed + 1)
        .open::<Root>(dir.path().join("mirror.ferro"))
        .unwrap();
    let mut model = Model::default();
    for key in 0..12_000u32 {
        model.plain.insert(key * 5, value(&mut rng));
    }
    for key in 0..8_000u32 {
        model.summed.insert(key * 3, value(&mut rng));
    }
    for outer in BIG_OUTER..BIG_OUTER + 3 {
        let inner = model.nested.entry(outer).or_default();
        for key in 0..5_000u32 {
            inner.insert(key * 7, value(&mut rng));
        }
    }
    for outer in 0..100u16 {
        let inner = model.nested.entry(outer).or_default();
        for _ in 0..rng.below(30) {
            inner.insert(rng.below(48) as u32, value(&mut rng));
        }
    }
    model.set.extend((0..3_000).map(|value| value * 3));
    for id in 0..800u32 {
        let row = row(&mut rng, id * 2);
        model.rows.insert(row.id, row);
    }
    let mut kept: Kept = Vec::new();
    let loaded = fresh(&model, seed);
    db.update(|root| *root = loaded).unwrap();
    check(&db, &model, seed);
    for round in 0..rounds {
        let bulk = rng.chance(8);
        let probe = rng.chance(15);
        let mut edits = 1 + rng.below(12);
        if rng.chance(10) {
            edits += 60;
        }
        let mut working = model.clone();
        db.update(|root| {
            if bulk {
                bulk_edit(root, &mut working, &mut rng);
            }
            for _ in 0..edits {
                edit(root, &mut working, &mut rng, &kept);
            }
            if probe {
                let built = fresh(&working, round);
                assert_eq!(
                    root.plain.root_hash(),
                    built.plain.root_hash(),
                    "dirty plain, round {round}"
                );
                assert_eq!(
                    root.summed.root_hash(),
                    built.summed.root_hash(),
                    "dirty summed, round {round}"
                );
                assert_eq!(
                    root.nested.root_hash(),
                    built.nested.root_hash(),
                    "dirty nested, round {round}"
                );
                assert_eq!(
                    hash_of(&root.rows),
                    hash_of(&built.rows),
                    "dirty rows, round {round}"
                );
                assert_eq!(
                    hash_of(&*root),
                    hash_of(&built),
                    "dirty root, round {round}"
                );
            }
        })
        .unwrap();
        model = working;
        check(&db, &model, round);
        if rng.chance(20) {
            if kept.len() == 4 {
                kept.remove(rng.below(4) as usize);
            }
            kept.push((db.read(), model.clone()));
        }
        if rng.chance(12) {
            take(&mut mirror, &db, dir.path());
            check(&mirror, &model, round ^ 9);
        }
        match rng.below(40) {
            0 | 1 => {
                drop(db);
                db = options(rng.next()).open::<Root>(&path).unwrap();
                check(&db, &model, round ^ 1);
            }
            2 => {
                db.compact(Keep::All).unwrap();
                check(&db, &model, round ^ 2);
            }
            3 => {
                db.compact(Keep::Last(1 + rng.below(4) as usize)).unwrap();
                check(&db, &model, round ^ 3);
            }
            4 => {
                db.compact(Keep::Last(1)).unwrap();
                drop(db);
                db = options(rng.next()).open::<Root>(&path).unwrap();
                check(&db, &model, round ^ 4);
            }
            5 => {
                db.verify_typed().unwrap();
            }
            _ => {}
        }
    }
    take(&mut mirror, &db, dir.path());
    check(&mirror, &model, seed ^ 11);
    mirror.verify_typed().unwrap();
    db.verify_typed().unwrap();
    same_objects(&db, &model, seed);
    db.compact(Keep::Last(1)).unwrap();
    same_objects(&db, &model, seed ^ 5);
    drop(db);
    let db = uncached().read_only(true).open::<Root>(&path).unwrap();
    check(&db, &model, seed ^ 7);
    db.verify_typed().unwrap();
}

#[test]
fn edits_across_commits_reopens_and_compactions_match_a_fresh_build() {
    for seed in 1..=u64::from(cases(3)) {
        run(seed, 80);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Data)]
#[ferro(transparent)]
struct Sum(u64);

impl Summary<u32, u64> for Sum {
    fn of(_key: &u32, value: &u64) -> Self {
        Sum(*value)
    }

    fn combine(&self, other: &Self) -> Self {
        Sum(self.0 + other.0)
    }
}

type Counters = Map<u32, u64, Sum>;

const WRITERS: u32 = 6;

fn writer(db: &Db<Counters>, thread: u32, closures: u64) -> BTreeMap<u32, u64> {
    let mut rng = Rng::new(u64::from(thread) + 100);
    let mut mine: BTreeMap<u32, u64> = BTreeMap::new();
    let mut last = 0;
    for _ in 0..closures {
        let mut planned = Vec::new();
        for _ in 0..1 + rng.below(4) {
            let key = thread + WRITERS * rng.below(400) as u32;
            planned.push((rng.below(5), key, rng.below(1_000)));
        }
        let outcome = rng.below(20);
        let mut working = mine.clone();
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.try_commit_with(|root| {
                for (kind, key, amount) in &planned {
                    assert_eq!(root.get(key).map(|value| *value), working.get(key).copied());
                    match kind {
                        0 | 1 => {
                            root.insert(*key, *amount);
                            working.insert(*key, *amount);
                        }
                        2 => {
                            root.remove(key);
                            working.remove(key);
                        }
                        _ => {
                            if let Some(value) = root.get_mut(key) {
                                *value += amount;
                            }
                            if let Some(value) = working.get_mut(key) {
                                *value += amount;
                            }
                        }
                    }
                }
                match outcome {
                    0 => Err(ferro::Error::Detached),
                    1 => std::panic::resume_unwind(Box::new("planned")),
                    _ => Ok((format!("t{thread}"), ())),
                }
            })
        }));
        match ran {
            Ok(Ok(committed)) => {
                assert_ne!(outcome, 0);
                assert!(committed.version >= last);
                last = committed.version;
                mine = working;
                let snapshot = db.read();
                assert!(snapshot.version() >= committed.version);
                for (_, key, _) in &planned {
                    assert_eq!(
                        snapshot.get(key).map(|value| *value),
                        mine.get(key).copied()
                    );
                }
            }
            Ok(Err(error)) => {
                assert_eq!(outcome, 0, "{error}");
            }
            Err(_) => assert_eq!(outcome, 1),
        }
    }
    mine
}

#[test]
fn coalesced_writers_keep_their_guarantees_and_match_a_fresh_build() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("coalesce.ferro");
    let synced = || fast().durability(ferro::Durability::Fsync);
    let choices: [&dyn Fn() -> Options; 4] = [&fast, &synced, &roomy, &uncached];
    for (round, options) in choices.into_iter().enumerate() {
        let db = options()
            .coalesce(true)
            .reclaim(Reclaim { every_bytes: 0 })
            .open::<Counters>(&path)
            .unwrap();
        db.update(|root| root.clear()).unwrap();
        let done = std::sync::atomic::AtomicBool::new(false);
        let models = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut version = 0;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    let snapshot = db.read();
                    assert!(snapshot.version() >= version);
                    version = snapshot.version();
                    let total: u64 = snapshot.values().map(|value| *value).sum();
                    assert_eq!(snapshot.summary().map_or(0, |sum| sum.0), total);
                }
            });
            let compactor = scope.spawn(|| {
                let mut rng = Rng::new(round as u64);
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    let keep = if rng.chance(50) {
                        Keep::All
                    } else {
                        Keep::Last(2)
                    };
                    db.compact(keep).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(3));
                }
            });
            let writers: Vec<_> = (0..WRITERS)
                .map(|thread| {
                    let db = &db;
                    scope.spawn(move || writer(db, thread, 400))
                })
                .collect();
            let models: Vec<BTreeMap<u32, u64>> = writers
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect();
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            reader.join().unwrap();
            compactor.join().unwrap();
            models
        });
        let model: BTreeMap<u32, u64> = models.into_iter().flatten().collect();
        let built: Counters = model.iter().map(|(key, value)| (*key, *value)).collect();
        assert_eq!(db.read().root_hash(), built.root_hash());
        assert_eq!(db.read().summary(), built.summary());
        db.verify_typed().unwrap();
        drop(db);
        let db = uncached().read_only(true).open::<Counters>(&path).unwrap();
        assert_eq!(db.read().root_hash(), built.root_hash());
    }
}

type Inner = BTreeMap<u32, u32>;
type Holder = (String, Map<u32, u32>);
type Outer = Map<u32, Holder>;

fn holder(pad: usize, inner: &Inner) -> Holder {
    (
        "p".repeat(pad),
        inner.iter().map(|(key, value)| (*key, *value)).collect(),
    )
}

fn outer_of(model: &BTreeMap<u32, (usize, Inner)>) -> Outer {
    model
        .iter()
        .map(|(key, (pad, inner))| (*key, holder(*pad, inner)))
        .collect()
}

#[test]
fn large_commits_of_nested_maps_match_a_fresh_build() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("nested.ferro");
    let other_path = dir.path().join("other.ferro");
    let mut rng = Rng::new(9);
    let mut model: BTreeMap<u32, (usize, Inner)> = BTreeMap::new();
    for key in 0..20_000u32 {
        let inner = (0..rng.below(4))
            .map(|step| (step as u32, rng.below(100) as u32))
            .collect();
        let pad = if rng.chance(5) {
            600
        } else {
            rng.below(8) as usize
        };
        model.insert(key * 3, (pad, inner));
    }
    let other = fast().open::<Outer>(&other_path).unwrap();
    let mut db = roomy()
        .reclaim(Reclaim { every_bytes: 0 })
        .open::<Outer>(&path)
        .unwrap();
    let initial = outer_of(&model);
    db.update(|root| *root = initial).unwrap();
    for round in 0..24u64 {
        let snapshot = db.read();
        other
            .update(|root| {
                for _ in 0..50 {
                    root.insert(rng.below(60_000) as u32, holder(3, &Inner::from([(1, 1)])));
                }
            })
            .unwrap();
        let foreign = other.read();
        let kind = round % 6;
        let mut working = model.clone();
        db.update(|root| {
            for _ in 0..2_000 + rng.below(4_000) {
                let key = rng.below(60_000) as u32;
                match kind {
                    0 => {
                        let (pad, inner) = working.entry(key).or_insert((4, Inner::new()));
                        let step = rng.below(10) as u32;
                        inner.insert(step, round as u32);
                        let pad = *pad;
                        match root.get_mut(&key) {
                            Some(value) => {
                                value.1.insert(step, round as u32);
                            }
                            None => {
                                root.insert(key, holder(pad, &Inner::from([(step, round as u32)])));
                            }
                        }
                    }
                    1 => {
                        let inner: Inner = (0..rng.below(5)).map(|step| (step as u32, 7)).collect();
                        let pad = if rng.chance(30) { 600 } else { 2 };
                        root.insert(key, holder(pad, &inner));
                        working.insert(key, (pad, inner));
                    }
                    2 => {
                        let source = (key / 3) * 3;
                        if let Some(value) = snapshot.get(&source) {
                            root.insert(key, (*value).clone());
                            let copied = model
                                .get(&source)
                                .cloned()
                                .expect("the snapshot matches the model");
                            working.insert(key, copied);
                        }
                    }
                    3 => {
                        if let Some((found, value)) = foreign.range(key..).next() {
                            root.insert(found, (*value).clone());
                            working.insert(found, (3, Inner::from([(1, 1)])));
                        }
                    }
                    4 => {
                        if root.get_mut(&key).is_some() {
                            working.get_mut(&key).expect("model holds every key");
                        }
                    }
                    _ => {
                        root.remove(&key);
                        working.remove(&key);
                    }
                }
            }
        })
        .unwrap();
        model = working;
        drop(snapshot);
        assert_eq!(
            db.read().root_hash(),
            outer_of(&model).root_hash(),
            "round {round}"
        );
        if round % 8 == 7 {
            db.compact(Keep::Last(1)).unwrap();
            drop(db);
            db = options(round).open::<Outer>(&path).unwrap();
            assert_eq!(
                db.read().root_hash(),
                outer_of(&model).root_hash(),
                "reopened {round}"
            );
        }
    }
    db.verify_typed().unwrap();
    let held: BTreeMap<u32, (usize, Inner)> = db
        .read()
        .iter()
        .map(|(key, value)| {
            (
                key,
                (
                    value.0.len(),
                    value.1.iter().map(|(k, v)| (k, *v)).collect(),
                ),
            )
        })
        .collect();
    assert!(held == model);
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct Note {
    text: String,
    #[ferro(skip)]
    scratch: u64,
}

fn notes(db: &Db<Map<u32, Note>>) -> Vec<(u32, Note)> {
    db.read()
        .iter()
        .map(|(key, note)| (key, (*note).clone()))
        .collect()
}

#[test]
fn a_value_written_beside_unchanged_neighbours_reads_as_the_file_holds_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.ferro");
    let db = roomy().open::<Map<u32, Note>>(&path).unwrap();
    let note = |key: u32, round: u32, scratch: u64| Note {
        text: format!("note {key} of round {round}"),
        scratch,
    };
    db.update(|root| root.extend((0..2_000u32).map(|key| (key, note(key, 0, 1)))))
        .unwrap();
    db.update(|root| {
        root.insert(5, note(5, 1, 7));
        root.insert(1_500, note(1_500, 1, 7));
    })
    .unwrap();
    let written = notes(&db);
    assert_eq!(written[5], (5, note(5, 1, 0)));
    assert_eq!(written[1_500], (1_500, note(1_500, 1, 0)));
    drop(db);
    let db = roomy().open::<Map<u32, Note>>(&path).unwrap();
    assert_eq!(notes(&db), written);
}
