mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::panic::{self, AssertUnwindSafe};

use common::{cases, fast};
use ferro::{Conflict, Data, Db, Record, Table, TableData, Taken};
use proptest::prelude::*;

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[unique(tenant, code)]
struct Item {
    #[key]
    id: u32,
    #[unique]
    slug: String,
    #[unique]
    email: Option<String>,
    tenant: u8,
    code: u8,
    #[index]
    group: u8,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    items: Table<Item>,
}

const IDS: u32 = 8;

type Fields = (String, Option<String>, u8, u8, u8);

fn fields() -> impl Strategy<Value = Fields> {
    (
        (0..6u8).prop_map(|n| format!("s{n}")),
        prop_oneof![
            Just(None),
            (0..4u8).prop_map(|n| Some(format!("e{n}"))),
        ],
        0..3u8,
        0..3u8,
        0..3u8,
    )
}

fn built(id: u32, fields: &Fields) -> Item {
    Item {
        id,
        slug: fields.0.clone(),
        email: fields.1.clone(),
        tenant: fields.2,
        code: fields.3,
        group: fields.4,
    }
}

#[derive(Clone, Debug)]
enum Op {
    TryInsert(u32, Fields),
    Insert(u32, Fields),
    TryUpdate(u32, Fields),
    Update(u32, Fields),
    GetMut(u32, Fields),
    Rekey(u32, u32, Fields),
    Remove(u32),
    Drain(Bound<u32>, Bound<u32>),
    Retain(u8),
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
        8 => (0..IDS, fields()).prop_map(|(id, f)| Op::TryInsert(id, f)),
        5 => (0..IDS, fields()).prop_map(|(id, f)| Op::Insert(id, f)),
        4 => (0..IDS, fields()).prop_map(|(id, f)| Op::TryUpdate(id, f)),
        4 => (0..IDS, fields()).prop_map(|(id, f)| Op::Update(id, f)),
        4 => (0..IDS, fields()).prop_map(|(id, f)| Op::GetMut(id, f)),
        4 => (0..IDS, 0..IDS, fields()).prop_map(|(id, to, f)| Op::Rekey(id, to, f)),
        3 => (0..IDS).prop_map(Op::Remove),
        1 => (id_bound(), id_bound()).prop_map(|(a, b)| Op::Drain(a, b)),
        1 => (2..4u8).prop_map(Op::Retain),
        2 => Just(Op::Commit),
        1 => Just(Op::Reopen),
    ]
}

fn clashes(model: &BTreeMap<u32, Item>, row: &Item, moving: Option<u32>) -> bool {
    model.values().any(|other| {
        other.id != row.id
            && Some(other.id) != moving
            && (other.slug == row.slug
                || (other.email.is_some() && other.email == row.email)
                || (other.tenant, other.code) == (row.tenant, row.code))
    })
}

fn within(id: u32, low: &Bound<u32>, high: &Bound<u32>) -> bool {
    (match low {
        Bound::Included(b) => id >= *b,
        Bound::Excluded(b) => id > *b,
        Bound::Unbounded => true,
    }) && (match high {
        Bound::Included(b) => id <= *b,
        Bound::Excluded(b) => id < *b,
        Bound::Unbounded => true,
    })
}

fn check(table: &Table<Item>, model: &BTreeMap<u32, Item>) {
    assert_eq!(table.len(), model.len());
    let slugs: BTreeMap<String, u32> = model.values().map(|r| (r.slug.clone(), r.id)).collect();
    let emails: BTreeMap<String, u32> = model
        .values()
        .filter_map(|r| Some((r.email.clone()?, r.id)))
        .collect();
    let pairs: BTreeMap<(u8, u8), u32> = model
        .values()
        .map(|r| ((r.tenant, r.code), r.id))
        .collect();
    let got: Vec<(String, u32)> = table.unique_slug().iter().map(|(k, v)| (k, *v)).collect();
    assert_eq!(got, slugs.clone().into_iter().collect::<Vec<_>>());
    let got: Vec<(String, u32)> = table.unique_email().iter().map(|(k, v)| (k, *v)).collect();
    assert_eq!(got, emails.clone().into_iter().collect::<Vec<_>>());
    let got: Vec<((u8, u8), u32)> = table
        .unique_tenant_code()
        .iter()
        .map(|(k, v)| (k, *v))
        .collect();
    assert_eq!(got, pairs.clone().into_iter().collect::<Vec<_>>());
    for row in model.values() {
        assert_eq!(table.by_slug(row.slug.as_str()).as_deref(), Some(row));
        assert_eq!(table.key_by_slug(row.slug.as_str()), Some(row.id));
        assert_eq!(table.by_tenant_code(&(row.tenant, row.code)).as_deref(), Some(row));
        assert_eq!(table.key_by_tenant_code(&(row.tenant, row.code)), Some(row.id));
        if let Some(email) = &row.email {
            assert_eq!(table.by_email(email.as_str()).as_deref(), Some(row));
        }
    }
    assert_eq!(table.by_slug("nobody"), None);
    assert_eq!(table.key_by_email("nobody"), None);
    let rows: Vec<u32> = model.keys().copied().collect();
    let got: BTreeSet<u32> = table.iter().map(|row| row.id).collect();
    assert_eq!(got.into_iter().collect::<Vec<_>>(), rows);
    let hash = |t: &Table<Item>| {
        (
            t.rows().root_hash(),
            t.unique_slug().root_hash(),
            t.unique_email().root_hash(),
            t.unique_tenant_code().root_hash(),
        )
    };
    let mut fresh: Table<Item> = Table::new();
    for row in model.values() {
        fresh.insert(row.clone());
    }
    assert_eq!(hash(table), hash(&fresh));
}

