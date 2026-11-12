//! Measures [`Db::verify`] on the file the versioned comparison builds, and the commits after it.
//!
//! `build` writes `verify.ferro` in `--dir` (default `target/bench/verify`) as
//! `benchmarks/compare/src/bin/versioned.rs` does: 100,000 keys in 100 commits of 1,000 with
//! `Durability::Fsync`, then 1,000 single-key commits with `Durability::Fsync` and 1,000 with
//! `Durability::Buffered`, about 46 MiB and 151,000 objects. `verify` opens it read-only with
//! `Options::copy_threads` set to `--threads` (default 0, one per core up to 8) and verifies it
//! `--rounds` times (default 10), printing each round's milliseconds, the user-space instructions
//! per object of every thread, counted through `perf_event_open` on Linux and 0 elsewhere, and the
//! read calls it made, from `syscr` in `/proc/self/io`, then the medians. `typed` does the same
//! with [`Db::verify_typed`]. `stall` copies the file, opens the copy with `Durability::Fsync`,
//! makes 200 single-key commits, then times `--commits` (default 20) more, verifies, and times
//! `--commits` more, printing each commit in milliseconds, for `--rounds` rounds (default 3), then
//! the median and slowest commit before and after verifying, and deletes the copy. `--pause`
//! sleeps that many milliseconds in place of the verify, which shows whether the commits after it
//! wait on the verify or on a disk that went idle.
//!
//! ```sh
//! cargo run --release -p ferro-bench --bin verify -- build
//! cargo run --release -p ferro-bench --bin verify -- verify --rounds 20 --threads 1
//! cargo run --release -p ferro-bench --bin verify -- stall --rounds 5
//! cargo run --release -p ferro-bench --bin verify -- stall --rounds 5 --pause 300
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use ferro::{Db, Durability, Map, Options};

type Kv = Map<u64, String>;

const KEYS: u64 = 100_000;
const BATCH: u64 = 1000;
const COMMITS: u64 = 1000;
const WARM: u64 = 200;

#[cfg(target_os = "linux")]
struct Counter(Option<i32>);

#[cfg(not(target_os = "linux"))]
struct Counter;

#[cfg(not(target_os = "linux"))]
impl Counter {
    fn instructions() -> Counter {
        Counter
    }

    fn read(&self) -> u64 {
        0
    }
}

#[cfg(target_os = "linux")]
impl Counter {
    fn instructions() -> Counter {
        let mut attr = [0u64; 16];
        attr[0] = 128 << 32;
        attr[1] = 1;
        attr[5] = 1 | 1 << 1 | 1 << 5 | 1 << 6;
        // SAFETY: `attr` is an initialized 128-byte `perf_event_attr` whose `size` field says 128,
        // with every byte past the fields set here zero, and it outlives the call, which only
        // reads it. Any argument the kernel rejects makes the call return -1.
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0, -1, -1, 0) };
        if fd < 0 {
            return Counter(None);
        }
        // SAFETY: `fd` is the descriptor `perf_event_open` just returned, checked non-negative
        // above, and `0x2400` is `PERF_EVENT_IOC_ENABLE`, whose argument is an integer the kernel
        // does not dereference.
        unsafe { libc::ioctl(fd as i32, 0x2400, 0) };
        Counter(Some(fd as i32))
    }

    fn read(&self) -> u64 {
        let Some(fd) = self.0 else {
            return 0;
        };
        let mut value = 0u64;
        // SAFETY: the pointer is to `value`, a live, writable `u64`, so all 8 bytes `read` may
        // write land in it, and `fd` came from a successful `perf_event_open` and is never closed.
        unsafe { libc::read(fd, (&mut value as *mut u64).cast(), 8) };
        value
    }
}

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

fn number(name: &str, default: usize) -> usize {
    argument(name).map_or(default, |value| value.parse().unwrap())
}

fn open(path: &Path, durability: Durability) -> Db<Kv> {
    Options::new()
        .durability(durability)
        .open::<Kv>(path)
        .unwrap()
}

