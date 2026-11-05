use crate::model::{Cents, Night, Status};

/// Every way a reservations command can fail, from a missing row or a taken night to a database
/// error.
///
/// Write closures return it to abort their commit, so a failed hold or confirm writes nothing.
#[derive(Debug, thiserror::Error)]
pub enum BookingError {
    #[error("no such venue: {0}")]
    NoVenue(String),
    #[error("no such room: {0}")]
    NoRoom(u32),
    #[error("no such customer: {0}")]
    NoCustomer(String),
    #[error("no such booking: #{0}")]
    NoBooking(u64),
    #[error("venue {0} is closed")]
    VenueClosed(u32),
    #[error("venue name already taken: {0}")]
    VenueExists(String),
    #[error("venue {venue} already has a room {number}")]
    RoomExists { venue: u32, number: u16 },
    #[error("email already registered to customer {holder}: {email}")]
    EmailTaken { email: String, holder: u32 },
    #[error("room {room} on {night} is taken by booking #{holder}")]
    Unavailable {
        room: u32,
        night: String,
        holder: u64,
    },
    #[error("a stay must be between 1 and {max} nights, not {given}")]
    BadStay { given: u32, max: u32 },
    #[error("night {night} is in the past")]
    PastNight { night: String },
    #[error("the hold on booking #{id} expired at {expired_ms}")]
    HoldExpired { id: u64, expired_ms: u64 },
    #[error("booking #{id} is {status}, not {wanted}")]
    WrongStatus {
        id: u64,
        status: Status,
        wanted: &'static str,
    },
    #[error("booking #{id} costs {total} cents, not the {expected} the customer agreed to")]
    PriceChanged {
        id: u64,
        total: Cents,
        expected: Cents,
    },
    #[error("customer {customer} has {available} cents of credit and needs {needed}")]
    InsufficientFunds {
        customer: u32,
        needed: Cents,
        available: Cents,
    },
    #[error("amount must be positive, not {0}")]
    BadAmount(Cents),
    #[error("amount overflow")]
    Overflow,
    #[error("invalid date `{0}`, expected YYYY-MM-DD")]
    BadDate(String),
    #[error("invalid amount `{0}`, expected a number like 129.50")]
    BadMoney(String),
    #[error("no such what-if branch: {0}")]
    NoScenario(String),
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Db(#[from] ferro::Error),
}

/// A result whose error is a [`BookingError`].
pub type Result<T> = std::result::Result<T, BookingError>;

impl BookingError {
    pub fn unavailable(room: u32, night: Night, holder: u64) -> Self {
        BookingError::Unavailable {
            room,
            night: crate::date::format_night(night),
            holder,
        }
    }
}
