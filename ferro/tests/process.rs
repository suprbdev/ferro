mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use common::Rng;
use common::fast;
use ferro::{Data, Db, Map, Options};

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    a: Map<u8, Vec<u8>>,
    b: Map<u8, Vec<u8>>,
    n: u64,
}

fn payload(n: u64) -> Vec<u8> {
    vec![(n % 251) as u8; 600 + (n % 50) as usize]
}

fn commit_one(db: &Db<Root>) -> u64 {
    db.update(|root| {
        root.n += 1;
        root.a.insert(0, payload(root.n));
        root.b.insert(0, payload(root.n));
        root.a.insert((root.n % 7) as u8 + 1, payload(root.n + 1));
        root.n
    })
    .unwrap()
}

fn check(root: &Root) {
    if root.n == 0 {
        assert!(root.a.is_empty() && root.b.is_empty());
        return;
    }
    let a = root.a.get(&0).expect("a present");
    let b = root.b.get(&0).expect("b present");
    assert_eq!(*a, payload(root.n), "a torn at n={}", root.n);
    assert_eq!(*b, payload(root.n), "b torn at n={}", root.n);
    let spill = root.a.get(&((root.n % 7) as u8 + 1)).expect("spill");
    assert_eq!(*spill, payload(root.n + 1));
}

const WRITERS: u64 = 4;

fn ack_forever(db: &Db<Root>) -> ! {
    loop {
        let n = commit_one(db);
        println!("ACK {n}");
    }
}

fn child(role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env("FERRO_ROLE", role);
    command
}

#[test]
fn child_entry() {
    let Ok(role) = std::env::var("FERRO_ROLE") else {
        return;
    };
    let path = std::env::var("FERRO_PATH").unwrap();
    let path = Path::new(&path);
    match role.as_str() {
        "writer" | "durable-writer" => {
            let options = if role == "writer" {
                fast()
            } else {
                Options::new()
            };
            let db: Db<Root> = options.open(path).unwrap();
            let millis: u64 = std::env::var("FERRO_MS").unwrap().parse().unwrap();
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(millis) {
                commit_one(&db);
            }
            db.update(|root| {
                root.a.insert(200, vec![1]);
            })
            .unwrap();
            println!("done {}", db.version());
        }
        "reader" => {
            let db: Db<Root> = Options::new().read_only().open(path).unwrap();
            let mut last = 0;
            let mut seen = 0u64;
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                db.refresh().unwrap();
                let snapshot = db.read();
                check(&snapshot);
                assert!(
                    snapshot.n >= last,
                    "went backwards {} -> {}",
                    last,
                    snapshot.n
                );
                if snapshot.n != last {
                    seen += 1;
                }
                last = snapshot.n;
                if snapshot.n > 3 {
                    let older = db.at(snapshot.n / 2 + 1).unwrap();
                    check(&older);
                }
                if snapshot.a.contains_key(&200) {
                    println!("reader saw {last} after {seen} refreshes");
                    return;
                }
                assert!(Instant::now() < deadline, "reader never saw the end");
            }
        }
        "killme" => {
            let db: Db<Root> = Options::new().open(path).unwrap();
            loop {
                let n = commit_one(&db);
                println!("ACK {n}");
            }
        }
        "killgroup" => {
            let db: Db<Root> = Options::new().open(path).unwrap();
            for _ in 1..WRITERS {
                let db = db.clone();
                std::thread::spawn(move || ack_forever(&db));
            }
            ack_forever(&db);
        }
        other => panic!("unknown role {other}"),
    }
}

fn wait_ok(mut child: Child, limit: Duration) -> String {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut out = String::new();
            if let Some(stdout) = child.stdout.take() {
                for line in BufReader::new(stdout).lines() {
                    out.push_str(&line.unwrap());
                    out.push('\n');
                }
            }
            assert!(status.success(), "child failed: {status}\n{out}");
            return out;
        }
        if start.elapsed() > limit {
            child.kill().unwrap();
            panic!("child timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_reader_process_never_sees_a_torn_commit() {
    reader_race("writer");
}

#[test]
fn a_reader_process_never_sees_a_torn_durable_commit() {
    reader_race("durable-writer");
}

fn reader_race(role: &str) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("w.ferro");
    {
        let db: Db<Root> = fast().open(&path).unwrap();
        commit_one(&db);
    }
    let writer = child(role)
        .env("FERRO_PATH", &path)
        .env("FERRO_MS", "2500")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let readers: Vec<Child> = (0..2)
        .map(|_| {
            child("reader")
                .env("FERRO_PATH", &path)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let out = wait_ok(writer, Duration::from_secs(15));
    assert!(out.contains("done"), "{out}");
    for reader in readers {
        let out = wait_ok(reader, Duration::from_secs(15));
        assert!(out.contains("reader saw"), "{out}");
    }
}

#[cfg(unix)]
#[test]
fn sigkill_during_group_commit_keeps_every_acknowledged_version() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("g.ferro");
    let mut rng = Rng::new(0xBEEF);
    let mut floor = 0u64;
    for round in 0..15 {
        let mut proc = child("killgroup")
            .env("FERRO_PATH", &path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = proc.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let mut acked = 0u64;
        for line in lines.by_ref() {
            if let Some(n) = line.unwrap().strip_prefix("ACK ") {
                acked = n.parse().unwrap();
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(rng.below(40)));
        proc.kill().unwrap();
        proc.wait().unwrap();
        for line in lines.map_while(Result::ok) {
            if let Some(n) = line.strip_prefix("ACK ")
                && let Ok(n) = n.parse::<u64>()
            {
                acked = acked.max(n);
            }
        }
        let db: Db<Root> = Options::new().open(&path).unwrap();
        let snapshot = db.read();
        check(&snapshot);
        assert_eq!(snapshot.n, db.version());
        assert!(
            snapshot.n >= acked && snapshot.n >= floor,
            "round {round}: acked {acked}, recovered {}",
            snapshot.n
        );
        db.verify().unwrap();
        floor = snapshot.n;
    }
}

#[cfg(unix)]
#[test]
fn sigkill_in_a_commit_loop_always_reopens_on_a_committed_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("k.ferro");
    let mut rng = Rng::new(0xC0FFEE);
    let mut acked_total = 0u64;
    let started = Instant::now();
    for round in 0..40 {
        let mut proc = child("killme")
            .env("FERRO_PATH", &path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = proc.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let mut acked = 0u64;
        for line in lines.by_ref() {
            if let Some(n) = line.unwrap().strip_prefix("ACK ") {
                acked = n.parse().unwrap();
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(rng.below(60)));
        proc.kill().unwrap();
        proc.wait().unwrap();
        for line in lines.map_while(Result::ok) {
            if let Some(n) = line.strip_prefix("ACK ")
                && let Ok(n) = n.parse::<u64>()
            {
                acked = acked.max(n);
            }
        }
        assert!(acked > acked_total, "round {round}: no progress");
        let db: Db<Root> = Options::new().open(&path).unwrap();
        let snapshot = db.read();
        check(&snapshot);
        assert_eq!(snapshot.n, db.version());
        assert!(
            snapshot.n >= acked && snapshot.n <= acked + 1,
            "round {round}: acked {acked}, recovered {}",
            snapshot.n
        );
        db.verify().unwrap();
        acked_total = snapshot.n;
        assert!(started.elapsed() < Duration::from_secs(60));
    }
}
