use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ferro::{Keep, Reclaim};
use ferro::{Data, Db};
use queue::{Job, NewJob, Queue, QueueOptions, Runner, RunnerConfig};

fn dir() -> PathBuf {
    let path = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/qbench"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn fresh(name: &str) -> PathBuf {
    let path = dir().join(name);
    let _ = fs::remove_file(&path);
    path
}

fn arg(args: &HashMap<String, String>, key: &str, default: u64) -> u64 {
    args.get(key).map(|v| v.parse().unwrap()).unwrap_or(default)
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index]
}

fn summary(label: &str, mut samples: Vec<u64>) -> String {
    samples.sort_unstable();
    format!(
        "{label} p50={}us p99={}us max={}us n={}",
        pct(&samples, 0.5) / 1000,
        pct(&samples, 0.99) / 1000,
        samples.last().copied().unwrap_or(0) / 1000,
        samples.len()
    )
}

fn open(path: &PathBuf) -> Queue {
    Queue::open(path, QueueOptions::default()).unwrap()
}

fn prefill(queue: &Queue, depth: u64) {
    let start = Instant::now();
    let mut made = 0;
    while made < depth {
        let take = 20_000.min(depth - made);
        let specs = (0..take)
            .map(|i| NewJob::new("q", format!("bg-{}", made + i)).priority(0))
            .collect();
        queue.enqueue_batch(specs).unwrap();
        made += take;
    }
    println!(
        "prefill depth={depth} took={:.1}s file_mib={:.1}",
        start.elapsed().as_secs_f64(),
        queue.size_on_disk() as f64 / 1048576.0
    );
}

fn throughput(args: &HashMap<String, String>) {
    let depth = arg(args, "depth", 1);
    let secs = arg(args, "secs", 10);
    let list: Vec<u64> = args
        .get("workers")
        .map(|v| v.split(',').map(|n| n.parse().unwrap()).collect())
        .unwrap_or_else(|| vec![1, 8, 32]);
    let path = fresh(&format!("throughput-{depth}.ferro"));
    let queue = open(&path);
    if depth > 1 {
        prefill(&queue, depth);
    }
    for workers in list {
        let stop = Arc::new(AtomicBool::new(false));
        let size_before = queue.size_on_disk();
        let version_before = queue.version();
        let start = Instant::now();
        let mut handles = Vec::new();
        for w in 0..workers {
            let queue = queue.clone();
            let stop = stop.clone();
            handles.push(thread::spawn(move || {
                let name = format!("w{w}");
                let mut enq = Vec::new();
                let mut lease = Vec::new();
                let mut ack = Vec::new();
                let mut jobs = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    queue.enqueue(NewJob::new("q", "x").priority(200)).unwrap();
                    enq.push(t.elapsed().as_nanos() as u64);
                    let t = Instant::now();
                    let got = queue.lease("q", &name, 60_000).unwrap().unwrap();
                    lease.push(t.elapsed().as_nanos() as u64);
                    let t = Instant::now();
                    queue.ack(got.lease()).unwrap();
                    ack.push(t.elapsed().as_nanos() as u64);
                    jobs += 1;
                }
                (jobs, enq, lease, ack)
            }));
        }
        thread::sleep(Duration::from_secs(secs));
        stop.store(true, Ordering::Relaxed);
        let mut jobs = 0;
        let (mut enq, mut lease, mut ack) = (Vec::new(), Vec::new(), Vec::new());
        for handle in handles {
            let (j, e, l, a) = handle.join().unwrap();
            jobs += j;
            enq.extend(e);
            lease.extend(l);
            ack.extend(a);
        }
        let elapsed = start.elapsed().as_secs_f64();
        let grown = queue.size_on_disk() - size_before;
        let commits = queue.version() - version_before;
        println!(
            "throughput depth={depth} workers={workers} jobs={jobs} jobs_per_s={:.0} commits_per_s={:.0} bytes_per_job={:.0} bytes_per_commit={:.0}",
            jobs as f64 / elapsed,
            commits as f64 / elapsed,
            grown as f64 / jobs as f64,
            grown as f64 / commits as f64
        );
        println!("  {}", summary("enqueue", enq));
        println!("  {}", summary("lease  ", lease));
        println!("  {}", summary("ack    ", ack));
    }
    println!("final file_mib={:.1} live_jobs={}", queue.size_on_disk() as f64 / 1048576.0, queue.db().read().jobs.len());
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct Counter {
    n: u64,
}

