use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shop::minihttp::{Client, SseStream};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    Read,
    Cart,
    Order,
}

struct Sample {
    kind: Kind,
    micros: u32,
    status: u16,
}

#[derive(Default)]
struct Tally {
    samples: Vec<Sample>,
    errors: u64,
    not_modified: u64,
    order_lines: HashMap<u64, u64>,
    orders_ok: u64,
    orders_refused: u64,
}

struct Plan {
    write: SocketAddr,
    read: SocketAddr,
    products: u64,
    customers: u64,
    mix: (u64, u64, u64),
    scarce_every: u64,
    deadline: Instant,
}

const CATEGORIES: [&str; 8] = [
    "tools", "garden", "kitchen", "toys", "books", "audio", "outdoor", "office",
];

async fn worker(plan: std::sync::Arc<Plan>, seed: u64) -> Tally {
    let mut rng = Rng(seed | 1);
    let mut tally = Tally::default();
    let mut writer = Client::connect(plan.write).await.expect("connect writer");
    let mut reader = if plan.read == plan.write {
        None
    } else {
        Some(Client::connect(plan.read).await.expect("connect reader"))
    };
    let mut etags: HashMap<String, String> = HashMap::new();
    let mut cursor: Option<(String, u64)> = None;
    let total = plan.mix.0 + plan.mix.1 + plan.mix.2;
    while Instant::now() < plan.deadline {
        let roll = rng.below(total);
        let started = Instant::now();
        if roll < plan.mix.0 {
            let pick = rng.below(100);
            let path = if pick < 40 {
                match cursor.take() {
                    Some((base, after)) if rng.below(5) < 2 => format!("{base}&after={after}"),
                    _ => {
                        let category = CATEGORIES[rng.below(8) as usize];
                        if rng.below(2) == 0 {
                            format!("/products?category={category}&limit=20")
                        } else {
                            "/products?limit=20".to_string()
                        }
                    }
                }
            } else if pick < 80 {
                format!("/products/{}", 1 + rng.below(plan.products))
            } else if pick < 92 {
                format!(
                    "/customers/{}/orders?limit=10",
                    1 + rng.below(plan.customers)
                )
            } else {
                format!("/orders/{}", 1 + rng.below(2000))
            };
            let tag = (rng.below(10) < 4)
                .then(|| etags.get(&path).cloned())
                .flatten();
            let headers: Vec<(&str, &str)> = match &tag {
                Some(tag) => vec![("if-none-match", tag.as_str())],
                None => Vec::new(),
            };
            let client = reader.as_mut().unwrap_or(&mut writer);
            match client.send("GET", &path, &headers, None).await {
                Ok(response) => {
                    let micros = started.elapsed().as_micros() as u32;
                    match response.status {
                        200 => {
                            if let Some(tag) = response.header("etag") {
                                if etags.len() > 5000 {
                                    etags.clear();
                                }
                                etags.insert(path.clone(), tag.to_string());
                            }
                            if path.starts_with("/products?")
                                && let Some(next) = response.json()["next_cursor"].as_u64()
                            {
                                let base = path.split("&after=").next().unwrap_or("").to_string();
                                cursor = Some((base, next));
                            }
                        }
                        304 => tally.not_modified += 1,
                        404 => {}
                        _ => tally.errors += 1,
                    }
                    tally.samples.push(Sample {
                        kind: Kind::Read,
                        micros,
                        status: response.status,
                    });
                }
                Err(_) => {
                    tally.errors += 1;
                    return tally;
                }
            }
        } else if roll < plan.mix.0 + plan.mix.1 {
            let cart = 1 + rng.below(plan.customers);
            let body = json!({"product_id": 1 + rng.below(plan.products), "qty": 1 + rng.below(3)});
            match writer
                .json("POST", &format!("/carts/{cart}/items"), &body)
                .await
            {
                Ok(response) => {
                    if response.status != 201 {
                        tally.errors += 1;
                    }
                    tally.samples.push(Sample {
                        kind: Kind::Cart,
                        micros: started.elapsed().as_micros() as u32,
                        status: response.status,
                    });
                }
                Err(_) => {
                    tally.errors += 1;
                    return tally;
                }
            }
        } else {
            let lines = 1 + rng.below(3);
            let mut items = Vec::new();
            for _ in 0..lines {
                let product = if plan.scarce_every > 0 && rng.below(4) == 0 {
                    plan.scarce_every * (1 + rng.below(plan.products / plan.scarce_every))
                } else {
                    1 + rng.below(plan.products)
                };
                items.push(json!({"product_id": product, "qty": 1 + rng.below(3)}));
            }
            let body = json!({"customer_id": 1 + rng.below(plan.customers), "items": items});
            match writer.json("POST", "/orders", &body).await {
                Ok(response) => {
                    match response.status {
                        201 => {
                            tally.orders_ok += 1;
                            let order = response.json();
                            for line in order["lines"].as_array().into_iter().flatten() {
                                *tally
                                    .order_lines
                                    .entry(line["product_id"].as_u64().unwrap_or(0))
                                    .or_default() += line["qty"].as_u64().unwrap_or(0);
                            }
                        }
                        409 => tally.orders_refused += 1,
                        _ => tally.errors += 1,
                    }
                    tally.samples.push(Sample {
                        kind: Kind::Order,
                        micros: started.elapsed().as_micros() as u32,
                        status: response.status,
                    });
                }
                Err(_) => {
                    tally.errors += 1;
                    return tally;
                }
            }
        }
    }
    tally
}

