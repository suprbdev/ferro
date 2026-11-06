mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Bound, RangeBounds};

use common::{Rng, fast, uncached};
use ferro::table::{IndexIter, IntoMatches, Matches};
use ferro::{Data, Db, Record, Table};
use proptest::prelude::*;

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(band, score)]
#[unique(shelf, slot)]
struct Row {
    #[key]
    id: u32,
    #[index(cover(score, band))]
    group: u8,
    #[index]
    score: u16,
    #[index(each)]
    tags: Vec<u16>,
    #[index]
    opt: Option<u16>,
    #[unique]
    rank: u32,
    #[unique]
    code: Option<u16>,
    #[unique(cover(score))]
    ticket: u32,
    band: u8,
    shelf: u8,
    slot: u16,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    rows: Table<Row>,
}

type Model = BTreeMap<u32, Row>;

#[derive(Clone, Copy)]
struct Shape {
    scores: u16,
    tags: u16,
    opts: u16,
}

const WIDE: Shape = Shape {
    scores: 1000,
    tags: 200,
    opts: 100,
};

const NARROW: Shape = Shape {
    scores: 30,
    tags: 12,
    opts: 8,
};

fn rows(rng: &mut Rng, ids: &[u32], shape: Shape) -> Model {
    let mut ranks: Vec<u32> = (0..ids.len() as u32).map(|rank| rank * 3).collect();
    let mut tickets = ranks.clone();
    let mut codes: Vec<u16> = (0..ids.len() as u16).collect();
    rng.shuffle(&mut ranks);
    rng.shuffle(&mut tickets);
    rng.shuffle(&mut codes);
    ids.iter()
        .enumerate()
        .map(|(at, &id)| {
            let group = match rng.below(100) {
                0 => 0,
                1..=4 => 1,
                5..=19 => 2,
                _ => 3,
            };
            let row = Row {
                id,
                group,
                score: rng.below(u64::from(shape.scores)) as u16,
                tags: (0..rng.below(4))
                    .map(|_| rng.below(u64::from(shape.tags)) as u16)
                    .collect(),
                opt: (!rng.chance(25)).then(|| rng.below(u64::from(shape.opts)) as u16),
                rank: ranks[at],
                code: (!rng.chance(30)).then_some(codes[at]),
                ticket: tickets[at],
                band: rng.below(4) as u8,
                shelf: (id % 8) as u8,
                slot: (id / 8) as u16,
            };
            (id, row)
        })
        .collect()
}

fn model(rng: &mut Rng, count: u32, shape: Shape) -> Model {
    let ids: Vec<u32> = (0..count).filter(|_| rng.chance(85)).collect();
    rows(rng, &ids, shape)
}

fn table_of(model: &Model) -> Table<Row> {
    let mut table: Table<Row> = Table::new();
    for row in model.values() {
        table.insert(row.clone());
    }
    table
}

fn stored(model: &Model) -> (tempfile::TempDir, Db<Root>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("combine_range.ferro");
    let db: Db<Root> = fast().open(&path).unwrap();
    let table = table_of(model);
    db.update(|root| root.rows = table).unwrap();
    drop(db);
    let db = uncached().open(&path).unwrap();
    (dir, db)
}

#[derive(Clone, Debug)]
enum Leaf {
    Group(u8),
    GroupAfter(u8, u32),
    Score(u16),
    Tag(u16),
    OptSome(u16),
    BandScore(u8, u16),
    ScoreRange(Bound<u16>, Bound<u16>),
    TagRange(u16, u16),
    OptRange(Option<u16>, Option<u16>),
    RankRange(u32, u32),
    CodeRange(u16, u16),
    TicketRange(u32, u32),
    GroupRange(u8, u8),
    Band(u8),
    BandSpan((u8, u16), (u8, u16)),
    Shelf(u8),
    ShelfSpan((u8, u16), (u8, u16)),
    Uncovered(u32, u32),
    Partly(Box<Leaf>, usize, usize),
}

