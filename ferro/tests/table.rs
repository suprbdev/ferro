mod common;

use ferro::Val;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::ops::Bound;
use std::panic::{self, AssertUnwindSafe};

use common::{Rng, fast};
use ferro::table::IndexIter;
use ferro::{Data, Db, Map, Record, Table};
use proptest::prelude::*;

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(n, opt)]
struct Row {
    #[key]
    id: u32,
    #[index]
    n: u8,
    #[index]
    opt: Option<String>,
    #[index(each)]
    tags: Vec<String>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    rows: Table<Row>,
    audit: Map<u32, u8>,
}

type Fields = (u8, Option<String>, Vec<String>);

const IDS: u32 = 10;

fn tag_pool() -> impl Strategy<Value = String> {
    prop_oneof![Just("x"), Just("y"), Just("z")].prop_map(str::to_string)
}

fn row_strategy() -> impl Strategy<Value = Row> {
    (
        0..IDS,
        0..4u8,
        prop_oneof![
            Just(None),
            Just(Some("a".to_string())),
            Just(Some("b".to_string())),
            Just(Some(String::new()))
        ],
        prop::collection::vec(tag_pool(), 0..5),
    )
        .prop_map(|(id, n, opt, tags)| Row { id, n, opt, tags })
}

fn fields() -> impl Strategy<Value = Fields> {
    (
        0..4u8,
        prop_oneof![
            Just(None),
            Just(Some("a".to_string())),
            Just(Some("c".to_string()))
        ],
        prop::collection::vec(tag_pool(), 0..5),
    )
}

#[derive(Clone, Debug)]
enum Op {
    Insert(Row),
    Remove(u32),
    Update(u32, Fields),
    GetMut(u32, Fields),
    ChangeKey(u32, u32, Fields),
    Untouched(u32),
    Rewrite(u32),
    Retain(u8),
    TakeRange(Bound<u32>, Bound<u32>),
    RemoveRange(Bound<u32>, Bound<u32>),
    PanicUpdate(u32, Fields),
    Forget(u32, Fields),
    Commit,
    Reopen,
}

fn id_bound() -> impl Strategy<Value = Bound<u32>> {
    prop_oneof![
        Just(Bound::Unbounded),
        (0..IDS).prop_map(Bound::Included),
        (0..IDS).prop_map(Bound::Excluded),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        10 => row_strategy().prop_map(Op::Insert),
        5 => (0..IDS).prop_map(Op::Remove),
        4 => (0..IDS, fields()).prop_map(|(k, f)| Op::Update(k, f)),
        4 => (0..IDS, fields()).prop_map(|(k, f)| Op::GetMut(k, f)),
        4 => (0..IDS, 0..IDS, fields()).prop_map(|(k, n, f)| Op::ChangeKey(k, n, f)),
        2 => (0..IDS).prop_map(Op::Untouched),
        2 => (0..IDS).prop_map(Op::Rewrite),
        1 => (2..4u8).prop_map(Op::Retain),
        2 => (id_bound(), id_bound()).prop_map(|(a, b)| Op::TakeRange(a, b)),
        1 => (id_bound(), id_bound()).prop_map(|(a, b)| Op::RemoveRange(a, b)),
        2 => (0..IDS, fields()).prop_map(|(k, f)| Op::PanicUpdate(k, f)),
        2 => (0..IDS, fields()).prop_map(|(k, f)| Op::Forget(k, f)),
        3 => Just(Op::Commit),
        1 => Just(Op::Reopen),
    ]
}

fn set_fields(row: &mut Row, fields: &Fields) {
    row.n = fields.0;
    row.opt = fields.1.clone();
    row.tags = fields.2.clone();
}

fn bounds<V: Clone>(values: &[V]) -> Vec<(Bound<V>, Bound<V>)> {
    let mut out = vec![(Bound::Unbounded, Bound::Unbounded)];
    for a in values {
        out.push((Bound::Included(a.clone()), Bound::Unbounded));
        out.push((Bound::Excluded(a.clone()), Bound::Unbounded));
        out.push((Bound::Unbounded, Bound::Included(a.clone())));
        out.push((Bound::Unbounded, Bound::Excluded(a.clone())));
        for b in values {
            for lo in [Bound::Included(a.clone()), Bound::Excluded(a.clone())] {
                for hi in [Bound::Included(b.clone()), Bound::Excluded(b.clone())] {
                    out.push((lo.clone(), hi));
                }
            }
        }
    }
    out
}

fn within<V: Ord>(value: &V, low: &Bound<V>, high: &Bound<V>) -> bool {
    (match low {
        Bound::Included(b) => value >= b,
        Bound::Excluded(b) => value > b,
        Bound::Unbounded => true,
    }) && (match high {
        Bound::Included(b) => value <= b,
        Bound::Excluded(b) => value < b,
        Bound::Unbounded => true,
    })
}

fn ids(iter: impl Iterator<Item = Val<Row>>) -> Vec<(u32, Val<Row>)> {
    iter.map(|row| (row.id, row)).collect()
}

#[allow(clippy::too_many_arguments)]
fn check_index<V: Ord + Clone + Debug>(
    model: &BTreeMap<u32, Row>,
    values: &[V],
    extract: impl Fn(&Row) -> Vec<V>,
    raw: Vec<(V, u32)>,
    eq: impl Fn(&V) -> IndexIter<Row>,
    range: impl Fn((Bound<V>, Bound<V>)) -> IndexIter<Row>,
    counts: Vec<(V, usize)>,
) {
    let mut entries: BTreeSet<(V, u32)> = BTreeSet::new();
    for row in model.values() {
        for value in extract(row) {
            entries.insert((value, row.id));
        }
    }
    let expected_raw: Vec<(V, u32)> = entries.iter().cloned().collect();
    assert_eq!(raw, expected_raw, "index holds stale or missing entries");
    let mut expected_counts: Vec<(V, usize)> = Vec::new();
    for (value, _) in &entries {
        match expected_counts.last_mut() {
            Some((last, count)) if last == value => *count += 1,
            _ => expected_counts.push((value.clone(), 1)),
        }
    }
    assert_eq!(counts, expected_counts);
    for value in values {
        let wanted: Vec<(u32, Val<Row>)> = entries
            .iter()
            .filter(|(v, _)| v == value)
            .map(|(_, id)| (*id, Val::new(model[id].clone())))
            .collect();
        let iter = eq(value);
        assert_eq!(iter.len(), wanted.len());
        assert_eq!(ids(iter), wanted, "eq {value:?}");
        let mut reversed = wanted.clone();
        reversed.reverse();
        assert_eq!(ids(eq(value).rev()), reversed);
    }
    for (low, high) in bounds(values) {
        let wanted: Vec<(u32, Val<Row>)> = entries
            .iter()
            .filter(|(v, _)| within(v, &low, &high))
            .map(|(_, id)| (*id, Val::new(model[id].clone())))
            .collect();
        let iter = range((low.clone(), high.clone()));
        assert_eq!(iter.len(), wanted.len(), "{low:?} {high:?}");
        assert_eq!(ids(iter), wanted, "{low:?} {high:?}");
        let mut reversed = wanted.clone();
        reversed.reverse();
        assert_eq!(ids(range((low, high)).rev()), reversed);
    }
}

