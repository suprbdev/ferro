use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use ferro::{Change, Db, Snapshot};

use crate::error::Result;
use crate::model::{Booking, Cents, Flow, Hotels, Load, Night, Status};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub version: u64,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct Walked {
    pub first: u64,
    pub last: u64,
    pub steps: u64,
    pub full: u64,
    pub takeovers: u64,
    pub expired: u64,
    pub expiry_lag_max_ms: u64,
    pub expiry_lag_total_ms: u64,
    pub slowest_expiry: Option<(u64, u64, u64)>,
    pub elapsed: Duration,
    pub violations: Vec<Violation>,
}

impl Walked {
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn expiry_lag_mean_ms(&self) -> f64 {
        if self.expired == 0 {
            0.0
        } else {
            self.expiry_lag_total_ms as f64 / self.expired as f64
        }
    }
}

struct Report<'a> {
    out: &'a mut Vec<String>,
}

impl Report<'_> {
    fn check(&mut self, ok: bool, message: impl FnOnce() -> String) {
        if !ok {
            self.out.push(message());
        }
    }
}

fn booking_alone(b: &Booking, out: &mut Report<'_>) {
    let id = b.id;
    out.check(b.nights >= 1, || format!("booking #{id} has no nights"));
    out.check(b.prices.len() == b.nights as usize, || {
        format!("booking #{id} has {} prices for {} nights", b.prices.len(), b.nights)
    });
    out.check(b.prices.iter().all(|price| *price > 0), || {
        format!("booking #{id} has a price that is not positive")
    });
    out.check(b.prices.iter().sum::<Cents>() == b.total, || {
        format!("booking #{id} total {} is not the sum of its prices", b.total)
    });
    out.check(b.created_ms <= b.expires_ms, || {
        format!("booking #{id} expires before it was created")
    });
    let held_until = (b.status == Status::Held).then_some(b.expires_ms);
    out.check(b.hold_until == held_until, || {
        format!("booking #{id} is {} with hold_until {:?}", b.status, b.hold_until)
    });
    out.check(b.paid_version.is_some() == b.paid_ms.is_some(), || {
        format!("booking #{id} was paid at {:?} in version {:?}", b.paid_ms, b.paid_version)
    });
    match b.status {
        Status::Held => {
            out.check(b.paid == 0 && b.refunded == 0 && b.paid_ms.is_none(), || {
                format!("held booking #{id} has money on it")
            });
            out.check(b.ended_ms.is_none(), || format!("held booking #{id} has ended"));
        }
        Status::Confirmed => {
            out.check(b.paid == b.total && b.refunded == 0, || {
                format!("confirmed booking #{id} paid {} of {}", b.paid, b.total)
            });
            out.check(b.paid_ms.is_some_and(|paid| paid < b.expires_ms), || {
                format!("booking #{id} was paid at {:?}, after its hold ran out", b.paid_ms)
            });
            out.check(b.ended_ms.is_none(), || format!("confirmed booking #{id} has ended"));
        }
        Status::Expired => {
            out.check(b.paid == 0 && b.refunded == 0 && b.paid_ms.is_none(), || {
                format!("expired booking #{id} has money on it")
            });
            out.check(b.ended_ms.is_some_and(|ended| ended >= b.expires_ms), || {
                format!(
                    "booking #{id} expired at {:?}, before its hold ran out at {}",
                    b.ended_ms, b.expires_ms
                )
            });
        }
        Status::Cancelled => {
            out.check(b.paid == 0 || b.paid == b.total, || {
                format!("cancelled booking #{id} paid {} of {}", b.paid, b.total)
            });
            out.check(b.refunded >= 0 && b.refunded <= b.paid, || {
                format!("cancelled booking #{id} refunded {} of {}", b.refunded, b.paid)
            });
            out.check(b.ended_ms.is_some(), || format!("cancelled booking #{id} has no end"));
        }
    }
}

