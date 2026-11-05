use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

const CACHE: u64 = 1 << 30;
const GETS: u64 = 500_000;
const RANGES: u64 = 1000;
const RANGE_LEN: usize = 1000;
const COMMITS: u64 = 1000;
const WRITERS: u64 = 8;
const WRITER_COMMITS: u64 = 200;
const READERS: u64 = 8;
const ROUNDS: usize = 3;
const ENGINES: [&str; 7] = [
    "ferro",
    "ferro-extend",
    "redb",
    "sqlite-full",
    "sqlite-normal",
    "sled",
    "fjall",
];

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

fn pick(j: u64, n: u64) -> u64 {
    key(mix(j ^ (1 << 50)) % n)
}

fn range_start(j: u64) -> u64 {
    mix(j ^ (1 << 51)) >> 1
}

fn commit_key(i: u64, n: u64) -> u64 {
    key(n + i)
}

fn writer_key(t: u64, i: u64) -> u64 {
    mix(1 << 40 | t << 32 | i)
}

trait Engine: Sync {
    fn bulk(&self, n: u64);
    fn gets(&self, n: u64, count: u64) -> u64;
    fn scan(&self) -> u64;
    fn ranges(&self, count: u64, len: usize) -> u64;
    fn commits(&self, n: u64, count: u64);
    fn write_burst(&self, t: u64, count: u64);
    fn read_until(&self, t: u64, n: u64, stop: &AtomicBool) -> u64;
}

mod ferro_engine {
    use super::*;
    use ferro::{Db, Map, Options};

    pub struct Ferro(Db<Map<u64, String>>, bool);

    pub fn open(dir: &Path, extend: bool) -> Ferro {
        Ferro(
            Options::new()
                .cache_bytes(CACHE)
                .open::<Map<u64, String>>(dir.join("db.ferro"))
                .unwrap(),
            extend,
        )
    }

    impl Engine for Ferro {
        fn bulk(&self, n: u64) {
            self.0
                .update(|root| {
                    if self.1 {
                        root.extend((0..n).map(|i| (key(i), value(i))));
                    } else {
                        for i in 0..n {
                            root.insert(key(i), value(i));
                        }
                    }
                })
                .unwrap();
        }
        fn gets(&self, n: u64, count: u64) -> u64 {
            let snap = self.0.read();
            (0..count).filter(|&j| snap.get(&pick(j, n)).is_some()).count() as u64
        }
        fn scan(&self) -> u64 {
            self.0.read().iter().count() as u64
        }
        fn ranges(&self, count: u64, len: usize) -> u64 {
            let snap = self.0.read();
            (0..count)
                .map(|j| snap.range(range_start(j)..).take(len).count() as u64)
                .sum()
        }
        fn commits(&self, n: u64, count: u64) {
            for i in 0..count {
                self.0
                    .update(|root| {
                        root.insert(commit_key(i, n), "x".to_string());
                    })
                    .unwrap();
            }
        }
        fn write_burst(&self, t: u64, count: u64) {
            for i in 0..count {
                self.0
                    .update(|root| {
                        root.insert(writer_key(t, i), "x".to_string());
                    })
                    .unwrap();
            }
        }
        fn read_until(&self, t: u64, n: u64, stop: &AtomicBool) -> u64 {
            let mut reads = 0;
            let mut j = t << 32;
            while !stop.load(Ordering::Relaxed) {
                let snap = self.0.read();
                for _ in 0..256 {
                    j += 1;
                    std::hint::black_box(snap.get(&pick(j, n)));
                }
                reads += 256;
            }
            reads
        }
    }
}

mod redb_engine {
    use super::*;
    use redb::{Database, ReadableTable, TableDefinition};

    const T: TableDefinition<u64, &str> = TableDefinition::new("kv");

    pub struct Redb(Database);

    pub fn open(dir: &Path) -> Redb {
        Redb(
            Database::builder()
                .set_cache_size(CACHE as usize)
                .create(dir.join("db.redb"))
                .unwrap(),
        )
    }

