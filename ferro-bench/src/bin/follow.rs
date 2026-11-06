//! Measures how soon a follower sees another process's commit, and what following costs.
//!
//! `--how` lists the followers to compare: `notify` for the default, `poll:<ms>` for one that looks
//! every so many milliseconds, and `none` for no follower at all (default
//! `none,notify,poll:1,poll:10,poll:100`). Each round, `--rounds` of them (default 3), runs every
//! follower in turn on a fresh file in a temporary directory under `--dir` (default
//! `target/bench`), with the writer in a copy of this program, through three phases:
//!
//! - latency: the writer makes `--commits` single-key commits (default 300), a random 0 to 2 times
//!   `--gap-ms` apart (default 20), and prints the `CLOCK_MONOTONIC` time each was acknowledged;
//!   the follower's watcher notes how long after that it received each one, and how many it skipped
//!   because a later one had landed first;
//! - idle: no writer, for `--idle-secs` (default 60), noting the CPU time of this process and of
//!   the follower's thread, and the thread's voluntary context switches and looks;
//! - busy: the writer commits as fast as it can for `--busy-secs` (default 5), noting its commits
//!   and the follower thread's CPU time, looks and refreshes.
//!
//! `none` runs only the busy phase, as the writer's baseline. The follower is read-only and the
//! writer a plain one, unless `--shared` makes both shared writers. Commits are
//! `Durability::Buffered` unless `--durability fsync`. It prints a Markdown row for each run and
//! then the median of the rounds for each follower. The `--child` and `--path` flags are for the
//! copy it starts. It reads `/proc` and `CLOCK_MONOTONIC`, so it runs on Linux only.
//!
//! ```sh
//! cargo run --release -p ferro-bench --bin follow -- --how notify,poll:10 --idle-secs 10
//! ```

#[cfg(target_os = "linux")]
mod measure {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use ferro::{Data, Db, Durability, Map, Options};

    #[derive(Data, Clone, Default)]
    struct Root {
        n: u64,
        keys: Map<u64, u64>,
    }

