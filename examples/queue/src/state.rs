use ferro::{Bytes, Data, Map, Set};

use crate::error::{QueueError, Result};
use crate::model::{BACKOFF_CAP_MS, Job, JobState, NewJob, Recurring};

pub type ReadyKey = (String, u8, u64);

const MAINTAIN_LIMIT: usize = 4096;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    pub expired: Vec<u64>,
    pub promoted: usize,
    pub spawned: usize,
    pub pruned: usize,
}

impl Tick {
    pub fn is_empty(&self) -> bool {
        self.expired.is_empty() && self.promoted == 0 && self.spawned == 0 && self.pruned == 0
    }
}

#[derive(Clone, Debug, Default, Data)]
pub struct State {
    pub next_id: u64,
    pub jobs: Map<u64, Job>,
    pub ready: Set<ReadyKey>,
    pub delayed: Set<(u64, String, u64)>,
    pub leases: Set<(u64, String, u64)>,
    pub blocked: Set<(String, u64)>,
    pub dead: Set<(String, u64)>,
    pub children: Set<(u64, u64)>,
    pub paused: Set<String>,
    pub idem: Map<(String, String), (u64, u64)>,
    pub idem_exp: Set<(u64, String, String)>,
    pub recurring: Map<String, Recurring>,
    pub rec_due: Set<(u64, String)>,
}

pub fn ready_key(queue: &str, priority: u8, id: u64) -> ReadyKey {
    (queue.to_string(), u8::MAX - priority, id)
}

fn backoff(base_ms: u64, attempts: u32) -> u64 {
    let shift = attempts.saturating_sub(1).min(32);
    base_ms
        .saturating_mul(1u64 << shift)
        .min(BACKOFF_CAP_MS)
}

