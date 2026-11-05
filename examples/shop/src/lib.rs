//! An HTTP shop on axum and ferro: products, stock, customers, carts and orders.
//!
//! [`Shop`] is the database root: tables of products, customers and orders, with indexes on product
//! category, customer email, order customer and status, and a compound index on customer and
//! status, plus maps of stock and carts. Writes are awaited with `try_commit_with_async` by
//! default, and [`WriteMode`] selects running them inline, on the blocking pool or on a pool of
//! writer threads instead. An order checks and takes its stock in the same commit, so it either
//! commits whole or is refused with a conflict that lists what is short. Reads carry `ETag`s made
//! from content hashes, of a row or of a table's root, so an unchanged answer is a
//! `304 Not Modified`. A broadcaster thread watches the database and diffs each new snapshot
//! against the last into one Server-Sent Event, and a client resuming from an older version gets
//! one diff from there. Order history comes from `history_of_range`, and any read route can be
//! pinned to a past version under `/admin/snapshot/{version}`. With [`Config::replica`] the server
//! opens the file read-only, refuses writes, and refreshes to the writer's newest version on a
//! timer. The modules hold the parts: `model` the rows, `app` the [`App`] and its [`Bridge`],
//! `routes` the axum router, `events` the change feed and `minihttp` a small HTTP and SSE client
//! for the load tester and the tests. The `shop`, `shop-load` and `shop-probe` binaries and the
//! tests use this library.
//!
//! ```sh
//! make shop DB=target/demo/shop.ferro ADDR=127.0.0.1:3000
//! make shop-load SECS=10
//! ```

pub mod app;
pub mod events;
pub mod minihttp;
pub mod model;
pub mod routes;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ferro::{Db, Durability, Keep, Options, Reclaim};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

pub use app::{App, Bridge, ReadMode, WriteMode};
pub use model::*;

/// How [`open_db`] and [`build`] set up the server: as a writer or a read-only replica, how reads
/// and writes run, durability and coalescing, how often a replica refreshes, how many change frames
/// the feed buffers, and how much history to keep and reclaim.
#[derive(Clone, Debug)]
pub struct Config {
    pub replica: bool,
    pub reads: ReadMode,
    pub writes: WriteMode,
    pub durability: Durability,
    pub refresh_ms: u64,
    pub feed_capacity: usize,
    pub keep_versions: Option<usize>,
    pub reclaim_bytes: u64,
    pub coalesce: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            replica: false,
            reads: ReadMode::Inline,
            writes: WriteMode::Async,
            durability: Durability::Fsync,
            refresh_ms: 5,
            feed_capacity: 1024,
            keep_versions: None,
            reclaim_bytes: Reclaim::default().every_bytes,
            coalesce: false,
        }
    }
}

/// Opens or creates the shop database at `path`, read-only when `config` is a replica.
///
/// # Errors
///
/// Returns the error ferro reports if it cannot open the database at `path`.
pub fn open_db(path: &Path, config: &Config) -> ferro::Result<Db<Shop>> {
    let options = Options::new()
        .durability(config.durability)
        .coalesce(config.coalesce)
        .read_only(config.replica);
    let options = match config.keep_versions {
        Some(count) if !config.replica => options.history(Keep::Last(count)).reclaim(Reclaim {
            every_bytes: config.reclaim_bytes,
        }),
        _ => options,
    };
    options.open::<Shop>(path)
}

/// Wraps an open database in an [`App`], starting the change broadcaster and, for a replica, the
/// thread that refreshes it.
pub fn build(db: Db<Shop>, config: &Config) -> Arc<App> {
    let (feed, _) = broadcast::channel(config.feed_capacity);
    events::spawn_broadcaster(&db, feed.clone());
    if config.replica {
        let db = db.clone();
        let pause = Duration::from_millis(config.refresh_ms.max(1));
        std::thread::Builder::new()
            .name("shop-refresh".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(pause);
                    if let Err(error) = db.refresh() {
                        eprintln!("refresh failed: {error}");
                    }
                }
            })
            .expect("spawn refresh");
    }
    Arc::new(App {
        db,
        bridge: Bridge::new(config.reads, config.writes),
        replica: config.replica,
        feed,
        counters: Default::default(),
        closing: watch::channel(false).0,
    })
}

/// Serves the shop API on `listener` until `shutdown` completes.
///
/// # Errors
///
/// Returns an I/O error if the server fails.
pub async fn serve(
    listener: TcpListener,
    app: Arc<App>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let closing = app.clone();
    axum::serve(listener, routes::router(app))
        .with_graceful_shutdown(async move {
            shutdown.await;
            closing.closing.send_replace(true);
        })
        .await
}
