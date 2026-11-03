use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chat::minihttp::{Client, SseStream};
use chat::{Config, build, open_db, serve};
use ferro::Durability;
use serde_json::{Value, json};
use tokio::net::TcpListener;

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

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Clone, Debug)]
struct Args {
    addr: Option<SocketAddr>,
    db: Option<PathBuf>,
    rooms: usize,
    users: usize,
    per_user: usize,
    clients: usize,
    posters: usize,
    seconds: f64,
    rate: f64,
    history: u64,
    receipts: f64,
    durability: Durability,
    coalesce: bool,
    seed: u64,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            addr: None,
            db: None,
            rooms: 1000,
            users: 10_000,
            per_user: 5,
            clients: 300,
            posters: 32,
            seconds: 20.0,
            rate: 0.0,
            history: 20,
            receipts: 0.25,
            durability: Durability::Fsync,
            coalesce: false,
            seed: 7,
        }
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: chat-load (--db PATH | --addr HOST:PORT) [--rooms N] [--users N] [--per-user N] \
         [--clients N] [--posters N] [--seconds S] [--rate POSTS_PER_SEC] [--history N] \
         [--receipts FRACTION] [--durability fsync|buffered] [--coalesce] [--seed N]"
    );
    std::process::exit(2)
}

fn parse() -> Args {
    let mut args = Args::default();
    let mut input = std::env::args().skip(1);
    while let Some(flag) = input.next() {
        let mut value = || input.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--addr" => args.addr = Some(value().parse().unwrap_or_else(|_| usage())),
            "--db" => args.db = Some(PathBuf::from(value())),
            "--rooms" => args.rooms = value().parse().unwrap_or_else(|_| usage()),
            "--users" => args.users = value().parse().unwrap_or_else(|_| usage()),
            "--per-user" => args.per_user = value().parse().unwrap_or_else(|_| usage()),
            "--clients" => args.clients = value().parse().unwrap_or_else(|_| usage()),
            "--posters" => args.posters = value().parse().unwrap_or_else(|_| usage()),
            "--seconds" => args.seconds = value().parse().unwrap_or_else(|_| usage()),
            "--rate" => args.rate = value().parse().unwrap_or_else(|_| usage()),
            "--history" => args.history = value().parse().unwrap_or_else(|_| usage()),
            "--receipts" => args.receipts = value().parse().unwrap_or_else(|_| usage()),
            "--seed" => args.seed = value().parse().unwrap_or_else(|_| usage()),
            "--coalesce" => args.coalesce = true,
            "--durability" => {
                args.durability = match value().as_str() {
                    "fsync" => Durability::Fsync,
                    "buffered" => Durability::Buffered,
                    _ => usage(),
                }
            }
            _ => usage(),
        }
    }
    if args.addr.is_none() && args.db.is_none() {
        usage();
    }
    if args.users == 0 || args.rooms == 0 || args.clients > args.users {
        usage();
    }
    args
}

struct Plan {
    rooms_of: Vec<Vec<usize>>,
    members: Vec<Vec<usize>>,
}

