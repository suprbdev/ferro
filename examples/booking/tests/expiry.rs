mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use booking::seed::Plan;
use booking::{BookingError, Config, HoldRequest, Load, Reservations, Status};
use common::{assert_clean, world, world_with};

#[test]
fn a_hold_blocks_others_until_the_moment_it_expires() {
    let w = world();
    let night = w.clock.today() + 7;
    let first = w.hold(1, 2, night, 2, 10_000).unwrap();
    assert_eq!(first.expires_ms, w.clock.now() + 10_000);

    w.clock.advance(9_999);
    assert!(matches!(
        w.hold(2, 2, night + 1, 1, 10_000),
        Err(BookingError::Unavailable { holder, .. }) if holder == first.id
    ));

    w.clock.advance(1);
    let second = w.hold(2, 2, night + 1, 1, 10_000).unwrap();
    assert_eq!(second.released, vec![first.id]);

    let h = w.svc.read();
    let lapsed = h.bookings.get(&first.id).unwrap();
    assert_eq!(lapsed.status, Status::Expired);
    assert_eq!(lapsed.ended_ms, Some(first.expires_ms));
    assert_eq!(h.holder(2, night), None, "the whole lapsed hold was released, not just the contested night");
    assert_eq!(h.holder(2, night + 1), Some(second.id));
    assert_eq!(h.loads.get(&(1, night)), None);
    drop(h);
    assert_clean(&w.svc);
}

#[test]
fn a_lapsed_hold_cannot_be_confirmed_even_before_the_sweep() {
    let w = world();
    let night = w.clock.today() + 2;
    let held = w.hold(1, 1, night, 1, 5_000).unwrap();
    w.clock.advance(5_000);
    assert!(matches!(
        w.svc.confirm(held.id, None),
        Err(BookingError::HoldExpired { id, .. }) if id == held.id
    ));
    assert_eq!(w.svc.read().bookings.get(&held.id).unwrap().status, Status::Held);
    assert_eq!(w.svc.read().till.charged, 0);
    assert!(matches!(
        w.svc.cancel(held.id),
        Ok(0)
    ));
}

#[test]
fn the_sweep_releases_due_holds_and_nothing_else() {
    let w = world();
    let night = w.clock.today() + 3;
    let short = w.hold(1, 1, night, 2, 1_000).unwrap();
    let long = w.hold(2, 2, night, 2, 60_000).unwrap();
    let paid = w.hold(3, 3, night, 2, 1_000).unwrap();
    w.svc.confirm(paid.id, None).unwrap();

    assert!(w.svc.sweep().unwrap().is_empty());
    let version = w.svc.db().version();
    assert!(w.svc.sweep().unwrap().is_empty());
    assert_eq!(w.svc.db().version(), version, "an empty sweep writes no version");

    w.clock.advance(1_000);
    assert_eq!(w.svc.sweep().unwrap(), vec![short.id]);
    assert!(w.svc.sweep().unwrap().is_empty());

    let h = w.svc.read();
    assert_eq!(h.bookings.get(&short.id).unwrap().status, Status::Expired);
    assert_eq!(h.bookings.get(&long.id).unwrap().status, Status::Held);
    assert_eq!(h.bookings.get(&paid.id).unwrap().status, Status::Confirmed);
    assert_eq!(h.loads.get(&(1, night)).map(|l| *l), Some(Load { held: 1, sold: 1 }));
    assert_eq!(h.due(w.clock.now() + 59_000), vec![long.id]);
    drop(h);

    w.clock.advance(59_000);
    assert_eq!(w.svc.sweep().unwrap(), vec![long.id]);
    assert_eq!(w.svc.read().bookings.by_status(&Status::Held).len(), 0);
    assert_clean(&w.svc);
}

#[test]
fn holds_expiring_while_threads_contend_keep_every_version_clean() {
    for coalesce in [false, true] {
        let w = world_with(
            Plan {
                venues: 1,
                rooms: 3,
                customers: 24,
                credit: 1_000_000,
                ..Plan::default()
            },
            coalesce,
        );
        let path = w.path();
        drop(w.svc);
        let svc = Reservations::open(&path, Config { coalesce, ..Config::default() }).unwrap();
        let first = booking::date::night_of(svc.now()) + 2;
        let stop = Arc::new(AtomicBool::new(false));
        let sweeper = {
            let (svc, stop) = (svc.clone(), stop.clone());
            thread::spawn(move || {
                let mut swept = 0;
                while !stop.load(Ordering::Relaxed) {
                    swept += svc.sweep().unwrap().len();
                    thread::sleep(Duration::from_millis(2));
                }
                swept
            })
        };
        let workers: Vec<_> = (0..12u32)
            .map(|i| {
                let svc = svc.clone();
                thread::spawn(move || {
                    let mut rng = fastrand::Rng::with_seed(u64::from(i));
                    let mut confirmed = 0;
                    for _ in 0..60 {
                        let request = HoldRequest {
                            customer: 1 + i * 2 + rng.u32(0..2),
                            room: 1 + rng.u32(0..3),
                            first: first + rng.u32(0..2),
                            nights: 1 + rng.u32(0..2),
                            ttl_ms: rng.u64(2..15),
                        };
                        let Ok(held) = svc.hold(&request) else {
                            continue;
                        };
                        if rng.u32(0..3) == 0 && svc.confirm(held.id, None).is_ok() {
                            confirmed += 1;
                            if rng.bool() {
                                svc.cancel(held.id).unwrap();
                            }
                        } else {
                            thread::sleep(Duration::from_millis(rng.u64(0..4)));
                        }
                    }
                    confirmed
                })
            })
            .collect();
        let confirmed: u32 = workers.into_iter().map(|h| h.join().unwrap()).sum();
        thread::sleep(Duration::from_millis(20));
        stop.store(true, Ordering::Relaxed);
        let swept = sweeper.join().unwrap();
        svc.sweep().unwrap();
        let h = svc.read();
        assert_eq!(h.bookings.by_status(&Status::Held).len(), 0);
        assert!(h.bookings.by_status(&Status::Expired).len() > 0);
        drop(h);
        let walked = assert_clean(&svc);
        assert!(confirmed > 0 && swept > 0, "coalesce={coalesce}: confirmed {confirmed}, swept {swept}");
        assert!(walked.expired > 0);
    }
}
