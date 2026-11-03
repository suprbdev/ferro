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
        },
    )?;
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    if read_only {
        commands::run_read(&mut out, &svc, cli.command)?;
    } else {
        commands::run_write(&mut out, &svc, cli.command)?;
    }
    out.flush().map_err(|error| BookingError::Db(ferro::Error::Io(error)))?;
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
