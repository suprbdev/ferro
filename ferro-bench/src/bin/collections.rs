use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use ferro::codec::{Encoder, HashSink, Object};
use ferro::{Bytes, Data, Db, Durability, List, Map, Options, Ptr, Val};

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
    drop(db);
    let extra = || (n..n + n / 10).map(|i| (i, format!("value-{i}")));
    let db = seeded(dir, n);
    let cost = measure(&db, |root| root.extend(extra()));
    report(&format!("extend 10% past the end of {n}"), &cost);
    drop(db);
    let db = seeded(dir, n);
    let cost = measure(&db, |root| {
        let mut other: Map<u64, String> = extra().collect();
        root.append(&mut other);
    });
    report(&format!("collect and append 10% past {n}"), &cost);
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
        total += AsRef::<[u8]>::as_ref(&*value)
            .iter()
            .step_by(4096)
            .map(|b| *b as usize)
            .sum::<usize>();
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

trait Seq: Data + Default {
    type Item: Data;
    fn fill(&mut self, items: Vec<Self::Item>);
    fn push(&mut self, item: Self::Item);
    fn fetch(&self, index: u64) -> Val<Self::Item>;
    fn scan(&self) -> u64;
}

impl<T: Data> Seq for List<T> {
    type Item = T;

    fn fill(&mut self, items: Vec<T>) {
        self.extend(items);
    }

    fn push(&mut self, item: T) {
        List::push(self, item);
    }

    fn fetch(&self, index: u64) -> Val<T> {
        self.get(index as usize).unwrap()
    }

    fn scan(&self) -> u64 {
        self.iter().count() as u64
    }
}

#[derive(Clone, Data)]
struct Chunked<T: Data + Ord, const N: u64> {
    len: u64,
    chunks: Map<u64, Vec<Arc<T>>>,
}

impl<T: Data + Ord, const N: u64> Default for Chunked<T, N> {
    fn default() -> Self {
        Chunked {
            len: 0,
            chunks: Map::new(),
        }
    }
}

impl<T: Data + Ord, const N: u64> Seq for Chunked<T, N> {
    type Item = T;

    fn fill(&mut self, items: Vec<T>) {
        self.len = items.len() as u64;
        let mut groups: Vec<(u64, Vec<Arc<T>>)> = Vec::new();
        for (index, item) in items.into_iter().enumerate() {
            let chunk = index as u64 / N;
            match groups.last_mut() {
                Some(last) if last.0 == chunk => last.1.push(Arc::new(item)),
                _ => groups.push((chunk, vec![Arc::new(item)])),
            }
        }
        self.chunks.extend(groups);
    }

    fn push(&mut self, item: T) {
        let chunk = self.len / N;
        self.chunks
            .get_or_insert_with(chunk, Vec::new)
            .push(Arc::new(item));
        self.len += 1;
    }

    fn fetch(&self, index: u64) -> Val<T> {
        Val::from(self.chunks.get(&(index / N)).unwrap()[(index % N) as usize].clone())
    }

    fn scan(&self) -> u64 {
        self.chunks.values().map(|chunk| chunk.len() as u64).sum()
    }
}

fn seq_bench<S: Seq>(dir: &Path, label: &str, n: u64, make: impl Fn(u64) -> S::Item) {
    let path = dir.join("seq.ferro");
    let _ = std::fs::remove_file(&path);
    let db = open::<S>(dir, "seq.ferro");
    let items: Vec<S::Item> = (0..n).map(&make).collect();
    db.update(|root| root.fill(items)).unwrap();
    let base = db.size_on_disk();
    let pushes = 20_000u64;
    let start = Instant::now();
    for i in 0..pushes {
        let item = make(n + i);
        db.update(|root| root.push(item)).unwrap();
    }
    let push_ns = start.elapsed().as_secs_f64() * 1e9 / pushes as f64;
    let per_push = (db.size_on_disk() - base) as f64 / pushes as f64;
    let snapshot = db.read();
    let start = Instant::now();
    for i in 0..500_000u64 {
        std::hint::black_box(snapshot.fetch(mix(i) % n));
    }
    let warm = start.elapsed().as_secs_f64() * 1e9 / 500_000.0;
    let start = Instant::now();
    let total = snapshot.scan();
    let scan = start.elapsed().as_secs_f64() * 1e9 / total as f64;
    drop(snapshot);
    let size = db.size_on_disk();
    drop(db);
    let cold_db = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20)
        .open::<S>(&path)
        .unwrap();
    let snapshot = cold_db.read();
    let start = Instant::now();
    for i in 0..100_000u64 {
        std::hint::black_box(snapshot.fetch(mix(i) % n));
    }
    let cold = start.elapsed().as_secs_f64() * 1e9 / 100_000.0;
    println!(
        "{label:<22} n {n:>8}: {per_push:>7.0} bytes/push {push_ns:>7.0} ns/push, file {:>6.1} B/item, warm get {warm:>6.0} ns, cold get {cold:>6.0} ns, scan {scan:>5.0} ns/item",
        size as f64 / n as f64
    );
}