fn panics(f: impl FnOnce()) -> bool {
    panic::catch_unwind(AssertUnwindSafe(f)).is_err()
}

fn run(ops: Vec<Op>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("u.ferro");
    let mut db: Db<Root> = fast().open(&path).unwrap();
    let mut table: Table<Item> = Table::new();
    let mut model: BTreeMap<u32, Item> = BTreeMap::new();
    let mut committed = model.clone();
    for op in ops {
        match op {
            Op::TryInsert(id, fields) => {
                let row = built(id, &fields);
                let result = table.try_insert(row.clone());
                assert_eq!(result.is_err(), clashes(&model, &row, None));
                if let Err(taken) = result {
                    assert_ne!(taken.holder, id);
                } else {
                    model.insert(id, row);
                }
            }
            Op::Insert(id, fields) => {
                let row = built(id, &fields);
                let clash = clashes(&model, &row, None);
                assert_eq!(panics(|| drop(table.insert(row.clone()))), clash);
                if !clash {
                    model.insert(id, row);
                }
            }
            Op::TryUpdate(id, fields) => {
                let row = built(id, &fields);
                let clash = clashes(&model, &row, None);
                let result = table.try_update(&id, |target| *target = row.clone());
                match result {
                    Err(_) => assert!(clash),
                    Ok(hit) => {
                        assert_eq!(hit, model.contains_key(&id));
                        if hit {
                            assert!(!clash);
                            model.insert(id, row);
                        }
                    }
                }
            }
            Op::Update(id, fields) => {
                let row = built(id, &fields);
                let clash = model.contains_key(&id) && clashes(&model, &row, None);
                let outcome = panics(|| {
                    table.update(&id, |target| *target = row.clone());
                });
                assert_eq!(outcome, clash);
                if model.contains_key(&id) && !clash {
                    model.insert(id, row);
                }
            }
            Op::GetMut(id, fields) => {
                let row = built(id, &fields);
                let clash = model.contains_key(&id) && clashes(&model, &row, None);
                let outcome = panics(|| {
                    if let Some(mut target) = table.get_mut(&id) {
                        *target = row.clone();
                    }
                });
                assert_eq!(outcome, clash);
                if model.contains_key(&id) && !clash {
                    model.insert(id, row);
                }
            }
            Op::Rekey(id, to, fields) => {
                let row = built(to, &fields);
                let clash = model.contains_key(&id) && clashes(&model, &row, Some(id));
                let outcome = panics(|| {
                    if let Some(mut target) = table.get_mut(&id) {
                        *target = row.clone();
                    }
                });
                assert_eq!(outcome, clash);
                if model.contains_key(&id) && !clash {
                    model.remove(&id);
                    model.insert(to, row);
                }
            }
            Op::Remove(id) => {
                let old = table.remove(&id);
                assert_eq!(old.map(|row| (*row).clone()), model.remove(&id));
            }
            Op::Drain(low, high) => {
                let drained = table.drain_range::<u32, _>((low, high));
                let wanted: Vec<u32> = model
                    .keys()
                    .copied()
                    .filter(|id| within(*id, &low, &high))
                    .collect();
                assert_eq!(drained.iter().map(|row| row.id).collect::<Vec<_>>(), wanted);
                model.retain(|id, _| !within(*id, &low, &high));
            }
            Op::Retain(modulus) => {
                table.retain(|row| row.group % modulus == 0);
                model.retain(|_, row| row.group % modulus == 0);
            }
            Op::Commit => {
                let snapshot = table.clone();
                db.update(|root| root.items = snapshot).unwrap();
                committed = model.clone();
                check(&db.read().items, &committed);
            }
            Op::Reopen => {
                drop(std::mem::take(&mut table));
                drop(db);
                db = fast().open(&path).unwrap();
                table = db.read().items.clone();
                model = committed.clone();
            }
        }
        check(&table, &model);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(300), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn unique_indexes_match_a_model(ops in prop::collection::vec(op(), 1..60)) {
        common::quiet_ferro_panics();
        run(ops);
    }
}

fn item(id: u32, slug: &str) -> Item {
    Item {
        id,
        slug: slug.to_string(),
        email: None,
        tenant: id as u8,
        code: 0,
        group: 0,
    }
}

#[test]
fn conflict_names_the_index_and_the_holder() {
    let mut table: Table<Item> = Table::new();
    table.insert(item(1, "a"));
    let taken = table.try_insert(item(2, "a")).unwrap_err();
    assert_eq!(
        taken,
        Taken {
            index: "slug",
            holder: 1
        }
    );
    assert_eq!(
        taken.to_string(),
        "unique index slug already holds this value for 1"
    );
    let mut row = item(2, "b");
    row.tenant = 1;
    let taken = table.try_insert(row).unwrap_err();
    assert_eq!(taken.index, "tenant_code");
    assert_eq!(table.len(), 1);
    let boxed: Box<dyn std::error::Error> = Box::new(taken);
    assert!(boxed.to_string().contains("tenant_code"));
}

#[test]
fn replacing_a_row_may_keep_its_own_value() {
    let mut table: Table<Item> = Table::new();
    table.insert(item(1, "a"));
    let mut row = item(1, "a");
    row.group = 9;
    assert!(table.try_insert(row).unwrap().is_some());
    assert_eq!(table.by_slug("a").unwrap().group, 9);
    assert_eq!(table.len(), 1);
}

#[test]
fn a_conflict_inside_update_discards_the_transaction() {
    common::quiet_ferro_panics();
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = fast().open(dir.path().join("t.ferro")).unwrap();
    db.update(|root| {
        root.items.insert(item(1, "a"));
        root.items.insert(item(2, "b"));
    })
    .unwrap();
    let version = db.version();
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = db.update(|root| {
            root.items.remove(&2);
            root.items.insert(item(3, "c"));
            root.items.insert(item(4, "c"));
        });
    }));
    let message = common::panic_text(&*outcome.unwrap_err());
    assert_eq!(message, "ferro: unique index slug already holds this value");
    assert_eq!(db.version(), version);
    let snapshot = db.read();
    assert_eq!(snapshot.items.by_slug("b").map(|row| row.id), Some(2));
    assert_eq!(snapshot.items.by_slug("c"), None);
}

