use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "wiki", about = "A wiki and document store on ferro")]
pub struct Cli {
    #[arg(long, global = true, env = "WIKI_DB", default_value = "wiki.ferro")]
    pub db: PathBuf,
    #[arg(long, global = true)]
    pub draft: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    Put {
        slug: String,
        #[arg(long, conflicts_with = "stdin")]
        file: Option<PathBuf>,
        #[arg(long)]
        stdin: bool,
        #[arg(long)]
        title: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long)]
        author: Option<String>,
    },
    Get {
        slug: String,
        #[arg(long)]
        at: Option<u64>,
        #[arg(long)]
        meta: bool,
    },
    Delete {
        slug: String,
    },
    Rename {
        old: String,
        new: String,
    },
    List {
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long, default_value_t = usize::MAX)]
        limit: usize,
    },
    Search {
        #[arg(required = true)]
        terms: Vec<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    Backlinks {
        slug: String,
    },
    Orphans,
    History {
        slug: String,
    },
    Diff {
        slug: String,
        v1: u64,
        v2: u64,
    },
    Revert {
        slug: String,
        version: u64,
    },
    Import {
        dir: PathBuf,
        #[arg(long)]
        author: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long, default_value_t = 10000)]
        batch_pages: usize,
        #[arg(long, default_value_t = 128)]
        batch_mib: usize,
    },
    Export {
        dir: PathBuf,
        #[arg(long)]
        at: Option<u64>,
    },
    #[command(subcommand)]
    Draft(DraftCommand),
    Stats {
        #[arg(long)]
        deep: bool,
    },
    Verify,
}

#[derive(Subcommand)]
pub enum DraftCommand {
    Start { name: String },
    Publish { name: String },
    List,
}

impl Command {
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Command::Get { .. }
                | Command::List { .. }
                | Command::Search { .. }
                | Command::Backlinks { .. }
                | Command::Orphans
                | Command::History { .. }
                | Command::Diff { .. }
                | Command::Export { .. }
                | Command::Stats { .. }
                | Command::Verify
                | Command::Draft(DraftCommand::List)
        )
    }
}
