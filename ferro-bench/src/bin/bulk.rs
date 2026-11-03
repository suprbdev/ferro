use std::path::PathBuf;
use std::time::Instant;

use ferro::{Durability, Map, Options};

struct Counter(Option<i32>);

impl Counter {
    fn instructions() -> Counter {
        let mut attr = [0u64; 16];
        attr[0] = 128 << 32;
        attr[1] = 1;
        attr[5] = 1 | 1 << 1 | 1 << 5 | 1 << 6;
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0, -1, -1, 0) };
        if fd < 0 {
            return Counter(None);
        }
        unsafe { libc::ioctl(fd as i32, 0x2400, 0) };
        Counter(Some(fd as i32))
    }

    fn read(&self) -> u64 {
        let Some(fd) = self.0 else {
            return 0;
        };
        let mut value = 0u64;
        unsafe { libc::read(fd, (&mut value as *mut u64).cast(), 8) };
        value
    }
}

fn cpu_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

#[derive(Clone, Copy, Default)]
struct Sample {
    wall: f64,
    cpu: u64,
    instructions: u64,
}

impl Sample {
    fn take(counter: &Counter, began: Instant) -> Sample {
        Sample {
            wall: began.elapsed().as_secs_f64(),
            cpu: cpu_ns(),
            instructions: counter.read(),
        }
    }

    fn since(&self, start: &Sample) -> Sample {
        Sample {
            wall: self.wall - start.wall,
            cpu: self.cpu - start.cpu,
            instructions: self.instructions - start.instructions,
        }
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

fn peak_rss_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmHWM:"))
                .and_then(|line| line.split_whitespace().nth(1)?.parse::<f64>().ok())
        })
        .map_or(0.0, |kib| kib / 1024.0)
}

fn print(name: &str, sample: &Sample, per: f64) {
    println!(
        "{name} wall_ms={:.1} cpu_ms={:.1} instructions_per={:.0}",
        sample.wall * 1e3,
        sample.cpu as f64 / 1e6,
        sample.instructions as f64 / per
    );
}

fn main() {
    let n: u64 = argument("--n").and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let split = std::env::args().any(|arg| arg == "--split");
    let sorted = std::env::args().any(|arg| arg == "--sorted");
    let batch = std::env::args().any(|arg| arg == "--extend");
    let key = |i: u64| if sorted { i } else { mix(i) };
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let path = dir.path().join("bulk.ferro");
    let counter = Counter::instructions();
    let db = Options::new()
        .cache_bytes(1 << 30)
        .open::<Map<u64, String>>(&path)
        .unwrap();
    let mut kept = None;
    let mut inside = Sample::default();
    let began = Instant::now();
    let start = Sample::take(&counter, began);
    db.update(|root| {
        let began = Instant::now();
        let start = Sample::take(&counter, began);
        if batch {
            root.extend((0..n).map(|i| (key(i), format!("value-{i}"))));
        } else {
            for i in 0..n {
                root.insert(key(i), format!("value-{i}"));
            }
        }
        inside = Sample::take(&counter, began).since(&start);
        if split {
            kept = Some(root.clone());
        }
    })
    .unwrap();
    let whole = Sample::take(&counter, began).since(&start);
    print("bulk", &whole, n as f64);
    print("insert", &inside, n as f64);
    let rest = Sample {
        wall: whole.wall - inside.wall,
        cpu: whole.cpu - inside.cpu,
        instructions: whole.instructions - inside.instructions,
    };
    if let Some(kept) = kept {
        print("commit", &rest, n as f64);
        let began = Instant::now();
        let start = Sample::take(&counter, began);
        drop(kept);
        print("drop", &Sample::take(&counter, began).since(&start), n as f64);
    } else {
        print("commit_and_drop", &rest, n as f64);
    }
    println!("bytes_per_key={:.2}", db.size_on_disk() as f64 / n as f64);
    let snapshot = db.read();
    let gets = 500_000;
    let began = Instant::now();
    let start = Sample::take(&counter, began);
    for j in 0..gets {
        assert!(snapshot.get(&key(mix(j ^ (1 << 50)) % n)).is_some());
    }
    print("get", &Sample::take(&counter, began).since(&start), gets as f64);
    let began = Instant::now();
    let start = Sample::take(&counter, began);
    assert_eq!(snapshot.iter().count() as u64, n);
    print("scan", &Sample::take(&counter, began).since(&start), n as f64);
    let began = Instant::now();
    let start = Sample::take(&counter, began);
    let mut read = 0;
    for j in 0..1000u64 {
        let from = if sorted { mix(j ^ (1 << 51)) % (n - 1000) } else { mix(j ^ (1 << 51)) >> 1 };
        read += snapshot.range(from..).take(1000).count();
    }
    assert!(read > 990_000);
    print("range", &Sample::take(&counter, began).since(&start), 1000.0);
    drop(snapshot);
    let commits = 2000;
    let began = Instant::now();
    let start = Sample::take(&counter, began);
    for i in 0..commits {
        db.update(|root| {
            root.insert(key(n + i), "x".to_string());
        })
        .unwrap();
    }
    print("commit_fsync", &Sample::take(&counter, began).since(&start), commits as f64);
    drop(db);
    let db = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 30)
        .open::<Map<u64, String>>(&path)
        .unwrap();
    let began = Instant::now();
    let start = Sample::take(&counter, began);
    for i in 0..commits {
        db.update(|root| {
            root.insert(key(2 * n + i), "x".to_string());
        })
        .unwrap();
    }
    print("commit_buffered", &Sample::take(&counter, began).since(&start), commits as f64);
    println!("peak_rss_mib={:.1}", peak_rss_mib());
}
