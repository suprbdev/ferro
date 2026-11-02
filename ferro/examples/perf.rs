use std::time::Instant;

use ferro::{Db, Durability, Map, Options};

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn timed<R>(label: &str, count: u64, f: impl FnOnce() -> R) -> R {
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    let per = elapsed.as_nanos() as f64 / count as f64;
    println!(
        "{label:<34} {:>10.1?}  {:>10.0} ns/op  {:>12.0} op/s",
        elapsed,
        per,
        1e9 / per
    );
    out
}

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);
    let dir = match std::env::args().nth(2) {
        Some(parent) => tempfile::tempdir_in(parent).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let path = dir.path().join("perf.ferro");
    let db = Options::new()
        .durability(Durability::Fsync)
        .open::<Map<u64, String>>(&path)
        .unwrap();

    timed("bulk insert, one commit", n, || {
        db.update(|root| {
            for i in 0..n {
                root.insert(mix(i), format!("value-{i}"));
            }
        })
        .unwrap();
    });
    println!("file size {:.1} MiB", db.size_on_disk() as f64 / 1048576.0);

    let snapshot = db.read();
    timed("random get", n, || {
        let mut hits = 0u64;
        for i in 0..n {
            if snapshot.get(&mix(i)).is_some() {
                hits += 1;
            }
        }
        assert_eq!(hits, n);
    });
    timed("full scan", n, || {
        assert_eq!(snapshot.iter().count() as u64, n);
    });
    timed("range 1000 keys, 1000 times", 1_000_000, || {
        for i in 0..1000u64 {
            let start = mix(i);
            let taken = snapshot.range(start..).take(1000).count();
            assert!(taken > 0);
        }
    });

    let commits = 2000u64;
    timed("single-key commit, fsync", commits, || {
        for i in 0..commits {
            db.update(|root| {
                root.insert(mix(i + n), "x".to_string());
            })
            .unwrap();
        }
    });
    drop(snapshot);
    drop(db);

    let db = Options::new()
        .durability(Durability::Buffered)
        .open::<Map<u64, String>>(&path)
        .unwrap();
    let before = db.size_on_disk();
    let commits = 100_000u64;
    timed("single-key commit, no fsync", commits, || {
        for i in 0..commits {
            db.update(|root| {
                root.insert(mix(i + 2 * n), "x".to_string());
            })
            .unwrap();
        }
    });
    println!(
        "bytes per commit {:.0}",
        (db.size_on_disk() - before) as f64 / commits as f64
    );

    let old = db.at(1).unwrap();
    let new = db.read();
    let changes = timed("diff across all commits", commits, || old.diff(&new).count());
    println!("changes {changes}");
    let reopen_start = Instant::now();
    drop(old);
    drop(new);
    drop(db);
    let db = Db::<Map<u64, String>>::open(&path).unwrap();
    println!("reopen {:?}, len {}", reopen_start.elapsed(), db.read().len());
}