impl Leaf {
    fn holds(&self, row: &Row) -> bool {
        match self {
            Leaf::Group(g) => row.group == *g,
            Leaf::GroupAfter(g, key) => row.group == *g && row.id > *key,
            Leaf::Score(s) => row.score == *s,
            Leaf::Tag(t) => row.tags.contains(t),
            Leaf::OptSome(v) => row.opt == Some(*v),
            Leaf::BandScore(b, s) => row.band == *b && row.score == *s,
            Leaf::ScoreRange(low, high) => (*low, *high).contains(&row.score),
            Leaf::TagRange(low, high) => row.tags.iter().any(|tag| (low..high).contains(&tag)),
            Leaf::OptRange(low, high) => (low..high).contains(&&row.opt),
            Leaf::RankRange(low, high) => (low..high).contains(&&row.rank),
            Leaf::CodeRange(low, high) => {
                row.code.is_some_and(|code| (*low..*high).contains(&code))
            }
            Leaf::TicketRange(low, high) | Leaf::Uncovered(low, high) => {
                (low..high).contains(&&row.ticket)
            }
            Leaf::GroupRange(low, high) => (low..high).contains(&&row.group),
            Leaf::Band(b) => row.band == *b,
            Leaf::BandSpan(low, high) => (low..=high).contains(&&(row.band, row.score)),
            Leaf::Shelf(s) => row.shelf == *s,
            Leaf::ShelfSpan(low, high) => (low..=high).contains(&&(row.shelf, row.slot)),
            Leaf::Partly(..) => unreachable!("a partly read lookup is answered by its keys"),
        }
    }

    fn iter(&self, table: &Table<Row>) -> IndexIter<Row> {
        match self {
            Leaf::ScoreRange(low, high) => table.by_score_range((*low, *high)),
            Leaf::TagRange(low, high) => table.by_tags_range(*low..*high),
            Leaf::OptRange(low, high) => table.by_opt_range(*low..*high),
            Leaf::RankRange(low, high) => table.by_rank_range(*low..*high),
            Leaf::CodeRange(low, high) => table.by_code_range(*low..*high),
            Leaf::Band(b) => table.by_band_score_prefix(b),
            Leaf::BandSpan(low, high) => table.by_band_score_range(*low..=*high),
            Leaf::Shelf(s) => table.by_shelf_slot_prefix(s),
            Leaf::ShelfSpan(low, high) => table.by_shelf_slot_range(*low..=*high),
            other => panic!("{other:?} has no plain lookup on the table"),
        }
    }

    fn partly(&self, table: &Table<Row>) -> IndexIter<Row> {
        let Leaf::Partly(leaf, front, back) = self else {
            return self.iter(table);
        };
        let mut iter = leaf.iter(table);
        for _ in 0..*front {
            iter.next();
        }
        for _ in 0..*back {
            iter.next_back();
        }
        iter
    }

    fn keys(&self, table: &Table<Row>, model: &Model) -> BTreeSet<u32> {
        match self {
            Leaf::Partly(..) => self.partly(table).keys().collect(),
            _ => model
                .values()
                .filter(|row| self.holds(row))
                .map(|row| row.id)
                .collect(),
        }
    }

    fn lookup(&self, table: &Table<Row>) -> Matches<Row> {
        let find = table.find();
        match self {
            Leaf::Group(g) => find.by_group(g).uncovered(),
            Leaf::GroupAfter(g, key) => find.by_group_after(g, key).uncovered(),
            Leaf::Score(s) => find.by_score(s),
            Leaf::Tag(t) => find.by_tags(t),
            Leaf::OptSome(v) => find.by_opt_some(v),
            Leaf::BandScore(b, s) => find.by_band_score(&(*b, *s)),
            Leaf::TicketRange(low, high) => table
                .by_ticket_range(*low..*high)
                .into_matches()
                .uncovered(),
            Leaf::GroupRange(low, high) => {
                table.by_group_range(*low..*high).into_matches().uncovered()
            }
            Leaf::Uncovered(low, high) => table
                .by_ticket_range(*low..*high)
                .uncovered()
                .into_matches(),
            _ => self.partly(table).into_matches(),
        }
    }
}