fn check(table: &Table<Row>, model: &BTreeMap<u32, Row>) {
    assert_eq!(table.len(), model.len());
    let listed: Vec<Row> = table.iter().map(|row| (*row).clone()).collect();
    let wanted: Vec<Row> = model.values().cloned().collect();
    assert_eq!(listed, wanted);
    check_index(
        model,
        &[0u8, 1, 2, 3, 4],
        |row| vec![row.n],
        table.by_n.keys().collect(),
        |v| table.by_n(v),
        |r| table.by_n_range::<u8, _>(r),
        table.by_n_counts(),
    );
    let opts: Vec<Option<String>> = vec![
        None,
        Some(String::new()),
        Some("a".into()),
        Some("b".into()),
        Some("c".into()),
        Some("zz".into()),
    ];
    check_index(
        model,
        &opts,
        |row| vec![row.opt.clone()],
        table.by_opt.keys().collect(),
        |v| table.by_opt(v),
        |r| table.by_opt_range::<Option<String>, _>(r),
        table.by_opt_counts(),
    );
    let pairs: Vec<(u8, Option<String>)> = vec![
        (0, None),
        (1, Some("a".into())),
        (2, Some("b".into())),
        (3, Some("c".into())),
    ];
    check_index(
        model,
        &pairs,
        |row| vec![(row.n, row.opt.clone())],
        table.by_n_opt.keys().collect(),
        |v| table.by_n_opt(v),
        |r| table.by_n_opt_range::<(u8, Option<String>), _>(r),
        table.by_n_opt_counts(),
    );
    for n in 0..5u8 {
        let mut wanted: Vec<&Row> = model.values().filter(|row| row.n == n).collect();
        wanted.sort_by_key(|row| (&row.opt, row.id));
        let wanted: Vec<u32> = wanted.iter().map(|row| row.id).collect();
        let got: Vec<u32> = table.by_n_opt_prefix(&n).map(|row| row.id).collect();
        assert_eq!(got, wanted);
        assert_eq!(table.by_n_opt_prefix(&n).len(), wanted.len());
        let back: Vec<u32> = table.by_n_opt_prefix(&n).rev().map(|row| row.id).collect();
        assert_eq!(back, wanted.iter().rev().cloned().collect::<Vec<_>>());
    }
    for probe in ["", "a", "b", "c", "zz"] {
        let wanted: Vec<u32> = model
            .values()
            .filter(|row| row.opt.as_deref() == Some(probe))
            .map(|row| row.id)
            .collect();
        let got: Vec<u32> = table.by_opt_some(probe).map(|row| row.id).collect();
        assert_eq!(got, wanted);
        assert_eq!(table.by_opt_some(probe).len(), wanted.len());
        let back: Vec<u32> = table.by_opt_some(probe).rev().map(|row| row.id).collect();
        assert_eq!(back, wanted.iter().rev().cloned().collect::<Vec<_>>());
    }
    let tags: Vec<String> = ["", "w", "x", "y", "z", "zz"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    check_index(
        model,
        &tags,
        |row| row.tags.clone(),
        table.by_tags.keys().collect(),
        |v| table.by_tags(v),
        |r| table.by_tags_range::<String, _>(r),
        table.by_tags_counts(),
    );
}

fn rebuilt(model: &BTreeMap<u32, Row>, seed: u64) -> Table<Row> {
    let mut rows: Vec<Row> = model.values().cloned().collect();
    Rng::new(seed).shuffle(&mut rows);
    let mut table: Table<Row> = Table::new();
    for row in rows {
        let mut noise = row.clone();
        noise.n = noise.n.wrapping_add(1);
        noise.tags.push("noise".into());
        table.insert(noise);
        table.insert(row);
    }
    table
}

fn same_form(a: &Table<Row>, b: &Table<Row>) {
    assert!(a == b);
    assert_eq!(a.rows.root_hash(), b.rows.root_hash());
    assert_eq!(a.by_n.root_hash(), b.by_n.root_hash());
    assert_eq!(a.by_opt.root_hash(), b.by_opt.root_hash());
    assert_eq!(a.by_tags.root_hash(), b.by_tags.root_hash());
    assert_eq!(a.by_n_opt.root_hash(), b.by_n_opt.root_hash());
}

fn run(ops: Vec<Op>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.ferro");
    let mut db: Db<Root> = fast().open(&path).unwrap();
    let mut table: Table<Row> = Table::new();
    let mut model: BTreeMap<u32, Row> = BTreeMap::new();
    let mut committed = model.clone();
    for (step, op) in ops.into_iter().enumerate() {
        match op {
            Op::Insert(row) => {
                let old = table.insert(row.clone());
                let expected = model.insert(row.id, row);
                assert_eq!(old.map(|row| (*row).clone()), expected);
            }
            Op::Remove(id) => {
                let old = table.remove(&id);
                assert_eq!(old.map(|row| (*row).clone()), model.remove(&id));
            }
            Op::Update(id, f) => {
                let hit = table.update(&id, |row| set_fields(row, &f));
                assert_eq!(hit.is_some(), model.contains_key(&id));
                if let Some(row) = model.get_mut(&id) {
                    set_fields(row, &f);
                }
            }
            Op::GetMut(id, f) => {
                if let Some(mut row) = table.get_mut(&id) {
                    set_fields(&mut row, &f);
                }
                if let Some(row) = model.get_mut(&id) {
                    set_fields(row, &f);
                }
            }
            Op::ChangeKey(id, new, f) => {
                if let Some(mut row) = table.get_mut(&id) {
                    set_fields(&mut row, &f);
                    row.id = new;
                }
                if let Some(mut row) = model.remove(&id) {
                    set_fields(&mut row, &f);
                    row.id = new;
                    model.insert(new, row);
                }
            }
            Op::Untouched(id) => {
                let before = table.clone();
                drop(table.get_mut(&id));
                assert!(table == before);
            }
            Op::Rewrite(id) => {
                if let Some(mut row) = table.get_mut(&id) {
                    let copy = (*row).clone();
                    *row = copy;
                }
            }
            Op::Retain(modulus) => {
                table.retain(|row| row.n % modulus == 0);
                model.retain(|_, row| row.n % modulus == 0);
            }
            Op::TakeRange(low, high) => {
                let wanted: BTreeMap<u32, Row> = model
                    .iter()
                    .filter(|(id, _)| within(*id, &low, &high))
                    .map(|(id, row)| (*id, row.clone()))
                    .collect();
                let taken = table.take_range::<u32, _>((low, high));
                check(&taken, &wanted);
                same_form(&taken, &rebuilt(&wanted, step as u64));
                model.retain(|_, row| !within(&row.id, &low, &high));
            }
            Op::RemoveRange(low, high) => {
                let before = model.len();
                model.retain(|_, row| !within(&row.id, &low, &high));
                let removed = table.remove_range::<u32, _>((low, high));
                assert_eq!(removed, before - model.len());
            }
            Op::PanicUpdate(id, f) => {
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    table.update(&id, |row| {
                        set_fields(row, &f);
                        panic!("boom");
                    })
                }));
                assert_eq!(result.is_err(), model.contains_key(&id));
            }
            Op::Forget(id, f) => {
                if let Some(mut row) = table.get_mut(&id) {
                    set_fields(&mut row, &f);
                    std::mem::forget(row);
                }
            }
            Op::Commit => {
                let snapshot = table.clone();
                db.update(|root| root.rows = snapshot).unwrap();
                committed = model.clone();
                let read = db.read();
                check(&read.rows, &committed);
            }
            Op::Reopen => {
                drop(std::mem::take(&mut table));
                drop(db);
                db = fast().open(&path).unwrap();
                table = db.read().rows.clone();
                model = committed.clone();
            }
        }
        check(&table, &model);
        if step % 5 == 0 {
            same_form(&table, &rebuilt(&model, step as u64));
        }
    }
    same_form(&table, &rebuilt(&model, 99));
    let snapshot = table.clone();
    db.update(|root| root.rows = snapshot).unwrap();
    drop(table);
    drop(db);
    let db: Db<Root> = fast().open(&path).unwrap();
    check(&db.read().rows, &model);
    same_form(&db.read().rows, &rebuilt(&model, 7));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(200)))]

    #[test]
    fn table_and_indexes_match_a_model(ops in prop::collection::vec(op(), 1..60)) {
        quiet();
        run(ops);
    }
}

