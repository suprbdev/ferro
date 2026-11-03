use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::query::Dir;

#[derive(Parser)]
#[command(name = "graph", about = "A social graph service on ferro")]
pub struct Cli {
    #[arg(long, global = true, default_value = "graph.ferro")]
    pub db: PathBuf,
    #[arg(long, global = true)]
    pub cache_mib: Option<u64>,
    #[arg(long, global = true)]
    pub buffered: bool,
    #[arg(long, global = true)]
    pub timing: bool,
    #[arg(long, global = true)]
    pub at: Option<u64>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Algo {
    Wcc,
    Lpa,
}

#[derive(Subcommand, Clone)]
pub enum ReadCmd {
    Followers {
        user: String,
        #[arg(long)]
        after: Option<u32>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Following {
        user: String,
        #[arg(long)]
        after: Option<u32>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Count {
        user: String,
        #[arg(long)]
        scan: bool,
    },
    Mutuals {
        a: String,
        b: String,
        #[arg(long)]
        followers: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Path {
        a: String,
        b: String,
        #[arg(long, default_value_t = 6)]
        max_depth: usize,
    },
    Recommend {
        user: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, default_value_t = 10_000)]
        max_fanout: usize,
    },
    Timeline {
        user: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        inbox: bool,
    },
    Reach {
        user: String,
        #[arg(long, default_value_t = 2)]
        hops: usize,
        #[arg(long, value_enum, default_value_t = Dir::Out)]
        dir: Dir,
    },
    Pagerank {
        #[arg(long, default_value_t = 20)]
        iterations: usize,
        #[arg(long, default_value_t = 8)]
        threads: usize,
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
    Communities {
        #[arg(long, value_enum, default_value_t = Algo::Wcc)]
        algo: Algo,
        #[arg(long, default_value_t = 10)]
        iterations: usize,
        #[arg(long, default_value_t = 8)]
        threads: usize,
        #[arg(long, default_value_t = 5)]
        top: usize,
    },
    Stats,
}

#[derive(Subcommand, Clone)]
pub enum WriteCmd {
    User {
        name: String,
    },
    Follow {
        a: String,
        b: String,
    },
    Unfollow {
        a: String,
        b: String,
    },
    Block {
        a: String,
        b: String,
    },
    Post {
        author: String,
        text: String,
        #[arg(long)]
        fanout: bool,
    },
    Like {
        user: String,
        post: u64,
    },
}

#[derive(Subcommand)]
pub enum Command {
    #[command(flatten)]
    Write(WriteCmd),
    #[command(flatten)]
    Read(ReadCmd),
    GraphAt {
        version: u64,
        #[command(subcommand)]
        command: ReadCmd,
    },
    Diff {
        v1: u64,
        v2: u64,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Seed {
        #[arg(long, default_value_t = 10_000)]
        users: u32,
        #[arg(long, default_value_t = 100_000)]
        edges: u64,
        #[arg(long, default_value_t = 0)]
        posts: u64,
        #[arg(long, default_value_t = 0)]
        likes: u64,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    Verify {
        #[arg(long, default_value_t = 20_000)]
        sample: usize,
    },
    Bench {
        #[command(subcommand)]
        which: BenchCmd,
    },
    Experiment {
        rep: String,
        #[arg(long, default_value_t = 1_000_000)]
        users: u32,
        #[arg(long, default_value_t = 5_000_000)]
        edges: u64,
        #[arg(long, default_value_t = 1_000_000)]
        celebrity_followers: u32,
    },
}

#[derive(Subcommand, Clone)]
pub enum BenchCmd {
    Overhead {
        #[arg(long, default_value_t = 200_000)]
        lookups: usize,
    },
    Pagerank {
        #[arg(long, default_value_t = 5)]
        iterations: usize,
        #[arg(long, default_value_t = 8)]
        threads: usize,
    },
    Writers {
        #[arg(long, default_value_t = 4)]
        writers: usize,
        #[arg(long, default_value_t = 3)]
        iterations: usize,
        #[arg(long, default_value_t = 3)]
        seconds: usize,
    },
    Counts {
        #[arg(long, default_value_t = 300)]
        commits: usize,
    },
    Fanout {
        #[arg(long, default_value_t = 20)]
        commits: usize,
    },
}

impl Command {
    pub fn is_read_only(&self) -> bool {
        !matches!(
            self,
            Command::Write(_)
                | Command::Seed { .. }
                | Command::Bench { .. }
                | Command::Experiment { .. }
        )
    }
}
