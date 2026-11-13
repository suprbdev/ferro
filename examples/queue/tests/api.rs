use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ferro::{Durability, Keep, Reclaim};
use queue::{JobState, NewJob, Queue, QueueError, QueueOptions, Recurring};

struct Fixture {
    queue: Queue,
    clock: Arc<AtomicU64>,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(AtomicU64::new(1_000_000));
    let shared = clock.clone();
    let options = QueueOptions {
        clock: Arc::new(move || shared.load(Ordering::SeqCst)),
        lock_wait: Duration::from_millis(100),
        ..QueueOptions::default()
    };
    let queue = Queue::open(dir.path().join("q.ferro"), options).unwrap();
    Fixture {
        queue,
        clock,
        _dir: dir,
    }
}

impl Fixture {
    fn advance(&self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
}

fn job(queue: &str, payload: impl Into<Vec<u8>>) -> NewJob {
    NewJob::new(queue, payload)
}

#[test]
fn priority_then_fifo() {
    let f = fixture();
    let q = &f.queue;
    let low = q.enqueue(job("a", "low").priority(1)).unwrap().id;
    let high1 = q.enqueue(job("a", "high1").priority(200)).unwrap().id;
    let high2 = q.enqueue(job("a", "high2").priority(200)).unwrap().id;
    let other = q.enqueue(job("b", "other")).unwrap().id;
    let order: Vec<u64> = (0..3)
        .map(|_| q.lease("a", "w", 1000).unwrap().unwrap().job.id)
        .collect();
    assert_eq!(order, vec![high1, high2, low]);
    assert!(q.lease("a", "w", 1000).unwrap().is_none());
    assert_eq!(q.lease("b", "w", 1000).unwrap().unwrap().job.id, other);
}

#[test]
fn lease_batch_takes_top_n() {
    let f = fixture();
    let q = &f.queue;
    let specs = (0..10)
        .map(|i| job("a", format!("{i}")).priority(i as u8))
        .collect();
    q.enqueue_batch(specs).unwrap();
    let got = q.lease_n("a", "w", 1000, 4).unwrap();
    let priorities: Vec<u8> = got.iter().map(|l| l.job.priority).collect();
    assert_eq!(priorities, vec![9, 8, 7, 6]);
    assert_eq!(q.stats(Some("a"))[0].ready, 6);
    assert_eq!(q.stats(Some("a"))[0].leased, 4);
}

#[test]
fn delayed_job_waits_for_its_time() {
    let f = fixture();
    let q = &f.queue;
    let id = q.enqueue(job("a", "later").delay_ms(5_000)).unwrap().id;
    assert!(q.lease("a", "w", 1000).unwrap().is_none());
    assert_eq!(q.get(id).unwrap().state, JobState::Delayed);
    f.advance(4_999);
    assert!(q.lease("a", "w", 1000).unwrap().is_none());
    f.advance(1);
    let got = q.lease("a", "w", 1000).unwrap().unwrap();
    assert_eq!(got.job.id, id);
}

#[test]
fn ack_removes_job_and_stale_ack_is_lost() {
    let f = fixture();
    let q = &f.queue;
    let id = q.enqueue(job("a", "x")).unwrap().id;
    let leased = q.lease("a", "w", 1000).unwrap().unwrap();
    q.ack(leased.lease()).unwrap();
    assert!(q.get(id).is_none());
    assert!(matches!(
        q.ack(leased.lease()),
        Err(QueueError::LeaseLost(_))
    ));
}

#[test]
fn nack_backs_off_exponentially_then_dead_letters() {
    let f = fixture();
    let q = &f.queue;
    let id = q
        .enqueue(job("a", "x").max_attempts(3).backoff_ms(1_000))
        .unwrap()
        .id;
    let first = q.lease("a", "w", 10_000).unwrap().unwrap();
    assert_eq!(
        q.nack(first.lease(), "boom", None).unwrap(),
        JobState::Delayed
    );
    assert_eq!(
        q.get(id).unwrap().run_at,
        f.clock.load(Ordering::SeqCst) + 1_000
    );
    assert!(q.lease("a", "w", 10_000).unwrap().is_none());
    f.advance(1_000);
    let second = q.lease("a", "w", 10_000).unwrap().unwrap();
    assert_eq!(second.job.attempts, 2);
    q.nack(second.lease(), "boom", None).unwrap();
    assert_eq!(
        q.get(id).unwrap().run_at,
        f.clock.load(Ordering::SeqCst) + 2_000
    );
    f.advance(2_000);
    let third = q.lease("a", "w", 10_000).unwrap().unwrap();
    assert_eq!(
        q.nack(third.lease(), "final", None).unwrap(),
        JobState::Dead
    );
    let dead = q.dlq_list(Some("a"), 10);
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].error.as_deref(), Some("final"));
    assert_eq!(q.stats(Some("a"))[0].dead, 1);
    f.advance(10_000_000);
    assert!(q.lease("a", "w", 1000).unwrap().is_none());
}

