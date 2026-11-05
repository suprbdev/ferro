//! `shop-probe`: experiments that measure how ferro behaves under the shop's workload.
//!
//! The first argument picks the experiment, each run on a fresh file under `target/bench` with
//! order-like commits. `lock` measures commit throughput as threads and the time spent inside a
//! commit closure grow, `snapshot` the cost of holding old snapshots while committing, `watch` the
//! cost of a watcher that drains or never reads, `history` and `step` the cost of `history_of` and
//! of each step back through the log, `open` the open time as history grows, and `etag` the cost of
//! computing `ETag`s directly and over HTTP.
//!
//! ```sh
//! cargo run --release -p shop --bin shop-probe -- lock quick
//! cargo run --release -p shop --bin shop-probe -- etag
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ferro::{Db, Durability, Options};
use shop::minihttp::Client;
use shop::model::{Line, Order, OrderStatus, Product, Shop, now_ms, products_tag, row_hash};
use shop::{Config, build, open_db, serve};

fn dir() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/bench");
    std::fs::create_dir_all(&path).expect("bench dir");
    path
}

fn fresh(name: &str) -> PathBuf {
    let path = dir().join(name);
    let _ = std::fs::remove_file(&path);
    path
}

fn rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn spin(micros: u64) {
    if micros == 0 {
        return;
    }
    let until = Instant::now() + Duration::from_micros(micros);
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

fn seed(db: &Db<Shop>, products: u64) {
    db.commit("seed", |shop| {
        for id in 1..=products {
            shop.next_product += 1;
            shop.products.insert(Product {
                id,
                name: format!("Product {id}"),
                category: "tools".into(),
                price_cents: 100,
                active: true,
            });
            shop.stock.insert(id, u32::MAX / 2);
        }
    })
    .expect("seed");
}

fn order_commit(db: &Db<Shop>, product: u64, extra_micros: u64) {
    db.commit("order", |shop| {
        spin(extra_micros);
        if let Some(stock) = shop.stock.get_mut(&product) {
            *stock -= 1;
        }
        shop.next_order += 1;
        let id = shop.next_order;
        shop.orders.insert(Order {
            id,
            customer: 1 + id % 100,
            status: OrderStatus::Placed,
            lines: vec![Line {
                product_id: product,
                qty: 1,
                price_cents: 100,
            }],
            total_cents: 100,
            created_ms: now_ms(),
        });
    })
    .expect("commit");
}

fn lock_experiment(quick: bool) {
    println!("closure lock experiment: threads x closure spin, commits/s (order-like closure)");
    let durabilities: &[Durability] = if quick {
        &[Durability::Fsync]
    } else {
        &[Durability::Fsync, Durability::Buffered]
    };
    let spins: &[u64] = if quick { &[0] } else { &[0, 50, 200, 1000] };
    let thread_counts: &[usize] = if quick {
        &[1, 2, 4, 8, 16]
    } else {
        &[1, 2, 8, 32]
    };
    for &durability in durabilities {
        for &spin_us in spins {
            let mut row = Vec::new();
            for &threads in thread_counts {
                let path = fresh("probe-lock.ferro");
                let db = Options::new()
                    .durability(durability)
                    .open::<Shop>(&path)
                    .expect("open");
                seed(&db, 64);
                let stop = Arc::new(AtomicBool::new(false));
                let done = Arc::new(AtomicU64::new(0));
                let mut handles = Vec::new();
                for thread in 0..threads {
                    let db = db.clone();
                    let stop = stop.clone();
                    let done = done.clone();
                    handles.push(std::thread::spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            order_commit(&db, 1 + (thread as u64 % 64), spin_us);
                            done.fetch_add(1, Ordering::Relaxed);
                        }
                    }));
                }
                std::thread::sleep(Duration::from_millis(500));
                let from = done.load(Ordering::Relaxed);
                let started = Instant::now();
                std::thread::sleep(Duration::from_secs(3));
                let count = done.load(Ordering::Relaxed) - from;
                let elapsed = started.elapsed().as_secs_f64();
                stop.store(true, Ordering::Relaxed);
                for handle in handles {
                    handle.join().expect("join");
                }
                row.push(format!("T{threads}={:.0}", count as f64 / elapsed));
            }
            println!("{durability:?} closure+{spin_us}us: {}", row.join("  "));
        }
    }
}

