mod common;

use std::collections::BTreeMap;

use common::{Rng, fast, uncached};
use ferro::table::{IntoMatches, Matches};
use ferro::{Data, Db, Record, Table};
use proptest::prelude::*;

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(a, b)]
struct Row {
    #[key]
    id: u32,
    #[index]
    a: u8,
    #[index]
    b: u8,
    #[index]
    opt: Option<u8>,
    #[index(each)]
    tags: Vec<u8>,
    #[index(cover(a, b))]
    c: u8,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    rows: Table<Row>,
}

#[derive(Clone, Debug)]
enum Query {
    A(u8),
    B(u8),
    Opt(Option<u8>),
    OptSome(u8),
    Tag(u8),
    Pair(u8, u8),
    Prefix(u8),
    Range(u8, u8),
    After(u8, u32),
    SomeAfter(u8, u32),
    C(u8),
    PlainA(u8),
    PlainTag(u8),
    And(Box<Query>, Box<Query>),
    Or(Box<Query>, Box<Query>),
}

impl Query {
    fn holds(&self, row: &Row) -> bool {
        match self {
            Query::A(v) => row.a == *v,
            Query::B(v) => row.b == *v,
            Query::Opt(v) => row.opt == *v,
            Query::OptSome(v) => row.opt == Some(*v),
            Query::Tag(v) => row.tags.contains(v),
            Query::Pair(a, b) => row.a == *a && row.b == *b,
            Query::Prefix(a) => row.a == *a,
            Query::Range(low, high) => (*low..*high).contains(&row.a),
            Query::After(v, key) => row.a == *v && row.id > *key,
            Query::SomeAfter(v, key) => row.opt == Some(*v) && row.id > *key,
            Query::C(v) => row.c == *v,
            Query::PlainA(v) => row.a == *v,
            Query::PlainTag(v) => row.tags.contains(v),
            Query::And(left, right) => left.holds(row) && right.holds(row),
            Query::Or(left, right) => left.holds(row) || right.holds(row),
        }
    }

    fn lookup(&self, table: &Table<Row>) -> Matches<Row> {
        let find = table.find();
        match self {
            Query::A(v) => find.by_a(v),
            Query::B(v) => find.by_b(v),
            Query::Opt(v) => find.by_opt(v),
            Query::OptSome(v) => find.by_opt_some(v),
            Query::Tag(v) => find.by_tags(v),
            Query::Pair(a, b) => find.by_a_b(&(*a, *b)),
            Query::Prefix(a) => table.by_a_b_prefix(a).into_matches(),
            Query::Range(low, high) => table.by_a_range(*low..*high).into_matches(),
            Query::After(v, key) => find.by_a_after(v, key),
            Query::SomeAfter(v, key) => find.by_opt_some_after(v, key),
            Query::C(v) => find.by_c(v).uncovered(),
            Query::PlainA(v) => table.by_a(v).into_matches(),
            Query::PlainTag(v) => table.by_tags(v).into_matches(),
            Query::And(left, right) => left.lookup(table).and(right.lookup(table)),
            Query::Or(left, right) => left.lookup(table).or(right.lookup(table)),
        }
    }
}

fn leaf(rng: &mut Rng, ids: u32) -> Query {
    let small = |rng: &mut Rng| rng.below(4) as u8;
    match rng.below(13) {
        0 => Query::A(small(rng)),
        1 => Query::B(small(rng)),
        2 => Query::Opt(if rng.chance(30) {
            None
        } else {
            Some(small(rng))
        }),
        3 => Query::OptSome(small(rng)),
        4 => Query::Tag(small(rng)),
        5 => Query::Pair(small(rng), small(rng)),
        6 => Query::Prefix(small(rng)),
        7 => {
            let low = small(rng);
            Query::Range(low, low + 1 + small(rng))
        }
        8 => Query::After(small(rng), rng.below(ids as u64 + 1) as u32),
        9 => Query::SomeAfter(small(rng), rng.below(ids as u64 + 1) as u32),
        10 => Query::PlainA(small(rng)),
        11 => Query::PlainTag(small(rng)),
        _ => Query::C(small(rng)),
    }
}

