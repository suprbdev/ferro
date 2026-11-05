use std::fmt;

use clap::ValueEnum;
use ferro::{Data, Map, Record, Summary, Table};

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

/// An account, unique by name, with its [`Kind`], its currency, whether it is closed, and its
/// current balance in minor units.
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

/// One line of an [`Entry`]: an amount in the account's minor units, positive for a debit and
/// negative for a credit.
#[derive(Clone, Debug, PartialEq, Data)]
pub struct Line {
    pub account: u32,
    pub amount: i64,
}

/// A posted journal entry: its date in days since 1970-01-01, its memo and lines, and the entry it
/// reverses or the closed date it adjusts, if any.
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

#[derive(Clone, Copy, Debug, PartialEq, Data)]
#[ferro(transparent)]
pub struct Net(pub i128);

impl Summary<PostingKey, i64> for Net {
    fn of(_key: &PostingKey, amount: &i64) -> Self {
        Net(i128::from(*amount))
    }

    fn combine(&self, other: &Self) -> Self {
        Net(self.0 + other.0)
    }
}

/// The database root: accounts, entries, the summarized postings, which entries have been reversed,
/// the date the books are closed through, and the id counters.
#[derive(Clone, Default, Data)]
pub struct Ledger {
    pub accounts: Table<Account>,
    pub entries: Table<Entry>,
    pub postings: Map<PostingKey, i64, Net>,
    pub reversed: Map<u64, u64>,
    pub locked_through: Option<i32>,
    pub next_account: u32,
    pub next_entry: u64,
}
