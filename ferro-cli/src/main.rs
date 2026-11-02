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
    #[command(about = "Show log, allocated and estimated live bytes")]
    Space { path: PathBuf },
    #[command(about = "Free dead space in place, optionally dropping old versions")]
    Reclaim {
        path: PathBuf,
        #[arg(long, conflicts_with = "since_ms", help = "Keep this many newest commits per branch")]
        keep_last: Option<usize>,
        #[arg(long, help = "Keep commits at or after this unix time in milliseconds")]
        since_ms: Option<u64>,
    },
    #[command(about = "Rewrite the file densely, optionally dropping old versions")]
    Compact {
        path: PathBuf,
        #[arg(long, conflicts_with = "since_ms", help = "Keep this many newest commits per branch")]
        keep_last: Option<usize>,
        #[arg(long, help = "Keep commits at or after this unix time in milliseconds")]
        since_ms: Option<u64>,
    },
}

fn retention(keep_last: Option<usize>, since_ms: Option<u64>) -> Keep {
    match (keep_last, since_ms) {
        (Some(count), _) => Keep::Last(count),
        (None, Some(time)) => Keep::Since(time),
        (None, None) => Keep::All,
    }
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
            println!("log        {}", mib(space.log_bytes));
            println!("allocated  {}", mib(space.allocated_bytes));
            println!("live       {} (estimate)", mib(space.live_bytes));
        }
        Command::Reclaim {
            path,
            keep_last,
            since_ms,
        } => {
            let report = admin::reclaim(&path, retention(keep_last, since_ms))?;
            println!(
                "{} to {}, {} objects live, {} moved, {} commits dropped",
                mib(report.before_bytes),
                mib(report.after_bytes),
                report.objects,
                mib(report.moved_bytes),
                report.commits_dropped
            );
        }
        Command::Compact {
            path,
            keep_last,
            since_ms,
        } => {
            let report = admin::compact(&path, retention(keep_last, since_ms))?;
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