fn quiet() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if info.payload().downcast_ref::<&str>() != Some(&"boom") {
                default(info);
            }
        }));
    });
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Halves {
    kept: Table<Row>,
    taken: Table<Row>,
}

#[test]
fn take_range_moves_stored_rows_with_their_index_entries_into_a_table_of_their_own() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("halves.ferro");
    let db: Db<Halves> = fast().open(&path).unwrap();
    let mut kept = BTreeMap::new();
    db.update(|halves| {
        for id in 0..IDS {
            let row = Row {
                id,
                n: (id % 4) as u8,
                opt: (id % 3 == 0).then(|| "a".to_string()),
                tags: vec!["x".to_string(); (id % 3) as usize],
            };
            halves.kept.insert(row.clone());
            kept.insert(id, row);
        }
    })
    .unwrap();
    drop(db);
    let mut taken = kept.split_off(&3);
    kept.append(&mut taken.split_off(&7));
    let db: Db<Halves> = fast().open(&path).unwrap();
    db.update(|halves| {
        halves.taken = halves.kept.take_range(3..7);
        assert!(halves.kept.take_range(3..7).is_empty());
        assert_eq!(halves.kept.remove_range(IDS..), 0);
        check(&halves.kept, &kept);
        check(&halves.taken, &taken);
    })
    .unwrap();
    drop(db);
    let db: Db<Halves> = fast().open(&path).unwrap();
    let read = db.read();
    check(&read.kept, &kept);
    check(&read.taken, &taken);
    same_form(&read.kept, &rebuilt(&kept, 3));
    same_form(&read.taken, &rebuilt(&taken, 4));
    drop(read);
    db.verify_typed().unwrap();
}

#[test]
fn equal_tables_built_by_different_paths_have_equal_indexes() {
    let mut a = Table::<Row>::new();
    let mut b = Table::<Row>::new();
    let r = |id, n, tags: &[&str]| Row {
        id,
        n,
        opt: None,
        tags: tags.iter().map(|t| t.to_string()).collect(),
    };
    a.insert(r(1, 1, &["x", "x"]));
    a.insert(r(2, 2, &[]));
    b.insert(r(2, 3, &["y"]));
    b.insert(r(1, 0, &["z"]));
    b.update(&2, |row| {
        row.n = 2;
        row.tags.clear();
    });
    b.get_mut(&1).unwrap().tags = vec!["x".into(), "x".into()];
    b.get_mut(&1).unwrap().n = 1;
    same_form(&a, &b);
}

#[test]
fn panic_inside_db_update_discards_the_whole_transaction() {
    quiet();
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = fast().open(dir.path().join("p.ferro")).unwrap();
    let row = |id, n| Row {
        id,
        n,
        opt: None,
        tags: vec!["x".into()],
    };
    db.update(|root| {
        root.rows.insert(row(1, 1));
    })
    .unwrap();
    let before = db.read().rows.clone();
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = db.update(|root| {
            root.rows.insert(row(2, 2));
            root.rows.update(&1, |r| {
                r.n = 3;
                r.id = 5;
                panic!("boom");
            });
        });
    }));
    assert!(outcome.is_err());
    let after = db.read().rows.clone();
    assert!(after == before);
    same_form(&after, &before);
    assert_eq!(db.version(), 1);
    db.update(|root| {
        root.rows.insert(row(9, 1));
    })
    .unwrap();
    assert_eq!(db.read().rows.by_n(&1).len(), 2);
}

struct BumpOnDrop<'a>(&'a mut Table<Row>);

impl Drop for BumpOnDrop<'_> {
    fn drop(&mut self) {
        self.0.update(&1, |row| row.n = 7);
    }
}

