use ferro::Val;

use crate::date::{format_night, night_of};
use crate::error::{BookingError, Result};
use crate::model::{
    Booking, Cents, Customer, Flow, Hotels, Load, Night, Payment, Room, Scenario, Status, Stay,
    Venue,
};

pub const MAX_NIGHTS: u32 = 14;
pub const FREE_CANCEL_NIGHTS: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldRequest {
    pub customer: u32,
    pub room: u32,
    pub first: Night,
    pub nights: u32,
    pub ttl_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub id: u64,
    pub total: Cents,
    pub expires_ms: u64,
    pub released: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    Customer,
    VenueClosed,
}

impl Why {
    fn as_str(self) -> &'static str {
        match self {
            Why::Customer => "cancelled by customer",
            Why::VenueClosed => "venue closed",
        }
    }
}

pub fn refund_for(booking: &Booking, now: u64, why: Why) -> Cents {
    if booking.status != Status::Confirmed {
        return 0;
    }
    let today = night_of(now);
    match why {
        Why::VenueClosed => booking.paid,
        Why::Customer if today + FREE_CANCEL_NIGHTS <= booking.first => booking.paid,
        Why::Customer if today <= booking.first => booking.paid / 2,
        Why::Customer => 0,
    }
}

pub fn scaled(rate: Cents, percent: i32) -> Cents {
    let factor = 100 + i64::from(percent);
    ((rate.saturating_mul(factor) + 50) / 100).max(1)
}

fn add(total: Cents, amount: Cents) -> Result<Cents> {
    total.checked_add(amount).ok_or(BookingError::Overflow)
}

impl Hotels {
    pub fn venue_named(&self, name: &str) -> Result<Val<Venue>> {
        if let Some(venue) = self.venues.by_name(name) {
            return Ok(venue);
        }
        name.parse::<u32>()
            .ok()
            .and_then(|id| self.venues.get(&id))
            .ok_or_else(|| BookingError::NoVenue(name.to_string()))
    }

    pub fn customer_named(&self, name: &str) -> Result<Val<Customer>> {
        if let Some(customer) = self.customers.by_email(name) {
            return Ok(customer);
        }
        name.parse::<u32>()
            .ok()
            .and_then(|id| self.customers.get(&id))
            .ok_or_else(|| BookingError::NoCustomer(name.to_string()))
    }

    pub fn room_at(&self, venue: u32, number: u16) -> Option<Val<Room>> {
        self.rooms.by_venue_number(&(venue, number))
    }

    pub fn add_venue(&mut self, name: &str, city: &str) -> Result<u32> {
        let id = self.next_venue + 1;
        let venue = Venue {
            id,
            name: name.to_string(),
            city: city.to_string(),
            closed_from: None,
        };
        self.venues
            .try_insert(venue)
            .map_err(|_| BookingError::VenueExists(name.to_string()))?;
        self.next_venue = id;
        Ok(id)
    }

    pub fn add_room(&mut self, venue: u32, number: u16, rate: Cents) -> Result<u32> {
        if rate <= 0 {
            return Err(BookingError::BadAmount(rate));
        }
        if !self.venues.contains(&venue) {
            return Err(BookingError::NoVenue(venue.to_string()));
        }
        let id = self.next_room + 1;
        let room = Room {
            id,
            venue,
            number,
            rate,
        };
        self.rooms
            .try_insert(room)
            .map_err(|_| BookingError::RoomExists { venue, number })?;
        self.next_room = id;
        Ok(id)
    }

    pub fn add_customer(&mut self, email: &str, name: &str) -> Result<u32> {
        let id = self.next_customer + 1;
        let customer = Customer {
            id,
            email: email.to_string(),
            name: name.to_string(),
            credit: 0,
            deposited: 0,
        };
        self.customers
            .try_insert(customer)
            .map_err(|taken| BookingError::EmailTaken {
                email: email.to_string(),
                holder: taken.holder,
            })?;
        self.next_customer = id;
        Ok(id)
    }

    pub fn deposit(&mut self, customer: u32, amount: Cents) -> Result<Cents> {
        if amount <= 0 {
            return Err(BookingError::BadAmount(amount));
        }
        let mut row = self
            .customers
            .get_mut(&customer)
            .ok_or_else(|| BookingError::NoCustomer(customer.to_string()))?;
        row.credit = add(row.credit, amount)?;
        row.deposited = add(row.deposited, amount)?;
        let credit = row.credit;
        drop(row);
        self.till.deposits = add(self.till.deposits, amount)?;
        Ok(credit)
    }

    pub fn set_rate(&mut self, room: u32, rate: Cents) -> Result<Cents> {
        if rate <= 0 {
            return Err(BookingError::BadAmount(rate));
        }
        let mut row = self
            .rooms
            .get_mut(&room)
            .ok_or(BookingError::NoRoom(room))?;
        let old = row.rate;
        row.rate = rate;
        Ok(old)
    }