fn query(rng: &mut Rng, ids: u32, depth: u32) -> Query {
    if depth == 0 || rng.chance(30) {
        return leaf(rng, ids);
    }
    let left = Box::new(query(rng, ids, depth - 1));
    let right = Box::new(query(rng, ids, depth - 1));
    if rng.chance(60) {
        Query::And(left, right)
    } else {
        Query::Or(left, right)
    }
}

fn row(rng: &mut Rng, id: u32) -> Row {
    let small = |rng: &mut Rng| rng.below(4) as u8;
    Row {
        id,
        a: small(rng),
        b: small(rng),
        opt: if rng.chance(30) {
            None
        } else {
            Some(small(rng))
        },
        tags: (0..rng.below(4)).map(|_| small(rng)).collect(),
        c: rng.below(3) as u8,
    }
}

fn wanted(model: &BTreeMap<u32, Row>, query: &Query) -> Vec<u32> {
    model
        .values()
        .filter(|row| query.holds(row))
        .map(|row| row.id)
        .collect()
}

fn agree(table: &Table<Row>, model: &BTreeMap<u32, Row>, query: &Query, rng: &mut Rng) {
    let wanted = wanted(model, query);
    let matches = query.lookup(table);
    assert!(
        matches
            .size_hint()
            .1
            .is_some_and(|upper| upper >= wanted.len()),
        "{query:?}"
    );
    let rows: Vec<Row> = matches.map(|row| (*row).clone()).collect();
    let expected: Vec<Row> = wanted.iter().map(|id| model[id].clone()).collect();
    assert_eq!(rows, expected, "{query:?}");
    let back: Vec<u32> = query.lookup(table).rev().map(|row| row.id).collect();
    assert_eq!(
        back,
        wanted.iter().rev().copied().collect::<Vec<_>>(),
        "{query:?}"
    );
    let keys: Vec<u32> = query.lookup(table).keys().collect();
    assert_eq!(keys, wanted, "{query:?}");
    let keys_back: Vec<u32> = query.lookup(table).keys().rev().collect();
    assert_eq!(
        keys_back,
        wanted.iter().rev().copied().collect::<Vec<_>>(),
        "{query:?}"
    );
    let mut both = query.lookup(table).keys();
    let (mut front, mut back) = (Vec::new(), Vec::new());
    loop {
        let next = if rng.chance(50) {
            both.next().map(|key| front.push(key))
        } else {
            both.next_back().map(|key| back.push(key))
        };
        if next.is_none() {
            break;
        }
    }
    front.extend(back.iter().rev());
    assert_eq!(front, wanted, "both ends of {query:?}");
}

fn covered_agree(table: &Table<Row>, model: &BTreeMap<u32, Row>, c: u8, rest: &Query) {
    let both = Query::And(Box::new(Query::C(c)), Box::new(rest.clone()));
    let expected: Vec<(u32, (u8, u8))> = wanted(model, &both)
        .into_iter()
        .map(|id| (id, (model[&id].a, model[&id].b)))
        .collect();
    let covered: Vec<(u32, (u8, u8))> = table
        .find()
        .by_c(&c)
        .and(rest.lookup(table))
        .covered()
        .collect();
    assert_eq!(covered, expected, "covered {both:?}");
    let listed: Vec<(u32, (u8, u8))> = table
        .by_c(&c)
        .into_matches()
        .and(rest.lookup(table))
        .covered()
        .collect();
    assert_eq!(listed, expected, "covered and listed {both:?}");
    let mut reversed: Vec<(u32, (u8, u8))> = table
        .find()
        .by_c(&c)
        .and(rest.lookup(table))
        .covered()
        .rev()
        .collect();
    reversed.reverse();
    assert_eq!(reversed, expected, "covered back {both:?}");
    let either = Query::Or(Box::new(Query::C(c)), Box::new(Query::C((c + 1) % 3)));
    let expected: Vec<(u32, (u8, u8))> = wanted(model, &either)
        .into_iter()
        .map(|id| (id, (model[&id].a, model[&id].b)))
        .collect();
    let covered: Vec<(u32, (u8, u8))> = table
        .find()
        .by_c(&c)
        .or(table.find().by_c(&((c + 1) % 3)))
        .covered()
        .collect();
    assert_eq!(covered, expected, "covered {either:?}");
}