    fn argument(name: &str) -> Option<String> {
        let args: Vec<String> = std::env::args().collect();
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1).cloned())
    }

    fn number(name: &str, default: u64) -> u64 {
        argument(name)
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    }

    fn clock(id: libc::clockid_t) -> u64 {
        let mut spec = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `spec` is a live, writable `timespec` that the call fills in.
        unsafe { libc::clock_gettime(id, &mut spec) };
        spec.tv_sec as u64 * 1_000_000_000 + spec.tv_nsec as u64
    }

    fn now() -> u64 {
        clock(libc::CLOCK_MONOTONIC)
    }

    fn process_cpu() -> u64 {
        clock(libc::CLOCK_PROCESS_CPUTIME_ID)
    }

    fn follower_task() -> Option<PathBuf> {
        std::fs::read_dir("/proc/self/task")
            .ok()?
            .filter_map(Result::ok)
            .map(|task| task.path())
            .find(|task| {
                std::fs::read_to_string(task.join("comm"))
                    .is_ok_and(|comm| comm.trim() == "ferro-follow")
            })
    }

    fn thread_cpu(task: &Path) -> u64 {
        std::fs::read_to_string(task.join("schedstat"))
            .ok()
            .and_then(|stat| stat.split_whitespace().next()?.parse().ok())
            .unwrap_or(0)
    }

    fn thread_switches(task: &Path) -> u64 {
        std::fs::read_to_string(task.join("status"))
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find(|line| line.starts_with("voluntary_ctxt_switches"))?
                    .split_whitespace()
                    .nth(1)?
                    .parse()
                    .ok()
            })
            .unwrap_or(0)
    }

    fn percentile(sorted: &[u64], fraction: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
        sorted[index] as f64 / 1000.0
    }

    fn durability_of(name: &str) -> Durability {
        match name {
            "fsync" => Durability::Fsync,
            _ => Durability::Buffered,
        }
    }

    fn commit(db: &Db<Root>) -> u64 {
        db.update(|root| {
            root.n += 1;
            root.keys.insert(root.n % 1000, root.n);
            root.n
        })
        .unwrap()
    }

    fn child(role: &str, path: &Path, shared: bool, durability: Durability) {
        let options = Options::new().durability(durability).shared(shared);
        let db: Db<Root> = options.open(path).unwrap();
        match role {
            "latency" => {
                let commits = number("--commits", 300);
                let gap = number("--gap-ms", 20) * 1000;
                for _ in 0..commits {
                    thread::sleep(Duration::from_micros(fastrand::u64(0..=2 * gap)));
                    let n = commit(&db);
                    println!("ACK {n} {}", now());
                }
            }
            _ => {
                let secs = number("--busy-secs", 5);
                let started = Instant::now();
                let mut commits = 0;
                while started.elapsed() < Duration::from_secs(secs) {
                    commit(&db);
                    commits += 1;
                }
                println!("BUSY {commits}");
            }
        }
    }

    fn spawn(role: &str, path: &Path, shared: bool, durability: &str) -> Vec<String> {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--child", role, "--path"])
            .arg(path)
            .args(["--durability", durability])
            .stdout(Stdio::piped());
        for name in ["--commits", "--gap-ms", "--busy-secs"] {
            if let Some(value) = argument(name) {
                command.args([name, &value]);
            }
        }
        if shared {
            command.arg("--shared");
        }
        let mut running = command.spawn().unwrap();
        let lines = BufReader::new(running.stdout.take().unwrap())
            .lines()
            .map_while(Result::ok)
            .collect();
        assert!(running.wait().unwrap().success());
        lines
    }

    const COLUMNS: [&str; 12] = [
        "After ack p50 µs",
        "p99 µs",
        "max µs",
        "skipped",
        "idle CPU ms",
        "thread ms",
        "switches",
        "looks",
        "busy commits",
        "thread ms",
        "looks",
        "refreshes",
    ];

    fn latency(db: &Db<Root>, path: &Path, shared: bool, durability: &str) -> [f64; 4] {
        let watcher = db.watch();
        let commits = number("--commits", 300);
        let receiver = thread::spawn(move || {
            let mut seen = Vec::new();
            let mut last = 0;
            while last < commits {
                let Ok(snapshot) = watcher.recv_timeout(Duration::from_secs(30)) else {
                    break;
                };
                seen.push((snapshot.n, now()));
                last = snapshot.n;
            }
            seen
        });
        let out = spawn("latency", path, shared, durability);
        let seen = receiver.join().unwrap();
        let acks: HashMap<u64, u64> = out
            .iter()
            .filter_map(|line| {
                let mut fields = line.strip_prefix("ACK ")?.split(' ');
                Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
            })
            .collect();
        let mut lags: Vec<u64> = seen
            .iter()
            .filter_map(|(n, at)| Some(at.saturating_sub(*acks.get(n)?)))
            .collect();
        lags.sort_unstable();
        [
            percentile(&lags, 0.5),
            percentile(&lags, 0.99),
            lags.last().map_or(0.0, |max| *max as f64 / 1000.0),
            (commits - seen.len() as u64) as f64,
        ]
    }

    fn run(how: &str, dir: &Path, shared: bool, durability: &str) -> [f64; 12] {
        let mut row = [f64::NAN; 12];
        let path = dir.join(format!(
            "{}-{}.ferro",
            how.replace(':', "-"),
            fastrand::u64(..)
        ));
        let plain = Options::new().durability(durability_of(durability));
        if how == "none" {
            drop(plain.open::<Root>(&path).unwrap());
            let out = spawn("busy", &path, shared, durability);
            row[8] = busy_commits(&out);
            return row;
        }
        let mut follow = plain.clone().follow(true);
        if let Some(ms) = how.strip_prefix("poll:") {
            follow = follow.follow_polling(Duration::from_millis(ms.parse().unwrap()));
        }
        let db: Db<Root> = match shared {
            true => follow.shared(true).open(&path).unwrap(),
            false => {
                drop(plain.open::<Root>(&path).unwrap());
                follow.read_only(true).open(&path).unwrap()
            }
        };
        row[..4].copy_from_slice(&latency(&db, &path, shared, durability));
        let task = follower_task().expect("a ferro-follow thread");
        let idle = Duration::from_secs(number("--idle-secs", 60));
        let (cpu, thread_was, switches, looks) = (
            process_cpu(),
            thread_cpu(&task),
            thread_switches(&task),
            db.following().unwrap().looks,
        );
        thread::sleep(idle);
        row[4] = (process_cpu() - cpu) as f64 / 1e6;
        row[5] = (thread_cpu(&task) - thread_was) as f64 / 1e6;
        row[6] = (thread_switches(&task) - switches) as f64;
        row[7] = (db.following().unwrap().looks - looks) as f64;
        let before = db.following().unwrap();
        let thread_was = thread_cpu(&task);
        let out = spawn("busy", &path, shared, durability);
        let after = db.following().unwrap();
        row[8] = busy_commits(&out);
        row[9] = (thread_cpu(&task) - thread_was) as f64 / 1e6;
        row[10] = (after.looks - before.looks) as f64;
        row[11] = (after.refreshes - before.refreshes) as f64;
        row
    }

    fn busy_commits(out: &[String]) -> f64 {
        out.iter()
            .find_map(|line| line.strip_prefix("BUSY "))
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0.0)
    }

    fn print(label: &str, row: &[f64; 12]) {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(column, value)| match (value.is_nan(), column) {
                (true, _) => "-".to_string(),
                (false, 4 | 5 | 9) => format!("{value:.1}"),
                (false, _) => format!("{value:.0}"),
            })
            .collect();
        println!("| {label} | {} |", cells.join(" | "));
    }

    pub fn main() {
        let shared = std::env::args().any(|arg| arg == "--shared");
        let durability = argument("--durability").unwrap_or_else(|| "buffered".to_string());
        if let (Some(role), Some(path)) = (argument("--child"), argument("--path")) {
            child(&role, Path::new(&path), shared, durability_of(&durability));
            return;
        }
        let root = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(&root).unwrap();
        let hows =
            argument("--how").unwrap_or_else(|| "none,notify,poll:1,poll:10,poll:100".to_string());
        let hows: Vec<&str> = hows.split(',').collect();
        let rounds = number("--rounds", 3);
        println!("| Follower | {} |", COLUMNS.join(" | "));
        println!("| --- |{}", " ---: |".repeat(COLUMNS.len()));
        let mut runs: HashMap<&str, Vec<[f64; 12]>> = HashMap::new();
        for round in 1..=rounds {
            for how in &hows {
                let row = run(how, dir.path(), shared, &durability);
                print(&format!("{how} #{round}"), &row);
                runs.entry(how).or_default().push(row);
            }
        }
        println!("\nMedian of {rounds} rounds:\n");
        println!("| Follower | {} |", COLUMNS.join(" | "));
        println!("| --- |{}", " ---: |".repeat(COLUMNS.len()));
        for how in &hows {
            let rows = &runs[how];
            let mut median = [f64::NAN; 12];
            for (column, cell) in median.iter_mut().enumerate() {
                let mut values: Vec<f64> = rows.iter().map(|row| row[column]).collect();
                values.sort_by(f64::total_cmp);
                *cell = values[values.len() / 2];
            }
            print(how, &median);
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    measure::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("follow reads /proc and CLOCK_MONOTONIC, so it runs on Linux only");
}
