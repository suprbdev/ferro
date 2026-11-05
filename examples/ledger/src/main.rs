//! The `ledger` command line: a double-entry ledger on ferro.
//!
//! Each run opens the database at `--db`, read-only for commands that only read, on `main` or on
//! the branch named by `--branch`, and runs one subcommand: adding, closing and listing accounts,
//! posting balanced entries with `--dr` and `--cr` lines, transfers, reversals, and closing a
//! period so that only adjusting entries can reach back into it. `balance` and `trial-balance` take
//! an `--as-of` date and `statement` a `--from` and `--to`, all answered from the summarized
//! postings. `forecast` posts hypothetical transfers on a forked branch, prints how each balance
//! would move, and drops the branch unless it was given a `--name`. `audit-trail` lists the commits
//! that changed an account from its history, `audit` checks the books, and `verify` re-hashes the
//! file.
//!
//! ```sh
//! cargo run --release -p ledger -- account add assets:cash --kind asset
//! cargo run --release -p ledger -- account add expense:rent --kind expense
//! cargo run --release -p ledger -- post --date 2024-02-01 --memo rent \
//!     --dr expense:rent=300.00 --cr assets:cash=300.00
//! cargo run --release -p ledger -- balance assets:cash --as-of 2024-02-01
//! cargo run --release -p ledger -- trial-balance
//! cargo run --release -p ledger -- forecast \
//!     --transfer "assets:cash>expense:rent=300.00@2024-06-01"
//! ```

mod cli;
mod commands;

use std::error::Error;
use std::io::{self, ErrorKind};
use std::process::ExitCode;

use clap::Parser;
use ferro::{Db, Options};
use ledger::Ledger;

use crate::cli::Cli;

pub type Res<T> = Result<T, Box<dyn Error>>;

fn run(cli: Cli) -> Res<()> {
    let read_only = cli.command.is_read_only();
    let main: Db<Ledger> = Options::new().read_only(read_only).open(&cli.db)?;
    let db = match &cli.branch {
        Some(name) if name != "main" => main.branch(name)?,
        _ => main,
    };
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    if read_only {
        commands::run_read(&mut out, &db, cli.command)?;
    } else {
        commands::run_write(&mut out, &db, cli.command)?;
    }
    io::Write::flush(&mut out)?;
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let broken_pipe = error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == ErrorKind::BrokenPipe);
            if !broken_pipe {
                eprintln!("error: {error}");
            }
            ExitCode::FAILURE
        }
    }
}
