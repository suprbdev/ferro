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

#[derive(Default)]
pub struct Counters {
    pub posts: AtomicU64,
    pub writes: AtomicU64,
}

pub struct App {
    pub db: Db<Chat>,
    pub feeds: Arc<Feeds>,
    pub counters: Counters,
    pub closing: watch::Sender<bool>,
}

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

pub fn build(db: Db<Chat>, config: &Config) -> Shared {
    Arc::new(App {
        db,
        feeds: Arc::new(Feeds::new(config.max_changes)),
        counters: Counters::default(),
        closing: watch::channel(false).0,
    })
}

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
