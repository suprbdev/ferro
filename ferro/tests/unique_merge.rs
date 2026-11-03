mod common;

use std::collections::{BTreeMap, BTreeSet};

use ferro::{Conflict, Data, Record, Table};
use proptest::prelude::*;

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u8,
    #[unique]
    slug: u8,
    #[unique]
    code: Option<u8>,
    #[index]
    n: u8,
    #[index(each)]
    tags: Vec<u8>,
}

type Fields = (u8, Option<u8>, u8);
type Side = BTreeMap<u8, Fields>;
type Entry = (u8, Fields);
type Call = (u8, Option<Entry>, Option<Entry>, Option<Entry>);

const IDS: u8 = 8;
const SLUGS: u8 = 6;

fn row(id: u8, fields: Fields) -> Row {
    Row {
        id,
        slug: fields.0,
        code: fields.1,
        n: fields.2,
        tags: vec![fields.2 % 2, fields.0 % 3],
    }
}

fn entry(row: &Row) -> Entry {
    (row.id, (row.slug, row.code, row.n))
}

fn fields() -> impl Strategy<Value = Fields> {
    (0..SLUGS, prop::option::of(0..3u8), 0..3u8)
}

fn valid(side: Side) -> Side {
    let mut slugs = BTreeSet::new();
    let mut codes = BTreeSet::new();
    let mut out = Side::new();
    for (id, fields) in side {
        if slugs.contains(&fields.0) || fields.1.is_some_and(|code| codes.contains(&code)) {
            continue;
        }
        slugs.insert(fields.0);
        codes.extend(fields.1);
        out.insert(id, fields);
    }
    out
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
                Some((key, ((o.0 + t.0 + 1) % SLUGS, t.1.or(o.1), o.2.max(t.2))))
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

fn holder(rows: &Side, id: u8, fields: &Fields) -> Option<u8> {
    let slug = rows
        .iter()
        .find(|(other, held)| **other != id && held.0 == fields.0);
    let code = rows
        .iter()
        .find(|(other, held)| **other != id && held.1.is_some() && held.1 == fields.1);
    slug.or(code).map(|(other, _)| *other)
}

fn settle(rows: &mut Side, held: &mut BTreeMap<u8, (Fields, bool)>) {
    while let Some(id) = held
        .iter()
        .find(|(id, (fields, _))| holder(rows, **id, fields).is_none())
        .map(|(id, _)| *id)
    {
        let (fields, _) = held.remove(&id).unwrap();
        rows.insert(id, fields);
    }
}

struct Expected {
    rows: Side,
    calls: Vec<Call>,
    written: usize,
}

fn model(base: &Side, ours: &Side, theirs: &Side, policy: Policy) -> Expected {
    let mut keys = BTreeSet::new();
    keys.extend(base.keys());
    keys.extend(ours.keys());
    keys.extend(theirs.keys());
    let at = |side: &Side, key: u8| side.get(&key).map(|fields| (key, *fields));
    let mut rows = ours.clone();
    let mut calls = Vec::new();
    let mut held = BTreeMap::new();
    let mut written = 0;
    for key in keys {
        let (b, o, t) = (at(base, key), at(ours, key), at(theirs, key));
        if b == t || o == t {
            continue;
        }
        written += 1;
        let (want, asked) = if o == b {
            (t, false)
        } else {
            calls.push((key, b, o, t));
            (answer(policy, key, o, t), true)
        };
        match want {
            None => {
                rows.remove(&key);
            }
            Some((id, fields)) if id == key => {
                if holder(&rows, key, &fields).is_none() {
                    rows.insert(key, fields);
                } else {
                    held.insert(key, (fields, asked));
                }
            }
            Some(_) => {}
        }
    }
    loop {
        settle(&mut rows, &mut held);
        let Some((key, fields)) = held
            .iter()
            .find(|(_, (_, asked))| !asked)
            .map(|(key, (fields, _))| (*key, *fields))
        else {
            break;
        };
        held.remove(&key);
        let by = holder(&rows, key, &fields).unwrap();
        let ours = Some((by, rows[&by]));
        let theirs = Some((key, fields));
        calls.push((key, at(base, key), ours, theirs));
        match answer(policy, key, ours, theirs) {
            None => {
                rows.remove(&key);
            }
            Some((id, fields)) if id == key => {
                held.insert(key, (fields, true));
            }
            Some(_) => {}
        }
    }
    Expected {
        rows,
        calls,
        written,
    }
}

fn table(side: &Side) -> Table<Row> {
    let mut table: Table<Row> = Table::new();
    for (id, fields) in side {
        table.insert(row(*id, *fields));
    }
    table
}

fn merged(
    base: &Side,
    ours: &Side,
    theirs: &Side,
    policy: Policy,
) -> (Table<Row>, Vec<Call>, usize) {
    let mut target = table(ours);
    let mut calls = Vec::new();
    let written = target.merge(&table(base), &table(theirs), |c: Conflict<u8, Row>| {
        let ours = c.ours.as_deref().map(entry);
        let theirs = c.theirs.as_deref().map(entry);
        calls.push((c.key, c.base.as_deref().map(entry), ours, theirs));
        answer(policy, c.key, ours, theirs).map(|(id, fields)| row(id, fields))
    });
    (target, calls, written)
}

fn assert_unique(side: &Side) -> Result<(), TestCaseError> {
    for (id, fields) in side {
        prop_assert_eq!(
            holder(side, *id, fields),
            None,
            "row {} shares a unique value",
            id
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(2000)))]

    #[test]
    fn unique_table_merge_matches_a_model((base, ours, theirs) in sides(), policy in policy()) {
        let expected = model(&base, &ours, &theirs, policy);
        let (got, calls, written) = merged(&base, &ours, &theirs, policy);

        let mut counts: BTreeMap<u8, usize> = BTreeMap::new();
        for call in &calls {
            *counts.entry(call.0).or_default() += 1;
        }
        prop_assert!(counts.values().all(|n| *n == 1), "a key resolved twice: {:?}", calls);
        for key in 0..IDS {
            let (b, o, t) = (base.get(&key), ours.get(&key), theirs.get(&key));
            if b != t && o != t && o != b {
                prop_assert_eq!(counts.get(&key), Some(&1), "key {} changed on both sides", key);
            }
        }

        prop_assert_eq!(&calls, &expected.calls);
        prop_assert_eq!(written, expected.written);
        let rows: Side = got.iter().map(|r| (r.id, (r.slug, r.code, r.n))).collect();
        prop_assert_eq!(&rows, &expected.rows);
        assert_unique(&rows)?;

        for (key, fields) in &ours {
            let asked = counts.contains_key(key);
            let theirs_removed = base.get(key) == Some(fields) && !theirs.contains_key(key);
            if !rows.contains_key(key) {
                prop_assert!(asked || theirs_removed, "row {} vanished without a decision", key);
            }
        }

        let want = table(&expected.rows);
        prop_assert!(got == want);
        prop_assert_eq!(got.rows().root_hash(), want.rows().root_hash());
        prop_assert_eq!(got.unique_slug().root_hash(), want.unique_slug().root_hash());
        prop_assert_eq!(got.unique_code().root_hash(), want.unique_code().root_hash());
        prop_assert_eq!(got.by_n.root_hash(), want.by_n.root_hash());
        prop_assert_eq!(got.by_tags.root_hash(), want.by_tags.root_hash());
        prop_assert_eq!(got.unique_slug().len(), rows.len());
        prop_assert_eq!(got.unique_code().len(), rows.values().filter(|f| f.1.is_some()).count());
    }
}

