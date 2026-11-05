//! A durable job queue on ferro, with priorities, delays, retries, dependencies and recurring jobs.
//!
//! [`State`] is the database root, built from maps and sets rather than tables: the jobs by id, and
//! sets that order the ready jobs by queue and priority, the delayed jobs and the leases by due
//! time, and the blocked and dead jobs by queue. A worker leases the head of a queue for a time to
//! live and acks or nacks it with its [`Lease`], so a worker whose lease lapsed and was taken over
//! cannot finish the job. [`Queue`] opens the database with commit coalescing on by default, waits
//! for a writer lock another process holds, and offers filtered watchers that wake when a queue has
//! ready jobs or something falls due. [`Runner`] runs worker threads on those watchers, and a
//! reaper thread that runs [`Queue::tick`] when maintenance falls due. [`Queue::history`] finds
//! every state one job passed through with `bisect` and `bisect_changes`, and [`Queue::replay`]
//! re-enqueues the jobs that were in flight at a past version. The `queue` and `queue-bench`
//! binaries and the tests use this library.
//!
//! ```sh
//! cargo run --release -p queue --bin queue -- enqueue emails hello --priority 10
//! make queue-bench ARGS='throughput secs=5'
//! ```

pub mod error;
pub mod model;
pub mod queue;
pub mod runner;
pub mod state;

pub use error::QueueError;
pub use model::{Job, JobState, NewJob, Recurring};
pub use queue::{
    Clock, Enqueued, Event, Lease, Leased, Queue, QueueOptions, Replayed, Stats, system_clock,
};
pub use runner::{Runner, RunnerConfig};
pub use state::{State, Tick};
