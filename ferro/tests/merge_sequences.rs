mod common;

use std::collections::BTreeSet;

use common::Rng;
use ferro::codec::Hash;
use ferro::{Conflict, Data, Record, Table};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[unique(shelf, bin)]
#[index(group, rank, cover(rank, note))]
struct Row {
    #[key]
    id: u8,
    #[unique(cover(group))]
    code: Option<u8>,
    #[unique]
    slug: u8,
    #[index(each, cover(slug))]
    tags: BTreeSet<u8>,
    #[index(cover(note))]
    group: u8,
    rank: u8,
    note: String,
    shelf: u8,
    bin: Option<u8>,
}

const IDS: u8 = 8;

fn hashes(table: &Table<Row>) -> Vec<Option<Hash>> {
    vec![
        table.rows.root_hash(),
        table.unique_shelf_bin.root_hash(),
        table.by_group_rank.root_hash(),
        table.unique_code.root_hash(),
        table.unique_slug.root_hash(),
        table.by_tags.root_hash(),
        table.by_group.root_hash(),
    ]
}

fn rebuilt(table: &Table<Row>, seed: u64) -> Table<Row> {
    let mut rows: Vec<Row> = table.iter().map(|row| (*row).clone()).collect();
    Rng::new(seed).shuffle(&mut rows);
    let mut fresh: Table<Row> = Table::new();
    for row in rows {
        fresh.insert(row);
    }
    fresh
}

fn check(table: &Table<Row>, seed: u64) {
    let rows: Vec<Row> = table.iter().map(|row| (*row).clone()).collect();
    for (i, a) in rows.iter().enumerate() {
        for b in &rows[i + 1..] {
            assert_ne!(a.slug, b.slug, "slug shared {a:?} {b:?}");
            assert!(
                a.code.is_none() || a.code != b.code,
                "code shared {a:?} {b:?}"
            );
            assert_ne!((a.shelf, a.bin), (b.shelf, b.bin), "shelf bin shared");
        }
    }
    assert_eq!(hashes(table), hashes(&rebuilt(table, seed)), "index hashes");
    for row in &rows {
        assert_eq!(table.key_by_slug(&row.slug), Some(row.id));
        if let Some(code) = row.code {
            assert_eq!(table.covered_by_code(&code), Some((row.id, row.group)));
        }
        assert_eq!(table.key_by_shelf_bin(&(row.shelf, row.bin)), Some(row.id));
        assert!(table.by_group(&row.group).keys().any(|id| id == row.id));
        for tag in &row.tags {
            let covered: Vec<(u8, u8)> = table.by_tags(tag).covered().collect();
            assert!(covered.contains(&(row.id, row.slug)));
        }
    }
}

#[derive(Clone, Debug)]
enum Change {
    Code(Option<u8>),
    Slug(u8),
    Tags(BTreeSet<u8>),
    Group(u8),
    Rank(u8),
    Note(u8),
    Shelf(u8, Option<u8>),
    Key(u8),
}

fn apply(row: &mut Row, change: &Change) {
    match change {
        Change::Code(code) => row.code = *code,
        Change::Slug(slug) => row.slug = *slug,
        Change::Tags(tags) => row.tags = tags.clone(),
        Change::Group(group) => row.group = *group,
        Change::Rank(rank) => row.rank = *rank,
        Change::Note(note) => row.note = "n".repeat(usize::from(*note)),
        Change::Shelf(shelf, bin) => {
            row.shelf = *shelf;
            row.bin = *bin;
        }
        Change::Key(id) => row.id = *id,
    }
}

fn change() -> impl Strategy<Value = Change> {
    prop_oneof![
        prop::option::of(0..4u8).prop_map(Change::Code),
        (0..6u8).prop_map(Change::Slug),
        prop::collection::btree_set(0..4u8, 0..3).prop_map(Change::Tags),
        (0..3u8).prop_map(Change::Group),
        (0..3u8).prop_map(Change::Rank),
        (0..3u8).prop_map(Change::Note),
        (0..2u8, prop::option::of(0..2u8)).prop_map(|(s, b)| Change::Shelf(s, b)),
        (0..IDS).prop_map(Change::Key),
    ]
}

