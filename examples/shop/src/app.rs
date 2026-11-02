use std::panic::{AssertUnwindSafe, catch_unwind};
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ferro::{Db, Snapshot};
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot, watch};

use crate::model::Shop;

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    Bad(String),
    Conflict(Value),
    ReadOnly,
    Internal(String),
}

impl From<ferro::Error> for ApiError {
    fn from(error: ferro::Error) -> Self {
        match error {
            ferro::Error::NoVersion(version) => ApiError::NotFound(format!("no version {version}")),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            ApiError::NotFound(what) => (StatusCode::NOT_FOUND, json!({ "error": what })),
            ApiError::Bad(what) => (StatusCode::UNPROCESSABLE_ENTITY, json!({ "error": what })),
            ApiError::Conflict(body) => (StatusCode::CONFLICT, body),
            ApiError::ReadOnly => (
                StatusCode::FORBIDDEN,
                json!({ "error": "read-only replica" }),
            ),
            ApiError::Internal(what) => (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": what })),
        };
        (status, Json(body)).into_response()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    Inline,
    Blocking,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    Inline,
    Blocking,
    Pool(usize),
}

impl FromStr for ReadMode {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "inline" => Ok(ReadMode::Inline),
            "blocking" => Ok(ReadMode::Blocking),
            other => Err(format!("unknown read mode {other}")),
        }
    }
}

impl FromStr for WriteMode {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "inline" => Ok(WriteMode::Inline),
            "blocking" => Ok(WriteMode::Blocking),
            other => match other.strip_prefix("pool:") {
                Some(count) => count
                    .parse()
                    .map(WriteMode::Pool)
                    .map_err(|_| format!("bad pool size {count}")),
                None => Err(format!("unknown write mode {other}")),
            },
        }
    }
}

type Job = Box<dyn FnOnce() + Send>;

pub struct Bridge {
    pub reads: ReadMode,
    pub writes: WriteMode,
    pool: Option<mpsc::Sender<Job>>,
}

impl Bridge {
    pub fn new(reads: ReadMode, writes: WriteMode) -> Self {
        let pool = match writes {
            WriteMode::Pool(size) => {
                let (sender, receiver) = mpsc::channel::<Job>();
                let receiver = Arc::new(Mutex::new(receiver));
                for index in 0..size.max(1) {
                    let receiver = receiver.clone();
                    std::thread::Builder::new()
                        .name(format!("shop-writer-{index}"))
                        .spawn(move || {
                            loop {
                                let job = match receiver.lock() {
                                    Ok(guard) => guard.recv(),
                                    Err(_) => return,
                                };
                                match job {
                                    Ok(job) => job(),
                                    Err(_) => return,
                                }
                            }
                        })
                        .expect("spawn writer");
                }
                Some(sender)
            }
            _ => None,
        };
        Bridge {
            reads,
            writes,
            pool,
        }
    }

    pub async fn read<R, F>(&self, work: F) -> Result<R, ApiError>
    where
        R: Send + 'static,
        F: FnOnce() -> R + Send + 'static,
    {
        match self.reads {
            ReadMode::Inline => Ok(work()),
            ReadMode::Blocking => blocking(work).await,
        }
    }

    pub async fn write<R, F>(&self, work: F) -> Result<R, ApiError>
    where
        R: Send + 'static,
        F: FnOnce() -> R + Send + 'static,
    {
        match (&self.pool, self.writes) {
            (Some(pool), _) => {
                let (reply, wait) = oneshot::channel();
                let job: Job = Box::new(move || {
                    let outcome = catch_unwind(AssertUnwindSafe(work));
                    let _ = reply.send(outcome.ok());
                });
                pool.send(job)
                    .map_err(|_| ApiError::Internal("writer pool closed".into()))?;
                match wait.await {
                    Ok(Some(value)) => Ok(value),
                    _ => Err(ApiError::Internal("writer panicked".into())),
                }
            }
            (None, WriteMode::Inline) => Ok(work()),
            (None, _) => blocking(work).await,
        }
    }
}

pub async fn blocking<R, F>(work: F) -> Result<R, ApiError>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ApiError::Internal(format!("task failed: {error}")))
}

#[derive(Default)]
pub struct Counters {
    pub orders_ok: AtomicU64,
    pub orders_conflict: AtomicU64,
    pub writes: AtomicU64,
    pub not_modified: AtomicU64,
    pub events_sent: AtomicU64,
    pub events_lagged: AtomicU64,
}

pub struct App {
    pub db: Db<Shop>,
    pub bridge: Bridge,
    pub replica: bool,
    pub feed: broadcast::Sender<Arc<crate::events::Frame>>,
    pub counters: Counters,
    pub closing: watch::Sender<bool>,
}

pub type Shared = Arc<App>;

impl App {
    pub fn read_snapshot(&self) -> Snapshot<Shop> {
        self.db.read()
    }

    pub fn writable(&self) -> Result<(), ApiError> {
        if self.replica {
            Err(ApiError::ReadOnly)
        } else {
            Ok(())
        }
    }
}
