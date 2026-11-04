mod common;

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::Bound;

use common::{Rng, fast};
use ferro::table::IndexIter;
use ferro::{Data, Db, Map, Record, Table};
use proptest::prelude::*;

#[derive(Data, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Open,
    Paid,
    Void,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(customer, day, cover(day, total, state))]
#[unique(shop, number, cover(total))]
struct Order {
    #[key]
    id: u32,
    #[index(cover(state, total, note))]
    customer: u8,
    #[index(cover(total))]
    total: i64,
    #[index(cover(total, state))]
    coupon: Option<String>,
    #[index(each, cover(state))]
    tags: Vec<String>,
    #[unique(cover(customer, state))]
    code: Option<String>,
    #[index]
    day: u8,
    state: State,
    note: String,
    shop: u8,
    number: u8,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    orders: Table<Order>,
}

type CodeEntry = (u32, (u8, State));
type CustomerCover = (State, i64, String);

const IDS: u32 = 10;
const TOTALS: [i64; 4] = [-5, 0, 7, 100];
const NOTES: [&str; 3] = ["", "x", "a longer note"];

fn order(id: u32, customer: u8, total: i64, state: State) -> Order {
    Order {
        id,
        customer,
        total,
        coupon: None,
        tags: Vec::new(),
        code: None,
        day: 0,
        state,
        note: String::new(),
        shop: 0,
        number: id as u8,
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

fn zigzag<C>(mut covered: ferro::table::IndexCovered<Order, C>) -> Vec<(u32, C)> {
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
    model: &'a BTreeMap<u32, Order>,
    values: fn(&Order) -> Vec<V>,
    cover: fn(&Order) -> C,
}

impl<V: Ord + Clone + Debug, C: Clone + PartialEq + Debug + 'static> Probe<'_, V, C> {
    fn entries(&self) -> Vec<(V, u32, C)> {
        let mut entries: BTreeMap<(V, u32), C> = BTreeMap::new();
        for row in self.model.values() {
            for value in (self.values)(row) {
                entries.insert((value, row.id), (self.cover)(row));
            }
        }
        entries
            .into_iter()
            .map(|((value, id), cover)| (value, id, cover))
            .collect()
    }

    fn wanted(&self, keep: impl Fn(&V, u32) -> bool) -> Vec<(u32, C)> {
        self.entries()
            .into_iter()
            .filter(|(value, id, _)| keep(value, *id))
            .map(|(_, id, cover)| (id, cover))
            .collect()
    }

    fn agree(&self, wanted: &[(u32, C)], lookup: impl Fn() -> IndexIter<Order, C>, what: &str) {
        let covered: Vec<(u32, C)> = lookup().covered().collect();
        assert_eq!(covered, wanted, "covered {what}");
        let read: Vec<(u32, C)> = lookup().map(|row| (row.id, (self.cover)(&row))).collect();
        assert_eq!(read, wanted, "rows {what}");
        let mut reversed = wanted.to_vec();
        reversed.reverse();
        assert_eq!(
            lookup().covered().rev().collect::<Vec<_>>(),
            reversed,
            "reversed {what}"
        );
        assert_eq!(zigzag(lookup().covered()), wanted, "both ends {what}");
        assert_eq!(lookup().covered().len(), wanted.len(), "len {what}");
        let keys: Vec<u32> = lookup().keys().collect();
        assert_eq!(
            keys,
            wanted.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            "keys {what}"
        );
        let plain: Vec<u32> = lookup().uncovered().map(|row| row.id).collect();
        assert_eq!(plain, keys, "uncovered {what}");
    }

    fn check(
        &self,
        probes: &[V],
        eq: impl Fn(&V) -> IndexIter<Order, C>,
        range: impl Fn((Bound<V>, Bound<V>)) -> IndexIter<Order, C>,
        after: impl Fn(&V, &u32) -> IndexIter<Order, C>,
    ) {
        for probe in probes {
            let wanted = self.wanted(|value, _| value == probe);
            self.agree(&wanted, || eq(probe), &format!("eq {probe:?}"));
            for cursor in 0..IDS {
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

fn coupons() -> Vec<Option<String>> {
    vec![
        None,
        Some(String::new()),
        Some("a".into()),
        Some("b".into()),
        Some("c".into()),
    ]
}

fn check(table: &Table<Order>, model: &BTreeMap<u32, Order>) {
    assert_eq!(table.len(), model.len());
    let rows: Vec<Order> = table.iter().map(|row| (*row).clone()).collect();
    assert_eq!(rows, model.values().cloned().collect::<Vec<_>>());

    Probe {
        model,
        values: |row| vec![row.customer],
        cover: |row| (row.state, row.total, row.note.clone()),
    }
    .check(
        &[0, 1, 2, 3, 4],
        |v| table.by_customer(v),
        |r| table.by_customer_range::<u8, _>(r),
        |v, a| table.by_customer_after(v, a),
    );
    Probe {
        model,
        values: |row| vec![row.total],
        cover: |row| row.total,
    }
    .check(
        &[-6, -5, 0, 7, 50, 100],
        |v| table.by_total(v),
        |r| table.by_total_range::<i64, _>(r),
        |v, a| table.by_total_after(v, a),
    );
    let by_coupon = Probe {
        model,
        values: |row| vec![row.coupon.clone()],
        cover: |row| (row.total, row.state),
    };
    by_coupon.check(
        &coupons(),
        |v| table.by_coupon(v),
        |r| table.by_coupon_range::<Option<String>, _>(r),
        |v, a| table.by_coupon_after(v, a),
    );
    for probe in ["", "a", "b", "c"] {
        let wanted = by_coupon.wanted(|value, _| value.as_deref() == Some(probe));
        by_coupon.agree(&wanted, || table.by_coupon_some(probe), probe);
        for cursor in 0..IDS {
            let wanted =
                by_coupon.wanted(|value, id| value.as_deref() == Some(probe) && id > cursor);
            by_coupon.agree(
                &wanted,
                || table.by_coupon_some_after(probe, &cursor),
                probe,
            );
        }
    }
    Probe {
        model,
        values: |row| row.tags.clone(),
        cover: |row| row.state,
    }
    .check(
        &["".to_string(), "x".into(), "y".into(), "z".into()],
        |v| table.by_tags(v),
        |r| table.by_tags_range::<String, _>(r),
        |v, a| table.by_tags_after(v, a),
    );
    let by_customer_day = Probe {
        model,
        values: |row| vec![(row.customer, row.day)],
        cover: |row| (row.day, row.total, row.state),
    };
    let pairs: Vec<(u8, u8)> = vec![(0, 0), (0, 2), (1, 1), (2, 3), (3, 0)];
    by_customer_day.check(
        &pairs,
        |v| table.by_customer_day(v),
        |r| table.by_customer_day_range::<(u8, u8), _>(r),
        |v, a| table.by_customer_day_after(v, a),
    );
    for customer in 0..5u8 {
        let wanted = by_customer_day.wanted(|value, _| value.0 == customer);
        by_customer_day.agree(
            &wanted,
            || table.by_customer_day_prefix(&customer),
            "prefix",
        );
    }
    Probe {
        model,
        values: |row| vec![row.day],
        cover: |_| (),
    }
    .check(
        &[0, 1, 2, 3, 4],
        |v| table.by_day(v),
        |r| table.by_day_range::<u8, _>(r),
        |v, a| table.by_day_after(v, a),
    );

    for code in ["", "p", "q", "r", "s"] {
        let holder = model.values().find(|row| row.code.as_deref() == Some(code));
        assert_eq!(
            table.covered_by_code(code),
            holder.map(|row| (row.id, (row.customer, row.state))),
            "code {code}"
        );
        assert_eq!(table.key_by_code(code), holder.map(|row| row.id));
        assert_eq!(
            table.by_code(code).map(|row| (*row).clone()),
            holder.cloned()
        );
    }
    for shop in 0..3u8 {
        for number in 0..12u8 {
            let holder = model
                .values()
                .find(|row| (row.shop, row.number) == (shop, number));
            assert_eq!(
                table.covered_by_shop_number(&(shop, number)),
                holder.map(|row| (row.id, row.total))
            );
            assert_eq!(
                table.key_by_shop_number(&(shop, number)),
                holder.map(|row| row.id)
            );
            assert_eq!(
                table
                    .by_shop_number(&(shop, number))
                    .map(|row| (*row).clone()),
                holder.cloned()
            );
        }
    }
    let codes: Vec<(String, CodeEntry)> = table
        .unique_code()
        .iter()
        .map(|(code, entry)| (code, *entry))
        .collect();
    let wanted: BTreeMap<String, CodeEntry> = model
        .values()
        .filter_map(|row| Some((row.code.clone()?, (row.id, (row.customer, row.state)))))
        .collect();
    assert_eq!(codes, wanted.into_iter().collect::<Vec<_>>());
}

fn rebuilt(model: &BTreeMap<u32, Order>, seed: u64) -> Table<Order> {
    let mut rows: Vec<Order> = model.values().cloned().collect();
    Rng::new(seed).shuffle(&mut rows);
    let mut table: Table<Order> = Table::new();
    for row in rows {
        table.insert(row);
    }
    table
}

fn same_form(a: &Table<Order>, b: &Table<Order>) {
    assert!(a == b);
    assert_eq!(a.rows.root_hash(), b.rows.root_hash());
    assert_eq!(a.by_customer.root_hash(), b.by_customer.root_hash());
    assert_eq!(a.by_total.root_hash(), b.by_total.root_hash());
    assert_eq!(a.by_coupon.root_hash(), b.by_coupon.root_hash());
    assert_eq!(a.by_tags.root_hash(), b.by_tags.root_hash());
    assert_eq!(a.unique_code.root_hash(), b.unique_code.root_hash());
    assert_eq!(a.by_day.root_hash(), b.by_day.root_hash());
    assert_eq!(a.by_customer_day.root_hash(), b.by_customer_day.root_hash());
    assert_eq!(
        a.unique_shop_number.root_hash(),
        b.unique_shop_number.root_hash()
    );
}

#[derive(Clone, Debug)]
enum Change {
    State(State),
    Total(i64),
    Note(&'static str),
    Customer(u8),
    Day(u8),
    Coupon(Option<String>),
    Tags(Vec<String>),
    Code(Option<String>),
    Slot(u8, u8),
    Key(u32),
}

fn apply(row: &mut Order, change: &Change) {
    match change {
        Change::State(state) => row.state = *state,
        Change::Total(total) => row.total = *total,
        Change::Note(note) => row.note = note.to_string(),
        Change::Customer(customer) => row.customer = *customer,
        Change::Day(day) => row.day = *day,
        Change::Coupon(coupon) => row.coupon = coupon.clone(),
        Change::Tags(tags) => row.tags = tags.clone(),
        Change::Code(code) => row.code = code.clone(),
        Change::Slot(shop, number) => {
            row.shop = *shop;
            row.number = *number;
        }
        Change::Key(id) => row.id = *id,
    }
}

#[derive(Clone, Debug)]
enum Op {
    Insert(Order),
    Remove(u32),
    Update(u32, Vec<Change>),
    Commit,
    Reopen,
}

fn state() -> impl Strategy<Value = State> {
    prop_oneof![Just(State::Open), Just(State::Paid), Just(State::Void)]
}

fn coupon() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        Just(Some("a".to_string())),
        Just(Some("b".to_string()))
    ]
}

fn tags() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop_oneof![Just("x"), Just("y"), Just("z")].prop_map(str::to_string),
        0..4,
    )
}

fn code() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        2 => Just(None),
        1 => Just(Some("p".to_string())),
        1 => Just(Some("q".to_string())),
        1 => Just(Some("r".to_string())),
    ]
}

fn row_strategy() -> impl Strategy<Value = Order> {
    (
        (
            0..IDS,
            0..4u8,
            prop::sample::select(TOTALS.to_vec()),
            coupon(),
            tags(),
        ),
        (
            code(),
            0..4u8,
            state(),
            prop::sample::select(NOTES.to_vec()),
            0..2u8,
            0..10u8,
        ),
    )
        .prop_map(
            |((id, customer, total, coupon, tags), (code, day, state, note, shop, number))| Order {
                id,
                customer,
                total,
                coupon,
                tags,
                code,
                day,
                state,
                note: note.to_string(),
                shop,
                number,
            },
        )
}

fn change() -> impl Strategy<Value = Change> {
    prop_oneof![
        4 => state().prop_map(Change::State),
        4 => prop::sample::select(TOTALS.to_vec()).prop_map(Change::Total),
        3 => prop::sample::select(NOTES.to_vec()).prop_map(Change::Note),
        2 => (0..4u8).prop_map(Change::Customer),
        2 => (0..4u8).prop_map(Change::Day),
        2 => coupon().prop_map(Change::Coupon),
        2 => tags().prop_map(Change::Tags),
        2 => code().prop_map(Change::Code),
        1 => (0..2u8, 0..10u8).prop_map(|(shop, number)| Change::Slot(shop, number)),
        1 => (0..IDS).prop_map(Change::Key),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => row_strategy().prop_map(Op::Insert),
        3 => (0..IDS).prop_map(Op::Remove),
        10 => (0..IDS, prop::collection::vec(change(), 0..3)).prop_map(|(id, c)| Op::Update(id, c)),
        2 => Just(Op::Commit),
        1 => Just(Op::Reopen),
    ]
}

fn clashes(model: &BTreeMap<u32, Order>, row: &Order, moving: Option<u32>) -> bool {
    model.values().any(|other| {
        other.id != row.id
            && Some(other.id) != moving
            && ((row.code.is_some() && other.code == row.code)
                || (other.shop, other.number) == (row.shop, row.number))
    })
}

fn run(ops: Vec<Op>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.ferro");
    let mut db: Db<Root> = fast().open(&path).unwrap();
    let mut table: Table<Order> = Table::new();
    let mut model: BTreeMap<u32, Order> = BTreeMap::new();
    let mut committed = model.clone();
    for (step, op) in ops.into_iter().enumerate() {
        match op {
            Op::Insert(row) => {
                let clash = clashes(&model, &row, None);
                let done = table.try_insert(row.clone());
                assert_eq!(done.is_err(), clash);
                if !clash {
                    model.insert(row.id, row);
                }
            }
            Op::Remove(id) => {
                let old = table.remove(&id);
                assert_eq!(old.map(|row| (*row).clone()), model.remove(&id));
            }
            Op::Update(id, changes) => {
                let Some(mut row) = model.get(&id).cloned() else {
                    assert_eq!(table.try_update(&id, |_| {}), Ok(false));
                    continue;
                };
                for change in &changes {
                    apply(&mut row, change);
                }
                let clash = clashes(&model, &row, Some(id));
                let done = table.try_update(&id, |stored| {
                    for change in &changes {
                        apply(stored, change);
                    }
                });
                assert_eq!(done.is_err(), clash);
                if !clash {
                    model.remove(&id);
                    model.insert(row.id, row);
                }
            }
            Op::Commit => {
                let snapshot = table.clone();
                db.update(|root| root.orders = snapshot).unwrap();
                committed = model.clone();
                check(&db.read().orders, &committed);
            }
            Op::Reopen => {
                drop(std::mem::take(&mut table));
                drop(db);
                db = fast().open(&path).unwrap();
                table = db.read().orders.clone();
                model = committed.clone();
            }
        }
        check(&table, &model);
        if step % 4 == 0 {
            same_form(&table, &rebuilt(&model, step as u64));
        }
    }
    same_form(&table, &rebuilt(&model, 99));
    let snapshot = table.clone();
    db.update(|root| root.orders = snapshot).unwrap();
    drop(table);
    drop(db);
    let db: Db<Root> = fast().open(&path).unwrap();
    check(&db.read().orders, &model);
    same_form(&db.read().orders, &rebuilt(&model, 7));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(64)))]

    #[test]
    fn covered_indexes_match_the_rows(ops in prop::collection::vec(op(), 1..50)) {
        run(ops);
    }
}

