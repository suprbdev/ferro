use ferro::{Commit, Db, Val};

use crate::error::{BookingError, Result};
use crate::model::{Booking, Cents, Hotels, Night};

#[derive(Clone, Debug)]
pub struct Event {
    pub version: u64,
    pub time_ms: u64,
    pub message: String,
    pub booking: Booking,
}

pub fn audit(db: &Db<Hotels>, id: u64) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    let mut history = db
        .history_of(|h| h.bookings.get(&id).map(Val::into_owned))
        .take_while(|(_, row)| row.is_some());
    while let Some((commit, row)) = history.try_next()? {
        if let Some(booking) = row {
            events.push(event(commit, booking));
        }
    }
    if events.is_empty() {
        return Err(BookingError::NoBooking(id));
    }
    events.reverse();
    Ok(events)
}

fn mentions(line: &str, id: u64) -> bool {
    let tag = format!("#{id}");
    line.match_indices(&tag)
        .any(|(at, _)| !line[at + tag.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

fn event(commit: Commit, booking: Booking) -> Event {
    let line = commit.messages().find(|line| mentions(line, booking.id));
    Event {
        version: commit.version,
        time_ms: commit.time_ms,
        message: line.unwrap_or(&commit.message).to_string(),
        booking,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PricePoint {
    pub version: u64,
    pub time_ms: u64,
    pub rate: Cents,
    pub overrides: Vec<(Night, Cents)>,
}

pub fn price_history(db: &Db<Hotels>, room: u32) -> Result<Vec<PricePoint>> {
    let project = |h: &Hotels| {
        h.rooms.get(&room).map(|row| {
            let overrides: Vec<(Night, Cents)> = h
                .rates
                .prefix(&room)
                .map(|((_, night), rate)| (night, *rate))
                .collect();
            (row.rate, overrides)
        })
    };
    let mut points = Vec::new();
    let mut history = db.history_of(project).take_while(|(_, row)| row.is_some());
    while let Some((commit, row)) = history.try_next()? {
        if let Some((rate, overrides)) = row {
            points.push(PricePoint {
                version: commit.version,
                time_ms: commit.time_ms,
                rate,
                overrides,
            });
        }
    }
    if points.is_empty() {
        return Err(BookingError::NoRoom(room));
    }
    points.reverse();
    Ok(points)
}

#[derive(Clone, Debug)]
pub struct Receipt {
    pub version: u64,
    pub paid_ms: u64,
    pub then: Booking,
    pub now: Booking,
    pub venue: String,
    pub room_number: u16,
    pub rates_then: Vec<Cents>,
    pub rates_now: Vec<Cents>,
    pub credit_before: Cents,
    pub credit_after: Cents,
}

pub fn as_paid(db: &Db<Hotels>, id: u64) -> Result<Receipt> {
    let current = db.read();
    let now = current
        .bookings
        .get(&id)
        .ok_or(BookingError::NoBooking(id))?
        .into_owned();
    let wrong = || BookingError::WrongStatus {
        id,
        status: now.status,
        wanted: "paid",
    };
    let paid_ms = now.paid_ms.ok_or_else(wrong)?;
    let version = now.paid_version.ok_or_else(wrong)?;
    let past = db.at(version)?;
    let before = db.at(version - 1)?;
    let then = past
        .bookings
        .get(&id)
        .ok_or(BookingError::NoBooking(id))?
        .into_owned();
    let room = past
        .rooms
        .get(&then.room)
        .ok_or(BookingError::NoRoom(then.room))?;
    let rates_then = then
        .nights()
        .map(|night| past.price(&room, night))
        .collect();
    let rates_now = match current.rooms.get(&now.room) {
        Some(room) => now
            .nights()
            .map(|night| current.price(&room, night))
            .collect(),
        None => Vec::new(),
    };
    let venue = past
        .venues
        .get(&then.venue)
        .map(|venue| venue.name.clone())
        .unwrap_or_default();
    let credit = |h: &Hotels| h.customers.get(&then.customer).map_or(0, |c| c.credit);
    Ok(Receipt {
        version,
        paid_ms,
        venue,
        room_number: room.number,
        rates_then,
        rates_now,
        credit_before: credit(&before),
        credit_after: credit(&past),
        then,
        now,
    })
}
