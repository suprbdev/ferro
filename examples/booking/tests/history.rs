mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use booking::date::{night_of, now_ms};
use booking::history::{as_paid, audit, price_history};
use booking::seed::{Plan, seed};
use booking::{Config, HoldRequest, Hotels, Reservations, Status};
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
    assert_eq!(receipt.now.paid_version, Some(paid_in));
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

fn paid_in(h: &Hotels, id: u64) -> bool {
    h.bookings.get(&id).is_some_and(|b| b.paid_ms.is_some())
}

fn receipts_while_rates_change(coalesce: bool, bookings: u32, buyers: u32) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        coalesce,
        buffered: true,
        ..Config::default()
    };
    let svc = Reservations::open(dir.path().join("p.ferro"), config).unwrap();
    let plan = Plan {
        venues: 1,
        rooms: 40,
        customers: 50,
        credit: 100_000_000,
        rate: 1000,
    };
    seed(&svc, &plan).unwrap();
    let first = night_of(now_ms()) + 1;
    let stop = Arc::new(AtomicBool::new(false));
    let (started, noise_started) = mpsc::channel();
    let noise = {
        let (svc, stop) = (svc.clone(), stop.clone());
        thread::spawn(move || {
            let mut changes = 1;
            svc.set_rate(40, 1000 + changes).unwrap();
            started.send(()).unwrap();
            while !stop.load(Ordering::Relaxed) {
                changes += 1;
                svc.set_rate(40, 1000 + changes % 50).unwrap();
            }
            changes
        })
    };
    noise_started.recv().unwrap();
    let workers: Vec<_> = (0..buyers)
        .map(|buyer| {
            let svc = svc.clone();
            thread::spawn(move || {
                let mut paid = Vec::new();
                for i in (buyer..bookings).step_by(buyers as usize) {
                    let request = HoldRequest {
                        customer: 1 + i % 50,
                        room: 1 + i % 39,
                        first: first + i / 39,
                        nights: 1,
                        ttl_ms: 60_000,
                    };
                    let held = svc.hold(&request).unwrap();
                    svc.confirm(held.id, None).unwrap();
                    paid.push(held.id);
                }
                paid
            })
        })
        .collect();
    let paid: Vec<u64> = workers.into_iter().flat_map(|worker| worker.join().unwrap()).collect();
    stop.store(true, Ordering::Relaxed);
    assert!(noise.join().unwrap() > 0);
    assert_eq!(paid.len(), bookings as usize);

    let db = svc.db();
    for id in paid {
        let receipt = as_paid(db, id).unwrap();
        assert_eq!(db.bisect(.., |h| paid_in(h, id)).unwrap(), Some(receipt.version));
        let past = db.at(receipt.version).unwrap();
        assert!(paid_in(&past, id) && !paid_in(&db.at(receipt.version - 1).unwrap(), id));
        let confirm = format!("confirm #{id} paid");
        assert!(past.commit().messages().any(|line| line.starts_with(&confirm)));
        assert_eq!(receipt.then.status, Status::Confirmed);
        assert_eq!(receipt.then.paid_version, Some(receipt.version));
        let room = past.rooms.get(&receipt.then.room).unwrap();
        assert_eq!(receipt.rates_then, vec![past.price(&room, receipt.then.first)]);
    }
    let walked = booking::walk(db, 0).unwrap();
    assert!(walked.ok(), "{:#?}", &walked.violations[..walked.violations.len().min(5)]);
}

#[test]
fn a_receipt_lands_on_its_payment_while_another_writer_keeps_changing_rates() {
    receipts_while_rates_change(false, 1_500, 1);
}

#[test]
fn a_coalesced_receipt_lands_on_the_group_that_paid() {
    receipts_while_rates_change(true, 800, 4);
}
