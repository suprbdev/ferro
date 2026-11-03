use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferro::{Db, Durability, Map, Options};
use tempfile::TempDir;

type Root = Map<u64, u64>;

fn open(dir: &TempDir, durability: Durability, coalesce: bool) -> Db<Root> {
    Options::new()
        .durability(durability)
        .coalesce(coalesce)
        .open(dir.path().join("db.ferro"))
        .expect("open")
}

fn kinds() -> [(Durability, bool); 4] {
    [
        (Durability::Fsync, false),
        (Durability::Buffered, false),
        (Durability::Fsync, true),
        (Durability::Buffered, true),
    ]
}

async fn settle(db: &Db<Root>, len: usize) {
    let watcher = db.watch();
    tokio::time::timeout(Duration::from_secs(30), async {
        while db.read().len() < len {
            watcher.changed().await.expect("feed open");
        }
    })
    .await
    .expect("writes landed");
}

#[tokio::test(flavor = "current_thread")]
async fn one_runtime_thread_carries_hundreds_of_writes() {
    for (durability, coalesce) in kinds() {
        let dir = tempfile::tempdir().expect("dir");
        let db = Arc::new(open(&dir, durability, coalesce));
        let tasks: Vec<_> = (0..400u64)
            .map(|key| {
                let db = db.clone();
                tokio::spawn(async move {
                    db.commit_with_async(|root| {
                        root.insert(key, key);
                        (format!("k{key}"), key)
                    })
                    .await
                    .expect("commit")
                })
            })
            .collect();
        let mut versions = BTreeSet::new();
        for task in tasks {
            let done = task.await.expect("task");
            let snapshot = db.at(done.version).expect("version");
            assert_eq!(snapshot.get(&done.value).as_deref(), Some(&done.value));
            versions.insert(done.version);
        }
        assert_eq!(db.read().len(), 400);
        if !coalesce {
            assert_eq!(versions, (1..=400).collect());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_lands_exactly_when_its_closure_ran() {
    for (durability, coalesce) in kinds() {
        let dir = tempfile::tempdir().expect("dir");
        let db = Arc::new(open(&dir, durability, coalesce));
        let ran = Arc::new(Mutex::new(BTreeSet::new()));
        for key in 0..50u64 {
            let ran = ran.clone();
            let write = db.update_async(move |root| {
                ran.lock().expect("ran").insert(key);
                root.insert(key, key);
            });
            let _ = tokio::time::timeout(Duration::ZERO, write).await;
        }
        assert_eq!(ran.lock().expect("ran").len(), 50);
        let tasks: Vec<_> = (50..150u64)
            .map(|key| {
                let (db, ran) = (db.clone(), ran.clone());
                tokio::spawn(async move {
                    db.update_async(move |root| {
                        ran.lock().expect("ran").insert(key);
                        root.insert(key, key);
                    })
                    .await
                })
            })
            .collect();
        tokio::task::yield_now().await;
        for task in tasks.iter().step_by(2) {
            task.abort();
        }
        for task in tasks {
            if let Ok(done) = task.await {
                done.expect("commit");
            }
        }
        let ran: BTreeSet<u64> = ran.lock().expect("ran").clone();
        settle(&db, ran.len()).await;
        let landed: BTreeSet<u64> = db.read().keys().collect();
        assert_eq!(landed, ran, "{durability:?} {coalesce}");
        drop(Arc::into_inner(db).expect("last handle"));
        let reopened = open(&dir, durability, coalesce);
        let landed: BTreeSet<u64> = reopened.read().keys().collect();
        assert_eq!(landed, ran, "{durability:?} {coalesce}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn async_tasks_and_blocking_writers_share_one_database() {
    for (durability, coalesce) in kinds() {
        let dir = tempfile::tempdir().expect("dir");
        let db = Arc::new(open(&dir, durability, coalesce));
        let mut tasks = Vec::new();
        for writer in 0..8u64 {
            let (db, blocking) = (db.clone(), db.clone());
            tasks.push(tokio::spawn(async move {
                for i in 0..50 {
                    db.update_async(|root| {
                        let count = root.get(&u64::MAX).map_or(0, |count| *count);
                        root.insert(u64::MAX, count + 1);
                        root.insert(writer * 1000 + i, i);
                    })
                    .await
                    .expect("async write");
                }
            }));
            tasks.push(tokio::task::spawn_blocking(move || {
                for i in 0..50 {
                    blocking.update(|root| {
                        let count = root.get(&u64::MAX).map_or(0, |count| *count);
                        root.insert(u64::MAX, count + 1);
                        root.insert(100_000 + writer * 1000 + i, i);
                    })
                    .expect("blocking write");
                }
            }));
        }
        for task in tasks {
            task.await.expect("writer");
        }
        let snapshot = db.read();
        assert_eq!(snapshot.get(&u64::MAX).as_deref(), Some(&800), "{durability:?} {coalesce}");
        assert_eq!(snapshot.len(), 801, "{durability:?} {coalesce}");
        db.verify().expect("verify");
    }
}
