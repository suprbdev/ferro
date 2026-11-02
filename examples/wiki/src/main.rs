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
    let mut options = Options::new();
    if read_only {
        options = options.read_only();
    }
    let main: Db<Wiki> = options.open(&cli.db)?;
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