fn booking_in(h: &Hotels, b: &Booking, out: &mut Report<'_>) {
    let id = b.id;
    booking_alone(b, out);
    out.check(id <= h.next_booking, || format!("booking #{id} is past next_booking"));
    out.check(h.customers.contains(&b.customer), || {
        format!("booking #{id} names missing customer {}", b.customer)
    });
    match h.rooms.get(&b.room) {
        Some(room) => out.check(room.venue == b.venue, || {
            format!("booking #{id} says venue {} but room {} is in {}", b.venue, b.room, room.venue)
        }),
        None => out.check(false, || format!("booking #{id} names missing room {}", b.room)),
    }
    let stays: Vec<_> = h.stays.range((id, 0)..=(id, Night::MAX)).collect();
    if b.status.active() {
        out.check(stays.len() == b.nights as usize, || {
            format!("{} booking #{id} holds {} of {} nights", b.status, stays.len(), b.nights)
        });
        for (slot, stay) in &stays {
            let sold = b.status == Status::Confirmed;
            let fits = stay.room == b.room
                && stay.venue == b.venue
                && stay.night == slot.1
                && stay.night >= b.first
                && stay.night <= b.last_night()
                && stay.sold == sold;
            out.check(fits, || format!("stay {slot:?} does not match booking #{id}"));
        }
    } else {
        out.check(stays.is_empty(), || {
            format!("{} booking #{id} still holds {} nights", b.status, stays.len())
        });
    }
}

pub fn check_state(h: &Hotels) -> Vec<String> {
    let mut messages = Vec::new();
    let out = &mut Report { out: &mut messages };

    let mut statuses: BTreeMap<Status, usize> = BTreeMap::new();
    for booking in h.bookings.iter() {
        *statuses.entry(booking.status).or_default() += 1;
        booking_in(h, &booking, out);
    }
    let indexed: BTreeMap<Status, usize> = h.bookings.by_status_counts().into_iter().collect();
    out.check(indexed == statuses, || {
        format!("status index {indexed:?} disagrees with rows {statuses:?}")
    });
    let mut held: Vec<u64> = h.bookings.by_status(&Status::Held).keys().collect();
    let mut due: Vec<u64> = h.bookings.by_hold_until_range(Some(0)..).keys().collect();
    held.sort_unstable();
    due.sort_unstable();
    out.check(held == due, || {
        format!("hold_until index lists {} bookings, {} are held", due.len(), held.len())
    });

    let mut units: HashMap<(u32, Night), (u64, Night)> = HashMap::new();
    let mut loads: BTreeMap<(u32, Night), Load> = BTreeMap::new();
    for (slot, stay) in h.stays.rows().iter() {
        if let Some(other) = units.insert((stay.room, stay.night), slot) {
            out.check(false, || {
                format!("room {} night {} is double-booked by {other:?} and {slot:?}", stay.room, stay.night)
            });
        }
        let holder = h.bookings.get(&slot.0);
        out.check(holder.as_ref().is_some_and(|b| b.status.active()), || {
            format!("stay {slot:?} belongs to no active booking")
        });
        let load = loads.entry((stay.venue, stay.night)).or_default();
        if stay.sold {
            load.sold += 1;
        } else {
            load.held += 1;
        }
        let open = h.venues.get(&stay.venue).is_some_and(|venue| venue.open_on(stay.night));
        out.check(open, || format!("stay {slot:?} is at a venue closed that night"));
    }
    let index = h.stays.unique_room_night();
    out.check(index.len() == units.len(), || {
        format!("room-night index has {} entries for {} stays", index.len(), units.len())
    });
    for (unit, slot) in index.iter() {
        out.check(units.get(&unit) == Some(&*slot), || {
            format!("room-night index maps {unit:?} to {:?}, rows disagree", *slot)
        });
    }
    let stored: BTreeMap<(u32, Night), Load> =
        h.loads.iter().map(|(key, load)| (key, *load)).collect();
    out.check(stored == loads, || {
        let wrong: Vec<_> = stored
            .iter()
            .filter(|(key, load)| loads.get(key) != Some(load))
            .map(|(key, _)| key)
            .take(5)
            .collect();
        format!("load counters disagree with stays at {wrong:?}")
    });
    for ((venue, night), load) in &loads {
        let capacity = h.capacity(*venue, *night);
        out.check(load.held + load.sold <= capacity, || {
            format!("venue {venue} night {night} has {} stays for {capacity} rooms", load.held + load.sold)
        });
    }

    let mut charged: HashMap<u64, Cents> = HashMap::new();
    let mut refunded: HashMap<u64, Cents> = HashMap::new();
    let mut net: HashMap<u32, Cents> = HashMap::new();
    let mut till_charged = 0;
    let mut till_refunded = 0;
    for payment in h.payments.iter() {
        out.check(payment.amount > 0, || format!("payment {} is not positive", payment.id));
        out.check(payment.id <= h.next_payment, || format!("payment {} is past next_payment", payment.id));
        let owner = h.bookings.get(&payment.booking).map(|b| b.customer);
        out.check(owner == Some(payment.customer), || {
            format!("payment {} is for {:?}, booking says {owner:?}", payment.id, payment.customer)
        });
        match payment.flow {
            Flow::Charge => {
                *charged.entry(payment.booking).or_default() += payment.amount;
                *net.entry(payment.customer).or_default() += payment.amount;
                till_charged += payment.amount;
            }
            Flow::Refund => {
                *refunded.entry(payment.booking).or_default() += payment.amount;
                *net.entry(payment.customer).or_default() -= payment.amount;
                till_refunded += payment.amount;
            }
        }
    }
    for booking in h.bookings.iter() {
        let paid = charged.get(&booking.id).copied().unwrap_or(0);
        let back = refunded.get(&booking.id).copied().unwrap_or(0);
        out.check(paid == booking.paid && back == booking.refunded, || {
            format!(
                "booking #{} says paid {} refunded {}, payments say {paid} and {back}",
                booking.id, booking.paid, booking.refunded
            )
        });
    }
    out.check(till_charged == h.till.charged && till_refunded == h.till.refunded, || {
        format!(
            "till says charged {} refunded {}, payments say {till_charged} and {till_refunded}",
            h.till.charged, h.till.refunded
        )
    });
    let mut deposits = 0;
    let mut credit = 0;
    for customer in h.customers.iter() {
        deposits += customer.deposited;
        credit += customer.credit;
        let spent = net.get(&customer.id).copied().unwrap_or(0);
        out.check(customer.credit >= 0, || {
            format!("customer {} has negative credit {}", customer.id, customer.credit)
        });
        out.check(customer.credit == customer.deposited - spent, || {
            format!(
                "customer {} has credit {} but deposited {} and spent {spent}",
                customer.id, customer.credit, customer.deposited
            )
        });
    }
    out.check(deposits == h.till.deposits, || {
        format!("customers deposited {deposits}, till says {}", h.till.deposits)
    });
    out.check(credit + h.till.charged - h.till.refunded == h.till.deposits, || {
        format!("credit {credit} plus net charges does not equal deposits {}", h.till.deposits)
    });
    messages
}