fn check(table: &Table<Row>, model: &BTreeMap<u32, Row>, seed: u64, queries: usize) {
    let ids = model.keys().next_back().map_or(1, |id| id + 1);
    let mut rng = Rng::new(seed);
    for _ in 0..queries {
        let query = query(&mut rng, ids, 3);
        agree(table, model, &query, &mut rng);
    }
    for c in 0..3 {
        let rest = query(&mut rng, ids, 2);
        covered_agree(table, model, c, &rest);
    }
}

fn table_of(model: &BTreeMap<u32, Row>) -> Table<Row> {
    let mut table: Table<Row> = Table::new();
    for row in model.values() {
        table.insert(row.clone());
    }
    table
}

fn stored(model: &BTreeMap<u32, Row>) -> (tempfile::TempDir, Db<Root>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("combine.ferro");
    let db: Db<Root> = fast().open(&path).unwrap();
    let table = table_of(model);
    db.update(|root| root.rows = table).unwrap();
    drop(db);
    let db = uncached().open(&path).unwrap();
    (dir, db)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(64)))]

    #[test]
    fn combined_lookups_match_a_filter(count in 0u32..120, seed in any::<u64>()) {
        let mut rng = Rng::new(seed);
        let mut model = BTreeMap::new();
        for id in 0..count {
            if rng.chance(80) {
                model.insert(id, row(&mut rng, id));
            }
        }
        check(&table_of(&model), &model, seed, 40);
        let (_dir, db) = stored(&model);
        check(&db.read().rows, &model, seed ^ 1, 40);
    }
}

fn spread(rows: u32, seed: u64) -> BTreeMap<u32, Row> {
    let mut rng = Rng::new(seed);
    (0..rows)
        .map(|id| {
            let a = if rng.chance(2) {
                0
            } else {
                1 + rng.below(3) as u8
            };
            let b = if id % 997 < 40 {
                0
            } else {
                1 + rng.below(3) as u8
            };
            let row = Row {
                id,
                a,
                b,
                opt: (id % 3 != 0).then_some((id / 500 % 4) as u8),
                tags: if rng.chance(50) { vec![0] } else { vec![1, 2] },
                c: rng.below(3) as u8,
            };
            (id, row)
        })
        .collect()
}

#[test]
fn skewed_and_clustered_lookups_match_a_filter() {
    let model = spread(6_000, 3);
    let (_dir, db) = stored(&model);
    let read = db.read();
    let table = &read.rows;
    let mut rng = Rng::new(11);
    let fixed = [
        Query::And(Box::new(Query::A(0)), Box::new(Query::Tag(1))),
        Query::And(Box::new(Query::Tag(1)), Box::new(Query::A(0))),
        Query::And(Box::new(Query::B(0)), Box::new(Query::OptSome(2))),
        Query::And(Box::new(Query::OptSome(0)), Box::new(Query::OptSome(3))),
        Query::And(Box::new(Query::A(0)), Box::new(Query::B(0))),
        Query::And(
            Box::new(Query::Or(Box::new(Query::A(0)), Box::new(Query::B(0)))),
            Box::new(Query::Tag(0)),
        ),
        Query::And(
            Box::new(Query::After(0, 3_000)),
            Box::new(Query::Range(0, 2)),
        ),
        Query::And(Box::new(Query::Tag(0)), Box::new(Query::Tag(2))),
    ];
    for query in &fixed {
        agree(table, &model, query, &mut rng);
    }
    check(table, &model, 5, 200);
    check(&table_of(&model), &model, 6, 100);
}

