use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use ferro::{Data, Db, Durability, Map, Options, Record, Table};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

const WARM_CACHE: u64 = 1 << 30;
const SMALL_CACHE: u64 = 16 << 20;
const RANGE_KEYS: u64 = 100;
const INDEX_SPAN: i64 = 100;
const BALANCES: u64 = 1_000_000;
const TIER_PAGE: usize = 200;

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Free,
    Pro,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Account {
    #[key]
    id: u64,
    #[index]
    tier: Tier,
    email: String,
    name: String,
    #[index]
    balance: i64,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Bank {
    accounts: Table<Account>,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn account(i: u64) -> Account {
    let seed = mix(i);
    Account {
        id: i,
        tier: if seed.is_multiple_of(3) {
            Tier::Pro
        } else {
            Tier::Free
        },
        email: format!("user{i}@example.com"),
        name: format!("Account Holder {}", seed % 100_000),
        balance: (seed % BALANCES) as i64,
    }
}

#[cfg(target_os = "linux")]
struct Counters(Vec<i32>);

#[cfg(target_os = "linux")]
impl Counters {
    fn open(events: &[(u32, u64)]) -> Counters {
        let mut fds = Vec::new();
        for &(kind, config) in events {
            let mut attr = [0u64; 16];
            attr[0] = u64::from(kind) | 128 << 32;
            attr[1] = config;
            attr[5] = 1 << 5 | 1 << 6;
            let fd =
                unsafe { libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0, -1, -1, 0) };
            fds.push(if fd < 0 { -1 } else { fd as i32 });
        }
        Counters(fds)
    }

    fn read(&self) -> Vec<u64> {
        self.0
            .iter()
            .map(|&fd| {
                if fd < 0 {
                    return 0;
                }
                let mut value = 0u64;
                unsafe { libc::read(fd, (&mut value as *mut u64).cast(), 8) };
                value
            })
            .collect()
    }
}

#[cfg(target_os = "linux")]
impl Drop for Counters {
    fn drop(&mut self) {
        for &fd in &self.0 {
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
struct Counters(usize);

#[cfg(not(target_os = "linux"))]
impl Counters {
    fn open(events: &[(u32, u64)]) -> Counters {
        Counters(events.len())
    }

    fn read(&self) -> Vec<u64> {
        vec![0; self.0]
    }
}

const EVENTS: [(&str, u32, u64); 4] = [
    ("cycles", 0, 0),
    ("instructions", 0, 1),
    ("fills_local_ccx", 4, 0x0243),
    ("fills_other_ccx", 4, 0x0443),
];

#[cfg(target_os = "linux")]
fn thread_cpu_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

#[cfg(not(target_os = "linux"))]
fn thread_cpu_ns() -> u64 {
    0
}

fn busy_ticks() -> (u64, u64) {
    let system = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let fields: Vec<u64> = system
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    let total: u64 = fields.iter().take(8).sum();
    let idle = fields.get(3).copied().unwrap_or(0) + fields.get(4).copied().unwrap_or(0);
    let own = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = own.rsplit(')').next().unwrap_or_default();
    let own: Vec<&str> = after.split_whitespace().collect();
    let field = |index: usize| {
        own.get(index)
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let mine = field(11) + field(12);
    (total.saturating_sub(idle), mine)
}

#[cfg(target_os = "linux")]
fn pin(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set);
    }
}

#[cfg(not(target_os = "linux"))]
fn pin(_cpu: usize) {}

fn cpu_order() -> Vec<usize> {
    let cpus = thread::available_parallelism().map_or(1, |n| n.get());
    let mut order = Vec::with_capacity(cpus);
    let mut seen = std::collections::BTreeSet::new();
    for cpu in 0..cpus {
        let siblings = std::fs::read_to_string(format!(
            "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
        ))
        .unwrap_or_default();
        let first = siblings
            .split([',', '-'])
            .next()
            .and_then(|text| text.trim().parse::<usize>().ok())
            .unwrap_or(cpu);
        if seen.insert(first) {
            order.push(cpu);
        }
    }
    for cpu in 0..cpus {
        if !order.contains(&cpu) {
            order.push(cpu);
        }
    }
    order
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Work {
    Get,
    Scan,
    Each,
    Range,
    Index,
    Tier,
}

impl Work {
    fn name(self) -> &'static str {
        match self {
            Work::Get => "get",
            Work::Scan => "scan",
            Work::Each => "each",
            Work::Range => "range",
            Work::Index => "index",
            Work::Tier => "tier",
        }
    }

    fn parse(name: &str) -> Work {
        match name {
            "get" => Work::Get,
            "scan" => Work::Scan,
            "each" => Work::Each,
            "range" => Work::Range,
            "index" => Work::Index,
            "tier" => Work::Tier,
            other => panic!("unknown workload {other}"),
        }
    }

    fn batch(self) -> u64 {
        match self {
            Work::Get => 256,
            Work::Scan | Work::Each => 1,
            Work::Range | Work::Index | Work::Tier => 16,
        }
    }

    fn unit(self) -> &'static str {
        match self {
            Work::Get => "gets",
            Work::Scan | Work::Each => "keys",
            Work::Range => "ranges",
            Work::Index => "lookups",
            Work::Tier => "pages",
        }
    }
}

struct Dbs {
    map: Db<Map<u64, String>>,
    bank: Db<Bank>,
}

struct Outcome {
    ops: u64,
    seconds: f64,
    counters: Vec<u64>,
    cpu_share: f64,
    foreign: f64,
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn read_batch(work: Work, dbs: &Dbs, n: u64, rng: &mut u64) -> u64 {
    match work {
        Work::Get => {
            let snapshot = dbs.map.read();
            let mut found = 0;
            for _ in 0..work.batch() {
                let key = mix(next(rng) % n);
                found += u64::from(snapshot.get(&key).is_some_and(|value| !value.is_empty()));
            }
            black_box(found);
            work.batch()
        }
        Work::Scan => {
            let snapshot = dbs.map.read();
            let total = snapshot
                .iter()
                .map(|(key, value)| key ^ value.len() as u64)
                .fold(0u64, u64::wrapping_add);
            black_box(total);
            snapshot.len() as u64
        }
        Work::Each => {
            let snapshot = dbs.map.read();
            let mut total = 0u64;
            snapshot.for_each(|key, value| total = total.wrapping_add(key ^ value.len() as u64));
            black_box(total);
            snapshot.len() as u64
        }
        Work::Range => {
            let snapshot = dbs.map.read();
            let width = u64::MAX / n * RANGE_KEYS;
            for _ in 0..work.batch() {
                let start = next(rng);
                let end = start.saturating_add(width);
                let total = snapshot
                    .range(start..end)
                    .map(|(key, value)| key ^ value.len() as u64)
                    .fold(0u64, u64::wrapping_add);
                black_box(total);
            }
            work.batch()
        }
        Work::Index => {
            let snapshot = dbs.bank.read();
            for _ in 0..work.batch() {
                let low = (next(rng) % BALANCES) as i64;
                let total = snapshot
                    .accounts
                    .by_balance_range(low..low + INDEX_SPAN)
                    .map(|row| row.balance)
                    .fold(0i64, i64::wrapping_add);
                black_box(total);
            }
            work.batch()
        }
        Work::Tier => {
            let snapshot = dbs.bank.read();
            for _ in 0..work.batch() {
                let tier = if next(rng).is_multiple_of(2) {
                    Tier::Pro
                } else {
                    Tier::Free
                };
                let total = snapshot
                    .accounts
                    .by_tier(&tier)
                    .take(TIER_PAGE)
                    .map(|row| row.balance)
                    .fold(0i64, i64::wrapping_add);
                black_box(total);
            }
            work.batch()
        }
    }
}

fn write_one(work: Work, dbs: &Dbs, n: u64, rows: u64, rng: &mut u64) {
    match work {
        Work::Index | Work::Tier => {
            let id = next(rng) % rows;
            let balance = (next(rng) % BALANCES) as i64;
            dbs.bank
                .update(|bank| {
                    bank.accounts.update(&id, |row| row.balance = balance);
                })
                .unwrap();
        }
        _ => {
            let i = next(rng) % n;
            let value = format!("value-{}", next(rng) % 1000);
            dbs.map
                .update(|root| {
                    root.insert(mix(i), value);
                })
                .unwrap();
        }
    }
}

struct Cell {
    work: Work,
    threads: usize,
    writers: usize,
}

fn run_cell(
    dbs: &Arc<Dbs>,
    cell: &Cell,
    n: u64,
    rows: u64,
    secs: f64,
    order: &[usize],
    pinned: bool,
) -> Outcome {
    let stop = Arc::new(AtomicBool::new(false));
    let total = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(cell.threads + 1));
    let events: Vec<(u32, u64)> = EVENTS
        .iter()
        .map(|&(_, kind, config)| (kind, config))
        .collect();
    let work = cell.work;
    let readers: Vec<_> = (0..cell.threads)
        .map(|t| {
            let (dbs, stop, total, barrier) =
                (dbs.clone(), stop.clone(), total.clone(), barrier.clone());
            let cpu = order[t % order.len()];
            let events = events.clone();
            thread::spawn(move || {
                if pinned {
                    pin(cpu);
                }
                let mut rng = mix(t as u64 + 1) | 1;
                let counters = Counters::open(&events);
                barrier.wait();
                let before = counters.read();
                let cpu_before = thread_cpu_ns();
                let mut ops = 0;
                while !stop.load(Ordering::Relaxed) {
                    ops += read_batch(work, &dbs, n, &mut rng);
                }
                let cpu = thread_cpu_ns() - cpu_before;
                let after = counters.read();
                total.fetch_add(ops, Ordering::Relaxed);
                let mut deltas: Vec<u64> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
                deltas.push(cpu);
                deltas
            })
        })
        .collect();
    let writers: Vec<_> = (0..cell.writers)
        .map(|t| {
            let (dbs, stop) = (dbs.clone(), stop.clone());
            thread::spawn(move || {
                let mut rng = mix(1000 + t as u64) | 1;
                let mut commits = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    write_one(work, &dbs, n, rows, &mut rng);
                    commits += 1;
                }
                commits
            })
        })
        .collect();
    barrier.wait();
    let start = Instant::now();
    let (busy_before, mine_before) = busy_ticks();
    thread::sleep(Duration::from_secs_f64(secs));
    stop.store(true, Ordering::Relaxed);
    let (busy_after, mine_after) = busy_ticks();
    let mut counters = vec![0u64; EVENTS.len() + 1];
    for reader in readers {
        for (sum, value) in counters.iter_mut().zip(reader.join().unwrap()) {
            *sum += value;
        }
    }
    let seconds = start.elapsed().as_secs_f64();
    let cpu = counters.pop().unwrap_or(0);
    let foreign = (busy_after - busy_before).saturating_sub(mine_after - mine_before) as f64
        / 100.0
        / seconds;
    for writer in writers {
        writer.join().unwrap();
    }
    Outcome {
        ops: total.load(Ordering::Relaxed),
        seconds,
        counters,
        cpu_share: cpu as f64 / 1e9 / seconds / cell.threads as f64,
        foreign,
    }
}

fn build(dir: &Path, n: u64, rows: u64) {
    let map = Options::new()
        .durability(Durability::Buffered)
        .open::<Map<u64, String>>(dir.join("map.ferro"))
        .unwrap();
    map.update(|root| root.extend((0..n).map(|i| (mix(i), format!("value-{i}")))))
        .unwrap();
    drop(map);
    let bank = Options::new()
        .durability(Durability::Buffered)
        .open::<Bank>(dir.join("bank.ferro"))
        .unwrap();
    for chunk in 0..rows.div_ceil(100_000) {
        bank.update(|bank| {
            for i in chunk * 100_000..((chunk + 1) * 100_000).min(rows) {
                bank.accounts.insert(account(i));
            }
        })
        .unwrap();
    }
}

fn open(dir: &Path, cache: u64) -> Dbs {
    let options = || {
        Options::new()
            .durability(Durability::Buffered)
            .cache_bytes(cache)
    };
    Dbs {
        map: options().open(dir.join("map.ferro")).unwrap(),
        bank: options().open(dir.join("bank.ferro")).unwrap(),
    }
}

fn warm(dbs: &Dbs) {
    for _ in 0..2 {
        let map = dbs.map.read();
        black_box(
            map.iter()
                .map(|(key, value)| key ^ value.len() as u64)
                .fold(0u64, u64::wrapping_add),
        );
        let bank = dbs.bank.read();
        black_box(
            bank.accounts
                .iter()
                .map(|row| row.balance)
                .fold(0i64, i64::wrapping_add),
        );
        black_box(
            bank.accounts
                .by_balance_range(0..BALANCES as i64)
                .keys()
                .count(),
        );
        black_box(bank.accounts.by_tier(&Tier::Pro).keys().count());
    }
}

fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn list<T>(name: &str, default: &str, parse: impl Fn(&str) -> T) -> Vec<T> {
    argument(name)
        .unwrap_or_else(|| default.to_string())
        .split(',')
        .map(|item| parse(item.trim()))
        .collect()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    }
}

