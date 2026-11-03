use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use queue::{JobState, NewJob, Queue, QueueOptions, Runner, RunnerConfig};

fn open(dir: &tempfile::TempDir) -> Queue {
    Queue::open(dir.path().join("q.ferro"), QueueOptions::default()).unwrap()
}

fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    done()
}

#[test]
fn exactly_once_lease_with_many_producers_and_workers() {
    const PRODUCERS: usize = 4;
    const PER_PRODUCER: usize = 300;
    const WORKERS: usize = 8;
    let dir = tempfile::tempdir().unwrap();
    let queue = open(&dir);
    let total = PRODUCERS * PER_PRODUCER;
    let executions: Arc<Mutex<HashMap<String, u32>>> = Arc::default();
    let held: Arc<Mutex<HashSet<u64>>> = Arc::default();
    let acked = Arc::new(AtomicUsize::new(0));

    let mut workers = Vec::new();
    for w in 0..WORKERS {
        let queue = queue.clone();
        let executions = executions.clone();
        let held = held.clone();
        let acked = acked.clone();
        workers.push(thread::spawn(move || {
            let watcher = queue.watch();
            let name = format!("w{w}");
            while acked.load(Ordering::SeqCst) < total {
                let batch = 1 + w % 3;
                let leased = queue.lease_n("jobs", &name, 60_000, batch).unwrap();
                if leased.is_empty() {
                    let _ = watcher.recv_timeout(Duration::from_millis(500));
                    continue;
                }
                for item in &leased {
                    assert!(held.lock().unwrap().insert(item.job.id), "job {} leased twice", item.job.id);
                    let payload = String::from_utf8(item.job.payload.to_vec()).unwrap();
                    *executions.lock().unwrap().entry(payload).or_default() += 1;
                }
                let leases: Vec<_> = leased.iter().map(|l| l.lease()).collect();
                for lease in &leases {
                    held.lock().unwrap().remove(&lease.id);
                }
                let lost = queue.ack_many(&leases).unwrap();
                assert!(lost.is_empty());
                acked.fetch_add(leases.len(), Ordering::SeqCst);
            }
        }));
    }

    let mut producers = Vec::new();
    for p in 0..PRODUCERS {
        let queue = queue.clone();
        producers.push(thread::spawn(move || {
            let mut sent = 0;
            while sent < PER_PRODUCER {
                let size = if sent % 2 == 0 { 1 } else { 5.min(PER_PRODUCER - sent) };
                let specs = (0..size)
                    .map(|i| NewJob::new("jobs", format!("p{p}-{}", sent + i)).priority(((sent + i) % 5) as u8))
                    .collect();
                let done = queue.enqueue_batch(specs).unwrap();
                assert_eq!(done.len(), size);
                sent += size;
            }
        }));
    }

    for producer in producers {
        producer.join().unwrap();
    }
    for worker in workers {
        worker.join().unwrap();
    }

    let executions = executions.lock().unwrap();
    assert_eq!(executions.len(), total, "a job was lost");
    assert!(executions.values().all(|n| *n == 1), "a job ran twice");
    let state = queue.db().read();
    assert_eq!(state.jobs.len(), 0);
    assert_eq!(state.ready.len(), 0);
    assert_eq!(state.leases.len(), 0);
    assert_eq!(state.next_id as usize, total);
}

#[test]
fn only_expiry_causes_a_second_run() {
    const TOTAL: usize = 120;
    let dir = tempfile::tempdir().unwrap();
    let queue = open(&dir);
    let stalled: Arc<Mutex<HashSet<String>>> = Arc::default();
    let executions: Arc<Mutex<HashMap<String, u32>>> = Arc::default();
    let handler = {
        let stalled = stalled.clone();
        let executions = executions.clone();
        Arc::new(move |job: &queue::Job| {
            let payload = String::from_utf8(job.payload.to_vec()).unwrap();
            *executions.lock().unwrap().entry(payload.clone()).or_default() += 1;
            let index: usize = payload.parse().unwrap();
            if job.attempts == 1 && index.is_multiple_of(7) {
                stalled.lock().unwrap().insert(payload);
                thread::sleep(Duration::from_millis(400));
            }
            Ok(())
        })
    };
    let mut config = RunnerConfig::new(vec!["jobs".into()], 6);
    config.ttl_ms = 150;
    config.idle_wait = Duration::from_secs(60);
    let runner = Runner::start(queue.clone(), config, handler);
    let specs = (0..TOTAL)
        .map(|i| NewJob::new("jobs", format!("{i}")).backoff_ms(5))
        .collect();
    queue.enqueue_batch(specs).unwrap();

    let finished = wait_for(Duration::from_secs(30), || queue.db().read().jobs.is_empty());
    assert!(finished, "queue did not drain");
    let stalled = stalled.lock().unwrap().clone();
    assert!(!stalled.is_empty());
    let executions = executions.lock().unwrap();
    assert_eq!(executions.len(), TOTAL, "a job was lost");
    for (payload, runs) in executions.iter() {
        if stalled.contains(payload) {
            assert!(*runs >= 2, "{payload} stalled but was not redelivered");
        } else {
            assert_eq!(*runs, 1, "{payload} ran twice without a lease expiring");
        }
    }
    thread::sleep(Duration::from_millis(500));
    assert_eq!(runner.done() as usize, TOTAL);
    assert!(runner.lost() as usize >= stalled.len());
    runner.stop();
}

