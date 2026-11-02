use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ferro::{Data, Map, Record, Table};

use crate::chunk::Chunk;

pub const BUCKET_MS: u64 = 600_000;

pub fn bucket_of(ts: u64) -> u64 {
    ts / BUCKET_MS * BUCKET_MS
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
#[unique(metric, labels)]
pub struct SeriesRec {
    #[key]
    pub id: u64,
    #[index]
    pub metric: String,
    #[index(each)]
    pub pairs: Vec<String>,
    pub labels: Vec<(String, String)>,
}

impl SeriesRec {
    pub fn new(id: u64, key: &SeriesKey) -> SeriesRec {
        SeriesRec {
            id,
            metric: key.metric.clone(),
            pairs: key.labels.iter().map(|(k, v)| format!("{k}={v}")).collect(),
            labels: key.labels.clone(),
        }
    }

    pub fn label(&self, name: &str) -> &str {
        self.labels
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    pub fn key(&self) -> SeriesKey {
        SeriesKey {
            metric: self.metric.clone(),
            labels: self.labels.clone(),
        }
    }
}

impl fmt::Display for SeriesRec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.key())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SeriesKey {
    pub metric: String,
    pub labels: Vec<(String, String)>,
}

impl SeriesKey {
    pub fn new(metric: String, mut labels: Vec<(String, String)>) -> SeriesKey {
        labels.sort();
        SeriesKey { metric, labels }
    }

    pub fn canonical(&self) -> String {
        self.to_string()
    }
}

#[derive(Clone, Debug)]
pub struct SeriesRef(pub Arc<SeriesKey>);

impl SeriesRef {
    pub fn new(key: SeriesKey) -> SeriesRef {
        SeriesRef(Arc::new(key))
    }
}

impl PartialEq for SeriesRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SeriesRef {}

impl std::hash::Hash for SeriesRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

impl fmt::Display for SeriesKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.metric)?;
        if self.labels.is_empty() {
            return Ok(());
        }
        write!(f, "{{")?;
        for (index, (k, v)) in self.labels.iter().enumerate() {
            if index > 0 {
                write!(f, ",")?;
            }
            write!(f, "{k}=\"")?;
            for c in v.chars() {
                match c {
                    '"' => write!(f, "\\\"")?,
                    '\\' => write!(f, "\\\\")?,
                    '\n' => write!(f, "\\n")?,
                    c => write!(f, "{c}")?,
                }
            }
            write!(f, "\"")?;
        }
        write!(f, "}}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Data)]
pub struct Rollup {
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub first: f64,
    pub last: f64,
}

impl Rollup {
    pub fn of(value: f64) -> Rollup {
        Rollup {
            count: 1,
            sum: value,
            min: value,
            max: value,
            first: value,
            last: value,
        }
    }

    pub fn add(&mut self, value: f64) {
        self.count += 1;
        self.sum += value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.last = value;
    }
}

#[derive(Clone, Default, Data)]
pub struct Metrics {
    pub series: Table<SeriesRec>,
    pub next_id: u64,
    pub chunks: Map<u64, Map<u64, Chunk>>,
    pub rollups: Map<u64, Map<(u64, u64), Rollup>>,
    pub raw_floor: u64,
    pub points: u64,
}
