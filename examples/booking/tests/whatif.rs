mod common;

use booking::ops::scaled;
use booking::whatif::{self, PREFIX};
use booking::{BookingError, Scenario, Status};
use common::{assert_clean, days, world};

#[test]
fn a_reprice_is_previewed_on_a_branch_then_merged_onto_a_moving_main() {
    let w = world();
    let night = w.clock.today() + 10;
    let paid = w.hold(1, 1, night, 2, days(1)).unwrap();
    w.svc.confirm(paid.id, None).unwrap();
    let held = w.hold(2, 2, night, 1, days(1)).unwrap();
    w.svc.set_night_rate(4, night, Some(30_000)).unwrap();

    let scenario = Scenario::Reprice {
        venue: Some(1),
        percent: -20,
    };
    let preview = whatif::propose(&w.svc, "spring", scenario.clone()).unwrap();
    assert_eq!(preview.scenario, scenario);
    assert_eq!(
        preview.repriced,
        vec![(1, 12_000, 9_600), (2, 14_500, 11_600), (3, 17_000, 13_600), (4, 12_000, 9_600)]
    );
    let exposed: Vec<(u64, i64)> = preview.exposure.iter().map(|e| (e.booking.id, e.at_new_rates)).collect();
    assert_eq!(exposed, vec![(held.id, 11_600), (paid.id, 19_200)]);
    assert!(preview.inventory_after < preview.inventory_before);
    assert!(preview.effects.is_empty());
    assert_eq!(w.svc.read().rooms.get(&1).unwrap().rate, 12_000, "main is untouched");

    w.svc.set_rate(3, 20_000).unwrap();
    w.svc.set_rate(5, 7_000).unwrap();
    w.svc.set_night_rate(1, night + 5, Some(25_000)).unwrap();
    assert_eq!(whatif::preview(&w.svc, "spring").unwrap().contested, vec![3]);

    let applied = whatif::apply(&w.svc, "spring").unwrap();
    assert_eq!(applied.conflicts, 1);
    let h = w.svc.read();
    let rates: Vec<i64> = (1..=5).map(|room| h.rooms.get(&room).unwrap().rate).collect();
    assert_eq!(rates, vec![9_600, 11_600, scaled(20_000, -20), 9_600, 7_000]);
    assert_eq!(h.rates.get(&(4, night)).map(|r| *r), Some(24_000));
    assert_eq!(h.rates.get(&(1, night + 5)).map(|r| *r), Some(25_000));
    assert_eq!(h.bookings.get(&paid.id).unwrap().total, paid.total, "a paid booking keeps its price");
    assert!(h.scenario.is_none());
    drop(h);
    assert!(w.svc.db().branches().iter().all(|b| !b.starts_with(PREFIX)));
    assert!(matches!(whatif::apply(&w.svc, "spring"), Err(BookingError::NoScenario(_))));
    assert_clean(&w.svc);
}

#[test]
fn a_closure_shows_its_refunds_and_its_merge_also_catches_later_bookings() {
    let w = world();
    let night = w.clock.today() + 6;
    let paid = w.hold(1, 5, night, 2, days(1)).unwrap();
    w.svc.confirm(paid.id, None).unwrap();
    let held = w.hold(2, 6, night, 1, days(1)).unwrap();
    let elsewhere = w.hold(3, 1, night, 1, days(1)).unwrap();
    w.svc.confirm(elsewhere.id, None).unwrap();

    let preview = whatif::propose(&w.svc, "storm", Scenario::Close { venue: 2 }).unwrap();
    let effects: Vec<(u64, Status, Status, i64)> = preview
        .effects
        .iter()
        .map(|e| (e.after.id, e.before.status, e.after.status, e.after.refunded))
        .collect();
    assert_eq!(
        effects,
        vec![
            (paid.id, Status::Confirmed, Status::Cancelled, paid.total),
            (held.id, Status::Held, Status::Cancelled, 0),
        ]
    );
    assert_eq!(preview.refunds, paid.total);
    assert_eq!(w.svc.read().bookings.get(&paid.id).unwrap().status, Status::Confirmed);

    let later = w.hold(4, 7, night + 1, 1, days(1)).unwrap();
    assert_eq!(whatif::preview(&w.svc, "storm").unwrap().drift, vec![later.id]);

    let applied = whatif::apply(&w.svc, "storm").unwrap();
    assert_eq!(applied.cancelled, vec![paid.id, held.id, later.id]);
    let h = w.svc.read();
    assert_eq!(h.venues.get(&2).unwrap().closed_from, Some(w.clock.today()));
    assert_eq!(h.bookings.get(&elsewhere.id).unwrap().status, Status::Confirmed);
    assert_eq!(h.till.refunded, paid.total);
    drop(h);
    assert!(matches!(w.hold(5, 8, night, 1, days(1)), Err(BookingError::VenueClosed(2))));
    assert_clean(&w.svc);
}

#[test]
fn a_dropped_what_if_leaves_main_as_it_was() {
    let w = world();
    let night = w.clock.today() + 2;
    let paid = w.hold(1, 9, night, 1, days(1)).unwrap();
    w.svc.confirm(paid.id, None).unwrap();
    let before = w.svc.read();
    whatif::propose(&w.svc, "maybe", Scenario::Close { venue: 3 }).unwrap();
    assert_eq!(whatif::list(&w.svc).unwrap().len(), 1);
    whatif::discard(&w.svc, "maybe").unwrap();
    assert!(whatif::list(&w.svc).unwrap().is_empty());
    let after = w.svc.read();
    assert_eq!(after.version(), before.version());
    assert!(after.venues == before.venues && after.bookings == before.bookings);
    assert!(matches!(whatif::discard(&w.svc, "maybe"), Err(BookingError::NoScenario(_))));
}

#[test]
fn a_bad_proposal_leaves_no_branch_and_a_failing_merge_commits_nothing() {
    let w = world();
    assert!(matches!(
        whatif::propose(&w.svc, "nowhere", Scenario::Close { venue: 42 }),
        Err(BookingError::NoVenue(_))
    ));
    assert!(matches!(
        whatif::propose(&w.svc, "free", Scenario::Reprice { venue: None, percent: -100 }),
        Err(BookingError::Usage(_))
    ));
    assert!(whatif::list(&w.svc).unwrap().is_empty());

    let branch = w.svc.db().fork(&format!("{PREFIX}rigged")).unwrap();
    branch
        .update(|h| h.scenario = Some(Scenario::Close { venue: 42 }))
        .unwrap();
    let version = w.svc.db().version();
    match whatif::apply(&w.svc, "rigged") {
        Err(BookingError::Usage(message)) => assert!(message.contains("no such venue"), "{message}"),
        other => panic!("expected the merge to be abandoned, got {other:?}"),
    }
    assert_eq!(w.svc.db().version(), version);
    assert_eq!(whatif::list(&w.svc).unwrap().len(), 1);
    assert_clean(&w.svc);
}