#[test]
fn covered_fields_are_stored_beside_each_entry() {
    let mut table: Table<Order> = Table::new();
    table.insert(order(1, 7, 250, State::Open));
    table.insert(order(2, 7, 90, State::Paid));
    table.insert(order(3, 8, 10, State::Open));
    let stored: Vec<((u8, u32), CustomerCover)> = table
        .by_customer
        .iter()
        .map(|(entry, cover)| (entry, (*cover).clone()))
        .collect();
    assert_eq!(
        stored,
        vec![
            ((7, 1), (State::Open, 250, String::new())),
            ((7, 2), (State::Paid, 90, String::new())),
            ((8, 3), (State::Open, 10, String::new())),
        ]
    );
    let page: Vec<(u32, (State, i64, String))> = table.by_customer(&7).covered().collect();
    assert_eq!(
        page,
        vec![
            (1, (State::Open, 250, String::new())),
            (2, (State::Paid, 90, String::new()))
        ]
    );
    let totals: Vec<(u32, i64)> = table.by_total_range(50..).covered().collect();
    assert_eq!(totals, vec![(2, 90), (1, 250)]);
}

#[test]
fn an_index_covering_only_its_own_fields_stores_nothing_more() {
    let mut table: Table<Order> = Table::new();
    for id in 0..30 {
        table.insert(order(id, (id % 3) as u8, i64::from(id) * 3, State::Open));
    }
    let _: &Map<(i64, u32), ()> = &table.by_total;
    let _: &Map<(u8, u32), ()> = &table.by_day;
    let mut plain: Table<PlainTotal> = Table::new();
    for id in 0..30 {
        plain.insert(PlainTotal {
            id,
            total: i64::from(id) * 3,
        });
    }
    assert_eq!(table.by_total.root_hash(), plain.by_total.root_hash());
}

