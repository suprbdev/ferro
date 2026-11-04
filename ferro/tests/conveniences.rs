mod common;

use common::fast;
use ferro::table::Taken;
use ferro::{Data, Map, Record, Table, Val};

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
