use std::io::Write;

use booking::date::{format_money, format_night, format_time, night_of, parse_money, parse_night, parse_time};
use booking::history::{as_paid, audit, price_history};
use booking::seed::{Plan, seed};
use booking::whatif::{self, Preview};
use booking::{
    Booking, BookingError, Flow, Hotels, HoldRequest, Reservations, Result, Scenario, Status, walk,
};

use crate::cli::{Command, CustomerCmd, RoomCmd, VenueCmd, WhatIfCmd};

type Out<'a> = &'a mut dyn Write;

fn io(error: std::io::Error) -> BookingError {
    BookingError::Db(ferro::Error::Io(error))
}

macro_rules! say {
    ($out:expr, $($arg:tt)*) => {
        writeln!($out, $($arg)*).map_err(io)?
    };
}

fn room_id(h: &Hotels, text: &str) -> Result<u32> {
    if let Some((venue, number)) = text.split_once('/') {
        let venue = h.venue_named(venue)?;
        let number: u16 = number
            .parse()
            .map_err(|_| BookingError::Usage(format!("bad room number in `{text}`")))?;
        return h
            .room_at(venue.id, number)
            .map(|room| room.id)
            .ok_or_else(|| BookingError::Usage(format!("venue {} has no room {number}", venue.name)));
    }
    let id: u32 = text
        .parse()
        .map_err(|_| BookingError::Usage(format!("expected a room id or VENUE/NUMBER, got `{text}`")))?;
    if h.rooms.contains(&id) {
        Ok(id)
    } else {
        Err(BookingError::NoRoom(id))
    }
}

fn line(out: Out<'_>, h: &Hotels, b: &Booking) -> Result<()> {
    let room = h.rooms.get(&b.room).map_or(0, |room| room.number);
    let venue = h.venues.get(&b.venue).map(|v| v.name.clone()).unwrap_or_default();
    say!(
        out,
        "#{:<5} {:<9} customer {:<4} {} room {} {} x{} total {}",
        b.id,
        b.status,
        b.customer,
        venue,
        room,
        format_night(b.first),
        b.nights,
        format_money(b.total)
    );
    Ok(())
}

fn detail(out: Out<'_>, h: &Hotels, b: &Booking) -> Result<()> {
    line(out, h, b)?;
    say!(out, "  created  {}", format_time(b.created_ms));
    match b.status {
        Status::Held => say!(out, "  held until {}", format_time(b.expires_ms)),
        _ => say!(out, "  hold ran to {}", format_time(b.expires_ms)),
    }
    let prices: Vec<String> = b.prices.iter().map(|p| format_money(*p)).collect();
    say!(out, "  nightly  {}", prices.join(" + "));
    if let Some(paid) = b.paid_ms {
        say!(out, "  paid     {} at {}", format_money(b.paid), format_time(paid));
    }
    if let Some(ended) = b.ended_ms {
        say!(
            out,
            "  ended    {} ({}), refunded {}",
            format_time(ended),
            b.reason.as_deref().unwrap_or(""),
            format_money(b.refunded)
        );
    }
    Ok(())
}

fn preview(out: Out<'_>, h: &Hotels, p: &Preview) -> Result<()> {
    say!(out, "what-if {}: {}", p.name, p.scenario);
    say!(out, "  forked from v{}, main is at v{}", p.base_version, p.main_version);
    for (room, before, after) in &p.repriced {
        say!(out, "  room {room}: {} -> {}", format_money(*before), format_money(*after));
    }
    for effect in &p.effects {
        say!(
            out,
            "  #{} {} -> {}, refund {}",
            effect.after.id,
            effect.before.status,
            effect.after.status,
            format_money(effect.after.refunded)
        );
    }
    if !p.effects.is_empty() {
        say!(out, "  {} bookings affected, refunds {}", p.effects.len(), format_money(p.refunds));
    }
    for exposure in &p.exposure {
        say!(
            out,
            "  #{} locked at {}, would be {} at the new rates",
            exposure.booking.id,
            format_money(exposure.booking.total),
            format_money(exposure.at_new_rates)
        );
    }
    say!(
        out,
        "  unsold inventory over the next {} nights: {} -> {}",
        whatif::HORIZON,
        format_money(p.inventory_before),
        format_money(p.inventory_after)
    );
    for id in &p.drift {
        if let Some(b) = h.bookings.get(id) {
            say!(out, "  main has since taken #{id}, which a merge would also cancel:");
            line(out, h, &b)?;
        }
    }
    for room in &p.contested {
        say!(out, "  main has also changed room {room}; a merge applies the change on top of main's rate");
    }
    Ok(())
}

