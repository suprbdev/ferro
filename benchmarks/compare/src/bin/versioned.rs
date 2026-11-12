use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use ferro::{Db, Durability, Map, Options, Outcome, Snapshot};

const KEYS: u64 = 100_000;
const BATCH: u64 = 1000;
const COMMITS: u64 = 1000;
const GETS: u64 = 20_000;
const RANGES: u64 = 1000;
const RANGE_LEN: usize = 100;
const MERGE_COMMITS: u64 = 100;
const PULL_COMMITS: u64 = 100;
const ROUNDS: usize = 5;
const ENGINES: [&str; 4] = ["ferro", "immudb", "dolt", "irmin"];

type Kv = Map<u64, String>;

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn key(i: u64) -> u64 {
    mix(i)
}

fn value(i: u64) -> String {
    format!("value-{i}")
}

fn pick(j: u64) -> u64 {
    mix(j ^ (1 << 50)) % KEYS
}

fn range_start(j: u64) -> u64 {
    mix(j ^ (1 << 51)) >> 1
}

fn updated(i: u64) -> u64 {
    mix(i ^ (1 << 52)) % KEYS
}

fn buffered(i: u64) -> u64 {
    mix(i ^ (1 << 53)) % KEYS
}

fn pulled(i: u64) -> u64 {
    mix(i ^ (1 << 54)) % KEYS
}

fn distinct(n: u64, f: fn(u64) -> u64) -> usize {
    (0..n).map(f).collect::<BTreeSet<_>>().len()
}

fn open(path: &Path, durability: Durability) -> Db<Kv> {
    Options::new()
        .durability(durability)
        .open::<Kv>(path)
        .unwrap()
}

fn dir_size(path: &Path) -> u64 {
    fs::read_dir(path)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let meta = e.metadata().unwrap();
            if meta.is_dir() {
                dir_size(&e.path())
            } else {
                meta.len()
            }
        })
        .sum()
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

fn quantile(v: &[f64], q: f64) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[(q * (s.len() - 1) as f64) as usize]
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len() as f64
}

fn set(db: &Db<Kv>, k: u64, v: String) {
    db.update(|m| {
        m.insert(k, v);
    })
    .unwrap();
}

fn keep_ours(ours: &mut Kv, base: &Snapshot<Kv>, theirs: &Snapshot<Kv>) -> (&'static str, usize) {
    ("merge", ours.merge(base, theirs, |c| c.keep_ours()))
}

fn ranges(snap: &Snapshot<Kv>) -> usize {
    (0..RANGES)
        .map(|j| snap.range(range_start(j)..).take(RANGE_LEN).count())
        .sum()
}

fn twice(mut f: impl FnMut()) -> f64 {
    let mut elapsed = 0.0;
    for _ in 0..2 {
        let t = Instant::now();
        f();
        elapsed = ms(t);
    }
    elapsed
}

