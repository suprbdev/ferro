use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use queue::{JobState, Queue, QueueOptions};

const BATCH: usize = 7;

fn spawn(db: &Path, args: &[&str]) -> Child {
    Command::new(assert_cmd::cargo::cargo_bin("queue"))
        .arg("--db")
        .arg(db)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("condition not reached in {limit:?}");
}

fn kill9(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

fn ids(path: &Path) -> BTreeSet<u64> {
    lines(path).iter().filter_map(|line| line.trim().parse().ok()).collect()
}

#[test]
fn kill_nine_producer_and_worker_lose_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.ferro");
    let produced = dir.path().join("produced.log");
    let done = dir.path().join("done.log");
    let exec = format!("echo $QUEUE_JOB_ID >> {}; sleep 0.003", done.display());
    let mut unacked = BTreeSet::new();

    for round in 0..3 {
        let before = ids(&produced).len();
        let producer = spawn(
            &db,
            &[
                "produce", "jobs", "--count", "1000000", "--batch", &BATCH.to_string(),
                "--log", produced.to_str().unwrap(), "--sleep-ms", "1",
            ],
        );
        wait_until(Duration::from_secs(20), || ids(&produced).len() >= before + 60);
        kill9(producer);

        let acked = ids(&produced);
        {
            let queue = Queue::open(&db, QueueOptions::default()).unwrap();
            let state = queue.db().read();
            for id in &acked {
                assert!(
                    state.jobs.contains_key(id) || ids(&done).contains(id),
                    "round {round}: acknowledged enqueue {id} is missing"
                );
            }
            let fresh: Vec<u64> = (0..state.next_id)
                .filter(|id| !acked.contains(id) && !unacked.contains(id))
                .collect();
            let in_flight: Vec<u64> = (state.next_id.saturating_sub(BATCH as u64)..state.next_id).collect();
            assert!(
                fresh.is_empty() || fresh == in_flight,
                "round {round}: unacknowledged ids {fresh:?} are not the one batch in flight {in_flight:?}"
            );
            unacked.extend(fresh);
        }

        let worker = spawn(
            &db,
            &["work", "jobs", "--workers", "4", "--batch", "2", "--ttl-ms", "400", "--exec", &exec],
        );
        let finished_before = ids(&done).len();
        wait_until(Duration::from_secs(30), || ids(&done).len() >= finished_before + 40);
        kill9(worker);

        let ran = ids(&done);
        let queue = Queue::open(&db, QueueOptions::default()).unwrap();
        let snapshot = queue.db().read();
        for id in &acked {
            match snapshot.jobs.get(id) {
                None => assert!(ran.contains(id), "round {round}: job {id} vanished without running"),
                Some(job) => {
                    assert!(
                        matches!(job.state, JobState::Ready | JobState::Leased | JobState::Delayed),
                        "round {round}: job {id} is {:?}",
                        job.state
                    );
                }
            }
        }
        drop(snapshot);
        drop(queue);
    }

    let finisher = Command::new(assert_cmd::cargo::cargo_bin("queue"))
        .arg("--db")
        .arg(&db)
        .args(["work", "jobs", "--workers", "4", "--batch", "4", "--ttl-ms", "400", "--exit-when-idle", "--exec", &exec])
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(finisher.success());

    let queue = Queue::open(&db, QueueOptions::default()).unwrap();
    let state = queue.db().read();
    assert_eq!(state.jobs.len(), 0);
    let ran: HashSet<u64> = ids(&done).into_iter().collect();
    for id in ids(&produced) {
        assert!(ran.contains(&id), "job {id} never ran");
    }
    let history_ok = queue.history(*ids(&produced).iter().next().unwrap());
    assert!(history_ok.is_ok());
    queue.db().verify().unwrap();
}

#[test]
fn kill_nine_leaves_leased_jobs_redeliverable_after_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.ferro");
    let done = dir.path().join("done.log");
    let produced = dir.path().join("produced.log");
    let make = Command::new(assert_cmd::cargo::cargo_bin("queue"))
        .arg("--db")
        .arg(&db)
        .args(["produce", "jobs", "--count", "30", "--batch", "30", "--log"])
        .stdout(Stdio::null())
        .arg(&produced)
        .status()
        .unwrap();
    assert!(make.success());

    let exec = format!("echo $QUEUE_JOB_ID >> {}; sleep 5", done.display());
    let worker = spawn(
        &db,
        &["work", "jobs", "--workers", "4", "--batch", "3", "--ttl-ms", "300", "--exec", &exec],
    );
    wait_until(Duration::from_secs(20), || ids(&done).len() >= 4);
    kill9(worker);

    {
        let queue = Queue::open(&db, QueueOptions::default()).unwrap();
        let stats = &queue.stats(Some("jobs"))[0];
        assert!(stats.leased >= 4);
        assert_eq!(stats.ready + stats.leased, 30);
        assert_eq!(queue.db().read().jobs.len(), 30);
    }

    std::thread::sleep(Duration::from_millis(400));
    let queue = Queue::open(&db, QueueOptions::default()).unwrap();
    let tick = queue.tick().unwrap();
    assert!(tick.expired.len() >= 4);
    let stats = &queue.stats(Some("jobs"))[0];
    assert_eq!(stats.leased, 0);
    assert_eq!(stats.ready + stats.delayed, 30);
    let redelivered = queue.db().read().jobs.values().filter(|job| job.attempts == 1).count();
    assert!(redelivered >= 4);
}