fn set(db: &Db<Kv>, k: u64, v: String) {
    db.update(|m| {
        m.insert(k, v);
    })
    .unwrap();
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn build(path: &Path) {
    let _ = fs::remove_file(path);
    let db = open(path, Durability::Fsync);
    for c in 0..KEYS / BATCH {
        db.update(|m| {
            for i in c * BATCH..(c + 1) * BATCH {
                m.insert(mix(i), format!("value-{i}"));
            }
        })
        .unwrap();
    }
    for i in 0..COMMITS {
        set(&db, mix(mix(i ^ (1 << 52)) % KEYS), format!("upd-{i}"));
    }
    drop(db);
    let db = open(path, Durability::Buffered);
    for i in 0..COMMITS {
        set(&db, mix(mix(i ^ (1 << 53)) % KEYS), format!("buf-{i}"));
    }
    drop(db);
    println!(
        "built {} bytes={}",
        path.display(),
        fs::metadata(path).unwrap().len()
    );
}

fn verify(path: &Path, rounds: usize, threads: usize, typed: bool) {
    let db = Options::new()
        .read_only(true)
        .copy_threads(threads)
        .open::<Kv>(path)
        .unwrap();
    let counter = Counter::instructions();
    let mut times = Vec::new();
    let mut per = Vec::new();
    for round in 0..rounds {
        let calls = read_calls();
        let before = counter.read();
        let t = Instant::now();
        let report = match typed {
            true => db.verify_typed(),
            false => db.verify(),
        }
        .unwrap();
        let elapsed = ms(t);
        let instructions = (counter.read() - before) as f64 / report.objects as f64;
        let calls = read_calls() - calls;
        println!(
            "round {round} ms={elapsed:.1} objects={} bytes={} instructions_per={instructions:.0} \
             reads={calls}",
            report.objects, report.bytes
        );
        times.push(elapsed);
        per.push(instructions);
    }
    println!(
        "median ms={:.1} instructions_per={:.0}",
        median(times),
        median(per)
    );
}

fn read_calls() -> u64 {
    fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|io| {
            io.lines()
                .find_map(|line| line.strip_prefix("syscr: "))
                .and_then(|calls| calls.trim().parse().ok())
        })
        .unwrap_or(0)
}

fn timed(db: &Db<Kv>, from: u64, n: u64) -> Vec<f64> {
    (from..from + n)
        .map(|i| {
            let t = Instant::now();
            set(db, mix(i ^ (1 << 55)) % KEYS, format!("stall-{i}"));
            ms(t)
        })
        .collect()
}

fn stall(path: &Path, rounds: usize, commits: u64, pause: Option<u64>) {
    let copy = path.with_extension("stall");
    let mut before = Vec::new();
    let mut after = Vec::new();
    for round in 0..rounds {
        let _ = fs::remove_file(&copy);
        fs::copy(path, &copy).unwrap();
        let db = open(&copy, Durability::Fsync);
        timed(&db, 0, WARM);
        let quiet = timed(&db, WARM, commits);
        let t = Instant::now();
        match pause {
            Some(pause) => thread::sleep(Duration::from_millis(pause)),
            None => drop(db.verify().unwrap()),
        }
        let paused_ms = ms(t);
        let next = timed(&db, WARM + commits, commits);
        let line = |v: &[f64]| {
            v.iter()
                .map(|ms| format!("{ms:.2}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        println!("round {round} paused_ms={paused_ms:.1}");
        println!("  before {}", line(&quiet));
        println!("  after  {}", line(&next));
        before.extend(quiet);
        after.extend(next);
        drop(db);
        fs::remove_file(&copy).unwrap();
    }
    let slowest = |v: &[f64]| v.iter().cloned().fold(0.0, f64::max);
    println!(
        "before median_ms={:.3} max_ms={:.2}; after median_ms={:.3} max_ms={:.2}",
        median(before.clone()),
        slowest(&before),
        median(after.clone()),
        slowest(&after)
    );
}

fn main() {
    let dir = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench/verify".into()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("verify.ferro");
    let command = std::env::args().nth(1).unwrap_or_default();
    let rounds = number("--rounds", 10);
    let threads = number("--threads", 0);
    match command.as_str() {
        "build" => build(&path),
        "verify" => verify(&path, rounds, threads, false),
        "typed" => verify(&path, rounds, threads, true),
        "stall" => stall(
            &path,
            number("--rounds", 3),
            number("--commits", 20) as u64,
            argument("--pause").map(|ms| ms.parse().unwrap()),
        ),
        _ => panic!("the command is build, verify, typed or stall"),
    }
}
