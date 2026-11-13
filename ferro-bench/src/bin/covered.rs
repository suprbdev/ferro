//! Measures what covered index fields cost in index size, commits, root decodes and opens.
//!
//! Builds four tables of `--rows` accounts (default 200,000), each in a `Durability::Buffered` file
//! of its own in a temporary directory under `--dir` (default `target/bench`), compacted to its
//! last version: `bare` with no index, `plain` with plain indexes on `tier` and `balance`,
//! `narrow` with both indexes covering `balance`, and `wide`, whose `tier` index also covers
//! `name` and `email`. Each of `--rounds` rounds (default 3) runs `--commits` (default 2,000)
//! balance updates, renames and inserts on each table, starting from a different table each round,
//! and prints each table's bytes per commit and median thread CPU microseconds on stderr. The
//! summary on stdout, headed by `--label` (default `run`), gives each table's file size, its index
//! size over `bare` and the medians of those per-round figures, then the size, links and decode
//! time of its root object, and last the time to open the `plain` and `narrow` files with the
//! schema they were written with and with the next wider one.
//!
//! ```sh
//! make bench-covered ARGS='--rows 200000 --commits 2000 --rounds 3'
//! cargo run --release -p ferro-bench --bin covered -- --label covered --dir target/bench
//! ```

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ferro::codec::{Decoder, Encoder, HashSink};
use ferro::{Data, Db, Durability, Keep, Options, Record, Table};

const BALANCES: u64 = 1_000_000;
const CHUNK: u64 = 100_000;

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Free,
    Pro,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn fields(i: u64) -> (Tier, String, String, i64) {
    let seed = mix(i);
    let tier = if seed.is_multiple_of(3) {
        Tier::Pro
    } else {
        Tier::Free
    };
    (
        tier,
        format!("user{i}@example.com"),
        format!("Account Holder {}", seed % 100_000),
        (seed % BALANCES) as i64,
    )
}

trait Bank: Data + Default {
    fn fill(&mut self, ids: Range<u64>);
    fn set_balance(&mut self, id: u64, balance: i64);
    fn set_name(&mut self, id: u64, name: String);
}

macro_rules! variant {
    ($record:ident, $root:ident, [$($tier:tt)*], [$($balance:tt)*]) => {
        #[derive(Data, Record, Clone, Debug, PartialEq)]
        struct $record {
            #[key]
            id: u64,
            $($tier)*
            tier: Tier,
            email: String,
            name: String,
            $($balance)*
            balance: i64,
        }

        #[derive(Data, Clone, Debug, Default, PartialEq)]
        struct $root {
            accounts: Table<$record>,
        }

        impl Bank for $root {
            fn fill(&mut self, ids: Range<u64>) {
                for id in ids {
                    let (tier, email, name, balance) = fields(id);
                    self.accounts.insert($record {
                        id,
                        tier,
                        email,
                        name,
                        balance,
                    });
                }
            }

            fn set_balance(&mut self, id: u64, balance: i64) {
                self.accounts.update(&id, |row| row.balance = balance);
            }

            fn set_name(&mut self, id: u64, name: String) {
                self.accounts.update(&id, |row| row.name = name);
            }
        }
    };
}

