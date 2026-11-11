mod common;

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Bound, Range, RangeBounds};

use common::{Rng, fast, uncached};
use ferro::table::{IndexEach, IndexIter, IntoMatches, Matches, Read};
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
    other: Table<Row>,
}

type Model = BTreeMap<u32, Row>;

type Version<'a> = (&'a Table<Row>, &'a Model);

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
    stored_pair(&table_of(model), &Table::new())
}

fn stored_pair(rows: &Table<Row>, other: &Table<Row>) -> (tempfile::TempDir, Db<Root>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("combine_range.ferro");
    let db: Db<Root> = fast().open(&path).unwrap();
    db.update(|root| {
        root.rows = rows.clone();
        root.other = other.clone();
    })
    .unwrap();
    drop(db);
    let db = uncached().open(&path).unwrap();
    (dir, db)
}

fn edited(rng: &mut Rng, table: &Table<Row>, model: &Model, shape: Shape) -> (Table<Row>, Model) {
    let mut table = table.clone();
    let mut model = model.clone();
    let ids: Vec<u32> = model.keys().copied().collect();
    for id in &ids {
        match rng.below(10) {
            0..=2 => {
                let row = model.get_mut(id).unwrap();
                row.group = rng.below(4) as u8;
                row.score = rng.below(u64::from(shape.scores)) as u16;
                row.tags = vec![rng.below(u64::from(shape.tags)) as u16];
                row.opt = (!rng.chance(25)).then(|| rng.below(u64::from(shape.opts)) as u16);
                row.band = rng.below(4) as u8;
                table.insert(row.clone());
            }
            3 => {
                table.remove(id);
                model.remove(id);
            }
            _ => {}
        }
    }
    let last = ids.last().map_or(0, |id| id + 1);
    let added: Vec<u32> = (last..last + 1 + ids.len() as u32 / 8).collect();
    for mut row in rows(rng, &added, shape).into_values() {
        row.rank += 10 * last + 10;
        row.ticket += 10 * last + 10;
        row.code = None;
        table.insert(row.clone());
        model.insert(row.id, row);
    }
    (table, model)
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

    fn partly(&self, table: &Table<Row>) -> Matches<Row> {
        let Leaf::Partly(leaf, ..) = self else {
            return self.iter(table).into_matches();
        };
        match &**leaf {
            Leaf::TicketRange(low, high) => read_partly(table.by_ticket_range(*low..*high), self)
                .into_matches()
                .uncovered(),
            Leaf::GroupRange(low, high) => read_partly(table.by_group_range(*low..*high), self)
                .into_matches()
                .uncovered(),
            Leaf::Uncovered(low, high) => {
                read_partly(table.by_ticket_range(*low..*high).uncovered(), self).into_matches()
            }
            _ => read_partly(leaf.iter(table), self).into_matches(),
        }
    }

    fn partly_keys(&self, table: &Table<Row>) -> BTreeSet<u32> {
        let Leaf::Partly(leaf, ..) = self else {
            return self.iter(table).keys().collect();
        };
        match &**leaf {
            Leaf::TicketRange(low, high) => read_partly(table.by_ticket_range(*low..*high), self)
                .keys()
                .collect(),
            Leaf::GroupRange(low, high) => read_partly(table.by_group_range(*low..*high), self)
                .keys()
                .collect(),
            Leaf::Uncovered(low, high) => {
                read_partly(table.by_ticket_range(*low..*high).uncovered(), self)
                    .keys()
                    .collect()
            }
            _ => read_partly(leaf.iter(table), self).keys().collect(),
        }
    }

    fn keys(&self, table: &Table<Row>, model: &Model) -> BTreeSet<u32> {
        match self {
            Leaf::Partly(..) => self.partly_keys(table),
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
            _ => self.partly(table),
        }
    }
}

fn read_partly<C>(mut iter: IndexIter<Row, C>, leaf: &Leaf) -> IndexIter<Row, C> {
    let Leaf::Partly(_, front, back) = leaf else {
        return iter;
    };
    for _ in 0..*front {
        iter.next();
    }
    for _ in 0..*back {
        iter.next_back();
    }
    iter
}

