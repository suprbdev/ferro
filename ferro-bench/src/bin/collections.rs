use std::path::Path;
use std::time::Instant;

use ferro::{Db, Durability, Map, Options};

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn open<T: ferro::Data + Default>(dir: &Path, name: &str) -> Db<T> {
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

fn measure<T: ferro::Data>(db: &Db<T>, f: impl FnOnce(&mut T)) -> Cost {
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map_or("all", String::as_str);
    let n: u64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    std::fs::create_dir_all("target/bench").unwrap();
    let dir = tempfile::tempdir_in("target/bench").unwrap();
    match command {
        "range" => range_bench(dir.path(), n),
        "merge" => merge_bench(dir.path(), n),
        _ => {
            println!("{}", mix(0));
            range_bench(dir.path(), n);
        }
    }
}
