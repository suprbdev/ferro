//! Measures commits of many keys at once into a `Map<u64, String>`, as the versioned comparison's
//! load makes them.
//!
//! Opens a fresh file in a temporary directory under `--dir` (default `target/bench`) with
//! `Durability::Fsync`, or `Durability::Buffered` under `--durability buffered`. `load` commits
//! `--batch` new scrambled keys (default 1,000) at a time with values `value-{i}` until the map
//! holds `--n` (default 100,000), as `benchmarks/compare` does. Then `into` commits `--commits`
//! more batches (default 100) of new keys into the full map, and `overwrite` as many batches that
//! give keys the map already holds the values `value-{i}+`. Each closure inserts key by key, as
//! the comparison does, unless `--extend` makes it call `extend`. For each phase it prints the
//! median wall and process CPU microseconds of a commit, the mean user-space instructions of its
//! closure and of the rest, counted through `perf_event_open` on Linux, and the bytes the file grew
//! by per commit.
//!
//! ```sh
//! make bench-batch ARGS='--durability buffered'
//! cargo run --release -p ferro-bench --bin batch -- --batch 10000 --n 1000000 --commits 20
//! ```

use std::path::PathBuf;
use std::time::Instant;

use ferro::{Db, Durability, Map, Options};

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

#[cfg(not(unix))]
fn cpu_ns() -> u64 {
    0
}

#[cfg(unix)]
fn cpu_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a live, writable `timespec`, the only memory `clock_gettime` writes, and a
    // clock the system lacks only makes the call return an error.
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
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

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

struct Phase {
    wall: Vec<f64>,
    cpu: Vec<f64>,
    inside: u64,
    outside: u64,
    bytes: u64,
}

fn run(
    db: &Db<Map<u64, String>>,
    counter: &Counter,
    batches: impl Iterator<Item = Vec<u64>>,
    insert: bool,
    tag: &str,
) -> Phase {
    let mut phase = Phase {
        wall: Vec::new(),
        cpu: Vec::new(),
        inside: 0,
        outside: 0,
        bytes: 0,
    };
    let size = db.size_on_disk();
    for batch in batches {
        let began = Instant::now();
        let cpu = cpu_ns();
        let start = counter.read();
        let mut closure = 0;
        db.update(|map| {
            let start = counter.read();
            let items = batch.iter().map(|&i| (mix(i), format!("value-{i}{tag}")));
            if insert {
                for (key, value) in items {
                    map.insert(key, value);
                }
            } else {
                map.extend(items);
            }
            closure = counter.read() - start;
        })
        .unwrap();
        let whole = counter.read() - start;
        phase.cpu.push((cpu_ns() - cpu) as f64 / 1e3);
        phase.wall.push(began.elapsed().as_secs_f64() * 1e6);
        phase.inside += closure;
        phase.outside += whole - closure;
    }
    phase.bytes = db.size_on_disk() - size;
    phase
}

fn print(name: &str, mut phase: Phase) {
    let commits = phase.wall.len() as f64;
    println!(
        "{name} commits={} wall_us={:.0} cpu_us={:.0} closure_instructions={:.0} \
         rest_instructions={:.0} bytes_per_commit={:.0}",
        phase.wall.len(),
        median(&mut phase.wall),
        median(&mut phase.cpu),
        phase.inside as f64 / commits,
        phase.outside as f64 / commits,
        phase.bytes as f64 / commits,
    );
}

fn main() {
    let n: u64 = argument("--n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let batch: u64 = argument("--batch")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let commits: u64 = argument("--commits")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let durability = match argument("--durability").as_deref() {
        Some("buffered") => Durability::Buffered,
        _ => Durability::Fsync,
    };
    let insert = !std::env::args().any(|arg| arg == "--extend");
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let path = dir.path().join("batch.ferro");
    let counter = Counter::instructions();
    let db = Options::new()
        .durability(durability)
        .open::<Map<u64, String>>(&path)
        .unwrap();
    let load = run(
        &db,
        &counter,
        (0..n / batch).map(|c| (c * batch..(c + 1) * batch).collect()),
        insert,
        "",
    );
    print("load", load);
    let into = run(
        &db,
        &counter,
        (0..commits).map(|c| (n + c * batch..n + (c + 1) * batch).collect()),
        insert,
        "",
    );
    print("into", into);
    let overwrite = run(
        &db,
        &counter,
        (0..commits).map(|c| {
            (0..batch)
                .map(|j| mix((c * batch + j) ^ (1 << 52)) % n)
                .collect()
        }),
        insert,
        "+",
    );
    print("overwrite", overwrite);
}