    impl Engine for Redb {
        fn bulk(&self, n: u64) {
            let w = self.0.begin_write().unwrap();
            {
                let mut t = w.open_table(T).unwrap();
                for i in 0..n {
                    t.insert(key(i), value(i).as_str()).unwrap();
                }
            }
            w.commit().unwrap();
        }
        fn gets(&self, n: u64, count: u64) -> u64 {
            let r = self.0.begin_read().unwrap();
            let t = r.open_table(T).unwrap();
            (0..count)
                .filter(|&j| t.get(pick(j, n)).unwrap().is_some())
                .count() as u64
        }
        fn scan(&self) -> u64 {
            let r = self.0.begin_read().unwrap();
            let t = r.open_table(T).unwrap();
            t.iter().unwrap().map(|e| e.unwrap()).count() as u64
        }
        fn ranges(&self, count: u64, len: usize) -> u64 {
            let r = self.0.begin_read().unwrap();
            let t = r.open_table(T).unwrap();
            (0..count)
                .map(|j| {
                    t.range(range_start(j)..)
                        .unwrap()
                        .take(len)
                        .map(|e| e.unwrap())
                        .count() as u64
                })
                .sum()
        }
        fn commits(&self, n: u64, count: u64) {
            for i in 0..count {
                let w = self.0.begin_write().unwrap();
                {
                    let mut t = w.open_table(T).unwrap();
                    t.insert(commit_key(i, n), "x").unwrap();
                }
                w.commit().unwrap();
            }
        }
        fn write_burst(&self, t: u64, count: u64) {
            for i in 0..count {
                let w = self.0.begin_write().unwrap();
                {
                    let mut tb = w.open_table(T).unwrap();
                    tb.insert(writer_key(t, i), "x").unwrap();
                }
                w.commit().unwrap();
            }
        }
        fn read_until(&self, t: u64, n: u64, stop: &AtomicBool) -> u64 {
            let mut reads = 0;
            let mut j = t << 32;
            while !stop.load(Ordering::Relaxed) {
                let r = self.0.begin_read().unwrap();
                let tb = r.open_table(T).unwrap();
                for _ in 0..256 {
                    j += 1;
                    std::hint::black_box(tb.get(pick(j, n)).unwrap());
                }
                reads += 256;
            }
            reads
        }
    }
}

mod sqlite_engine {
    use super::*;
    use rusqlite::Connection;

    pub struct Sqlite {
        path: PathBuf,
        sync: &'static str,
        main: Mutex<Connection>,
    }

    fn bias(k: u64) -> i64 {
        (k ^ (1 << 63)) as i64
    }

    fn connect(path: &Path, sync: &str) -> Connection {
        let c = Connection::open(path).unwrap();
        c.busy_timeout(std::time::Duration::from_secs(600)).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.pragma_update(None, "synchronous", sync).unwrap();
        c.pragma_update(None, "cache_size", -((CACHE / 1024) as i64))
            .unwrap();
        c
    }

    pub fn open(dir: &Path, sync: &'static str) -> Sqlite {
        let path = dir.join("db.sqlite");
        let c = connect(&path, sync);
        c.execute(
            "CREATE TABLE IF NOT EXISTS kv (k INTEGER PRIMARY KEY, v TEXT NOT NULL)",
            [],
        )
        .unwrap();
        Sqlite {
            path,
            sync,
            main: Mutex::new(c),
        }
    }

