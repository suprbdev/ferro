use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "metrics", about = "A time-series metrics store on ferro")]
pub struct Cli {
    #[arg(long, global = true, default_value = "metrics.ferro")]
    pub db: PathBuf,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    Ingest {
        file: Option<PathBuf>,
        #[arg(long, default_value_t = 50_000)]
        batch_points: usize,
        #[arg(long, default_value_t = 1000)]
        batch_ms: u64,
        #[arg(long)]
        no_sync: bool,
        #[arg(long)]
        strict: bool,
    },
    Query {
        selector: String,
        #[arg(long, default_value = "0")]
        from: String,
        #[arg(long, default_value = "max")]
        to: String,
        #[arg(long)]
        limit: Option<usize>,
    },
    Aggregate {
        selector: String,
        #[arg(long = "fn", default_value = "avg")]
        func: String,
        #[arg(long, default_value = "0")]
        from: String,
        #[arg(long, default_value = "max")]
        to: String,
        #[arg(long)]
        step: Option<String>,
        #[arg(long)]
        by: Option<String>,
        #[arg(long)]
        rollup: Option<String>,
    },
    Series {
        selector: Option<String>,
        #[arg(long)]
        count: bool,
    },
    Labels {
        name: Option<String>,
    },
    Downsample {
        #[arg(long)]
        step: String,
        #[arg(long)]
        selector: Option<String>,
        #[arg(long, default_value = "0")]
        from: String,
        #[arg(long, default_value = "max")]
        to: String,
    },
    Retain {
        #[arg(long)]
        older_than: String,
        #[arg(long)]
        now: Option<u64>,
        #[arg(long, default_value_t = 1)]
        keep_versions: usize,
        #[arg(long, conflicts_with = "keep_versions")]
        keep_since: Option<String>,
        #[arg(long)]
        no_compact: bool,
    },
    Stats,
    Verify,
    Load {
        rate: u64,
        seconds: u64,
        #[arg(long, default_value_t = 1000)]
        series: usize,
        #[arg(long, default_value_t = 4)]
        threads: usize,
        #[arg(long, default_value_t = 100)]
        batch_ms: u64,
        #[arg(long, default_value_t = 10_000)]
        interval_ms: u64,
        #[arg(long)]
        start_ts: Option<u64>,
        #[arg(long, default_value_t = 64)]
        reclaim_mb: u64,
        #[arg(long, default_value_t = 1)]
        keep_versions: usize,
        #[arg(long)]
        no_sync: bool,
    },
}