#[derive(Clone, Debug)]
enum Query {
    Leaf(Leaf),
    And(Box<Query>, Box<Query>),
    Or(Box<Query>, Box<Query>),
}

impl Query {
    fn keys(&self, table: &Table<Row>, model: &Model) -> BTreeSet<u32> {
        match self {
            Query::Leaf(leaf) => leaf.keys(table, model),
            Query::And(left, right) => left
                .keys(table, model)
                .intersection(&right.keys(table, model))
                .copied()
                .collect(),
            Query::Or(left, right) => left
                .keys(table, model)
                .union(&right.keys(table, model))
                .copied()
                .collect(),
        }
    }

    fn lookup(&self, table: &Table<Row>) -> Matches<Row> {
        match self {
            Query::Leaf(leaf) => leaf.lookup(table),
            Query::And(left, right) => left.lookup(table).and(right.lookup(table)),
            Query::Or(left, right) => left.lookup(table).or(right.lookup(table)),
        }
    }
}

fn bound(rng: &mut Rng, value: u16) -> Bound<u16> {
    match rng.below(5) {
        0 => Bound::Unbounded,
        1 | 2 => Bound::Included(value),
        _ => Bound::Excluded(value),
    }
}

fn span<T: Copy + Ord>(a: T, b: T) -> (T, T) {
    (a.min(b), a.max(b))
}

fn small(rng: &mut Rng, ids: u32, shape: Shape) -> Leaf {
    match rng.below(7) {
        0 => Leaf::Group(rng.below(3) as u8),
        1 => Leaf::GroupAfter(rng.below(3) as u8, rng.below(u64::from(ids) + 1) as u32),
        2 => Leaf::Score(rng.below(u64::from(shape.scores)) as u16),
        3 => Leaf::Tag(rng.below(u64::from(shape.tags)) as u16),
        4 => Leaf::OptSome(rng.below(u64::from(shape.opts)) as u16),
        5 => Leaf::BandScore(
            rng.below(4) as u8,
            rng.below(u64::from(shape.scores)) as u16,
        ),
        _ => Leaf::Group(3),
    }
}

fn ranged(rng: &mut Rng, ids: u32, shape: Shape) -> Leaf {
    let score = |rng: &mut Rng| rng.below(u64::from(shape.scores) + 1) as u16;
    let ranks = u64::from(ids) * 3 + 1;
    let leaf = match rng.below(12) {
        0 | 1 => {
            let (low, high) = span(score(rng), score(rng));
            Leaf::ScoreRange(bound(rng, low), bound(rng, high))
        }
        2 => {
            let tag = |rng: &mut Rng| rng.below(u64::from(shape.tags) + 1) as u16;
            let (low, high) = span(tag(rng), tag(rng));
            Leaf::TagRange(low, high)
        }
        3 => {
            let opt = |rng: &mut Rng| {
                (!rng.chance(20)).then(|| rng.below(u64::from(shape.opts) + 1) as u16)
            };
            let (low, high) = span(opt(rng), opt(rng));
            Leaf::OptRange(low, high)
        }
        4 => {
            let (low, high) = span(rng.below(ranks) as u32, rng.below(ranks) as u32);
            Leaf::RankRange(low, high)
        }
        5 => {
            let code = |rng: &mut Rng| rng.below(u64::from(ids) + 1) as u16;
            let (low, high) = span(code(rng), code(rng));
            Leaf::CodeRange(low, high)
        }
        6 => {
            let (low, high) = span(rng.below(ranks) as u32, rng.below(ranks) as u32);
            Leaf::TicketRange(low, high)
        }
        7 => {
            let (low, high) = span(rng.below(5) as u8, rng.below(5) as u8);
            Leaf::GroupRange(low, high)
        }
        8 => {
            let pair = |rng: &mut Rng| (rng.below(4) as u8, score(rng));
            let (low, high) = span(pair(rng), pair(rng));
            if rng.chance(50) {
                Leaf::BandSpan(low, high)
            } else {
                Leaf::Band(low.0)
            }
        }
        9 => {
            let pair =
                |rng: &mut Rng| (rng.below(9) as u8, rng.below(u64::from(ids) / 8 + 2) as u16);
            let (low, high) = span(pair(rng), pair(rng));
            if rng.chance(50) {
                Leaf::ShelfSpan(low, high)
            } else {
                Leaf::Shelf(low.0)
            }
        }
        10 => {
            let (low, high) = span(rng.below(ranks) as u32, rng.below(ranks) as u32);
            Leaf::Uncovered(low, high)
        }
        _ => {
            let (low, high) = span(score(rng), score(rng));
            Leaf::ScoreRange(Bound::Included(low), Bound::Excluded(high))
        }
    };
    let plain = !matches!(
        leaf,
        Leaf::TicketRange(..) | Leaf::GroupRange(..) | Leaf::Uncovered(..)
    );
    if plain && rng.chance(20) {
        Leaf::Partly(
            Box::new(leaf),
            rng.below(40) as usize,
            rng.below(40) as usize,
        )
    } else {
        leaf
    }
}