    impl Engine for Sqlite {
        fn bulk(&self, n: u64) {
            let mut c = self.main.lock().unwrap();
            let tx = c.transaction().unwrap();
            {
                let mut st = tx.prepare("INSERT INTO kv (k, v) VALUES (?1, ?2)").unwrap();
                for i in 0..n {
                    st.execute((bias(key(i)), value(i))).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        fn gets(&self, n: u64, count: u64) -> u64 {
            let c = self.main.lock().unwrap();
            let mut st = c.prepare("SELECT v FROM kv WHERE k = ?1").unwrap();
            (0..count)
                .filter(|&j| {
                    st.query_row([bias(pick(j, n))], |r| r.get::<_, String>(0))
                        .is_ok()
                })
                .count() as u64
        }
        fn scan(&self) -> u64 {
            let c = self.main.lock().unwrap();
            let mut st = c.prepare("SELECT k, v FROM kv ORDER BY k").unwrap();
            let mut rows = st.query([]).unwrap();
            let mut count = 0;
            while let Some(r) = rows.next().unwrap() {
                std::hint::black_box((r.get_ref(0).unwrap(), r.get_ref(1).unwrap()));
                count += 1;
            }
            count
        }
        fn ranges(&self, count: u64, len: usize) -> u64 {
            let c = self.main.lock().unwrap();
            let mut st = c
                .prepare("SELECT k, v FROM kv WHERE k >= ?1 ORDER BY k LIMIT ?2")
                .unwrap();
            let mut total = 0;
            for j in 0..count {
                let mut rows = st.query((bias(range_start(j)), len as i64)).unwrap();
                while let Some(r) = rows.next().unwrap() {
                    std::hint::black_box((r.get_ref(0).unwrap(), r.get_ref(1).unwrap()));
                    total += 1;
                }
            }
            total
        }
        fn commits(&self, n: u64, count: u64) {
            let c = self.main.lock().unwrap();
            let mut st = c
                .prepare("INSERT OR REPLACE INTO kv (k, v) VALUES (?1, 'x')")
                .unwrap();
            for i in 0..count {
                st.execute([bias(commit_key(i, n))]).unwrap();
            }
        }
        fn write_burst(&self, t: u64, count: u64) {
            let c = connect(&self.path, self.sync);
            let mut st = c
                .prepare("INSERT OR REPLACE INTO kv (k, v) VALUES (?1, 'x')")
                .unwrap();
            for i in 0..count {
                st.execute([bias(writer_key(t, i))]).unwrap();
            }
        }
        fn read_until(&self, t: u64, n: u64, stop: &AtomicBool) -> u64 {
            let c = connect(&self.path, self.sync);
            let mut st = c.prepare("SELECT v FROM kv WHERE k = ?1").unwrap();
            let mut reads = 0;
            let mut j = t << 32;
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..256 {
                    j += 1;
                    std::hint::black_box(
                        st.query_row([bias(pick(j, n))], |r| r.get::<_, String>(0))
                            .ok(),
                    );
                }
                reads += 256;
            }
            reads
        }
    }
}

mod sled_engine {
    use super::*;

    pub struct Sled(sled::Db);

    pub fn open(dir: &Path) -> Sled {
        Sled(
            sled::Config::new()
                .path(dir.join("db.sled"))
                .cache_capacity(CACHE)
                .open()
                .unwrap(),
        )
    }

    impl Engine for Sled {
        fn bulk(&self, n: u64) {
            let mut batch = sled::Batch::default();
            for i in 0..n {
                batch.insert(&key(i).to_be_bytes(), value(i).as_bytes());
            }
            self.0.apply_batch(batch).unwrap();
            self.0.flush().unwrap();
        }
        fn gets(&self, n: u64, count: u64) -> u64 {
            (0..count)
                .filter(|&j| self.0.get(pick(j, n).to_be_bytes()).unwrap().is_some())
                .count() as u64
        }
        fn scan(&self) -> u64 {
            self.0.iter().map(|e| e.unwrap()).count() as u64
        }
        fn ranges(&self, count: u64, len: usize) -> u64 {
            (0..count)
                .map(|j| {
                    self.0
                        .range(range_start(j).to_be_bytes()..)
                        .take(len)
                        .map(|e| e.unwrap())
                        .count() as u64
                })
                .sum()
        }
        fn commits(&self, n: u64, count: u64) {
            for i in 0..count {
                self.0.insert(commit_key(i, n).to_be_bytes(), "x").unwrap();
                self.0.flush().unwrap();
            }
        }
        fn write_burst(&self, t: u64, count: u64) {
            for i in 0..count {
                self.0.insert(writer_key(t, i).to_be_bytes(), "x").unwrap();
                self.0.flush().unwrap();
            }
        }
        fn read_until(&self, t: u64, n: u64, stop: &AtomicBool) -> u64 {
            let mut reads = 0;
            let mut j = t << 32;
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..256 {
                    j += 1;
                    std::hint::black_box(self.0.get(pick(j, n).to_be_bytes()).unwrap());
                }
                reads += 256;
            }
            reads
        }
    }
}

mod fjall_engine {
    use super::*;
    use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle, PersistMode};