pub fn run_read(out: Out<'_>, svc: &Reservations, command: Command) -> Result<()> {
    let h = svc.read();
    match command {
        Command::Venue { cmd: VenueCmd::List } => {
            for venue in h.venues.iter() {
                let closed = venue
                    .closed_from
                    .map(|from| format!(" closed from {}", format_night(from)))
                    .unwrap_or_default();
                let rooms = h.rooms.by_venue(&venue.id).len();
                say!(out, "{:<3} {:<18} {:<10} {rooms} rooms{closed}", venue.id, venue.name, venue.city);
            }
        }
        Command::Room { cmd: RoomCmd::List { venue } } => {
            let venue = venue.map(|v| h.venue_named(&v).map(|v| v.id)).transpose()?;
            for room in h.rooms.iter() {
                if venue.is_some_and(|v| v != room.venue) {
                    continue;
                }
                say!(out, "{:<4} venue {:<3} room {:<4} {}", room.id, room.venue, room.number, format_money(room.rate));
            }
        }
        Command::Room { cmd: RoomCmd::Prices { room } } => {
            let room = room_id(&h, &room)?;
            for point in price_history(svc.db(), room)? {
                let overrides: Vec<String> = point
                    .overrides
                    .iter()
                    .map(|(night, rate)| format!("{}={}", format_night(*night), format_money(*rate)))
                    .collect();
                say!(
                    out,
                    "v{:<6} {}  {}  {}",
                    point.version,
                    format_time(point.time_ms),
                    format_money(point.rate),
                    overrides.join(" ")
                );
            }
        }
        Command::Customer { cmd: CustomerCmd::List } => {
            for c in h.customers.iter() {
                say!(out, "{:<4} {:<28} {:<16} credit {}", c.id, c.email, c.name, format_money(c.credit));
            }
        }
        Command::Customer { cmd: CustomerCmd::Show { customer } } => {
            let c = h.customer_named(&customer)?;
            say!(out, "{} {} <{}>", c.id, c.name, c.email);
            say!(out, "  deposited {}, credit {}", format_money(c.deposited), format_money(c.credit));
            for b in h.bookings.by_customer(&c.id) {
                line(out, &h, &b)?;
            }
        }
        Command::Avail { venue, from, nights } => {
            let venue = h.venue_named(&venue)?;
            let first = parse_night(&from)?;
            let now = svc.now();
            for room in h.rooms.by_venue(&venue.id) {
                let mut cells = Vec::new();
                let mut total = 0;
                for night in first..first + nights {
                    let holder = h.holder(room.id, night).filter(|id| !h.lapsed(*id, now));
                    cells.push(match holder {
                        Some(id) => format!("#{id}"),
                        None if !venue.open_on(night) => "closed".to_string(),
                        None => {
                            total += h.price(&room, night);
                            format_money(h.price(&room, night))
                        }
                    });
                }
                say!(out, "room {:<4} {}  total free {}", room.number, cells.join(" "), format_money(total));
            }
        }
        Command::Show { id, at, as_of } => {
            let snapshot = match (at, as_of) {
                (Some(version), _) => svc.db().at(version)?,
                (None, Some(time)) => svc.db().as_of(parse_time(&time)?)?,
                (None, None) => h.clone(),
            };
            let b = snapshot.bookings.get(&id).ok_or(BookingError::NoBooking(id))?;
            say!(out, "as of v{} ({})", snapshot.version(), format_time(snapshot.commit().time_ms));
            detail(out, &snapshot, &b)?;
        }
        Command::Receipt { id } => {
            let receipt = as_paid(svc.db(), id)?;
            say!(out, "receipt for #{id}, paid {} in v{}", format_time(receipt.paid_ms), receipt.version);
            say!(out, "  {}", receipt.venue);
            say!(out, "  room {} {} x{}", receipt.room_number, format_night(receipt.then.first), receipt.then.nights);
            for ((night, quoted), (then, now)) in receipt
                .then
                .nights()
                .zip(&receipt.then.prices)
                .zip(receipt.rates_then.iter().zip(&receipt.rates_now))
            {
                say!(
                    out,
                    "  {}  quoted {}  rate then {}  rate now {}",
                    format_night(night),
                    format_money(*quoted),
                    format_money(*then),
                    format_money(*now)
                );
            }
            say!(
                out,
                "  total {}, credit {} before and {} after",
                format_money(receipt.then.total),
                format_money(receipt.credit_before),
                format_money(receipt.credit_after)
            );
            say!(out, "  now {}, refunded {}", receipt.now.status, format_money(receipt.now.refunded));
        }
        Command::Audit { id } => {
            for event in audit(svc.db(), id)? {
                say!(
                    out,
                    "v{:<6} {}  {:<9} paid {:>8} refunded {:>8}  {}",
                    event.version,
                    format_time(event.time_ms),
                    event.booking.status,
                    format_money(event.booking.paid),
                    format_money(event.booking.refunded),
                    event.message
                );
            }
        }
        Command::Report { from, nights, as_of } => {
            let h = match as_of {
                Some(time) => svc.db().as_of(parse_time(&time)?)?,
                None => h,
            };
            say!(out, "as of v{} ({})", h.version(), format_time(h.commit().time_ms));
            let first = match from {
                Some(from) => parse_night(&from)?,
                None => night_of(svc.now()),
            };
            for venue in h.venues.iter() {
                let mut cells = Vec::new();
                for night in first..first + nights {
                    let load = h.loads.get(&(venue.id, night)).map(|l| *l).unwrap_or_default();
                    cells.push(format!("{}+{}/{}", load.sold, load.held, h.capacity(venue.id, night)));
                }
                say!(out, "{:<18} {}", venue.name, cells.join(" "));
            }
            let counts = h.bookings.by_status_counts();
            let counts: Vec<String> = counts.iter().map(|(s, n)| format!("{s} {n}")).collect();
            say!(out, "bookings: {}", counts.join(", "));
            say!(
                out,
                "deposits {}  charged {}  refunded {}  customer credit {}",
                format_money(h.till.deposits),
                format_money(h.till.charged),
                format_money(h.till.refunded),
                format_money(h.customers.iter().map(|c| c.credit).sum())
            );
            let refunds = h.payments.iter().filter(|p| p.flow == Flow::Refund).count();
            say!(out, "payments {} ({refunds} refunds)", h.payments.len());
        }
        Command::Check { every } => {
            let walked = walk(svc.db(), every)?;
            for violation in walked.violations.iter().take(20) {
                say!(out, "v{}: {}", violation.version, violation.message);
            }
            say!(
                out,
                "checked v{}..v{}: {} transitions, {} full checks, {} takeovers of lapsed holds, {} violations in {:.2?}",
                walked.first,
                walked.last,
                walked.steps,
                walked.full,
                walked.takeovers,
                walked.violations.len(),
                walked.elapsed
            );
            if !walked.ok() {
                return Err(BookingError::Usage(format!("{} invariant violations", walked.violations.len())));
            }
        }
        Command::Log { limit } => {
            for commit in svc.db().log().values().rev().take(limit) {
                say!(out, "v{:<6} {}  {}", commit.version, format_time(commit.time_ms), commit.message.replace('\n', " | "));
            }
        }
        Command::Whatif { cmd: WhatIfCmd::List } => {
            for (name, scenario) in whatif::list(svc)? {
                let scenario = scenario.map(|s| s.to_string()).unwrap_or_default();
                say!(out, "{name}  {scenario}");
            }
        }
        Command::Whatif { cmd: WhatIfCmd::Show { name } } => {
            let p = whatif::preview(svc, &name)?;
            preview(out, &h, &p)?;
        }
        _ => unreachable!("write command routed to run_read"),
    }
    Ok(())
}

