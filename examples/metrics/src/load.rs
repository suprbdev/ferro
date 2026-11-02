use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use ferro::admin::{self, Keep};
use ferro::{Durability, Result};

use crate::model::{SeriesKey, SeriesRef, now_ms};
use crate::tsdb::{Batch, Tsdb};

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
    pub compact_every: Option<u64>,
    pub keep: Keep,
    pub durability: Durability,
}

#[derive(Clone, Debug, Default)]
pub struct CompactionEvent {
    pub at_secs: f64,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub pause_secs: f64,
}

#[derive(Clone, Debug, Default)]
pub struct LoadReport {
    pub points: u64,
    pub wall_secs: f64,
    pub commits: u64,
    pub latencies_ms: Vec<f64>,
    pub file_bytes: u64,
    pub versions: usize,
    pub compactions: Vec<CompactionEvent>,
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

fn run_epoch(
    tsdb: &Tsdb,
    workers: &mut [Worker],
    options: &LoadOptions,
    run_start: Instant,
    epoch_end: Instant,
    latencies: &mut Vec<f64>,
    commits: &mut u64,
) -> Result<()> {
    let per_thread_rate = options.rate as f64 / workers.len() as f64;
    let results: Vec<Result<(Vec<f64>, u64)>> = thread::scope(|scope| {
        let handles: Vec<_> = workers
            .iter_mut()
            .map(|worker| {
                scope.spawn(move || -> Result<(Vec<f64>, u64)> {
                    let mut writer = tsdb.writer();
                    let mut lat = Vec::new();
                    let mut count = 0u64;
                    let mut next_tick = Instant::now();
                    loop {
                        let now = Instant::now();
                        if now >= epoch_end {
                            break;
                        }
                        if next_tick > now {
                            thread::sleep(next_tick - now);
                        }
                        next_tick += options.batch;
                        let elapsed = run_start.elapsed().as_secs_f64();
                        let target = (per_thread_rate * elapsed) as u64;
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
                        lat.push(written.commit.as_secs_f64() * 1000.0);
                        count += 1;
                    }
                    Ok((lat, count))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("load worker panicked"))
            .collect()
    });
    for result in results {
        let (lat, count) = result?;
        latencies.extend(lat);
        *commits += count;
    }
    Ok(())
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
    let mut tsdb = Tsdb::open(path, options.durability)?;
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
    let mut run_start = Instant::now();
    let wall_start = run_start;
    for worker in workers.iter_mut() {
        worker.emitted = worker.keys.len() as u64;
    }
    let initial: u64 = workers.iter().map(|w| w.emitted).sum();
    loop {
        let spent = run_start.elapsed();
        if spent >= total {
            break;
        }
        let epoch_len = match options.compact_every {
            Some(secs) => Duration::from_secs(secs).min(total - spent),
            None => total - spent,
        };
        let epoch_end = Instant::now() + epoch_len;
        run_epoch(
            &tsdb,
            &mut workers,
            options,
            run_start,
            epoch_end,
            &mut report.latencies_ms,
            &mut report.commits,
        )?;
        report
            .size_samples
            .push((wall_start.elapsed().as_secs_f64(), tsdb.db.size_on_disk()));
        let finished = run_start.elapsed() >= total;
        if options.compact_every.is_some() && !finished {
            let before = tsdb.db.size_on_disk();
            let pause = Instant::now();
            drop(tsdb);
            let result = admin::compact(path, options.keep)?;
            tsdb = Tsdb::open(path, options.durability)?;
            let stall = pause.elapsed();
            run_start += stall;
            report.compactions.push(CompactionEvent {
                at_secs: wall_start.elapsed().as_secs_f64(),
                before_bytes: before,
                after_bytes: result.after_bytes,
                pause_secs: stall.as_secs_f64(),
            });
            report
                .size_samples
                .push((wall_start.elapsed().as_secs_f64(), tsdb.db.size_on_disk()));
        }
    }
    report.wall_secs = wall_start.elapsed().as_secs_f64();
    report.points = workers.iter().map(|w| w.emitted).sum::<u64>() - initial;
    report.latencies_ms.sort_by(|a, b| a.total_cmp(b));
    report.file_bytes = tsdb.db.size_on_disk();
    report.versions = tsdb.db.log().len();
    Ok(report)
}

pub fn default_start() -> u64 {
    now_ms()
}