    pub struct Fjall(Keyspace, PartitionHandle);

    pub fn open(dir: &Path) -> Fjall {
        let ks = Config::new(dir.join("db.fjall"))
            .cache_size(CACHE)
            .open()
            .unwrap();
        let p = ks
            .open_partition("kv", PartitionCreateOptions::default())
            .unwrap();
        Fjall(ks, p)
    }

    impl Engine for Fjall {
        fn bulk(&self, n: u64) {
            let mut b = self.0.batch();
            for i in 0..n {
                b.insert(&self.1, key(i).to_be_bytes(), value(i));
            }
            b.commit().unwrap();
            self.0.persist(PersistMode::SyncAll).unwrap();
        }
        fn gets(&self, n: u64, count: u64) -> u64 {
            (0..count)
                .filter(|&j| self.1.get(pick(j, n).to_be_bytes()).unwrap().is_some())
                .count() as u64
        }
        fn scan(&self) -> u64 {
            self.1.iter().map(|e| e.unwrap()).count() as u64
        }
        fn ranges(&self, count: u64, len: usize) -> u64 {
            (0..count)
                .map(|j| {
                    self.1
                        .range(range_start(j).to_be_bytes()..)
                        .take(len)
                        .map(|e| e.unwrap())
                        .count() as u64
                })
                .sum()
        }
        fn commits(&self, n: u64, count: u64) {
            for i in 0..count {
                self.1.insert(commit_key(i, n).to_be_bytes(), "x").unwrap();
                self.0.persist(PersistMode::SyncAll).unwrap();
            }
        }
        fn write_burst(&self, t: u64, count: u64) {
            for i in 0..count {
                self.1.insert(writer_key(t, i).to_be_bytes(), "x").unwrap();
                self.0.persist(PersistMode::SyncAll).unwrap();
            }
        }
        fn read_until(&self, t: u64, n: u64, stop: &AtomicBool) -> u64 {
            let mut reads = 0;
            let mut j = t << 32;
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..256 {
                    j += 1;
                    std::hint::black_box(self.1.get(pick(j, n).to_be_bytes()).unwrap());
                }
                reads += 256;
            }
            reads
        }
    }
}

