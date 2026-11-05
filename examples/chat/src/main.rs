//! The `chat` server: multi-room chat over HTTP, with live feeds as Server-Sent Events.
//!
//! It opens the database at `--db`, builds the service with [`chat::build`] and serves it on
//! `--addr` until Ctrl-C. Requests made as a user carry its id in an `x-user` header. Clients can
//! create users and rooms, join and leave, post, edit and delete their own messages, react, mark
//! messages read, page through history, search a room's words, and follow a room or all of a user's
//! rooms at `/rooms/{room}/events` or `/users/{user}/events`. `--durability`, `--coalesce`,
//! `--keep-versions`, `--reclaim-mb` and `--max-changes` set the [`chat::Config`].
//!
//! ```sh
//! make chat DB=target/demo/chat.ferro ADDR=127.0.0.1:3000
//! cargo run --release -p chat --bin chat -- --db chat.ferro
//! cargo run --release -p chat --bin chat -- --db chat.ferro --addr 127.0.0.1:3000 --coalesce
//! ```

use std::path::PathBuf;

use chat::{Config, build, open_db, serve};
use ferro::Durability;
use tokio::net::TcpListener;

fn usage() -> ! {
    eprintln!(
        "usage: chat --db PATH [--addr HOST:PORT] [--durability fsync|buffered] [--coalesce] \
         [--keep-versions N] [--reclaim-mb N] [--max-changes N]"
    );
    std::process::exit(2)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut db = None;
    let mut addr = "127.0.0.1:8080".to_string();
    let mut config = Config::default();
    while let Some(flag) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(value())),
            "--addr" => addr = value(),
            "--coalesce" => config.coalesce = true,
            "--keep-versions" => {
                config.keep_versions = Some(value().parse().unwrap_or_else(|_| usage()));
            }
            "--reclaim-mb" => {
                config.reclaim_bytes = value().parse::<u64>().unwrap_or_else(|_| usage()) << 20;
            }
            "--max-changes" => config.max_changes = value().parse().unwrap_or_else(|_| usage()),
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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let handle = open_db(&db, &config).unwrap_or_else(|error| {
            eprintln!("cannot open {}: {error}", db.display());
            std::process::exit(1)
        });
        let version = handle.version();
        let app = build(handle, &config);
        let listener = TcpListener::bind(&addr).await.expect("bind");
        println!(
            "chat listening on {} version={version} coalesce={}",
            listener.local_addr().expect("addr"),
            config.coalesce
        );
        let stop = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        serve(listener, app, stop).await.expect("serve");
    });
}
