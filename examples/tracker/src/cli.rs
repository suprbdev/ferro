use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::model::{Priority, Status};

#[derive(Parser)]
#[command(name = "tracker", about = "A command-line issue tracker on ferro")]
pub struct Cli {
    #[arg(long, global = true, default_value = "tracker.ferro")]
    pub db: PathBuf,
    #[arg(long, global = true)]
    pub branch: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Clone)]
pub struct ListArgs {
    #[arg(long, value_enum)]
    pub status: Option<Status>,
    #[arg(long, conflicts_with = "unassigned")]
    pub assignee: Option<String>,
    #[arg(long)]
    pub unassigned: bool,
    #[arg(long)]
    pub label: Option<String>,
    #[arg(long, value_enum)]
    pub priority: Option<Priority>,
    #[arg(long)]
    pub text: Option<String>,
    #[arg(long)]
    pub limit: Option<usize>,
    #[arg(long)]
    pub explain: bool,
}

#[derive(Subcommand, Clone)]
pub enum ReadCmd {
    List(ListArgs),
    Show { id: u64 },
    Stats,
    Users,
}

#[derive(Subcommand)]
pub enum Command {
    Add {
        title: String,
        #[arg(long, default_value = "")]
        body: String,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long = "label")]
        labels: Vec<String>,
        #[arg(long, value_enum, default_value_t = Priority::Medium)]
        priority: Priority,
    },
    #[command(flatten)]
    Read(ReadCmd),
    Edit {
        id: u64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long, value_enum)]
        priority: Option<Priority>,
    },
    Assign {
        id: u64,
        user: String,
    },
    Unassign {
        id: u64,
    },
    Label {
        id: u64,
        label: String,
    },
    Unlabel {
        id: u64,
        label: String,
    },
    Comment {
        id: u64,
        text: String,
        #[arg(long, default_value = "anonymous")]
        author: String,
    },
    Start {
        id: u64,
    },
    Close {
        id: u64,
    },
    Reopen {
        id: u64,
    },
    History {
        id: u64,
    },
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Diff {
        from: u64,
        to: u64,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Undo,
    Restore {
        version: u64,
    },
    At {
        version: u64,
        #[command(subcommand)]
        read: ReadCmd,
    },
    AsOf {
        unix_ms: u64,
        #[command(subcommand)]
        read: ReadCmd,
    },
    Verify,
    Seed {
        n: u64,
        #[arg(long, default_value_t = 1000)]
        batch: u64,
    },
    Branch {
        name: String,
    },
    Branches,
    Merge {
        from: String,
        #[arg(long)]
        theirs: bool,
    },
    Compare {
        other: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}

impl Command {
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Command::Read(_)
                | Command::History { .. }
                | Command::Log { .. }
                | Command::Diff { .. }
                | Command::At { .. }
                | Command::AsOf { .. }
                | Command::Verify
                | Command::Branches
                | Command::Compare { .. }
        )
    }
}