impl State {
    pub fn due_at(&self) -> Option<u64> {
        [
            self.idem_exp.first().map(|key| key.0),
            self.leases.first().map(|key| key.0),
            self.delayed.first().map(|key| key.0),
            self.rec_due.first().map(|key| key.0),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn maintenance_due(&self, now: u64) -> bool {
        self.due_at().is_some_and(|at| at <= now)
    }

    pub fn ready_in(&self, queue: &str) -> bool {
        if self.paused.contains(queue) {
            return false;
        }
        let low = (queue.to_string(), 0u8, 0u64);
        let high = (queue.to_string(), u8::MAX, u64::MAX);
        self.ready.range::<ReadyKey, _>(low..=high).next().is_some()
    }

    pub fn count_ready(&self, queue: &str) -> usize {
        let low = (queue.to_string(), 0u8, 0u64);
        let high = (queue.to_string(), u8::MAX, u64::MAX);
        self.ready.range::<ReadyKey, _>(low..=high).len()
    }

    pub fn count_dead(&self, queue: &str) -> usize {
        let low = (queue.to_string(), 0u64);
        let high = (queue.to_string(), u64::MAX);
        self.dead.range::<(String, u64), _>(low..=high).len()
    }

    pub fn count_blocked(&self, queue: &str) -> usize {
        let low = (queue.to_string(), 0u64);
        let high = (queue.to_string(), u64::MAX);
        self.blocked.range::<(String, u64), _>(low..=high).len()
    }

    fn place(&mut self, job: &mut Job, now: u64) {
        if job.run_at > now {
            job.state = JobState::Delayed;
            self.delayed.insert((job.run_at, job.queue.clone(), job.id));
        } else {
            job.state = JobState::Ready;
            self.ready.insert(ready_key(&job.queue, job.priority, job.id));
        }
    }

    pub fn enqueue(&mut self, spec: &NewJob, siblings: &[u64], now: u64) -> Result<(u64, bool)> {
        if spec.queue.is_empty() {
            return Err(QueueError::Invalid("queue name is empty".into()));
        }
        if spec.max_attempts == 0 {
            return Err(QueueError::Invalid("max attempts must be at least 1".into()));
        }
        if let Some(key) = &spec.key {
            let slot = (spec.queue.clone(), key.clone());
            if let Some(found) = self.idem.get(&slot) {
                if found.1 > now {
                    return Ok((found.0, true));
                }
                self.idem_exp.remove(&(found.1, slot.0.clone(), slot.1.clone()));
                self.idem.remove(&slot);
            }
        }
        let mut parents = spec.after.clone();
        for index in &spec.after_batch {
            let id = siblings.get(*index).ok_or_else(|| {
                QueueError::Invalid(format!("batch dependency {index} is not an earlier job"))
            })?;
            parents.push(*id);
        }
        let mut live = Vec::new();
        for parent in parents {
            if parent >= self.next_id {
                return Err(QueueError::UnknownDependency(parent));
            }
            if self.jobs.contains_key(&parent) && !live.contains(&parent) {
                live.push(parent);
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut job = Job {
            id,
            queue: spec.queue.clone(),
            priority: spec.priority,
            payload: Bytes::from(spec.payload.clone()),
            state: JobState::Ready,
            attempts: 0,
            max_attempts: spec.max_attempts,
            backoff_ms: spec.backoff_ms,
            run_at: now + spec.delay_ms,
            created_ms: now,
            deps_left: live.len() as u32,
            worker: None,
            expires_ms: 0,
            error: None,
            key: spec.key.clone(),
        };
        if live.is_empty() {
            self.place(&mut job, now);
        } else {
            job.state = JobState::Blocked;
            self.blocked.insert((job.queue.clone(), id));
            for parent in live {
                self.children.insert((parent, id));
            }
        }
        if let Some(key) = &spec.key {
            let expires = now + spec.key_ttl_ms;
            self.idem.insert((spec.queue.clone(), key.clone()), (id, expires));
            self.idem_exp.insert((expires, spec.queue.clone(), key.clone()));
        }
        self.jobs.insert(id, job);
        Ok((id, false))
    }

    pub fn lease(&mut self, queue: &str, worker: &str, ttl_ms: u64, n: usize, now: u64) -> Vec<Job> {
        if self.paused.contains(queue) {
            return Vec::new();
        }
        let low = (queue.to_string(), 0u8, 0u64);
        let high = (queue.to_string(), u8::MAX, u64::MAX);
        let keys: Vec<ReadyKey> = self
            .ready
            .range::<ReadyKey, _>(low..=high)
            .take(n)
            .collect();
        let expires = now + ttl_ms;
        let mut leased = Vec::with_capacity(keys.len());
        let mut lease_edits = Vec::with_capacity(keys.len());
        let mut job_edits = Vec::with_capacity(keys.len());
        for key in &keys {
            let Some(found) = self.jobs.get(&key.2) else {
                continue;
            };
            let mut job = (*found).clone();
            job.state = JobState::Leased;
            job.attempts += 1;
            job.worker = Some(worker.to_string());
            job.expires_ms = expires;
            lease_edits.push(((expires, job.queue.clone(), job.id), true));
            job_edits.push((job.id, Some(job.clone())));
            leased.push(job);
        }
        self.ready.apply(keys.into_iter().map(|key| (key, false)));
        self.leases.apply(lease_edits);
        self.jobs.apply(job_edits);
        leased
    }

    fn held(&self, id: u64, attempt: u32) -> Result<Job> {
        match self.jobs.get(&id) {
            Some(job) if job.state == JobState::Leased && job.attempts == attempt => {
                Ok((*job).clone())
            }
            _ => Err(QueueError::LeaseLost(id)),
        }
    }

    pub fn ack(&mut self, id: u64, attempt: u32, now: u64) -> Result<()> {
        let job = self.held(id, attempt)?;
        self.leases
            .remove(&(job.expires_ms, job.queue.clone(), id));
        self.jobs.remove(&id);
        self.release_children(id, now);
        Ok(())
    }

    pub fn extend(&mut self, id: u64, attempt: u32, ttl_ms: u64, now: u64) -> Result<()> {
        let mut job = self.held(id, attempt)?;
        self.leases
            .remove(&(job.expires_ms, job.queue.clone(), id));
        job.expires_ms = now + ttl_ms;
        self.leases.insert((job.expires_ms, job.queue.clone(), id));
        self.jobs.insert(id, job);
        Ok(())
    }

    pub fn nack(
        &mut self,
        id: u64,
        attempt: u32,
        error: &str,
        retry_in_ms: Option<u64>,
        now: u64,
    ) -> Result<JobState> {
        let job = self.held(id, attempt)?;
        self.leases
            .remove(&(job.expires_ms, job.queue.clone(), id));
        Ok(self.fail(job, error, retry_in_ms, now))
    }

    fn fail(&mut self, mut job: Job, error: &str, retry_in_ms: Option<u64>, now: u64) -> JobState {
        job.worker = None;
        job.expires_ms = 0;
        job.error = Some(error.to_string());
        if job.attempts >= job.max_attempts {
            job.state = JobState::Dead;
            self.dead.insert((job.queue.clone(), job.id));
        } else {
            let delay = retry_in_ms.unwrap_or_else(|| backoff(job.backoff_ms, job.attempts));
            job.run_at = now + delay;
            self.place(&mut job, now);
        }
        let state = job.state;
        self.jobs.insert(job.id, job);
        state
    }

    fn release_children(&mut self, parent: u64, now: u64) {
        let low = (parent, 0u64);
        let high = (parent, u64::MAX);
        let kids: Vec<u64> = self
            .children
            .range::<(u64, u64), _>(low..=high)
            .map(|key| key.1)
            .collect();
        if kids.is_empty() {
            return;
        }
        self.children.remove_range::<(u64, u64), _>(low..=high);
        for kid in kids {
            let Some(found) = self.jobs.get(&kid) else {
                continue;
            };
            let mut job = (*found).clone();
            job.deps_left = job.deps_left.saturating_sub(1);
            if job.deps_left == 0 && job.state == JobState::Blocked {
                self.blocked.remove(&(job.queue.clone(), kid));
                self.place(&mut job, now);
            }
            self.jobs.insert(kid, job);
        }
    }

    pub fn retry_dead(&mut self, id: u64, now: u64) -> Result<()> {
        let found = self.jobs.get(&id).ok_or(QueueError::NotFound(id))?;
        if found.state != JobState::Dead {
            return Err(QueueError::NotDead(id));
        }
        let mut job = (*found).clone();
        self.dead.remove(&(job.queue.clone(), id));
        job.attempts = 0;
        job.run_at = now;
        self.place(&mut job, now);
        self.jobs.insert(id, job);
        Ok(())
    }

    pub fn maintain(&mut self, now: u64) -> Tick {
        let mut tick = Tick::default();
        while tick.pruned < MAINTAIN_LIMIT {
            let Some(key) = self.idem_exp.first() else {
                break;
            };
            if key.0 > now {
                break;
            }
            self.idem_exp.remove(&key);
            self.idem.remove(&(key.1, key.2));
            tick.pruned += 1;
        }
        while tick.expired.len() < MAINTAIN_LIMIT {
            let Some(key) = self.leases.first() else {
                break;
            };
            if key.0 > now {
                break;
            }
            self.leases.remove(&key);
            let Some(found) = self.jobs.get(&key.2) else {
                continue;
            };
            if found.state != JobState::Leased || found.expires_ms != key.0 {
                continue;
            }
            self.fail((*found).clone(), "lease expired", None, now);
            tick.expired.push(key.2);
        }
        while tick.promoted < MAINTAIN_LIMIT {
            let Some(key) = self.delayed.first() else {
                break;
            };
            if key.0 > now {
                break;
            }
            self.delayed.remove(&key);
            let Some(found) = self.jobs.get(&key.2) else {
                continue;
            };
            let mut job = (*found).clone();
            self.place(&mut job, now);
            self.jobs.insert(job.id, job);
            tick.promoted += 1;
        }
        while tick.spawned < MAINTAIN_LIMIT {
            let Some(key) = self.rec_due.first() else {
                break;
            };
            if key.0 > now {
                break;
            }
            self.rec_due.remove(&key);
            let Some(found) = self.recurring.get(&key.1) else {
                continue;
            };
            let mut rec = (*found).clone();
            let spec = NewJob::new(rec.queue.clone(), rec.payload.to_vec())
                .priority(rec.priority)
                .max_attempts(rec.max_attempts)
                .key(format!("rec:{}:{}", rec.name, key.0))
                .key_ttl_ms(rec.every_ms.max(30_000) * 2);
            let _ = self.enqueue(&spec, &[], now);
            let missed = (now - key.0) / rec.every_ms;
            rec.next_run = key.0 + rec.every_ms * (missed + 1);
            self.rec_due.insert((rec.next_run, rec.name.clone()));
            self.recurring.insert(rec.name.clone(), rec);
            tick.spawned += 1;
        }
        tick
    }
}