fn percentile(sorted: &[u32], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    f64::from(sorted[index]) / 1000.0
}

async fn stock_map(client: &mut Client) -> HashMap<u64, u64> {
    let mut stock = HashMap::new();
    let mut after = 0u64;
    loop {
        let page = client
            .get(&format!("/products?limit=200&after={after}"))
            .await
            .expect("list")
            .json();
        for item in page["items"].as_array().into_iter().flatten() {
            stock.insert(
                item["id"].as_u64().unwrap(),
                item["stock"].as_u64().unwrap(),
            );
        }
        match page["next_cursor"].as_u64() {
            Some(next) => after = next,
            None => return stock,
        }
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("socket address")
}

async fn run(args: Vec<String>) {
    let write = addr(&arg(&args, "--addr").unwrap_or_else(|| "127.0.0.1:8080".into()));
    let read = arg(&args, "--read-addr")
        .map(|text| addr(&text))
        .unwrap_or(write);
    let conns: usize = arg(&args, "--conns")
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    let secs: f64 = arg(&args, "--secs")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10.0);
    let products: u64 = arg(&args, "--products")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let customers: u64 = arg(&args, "--customers")
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    let scarce_every: u64 = arg(&args, "--scarce-every")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let scarce_stock: u32 = arg(&args, "--scarce-stock")
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let mix: Vec<u64> = arg(&args, "--mix")
        .unwrap_or_else(|| "90,8,2".into())
        .split(',')
        .map(|part| part.parse().expect("mix"))
        .collect();
    let label = arg(&args, "--label").unwrap_or_default();
    let mut admin = Client::connect(write).await.expect("connect");
    if args.iter().any(|arg| arg == "--seed") {
        let seeded = admin
            .json(
                "POST",
                "/admin/seed",
                &json!({"products": products, "customers": customers, "stock": 1_000_000, "scarce_every": scarce_every, "scarce_stock": scarce_stock}),
            )
            .await
            .expect("seed");
        assert_eq!(seeded.status, 200, "seed failed");
        if read != write {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let before_stats = admin.get("/admin/stats").await.expect("stats").json();
    let mut reader = Client::connect(read).await.expect("connect");
    let before = stock_map(&mut reader).await;

    let plan = std::sync::Arc::new(Plan {
        write,
        read,
        products,
        customers,
        mix: (mix[0], mix[1], mix[2]),
        scarce_every,
        deadline: Instant::now() + Duration::from_secs_f64(secs),
    });
    let started = Instant::now();
    let mut tasks = Vec::new();
    for index in 0..conns {
        tasks.push(tokio::spawn(worker(
            plan.clone(),
            0x9e37_79b9_7f4a_7c15u64.wrapping_mul(index as u64 + 1),
        )));
    }
    let mut all = Tally::default();
    for task in tasks {
        let tally = task.await.expect("worker");
        all.samples.extend(tally.samples);
        all.errors += tally.errors;
        all.not_modified += tally.not_modified;
        all.orders_ok += tally.orders_ok;
        all.orders_refused += tally.orders_refused;
        for (product, qty) in tally.order_lines {
            *all.order_lines.entry(product).or_default() += qty;
        }
    }
    let elapsed = started.elapsed().as_secs_f64();

    let total = all.samples.len();
    let mut line = format!(
        "{label} conns={conns} ops={total} thr={:.0}/s errors={}",
        total as f64 / elapsed,
        all.errors
    );
    let mut every: Vec<u32> = all.samples.iter().map(|sample| sample.micros).collect();
    every.sort_unstable();
    line.push_str(&format!(
        " all p50={:.2}ms p99={:.2}ms",
        percentile(&every, 0.5),
        percentile(&every, 0.99)
    ));
    for kind in [Kind::Read, Kind::Cart, Kind::Order] {
        let mut micros: Vec<u32> = all
            .samples
            .iter()
            .filter(|sample| sample.kind == kind)
            .map(|sample| sample.micros)
            .collect();
        micros.sort_unstable();
        line.push_str(&format!(
            " | {kind:?} n={} p50={:.2} p99={:.2}",
            micros.len(),
            percentile(&micros, 0.5),
            percentile(&micros, 0.99)
        ));
    }
    println!("{line}");
    let mut refused_micros: Vec<u32> = all
        .samples
        .iter()
        .filter(|s| s.kind == Kind::Order && s.status == 409)
        .map(|s| s.micros)
        .collect();
    refused_micros.sort_unstable();
    let mut placed_micros: Vec<u32> = all
        .samples
        .iter()
        .filter(|s| s.kind == Kind::Order && s.status == 201)
        .map(|s| s.micros)
        .collect();
    placed_micros.sort_unstable();
    println!(
        "  order 201 p50={:.2} p99={:.2} | order 409 p50={:.2} p99={:.2}",
        percentile(&placed_micros, 0.5),
        percentile(&placed_micros, 0.99),
        percentile(&refused_micros, 0.5),
        percentile(&refused_micros, 0.99)
    );
    let conflicts = refused_micros.len();
    println!(
        "  orders ok={} refused={} (409={conflicts}) 304s={}",
        all.orders_ok, all.orders_refused, all.not_modified
    );

    if read != write {
        for _ in 0..200 {
            let stats = reader.get("/admin/stats").await.expect("stats").json();
            let primary = admin.get("/admin/stats").await.expect("stats").json();
            if stats["version"] == primary["version"] {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let after_stats = admin.get("/admin/stats").await.expect("stats").json();
    let after = stock_map(&mut admin).await;
    let mut failures = Vec::new();
    let mut consumed_total = 0u64;
    for (product, stock_before) in &before {
        let stock_after = after.get(product).copied().unwrap_or(0);
        let consumed = all.order_lines.get(product).copied().unwrap_or(0);
        consumed_total += consumed;
        if stock_after > *stock_before {
            failures.push(format!("product {product} stock grew"));
        }
        if stock_before - stock_after.min(*stock_before) != consumed {
            failures.push(format!(
                "product {product}: before={stock_before} after={stock_after} consumed_by_orders={consumed}"
            ));
        }
    }
    let server_orders =
        after_stats["orders"].as_u64().unwrap() - before_stats["orders"].as_u64().unwrap();
    let server_ok =
        after_stats["orders_ok"].as_u64().unwrap() - before_stats["orders_ok"].as_u64().unwrap();
    if server_orders != all.orders_ok || server_ok != all.orders_ok {
        failures.push(format!(
            "orders: client ok={} server table delta={server_orders} server counter delta={server_ok}",
            all.orders_ok
        ));
    }
    if all.errors > 0 {
        failures.push(format!("{} request errors", all.errors));
    }
    let report = admin
        .get("/admin/invariants")
        .await
        .expect("invariants")
        .json();
    println!(
        "  check: units consumed={consumed_total} server live_orders={} consumed_units={} min_stock_ok={} => {}",
        report["live_orders"],
        report["consumed_units"],
        after.values().all(|stock| *stock < u64::from(u32::MAX)),
        if failures.is_empty() {
            "PASS".to_string()
        } else {
            format!("FAIL {failures:?}")
        }
    );
    println!(
        "  server: db_bytes={} version={} events_lagged={}",
        after_stats["db_bytes"], after_stats["version"], after_stats["events_lagged"]
    );
    if !failures.is_empty() {
        std::process::exit(1);
    }
}

async fn replica_lag(args: Vec<String>) {
    let write = addr(&arg(&args, "--addr").expect("--addr"));
    let read = addr(&arg(&args, "--read-addr").expect("--read-addr"));
    let rounds: usize = arg(&args, "--rounds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let mut primary = Client::connect(write).await.expect("connect");
    let mut replica = Client::connect(read).await.expect("connect");
    let mut lags = Vec::new();
    for round in 0..rounds {
        let response = primary
            .json(
                "PATCH",
                "/products/1",
                &json!({"name": format!("lag {round}")}),
            )
            .await
            .expect("patch");
        let version = response.json()["version"].as_u64().unwrap();
        let started = Instant::now();
        loop {
            let health = replica.get("/health").await.expect("health").json();
            if health["version"].as_u64().unwrap() >= version {
                break;
            }
            if started.elapsed() > Duration::from_secs(5) {
                panic!("replica never reached {version}");
            }
        }
        lags.push(started.elapsed().as_micros() as u32);
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
    lags.sort_unstable();
    println!(
        "replica lag after ack: p50={:.2}ms p99={:.2}ms max={:.2}ms over {rounds} commits",
        percentile(&lags, 0.5),
        percentile(&lags, 0.99),
        percentile(&lags, 1.0)
    );
}

async fn slow_sse(args: Vec<String>) {
    let write = addr(&arg(&args, "--addr").expect("--addr"));
    let slow: usize = arg(&args, "--slow")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let writes: usize = arg(&args, "--writes")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let payload: usize = arg(&args, "--payload")
        .and_then(|v| v.parse().ok())
        .unwrap_or(4096);
    let mut client = Client::connect(write).await.expect("connect");
    let mut streams = Vec::new();
    for _ in 0..slow {
        let mut stream = SseStream::open(write, "/events", &[]).await.expect("sse");
        stream.next().await.expect("hello");
        streams.push(stream);
    }
    let bulk = "y".repeat(payload);
    let mut micros = Vec::new();
    let started = Instant::now();
    for round in 0..writes {
        let at = Instant::now();
        let response = client
            .json(
                "PATCH",
                "/products/1",
                &json!({"name": format!("{round}{bulk}")}),
            )
            .await
            .expect("patch");
        assert_eq!(response.status, 200);
        micros.push(at.elapsed().as_micros() as u32);
    }
    let elapsed = started.elapsed().as_secs_f64();
    micros.sort_unstable();
    let stats = client.get("/admin/stats").await.expect("stats").json();
    println!(
        "slow_sse={slow} writes={writes} payload={payload}B: {:.0} writes/s p50={:.2}ms p99={:.2}ms subscribers={} lagged={} events_sent={}",
        writes as f64 / elapsed,
        percentile(&micros, 0.5),
        percentile(&micros, 0.99),
        stats["feed_subscribers"],
        stats["events_lagged"],
        stats["events_sent"]
    );
    let mut caught_up = 0usize;
    for stream in &mut streams {
        let mut last = 0u64;
        while last < stats["version"].as_u64().unwrap() {
            match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
                Ok(Ok(Some(event))) if event.event == "change" => {
                    let body: Value = serde_json::from_str(&event.data).unwrap();
                    last = body["version"].as_u64().unwrap();
                }
                Ok(Ok(Some(_))) => {}
                _ => break,
            }
        }
        if last >= stats["version"].as_u64().unwrap() {
            caught_up += 1;
        }
    }
    println!("slow consumers that caught up to final version: {caught_up}/{slow}");
}

async fn crash_writer(args: Vec<String>) {
    let write = addr(&arg(&args, "--addr").expect("--addr"));
    let conns: usize = arg(&args, "--conns")
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let out = arg(&args, "--out").expect("--out");
    let mut admin = Client::connect(write).await.expect("connect");
    let seeded = admin
        .json(
            "POST",
            "/admin/seed",
            &json!({"products": 500, "customers": 50, "stock": 100000, "scarce_every": 0, "scarce_stock": 0}),
        )
        .await
        .expect("seed");
    assert_eq!(seeded.status, 200);
    let start = admin
        .get("/admin/invariants")
        .await
        .expect("invariants")
        .json();
    let total_units =
        start["stock_units"].as_u64().unwrap() + start["consumed_units"].as_u64().unwrap();
    println!("seeded, total units {total_units}; kill the server now");
    let mut tasks = Vec::new();
    for index in 0..conns {
        tasks.push(tokio::spawn(async move {
            let mut rng = Rng(0x1234_5678_9abc_def1u64.wrapping_mul(index as u64 + 1) | 1);
            let mut client = Client::connect(write).await.expect("connect");
            let mut acked = Vec::new();
            loop {
                let body = json!({"customer_id": 1 + rng.below(50), "items": [{"product_id": 1 + rng.below(500), "qty": 1 + rng.below(3)}]});
                match client.json("POST", "/orders", &body).await {
                    Ok(response) if response.status == 201 => {
                        acked.push(response.json()["id"].as_u64().unwrap());
                    }
                    _ => return acked,
                }
            }
        }));
    }
    let mut ids = Vec::new();
    for task in tasks {
        ids.extend(task.await.expect("worker"));
    }
    let text = json!({"total_units": total_units, "acked": ids}).to_string();
    std::fs::write(&out, text).expect("write ids");
    println!("server went away after {} acknowledged orders", ids.len());
}

async fn crash_verify(args: Vec<String>) {
    let write = addr(&arg(&args, "--addr").expect("--addr"));
    let input = arg(&args, "--out").expect("--out");
    let saved: Value =
        serde_json::from_slice(&std::fs::read(&input).expect("read ids")).expect("json");
    let mut client = Client::connect(write).await.expect("connect");
    let acked = saved["acked"].as_array().unwrap();
    let mut missing = 0;
    for id in acked {
        let response = client
            .get(&format!("/orders/{}", id.as_u64().unwrap()))
            .await
            .expect("get");
        if response.status != 200 {
            missing += 1;
        }
    }
    let report = client
        .get("/admin/invariants")
        .await
        .expect("invariants")
        .json();
    let stats = client.get("/admin/stats").await.expect("stats").json();
    let units =
        report["stock_units"].as_u64().unwrap() + report["consumed_units"].as_u64().unwrap();
    let orders = stats["orders"].as_u64().unwrap();
    println!(
        "acked={} missing_after_restart={missing} orders_in_db={orders} durable_but_unacked={} units_conserved={}",
        acked.len(),
        orders as i64 - acked.len() as i64,
        units == saved["total_units"].as_u64().unwrap()
    );
    if missing > 0 || units != saved["total_units"].as_u64().unwrap() {
        std::process::exit(1);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        match args.first().map(String::as_str) {
            Some("run") => run(args).await,
            Some("replica-lag") => replica_lag(args).await,
            Some("slow-sse") => slow_sse(args).await,
            Some("crash-writer") => crash_writer(args).await,
            Some("crash-verify") => crash_verify(args).await,
            _ => {
                eprintln!("usage: shop-load run|replica-lag|slow-sse [--addr A] [--read-addr B] [--conns N] [--secs S] [--seed] [--mix 90,8,2] [--label L]");
                std::process::exit(2);
            }
        }
    });
}
