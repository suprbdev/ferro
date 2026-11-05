//! The `wiki` command line: a wiki and document store on ferro.
//!
//! Each run opens the database at `--db`, or `WIKI_DB`, read-only for commands that only read, on
//! `main` or on the draft named by `--draft`. Pages can be put from a file or stdin, read, deleted,
//! renamed with the links to them rewritten, listed by tag or slug prefix, searched, and imported
//! from or exported to a directory of Markdown files. `backlinks` and `orphans` read the link
//! index; `history`, `diff` and `revert` work from past versions, and `get --at` and `export --at`
//! read one. `draft start` forks a draft, writes with `--draft` go to it, and `draft publish`
//! merges it into the current branch page by page and drops it if nothing conflicted. `stats` and
//! `verify` describe and check the database.
//!
//! ```sh
//! cargo run --release -p wiki --bin wiki -- put home --file home.md --title Home --tag start
//! cargo run --release -p wiki --bin wiki -- draft start redesign
//! cargo run --release -p wiki --bin wiki -- --draft redesign put home --file new-home.md
//! cargo run --release -p wiki --bin wiki -- draft publish redesign
//! cargo run --release -p wiki --bin wiki -- history home
//! cargo run --release -p wiki --bin wiki -- search getting started
//! ```

use std::io::{self, ErrorKind};
use std::process::ExitCode;

use clap::Parser;
use ferro::{Db, Options};

use wiki::Res;
use wiki::cli::Cli;
use wiki::commands;
use wiki::model::Wiki;

fn run(cli: Cli) -> Res<()> {
    let read_only = cli.command.is_read_only();
    let main: Db<Wiki> = Options::new().read_only(read_only).open(&cli.db)?;
    let db = match &cli.draft {
        Some(name) if name != "main" => main.branch(name)?,
        _ => main,
    };
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    commands::run(&mut out, &db, cli.command)?;
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