#[test]
fn nack_with_explicit_retry_delay() {
    let f = fixture();
    let q = &f.queue;
    let id = q.enqueue(job("a", "x")).unwrap().id;
    let leased = q.lease("a", "w", 1000).unwrap().unwrap();
    q.nack(leased.lease(), "slow down", Some(250)).unwrap();
    assert_eq!(
        q.get(id).unwrap().run_at,
        f.clock.load(Ordering::SeqCst) + 250
    );
}

#[test]
fn dlq_retry_one_and_all() {
    let f = fixture();
    let q = &f.queue;
    let ids: Vec<u64> = (0..3)
        .map(|i| {
            q.enqueue(job("a", format!("{i}")).max_attempts(1))
                .unwrap()
                .id
        })
        .collect();
    for _ in 0..3 {
        let leased = q.lease("a", "w", 1000).unwrap().unwrap();
        q.nack(leased.lease(), "bad", None).unwrap();
    }
    assert_eq!(q.dlq_list(None, 10).len(), 3);
    q.dlq_retry(ids[0]).unwrap();
    assert_eq!(q.get(ids[0]).unwrap().state, JobState::Ready);
    assert_eq!(q.get(ids[0]).unwrap().attempts, 0);
    assert!(matches!(q.dlq_retry(ids[0]), Err(QueueError::NotDead(_))));
    assert_eq!(q.dlq_retry_queue("a").unwrap(), 2);
    assert!(q.dlq_list(None, 10).is_empty());
    assert_eq!(q.stats(Some("a"))[0].ready, 3);
}

#[test]
fn expired_lease_is_redelivered_and_old_holder_is_fenced() {
    let f = fixture();
    let q = &f.queue;
    let id = q.enqueue(job("a", "x")).unwrap().id;
    let slow = q.lease("a", "slow", 1_000).unwrap().unwrap();
    f.advance(1_001);
    let tick = q.tick().unwrap();
    assert_eq!(tick.expired, vec![id]);
    assert_eq!(q.get(id).unwrap().state, JobState::Delayed);
    f.advance(1_000);
    let fast = q.lease("a", "fast", 1_000).unwrap().unwrap();
    assert_eq!(fast.job.attempts, 2);
    assert!(matches!(q.ack(slow.lease()), Err(QueueError::LeaseLost(_))));
    q.ack(fast.lease()).unwrap();
}

#[test]
fn lease_applies_expiry_without_a_reaper() {
    let f = fixture();
    let q = &f.queue;
    q.enqueue(job("a", "x").backoff_ms(0)).unwrap();
    q.lease("a", "w1", 100).unwrap().unwrap();
    f.advance(101);
    let again = q.lease("a", "w2", 100).unwrap().unwrap();
    assert_eq!(again.job.attempts, 2);
}

#[test]
fn expiry_exhausts_attempts_into_dlq() {
    let f = fixture();
    let q = &f.queue;
    let id = q.enqueue(job("a", "x").max_attempts(1)).unwrap().id;
    q.lease("a", "w", 100).unwrap().unwrap();
    f.advance(101);
    q.tick().unwrap();
    assert_eq!(q.get(id).unwrap().state, JobState::Dead);
}

#[test]
fn extend_pushes_expiry() {
    let f = fixture();
    let q = &f.queue;
    let id = q.enqueue(job("a", "x")).unwrap().id;
    let leased = q.lease("a", "w", 100).unwrap().unwrap();
    f.advance(90);
    q.extend(leased.lease(), 100).unwrap();
    f.advance(50);
    assert!(q.tick().unwrap().expired.is_empty());
    assert_eq!(q.get(id).unwrap().state, JobState::Leased);
    f.advance(60);
    assert_eq!(q.tick().unwrap().expired, vec![id]);
}