fn row_strategy() -> impl Strategy<Value = Row> {
    (
        0..IDS,
        prop::option::of(0..4u8),
        0..6u8,
        prop::collection::btree_set(0..4u8, 0..3),
        0..3u8,
        0..3u8,
        0..3u8,
        0..2u8,
        prop::option::of(0..2u8),
    )
        .prop_map(
            |(id, code, slug, tags, group, rank, note, shelf, bin)| Row {
                id,
                code,
                slug,
                tags,
                group,
                rank,
                note: "n".repeat(usize::from(note)),
                shelf,
                bin,
            },
        )
}

#[derive(Clone, Debug)]
enum Answer {
    Ours,
    Theirs,
    Drop,
    Other(u8),
    Edited(Vec<Change>),
}

fn answer() -> impl Strategy<Value = Answer> {
    prop_oneof![
        Just(Answer::Ours),
        Just(Answer::Theirs),
        Just(Answer::Drop),
        (0..IDS).prop_map(Answer::Other),
        prop::collection::vec(change(), 1..3).prop_map(Answer::Edited),
    ]
}

#[derive(Clone, Debug)]
enum Op {
    Insert(usize, Row),
    Update(usize, u8, Vec<Change>),
    Remove(usize, u8),
    Drain(usize, u8, u8),
    Merge(usize, Vec<Answer>, bool),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..2usize, row_strategy()).prop_map(|(s, r)| Op::Insert(s, r)),
        6 => (0..2usize, 0..IDS, prop::collection::vec(change(), 1..4))
            .prop_map(|(s, id, c)| Op::Update(s, id, c)),
        1 => (0..2usize, 0..IDS).prop_map(|(s, id)| Op::Remove(s, id)),
        1 => (0..2usize, 0..IDS, 0..IDS).prop_map(|(s, a, b)| Op::Drain(s, a, b)),
        3 => (0..2usize, prop::collection::vec(answer(), 1..8), any::<bool>())
            .prop_map(|(s, a, sync)| Op::Merge(s, a, sync)),
    ]
}

fn run(ops: Vec<Op>) {
    let mut sides: [Table<Row>; 2] = [Table::new(), Table::new()];
    let mut base: Table<Row> = Table::new();
    for (step, op) in ops.into_iter().enumerate() {
        match op {
            Op::Insert(s, row) => {
                let _ = sides[s].try_insert(row);
            }
            Op::Update(s, id, changes) => {
                let _ = sides[s].try_update(&id, |row| changes.iter().for_each(|c| apply(row, c)));
            }
            Op::Remove(s, id) => {
                sides[s].remove(&id);
            }
            Op::Drain(s, a, b) => {
                sides[s].remove_range(a.min(b)..a.max(b));
            }
            Op::Merge(s, answers, sync) => {
                let theirs = sides[1 - s].clone();
                let mut calls = 0usize;
                let mut seen = BTreeSet::new();
                sides[s].merge(&base, &theirs, |c: Conflict<u8, Row>| {
                    assert!(seen.insert(c.key), "resolve called twice for {}", c.key);
                    let pick = &answers[calls % answers.len()];
                    calls += 1;
                    let ours = c.ours.as_deref().cloned();
                    let theirs = c.theirs.as_deref().cloned();
                    match pick {
                        Answer::Ours => ours,
                        Answer::Theirs => theirs,
                        Answer::Drop => None,
                        Answer::Other(id) => theirs.or(ours).map(|mut row| {
                            row.id = *id;
                            row
                        }),
                        Answer::Edited(changes) => theirs.or(ours).map(|mut row| {
                            changes
                                .iter()
                                .filter(|c| !matches!(c, Change::Key(_)))
                                .for_each(|c| apply(&mut row, c));
                            row
                        }),
                    }
                });
                if sync {
                    sides[1 - s] = sides[s].clone();
                    base = sides[s].clone();
                }
            }
        }
        check(&sides[0], step as u64);
        check(&sides[1], step as u64 + 100);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        rng_seed: RngSeed::Fixed(0xC0DE_0005),
        ..ProptestConfig::with_cases(common::cases(3000))
    })]

    #[test]
    fn merge_sequences_keep_every_index_in_step(ops in prop::collection::vec(op(), 1..40)) {
        run(ops);
    }
}
