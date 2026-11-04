use std::path::{Path, PathBuf};
use std::time::Instant;

use ferro::{Data, Db, Durability, Map, Options, Summary};

const COMMITS: u64 = 4_000;
const QUERIES: u64 = 20_000;

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn amount(i: u64) -> i64 {
    (mix(i) % 250_000) as i64 - 125_000
}

#[derive(Clone, Copy, Debug, PartialEq, Data)]
#[ferro(transparent)]
struct Sum64(i64);

impl Summary<u64, i64> for Sum64 {
    fn of(_key: &u64, value: &i64) -> Self {
        Sum64(*value)
    }

    fn combine(&self, other: &Self) -> Self {
        Sum64(self.0.wrapping_add(other.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Data)]
#[ferro(transparent)]
struct Sum128(i128);

impl Summary<u64, i64> for Sum128 {
    fn of(_key: &u64, value: &i64) -> Self {
        Sum128(i128::from(*value))
    }

    fn combine(&self, other: &Self) -> Self {
        Sum128(self.0 + other.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Data)]
#[ferro(transparent)]
struct Tuple((u64, i64, i64, i64));

impl Summary<u64, i64> for Tuple {
    fn of(_key: &u64, value: &i64) -> Self {
        Tuple((1, *value, *value, *value))
    }

    fn combine(&self, other: &Self) -> Self {
        let (a, b) = (self.0, other.0);
        Tuple((a.0 + b.0, a.1.wrapping_add(b.1), a.2.min(b.2), a.3.max(b.3)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Data)]
struct Fields {
    count: u64,
    sum: i64,
    min: i64,
    max: i64,
}

impl Summary<u64, i64> for Fields {
    fn of(_key: &u64, value: &i64) -> Self {
        Fields {
            count: 1,
            sum: *value,
            min: *value,
            max: *value,
        }
    }

    fn combine(&self, other: &Self) -> Self {
        Fields {
            count: self.count + other.count,
            sum: self.sum.wrapping_add(other.sum),
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }
}

fn spread(i: u64) -> (u64, u64) {
    let a = mix(i);
    let b = mix(i ^ (1 << 40));
    (a.min(b), a.max(b))
}

fn run<S: Summary<u64, i64>>(
    label: &str,
    dir: &Path,
    n: u64,
    cache: Option<u64>,
    baseline: &mut Option<f64>,
) {
    let path = dir.join(format!("{label}.ferro"));
    let _ = std::fs::remove_file(&path);
    let db: Db<Map<u64, i64, S>> = Options::new()
        .durability(Durability::Buffered)
        .open(&path)
        .unwrap();
    let started = Instant::now();
    db.update(|root| root.extend((0..n).map(|i| (mix(i), amount(i)))))
        .unwrap();
    let bulk_ns = started.elapsed().as_nanos() as f64 / n as f64;
    let bulk = db.size_on_disk() as f64 / n as f64;
    let db = match cache {
        Some(bytes) => {
            drop(db);
            Options::new()
                .durability(Durability::Buffered)
                .cache_bytes(bytes)
                .open(&path)
                .unwrap()
        }
        None => db,
    };
    let before = db.size_on_disk();
    let started = Instant::now();
    for i in 0..COMMITS {
        db.update(|root| {
            root.insert(mix(n + i), amount(n + i));
        })
        .unwrap();
    }
    let insert_ns = started.elapsed().as_nanos() as f64 / COMMITS as f64;
    let inserted = (db.size_on_disk() - before) as f64 / COMMITS as f64;
    let before = db.size_on_disk();
    for i in 0..COMMITS {
        db.update(|root| {
            root.insert(mix(i), amount(i) + 1);
        })
        .unwrap();
    }
    let updated = (db.size_on_disk() - before) as f64 / COMMITS as f64;
    let snapshot = db.read();
    let started = Instant::now();
    let mut found = 0u64;
    for i in 0..QUERIES {
        let (low, high) = spread(i);
        found += u64::from(snapshot.range_summary(low..high).is_some());
    }
    let warm_ns = started.elapsed().as_nanos() as f64 / QUERIES as f64;
    let started = Instant::now();
    found += snapshot.iter().count() as u64;
    let scan_ns = started.elapsed().as_nanos() as f64 / n as f64;
    let started = Instant::now();
    for i in 0..QUERIES {
        found += u64::from(snapshot.get(&mix(i)).is_some());
    }
    let get_ns = started.elapsed().as_nanos() as f64 / QUERIES as f64;
    drop(snapshot);
    drop(db);
    let db: Db<Map<u64, i64, S>> = Options::new()
        .read_only(true)
        .cache_bytes(1 << 20)
        .open(&path)
        .unwrap();
    let snapshot = db.read();
    let started = Instant::now();
    for i in 0..QUERIES {
        let (low, high) = spread(i);
        found += u64::from(snapshot.range_summary(low..high).is_some());
    }
    let cold_ns = started.elapsed().as_nanos() as f64 / QUERIES as f64;
    assert!(found > 0);
    let growth = match baseline {
        Some(plain) => format!("{:+6.1}%", (inserted / *plain - 1.0) * 100.0),
        None => {
            *baseline = Some(inserted);
            "       ".to_string()
        }
    };
    println!(
        "{label:<8} bulk {bulk:6.1} B/key {:6.0} ns/key | insert {inserted:7.0} B/commit {growth} {:6.1} us | update {updated:7.0} B/commit | fold warm {:7.2} us cold {:7.2} us | scan {scan_ns:5.1} ns/key get {get_ns:6.0} ns",
        bulk_ns,
        insert_ns / 1e3,
        warm_ns / 1e3,
        cold_ns / 1e3,
    );
    let _ = std::fs::remove_file(&path);
}

fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn main() {
    let n: u64 = argument("--n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let dir = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&dir).unwrap();
    let cache = argument("--cache").and_then(|v| v.parse().ok());
    let mut baseline = None;
    run::<()>("plain", &dir, n, cache, &mut baseline);
    run::<Sum64>("sum64", &dir, n, cache, &mut baseline);
    run::<Sum128>("sum128", &dir, n, cache, &mut baseline);
    run::<Tuple>("tuple4", &dir, n, cache, &mut baseline);
    run::<Fields>("fields4", &dir, n, cache, &mut baseline);
}
