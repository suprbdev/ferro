use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::model::Job;
use crate::queue::{Lease, Queue};

pub type Handler = Arc<dyn Fn(&Job) -> Result<(), String> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct RunnerConfig {
    pub queues: Vec<String>,
    pub workers: usize,
    pub batch: usize,
    pub ttl_ms: u64,
    pub name: String,
    pub idle_wait: Duration,
}

impl RunnerConfig {
    pub fn new(queues: Vec<String>, workers: usize) -> Self {
        RunnerConfig {
            queues,
            workers,
            batch: 1,
            ttl_ms: 30_000,
            name: format!("worker-{}", std::process::id()),
            idle_wait: Duration::from_secs(30),
        }
    }
}

#[derive(Default)]
struct Counters {
    done: AtomicU64,
    failed: AtomicU64,
    lost: AtomicU64,
}

pub struct Runner {
    queue: Queue,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    threads: Vec<JoinHandle<()>>,
}

impl Runner {
    pub fn start(queue: Queue, config: RunnerConfig, handler: Handler) -> Runner {
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let mut threads = Vec::new();
        for index in 0..config.workers {
            let queue = queue.clone();
            let config = config.clone();
            let handler = handler.clone();
            let stop = stop.clone();
            let counters = counters.clone();
            let handle = thread::Builder::new()
                .name(format!("{}-{index}", config.name))
                .spawn(move || work(queue, config, index, handler, stop, counters))
                .expect("spawn worker");
            threads.push(handle);
        }
        {
            let queue = queue.clone();
            let stop = stop.clone();
            let handle = thread::Builder::new()
                .name("reaper".into())
                .spawn(move || reap(queue, stop))
                .expect("spawn reaper");
            threads.push(handle);
        }
        Runner {
            queue,
            stop,
            counters,
            threads,
        }
    }

    pub fn done(&self) -> u64 {
        self.counters.done.load(Ordering::Relaxed)
    }

    pub fn failed(&self) -> u64 {
        self.counters.failed.load(Ordering::Relaxed)
    }

    pub fn lost(&self) -> u64 {
        self.counters.lost.load(Ordering::Relaxed)
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.queue.poke();
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

fn work(
    queue: Queue,
    config: RunnerConfig,
    index: usize,
    handler: Handler,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
) {
    let watcher = queue.watch();
    let name = format!("{}-{index}", config.name);
    while !stop.load(Ordering::SeqCst) {
        let mut busy = false;
        for name_of_queue in &config.queues {
            let leased = match queue.lease_n(name_of_queue, &name, config.ttl_ms, config.batch) {
                Ok(leased) => leased,
                Err(_) => return,
            };
            if leased.is_empty() {
                continue;
            }
            busy = true;
            let mut acks: Vec<Lease> = Vec::with_capacity(leased.len());
            for item in &leased {
                let outcome = catch_unwind(AssertUnwindSafe(|| handler(&item.job)))
                    .unwrap_or_else(|_| Err("handler panicked".to_string()));
                match outcome {
                    Ok(()) => acks.push(item.lease()),
                    Err(message) => {
                        counters.failed.fetch_add(1, Ordering::Relaxed);
                        if queue.nack(item.lease(), &message, None).is_err() {
                            counters.lost.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            if acks.is_empty() {
                continue;
            }
            match queue.ack_many(&acks) {
                Ok(lost) => {
                    counters.lost.fetch_add(lost.len() as u64, Ordering::Relaxed);
                    counters
                        .done
                        .fetch_add((acks.len() - lost.len()) as u64, Ordering::Relaxed);
                }
                Err(_) => return,
            }
        }
        if !busy && watcher.recv_timeout(config.idle_wait).is_err_and(|e| {
            matches!(e, std::sync::mpsc::RecvTimeoutError::Disconnected)
        }) {
            return;
        }
    }
}

fn reap(queue: Queue, stop: Arc<AtomicBool>) {
    let watcher = queue.watch();
    while !stop.load(Ordering::SeqCst) {
        let wait = match queue.next_due() {
            Some(at) => {
                let now = queue.now();
                if at <= now {
                    if queue.tick().is_err() {
                        return;
                    }
                    continue;
                }
                Duration::from_millis((at - now).min(3_600_000))
            }
            None => Duration::from_secs(3600),
        };
        if watcher
            .recv_timeout(wait)
            .is_err_and(|e| matches!(e, std::sync::mpsc::RecvTimeoutError::Disconnected))
        {
            return;
        }
    }
}