#[test]
fn idempotency_key_dedupes_within_retention_only() {
    let f = fixture();
    let q = &f.queue;
    let first = q
        .enqueue(job("a", "x").key("k1").key_ttl_ms(1_000))
        .unwrap();
    assert!(!first.duplicate);
    let again = q
        .enqueue(job("a", "x").key("k1").key_ttl_ms(1_000))
        .unwrap();
    assert!(again.duplicate);
    assert_eq!(again.id, first.id);
    let other_queue = q.enqueue(job("b", "x").key("k1")).unwrap();
    assert!(!other_queue.duplicate);
    let leased = q.lease("a", "w", 1000).unwrap().unwrap();
    q.ack(leased.lease()).unwrap();
    assert!(q.enqueue(job("a", "x").key("k1")).unwrap().duplicate);
    f.advance(1_001);
    let after = q
        .enqueue(job("a", "x").key("k1").key_ttl_ms(1_000))
        .unwrap();
    assert!(!after.duplicate);
    assert_ne!(after.id, first.id);
}

#[test]
fn idempotency_keys_are_pruned() {
    let f = fixture();
    let q = &f.queue;
    q.enqueue(job("a", "x").key("k").key_ttl_ms(100)).unwrap();
    assert_eq!(q.db().read().idem.len(), 1);
    f.advance(101);
    assert_eq!(q.tick().unwrap().pruned, 1);
    assert_eq!(q.db().read().idem.len(), 0);
    assert_eq!(q.db().read().idem_exp.len(), 0);
}

#[test]
fn batch_enqueue_is_atomic_and_dedupes_inside() {
    let f = fixture();
    let q = &f.queue;
    let bad = vec![job("a", "1"), job("a", "2").after([999])];
    assert!(matches!(
        q.enqueue_batch(bad),
        Err(QueueError::UnknownDependency(999))
    ));
    assert_eq!(q.db().read().jobs.len(), 0);
    let done = q
        .enqueue_batch(vec![
            job("a", "1").key("k"),
            job("a", "2").key("k"),
            job("a", "3"),
        ])
        .unwrap();
    assert!(!done[0].duplicate);
    assert!(done[1].duplicate);
    assert_eq!(done[0].id, done[1].id);
    assert_eq!(q.db().read().jobs.len(), 2);
}

#[test]
fn dependencies_release_when_all_parents_complete() {
    let f = fixture();
    let q = &f.queue;
    let p1 = q.enqueue(job("a", "p1")).unwrap().id;
    let p2 = q.enqueue(job("a", "p2")).unwrap().id;
    let child = q.enqueue(job("b", "child").after([p1, p2])).unwrap().id;
    assert_eq!(q.get(child).unwrap().state, JobState::Blocked);
    assert_eq!(q.stats(Some("b"))[0].blocked, 1);
    assert!(q.lease("b", "w", 1000).unwrap().is_none());
    let l1 = q.lease("a", "w", 1000).unwrap().unwrap();
    q.ack(l1.lease()).unwrap();
    assert_eq!(q.get(child).unwrap().deps_left, 1);
    assert!(q.lease("b", "w", 1000).unwrap().is_none());
    let l2 = q.lease("a", "w", 1000).unwrap().unwrap();
    q.ack(l2.lease()).unwrap();
    assert_eq!(q.get(child).unwrap().state, JobState::Ready);
    assert_eq!(q.lease("b", "w", 1000).unwrap().unwrap().job.id, child);
    assert_eq!(q.db().read().children.len(), 0);
    assert_eq!(q.db().read().blocked.len(), 0);
}

#[test]
fn dependency_on_finished_job_is_satisfied_and_future_id_rejected() {
    let f = fixture();
    let q = &f.queue;
    let parent = q.enqueue(job("a", "p")).unwrap().id;
    let leased = q.lease("a", "w", 1000).unwrap().unwrap();
    q.ack(leased.lease()).unwrap();
    let child = q.enqueue(job("a", "c").after([parent])).unwrap().id;
    assert_eq!(q.get(child).unwrap().state, JobState::Ready);
    assert!(matches!(
        q.enqueue(job("a", "c").after([child + 100])),
        Err(QueueError::UnknownDependency(_))
    ));
}

