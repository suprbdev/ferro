#![cfg(target_os = "linux")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use common::fast;
use ferro::{Data, Db, Map, Record, Snapshot, Table};

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u64,
    #[index]
    a: u32,
    #[index]
    b: u64,
    #[index]
    c: u16,
    #[unique]
    d: u64,
}

#[derive(Data, Clone, Default)]
struct Root {
    tables: Map<u64, Table<Row>>,
}

fn threads() -> usize {
    std::fs::read_dir("/proc/self/task").map_or(0, |tasks| tasks.count())
}

fn most_threads<R>(run: impl FnOnce() -> R) -> (R, usize) {
    let stop = Arc::new(AtomicBool::new(false));
    let most = Arc::new(AtomicUsize::new(0));
    let watcher = {
        let (stop, most) = (stop.clone(), most.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                most.fetch_max(threads(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_micros(200));
            }
        })
    };
    std::thread::sleep(Duration::from_millis(5));
    let before = threads();
    let out = run();
    stop.store(true, Ordering::Relaxed);
    watcher.join().unwrap();
    (out, most.load(Ordering::Relaxed).saturating_sub(before))
}

fn unmerged<T>(_: &mut T, _: &Snapshot<T>, _: &Snapshot<T>) -> (String, ()) {
    ("merge".to_string(), ())
}

#[test]
fn checking_tables_takes_eight_threads_at_most() {
    let dir = tempfile::tempdir().unwrap();
    let peer: Db<Root> = fast().open(dir.path().join("peer.ferro")).unwrap();
    let mine: Db<Root> = fast().open(dir.path().join("mine.ferro")).unwrap();
    for t in 0..8u64 {
        peer.update(|root| {
            let mut table: Table<Row> = Table::new();
            for i in 0..30_000u64 {
                let x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ t;
                table.insert(Row {
                    id: i,
                    a: x as u32,
                    b: x >> 7,
                    c: (x >> 3) as u16,
                    d: i * 1000 + t,
                });
            }
            root.tables.insert(t, table);
        })
        .unwrap();
    }
    let bundle = peer.bundle(&mine.have().unwrap()).unwrap();
    let (received, most) = most_threads(|| mine.receive("peer", &bundle, unmerged));
    received.unwrap();
    assert!(most <= 8, "a receive took {most} threads beside its own");
    let (verified, most) = most_threads(|| mine.verify_typed());
    verified.unwrap();
    assert!(most <= 8, "verify_typed took {most} threads beside its own");
}
