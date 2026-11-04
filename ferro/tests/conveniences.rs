mod common;

use std::collections::BTreeMap;

use common::{Rng, fast};
use ferro::table::Taken;
use ferro::{Data, Map, Record, Set, Summary, Table, Val};

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Seat {
    #[key]
    id: u32,
    #[unique]
    label: String,
    #[index]
    row: u8,
    sold: bool,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Hall {
    seats: Table<Seat>,
    prices: Map<u32, u64>,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Stay {
    #[key]
    slot: (u64, u32, u8),
    guest: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Data)]
#[ferro(transparent)]
struct Total(u64);

impl Summary<(u8, u16), u32> for Total {
    fn of(_key: &(u8, u16), value: &u32) -> Self {
        Total(u64::from(*value))
    }

    fn combine(&self, other: &Self) -> Self {
        Total(self.0 + other.0)
    }
}

fn seat(id: u32) -> Seat {
    Seat {
        id,
        label: format!("s{id}"),
        row: (id % 3) as u8,
        sold: false,
    }
}

fn hall() -> Hall {
    let mut hall = Hall::default();
    for id in 1..=9 {
        hall.seats.insert(seat(id));
        hall.prices.insert(id, u64::from(id) * 100);
    }
    hall
}

fn ids(rows: impl Iterator<Item = Val<Seat>>) -> Vec<u32> {
    rows.map(|row| row.id).collect()
}

#[test]
fn map_update_returns_what_the_closure_returns() {
    let mut hall = hall();
    let raised = hall.prices.update(&4, |price| {
        *price += 50;
        *price
    });
    assert_eq!(raised, Some(450));
    assert_eq!(hall.prices.get(&4).map(|price| *price), Some(450));
    assert_eq!(hall.prices.update(&40, |price| *price), None);
    assert_eq!(hall.prices.len(), 9);
}

#[test]
fn table_update_returns_what_the_closure_returns_and_reindexes() {
    let mut hall = hall();
    let moved = hall.seats.update(&5, |seat| {
        seat.row = 7;
        seat.label.clone()
    });
    assert_eq!(moved.as_deref(), Some("s5"));
    assert_eq!(ids(hall.seats.by_row(&7)), vec![5]);
    assert!(!ids(hall.seats.by_row(&2)).contains(&5));
    assert_eq!(hall.seats.update(&50, |seat| seat.row), None);
}

#[test]
fn try_update_returns_the_closure_value_or_the_clash() {
    let mut hall = hall();
    let renamed = hall.seats.try_update(&2, |seat| {
        seat.label = "aisle".to_string();
        seat.id
    });
    assert_eq!(renamed, Ok(Some(2)));
    assert_eq!(hall.seats.by_label("aisle").map(|seat| seat.id), Some(2));
    let clash = hall
        .seats
        .try_update(&3, |seat| seat.label = "aisle".to_string());
    assert_eq!(
        clash,
        Err(Taken {
            index: "label",
            holder: 2
        })
    );
    assert_eq!(
        hall.seats.get(&3).map(|seat| seat.label.clone()),
        Some("s3".to_string())
    );
    assert_eq!(hall.seats.try_update(&30, |seat| seat.id), Ok(None));
}

#[derive(Debug, PartialEq)]
enum Refused {
    Sold(u32),
    Missing(u32),
    Db(String),
}

impl From<ferro::Error> for Refused {
    fn from(error: ferro::Error) -> Self {
        Refused::Db(error.to_string())
    }
}

#[test]
fn a_closure_that_fails_inside_update_discards_the_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Hall>(dir.path().join("h.ferro")).unwrap();
    db.update(|root| *root = hall()).unwrap();
    let before = db.version();
    let sell = |id: u32| {
        db.try_update(|hall| {
            hall.prices.insert(id, 0);
            hall.seats
                .update(&id, |seat| {
                    let was = seat.sold;
                    seat.sold = true;
                    match was {
                        true => Err(Refused::Sold(id)),
                        false => Ok(seat.label.clone()),
                    }
                })
                .ok_or(Refused::Missing(id))?
        })
    };
    assert_eq!(sell(6), Ok("s6".to_string()));
    assert_eq!(db.version(), before + 1);
    assert_eq!(sell(6), Err(Refused::Sold(6)));
    assert_eq!(sell(60), Err(Refused::Missing(60)));
    assert_eq!(db.version(), before + 1);
    let now = db.read();
    assert!(now.seats.get(&6).unwrap().sold);
    assert_eq!(now.prices.get(&60), None);
}