fn growth(args: &HashMap<String, String>) {
    let total = arg(args, "jobs", 20_000);
    let floor_path = fresh("floor.ferro");
    {
        let db = Db::<Counter>::open(&floor_path).unwrap();
        let before = db.size_on_disk();
        let start = Instant::now();
        for _ in 0..2000 {
            db.commit("bump", |c| c.n += 1).unwrap();
        }
        println!(
            "floor trivial_commit bytes_per_commit={:.0} commits_per_s={:.0}",
            (db.size_on_disk() - before) as f64 / 2000.0,
            2000.0 / start.elapsed().as_secs_f64()
        );
    }
    for batch in [1u64, 10, 100] {
        let path = fresh(&format!("growth-{batch}.ferro"));
        let queue = open(&path);
        let base = queue.size_on_disk();
        let (mut enq_b, mut lease_b, mut ack_b) = (0u64, 0u64, 0u64);
        let (mut enq_t, mut lease_t, mut ack_t) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        let mut done = 0;
        let start = Instant::now();
        while done < total {
            let s0 = queue.size_on_disk();
            let t = Instant::now();
            let specs = (0..batch).map(|i| NewJob::new("q", format!("job-{}", done + i))).collect();
            queue.enqueue_batch(specs).unwrap();
            enq_t += t.elapsed();
            let s1 = queue.size_on_disk();
            let t = Instant::now();
            let got = queue.lease_n("q", "w", 60_000, batch as usize).unwrap();
            lease_t += t.elapsed();
            let s2 = queue.size_on_disk();
            let leases: Vec<_> = got.iter().map(|l| l.lease()).collect();
            let t = Instant::now();
            queue.ack_many(&leases).unwrap();
            ack_t += t.elapsed();
            let s3 = queue.size_on_disk();
            enq_b += s1 - s0;
            lease_b += s2 - s1;
            ack_b += s3 - s2;
            done += batch;
        }
        let grown = queue.size_on_disk() - base;
        let commits = (done / batch) as f64;
        println!(
            "growth batch={batch} jobs={done} bytes_per_job={:.0} (enqueue {:.0} + lease {:.0} + ack {:.0}) bytes_per_commit enq={:.0} lease={:.0} ack={:.0} jobs_per_s={:.0}",
            grown as f64 / done as f64,
            enq_b as f64 / done as f64,
            lease_b as f64 / done as f64,
            ack_b as f64 / done as f64,
            enq_b as f64 / commits,
            lease_b as f64 / commits,
            ack_b as f64 / commits,
            done as f64 / start.elapsed().as_secs_f64()
        );
        println!(
            "  per commit latency enq={:.0}us lease={:.0}us ack={:.0}us",
            enq_t.as_micros() as f64 / commits,
            lease_t.as_micros() as f64 / commits,
            ack_t.as_micros() as f64 / commits
        );
        let compacted = queue.compact(Keep::Last(1)).unwrap();
        println!(
            "  compact Keep::Last(1): {} -> {} bytes ({:.2}% of original) objects={} pause={:?}",
            compacted.before_bytes,
            compacted.after_bytes,
            100.0 * compacted.after_bytes as f64 / compacted.before_bytes as f64,
            compacted.objects,
            compacted.pause
        );
    }
}

fn wake(args: &HashMap<String, String>) {
    let samples = arg(args, "samples", 300);
    for workers in [1usize, 8, 32] {
        let path = fresh("wake.ferro");
        let queue = open(&path);
        let epoch = Instant::now();
        let log: Arc<Mutex<Vec<(u64, u64)>>> = Arc::default();
        let handler = {
            let log = log.clone();
            Arc::new(move |job: &Job| {
                let picked = epoch.elapsed().as_nanos() as u64;
                let started: u64 = String::from_utf8_lossy(&job.payload).parse().unwrap();
                log.lock().unwrap().push((started, picked));
                Ok(())
            })
        };
        let mut config = RunnerConfig::new(vec!["q".into()], workers);
        config.idle_wait = Duration::from_secs(600);
        let runner = Runner::start(queue.clone(), config, handler);
        thread::sleep(Duration::from_millis(200));
        let mut returned = Vec::new();
        for _ in 0..samples {
            let started = epoch.elapsed().as_nanos() as u64;
            queue.enqueue(NewJob::new("q", started.to_string())).unwrap();
            returned.push((started, epoch.elapsed().as_nanos() as u64));
            thread::sleep(Duration::from_millis(15));
        }
        thread::sleep(Duration::from_millis(300));
        let log = log.lock().unwrap();
        let ret: HashMap<u64, u64> = returned.into_iter().collect();
        let from_start: Vec<u64> = log.iter().map(|(s, p)| p - s).collect();
        let from_return: Vec<u64> = log.iter().map(|(s, p)| p.saturating_sub(ret[s])).collect();
        println!("wake workers={workers}");
        println!("  {}", summary("enqueue_start_to_handler ", from_start));
        println!("  {}", summary("enqueue_return_to_handler", from_return));
        runner.stop();
    }
}

