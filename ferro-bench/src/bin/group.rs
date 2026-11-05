//! Measures single-key commits a second from many writer threads, on one branch or their own.
//!
//! For each count in `--writers` (default `1,8,32`) it opens a fresh file in a temporary directory
//! under `--dir` (default `target/bench`), loads `--keys` keys (default 100,000), and runs that
//! many threads, each committing one random key at a time, for `--secs` seconds (default 3). The
//! writers share the main branch unless `--branches own` gives each after the first a branch of
//! its own. `--durability buffered` switches from `Durability::Fsync` to `Durability::Buffered`,
//! `--coalesce 1` and `--shared 1` turn on those `Options`, `--async 1` commits through
//! `update_async`, awaited on each writer's thread, and `--closure-us` spins for that many
//! microseconds inside every closure (default 0). It prints a line for each count, starting with
//! `--label` (default `run`): closures and versions a second, closures per version, bytes written
//! per closure, and the p50, p99 and slowest commit in microseconds.
//!
//! ```sh
//! cargo run --release -p ferro-bench --bin group -- --writers 1,4,8,16 --durability buffered
//! cargo run --release -p ferro-bench --bin group -- --writers 16 --coalesce 1 --label coalesce
//! ```

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use ferro::{Durability, Map, Options};

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

struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        thread::park();
    }
}

fn spin(work: Duration) {
    let until = Instant::now() + work;
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

fn main() {
    let writers: Vec<u64> = argument("--writers")
        .unwrap_or_else(|| "1,8,32".to_string())
        .split(',')
        .map(|n| n.parse().unwrap())
        .collect();
    let secs: f64 = argument("--secs")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3.0);
    let keys: u64 = argument("--keys")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let work = Duration::from_micros(
        argument("--closure-us")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    );
    let coalesce = argument("--coalesce").is_some_and(|v| v == "1");
    let awaited = argument("--async").is_some_and(|v| v == "1");
    let own = argument("--branches").is_some_and(|v| v == "own");
    let shared = argument("--shared").is_some_and(|v| v == "1");
    let durability = match argument("--durability").as_deref() {
        Some("buffered") => Durability::Buffered,
        _ => Durability::Fsync,
    };
    let label = argument("--label").unwrap_or_else(|| "run".to_string());
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    for count in writers {
        let path = dir.path().join(format!("group-{count}.ferro"));
        let db = Options::new()
            .durability(durability)
            .coalesce(coalesce)
            .shared(shared)
            .open::<Root>(&path)
            .unwrap();
        db.update(|root| {
            for i in 0..keys {
                root.insert(i, format!("value-{i}"));
            }
        })
        .unwrap();
        let lanes: Vec<_> = (0..count)
            .map(|writer| match own && writer > 0 {
                true => db.fork(&format!("writer-{writer}")).unwrap(),
                false => db.clone(),
            })
            .collect();
        let total = |lanes: &[ferro::Db<Root>]| -> u64 {
            match own {
                true => lanes.iter().map(|lane| lane.version()).sum(),
                false => db.version(),
            }
        };
        let (bytes_before, version_before) = (db.size_on_disk(), total(&lanes));
        let stop = Arc::new(AtomicBool::new(false));
        let handles: Vec<_> = (0..count)
            .map(|writer| {
                let (db, stop) = (lanes[writer as usize].clone(), stop.clone());
                thread::spawn(move || {
                    let mut samples = Vec::new();
                    let mut i = writer << 40;
                    while !stop.load(Ordering::Relaxed) {
                        i += 1;
                        let key = mix(i) % keys;
                        let started = Instant::now();
                        let write = |root: &mut Root| {
                            if !work.is_zero() {
                                spin(work);
                            }
                            root.insert(key, format!("v-{i}"));
                        };
                        if awaited {
                            block_on(db.update_async(write)).unwrap();
                        } else {
                            db.update(write).unwrap();
                        }
                        samples.push(started.elapsed().as_nanos() as u64);
                    }
                    samples
                })
            })
            .collect();
        let started = Instant::now();
        thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
        let mut samples = Vec::new();
        for handle in handles {
            samples.extend(handle.join().unwrap());
        }
        let elapsed = started.elapsed().as_secs_f64();
        samples.sort_unstable();
        let closures = samples.len() as f64;
        let grown = (db.size_on_disk() - bytes_before) as f64;
        let versions = (total(&lanes) - version_before) as f64;
        println!(
            "{label} writers={count} closures_per_s={:.0} versions_per_s={:.0} per_version={:.2} bytes_per_closure={:.0} p50={:.0}us p99={:.0}us max={:.0}us",
            closures / elapsed,
            versions / elapsed,
            closures / versions.max(1.0),
            grown / closures,
            percentile(&samples, 0.5),
            percentile(&samples, 0.99),
            percentile(&samples, 1.0),
        );
    }
}