#[derive(Default)]
struct Step {
    takeovers: u64,
    expired: u64,
    lag_max: u64,
    lag_total: u64,
    slowest: Option<(u64, u64)>,
}

fn same_terms(a: &Booking, b: &Booking) -> bool {
    a.customer == b.customer
        && a.room == b.room
        && a.venue == b.venue
        && a.first == b.first
        && a.nights == b.nights
        && a.prices == b.prices
        && a.total == b.total
        && a.created_ms == b.created_ms
        && a.expires_ms == b.expires_ms
}

fn quoted(prev: &Hotels, cur: &Hotels, b: &Booking) -> bool {
    let at = |h: &Hotels, night: Night| h.rooms.get(&b.room).map(|room| h.price(&room, night));
    b.nights().zip(&b.prices).all(|(night, price)| {
        at(prev, night) == Some(*price) || at(cur, night) == Some(*price)
    })
}

fn paid_here(version: Option<u64>, before: Option<u64>, b: &Booking, out: &mut Report<'_>) {
    let Some(version) = version.filter(|_| before.is_none() && b.paid_version.is_some()) else {
        return;
    };
    out.check(b.paid_version == Some(version), || {
        format!("booking #{} was paid in v{version} but recorded {:?}", b.id, b.paid_version)
    });
}

