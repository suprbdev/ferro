mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ferro::db::Hook;
use ferro::{Data, Db, Durability, Keep, Map, Options, Reclaim};

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    n: u64,
    order: Map<u64, (u64, u64)>,
    last: Map<u64, u64>,
}

const MANUAL: Reclaim = Reclaim { every_bytes: 0 };

fn env(name: &str) -> String {
    std::env::var(name).unwrap()
}

fn number(name: &str) -> u64 {
    env(name).parse().unwrap()
}

fn options() -> Options {
    let durability = match std::env::var("FERRO_SHARED_DURABILITY").as_deref() {
        Ok("buffered") => Durability::Buffered,
        _ => Durability::Fsync,
    };
    Options::new()
        .shared(true)
        .durability(durability)
        .reclaim(MANUAL)
}

fn abort_at(step: String, nth: u64) -> Hook {
    let seen = AtomicU64::new(0);
    Hook(Arc::new(move |name| {
        if name == step && seen.fetch_add(1, Ordering::Relaxed) + 1 == nth {
            std::process::abort();
        }
    }))
}

fn commit(db: &Db<Root>, writer: u64, i: u64) -> u64 {
    db.update(|root| {
        root.n += 1;
        root.order.insert(root.n, (writer, i));
        root.last.insert(writer, i);
        root.n
    })
    .unwrap()
}

#[test]
fn shared_child() {
    let Ok(role) = std::env::var("FERRO_SHARED_ROLE") else {
        return;
    };
    let path = env("FERRO_SHARED_PATH");
    let writer = number("FERRO_SHARED_WRITER");
    let first = number("FERRO_SHARED_FIRST");
    let count = number("FERRO_SHARED_COUNT");
    if role == "reader" {
        follow(&path, count);
        return;
    }
    let mut options = options();
    if let Ok(step) = std::env::var("FERRO_SHARED_ABORT") {
        options = options.hook(abort_at(step, number("FERRO_SHARED_NTH")));
    }
    let cycles = role == "cycler";
    if cycles {
        options = options.history(Keep::Last(4));
    }
    let db: Db<Root> = options.open(&path).unwrap();
    for i in first..first + count {
        let n = commit(&db, writer, i);
        println!("ACK {writer} {i} {n}");
        if cycles && i % 25 == 0 {
            db.compact(Keep::Last(4)).unwrap();
            println!("CYCLE {i}");
        }
    }
    println!("DONE {writer}");
}