fn query(rng: &mut Rng, ids: u32, shape: Shape, depth: u32) -> Query {
    if depth == 0 || rng.chance(25) {
        let leaf = if rng.chance(50) {
            ranged(rng, ids, shape)
        } else {
            small(rng, ids, shape)
        };
        return Query::Leaf(leaf);
    }
    let left = Box::new(query(rng, ids, shape, depth - 1));
    let right = Box::new(query(rng, ids, shape, depth - 1));
    if rng.chance(65) {
        Query::And(left, right)
    } else {
        Query::Or(left, right)
    }
}

fn led(rng: &mut Rng, ids: u32, shape: Shape) -> Query {
    let lead = Box::new(Query::Leaf(small(rng, ids, shape)));
    let range = Box::new(Query::Leaf(ranged(rng, ids, shape)));
    if rng.chance(80) {
        Query::And(lead, range)
    } else {
        Query::And(range, lead)
    }
}

fn zigzag<I: DoubleEndedIterator<Item = u32>>(mut iter: I, rng: &mut Rng) -> Vec<u32> {
    let (mut front, mut back) = (Vec::new(), Vec::new());
    loop {
        let next = if rng.chance(50) {
            iter.next().map(|key| front.push(key))
        } else {
            iter.next_back().map(|key| back.push(key))
        };
        if next.is_none() {
            break;
        }
    }
    front.extend(back.iter().rev());
    front
}

fn agree(table: &Table<Row>, model: &Model, query: &Query, rng: &mut Rng) {
    let wanted: Vec<u32> = query.keys(table, model).into_iter().collect();
    let what = format!("{query:?}");
    let matches = query.lookup(table);
    assert!(
        matches
            .size_hint()
            .1
            .is_some_and(|upper| upper >= wanted.len()),
        "size hint of {what}"
    );
    let rows: Vec<Row> = matches.map(|row| (*row).clone()).collect();
    let expected: Vec<Row> = wanted.iter().map(|id| model[id].clone()).collect();
    assert_eq!(rows, expected, "rows of {what}");
    let back: Vec<u32> = query.lookup(table).rev().map(|row| row.id).collect();
    let reversed: Vec<u32> = wanted.iter().rev().copied().collect();
    assert_eq!(back, reversed, "rows from the back of {what}");
    let keys: Vec<u32> = query.lookup(table).keys().collect();
    assert_eq!(keys, wanted, "keys of {what}");
    let keys_back: Vec<u32> = query.lookup(table).keys().rev().collect();
    assert_eq!(keys_back, reversed, "keys from the back of {what}");
    assert_eq!(
        zigzag(query.lookup(table).keys(), rng),
        wanted,
        "keys from both ends of {what}"
    );
    assert_eq!(
        zigzag(query.lookup(table).map(|row| row.id), rng),
        wanted,
        "rows from both ends of {what}"
    );
    let mut lent = Vec::new();
    let mut matches = query.lookup(table);
    while let Some(row) = matches.next_ref() {
        lent.push(row.id);
    }
    assert_eq!(lent, wanted, "lent rows of {what}");
    let few: Vec<u32> = query.lookup(table).keys().take(3).collect();
    assert_eq!(few, wanted.iter().take(3).copied().collect::<Vec<_>>());
}

