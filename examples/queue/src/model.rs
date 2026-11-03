use std::fmt;

use ferro::{Bytes, Data};

pub const DAY_MS: u64 = 86_400_000;
pub const DEFAULT_PRIORITY: u8 = 100;
pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;
pub const DEFAULT_BACKOFF_MS: u64 = 1_000;
pub const BACKOFF_CAP_MS: u64 = 3_600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Data)]
pub enum JobState {
    Ready,
    Delayed,
    Leased,
    Blocked,
    Dead,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Ready => "ready",
            JobState::Delayed => "delayed",
            JobState::Leased => "leased",
            JobState::Blocked => "blocked",
            JobState::Dead => "dead",
        }
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Data)]
pub struct Job {
    pub id: u64,
    pub queue: String,
    pub priority: u8,
    pub payload: Bytes,
    pub state: JobState,
    pub attempts: u32,
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub run_at: u64,
    pub created_ms: u64,
    pub deps_left: u32,
    pub worker: Option<String>,
    pub expires_ms: u64,
    pub error: Option<String>,
    pub key: Option<String>,
}

impl fmt::Display for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "job={} queue={} state={} priority={} attempts={}/{}",
            self.id, self.queue, self.state, self.priority, self.attempts, self.max_attempts
        )?;
        if let Some(worker) = &self.worker {
            write!(f, " worker={worker} expires_ms={}", self.expires_ms)?;
        }
        if self.state == JobState::Delayed {
            write!(f, " run_at={}", self.run_at)?;
        }
        if self.deps_left > 0 {
            write!(f, " deps_left={}", self.deps_left)?;
        }
        if let Some(error) = &self.error {
            write!(f, " error={error:?}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Data)]
pub struct Recurring {
    pub name: String,
    pub queue: String,
    pub priority: u8,
    pub payload: Bytes,
    pub every_ms: u64,
    pub next_run: u64,
    pub max_attempts: u32,
}

#[derive(Clone, Debug)]
pub struct NewJob {
    pub queue: String,
    pub payload: Vec<u8>,
    pub priority: u8,
    pub delay_ms: u64,
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub key: Option<String>,
    pub key_ttl_ms: u64,
    pub after: Vec<u64>,
    pub after_batch: Vec<usize>,
}

impl NewJob {
    pub fn new(queue: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        NewJob {
            queue: queue.into(),
            payload: payload.into(),
            priority: DEFAULT_PRIORITY,
            delay_ms: 0,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            backoff_ms: DEFAULT_BACKOFF_MS,
            key: None,
            key_ttl_ms: DAY_MS,
            after: Vec::new(),
            after_batch: Vec::new(),
        }
    }

    pub fn priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }

    pub fn delay_ms(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    pub fn max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    pub fn backoff_ms(mut self, backoff_ms: u64) -> Self {
        self.backoff_ms = backoff_ms;
        self
    }

    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn key_ttl_ms(mut self, key_ttl_ms: u64) -> Self {
        self.key_ttl_ms = key_ttl_ms;
        self
    }

    pub fn after(mut self, parents: impl IntoIterator<Item = u64>) -> Self {
        self.after.extend(parents);
        self
    }

    pub fn after_batch(mut self, siblings: impl IntoIterator<Item = usize>) -> Self {
        self.after_batch.extend(siblings);
        self
    }
}
