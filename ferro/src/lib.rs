//! An embedded database where the database is one of your own types.
//!
//! There is no query language, schema file or mapping layer. You define a type, derive [`Data`]
//! for it, and open a file of it as a [`Db`]. Every commit makes a new immutable version of that
//! value, and earlier versions stay readable. ferro is an experiment: its file format is not
//! stable.
//!
//! # The model
//!
//! - **Your type is the database.** [`Db<T>`](Db) holds one value of `T`. Inside it, [`Map`],
//!   [`Set`], [`List`] and [`Table`] are persistent collections that store only what changed.
//! - **Reads are snapshots.** [`Db::read`] returns a [`Snapshot`] that derefs to `T`. It never
//!   blocks and never changes, however long it is held.
//! - **Writes are closures.** [`Db::update`], [`Db::commit`] and their relatives run a closure over
//!   `&mut T` and commit what it changed atomically. Closures on one branch run one at a time, so a
//!   transaction is serializable by construction, and a closure that panics or returns `Err`
//!   commits nothing. The `_async` forms are futures that need no particular runtime.
//! - **History.** [`Db::at`], [`Db::as_of`], [`Db::history_of`], [`Db::bisect`] and [`Db::log`]
//!   read old versions, and [`Db::restore`] commits one again. [`Options::history`] bounds how much
//!   is kept, and reclamation copies what is kept into a new file while writers run.
//! - **Branches.** [`Db::fork`] makes a branch in one small commit, and [`Db::merge_with`] merges
//!   one into another three ways, with conflicts resolved by your closure.
//! - **Sync.** [`Db::pull`] brings another file's commits into this one, and [`Db::have`],
//!   [`Db::bundle`] and [`Db::receive`] do the same as bytes over any transport.
//! - **Watchers.** [`Db::watch`] returns a [`Watcher`] that hands out each new snapshot of a
//!   branch.
//!
//! # Examples
//!
//! ```no_run
//! use ferro::{Data, Db, Map};
//!
//! #[derive(Data, Clone, Default)]
//! struct Shop {
//!     stock: Map<String, u32>,
//! }
//!
//! # fn main() -> ferro::Result<()> {
//! let db = Db::<Shop>::open("shop.ferro")?;
//!
//! db.commit("receive stock", |shop| {
//!     shop.stock.insert("anvil".to_string(), 5);
//! })?;
//!
//! let shop = db.read();
//! assert_eq!(shop.stock.get("anvil").as_deref(), Some(&5));
//! assert_eq!(shop.version(), 1);
//! # Ok(())
//! # }
//! ```
//!
//! # Guides
//!
//! The guides in the repository's `docs` directory are the long form:
//!
//! - [Getting started](docs/guide/getting-started.md): a first database, reading, writing and
//!   [`Options`].
//! - [Collections](docs/guide/collections.md) and [Tables and indexes](docs/guide/tables.md): what
//!   goes inside your type.
//! - [History, versions and branches](docs/guide/history.md) and [Merging](docs/guide/merging.md).
//! - [Watchers](docs/guide/watchers.md) and [Sync between files](docs/guide/sync.md).
//! - [Durability and crash safety](docs/guide/durability.md) and [Retention and
//!   reclamation](docs/guide/retention.md).
//! - [Writers and concurrency](docs/guide/concurrency.md): branch locks, awaited writes,
//!   coalescing and several processes.
//! - [Changing your types](docs/guide/schema-evolution.md) and
//!   [Limitations](docs/guide/limitations.md).
//!
//! [The documentation index](docs/README.md) lists the internals, which describe how each part
//! works and what it costs.

extern crate self as ferro;

pub mod admin;
pub mod codec;
pub mod db;
mod dense;
mod epoch;
pub mod error;
mod graph;
pub mod history;
pub mod list;
pub mod log;
pub mod map;
mod platform;
mod prefix;
mod reclaim;
mod reentry;
mod refs;
pub mod set;
mod shared;
pub mod store;
pub mod sync;
pub mod table;
pub mod watch;

pub use admin::{Compacted, Keep, Reclaim, Space};
pub use codec::{Bytes, Data, Hash, Ptr, hash_of};
pub use db::{Commit, Committed, Db, Options, Snapshot, Stamp, Verified};
pub use error::{Error, Result};
pub use ferro_derive::{Data, Record};
pub use history::History;
pub use list::List;
pub use log::Log;
pub use map::{Change, Conflict, Cursor, Map, Summary, Val};
pub use prefix::Prefix;
pub use set::Set;
pub use store::Durability;
pub use sync::{Bundle, Have, Outcome, Pulled};
pub use table::{Record, RowMut, Table, TableData, Taken};
pub use watch::{Changed, Closer, Watcher};