    pub fn set_night_rate(&mut self, room: u32, night: Night, rate: Option<Cents>) -> Result<()> {
        if !self.rooms.contains(&room) {
            return Err(BookingError::NoRoom(room));
        }
        match rate {
            Some(rate) if rate <= 0 => return Err(BookingError::BadAmount(rate)),
            Some(rate) => {
                self.rates.insert((room, night), rate);
            }
            None => {
                self.rates.remove(&(room, night));
            }
        }
        Ok(())
    }

    pub fn lapsed(&self, id: u64, now: u64) -> bool {
        self.bookings
            .get(&id)
            .is_some_and(|booking| booking.status == Status::Held && booking.expires_ms <= now)
    }

    pub fn hold(&mut self, now: u64, request: &HoldRequest) -> Result<Held> {
        if request.nights == 0 || request.nights > MAX_NIGHTS {
            return Err(BookingError::BadStay {
                given: request.nights,
                max: MAX_NIGHTS,
            });
        }
        if request.first < night_of(now) {
            return Err(BookingError::PastNight {
                night: format_night(request.first),
            });
        }
        if !self.customers.contains(&request.customer) {
            return Err(BookingError::NoCustomer(request.customer.to_string()));
        }
        let room = self
            .rooms
            .get(&request.room)
            .ok_or(BookingError::NoRoom(request.room))?
            .into_owned();
        let last = request.first + request.nights - 1;
        if self
            .venues
            .get(&room.venue)
            .is_none_or(|venue| !venue.open_on(last))
        {
            return Err(BookingError::VenueClosed(room.venue));
        }
        let id = self.next_booking + 1;
        let mut released = Vec::new();
        let mut prices = Vec::with_capacity(request.nights as usize);
        for night in request.first..request.first + request.nights {
            prices.push(self.price(&room, night));
            let stay = Stay {
                slot: (id, night),
                room: room.id,
                night,
                venue: room.venue,
                sold: false,
            };
            while let Err(taken) = self.stays.try_insert(stay.clone()) {
                let holder = taken.holder.0;
                if !self.lapsed(holder, now) {
                    return Err(BookingError::unavailable(room.id, night, holder));
                }
                self.expire(holder, now);
                released.push(holder);
            }
            self.loads
                .get_or_insert_with((room.venue, night), Load::default)
                .held += 1;
        }
        let total = prices.iter().try_fold(0, |sum, price| add(sum, *price))?;
        let expires_ms = now.saturating_add(request.ttl_ms);
        self.bookings.insert(Booking {
            id,
            customer: request.customer,
            status: Status::Held,
            hold_until: Some(expires_ms),
            venue: room.venue,
            room: room.id,
            first: request.first,
            nights: request.nights,
            prices,
            total,
            created_ms: now,
            expires_ms,
            paid: 0,
            refunded: 0,
            paid_ms: None,
            paid_version: None,
            ended_ms: None,
            reason: None,
        });
        self.next_booking = id;
        Ok(Held {
            id,
            total,
            expires_ms,
            released,
        })
    }

    fn booking(&self, id: u64) -> Result<Booking> {
        self.bookings
            .get(&id)
            .map(Val::into_owned)
            .ok_or(BookingError::NoBooking(id))
    }

    fn pay(&mut self, booking: &Booking, flow: Flow, amount: Cents, now: u64) -> Result<()> {
        let id = self.next_payment + 1;
        self.payments.insert(Payment {
            id,
            booking: booking.id,
            customer: booking.customer,
            flow,
            amount,
            at_ms: now,
        });
        self.next_payment = id;
        let mut customer = self
            .customers
            .get_mut(&booking.customer)
            .ok_or_else(|| BookingError::NoCustomer(booking.customer.to_string()))?;
        match flow {
            Flow::Charge => {
                customer.credit -= amount;
                drop(customer);
                self.till.charged = add(self.till.charged, amount)?;
            }
            Flow::Refund => {
                customer.credit = add(customer.credit, amount)?;
                drop(customer);
                self.till.refunded = add(self.till.refunded, amount)?;
            }
        }
        Ok(())
    }

    fn release(&mut self, booking: &Booking) {
        for night in booking.nights() {
            let Some(stay) = self.stays.remove(&(booking.id, night)) else {
                continue;
            };
            let key = (stay.venue, night);
            let empty = match self.loads.get_mut(&key) {
                Some(load) => {
                    if stay.sold {
                        load.sold -= 1;
                    } else {
                        load.held -= 1;
                    }
                    *load == Load::default()
                }
                None => false,
            };
            if empty {
                self.loads.remove(&key);
            }
        }
    }

    pub fn expire(&mut self, id: u64, now: u64) -> bool {
        let Ok(mut booking) = self.booking(id) else {
            return false;
        };
        if booking.status != Status::Held || booking.expires_ms > now {
            return false;
        }
        self.release(&booking);
        booking.status = Status::Expired;
        booking.hold_until = None;
        booking.ended_ms = Some(now);
        booking.reason = Some("hold expired".to_string());
        self.bookings.insert(booking);
        true
    }

    pub fn due(&self, now: u64) -> Vec<u64> {
        self.bookings
            .by_hold_until_range(Some(0)..=Some(now))
            .keys()
            .collect()
    }