fn covered_agree(table: &Table<Row>, model: &Model, group: u8, rest: &Query, rng: &mut Rng) {
    let both: BTreeSet<u32> = rest
        .keys(table, model)
        .into_iter()
        .filter(|id| model[id].group == group)
        .collect();
    let fields = |ids: &BTreeSet<u32>| -> Vec<(u32, (u16, u8))> {
        ids.iter()
            .map(|id| (*id, (model[id].score, model[id].band)))
            .collect()
    };
    let wanted = fields(&both);
    let what = format!("group {group} and {rest:?}");
    let find = table.find();
    let covered: Vec<(u32, (u16, u8))> = find
        .by_group(&group)
        .and(rest.lookup(table))
        .covered()
        .collect();
    assert_eq!(covered, wanted, "covered {what}");
    let mut back: Vec<(u32, (u16, u8))> = find
        .by_group(&group)
        .and(rest.lookup(table))
        .covered()
        .rev()
        .collect();
    back.reverse();
    assert_eq!(back, wanted, "covered from the back {what}");
    let listed: Vec<(u32, (u16, u8))> = table
        .by_group(&group)
        .into_matches()
        .and(rest.lookup(table))
        .covered()
        .collect();
    assert_eq!(listed, wanted, "covered and listed {what}");

    let high = group + 1 + rng.below(2) as u8;
    let either: BTreeSet<u32> = model
        .values()
        .filter(|row| row.group == group || (group + 1..high).contains(&row.group))
        .map(|row| row.id)
        .filter(|id| rest.keys(table, model).contains(id))
        .collect();
    let covered: Vec<(u32, (u16, u8))> = find
        .by_group(&group)
        .or(table.by_group_range(group + 1..high))
        .and(rest.lookup(table))
        .covered()
        .collect();
    assert_eq!(covered, fields(&either), "covered either {what}");
}

fn started_agree(table: &Table<Row>, model: &Model, rng: &mut Rng, ids: u32, shape: Shape) {
    let first = led(rng, ids, shape);
    let then = Query::Leaf(small(rng, ids, shape));
    let wanted: Vec<u32> = first.keys(table, model).into_iter().collect();
    let what = format!("{first:?}, then {then:?}");
    let mut started = first.lookup(table);
    let front: Vec<u32> = (&mut started).take(2).map(|row| row.id).collect();
    let back: Vec<u32> = (&mut started).rev().take(2).map(|row| row.id).collect();
    let rest: BTreeSet<u32> = wanted
        .iter()
        .copied()
        .filter(|id| front.last().is_none_or(|low| id > low))
        .filter(|id| back.last().is_none_or(|high| id < high))
        .collect();
    let later = then.keys(table, model);
    let and: Vec<u32> = rest.intersection(&later).copied().collect();
    let or: Vec<u32> = rest.union(&later).copied().collect();
    let mut again = first.lookup(table);
    (&mut again).take(2).for_each(drop);
    (&mut again).rev().take(2).for_each(drop);
    assert_eq!(
        started.and(then.lookup(table)).keys().collect::<Vec<_>>(),
        and,
        "started and {what}"
    );
    assert_eq!(
        again.or(then.lookup(table)).keys().collect::<Vec<_>>(),
        or,
        "started or {what}"
    );

    let group = rng.below(3) as u8;
    let high = group + 1 + rng.below(2) as u8;
    let score = Leaf::Score(rng.below(u64::from(shape.scores)) as u16);
    let find = table.find();
    let mut covering = find
        .by_group(&group)
        .or(table.by_group_range(group + 1..high))
        .and(score.lookup(table));
    let first = covering.next().map(|row| row.id);
    let wanted: Vec<(u32, (u16, u8))> = model
        .values()
        .filter(|row| row.group == group || (group + 1..high).contains(&row.group))
        .filter(|row| score.holds(row) && row.group != 0)
        .filter(|row| first.is_none_or(|first| row.id > first))
        .map(|row| (row.id, (row.score, row.band)))
        .collect();
    let covered: Vec<(u32, (u16, u8))> = covering
        .and(
            find.by_group(&0)
                .or(find.by_group(&1))
                .or(find.by_group(&2))
                .or(find.by_group(&3)),
        )
        .and(Query::Leaf(Leaf::GroupRange(1, 4)).lookup(table))
        .covered()
        .collect();
    assert_eq!(
        covered, wanted,
        "started covering group {group}..{high} and {score:?}"
    );
}

