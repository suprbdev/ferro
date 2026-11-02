use std::time::{SystemTime, UNIX_EPOCH};

use ferro::{Data, Map, Record, Set, Table};

pub type Body = Map<u64, String>;

#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct Page {
    #[key]
    pub id: u64,
    #[unique]
    pub slug: String,
    pub title: String,
    pub body: Body,
    #[index(each)]
    pub tags: Vec<String>,
    #[index]
    pub author: String,
    pub updated: u64,
}

#[derive(Clone, Default, Data)]
pub struct Wiki {
    pub pages: Table<Page>,
    pub links: Map<String, Set<u64>>,
    pub terms: Map<String, Map<u64, u32>>,
    pub next_id: u64,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