fn plan(args: &Args) -> Plan {
    let mut rng = Rng(args.seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut rooms_of = vec![Vec::new(); args.users];
    let mut members = vec![Vec::new(); args.rooms];
    for (room, list) in members.iter_mut().enumerate() {
        let user = room % args.users;
        rooms_of[user].push(room);
        list.push(user);
    }
    for (user, joined) in rooms_of.iter_mut().enumerate() {
        let mut tries = 0;
        while joined.len() < args.per_user.min(args.rooms) && tries < 100 {
            tries += 1;
            let skewed = rng.unit() * rng.unit();
            let room = ((skewed * args.rooms as f64) as usize).min(args.rooms - 1);
            if !joined.contains(&room) {
                joined.push(room);
                members[room].push(user);
            }
        }
    }
    for list in &mut members {
        list.sort_unstable();
    }
    Plan { rooms_of, members }
}

fn percentile(sorted: &[u32], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    f64::from(sorted[index.min(sorted.len() - 1)]) / 1000.0
}

fn summary(name: &str, mut micros: Vec<u32>) -> String {
    micros.sort_unstable();
    format!(
        "{name}: n={} p50={:.2}ms p90={:.2}ms p99={:.2}ms max={:.2}ms",
        micros.len(),
        percentile(&micros, 0.5),
        percentile(&micros, 0.9),
        percentile(&micros, 0.99),
        percentile(&micros, 1.0),
    )
}

#[derive(Default)]
struct Shared {
    sent: Mutex<HashMap<u64, Instant>>,
    deliveries: Mutex<Vec<u32>>,
    expected: AtomicU64,
    delivered: AtomicU64,
    duplicates: AtomicU64,
    frames: AtomicU64,
    receipts: AtomicU64,
    receipt_errors: AtomicU64,
    stop: AtomicU64,
}

async fn send_receipts(
    addr: SocketAddr,
    user: u64,
    shared: Arc<Shared>,
    mut wanted: tokio::sync::mpsc::Receiver<(u64, u64)>,
) {
    let mut writer = Client::connect(addr).await.expect("connect");
    while let Some((room, seq)) = wanted.recv().await {
        if shared.stop.load(Ordering::Relaxed) == 1 {
            return;
        }
        let path = format!("/rooms/{room}/read?seq={seq}");
        match writer.act(user, "POST", &path, None).await {
            Ok(response) if response.status == 200 => {
                shared.receipts.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                shared.receipt_errors.fetch_add(1, Ordering::Relaxed);
                writer = Client::connect(addr).await.expect("reconnect");
            }
        }
    }
}

async fn listen(
    addr: SocketAddr,
    user: u64,
    shared: Arc<Shared>,
    receipts: f64,
    seed: u64,
    ready: tokio::sync::mpsc::Sender<()>,
) {
    let mut rng = Rng(seed | 1);
    let mut stream = SseStream::open(addr, &format!("/users/{user}/events"), &[])
        .await
        .expect("open feed");
    assert_eq!(stream.status, 200);
    let (receipt, wanted) = tokio::sync::mpsc::channel(1);
    tokio::spawn(send_receipts(addr, user, shared.clone(), wanted));
    let mut seen = HashSet::new();
    let mut ready = Some(ready);
    while let Ok(Some(event)) = stream.next().await {
        if event.event == "hello" {
            if let Some(ready) = ready.take() {
                let _ = ready.send(()).await;
            }
            continue;
        }
        if event.event != "changes" {
            continue;
        }
        let now = Instant::now();
        shared.frames.fetch_add(1, Ordering::Relaxed);
        let data: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);
        let mut rooms = Vec::new();
        for change in data["changes"].as_array().into_iter().flatten() {
            if change["type"] != "message" {
                continue;
            }
            let message = &change["message"];
            let nonce = message["text"]
                .as_str()
                .and_then(|text| text.strip_prefix('#'))
                .and_then(|text| text.split_whitespace().next())
                .and_then(|nonce| nonce.parse::<u64>().ok());
            let Some(nonce) = nonce else {
                continue;
            };
            if !seen.insert(nonce) {
                shared.duplicates.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let started = shared.sent.lock().unwrap().get(&nonce).copied();
            if let Some(started) = started {
                let micros = now.duration_since(started).as_micros() as u32;
                shared.deliveries.lock().unwrap().push(micros);
                shared.delivered.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(room) = message["room"].as_u64() {
                rooms.push((room, message["seq"].as_u64().unwrap_or(0)));
            }
        }
        if let Some(&(room, seq)) = rooms.last()
            && rng.unit() < receipts
        {
            let _ = receipt.try_send((room, seq));
        }
    }
}

struct Poster {
    latencies: Vec<u32>,
    errors: u64,
}

const WORDS: [&str; 12] = [
    "ship", "it", "today", "review", "please", "lunch", "build", "green", "flaky", "test", "merge",
    "thanks",
];

struct Posting {
    addr: SocketAddr,
    shared: Arc<Shared>,
    plan: Arc<Plan>,
    connected: Vec<u32>,
    first_user: u64,
    first_room: u64,
    begun: Instant,
    deadline: Instant,
    interval: Option<Duration>,
    posters: usize,
    counter: AtomicU64,
}

async fn post_loop(job: Arc<Posting>, index: usize, seed: u64) -> Poster {
    let mut rng = Rng(seed | 1);
    let mut client = Client::connect(job.addr).await.expect("connect");
    let mut latencies = Vec::new();
    let mut errors = 0;
    let mut next = match job.interval {
        Some(interval) => job.begun + interval.mul_f64(index as f64 / job.posters as f64),
        None => job.begun,
    };
    while Instant::now() < job.deadline {
        if let Some(interval) = job.interval {
            let now = Instant::now();
            if next > now {
                tokio::time::sleep(next - now).await;
            }
            next = next.max(now) + interval;
        }
        let user = rng.below(job.plan.rooms_of.len() as u64) as usize;
        let rooms = &job.plan.rooms_of[user];
        let room = rooms[rng.below(rooms.len() as u64) as usize];
        let nonce = job.counter.fetch_add(1, Ordering::Relaxed);
        let words: Vec<&str> = (0..6)
            .map(|_| WORDS[rng.below(WORDS.len() as u64) as usize])
            .collect();
        let text = format!("#{nonce} {}", words.join(" "));
        let started = Instant::now();
        job.shared.sent.lock().unwrap().insert(nonce, started);
        let response = client
            .act(
                job.first_user + user as u64,
                "POST",
                &format!("/rooms/{}/messages", job.first_room + room as u64),
                Some(&json!({"text": text})),
            )
            .await;
        match response {
            Ok(response) if response.status == 201 => {
                latencies.push(started.elapsed().as_micros() as u32);
                job.shared
                    .expected
                    .fetch_add(u64::from(job.connected[room]), Ordering::Relaxed);
            }
            _ => {
                errors += 1;
                job.shared.sent.lock().unwrap().remove(&nonce);
                client = Client::connect(job.addr).await.expect("reconnect");
            }
        }
    }
    Poster { latencies, errors }
}

async fn health(addr: SocketAddr) -> Value {
    let mut client = Client::connect(addr).await.expect("connect");
    client.get("/health").await.expect("health").json()
}

async fn run(args: Args) {
    let mut server = None;
    let addr = match args.addr {
        Some(addr) => addr,
        None => {
            let path = args.db.clone().unwrap_or_else(|| usage());
            let _ = std::fs::remove_file(&path);
            let config = Config {
                durability: args.durability,
                coalesce: args.coalesce,
                ..Config::default()
            };
            let db = open_db(&path, &config).expect("open db");
            let app = build(db, &config);
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let (stop, wait) = tokio::sync::oneshot::channel::<()>();
            let task = tokio::spawn(async move {
                serve(listener, app, async {
                    let _ = wait.await;
                })
                .await
                .expect("serve");
            });
            server = Some((stop, task));
            addr
        }
    };
    println!(
        "chat-load rooms={} users={} per_user={} clients={} posters={} seconds={} rate={} history={} receipts={} durability={:?} coalesce={}",
        args.rooms,
        args.users,
        args.per_user,
        args.clients,
        args.posters,
        args.seconds,
        args.rate,
        args.history,
        args.receipts,
        args.durability,
        args.coalesce
    );
    let plan = Arc::new(plan(&args));
    let memberships: usize = plan.members.iter().map(Vec::len).sum();
    let busiest = plan.members.iter().map(Vec::len).max().unwrap_or(0);
    let seed_body = json!({
        "users": (0..args.users).map(|user| format!("user{user}")).collect::<Vec<_>>(),
        "rooms": plan.members.iter().enumerate().map(|(room, members)| json!({"name": format!("room{room}"), "members": members})).collect::<Vec<_>>(),
        "messages_per_room": args.history,
    });
    let started = Instant::now();
    let mut client = Client::connect(addr).await.expect("connect");
    let seeded = client
        .json("POST", "/admin/seed", &seed_body)
        .await
        .expect("seed");
    assert_eq!(
        seeded.status,
        201,
        "{}",
        String::from_utf8_lossy(&seeded.body)
    );
    let seeded = seeded.json();
    let first_user = seeded["first_user"].as_u64().expect("first user");
    let first_room = seeded["first_room"].as_u64().expect("first room");
    let before = health(addr).await;
    println!(
        "seeded {} memberships (busiest room {busiest}) and {} messages in {:.2}s, file {:.1} MiB",
        memberships,
        args.history * args.rooms as u64,
        started.elapsed().as_secs_f64(),
        before["file_bytes"].as_f64().unwrap_or(0.0) / (1 << 20) as f64
    );
    let mut rng = Rng(args.seed.wrapping_add(99) | 1);
    let mut chosen = HashSet::new();
    while chosen.len() < args.clients {
        chosen.insert(rng.below(args.users as u64) as usize);
    }
    let mut connected = vec![0u32; args.rooms];
    for &user in &chosen {
        for &room in &plan.rooms_of[user] {
            connected[room] += 1;
        }
    }
    let shared = Arc::new(Shared::default());
    let (ready, mut waiting) = tokio::sync::mpsc::channel(args.clients.max(1));
    let opened = Instant::now();
    let mut listeners = Vec::new();
    let chosen: Vec<usize> = chosen.into_iter().collect();
    for (wave, users) in chosen.chunks(50).enumerate() {
        for (offset, &user) in users.iter().enumerate() {
            listeners.push(tokio::spawn(listen(
                addr,
                first_user + user as u64,
                shared.clone(),
                args.receipts,
                args.seed + (wave * 50 + offset) as u64 * 7919,
                ready.clone(),
            )));
        }
        for _ in users {
            waiting.recv().await;
        }
    }
    drop(ready);
    println!(
        "{} feeds open in {:.2}s, server reports {} live",
        args.clients,
        opened.elapsed().as_secs_f64(),
        health(addr).await["feeds"]
    );
    let interval =
        (args.rate > 0.0).then(|| Duration::from_secs_f64(args.posters as f64 / args.rate));
    let begun = Instant::now();
    let job = Arc::new(Posting {
        addr,
        shared: shared.clone(),
        plan: plan.clone(),
        connected,
        first_user,
        first_room,
        begun,
        deadline: begun + Duration::from_secs_f64(args.seconds),
        interval,
        posters: args.posters.max(1),
        counter: AtomicU64::new(1),
    });
    let mut posters = Vec::new();
    for index in 0..args.posters {
        posters.push(tokio::spawn(post_loop(
            job.clone(),
            index,
            args.seed * 31 + index as u64,
        )));
    }
    let mut latencies = Vec::new();
    let mut errors = 0;
    for poster in posters {
        let poster = poster.await.expect("poster");
        latencies.extend(poster.latencies);
        errors += poster.errors;
    }
    let elapsed = begun.elapsed().as_secs_f64();
    let settle = Instant::now();
    while shared.delivered.load(Ordering::Relaxed) < shared.expected.load(Ordering::Relaxed)
        && settle.elapsed() < Duration::from_secs(15)
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let settled = settle.elapsed().as_secs_f64();
    shared.stop.store(1, Ordering::Relaxed);
    let after = health(addr).await;
    for listener in &listeners {
        listener.abort();
    }
    let deliveries = std::mem::take(&mut *shared.deliveries.lock().unwrap());
    let posts = latencies.len();
    let versions = after["version"].as_u64().unwrap_or(0) - before["version"].as_u64().unwrap_or(0);
    let grown =
        after["file_bytes"].as_u64().unwrap_or(0) - before["file_bytes"].as_u64().unwrap_or(0);
    let expected = shared.expected.load(Ordering::Relaxed);
    let delivered = shared.delivered.load(Ordering::Relaxed);
    let frames = shared.frames.load(Ordering::Relaxed);
    println!(
        "posts={posts} errors={errors} in {elapsed:.2}s: {:.0} posts/s, {versions} versions ({:.0}/s), {} receipts ({} failed)",
        posts as f64 / elapsed,
        versions as f64 / elapsed,
        shared.receipts.load(Ordering::Relaxed),
        shared.receipt_errors.load(Ordering::Relaxed)
    );
    println!("{}", summary("post latency", latencies));
    println!("{}", summary("delivery latency", deliveries));
    println!(
        "deliveries={delivered}/{expected} missing={} duplicates={} frames={frames} ({:.2} messages per frame), settled in {settled:.2}s",
        expected.saturating_sub(delivered),
        shared.duplicates.load(Ordering::Relaxed),
        delivered as f64 / frames.max(1) as f64
    );
    println!(
        "file grew {:.1} MiB, {:.0} bytes per version, server sent {} frames",
        grown as f64 / (1 << 20) as f64,
        grown as f64 / versions.max(1) as f64,
        after["frames"]
    );
    if let Some((stop, task)) = server {
        let _ = stop.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
    }
}

fn main() {
    let args = parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(run(args));
}