fn child(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("db.ferro");
    let mut out: BTreeMap<&str, f64> = BTreeMap::new();

    let db = open(&path, Durability::Fsync);
    let mut lat = Vec::new();
    let start = Instant::now();
    for c in 0..KEYS / BATCH {
        let t = Instant::now();
        db.update(|m| {
            for i in c * BATCH..(c + 1) * BATCH {
                m.insert(key(i), value(i));
            }
        })
        .unwrap();
        lat.push(ms(t));
    }
    out.insert("load_s", start.elapsed().as_secs_f64());
    out.insert("batch_ms", quantile(&lat, 0.5));
    let v0 = db.version();
    drop(db);
    let size_load = dir_size(dir);
    out.insert("size_load_mib", size_load as f64 / 1048576.0);

    let t = Instant::now();
    let db = open(&path, Durability::Fsync);
    assert!(db.read().get(&key(0)).is_some());
    out.insert("reopen_ms", ms(t));

    lat.clear();
    for i in 0..COMMITS {
        let t = Instant::now();
        set(&db, key(updated(i)), format!("upd-{i}"));
        lat.push(ms(t));
    }
    let v1 = db.version();
    out.insert("commit_p50_ms", quantile(&lat, 0.5));
    out.insert("commit_p99_ms", quantile(&lat, 0.99));
    out.insert("commit_mean_ms", mean(&lat));
    drop(db);
    out.insert(
        "commit_bytes",
        (dir_size(dir) - size_load) as f64 / COMMITS as f64,
    );

    let db = open(&path, Durability::Buffered);
    lat.clear();
    for i in 0..COMMITS {
        let t = Instant::now();
        set(&db, key(buffered(i)), format!("buf-{i}"));
        lat.push(ms(t));
    }
    out.insert("buffered_p50_ms", quantile(&lat, 0.5));
    out.insert("buffered_mean_ms", mean(&lat));
    drop(db);

    let db = open(&path, Durability::Fsync);
    let elapsed = twice(|| {
        let snap = db.read();
        for j in 0..GETS {
            assert!(snap.get(&key(pick(j))).is_some());
        }
    });
    out.insert("get_us", elapsed * 1e3 / GETS as f64);
    let elapsed = twice(|| {
        let snap = db.read();
        assert!(ranges(&snap) >= RANGES as usize * RANGE_LEN * 99 / 100);
    });
    out.insert("range_us", elapsed * 1e3 / RANGES as f64);
    let elapsed = twice(|| {
        let old = db.at(v0).unwrap();
        for j in 0..GETS {
            let i = pick(j);
            assert_eq!(old.get(&key(i)).as_deref(), Some(&value(i)));
        }
    });
    out.insert("old_get_us", elapsed * 1e3 / GETS as f64);
    let elapsed = twice(|| {
        let old = db.at(v0).unwrap();
        assert!(ranges(&old) >= RANGES as usize * RANGE_LEN * 99 / 100);
    });
    out.insert("old_range_us", elapsed * 1e3 / RANGES as f64);

    let t = Instant::now();
    let changes = db.at(v0).unwrap().diff(&db.at(v1).unwrap()).count();
    out.insert("diff_ms", ms(t));
    assert_eq!(changes, distinct(COMMITS, updated));
    out.insert("diff_changes", changes as f64);

    let t = Instant::now();
    let verified = db.verify().unwrap();
    out.insert("verify_ms", ms(t));
    out.insert("verify_items", verified.objects as f64);
    drop(db);

    let db = open(&path, Durability::Fsync);
    assert!(db.read().get(&key(0)).is_some());
    let t = Instant::now();
    set(&db, key(KEYS - 1), "first".to_string());
    out.insert("first_commit_ms", ms(t));
    let t = Instant::now();
    let branch = db.fork("b").unwrap();
    out.insert("fork_ms", ms(t));
    for j in 0..MERGE_COMMITS {
        set(&branch, key(2 * j), format!("br-{j}"));
    }
    for j in 0..MERGE_COMMITS {
        set(&db, key(2 * j + 1), format!("mn-{j}"));
    }
    let t = Instant::now();
    db.merge_with(&branch, keep_ours).unwrap();
    out.insert("merge_ms", ms(t));
    let snap = db.read();
    assert_eq!(
        snap.get(&key(0)).as_deref().map(String::as_str),
        Some("br-0")
    );
    assert_eq!(
        snap.get(&key(1)).as_deref().map(String::as_str),
        Some("mn-0")
    );
    drop(snap);
    drop(branch);

    let replica = open(&dir.join("replica.ferro"), Durability::Fsync);
    let t = Instant::now();
    let bundle = db.bundle(&replica.have().unwrap()).unwrap();
    out.insert("clone_send_ms", ms(t));
    let got = replica.receive("origin", &bundle, keep_ours).unwrap();
    out.insert("clone_ms", ms(t));
    out.insert("clone_mib", bundle.as_bytes().len() as f64 / 1048576.0);
    assert_eq!(got.outcome, Outcome::FastForward);
    assert_eq!(replica.head_hash(), db.head_hash());

    for j in 0..PULL_COMMITS {
        set(&db, key(pulled(j)), format!("pl-{j}"));
    }
    let t = Instant::now();
    let bundle = db.bundle(&replica.have().unwrap()).unwrap();
    let got = replica.receive("origin", &bundle, keep_ours).unwrap();
    out.insert("pull_ms", ms(t));
    out.insert("pull_kib", bundle.as_bytes().len() as f64 / 1024.0);
    assert_eq!(got.outcome, Outcome::FastForward);
    assert_eq!(replica.head_hash(), db.head_hash());
    drop(replica);
    drop(db);
    out.insert(
        "size_end_mib",
        fs::metadata(&path).unwrap().len() as f64 / 1048576.0,
    );

    let body: Vec<String> = out.iter().map(|(k, v)| format!("{k}={v}")).collect();
    println!("RESULT {}", body.join(" "));
}

