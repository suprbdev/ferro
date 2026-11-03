use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::error::{Error, Result};

#[derive(Clone, Copy)]
pub(crate) enum Needs<'a> {
    Lane(&'a str),
    Every,
    Tip,
    Ack,
}

struct Hold {
    db: u64,
    lane: usize,
    name: String,
}

struct Holds {
    list: Vec<Hold>,
    live: usize,
}

thread_local! {
    static HOLDS: RefCell<Holds> = const {
        RefCell::new(Holds {
            list: Vec::new(),
            live: 0,
        })
    };
}

static NEXT: AtomicU64 = AtomicU64::new(1);
static CROSSINGS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

pub(crate) fn next_db() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn hold(db: u64, lane: usize, name: &str) {
    let _ = HOLDS.try_with(|holds| {
        let mut holds = holds.borrow_mut();
        let live = holds.live;
        match holds.list.get_mut(live) {
            Some(hold) => {
                hold.db = db;
                hold.lane = lane;
                hold.name.clear();
                hold.name.push_str(name);
            }
            None => holds.list.push(Hold {
                db,
                lane,
                name: name.to_string(),
            }),
        }
        holds.live += 1;
    });
}

pub(crate) fn release(db: u64, lane: usize) {
    let _ = HOLDS.try_with(|holds| {
        let mut holds = holds.borrow_mut();
        let live = holds.live;
        let held = &holds.list[..live];
        if let Some(at) = held
            .iter()
            .rposition(|hold| hold.db == db && hold.lane == lane)
        {
            holds.list.swap(at, live - 1);
            holds.live -= 1;
        }
    });
}

pub(crate) struct Whole(u64);

pub(crate) fn whole(db: u64) -> Whole {
    hold(db, 0, "");
    Whole(db)
}

impl Drop for Whole {
    fn drop(&mut self) {
        release(self.0, 0);
    }
}

pub(crate) struct Crossing(Vec<(u64, u64)>);

impl Drop for Crossing {
    fn drop(&mut self) {
        let mut crossings = CROSSINGS.lock();
        for edge in &self.0 {
            if let Some(at) = crossings.iter().position(|held| held == edge) {
                crossings.swap_remove(at);
            }
        }
    }
}

pub(crate) fn check(db: u64, coalesce: bool, needs: Needs<'_>) -> Result<Option<Crossing>> {
    let checked = HOLDS.try_with(|holds| {
        let holds = holds.borrow();
        if holds.live == 0 {
            return Ok(None);
        }
        let mut from = Vec::new();
        for hold in &holds.list[..holds.live] {
            if hold.db != db {
                if !from.contains(&hold.db) {
                    from.push(hold.db);
                }
                continue;
            }
            let safe = match needs {
                _ if hold.lane == 0 => false,
                Needs::Tip => true,
                _ if coalesce => false,
                Needs::Ack => true,
                Needs::Lane(name) => name > hold.name.as_str(),
                Needs::Every => false,
            };
            if !safe {
                return Err(Error::Reentrant);
            }
        }
        cross(from, db)
    });
    checked.unwrap_or(Ok(None))
}

fn cross(from: Vec<u64>, to: u64) -> Result<Option<Crossing>> {
    if from.is_empty() {
        return Ok(None);
    }
    let mut crossings = CROSSINGS.lock();
    let mut reached = vec![to];
    let mut next = 0;
    while let Some(&at) = reached.get(next) {
        next += 1;
        for &(source, sink) in crossings.iter() {
            if source == at && !reached.contains(&sink) {
                reached.push(sink);
            }
        }
    }
    if from.iter().any(|db| reached.contains(db)) {
        return Err(Error::Reentrant);
    }
    let edges: Vec<(u64, u64)> = from.into_iter().map(|db| (db, to)).collect();
    crossings.extend_from_slice(&edges);
    Ok(Some(Crossing(edges)))
}
