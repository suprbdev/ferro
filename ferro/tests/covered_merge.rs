mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::fast;
use ferro::{Conflict, Data, Db, Record, Snapshot, Table, Val};
use proptest::prelude::*;

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(group, rank, cover(rank, label))]
struct Item {
    #[key]
    id: u8,
    #[index(cover(label, rank))]
    group: u8,
    #[unique(cover(group, label))]
    slug: u8,
    #[index(each, cover(rank))]
    tags: Vec<u8>,
    rank: u8,
    label: String,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(group, rank)]
struct PlainItem {
    #[key]
    id: u8,
    #[index]
    group: u8,
    #[unique]
    slug: u8,
    #[index(each)]
    tags: Vec<u8>,
    rank: u8,
    label: String,
}

type Fields = (u8, u8, u8, u8);
type Side = BTreeMap<u8, Fields>;
type Entry = (u8, Fields);
type Call = (u8, Option<Entry>, Option<Entry>, Option<Entry>);

const IDS: u8 = 8;
const GROUPS: u8 = 3;
const SLUGS: u8 = 6;
const LABELS: [&str; 3] = ["", "red", "a long blue label"];

trait Shape: Record<Key = u8> + PartialEq {
    fn make(id: u8, fields: Fields) -> Self;
    fn entry(&self) -> Entry;
}

fn label_of(text: &str) -> u8 {
    LABELS.iter().position(|label| *label == text).unwrap() as u8
}

impl Shape for Item {
    fn make(id: u8, (group, slug, rank, label): Fields) -> Self {
        Item {
            id,
            group,
            slug,
            tags: vec![rank % 2, slug % 3],
            rank,
            label: LABELS[usize::from(label)].to_string(),
        }
    }

    fn entry(&self) -> Entry {
        (
            self.id,
            (self.group, self.slug, self.rank, label_of(&self.label)),
        )
    }
}

impl Shape for PlainItem {
    fn make(id: u8, (group, slug, rank, label): Fields) -> Self {
        PlainItem {
            id,
            group,
            slug,
            tags: vec![rank % 2, slug % 3],
            rank,
            label: LABELS[usize::from(label)].to_string(),
        }
    }

    fn entry(&self) -> Entry {
        (
            self.id,
            (self.group, self.slug, self.rank, label_of(&self.label)),
        )
    }
}

fn fields() -> impl Strategy<Value = Fields> {
    (0..GROUPS, 0..SLUGS, 0..4u8, 0..LABELS.len() as u8)
}

fn valid(side: Side) -> Side {
    let mut slugs = BTreeSet::new();
    side.into_iter()
        .filter(|(_, fields)| slugs.insert(fields.1))
        .collect()
}

fn edits() -> impl Strategy<Value = Vec<(u8, Option<Fields>)>> {
    prop::collection::vec((0..IDS, prop::option::of(fields())), 0..8)
}

fn edited(base: &Side, edits: &[(u8, Option<Fields>)]) -> Side {
    let mut side = base.clone();
    for (id, change) in edits {
        match change {
            Some(fields) => {
                side.insert(*id, *fields);
            }
            None => {
                side.remove(id);
            }
        }
    }
    valid(side)
}

fn sides() -> impl Strategy<Value = (Side, Side, Side)> {
    (
        prop::collection::btree_map(0..IDS, fields(), 0..IDS as usize),
        edits(),
        edits(),
    )
        .prop_map(|(base, ours, theirs)| {
            let base = valid(base);
            let ours = edited(&base, &ours);
            let theirs = edited(&base, &theirs);
            (base, ours, theirs)
        })
}

#[derive(Clone, Copy, Debug)]
enum Policy {
    Ours,
    Theirs,
    Drop,
    Combine,
    Mixed,
}

fn policy() -> impl Strategy<Value = Policy> {
    prop_oneof![
        Just(Policy::Ours),
        Just(Policy::Theirs),
        Just(Policy::Drop),
        Just(Policy::Combine),
        Just(Policy::Mixed),
    ]
}

fn answer(policy: Policy, key: u8, ours: Option<Entry>, theirs: Option<Entry>) -> Option<Entry> {
    match policy {
        Policy::Ours => ours,
        Policy::Theirs => theirs,
        Policy::Drop => None,
        Policy::Combine => match (ours, theirs) {
            (Some((_, o)), Some((_, t))) => {
                Some((key, (t.0, (o.1 + t.1 + 1) % SLUGS, o.2.max(t.2), o.3)))
            }
            (Some((_, o)), None) => Some((key, o)),
            (None, theirs) => theirs,
        },
        Policy::Mixed => {
            let pick = [Policy::Combine, Policy::Ours, Policy::Theirs, Policy::Drop];
            answer(pick[usize::from(key % 4)], key, ours, theirs)
        }
    }
}