#[test]
fn runner_wakes_on_enqueue_and_on_delayed_due() {
    let dir = tempfile::tempdir().unwrap();
    let queue = open(&dir);
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let handler = {
        let seen = seen.clone();
        Arc::new(move |job: &queue::Job| {
            seen.lock().unwrap().push(String::from_utf8(job.payload.to_vec()).unwrap());
            Ok(())
        })
    };
    let mut config = RunnerConfig::new(vec!["jobs".into()], 2);
    config.idle_wait = Duration::from_secs(600);
    let runner = Runner::start(queue.clone(), config, handler);
    thread::sleep(Duration::from_millis(100));

    let start = Instant::now();
    queue.enqueue(NewJob::new("jobs", "now")).unwrap();
    assert!(wait_for(Duration::from_secs(2), || seen.lock().unwrap().len() == 1));
    assert!(start.elapsed() < Duration::from_secs(2));

    queue.enqueue(NewJob::new("jobs", "later").delay_ms(300)).unwrap();
    assert!(wait_for(Duration::from_secs(5), || seen.lock().unwrap().len() == 2));
    assert_eq!(seen.lock().unwrap()[1], "later");
    runner.stop();
}

#[test]
fn runner_nacks_failures_and_dead_letters_panics() {
    let dir = tempfile::tempdir().unwrap();
    let queue = open(&dir);
    let handler = Arc::new(|job: &queue::Job| {
        if job.payload.as_ref() == b"panic" {
            panic!("boom");
        }
        Err("always fails".to_string())
    });
    let mut config = RunnerConfig::new(vec!["jobs".into()], 2);
    config.idle_wait = Duration::from_secs(600);
    let runner = Runner::start(queue.clone(), config, handler);
    queue
        .enqueue_batch(vec![
            NewJob::new("jobs", "fail").max_attempts(2).backoff_ms(5),
            NewJob::new("jobs", "panic").max_attempts(1),
        ])
        .unwrap();
    assert!(wait_for(Duration::from_secs(10), || queue.dlq_list(None, 10).len() == 2));
    let dead = queue.dlq_list(None, 10);
    assert!(dead.iter().all(|job| job.state == JobState::Dead));
    assert!(dead.iter().any(|job| job.error.as_deref() == Some("handler panicked")));
    assert!(dead.iter().any(|job| job.error.as_deref() == Some("always fails") && job.attempts == 2));
    runner.stop();
}

#[test]
fn stop_returns_promptly_when_idle() {
    let dir = tempfile::tempdir().unwrap();
    let queue = open(&dir);
    let mut config = RunnerConfig::new(vec!["jobs".into()], 4);
    config.idle_wait = Duration::from_secs(600);
    let runner = Runner::start(queue, config, Arc::new(|_| Ok(())));
    thread::sleep(Duration::from_millis(100));
    let start = Instant::now();
    let flag = Arc::new(AtomicBool::new(false));
    let seen = flag.clone();
    let handle = thread::spawn(move || {
        runner.stop();
        seen.store(true, Ordering::SeqCst);
    });
    assert!(wait_for(Duration::from_secs(5), || flag.load(Ordering::SeqCst)));
    handle.join().unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn stopping_an_idle_runner_commits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let queue = open(&dir);
    queue.enqueue(NewJob::new("elsewhere", "x")).unwrap();
    let before = queue.version();
    let mut config = RunnerConfig::new(vec!["jobs".into()], 4);
    config.idle_wait = Duration::from_secs(600);
    let runner = Runner::start(queue.clone(), config, Arc::new(|_| Ok(())));
    thread::sleep(Duration::from_millis(50));
    runner.stop();
    assert_eq!(queue.version(), before);
}

#[test]
fn a_parked_reaper_wakes_for_something_due_sooner() {
    let dir = tempfile::tempdir().unwrap();
    let queue = open(&dir);
    queue.enqueue(NewJob::new("jobs", "much later").delay_ms(3_600_000)).unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let handler = {
        let seen = seen.clone();
        Arc::new(move |job: &queue::Job| {
            seen.lock().unwrap().push(String::from_utf8(job.payload.to_vec()).unwrap());
            Ok(())
        })
    };
    let mut config = RunnerConfig::new(vec!["jobs".into()], 2);
    config.idle_wait = Duration::from_secs(600);
    let runner = Runner::start(queue.clone(), config, handler);
    thread::sleep(Duration::from_millis(100));
    queue.enqueue(NewJob::new("jobs", "soon").delay_ms(150)).unwrap();
    assert!(wait_for(Duration::from_secs(5), || seen.lock().unwrap().len() == 1));
    assert_eq!(seen.lock().unwrap()[0], "soon");
    runner.stop();
}