fn snapshot_experiment() {
    println!("long-held snapshot experiment (order-like commits, buffered, single writer)");
    let choice: usize = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let scenarios = [
        ("no snapshots held", 0usize),
        ("snapshot held every 500 commits", 500),
        ("snapshot held every 10 commits", 10),
        ("one snapshot from v1", usize::MAX),
    ];
    for (label, hold_every) in [scenarios[choice]] {
        let path = fresh("probe-snap.ferro");
        let db = Options::new()
            .durability(Durability::Buffered)
            .cache_bytes(64 << 20)
            .open::<Shop>(&path)
            .expect("open");
        seed(&db, 2000);
        let rss_before = rss_kb();
        let mut held = Vec::new();
        if hold_every == usize::MAX {
            held.push(db.read());
        }
        let commits = 30_000usize;
        let started = Instant::now();
        for index in 0..commits {
            order_commit(&db, 1 + (index as u64 % 2000), 0);
            if hold_every != 0 && hold_every != usize::MAX && index % hold_every == 0 {
                held.push(db.read());
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        let rss_after = rss_kb();
        let probe = held.first().cloned();
        let mut read_micros = 0.0;
        if let Some(old) = &probe {
            let at = Instant::now();
            let mut sum = 0u64;
            for id in 1..=2000u64 {
                sum += old.stock.get(&id).map(|s| u64::from(*s)).unwrap_or(0) % 2;
            }
            read_micros = at.elapsed().as_secs_f64() * 1e6 / 2000.0;
            std::hint::black_box(sum);
        }
        let held_count = held.len();
        let file_before = db.size_on_disk();
        drop(held);
        drop(probe);
        println!(
            "{label}: {:.0} commits/s, rss +{} MB over {held_count} held, old-snapshot get {read_micros:.2}us, file {} MB",
            commits as f64 / elapsed,
            (rss_after.saturating_sub(rss_before)) / 1024,
            file_before / (1024 * 1024)
        );
    }
    if choice != 0 {
        return;
    }
    let path = fresh("probe-snap-lock.ferro");
    let db = Db::<Shop>::open(&path).expect("open");
    seed(&db, 10);
    let held = db.read();
    drop(db);
    let blocked = Db::<Shop>::open(&path).err().map(|e| e.to_string());
    drop(held);
    let freed = Db::<Shop>::open(&path).is_ok();
    println!(
        "reopen as writer while a Snapshot outlives the Db: {blocked:?}; after drop: ok={freed}"
    );
}

fn watch_experiment() {
    println!(
        "watch backlog experiment (order-like commits, buffered, single writer, 100k commits)"
    );
    let choice: usize = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let commits: usize = std::env::args()
        .nth(3)
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    for mode in [["no watcher", "draining watcher", "watcher that never reads"][choice]] {
        let path = fresh("probe-watch.ferro");
        let db = Options::new()
            .durability(Durability::Buffered)
            .cache_bytes(64 << 20)
            .open::<Shop>(&path)
            .expect("open");
        seed(&db, 2000);
        let rss_before = rss_kb();
        let receiver = match mode {
            "no watcher" => None,
            _ => Some(db.watch()),
        };
        let seen = Arc::new(AtomicU64::new(0));
        let mut drain = None;
        let mut parked = None;
        match (mode, receiver) {
            ("draining watcher", Some(receiver)) => {
                let seen = seen.clone();
                drain = Some(std::thread::spawn(move || {
                    for _ in receiver {
                        seen.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            (_, Some(receiver)) => parked = Some(receiver),
            _ => {}
        }
        let started = Instant::now();
        for index in 0..commits {
            order_commit(&db, 1 + (index as u64 % 2000), 0);
        }
        let elapsed = started.elapsed().as_secs_f64();
        let rss_after = rss_kb();
        let mut line = format!(
            "{mode}: {:.0} commits/s, rss +{} MB",
            commits as f64 / elapsed,
            rss_after.saturating_sub(rss_before) / 1024
        );
        if let Some(receiver) = parked {
            let at = Instant::now();
            let mut drained = 0u64;
            while receiver.try_recv().is_ok() {
                drained += 1;
            }
            line.push_str(&format!(
                ", queued {drained} snapshots ({} B each), drained in {:.0} ms, rss after drain +{} MB",
                rss_after.saturating_sub(rss_before) * 1024 / drained.max(1),
                at.elapsed().as_secs_f64() * 1e3,
                rss_kb().saturating_sub(rss_before) / 1024
            ));
        }
        println!("{line}");
        drop(db);
        if let Some(handle) = drain {
            handle.join().expect("drain thread exits when the Db drops");
            println!(
                "  drain thread saw {} snapshots",
                seen.load(Ordering::Relaxed)
            );
        }
    }
}

fn history_experiment() {
    println!("history_of(order 1) cost vs commit count");
    for total in [1_000usize, 10_000, 50_000] {
        let path = fresh("probe-history.ferro");
        let db = Options::new()
            .durability(Durability::Buffered)
            .cache_bytes(64 << 20)
            .open::<Shop>(&path)
            .expect("open");
        seed(&db, 100);
        for index in 0..total {
            order_commit(&db, 1 + (index as u64 % 100), 0);
            if index == total / 2 {
                db.commit("ship", |shop| {
                    shop.orders
                        .update(&1, |order| order.status = OrderStatus::Shipped);
                })
                .expect("ship");
            }
        }
        let row = |id: u64| {
            let at = Instant::now();
            let count = db
                .history_of(|shop| shop.orders.get(&id))
                .while_some()
                .count();
            (count, at.elapsed().as_secs_f64() * 1e3)
        };
        let (old, old_ms) = row(1);
        let (latest, latest_ms) = row(total as u64);
        let at = Instant::now();
        let full = db.history_of(|shop| shop.orders.get(&1)).count();
        let full_ms = at.elapsed().as_secs_f64() * 1e3;
        let at = Instant::now();
        let recent = db.log().iter().rev().take(20).count();
        let recent_ms = at.elapsed().as_secs_f64() * 1e3;
        println!(
            "{total} commits: order 1 -> {old} entries in {old_ms:.1} ms; newest order -> {latest} entries in {latest_ms:.2} ms; full walk {full} entries in {full_ms:.1} ms; last {recent} log entries in {recent_ms:.3} ms"
        );
    }
}

fn step_experiment() {
    println!("cost of one history step, us per commit");
    let total = 50_000usize;
    let path = fresh("probe-step.ferro");
    {
        let db = Options::new()
            .durability(Durability::Buffered)
            .open::<Shop>(&path)
            .expect("open");
        seed(&db, 100);
        for index in 0..total {
            order_commit(&db, 1 + (index as u64 % 100), 0);
        }
    }
    let fresh_handle = || {
        Options::new()
            .read_only(true)
            .open::<Shop>(&path)
            .expect("open")
    };
    let steps = total as f64 + 1.0;
    let per = |at: Instant| at.elapsed().as_secs_f64() * 1e6 / steps;
    let latest = total as u64;
    let db = fresh_handle();
    let at = Instant::now();
    db.log().iter().rev().count();
    let walk_cold = per(at);
    let at = Instant::now();
    db.log().iter().rev().count();
    let walk_warm = per(at);
    let db = fresh_handle();
    let at = Instant::now();
    db.history_of(|_| 0u8).count();
    let root_cold = per(at);
    let at = Instant::now();
    db.history_of(|_| 0u8).count();
    let root_warm = per(at);
    let db = fresh_handle();
    let at = Instant::now();
    db.history_of(|shop| shop.orders.get(&1)).count();
    let old_cold = per(at);
    let at = Instant::now();
    db.history_of(|shop| shop.orders.get(&1)).count();
    let old_warm = per(at);
    let db = fresh_handle();
    let at = Instant::now();
    db.history_of(|shop| shop.orders.get(&latest)).count();
    let new_cold = per(at);
    let at = Instant::now();
    db.history_of(|shop| shop.orders.get(&latest)).count();
    let new_warm = per(at);
    let db = fresh_handle();
    let at = Instant::now();
    for version in (1..=db.version()).rev().take(5000) {
        std::hint::black_box(db.at(version).expect("at").orders.get(&latest));
    }
    let via_at = at.elapsed().as_secs_f64() * 1e6 / 5000.0;
    println!("log entry only        cold {walk_cold:.2} warm {walk_warm:.2}");
    println!("entry and root        cold {root_cold:.2} warm {root_warm:.2}");
    println!("order 1               cold {old_cold:.2} warm {old_warm:.2}");
    println!("newest order          cold {new_cold:.2} warm {new_warm:.2}");
    println!("at(v) and newest row  cold {via_at:.2}");
}

fn open_experiment() {
    println!("open time vs history length (single-order commits, buffered)");
    for total in [1_000usize, 20_000, 100_000, 300_000] {
        let path = fresh("probe-open.ferro");
        {
            let db = Options::new()
                .durability(Durability::Buffered)
                .cache_bytes(64 << 20)
                .open::<Shop>(&path)
                .expect("open");
            seed(&db, 2000);
            for index in 0..total {
                order_commit(&db, 1 + (index as u64 % 2000), 0);
            }
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let at = Instant::now();
        let db = Db::<Shop>::open(&path).expect("reopen");
        let open = at.elapsed();
        let at = Instant::now();
        let first = db.read().products.get(&5).is_some();
        let read = at.elapsed();
        let at = Instant::now();
        let scan = db.read().orders.iter().count();
        let scan_time = at.elapsed();
        println!(
            "{total} commits, file {:.1} MB: open {:.2} ms, first get {:.2} ms ({first}), scan {scan} orders in {:.0} ms, {} B/commit",
            size as f64 / 1e6,
            open.as_secs_f64() * 1e3,
            read.as_secs_f64() * 1e3,
            scan_time.as_secs_f64() * 1e3,
            size / (total as u64 + 1)
        );
    }
}

fn etag_experiment() {
    println!("etag cost");
    let path = fresh("probe-etag.ferro");
    let db = Db::<Shop>::open(&path).expect("open");
    seed(&db, 20_000);
    let snap = db.read();
    let rounds = 200_000;
    let at = Instant::now();
    let mut keep = 0usize;
    for _ in 0..rounds {
        keep += products_tag(&snap).len();
    }
    println!(
        "products_tag (root hash of products + stock, 20k products): {:.0} ns",
        at.elapsed().as_nanos() as f64 / rounds as f64
    );
    let product = snap.products.get(&777).expect("product");
    let at = Instant::now();
    for _ in 0..rounds {
        keep += row_hash(&*product).len();
    }
    println!(
        "row_hash of one product: {:.0} ns",
        at.elapsed().as_nanos() as f64 / rounds as f64
    );
    let at = Instant::now();
    for _ in 0..rounds {
        let mut hasher = 0u64;
        for item in snap.products.range(1000u64..1200).take(200) {
            hasher += item.id + item.price_cents;
        }
        keep += hasher as usize & 1;
    }
    println!(
        "scan 200 products in range: {:.0} ns",
        at.elapsed().as_nanos() as f64 / rounds as f64
    );
    std::hint::black_box(keep);
}

async fn etag_http() {
    let path = fresh("probe-etag-http.ferro");
    let config = Config {
        durability: Durability::Buffered,
        ..Config::default()
    };
    let db = open_db(&path, &config).expect("open");
    seed(&db, 20_000);
    let app = build(db, &config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(serve(listener, app, std::future::pending()));
    let mut client = Client::connect(addr).await.expect("connect");
    for (label, route) in [
        ("list 200 items", "/products?limit=200"),
        ("single product", "/products/777"),
    ] {
        let first = client.get(route).await.expect("get");
        let tag = first.header("etag").expect("etag").to_string();
        let rounds = 20_000;
        let mut full = Vec::new();
        let mut cond = Vec::new();
        for _ in 0..rounds {
            let at = Instant::now();
            let response = client.get(route).await.expect("get");
            full.push(at.elapsed().as_nanos() as u64);
            assert_eq!(response.status, 200);
            let at = Instant::now();
            let response = client
                .send("GET", route, &[("if-none-match", &tag)], None)
                .await
                .expect("get");
            cond.push(at.elapsed().as_nanos() as u64);
            assert_eq!(response.status, 304);
        }
        full.sort_unstable();
        cond.sort_unstable();
        println!(
            "{label}: 200 body {} B p50={:.1}us p99={:.1}us | 304 p50={:.1}us p99={:.1}us",
            first.body.len(),
            full[rounds / 2] as f64 / 1e3,
            full[rounds * 99 / 100] as f64 / 1e3,
            cond[rounds / 2] as f64 / 1e3,
            cond[rounds * 99 / 100] as f64 / 1e3
        );
    }
}

fn require_path(path: &Path) {
    println!("data dir: {}", path.display());
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_default();
    require_path(&dir());
    match which.as_str() {
        "lock" => lock_experiment(std::env::args().nth(2).as_deref() == Some("quick")),
        "snapshot" => snapshot_experiment(),
        "watch" => watch_experiment(),
        "history" => history_experiment(),
        "step" => step_experiment(),
        "open" => open_experiment(),
        "etag" => {
            etag_experiment();
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(etag_http());
        }
        _ => eprintln!("usage: shop-probe lock|snapshot|watch|history|step|open|etag"),
    }
}