fn table<T: Shape>(side: &Side) -> Table<T> {
    let mut table: Table<T> = Table::new();
    for (id, fields) in side {
        table.insert(T::make(*id, *fields));
    }
    table
}

fn merged<T: Shape>(
    base: &Side,
    ours: &Side,
    theirs: &Side,
    policy: Policy,
) -> (Table<T>, Vec<Call>, usize) {
    let mut target = table::<T>(ours);
    let mut calls = Vec::new();
    let written = target.merge(&table(base), &table(theirs), |c: Conflict<u8, T>| {
        let ours = c.ours.as_deref().map(T::entry);
        let theirs = c.theirs.as_deref().map(T::entry);
        calls.push((c.key, c.base.as_deref().map(T::entry), ours, theirs));
        answer(policy, c.key, ours, theirs).map(|(id, fields)| T::make(id, fields))
    });
    (target, calls, written)
}

fn rows_of<T: Shape>(table: &Table<T>) -> Vec<Entry> {
    table.iter().map(|row| row.entry()).collect()
}

fn check(table: &Table<Item>) {
    let rows: Vec<Item> = table.iter().map(|row| (*row).clone()).collect();
    for group in 0..GROUPS + 1 {
        let wanted: Vec<(u8, (String, u8))> = rows
            .iter()
            .filter(|row| row.group == group)
            .map(|row| (row.id, (row.label.clone(), row.rank)))
            .collect();
        assert_eq!(table.by_group(&group).covered().collect::<Vec<_>>(), wanted);
        let read: Vec<(u8, (String, u8))> = table
            .by_group(&group)
            .map(|row| (row.id, (row.label.clone(), row.rank)))
            .collect();
        assert_eq!(read, wanted);
        let mut ranked: Vec<&Item> = rows.iter().filter(|row| row.group == group).collect();
        ranked.sort_by_key(|row| (row.rank, row.id));
        let wanted: Vec<(u8, (u8, String))> = ranked
            .iter()
            .map(|row| (row.id, (row.rank, row.label.clone())))
            .collect();
        assert_eq!(
            table
                .by_group_rank_prefix(&group)
                .covered()
                .collect::<Vec<_>>(),
            wanted
        );
    }
    for tag in 0..4u8 {
        let wanted: Vec<(u8, u8)> = rows
            .iter()
            .filter(|row| row.tags.contains(&tag))
            .map(|row| (row.id, row.rank))
            .collect();
        assert_eq!(table.by_tags(&tag).covered().collect::<Vec<_>>(), wanted);
    }
    for slug in 0..SLUGS {
        let holder = rows.iter().find(|row| row.slug == slug);
        assert_eq!(
            table.covered_by_slug(&slug),
            holder.map(|row| (row.id, (row.group, row.label.clone())))
        );
    }
    let mut rebuilt: Table<Item> = Table::new();
    for row in rows.into_iter().rev() {
        rebuilt.insert(row);
    }
    same_form(table, &rebuilt);
}

fn same_form(a: &Table<Item>, b: &Table<Item>) {
    assert!(a == b);
    assert_eq!(a.rows.root_hash(), b.rows.root_hash());
    assert_eq!(a.by_group.root_hash(), b.by_group.root_hash());
    assert_eq!(a.by_group_rank.root_hash(), b.by_group_rank.root_hash());
    assert_eq!(a.by_tags.root_hash(), b.by_tags.root_hash());
    assert_eq!(a.unique_slug.root_hash(), b.unique_slug.root_hash());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(1000)))]

    #[test]
    fn merging_covered_tables_matches_a_plain_merge_and_the_rows(
        (base, ours, theirs) in sides(),
        policy in policy(),
    ) {
        let (covered, covered_calls, covered_written) = merged::<Item>(&base, &ours, &theirs, policy);
        let (plain, plain_calls, plain_written) = merged::<PlainItem>(&base, &ours, &theirs, policy);
        prop_assert_eq!(rows_of(&covered), rows_of(&plain));
        prop_assert_eq!(covered_calls, plain_calls);
        prop_assert_eq!(covered_written, plain_written);
        check(&covered);
    }
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Shop {
    items: Table<Item>,
}