variant!(Bare, BareBank, [], []);
variant!(Plain, PlainBank, [#[index]], [#[index]]);
variant!(Narrow, NarrowBank, [#[index(cover(balance))]], [#[index(cover(balance))]]);
variant!(Wide, WideBank, [#[index(cover(name, email, balance))]], [#[index(cover(balance))]]);

#[cfg(unix)]
fn cpu_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a live, writable `timespec`, the only memory `clock_gettime` writes, and a
    // clock the system lacks only makes the call return an error.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

#[cfg(not(unix))]
fn cpu_ns() -> u64 {
    0
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

#[derive(Clone, Copy, Default)]
struct Cost {
    bytes: f64,
    cpu_us: f64,
}

const KINDS: [&str; 3] = ["balance", "name", "insert"];
const DECODES: u32 = 20_000;

struct Decoded {
    body: usize,
    links: usize,
    ns: f64,
}

trait Subject {
    fn name(&self) -> &'static str;
    fn size(&self) -> u64;
    fn round(&mut self, commits: u64) -> [Cost; 3];
    fn decode(&self, rounds: usize) -> Decoded;
}

fn reopen<B: Bank>(path: &Path, rounds: usize) -> f64 {
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = Instant::now();
        let db = Options::new()
            .durability(Durability::Buffered)
            .open::<B>(path)
            .unwrap();
        std::hint::black_box(&*db.read());
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        drop(db);
    }
    median(samples)
}

struct Opened<B: Bank> {
    name: &'static str,
    db: Db<B>,
    rows: u64,
    added: u64,
    rng: u64,
    size: u64,
}

impl<B: Bank> Opened<B> {
    fn build(name: &'static str, dir: &Path, rows: u64) -> Self {
        let db = Options::new()
            .durability(Durability::Buffered)
            .open::<B>(dir.join(format!("{name}.ferro")))
            .unwrap();
        for chunk in 0..rows.div_ceil(CHUNK) {
            db.update(|bank| bank.fill(chunk * CHUNK..((chunk + 1) * CHUNK).min(rows)))
                .unwrap();
        }
        db.compact(Keep::Last(1)).unwrap();
        let size = db.log_bytes();
        Opened {
            name,
            db,
            rows,
            added: 0,
            rng: mix(rows) | 1,
            size,
        }
    }

    fn commits(&mut self, count: u64, mut edit: impl FnMut(&mut B, &mut u64, u64)) -> Cost {
        let before = self.db.log_bytes();
        let mut cpu = Vec::with_capacity(count as usize);
        for i in 0..count {
            let rng = &mut self.rng;
            let started = cpu_ns();
            self.db.update(|bank| edit(bank, rng, i)).unwrap();
            cpu.push((cpu_ns() - started) as f64 / 1000.0);
        }
        Cost {
            bytes: (self.db.log_bytes() - before) as f64 / count as f64,
            cpu_us: median(cpu),
        }
    }
}

impl<B: Bank> Subject for Opened<B> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn round(&mut self, commits: u64) -> [Cost; 3] {
        let rows = self.rows;
        let balance = self.commits(commits, |bank, rng, _| {
            let id = next(rng) % rows;
            bank.set_balance(id, (next(rng) % BALANCES) as i64);
        });
        let name = self.commits(commits, |bank, rng, i| {
            let id = next(rng) % rows;
            bank.set_name(id, format!("Renamed Holder {i}"));
        });
        let start = rows + self.added;
        let insert = self.commits(commits, |bank, _, i| bank.fill(start + i..start + i + 1));
        self.added += commits;
        [balance, name, insert]
    }

    fn decode(&self, rounds: usize) -> Decoded {
        let root = self.db.read();
        let mut sink = HashSink;
        let mut encoder = Encoder::new(&mut sink);
        root.encode(&mut encoder).unwrap();
        let object = encoder.into_object();
        let mut samples = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            let started = Instant::now();
            for _ in 0..DECODES {
                let mut decoder = Decoder::new(&object.body, &object.links, None);
                std::hint::black_box(B::decode(&mut decoder).unwrap());
            }
            samples.push(started.elapsed().as_nanos() as f64 / f64::from(DECODES));
        }
        Decoded {
            body: object.body.len(),
            links: object.links.len(),
            ns: median(samples),
        }
    }
}

fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn main() {
    let rows: u64 = argument("--rows")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);
    let commits: u64 = argument("--commits")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    let rounds: usize = argument("--rounds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let label = argument("--label").unwrap_or_else(|| "run".to_string());
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let mut subjects: Vec<Box<dyn Subject>> = vec![
        Box::new(Opened::<BareBank>::build("bare", dir.path(), rows)),
        Box::new(Opened::<PlainBank>::build("plain", dir.path(), rows)),
        Box::new(Opened::<NarrowBank>::build("narrow", dir.path(), rows)),
        Box::new(Opened::<WideBank>::build("wide", dir.path(), rows)),
    ];
    let mut samples: Vec<Vec<[Cost; 3]>> = vec![Vec::new(); subjects.len()];
    for round in 0..rounds {
        let count = subjects.len();
        for step in 0..count {
            let at = (step + round) % count;
            let costs = subjects[at].round(commits);
            eprintln!(
                "round {round} {:<6} {}",
                subjects[at].name(),
                KINDS
                    .iter()
                    .zip(&costs)
                    .map(|(kind, cost)| format!("{kind} {:.0} B {:.1} us", cost.bytes, cost.cpu_us))
                    .collect::<Vec<_>>()
                    .join("  ")
            );
            samples[at].push(costs);
        }
    }
    let bare = subjects[0].size();
    println!(
        "{label}: {rows} rows, {commits} commits of each kind per round, medians of {rounds} rounds"
    );
    println!(
        "{:<8} {:>9} {:>11} {:>14} {:>9} {:>14} {:>9} {:>14} {:>9}",
        "variant",
        "file MiB",
        "index MiB",
        "balance B/op",
        "cpu us",
        "name B/op",
        "cpu us",
        "insert B/op",
        "cpu us"
    );
    for (subject, rounds) in subjects.iter().zip(&samples) {
        let pick = |kind: usize, value: fn(&Cost) -> f64| {
            median(rounds.iter().map(|costs| value(&costs[kind])).collect())
        };
        println!(
            "{:<8} {:>9.2} {:>11.2} {:>14.0} {:>9.1} {:>14.0} {:>9.1} {:>14.0} {:>9.1}",
            subject.name(),
            subject.size() as f64 / 1048576.0,
            subject.size().saturating_sub(bare) as f64 / 1048576.0,
            pick(0, |cost| cost.bytes),
            pick(0, |cost| cost.cpu_us),
            pick(1, |cost| cost.bytes),
            pick(1, |cost| cost.cpu_us),
            pick(2, |cost| cost.bytes),
            pick(2, |cost| cost.cpu_us),
        );
    }
    println!(
        "{:<8} {:>11} {:>7} {:>11}",
        "variant", "root bytes", "links", "decode ns"
    );
    for subject in &subjects {
        let decoded = subject.decode(rounds.max(5));
        println!(
            "{:<8} {:>11} {:>7} {:>11.1}",
            subject.name(),
            decoded.body,
            decoded.links,
            decoded.ns
        );
    }
    drop(subjects);
    let file = |name: &str| dir.path().join(format!("{name}.ferro"));
    let opens = [
        (
            "plain",
            "plain",
            reopen::<PlainBank>(&file("plain"), rounds.max(5)),
        ),
        (
            "narrow",
            "narrow",
            reopen::<NarrowBank>(&file("narrow"), rounds.max(5)),
        ),
        (
            "plain",
            "narrow",
            reopen::<NarrowBank>(&file("plain"), rounds.max(5)),
        ),
        (
            "narrow",
            "wide",
            reopen::<WideBank>(&file("narrow"), rounds.max(5)),
        ),
    ];
    println!("{:<8} {:<9} {:>9}", "file", "opened as", "open ms");
    for (written, opened, ms) in opens {
        println!("{written:<8} {opened:<9} {ms:>9.2}");
    }
}