fn voluntary_switches() -> u64 {
    let mut total = 0;
    for task in fs::read_dir("/proc/self/task").unwrap().flatten() {
        let status = fs::read_to_string(task.path().join("status")).unwrap_or_default();
        total += status
            .lines()
            .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
            .and_then(|count| count.trim().parse::<u64>().ok())
            .unwrap_or(0);
    }
    total
}

fn idle(args: &HashMap<String, String>) {
    let commits = arg(args, "commits", 3000);
    for workers in [0usize, 8, 32] {
        let path = fresh("idle.ferro");
        let queue = open(&path);
        let handler: queue::runner::Handler = Arc::new(|_| Ok(()));
        let mut config = RunnerConfig::new(vec!["q".into()], workers);
        config.idle_wait = Duration::from_secs(600);
        let runner = Runner::start(queue.clone(), config, handler);
        thread::sleep(Duration::from_millis(200));
        let before = voluntary_switches();
        let start = Instant::now();
        for i in 0..commits {
            queue.enqueue(NewJob::new("elsewhere", format!("{i}"))).unwrap();
        }
        let elapsed = start.elapsed().as_secs_f64();
        let switches = voluntary_switches() - before;
        println!(
            "idle workers={workers} commits_per_s={:.0} commit_us={:.1} switches_per_commit={:.2}",
            commits as f64 / elapsed,
            elapsed * 1e6 / commits as f64,
            switches as f64 / commits as f64
        );
        runner.stop();
    }
}