#[test]
fn panicking_while_a_conflicting_row_is_open_does_not_abort() {
    common::quiet_ferro_panics();
    let mut table: Table<Item> = Table::new();
    table.insert(item(1, "a"));
    table.insert(item(2, "b"));
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        table.update(&2, |row| {
            row.slug = "a".to_string();
            panic!("ferro: boom");
        });
    }));
    assert!(outcome.is_err());
    assert_eq!(table.by_slug("a").map(|row| row.id), Some(1));
    assert_eq!(table.by_slug("b").map(|row| row.id), Some(2));
}

#[test]
fn swapping_two_values_needs_a_free_value_between() {
    let mut table: Table<Item> = Table::new();
    table.insert(item(1, "a"));
    table.insert(item(2, "b"));
    assert!(
        table
            .try_update(&1, |row| row.slug = "b".to_string())
            .is_err()
    );
    assert_eq!(table.by_slug("a").map(|row| row.id), Some(1));
    table.update(&1, |row| row.slug = "free".to_string());
    table.update(&2, |row| row.slug = "a".to_string());
    table.update(&1, |row| row.slug = "b".to_string());
    assert_eq!(table.key_by_slug("a"), Some(2));
    assert_eq!(table.key_by_slug("b"), Some(1));
    assert_eq!(table.unique_slug().len(), 2);
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct OldItem {
    #[key]
    id: u32,
    slug: String,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct NewItem {
    #[key]
    id: u32,
    #[unique]
    slug: String,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct OldRoot {
    rows: Table<OldItem>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct NewRoot {
    rows: Table<NewItem>,
}

#[test]
fn a_table_written_before_the_index_existed_gets_it_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.ferro");
    {
        let db: Db<OldRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..50u32 {
                root.rows.insert(OldItem {
                    id,
                    slug: format!("s{id}"),
                });
            }
        })
        .unwrap();
    }
    let db: Db<NewRoot> = fast().open(&path).unwrap();
    assert_eq!(db.read().rows.key_by_slug("s7"), Some(7));
    assert_eq!(db.read().rows.unique_slug().len(), 50);
    db.update(|root| {
        root.rows.update(&7, |row| row.slug = "renamed".to_string());
    })
    .unwrap();
    drop(db);
    let db: Db<NewRoot> = fast().open(&path).unwrap();
    assert_eq!(db.read().rows.by_slug("s7"), None);
    assert_eq!(db.read().rows.key_by_slug("renamed"), Some(7));
    assert_eq!(db.read().rows.unique_slug().len(), 50);
    assert_eq!(db.at(1).unwrap().rows.key_by_slug("s7"), Some(7));
}

#[test]
fn rebuilding_over_duplicates_reports_a_decode_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.ferro");
    {
        let db: Db<OldRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..3u32 {
                root.rows.insert(OldItem {
                    id,
                    slug: "same".to_string(),
                });
            }
        })
        .unwrap();
    }
    match fast().open::<NewRoot>(&path) {
        Err(ferro::Error::Decode(message)) => {
            assert_eq!(message, "two rows of NewItem share a value of unique index slug");
        }
        Err(other) => panic!("wrong error {other}"),
        Ok(_) => panic!("duplicates passed the rebuild"),
    }
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Plain {
    #[key]
    id: u32,
    #[index]
    slug: String,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct PlainRoot {
    rows: Table<Plain>,
}

#[test]
fn turning_an_index_into_a_unique_index_rebuilds_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.ferro");
    {
        let db: Db<PlainRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            root.rows.insert(Plain {
                id: 1,
                slug: "a".to_string(),
            });
        })
        .unwrap();
    }
    let db: Db<NewRoot> = fast().open(&path).unwrap();
    assert_eq!(db.read().rows.key_by_slug("a"), Some(1));
}