fn follow(path: &str, until: u64) {
    let db: Db<Root> = loop {
        match Options::new().read_only().open(path) {
            Ok(db) => break db,
            Err(ferro::Error::NoDatabase(_) | ferro::Error::NotFerro(_)) => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = 0;
    let mut refreshes = 0;
    while seen < until {
        db.refresh().unwrap();
        refreshes += 1;
        let root = db.read();
        assert!(root.n >= seen, "went back from {seen} to {}", root.n);
        assert_eq!(root.order.len() as u64, root.n);
        seen = root.n;
        assert!(Instant::now() < deadline, "the reader stopped at {seen}");
    }
    println!("FOLLOWED {seen} {refreshes}");
}

struct Spawned {
    child: Child,
}

fn spawn(
    path: &Path,
    role: &str,
    writer: u64,
    first: u64,
    count: u64,
    extra: &[(&str, String)],
) -> Spawned {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "shared_child", "--nocapture", "--test-threads=1"])
        .env("FERRO_SHARED_ROLE", role)
        .env("FERRO_SHARED_PATH", path)
        .env("FERRO_SHARED_WRITER", writer.to_string())
        .env("FERRO_SHARED_FIRST", first.to_string())
        .env("FERRO_SHARED_COUNT", count.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (name, value) in extra {
        command.env(name, value);
    }
    Spawned {
        child: command.spawn().unwrap(),
    }
}

#[derive(Default)]
struct Acks {
    by_n: BTreeMap<u64, (u64, u64)>,
    done: Vec<u64>,
}

impl Spawned {
    fn finish(mut self, acks: &mut Acks, limit: Duration) -> bool {
        let stdout = self.child.stdout.take().unwrap();
        let started = Instant::now();
        let reader = std::thread::spawn(move || {
            BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
                .collect::<Vec<_>>()
        });
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if started.elapsed() > limit {
                self.child.kill().unwrap();
                panic!("a child timed out");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        for line in reader.join().unwrap() {
            if let Some((_, rest)) = line.split_once("DONE ") {
                acks.done.push(rest.trim().parse().unwrap());
                continue;
            }
            let Some((_, rest)) = line.split_once("ACK ") else {
                continue;
            };
            let fields: Vec<u64> = rest.split(' ').filter_map(|f| f.parse().ok()).collect();
            if let [writer, i, n] = fields[..] {
                assert!(
                    acks.by_n.insert(n, (writer, i)).is_none(),
                    "{n} acked twice"
                );
            }
        }
        status.success()
    }
}

fn check(path: &Path, acks: &Acks, complete: &[(u64, u64)]) -> Root {
    let db: Db<Root> = Options::new().reclaim(MANUAL).open(path).unwrap();
    let root = (*db.read()).clone();
    assert_eq!(db.version(), root.n);
    assert_eq!(root.order.len() as u64, root.n, "a commit was lost");
    let mut next: BTreeMap<u64, u64> = BTreeMap::new();
    for n in 1..=root.n {
        let (writer, i) = *root
            .order
            .get(&n)
            .unwrap_or_else(|| panic!("n={n} missing"));
        let expected = next.entry(writer).or_insert(i);
        assert!(i >= *expected, "writer {writer} reordered at n={n}");
        *expected = i + 1;
    }
    for (n, acked) in &acks.by_n {
        assert_eq!(
            root.order.get(n).as_deref(),
            Some(acked),
            "acked n={n} changed"
        );
    }
    for &(writer, last) in complete {
        assert_eq!(*root.last.get(&writer).unwrap(), last, "writer {writer}");
    }
    db.verify().unwrap();
    root
}

fn rounds(durability: &str, writers: u64, each: u64) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("shared.ferro");
    let extra = [("FERRO_SHARED_DURABILITY", durability.to_string())];
    let children: Vec<_> = (0..writers)
        .map(|writer| spawn(&path, "writer", writer, 1, each, &extra))
        .collect();
    let mut acks = Acks::default();
    for child in children {
        assert!(child.finish(&mut acks, Duration::from_secs(120)));
    }
    assert_eq!(acks.by_n.len() as u64, writers * each);
    let complete: Vec<_> = (0..writers).map(|writer| (writer, each)).collect();
    let root = check(&path, &acks, &complete);
    assert_eq!(root.n, writers * each);
}

#[test]
fn processes_writing_one_file_keep_every_acknowledged_commit_in_order() {
    rounds("fsync", 4, 150);
}

#[test]
fn buffered_processes_writing_one_file_keep_every_commit_in_order() {
    rounds("buffered", 6, 200);
}

#[cfg(unix)]
#[test]
fn a_process_that_dies_inside_a_commit_holds_up_no_other() {
    for (step, nth) in [
        ("drafted", 40),
        ("landing", 60),
        ("landed", 80),
        ("drafted", 1),
    ] {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let path = dir.path().join("dies.ferro");
        let steady = spawn(&path, "writer", 0, 1, 400, &[]);
        let abort = [
            ("FERRO_SHARED_ABORT", step.to_string()),
            ("FERRO_SHARED_NTH", nth.to_string()),
        ];
        let dying = spawn(&path, "writer", 1, 1, 400, &abort);
        let mut acks = Acks::default();
        assert!(
            !dying.finish(&mut acks, Duration::from_secs(120)),
            "{step}: no abort"
        );
        let died_after = acks
            .by_n
            .values()
            .filter(|(writer, _)| *writer == 1)
            .count() as u64;
        assert!(died_after < nth, "{step}: acked {died_after}");
        let again = spawn(&path, "writer", 1, nth + 1, 100, &[]);
        assert!(steady.finish(&mut acks, Duration::from_secs(120)), "{step}");
        assert!(again.finish(&mut acks, Duration::from_secs(120)), "{step}");
        let root = check(&path, &acks, &[(0, 400), (1, nth + 100)]);
        let landed = root.order.values().filter(|pair| pair.0 == 1).count() as u64;
        assert!(
            landed == nth + 99 || landed == nth + 100,
            "{step}: {landed}"
        );
        assert_eq!(root.n, 400 + landed, "{step}");
    }
}

#[cfg(unix)]
#[test]
fn a_process_killed_at_random_while_others_write_loses_only_unacknowledged_commits() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("killed.ferro");
    let mut rng = common::Rng::new(0x5EED);
    let steady = spawn(&path, "writer", 0, 1, 1500, &[]);
    let mut acks = Acks::default();
    for round in 0..8 {
        let first = round * 1_000_000 + 1;
        let mut victim = spawn(&path, "writer", 1, first, 100_000, &[]);
        std::thread::sleep(Duration::from_millis(20 + rng.below(80)));
        victim.child.kill().unwrap();
        victim.finish(&mut acks, Duration::from_secs(60));
    }
    assert!(steady.finish(&mut acks, Duration::from_secs(120)));
    check(&path, &acks, &[(0, 1500)]);
}

#[test]
fn cycles_in_one_process_while_others_write_keep_every_commit() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("cycles.ferro");
    let writers: Vec<_> = (0..3)
        .map(|writer| spawn(&path, "writer", writer, 1, 300, &[]))
        .collect();
    let cycler = spawn(&path, "cycler", 3, 1, 200, &[]);
    let reader = spawn(&path, "reader", 0, 0, 1100, &[]);
    let mut acks = Acks::default();
    for child in writers {
        assert!(child.finish(&mut acks, Duration::from_secs(120)));
    }
    assert!(cycler.finish(&mut acks, Duration::from_secs(120)));
    assert!(reader.finish(&mut acks, Duration::from_secs(120)));
    let complete = [(0, 300), (1, 300), (2, 300), (3, 200)];
    let root = check(&path, &acks, &complete);
    assert_eq!(root.n, 1100);
}

#[cfg(unix)]
#[test]
fn a_cycle_killed_at_every_stage_keeps_the_other_processes_writing() {
    for stage in [
        "created", "copied", "synced", "paused", "swapped", "retired",
    ] {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let path = dir.path().join("cycle-killed.ferro");
        let writers: Vec<_> = (0..2)
            .map(|writer| spawn(&path, "writer", writer, 1, 300, &[]))
            .collect();
        let abort = [
            ("FERRO_SHARED_ABORT", stage.to_string()),
            ("FERRO_SHARED_NTH", "2".to_string()),
        ];
        let cycler = spawn(&path, "cycler", 2, 1, 200, &abort);
        let mut acks = Acks::default();
        assert!(
            !cycler.finish(&mut acks, Duration::from_secs(120)),
            "{stage}"
        );
        for child in writers {
            assert!(child.finish(&mut acks, Duration::from_secs(120)), "{stage}");
        }
        let root = check(&path, &acks, &[(0, 300), (1, 300)]);
        let cycled = acks
            .by_n
            .values()
            .filter(|(writer, _)| *writer == 2)
            .count() as u64;
        assert!(root.n >= 600 + cycled, "{stage}");
    }
}
