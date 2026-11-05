//! The `tracker` command line: an issue tracker on ferro.
//!
//! Issues live in a table indexed by status, assignee, priority and each label, and every query is
//! plain iterator code over those indexes; `list --explain` prints which index a listing started
//! from. Each run opens the database at `--db`, read-only for commands that only read, on `main` or
//! on the branch named by `--branch`. Issues can be added, edited, assigned, labelled, commented
//! on, started, closed and reopened. Every write is a commit, so `history` shows an issue's changes
//! from `history_of`, `log` and `diff` compare versions, `undo` and `restore` bring back an earlier
//! state as a new commit, and `at` and `as-of` run a read at a past version or time. `branch` forks
//! a new branch from the current one, `compare` diffs the current branch against another, and
//! `merge` merges another branch into the current one, keeping the current side of an issue both
//! changed unless `--theirs` is given and renumbering issues both sides added under the same id.
//!
//! ```sh
//! make tracker ARGS='list'
//! cargo run --release -p tracker -- add "Fix login" --label bug --priority high
//! cargo run --release -p tracker -- list --label bug
//! cargo run --release -p tracker -- close 1
//! cargo run --release -p tracker -- history 1
//! cargo run --release -p tracker -- undo
//! cargo run --release -p tracker -- branch triage
//! cargo run --release -p tracker -- --branch triage assign 1 alice
//! cargo run --release -p tracker -- merge triage
//! ```

mod cli;
mod commands;
mod model;
mod render;
mod seed;

use std::error::Error;
use std::io::{self, ErrorKind};
use std::process::ExitCode;

use clap::Parser;
use ferro::{Db, Options};

use crate::cli::Cli;
use crate::model::Tracker;

pub type Res<T> = Result<T, Box<dyn Error>>;

fn run(cli: Cli) -> Res<()> {
    let read_only = cli.command.is_read_only();
    let main: Db<Tracker> = Options::new().read_only(read_only).open(&cli.db)?;
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