fn item(id: u8, group: u8, slug: u8, rank: u8, label: &str) -> Item {
    Item {
        id,
        group,
        slug,
        tags: vec![rank % 2, slug % 3],
        rank,
        label: label.to_string(),
    }
}

fn resolve(conflict: Conflict<u8, Item>) -> Option<Item> {
    match (conflict.ours, conflict.theirs) {
        (Some(ours), Some(theirs)) => {
            let mut row = theirs.into_owned();
            row.label = format!("{}+{}", ours.label, row.label);
            Some(row)
        }
        (ours, theirs) => theirs.or(ours).map(Val::into_owned),
    }
}

fn merge(root: &mut Shop, base: &Snapshot<Shop>, theirs: &Snapshot<Shop>) -> (String, ()) {
    root.items.merge(&base.items, &theirs.items, resolve);
    ("merge".to_string(), ())
}

fn open(dir: &tempfile::TempDir, name: &str) -> Db<Shop> {
    fast().open(dir.path().join(name)).unwrap()
}

#[test]
fn covered_indexes_survive_sync_and_merge_between_devices() {
    let dir = tempfile::tempdir().unwrap();
    let a = open(&dir, "a.ferro");
    let b = open(&dir, "b.ferro");
    a.update(|shop| {
        for id in 0..20u8 {
            shop.items.insert(item(id, id % 3, id, id % 4, "new"));
        }
    })
    .unwrap();
    b.pull(&a, "a", merge).unwrap();
    check(&b.read().items);
    assert_eq!(b.read().items.by_group(&1).covered().count(), 7);

    a.update(|shop| {
        for id in 0..5u8 {
            shop.items.update(&id, |row| row.label = "a-label".into());
        }
        for id in 5..10u8 {
            shop.items.update(&id, |row| row.rank += 4);
        }
        shop.items.remove(&10);
        shop.items.remove(&11);
        for id in 20..23u8 {
            shop.items.insert(item(id, 2, id, 1, "from a"));
        }
    })
    .unwrap();
    b.update(|shop| {
        for id in 3..8u8 {
            shop.items.update(&id, |row| row.label = "b-label".into());
        }
        shop.items.update(&15, |row| row.slug = 40);
        shop.items.update(&16, |row| row.group = 0);
        for id in 30..33u8 {
            shop.items.insert(item(id, 1, id, 2, "from b"));
        }
    })
    .unwrap();

    b.pull(&a, "a", merge).unwrap();
    let merged = b.read();
    check(&merged.items);
    assert_eq!(
        merged.items.covered_by_slug(&3),
        Some((3, (0, "b-label+a-label".to_string())))
    );
    assert_eq!(
        merged.items.covered_by_slug(&6),
        Some((6, (0, "b-label+new".to_string())))
    );
    assert_eq!(
        merged.items.covered_by_slug(&2),
        Some((2, (2, "a-label".to_string())))
    );
    assert_eq!(
        merged.items.covered_by_slug(&40),
        Some((15, (0, "new".to_string())))
    );
    assert_eq!(merged.items.covered_by_slug(&15), None);
    let ranks: Vec<(u8, u8)> = merged
        .items
        .by_tags(&1)
        .covered()
        .filter(|(id, _)| (5..10).contains(id))
        .collect();
    assert_eq!(ranks, vec![(5, 5), (7, 7), (9, 5)]);
    assert!(merged.items.get(&10).is_none());
    assert_eq!(merged.items.len(), 24);
    drop(merged);

    a.pull(&b, "b", merge).unwrap();
    let (left, right) = (a.read(), b.read());
    check(&left.items);
    same_form(&left.items, &right.items);
    drop((left, right));

    let c = open(&dir, "c.ferro");
    let bundle = b.bundle(&c.have().unwrap()).unwrap();
    c.receive("b", &bundle, merge).unwrap();
    same_form(&c.read().items, &b.read().items);
    check(&c.read().items);

    let wanted = b.read().items.clone();
    drop((a, b, c));
    for name in ["a.ferro", "b.ferro", "c.ferro"] {
        let db = open(&dir, name);
        let read = db.read();
        assert!(!read.items.by_group.is_dirty());
        assert!(!read.items.unique_slug.is_dirty());
        check(&read.items);
        same_form(&read.items, &wanted);
    }
}
