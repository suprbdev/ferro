mod common;

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::Bound;

use common::{Rng, fast};
use ferro::admin::Keep;
use ferro::codec::Hash;
use ferro::table::{IndexCovered, IndexIter};
use ferro::{Conflict, Data, Db, Record, Snapshot, Table, Val};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

#[derive(Data, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Draft,
    Live,
    Gone,
}

#[derive(Data, Clone, Debug, PartialEq)]
struct Price {
    cents: i64,
    currency: String,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(owner, stage, cover(stage, weight, memo))]
#[unique(shelf, slot, cover(price, weight))]
struct Item {
    #[key]
    id: u8,
    #[index(cover(weight, price, memo))]
    owner: u8,
    #[index(cover(stage, price))]
    stage: Stage,
    #[index(each, cover(weight, stage))]
    tags: Vec<u8>,
    #[index(each, cover(owner))]
    alias: Option<u8>,
    #[index(cover(weight, label))]
    label: Option<String>,
    #[unique(cover(owner, memo))]
    code: Option<u8>,
    weight: f64,
    price: Price,
    memo: String,
    shelf: u8,
    slot: u8,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(owner, stage)]
#[unique(shelf, slot)]
struct PlainItem {
    #[key]
    id: u8,
    #[index]
    owner: u8,
    #[index]
    stage: Stage,
    #[index(each)]
    tags: Vec<u8>,
    #[index(each)]
    alias: Option<u8>,
    #[index]
    label: Option<String>,
    #[unique]
    code: Option<u8>,
    weight: f64,
    price: Price,
    memo: String,
    shelf: u8,
    slot: u8,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    items: Table<Item>,
}

const IDS: u8 = 8;
const OWNERS: u8 = 3;
const STAGES: [Stage; 3] = [Stage::Draft, Stage::Live, Stage::Gone];
const WEIGHTS: [f64; 6] = [0.0, -0.0, 1.5, -2.25, f64::NAN, f64::INFINITY];
const CENTS: [i64; 3] = [0, 7, -3];
const CURRENCIES: [&str; 2] = ["", "eur"];
const LABELS: [&str; 3] = ["", "a", "b"];

fn memo(which: usize) -> String {
    match which % 4 {
        0 => String::new(),
        1 => "m".into(),
        2 => "x".repeat(600),
        _ => "long memo ".repeat(70),
    }
}

fn config(cases: u32, seed: u64) -> ProptestConfig {
    ProptestConfig {
        rng_seed: RngSeed::Fixed(seed),
        ..ProptestConfig::with_cases(common::cases(cases))
    }
}

fn shown<T: Debug>(value: &T) -> String {
    format!("{value:?}")
}

trait Shape: Record<Key = u8> + Debug + PartialEq {
    fn of(item: &Item) -> Self;
    fn item(&self) -> Item;
}

impl Shape for Item {
    fn of(item: &Item) -> Self {
        item.clone()
    }

    fn item(&self) -> Item {
        self.clone()
    }
}

impl Shape for PlainItem {
    fn of(item: &Item) -> Self {
        PlainItem {
            id: item.id,
            owner: item.owner,
            stage: item.stage,
            tags: item.tags.clone(),
            alias: item.alias,
            label: item.label.clone(),
            code: item.code,
            weight: item.weight,
            price: item.price.clone(),
            memo: item.memo.clone(),
            shelf: item.shelf,
            slot: item.slot,
        }
    }

    fn item(&self) -> Item {
        Item {
            id: self.id,
            owner: self.owner,
            stage: self.stage,
            tags: self.tags.clone(),
            alias: self.alias,
            label: self.label.clone(),
            code: self.code,
            weight: self.weight,
            price: self.price.clone(),
            memo: self.memo.clone(),
            shelf: self.shelf,
            slot: self.slot,
        }
    }
}