#[test]
fn a_change_to_a_covered_field_rewrites_only_the_indexes_that_cover_it() {
    let mut table: Table<Order> = Table::new();
    for id in 0..20 {
        let mut row = order(id, (id % 4) as u8, i64::from(id), State::Open);
        row.coupon = Some("a".into());
        row.tags = vec!["x".into()];
        row.code = Some(format!("c{id}"));
        table.insert(row);
    }
    let before = table.clone();
    table.update(&5, |row| row.note = "noted".into());
    assert_ne!(
        table.by_customer.root_hash(),
        before.by_customer.root_hash()
    );
    assert_eq!(table.by_total.root_hash(), before.by_total.root_hash());
    assert_eq!(table.by_day.root_hash(), before.by_day.root_hash());
    assert_eq!(table.by_coupon.root_hash(), before.by_coupon.root_hash());
    assert_eq!(table.by_tags.root_hash(), before.by_tags.root_hash());
    assert_eq!(
        table.unique_code.root_hash(),
        before.unique_code.root_hash()
    );
    assert_eq!(
        table.by_customer_day.root_hash(),
        before.by_customer_day.root_hash()
    );
    assert_eq!(table.covered_by_code("c5"), Some((5, (1, State::Open))));
    let note: Vec<String> = table
        .by_customer(&1)
        .covered()
        .map(|(_, (_, _, note))| note)
        .collect();
    assert_eq!(note, vec!["", "noted", "", "", ""]);

    let before = table.clone();
    table.update(&5, |row| row.state = State::Paid);
    assert_ne!(
        table.by_customer.root_hash(),
        before.by_customer.root_hash()
    );
    assert_ne!(table.by_coupon.root_hash(), before.by_coupon.root_hash());
    assert_ne!(table.by_tags.root_hash(), before.by_tags.root_hash());
    assert_ne!(
        table.unique_code.root_hash(),
        before.unique_code.root_hash()
    );
    assert_ne!(
        table.by_customer_day.root_hash(),
        before.by_customer_day.root_hash()
    );
    assert_eq!(table.by_total.root_hash(), before.by_total.root_hash());
    assert_eq!(table.covered_by_code("c5"), Some((5, (1, State::Paid))));
    assert_eq!(table.by_tags("x").covered().nth(5), Some((5, State::Paid)));

    let unchanged = table.clone();
    table.update(&5, |row| row.state = State::Paid);
    same_form(&table, &unchanged);
}