#[test]
fn dependency_waits_for_dead_parent_to_be_retried() {
    let f = fixture();
    let q = &f.queue;
    let parent = q.enqueue(job("a", "p").max_attempts(1)).unwrap().id;
    let child = q.enqueue(job("a", "c").after([parent])).unwrap().id;
    let leased = q.lease("a", "w", 1000).unwrap().unwrap();
    q.nack(leased.lease(), "bad", None).unwrap();
    assert_eq!(q.get(child).unwrap().state, JobState::Blocked);
    q.dlq_retry(parent).unwrap();
    let again = q.lease("a", "w", 1000).unwrap().unwrap();
    q.ack(again.lease()).unwrap();
    assert_eq!(q.get(child).unwrap().state, JobState::Ready);
}

#[test]
fn batch_dependencies_by_index() {
    let f = fixture();
    let q = &f.queue;
    let done = q
        .enqueue_batch(vec![job("a", "p"), job("a", "c").after_batch([0])])
        .unwrap();
    assert_eq!(q.get(done[1].id).unwrap().state, JobState::Blocked);
    assert_eq!(q.lease("a", "w", 1000).unwrap().unwrap().job.id, done[0].id);
    assert!(q.lease("a", "w", 1000).unwrap().is_none());
}

#[test]
fn pause_blocks_leasing_but_not_enqueue() {
    let f = fixture();
    let q = &f.queue;
    assert!(q.pause("a").unwrap());
    assert!(!q.pause("a").unwrap());
    q.enqueue(job("a", "x")).unwrap();
    assert!(q.lease("a", "w", 1000).unwrap().is_none());
    assert!(q.stats(Some("a"))[0].paused);
    assert!(q.resume("a").unwrap());
    assert!(q.lease("a", "w", 1000).unwrap().is_some());
}

#[test]
fn recurring_spawns_once_per_interval_and_skips_missed() {
    let f = fixture();
    let q = &f.queue;
    let start = f.clock.load(Ordering::SeqCst);
    q.recurring_add(Recurring {
        name: "cron".into(),
        queue: "a".into(),
        priority: 100,
        payload: b"tick".to_vec().into(),
        every_ms: 1_000,
        next_run: start + 1_000,
        max_attempts: 3,
    })
    .unwrap();
    assert_eq!(q.tick().unwrap().spawned, 0);
    f.advance(1_000);
    assert_eq!(q.tick().unwrap().spawned, 1);
    assert_eq!(q.tick().unwrap().spawned, 0);
    assert_eq!(q.stats(Some("a"))[0].ready, 1);
    f.advance(10_500);
    assert_eq!(q.tick().unwrap().spawned, 1);
    assert_eq!(q.stats(Some("a"))[0].ready, 2);
    assert_eq!(q.recurring_list().len(), 1);
    assert!(q.recurring_list()[0].next_run > f.clock.load(Ordering::SeqCst));
    assert!(q.recurring_remove("cron").unwrap());
    f.advance(5_000);
    assert_eq!(q.tick().unwrap().spawned, 0);
}

#[test]
fn history_lists_every_state_of_a_finished_job() {
    let f = fixture();
    let q = &f.queue;
    for i in 0..20 {
        q.enqueue(job("noise", format!("{i}"))).unwrap();
    }
    let id = q.enqueue(job("a", "x").backoff_ms(10)).unwrap().id;
    for i in 0..20 {
        q.enqueue(job("noise", format!("{i}"))).unwrap();
    }
    let first = q.lease("a", "w", 1000).unwrap().unwrap();
    for i in 0..20 {
        q.enqueue(job("noise", format!("{i}"))).unwrap();
    }
    q.nack(first.lease(), "boom", None).unwrap();
    f.advance(20);
    let second = q.lease("a", "w", 1000).unwrap().unwrap();
    q.ack(second.lease()).unwrap();
    for i in 0..20 {
        q.enqueue(job("noise", format!("{i}"))).unwrap();
    }
    let events = q.history(id).unwrap();
    let states: Vec<Option<JobState>> = events
        .iter()
        .map(|e| e.job.as_ref().map(|j| j.state))
        .collect();
    assert_eq!(
        states,
        vec![
            Some(JobState::Ready),
            Some(JobState::Leased),
            Some(JobState::Delayed),
            Some(JobState::Leased),
            None
        ]
    );
    assert!(events.windows(2).all(|w| w[0].version < w[1].version));
    assert!(events[0].message.contains("enqueue"));
    assert!(events[4].message.contains("ack"));
}

