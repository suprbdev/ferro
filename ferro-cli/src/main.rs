//! `ferro`, a command-line tool that inspects and maintains database files without their types.
//!
//! Each command opens a file through `ferro::admin`, so it works on any database, whatever root
//! type wrote it:
//!
//! - `info` prints the file's size and each branch with its version, commit count and last commit
//!   time.
//! - `log` lists a branch's commits newest first, `--branch` naming the branch and `--limit` the
//!   number shown.
//! - `verify` re-hashes every object reachable from the file's current head, checks the branch
//!   table's map, and checks that each branch's merge records name a branch the file holds.
//! - `space` prints the file's size against an estimate of its live bytes.
//! - `compact` copies what is reachable to a new file and swaps it in atomically, keeping every
//!   commit, the `--keep-last` newest commits per branch, or those at or after `--since-ms`. It
//!   takes the writer lock, so it fails while another process writes to the file.
//!
//! ```sh
//! cargo run --release -p ferro-cli -- info shop.ferro
//! cargo run --release -p ferro-cli -- log shop.ferro --branch main --limit 50
//! cargo run --release -p ferro-cli -- compact shop.ferro --keep-last 1000
//! make info DB=shop.ferro
//! ```
//!
//! `docs/guide/cli.md` describes every command.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ferro::admin::{self, Keep};

#[derive(Parser)]
#[command(name = "ferro", about = "Inspect and maintain ferro database files")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Show file size and branches")]
    Info { path: PathBuf },
    #[command(about = "List commits, newest first")]
    Log {
        path: PathBuf,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    #[command(about = "Re-hash every object reachable from the current head")]
    Verify { path: PathBuf },
    #[command(about = "Show file size and estimated live bytes")]
    Space { path: PathBuf },
    #[command(about = "Rewrite the file, optionally dropping old versions")]
    Compact {
        path: PathBuf,
        #[arg(
            long,
            conflicts_with = "since_ms",
            help = "Keep this many newest commits per branch"
        )]
        keep_last: Option<usize>,
        #[arg(long, help = "Keep commits at or after this unix time in milliseconds")]
        since_ms: Option<u64>,
    },
}

fn mib(bytes: u64) -> String {
    format!("{:.2} MiB", bytes as f64 / 1048576.0)
}

fn run(cli: Cli) -> ferro::Result<()> {
    match cli.command {
        Command::Info { path } => {
            let info = admin::info(&path)?;
            println!("file      {}", mib(info.file_bytes));
            for branch in info.branches {
                println!(
                    "branch    {}  version {}  commits {}  last commit {}",
                    branch.name, branch.version, branch.commits, branch.last_commit_ms
                );
            }
        }
        Command::Log {
            path,
            branch,
            limit,
        } => {
            for commit in admin::log(&path, &branch, limit)? {
                println!(
                    "v{}  {}  {}",
                    commit.version, commit.time_ms, commit.message
                );
            }
        }
        Command::Verify { path } => {
            let report = admin::verify(&path)?;
            println!(
                "ok: {} objects, {} live of {} on disk",
                report.objects,
                mib(report.bytes),
                mib(report.file_bytes)
            );
        }
        Command::Space { path } => {
            let space = admin::space(&path)?;
            println!("file  {}", mib(space.file_bytes));
            println!("live  {} (estimate)", mib(space.live_bytes));
        }
        Command::Compact {
            path,
            keep_last,
            since_ms,
        } => {
            let keep = match (keep_last, since_ms) {
                (Some(count), _) => Keep::Last(count),
                (None, Some(time)) => Keep::Since(time),
                (None, None) => Keep::All,
            };
            let report = admin::compact(&path, keep)?;
            println!(
                "{} to {}, {} objects kept, {} commits dropped",
                mib(report.before_bytes),
                mib(report.after_bytes),
                report.objects,
                report.commits_dropped
            );
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ferro: {error}");
            ExitCode::FAILURE
        }
    }
}