pub fn run_write(out: Out<'_>, svc: &Reservations, command: Command) -> Result<()> {
    match command {
        Command::Seed {
            venues,
            rooms,
            customers,
            credit,
        } => {
            let plan = Plan {
                venues,
                rooms,
                customers,
                credit: parse_money(&credit)?,
                ..Plan::default()
            };
            let seeded = seed(svc, &plan)?;
            say!(
                out,
                "seeded {} venues, {} rooms, {} customers",
                seeded.venues.len(),
                seeded.rooms.len(),
                seeded.customers.len()
            );
        }
        Command::Venue { cmd: VenueCmd::Add { name, city } } => {
            let id = svc.add_venue(&name, &city)?;
            say!(out, "venue {id} {name}");
        }
        Command::Room { cmd: RoomCmd::Add { venue, number, rate } } => {
            let venue = svc.read().venue_named(&venue)?.id;
            let id = svc.add_room(venue, number, parse_money(&rate)?)?;
            say!(out, "room {id} is number {number} at venue {venue}");
        }
        Command::Room { cmd: RoomCmd::Rate { room, rate, night, clear } } => {
            let room = room_id(&svc.read(), &room)?;
            let rate = match (rate, clear) {
                (Some(rate), false) => Some(parse_money(&rate)?),
                (None, true) => None,
                _ => return Err(BookingError::Usage("give a rate or --clear".to_string())),
            };
            match (night, rate) {
                (Some(night), rate) => {
                    svc.set_night_rate(room, parse_night(&night)?, rate)?;
                    say!(out, "room {room} on {night}: {}", rate.map_or("base rate".to_string(), format_money));
                }
                (None, Some(rate)) => {
                    let old = svc.set_rate(room, rate)?;
                    say!(out, "room {room}: {} -> {}", format_money(old), format_money(rate));
                }
                (None, None) => return Err(BookingError::Usage("--clear needs --night".to_string())),
            }
        }
        Command::Customer { cmd: CustomerCmd::Add { email, name, deposit } } => {
            let id = svc.add_customer(&email, &name)?;
            if let Some(amount) = deposit {
                svc.deposit(id, parse_money(&amount)?)?;
            }
            say!(out, "customer {id} {email}");
        }
        Command::Customer { cmd: CustomerCmd::Deposit { customer, amount } } => {
            let id = svc.read().customer_named(&customer)?.id;
            let credit = svc.deposit(id, parse_money(&amount)?)?;
            say!(out, "customer {id} credit {}", format_money(credit));
        }
        Command::Hold {
            customer,
            room,
            from,
            nights,
            ttl,
        } => {
            let snapshot = svc.read();
            let request = HoldRequest {
                customer: snapshot.customer_named(&customer)?.id,
                room: room_id(&snapshot, &room)?,
                first: parse_night(&from)?,
                nights,
                ttl_ms: ttl.saturating_mul(1000),
            };
            drop(snapshot);
            let held = svc.hold(&request)?;
            for id in &held.released {
                say!(out, "released lapsed hold #{id}");
            }
            say!(
                out,
                "held #{} for {} until {}",
                held.id,
                format_money(held.total),
                format_time(held.expires_ms)
            );
        }
        Command::Confirm { id, expect } => {
            let expect = expect.map(|e| parse_money(&e)).transpose()?;
            let paid = svc.confirm(id, expect)?;
            say!(out, "confirmed #{id}, paid {}", format_money(paid));
        }
        Command::Cancel { id } => {
            let refund = svc.cancel(id)?;
            say!(out, "cancelled #{id}, refunded {}", format_money(refund));
        }
        Command::Sweep => {
            let expired = svc.sweep()?;
            say!(out, "released {} lapsed holds", expired.len());
        }
        Command::Whatif { cmd } => {
            let p = match cmd {
                WhatIfCmd::Reprice { name, venue, percent } => {
                    let venue = venue.map(|v| svc.read().venue_named(&v).map(|v| v.id)).transpose()?;
                    whatif::propose(svc, &name, Scenario::Reprice { venue, percent })?
                }
                WhatIfCmd::Close { name, venue } => {
                    let venue = svc.read().venue_named(&venue)?.id;
                    whatif::propose(svc, &name, Scenario::Close { venue })?
                }
                WhatIfCmd::Apply { name } => {
                    let applied = whatif::apply(svc, &name)?;
                    say!(out, "merged {name} as v{}: {}", applied.version, applied.summary);
                    if applied.conflicts > 0 {
                        say!(out, "  {} rows changed on both sides, rebased onto main", applied.conflicts);
                    }
                    return Ok(());
                }
                WhatIfCmd::Drop { name } => {
                    whatif::discard(svc, &name)?;
                    say!(out, "dropped {name}");
                    return Ok(());
                }
                WhatIfCmd::Show { .. } | WhatIfCmd::List => unreachable!("read command routed to run_write"),
            };
            preview(out, &svc.read(), &p)?;
            say!(out, "apply with `whatif apply {}` or discard with `whatif drop {}`", p.name, p.name);
        }
        _ => unreachable!("read command routed to run_write"),
    }
    Ok(())
}