fn sendable<T: Send>(value: T) -> T {
    value
}

#[test]
fn empty_single_and_disjoint_lookups() {
    let mut model = BTreeMap::new();
    let table = table_of(&model);
    let find = table.find();
    assert_eq!(
        sendable(find.by_a(&0).and(find.by_c(&1)).or(table.by_tags(&2))).count(),
        0
    );
    let mut rng = Rng::new(1);
    agree(
        &table,
        &model,
        &Query::And(Box::new(Query::A(0)), Box::new(Query::B(0))),
        &mut rng,
    );
    assert_eq!(find.by_a(&0).or(find.by_b(&1)).count(), 0);
    let mut lone = row(&mut rng, 7);
    lone.a = 1;
    lone.b = 2;
    model.insert(7, lone.clone());
    let table = table_of(&model);
    let find = table.find();
    let found: Vec<Row> = find
        .by_a(&1)
        .and(find.by_b(&2))
        .map(|row| (*row).clone())
        .collect();
    assert_eq!(found, vec![lone]);
    assert_eq!(find.by_a(&1).and(find.by_b(&1)).next(), None);
    assert_eq!(find.by_a(&1).and(find.by_b(&2)).next_back().unwrap().id, 7);
    for id in 0..400 {
        let mut row = row(&mut rng, id);
        row.a = if id < 200 { 1 } else { 2 };
        row.b = if id % 2 == 0 { 1 } else { 2 };
        model.insert(id, row);
    }
    let table = table_of(&model);
    let find = table.find();
    assert_eq!(find.by_a(&1).and(find.by_a(&2)).count(), 0);
    assert_eq!(find.by_a(&2).and(find.by_a(&1)).rev().count(), 0);
    assert_eq!(find.by_b(&1).and(find.by_b(&2)).keys().count(), 0);
    let odd_low: Vec<u32> = find.by_a(&1).and(find.by_b(&2)).keys().collect();
    assert_eq!(odd_low, (1..200).step_by(2).collect::<Vec<_>>());
}

#[test]
fn a_combination_reads_the_table_as_it_was_built() {
    let mut rng = Rng::new(21);
    let mut model: BTreeMap<u32, Row> = (0..300).map(|id| (id, row(&mut rng, id))).collect();
    let mut table = table_of(&model);
    let query = Query::And(Box::new(Query::A(1)), Box::new(Query::Tag(2)));
    let before = wanted(&model, &query);
    let mut early = query.lookup(&table);
    let first = early.next().map(|row| row.id);
    let late = query.lookup(&table);
    for id in (0..300).step_by(7) {
        table.update(&id, |row| {
            row.a = 1;
            row.tags = vec![2];
        });
        model.get_mut(&id).unwrap().a = 1;
        model.get_mut(&id).unwrap().tags = vec![2];
    }
    for id in (0..300).step_by(5) {
        table.remove(&id);
        model.remove(&id);
    }
    table.insert(Row {
        id: 1_000,
        a: 1,
        b: 0,
        opt: None,
        tags: vec![2],
        c: 0,
    });
    model.insert(
        1_000,
        Row {
            id: 1_000,
            a: 1,
            b: 0,
            opt: None,
            tags: vec![2],
            c: 0,
        },
    );
    let mut rest: Vec<u32> = first.into_iter().collect();
    rest.extend(early.map(|row| row.id));
    assert_eq!(rest, before);
    assert_eq!(late.keys().collect::<Vec<_>>(), before);
    agree(&table, &model, &query, &mut rng);
}

