mod common;

use std::thread;
use std::time::Duration;

use booking::date::{night_of, now_ms};
use booking::history::{as_paid, audit, paid_version, price_history};
use booking::seed::{Plan, seed};
use booking::{HoldRequest, Status};
use common::{days, open, world};

#[test]
fn the_audit_trail_shows_each_change_to_one_booking_oldest_first() {
    let w = world();
    let night = w.clock.today() + 9;
    let held = w.hold(1, 1, night, 2, days(1)).unwrap();
    let other = w.hold(2, 2, night, 2, days(1)).unwrap();
    w.svc.set_rate(1, 20_000).unwrap();
    w.svc.confirm(held.id, None).unwrap();
    w.svc.confirm(other.id, None).unwrap();
    w.svc.deposit(1, 5_000).unwrap();
    w.svc.cancel(held.id).unwrap();

    let events = audit(w.svc.db(), held.id).unwrap();
    let steps: Vec<(Status, i64, i64)> = events
        .iter()
        .map(|e| (e.booking.status, e.booking.paid, e.booking.refunded))
        .collect();
    assert_eq!(
        steps,
        vec![
            (Status::Held, 0, 0),
            (Status::Confirmed, held.total, 0),
            (Status::Cancelled, held.total, held.total),
        ]
    );
    assert!(events[0].message.starts_with(&format!("hold #{}", held.id)));
    assert!(events[1].message.starts_with(&format!("confirm #{}", held.id)));
    assert!(events[2].message.starts_with(&format!("cancel #{}", held.id)));
    assert!(events.windows(2).all(|pair| pair[0].version < pair[1].version));
    assert!(matches!(audit(w.svc.db(), 999), Err(booking::BookingError::NoBooking(999))));
}

#[test]
fn price_history_lists_every_rate_and_override_change() {
    let w = world();
    let night = w.clock.today() + 12;
    w.svc.set_rate(2, 15_000).unwrap();
    w.hold(1, 1, night, 1, days(1)).unwrap();
    w.svc.set_night_rate(2, night, Some(40_000)).unwrap();
    w.svc.set_rate(2, 16_000).unwrap();
    w.svc.set_night_rate(2, night, None).unwrap();

    let points = price_history(w.svc.db(), 2).unwrap();
    let rates: Vec<(i64, usize)> = points.iter().map(|p| (p.rate, p.overrides.len())).collect();
    assert_eq!(rates, vec![(14_500, 0), (15_000, 0), (15_000, 1), (16_000, 1), (16_000, 0)]);
    assert_eq!(points[2].overrides, vec![(night, 40_000)]);
}

#[test]
fn a_receipt_shows_the_booking_and_rates_as_they_were_when_paid() {
    let dir = tempfile::tempdir().unwrap();
    let svc = open(&dir.path().join("hotels.ferro"), false);
    seed(&svc, &Plan::default()).unwrap();
    let night = night_of(now_ms()) + 14;
    let held = svc
        .hold(&HoldRequest {
            customer: 2,
            room: 6,
            first: night,
            nights: 2,
            ttl_ms: 60_000,
        })
        .unwrap();
    svc.set_night_rate(6, night + 1, Some(21_000)).unwrap();
    svc.confirm(held.id, Some(held.total)).unwrap();
    let paid_in = svc.db().version();
    svc.set_rate(6, 30_000).unwrap();
    svc.set_night_rate(6, night + 1, None).unwrap();
    svc.deposit(2, 1_000).unwrap();
    svc.cancel(held.id).unwrap();

    let receipt = as_paid(svc.db(), held.id).unwrap();
    assert_eq!(receipt.version, paid_in);
    assert_eq!(receipt.then.status, Status::Confirmed);
    assert_eq!(receipt.now.status, Status::Cancelled);
    assert_eq!(receipt.then.prices, vec![15_500, 15_500]);
    assert_eq!(receipt.rates_then, vec![15_500, 21_000]);
    assert_eq!(receipt.rates_now, vec![30_000, 30_000]);
    assert_eq!(receipt.credit_before, 225_000);
    assert_eq!(receipt.credit_after, 225_000 - held.total);
    assert_eq!(receipt.venue, "Alpine Lodge");
    assert_eq!(paid_version(svc.db(), held.id, receipt.paid_ms).unwrap(), Some(paid_in));
}

#[test]
fn a_receipt_is_exact_even_when_the_clock_is_not_the_commit_clock() {
    let w = world();
    let night = w.clock.today() + 3;
    let held = w.hold(1, 1, night, 1, days(1)).unwrap();
    w.svc.confirm(held.id, None).unwrap();
    let paid_in = w.svc.db().version();
    w.svc.set_rate(1, 50_000).unwrap();
    w.svc.cancel(held.id).unwrap();
    let receipt = as_paid(w.svc.db(), held.id).unwrap();
    assert_eq!(receipt.version, paid_in);
    assert_eq!(receipt.rates_then, vec![12_000]);
    assert_eq!(receipt.rates_now, vec![50_000]);
}

#[test]
fn as_of_reads_the_hotels_at_a_wall_clock_time() {
    let dir = tempfile::tempdir().unwrap();
    let svc = open(&dir.path().join("hotels.ferro"), false);
    let before_anything = now_ms().saturating_sub(1);
    seed(&svc, &Plan::default()).unwrap();
    thread::sleep(Duration::from_millis(3));
    let morning = now_ms();
    thread::sleep(Duration::from_millis(3));
    svc.set_rate(1, 99_000).unwrap();
    let night = night_of(now_ms()) + 2;
    let held = svc
        .hold(&HoldRequest {
            customer: 1,
            room: 1,
            first: night,
            nights: 1,
            ttl_ms: 60_000,
        })
        .unwrap();

    let then = svc.db().as_of(morning).unwrap();
    assert_eq!(then.version(), 1);
    assert_eq!(then.rooms.get(&1).unwrap().rate, 12_000);
    assert!(then.bookings.get(&held.id).is_none());
    assert_eq!(svc.db().as_of(now_ms()).unwrap().holder(1, night), Some(held.id));
    assert_eq!(svc.db().as_of(before_anything).unwrap().version(), 0);
}