fn side(rows: &[(u8, u8)]) -> Side {
    rows.iter()
        .map(|(id, slug)| (*id, (*slug, None, 0)))
        .collect()
}

#[test]
fn a_combined_row_that_clashes_is_resolved_once_and_keeps_our_row() {
    let base = side(&[(1, 0)]);
    let ours = side(&[(1, 1), (2, 5)]);
    let theirs = side(&[(1, 3)]);
    let (got, calls, _) = merged(&base, &ours, &theirs, Policy::Combine);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, 1);
    assert_eq!(got.get(&1).map(|r| r.slug), Some(1));
    assert_eq!(got.get(&2).map(|r| r.slug), Some(5));
}

#[test]
fn keeping_ours_on_a_clash_freed_by_an_earlier_answer_keeps_the_row() {
    let base = side(&[(1, 0), (2, 1)]);
    let ours = side(&[(1, 0), (2, 1), (5, 2)]);
    let theirs = side(&[(1, 2), (2, 0)]);
    let mut calls = Vec::new();
    let mut target = table(&ours);
    target.merge(&table(&base), &table(&theirs), |c: Conflict<u8, Row>| {
        calls.push(c.key);
        if c.key == 1 {
            None
        } else {
            c.ours.as_deref().cloned()
        }
    });
    assert_eq!(calls, [1]);
    assert_eq!(target.get(&1), None);
    assert_eq!(target.get(&2).map(|r| r.slug), Some(0));
    assert_eq!(target.get(&5).map(|r| r.slug), Some(2));
}

#[test]
fn rows_that_swap_values_reach_resolve_once_each() {
    let base = side(&[(1, 0), (2, 1)]);
    let theirs = side(&[(1, 1), (2, 0)]);
    let (got, calls, _) = merged(&base, &base, &theirs, Policy::Ours);
    assert_eq!(calls.iter().map(|c| c.0).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(calls[0].2.map(|e| e.0), Some(2));
    assert_eq!(got.unique_slug().len(), 2);
    assert_eq!(got.get(&1).map(|r| r.slug), Some(0));
    assert_eq!(got.get(&2).map(|r| r.slug), Some(1));
}
