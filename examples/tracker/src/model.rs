use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use ferro::{Data, List, Map, Record, Table};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Data, ValueEnum)]
pub enum Status {
    Open,
    InProgress,
    Closed,
}

impl Status {
    pub const ALL: [Status; 3] = [Status::Open, Status::InProgress, Status::Closed];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::InProgress => "in-progress",
            Status::Closed => "closed",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Data, ValueEnum)]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}

impl Priority {
    pub const ALL: [Priority; 4] = [
        Priority::Low,
        Priority::Medium,
        Priority::High,
        Priority::Critical,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Medium => "medium",
            Priority::High => "high",
            Priority::Critical => "critical",
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Data)]
pub struct Comment {
    pub author: String,
    pub text: String,
    pub at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Data)]
pub struct User {
    pub name: String,
    pub created_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct Issue {
    #[key]
    pub id: u64,
    pub title: String,
    pub body: String,
    #[index]
    pub status: Status,
    #[index]
    pub assignee: Option<String>,
    #[index(each)]
    pub labels: Vec<String>,
    #[index]
    pub priority: Priority,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub comments: List<Comment>,
}

#[derive(Clone, Default, Data)]
pub struct Tracker {
    pub issues: Table<Issue>,
    pub users: Map<String, User>,
    pub next_id: u64,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
