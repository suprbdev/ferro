//! A multi-room chat service on axum and ferro, with live updates as Server-Sent Events.
//!
//! [`Chat`] is the database root: a table of users and a table of rooms, where each [`Room`] row
//! holds its members, its messages and a word index for search. The messages are a
//! `Map<u64, Message, Live>` whose [`Live`] summary counts the ones not deleted, so an unread count
//! is one `range_summary` past the reader's receipt however long the history is. Each live feed is
//! a [`Subscription`] on a `watch_changes` watcher whose test compares the rooms in its [`Scope`],
//! and its events are a `diff` between the snapshot it delivered last and the new one. The event id
//! is the version, so a client that reconnects with `Last-Event-ID` gets one diff from there, or a
//! reset if that version is no longer kept. [`App::write`] awaits each write with
//! `try_commit_with_async`, so neither writes nor feeds hold a thread. The modules hold the parts:
//! `model` the rows and their JSON views, `ops` the writes, `feed` the subscriptions, `routes` the
//! axum router, `error` the [`ApiError`], and `minihttp` a small HTTP and SSE client for the load
//! tester and the tests. The `chat` and `chat-load` binaries and the tests use this library.
//!
//! ```sh
//! make chat DB=target/demo/chat.ferro ADDR=127.0.0.1:3000
//! make chat-load SECS=10
//! ```

pub mod error;
pub mod feed;
pub mod minihttp;
pub mod model;
pub mod ops;
pub mod routes;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use axum::serve::ListenerExt;
use ferro::{Committed, Db, Durability, Keep, Options, Reclaim};
use tokio::net::TcpListener;
use tokio::sync::watch;

pub use error::ApiError;
pub use feed::{Feeds, Frame, Scope, Subscription};
pub use model::*;

/// How [`open_db`] and [`build`] set up the service: durability, commit coalescing, how many
/// versions to keep and how often to reclaim space, and the most changes one feed frame carries.
#[derive(Clone, Debug)]
pub struct Config {
    pub durability: Durability,
    pub coalesce: bool,
    pub keep_versions: Option<usize>,
    pub reclaim_bytes: u64,
    pub max_changes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            durability: Durability::Fsync,
            coalesce: false,
            keep_versions: None,
            reclaim_bytes: Reclaim::default().every_bytes,
            max_changes: 2000,
        }
    }
}

/// Opens or creates the chat database at `path` with the options in `config`.
///
/// # Errors
///
/// Returns the error ferro reports if it cannot open the database at `path`.
pub fn open_db(path: &Path, config: &Config) -> ferro::Result<Db<Chat>> {
    let options = Options::new()
        .durability(config.durability)
        .coalesce(config.coalesce);
    let options = match config.keep_versions {
        Some(count) => options.history(Keep::Last(count)).reclaim(Reclaim {
            every_bytes: config.reclaim_bytes,
        }),
        None => options,
    };
    options.open::<Chat>(path)
}

/// Counters kept while the service runs; `/health` reports the number of posts.
#[derive(Default)]
pub struct Counters {
    pub posts: AtomicU64,
    pub writes: AtomicU64,
}

/// The running service: the database, the open feeds, counters, and the signal that it is shutting
/// down.
pub struct App {
    pub db: Db<Chat>,
    pub feeds: Arc<Feeds>,
    pub counters: Counters,
    pub closing: watch::Sender<bool>,
}

/// The [`App`] as the axum handlers share it.
pub type Shared = Arc<App>;

impl App {
    pub async fn write<R, F>(&self, work: F) -> Result<Committed<R>, ApiError>
    where
        F: FnOnce(&mut Chat) -> Result<(String, R), ApiError>,
    {
        self.db.try_commit_with_async(work).await
    }

    pub fn subscribe(&self, scope: Scope, since: Option<u64>) -> Result<Subscription, ApiError> {
        feed::subscribe(&self.feeds, &self.db, scope, since)
    }

    pub fn close(&self) {
        self.closing.send_replace(true);
        self.feeds.close_all();
    }
}

/// Wraps an open database in an [`App`] with no feeds yet.
pub fn build(db: Db<Chat>, config: &Config) -> Shared {
    Arc::new(App {
        db,
        feeds: Arc::new(Feeds::new(config.max_changes)),
        counters: Counters::default(),
        closing: watch::channel(false).0,
    })
}

/// Serves the chat API on `listener` until `shutdown` completes, then closes every feed.
///
/// # Errors
///
/// Returns an I/O error if the server fails.
pub async fn serve(
    listener: TcpListener,
    app: Shared,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let closing = app.clone();
    let listener = listener.tap_io(|stream| {
        let _ = stream.set_nodelay(true);
    });
    let served = axum::serve(listener, routes::router(app.clone()))
        .with_graceful_shutdown(async move {
            shutdown.await;
            closing.closing.send_replace(true);
        })
        .await;
    app.close();
    served
}
