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
