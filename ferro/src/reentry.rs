//! The record of the writer locks each thread holds, behind [`Error::Reentrant`].
//!
//! A thread notes each branch lock, whole-file lock and reclamation cycle lock it takes, and every
//! call that could wait on a lock checks the list first. A call that could deadlock on a lock this
//! thread holds, or on a write waiting for this thread, fails with [`Error::Reentrant`] instead of
//! hanging. A thread that writes into one file while it holds locks in another records the
//! crossing, so that two threads crossing between the same files in opposite directions are
//! refused as well.
//! [Writes from inside a closure](https://github.com/archlemon/ferro/blob/main/docs/internals/reentrancy.md)
//! describes the rules.

use std::cell::RefCell;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use xxhash_rust::xxh3::xxh3_64;

use crate::error::{Error, Result};
use crate::store::lane_of;

/// The locks a call will wait on, for [`check`].
#[derive(Clone, Copy)]
pub(crate) enum Needs<'a> {
    /// The writer lock of the named branch.
    Lane(&'a str),
    /// The writer lock of every branch of the file, as a cycle or a landing bundle takes.
    Every,
    /// The file's reclamation cycle lock and then the writer lock of every branch, as a cycle and
    /// a landing bundle that spilled take.
    Cycle,
    /// Only the file's tip lock, briefly.
    Tip,
    /// The acknowledgement of a commit whose closure has already run.
    Ack,
}

struct Hold {
    db: u64,
    file: u64,
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

/// The lane number that stands for a database's reclamation cycle lock, which [`cycle`] records.
const CYCLE: usize = usize::MAX;

static NEXT: AtomicU64 = AtomicU64::new(1);
static CROSSINGS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// A new id for an opened database, unique within the process.
pub(crate) fn next_db() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Records that this thread holds lock `lane`, of the branch `name`, in database `db`, whose
/// file [`file_id`] names `file`.
///
/// Lane 0 stands for the whole file, which [`whole`] records, and [`CYCLE`] for the reclamation
/// cycle lock, which [`cycle`] records.
pub(crate) fn hold(db: u64, file: u64, lane: usize, name: &str) {
    let _ = HOLDS.try_with(|holds| {
        let mut holds = holds.borrow_mut();
        let live = holds.live;
        match holds.list.get_mut(live) {
            Some(hold) => {
                hold.db = db;
                hold.file = file;
                hold.lane = lane;
                hold.name.clear();
                hold.name.push_str(name);
            }
            None => holds.list.push(Hold {
                db,
                file,
                lane,
                name: name.to_string(),
            }),
        }
        holds.live += 1;
    });
}

/// Removes the newest record that this thread holds lock `lane` in database `db`.
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

/// The id that [`hold`] records for the database file at the canonical path `path`, the same for
/// every handle on that file.
pub(crate) fn file_id(path: &Path) -> u64 {
    xxh3_64(path.as_os_str().as_encoded_bytes())
}

/// Whether this thread holds any lock it records on the file `file`, through any handle, other
/// than the branch lock of `name` in database `db`.
///
/// A thread that does may hold what another process waiting for every branch lock of that file
/// waits for, so it does not wait at that process's gate. Locks on other files do not count, since
/// that process waits only for this file's. A thread whose record cannot be read counts as holding
/// one.
pub(crate) fn holds_besides(db: u64, file: u64, name: &str) -> bool {
    HOLDS
        .try_with(|holds| {
            let holds = holds.borrow();
            holds.list[..holds.live].iter().any(|hold| {
                hold.file == file
                    && (hold.db != db || hold.lane == 0 || hold.lane == CYCLE || hold.name != name)
            })
        })
        .unwrap_or(true)
}

/// A record that this thread holds a lock on all of a database, removed when dropped.
pub(crate) struct Whole(u64);

/// Records that this thread holds a lock on all of database `db`, of the file `file`, until the
/// guard is dropped.
pub(crate) fn whole(db: u64, file: u64) -> Whole {
    hold(db, file, 0, "");
    Whole(db)
}

impl Drop for Whole {
    fn drop(&mut self) {
        release(self.0, 0);
    }
}

/// A record that this thread holds the reclamation cycle lock of a database, removed when dropped.
pub(crate) struct Cycle(u64);

/// Records that this thread holds the reclamation cycle lock of database `db`, of the file `file`,
/// until the guard is dropped.
///
/// A cycle holds it without any branch lock while it copies the file and once it has swapped it
/// in, and a landing bundle that spilled while it places its pieces, so the record refuses only
/// [`Needs::Cycle`].
pub(crate) fn cycle(db: u64, file: u64) -> Cycle {
    hold(db, file, CYCLE, "");
    Cycle(db)
}

impl Drop for Cycle {
    fn drop(&mut self) {
        release(self.0, CYCLE);
    }
}

/// The crossings a call recorded from the databases this thread holds locks in to the one it
/// entered, removed when dropped.
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

/// Checks that a call on database `db` that waits on `needs` cannot deadlock on this thread's
/// locks, and records its crossing from other databases while it runs.
///
/// A whole-file hold on `db` refuses everything. A branch lock refuses [`Needs::Every`] and
/// [`Needs::Cycle`], and a [`Needs::Lane`] whose name does not sort after the held branch's, since
/// cycles take branches in name order. With `coalesce` a branch lock also refuses every lane and
/// [`Needs::Ack`], since a group may be waiting for this thread. [`Needs::Tip`] is safe under any
/// branch lock. The cycle lock refuses only [`Needs::Cycle`].
///
/// `file` is the [`file_id`] of `db` when it locks its file against other opens, as a shared
/// one does. Another open of that file locks it separately, so a lock this thread holds through
/// that open is one the call would wait for in the kernel: a branch lock refuses a
/// [`Needs::Lane`] on the same byte, [`Needs::Every`] and [`Needs::Cycle`], and the cycle lock
/// refuses [`Needs::Cycle`]. Its other locks count only as a crossing, as those of other files
/// do.
///
/// # Errors
///
/// [`Error::Reentrant`] where the call could deadlock, including when `db` already reaches back,
/// through crossings other threads hold, to a database this thread holds locks in.
pub(crate) fn check(
    db: u64,
    file: Option<u64>,
    coalesce: bool,
    needs: Needs<'_>,
) -> Result<Option<Crossing>> {
    let checked = HOLDS.try_with(|holds| {
        let holds = holds.borrow();
        if holds.live == 0 {
            return Ok(None);
        }
        let mut from = Vec::new();
        for hold in &holds.list[..holds.live] {
            if hold.db != db {
                if file == Some(hold.file) && waits_across(hold, needs) {
                    return Err(Error::Reentrant);
                }
                if !from.contains(&hold.db) {
                    from.push(hold.db);
                }
                continue;
            }
            let safe = match needs {
                _ if hold.lane == 0 => false,
                Needs::Cycle => false,
                _ if hold.lane == CYCLE => true,
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

/// Whether a call that waits on `needs` through one open of a file would wait in the kernel for
/// `hold`, a lock this thread holds through another open of it.
fn waits_across(hold: &Hold, needs: Needs<'_>) -> bool {
    match hold.lane {
        0 => false,
        CYCLE => matches!(needs, Needs::Cycle),
        _ => match needs {
            Needs::Lane(name) => lane_of(name) == lane_of(&hold.name),
            Needs::Every | Needs::Cycle => true,
            Needs::Tip | Needs::Ack => false,
        },
    }
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