fn check(table: &Table<Row>, model: &Model, seed: u64, queries: usize, shape: Shape) {
    let ids = model.keys().next_back().map_or(1, |id| id + 1);
    let mut rng = Rng::new(seed);
    for _ in 0..queries {
        let query = if rng.chance(50) {
            led(&mut rng, ids, shape)
        } else {
            query(&mut rng, ids, shape, 3)
        };
        agree(table, model, &query, &mut rng);
    }
    for group in 0..3 {
        let rest = if rng.chance(50) {
            Query::Leaf(ranged(&mut rng, ids, shape))
        } else {
            query(&mut rng, ids, shape, 2)
        };
        covered_agree(table, model, group, &rest, &mut rng);
    }
    for _ in 0..4 {
        started_agree(table, model, &mut rng, ids, shape);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(48)))]

    #[test]
    fn ranges_combined_with_smaller_lookups_match_a_filter(
        count in 0u32..700,
        seed in any::<u64>(),
        narrow in any::<bool>(),
    ) {
        let shape = if narrow { NARROW } else { WIDE };
        let mut rng = Rng::new(seed);
        let model = model(&mut rng, count, shape);
        check(&table_of(&model), &model, seed, 30, shape);
        let (_dir, db) = stored(&model);
        check(&db.read().rows, &model, seed ^ 1, 30, shape);
    }
}

#[test]
fn large_ranges_combined_with_few_keys_match_a_filter() {
    let mut rng = Rng::new(17);
    let model = model(&mut rng, 6_000, WIDE);
    let (_dir, db) = stored(&model);
    let read = db.read();
    check(&read.rows, &model, 23, 300, WIDE);
    check(&table_of(&model), &model, 29, 150, WIDE);
}

fn every_score_range(shape: Shape) -> Vec<(Bound<u16>, Bound<u16>)> {
    let mut out = vec![(Bound::Unbounded, Bound::Unbounded)];
    for a in 0..=shape.scores {
        out.push((Bound::Included(a), Bound::Unbounded));
        out.push((Bound::Excluded(a), Bound::Unbounded));
        out.push((Bound::Unbounded, Bound::Included(a)));
        out.push((Bound::Unbounded, Bound::Excluded(a)));
        for b in a..=shape.scores {
            out.push((Bound::Included(a), Bound::Excluded(b)));
            out.push((Bound::Included(a), Bound::Included(b)));
            out.push((Bound::Excluded(a), Bound::Included(b)));
        }
    }
    out
}

