use std::path::Path;
use std::time::Instant;

use ferro::{Bytes, Data, Db, Durability, Map, Options};

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn open<T: Data + Default>(dir: &Path, name: &str) -> Db<T> {
    Options::new()
        .durability(Durability::Buffered)
        .open::<T>(dir.join(name))
        .unwrap()
}

struct Cost {
    closure_ms: f64,
    commit_ms: f64,
    bytes: u64,
}

fn measure<T: Data>(db: &Db<T>, f: impl FnOnce(&mut T)) -> Cost {
    let before = db.size_on_disk();
    let started = Instant::now();
    let mut closure_ms = 0.0;
    db.update(|root| {
        let inner = Instant::now();
        f(root);
        closure_ms = inner.elapsed().as_secs_f64() * 1e3;
    })
    .unwrap();
    Cost {
        closure_ms,
        commit_ms: started.elapsed().as_secs_f64() * 1e3,
        bytes: db.size_on_disk() - before,
    }
}

fn report(label: &str, cost: &Cost) {
    println!(
        "{label:<44} closure {:>10.2} ms   commit {:>10.2} ms   {:>12} bytes",
        cost.closure_ms, cost.commit_ms, cost.bytes
    );
}

fn seeded(dir: &Path, n: u64) -> Db<Map<u64, String>> {
    let path = dir.join("range.ferro");
    let _ = std::fs::remove_file(&path);
    let db = open::<Map<u64, String>>(dir, "range.ferro");
    db.update(|root| root.extend((0..n).map(|i| (i, format!("value-{i}")))))
        .unwrap();
    db
}

fn range_bench(dir: &Path, n: u64) {
    for percent in [1u64, 10, 50, 90] {
        let low = n / 2 - n * percent / 200;
        let high = low + n * percent / 100;
        let db = seeded(dir, n);
        let cost = measure(&db, |root| {
            root.remove_range(low..high);
        });
        report(&format!("remove_range {percent}% of {n}"), &cost);
        drop(db);
        let db = seeded(dir, n);
        let cost = measure(&db, |root| {
            let doomed: Vec<u64> = root.range(low..high).map(|(key, _)| key).collect();
            for key in doomed {
                root.remove(&key);
            }
        });
        report(&format!("collect then remove {percent}% of {n}"), &cost);
        drop(db);
    }
    let db = seeded(dir, n);
    let cost = measure(&db, |root| {
        let tail = root.split_off(&(n / 2));
        *root = tail;
    });
    report(&format!("split_off half of {n}"), &cost);
    drop(db);
    let db = seeded(dir, n);
    let mut tail = (*db.read()).clone();
    tail.remove_range(..n / 2);
    db.update(|root| {
        root.remove_range(n / 2..);
    })
    .unwrap();
    let cost = measure(&db, |root| root.append(&mut tail));
    report(&format!("append disjoint halves of {n}"), &cost);
}

fn seeded_mixed(dir: &Path, n: u64) -> Db<Map<u64, String>> {
    let path = dir.join("merge.ferro");
    let _ = std::fs::remove_file(&path);
    let db = open::<Map<u64, String>>(dir, "merge.ferro");
    db.update(|root| root.extend((0..n).map(|i| (mix(i), format!("value-{i}")))))
        .unwrap();
    db
}

fn merge_bench(dir: &Path, n: u64) {
    for size in [1u64, 10, 100, 1_000, 10_000, 100_000, 1_000_000] {
        if size > n {
            continue;
        }
        for (mode, offset) in [("update", 0), ("new", n)] {
            let mut items: Vec<(u64, String)> = (0..size)
                .map(|i| (mix(mix(i) % n + offset), format!("fresh-{i}")))
                .collect();
            items.sort();
            items.dedup_by_key(|item| item.0);
            let count = items.len() as f64;
            let db = seeded_mixed(dir, n);
            let mut batch = Some(items.clone());
            let merged = measure(&db, |root| root.extend(batch.take().unwrap()));
            drop(db);
            let db = seeded_mixed(dir, n);
            let mut batch = Some(items);
            let incremental = measure(&db, |root| {
                for (key, value) in batch.take().unwrap() {
                    root.insert(key, value);
                }
            });
            drop(db);
            println!(
                "{mode:<7} batch {size:>8} of {n}: merge {:>9.0} ns/key commit {:>9.0}, insert {:>9.0} ns/key commit {:>9.0}, bytes {} vs {}",
                merged.closure_ms * 1e6 / count,
                merged.commit_ms * 1e6 / count,
                incremental.closure_ms * 1e6 / count,
                incremental.commit_ms * 1e6 / count,
                merged.bytes,
                incremental.bytes,
            );
        }
    }
}

fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    let mut word = seed;
    while out.len() < len {
        word = mix(word);
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn value_bench<T: Data + AsRef<[u8]>>(
    dir: &Path,
    label: &str,
    size: usize,
    count: u64,
    make: impl Fn(Vec<u8>) -> T,
) {
    let path = dir.join("bytes.ferro");
    let _ = std::fs::remove_file(&path);
    let values: Vec<T> = (0..count).map(|i| make(noise(i, size))).collect();
    let db = open::<Map<u64, T>>(dir, "bytes.ferro");
    let start = Instant::now();
    let mut pending = Some(values);
    db.update(|root| {
        root.extend(
            pending
                .take()
                .unwrap()
                .into_iter()
                .enumerate()
                .map(|(i, value)| (i as u64, value)),
        )
    })
    .unwrap();
    let write = start.elapsed().as_secs_f64() * 1e9 / count as f64;
    drop(db);
    let db = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20)
        .open::<Map<u64, T>>(&path)
        .unwrap();
    let snapshot = db.read();
    let mut total = 0usize;
    let start = Instant::now();
    for i in 0..count {
        total += AsRef::<[u8]>::as_ref(&*snapshot.get(&i).unwrap()).len();
    }
    let cold = start.elapsed().as_secs_f64() * 1e9 / count as f64;
    let start = Instant::now();
    for (_, value) in snapshot.iter() {
        total += AsRef::<[u8]>::as_ref(&*value).iter().step_by(4096).map(|b| *b as usize).sum::<usize>();
    }
    let scan = start.elapsed().as_secs_f64() * 1e9 / count as f64;
    std::hint::black_box(total);
    println!(
        "{label:<10} {size:>8} bytes x {count:>6}: commit {write:>12.0} ns/value, cold get {cold:>12.0} ns/value, scan {scan:>12.0} ns/value"
    );
}

fn bytes_bench(dir: &Path) {
    for round in 0..2 {
        println!("round {round}");
        for (size, count) in [(1_024usize, 50_000u64), (64 << 10, 1_000), (1 << 20, 100)] {
            value_bench(dir, "Vec<u8>", size, count, |v| v);
            value_bench(dir, "Bytes", size, count, Bytes::from);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map_or("all", String::as_str);
    let n: u64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    std::fs::create_dir_all("target/bench").unwrap();
    let dir = tempfile::tempdir_in("target/bench").unwrap();
    match command {
        "range" => range_bench(dir.path(), n),
        "merge" => merge_bench(dir.path(), n),
        "bytes" => bytes_bench(dir.path()),
        _ => {
            range_bench(dir.path(), n);
            merge_bench(dir.path(), n);
            bytes_bench(dir.path());
        }
    }
}