fn bounds<V: Clone>(values: &[V]) -> Vec<(Bound<V>, Bound<V>)> {
    let mut out = vec![(Bound::Unbounded, Bound::Unbounded)];
    for a in values {
        out.push((Bound::Included(a.clone()), Bound::Unbounded));
        out.push((Bound::Excluded(a.clone()), Bound::Unbounded));
        out.push((Bound::Unbounded, Bound::Included(a.clone())));
        out.push((Bound::Unbounded, Bound::Excluded(a.clone())));
        for b in values {
            out.push((Bound::Included(a.clone()), Bound::Included(b.clone())));
            out.push((Bound::Excluded(a.clone()), Bound::Excluded(b.clone())));
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

fn zigzag<C>(mut covered: IndexCovered<Item, C>) -> Vec<(u8, C)> {
    let mut front = Vec::new();
    let mut back = Vec::new();
    while covered.len() > 0 {
        if let Some(item) = covered.next() {
            front.push(item);
        }
        if let Some(item) = covered.next_back() {
            back.push(item);
        }
    }
    back.reverse();
    front.extend(back);
    front
}

struct Probe<'a, V, C> {
    model: &'a BTreeMap<u8, Item>,
    values: fn(&Item) -> Vec<V>,
    cover: fn(&Item) -> C,
}

impl<V: Ord + Clone + Debug, C: Clone + Debug + 'static> Probe<'_, V, C> {
    fn wanted(&self, keep: impl Fn(&V, u8) -> bool) -> Vec<(u8, C)> {
        let mut entries: BTreeMap<(V, u8), C> = BTreeMap::new();
        for row in self.model.values() {
            for value in (self.values)(row) {
                entries.insert((value, row.id), (self.cover)(row));
            }
        }
        entries
            .into_iter()
            .filter(|((value, id), _)| keep(value, *id))
            .map(|((_, id), cover)| (id, cover))
            .collect()
    }

    fn agree(&self, wanted: &[(u8, C)], lookup: impl Fn() -> IndexIter<Item, C>, what: &str) {
        let want = shown(&wanted);
        let covered: Vec<(u8, C)> = lookup().covered().collect();
        assert_eq!(shown(&covered), want, "covered {what}");
        let read: Vec<(u8, C)> = lookup().map(|row| (row.id, (self.cover)(&row))).collect();
        assert_eq!(shown(&read), want, "rows {what}");
        let mut reversed: Vec<(u8, C)> = lookup().covered().rev().collect();
        reversed.reverse();
        assert_eq!(shown(&reversed), want, "reversed {what}");
        assert_eq!(shown(&zigzag(lookup().covered())), want, "both ends {what}");
        assert_eq!(lookup().len(), wanted.len(), "len {what}");
        let keys: Vec<u8> = wanted.iter().map(|(id, _)| *id).collect();
        assert_eq!(lookup().keys().collect::<Vec<_>>(), keys, "keys {what}");
        let plain: Vec<u8> = lookup().uncovered().map(|row| row.id).collect();
        assert_eq!(plain, keys, "uncovered {what}");
        let mut back: Vec<u8> = lookup()
            .uncovered()
            .covered()
            .rev()
            .map(|(id, ())| id)
            .collect();
        back.reverse();
        assert_eq!(back, keys, "uncovered covered {what}");
        if let Some(((first, _), rest)) = wanted.split_first() {
            let mut mixed = lookup();
            assert_eq!(mixed.next().map(|row| row.id), Some(*first), "{what}");
            let tail: Vec<(u8, C)> = mixed.covered().collect();
            assert_eq!(shown(&tail), shown(&rest), "front then covered {what}");
        }
        if let Some(((last, _), front)) = wanted.split_last() {
            let mut mixed = lookup();
            assert_eq!(mixed.next_back().map(|row| row.id), Some(*last), "{what}");
            let head: Vec<(u8, C)> = mixed.covered().collect();
            assert_eq!(shown(&head), shown(&front), "back then covered {what}");
        }
    }

    fn check(
        &self,
        probes: &[V],
        eq: impl Fn(&V) -> IndexIter<Item, C>,
        range: impl Fn((Bound<V>, Bound<V>)) -> IndexIter<Item, C>,
        after: impl Fn(&V, &u8) -> IndexIter<Item, C>,
    ) {
        for probe in probes {
            let wanted = self.wanted(|value, _| value == probe);
            self.agree(&wanted, || eq(probe), &format!("eq {probe:?}"));
            for cursor in [0, 2, 5, IDS] {
                let wanted = self.wanted(|value, id| value == probe && id > cursor);
                self.agree(
                    &wanted,
                    || after(probe, &cursor),
                    &format!("after {probe:?} {cursor}"),
                );
            }
        }
        for (low, high) in bounds(probes) {
            let wanted = self.wanted(|value, _| within(value, &low, &high));
            self.agree(
                &wanted,
                || range((low.clone(), high.clone())),
                &format!("range {low:?} {high:?}"),
            );
        }
    }
}

fn labels() -> Vec<Option<String>> {
    let mut out = vec![None];
    out.extend(LABELS.iter().map(|label| Some(label.to_string())));
    out.push(Some("c".into()));
    out
}

fn check(table: &Table<Item>, model: &BTreeMap<u8, Item>) {
    let rows: Vec<Item> = table.iter().map(|row| (*row).clone()).collect();
    assert_eq!(
        shown(&rows),
        shown(&model.values().collect::<Vec<_>>()),
        "rows"
    );

    Probe {
        model,
        values: |row| vec![row.owner],
        cover: |row| (row.weight, row.price.clone(), row.memo.clone()),
    }
    .check(
        &[0, 1, 2, 3],
        |v| table.by_owner(v),
        |r| table.by_owner_range::<u8, _>(r),
        |v, a| table.by_owner_after(v, a),
    );
    Probe {
        model,
        values: |row| vec![row.stage],
        cover: |row| (row.stage, row.price.clone()),
    }
    .check(
        &STAGES,
        |v| table.by_stage(v),
        |r| table.by_stage_range::<Stage, _>(r),
        |v, a| table.by_stage_after(v, a),
    );
    Probe {
        model,
        values: |row| row.tags.clone(),
        cover: |row| (row.weight, row.stage),
    }
    .check(
        &[0, 1, 2, 3],
        |v| table.by_tags(v),
        |r| table.by_tags_range::<u8, _>(r),
        |v, a| table.by_tags_after(v, a),
    );
    Probe {
        model,
        values: |row| row.alias.into_iter().collect(),
        cover: |row| row.owner,
    }
    .check(
        &[0, 1, 2, 3],
        |v| table.by_alias(v),
        |r| table.by_alias_range::<u8, _>(r),
        |v, a| table.by_alias_after(v, a),
    );
    let by_label = Probe {
        model,
        values: |row| vec![row.label.clone()],
        cover: |row| (row.weight, row.label.clone()),
    };
    by_label.check(
        &labels(),
        |v| table.by_label(v),
        |r| table.by_label_range::<Option<String>, _>(r),
        |v, a| table.by_label_after(v, a),
    );
    for probe in ["", "a", "b", "c"] {
        let wanted = by_label.wanted(|value, _| value.as_deref() == Some(probe));
        by_label.agree(&wanted, || table.by_label_some(probe), probe);
        for cursor in [0, 3, IDS] {
            let wanted =
                by_label.wanted(|value, id| value.as_deref() == Some(probe) && id > cursor);
            by_label.agree(&wanted, || table.by_label_some_after(probe, &cursor), probe);
        }
    }
    let by_owner_stage = Probe {
        model,
        values: |row| vec![(row.owner, row.stage)],
        cover: |row| (row.stage, row.weight, row.memo.clone()),
    };
    by_owner_stage.check(
        &[
            (0, Stage::Draft),
            (0, Stage::Gone),
            (1, Stage::Live),
            (2, Stage::Draft),
        ],
        |v| table.by_owner_stage(v),
        |r| table.by_owner_stage_range::<(u8, Stage), _>(r),
        |v, a| table.by_owner_stage_after(v, a),
    );
    for owner in 0..OWNERS + 1 {
        let wanted = by_owner_stage.wanted(|value, _| value.0 == owner);
        by_owner_stage.agree(&wanted, || table.by_owner_stage_prefix(&owner), "prefix");
    }

    for code in 0..5u8 {
        let holder = model.values().find(|row| row.code == Some(code));
        assert_eq!(
            shown(&table.covered_by_code(&code)),
            shown(&holder.map(|row| (row.id, (row.owner, row.memo.clone())))),
            "code {code}"
        );
        assert_eq!(table.key_by_code(&code), holder.map(|row| row.id));
        assert_eq!(
            shown(&table.by_code(&code).map(|row| (*row).clone())),
            shown(&holder)
        );
    }
    for shelf in 0..3u8 {
        for slot in 0..5u8 {
            let holder = model
                .values()
                .find(|row| (row.shelf, row.slot) == (shelf, slot));
            assert_eq!(
                shown(&table.covered_by_shelf_slot(&(shelf, slot))),
                shown(&holder.map(|row| (row.id, (row.price.clone(), row.weight)))),
                "slot {shelf} {slot}"
            );
            assert_eq!(
                table.key_by_shelf_slot(&(shelf, slot)),
                holder.map(|row| row.id)
            );
        }
    }
}

fn hashes(table: &Table<Item>) -> Vec<Option<Hash>> {
    vec![
        table.rows.root_hash(),
        table.by_owner.root_hash(),
        table.by_stage.root_hash(),
        table.by_tags.root_hash(),
        table.by_alias.root_hash(),
        table.by_label.root_hash(),
        table.unique_code.root_hash(),
        table.by_owner_stage.root_hash(),
        table.unique_shelf_slot.root_hash(),
    ]
}

fn rebuilt(model: &BTreeMap<u8, Item>, seed: u64) -> Table<Item> {
    let mut rows: Vec<Item> = model.values().cloned().collect();
    Rng::new(seed).shuffle(&mut rows);
    let mut table: Table<Item> = Table::new();
    for row in rows {
        table.insert(row);
    }
    table
}

fn same_form(table: &Table<Item>, model: &BTreeMap<u8, Item>, seed: u64) {
    assert_eq!(hashes(table), hashes(&rebuilt(model, seed)), "index hashes");
}

#[derive(Clone, Debug)]
enum Change {
    Owner(u8),
    Stage(Stage),
    Tags(Vec<u8>),
    Alias(Option<u8>),
    Label(Option<String>),
    Code(Option<u8>),
    Weight(f64),
    Price(i64, &'static str),
    Memo(usize),
    Slot(u8, u8),
    Key(u8),
}

fn apply(row: &mut Item, change: &Change) {
    match change {
        Change::Owner(owner) => row.owner = *owner,
        Change::Stage(stage) => row.stage = *stage,
        Change::Tags(tags) => row.tags = tags.clone(),
        Change::Alias(alias) => row.alias = *alias,
        Change::Label(label) => row.label = label.clone(),
        Change::Code(code) => row.code = *code,
        Change::Weight(weight) => row.weight = *weight,
        Change::Price(cents, currency) => {
            row.price = Price {
                cents: *cents,
                currency: currency.to_string(),
            }
        }
        Change::Memo(which) => row.memo = memo(*which),
        Change::Slot(shelf, slot) => {
            row.shelf = *shelf;
            row.slot = *slot;
        }
        Change::Key(id) => row.id = *id,
    }
}

fn stage() -> impl Strategy<Value = Stage> {
    prop::sample::select(STAGES.to_vec())
}

fn label() -> impl Strategy<Value = Option<String>> {
    prop::option::of(prop::sample::select(LABELS.to_vec()).prop_map(str::to_string))
}

fn code() -> impl Strategy<Value = Option<u8>> {
    prop_oneof![2 => Just(None), 3 => (0..4u8).prop_map(Some)]
}

fn row_strategy() -> impl Strategy<Value = Item> {
    (
        (
            0..IDS,
            0..OWNERS,
            stage(),
            prop::collection::vec(0..3u8, 0..3),
            prop::option::of(0..3u8),
            label(),
        ),
        (
            code(),
            prop::sample::select(WEIGHTS.to_vec()),
            prop::sample::select(CENTS.to_vec()),
            prop::sample::select(CURRENCIES.to_vec()),
            0..4usize,
            0..2u8,
            0..4u8,
        ),
    )
        .prop_map(
            |(
                (id, owner, stage, tags, alias, label),
                (code, weight, cents, currency, which, shelf, slot),
            )| Item {
                id,
                owner,
                stage,
                tags,
                alias,
                label,
                code,
                weight,
                price: Price {
                    cents,
                    currency: currency.to_string(),
                },
                memo: memo(which),
                shelf,
                slot,
            },
        )
}

fn change() -> impl Strategy<Value = Change> {
    prop_oneof![
        2 => (0..OWNERS).prop_map(Change::Owner),
        2 => stage().prop_map(Change::Stage),
        2 => prop::collection::vec(0..3u8, 0..3).prop_map(Change::Tags),
        2 => prop::option::of(0..3u8).prop_map(Change::Alias),
        2 => label().prop_map(Change::Label),
        2 => code().prop_map(Change::Code),
        4 => prop::sample::select(WEIGHTS.to_vec()).prop_map(Change::Weight),
        3 => (prop::sample::select(CENTS.to_vec()), prop::sample::select(CURRENCIES.to_vec()))
            .prop_map(|(cents, currency)| Change::Price(cents, currency)),
        3 => (0..4usize).prop_map(Change::Memo),
        1 => (0..2u8, 0..4u8).prop_map(|(shelf, slot)| Change::Slot(shelf, slot)),
        1 => (0..IDS).prop_map(Change::Key),
    ]
}

#[derive(Clone, Debug)]
enum Op {
    Insert(Item),
    Remove(u8),
    Update(u8, Vec<Change>),
    Edit(u8, Vec<Change>),
    Retain(u8),
    TakeRange(u8, u8),
    RemoveRange(u8, u8),
    Commit,
    Reopen,
    Compact,
}

fn op() -> impl Strategy<Value = Op> {
    let changes = || prop::collection::vec(change(), 0..4);
    prop_oneof![
        8 => row_strategy().prop_map(Op::Insert),
        2 => (0..IDS).prop_map(Op::Remove),
        6 => (0..IDS, changes()).prop_map(|(id, c)| Op::Update(id, c)),
        6 => (0..IDS, changes()).prop_map(|(id, c)| Op::Edit(id, c)),
        1 => any::<u8>().prop_map(Op::Retain),
        1 => (0..IDS, 0..IDS + 1).prop_map(|(lo, hi)| Op::TakeRange(lo, hi)),
        1 => (0..IDS, 0..IDS + 1).prop_map(|(lo, hi)| Op::RemoveRange(lo, hi)),
        2 => Just(Op::Commit),
        1 => Just(Op::Reopen),
        1 => Just(Op::Compact),
    ]
}

fn clashes(model: &BTreeMap<u8, Item>, row: &Item, moving: Option<u8>) -> bool {
    model.values().any(|other| {
        other.id != row.id
            && Some(other.id) != moving
            && ((row.code.is_some() && other.code == row.code)
                || (other.shelf, other.slot) == (row.shelf, row.slot))
    })
}

fn run(ops: Vec<Op>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model.ferro");
    let mut db: Db<Root> = fast().open(&path).unwrap();
    let mut table: Table<Item> = Table::new();
    let mut model: BTreeMap<u8, Item> = BTreeMap::new();
    let mut committed = model.clone();
    for (step, op) in ops.into_iter().enumerate() {
        match op {
            Op::Insert(row) => {
                let clash = clashes(&model, &row, None);
                assert_eq!(table.try_insert(row.clone()).is_err(), clash);
                if !clash {
                    model.insert(row.id, row);
                }
            }
            Op::Remove(id) => {
                let old = table.remove(&id);
                assert_eq!(
                    shown(&old.map(|row| (*row).clone())),
                    shown(&model.remove(&id))
                );
            }
            Op::Update(id, _) | Op::Edit(id, _) if !model.contains_key(&id) => {
                assert_eq!(table.try_update(&id, |_| {}), Ok(None));
                assert!(table.get_mut(&id).is_none());
            }
            Op::Update(id, changes) => {
                let mut row = model[&id].clone();
                changes.iter().for_each(|change| apply(&mut row, change));
                let clash = clashes(&model, &row, Some(id));
                let done = table.try_update(&id, |stored| {
                    changes.iter().for_each(|change| apply(stored, change));
                });
                assert_eq!(done.is_err(), clash);
                if !clash {
                    model.remove(&id);
                    model.insert(row.id, row);
                }
            }
            Op::Edit(id, changes) => {
                let mut row = model[&id].clone();
                changes.iter().for_each(|change| apply(&mut row, change));
                if clashes(&model, &row, Some(id)) {
                    let done = table.try_update(&id, |stored| {
                        changes.iter().for_each(|change| apply(stored, change));
                    });
                    assert!(done.is_err());
                } else {
                    let mut stored = table.get_mut(&id).unwrap();
                    changes.iter().for_each(|change| apply(&mut stored, change));
                    drop(stored);
                    model.remove(&id);
                    model.insert(row.id, row);
                }
            }
            Op::Retain(mask) => {
                table.retain(|row| mask & (1 << row.id) != 0);
                model.retain(|id, _| mask & (1 << id) != 0);
            }
            Op::TakeRange(lo, hi) => {
                let wanted: BTreeMap<u8, Item> = model
                    .range(lo..hi.max(lo))
                    .map(|(id, row)| (*id, row.clone()))
                    .collect();
                let taken = table.take_range(lo..hi);
                check(&taken, &wanted);
                same_form(&taken, &wanted, step as u64);
                model.retain(|id, _| !(lo..hi).contains(id));
            }
            Op::RemoveRange(lo, hi) => {
                let wanted = model.range(lo..hi.max(lo)).count();
                assert_eq!(table.remove_range(lo..hi), wanted);
                model.retain(|id, _| !(lo..hi).contains(id));
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
                assert!(!table.by_owner.is_dirty());
            }
            Op::Compact => {
                db.compact(Keep::Last(1)).unwrap();
                check(&db.read().items, &committed);
                same_form(&db.read().items, &committed, step as u64);
            }
        }
        check(&table, &model);
        same_form(&table, &model, step as u64);
    }
    let snapshot = table.clone();
    db.update(|root| root.items = snapshot).unwrap();
    drop(table);
    drop(db);
    let db: Db<Root> = fast().open(&path).unwrap();
    check(&db.read().items, &model);
    same_form(&db.read().items, &model, 7);
}

proptest! {
    #![proptest_config(config(48, 0xC0DE_0001))]

    #[test]
    fn covered_lookups_match_the_rows_through_every_table_edit(
        ops in prop::collection::vec(op(), 1..40),
    ) {
        run(ops);
    }
}

fn negative_zero(id: u8) -> Item {
    Item {
        id,
        owner: 1,
        stage: Stage::Live,
        tags: vec![2],
        alias: Some(1),
        label: Some("a".into()),
        code: Some(id),
        weight: 0.0,
        price: Price {
            cents: 5,
            currency: "eur".into(),
        },
        memo: String::new(),
        shelf: 0,
        slot: id,
    }
}

#[test]
fn a_covered_float_that_turns_to_negative_zero_is_rewritten() {
    let mut table: Table<Item> = Table::new();
    for id in 0..3 {
        table.insert(negative_zero(id));
    }
    table.update(&1, |row| row.weight = -0.0);
    let weights: Vec<u64> = table
        .by_owner(&1)
        .covered()
        .map(|(_, (weight, _, _))| weight.to_bits())
        .collect();
    assert_eq!(weights, vec![0, (-0.0f64).to_bits(), 0]);
    let tagged: Vec<u64> = table
        .by_tags(&2)
        .covered()
        .map(|(_, (weight, _))| weight.to_bits())
        .collect();
    assert_eq!(tagged, weights);
    let slotted = table.covered_by_shelf_slot(&(0, 1)).unwrap();
    assert_eq!((slotted.1).1.to_bits(), (-0.0f64).to_bits());
    let model: BTreeMap<u8, Item> = table.iter().map(|row| (row.id, (*row).clone())).collect();
    same_form(&table, &model, 1);

    let before = hashes(&table);
    table.update(&1, |row| row.weight = -0.0);
    assert_eq!(hashes(&table), before);
}

#[derive(Data, Clone, Debug)]
#[ferro(transparent)]
struct Folded(String);

impl Folded {
    fn folded(&self) -> String {
        self.0.to_lowercase()
    }
}

impl PartialEq for Folded {
    fn eq(&self, other: &Self) -> bool {
        self.folded() == other.folded()
    }
}

impl Eq for Folded {}

impl PartialOrd for Folded {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Folded {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.folded().cmp(&other.folded())
    }
}

#[derive(Data, Record, Clone, Debug)]
#[index(team, name, cover(name, balance))]
struct Member {
    #[key]
    id: u8,
    #[index(cover(name, balance))]
    name: Folded,
    #[index(cover(alias))]
    alias: Folded,
    #[index(cover(name))]
    team: u8,
    balance: i64,
}

fn member_hashes(table: &Table<Member>) -> Vec<Option<Hash>> {
    vec![
        table.rows.root_hash(),
        table.by_name.root_hash(),
        table.by_alias.root_hash(),
        table.by_team.root_hash(),
        table.by_team_name.root_hash(),
    ]
}

#[test]
fn a_covered_keyed_field_whose_encoding_changes_while_eq_holds_is_rewritten() {
    let member = |id: u8, name: &str, alias: &str| Member {
        id,
        name: Folded(name.into()),
        alias: Folded(alias.into()),
        team: 0,
        balance: i64::from(id),
    };
    let mut table: Table<Member> = Table::new();
    table.insert(member(1, "Ana", "BO"));
    table.insert(member(2, "Cy", "Di"));
    table.update(&1, |row| {
        row.name = Folded("ana".into());
        row.alias = Folded("bo".into());
    });
    let names = |covered: Vec<(u8, Folded)>| -> Vec<(u8, String)> {
        covered.into_iter().map(|(id, name)| (id, name.0)).collect()
    };
    let by_name: Vec<(u8, String, i64)> = table
        .by_name(&Folded("ANA".into()))
        .covered()
        .map(|(id, (name, balance))| (id, name.0, balance))
        .collect();
    assert_eq!(by_name, vec![(1, "ana".to_string(), 1)]);
    let by_alias = names(table.by_alias(&Folded("Bo".into())).covered().collect());
    assert_eq!(by_alias, vec![(1, "bo".to_string())]);
    let by_team = names(table.by_team(&0).covered().collect());
    assert_eq!(by_team, vec![(1, "ana".to_string()), (2, "Cy".to_string())]);
    let by_team_name: Vec<(u8, String)> = table
        .by_team_name_prefix(&0)
        .covered()
        .map(|(id, (name, _))| (id, name.0))
        .collect();
    assert_eq!(
        by_team_name,
        vec![(1, "ana".to_string()), (2, "Cy".to_string())]
    );
    let mut fresh: Table<Member> = Table::new();
    for row in table.iter() {
        fresh.insert((*row).clone());
    }
    assert_eq!(member_hashes(&table), member_hashes(&fresh));
    assert!(table.same_encoding(&fresh));
    let before = member_hashes(&table);
    table.update(&1, |row| row.name = Folded("ana".into()));
    assert_eq!(member_hashes(&table), before);
}

#[test]
fn changing_only_a_nested_covered_field_rewrites_the_entries_that_cover_it() {
    let mut table: Table<Item> = Table::new();
    for id in 0..3 {
        table.insert(negative_zero(id));
    }
    let before = hashes(&table);
    table.update(&2, |row| row.price.currency = "usd".into());
    let after = hashes(&table);
    assert_ne!(after[1], before[1]);
    assert_ne!(after[2], before[2]);
    assert_ne!(after[8], before[8]);
    assert_eq!(after[3], before[3]);
    assert_eq!(after[6], before[6]);
    assert_eq!(
        table
            .covered_by_shelf_slot(&(0, 2))
            .map(|(_, (price, _))| price.currency),
        Some("usd".to_string())
    );
}

#[derive(Clone, Copy, Debug)]
enum Policy {
    Ours,
    Theirs,
    Drop,
    Blend,
    Fail(usize),
}

fn policy() -> impl Strategy<Value = Policy> {
    prop_oneof![
        Just(Policy::Ours),
        Just(Policy::Theirs),
        Just(Policy::Drop),
        Just(Policy::Blend),
        (0..4usize).prop_map(Policy::Fail),
    ]
}

fn blend(key: u8, ours: Option<Item>, theirs: Option<Item>) -> Option<Item> {
    match (ours, theirs) {
        (Some(ours), Some(mut theirs)) => {
            theirs.weight = ours.weight;
            theirs.memo = ours.memo;
            theirs.code = theirs.code.map(|code| (code + key) % 5);
            Some(theirs)
        }
        (ours, theirs) => theirs.or(ours),
    }
}

type Call = (u8, String, String, String);

fn merged<T: Shape>(
    base: &[Item],
    ours: &[Item],
    theirs: &[Item],
    policy: Policy,
) -> (Table<T>, Vec<Call>, Result<usize, usize>) {
    let table = |rows: &[Item]| {
        let mut table: Table<T> = Table::new();
        for row in rows {
            let _ = table.try_insert(T::of(row));
        }
        table
    };
    let mut target = table(ours);
    let mut calls = Vec::new();
    let written = target.try_merge(&table(base), &table(theirs), |c: Conflict<u8, T>| {
        let item = |side: &Option<Val<T>>| side.as_deref().map(T::item);
        let (ours, theirs) = (item(&c.ours), item(&c.theirs));
        calls.push((c.key, shown(&item(&c.base)), shown(&ours), shown(&theirs)));
        let row = match policy {
            Policy::Ours => ours,
            Policy::Theirs => theirs,
            Policy::Drop => None,
            Policy::Blend => blend(c.key, ours, theirs),
            Policy::Fail(at) if calls.len() > at => return Err(calls.len()),
            Policy::Fail(_) => blend(c.key, ours, theirs),
        };
        Ok(row.map(|row| T::of(&row)))
    });
    (target, calls, written)
}

fn model_of(table: &Table<Item>) -> BTreeMap<u8, Item> {
    table.iter().map(|row| (row.id, (*row).clone())).collect()
}

fn side(rows: Vec<Item>) -> Vec<Item> {
    let mut table: Table<Item> = Table::new();
    for row in rows {
        let _ = table.try_insert(row);
    }
    table.iter().map(|row| (*row).clone()).collect()
}

fn edited(base: &[Item], edits: Vec<(u8, Vec<Change>)>, added: Vec<Item>) -> Vec<Item> {
    let mut table: Table<Item> = Table::new();
    for row in base {
        table.insert(row.clone());
    }
    for (id, changes) in edits {
        if changes.is_empty() {
            table.remove(&id);
            continue;
        }
        let _ = table.try_update(&id, |row| changes.iter().for_each(|c| apply(row, c)));
    }
    for row in added {
        let _ = table.try_insert(row);
    }
    table.iter().map(|row| (*row).clone()).collect()
}

fn sides() -> impl Strategy<Value = (Vec<Item>, Vec<Item>, Vec<Item>)> {
    let edits = || {
        (
            prop::collection::vec((0..IDS, prop::collection::vec(change(), 0..3)), 0..6),
            prop::collection::vec(row_strategy(), 0..3),
        )
    };
    (
        prop::collection::vec(row_strategy(), 0..8),
        edits(),
        edits(),
    )
        .prop_map(|(base, (ours, ours_added), (theirs, theirs_added))| {
            let base = side(base);
            let ours = edited(&base, ours, ours_added);
            let theirs = edited(&base, theirs, theirs_added);
            (base, ours, theirs)
        })
}

proptest! {
    #![proptest_config(config(400, 0xC0DE_0002))]

    #[test]
    fn merged_covered_tables_match_a_plain_merge_and_a_rebuild(
        (base, ours, theirs) in sides(),
        policy in policy(),
    ) {
        let (covered, covered_calls, covered_written) = merged::<Item>(&base, &ours, &theirs, policy);
        let (plain, plain_calls, plain_written) = merged::<PlainItem>(&base, &ours, &theirs, policy);
        let rows = |table: &Table<Item>| shown(&table.iter().map(|row| (*row).clone()).collect::<Vec<_>>());
        let plain_rows: Vec<Item> = plain.iter().map(|row| row.item()).collect();
        prop_assert_eq!(rows(&covered), shown(&plain_rows));
        prop_assert_eq!(&covered_calls, &plain_calls);
        prop_assert_eq!(covered_written, plain_written);
        let model = model_of(&covered);
        check(&covered, &model);
        same_form(&covered, &model, 3);
        if covered_written.is_err() {
            let mut untouched: Table<Item> = Table::new();
            for row in &ours {
                untouched.insert(row.clone());
            }
            prop_assert_eq!(hashes(&covered), hashes(&untouched));
        }
    }
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Shop {
    items: Table<Item>,
}

fn sync_merge(root: &mut Shop, base: &Snapshot<Shop>, theirs: &Snapshot<Shop>) -> (String, ()) {
    root.items.merge(&base.items, &theirs.items, |c| {
        let ours = c.ours.map(Val::into_owned);
        let theirs = c.theirs.map(Val::into_owned);
        blend(c.key, ours, theirs)
    });
    ("merge".to_string(), ())
}

#[derive(Clone, Debug)]
enum Sync {
    Edit(usize, u8, Vec<Change>),
    Insert(usize, Item),
    Remove(usize, u8),
    Pull(usize),
    Bundle(usize),
    Compact(usize),
    Reopen(usize),
}

fn sync_op() -> impl Strategy<Value = Sync> {
    prop_oneof![
        6 => (0..2usize, 0..IDS, prop::collection::vec(change(), 1..3))
            .prop_map(|(at, id, c)| Sync::Edit(at, id, c)),
        4 => (0..2usize, row_strategy()).prop_map(|(at, row)| Sync::Insert(at, row)),
        1 => (0..2usize, 0..IDS).prop_map(|(at, id)| Sync::Remove(at, id)),
        3 => (0..2usize).prop_map(Sync::Pull),
        2 => (0..2usize).prop_map(Sync::Bundle),
        1 => (0..2usize).prop_map(Sync::Compact),
        1 => (0..2usize).prop_map(Sync::Reopen),
    ]
}

fn checked(db: &Db<Shop>) {
    let read = db.read();
    let model = model_of(&read.items);
    check(&read.items, &model);
    same_form(&read.items, &model, 5);
}

proptest! {
    #![proptest_config(config(24, 0xC0DE_0003))]

    #[test]
    fn covered_indexes_follow_their_rows_between_files(
        ops in prop::collection::vec(sync_op(), 1..24),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let paths = [dir.path().join("a.ferro"), dir.path().join("b.ferro")];
        let mut dbs: Vec<Db<Shop>> = paths.iter().map(|path| fast().open(path).unwrap()).collect();
        for op in ops {
            match op {
                Sync::Edit(at, id, changes) => {
                    dbs[at]
                        .update(|shop| {
                            let _ = shop.items.try_update(&id, |row| {
                                changes.iter().for_each(|change| apply(row, change));
                            });
                        })
                        .unwrap();
                }
                Sync::Insert(at, row) => {
                    dbs[at]
                        .update(|shop| {
                            let _ = shop.items.try_insert(row);
                        })
                        .unwrap();
                }
                Sync::Remove(at, id) => {
                    dbs[at].update(|shop| shop.items.remove(&id)).unwrap();
                }
                Sync::Pull(at) => {
                    let (ours, theirs) = (&dbs[at], &dbs[1 - at]);
                    ours.pull(theirs, if at == 0 { "b" } else { "a" }, sync_merge).unwrap();
                }
                Sync::Bundle(at) => {
                    let bundle = dbs[1 - at].bundle(&dbs[at].have().unwrap()).unwrap();
                    dbs[at]
                        .receive(if at == 0 { "b" } else { "a" }, &bundle, sync_merge)
                        .unwrap();
                }
                Sync::Compact(at) => {
                    dbs[at].compact(Keep::All).unwrap();
                }
                Sync::Reopen(at) => {
                    drop(dbs.remove(at));
                    dbs.insert(at, fast().open(&paths[at]).unwrap());
                    prop_assert!(!dbs[at].read().items.by_owner.is_dirty());
                }
            }
            checked(&dbs[0]);
            checked(&dbs[1]);
        }
        dbs[0].pull(&dbs[1], "b", sync_merge).unwrap();
        dbs[1].pull(&dbs[0], "a", sync_merge).unwrap();
        checked(&dbs[0]);
        checked(&dbs[1]);
    }
}