#[test]
fn every_score_range_against_each_lead_from_both_ends() {
    let mut rng = Rng::new(41);
    let model = model(&mut rng, 900, NARROW);
    let (_dir, db) = stored(&model);
    let read = db.read();
    let leads = [
        Leaf::Group(0),
        Leaf::Group(1),
        Leaf::GroupAfter(2, 450),
        Leaf::Score(3),
        Leaf::Tag(5),
        Leaf::BandScore(1, 7),
    ];
    for table in [&read.rows, &table_of(&model)] {
        let find = table.find();
        for (low, high) in every_score_range(NARROW) {
            let ranged: Vec<u32> = model
                .values()
                .filter(|row| (low, high).contains(&row.score))
                .map(|row| row.id)
                .collect();
            for lead in &leads {
                let wanted: Vec<u32> = ranged
                    .iter()
                    .copied()
                    .filter(|id| lead.holds(&model[id]))
                    .collect();
                let what = format!("{lead:?} and {low:?}..{high:?}");
                let keys: Vec<u32> = lead
                    .lookup(table)
                    .and(table.by_score_range((low, high)))
                    .keys()
                    .collect();
                assert_eq!(keys, wanted, "{what}");
                let mut back: Vec<u32> = lead
                    .lookup(table)
                    .and(table.by_score_range((low, high)))
                    .rev()
                    .map(|row| row.id)
                    .collect();
                back.reverse();
                assert_eq!(back, wanted, "from the back, {what}");
                let both = zigzag(
                    lead.lookup(table)
                        .and(table.by_score_range((low, high)))
                        .keys(),
                    &mut rng,
                );
                assert_eq!(both, wanted, "from both ends, {what}");
                let either: Vec<u32> = model
                    .values()
                    .filter(|row| {
                        lead.holds(row) && (low, high).contains(&row.score)
                            || row.group == 3 && row.band == 0
                    })
                    .map(|row| row.id)
                    .collect();
                let or: Vec<u32> = lead
                    .lookup(table)
                    .and(table.by_score_range((low, high)))
                    .or(find.by_group(&3).uncovered().and(
                        find.by_band_score(&(0, 0))
                            .or(table.by_band_score_prefix(&0)),
                    ))
                    .keys()
                    .collect();
                assert_eq!(or, either, "or, {what}");
            }
        }
    }
}

#[test]
fn a_combination_with_a_range_reads_the_table_as_it_was_built() {
    let mut rng = Rng::new(53);
    let mut model = model(&mut rng, 3_000, WIDE);
    let mut table = table_of(&model);
    let queries = [
        Query::And(
            Box::new(Query::Leaf(Leaf::Group(0))),
            Box::new(Query::Leaf(Leaf::ScoreRange(
                Bound::Included(100),
                Bound::Excluded(900),
            ))),
        ),
        Query::And(
            Box::new(Query::Leaf(Leaf::Group(1))),
            Box::new(Query::Leaf(Leaf::RankRange(0, 6_000))),
        ),
        Query::And(
            Box::new(Query::Leaf(Leaf::Score(17))),
            Box::new(Query::Leaf(Leaf::TagRange(0, 150))),
        ),
        Query::And(
            Box::new(Query::Leaf(Leaf::Group(0))),
            Box::new(Query::Leaf(Leaf::Band(2))),
        ),
    ];
    let before: Vec<Vec<u32>> = queries
        .iter()
        .map(|query| query.keys(&table, &model).into_iter().collect())
        .collect();
    let held: Vec<Matches<Row>> = queries.iter().map(|query| query.lookup(&table)).collect();
    let mut early: Vec<Matches<Row>> = queries.iter().map(|query| query.lookup(&table)).collect();
    let firsts: Vec<Option<u32>> = early
        .iter_mut()
        .map(|matches| matches.next().map(|row| row.id))
        .collect();
    let ids: Vec<u32> = model.keys().copied().collect();
    for id in ids.iter().step_by(3) {
        table.update(id, |row| {
            row.score = (row.score + 500) % 1000;
            row.tags = vec![row.tags.first().map_or(0, |tag| tag + 100)];
            row.band = (row.band + 1) % 4;
            row.group = 0;
        });
        let row = model.get_mut(id).unwrap();
        row.score = (row.score + 500) % 1000;
        row.tags = vec![row.tags.first().map_or(0, |tag| tag + 100)];
        row.band = (row.band + 1) % 4;
        row.group = 0;
    }
    for id in ids.iter().step_by(5) {
        table.remove(id);
        model.remove(id);
    }
    let added = rows(&mut rng, &[10_000, 10_001, 10_002], WIDE);
    for mut row in added.into_values() {
        row.group = 0;
        row.score = 500;
        row.rank += 50_000;
        row.ticket += 50_000;
        row.code = None;
        table.insert(row.clone());
        model.insert(row.id, row);
    }
    for (((matches, early), first), wanted) in held.into_iter().zip(early).zip(firsts).zip(&before)
    {
        assert_eq!(&matches.keys().collect::<Vec<_>>(), wanted);
        let mut rest: Vec<u32> = first.into_iter().collect();
        rest.extend(early.map(|row| row.id));
        assert_eq!(&rest, wanted);
    }
    for query in &queries {
        agree(&table, &model, query, &mut rng);
    }
}

