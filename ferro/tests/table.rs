mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::ops::Bound;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;

use common::{Rng, fast};
use ferro::table::IndexIter;
use ferro::{Data, Db, Map, Record, Table};
use proptest::prelude::*;

#[derive(Data, Record, Clone, Debug, PartialEq)]
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
    DrainRange(Bound<u32>, Bound<u32>),
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
        2 => (id_bound(), id_bound()).prop_map(|(a, b)| Op::DrainRange(a, b)),
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

fn ids(iter: impl Iterator<Item = Arc<Row>>) -> Vec<(u32, Arc<Row>)> {
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
        let wanted: Vec<(u32, Arc<Row>)> = entries
            .iter()
            .filter(|(v, _)| v == value)
            .map(|(_, id)| (*id, Arc::new(model[id].clone())))
            .collect();
        let iter = eq(value);
        assert_eq!(iter.len(), wanted.len());
        assert_eq!(ids(iter), wanted, "eq {value:?}");
        let mut reversed = wanted.clone();
        reversed.reverse();
        assert_eq!(ids(eq(value).rev()), reversed);
    }
    for (low, high) in bounds(values) {
        let wanted: Vec<(u32, Arc<Row>)> = entries
            .iter()
            .filter(|(v, _)| within(v, &low, &high))
            .map(|(_, id)| (*id, Arc::new(model[id].clone())))
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
                assert_eq!(hit, model.contains_key(&id));
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
            Op::DrainRange(low, high) => {
                let wanted: Vec<Row> = model
                    .values()
                    .filter(|row| within(&row.id, &low, &high))
                    .cloned()
                    .collect();
                let drained: Vec<Row> = table
                    .drain_range::<u32, _>((low, high))
                    .iter()
                    .map(|row| (**row).clone())
                    .collect();
                assert_eq!(drained, wanted);
                model.retain(|_, row| !within(&row.id, &low, &high));
            }
            Op::PanicUpdate(id, f) => {
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    table.update(&id, |row| {
                        set_fields(row, &f);
                        panic!("boom");
                    })
                }));
                assert_eq!(result.is_err(), model.contains_key(&id));
                if let Some(row) = model.get_mut(&id) {
                    set_fields(row, &f);
                }
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
