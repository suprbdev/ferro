use std::error::Error;
use std::io::{self, ErrorKind, Write};
use std::path::Path;
use std::process::ExitCode;

use clap::Parser;
use ferro::{Db, Durability, Options};

use graph::bench;
use graph::cli::{Cli, Command};
use graph::commands;
use graph::experiment;
use graph::model::Graph;
use graph::seed::{self, SeedOpts};

type Res<T> = Result<T, Box<dyn Error>>;

fn options(cli: &Cli, read_only: bool) -> Options {
    let mut options = Options::new();
    if read_only {
        options = options.read_only();
    }
    if cli.buffered {
        options = options.durability(Durability::Buffered);
    }
    if let Some(mib) = cli.cache_mib {
        options = options.cache_bytes(mib * 1024 * 1024);
    }
    options
}

fn run(cli: Cli) -> Res<()> {
    let started = std::time::Instant::now();
    let timing = cli.timing;
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let read_only = cli.command.is_read_only();
    if let Command::Experiment {
        rep,
        users,
        edges,
        celebrity_followers,
    } = &cli.command
    {
        let dir = cli.db.parent().unwrap_or(Path::new(".")).to_path_buf();
        experiment::experiment(&dir, rep, *users, *edges, *celebrity_followers);
        return Ok(());
    }
    let db: Db<Graph> = options(&cli, read_only).open(&cli.db)?;
    match cli.command {
        Command::Write(cmd) => commands::run_write(&mut out, &db, cmd)?,
        Command::Read(cmd) => {
            let snap = match cli.at {
                Some(version) => db.at(version)?,
                None => db.read(),
            };
            commands::run_read(&mut out, &snap, cmd)?;
        }
        Command::GraphAt { version, command } => {
            let snap = db.at(version)?;
            commands::run_read(&mut out, &snap, command)?;
        }
        Command::Diff { v1, v2, limit } => commands::diff(&mut out, &db, v1, v2, limit)?,
        Command::Log { limit } => commands::log(&mut out, &db, limit)?,
        Command::Verify { sample } => commands::verify(&mut out, &db, sample)?,
        Command::Seed {
            users,
            edges,
            posts,
            likes,
            seed,
        } => {
            let opts = SeedOpts {
                users,
                edges,
                posts,
                likes,
                seed,
            };
            seed::seed(&db, &opts, &mut |line| {
                let _ = writeln!(io::stdout(), "{line}");
            })?;
        }
        Command::Bench { which } => bench::run(&mut out, &db, which)?,
        Command::Experiment { .. } => {}
    }
    out.flush()?;
    if timing {
        eprintln!(
            "elapsed {:.3}s peak_rss {} MiB",
            started.elapsed().as_secs_f64(),
            bench::rss_mib()
        );
    }
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