#[derive(Clone, Debug)]
enum Query {
    Leaf(Leaf),
    And(Box<Query>, Box<Query>),
    Or(Box<Query>, Box<Query>),
    On(usize, Box<Query>),
}

impl Query {
    fn rows<'a>(&self, versions: &[Version<'a>], on: usize) -> BTreeMap<u32, &'a Row> {
        match self {
            Query::Leaf(leaf) => {
                let (table, model) = versions[on];
                leaf.keys(table, model)
                    .into_iter()
                    .map(|id| (id, &model[&id]))
                    .collect()
            }
            Query::And(left, right) => {
                let right = right.rows(versions, on);
                let mut rows = left.rows(versions, on);
                rows.retain(|id, _| right.contains_key(id));
                rows
            }
            Query::Or(left, right) => {
                let mut rows = right.rows(versions, on);
                rows.extend(left.rows(versions, on));
                rows
            }
            Query::On(at, query) => query.rows(versions, *at),
        }
    }

    fn keys(&self, table: &Table<Row>, model: &Model) -> BTreeSet<u32> {
        self.rows(&[(table, model)], 0).into_keys().collect()
    }

    fn links(&self) -> Vec<&Query> {
        match self {
            Query::And(left, right) => {
                let mut links = left.links();
                links.push(right);
                links
            }
            other => vec![other],
        }
    }

    fn lookup_in(&self, tables: &[&Table<Row>], on: usize) -> Matches<Row> {
        match self {
            Query::Leaf(leaf) => leaf.lookup(tables[on]),
            Query::And(left, right) => left.lookup_in(tables, on).and(right.lookup_in(tables, on)),
            Query::Or(left, right) => left.lookup_in(tables, on).or(right.lookup_in(tables, on)),
            Query::On(at, query) => query.lookup_in(tables, *at),
        }
    }

    fn lookup(&self, table: &Table<Row>) -> Matches<Row> {
        self.lookup_in(&[table], 0)
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
    if rng.chance(20) {
        Leaf::Partly(
            Box::new(leaf),
            rng.below(40) as usize,
            rng.below(40) as usize,
        )
    } else {
        leaf
    }
}

fn on(rng: &mut Rng, versions: usize, query: Query) -> Query {
    if versions > 1 && rng.chance(40) {
        Query::On(rng.below(versions as u64) as usize, Box::new(query))
    } else {
        query
    }
}

fn query(rng: &mut Rng, ids: u32, shape: Shape, depth: u32, versions: usize) -> Query {
    if depth == 0 || rng.chance(25) {
        let leaf = if rng.chance(50) {
            ranged(rng, ids, shape)
        } else {
            small(rng, ids, shape)
        };
        return on(rng, versions, Query::Leaf(leaf));
    }
    let left = Box::new(query(rng, ids, shape, depth - 1, versions));
    let right = Box::new(query(rng, ids, shape, depth - 1, versions));
    let query = if rng.chance(65) {
        Query::And(left, right)
    } else {
        Query::Or(left, right)
    };
    on(rng, versions, query)
}

fn led(rng: &mut Rng, ids: u32, shape: Shape, versions: usize) -> Query {
    let lead = Query::Leaf(small(rng, ids, shape));
    let lead = Box::new(on(rng, versions, lead));
    let range = Query::Leaf(ranged(rng, ids, shape));
    let range = Box::new(on(rng, versions, range));
    if rng.chance(80) {
        Query::And(lead, range)
    } else {
        Query::And(range, lead)
    }
}

fn chain(rng: &mut Rng, ids: u32, shape: Shape, versions: usize) -> Query {
    let sides = 3 + rng.below(8);
    let leaf = |rng: &mut Rng| {
        let leaf = if rng.chance(50) {
            Query::Leaf(ranged(rng, ids, shape))
        } else {
            Query::Leaf(small(rng, ids, shape))
        };
        on(rng, versions, leaf)
    };
    let mut query = leaf(rng);
    for _ in 1..sides {
        let side = if rng.chance(15) {
            Query::Or(Box::new(leaf(rng)), Box::new(leaf(rng)))
        } else {
            leaf(rng)
        };
        query = Query::And(Box::new(query), Box::new(side));
    }
    query
}

