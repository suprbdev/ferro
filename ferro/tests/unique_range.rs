mod common;

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::Bound;

use common::fast;
use ferro::table::{IndexIter, IntoMatches};
use ferro::{Data, Db, Record, Table};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[unique(shelf, slot)]
#[unique(room, night, cover(guest, total))]
struct Stay {
    #[key]
    id: u16,
    #[index]
    owner: u8,
    #[unique]
    slug: String,
    #[unique]
    code: Option<u8>,
    #[unique(cover(owner, memo))]
    serial: Option<u16>,
    #[unique(cover(total))]
    ticket: u32,
    shelf: u8,
    slot: u8,
    room: u8,
    night: u16,
    guest: String,
    total: i64,
    memo: String,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    stays: Table<Stay>,
}

type Model = BTreeMap<u16, Stay>;

const IDS: u16 = 400;
const OWNERS: u8 = 4;
const SHELVES: u8 = 5;
const ROOMS: u8 = 6;

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

fn bounds<V: Clone>(values: &[V]) -> Vec<(Bound<V>, Bound<V>)> {
    let mut out = vec![(Bound::Unbounded, Bound::Unbounded)];
    for a in values {
        out.push((Bound::Included(a.clone()), Bound::Unbounded));
        out.push((Bound::Excluded(a.clone()), Bound::Unbounded));
        out.push((Bound::Unbounded, Bound::Included(a.clone())));
        out.push((Bound::Unbounded, Bound::Excluded(a.clone())));
        for b in values {
            out.push((Bound::Included(a.clone()), Bound::Included(b.clone())));
            out.push((Bound::Included(a.clone()), Bound::Excluded(b.clone())));
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

fn zigzag<I: DoubleEndedIterator + ExactSizeIterator>(mut items: I) -> Vec<I::Item> {
    let mut front = Vec::new();
    let mut back = Vec::new();
    while items.len() > 0 {
        front.extend(items.next());
        back.extend(items.next_back());
    }
    back.reverse();
    front.extend(back);
    front
}

struct Unique<'a, V, C> {
    model: &'a Model,
    value: fn(&Stay) -> Option<V>,
    cover: fn(&Stay) -> C,
}

impl<V: Ord + Clone + Debug, C: PartialEq + Debug + 'static> Unique<'_, V, C> {
    fn wanted(&self, keep: impl Fn(&V) -> bool) -> Vec<(u16, C)> {
        let mut entries: BTreeMap<V, (u16, C)> = BTreeMap::new();
        for row in self.model.values() {
            if let Some(value) = (self.value)(row) {
                let held = entries.insert(value, (row.id, (self.cover)(row)));
                assert!(held.is_none(), "two rows of the model share a value");
            }
        }
        entries
            .into_iter()
            .filter(|(value, _)| keep(value))
            .map(|(_, entry)| entry)
            .collect()
    }

    fn agree(&self, wanted: &[(u16, C)], lookup: impl Fn() -> IndexIter<Stay, C>, what: &str) {
        let keys: Vec<u16> = wanted.iter().map(|(id, _)| *id).collect();
        let rows: Vec<Stay> = lookup().map(|row| (*row).clone()).collect();
        let held: Vec<&Stay> = keys.iter().map(|id| &self.model[id]).collect();
        assert_eq!(shown(&rows), shown(&held), "rows {what}");

        let mut reversed: Vec<u16> = lookup().rev().map(|row| row.id).collect();
        reversed.reverse();
        assert_eq!(reversed, keys, "reversed rows {what}");
        let both: Vec<u16> = zigzag(lookup()).iter().map(|row| row.id).collect();
        assert_eq!(both, keys, "rows from both ends {what}");
        assert_eq!(lookup().len(), wanted.len(), "len {what}");

        assert_eq!(lookup().keys().collect::<Vec<_>>(), keys, "keys {what}");
        let mut back: Vec<u16> = lookup().keys().rev().collect();
        back.reverse();
        assert_eq!(back, keys, "reversed keys {what}");
        assert_eq!(zigzag(lookup().keys()), keys, "keys from both ends {what}");

        let covered: Vec<(u16, C)> = lookup().covered().collect();
        assert_eq!(covered, wanted, "covered {what}");
        let mut back: Vec<(u16, C)> = lookup().covered().rev().collect();
        back.reverse();
        assert_eq!(back, wanted, "reversed covered {what}");
        assert_eq!(
            zigzag(lookup().covered()),
            wanted,
            "covered from both ends {what}"
        );
        let read: Vec<(u16, C)> = lookup().map(|row| (row.id, (self.cover)(&row))).collect();
        assert_eq!(read, wanted, "covered fields of the rows {what}");

        let plain: Vec<u16> = lookup().uncovered().map(|row| row.id).collect();
        assert_eq!(plain, keys, "uncovered {what}");
        let mut back: Vec<u16> = lookup()
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
            assert_eq!(mixed.size_hint(), (rest.len(), Some(rest.len())), "{what}");
            let tail: Vec<(u16, C)> = mixed.covered().collect();
            assert_eq!(tail, rest, "front then covered {what}");
        }
        if let Some(((last, _), front)) = wanted.split_last() {
            let mut mixed = lookup();
            assert_eq!(mixed.next_back().map(|row| row.id), Some(*last), "{what}");
            let head: Vec<u16> = mixed.keys().collect();
            assert_eq!(head, keys[..front.len()], "back then keys {what}");
        }
    }

    fn ranges(&self, probes: &[V], range: impl Fn((Bound<V>, Bound<V>)) -> IndexIter<Stay, C>) {
        for (low, high) in bounds(probes) {
            let wanted = self.wanted(|value| within(value, &low, &high));
            self.agree(
                &wanted,
                || range((low.clone(), high.clone())),
                &format!("range {low:?} {high:?}"),
            );
        }
    }
}

fn slugs() -> Vec<String> {
    ["", "a", "abc", "b", "bd", "cccc", "d", "dddd", "e"]
        .map(String::from)
        .to_vec()
}

fn borrowed(bound: &Bound<String>) -> Bound<&str> {
    match bound {
        Bound::Included(slug) => Bound::Included(slug.as_str()),
        Bound::Excluded(slug) => Bound::Excluded(slug.as_str()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn check(table: &Table<Stay>, model: &Model) {
    let rows: Vec<Stay> = table.iter().map(|row| (*row).clone()).collect();
    assert_eq!(shown(&rows), shown(&model.values().collect::<Vec<_>>()));

    let by_slug = Unique {
        model,
        value: |row| Some(row.slug.clone()),
        cover: |_| (),
    };
    by_slug.ranges(&slugs(), |range| table.by_slug_range::<String, _>(range));
    by_slug.ranges(&slugs(), |(low, high)| {
        table.by_slug_range::<str, _>((borrowed(&low), borrowed(&high)))
    });

    let by_code = Unique {
        model,
        value: |row| row.code,
        cover: |_| (),
    };
    by_code.ranges(&[0, 1, 99, 100, 199, 255], |range| {
        table.by_code_range::<u8, _>(range)
    });
    let coded = model.values().filter(|row| row.code.is_some()).count();
    assert_eq!(table.by_code_range::<u8, _>(..).len(), coded);

    let by_serial = Unique {
        model,
        value: |row| row.serial,
        cover: |row| (row.owner, row.memo.clone()),
    };
    by_serial.ranges(&[0, 10, 500, 999, 1000], |range| {
        table.by_serial_range::<u16, _>(range)
    });

    let by_ticket = Unique {
        model,
        value: |row| Some(row.ticket),
        cover: |row| row.total,
    };
    by_ticket.ranges(&[0, 1, 1000, 1999, 2000], |range| {
        table.by_ticket_range::<u32, _>(range)
    });

    let by_shelf_slot = Unique {
        model,
        value: |row| Some((row.shelf, row.slot)),
        cover: |_| (),
    };
    by_shelf_slot.ranges(&[(0, 0), (1, 50), (2, 99), (4, 0), (SHELVES, 0)], |range| {
        table.by_shelf_slot_range::<(u8, u8), _>(range)
    });
    for shelf in 0..=SHELVES {
        let wanted = by_shelf_slot.wanted(|value| value.0 == shelf);
        by_shelf_slot.agree(
            &wanted,
            || table.by_shelf_slot_prefix(&shelf),
            &format!("shelf {shelf}"),
        );
    }

    let by_room_night = Unique {
        model,
        value: |row| Some((row.room, row.night)),
        cover: |row| (row.guest.clone(), row.total),
    };
    by_room_night.ranges(&[(0, 0), (1, 60), (3, 119), (5, 0), (ROOMS, 0)], |range| {
        table.by_room_night_range::<(u8, u16), _>(range)
    });
    for room in 0..=ROOMS {
        let wanted = by_room_night.wanted(|value| value.0 == room);
        by_room_night.agree(
            &wanted,
            || table.by_room_night_prefix(&room),
            &format!("room {room}"),
        );
        let from = 60;
        let wanted = by_room_night.wanted(|value| value.0 == room && value.1 >= from);
        by_room_night.agree(
            &wanted,
            || table.by_room_night_range((room, from)..=(room, u16::MAX)),
            &format!("room {room} from {from}"),
        );
    }

    combined(table, model);
}

fn sorted(mut keys: Vec<u16>) -> Vec<u16> {
    keys.sort_unstable();
    keys.dedup();
    keys
}

fn combined(table: &Table<Stay>, model: &Model) {
    let find = table.find();
    let slug_ranges = [
        (Bound::Unbounded, Bound::Unbounded),
        (
            Bound::Included("b".to_string()),
            Bound::Excluded("d".to_string()),
        ),
        (Bound::Excluded("cccc".to_string()), Bound::Unbounded),
        (Bound::Included("e".to_string()), Bound::Unbounded),
    ];
    for owner in 0..=OWNERS {
        let owned: Vec<u16> = model
            .values()
            .filter(|row| row.owner == owner)
            .map(|row| row.id)
            .collect();
        let holds = |id: &u16| model[id].owner == owner;
        for (low, high) in &slug_ranges {
            let slug_range = || table.by_slug_range::<String, _>((low.clone(), high.clone()));
            let ranged: Vec<u16> = slug_range().keys().collect();
            let both = sorted(ranged.iter().copied().filter(holds).collect());
            let what = format!("owner {owner} and slugs {low:?} {high:?}");
            let and: Vec<u16> = find.by_owner(&owner).and(slug_range()).keys().collect();
            assert_eq!(and, both, "{what}");
            let mut back: Vec<u16> = find
                .by_owner(&owner)
                .and(slug_range())
                .rev()
                .map(|row| row.id)
                .collect();
            back.reverse();
            assert_eq!(back, both, "reversed {what}");
            let led: Vec<u16> = slug_range()
                .into_matches()
                .and(find.by_owner(&owner))
                .keys()
                .collect();
            assert_eq!(led, both, "led by the range, {what}");
            let either = sorted(
                owned
                    .iter()
                    .copied()
                    .chain(ranged.iter().copied())
                    .collect(),
            );
            let or: Vec<u16> = find.by_owner(&owner).or(slug_range()).keys().collect();
            assert_eq!(or, either, "owner {owner} or slugs {low:?} {high:?}");
        }
        for room in 0..=ROOMS {
            let roomed: Vec<u16> = table.by_room_night_prefix(&room).keys().collect();
            let both = sorted(roomed.into_iter().filter(holds).collect());
            let and: Vec<(u16, (String, i64))> = table
                .by_room_night_prefix(&room)
                .into_matches()
                .and(find.by_owner(&owner))
                .covered()
                .collect();
            let wanted: Vec<(u16, (String, i64))> = both
                .iter()
                .map(|id| (*id, (model[id].guest.clone(), model[id].total)))
                .collect();
            assert_eq!(and, wanted, "room {room} and owner {owner}");
        }
        let tickets = || table.by_ticket_range(500u32..1500);
        let ticketed: Vec<u16> = tickets().keys().collect();
        let both = sorted(ticketed.into_iter().filter(holds).collect());
        let and: Vec<(u16, i64)> = tickets()
            .into_matches()
            .and(find.by_owner(&owner))
            .covered()
            .rev()
            .collect();
        let mut wanted: Vec<(u16, i64)> = both.iter().map(|id| (*id, model[id].total)).collect();
        wanted.reverse();
        assert_eq!(and, wanted, "tickets and owner {owner}");
    }
}

fn slug() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(vec!['a', 'b', 'c', 'd']), 1..5)
        .prop_map(|letters| letters.into_iter().collect())
}

fn stay() -> impl Strategy<Value = Stay> {
    (
        (
            0..IDS,
            0..OWNERS,
            slug(),
            prop::option::weighted(0.6, 0..200u8),
            prop::option::of(0..1000u16),
            0..2000u32,
        ),
        (
            0..SHELVES,
            0..100u8,
            0..ROOMS,
            0..120u16,
            0..4usize,
            -50..50i64,
        ),
    )
        .prop_map(
            |(
                (id, owner, slug, code, serial, ticket),
                (shelf, slot, room, night, which, total),
            )| Stay {
                id,
                owner,
                slug,
                code,
                serial,
                ticket,
                shelf,
                slot,
                room,
                night,
                guest: format!("g{}", id % 7),
                total,
                memo: memo(which),
            },
        )
}

fn clashes(model: &Model, row: &Stay) -> bool {
    model.values().any(|other| {
        other.id != row.id
            && (other.slug == row.slug
                || (row.code.is_some() && other.code == row.code)
                || (row.serial.is_some() && other.serial == row.serial)
                || other.ticket == row.ticket
                || (other.shelf, other.slot) == (row.shelf, row.slot)
                || (other.room, other.night) == (row.room, row.night))
    })
}

fn build(inserts: Vec<Stay>, updates: Vec<(u16, Stay)>, removes: Vec<u16>) -> (Table<Stay>, Model) {
    let mut table: Table<Stay> = Table::new();
    let mut model = Model::new();
    for row in inserts {
        let clash = clashes(&model, &row);
        assert_eq!(table.try_insert(row.clone()).is_err(), clash);
        if !clash {
            model.insert(row.id, row);
        }
    }
    for (id, row) in updates {
        let row = Stay { id, ..row };
        let done = table.try_update(&id, |held| *held = row.clone());
        match model.contains_key(&id) {
            false => assert_eq!(done, Ok(None)),
            true if clashes(&model, &row) => assert!(done.is_err()),
            true => {
                assert_eq!(done, Ok(Some(())));
                model.insert(id, row);
            }
        }
    }
    for id in removes {
        assert_eq!(
            table.remove(&id).map(|row| row.id),
            model.remove(&id).map(|row| row.id)
        );
    }
    (table, model)
}

fn run(inserts: Vec<Stay>, updates: Vec<(u16, Stay)>, removes: Vec<u16>) {
    let (table, model) = build(inserts, updates, removes);
    check(&table, &model);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unique.ferro");
    let db: Db<Root> = fast().open(&path).unwrap();
    db.update(|root| root.stays = table.clone()).unwrap();
    drop(table);
    drop(db);
    let db: Db<Root> = fast().open(&path).unwrap();
    let snapshot = db.read();
    check(&snapshot.stays, &model);
}

proptest! {
    #![proptest_config(config(24, 0xC0DE_0072))]

    #[test]
    fn unique_ranges_and_prefixes_match_a_model_of_the_rows(
        inserts in prop::collection::vec(stay(), 0..300),
        updates in prop::collection::vec((0..IDS, stay()), 0..60),
        removes in prop::collection::vec(0..IDS, 0..40),
    ) {
        run(inserts, updates, removes);
    }
}

#[test]
fn an_empty_table_has_empty_unique_ranges_and_prefixes() {
    let table: Table<Stay> = Table::new();
    assert_eq!(table.by_slug_range::<str, _>(..).len(), 0);
    assert!(table.by_code_range::<u8, _>(..).next().is_none());
    assert!(
        table
            .by_serial_range::<u16, _>(..)
            .covered()
            .next_back()
            .is_none()
    );
    assert!(table.by_ticket_range::<u32, _>(..).keys().next().is_none());
    assert!(table.by_shelf_slot_prefix(&0).next().is_none());
    assert!(table.by_room_night_prefix(&0).covered().next().is_none());
    check(&table, &Model::new());
}

#[test]
fn a_full_range_walks_every_row_that_holds_a_value() {
    let rows = (0..IDS).map(|id| Stay {
        id,
        owner: (id % 4) as u8,
        slug: format!("s{:04}", IDS - id),
        code: (id % 3 == 0).then_some((id / 3) as u8),
        serial: (id % 2 == 1).then_some(id),
        ticket: u32::from(id) * 5,
        shelf: (id / 100) as u8,
        slot: (id % 100) as u8,
        room: (id % ROOMS as u16) as u8,
        night: id / ROOMS as u16,
        guest: format!("g{id}"),
        total: i64::from(id) - 200,
        memo: memo(id as usize),
    });
    let (table, model) = build(rows.collect(), Vec::new(), Vec::new());
    assert_eq!(table.len(), IDS as usize);
    let by_slug: Vec<u16> = table.by_slug_range::<str, _>(..).keys().collect();
    assert_eq!(by_slug, (0..IDS).rev().collect::<Vec<_>>());
    let by_serial: Vec<u16> = table.by_serial_range::<u16, _>(..).keys().collect();
    assert_eq!(
        by_serial,
        (0..IDS).filter(|id| id % 2 == 1).collect::<Vec<_>>()
    );
    let by_code = table.by_code_range::<u8, _>(..).len();
    assert_eq!(by_code, (0..IDS).filter(|id| id % 3 == 0).count());
    check(&table, &model);
}