fn load_average() -> f64 {
    fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(f64::NAN)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn number(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 10.0 {
        format!("{v:.1}")
    } else if v >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.3}")
    }
}

const ROWS: [(&str, &str, &str); 26] = [
    ("load_s", "load 100k keys, 100 commits of 1000", "s"),
    ("batch_ms", "commit of 1000 keys, median", "ms"),
    ("commit_p50_ms", "single-key commit, durable, p50", "ms"),
    ("commit_p99_ms", "single-key commit, durable, p99", "ms"),
    ("commit_mean_ms", "single-key commit, durable, mean", "ms"),
    (
        "buffered_p50_ms",
        "single-key commit, not synced, p50",
        "ms",
    ),
    (
        "buffered_mean_ms",
        "single-key commit, not synced, mean",
        "ms",
    ),
    ("get_us", "get at latest", "us"),
    ("range_us", "range of 100 keys at latest", "us"),
    ("old_get_us", "get at an old version", "us"),
    ("old_range_us", "range of 100 keys at an old version", "us"),
    ("diff_ms", "diff of versions 1000 commits apart", "ms"),
    ("verify_ms", "verify everything retained", "ms"),
    (
        "verified_get_us",
        "get with inclusion and dual proofs",
        "us",
    ),
    (
        "first_commit_ms",
        "first single-key commit after a reopen",
        "ms",
    ),
    ("fork_ms", "create a branch", "ms"),
    ("merge_ms", "merge 100 commits into 100 others", "ms"),
    ("clone_ms", "clone into an empty database", "ms"),
    ("clone_send_ms", "of which the sending side", "ms"),
    ("clone_mib", "clone bytes", "MiB"),
    ("pull_ms", "pull 100 commits", "ms"),
    ("pull_kib", "pull bytes", "KiB"),
    ("size_load_mib", "size after load", "MiB"),
    ("commit_bytes", "growth per single-key commit", "bytes"),
    ("size_end_mib", "size at the end", "MiB"),
    ("reopen_ms", "reopen and first get", "ms"),
];

