//! The `shop` server: an HTTP API for products, stock, customers, carts and orders.
//!
//! It opens the database at `--db`, builds the server with [`shop::build`] and serves it on
//! `--addr` until Ctrl-C. Writes are awaited with `try_commit_with_async` unless `--writes` picks
//! `inline`, `blocking` or `pool:N`, and `--reads` picks where reads run. `--replica` serves a
//! read-only copy of a file another `shop` process writes, refreshing it every `--refresh-ms`.
//! `--coalesce` turns on commit coalescing, `--durability` picks `fsync` or `buffered`,
//! `--keep-versions` and `--reclaim-mb` bound the history kept, and `--workers` sets the number of
//! Tokio worker threads.
//!
//! ```sh
//! make shop DB=target/demo/shop.ferro ADDR=127.0.0.1:3000
//! cargo run --release -p shop --bin shop -- --db shop.ferro --coalesce
//! cargo run --release -p shop --bin shop -- --db shop.ferro --addr 127.0.0.1:3001 --replica
//! ```

use std::path::PathBuf;
use std::time::Instant;

use ferro::Durability;
use shop::{Config, build, open_db, serve};
use tokio::net::TcpListener;

fn usage() -> ! {
    eprintln!(
        "usage: shop --db PATH [--addr HOST:PORT] [--replica] [--reads inline|blocking] \
         [--writes async|inline|blocking|pool:N] [--durability fsync|buffered] [--refresh-ms N] \
         [--workers N] [--keep-versions N] [--reclaim-mb N] [--coalesce]"
    );
    std::process::exit(2)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut db = None;
    let mut addr = "127.0.0.1:8080".to_string();
    let mut config = Config::default();
    let mut workers = None;
    while let Some(flag) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(value())),
            "--addr" => addr = value(),
            "--replica" => config.replica = true,
            "--coalesce" => config.coalesce = true,
            "--reads" => config.reads = value().parse().unwrap_or_else(|_| usage()),
            "--writes" => config.writes = value().parse().unwrap_or_else(|_| usage()),
            "--refresh-ms" => config.refresh_ms = value().parse().unwrap_or_else(|_| usage()),
            "--keep-versions" => {
                config.keep_versions = Some(value().parse().unwrap_or_else(|_| usage()));
            }
            "--reclaim-mb" => {
                config.reclaim_bytes = value().parse::<u64>().unwrap_or_else(|_| usage()) << 20;
            }
            "--workers" => workers = Some(value().parse::<usize>().unwrap_or_else(|_| usage())),
            "--durability" => {
                config.durability = match value().as_str() {
                    "fsync" => Durability::Fsync,
                    "buffered" => Durability::Buffered,
                    _ => usage(),
                }
            }
            _ => usage(),
        }
    }
    let Some(db) = db else { usage() };
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all();
    if let Some(workers) = workers {
        runtime.worker_threads(workers);
    }
    let runtime = runtime.build().expect("runtime");
    runtime.block_on(async move {
        let started = Instant::now();
        let handle = open_db(&db, &config).unwrap_or_else(|error| {
            eprintln!("cannot open {}: {error}", db.display());
            std::process::exit(1)
        });
        let version = handle.version();
        let opened = started.elapsed();
        let app = build(handle, &config);
        let listener = TcpListener::bind(&addr).await.expect("bind");
        println!(
            "listening on {} version={version} open_ms={:.2} replica={} reads={:?} writes={:?}",
            listener.local_addr().expect("addr"),
            opened.as_secs_f64() * 1e3,
            config.replica,
            config.reads,
            config.writes,
        );
        let stop = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        serve(listener, app, stop).await.expect("serve");
    });
}
