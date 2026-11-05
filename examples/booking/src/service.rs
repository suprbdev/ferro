use std::path::Path;
use std::sync::Arc;

use ferro::{Db, Durability, Keep, Options, Reclaim, Snapshot, Stamp};

use crate::date::{format_night, now_ms};
use crate::error::Result;
use crate::model::{Cents, Hotels, Night};
use crate::ops::{Held, HoldRequest, Why};

/// The source of the current time in Unix milliseconds, replaceable so tests can move time.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// How [`Reservations::open`] opens the database: commit coalescing, read-only, buffered
/// durability, and how many versions of history to keep.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub coalesce: bool,
    pub read_only: bool,
    pub buffered: bool,
    pub keep_last: Option<usize>,
}

/// The reservations service: a [`Db<Hotels>`](Db) and the [`Clock`] its writes read.
///
/// Every write runs as one `try_commit_stamped` closure that returns a commit message or a
/// [`BookingError`](crate::BookingError), so it either commits whole or writes nothing.
#[derive(Clone)]
pub struct Reservations {
    db: Db<Hotels>,
    clock: Clock,
}

impl Reservations {
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        let mut options = Options::new()
            .coalesce(config.coalesce)
            .read_only(config.read_only);
        if config.buffered {
            options = options.durability(Durability::Buffered);
        }
        if let Some(versions) = config.keep_last {
            options = options.history(Keep::Last(versions)).reclaim(Reclaim {
                every_bytes: 32 << 20,
            });
        }
        Ok(Reservations::new(options.open(path)?))
    }

    pub fn new(db: Db<Hotels>) -> Self {
        Reservations {
            db,
            clock: Arc::new(now_ms),
        }
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn on(&self, db: Db<Hotels>) -> Self {
        Reservations {
            db,
            clock: self.clock.clone(),
        }
    }

    pub fn db(&self) -> &Db<Hotels> {
        &self.db
    }

    pub fn clock(&self) -> Clock {
        self.clock.clone()
    }

    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    pub fn read(&self) -> Snapshot<Hotels> {
        self.db.read()
    }

    pub fn write<R>(&self, f: impl FnOnce(&mut Hotels, u64) -> Result<(String, R)>) -> Result<R> {
        self.write_stamped(|hotels, now, _| f(hotels, now))
    }

    pub fn write_stamped<R>(
        &self,
        f: impl FnOnce(&mut Hotels, u64, &Stamp<'_>) -> Result<(String, R)>,
    ) -> Result<R> {
        let clock = &self.clock;
        let done = self
            .db
            .try_commit_stamped(|hotels, stamp| f(hotels, clock(), stamp))?;
        Ok(done.value)
    }

    pub fn add_venue(&self, name: &str, city: &str) -> Result<u32> {
        self.write(|h, _| {
            let id = h.add_venue(name, city)?;
            Ok((format!("add venue {id} {name}"), id))
        })
    }

    pub fn add_room(&self, venue: u32, number: u16, rate: Cents) -> Result<u32> {
        self.write(|h, _| {
            let id = h.add_room(venue, number, rate)?;
            Ok((format!("add room {id} ({number}) to venue {venue}"), id))
        })
    }

    pub fn add_customer(&self, email: &str, name: &str) -> Result<u32> {
        self.write(|h, _| {
            let id = h.add_customer(email, name)?;
            Ok((format!("add customer {id} {email}"), id))
        })
    }

    pub fn deposit(&self, customer: u32, amount: Cents) -> Result<Cents> {
        self.write(|h, _| {
            let credit = h.deposit(customer, amount)?;
            Ok((format!("deposit {amount} for customer {customer}"), credit))
        })
    }

    pub fn set_rate(&self, room: u32, rate: Cents) -> Result<Cents> {
        self.write(|h, _| {
            let old = h.set_rate(room, rate)?;
            Ok((format!("rate room {room} {old} -> {rate}"), old))
        })
    }

    pub fn set_night_rate(&self, room: u32, night: Night, rate: Option<Cents>) -> Result<()> {
        self.write(|h, _| {
            h.set_night_rate(room, night, rate)?;
            let what = rate.map_or("base".to_string(), |rate| rate.to_string());
            Ok((
                format!("rate room {room} on {} -> {what}", format_night(night)),
                (),
            ))
        })
    }

    pub fn hold(&self, request: &HoldRequest) -> Result<Held> {
        self.write(|h, now| {
            let held = h.hold(now, request)?;
            let mut message = format!(
                "hold #{} room {} {} x{} for customer {}",
                held.id,
                request.room,
                format_night(request.first),
                request.nights,
                request.customer
            );
            if !held.released.is_empty() {
                let released: Vec<String> =
                    held.released.iter().map(|id| format!("#{id}")).collect();
                message.push_str(&format!(", releasing lapsed {}", released.join(" ")));
            }
            Ok((message, held))
        })
    }

    pub fn confirm(&self, id: u64, expected: Option<Cents>) -> Result<Cents> {
        self.write_stamped(|h, now, stamp| {
            let total = h.confirm(now, stamp.version(), id, expected)?;
            Ok((format!("confirm #{id} paid {total}"), total))
        })
    }

    pub fn cancel(&self, id: u64) -> Result<Cents> {
        self.write(|h, now| {
            let refund = h.cancel(now, id, Why::Customer)?;
            Ok((format!("cancel #{id} refund {refund}"), refund))
        })
    }

    pub fn sweep(&self) -> Result<Vec<u64>> {
        if self.db.read().due(self.now()).is_empty() {
            return Ok(Vec::new());
        }
        self.write(|h, now| {
            let expired = h.expire_due(now);
            Ok((format!("expire {} holds", expired.len()), expired))
        })
    }
}
