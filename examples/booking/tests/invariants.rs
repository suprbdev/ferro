mod common;

use std::sync::{Arc, Barrier};
use std::thread;

use booking::seed::Plan;
use booking::{BookingError, Load, Status, Stay, check_pair, check_state};
use common::{assert_clean, days, world, world_with};

#[test]
fn a_room_night_has_one_holder_and_a_clash_names_it() {
    let w = world();
    let night = w.clock.today() + 10;
    let first = w.hold(1, 1, night, 3, days(1)).unwrap();
    assert_eq!(first.total, 3 * 12_000);

    let clash = w.hold(2, 1, night + 1, 3, days(1)).unwrap_err();
    match clash {
        BookingError::Unavailable { room, holder, .. } => {
            assert_eq!((room, holder), (1, first.id));
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }

    let h = w.svc.read();
    assert_eq!(h.stays.len(), 3, "the refused hold left no partial stays");
    assert_eq!(h.bookings.len(), 1);
    assert_eq!(h.next_booking, first.id);
    assert_eq!(
        h.loads.get(&(1, night + 1)).map(|l| *l),
        Some(Load { held: 1, sold: 0 })
    );
    assert_eq!(h.holder(1, night + 2), Some(first.id));
    assert_eq!(h.holder(1, night + 3), None);
    drop(h);

    let after = w.hold(2, 1, night + 3, 2, days(1)).unwrap();
    assert_eq!(after.id, first.id + 1);
    let next_door = w.hold(2, 2, night, 3, days(1)).unwrap();
    assert_eq!(next_door.total, 3 * 14_500);
    assert_clean(&w.svc);
}

#[test]
fn the_unique_room_night_index_refuses_a_second_stay_directly() {
    let w = world();
    let night = w.clock.today() + 5;
    let held = w.hold(1, 1, night, 1, days(1)).unwrap();
    let refused = w.svc.db().update(|h| {
        h.stays.try_insert(Stay {
            slot: (999, night),
            room: 1,
            night,
            venue: 1,
            sold: false,
        })
    });
    let taken = refused.unwrap().unwrap_err();
    assert_eq!(taken.holder, (held.id, night));
    assert_eq!(taken.index, "room_night");
    assert_eq!(w.svc.read().stays.len(), 1);
}

#[test]
fn names_emails_and_room_numbers_are_unique() {
    let w = world();
    match w.svc.add_customer("guest1@example.com", "Someone Else") {
        Err(BookingError::EmailTaken { holder, .. }) => assert_eq!(holder, 1),
        other => panic!("expected EmailTaken, got {other:?}"),
    }
    assert!(matches!(
        w.svc.add_room(1, 101, 5_000),
        Err(BookingError::RoomExists {
            venue: 1,
            number: 101
        })
    ));
    assert!(matches!(
        w.svc.add_venue("Harbour House", "Porto"),
        Err(BookingError::VenueExists(_))
    ));
    let id = w.svc.add_room(2, 201, 5_000).unwrap();
    assert_eq!(w.svc.read().room_at(2, 201).unwrap().id, id);
    assert_eq!(w.svc.read().next_customer, 5);
}

#[test]
fn confirm_needs_funds_and_the_agreed_price_and_charges_the_quote() {
    let w = world();
    let night = w.clock.today() + 20;
    let held = w.hold(1, 3, night, 14, days(1)).unwrap();
    assert_eq!(held.total, 14 * 17_000);

    match w.svc.confirm(held.id, Some(held.total)) {
        Err(BookingError::InsufficientFunds {
            needed, available, ..
        }) => {
            assert_eq!((needed, available), (238_000, 200_000));
        }
        other => panic!("expected InsufficientFunds, got {other:?}"),
    }
    w.svc.deposit(1, 50_000).unwrap();
    assert!(matches!(
        w.svc.confirm(held.id, Some(held.total - 1)),
        Err(BookingError::PriceChanged { .. })
    ));

    w.svc.set_rate(3, 99_000).unwrap();
    assert_eq!(
        w.svc.confirm(held.id, Some(held.total)).unwrap(),
        held.total
    );
    let h = w.svc.read();
    let customer = h.customers.get(&1).unwrap();
    assert_eq!(customer.credit, 250_000 - held.total);
    assert_eq!(h.till.charged, held.total);
    assert_eq!(
        h.loads.get(&(1, night)).map(|l| *l),
        Some(Load { held: 0, sold: 1 })
    );
    drop(h);
    assert!(matches!(
        w.svc.confirm(held.id, None),
        Err(BookingError::WrongStatus {
            status: Status::Confirmed,
            ..
        })
    ));
    assert_clean(&w.svc);
}

#[test]
fn bad_requests_are_typed_errors_and_change_nothing() {
    let w = world();
    let today = w.clock.today();
    let version = w.svc.db().version();
    assert!(matches!(
        w.hold(1, 1, today, 0, 1),
        Err(BookingError::BadStay { .. })
    ));
    assert!(matches!(
        w.hold(1, 1, today, 15, 1),
        Err(BookingError::BadStay { .. })
    ));
    assert!(matches!(
        w.hold(1, 1, today - 1, 1, 1),
        Err(BookingError::PastNight { .. })
    ));
    assert!(matches!(
        w.hold(77, 1, today, 1, 1),
        Err(BookingError::NoCustomer(_))
    ));
    assert!(matches!(
        w.hold(1, 77, today, 1, 1),
        Err(BookingError::NoRoom(77))
    ));
    assert!(matches!(
        w.svc.confirm(42, None),
        Err(BookingError::NoBooking(42))
    ));
    assert!(matches!(
        w.svc.deposit(1, 0),
        Err(BookingError::BadAmount(0))
    ));
    assert!(matches!(
        w.svc.set_rate(1, -5),
        Err(BookingError::BadAmount(-5))
    ));
    assert_eq!(w.svc.db().version(), version);
}

#[test]
fn threads_racing_for_one_room_night_produce_one_booking() {
    for coalesce in [false, true] {
        let w = world_with(
            Plan {
                customers: 40,
                ..Plan::default()
            },
            coalesce,
        );
        let night = w.clock.today() + 3;
        let racers = 32;
        let gate = Arc::new(Barrier::new(racers));
        let handles: Vec<_> = (0..racers)
            .map(|i| {
                let svc = w.svc.clone();
                let gate = gate.clone();
                thread::spawn(move || {
                    gate.wait();
                    let request = booking::HoldRequest {
                        customer: 1 + i as u32,
                        room: 4,
                        first: night,
                        nights: 1 + (i as u32 % 3),
                        ttl_ms: days(1),
                    };
                    let held = svc.hold(&request);
                    held.and_then(|held| svc.confirm(held.id, Some(held.total)).map(|_| held.id))
                })
            })
            .collect();
        let mut won = Vec::new();
        for handle in handles {
            match handle.join().unwrap() {
                Ok(id) => won.push(id),
                Err(BookingError::Unavailable { .. }) => {}
                Err(other) => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(won.len(), 1, "coalesce={coalesce}: {won:?}");
        let h = w.svc.read();
        assert_eq!(h.bookings.by_status(&Status::Confirmed).len(), 1);
        assert_eq!(h.holder(4, night), Some(won[0]));
        drop(h);
        assert_clean(&w.svc);
    }
}

#[test]
fn the_checker_reports_broken_counters_and_stolen_holds() {
    let w = world();
    let night = w.clock.today() + 4;
    let held = w.hold(1, 1, night, 2, days(1)).unwrap();
    assert!(check_state(&w.svc.read()).is_empty());

    let mut broken = (*w.svc.read()).clone();
    broken.loads.insert((1, night), Load { held: 2, sold: 0 });
    broken.till.charged += 1;
    let found = check_state(&broken);
    assert!(
        found.iter().any(|m| m.contains("load counters")),
        "{found:?}"
    );
    assert!(found.iter().any(|m| m.contains("till says")), "{found:?}");

    let before = (*w.svc.read()).clone();
    let mut stolen = before.clone();
    stolen.stays.remove(&(held.id, night));
    stolen.stays.insert(Stay {
        slot: (held.id + 1, night),
        room: 1,
        night,
        venue: 1,
        sold: false,
    });
    let found = check_pair(&before, &stolen);
    assert!(
        found.iter().any(|m| m.contains("while still held")),
        "{found:?}"
    );

    w.svc
        .db()
        .update(|h| {
            h.customers.update(&2, |c| c.credit += 1);
        })
        .unwrap();
    let walked = booking::walk(w.svc.db(), 1).unwrap();
    assert_eq!(walked.violations.len(), 2, "{:?}", walked.violations);
    assert!(
        walked
            .violations
            .iter()
            .all(|v| v.version == w.svc.db().version())
    );
}