#[test]
fn covered_and_plain_lookups_share_a_type_through_uncovered() {
    let mut table: Table<Order> = Table::new();
    let mut row = order(1, 2, 3, State::Open);
    row.tags = vec!["x".into()];
    table.insert(row);
    table.insert(order(2, 2, 4, State::Void));
    let mut sources: Vec<IndexIter<Order>> = vec![
        table.by_customer(&2).uncovered(),
        table.by_tags("x").uncovered(),
        table.by_day(&0),
    ];
    sources.sort_by_key(|source| source.len());
    assert_eq!(sources.remove(0).next().unwrap().id, 1);
    let covered: Vec<(u32, ())> = table.by_day(&0).covered().collect();
    assert_eq!(covered, vec![(1, ()), (2, ())]);
}

#[test]
fn covered_unique_indexes_still_refuse_a_clash() {
    let mut table: Table<Order> = Table::new();
    let mut first = order(1, 1, 10, State::Open);
    first.code = Some("p".into());
    table.insert(first);
    let mut second = order(2, 1, 20, State::Open);
    second.code = Some("p".into());
    let taken = table.try_insert(second.clone()).unwrap_err();
    assert_eq!((taken.index, taken.holder), ("code", 1));
    second.code = None;
    second.number = 1;
    let taken = table.try_insert(second).unwrap_err();
    assert_eq!((taken.index, taken.holder), ("shop_number", 1));
    assert_eq!(table.try_update(&1, |row| row.number = 9), Ok(true));
    assert_eq!(table.covered_by_shop_number(&(0, 9)), Some((1, 10)));
    assert_eq!(table.covered_by_shop_number(&(0, 1)), None);
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct PlainTotal {
    #[key]
    id: u32,
    #[index]
    total: i64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct OwnTotal {
    #[key]
    id: u32,
    #[index(cover(total))]
    total: i64,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct PlainTotals {
    orders: Table<PlainTotal>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct OwnTotals {
    orders: Table<OwnTotal>,
}

#[test]
fn covering_an_index_with_its_own_field_reads_the_plain_index_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("own.ferro");
    {
        let db: Db<PlainTotals> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..50 {
                root.orders.insert(PlainTotal {
                    id,
                    total: i64::from(id % 7),
                });
            }
        })
        .unwrap();
    }
    let db: Db<OwnTotals> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(!read.orders.by_total.is_dirty());
    let wanted: Vec<(u32, i64)> = (0..50u32)
        .filter(|id| id % 7 == 3)
        .map(|id| (id, 3))
        .collect();
    assert_eq!(
        read.orders.by_total(&3).covered().collect::<Vec<_>>(),
        wanted
    );
}

macro_rules! version {
    ($record:ident, $root:ident, $($attr:tt)*) => {
        #[derive(Data, Record, Clone, Debug, PartialEq)]
        struct $record {
            #[key]
            id: u32,
            $($attr)*
            customer: u8,
            #[unique]
            code: String,
            total: i64,
            note: String,
        }

        #[derive(Data, Clone, Debug, Default, PartialEq)]
        struct $root {
            orders: Table<$record>,
        }

        impl Line for $record {
            fn line(&self) -> (u32, u8, i64, String) {
                (self.id, self.customer, self.total, self.note.clone())
            }
        }
    };
}

trait Line {
    fn line(&self) -> (u32, u8, i64, String);
}

version!(Bare, BareRoot, #[index]);
version!(Totals, TotalsRoot, #[index(cover(total))]);
version!(TotalsNotes, TotalsNotesRoot, #[index(cover(total, note))]);
version!(NotesTotals, NotesTotalsRoot, #[index(cover(note, total))]);
version!(Notes, NotesRoot, #[index(cover(note))]);

fn lines<T: Line>(rows: impl Iterator<Item = ferro::Val<T>>) -> Vec<(u32, u8, i64, String)> {
    rows.map(|row| row.line()).collect()
}

fn fill(path: &std::path::Path) -> Vec<(u32, u8, i64, String)> {
    let db: Db<BareRoot> = fast().open(path).unwrap();
    db.update(|root| {
        for id in 0..60 {
            root.orders.insert(Bare {
                id,
                customer: (id % 5) as u8,
                code: format!("c{id}"),
                total: i64::from(id) * 11,
                note: format!("n{}", id % 3),
            });
        }
    })
    .unwrap();
    lines(db.read().orders.iter())
}

#[test]
fn adding_changing_and_removing_a_cover_rebuilds_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evolve.ferro");
    let mut rows = fill(&path);
    let customer = |rows: &[(u32, u8, i64, String)], c: u8| -> Vec<(u32, u8, i64, String)> {
        rows.iter().filter(|row| row.1 == c).cloned().collect()
    };

    {
        let db: Db<TotalsRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert!(read.orders.by_customer.is_dirty());
        assert_eq!(read.orders.key_by_code("c33"), Some(33));
        let page: Vec<(u32, i64)> = read.orders.by_customer(&2).covered().collect();
        let wanted: Vec<(u32, i64)> = customer(&rows, 2)
            .iter()
            .map(|row| (row.0, row.2))
            .collect();
        assert_eq!(page, wanted);
        drop(read);
        db.update(|root| {
            root.orders.update(&7, |row| row.total = -1);
        })
        .unwrap();
        rows[7].2 = -1;
    }
    {
        let db: Db<TotalsRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert!(!read.orders.by_customer.is_dirty());
        assert_eq!(read.orders.by_customer(&2).covered().nth(1), Some((7, -1)));
    }
    {
        let db: Db<TotalsNotesRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert!(read.orders.by_customer.is_dirty());
        let page: Vec<(u32, (i64, String))> = read.orders.by_customer(&2).covered().collect();
        let wanted: Vec<(u32, (i64, String))> = customer(&rows, 2)
            .iter()
            .map(|row| (row.0, (row.2, row.3.clone())))
            .collect();
        assert_eq!(page, wanted);
        assert_eq!(lines(read.orders.by_customer(&2)), customer(&rows, 2));
        drop(read);
        db.update(|root| {
            root.orders.update(&12, |row| row.note = "moved".into());
        })
        .unwrap();
        rows[12].3 = "moved".into();
    }
    {
        let db: Db<NotesTotalsRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert!(read.orders.by_customer.is_dirty());
        let page: Vec<(u32, (String, i64))> = read.orders.by_customer(&2).covered().collect();
        let wanted: Vec<(u32, (String, i64))> = customer(&rows, 2)
            .iter()
            .map(|row| (row.0, (row.3.clone(), row.2)))
            .collect();
        assert_eq!(page, wanted);
        assert_eq!(page[2], (12, ("moved".to_string(), 132)));
    }
    {
        let db: Db<NotesRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert!(read.orders.by_customer.is_dirty());
        let page: Vec<(u32, String)> = read.orders.by_customer(&2).covered().collect();
        let wanted: Vec<(u32, String)> = customer(&rows, 2)
            .iter()
            .map(|row| (row.0, row.3.clone()))
            .collect();
        assert_eq!(page, wanted);
        drop(read);
        db.update(|root| {
            root.orders.remove(&2);
        })
        .unwrap();
        rows.retain(|row| row.0 != 2);
    }
    {
        let db: Db<BareRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert!(read.orders.by_customer.is_dirty());
        assert_eq!(lines(read.orders.iter()), rows);
        for c in 0..5 {
            assert_eq!(lines(read.orders.by_customer(&c)), customer(&rows, c));
        }
        assert_eq!(
            read.orders
                .by_customer_counts()
                .iter()
                .map(|(_, n)| n)
                .sum::<usize>(),
            59
        );
    }
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Slug {
    #[key]
    id: u32,
    #[unique]
    slug: String,
    rank: u16,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct RankedSlug {
    #[key]
    id: u32,
    #[unique(cover(rank))]
    slug: String,
    rank: u16,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct SlugRoot {
    rows: Table<Slug>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct RankedSlugRoot {
    rows: Table<RankedSlug>,
}

#[test]
fn covering_a_unique_index_rebuilds_it_and_keeps_the_constraint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("slug.ferro");
    {
        let db: Db<SlugRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..40 {
                root.rows.insert(Slug {
                    id,
                    slug: format!("s{id}"),
                    rank: (id * 3) as u16,
                });
            }
        })
        .unwrap();
    }
    {
        let db: Db<RankedSlugRoot> = fast().open(&path).unwrap();
        assert!(db.read().rows.unique_slug.is_dirty());
        assert_eq!(db.read().rows.covered_by_slug("s9"), Some((9, 27)));
        db.update(|root| {
            root.rows.update(&9, |row| row.rank = 1);
            let clash = root.rows.try_update(&10, |row| row.slug = "s9".into());
            assert_eq!(clash.map_err(|taken| taken.holder), Err(9));
        })
        .unwrap();
    }
    let db: Db<RankedSlugRoot> = fast().open(&path).unwrap();
    assert!(!db.read().rows.unique_slug.is_dirty());
    assert_eq!(db.read().rows.covered_by_slug("s9"), Some((9, 1)));
    assert_eq!(db.read().rows.covered_by_slug("s10"), Some((10, 30)));
    drop(db);
    let db: Db<SlugRoot> = fast().open(&path).unwrap();
    assert!(db.read().rows.unique_slug.is_dirty());
    assert_eq!(db.read().rows.key_by_slug("s9"), Some(9));
    db.update(|root| {
        let clash = root.rows.try_insert(Slug {
            id: 99,
            slug: "s10".into(),
            rank: 0,
        });
        assert_eq!(clash.map_err(|taken| taken.holder), Err(10));
    })
    .unwrap();
}
