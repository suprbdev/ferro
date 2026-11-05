//! Measures what parked watchers cost a commit, and how soon a commit reaches a watcher it wakes.
//!
//! For each count in `--watchers` (default `0,1000,10000`) it fills a fresh `Durability::Buffered`
//! `Map<u64, u64>` with 100,000 keys, in a temporary directory under `--dir` (default
//! `target/bench`), and parks that many `watch_when` watchers, each on a thread of its own, whose
//! test never passes: it looks up a key that is never written, or with `--test false` returns
//! `false` without reading. It times `--commits` single-key commits (default 5,000) and counts the
//! voluntary context switches of all the process's threads during them, then adds one watcher
//! waiting for a target key and commits that key `--rounds` times (default 300), timing each
//! commit and how long after it began its snapshot reached the watcher. It prints a Markdown table
//! with a row for each count: the mean commit, switches per commit, the mean commit of the target
//! key, and the p50 and p99 delivery, all in microseconds but the switches.
//!
//! ```sh
//! cargo run --release -p ferro-bench --bin watchers -- --watchers 0,1000,10000 --test false
//! ```

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

use ferro::{Db, Durability, Map, Options};

type Root = Map<u64, u64>;

const TARGET: u64 = 2_000_000;

fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn percentile(sorted: &[u64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[index] as f64 / 1000.0
}

fn switches() -> u64 {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    tasks
        .filter_map(Result::ok)
        .filter_map(|task| std::fs::read_to_string(task.path().join("status")).ok())
        .flat_map(|status| {
            status
                .lines()
                .filter(|line| line.starts_with("voluntary_ctxt_switches"))
                .filter_map(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
                .collect::<Vec<_>>()
        })
        .sum()
}

fn put(db: &Db<Root>, key: u64, value: u64) {
    db.update(|map| {
        map.insert(key, value);
    })
    .unwrap();
}

fn main() {
    let counts: Vec<usize> = argument("--watchers")
        .unwrap_or_else(|| "0,1000,10000".to_string())
        .split(',')
        .map(|n| n.parse().unwrap())
        .collect();
    let commits: u64 = argument("--commits")
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000);
    let rounds: u64 = argument("--rounds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let never = argument("--test").is_some_and(|v| v == "false");
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    println!(
        "| Parked watchers | Commit | Switches per commit | Commit with target | Delivery p50 | Delivery p99 |"
    );
    println!("| ---: | ---: | ---: | ---: | ---: | ---: |");
    for watchers in counts {
        let path = dir.path().join(format!("w{watchers}.ferro"));
        let db = Options::new()
            .durability(Durability::Buffered)
            .open::<Root>(&path)
            .unwrap();
        db.update(|map| map.extend((0..100_000u64).map(|key| (key, 0))))
            .unwrap();
        let mut closers = Vec::new();
        let mut threads = Vec::new();
        for n in 0..watchers {
            let key = 1_000_000 + n as u64;
            let watcher = if never {
                db.watch_when(|_| false)
            } else {
                db.watch_when(move |snapshot| snapshot.get(&key).is_some())
            };
            closers.push(watcher.closer());
            threads.push(
                thread::Builder::new()
                    .stack_size(64 << 10)
                    .spawn(move || while watcher.recv().is_ok() {})
                    .unwrap(),
            );
        }
        thread::sleep(Duration::from_millis(300));
        let before = switches();
        let started = Instant::now();
        for n in 0..commits {
            put(&db, n % 100_000, n);
        }
        let commit = started.elapsed() / commits as u32;
        let switched = (switches() - before) as f64 / commits as f64;
        let target = db.watch_when(|snapshot| snapshot.get(&TARGET).is_some());
        closers.push(target.closer());
        let (seen, arrivals) = channel();
        threads.push(
            thread::Builder::new()
                .spawn(move || {
                    while let Ok(snapshot) = target.recv() {
                        let value = *snapshot.get(&TARGET).unwrap();
                        if seen.send((value, Instant::now())).is_err() {
                            return;
                        }
                    }
                })
                .unwrap(),
        );
        thread::sleep(Duration::from_millis(50));
        let mut delays = Vec::new();
        let mut spent = Duration::ZERO;
        for round in 1..=rounds {
            let begun = Instant::now();
            put(&db, TARGET, round);
            spent += begun.elapsed();
            loop {
                let (value, at) = arrivals.recv_timeout(Duration::from_secs(30)).unwrap();
                if value == round {
                    delays.push(at.duration_since(begun).as_nanos() as u64);
                    break;
                }
            }
            thread::sleep(Duration::from_micros(200));
        }
        delays.sort_unstable();
        println!(
            "| {watchers} | {:.1} µs | {switched:.2} | {:.1} µs | {:.1} µs | {:.1} µs |",
            commit.as_nanos() as f64 / 1000.0,
            spent.as_nanos() as f64 / rounds as f64 / 1000.0,
            percentile(&delays, 0.5),
            percentile(&delays, 0.99),
        );
        closers.iter().for_each(|closer| closer.close());
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
    }
}