#[test]
fn history_of_live_and_unknown_jobs() {
    let f = fixture();
    let q = &f.queue;
    let id = q.enqueue(job("a", "x")).unwrap().id;
    assert_eq!(q.history(id).unwrap().len(), 1);
    assert!(matches!(q.history(99), Err(QueueError::NotFound(99))));
}

#[test]
fn replay_reenqueues_jobs_in_flight_at_a_past_version() {
    let f = fixture();
    let q = &f.queue;
    q.enqueue(job("a", "A").priority(7)).unwrap();
    let b = q.enqueue(job("a", "B")).unwrap().id;
    q.enqueue(job("a", "C")).unwrap();
    let la = q.lease("a", "w", 10_000).unwrap().unwrap();
    let lb = q.lease("a", "w", 10_000).unwrap().unwrap();
    assert_eq!(la.job.id, b);
    let in_flight_at = q.version();
    q.ack(la.lease()).unwrap();
    q.ack(lb.lease()).unwrap();
    let done = q.replay(in_flight_at, false, false).unwrap();
    assert_eq!(done.len(), 2);
    let copies: Vec<String> = done
        .iter()
        .map(|r| String::from_utf8(q.get(r.new_job).unwrap().payload.to_vec()).unwrap())
        .collect();
    assert!(copies.contains(&"B".to_string()) && copies.contains(&"C".to_string()));
    let again = q.replay(in_flight_at, false, false).unwrap();
    assert!(again.iter().all(|r| r.duplicate));
    let pending = q.replay(in_flight_at, true, true).unwrap();
    assert_eq!(pending.iter().filter(|r| !r.duplicate).count(), 1);
}

#[test]
fn replay_skips_jobs_still_live_unless_forced() {
    let f = fixture();
    let q = &f.queue;
    q.enqueue(job("a", "A")).unwrap();
    q.lease("a", "w", 10_000).unwrap().unwrap();
    let v = q.version();
    assert!(q.replay(v, false, false).unwrap().is_empty());
    assert_eq!(q.replay(v, false, true).unwrap().len(), 1);
}

#[test]
fn invalid_specs_are_rejected() {
    let f = fixture();
    let q = &f.queue;
    assert!(matches!(
        q.enqueue(job("", "x")),
        Err(QueueError::Invalid(_))
    ));
    assert!(matches!(
        q.enqueue(job("a", "x").max_attempts(0)),
        Err(QueueError::Invalid(_))
    ));
}

#[test]
fn state_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.ferro");
    {
        let q = Queue::open(&path, QueueOptions::default()).unwrap();
        q.enqueue(job("a", "x").key("k")).unwrap();
        q.lease("a", "w", 60_000).unwrap().unwrap();
        q.enqueue(job("a", "y").delay_ms(60_000)).unwrap();
    }
    let q = Queue::open(&path, QueueOptions::default()).unwrap();
    let s = &q.stats(Some("a"))[0];
    assert_eq!((s.ready, s.leased, s.delayed), (0, 1, 1));
    assert!(q.enqueue(job("a", "x").key("k")).unwrap().duplicate);
}

#[test]
fn retention_keeps_a_busy_queue_small_without_stopping_it() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("q.ferro");
    let options = QueueOptions {
        durability: Durability::Buffered,
        history: Keep::Last(50),
        reclaim: Reclaim::every(256 << 10),
        ..QueueOptions::default()
    };
    let q = Queue::open(&path, options).unwrap();
    let mut peak = 0;
    let mut done = 0;
    while q.space().unwrap().cycles < 3 {
        let job = q.enqueue(NewJob::new("work", vec![7; 300])).unwrap().id;
        let leased = q.lease("work", "w", 30_000).unwrap().unwrap();
        assert_eq!(leased.job.id, job);
        q.ack(leased.lease()).unwrap();
        done += 1;
        peak = peak.max(q.size_on_disk());
        assert!(done < 200_000, "no cycle ran: {:?}", q.space().unwrap());
    }
    assert!(q.space().unwrap().last_error.is_none());
    assert!(peak < 16 << 20, "peak {peak} after {done} jobs");
    q.db().verify().unwrap();
    let job = q.enqueue(NewJob::new("work", vec![1])).unwrap().id;
    drop(q);
    let q = Queue::open(&path, QueueOptions::default()).unwrap();
    assert!(
        q.lease("work", "w", 30_000)
            .unwrap()
            .is_some_and(|leased| leased.job.id == job)
    );
}
