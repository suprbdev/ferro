mod common;

use booking::{BookingError, Flow, Status};
use common::{assert_clean, days, world};

#[test]
fn refunds_follow_the_policy() {
    let w = world();
    let today = w.clock.today();
    let early = w.hold(1, 1, today + 10, 2, days(1)).unwrap();
    let close = w.hold(2, 2, today + 1, 2, days(1)).unwrap();
    let started = w.hold(3, 3, today, 3, days(1)).unwrap();
    for held in [&early, &close, &started] {
        w.svc.confirm(held.id, Some(held.total)).unwrap();
    }

    assert_eq!(w.svc.cancel(early.id).unwrap(), early.total);
    assert_eq!(w.svc.cancel(close.id).unwrap(), close.total / 2);
    w.clock.advance(days(1));
    assert_eq!(w.svc.cancel(started.id).unwrap(), 0);

    let h = w.svc.read();
    let refunds: Vec<i64> = h
        .payments
        .iter()
        .filter(|p| p.flow == Flow::Refund)
        .map(|p| p.amount)
        .collect();
    assert_eq!(refunds, vec![early.total, close.total / 2]);
    assert_eq!(h.till.refunded, early.total + close.total / 2);
    assert_eq!(h.till.charged, early.total + close.total + started.total);
    let credit: i64 = h.customers.iter().map(|c| c.credit).sum();
    assert_eq!(credit, h.till.deposits - h.till.charged + h.till.refunded);
    for id in [early.id, close.id, started.id] {
        let b = h.bookings.get(&id).unwrap();
        assert_eq!(b.status, Status::Cancelled);
        assert_eq!(h.stays.range((id, 0)..=(id, u32::MAX)).count(), 0);
        assert_eq!(h.payments.by_booking(&id).count(), 1 + usize::from(b.refunded > 0));
    }
    assert!(h.loads.is_empty());
    drop(h);
    assert_clean(&w.svc);
}

#[test]
fn a_cancelled_or_expired_booking_cannot_be_cancelled_again() {
    let w = world();
    let night = w.clock.today() + 5;
    let held = w.hold(1, 1, night, 1, 1_000).unwrap();
    assert_eq!(w.svc.cancel(held.id).unwrap(), 0);
    assert!(matches!(
        w.svc.cancel(held.id),
        Err(BookingError::WrongStatus { status: Status::Cancelled, .. })
    ));
    assert!(matches!(
        w.svc.confirm(held.id, None),
        Err(BookingError::WrongStatus { status: Status::Cancelled, .. })
    ));

    let lapsing = w.hold(1, 1, night, 1, 1_000).unwrap();
    w.clock.advance(1_000);
    w.svc.sweep().unwrap();
    assert!(matches!(
        w.svc.cancel(lapsing.id),
        Err(BookingError::WrongStatus { status: Status::Expired, .. })
    ));
    assert_eq!(w.svc.read().till.refunded, 0);
    assert_clean(&w.svc);
}

#[test]
fn cancelled_nights_go_back_on_sale_at_the_current_price() {
    let w = world();
    let night = w.clock.today() + 30;
    let first = w.hold(1, 4, night, 3, days(1)).unwrap();
    w.svc.confirm(first.id, None).unwrap();
    assert!(matches!(w.hold(2, 4, night, 1, days(1)), Err(BookingError::Unavailable { .. })));

    w.svc.cancel(first.id).unwrap();
    w.svc.set_night_rate(4, night + 1, Some(30_000)).unwrap();
    let second = w.hold(2, 4, night, 3, days(1)).unwrap();
    assert_eq!(second.total, 12_000 + 30_000 + 12_000);
    w.svc.confirm(second.id, Some(second.total)).unwrap();

    let h = w.svc.read();
    assert_eq!(h.bookings.get(&second.id).unwrap().prices, vec![12_000, 30_000, 12_000]);
    assert_eq!(h.holder(4, night + 2), Some(second.id));
    let customer = h.customers.get(&1).unwrap();
    assert_eq!(customer.credit, customer.deposited);
    drop(h);
    assert_clean(&w.svc);
}