fn zigzag<T, I: DoubleEndedIterator<Item = T>>(mut iter: I, rng: &mut Rng) -> Vec<T> {
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
    front.extend(back.into_iter().rev());
    front
}

fn agree(table: &Table<Row>, model: &Model, query: &Query, rng: &mut Rng) {
    agree_in(&[(table, model)], query, rng);
}

fn tables_of<'a>(versions: &[Version<'a>]) -> Vec<&'a Table<Row>> {
    versions.iter().map(|(table, _)| *table).collect()
}

fn agree_in(versions: &[Version<'_>], query: &Query, rng: &mut Rng) {
    let tables = tables_of(versions);
    let lookup = || query.lookup_in(&tables, 0);
    let rows = query.rows(versions, 0);
    let wanted: Vec<u32> = rows.keys().copied().collect();
    let expected: Vec<Row> = rows.values().map(|row| (*row).clone()).collect();
    let what = format!("{query:?}");
    let matches = lookup();
    assert!(
        matches
            .size_hint()
            .1
            .is_some_and(|upper| upper >= wanted.len()),
        "size hint of {what}"
    );
    let got: Vec<Row> = matches.map(|row| (*row).clone()).collect();
    assert_eq!(got, expected, "rows of {what}");
    let mut back: Vec<Row> = lookup().rev().map(|row| (*row).clone()).collect();
    back.reverse();
    assert_eq!(back, expected, "rows from the back of {what}");
    let reversed: Vec<u32> = wanted.iter().rev().copied().collect();
    let keys: Vec<u32> = lookup().keys().collect();
    assert_eq!(keys, wanted, "keys of {what}");
    let keys_back: Vec<u32> = lookup().keys().rev().collect();
    assert_eq!(keys_back, reversed, "keys from the back of {what}");
    assert_eq!(
        zigzag(lookup().keys(), rng),
        wanted,
        "keys from both ends of {what}"
    );
    assert_eq!(
        zigzag(lookup().map(|row| (*row).clone()), rng),
        expected,
        "rows from both ends of {what}"
    );
    let mut lent = Vec::new();
    let mut matches = lookup();
    while let Some(row) = matches.next_ref() {
        lent.push(row.clone());
    }
    assert_eq!(lent, expected, "lent rows of {what}");
    let mut visited = Vec::new();
    lookup().visit(|row| visited.push(row.clone()));
    assert_eq!(visited, expected, "visited rows of {what}");
    let few: Vec<u32> = lookup().keys().take(3).collect();
    assert_eq!(few, wanted.iter().take(3).copied().collect::<Vec<_>>());
    let first: Vec<Row> = lookup().take(3).map(|row| (*row).clone()).collect();
    assert_eq!(first, expected.iter().take(3).cloned().collect::<Vec<_>>());
}

fn fields(rows: &BTreeMap<u32, &Row>) -> Vec<(u32, (u16, u8))> {
    rows.iter()
        .map(|(id, row)| (*id, (row.score, row.band)))
        .collect()
}

fn grouped<'a>(
    model: &'a Model,
    groups: Range<u8>,
    keys: &BTreeSet<u32>,
) -> BTreeMap<u32, &'a Row> {
    model
        .values()
        .filter(|row| groups.contains(&row.group) && keys.contains(&row.id))
        .map(|row| (row.id, row))
        .collect()
}