fn check_step(
    prev: &Hotels,
    cur: &Hotels,
    version: Option<u64>,
    out: &mut Report<'_>,
    step: &mut Step,
) {
    for change in prev.bookings.diff(&cur.bookings) {
        match change {
            Change::Removed(id, _) => out.check(false, || format!("booking #{id} was deleted")),
            Change::Added(id, b) => {
                out.check(id > prev.next_booking, || format!("booking #{id} reused an id"));
                out.check(quoted(prev, cur, &b), || {
                    format!("booking #{id} was quoted {:?}, not the rates in force", b.prices)
                });
                paid_here(version, None, &b, out);
                booking_in(cur, &b, out);
            }
            Change::Changed(id, a, b) => {
                out.check(same_terms(&a, &b), || format!("booking #{id} changed its terms"));
                out.check(a.paid_version.is_none() || a.paid_version == b.paid_version, || {
                    let (was, now) = (a.paid_version, b.paid_version);
                    format!("booking #{id} moved its paid version from {was:?} to {now:?}")
                });
                paid_here(version, a.paid_version, &b, out);
                let legal = matches!(
                    (a.status, b.status),
                    (Status::Held, Status::Confirmed)
                        | (Status::Held, Status::Expired)
                        | (Status::Held, Status::Cancelled)
                        | (Status::Confirmed, Status::Cancelled)
                );
                out.check(legal, || format!("booking #{id} went from {} to {}", a.status, b.status));
                if b.status == Status::Expired {
                    let lag = b.ended_ms.unwrap_or(0).saturating_sub(b.expires_ms);
                    step.expired += 1;
                    if lag >= step.lag_max {
                        step.slowest = Some((lag, id));
                    }
                    step.lag_max = step.lag_max.max(lag);
                    step.lag_total += lag;
                }
                booking_in(cur, &b, out);
            }
        }
    }
    let before = prev.stays.unique_room_night();
    let after = cur.stays.unique_room_night();
    for change in before.diff(after) {
        let Change::Changed((room, night), old, new) = change else {
            continue;
        };
        if old.0 == new.0 {
            continue;
        }
        step.takeovers += 1;
        let loser = cur.bookings.get(&old.0);
        let winner = cur.bookings.get(&new.0);
        let fair = match (loser, winner) {
            (Some(loser), Some(winner)) => match loser.status {
                Status::Expired => loser.expires_ms <= winner.created_ms,
                Status::Cancelled => loser.ended_ms.is_some_and(|ended| ended <= winner.created_ms),
                _ => false,
            },
            _ => false,
        };
        out.check(fair, || {
            format!("room {room} night {night} passed from #{} to #{} while still held", old.0, new.0)
        });
    }
    let mut charged = 0;
    let mut refunded = 0;
    for change in prev.payments.diff(&cur.payments) {
        match change {
            Change::Added(id, payment) => {
                out.check(id > prev.next_payment, || format!("payment {id} reused an id"));
                match payment.flow {
                    Flow::Charge => charged += payment.amount,
                    Flow::Refund => refunded += payment.amount,
                }
            }
            other => out.check(false, || format!("payment {:?} was rewritten", other.key())),
        }
    }
    out.check(cur.till.charged - prev.till.charged == charged, || {
        format!("till charged moved by {} for {charged} of new charges", cur.till.charged - prev.till.charged)
    });
    out.check(cur.till.refunded - prev.till.refunded == refunded, || {
        format!("till refunded moved by {} for {refunded} of new refunds", cur.till.refunded - prev.till.refunded)
    });
    out.check(cur.till.deposits >= prev.till.deposits, || "deposits went down".to_string());
    for change in prev.venues.diff(&cur.venues) {
        if let Change::Changed(id, a, b) = change {
            let kept = match (a.closed_from, b.closed_from) {
                (Some(was), Some(now)) => now <= was,
                (Some(_), None) => false,
                _ => true,
            };
            out.check(kept, || format!("venue {id} reopened"));
        }
    }
}

pub fn check_pair(prev: &Hotels, cur: &Hotels) -> Vec<String> {
    let mut messages = Vec::new();
    check_step(prev, cur, None, &mut Report { out: &mut messages }, &mut Step::default());
    messages
}

pub fn walk(db: &Db<Hotels>, full_every: u64) -> Result<Walked> {
    let started = Instant::now();
    let log = db.log();
    let mut walked = Walked::default();
    let (Some((first, _)), Some((last, _))) = (log.first(), log.last()) else {
        walked.elapsed = started.elapsed();
        return Ok(walked);
    };
    walked.first = first;
    walked.last = last;
    let mut prev: Snapshot<Hotels> = match db.at(first - 1) {
        Err(ferro::Error::NoVersion(_)) => db.at(first)?,
        other => other?,
    };
    for version in first..=last {
        let cur = db.at(version)?;
        let mut messages = Vec::new();
        let mut step = Step::default();
        if cur.version() != prev.version() {
            check_step(&prev, &cur, Some(version), &mut Report { out: &mut messages }, &mut step);
            walked.steps += 1;
        }
        let sampled = full_every > 0 && (version % full_every == 0 || version == last);
        if sampled || version == first {
            messages.extend(check_state(&cur));
            walked.full += 1;
        }
        walked.takeovers += step.takeovers;
        walked.expired += step.expired;
        if let Some((lag, id)) = step.slowest
            && walked.slowest_expiry.is_none_or(|(most, _, _)| lag > most)
        {
            walked.slowest_expiry = Some((lag, version, id));
        }
        walked.expiry_lag_max_ms = walked.expiry_lag_max_ms.max(step.lag_max);
        walked.expiry_lag_total_ms += step.lag_total;
        walked
            .violations
            .extend(messages.into_iter().map(|message| Violation { version, message }));
        prev = cur;
    }
    walked.elapsed = started.elapsed();
    Ok(walked)
}
