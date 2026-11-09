use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use ferro::admin::{self, Keep};
use ferro::{Db, Durability, Map, Options};

type Root = Map<u64, String>;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "ferro-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn command(args: &[&str], path: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferro"));
    command.arg(args[0]).arg(path).args(&args[1..]);
    command
}

fn ferro(args: &[&str], path: &Path) -> Output {
    command(args, path).output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn many_commits(path: &Path, commits: u64) {
    let db = Options::new()
        .durability(Durability::Buffered)
        .open::<Root>(path)
        .unwrap();
    for i in 0..commits {
        db.commit(format!("commit number {i}"), |root| {
            root.insert(i % 50, format!("value {i}"));
        })
        .unwrap();
    }
}

fn large_commits(db: &Db<Root>, commits: u64) {
    for i in 0..commits {
        db.commit(format!("c{i}"), |root| {
            root.insert(i, "x".repeat(2000));
        })
        .unwrap();
    }
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

fn mib(bytes: u64) -> String {
    format!("{:.2} MiB", bytes as f64 / 1048576.0)
}

fn shared_file_left_with_preallocated_zeroes(path: &Path) {
    let open = || Options::new().shared(true).open::<Root>(path).unwrap();
    let (a, b) = (open(), open());
    for i in 0..300u64 {
        let db = if i % 2 == 0 { &a } else { &b };
        db.commit(format!("c{i}"), |root| {
            root.insert(i, "x".repeat(2000));
        })
        .unwrap();
    }
    drop((a, b));
}

#[test]
fn log_into_a_pipe_the_reader_closed_ends_quietly_with_success() {
    let path = scratch("pipe.ferro");
    many_commits(&path, 5000);
    let mut child = command(&["log", "--limit", "100000"], &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert!(first.starts_with("v5000 "), "{first}");
    let output = child.wait_with_output().unwrap();
    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
}

#[cfg(target_os = "linux")]
#[test]
fn a_failed_write_to_stdout_is_an_error_with_status_1() {
    let path = scratch("full.ferro");
    many_commits(&path, 10);
    let output = command(&["info"], &path)
        .stdout(std::fs::File::create("/dev/full").unwrap())
        .output()
        .unwrap();
    let stderr = text(&output.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(stderr.starts_with("ferro: "), "{stderr}");
    assert_eq!(output.status.code(), Some(1), "{stderr}");
}

#[test]
fn info_space_and_verify_report_the_length_of_a_file_shared_writers_closed() {
    let path = scratch("shared.ferro");
    shared_file_left_with_preallocated_zeroes(&path);
    let on_disk = file_len(&path);
    let info = admin::info(&path).unwrap();
    let space = admin::space(&path).unwrap();
    let verified = admin::verify(&path).unwrap();
    assert_eq!(
        (info.file_bytes, space.file_bytes, verified.file_bytes),
        (on_disk, on_disk, on_disk)
    );
    assert!(space.log_bytes < space.file_bytes, "{space:?}");
    assert!(space.live_bytes <= space.log_bytes, "{space:?}");
    let printed = text(&ferro(&["space"], &path).stdout);
    assert_eq!(
        printed,
        format!(
            "file  {}\nlog   {}\nlive  {} (estimate)\n",
            mib(on_disk),
            mib(space.log_bytes),
            mib(space.live_bytes)
        )
    );
}

#[test]
fn info_and_space_report_the_length_of_a_file_a_writer_has_open() {
    let path = scratch("open.ferro");
    let db = Db::<Root>::open(&path).unwrap();
    large_commits(&db, 300);
    let on_disk = file_len(&path);
    let space = admin::space(&path).unwrap();
    assert_eq!(admin::info(&path).unwrap().file_bytes, on_disk);
    assert_eq!(space.file_bytes, on_disk);
    assert_eq!(space.log_bytes, db.size_on_disk());
    let own = db.space().unwrap();
    assert!(own.live_bytes <= own.log_bytes, "{own:?}");
    assert!(own.log_bytes <= own.file_bytes, "{own:?}");
    assert!(own.file_bytes <= file_len(&path), "{own:?}");
}

#[test]
fn compact_reports_the_lengths_of_the_file_before_and_after() {
    let path = scratch("compact.ferro");
    shared_file_left_with_preallocated_zeroes(&path);
    let before = file_len(&path);
    let report = admin::compact(&path, Keep::All).unwrap();
    assert_eq!(report.before_bytes, before);
    assert_eq!(report.after_bytes, file_len(&path));
    assert!(report.after_bytes < report.before_bytes, "{report:?}");
}

#[test]
fn a_cycle_reports_the_lengths_of_the_file_before_and_after() {
    let path = scratch("cycle.ferro");
    let db = Db::<Root>::open(&path).unwrap();
    large_commits(&db, 300);
    let before = file_len(&path);
    let report = db.compact(Keep::Last(1)).unwrap();
    assert_eq!(report.before_bytes, before);
    assert_eq!(report.after_bytes, file_len(&path));
}

#[test]
fn log_prints_one_line_per_commit_with_its_message_escaped() {
    let path = scratch("lines.ferro");
    let db = Db::<Root>::open(&path).unwrap();
    db.commit("first", |root| {
        root.insert(1, "a".into());
    })
    .unwrap();
    db.commit("one\nv1  0  forged\t\\ \u{1b}[31m", |root| {
        root.insert(2, "b".into());
    })
    .unwrap();
    let version = db.version();
    drop(db);
    let output = ferro(&["log", "--limit", "1"], &path);
    let stdout = text(&output.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(
        stdout.ends_with("  one\\nv1  0  forged\\t\\\\ \\u{1b}[31m\n"),
        "{stdout}"
    );
    assert!(stdout.starts_with(&format!("v{version}  ")), "{stdout}");
}

#[test]
fn log_joins_the_messages_of_a_coalesced_commit_on_one_line() {
    let path = scratch("coalesced.ferro");
    let db = Options::new().coalesce(true).open::<Root>(&path).unwrap();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for writer in 0..8u64 {
            let (db, barrier) = (&db, &barrier);
            scope.spawn(move || {
                barrier.wait();
                for i in 0..50u64 {
                    db.commit(format!("w{writer}\nn{i}"), |root| {
                        root.insert(writer * 1000 + i, "v".into());
                    })
                    .unwrap();
                }
            });
        }
    });
    let commits = db.log().len();
    drop(db);
    let output = ferro(&["log", "--limit", "1000"], &path);
    let stdout = text(&output.stdout);
    assert_eq!(stdout.lines().count(), commits, "{stdout}");
    let messages: usize = stdout.lines().map(|line| line.split("; ").count()).sum();
    assert_eq!(messages, 400, "{stdout}");
}