fn covered_agree(versions: &[Version<'_>], group: u8, rest: &Query, rng: &mut Rng) {
    let tables = tables_of(versions);
    let (table, model) = versions[0];
    let at = rng.below(versions.len() as u64) as usize;
    let rest_keys: BTreeSet<u32> = rest.rows(versions, at).into_keys().collect();
    let wanted = fields(&grouped(model, group..group + 1, &rest_keys));
    let what = format!("group {group} and {rest:?} on {at}");
    let find = table.find();
    let covered: Vec<(u32, (u16, u8))> = find
        .by_group(&group)
        .and(rest.lookup_in(&tables, at))
        .covered()
        .collect();
    assert_eq!(covered, wanted, "covered {what}");
    let mut back: Vec<(u32, (u16, u8))> = find
        .by_group(&group)
        .and(rest.lookup_in(&tables, at))
        .covered()
        .rev()
        .collect();
    back.reverse();
    assert_eq!(back, wanted, "covered from the back {what}");
    let listed: Vec<(u32, (u16, u8))> = table
        .by_group(&group)
        .into_matches()
        .and(rest.lookup_in(&tables, at))
        .covered()
        .collect();
    assert_eq!(listed, wanted, "covered and listed {what}");
    let chained = || {
        rest.links()
            .into_iter()
            .fold(find.by_group(&group), |all, link| {
                all.and(link.lookup_in(&tables, at))
            })
    };
    let covered: Vec<(u32, (u16, u8))> = chained().covered().collect();
    assert_eq!(covered, wanted, "covered chain {what}");
    let rows: Vec<(u32, (u16, u8))> = chained()
        .rev()
        .map(|row| (row.id, (row.score, row.band)))
        .collect();
    let reversed: Vec<(u32, (u16, u8))> = wanted.iter().rev().copied().collect();
    assert_eq!(rows, reversed, "rows of the chain from the back {what}");

    let high = group + 1 + rng.below(2) as u8;
    let other = rng.below(versions.len() as u64) as usize;
    let (other_table, other_model) = versions[other];
    let mut either = grouped(other_model, group + 1..high, &rest_keys);
    either.extend(grouped(model, group..group + 1, &rest_keys));
    let either_lookup = || {
        find.by_group(&group)
            .or(other_table.by_group_range(group + 1..high))
            .and(rest.lookup_in(&tables, at))
    };
    let what = format!("{what}, or groups {}..{high} of {other}", group + 1);
    let covered: Vec<(u32, (u16, u8))> = either_lookup().covered().collect();
    assert_eq!(covered, fields(&either), "covered either {what}");
    let rows: Vec<(u32, (u16, u8))> = either_lookup()
        .map(|row| (row.id, (row.score, row.band)))
        .collect();
    assert_eq!(rows, fields(&either), "rows of either {what}");
}

fn rows_of(rows: &BTreeMap<u32, &Row>) -> Vec<Row> {
    rows.values().map(|row| (*row).clone()).collect()
}

fn started_agree(versions: &[Version<'_>], rng: &mut Rng, ids: u32, shape: Shape) {
    let tables = tables_of(versions);
    let (table, model) = versions[0];
    let count = versions.len();
    let first = if count > 1 && rng.chance(60) {
        let left = led(rng, ids, shape, count);
        let right = led(rng, ids, shape, count);
        Query::Or(
            Box::new(on(rng, count, left)),
            Box::new(on(rng, count, right)),
        )
    } else {
        led(rng, ids, shape, count)
    };
    let then = Query::Leaf(small(rng, ids, shape));
    let then = on(rng, count, then);
    let wanted = first.rows(versions, 0);
    let what = format!("{first:?}, then {then:?}");
    let started = || {
        let mut started = first.lookup_in(&tables, 0);
        let front: Vec<Row> = (&mut started).take(2).map(|row| (*row).clone()).collect();
        let back: Vec<Row> = (&mut started)
            .rev()
            .take(2)
            .map(|row| (*row).clone())
            .collect();
        (started, front, back)
    };
    let (_, front, back) = started();
    let mut ends = front.clone();
    ends.extend(back.iter().cloned());
    let mut expected_ends: Vec<Row> = rows_of(&wanted).into_iter().take(front.len()).collect();
    expected_ends.extend(rows_of(&wanted).into_iter().rev().take(back.len()));
    assert_eq!(ends, expected_ends, "rows read first from {what}");
    let mut rest = wanted.clone();
    rest.retain(|id, _| front.last().is_none_or(|low| *id > low.id));
    rest.retain(|id, _| back.last().is_none_or(|high| *id < high.id));
    let later = then.rows(versions, 0);
    let mut and = rest.clone();
    and.retain(|id, _| later.contains_key(id));
    let mut or = later.clone();
    or.extend(rest.iter().map(|(id, row)| (*id, *row)));
    let both = || started().0.and(then.lookup_in(&tables, 0));
    let either = || started().0.or(then.lookup_in(&tables, 0));
    assert_eq!(
        both().keys().collect::<Vec<_>>(),
        and.keys().copied().collect::<Vec<_>>(),
        "started and {what}"
    );
    assert_eq!(
        either().keys().collect::<Vec<_>>(),
        or.keys().copied().collect::<Vec<_>>(),
        "started or {what}"
    );
    let read: Vec<Row> = both().map(|row| (*row).clone()).collect();
    assert_eq!(read, rows_of(&and), "rows of started and {what}");
    let mut read: Vec<Row> = either().rev().map(|row| (*row).clone()).collect();
    read.reverse();
    assert_eq!(
        read,
        rows_of(&or),
        "rows of started or from the back {what}"
    );
    let mut lent = Vec::new();
    either().visit(|row| lent.push(row.clone()));
    assert_eq!(lent, rows_of(&or), "lent rows of started or {what}");

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

    let low = rng.below(u64::from(shape.scores)) as u16;
    let scores = low..low + 1 + rng.below(3) as u16;
    let mut lead = find.by_score(&scores.start);
    for value in scores.start + 1..scores.end {
        lead = lead.or(find.by_score(&value));
    }
    let mut direct = find
        .by_group(&group)
        .or(table.by_group_range(group + 1..high))
        .and(lead);
    let front: Vec<u32> = (&mut direct)
        .take(rng.below(3) as usize)
        .map(|row| row.id)
        .collect();
    let back: Vec<u32> = (&mut direct)
        .rev()
        .take(rng.below(3) as usize)
        .map(|row| row.id)
        .collect();
    let rest: Vec<(u32, (u16, u8))> = direct.covered().collect();
    let all: Vec<u32> = model
        .values()
        .filter(|row| row.group == group || (group + 1..high).contains(&row.group))
        .filter(|row| scores.contains(&row.score))
        .map(|row| row.id)
        .collect();
    let mut read = front.clone();
    read.extend(rest.iter().map(|(id, _)| *id));
    read.extend(back.iter().rev());
    let what = format!("covered after reading group {group}..{high} and scores {scores:?}");
    assert_eq!(read, all, "{what}");
    for (id, fields) in &rest {
        assert_eq!(*fields, (model[id].score, model[id].band), "{what}");
    }
}

fn check(table: &Table<Row>, model: &Model, seed: u64, queries: usize, shape: Shape) {
    check_in(&[(table, model)], seed, queries, shape);
}

fn ids_in(versions: &[Version<'_>]) -> u32 {
    versions
        .iter()
        .filter_map(|(_, model)| model.keys().next_back())
        .max()
        .map_or(1, |id| id + 1)
}

fn check_deep_in(versions: &[Version<'_>], seed: u64, queries: usize, shape: Shape) {
    let (count, ids) = (versions.len(), ids_in(versions));
    let mut rng = Rng::new(seed);
    for _ in 0..queries {
        let query = query(&mut rng, ids, shape, 4, count);
        agree_in(versions, &query, &mut rng);
    }
}

fn check_in(versions: &[Version<'_>], seed: u64, queries: usize, shape: Shape) {
    let (count, ids) = (versions.len(), ids_in(versions));
    let mut rng = Rng::new(seed);
    for _ in 0..queries {
        let query = match rng.below(5) {
            0 | 1 => led(&mut rng, ids, shape, count),
            2 => chain(&mut rng, ids, shape, count),
            _ => query(&mut rng, ids, shape, 3, count),
        };
        agree_in(versions, &query, &mut rng);
    }
    for group in 0..3 {
        let rest = match rng.below(3) {
            0 => {
                let leaf = Query::Leaf(ranged(&mut rng, ids, shape));
                on(&mut rng, count, leaf)
            }
            1 => chain(&mut rng, ids, shape, count),
            _ => query(&mut rng, ids, shape, 2, count),
        };
        covered_agree(versions, group, &rest, &mut rng);
    }
    for _ in 0..4 {
        started_agree(versions, &mut rng, ids, shape);
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

fn two_tables(rng: &mut Rng, count: u32, shape: Shape) -> [(Table<Row>, Model); 2] {
    let mut ids = || -> Vec<u32> { (0..count).filter(|_| rng.chance(70)).collect() };
    let (first, second) = (ids(), ids());
    let first = rows(rng, &first, shape);
    let second = rows(rng, &second, shape);
    [(table_of(&first), first), (table_of(&second), second)]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(32)))]

    #[test]
    fn lookups_of_two_tables_or_versions_match_a_filter_over_each(
        count in 0u32..500,
        seed in any::<u64>(),
        narrow in any::<bool>(),
    ) {
        let shape = if narrow { NARROW } else { WIDE };
        let mut rng = Rng::new(seed);
        let [(a, first), (b, second)] = two_tables(&mut rng, count, shape);
        check_in(&[(&a, &first), (&b, &second)], seed, 20, shape);
        let (_dir, db) = stored_pair(&a, &b);
        let read = db.read();
        check_in(&[(&read.rows, &first), (&read.other, &second)], seed ^ 1, 20, shape);

        let (newer, changed) = edited(&mut rng, &a, &first, shape);
        check_in(&[(&a, &first), (&newer, &changed)], seed ^ 2, 20, shape);
        let (_dir, db) = stored_pair(&newer, &a);
        let read = db.read();
        check_in(&[(&read.rows, &changed), (&read.other, &first)], seed ^ 3, 20, shape);

        let copy = a.clone();
        check_in(&[(&a, &first), (&copy, &first), (&b, &second)], seed ^ 4, 10, shape);
        let four = [(&a, &first), (&newer, &changed), (&b, &second), (&copy, &first)];
        check_in(&four, seed ^ 5, 10, shape);
        check_deep_in(&four, seed ^ 6, 20, shape);
    }
}

#[test]
fn large_lookups_of_two_versions_match_a_filter_over_each() {
    let mut rng = Rng::new(19);
    let model = model(&mut rng, 4_000, WIDE);
    let table = table_of(&model);
    let (newer, changed) = edited(&mut rng, &table, &model, WIDE);
    let (_dir, db) = stored_pair(&table, &newer);
    let read = db.read();
    check_in(
        &[(&read.rows, &model), (&read.other, &changed)],
        31,
        150,
        WIDE,
    );
    check_in(&[(&newer, &changed), (&table, &model)], 37, 100, WIDE);
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

thread_local! {
    static TESTED: Cell<u64> = const { Cell::new(0) };
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Counted(Vec<u16>);

impl IndexEach for Counted {
    type Value = u16;

    fn values(&self) -> Vec<u16> {
        TESTED.with(|tested| tested.set(tested.get() + 1));
        self.0.clone()
    }
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Tagged {
    #[key]
    id: u32,
    #[index]
    group: u8,
    #[index(each)]
    tags: Counted,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Tags {
    rows: Table<Tagged>,
}

fn counted<R>(read: impl FnOnce() -> R) -> (R, u64) {
    TESTED.with(|tested| tested.set(0));
    let out = read();
    (out, TESTED.with(Cell::get))
}

#[test]
fn a_range_against_few_keys_tests_each_once_and_reads_in_full_when_the_rows_are_on_file() {
    let mut rng = Rng::new(71);
    let rows: Vec<Tagged> = (0..5_000)
        .map(|id| Tagged {
            id,
            group: if rng.chance(3) {
                0
            } else {
                1 + rng.below(3) as u8
            },
            tags: Counted(
                (0..1 + rng.below(3))
                    .map(|_| rng.below(200) as u16)
                    .collect(),
            ),
        })
        .collect();
    let mut table: Table<Tagged> = Table::new();
    for row in &rows {
        table.insert(row.clone());
    }
    let lead: Vec<u32> = rows
        .iter()
        .filter(|row| row.group == 0)
        .map(|row| row.id)
        .collect();
    let wanted: Vec<u32> = rows
        .iter()
        .filter(|row| row.group == 0 && row.tags.0.iter().any(|tag| *tag < 60))
        .map(|row| row.id)
        .collect();
    assert!(lead.len() > 100 && table.by_tags_range(0..60).len() > 4 * lead.len());
    let rows_by_id: BTreeMap<u32, Tagged> = rows.iter().map(|row| (row.id, row.clone())).collect();
    let find = table.find();

    let (keys, tested) = counted(|| {
        find.by_group(&0)
            .and(table.by_tags_range(0..60))
            .keys()
            .collect::<Vec<_>>()
    });
    assert_eq!((keys, tested), (wanted.clone(), lead.len() as u64));
    let (back, tested) = counted(|| {
        find.by_group(&0)
            .and(table.by_tags_range(0..60))
            .rev()
            .map(|row| row.id)
            .collect::<Vec<_>>()
    });
    assert_eq!(back, wanted.iter().rev().copied().collect::<Vec<_>>());
    assert_eq!(tested, lead.len() as u64);

    let (keys, tested) = counted(|| {
        find.by_group(&0)
            .and(table.by_tags_range(0..60).uncovered())
            .keys()
            .collect::<Vec<_>>()
    });
    assert_eq!((keys, tested), (wanted.clone(), 0));
    let first = || {
        table
            .by_tags_range(0..60)
            .into_matches()
            .and(find.by_group(&0))
    };
    assert_eq!(
        (
            first().explain().sides[0].read,
            first().explain().sides[1].read
        ),
        (Read::Tested, Read::Walked)
    );
    let (keys, tested) = counted(|| first().keys().collect::<Vec<_>>());
    assert_eq!((keys, tested), (wanted.clone(), lead.len() as u64));
    let (back, tested) = counted(|| first().rev().map(|row| row.id).collect::<Vec<_>>());
    assert_eq!(back, wanted.iter().rev().copied().collect::<Vec<_>>());
    assert_eq!(tested, lead.len() as u64);
    let (keys, tested) = counted(|| {
        table
            .by_tags_range(0..60)
            .into_matches()
            .and(table.by_tags_range(0..10))
            .and(find.by_group(&0))
            .keys()
            .collect::<Vec<_>>()
    });
    let narrow: Vec<u32> = rows
        .iter()
        .filter(|row| row.group == 0 && row.tags.0.iter().any(|tag| *tag < 10))
        .map(|row| row.id)
        .collect();
    assert_eq!(keys, narrow);
    assert!(
        tested > 0 && tested <= 2 * lead.len() as u64,
        "{tested} tests"
    );
    let mut moved = table.clone();
    moved.remove(&lead[0]);
    let (keys, tested) = counted(|| {
        table
            .by_tags_range(0..60)
            .into_matches()
            .and(moved.find().by_group(&0))
            .keys()
            .collect::<Vec<_>>()
    });
    let kept: Vec<u32> = wanted.iter().copied().filter(|id| *id != lead[0]).collect();
    assert_eq!((keys, tested), (kept, 0));
    let many: Vec<u32> = rows
        .iter()
        .filter(|row| row.group != 0 && row.tags.0.iter().any(|tag| *tag < 60))
        .map(|row| row.id)
        .collect();
    let (keys, tested) = counted(|| {
        find.by_group(&1)
            .or(find.by_group(&2))
            .or(find.by_group(&3))
            .and(table.by_tags_range(0..60))
            .keys()
            .collect::<Vec<_>>()
    });
    assert_eq!((keys, tested), (many, 0));

    let mut other = table.clone();
    other.remove(&lead[0]);
    for elsewhere in [&table, &other] {
        let either = || elsewhere.find().by_group(&9).or(table.by_tags_range(0..60));
        let (keys, tested) = counted(|| find.by_group(&0).and(either()).keys().collect::<Vec<_>>());
        assert_eq!((keys, tested), (wanted.clone(), lead.len() as u64));
        let (rows, tested) = counted(|| {
            find.by_group(&0)
                .and(either())
                .map(|row| row.id)
                .collect::<Vec<_>>()
        });
        assert_eq!((rows, tested), (wanted.clone(), lead.len() as u64));
        let (keys, tested) = counted(|| either().and(find.by_group(&0)).keys().collect::<Vec<_>>());
        assert_eq!((keys, tested), (wanted.clone(), lead.len() as u64));
        let rows: Vec<Tagged> = either()
            .and(find.by_group(&0))
            .map(|row| (*row).clone())
            .collect();
        let expected: Vec<Tagged> = wanted.iter().map(|id| rows_by_id[id].clone()).collect();
        assert_eq!(rows, expected);
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tested.ferro");
    let db: Db<Tags> = fast().open(&path).unwrap();
    db.update(|root| root.rows = table.clone()).unwrap();
    drop(db);
    let db: Db<Tags> = uncached().open(&path).unwrap();
    let read = db.read();
    let stored = &read.rows;
    for rowless in [true, false] {
        let (keys, tested) = counted(|| {
            let matches = stored.find().by_group(&0).and(stored.by_tags_range(0..60));
            if rowless {
                matches.keys().collect::<Vec<_>>()
            } else {
                matches.map(|row| row.id).collect()
            }
        });
        assert_eq!(keys, wanted);
        assert!(
            tested > 0 && tested < lead.len() as u64,
            "{tested} of {} keys tested with rows read from the file",
            lead.len()
        );
    }
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Spread {
    #[key]
    id: u32,
    #[index]
    wide: u8,
    #[index]
    mid: u8,
    #[index]
    narrow: u8,
    #[index(each)]
    tags: Counted,
}

#[test]
fn a_chain_of_ands_drives_from_its_smallest_side_whatever_the_order() {
    let mut rng = Rng::new(83);
    let mut table: Table<Spread> = Table::new();
    let mut rows = Vec::new();
    for id in 0..6_000 {
        let row = Spread {
            id,
            wide: (id % 2) as u8,
            mid: (id % 10) as u8,
            narrow: (id % 97) as u8,
            tags: Counted(vec![rng.below(100) as u16]),
        };
        table.insert(row.clone());
        rows.push(row);
    }
    let wanted: Vec<u32> = rows
        .iter()
        .filter(|row| row.wide == 0 && row.mid == 4 && row.narrow == 5 && row.tags.0[0] < 70)
        .map(|row| row.id)
        .collect();
    let narrow = table.by_narrow(&5).len() as u64;
    let find = table.find();
    let lookup = |side: usize| match side {
        0 => find.by_wide(&0),
        1 => find.by_mid(&4),
        2 => find.by_narrow(&5),
        _ => table.by_tags_range(..70).into_matches(),
    };
    let mut orders = vec![vec![0, 1, 2, 3]];
    for _ in 0..23 {
        let mut order = orders.last().unwrap().clone();
        let (a, b) = (rng.below(4) as usize, rng.below(4) as usize);
        order.swap(a, b);
        orders.push(order);
    }
    let mut costs = BTreeSet::new();
    for order in &orders {
        let combined = || {
            order[1..]
                .iter()
                .fold(lookup(order[0]), |all, side| all.and(lookup(*side)))
        };
        let plan = combined().explain();
        assert_eq!(plan.sides.len(), 4, "{order:?}: {plan}");
        let walked: Vec<usize> = plan
            .sides
            .iter()
            .enumerate()
            .filter(|(_, side)| side.read == Read::Walked)
            .map(|(at, _)| order[at])
            .collect();
        assert_eq!(walked, [2], "{order:?}: {plan}");
        let range = order.iter().position(|side| *side == 3).unwrap();
        assert_eq!(plan.sides[range].read, Read::Tested, "{order:?}: {plan}");
        let (keys, tested) = counted(|| combined().keys().collect::<Vec<_>>());
        assert_eq!(keys, wanted, "{order:?}");
        assert!(tested <= narrow, "{order:?}: {tested} tests");
        let (ids, again) = counted(|| combined().rev().map(|row| row.id).collect::<Vec<_>>());
        assert_eq!(
            ids,
            wanted.iter().rev().copied().collect::<Vec<_>>(),
            "{order:?}"
        );
        assert_eq!(again, tested, "{order:?}");
        costs.insert(tested);
    }
    assert_eq!(costs.len(), 1, "tests by order: {costs:?}");
}