#[test]
fn a_panic_while_a_row_is_changed_keeps_the_old_row() {
    quiet();
    let row = |id, n, tag: &str| Row {
        id,
        n,
        opt: Some("a".into()),
        tags: vec![tag.into()],
    };
    let mut t = Table::<Row>::new();
    t.insert(row(1, 1, "x"));
    t.insert(row(2, 2, "y"));
    let mut model = BTreeMap::new();
    model.insert(1, row(1, 1, "x"));
    model.insert(2, row(2, 2, "y"));
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        t.update(&1, |r| {
            r.n = 9;
            r.tags = vec!["z".into()];
            panic!("boom");
        });
    }));
    assert!(outcome.is_err());
    check(&t, &model);
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        t.update(&1, |r| {
            r.id = 2;
            panic!("boom");
        });
    }));
    assert!(outcome.is_err());
    check(&t, &model);
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut held = t.get_mut(&2).unwrap();
        held.opt = None;
        held.n = 5;
        panic!("boom");
    }));
    assert!(outcome.is_err());
    check(&t, &model);
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let _bump = BumpOnDrop(&mut t);
        panic!("boom");
    }));
    assert!(outcome.is_err());
    model.insert(1, row(1, 7, "x"));
    check(&t, &model);
}

#[test]
fn key_change_onto_an_existing_row_replaces_it_everywhere() {
    let mut t = Table::<Row>::new();
    let r = |id, n, tag: &str| Row {
        id,
        n,
        opt: Some("a".into()),
        tags: vec![tag.into()],
    };
    t.insert(r(1, 1, "x"));
    t.insert(r(2, 2, "y"));
    t.get_mut(&1).unwrap().id = 2;
    let mut model = BTreeMap::new();
    model.insert(2, r(2, 1, "x"));
    check(&t, &model);
    assert_eq!(t.by_tags("y").len(), 0);
    assert_eq!(t.by_n(&2).len(), 0);
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Before {
    #[key]
    id: u32,
    n: u8,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct After {
    #[key]
    id: u32,
    #[index]
    n: u8,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct OldRoot {
    rows: Table<Before>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct NewRoot {
    rows: Table<After>,
}

#[test]
fn a_table_written_before_an_index_existed_gets_the_index_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.ferro");
    {
        let db: Db<OldRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..40u32 {
                root.rows.insert(Before {
                    id,
                    n: (id % 4) as u8,
                });
            }
        })
        .unwrap();
    }
    let db: Db<NewRoot> = fast().open(&path).unwrap();
    let table = db.read().rows.clone();
    assert_eq!(table.by_n(&1).len(), 10);
    assert_eq!(
        table.by_n_counts(),
        vec![(0, 10), (1, 10), (2, 10), (3, 10)]
    );
    assert_eq!(table.by_n_range::<u8, _>(1..3).len(), 20);
    drop(table);
    db.update(|root| {
        root.rows.update(&1, |row| row.n = 9);
        root.rows.remove(&5);
    })
    .unwrap();
    let table = db.read().rows.clone();
    assert_eq!(table.by_n(&1).len(), 8);
    assert_eq!(table.by_n(&9).len(), 1);
    assert_eq!(db.at(1).unwrap().rows.by_n(&1).len(), 10);
    drop(table);
    drop(db);
    let db: Db<NewRoot> = fast().open(&path).unwrap();
    assert_eq!(db.read().rows.by_n(&9).len(), 1);
    assert_eq!(
        db.read()
            .rows
            .by_n_counts()
            .iter()
            .map(|(_, c)| c)
            .sum::<usize>(),
        39
    );
}

mod failed_loads {
    use std::cell::Cell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::panic::{self, AssertUnwindSafe};

    use std::cmp::Ordering;

    use ferro::codec::{Decoder, Encoder};
    use ferro::{Data, Map, Options, Record, Set, Table, hash_of};

    use crate::common::{
        Brittle, Fragile, Rng, cases, count_decodes, decodes, defuse, fuse, interrupted,
        quiet_ferro_panics, roomy, text, uncached,
    };

    #[derive(Clone, Debug)]
    struct Loose(u64);

    impl PartialEq for Loose {
        fn eq(&self, other: &Self) -> bool {
            self.0 / 2 == other.0 / 2
        }
    }

    impl Eq for Loose {}

    impl PartialOrd for Loose {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for Loose {
        fn cmp(&self, other: &Self) -> Ordering {
            (self.0 / 2).cmp(&(other.0 / 2))
        }
    }

    impl Data for Loose {
        fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
            self.0.encode(w)
        }

        fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
            let Brittle(value) = Brittle::decode(r)?;
            Ok(Loose(value))
        }
    }

    #[derive(Data, Record, Clone, Debug, PartialEq)]
    #[index(group, rank)]
    struct Frail {
        #[key]
        id: u64,
        #[index(cover(note))]
        group: Brittle,
        #[unique(cover(rank))]
        slug: Option<Brittle>,
        #[index(each)]
        tags: Vec<Brittle>,
        #[index(each, cover(rank))]
        marks: Set<Brittle>,
        rank: u64,
        note: String,
        #[index(cover(label))]
        label: Loose,
    }

    #[derive(Clone, Debug, PartialEq)]
    struct Plain {
        group: u64,
        slug: Option<u64>,
        tags: Vec<u64>,
        marks: BTreeSet<u64>,
        rank: u64,
        note: String,
        label: u64,
    }

    type Frails = Table<Frail>;
    type Model = BTreeMap<u64, Plain>;

    const IDS: u64 = 400;

    fn row(id: u64, plain: &Plain) -> Frail {
        Frail {
            id,
            group: Brittle(plain.group),
            slug: plain.slug.map(Brittle),
            tags: plain.tags.iter().copied().map(Brittle).collect(),
            marks: plain.marks.iter().copied().map(Brittle).collect(),
            rank: plain.rank,
            note: plain.note.clone(),
            label: Loose(plain.label),
        }
    }

    fn plain(row: &Frail) -> Plain {
        Plain {
            group: row.group.0,
            slug: row.slug.as_ref().map(|slug| slug.0),
            tags: row.tags.iter().map(|tag| tag.0).collect(),
            marks: row.marks.iter().map(|mark| mark.0).collect(),
            rank: row.rank,
            note: row.note.clone(),
            label: row.label.0,
        }
    }

    fn made(rng: &mut Rng) -> Plain {
        let len = if rng.chance(10) { 600 } else { rng.below(12) };
        Plain {
            group: rng.below(20),
            slug: rng.chance(70).then(|| rng.below(300)),
            tags: (0..rng.below(4)).map(|_| rng.below(40)).collect(),
            marks: (0..rng.below(5)).map(|_| rng.below(30)).collect(),
            rank: rng.below(5),
            note: text(rng.next(), len as usize),
            label: rng.below(8),
        }
    }

    fn changed(rng: &mut Rng, plain: &mut Plain) {
        let fresh = made(rng);
        match rng.below(7) {
            6 => plain.label ^= 1,
            0 => plain.group = fresh.group,
            1 => plain.slug = fresh.slug,
            2 => plain.tags = fresh.tags,
            3 => plain.marks = fresh.marks,
            4 => plain.rank = fresh.rank,
            _ => plain.note = fresh.note,
        }
    }

    fn fresh(model: &Model) -> Frails {
        let mut table = Frails::new();
        for (id, plain) in model {
            table.insert(row(*id, plain));
        }
        table
    }

    fn read_back(table: &Frails) -> Model {
        table.iter().map(|row| (row.id, plain(&row))).collect()
    }

    fn same(table: &Frails, model: &Model, what: &str) {
        assert_eq!(table.len(), model.len(), "{what}: rows");
        assert!(read_back(table) == *model, "{what}: rows differ");
        assert_eq!(
            hash_of(table),
            hash_of(&fresh(model)),
            "{what}: the indexes are not those of the rows"
        );
    }

    fn clashes(model: &Model, id: u64, slug: Option<u64>, moving: Option<u64>) -> bool {
        slug.is_some()
            && model
                .iter()
                .any(|(held, plain)| plain.slug == slug && *held != id && Some(*held) != moving)
    }

    fn moved(model: &mut Model, old: u64, id: u64, plain: Plain) {
        model.remove(&old);
        model.insert(id, plain);
    }

    fn held(model: &Model, rng: &mut Rng) -> u64 {
        match model.keys().nth(rng.below(model.len() as u64 + 1) as usize) {
            Some(id) if rng.chance(85) => *id,
            _ => rng.below(IDS),
        }
    }

    fn step(table: &mut Frails, model: &mut Model, base: &Frails, rng: &mut Rng) -> &'static str {
        let before = model.clone();
        let (name, done) = match rng.below(15) {
            0..=2 => {
                let (id, plain) = (held(model, rng), made(rng));
                let done = interrupted(rng, || table.try_insert(row(id, &plain)).is_ok());
                if done == Some(true) {
                    model.insert(id, plain);
                }
                ("insert", done.is_some())
            }
            3 | 4 => {
                let id = held(model, rng);
                let done = interrupted(rng, || table.remove(&id)).is_some();
                if done {
                    model.remove(&id);
                }
                ("remove", done)
            }
            5..=7 => {
                let old = held(model, rng);
                let Some(mut plain) = model.get(&old).cloned() else {
                    return "update of an absent row";
                };
                changed(rng, &mut plain);
                let id = if rng.chance(20) { rng.below(IDS) } else { old };
                let how = rng.below(3);
                let clash = clashes(model, id, plain.slug, Some(old));
                let next = row(id, &plain);
                let done = interrupted(rng, || match how {
                    0 => table.try_update(&old, |row| *row = next.clone()).is_ok(),
                    1 => {
                        table.update(&old, |row| *row = next.clone());
                        true
                    }
                    _ => {
                        let mut held = table.get_mut(&old).expect("the row is held");
                        *held = next.clone();
                        drop(held);
                        true
                    }
                });
                if done == Some(true) {
                    assert!(!clash, "a clashing update went in");
                    moved(model, old, id, plain);
                }
                ("update", done == Some(true))
            }
            8 => {
                let (group, rank) = (rng.below(20), rng.below(5));
                let doomed = move |plain: &Plain| plain.group == group || plain.rank == rank;
                let done = interrupted(rng, || {
                    table.retain(|row| !doomed(&plain(row)));
                })
                .is_some();
                if done {
                    model.retain(|_, plain| !doomed(plain));
                }
                ("retain", done)
            }
            9 => {
                let low = rng.below(IDS);
                let high = low + rng.below(40);
                let wanted: Model = model
                    .range(low..high)
                    .map(|(id, plain)| (*id, plain.clone()))
                    .collect();
                if rng.chance(50) {
                    let taken = interrupted(rng, || table.take_range(low..high));
                    let done = taken.is_some();
                    if let Some(taken) = taken {
                        same(&taken, &wanted, "the table take_range returned");
                        model.retain(|id, _| !(low..high).contains(id));
                    }
                    ("take_range", done)
                } else {
                    let removed = interrupted(rng, || table.remove_range(low..high));
                    let done = removed.is_some();
                    if let Some(removed) = removed {
                        assert_eq!(removed, wanted.len(), "remove_range");
                        model.retain(|id, _| !(low..high).contains(id));
                    }
                    ("remove_range", done)
                }
            }
            10 | 11 => {
                let mut theirs = base.clone();
                for _ in 0..rng.below(30) {
                    let id = rng.below(IDS);
                    if rng.chance(30) {
                        theirs.remove(&id);
                    } else {
                        let _ = theirs.try_insert(row(id, &made(rng)));
                    }
                }
                let refuse = rng.chance(30);
                let done = interrupted(rng, || {
                    table
                        .try_merge(base, &theirs, |conflict| {
                            if refuse {
                                Err(conflict.key)
                            } else {
                                Ok(conflict.keep_theirs())
                            }
                        })
                        .is_ok()
                });
                if done == Some(true) {
                    *model = read_back(table);
                }
                ("merge", done == Some(true))
            }
            12 => {
                let done = interrupted(rng, || table.rewrite()).is_some();
                ("rewrite", done)
            }
            _ => {
                let (id, plain) = (rng.below(IDS), made(rng));
                let clash = clashes(model, id, plain.slug, None);
                let done = interrupted(rng, || {
                    table.insert(row(id, &plain));
                })
                .is_some();
                if done {
                    assert!(!clash, "a clashing insert went in");
                    model.insert(id, plain);
                }
                ("insert or clash", done)
            }
        };
        if done {
            same(table, model, &format!("{name} done"));
        } else {
            same(
                table,
                &before,
                &format!("a failed {name} changed the table"),
            );
        }
        name
    }

    fn run(seed: u64, options: fn() -> Options) {
        quiet_ferro_panics();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frail.ferro");
        let open = || options().open::<Frails>(&path).unwrap();
        let mut db = open();
        let mut rng = Rng::new(seed);
        let mut model = Model::new();
        db.update(|table| {
            for _ in 0..300 {
                let (id, plain) = (rng.below(IDS), made(&mut rng));
                if table.try_insert(row(id, &plain)).is_ok() {
                    model.insert(id, plain);
                }
            }
        })
        .unwrap();
        for commit in 0..8 {
            let mut working = model.clone();
            let mut trail = Vec::new();
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                db.update(|table| {
                    let base = table.clone();
                    for _ in 0..25 {
                        trail.push(step(table, &mut working, &base, &mut rng));
                    }
                })
                .unwrap();
            }));
            if let Err(payload) = outcome {
                eprintln!("seed {seed} commit {commit} after {trail:?}");
                panic::resume_unwind(payload);
            }
            model = working;
            same(&db.read(), &model, "the commit");
            if commit % 3 == 2 {
                db.verify_typed().unwrap();
                drop(db);
                db = open();
                same(&db.read(), &model, "the reopened file");
            }
        }
        db.verify_typed().unwrap();
    }

    #[test]
    fn every_table_write_keeps_rows_and_indexes_together_through_failed_loads_uncached() {
        for seed in 0..u64::from(cases(3)) {
            run(seed, uncached);
        }
    }

    #[test]
    fn every_table_write_keeps_rows_and_indexes_together_through_failed_loads_roomy() {
        for seed in 50..50 + u64::from(cases(3)) {
            run(seed, roomy);
        }
    }

    fn sweep(table: &Frails, before: &Model, after: &Model, what: &str, op: impl Fn(&mut Frails)) {
        let hash = hash_of(table);
        let mut at = 0;
        loop {
            let mut trial = table.clone();
            fuse(at, at % 2 == 1);
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| op(&mut trial)));
            defuse();
            if outcome.is_ok() {
                same(&trial, after, &format!("{what} done"));
                return;
            }
            assert_eq!(hash_of(&trial), hash, "{what} failed at decode {at}");
            if at % 16 == 0 {
                same(&trial, before, &format!("{what} failed at decode {at}"));
            }
            at += 1;
        }
    }

    #[test]
    fn every_load_point_of_every_table_write_leaves_the_table_as_it_was() {
        quiet_ferro_panics();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sweep.ferro");
        let mut rng = Rng::new(7);
        let mut model = Model::new();
        let db = uncached().open::<Frails>(&path).unwrap();
        db.update(|table| {
            for id in 0..IDS {
                let plain = Plain {
                    slug: Some(id),
                    ..made(&mut rng)
                };
                table.insert(row(id, &plain));
                model.insert(id, plain);
            }
        })
        .unwrap();
        drop(db);
        let db = uncached().open::<Frails>(&path).unwrap();
        db.update(|table| {
            for round in 0..2 {
                let id = IDS + round;
                let plain = Plain {
                    slug: Some(IDS + round),
                    ..made(&mut rng)
                };
                let mut after = model.clone();
                after.insert(id, plain.clone());
                sweep(table, &model, &after, "insert", |table| {
                    table.insert(row(id, &plain));
                });

                let id = 17 + round * 101;
                let mut plain = Plain {
                    slug: Some(IDS + 10 + round),
                    ..made(&mut rng)
                };
                plain.marks.insert(29);
                let mut after = model.clone();
                after.insert(id, plain.clone());
                sweep(table, &model, &after, "replace", |table| {
                    table.insert(row(id, &plain));
                });

                let id = 33 + round * 97;
                let mut after = model.clone();
                after.remove(&id);
                sweep(table, &model, &after, "remove", |table| {
                    table.remove(&id);
                });

                let id = 51 + round * 89;
                let mut plain = model[&id].clone();
                changed(&mut rng, &mut plain);
                plain.group = (plain.group + 1) % 20;
                let mut after = model.clone();
                after.insert(id, plain.clone());
                sweep(table, &model, &after, "update", |table| {
                    table.update(&id, |held| *held = row(id, &plain));
                });

                let (old, id) = (77 + round * 83, 140 + round * 61);
                let plain = model[&old].clone();
                let mut after = model.clone();
                moved(&mut after, old, id, plain.clone());
                sweep(table, &model, &after, "key change", |table| {
                    table.update(&old, |held| *held = row(id, &plain));
                });

                let id = 23 + round * 71;
                let mut plain = model[&id].clone();
                plain.label ^= 1;
                let mut after = model.clone();
                after.insert(id, plain.clone());
                sweep(table, &model, &after, "relabel", |table| {
                    table.update(&id, |held| *held = row(id, &plain));
                });

                let (old, id) = (91 + round * 67, 160 + round * 53);
                let mut plain = model[&old].clone();
                plain.label = model[&id].label ^ 1;
                let mut after = model.clone();
                moved(&mut after, old, id, plain.clone());
                sweep(table, &model, &after, "key change and relabel", |table| {
                    table.update(&old, |held| *held = row(id, &plain));
                });

                let rank = round;
                let mut after = model.clone();
                after.retain(|_, plain| plain.rank != rank);
                sweep(table, &model, &after, "retain", |table| {
                    table.retain(|row| row.rank != rank);
                });

                let low = 200 + round * 50;
                let mut after = model.clone();
                after.retain(|id, _| !(low..low + 30).contains(id));
                sweep(table, &model, &after, "take_range", |table| {
                    let _ = table.take_range(low..low + 30);
                });
                sweep(table, &model, &after, "remove_range", |table| {
                    table.remove_range(low..low + 30);
                });
            }
        })
        .unwrap();
    }

    #[derive(Data, Record, Clone, Debug, PartialEq)]
    struct Stamped {
        #[key]
        id: u64,
        #[index(cover(label, body))]
        label: Loose,
        body: Fragile,
    }

    fn rebuilt<T: Record + Clone>(table: &Table<T>) -> Table<T> {
        let mut fresh: Table<T> = Table::new();
        for row in table.iter() {
            fresh.insert((*row).clone());
        }
        fresh
    }

    #[test]
    fn a_relabel_whose_covered_value_is_stored_apart_is_whole_through_failed_loads() {
        quiet_ferro_panics();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stamped.ferro");
        let db = uncached().open::<Table<Stamped>>(&path).unwrap();
        db.update(|table| {
            for id in 0..40 {
                table.insert(Stamped {
                    id,
                    label: Loose(id * 2),
                    body: Fragile(id, text(id, 2000)),
                });
            }
        })
        .unwrap();
        drop(db);
        let db = uncached().open::<Table<Stamped>>(&path).unwrap();
        db.update(|table| {
            let hash = hash_of(table);
            let mut at = 0;
            loop {
                let mut trial = table.clone();
                fuse(at, at % 2 == 1);
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                    trial.update(&17, |row| row.label = Loose(35));
                }));
                defuse();
                if outcome.is_ok() {
                    assert_eq!(trial.get(&17).unwrap().label.0, 35);
                    assert_eq!(hash_of(&trial), hash_of(&rebuilt(&trial)));
                    break;
                }
                assert_eq!(hash_of(&trial), hash, "relabel failed at decode {at}");
                at += 1;
            }
            assert!(at > 4, "the relabel loaded too little to test");
        })
        .unwrap();
    }

    #[derive(Data, Record, Clone, Debug, PartialEq)]
    struct Big {
        #[key]
        id: u64,
        #[index]
        group: u64,
        body: Fragile,
    }

    fn big(id: u64) -> Big {
        Big {
            id,
            group: id % 7,
            body: Fragile(id, text(id, 4000)),
        }
    }

    #[test]
    fn writes_read_a_row_stored_apart_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.ferro");
        let db = uncached().open::<Table<Big>>(&path).unwrap();
        db.update(|table| {
            for id in 0..200 {
                table.insert(big(id));
            }
        })
        .unwrap();
        drop(db);
        let db = uncached().open::<Table<Big>>(&path).unwrap();
        db.update(|table| {
            let mut counted = |what: &str, write: &mut dyn FnMut(&mut Table<Big>)| {
                count_decodes();
                write(table);
                assert_eq!(decodes(), 1, "{what}");
            };
            counted("remove", &mut |table| {
                table.remove(&50);
            });
            counted("update", &mut |table| {
                table.update(&60, |row| row.group = 99);
            });
            counted("get_mut", &mut |table| {
                table.get_mut(&61).unwrap().group = 98;
            });
            counted("insert over", &mut |table| {
                table.insert(big(70));
            });
        })
        .unwrap();
        let map_path = dir.path().join("map.ferro");
        let maps = uncached().open::<Map<u64, Fragile>>(&map_path).unwrap();
        maps.update(|map| {
            for id in 0..200 {
                map.insert(id, big(id).body);
            }
        })
        .unwrap();
        drop(maps);
        let maps = uncached().open::<Map<u64, Fragile>>(&map_path).unwrap();
        maps.update(|map| {
            count_decodes();
            map.pop_first();
            assert_eq!(decodes(), 1, "pop_first");
            count_decodes();
            map.pop_last();
            assert_eq!(decodes(), 1, "pop_last");
        })
        .unwrap();
    }

    #[derive(Data, Record, Clone, Debug, PartialEq)]
    struct Leaf {
        #[key]
        id: u64,
        #[index]
        group: Brittle,
        #[unique]
        slug: Option<Brittle>,
        #[index(each)]
        tags: Vec<Brittle>,
        note: String,
    }

    #[derive(Data, Record, Clone, Debug, PartialEq)]
    struct Shelf {
        #[key]
        id: Brittle,
        #[index]
        kind: Brittle,
        #[unique]
        name: Option<Brittle>,
        #[index(each)]
        labels: Set<Brittle>,
        leaves: Table<Leaf>,
        note: String,
    }

    fn leaf(rng: &mut Rng, id: u64) -> Leaf {
        Leaf {
            id,
            group: Brittle(rng.below(5)),
            slug: rng.chance(50).then(|| Brittle(id + 1000)),
            tags: (0..rng.below(3)).map(|_| Brittle(rng.below(6))).collect(),
            note: text(rng.next(), if rng.chance(10) { 700 } else { 5 }),
        }
    }

    fn shelf(rng: &mut Rng, id: u64) -> Shelf {
        let mut leaves: Table<Leaf> = Table::new();
        for at in 0..rng.below(30) {
            leaves.insert(leaf(rng, at));
        }
        let mut labels = Set::new();
        for _ in 0..rng.below(4) {
            labels.insert(Brittle(rng.below(8)));
        }
        Shelf {
            id: Brittle(id),
            kind: Brittle(rng.below(4)),
            name: rng.chance(60).then(|| Brittle(id + 500)),
            labels,
            leaves,
            note: text(rng.next(), if rng.chance(20) { 900 } else { 8 }),
        }
    }

    fn whole(table: &Table<Shelf>, what: &str) {
        assert_eq!(hash_of(table), hash_of(&rebuilt(table)), "{what}: outer");
        for shelf in table.iter() {
            assert_eq!(
                hash_of(&shelf.leaves),
                hash_of(&rebuilt(&shelf.leaves)),
                "{what}: the leaves of {:?}",
                shelf.id
            );
        }
    }

    fn swept(table: &Table<Shelf>, what: &str, op: impl Fn(&mut Table<Shelf>)) -> Table<Shelf> {
        let hash = hash_of(table);
        let mut at = 0;
        loop {
            let mut trial = table.clone();
            fuse(at, at % 2 == 1);
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| op(&mut trial)));
            defuse();
            if outcome.is_ok() {
                whole(&trial, &format!("{what} done"));
                return trial;
            }
            assert_eq!(hash_of(&trial), hash, "{what} failed at decode {at}");
            if at % 8 == 0 {
                whole(&trial, &format!("{what} failed at decode {at}"));
            }
            at += 1;
        }
    }

    #[test]
    fn nested_tables_and_brittle_keys_survive_every_load_point() {
        quiet_ferro_panics();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shelves.ferro");
        let mut rng = Rng::new(11);
        let db = uncached().open::<Table<Shelf>>(&path).unwrap();
        db.update(|table| {
            for id in 0..60 {
                table.insert(shelf(&mut rng, id));
            }
        })
        .unwrap();
        drop(db);
        let db = uncached().open::<Table<Shelf>>(&path).unwrap();
        db.update(|table| {
            let replacement = shelf(&mut Rng::new(12), 7);
            *table = swept(table, "insert over", |table| {
                table.insert(replacement.clone());
            });
            *table = swept(table, "insert", |table| {
                table.insert(shelf(&mut Rng::new(5), 900));
            });
            *table = swept(table, "remove", |table| {
                table.remove(&Brittle(13));
            });
            *table = swept(table, "inner write", |table| {
                table.update(&Brittle(21), |shelf| {
                    shelf.leaves.insert(leaf(&mut Rng::new(3), 2));
                    shelf.leaves.remove(&4);
                    shelf.leaves.update(&6, |leaf| leaf.group = Brittle(9));
                });
            });
            *table = swept(table, "relabel", |table| {
                table.update(&Brittle(22), |shelf| {
                    shelf.labels.insert(Brittle(7));
                    shelf.labels.remove(&Brittle(0));
                    shelf.name = None;
                    shelf.kind = Brittle(3);
                });
            });
            *table = swept(table, "key change", |table| {
                table.update(&Brittle(30), |shelf| {
                    shelf.id = Brittle(31);
                    shelf.name = Some(Brittle(531));
                });
            });
            *table = swept(table, "retain", |table| {
                table.retain(|shelf| shelf.kind.0 != 2)
            });
            *table = swept(table, "take_range", |table| {
                let _ = table.take_range(Brittle(40)..Brittle(46));
            });
            *table = swept(table, "remove_range", |table| {
                table.remove_range(Brittle(46)..Brittle(50));
            });
            *table = swept(table, "rewrite", |table| table.rewrite());
        })
        .unwrap();
        db.verify_typed().unwrap();
    }

    #[derive(Data, Record, Clone, Debug, PartialEq)]
    #[index(group, rank)]
    struct Slim {
        #[key]
        id: u64,
        #[index(cover(note))]
        group: Brittle,
        #[unique(cover(rank))]
        slug: Option<Brittle>,
        #[index]
        label: Brittle,
        rank: u64,
        note: String,
    }

    fn slim(rng: &mut Rng, id: u64) -> Slim {
        Slim {
            id,
            group: Brittle(rng.below(20)),
            slug: rng.chance(70).then(|| Brittle(id + 1000)),
            label: Brittle(rng.below(8)),
            rank: rng.below(5),
            note: text(rng.next(), if rng.chance(10) { 600 } else { 8 }),
        }
    }

    fn slims(table: &Table<Slim>) -> BTreeMap<u64, Slim> {
        table.iter().map(|row| (row.id, (*row).clone())).collect()
    }

    fn agrees(table: &Table<Slim>, model: &BTreeMap<u64, Slim>, what: &str) {
        assert!(slims(table) == *model, "{what}: rows differ");
        assert_eq!(
            hash_of(table),
            hash_of(&rebuilt(table)),
            "{what}: the indexes are not those of the rows"
        );
    }

    fn every_point<T: Record + Clone>(
        table: &Table<T>,
        what: &str,
        op: impl Fn(&mut Table<T>),
    ) -> (Table<T>, u32) {
        let hash = hash_of(table);
        let mut at = 0;
        loop {
            let mut trial = table.clone();
            fuse(at, at % 2 == 1);
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| op(&mut trial)));
            defuse();
            if outcome.is_ok() {
                return (trial, at);
            }
            assert_eq!(hash_of(&trial), hash, "{what} failed at decode {at}");
            at += 1;
        }
    }

    #[test]
    fn a_new_row_goes_into_every_index_or_none_at_every_load_point() {
        quiet_ferro_panics();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slim.ferro");
        let mut rng = Rng::new(17);
        let db = uncached().open::<Table<Slim>>(&path).unwrap();
        db.update(|table| {
            for id in 0..300 {
                table.insert(slim(&mut rng, id * 2));
            }
        })
        .unwrap();
        drop(db);
        let db = uncached().open::<Table<Slim>>(&path).unwrap();
        db.update(|table| {
            let mut model = slims(table);
            for (round, id) in [1, 301, 599, 701].into_iter().enumerate() {
                let mut row = slim(&mut rng, id);
                if round % 2 == 1 {
                    row.slug = None;
                }
                let (after, points) = every_point(table, "insert", |table| {
                    table.insert(row.clone());
                });
                assert!(points >= 4, "the insert of {id} loaded too little to test");
                model.insert(id, row);
                *table = after;
                agrees(table, &model, "insert");
            }
        })
        .unwrap();
        db.verify_typed().unwrap();
    }

    fn slim_run(seed: u64, options: fn() -> Options) {
        quiet_ferro_panics();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slim.ferro");
        let open = || options().open::<Table<Slim>>(&path).unwrap();
        let mut db = open();
        let mut rng = Rng::new(seed);
        let mut model = BTreeMap::new();
        db.update(|table| {
            for _ in 0..200 {
                let id = rng.below(IDS);
                let row = slim(&mut rng, id);
                model.insert(id, row.clone());
                table.insert(row);
            }
        })
        .unwrap();
        for commit in 0..6 {
            let mut working = model.clone();
            db.update(|table| {
                for _ in 0..30 {
                    let before = working.clone();
                    let id = rng.below(IDS);
                    let row = slim(&mut rng, id);
                    let (what, done) = match rng.below(4) {
                        0 | 1 => {
                            let done = interrupted(&mut rng, || {
                                table.insert(row.clone());
                            });
                            if done.is_some() {
                                working.insert(id, row);
                            }
                            ("insert", done.is_some())
                        }
                        2 => {
                            let done = interrupted(&mut rng, || {
                                table.remove(&id);
                            });
                            if done.is_some() {
                                working.remove(&id);
                            }
                            ("remove", done.is_some())
                        }
                        _ => {
                            let label = row.label.clone();
                            let done = interrupted(&mut rng, || {
                                table.update(&id, |held| held.label = label.clone());
                            });
                            if done.is_some()
                                && let Some(held) = working.get_mut(&id)
                            {
                                held.label = label;
                            }
                            ("update", done.is_some())
                        }
                    };
                    if done {
                        agrees(table, &working, &format!("{what} done"));
                    } else {
                        agrees(
                            table,
                            &before,
                            &format!("a failed {what} changed the table"),
                        );
                    }
                }
            })
            .unwrap();
            model = working;
            agrees(&db.read(), &model, "the commit");
            if commit % 2 == 1 {
                db.verify_typed().unwrap();
                drop(db);
                db = open();
                agrees(&db.read(), &model, "the reopened file");
            }
        }
    }

    #[test]
    fn new_rows_keep_rows_and_indexes_together_through_failed_loads() {
        for seed in 0..u64::from(cases(2)) {
            slim_run(seed, uncached);
        }
        for seed in 50..50 + u64::from(cases(2)) {
            slim_run(seed, roomy);
        }
    }

    thread_local! {
        static TOUCHY: Cell<bool> = const { Cell::new(false) };
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Touchy(u64);

    impl Clone for Touchy {
        fn clone(&self) -> Self {
            assert!(!TOUCHY.get(), "touched");
            Touchy(self.0)
        }
    }

    impl Data for Touchy {
        fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
            self.0.encode(w)
        }

        fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
            Ok(Touchy(u64::decode(r)?))
        }
    }

    #[derive(Data, Record, Clone, Debug, PartialEq)]
    struct Prickly {
        #[key]
        id: u64,
        #[index]
        group: u64,
        #[unique]
        slug: Option<u64>,
        #[index]
        touchy: Touchy,
    }

    #[test]
    fn a_new_row_whose_last_index_value_panics_leaves_no_entry_behind() {
        let mut table: Table<Prickly> = Table::new();
        for id in 0..100 {
            table.insert(Prickly {
                id,
                group: id % 3,
                slug: Some(id),
                touchy: Touchy(id),
            });
        }
        let hash = hash_of(&table);
        TOUCHY.set(true);
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            table.insert(Prickly {
                id: 500,
                group: 1,
                slug: Some(500),
                touchy: Touchy(5),
            })
        }));
        TOUCHY.set(false);
        assert!(outcome.is_err(), "the index value did not panic");
        assert_eq!(hash_of(&table), hash);
        assert!(table.get(&500).is_none());
        assert_eq!(table.by_group(&1).len(), 33);
        assert!(table.by_slug(&500).is_none());
        assert_eq!(hash_of(&table), hash_of(&rebuilt(&table)));
    }
}