#[test]
fn table_range_yields_rows_as_iter_does() {
    let hall = hall();
    assert_eq!(ids(hall.seats.range(3..6)), vec![3, 4, 5]);
    assert_eq!(ids(hall.seats.range(..=2).rev()), vec![2, 1]);
    assert_eq!(hall.seats.range(4..).len(), 6);
    let mut tail = hall.seats.range(7..);
    assert_eq!(tail.next_back().map(|seat| seat.id), Some(9));
    assert_eq!(tail.len(), 2);
    assert_eq!(
        ids(hall.seats.range(..)),
        ids(hall.seats.iter()),
        "a full range is the table"
    );
}

#[test]
fn table_keys_lists_the_keys_in_order() {
    let mut hall = hall();
    hall.seats.remove(&4);
    let keys: Vec<u32> = hall.seats.keys().collect();
    assert_eq!(keys, vec![1, 2, 3, 5, 6, 7, 8, 9]);
    assert_eq!(hall.seats.keys().rev().next(), Some(9));
    assert_eq!(hall.seats.keys().len(), hall.seats.len());
    assert_eq!(Table::<Seat>::new().keys().next(), None);
}

#[test]
fn map_prefix_matches_a_range_over_the_other_fields() {
    let mut rng = Rng::new(66);
    let mut model: BTreeMap<(u8, u16), u32> = BTreeMap::new();
    let mut map: Map<(u8, u16), u32, Total> = Map::default();
    for _ in 0..3_000 {
        let key = (
            rng.below(12) as u8 * 2,
            rng.below(u64::from(u16::MAX) + 1) as u16,
        );
        let value = rng.below(1_000) as u32;
        model.insert(key, value);
        map.insert(key, value);
    }
    for head in 0..=24u8 {
        let want: Vec<((u8, u16), u32)> = model
            .range((head, 0)..=(head, u16::MAX))
            .map(|(key, value)| (*key, *value))
            .collect();
        let got: Vec<((u8, u16), u32)> = map
            .prefix(&head)
            .map(|(key, value)| (key, *value))
            .collect();
        assert_eq!(got, want, "head {head}");
        let back: Vec<(u8, u16)> = map.prefix(&head).rev().map(|(key, _)| key).collect();
        assert!(back.iter().rev().eq(want.iter().map(|(key, _)| key)));
        assert_eq!(map.prefix(&head).len(), want.len());
        let total: u64 = want.iter().map(|(_, value)| u64::from(*value)).sum();
        assert_eq!(
            map.prefix_summary(&head).map(|sum| sum.0),
            (!want.is_empty()).then_some(total)
        );
    }
}

#[test]
fn set_and_table_prefixes_take_a_borrowed_head() {
    let mut queues: Set<(String, u8, u64)> = Set::new();
    for (queue, priority, id) in [
        ("mail", 1, 4),
        ("mail", 0, 9),
        ("mailbox", 0, 1),
        ("ma", 3, 2),
    ] {
        queues.insert((queue.to_string(), priority, id));
    }
    let mail: Vec<u64> = queues.prefix("mail").map(|key| key.2).collect();
    assert_eq!(mail, vec![9, 4]);
    assert_eq!(queues.prefix("mailbox").len(), 1);
    assert_eq!(queues.prefix("m").next(), None);
    assert_eq!(queues.prefix("ma").next_back().map(|key| key.2), Some(2));

    let mut stays: Table<Stay> = Table::new();
    for booking in [3u64, 1, 2] {
        for night in [20, 10, 30] {
            stays.insert(Stay {
                slot: (booking, night, 0),
                guest: format!("g{booking}"),
            });
        }
    }
    let nights: Vec<u32> = stays.prefix(&2).map(|stay| stay.slot.1).collect();
    assert_eq!(nights, vec![10, 20, 30]);
    assert!(stays.prefix(&2).all(|stay| stay.guest == "g2"));
    assert_eq!(stays.prefix(&9).len(), 0);

    let mut wide: Map<(u32, u8, u8, u8), ()> = Map::new();
    wide.insert((7, 1, 2, 3), ());
    wide.insert((8, 0, 0, 0), ());
    let sevens: Vec<_> = wide.prefix(&7).map(|(key, _)| key).collect();
    assert_eq!(sevens, vec![(7, 1, 2, 3)]);
}
