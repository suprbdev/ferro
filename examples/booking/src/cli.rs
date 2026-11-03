use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "booking", about = "Hotel reservations with holds, refunds and what-ifs, on ferro")]
pub struct Cli {
    #[arg(long, global = true, default_value = "booking.ferro")]
    pub db: PathBuf,
    #[arg(long, global = true)]
    pub coalesce: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum VenueCmd {
    Add {
        name: String,
        #[arg(long, default_value = "")]
        city: String,
    },
    List,
}

#[derive(Subcommand)]
pub enum RoomCmd {
    Add {
        venue: String,
        number: u16,
        #[arg(long)]
        rate: String,
    },
    Rate {
        room: String,
        rate: Option<String>,
        #[arg(long)]
        night: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    Prices {
        room: String,
    },
    List {
        venue: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum CustomerCmd {
    Add {
        email: String,
        name: String,
        #[arg(long)]
        deposit: Option<String>,
    },
    Deposit {
        customer: String,
        amount: String,
    },
    Show {
        customer: String,
    },
    List,
}

#[derive(Subcommand)]
pub enum WhatIfCmd {
    Reprice {
        name: String,
        #[arg(long)]
        venue: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        percent: i32,
    },
    Close {
        name: String,
        #[arg(long)]
        venue: String,
    },
    Show {
        name: String,
    },
    Apply {
        name: String,
    },
    Drop {
        name: String,
    },
    List,
}

#[derive(Subcommand)]
pub enum Command {
    Seed {
        #[arg(long, default_value_t = 3)]
        venues: u32,
        #[arg(long, default_value_t = 4)]
        rooms: u32,
        #[arg(long, default_value_t = 5)]
        customers: u32,
        #[arg(long, default_value = "2000")]
        credit: String,
    },
    Venue {
        #[command(subcommand)]
        cmd: VenueCmd,
    },
    Room {
        #[command(subcommand)]
        cmd: RoomCmd,
    },
    Customer {
        #[command(subcommand)]
        cmd: CustomerCmd,
    },
    Avail {
        venue: String,
        #[arg(long)]
        from: String,
        #[arg(long, default_value_t = 1)]
        nights: u32,
    },
    Hold {
        customer: String,
        room: String,
        #[arg(long)]
        from: String,
        #[arg(long, default_value_t = 1)]
        nights: u32,
        #[arg(long, default_value_t = 600)]
        ttl: u64,
    },
    Confirm {
        id: u64,
        #[arg(long)]
        expect: Option<String>,
    },
    Cancel {
        id: u64,
    },
    Sweep,
    Show {
        id: u64,
        #[arg(long)]
        at: Option<u64>,
        #[arg(long)]
        as_of: Option<String>,
    },
    Receipt {
        id: u64,
    },
    Audit {
        id: u64,
    },
    Report {
        #[arg(long)]
        from: Option<String>,
        #[arg(long, default_value_t = 7)]
        nights: u32,
        #[arg(long)]
        as_of: Option<String>,
    },
    Check {
        #[arg(long, default_value_t = 1)]
        every: u64,
    },
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Whatif {
        #[command(subcommand)]
        cmd: WhatIfCmd,
    },
}

impl Command {
    pub fn is_read_only(&self) -> bool {
        match self {
            Command::Venue { cmd } => matches!(cmd, VenueCmd::List),
            Command::Room { cmd } => matches!(cmd, RoomCmd::Prices { .. } | RoomCmd::List { .. }),
            Command::Customer { cmd } => matches!(cmd, CustomerCmd::Show { .. } | CustomerCmd::List),
            Command::Whatif { cmd } => matches!(cmd, WhatIfCmd::Show { .. } | WhatIfCmd::List),
            Command::Avail { .. }
            | Command::Show { .. }
            | Command::Receipt { .. }
            | Command::Audit { .. }
            | Command::Report { .. }
            | Command::Check { .. }
            | Command::Log { .. } => true,
            _ => false,
        }
    }
}
