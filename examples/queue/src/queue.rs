use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferro::{Db, Durability, Hash, Options, Watcher};

use crate::error::{QueueError, Result};
use crate::model::{Job, JobState, NewJob, Recurring};
use crate::state::{State, Tick, ready_key};

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0)
    })
}

#[derive(Clone)]
pub struct QueueOptions {
    pub clock: Clock,
    pub durability: Durability,
    pub lock_wait: Duration,
    pub cache_bytes: Option<u64>,
}

impl Default for QueueOptions {
    fn default() -> Self {
        QueueOptions {
            clock: system_clock(),
            durability: Durability::Fsync,
            lock_wait: Duration::from_secs(10),
            cache_bytes: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease {
    pub id: u64,
    pub attempt: u32,
}

#[derive(Clone, Debug)]
pub struct Leased {
    pub job: Job,
}

impl Leased {
    pub fn lease(&self) -> Lease {
        Lease {
            id: self.job.id,
            attempt: self.job.attempts,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Enqueued {
    pub id: u64,
    pub duplicate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stats {
    pub queue: String,
    pub ready: usize,
    pub delayed: usize,
    pub leased: usize,
    pub blocked: usize,
    pub dead: usize,
    pub paused: bool,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub version: u64,
    pub time_ms: u64,
    pub message: String,
    pub job: Option<Job>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replayed {
    pub from_job: u64,
    pub new_job: u64,
    pub duplicate: bool,
}

#[derive(Clone)]
pub struct Queue {
    db: Db<State>,
    clock: Clock,
}

impl Queue {
    pub fn open(path: impl AsRef<Path>, options: QueueOptions) -> Result<Queue> {
        let deadline = Instant::now() + options.lock_wait;
        loop {
            let mut opened = Options::new().durability(options.durability);
            if let Some(bytes) = options.cache_bytes {
                opened = opened.cache_bytes(bytes);
            }
            match opened.open::<State>(path.as_ref()) {
                Ok(db) => {
                    return Ok(Queue {
                        db,
                        clock: options.clock,
                    });
                }
                Err(ferro::Error::Locked) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub fn open_read_only(path: impl AsRef<Path>, options: QueueOptions) -> Result<Queue> {
        let db = Options::new().read_only().open::<State>(path.as_ref())?;
        Ok(Queue {
            db,
            clock: options.clock,
        })
    }

    pub fn db(&self) -> &Db<State> {
        &self.db
    }

    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    pub fn watch(&self) -> Watcher<State> {
        self.db.watch()
    }

    pub fn refresh(&self) -> Result<bool> {
        Ok(self.db.refresh()?)
    }

    pub fn size_on_disk(&self) -> u64 {
        self.db.size_on_disk()
    }

    pub fn version(&self) -> u64 {
        self.db.version()
    }

    pub fn poke(&self) -> Result<()> {
        self.db.update(|state| state.epoch += 1)?;
        Ok(())
    }

    pub fn next_due(&self) -> Option<u64> {
        self.db.read().due_at()
    }

    pub fn enqueue(&self, spec: NewJob) -> Result<Enqueued> {
        let mut done = self.enqueue_batch(vec![spec])?;
        Ok(done.remove(0))
    }

    pub fn enqueue_batch(&self, specs: Vec<NewJob>) -> Result<Vec<Enqueued>> {
        let committed = self.db.try_commit_with::<_, _, QueueError>(|state| {
            let now = self.now();
            let mut ids = Vec::with_capacity(specs.len());
            let mut out = Vec::with_capacity(specs.len());
            for spec in &specs {
                let (id, duplicate) = state.enqueue(spec, &ids, now)?;
                ids.push(id);
                out.push(Enqueued { id, duplicate });
            }
            let message = match specs.as_slice() {
                [only] => format!("enqueue {} #{}", only.queue, ids[0]),
                _ => format!("enqueue {} jobs", specs.len()),
            };
            Ok((message, out))
        })?;
        Ok(committed.value)
    }

    pub fn lease(&self, queue: &str, worker: &str, ttl_ms: u64) -> Result<Option<Leased>> {
        Ok(self.lease_n(queue, worker, ttl_ms, 1)?.pop())
    }

    pub fn lease_n(&self, queue: &str, worker: &str, ttl_ms: u64, n: usize) -> Result<Vec<Leased>> {
        {
            let snapshot = self.db.read();
            if !snapshot.ready_in(queue) && !snapshot.maintenance_due(self.now()) {
                return Ok(Vec::new());
            }
        }
        let committed = self.db.try_commit_with::<_, _, QueueError>(|state| {
            let now = self.now();
            state.maintain(now);
            let jobs = state.lease(queue, worker, ttl_ms, n, now);
            let message = match jobs.as_slice() {
                [] => "maintain".to_string(),
                [only] => format!("lease {queue} #{}", only.id),
                [first, rest @ ..] => format!("lease {queue} #{}+{}", first.id, rest.len()),
            };
            Ok((message, jobs))
        })?;
        Ok(committed
            .value
            .into_iter()
            .map(|job| Leased { job })
            .collect())
    }

    pub fn ack(&self, lease: Lease) -> Result<()> {
        let lost = self.ack_many(&[lease])?;
        match lost.first() {
            Some(id) => Err(QueueError::LeaseLost(*id)),
            None => Ok(()),
        }
    }

    pub fn ack_many(&self, leases: &[Lease]) -> Result<Vec<u64>> {
        let committed = self.db.try_commit_with::<_, _, QueueError>(|state| {
            let now = self.now();
            let mut lost = Vec::new();
            for lease in leases {
                if state.ack(lease.id, lease.attempt, now).is_err() {
                    lost.push(lease.id);
                }
            }
            let message = match leases {
                [only] => format!("ack #{}", only.id),
                _ => format!("ack {} jobs", leases.len()),
            };
            Ok((message, lost))
        })?;
        Ok(committed.value)
    }

    pub fn nack(&self, lease: Lease, error: &str, retry_in_ms: Option<u64>) -> Result<JobState> {
        let committed = self.db.try_commit_with::<_, _, QueueError>(|state| {
            let now = self.now();
            let outcome = state.nack(lease.id, lease.attempt, error, retry_in_ms, now)?;
            Ok((format!("nack #{}", lease.id), outcome))
        })?;
        Ok(committed.value)
    }

    pub fn extend(&self, lease: Lease, ttl_ms: u64) -> Result<()> {
        self.db.try_commit_with::<_, _, QueueError>(|state| {
            state.extend(lease.id, lease.attempt, ttl_ms, self.now())?;
            Ok((format!("extend #{}", lease.id), ()))
        })?;
        Ok(())
    }

    pub fn tick(&self) -> Result<Tick> {
        let committed = self.db.commit_with(|state| {
            let tick = state.maintain(self.now());
            let message = format!(
                "tick expired={} promoted={} spawned={} pruned={}",
                tick.expired.len(),
                tick.promoted,
                tick.spawned,
                tick.pruned
            );
            (message, tick)
        })?;
        Ok(committed.value)
    }

    pub fn get(&self, id: u64) -> Option<Job> {
        self.db.read().jobs.get(&id).map(|job| (*job).clone())
    }

    pub fn pause(&self, queue: &str) -> Result<bool> {
        let committed = self.db.commit_with(|state| {
            let changed = state.paused.insert(queue.to_string());
            (format!("pause {queue}"), changed)
        })?;
        Ok(committed.value)
    }

    pub fn resume(&self, queue: &str) -> Result<bool> {
        let committed = self.db.commit_with(|state| {
            let changed = state.paused.remove(queue);
            (format!("resume {queue}"), changed)
        })?;
        Ok(committed.value)
    }

    pub fn stats(&self, queue: Option<&str>) -> Vec<Stats> {
        let snapshot = self.db.read();
        let names: Vec<String> = match queue {
            Some(name) => vec![name.to_string()],
            None => {
                let mut names: BTreeMap<String, ()> = BTreeMap::new();
                for (_, job) in snapshot.jobs.iter() {
                    names.insert(job.queue.clone(), ());
                }
                for name in snapshot.paused.iter() {
                    names.insert(name, ());
                }
                names.into_keys().collect()
            }
        };
        names
            .into_iter()
            .map(|name| Stats {
                ready: snapshot.count_ready(&name),
                delayed: snapshot.delayed.iter().filter(|key| key.1 == name).count(),
                leased: snapshot.leases.iter().filter(|key| key.1 == name).count(),
                blocked: snapshot.count_blocked(&name),
                dead: snapshot.count_dead(&name),
                paused: snapshot.paused.contains(name.as_str()),
                queue: name,
            })
            .collect()
    }

    pub fn dlq_list(&self, queue: Option<&str>, limit: usize) -> Vec<Job> {
        let snapshot = self.db.read();
        snapshot
            .dead
            .iter()
            .filter(|key| queue.is_none_or(|name| key.0 == name))
            .take(limit)
            .filter_map(|key| snapshot.jobs.get(&key.1))
            .map(|job| (*job).clone())
            .collect()
    }

    pub fn dlq_retry(&self, id: u64) -> Result<()> {
        self.db.try_commit_with::<_, _, QueueError>(|state| {
            state.retry_dead(id, self.now())?;
            Ok((format!("dlq retry #{id}"), ()))
        })?;
        Ok(())
    }

    pub fn dlq_retry_queue(&self, queue: &str) -> Result<usize> {
        let committed = self.db.try_commit_with::<_, _, QueueError>(|state| {
            let now = self.now();
            let ids: Vec<u64> = state
                .dead
                .range::<(String, u64), _>((queue.to_string(), 0u64)..=(queue.to_string(), u64::MAX))
                .map(|key| key.1)
                .collect();
            for id in &ids {
                state.retry_dead(*id, now)?;
            }
            Ok((format!("dlq retry {queue} x{}", ids.len()), ids.len()))
        })?;
        Ok(committed.value)
    }

    pub fn recurring_add(&self, rec: Recurring) -> Result<()> {
        if rec.every_ms == 0 {
            return Err(QueueError::Invalid("interval must be positive".into()));
        }
        self.db.try_commit_with::<_, _, QueueError>(|state| {
            if let Some(old) = state.recurring.get(&rec.name) {
                state.rec_due.remove(&(old.next_run, rec.name.clone()));
            }
            state.rec_due.insert((rec.next_run, rec.name.clone()));
            let message = format!("recurring add {}", rec.name);
            state.recurring.insert(rec.name.clone(), rec);
            Ok((message, ()))
        })?;
        Ok(())
    }

    pub fn recurring_remove(&self, name: &str) -> Result<bool> {
        let committed = self.db.commit_with(|state| {
            let removed = match state.recurring.remove(name) {
                Some(old) => {
                    state.rec_due.remove(&(old.next_run, name.to_string()));
                    true
                }
                None => false,
            };
            (format!("recurring remove {name}"), removed)
        })?;
        Ok(committed.value)
    }

    pub fn recurring_list(&self) -> Vec<Recurring> {
        self.db
            .read()
            .recurring
            .values()
            .map(|rec| (*rec).clone())
            .collect()
    }

    fn job_hash_at(&self, version: u64, id: u64) -> Result<Option<Hash>> {
        Ok(self.db.at(version)?.jobs.value_hash(&id))
    }

    pub fn history(&self, id: u64) -> Result<Vec<Event>> {
        let tip = self.db.read();
        if id >= tip.next_id {
            return Err(QueueError::NotFound(id));
        }
        let last = tip.version();
        let first = self.db.log().first().map(|(version, _)| version).unwrap_or(1);
        let (mut low, mut high) = (first, last);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.db.at(mid)?.next_id > id {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        let created = low;
        let Some(created_hash) = self.job_hash_at(created, id)? else {
            return Err(QueueError::NotFound(id));
        };
        let end = if tip.jobs.contains_key(&id) {
            last + 1
        } else {
            let (mut low, mut high) = (created, last);
            while low < high {
                let mid = low + (high - low) / 2;
                if self.job_hash_at(mid, id)?.is_none() {
                    high = mid;
                } else {
                    low = mid + 1;
                }
            }
            low
        };
        let mut versions = vec![created];
        if end - 1 > created {
            let end_hash = self.job_hash_at(end - 1, id)?;
            self.changes(id, (created, created_hash), (end - 1, end_hash.unwrap_or(created_hash)), &mut versions)?;
        }
        if end <= last {
            versions.push(end);
        }
        versions.sort_unstable();
        versions.dedup();
        versions
            .into_iter()
            .map(|version| {
                let snapshot = self.db.at(version)?;
                Ok(Event {
                    version,
                    time_ms: snapshot.commit().time_ms,
                    message: snapshot.commit().message.clone(),
                    job: snapshot.jobs.get(&id).map(|job| (*job).clone()),
                })
            })
            .collect()
    }

    fn changes(
        &self,
        id: u64,
        low: (u64, Hash),
        high: (u64, Hash),
        out: &mut Vec<u64>,
    ) -> Result<()> {
        if low.1 == high.1 {
            return Ok(());
        }
        if high.0 == low.0 + 1 {
            out.push(high.0);
            return Ok(());
        }
        let mid = low.0 + (high.0 - low.0) / 2;
        let Some(mid_hash) = self.job_hash_at(mid, id)? else {
            return Ok(());
        };
        self.changes(id, low, (mid, mid_hash), out)?;
        self.changes(id, (mid, mid_hash), high, out)
    }

    pub fn replay(&self, from: u64, include_pending: bool, force: bool) -> Result<Vec<Replayed>> {
        let past = self.db.at(from)?;
        let mut originals: Vec<Job> = past
            .leases
            .iter()
            .filter_map(|key| past.jobs.get(&key.2))
            .map(|job| (*job).clone())
            .collect();
        if include_pending {
            originals.extend(
                past.jobs
                    .values()
                    .filter(|job| job.state != JobState::Leased && job.state != JobState::Dead)
                    .map(|job| (*job).clone()),
            );
        }
        originals.sort_by_key(|job| job.id);
        let committed = self.db.try_commit_with::<_, _, QueueError>(|state| {
            let now = self.now();
            let mut out = Vec::new();
            for job in &originals {
                if !force && state.jobs.contains_key(&job.id) {
                    continue;
                }
                let spec = NewJob::new(job.queue.clone(), job.payload.to_vec())
                    .priority(job.priority)
                    .max_attempts(job.max_attempts)
                    .backoff_ms(job.backoff_ms)
                    .key(format!("replay:{from}:{}", job.id))
                    .key_ttl_ms(7 * crate::model::DAY_MS);
                let (new_job, duplicate) = state.enqueue(&spec, &[], now)?;
                out.push(Replayed {
                    from_job: job.id,
                    new_job,
                    duplicate,
                });
            }
            Ok((format!("replay from v{from} x{}", out.len()), out))
        })?;
        Ok(committed.value)
    }

    pub fn ready_head(&self, queue: &str) -> Option<u64> {
        let snapshot = self.db.read();
        let low = ready_key(queue, u8::MAX, 0);
        let high = ready_key(queue, 0, u64::MAX);
        snapshot
            .ready
            .range::<(String, u8, u64), _>(low..=high)
            .next()
            .map(|key| key.2)
    }
}
