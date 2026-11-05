//! Compares commits from several processes sharing one file with the same writers as threads.
//!
//! For each count in `--processes` (default `1,2,4,8`) it loads `--keys` keys (default 100,000)
//! into a fresh file opened with `Options::shared`, in a temporary directory under `--dir`
//! (default `target/bench`), and starts that many copies of itself, each running `--threads`
//! writer threads (default 1) that commit one random key at a time for `--secs` seconds (default
//! 3). It then runs the same writers as threads of this one process on a second file opened the
//! same way. The writers share the main branch unless `--branches own` gives each process after
//! the first a branch of its own, `--durability buffered` switches from `Durability::Fsync` to
//! `Durability::Buffered`, and `--closure-us` spins for that many microseconds inside every
//! closure (default 0). For each count it prints a `processes` and a `threads` line, starting
//! with `--label` (default `run`), of commits a second and p50 and p99 commit microseconds. The
//! `--child`, `--process` and `--branch` flags are for the copies it starts.
//!
//! ```sh
//! cargo run --release -p ferro-bench --bin processes -- --processes 1,2,4 --threads 2
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ferro::{Db, Durability, Map, Options};

type Root = Map<u64, String>;

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

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

fn spin(work: Duration) {
    let until = Instant::now() + work;
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

#[derive(Clone, Copy)]
struct Run {
    durability: Durability,
    shared: bool,
    secs: f64,
    keys: u64,
    work: Duration,
    threads: u64,
}

fn options(run: &Run) -> Options {
    Options::new().durability(run.durability).shared(run.shared)
}

fn write(db: &Db<Root>, run: &Run, writer: u64, stop: &AtomicBool) -> Vec<u64> {
    let mut samples = Vec::new();
    let mut i = writer << 40;
    while !stop.load(Ordering::Relaxed) {
        i += 1;
        let key = mix(i) % run.keys;
        let started = Instant::now();
        db.update(|root| {
            if !run.work.is_zero() {
                spin(run.work);
            }
            root.insert(key, format!("v-{i}"));
        })
        .unwrap();
        samples.push(started.elapsed().as_nanos() as u64);
    }
    samples
}

fn threads(db: &Db<Root>, run: &Run, first: u64) -> Vec<u64> {
    let stop = Arc::new(AtomicBool::new(false));
    let handles: Vec<_> = (0..run.threads)
        .map(|thread| {
            let db = db.clone();
            let stop = stop.clone();
            let run = *run;
            thread::spawn(move || write(&db, &run, first + thread, &stop))
        })
        .collect();
    thread::sleep(Duration::from_secs_f64(run.secs));
    stop.store(true, Ordering::Relaxed);
    handles
        .into_iter()
        .flat_map(|handle| handle.join().unwrap())
        .collect()
}

fn child(path: &Path, run: &Run) {
    let process: u64 = argument("--process").unwrap().parse().unwrap();
    let branch = argument("--branch").unwrap();
    let db = options(run).open::<Root>(path).unwrap();
    let db = match branch.as_str() {
        "main" => db,
        name => db.branch(name).unwrap(),
    };
    let started = Instant::now();
    let samples = threads(&db, run, process << 16);
    let elapsed = started.elapsed().as_secs_f64();
    let line: Vec<String> = samples.iter().map(u64::to_string).collect();
    println!("elapsed {elapsed}");
    println!("samples {}", line.join(","));
}

fn report(label: &str, mode: &str, count: u64, run: &Run, samples: &mut [u64], rate: f64) {
    samples.sort_unstable();
    println!(
        "{label} {mode} processes={count} threads={} commits_per_s={:.0} p50={:.0}us p99={:.0}us",
        run.threads,
        rate,
        percentile(samples, 0.5),
        percentile(samples, 0.99),
    );
}

fn main() {
    let durability = match argument("--durability").as_deref() {
        Some("buffered") => Durability::Buffered,
        _ => Durability::Fsync,
    };
    let run = Run {
        durability,
        shared: true,
        secs: argument("--secs")
            .and_then(|v| v.parse().ok())
            .unwrap_or(3.0),
        keys: argument("--keys")
            .and_then(|v| v.parse().ok())
            .unwrap_or(100_000),
        work: Duration::from_micros(
            argument("--closure-us")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        ),
        threads: argument("--threads")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1),
    };
    if let Some(path) = argument("--child") {
        child(Path::new(&path), &run);
        return;
    }
    let counts: Vec<u64> = argument("--processes")
        .unwrap_or_else(|| "1,2,4,8".to_string())
        .split(',')
        .map(|n| n.parse().unwrap())
        .collect();
    let own = argument("--branches").is_some_and(|v| v == "own");
    let label = argument("--label").unwrap_or_else(|| "run".to_string());
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let exe = std::env::current_exe().unwrap();
    for count in counts {
        let path = dir.path().join(format!("processes-{count}.ferro"));
        let names: Vec<String> = (0..count)
            .map(|process| match own && process > 0 {
                true => format!("writer-{process}"),
                false => "main".to_string(),
            })
            .collect();
        {
            let db = options(&run).open::<Root>(&path).unwrap();
            db.update(|root| {
                for i in 0..run.keys {
                    root.insert(i, format!("value-{i}"));
                }
            })
            .unwrap();
            for name in names.iter().filter(|name| *name != "main") {
                db.fork(name).unwrap();
            }
        }
        let children: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(process, name)| {
                let mut command = Command::new(&exe);
                command
                    .args(std::env::args().skip(1))
                    .args(["--child", path.to_str().unwrap()])
                    .args(["--process", &process.to_string()])
                    .args(["--branch", name])
                    .stdout(Stdio::piped());
                command.spawn().unwrap()
            })
            .collect();
        let mut samples = Vec::new();
        let mut rate = 0.0;
        for child in children {
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            let text = String::from_utf8(out.stdout).unwrap();
            let elapsed: f64 = text
                .lines()
                .find_map(|line| line.strip_prefix("elapsed "))
                .unwrap()
                .parse()
                .unwrap();
            let line = text.lines().find_map(|line| line.strip_prefix("samples "));
            let before = samples.len();
            samples.extend(
                line.unwrap()
                    .split(',')
                    .filter(|value| !value.is_empty())
                    .map(|value| value.parse::<u64>().unwrap()),
            );
            rate += (samples.len() - before) as f64 / elapsed;
        }
        report(&label, "processes", count, &run, &mut samples, rate);
        let path = dir.path().join(format!("threads-{count}.ferro"));
        let db = options(&run).open::<Root>(&path).unwrap();
        db.update(|root| {
            for i in 0..run.keys {
                root.insert(i, format!("value-{i}"));
            }
        })
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let handles: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(process, name)| {
                let db = match name.as_str() {
                    "main" => db.clone(),
                    name => db.fork(name).unwrap(),
                };
                let stop = stop.clone();
                thread::spawn(move || {
                    let inner = Run { threads: 1, ..run };
                    let mut all = Vec::new();
                    let per: Vec<_> = (0..run.threads)
                        .map(|thread| {
                            let db = db.clone();
                            let stop = stop.clone();
                            let writer = (process as u64) << 16 | thread;
                            thread::spawn(move || write(&db, &inner, writer, &stop))
                        })
                        .collect();
                    for handle in per {
                        all.extend(handle.join().unwrap());
                    }
                    all
                })
            })
            .collect();
        let started = Instant::now();
        thread::sleep(Duration::from_secs_f64(run.secs));
        stop.store(true, Ordering::Relaxed);
        let mut samples = Vec::new();
        for handle in handles {
            samples.extend(handle.join().unwrap());
        }
        let elapsed = started.elapsed().as_secs_f64();
        let rate = samples.len() as f64 / elapsed;
        report(&label, "threads", count, &run, &mut samples, rate);
    }
}
