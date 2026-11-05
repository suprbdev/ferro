//! A double-entry ledger on ferro, with balances as of any date from summaries.
//!
//! [`Ledger`] is the database root: a table of [`Account`] rows unique by name, a table of journal
//! [`Entry`] rows, and every posting line in a `Map` keyed by account, date, entry and line whose
//! summary sums the amounts. An account's balance as of a date, whether asked for alone, in a trial
//! balance or as a statement's opening balance, is then one `range_summary` over its postings,
//! without reading them. Each account row also carries its current balance, moved in the same
//! commit as its postings. [`Ledger::post`] checks that the debits and credits of each currency
//! agree and that the date is not in a closed period before it writes anything. The modules hold
//! the parts: `model` the rows and the root, `ops` posting, reversing, period closing and balances,
//! `audit` a check that rebuilds the postings and balances from the journal and diffs sampled
//! versions to confirm they balance, `money` amounts and dates, `seed` demo data and `error` the
//! [`LedgerError`]. The `ledger` binary and the tests use this library.
//!
//! ```sh
//! cargo run --release -p ledger -- account add assets:cash --kind asset
//! cargo run --release -p ledger -- balance assets:cash --as-of 2024-02-01
//! ```

pub mod audit;
pub mod error;
pub mod model;
pub mod money;
pub mod ops;
pub mod seed;

pub use error::LedgerError;
pub use model::{Account, Entry, Kind, Ledger, Line};
pub use ops::{NewEntry, NewLine};