fn batching(args: &HashMap<String, String>) {
    let total = arg(args, "jobs", 100_000);
    for batch in [1usize, 4, 16, 64] {
        let path = fresh("batching.ferro");
        let queue = open(&path);
        let mut made = 0;
        while made < total {
            let take = 10_000.min(total - made);
            let specs = (0..take).map(|i| NewJob::new("q", format!("{}", made + i))).collect();
            queue.enqueue_batch(specs).unwrap();
            made += take;
        }
        let size_before = queue.size_on_disk();
        let version_before = queue.version();
        let acked = Arc::new(AtomicU64::new(0));
        let start = Instant::now();
        let mut handles = Vec::new();
        for w in 0..8 {
            let queue = queue.clone();
            let acked = acked.clone();
            handles.push(thread::spawn(move || {
                let name = format!("w{w}");
                loop {
                    let got = queue.lease_n("q", &name, 60_000, batch).unwrap();
                    if got.is_empty() {
                        break;
                    }
                    let leases: Vec<_> = got.iter().map(|l| l.lease()).collect();
                    queue.ack_many(&leases).unwrap();
                    acked.fetch_add(leases.len() as u64, Ordering::Relaxed);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let elapsed = start.elapsed().as_secs_f64();
        let jobs = acked.load(Ordering::Relaxed);
        println!(
            "batching lease_batch={batch} workers=8 jobs={jobs} jobs_per_s={:.0} commits={} bytes_per_job={:.0}",
            jobs as f64 / elapsed,
            queue.version() - version_before,
            (queue.size_on_disk() - size_before) as f64 / jobs as f64
        );
    }
}

fn day(args: &HashMap<String, String>) {
    let rate = arg(args, "rate", 1000);
    let secs = arg(args, "secs", 120);
    let workers = arg(args, "workers", 8) as usize;
    let batch = arg(args, "batch", 10);
    let pace = arg(args, "pace_ms", 1);
    let path = fresh("day.ferro");
    let queue = open(&path);
    let handler: queue::runner::Handler = Arc::new(|_| Ok(()));
    let mut config = RunnerConfig::new(vec!["q".into()], workers);
    config.batch = batch as usize;
    config.idle_wait = Duration::from_secs(600);
    let runner = Runner::start(queue.clone(), config, handler);
    let start = Instant::now();
    let mut sent = 0u64;
    let mut next_report = 10u64;
    let mut late = 0u64;
    let base = queue.size_on_disk();
    while start.elapsed().as_secs() < secs {
        let due = (start.elapsed().as_secs_f64() * rate as f64) as u64;
        if sent < due {
            let take = due - sent;
            let specs = (0..take).map(|i| NewJob::new("q", format!("{}", sent + i))).collect();
            queue.enqueue_batch(specs).unwrap();
            sent += take;
            if due - sent > rate / 10 {
                late += 1;
            }
        }
        thread::sleep(Duration::from_millis(pace));
        if start.elapsed().as_secs() >= next_report {
            let state = queue.db().read();
            println!(
                "t={}s sent={sent} done={} ready={} leased={} file_mib={:.1} version={}",
                next_report,
                runner.done(),
                state.ready.len(),
                state.leases.len(),
                queue.size_on_disk() as f64 / 1048576.0,
                queue.version()
            );
            next_report += 10;
        }
    }
    while runner.done() < sent {
        thread::sleep(Duration::from_millis(50));
    }
    let elapsed = start.elapsed().as_secs_f64();
    let size = queue.size_on_disk();
    let version = queue.version();
    println!(
        "day rate_target={rate} sent={sent} achieved_per_s={:.0} late_ticks={late} commits={version} commits_per_job={:.2} file_bytes={size} bytes_per_job={:.0} bytes_per_commit={:.0}",
        sent as f64 / elapsed,
        version as f64 / sent as f64,
        (size - base) as f64 / sent as f64,
        (size - base) as f64 / version as f64
    );
    println!(
        "  extrapolated_24h jobs={} file_gib={:.1}",
        rate * 86_400,
        (size - base) as f64 / sent as f64 * (rate * 86_400) as f64 / 1073741824.0
    );
    for (label, keep) in [
        ("Keep::All", Keep::All),
        ("Keep::Last(1000)", Keep::Last(1000)),
        ("Keep::Last(1)", Keep::Last(1)),
    ] {
        let t = Instant::now();
        let compacted = queue.compact(keep).unwrap();
        println!(
            "  compact {label}: {} -> {} bytes in {:.2}s pause={:?} objects={} dropped_commits={}",
            compacted.before_bytes,
            compacted.after_bytes,
            t.elapsed().as_secs_f64(),
            compacted.pause,
            compacted.objects,
            compacted.commits_dropped
        );
    }
    runner.stop();
}

fn cycle(args: &HashMap<String, String>) {
    let rate = arg(args, "rate", 1000);
    let secs = arg(args, "secs", 120);
    let pace = arg(args, "pace_ms", 10);
    let every = arg(args, "reclaim_mb", 64);
    let keep = arg(args, "keep", 1) as usize;
    let path = fresh("cycle.ferro");
    let queue = Queue::open(
        &path,
        QueueOptions {
            history: Keep::Last(keep),
            reclaim: Reclaim {
                every_bytes: every << 20,
            },
            ..QueueOptions::default()
        },
    )
    .unwrap();
    let handler: queue::runner::Handler = Arc::new(|_| Ok(()));
    let mut config = RunnerConfig::new(vec!["q".into()], 8);
    config.batch = 10;
    config.idle_wait = Duration::from_secs(600);
    let runner = Runner::start(queue.clone(), config, handler);
    let start = Instant::now();
    let mut sent = 0u64;
    let mut peak = 0u64;
    while start.elapsed().as_secs() < secs {
        let due = (start.elapsed().as_secs_f64() * rate as f64) as u64;
        if sent < due {
            let take = due - sent;
            let specs = (0..take).map(|i| NewJob::new("q", format!("{}", sent + i))).collect();
            queue.enqueue_batch(specs).unwrap();
            sent += take;
        }
        peak = peak.max(queue.size_on_disk());
        thread::sleep(Duration::from_millis(pace));
    }
    while runner.done() < sent {
        thread::sleep(Duration::from_millis(20));
    }
    let space = queue.space().unwrap();
    println!(
        "cycle rate={rate} secs={secs} reclaim_mb={every} keep_last={keep} sent={sent} peak_file_bytes={peak} final_file_bytes={} cycles={} worst_pause_ms={:.1}",
        space.file_bytes,
        space.cycles,
        space.longest_pause.as_secs_f64() * 1000.0
    );
    runner.stop();
}

fn hist(args: &HashMap<String, String>) {
    let file = args.get("file").expect("file=").clone();
    let id = arg(args, "id", 1000050);
    let queue = Queue::open_read_only(&file, QueueOptions::default()).unwrap();
    println!("commits={}", queue.version());
    let t = Instant::now();
    let events = queue.history(id).unwrap();
    println!("queue history events={} took={:.1}ms", events.len(), t.elapsed().as_secs_f64() * 1000.0);
    let db = queue.db();
    let t = Instant::now();
    let created = db.bisect(.., |state| state.next_id > id).unwrap().unwrap();
    let gone = db.bisect(created.., |state| !state.jobs.contains_key(&id)).unwrap();
    println!("two bisects created={created} gone={gone:?} took={:.2}ms", t.elapsed().as_secs_f64() * 1000.0);
    let last = gone.unwrap_or_else(|| db.version());
    let t = Instant::now();
    let steps = db.history_of_range(created..=last, |state| state.jobs.get(&id)).count();
    println!("history_of_range steps={steps} span={} took={:.1}ms", last - created, t.elapsed().as_secs_f64() * 1000.0);
    let t = Instant::now();
    let steps = db.bisect_changes(created..=last, |state| state.jobs.value_hash(&id)).unwrap().len();
    println!("bisect_changes steps={steps} took={:.2}ms", t.elapsed().as_secs_f64() * 1000.0);
    let t = Instant::now();
    let steps = db.history_of(|state| state.jobs.get(&id)).count();
    println!("history_of steps={steps} took={:.1}ms", t.elapsed().as_secs_f64() * 1000.0);
}

fn split(args: &HashMap<String, String>) {
    let depth = arg(args, "depth", 1_000_000);
    let rounds = 3000u64;
    let path = fresh("split.ferro");
    let queue = open(&path);
    prefill(&queue, depth);
    let db = queue.db().clone();
    let measure = |label: &str, f: &dyn Fn(&mut queue::State, u64)| {
        let before = db.size_on_disk();
        let t = Instant::now();
        for i in 0..rounds {
            db.commit("x", |state| f(state, i)).unwrap();
        }
        println!(
            "split depth={depth} {label}: bytes_per_commit={:.0} commit_us={:.0}",
            (db.size_on_disk() - before) as f64 / rounds as f64,
            t.elapsed().as_micros() as f64 / rounds as f64
        );
    };
    measure("next_id only (root struct + log entry)", &|state, _| state.next_id += 1);
    measure("jobs map only (one job record rewritten)", &|state, i| {
        let id = 7 + (i * 13) % (depth - 20);
        let mut job = (*state.jobs.get(&id).unwrap()).clone();
        job.attempts += 1;
        state.jobs.insert(id, job);
    });
    measure("leases map only (insert one key)", &|state, i| {
        state.leases.insert((10_000_000 + i, "q".to_string(), i));
    });
    measure("ready map only (remove one key)", &|state, i| {
        let id = 11 + (i * 17) % (depth - 20);
        state.ready.remove(&queue::state::ready_key("q", 0, id));
    });
    measure("jobs + ready + leases (a lease)", &|state, i| {
        let id = 100_000 + i;
        state.ready.remove(&queue::state::ready_key("q", 0, id));
        let mut job = (*state.jobs.get(&id).unwrap()).clone();
        job.attempts += 1;
        state.leases.insert((20_000_000 + i, "q".to_string(), id));
        state.jobs.insert(id, job);
    });
}

fn main() {
    let mut argv = std::env::args().skip(1);
    let Some(name) = argv.next() else {
        eprintln!("usage: queue-bench throughput|growth|wake|batching|day [key=value...]");
        std::process::exit(2);
    };
    let args: HashMap<String, String> = argv
        .filter_map(|a| a.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();
    match name.as_str() {
        "throughput" => throughput(&args),
        "growth" => growth(&args),
        "wake" => wake(&args),
        "idle" => idle(&args),
        "batching" => batching(&args),
        "day" => day(&args),
        "cycle" => cycle(&args),
        "hist" => hist(&args),
        "split" => split(&args),
        other => {
            eprintln!("unknown bench {other}");
            std::process::exit(2);
        }
    }
}