#[test]
fn a_range_inside_a_commit_sees_the_commit_as_it_was_when_built() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Root> = fast().open(dir.path().join("commit.ferro")).unwrap();
    let mut rng = Rng::new(61);
    let mut model = model(&mut rng, 2_000, WIDE);
    let table = table_of(&model);
    db.update(|root| root.rows = table).unwrap();
    let snapshot = db.read();
    let before = model.clone();
    let query = Query::And(
        Box::new(Query::Leaf(Leaf::Group(1))),
        Box::new(Query::Leaf(Leaf::ScoreRange(
            Bound::Included(0),
            Bound::Excluded(700),
        ))),
    );
    let held = query.lookup(&snapshot.rows);
    let edited = db
        .update(|root| {
            let early = query.lookup(&root.rows);
            let ids: Vec<u32> = root.rows.keys().collect();
            for id in ids.iter().step_by(2) {
                root.rows.update(id, |row| row.score = 999 - row.score);
            }
            (
                early.keys().collect::<Vec<_>>(),
                query.lookup(&root.rows).keys().collect::<Vec<_>>(),
            )
        })
        .unwrap();
    let ids: Vec<u32> = model.keys().copied().collect();
    for id in ids.iter().step_by(2) {
        let row = model.get_mut(id).unwrap();
        row.score = 999 - row.score;
    }
    let wanted = |model: &Model| -> Vec<u32> {
        model
            .values()
            .filter(|row| row.group == 1 && row.score < 700)
            .map(|row| row.id)
            .collect()
    };
    assert_eq!(edited.0, wanted(&before));
    assert_eq!(edited.1, wanted(&model));
    assert_eq!(held.keys().collect::<Vec<_>>(), wanted(&before));
    agree(&db.read().rows, &model, &query, &mut rng);
    agree(&snapshot.rows, &before, &query, &mut rng);
}

#[test]
fn a_range_from_another_version_of_the_table_answers_for_its_own() {
    let mut rng = Rng::new(67);
    let model = model(&mut rng, 2_000, WIDE);
    let old = table_of(&model);
    let mut new = old.clone();
    let mut changed = model.clone();
    for row in changed.values_mut() {
        row.score = 999 - row.score;
        new.insert(row.clone());
    }
    let lead: Vec<u32> = model
        .values()
        .filter(|row| row.group == 0)
        .map(|row| row.id)
        .collect();
    for (range_table, range_model) in [(&old, &model), (&new, &changed)] {
        let wanted: Vec<u32> = lead
            .iter()
            .copied()
            .filter(|id| range_model[id].score < 600)
            .collect();
        let keys: Vec<u32> = old
            .find()
            .by_group(&0)
            .and(range_table.by_score_range(..600))
            .keys()
            .collect();
        assert_eq!(keys, wanted);
        let rows: Vec<Row> = new
            .find()
            .by_group(&0)
            .and(range_table.by_score_range(..600))
            .map(|row| (*row).clone())
            .collect();
        let expected: Vec<Row> = wanted.iter().map(|id| changed[id].clone()).collect();
        assert_eq!(rows, expected);
    }
}