fn merged(
    base: &Table<NewItem>,
    ours: &Table<NewItem>,
    theirs: &Table<NewItem>,
    resolve: impl FnMut(Conflict<u32, NewItem>) -> Option<NewItem>,
) -> Table<NewItem> {
    let mut out = ours.clone();
    out.merge(base, theirs, resolve);
    out
}

fn named(rows: &[(u32, &str)]) -> Table<NewItem> {
    let mut table: Table<NewItem> = Table::new();
    for (id, slug) in rows {
        table.insert(NewItem {
            id: *id,
            slug: slug.to_string(),
        });
    }
    table
}

fn listing(table: &Table<NewItem>) -> Vec<(u32, String)> {
    table.iter().map(|row| (row.id, row.slug.clone())).collect()
}

#[test]
fn a_merged_chain_of_renames_is_not_a_conflict() {
    let base = named(&[(1, "a"), (2, "b"), (3, "c")]);
    let theirs = named(&[(1, "b"), (2, "c"), (3, "d")]);
    let out = merged(&base, &base, &theirs, |_| panic!("no conflict expected"));
    assert_eq!(listing(&out), listing(&theirs));
    assert_eq!(out.key_by_slug("a"), None);
    assert_eq!(out.key_by_slug("d"), Some(3));
}

#[test]
fn rows_added_on_both_sides_with_one_value_go_to_resolve() {
    let base = named(&[(1, "a")]);
    let ours = named(&[(1, "a"), (2, "x")]);
    let theirs = named(&[(1, "a"), (3, "x")]);
    let mut seen = Vec::new();
    let dropped = merged(&base, &ours, &theirs, |conflict| {
        seen.push((
            conflict.key,
            conflict.ours.map(|row| row.id),
            conflict.theirs.map(|row| row.id),
        ));
        None
    });
    assert_eq!(seen, [(3, Some(2), Some(3))]);
    assert_eq!(listing(&dropped), listing(&ours));
    let renamed = merged(&base, &ours, &theirs, |conflict| {
        Some(NewItem {
            id: conflict.key,
            slug: "x2".to_string(),
        })
    });
    assert_eq!(renamed.key_by_slug("x2"), Some(3));
    assert_eq!(renamed.key_by_slug("x"), Some(2));
}

#[test]
fn a_rename_into_a_value_added_by_ours_keeps_the_row_when_asked() {
    let base = named(&[(1, "a")]);
    let ours = named(&[(1, "a"), (2, "b")]);
    let theirs = named(&[(1, "b")]);
    let out = merged(&base, &ours, &theirs, |conflict| {
        conflict.base.as_deref().cloned()
    });
    assert_eq!(listing(&out), listing(&ours));
}