fn open(name: &str, dir: &Path) -> Box<dyn Engine> {
    match name {
        "ferro" => Box::new(ferro_engine::open(dir, false)),
        "ferro-extend" => Box::new(ferro_engine::open(dir, true)),
        "redb" => Box::new(redb_engine::open(dir)),
        "sqlite-full" => Box::new(sqlite_engine::open(dir, "FULL")),
        "sqlite-normal" => Box::new(sqlite_engine::open(dir, "NORMAL")),
        "sled" => Box::new(sled_engine::open(dir)),
        "fjall" => Box::new(fjall_engine::open(dir)),
        _ => panic!("unknown engine {name}"),
    }
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

fn secs(start: Instant) -> f64 {
    start.elapsed().as_secs_f64()
}

fn child(name: &str, dir: &Path, n: u64) {
    fs::create_dir_all(dir).unwrap();
    let mut out: BTreeMap<&str, f64> = BTreeMap::new();
    {
        let db = open(name, dir);
        let t = Instant::now();
        db.bulk(n);
        out.insert("bulk_s", secs(t));
        out.insert("size_mib", dir_size(dir) as f64 / 1048576.0);
        let t = Instant::now();
        assert_eq!(db.gets(n, GETS), GETS);
        out.insert("get_us", secs(t) * 1e6 / GETS as f64);
        let t = Instant::now();
        assert_eq!(db.scan(), n);
        out.insert("scan_ns", secs(t) * 1e9 / n as f64);
        let t = Instant::now();
        let got = db.ranges(RANGES, RANGE_LEN);
        assert!(got >= RANGES * RANGE_LEN as u64 * 99 / 100);
        out.insert("range_us", secs(t) * 1e6 / RANGES as f64);
        let t = Instant::now();
        db.commits(n, COMMITS);
        out.insert("commit_ms", secs(t) * 1e3 / COMMITS as f64);
        let stop = AtomicBool::new(false);
        let reads = AtomicU64::new(0);
        let db_ref: &dyn Engine = &*db;
        let start = Instant::now();
        let mut elapsed = 0.0;
        std::thread::scope(|s| {
            let readers: Vec<_> = (0..READERS)
                .map(|t| {
                    let (stop, reads) = (&stop, &reads);
                    s.spawn(move || {
                        reads.fetch_add(db_ref.read_until(t, n, stop), Ordering::Relaxed);
                    })
                })
                .collect();
            let writers: Vec<_> = (0..WRITERS)
                .map(|t| s.spawn(move || db_ref.write_burst(t, WRITER_COMMITS)))
                .collect();
            for w in writers {
                w.join().unwrap();
            }
            elapsed = secs(start);
            stop.store(true, Ordering::Relaxed);
            for r in readers {
                r.join().unwrap();
            }
        });
        out.insert(
            "conc_commits_s",
            (WRITERS * WRITER_COMMITS) as f64 / elapsed,
        );
        out.insert("conc_reads_s", reads.load(Ordering::Relaxed) as f64 / elapsed);
    }
    let t = Instant::now();
    let db = open(name, dir);
    assert_eq!(db.gets(n, 1), 1);
    out.insert("reopen_ms", secs(t) * 1e3);
    drop(db);
    let body: Vec<String> = out.iter().map(|(k, v)| format!("{k}={v}")).collect();
    println!("RESULT {}", body.join(" "));
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn parent(n: u64) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/runs");
    let exe = std::env::current_exe().unwrap();
    let mut samples: BTreeMap<&str, BTreeMap<String, Vec<f64>>> = BTreeMap::new();
    for round in 0..ROUNDS {
        for k in 0..ENGINES.len() {
            let name = ENGINES[(k + round) % ENGINES.len()];
            let dir = root.join(format!("{name}-{round}"));
            let _ = fs::remove_dir_all(&dir);
            eprintln!("round {round} {name}");
            let out = Command::new(&exe)
                .args(["--child", name, dir.to_str().unwrap(), &n.to_string()])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{name} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let text = String::from_utf8(out.stdout).unwrap();
            let line = text.lines().find(|l| l.starts_with("RESULT ")).unwrap();
            for kv in line["RESULT ".len()..].split(' ') {
                let (k, v) = kv.split_once('=').unwrap();
                samples
                    .entry(name)
                    .or_default()
                    .entry(k.to_string())
                    .or_default()
                    .push(v.parse().unwrap());
            }
            fs::remove_dir_all(&dir).unwrap();
        }
    }
    let m = |name: &str, k: &str| median(samples[name][k].clone());
    let mut table = String::new();
    table.push_str("| engine | durability | bulk load (s) | get (us) | scan (ns/key) | range 1000 keys (us) | durable commit (ms) | 8W+8R commits/s | 8W+8R reads/s (M) | size (MiB) | reopen (ms) |\n");
    table.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for name in ENGINES {
        let dur = match name {
            "ferro" => "fdatasync per commit, group commit",
            "ferro-extend" => "as ferro, bulk load through `extend`",
            "redb" => "Durability::Immediate (fsync per commit)",
            "sqlite-full" => "WAL, synchronous=FULL",
            "sqlite-normal" => "WAL, synchronous=NORMAL (commit not fsynced)",
            "sled" => "insert then flush() (fsync)",
            _ => "PersistMode::SyncAll after each write",
        };
        table.push_str(&format!(
            "| {name} | {dur} | {:.2} | {:.2} | {:.0} | {:.0} | {:.2} | {:.0} | {:.2} | {:.0} | {:.1} |\n",
            m(name, "bulk_s"),
            m(name, "get_us"),
            m(name, "scan_ns"),
            m(name, "range_us"),
            m(name, "commit_ms"),
            m(name, "conc_commits_s"),
            m(name, "conc_reads_s") / 1e6,
            m(name, "size_mib"),
            m(name, "reopen_ms"),
        ));
    }
    println!("{table}");
    fs::write(root.parent().unwrap().join("table.md"), &table).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--child") {
        child(&args[2], Path::new(&args[3]), args[4].parse().unwrap());
    } else {
        let n = args.get(1).map_or(1_000_000, |v| v.parse().unwrap());
        parent(n);
    }
}