#[test]
fn combinations_inside_a_commit_see_its_own_edits() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = fast().open(dir.path().join("edits.ferro")).unwrap();
    let mut rng = Rng::new(5);
    let mut model: BTreeMap<u32, Row> = (0..500).map(|id| (id, row(&mut rng, id))).collect();
    let table = table_of(&model);
    db.update(|root| root.rows = table).unwrap();
    let snapshot = db.read();
    let before = model.clone();
    let query = Query::And(
        Box::new(Query::Or(Box::new(Query::A(0)), Box::new(Query::A(3)))),
        Box::new(Query::B(2)),
    );
    let held = query.lookup(&snapshot.rows);
    let edited = db
        .update(|root| {
            let early = query.lookup(&root.rows);
            for id in (0..500).step_by(3) {
                root.rows.update(&id, |row| row.b = 2);
            }
            root.rows.remove(&4);
            (
                early.keys().collect::<Vec<_>>(),
                query.lookup(&root.rows).keys().collect::<Vec<_>>(),
            )
        })
        .unwrap();
    for id in (0..500).step_by(3) {
        model.get_mut(&id).unwrap().b = 2;
    }
    model.remove(&4);
    assert_eq!(edited.0, wanted(&before, &query));
    assert_eq!(edited.1, wanted(&model, &query));
    assert_eq!(held.keys().collect::<Vec<_>>(), wanted(&before, &query));
    agree(&db.read().rows, &model, &query, &mut rng);
    agree(&snapshot.rows, &before, &query, &mut rng);
}

#[test]
fn started_lookups_and_combinations_combine_from_where_they_are() {
    let mut rng = Rng::new(9);
    let model: BTreeMap<u32, Row> = (0..2_000).map(|id| (id, row(&mut rng, id))).collect();
    let table = table_of(&model);
    let mut a = table.by_a(&1);
    let skipped: Vec<u32> = (&mut a).take(30).map(|row| row.id).collect();
    let last = *skipped.last().unwrap();
    let mut a_back = table.by_a(&1);
    let from_back: Vec<u32> = (&mut a_back).rev().take(30).map(|row| row.id).collect();
    let first_back = *from_back.last().unwrap();
    let wanted_b: Vec<u32> = model
        .values()
        .filter(|row| row.a == 1 && row.b == 2 && row.id > last)
        .map(|row| row.id)
        .collect();
    let find = table.find();
    assert_eq!(
        a.into_matches()
            .and(find.by_b(&2))
            .keys()
            .collect::<Vec<_>>(),
        wanted_b
    );
    let wanted_back: Vec<u32> = model
        .values()
        .filter(|row| row.a == 1 && row.b == 2 && row.id < first_back)
        .map(|row| row.id)
        .collect();
    assert_eq!(
        find.by_b(&2).and(a_back).keys().collect::<Vec<_>>(),
        wanted_back
    );
    let mut started = find.by_a(&1).and(find.by_tags(&0));
    let front: Vec<u32> = (&mut started).take(5).map(|row| row.id).collect();
    let back: Vec<u32> = (&mut started).rev().take(5).map(|row| row.id).collect();
    let combined: Vec<u32> = started.or(find.by_b(&3)).keys().collect();
    let wanted: Vec<u32> = model
        .values()
        .filter(|row| {
            let inside = front.last().is_none_or(|low| row.id > *low)
                && back.last().is_none_or(|high| row.id < *high);
            (row.a == 1 && row.tags.contains(&0) && inside) || row.b == 3
        })
        .map(|row| row.id)
        .collect();
    assert_eq!(combined, wanted);
    let mut started = find.by_c(&1).and(find.by_b(&0));
    let first = started.next().unwrap().id;
    let covered: Vec<(u32, (u8, u8))> = started.and(find.by_a(&2)).covered().collect();
    let wanted: Vec<(u32, (u8, u8))> = model
        .values()
        .filter(|row| row.c == 1 && row.b == 0 && row.a == 2 && row.id > first)
        .map(|row| (row.id, (row.a, row.b)))
        .collect();
    assert_eq!(covered, wanted);
}
