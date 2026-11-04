use std::fmt;

use ferro::{Data, Map, Record, Table};

pub type Cents = i64;
pub type Night = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Data)]
pub enum Status {
    Held,
    Confirmed,
    Cancelled,
    Expired,
}

impl Status {
    pub fn active(self) -> bool {
        matches!(self, Status::Held | Status::Confirmed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Held => "held",
            Status::Confirmed => "confirmed",
            Status::Cancelled => "cancelled",
            Status::Expired => "expired",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct Venue {
    #[key]
    pub id: u32,
    #[unique]
    pub name: String,
    pub city: String,
    pub closed_from: Option<Night>,
}

impl Venue {
    pub fn open_on(&self, night: Night) -> bool {
        self.closed_from.is_none_or(|from| night < from)
    }
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
#[unique(venue, number)]
pub struct Room {
    #[key]
    pub id: u32,
    #[index]
    pub venue: u32,
    pub number: u16,
    pub rate: Cents,
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct Customer {
    #[key]
    pub id: u32,
    #[unique]
    pub email: String,
    pub name: String,
    pub credit: Cents,
    pub deposited: Cents,
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct Booking {
    #[key]
    pub id: u64,
    #[index(cover(status, venue, room, first, nights, total))]
    pub customer: u32,
    #[index]
    pub status: Status,
    #[index]
    pub hold_until: Option<u64>,
    pub venue: u32,
    pub room: u32,
    pub first: Night,
    pub nights: u32,
    pub prices: Vec<Cents>,
    pub total: Cents,
    pub created_ms: u64,
    pub expires_ms: u64,
    pub paid: Cents,
    pub refunded: Cents,
    pub paid_ms: Option<u64>,
    pub paid_version: Option<u64>,
    pub ended_ms: Option<u64>,
    pub reason: Option<String>,
}

impl Booking {
    pub fn nights(&self) -> impl Iterator<Item = Night> + use<> {
        self.first..self.first + self.nights
    }

    pub fn last_night(&self) -> Night {
        self.first + self.nights - 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub id: u64,
    pub status: Status,
    pub venue: u32,
    pub room: u32,
    pub first: Night,
    pub nights: u32,
    pub total: Cents,
}

impl From<&Booking> for Line {
    fn from(booking: &Booking) -> Self {
        Line {
            id: booking.id,
            status: booking.status,
            venue: booking.venue,
            room: booking.room,
            first: booking.first,
            nights: booking.nights,
            total: booking.total,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
#[unique(room, night)]
pub struct Stay {
    #[key]
    pub slot: (u64, Night),
    pub room: u32,
    pub night: Night,
    pub venue: u32,
    pub sold: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Data)]
pub enum Flow {
    Charge,
    Refund,
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct Payment {
    #[key]
    pub id: u64,
    #[index]
    pub booking: u64,
    pub customer: u32,
    pub flow: Flow,
    pub amount: Cents,
    pub at_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Data)]
pub struct Load {
    pub held: u32,
    pub sold: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Data)]
pub struct Till {
    pub deposits: Cents,
    pub charged: Cents,
    pub refunded: Cents,
}

#[derive(Clone, Debug, PartialEq, Eq, Data)]
pub enum Scenario {
    Reprice { venue: Option<u32>, percent: i32 },
    Close { venue: u32 },
}

impl fmt::Display for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scenario::Reprice {
                venue: Some(venue),
                percent,
            } => {
                write!(f, "reprice venue {venue} by {percent:+}%")
            }
            Scenario::Reprice {
                venue: None,
                percent,
            } => write!(f, "reprice every venue by {percent:+}%"),
            Scenario::Close { venue } => write!(f, "close venue {venue}"),
        }
    }
}

#[derive(Clone, Default, Data)]
pub struct Hotels {
    pub venues: Table<Venue>,
    pub rooms: Table<Room>,
    pub rates: Map<(u32, Night), Cents>,
    pub customers: Table<Customer>,
    pub bookings: Table<Booking>,
    pub stays: Table<Stay>,
    pub payments: Table<Payment>,
    pub loads: Map<(u32, Night), Load>,
    pub till: Till,
    pub scenario: Option<Scenario>,
    pub next_venue: u32,
    pub next_room: u32,
    pub next_customer: u32,
    pub next_booking: u64,
    pub next_payment: u64,
}

impl Hotels {
    pub fn price(&self, room: &Room, night: Night) -> Cents {
        self.rates
            .get(&(room.id, night))
            .map_or(room.rate, |rate| *rate)
    }

    pub fn capacity(&self, venue: u32, night: Night) -> u32 {
        match self.venues.get(&venue) {
            Some(row) if row.open_on(night) => self.rooms.by_venue(&venue).len() as u32,
            _ => 0,
        }
    }

    pub fn holder(&self, room: u32, night: Night) -> Option<u64> {
        self.stays
            .key_by_room_night(&(room, night))
            .map(|slot| slot.0)
    }

    pub fn lines_of(
        &self,
        customer: u32,
    ) -> impl DoubleEndedIterator<Item = Line> + ExactSizeIterator + use<> {
        self.bookings.by_customer(&customer).covered().map(
            |(id, (status, venue, room, first, nights, total))| Line {
                id,
                status,
                venue,
                room,
                first,
                nights,
                total,
            },
        )
    }
}
