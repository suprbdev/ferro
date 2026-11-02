use std::fmt;

use clap::ValueEnum;
use ferro::{Data, Map, Record, Table};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Data, ValueEnum)]
pub enum Kind {
    Asset,
    Liability,
    Equity,
    Income,
    Expense,
}

impl Kind {
    pub fn debit_normal(self) -> bool {
        matches!(self, Kind::Asset | Kind::Expense)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Asset => "asset",
            Kind::Liability => "liability",
            Kind::Equity => "equity",
            Kind::Income => "income",
            Kind::Expense => "expense",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct Account {
    #[key]
    pub id: u32,
    #[unique]
    pub name: String,
    pub kind: Kind,
    pub currency: String,
    pub closed: bool,
    pub balance: i64,
}

#[derive(Clone, Debug, PartialEq, Data)]
pub struct Line {
    pub account: u32,
    pub amount: i64,
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct Entry {
    #[key]
    pub id: u64,
    pub date: i32,
    pub memo: String,
    pub lines: Vec<Line>,
    pub posted_ms: u64,
    pub reverses: Option<u64>,
    pub adjusts: Option<i32>,
}

pub type PostingKey = (u32, i32, u64, u16);

#[derive(Clone, Default, Data)]
pub struct Ledger {
    pub accounts: Table<Account>,
    pub entries: Table<Entry>,
    pub postings: Map<PostingKey, i64>,
    pub reversed: Map<u64, u64>,
    pub locked_through: Option<i32>,
    pub next_account: u32,
    pub next_entry: u64,
}
