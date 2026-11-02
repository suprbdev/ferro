use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use ledger::Kind;

#[derive(Parser)]
#[command(name = "ledger", about = "Double-entry ledger on ferro")]
pub struct Cli {
    #[arg(long, global = true, default_value = "ledger.ferro")]
    pub db: PathBuf,
    #[arg(long, global = true)]
    pub branch: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum AccountCmd {
    Add {
        name: String,
        #[arg(long, value_enum)]
        kind: Kind,
        #[arg(long, default_value = "USD")]
        currency: String,
    },
    Close {
        name: String,
    },
    List,
}

#[derive(Args)]
pub struct PostArgs {
    #[arg(long)]
    pub date: Option<String>,
    #[arg(long, default_value = "")]
    pub memo: String,
    #[arg(long = "dr", value_name = "ACCOUNT=AMOUNT[ CUR]")]
    pub debits: Vec<String>,
    #[arg(long = "cr", value_name = "ACCOUNT=AMOUNT[ CUR]")]
    pub credits: Vec<String>,
    #[arg(long, value_name = "DATE")]
    pub adjusts: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    Account {
        #[command(subcommand)]
        cmd: AccountCmd,
    },
    Post(PostArgs),
    Transfer {
        from: String,
        to: String,
        amount: String,
        #[arg(long)]
        currency: Option<String>,
        #[arg(long)]
        date: Option<String>,
        #[arg(long, default_value = "transfer")]
        memo: String,
    },
    Balance {
        account: String,
        #[arg(long)]
        as_of: Option<String>,
    },
    TrialBalance {
        #[arg(long)]
        as_of: Option<String>,
    },
    Statement {
        account: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    Reverse {
        entry: u64,
        #[arg(long)]
        date: Option<String>,
        #[arg(long)]
        memo: Option<String>,
    },
    Audit {
        #[arg(long, default_value_t = 16)]
        samples: usize,
    },
    AuditTrail {
        account: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Forecast {
        #[arg(long = "transfer", value_name = "FROM>TO=AMOUNT[@DATE]")]
        transfers: Vec<String>,
        #[arg(long)]
        name: Option<String>,
    },
    ClosePeriod {
        #[arg(long)]
        through: String,
    },
    Seed {
        n: u64,
        #[arg(long, default_value_t = 1000)]
        batch: u64,
    },
    Verify,
    Show {
        entry: u64,
    },
}

impl Command {
    pub fn is_read_only(&self) -> bool {
        match self {
            Command::Account { cmd } => matches!(cmd, AccountCmd::List),
            Command::Balance { .. }
            | Command::TrialBalance { .. }
            | Command::Statement { .. }
            | Command::Audit { .. }
            | Command::AuditTrail { .. }
            | Command::Verify
            | Command::Show { .. } => true,
            _ => false,
        }
    }
}
