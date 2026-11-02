use std::path::PathBuf;
use std::time::Instant;

use ferro::admin::{self, Keep};
use ferro::{Data, Db, Map};
use metrics::chunk::Chunk;

const BUCKET: u64 = 3_600_000;

#[derive(Clone, Default, Data)]
struct Root {
    flat: Map<(u64, u64), f64>,
    chunks: Map<(u64, u64), Chunk>,
    parts: Map<u64, Map<u64, Chunk>>,
    pieces: Map<u64, Map<(u64, u64), Chunk>>,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn value(sid: u64, step: u64, rng: &mut Rng) -> f64 {
    if sid % 2 == 0 {
        (step * 10 + sid * 1000 + rng.next() % 10) as f64
    } else {
        50.0 + ((rng.next() % 10_000) as f64) / 100.0
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let layout = args[1].clone();
    let series: u64 = args[2].parse().unwrap();
    let per_series: u64 = args[3].parse().unwrap();
    let scrapes_per_commit: u64 = args[4].parse().unwrap();
    let interval: u64 = 15_000;
    let path: PathBuf = PathBuf::from(format!("target/modelbench-{layout}.ferro"));
    let _ = std::fs::remove_file(&path);
    let db: Db<Root> = Db::open(&path).unwrap();
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let base = 1_700_000_000_000u64 / BUCKET * BUCKET;
    let started = Instant::now();
    let mut commits = 0u64;
    let mut step = 0;
    while step < per_series {
        let end = (step + scrapes_per_commit).min(per_series);
        let mut batch: Vec<(u64, u64, f64)> = Vec::new();
        for s in step..end {
            for sid in 0..series {
                batch.push((sid, base + s * interval, value(sid, s, &mut rng)));
            }
        }
        match layout.as_str() {
            "flat" => db
                .commit("b", |r| {
                    for (sid, ts, v) in &batch {
                        r.flat.insert((*sid, *ts), *v);
                    }
                })
                .unwrap(),
            "chunk" => db
                .commit("b", |r| group(&batch, |sid, bucket, pts| {
                    let key = (sid, bucket);
                    let next = match r.chunks.get(&key) {
                        Some(c) => c.merged(pts).unwrap(),
                        None => Chunk::encode_points(pts),
                    };
                    r.chunks.insert(key, next);
                }))
                .unwrap(),
            "pieces" => db
                .commit("b", |r| group(&batch, |sid, bucket, pts| {
                    let part = r.pieces.get_or_insert_with(bucket, Map::new);
                    part.insert((sid, pts[0].0), Chunk::encode_points(pts));
                }))
                .unwrap(),
            _ => db
                .commit("b", |r| group(&batch, |sid, bucket, pts| {
                    let part = r.parts.get_or_insert_with(bucket, Map::new);
                    let next = match part.get(&sid) {
                        Some(c) => c.merged(pts).unwrap(),
                        None => Chunk::encode_points(pts),
                    };
                    part.insert(sid, next);
                }))
                .unwrap(),
        }
        commits += 1;
        step = end;
    }
    let ingest = started.elapsed();
    let total = series * per_series;
    let size = db.size_on_disk();
    println!(
        "{layout}: {total} pts in {:.2}s = {:.0} pts/s, {commits} commits ({:.1} ms/commit), file {} MB = {:.2} B/pt",
        ingest.as_secs_f64(),
        total as f64 / ingest.as_secs_f64(),
        ingest.as_secs_f64() * 1000.0 / commits as f64,
        size / 1_000_000,
        size as f64 / total as f64
    );
    let snap = db.read();
    let from = base;
    let to = base + per_series * interval;
    let t = Instant::now();
    let mut n = 0;
    for _ in 0..20 {
        n = query(&layout, &snap, 7, from, to).len();
    }
    println!("{layout}: one series full range {n} pts: {:.3} ms", t.elapsed().as_secs_f64() * 1000.0 / 20.0);
    let t = Instant::now();
    let mut total_sum = 0.0;
    for sid in 0..series {
        total_sum += query(&layout, &snap, sid, from, to).iter().map(|p| p.1).sum::<f64>();
    }
    println!("{layout}: all {series} series full range: {:.1} ms (sum {total_sum:.0})", t.elapsed().as_secs_f64() * 1000.0);
    drop(snap);
    let versions = db.log().len();
    let verify = db.verify().unwrap();
    println!("{layout}: versions {versions}, objects {}, live bytes {}", verify.objects, verify.bytes);
    drop(db);
    let t = Instant::now();
    let c = admin::compact(&path, Keep::Last(1)).unwrap();
    println!(
        "{layout}: compact keep-last-1 {:.2}s: {} -> {} bytes ({:.2} B/pt)",
        t.elapsed().as_secs_f64(),
        c.before_bytes,
        c.after_bytes,
        c.after_bytes as f64 / total as f64
    );
}

fn group(batch: &[(u64, u64, f64)], mut f: impl FnMut(u64, u64, &[(u64, f64)])) {
    let mut sorted: Vec<&(u64, u64, f64)> = batch.iter().collect();
    sorted.sort_by_key(|p| (p.1 / BUCKET, p.0, p.1));
    let mut i = 0;
    while i < sorted.len() {
        let key = (sorted[i].1 / BUCKET, sorted[i].0);
        let mut pts = Vec::new();
        while i < sorted.len() && (sorted[i].1 / BUCKET, sorted[i].0) == key {
            pts.push((sorted[i].1, sorted[i].2));
            i += 1;
        }
        f(key.1, key.0 * BUCKET, &pts);
    }
}

fn query(layout: &str, root: &Root, sid: u64, from: u64, to: u64) -> Vec<(u64, f64)> {
    match layout {
        "flat" => root
            .flat
            .range((sid, from)..(sid, to))
            .map(|((_, ts), v)| (ts, *v))
            .collect(),
        "chunk" => {
            let mut out = Vec::new();
            for (_, chunk) in root.chunks.range((sid, from / BUCKET * BUCKET)..=(sid, to)) {
                out.extend(chunk.decode_points().unwrap().into_iter().filter(|p| p.0 >= from && p.0 < to));
            }
            out
        }
        "pieces" => {
            let mut out = Vec::new();
            for (_, part) in root.pieces.range((from / BUCKET * BUCKET)..=to) {
                for (_, chunk) in part.range((sid, 0)..=(sid, u64::MAX)) {
                    out.extend(chunk.decode_points().unwrap().into_iter().filter(|p| p.0 >= from && p.0 < to));
                }
            }
            out
        }
        _ => {
            let mut out = Vec::new();
            for (_, part) in root.parts.range((from / BUCKET * BUCKET)..=to) {
                if let Some(chunk) = part.get(&sid) {
                    out.extend(chunk.decode_points().unwrap().into_iter().filter(|p| p.0 >= from && p.0 < to));
                }
            }
            out
        }
    }
}
