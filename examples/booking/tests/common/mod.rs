#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use booking::date::{DAY_MS, night_of};
use booking::seed::{Plan, Seeded, seed};
use booking::{Config, HoldRequest, Night, Reservations, walk};
use tempfile::TempDir;

pub const START_MS: u64 = 1_893_456_000_000;

pub struct Manual(Arc<AtomicU64>);

impl Manual {
    pub fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }

    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    pub fn today(&self) -> Night {
        night_of(self.now())
    }
}

pub struct World {
    pub dir: TempDir,
    pub svc: Reservations,
    pub clock: Manual,
    pub seeded: Seeded,
}

impl World {
    pub fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("hotels.ferro")
    }

    pub fn hold(&self, customer: u32, room: u32, first: Night, nights: u32, ttl_ms: u64) -> booking::Result<booking::Held> {
        self.svc.hold(&HoldRequest {
            customer,
            room,
            first,
            nights,
            ttl_ms,
        })
    }
}

pub fn open(path: &Path, coalesce: bool) -> Reservations {
    Reservations::open(path, Config { coalesce, read_only: false }).unwrap()
}

pub fn world_with(plan: Plan, coalesce: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let svc = open(&dir.path().join("hotels.ferro"), coalesce);
    let ms = Arc::new(AtomicU64::new(START_MS));
    let svc = {
        let ms = ms.clone();
        svc.with_clock(Arc::new(move || ms.load(Ordering::SeqCst)))
    };
    let seeded = seed(&svc, &plan).unwrap();
    World {
        dir,
        svc,
        clock: Manual(ms),
        seeded,
    }
}

pub fn world() -> World {
    world_with(Plan::default(), false)
}

pub fn days(n: u64) -> u64 {
    n * DAY_MS
}

pub fn assert_clean(svc: &Reservations) -> booking::Walked {
    let walked = walk(svc.db(), 1).unwrap();
    assert!(
        walked.ok(),
        "invariant violations: {:#?}",
        &walked.violations[..walked.violations.len().min(10)]
    );
    walked
}
