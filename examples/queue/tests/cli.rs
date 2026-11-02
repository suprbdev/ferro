use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        Env {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("q.ferro")
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("queue").unwrap();
        cmd.arg("--db").arg(self.db());
        cmd
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.cmd().args(args).assert().success().get_output().stdout.clone();
        String::from_utf8(out).unwrap()
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.cmd().args(args).assert().failure().get_output().stderr.clone();
        String::from_utf8(out).unwrap()
    }
}

fn field(line: &str, name: &str) -> String {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in {line:?}"))
        .to_string()
}

fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

fn spawn_queue(db: &Path, args: &[&str]) -> Child {
    StdCommand::new(assert_cmd::cargo::cargo_bin("queue"))
        .arg("--db")
        .arg(db)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn enqueue_lease_ack_show() {
    let env = Env::new();
    env.cmd()
        .args(["enqueue", "mail", "hello", "--priority", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("enqueued job=0"));
    env.cmd()
        .args(["show", "0"])
        .assert()
        .success()
        .stdout(predicate::str::contains("state=ready").and(predicate::str::contains("payload=hello")));
    let leased = env.run(&["lease", "mail", "--worker", "w1", "--ttl-ms", "60000"]);
    assert!(leased.contains("job=0 attempt=1 queue=mail priority=5 payload=hello"));
    env.cmd()
        .args(["show", "0"])
        .assert()
        .success()
        .stdout(predicate::str::contains("state=leased").and(predicate::str::contains("worker=w1")));
    env.cmd().args(["lease", "mail"]).assert().success().stdout("empty\n");
    env.cmd()
        .args(["ack", "0", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("acked job=0"));
    assert!(env.fails(&["show", "0"]).contains("no such job: 0"));
    assert!(env.fails(&["ack", "0", "1"]).contains("lease on job 0 is lost"));
}

#[test]
fn priorities_and_batch_lease() {
    let env = Env::new();
    env.run(&["enqueue", "q", "low", "--priority", "1"]);
    env.run(&["enqueue", "q", "high", "--priority", "250"]);
    env.run(&["enqueue", "q", "mid", "--priority", "100"]);
    let out = env.run(&["lease", "q", "--n", "2"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("payload=high"));
    assert!(lines[1].contains("payload=mid"));
}

#[test]
fn enqueue_batch_from_stdin_and_file() {
    let env = Env::new();
    env.cmd()
        .args(["enqueue-batch", "q"])
        .write_stdin("a\nb\nc\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("enqueued count=3 first=0 last=2"));
    let file = env.dir.path().join("lines.txt");
    std::fs::write(&file, "d\ne\n").unwrap();
    env.cmd()
        .args(["enqueue-batch", "q", "--file"])
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("count=2"));
    assert!(env.run(&["stats", "q"]).contains("ready=5"));
    assert!(env.fails(&["enqueue-batch", "q", "--key", "k"]).contains("--key applies to a single job"));
}

#[test]
fn delayed_job_not_ready_until_due() {
    let env = Env::new();
    env.run(&["enqueue", "q", "later", "--delay-ms", "400"]);
    assert!(env.run(&["stats", "q"]).contains("delayed=1"));
    env.cmd().args(["lease", "q"]).assert().stdout("empty\n");
    std::thread::sleep(Duration::from_millis(450));
    assert!(env.run(&["lease", "q"]).contains("payload=later"));
}

#[test]
fn nack_backoff_then_dead_letter_and_dlq_commands() {
    let env = Env::new();
    env.run(&["enqueue", "q", "x", "--max-attempts", "2", "--backoff-ms", "1"]);
    env.run(&["lease", "q"]);
    let nacked = env.run(&["nack", "0", "1", "--error", "boom"]);
    assert!(nacked.contains("state=delayed"));
    std::thread::sleep(Duration::from_millis(20));
    let again = env.run(&["lease", "q"]);
    assert!(again.contains("attempt=2"));
    let dead = env.run(&["nack", "0", "2", "--error", "fatal"]);
    assert!(dead.contains("state=dead"));
    env.cmd()
        .args(["dlq", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("state=dead").and(predicate::str::contains("error=\"fatal\"")));
    assert!(env.run(&["stats", "q"]).contains("dead=1"));
    env.cmd()
        .args(["dlq", "retry", "0"])
        .assert()
        .success()
        .stdout("retried count=1\n");
    env.cmd().args(["dlq", "list"]).assert().stdout("dead letter queue is empty\n");
    assert!(env.run(&["lease", "q"]).contains("attempt=1"));
}

#[test]
fn dlq_retry_whole_queue_and_listing_by_queue() {
    let env = Env::new();
    for q in ["a", "a", "b"] {
        env.run(&["enqueue", q, "x", "--max-attempts", "1"]);
    }
    for (q, id, n) in [("a", "0", "1"), ("a", "1", "1"), ("b", "2", "1")] {
        env.run(&["lease", q]);
        env.run(&["nack", id, n]);
    }
    let listed = env.run(&["dlq", "list", "--queue", "a"]);
    assert_eq!(listed.lines().count(), 2);
    env.cmd()
        .args(["dlq", "retry", "--queue", "a"])
        .assert()
        .success()
        .stdout("retried count=2\n");
    assert!(env.run(&["stats", "a"]).contains("ready=2"));
    assert!(env.run(&["stats", "b"]).contains("dead=1"));
    assert!(env.fails(&["dlq", "retry"]).contains("give a job id or --queue"));
    assert!(env.fails(&["dlq", "retry", "0"]).contains("not in the dead letter queue"));
}

#[test]
fn extend_and_tick_reap_expired_leases() {
    let env = Env::new();
    env.run(&["enqueue", "q", "x", "--backoff-ms", "1"]);
    env.run(&["lease", "q", "--ttl-ms", "150"]);
    env.cmd()
        .args(["extend", "0", "1", "--ttl-ms", "60000"])
        .assert()
        .success()
        .stdout("extended job=0\n");
    std::thread::sleep(Duration::from_millis(200));
    assert!(env.run(&["tick"]).contains("expired=0"));
    env.run(&["nack", "0", "1", "--retry-in-ms", "0"]);
    env.run(&["lease", "q", "--ttl-ms", "50"]);
    std::thread::sleep(Duration::from_millis(100));
    assert!(env.run(&["tick"]).contains("expired=1"));
    assert!(env.run(&["stats", "q"]).contains("leased=0"));
    assert!(env.fails(&["ack", "0", "2"]).contains("lost"));
}

#[test]
fn idempotency_key_dedupes() {
    let env = Env::new();
    assert!(env.run(&["enqueue", "q", "x", "--key", "order-9"]).contains("enqueued job=0"));
    assert!(env.run(&["enqueue", "q", "x", "--key", "order-9"]).contains("duplicate job=0"));
    assert!(env.run(&["enqueue", "q", "x", "--key", "order-10"]).contains("enqueued job=1"));
    assert!(env.run(&["stats", "q"]).contains("ready=2"));
}

#[test]
fn dependencies_block_until_parent_acked() {
    let env = Env::new();
    env.run(&["enqueue", "q", "parent"]);
    env.run(&["enqueue", "q", "child", "--after", "0"]);
    assert!(env.run(&["stats", "q"]).contains("blocked=1"));
    assert!(env.run(&["show", "1"]).contains("state=blocked"));
    let first = env.run(&["lease", "q", "--n", "5"]);
    assert_eq!(first.lines().count(), 1);
    env.run(&["ack", "0", "1"]);
    assert!(env.run(&["lease", "q"]).contains("payload=child"));
    assert!(env.fails(&["enqueue", "q", "x", "--after", "77"]).contains("unknown dependency"));
}

#[test]
fn pause_and_resume() {
    let env = Env::new();
    env.run(&["enqueue", "q", "x"]);
    env.cmd().args(["pause", "q"]).assert().success().stdout("paused queue=q\n");
    assert!(env.run(&["stats", "q"]).contains("paused=true"));
    env.cmd().args(["lease", "q"]).assert().stdout("empty\n");
    env.cmd().args(["resume", "q"]).assert().success().stdout("resumed queue=q\n");
    assert!(env.run(&["lease", "q"]).contains("payload=x"));
}

#[test]
fn recurring_add_list_remove_and_tick_spawns() {
    let env = Env::new();
    env.cmd()
        .args(["recurring", "add", "nightly", "q", "report", "--every-ms", "100"])
        .assert()
        .success()
        .stdout("recurring added name=nightly\n");
    assert!(env.run(&["recurring", "list"]).contains("name=nightly queue=q every_ms=100"));
    std::thread::sleep(Duration::from_millis(150));
    assert!(env.run(&["tick"]).contains("spawned=1"));
    assert!(env.run(&["stats", "q"]).contains("ready=1"));
    env.cmd()
        .args(["recurring", "remove", "nightly"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed=true"));
    std::thread::sleep(Duration::from_millis(150));
    assert!(env.run(&["tick"]).contains("spawned=0"));
    assert_eq!(env.run(&["recurring", "list"]), "");
}

#[test]
fn history_shows_each_state() {
    let env = Env::new();
    env.run(&["enqueue", "q", "x", "--backoff-ms", "1"]);
    env.run(&["enqueue", "q", "noise"]);
    env.run(&["lease", "q", "--n", "1"]);
    env.run(&["nack", "0", "1", "--error", "oops"]);
    std::thread::sleep(Duration::from_millis(10));
    env.run(&["lease", "q"]);
    env.run(&["ack", "0", "2"]);
    let out = env.run(&["history", "0"]);
    let states: Vec<String> = out
        .lines()
        .map(|line| {
            if line.contains("finished") {
                "finished".to_string()
            } else {
                field(line, "state")
            }
        })
        .collect();
    assert_eq!(states, ["ready", "leased", "delayed", "leased", "finished"]);
    assert!(out.contains("error=\"oops\""));
    assert!(env.fails(&["history", "99"]).contains("no such job: 99"));
}

#[test]
fn replay_from_version_reenqueues_in_flight() {
    let env = Env::new();
    env.run(&["enqueue", "q", "alpha"]);
    env.run(&["enqueue", "q", "beta"]);
    env.run(&["lease", "q", "--n", "2"]);
    let info = env.run(&["info"]);
    let version = field(info.trim(), "version");
    env.run(&["ack", "0", "1"]);
    env.run(&["ack", "1", "1"]);
    assert!(env.run(&["stats", "q"]).contains("ready=0"));
    let out = env.run(&["replay", "--from", &version]);
    assert!(out.contains("replayed count=2"), "{out}");
    assert!(out.contains("job=0 as=2"));
    let leased = env.run(&["lease", "q", "--n", "5"]);
    assert!(leased.contains("payload=alpha") && leased.contains("payload=beta"));
    let again = env.run(&["replay", "--from", &version]);
    assert!(again.contains("duplicate=true"));
    assert!(env.fails(&["replay", "--from", "9999"]).contains("no such version"));
}

#[test]
fn produce_logs_every_acknowledged_id() {
    let env = Env::new();
    let log = env.dir.path().join("ids.log");
    env.cmd()
        .args(["produce", "q", "--count", "25", "--batch", "10", "--log"])
        .arg(&log)
        .assert()
        .success()
        .stdout("produced count=25\n");
    let ids = std::fs::read_to_string(&log).unwrap();
    assert_eq!(ids.lines().count(), 25);
    assert!(env.run(&["stats", "q"]).contains("ready=25"));
}

#[test]
fn work_runs_command_per_job_and_retries_failures() {
    let env = Env::new();
    let out = env.dir.path().join("out.txt");
    env.cmd()
        .args(["enqueue-batch", "q"])
        .write_stdin("one\ntwo\nthree\n")
        .assert()
        .success();
    let exec = format!("cat >> {}; echo \" id=$QUEUE_JOB_ID q=$QUEUE_NAME a=$QUEUE_ATTEMPT\" >> {}", out.display(), out.display());
    env.cmd()
        .args(["work", "q", "--workers", "2", "--batch", "2", "--exit-when-idle", "--exec", &exec])
        .assert()
        .success()
        .stdout(predicate::str::contains("done=3 failed=0 lost=0"));
    let text = std::fs::read_to_string(&out).unwrap();
    for word in ["one", "two", "three"] {
        assert!(text.contains(word), "{text}");
    }
    assert!(text.contains("q=q a=1"));
    assert!(env.run(&["stats", "q"]).contains("ready=0 delayed=0 leased=0"));

    env.run(&["enqueue", "f", "x", "--max-attempts", "2", "--backoff-ms", "1"]);
    env.cmd()
        .args(["work", "f", "--exit-when-idle", "--exec", "echo nope >&2; exit 3"])
        .assert()
        .success()
        .stdout(predicate::str::contains("failed=2"));
    assert!(env.run(&["dlq", "list"]).contains("exit exit status: 3: nope"));
}

#[test]
fn work_stops_after_max_jobs() {
    let env = Env::new();
    env.cmd().args(["enqueue-batch", "q"]).write_stdin("1\n2\n3\n4\n").assert().success();
    env.cmd()
        .args(["work", "q", "--max-jobs", "2", "--exec", "true"])
        .assert()
        .success()
        .stdout(predicate::str::contains("done="));
    let ready = field(env.run(&["stats", "q"]).trim(), "ready");
    assert!(ready.parse::<u32>().unwrap() <= 2);
}

#[test]
fn stats_without_queue_lists_all_and_info_reports_size() {
    let env = Env::new();
    env.run(&["enqueue", "alpha", "x"]);
    env.run(&["enqueue", "beta", "x"]);
    let out = env.run(&["stats"]);
    assert!(out.contains("queue=alpha") && out.contains("queue=beta"));
    let info = env.run(&["info"]);
    assert!(field(info.trim(), "file_bytes").parse::<u64>().unwrap() > 0);
}

#[test]
fn readers_work_while_a_writer_process_is_alive() {
    let env = Env::new();
    env.run(&["enqueue", "q", "x"]);
    let mut worker = spawn_queue(
        &env.db(),
        &["work", "idle", "--exec", "true"],
    );
    std::thread::sleep(Duration::from_millis(500));
    env.cmd()
        .args(["--lock-wait-ms", "0", "stats", "q"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ready=1"));
    let err = env.fails(&["--lock-wait-ms", "0", "enqueue", "q", "y"]);
    assert!(err.contains("locked"), "{err}");
    worker.kill().unwrap();
    worker.wait().unwrap();
    assert!(wait_for(Duration::from_secs(5), || {
        env.cmd()
            .args(["--lock-wait-ms", "0", "enqueue", "q", "y"])
            .assert()
            .try_success()
            .is_ok()
    }));
}

#[test]
fn writer_waits_for_the_lock() {
    let env = Env::new();
    env.run(&["enqueue", "q", "x"]);
    let mut holder = spawn_queue(&env.db(), &["work", "idle", "--exec", "true"]);
    std::thread::sleep(Duration::from_millis(500));
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        holder.kill().unwrap();
        holder.wait().unwrap();
    });
    env.cmd()
        .args(["--lock-wait-ms", "5000", "enqueue", "q", "y"])
        .assert()
        .success();
    killer.join().unwrap();
}