fn parent(rounds: usize, images: &BTreeMap<String, String>, engines: &[String]) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/versioned");
    let runs = root.join("runs");
    fs::create_dir_all(&runs).unwrap();
    let meta = fs::metadata(&runs).unwrap();
    let user = format!("{}:{}", meta.uid(), meta.gid());
    let exe = std::env::current_exe().unwrap();
    let mut samples: BTreeMap<String, BTreeMap<String, Vec<f64>>> = BTreeMap::new();
    let mut loads = Vec::new();
    for round in 0..rounds {
        for k in 0..engines.len() {
            let name = &engines[(k + round) % engines.len()];
            let dir = runs.join(format!("{name}-{round}"));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let load = load_average();
            loads.push(load);
            eprintln!("round {round} {name} (load {load:.1})");
            let mut cmd = if name == "ferro" {
                let mut cmd = Command::new(&exe);
                cmd.arg("--child").arg(&dir);
                cmd
            } else {
                let mut cmd = Command::new("docker");
                cmd.args([
                    "run",
                    "--rm",
                    "--memory=16g",
                    "--memory-swap=16g",
                    "--network",
                    "none",
                    "-e",
                    "DOLT_METRICS_DISABLED=1",
                    "-e",
                    "HOME=/runs",
                    "--user",
                    &user,
                    "--entrypoint",
                    &format!("/bench/{name}"),
                    "-v",
                ]);
                cmd.arg(format!("{}:/bench:ro", root.join("bin").display()));
                cmd.arg("-v").arg(format!("{}:/runs", runs.display()));
                cmd.arg(&images[name.as_str()])
                    .arg(format!("/runs/{name}-{round}"));
                cmd
            };
            let out = cmd.output().unwrap();
            assert!(
                out.status.success(),
                "{name} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let text = String::from_utf8(out.stdout).unwrap();
            let line = text.lines().find(|l| l.starts_with("RESULT ")).unwrap();
            eprintln!("  {line}");
            for kv in line["RESULT ".len()..].split(' ') {
                let (k, v) = kv.split_once('=').unwrap();
                samples
                    .entry(name.clone())
                    .or_default()
                    .entry(k.to_string())
                    .or_default()
                    .push(v.parse().unwrap());
            }
            fs::remove_dir_all(&dir).unwrap();
        }
    }
    let cell = |name: &str, k: &str| {
        samples
            .get(name)
            .and_then(|m| m.get(k))
            .map_or("n/a".to_string(), |v| number(median(v.clone())))
    };
    let spread = |name: &str, k: &str| {
        samples
            .get(name)
            .and_then(|m| m.get(k))
            .map_or("n/a".to_string(), |v| {
                let lo = v.iter().cloned().fold(f64::INFINITY, f64::min);
                let hi = v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                format!("{}..{}", number(lo), number(hi))
            })
    };
    let mut table = String::new();
    let mut ranges = String::new();
    let header = format!("| workload | unit | {} |\n", engines.join(" | "));
    let rule = format!("| --- | --- |{}\n", " ---: |".repeat(engines.len()));
    table.push_str(&header);
    table.push_str(&rule);
    ranges.push_str(&header);
    ranges.push_str(&rule);
    for (k, label, unit) in ROWS {
        let cells: Vec<String> = engines.iter().map(|e| cell(e, k)).collect();
        table.push_str(&format!("| {label} | {unit} | {} |\n", cells.join(" | ")));
        let cells: Vec<String> = engines.iter().map(|e| spread(e, k)).collect();
        ranges.push_str(&format!("| {label} | {unit} | {} |\n", cells.join(" | ")));
    }
    let lo = loads.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = loads.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let report = format!(
        "{rounds} rounds, 1-minute load average {lo:.1} to {hi:.1} (median {:.1}) at the start of each run\n\nMedians:\n\n{table}\nMin..max:\n\n{ranges}",
        median(loads.clone())
    );
    println!("{report}");
    fs::write(root.join("table.md"), &report).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--child") {
        child(Path::new(&args[2]));
        return;
    }
    let mut rounds = ROUNDS;
    let mut image = None;
    let mut irmin_image = None;
    let mut engines: Vec<String> = ENGINES.iter().map(|e| e.to_string()).collect();
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--rounds" => rounds = rest.next().unwrap().parse().unwrap(),
            "--image" => image = rest.next().cloned(),
            "--irmin-image" => irmin_image = rest.next().cloned(),
            "--engines" => {
                engines = rest
                    .next()
                    .unwrap()
                    .split(',')
                    .map(str::to_string)
                    .collect()
            }
            _ => panic!("unknown argument {arg}"),
        }
    }
    let mut images = BTreeMap::new();
    for name in &engines {
        let (flag, given) = match name.as_str() {
            "ferro" => continue,
            "immudb" | "dolt" => ("--image", &image),
            "irmin" => ("--irmin-image", &irmin_image),
            _ => panic!("unknown engine {name}"),
        };
        let image = given
            .clone()
            .unwrap_or_else(|| panic!("{flag} names the image the {name} harness runs in"));
        images.insert(name.clone(), image);
    }
    parent(rounds, &images, &engines);
}
