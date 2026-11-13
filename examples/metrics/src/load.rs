use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use ferro::{Durability, Keep, Result};

use crate::model::{SeriesKey, SeriesRef, now_ms};
use crate::tsdb::{Batch, Tsdb};

const SIZE_SAMPLES: u32 = 20;
const METRICS: [&str; 10] = [
    "http_requests_total",
    "cpu_seconds_total",
    "mem_bytes",
    "disk_io_bytes_total",
    "latency_ms",
    "queue_depth",
    "errors_total",
    "net_rx_bytes_total",
    "net_tx_bytes_total",
    "temperature_celsius",
];

pub fn synthetic_series(count: usize) -> Vec<SeriesRef> {
    (0..count)
        .map(|i| {
            SeriesRef::new(SeriesKey::new(
                METRICS[i % METRICS.len()].to_string(),
                vec![
                    ("host".to_string(), format!("h{}", i / 100)),
                    ("region".to_string(), format!("r{}", (i / 10) % 10)),
                ],
            ))
        })
        .collect()
}

fn is_counter(key: &SeriesRef) -> bool {
    key.0.metric.ends_with("_total")
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

struct Worker {
    keys: Vec<SeriesRef>,
    values: Vec<f64>,
    counter: Vec<bool>,
    emitted: u64,
    rng: Rng,
}

impl Worker {
    fn point(&mut self) -> (SeriesRef, usize) {
        let index = (self.emitted % self.keys.len() as u64) as usize;
        let step = self.rng.next() % 100;
        if self.counter[index] {
            self.values[index] += (step + 1) as f64;
        } else {
            let wobble = (step as f64 - 49.5) / 10.0;
            self.values[index] = (self.values[index] + wobble).clamp(0.0, 1000.0);
        }
        self.emitted += 1;
        (self.keys[index].clone(), index)
    }
}

#[derive(Clone, Debug)]
pub struct LoadOptions {
    pub rate: u64,
    pub seconds: u64,
    pub series: usize,
    pub threads: usize,
    pub batch: Duration,
    pub interval_ms: u64,
    pub start_ts: u64,
    pub keep: Keep,
    pub reclaim_bytes: u64,
    pub durability: Durability,
}

#[derive(Clone, Debug, Default)]
pub struct LoadReport {
    pub points: u64,
    pub wall_secs: f64,
    pub commits: u64,
    pub latencies_ms: Vec<f64>,
    pub file_bytes: u64,
    pub live_bytes: u64,
    pub versions: usize,
    pub cycles: u64,
    pub longest_pause: Duration,
    pub size_samples: Vec<(f64, u64)>,
}

impl LoadReport {
    pub fn percentile(&self, p: f64) -> f64 {
        if self.latencies_ms.is_empty() {
            return 0.0;
        }
        let rank = (p / 100.0 * self.latencies_ms.len() as f64).ceil() as usize;
        self.latencies_ms[rank.max(1) - 1]
    }
}

fn write_load(
    tsdb: &Tsdb,
    worker: &mut Worker,
    options: &LoadOptions,
    per_thread_rate: f64,
    start: Instant,
    end: Instant,
) -> Result<(Vec<f64>, u64)> {
    let mut writer = tsdb.writer();
    let mut latencies = Vec::new();
    let mut commits = 0u64;
    let mut next_tick = Instant::now();
    loop {
        let now = Instant::now();
        if now >= end {
            break;
        }
        if next_tick > now {
            thread::sleep(next_tick - now);
        }
        next_tick += options.batch;
        let target = (per_thread_rate * start.elapsed().as_secs_f64()) as u64;
        if target <= worker.emitted {
            continue;
        }
        let due = target - worker.emitted;
        let mut batch = Batch::default();
        for _ in 0..due {
            let scrape = worker.emitted / worker.keys.len() as u64;
            let (key, index) = worker.point();
            let ts = options.start_ts + scrape * options.interval_ms;
            batch.push(&key, ts, worker.values[index]);
        }
        let written = writer.write(batch)?;
        latencies.push(written.commit.as_secs_f64() * 1000.0);
        commits += 1;
    }
    Ok((latencies, commits))
}

fn sample_sizes(tsdb: &Tsdb, total: Duration, start: Instant) -> Vec<(f64, u64)> {
    let pause = (total / SIZE_SAMPLES).max(Duration::from_millis(50));
    let mut samples = Vec::new();
    while start.elapsed() < total {
        samples.push((start.elapsed().as_secs_f64(), tsdb.db.log_bytes()));
        thread::sleep(pause.min(total.saturating_sub(start.elapsed())));
    }
    samples.push((start.elapsed().as_secs_f64(), tsdb.db.log_bytes()));
    samples
}

pub fn run(path: &Path, options: &LoadOptions) -> Result<LoadReport> {
    let all = synthetic_series(options.series);
    let threads = options.threads.max(1).min(all.len().max(1));
    let mut workers: Vec<Worker> = (0..threads)
        .map(|t| {
            let keys: Vec<SeriesRef> = all.iter().skip(t).step_by(threads).cloned().collect();
            Worker {
                counter: keys.iter().map(is_counter).collect(),
                values: vec![100.0; keys.len()],
                keys,
                emitted: 0,
                rng: Rng(0x9e37_79b9_7f4a_7c15 ^ ((t as u64 + 1) * 0x1234_5678_9abc_def1)),
            }
        })
        .collect();
    let mut report = LoadReport::default();
    let tsdb = match options.reclaim_bytes {
        0 => Tsdb::open(path, options.durability)?,
        bytes => Tsdb::open_retaining(path, options.durability, options.keep, bytes)?,
    };
    {
        let mut writer = tsdb.writer();
        for worker in workers.iter_mut() {
            let mut batch = Batch::default();
            for _ in 0..worker.keys.len() {
                let (key, index) = worker.point();
                batch.push(&key, options.start_ts, worker.values[index]);
            }
            writer.write(batch)?;
        }
    }
    let total = Duration::from_secs(options.seconds);
    let start = Instant::now();
    let end = start + total;
    for worker in workers.iter_mut() {
        worker.emitted = worker.keys.len() as u64;
    }
    let initial: u64 = workers.iter().map(|w| w.emitted).sum();
    let per_thread_rate = options.rate as f64 / workers.len() as f64;
    let (results, samples) = thread::scope(|scope| {
        let sampler = scope.spawn(|| sample_sizes(&tsdb, total, start));
        let handles: Vec<_> = workers
            .iter_mut()
            .map(|worker| {
                let tsdb = &tsdb;
                scope.spawn(move || write_load(tsdb, worker, options, per_thread_rate, start, end))
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("load worker panicked"))
            .collect();
        (results, sampler.join().expect("size sampler panicked"))
    });
    for result in results {
        let (latencies, commits) = result?;
        report.latencies_ms.extend(latencies);
        report.commits += commits;
    }
    report.wall_secs = start.elapsed().as_secs_f64();
    report.points = workers.iter().map(|w| w.emitted).sum::<u64>() - initial;
    report.latencies_ms.sort_by(|a, b| a.total_cmp(b));
    report.size_samples = samples;
    let space = tsdb.db.space()?;
    report.file_bytes = tsdb.db.log_bytes();
    report.live_bytes = space.live_bytes;
    report.cycles = space.cycles;
    report.longest_pause = space.longest_pause;
    report.versions = tsdb.db.log().len();
    Ok(report)
}

pub fn default_start() -> u64 {
    now_ms()
}
