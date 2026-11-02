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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            replica: false,
            reads: ReadMode::Inline,
            writes: WriteMode::Pool(32),
            durability: Durability::Fsync,
            refresh_ms: 5,
            feed_capacity: 1024,
            keep_versions: None,
            reclaim_bytes: Reclaim::default().every_bytes,
        }
    }
}

pub fn open_db(path: &Path, config: &Config) -> ferro::Result<Db<Shop>> {
    let options = Options::new().durability(config.durability);
    let options = match config.keep_versions {
        _ if config.replica => options.read_only(),
        Some(count) => options.history(Keep::Last(count)).reclaim(Reclaim {
            every_bytes: config.reclaim_bytes,
            ..Reclaim::default()
        }),
        None => options,
    };
    options.open::<Shop>(path)
}

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
                        if matches!(error, ferro::Error::Replaced) {
                            return;
                        }
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