type Key = (u64, usize, Work, usize);

fn main() {
    let n: u64 = argument("--n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let rows: u64 = argument("--rows")
        .and_then(|v| v.parse().ok())
        .unwrap_or(n / 5);
    let rounds: usize = argument("--rounds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let secs: f64 = argument("--secs")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    let label = argument("--label").unwrap_or_else(|| "run".to_string());
    let threads = list("--threads", "1,2,4,8,16,32", |item| {
        item.parse::<usize>().unwrap()
    });
    let works = list("--work", "get,scan,each,range,index,tier", Work::parse);
    let writers = list("--writers", "0,1", |item| item.parse::<usize>().unwrap());
    let caches = list("--cache", "warm,small", |item| match item {
        "warm" => WARM_CACHE,
        "small" => SMALL_CACHE,
        other => other.parse::<u64>().unwrap(),
    });
    let pinned = !std::env::args().any(|arg| arg == "--no-pin");
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    match argument("--from") {
        Some(golden) => {
            for name in ["map.ferro", "bank.ferro"] {
                std::fs::copy(Path::new(&golden).join(name), dir.path().join(name)).unwrap();
            }
        }
        None => build(dir.path(), n, rows),
    }
    if let Some(golden) = argument("--save") {
        std::fs::create_dir_all(&golden).unwrap();
        for name in ["map.ferro", "bank.ferro"] {
            std::fs::copy(dir.path().join(name), Path::new(&golden).join(name)).unwrap();
        }
        return;
    }
    let order = cpu_order();
    let mut results: BTreeMap<Key, Vec<(f64, Vec<f64>)>> = BTreeMap::new();
    for round in 0..rounds {
        for &cache in &caches {
            let dbs = Arc::new(open(dir.path(), cache));
            warm(&dbs);
            let mut cells = Vec::new();
            for &work in &works {
                for &writer_count in &writers {
                    for &thread_count in &threads {
                        cells.push(Cell {
                            work,
                            threads: thread_count,
                            writers: writer_count,
                        });
                    }
                }
            }
            let shift = round % cells.len().max(1);
            cells.rotate_left(shift);
            if round % 2 == 1 {
                cells.reverse();
            }
            for cell in &cells {
                let outcome = run_cell(&dbs, cell, n, rows, secs, &order, pinned);
                let rate = outcome.ops as f64 / outcome.seconds;
                let mut per_op: Vec<f64> = outcome
                    .counters
                    .iter()
                    .map(|&c| c as f64 / outcome.ops.max(1) as f64)
                    .collect();
                per_op.push(outcome.cpu_share);
                per_op.push(outcome.foreign);
                eprintln!(
                    "round {round} cache {cache} {} writers {} threads {:>2}: {:>14.0} {}/s cpu {:.2} others {:.1}",
                    cell.work.name(),
                    cell.writers,
                    cell.threads,
                    rate,
                    cell.work.unit(),
                    outcome.cpu_share,
                    outcome.foreign
                );
                results
                    .entry((cache, cell.writers, cell.work, cell.threads))
                    .or_default()
                    .push((rate, per_op));
            }
        }
    }
    println!("{label}");
    let mut last: Option<(u64, usize, Work)> = None;
    let mut base = 0.0;
    for ((cache, writer_count, work, thread_count), samples) in &results {
        let group = (*cache, *writer_count, *work);
        let mut rates: Vec<f64> = samples.iter().map(|(rate, _)| *rate).collect();
        let rate = median(&mut rates);
        if last != Some(group) {
            last = Some(group);
            base = rate;
            println!(
                "\n{} cache {} MiB, {} writer(s), {}/s",
                work.name(),
                cache >> 20,
                writer_count,
                work.unit()
            );
            println!(
                "{:>8} {:>14} {:>8} {:>10} {:>8} {:>8} {:>10} {:>10} {:>6} {:>7}",
                "threads",
                "rate",
                "speedup",
                "per-thread",
                "cyc/op",
                "ins/op",
                "localccx",
                "otherccx",
                "cpu",
                "others"
            );
        }
        let per: Vec<f64> = (0..EVENTS.len() + 2)
            .map(|event| {
                let mut values: Vec<f64> =
                    samples.iter().map(|(_, per_op)| per_op[event]).collect();
                median(&mut values)
            })
            .collect();
        println!(
            "{:>8} {:>14.0} {:>8.2} {:>10.0} {:>8.0} {:>8.0} {:>10.2} {:>10.2} {:>6.2} {:>7.1}",
            thread_count,
            rate,
            rate / base,
            rate / *thread_count as f64,
            per[0],
            per[1],
            per[2],
            per[3],
            per[4],
            per[5]
        );
    }
}
