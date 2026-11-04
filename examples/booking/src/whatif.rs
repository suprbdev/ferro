use ferro::{Change, Db, Error};

use crate::date::night_of;
use crate::error::{BookingError, Result};
use crate::model::{Booking, Cents, Hotels, Night, Scenario, Status};
use crate::ops::scaled;
use crate::service::Reservations;

pub const PREFIX: &str = "whatif-";
pub const HORIZON: Night = 30;

#[derive(Clone, Debug)]
pub struct Effect {
    pub before: Booking,
    pub after: Booking,
}

#[derive(Clone, Debug)]
pub struct Exposure {
    pub booking: Booking,
    pub at_new_rates: Cents,
}

#[derive(Clone, Debug)]
pub struct Preview {
    pub name: String,
    pub scenario: Scenario,
    pub base_version: u64,
    pub main_version: u64,
    pub effects: Vec<Effect>,
    pub refunds: Cents,
    pub repriced: Vec<(u32, Cents, Cents)>,
    pub exposure: Vec<Exposure>,
    pub inventory_before: Cents,
    pub inventory_after: Cents,
    pub drift: Vec<u64>,
    pub contested: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct Applied {
    pub version: u64,
    pub summary: String,
    pub conflicts: usize,
    pub cancelled: Vec<u64>,
}

fn branch_of(db: &Db<Hotels>, name: &str) -> Result<Db<Hotels>> {
    db.branch(&format!("{PREFIX}{name}"))
        .map_err(|error| match error {
            Error::NoBranch(_) => BookingError::NoScenario(name.to_string()),
            other => other.into(),
        })
}

pub fn list(svc: &Reservations) -> Result<Vec<(String, Option<Scenario>)>> {
    let mut found = Vec::new();
    for branch in svc.db().branches() {
        if let Some(name) = branch.strip_prefix(PREFIX) {
            let scenario = svc.db().branch(&branch)?.read().scenario.clone();
            found.push((name.to_string(), scenario));
        }
    }
    Ok(found)
}

pub fn propose(svc: &Reservations, name: &str, scenario: Scenario) -> Result<Preview> {
    let snapshot = svc.read();
    match &scenario {
        Scenario::Close { venue }
        | Scenario::Reprice {
            venue: Some(venue), ..
        } => {
            if !snapshot.venues.contains(venue) {
                return Err(BookingError::NoVenue(venue.to_string()));
            }
        }
        Scenario::Reprice { venue: None, .. } => {}
    }
    let full = format!("{PREFIX}{name}");
    let branch = svc.on(svc.db().fork(&full)?);
    let applied = branch.write(|h, now| {
        let summary = h.apply(now, &scenario)?;
        h.scenario = Some(scenario.clone());
        Ok((format!("what-if {name}: {summary}"), ()))
    });
    if let Err(error) = applied {
        svc.db().drop_branch(&full)?;
        return Err(error);
    }
    preview(svc, name)
}

fn inventory(h: &Hotels, from: Night) -> Cents {
    let mut value = 0;
    for room in h.rooms.iter() {
        let open = h.venues.get(&room.venue);
        for night in from..from + HORIZON {
            let sellable = open.as_ref().is_some_and(|venue| venue.open_on(night));
            if sellable && h.holder(room.id, night).is_none() {
                value += h.price(&room, night);
            }
        }
    }
    value
}

pub fn preview(svc: &Reservations, name: &str) -> Result<Preview> {
    let branch = branch_of(svc.db(), name)?;
    let theirs = branch.read();
    let scenario = theirs
        .scenario
        .clone()
        .ok_or_else(|| BookingError::NoScenario(name.to_string()))?;
    let base = svc.db().common_ancestor(&branch)?;
    let main = svc.read();
    let today = night_of(svc.now());

    let mut effects = Vec::new();
    for change in base.bookings.diff(&theirs.bookings) {
        if let Change::Changed(_, before, after) = change {
            effects.push(Effect {
                before: before.into_owned(),
                after: after.into_owned(),
            });
        }
    }
    let mut repriced = Vec::new();
    for change in base.rooms.diff(&theirs.rooms) {
        if let Change::Changed(id, before, after) = change
            && before.rate != after.rate
        {
            repriced.push((id, before.rate, after.rate));
        }
    }
    let mut exposure = Vec::new();
    for status in [Status::Held, Status::Confirmed] {
        for booking in main.bookings.by_status(&status) {
            if booking.last_night() < today
                || !repriced.iter().any(|(id, _, _)| *id == booking.room)
            {
                continue;
            }
            let Some(room) = theirs.rooms.get(&booking.room) else {
                continue;
            };
            let at_new_rates = booking
                .nights()
                .map(|night| theirs.price(&room, night))
                .sum();
            if at_new_rates != booking.total {
                exposure.push(Exposure {
                    booking: booking.into_owned(),
                    at_new_rates,
                });
            }
        }
    }
    let drift = match &scenario {
        Scenario::Close { venue } => main
            .active_at(*venue, today)
            .into_iter()
            .filter(|id| !effects.iter().any(|effect| effect.after.id == *id))
            .collect(),
        Scenario::Reprice { .. } => Vec::new(),
    };
    let contested = base
        .rooms
        .diff(&main.rooms)
        .map(|change| *change.key())
        .filter(|id| repriced.iter().any(|(room, _, _)| room == id))
        .collect();
    Ok(Preview {
        name: name.to_string(),
        scenario,
        base_version: base.version(),
        main_version: main.version(),
        refunds: theirs.till.refunded - base.till.refunded,
        effects,
        repriced,
        exposure,
        inventory_before: inventory(&main, today),
        inventory_after: inventory(&theirs, today),
        drift,
        contested,
    })
}

fn merged(
    ours: &mut Hotels,
    base: &Hotels,
    theirs: &Hotels,
    scenario: &Scenario,
    now: u64,
) -> Result<(String, usize, Vec<u64>)> {
    match scenario {
        Scenario::Reprice { percent, .. } => {
            let mut conflicts = 0;
            let rooms = ours.rooms.merge(&base.rooms, &theirs.rooms, |conflict| {
                conflicts += 1;
                conflict.keep_ours().map(|mut room| {
                    room.rate = scaled(room.rate, *percent);
                    room
                })
            });
            let rates = ours.rates.merge(&base.rates, &theirs.rates, |conflict| {
                conflicts += 1;
                conflict.ours.map(|rate| scaled(*rate, *percent))
            });
            Ok((
                format!("{scenario}: {rooms} rooms, {rates} night rates"),
                conflicts,
                Vec::new(),
            ))
        }
        Scenario::Close { venue } => {
            let cancelled = ours.close_venue(now, *venue)?;
            let refunds: Cents = cancelled.iter().map(|(_, refund)| refund).sum();
            let summary = format!(
                "{scenario}: cancelled {}, refunded {refunds}",
                cancelled.len()
            );
            Ok((
                summary,
                0,
                cancelled.into_iter().map(|(id, _)| id).collect(),
            ))
        }
    }
}

pub fn apply(svc: &Reservations, name: &str) -> Result<Applied> {
    let branch = branch_of(svc.db(), name)?;
    let scenario = branch
        .read()
        .scenario
        .clone()
        .ok_or_else(|| BookingError::NoScenario(name.to_string()))?;
    let now = svc.now();
    let done = svc.db().try_merge_with(&branch, |ours, base, theirs| {
        let (summary, conflicts, cancelled) = merged(ours, base, theirs, &scenario, now)?;
        let message = format!("merge what-if {name}: {summary}");
        Ok::<_, BookingError>((message, (summary, conflicts, cancelled)))
    })?;
    svc.db().drop_branch(&format!("{PREFIX}{name}"))?;
    let (summary, conflicts, cancelled) = done.value;
    Ok(Applied {
        version: done.version,
        summary,
        conflicts,
        cancelled,
    })
}

pub fn discard(svc: &Reservations, name: &str) -> Result<()> {
    branch_of(svc.db(), name)?;
    svc.db().drop_branch(&format!("{PREFIX}{name}"))?;
    Ok(())
}
