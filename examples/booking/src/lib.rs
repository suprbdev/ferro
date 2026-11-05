//! Hotel reservations sold by the night, with holds, refunds and what-ifs, on ferro.
//!
//! [`Hotels`] is the database root: venues, rooms, customers, bookings, the [`Stay`] each booking
//! claims per night, payments, per-night [`Load`] counters and the [`Till`]. A hold claims its
//! nights through the `#[unique(room, night)]` index on [`Stay`], so a second claim fails inside
//! the commit and names the holder. A customer's bookings page, [`Hotels::lines_of`], reads the
//! covered fields of the customer index and touches no booking row. [`Reservations`] runs every
//! write as a `try_commit_stamped` closure that returns a [`BookingError`], so a confirm can record
//! its own version for the receipt. The `history` module builds audit trails and price history with
//! `history_of`, `whatif` previews scenarios on forked branches and applies them with
//! `try_merge_with`, `check` verifies the invariants at every version, `seed` adds demo data and
//! `date` parses and formats nights, times and money. The `booking` and `booking-stress` binaries
//! and the tests use this library.
//!
//! ```sh
//! make booking ARGS='seed'
//! make booking-stress ARGS='--threads 64'
//! ```

pub mod check;
pub mod date;
pub mod error;
pub mod history;
pub mod model;
pub mod ops;
pub mod seed;
pub mod service;
pub mod whatif;

pub use check::{Violation, Walked, check_pair, check_state, walk};
pub use error::{BookingError, Result};
pub use model::{
    Booking, Cents, Customer, Flow, Hotels, Line, Load, Night, Payment, Room, Scenario, Status,
    Stay, Till, Venue,
};
pub use ops::{Held, HoldRequest, Why};
pub use service::{Clock, Config, Reservations};
