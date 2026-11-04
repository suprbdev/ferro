use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ferro::{Data, Durability, Map, Options, Record, Table};

const READ_SAMPLES: u64 = 500_000;
const COLD_SAMPLES: u64 = 100_000;
const FSYNC_COMMITS: u64 = 1_000;
const BUFFERED_COMMITS: u64 = 20_000;
const WRITERS: u64 = 8;
const WRITER_COMMITS: u64 = 200;
const READERS: u64 = 8;
const COLD_CACHE: u64 = 1 << 20;

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Free,
    Pro,
    Team,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Account {
    #[key]
    id: u64,
    #[index]
    tier: Tier,
    #[index]
    country: String,
    email: String,
    name: String,
    balance: i64,
    tags: Vec<String>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Bank {
    accounts: Table<Account>,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn account(i: u64) -> Account {
    let seed = mix(i);
    Account {
        id: i,
        tier: match seed % 10 {
            0 => Tier::Team,
            1 | 2 => Tier::Pro,
            _ => Tier::Free,
        },
        country: ["nz", "au", "de", "us", "jp", "br", "ca", "fr"][(seed >> 8) as usize % 8]
            .to_string(),
        email: format!("user{i}@example.com"),
        name: format!("Account Holder {}", seed % 100_000),
        balance: (seed % 1_000_000) as i64,
        tags: vec!["beta".to_string(), format!("cohort-{}", seed % 12)],
    }
}

struct Report(BTreeMap<String, f64>);

impl Report {
    fn set(&mut self, name: &str, value: f64) {
        self.0.insert(name.to_string(), value);
    }

    fn timed<R>(&mut self, name: &str, count: u64, f: impl FnOnce() -> R) -> R {
        let start = Instant::now();
        let out = f();
        let nanos = start.elapsed().as_nanos() as f64 / count as f64;
        self.set(&format!("{name}_ns"), nanos);
        out
    }
}

fn map_workload(report: &mut Report, dir: &Path, n: u64) {
    let path = dir.join("map.ferro");
    {
        let db = Options::new().open::<Map<u64, String>>(&path).unwrap();
        report.timed("map_bulk", n, || {
            db.update(|root| {
                for i in 0..n {
                    root.insert(mix(i), format!("value-{i}"));
                }
            })
            .unwrap();
        });
        report.set(
            "map_bulk_bytes_per_key",
            db.size_on_disk() as f64 / n as f64,
        );
        let snapshot = db.read();
        for _ in 0..2 {
            report.timed("map_get_warm", READ_SAMPLES, || {
                for i in 0..READ_SAMPLES {
                    assert!(snapshot.get(&mix(i % n)).is_some());
                }
            });
        }
        report.timed("map_scan", n, || {
            assert_eq!(snapshot.iter().count() as u64, n);
        });
        report.timed("map_range_1k", 1_000_000, || {
            for i in 0..1000u64 {
                assert!(snapshot.range(mix(i)..).take(1000).count() > 0);
            }
        });
        let before = db.size_on_disk();
        report.timed("map_commit_fsync", FSYNC_COMMITS, || {
            for i in 0..FSYNC_COMMITS {
                db.update(|root| {
                    root.insert(mix(n + i), "x".to_string());
                })
                .unwrap();
            }
        });
        let after = db.size_on_disk();
        report.set(
            "map_bytes_per_commit",
            (after - before) as f64 / FSYNC_COMMITS as f64,
        );
        let old = db.at(1).unwrap();
        let new = db.read();
        let changes = old.diff(&new).count() as u64;
        report.timed("map_diff_per_change", changes.max(1), || {
            assert_eq!(old.diff(&new).count() as u64, changes);
        });
    }
    {
        let start = Instant::now();
        let db = Options::new()
            .durability(Durability::Buffered)
            .open::<Map<u64, String>>(&path)
            .unwrap();
        report.set("open_ms", start.elapsed().as_secs_f64() * 1000.0);
        report.timed("map_commit_buffered", BUFFERED_COMMITS, || {
            for i in 0..BUFFERED_COMMITS {
                db.update(|root| {
                    root.insert(mix(2 * n + i), "x".to_string());
                })
                .unwrap();
            }
        });
    }
    cold_gets(
        report,
        "map_get_cold",
        Options::new().read_only(true).cache_bytes(COLD_CACHE),
        &path,
        n,
    );
    cold_gets(
        report,
        "map_get_cold_5m",
        Options::new().read_only(true).cache_bytes(5 << 20),
        &path,
        n,
    );
    let writer = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(COLD_CACHE);
    cold_gets(report, "map_get_cold_writer", writer, &path, n);
}

fn cold_gets(report: &mut Report, name: &str, options: Options, path: &Path, n: u64) {
    let db = options.open::<Map<u64, String>>(path).unwrap();
    let snapshot = db.read();
    report.timed(name, COLD_SAMPLES, || {
        for i in 0..COLD_SAMPLES {
            assert!(snapshot.get(&mix(i % n)).is_some());
        }
    });
}

fn table_workload(report: &mut Report, dir: &Path, n: u64) {
    let path = dir.join("table.ferro");
    let db = Options::new().open::<Bank>(&path).unwrap();
    report.timed("table_bulk", n, || {
        for chunk in 0..n.div_ceil(10_000) {
            db.update(|bank| {
                for i in (chunk * 10_000)..((chunk + 1) * 10_000).min(n) {
                    bank.accounts.insert(account(i));
                }
            })
            .unwrap();
        }
    });
    report.set("table_bytes_per_row", db.size_on_disk() as f64 / n as f64);
    let snapshot = db.read();
    report.timed("table_get", READ_SAMPLES, || {
        for i in 0..READ_SAMPLES {
            assert!(snapshot.accounts.get(&(mix(i) % n)).is_some());
        }
    });
    report.timed("table_scan_filter", n, || {
        let rich = snapshot
            .accounts
            .iter()
            .filter(|row| row.balance > 500_000)
            .count();
        assert!(rich > 0);
    });
    report.timed("table_index_page", 2_000, || {
        for i in 0..2_000u64 {
            let country = ["nz", "au", "de", "us"][i as usize % 4];
            assert_eq!(
                snapshot.accounts.by_country(country).rev().take(20).count(),
                20
            );
        }
    });
    report.timed("table_group_counts", 1_000, || {
        for _ in 0..1_000 {
            assert_eq!(snapshot.accounts.by_country_counts().len(), 8);
        }
    });
    let before = db.size_on_disk();
    report.timed("table_update_fsync", FSYNC_COMMITS, || {
        for i in 0..FSYNC_COMMITS {
            let id = mix(i) % n;
            db.update(|bank| {
                bank.accounts.get_mut(&id).unwrap().balance += 1;
            })
            .unwrap();
        }
    });
    let after = db.size_on_disk();
    report.set(
        "table_bytes_per_update",
        (after - before) as f64 / FSYNC_COMMITS as f64,
    );
}

fn concurrent_workload(report: &mut Report, dir: &Path, n: u64) {
    let db = Options::new()
        .coalesce(std::env::args().any(|arg| arg == "--coalesce"))
        .open::<Map<u64, u64>>(dir.join("concurrent.ferro"))
        .unwrap();
    db.update(|root| {
        for i in 0..n / 10 {
            root.insert(i, i);
        }
    })
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let readers: Vec<_> = (0..READERS)
        .map(|t| {
            let (db, stop, reads) = (db.clone(), stop.clone(), reads.clone());
            thread::spawn(move || {
                let mut i = t;
                while !stop.load(Ordering::Relaxed) {
                    let snapshot = db.read();
                    for _ in 0..256 {
                        i = i.wrapping_add(1);
                        std::hint::black_box(snapshot.get(&(mix(i) % (n / 10))));
                    }
                    reads.fetch_add(256, Ordering::Relaxed);
                }
            })
        })
        .collect();
    let start = Instant::now();
    let writers: Vec<_> = (0..WRITERS)
        .map(|t| {
            let db = db.clone();
            thread::spawn(move || {
                for i in 0..WRITER_COMMITS {
                    db.update(|root| {
                        root.insert(mix(t << 32 | i), i);
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let elapsed: Duration = start.elapsed();
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }
    let commits = (WRITERS * WRITER_COMMITS) as f64;
    report.set("concurrent_commits_per_s", commits / elapsed.as_secs_f64());
    report.set(
        "concurrent_reads_per_s",
        reads.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64(),
    );
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
    let label = argument("--label").unwrap_or_else(|| "run".to_string());
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let mut report = Report(BTreeMap::new());
    if !std::env::args().any(|arg| arg == "--concurrent-only") {
        map_workload(&mut report, dir.path(), n);
        table_workload(&mut report, dir.path(), n / 5);
    }
    concurrent_workload(&mut report, dir.path(), n);
    report.set("peak_rss_mib", peak_rss_mib());
    println!("{label}");
    for (name, value) in &report.0 {
        println!("  {name:<28} {value:>14.1}");
    }
    let body: Vec<String> = report
        .0
        .iter()
        .map(|(k, v)| format!("\"{k}\":{v:.1}"))
        .collect();
    let mut out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(parent.join("results.jsonl"))
        .unwrap();
    writeln!(out, "{{\"label\":\"{label}\",{}}}", body.join(",")).unwrap();
}