fn comment(i: u64) -> String {
    let word = |x: u64| {
        [
            "ferro", "commit", "merge", "branch", "index", "table", "scan", "flush",
        ][x as usize % 8]
    };
    let seed = mix(i);
    format!(
        "user{} wrote about {} and {} near {} on {} {} {}",
        seed % 5000,
        word(seed >> 8),
        word(seed >> 16),
        word(seed >> 24),
        word(seed >> 32),
        word(seed >> 40),
        seed % 1000,
    )
}

fn trie_record(body: Vec<u8>, links: usize, seed: u64) -> usize {
    let links: Vec<Ptr> = (0..links)
        .map(|i| {
            let mut hash = [0u8; 20];
            hash[..8].copy_from_slice(&mix(seed ^ i as u64).to_le_bytes());
            hash[8..16].copy_from_slice(&mix(seed + i as u64).to_le_bytes());
            Ptr {
                hash,
                offset: 1 << 16,
            }
        })
        .collect();
    ferro::store::record_len(1 << 40, &Object { body, links })
}

fn trie_estimate(label: &str, n: u64, fanout: u64, make: impl Fn(u64) -> Vec<u8>) {
    let mut total = 0usize;
    let pushes = 20_000u64;
    for i in 0..pushes {
        let length = n + i + 1;
        let held = (length - 1) % fanout + 1;
        let mut body = Vec::new();
        push(&mut body, held);
        for item in length - held..length {
            body.extend_from_slice(&make(item));
        }
        total += trie_record(body, 0, i);
        let mut span = fanout;
        while span < length {
            let links = ((length - 1) / span) % fanout + 1;
            let mut body = Vec::new();
            push(&mut body, links);
            total += trie_record(body, links as usize, i + span);
            span *= fanout;
        }
    }
    println!(
        "{label:<22} n {n:>8}: {:>7.0} bytes/push before commit overhead",
        total as f64 / pushes as f64
    );
}

fn push(body: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        body.push(value as u8 | 0x80);
        value >>= 7;
    }
    body.push(value as u8);
}

fn encoded<T: Data>(value: &T) -> Vec<u8> {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder).unwrap();
    encoder.into_buf()
}

fn list_bench(dir: &Path) {
    for n in [100_000u64, 1_000_000] {
        seq_bench::<List<u64>>(dir, "List u64", n, |i| i);
        seq_bench::<Chunked<u64, 16>>(dir, "Chunked16 u64", n, |i| i);
        seq_bench::<Chunked<u64, 64>>(dir, "Chunked64 u64", n, |i| i);
        seq_bench::<Chunked<u64, 256>>(dir, "Chunked256 u64", n, |i| i);
        trie_estimate("Trie8 u64", n, 8, |i| encoded(&i));
        trie_estimate("Trie32 u64", n, 32, |i| encoded(&i));
        seq_bench::<List<String>>(dir, "List text", n, comment);
        seq_bench::<Chunked<String, 16>>(dir, "Chunked16 text", n, comment);
        seq_bench::<Chunked<String, 64>>(dir, "Chunked64 text", n, comment);
        trie_estimate("Trie8 text", n, 8, |i| encoded(&comment(i)));
        trie_estimate("Trie32 text", n, 32, |i| encoded(&comment(i)));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map_or("all", String::as_str);
    let n: u64 = args
        .get(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    std::fs::create_dir_all("target/bench").unwrap();
    let dir = tempfile::tempdir_in("target/bench").unwrap();
    match command {
        "range" => range_bench(dir.path(), n),
        "merge" => merge_bench(dir.path(), n),
        "bytes" => bytes_bench(dir.path()),
        "list" => list_bench(dir.path()),
        _ => {
            range_bench(dir.path(), n);
            merge_bench(dir.path(), n);
            bytes_bench(dir.path());
            list_bench(dir.path());
        }
    }
}
