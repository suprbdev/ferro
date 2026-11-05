//! The `booking` command line: hotel reservations with holds, refunds and what-ifs.
//!
//! Each run opens the database named by `--db`, read-only for commands that only read, and runs one
//! subcommand through [`booking::Reservations`]: adding venues, rooms and customers, holding rooms
//! for a stay, confirming, cancelling and sweeping lapsed holds, showing a booking `--at` a version
//! or `--as-of` a time, receipts and audit trails from history, reports, a full invariant `check`,
//! and what-if scenarios that run on a fork and are previewed, applied by a fallible merge or
//! dropped. `--coalesce` turns on ferro's commit coalescing.
//!
//! ```sh
//! make booking ARGS='seed'
//! cargo run --release -p booking --bin booking -- seed
//! cargo run --release -p booking --bin booking -- hold guest1@example.com "Harbour House/101" \
//!     --from 2030-01-10 --nights 2
//! cargo run --release -p booking --bin booking -- confirm 1
//! cargo run --release -p booking --bin booking -- receipt 1
//! cargo run --release -p booking --bin booking -- whatif close storm --venue "Harbour House"
//! cargo run --release -p booking --bin booking -- check
//! ```

mod cli;
mod commands;

use std::io::{self, ErrorKind, Write};
use std::process::ExitCode;

use booking::{BookingError, Config, Reservations};
use clap::Parser;

use crate::cli::Cli;

fn run(cli: Cli) -> Result<(), BookingError> {
    let read_only = cli.command.is_read_only();
    let svc = Reservations::open(
        &cli.db,
        Config {
            coalesce: cli.coalesce,
            read_only,
            ..Config::default()
        },
    )?;
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    if read_only {
        commands::run_read(&mut out, &svc, cli.command)?;
    } else {
        commands::run_write(&mut out, &svc, cli.command)?;
    }
    out.flush()
        .map_err(|error| BookingError::Db(ferro::Error::Io(error)))?;
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(BookingError::Db(ferro::Error::Io(error))) if error.kind() == ErrorKind::BrokenPipe => {
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