    pub fn expire_due(&mut self, now: u64) -> Vec<u64> {
        let due = self.due(now);
        due.into_iter().filter(|id| self.expire(*id, now)).collect()
    }

    pub fn confirm(
        &mut self,
        now: u64,
        version: u64,
        id: u64,
        expected: Option<Cents>,
    ) -> Result<Cents> {
        let mut booking = self.booking(id)?;
        if booking.status != Status::Held {
            return Err(BookingError::WrongStatus {
                id,
                status: booking.status,
                wanted: "held",
            });
        }
        if now >= booking.expires_ms {
            return Err(BookingError::HoldExpired {
                id,
                expired_ms: booking.expires_ms,
            });
        }
        if let Some(expected) = expected.filter(|expected| *expected != booking.total) {
            return Err(BookingError::PriceChanged {
                id,
                total: booking.total,
                expected,
            });
        }
        let credit = self
            .customers
            .get(&booking.customer)
            .map(|customer| customer.credit)
            .ok_or_else(|| BookingError::NoCustomer(booking.customer.to_string()))?;
        if credit < booking.total {
            return Err(BookingError::InsufficientFunds {
                customer: booking.customer,
                needed: booking.total,
                available: credit,
            });
        }
        self.pay(&booking, Flow::Charge, booking.total, now)?;
        for night in booking.nights() {
            self.stays.update(&(id, night), |stay| stay.sold = true);
            if let Some(load) = self.loads.get_mut(&(booking.venue, night)) {
                load.held -= 1;
                load.sold += 1;
            }
        }
        booking.status = Status::Confirmed;
        booking.hold_until = None;
        booking.paid = booking.total;
        booking.paid_ms = Some(now);
        booking.paid_version = Some(version);
        let total = booking.total;
        self.bookings.insert(booking);
        Ok(total)
    }

    pub fn cancel(&mut self, now: u64, id: u64, why: Why) -> Result<Cents> {
        let mut booking = self.booking(id)?;
        if !booking.status.active() {
            return Err(BookingError::WrongStatus {
                id,
                status: booking.status,
                wanted: "held or confirmed",
            });
        }
        let refund = refund_for(&booking, now, why);
        if refund > 0 {
            self.pay(&booking, Flow::Refund, refund, now)?;
        }
        self.release(&booking);
        booking.status = Status::Cancelled;
        booking.hold_until = None;
        booking.refunded = refund;
        booking.ended_ms = Some(now);
        booking.reason = Some(why.as_str().to_string());
        self.bookings.insert(booking);
        Ok(refund)
    }

    pub fn active_at(&self, venue: u32, from: Night) -> Vec<u64> {
        let mut ids: Vec<u64> = Vec::new();
        for room in self.rooms.by_venue(&venue).keys() {
            let index = self.stays.unique_room_night();
            for (_, slot) in index.range((room, from)..=(room, Night::MAX)) {
                ids.push(slot.0);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    pub fn close_venue(&mut self, now: u64, venue: u32) -> Result<Vec<(u64, Cents)>> {
        let mut row = self
            .venues
            .get_mut(&venue)
            .ok_or_else(|| BookingError::NoVenue(venue.to_string()))?;
        let today = night_of(now);
        row.closed_from = Some(row.closed_from.map_or(today, |from| from.min(today)));
        let from = row.closed_from.unwrap_or(today);
        drop(row);
        let mut cancelled = Vec::new();
        for id in self.active_at(venue, from) {
            let refund = self.cancel(now, id, Why::VenueClosed)?;
            cancelled.push((id, refund));
        }
        Ok(cancelled)
    }

    pub fn reprice(&mut self, venue: Option<u32>, percent: i32) -> Result<usize> {
        if percent <= -100 {
            return Err(BookingError::Usage(format!("cannot reprice by {percent}%")));
        }
        let rooms: Vec<u32> = match venue {
            Some(venue) => self.rooms.by_venue(&venue).keys().collect(),
            None => self.rooms.keys().collect(),
        };
        for room in &rooms {
            self.rooms
                .update(room, |row| row.rate = scaled(row.rate, percent));
            let overrides: Vec<(u32, Night)> =
                self.rates.prefix(room).map(|(key, _)| key).collect();
            for key in overrides {
                if let Some(rate) = self.rates.get_mut(&key) {
                    *rate = scaled(*rate, percent);
                }
            }
        }
        Ok(rooms.len())
    }

    pub fn apply(&mut self, now: u64, scenario: &Scenario) -> Result<String> {
        match scenario {
            Scenario::Reprice { venue, percent } => {
                let rooms = self.reprice(*venue, *percent)?;
                Ok(format!("repriced {rooms} rooms by {percent:+}%"))
            }
            Scenario::Close { venue } => {
                let cancelled = self.close_venue(now, *venue)?;
                let refunds: Cents = cancelled.iter().map(|(_, refund)| refund).sum();
                Ok(format!(
                    "closed venue {venue}, cancelled {} bookings, refunded {refunds} cents",
                    cancelled.len()
                ))
            }
        }
    }
}
