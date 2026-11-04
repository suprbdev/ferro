use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ferro::admin::{self, Layout};
use ferro::codec::{Decoder, Encoder};
use ferro::{Compacted, Data, Db, Durability, Keep, Options, Reclaim, Record, Table};
use ledger::Ledger;
use ledger::ops::NewEntry;
use metrics::load::{self, LoadOptions, synthetic_series};
use metrics::model::Metrics;
use metrics::tsdb::{Batch, Tsdb};
use wiki::index::Tx;
use wiki::model::Wiki;
use wiki::ops::{self, Edit};
use wiki::synth::{Rng, Vocab, prose};

const BLOCK: u64 = 4096;

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Free,
    Pro,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Account {
    #[key]
    id: u64,
    #[index]
    tier: Tier,
    #[index]
    country: String,
    email: String,
    name: String,
    balance: i64,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Bank {
    accounts: Table<Account>,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn account(i: u64) -> Account {
    let seed = mix(i);
    Account {
        id: i,
        tier: if seed.is_multiple_of(3) {
            Tier::Pro
        } else {
            Tier::Free
        },
        country: ["nz", "au", "de", "us", "jp", "br", "ca", "fr"][(seed >> 8) as usize % 8]
            .to_string(),
        email: format!("user{i}@example.com"),
        name: format!("Account Holder {}", seed % 100_000),
        balance: (seed % 1_000_000) as i64,
    }
}

fn fast() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 28)
}

fn build_metrics(path: &Path, seconds: u64) {
    let options = LoadOptions {
        rate: 20_000,
        seconds,
        series: 1000,
        threads: 4,
        batch: Duration::from_millis(1),
        interval_ms: 1000,
        start_ts: 1_700_000_000_000,
        keep: Keep::All,
        reclaim_bytes: 0,
        durability: Durability::Buffered,
    };
    let report = load::run(path, &options).unwrap();
    println!(
        "metrics {} points {} commits",
        report.points, report.commits
    );
}

fn build_ledger(path: &Path, entries: u64) {
    let db: Db<Ledger> = fast().open(path).unwrap();
    ledger::seed::run(&db, entries, 1).unwrap();
}

fn slug(index: u64) -> String {
    format!("page-{index}")
}

fn wiki_edit(db: &Db<Wiki>, edit: Edit) {
    db.try_commit_with::<_, _, Box<dyn std::error::Error>>(|root| {
        let mut tx = Tx::new(root);
        let outcome = ops::put(&mut tx, edit, 1);
        tx.finish();
        outcome.map(|_| ("edit".to_string(), ()))
    })
    .unwrap();
}

fn build_wiki(path: &Path, pages: u64, edits: u64) {
    let db: Db<Wiki> = fast().open(path).unwrap();
    let mut rng = Rng::new(7);
    let vocab = Vocab::new(2000);
    let mut texts = Vec::new();
    for index in 0..pages {
        let text = prose(&mut rng, &vocab, 64 * 1024, &mut |_| None);
        wiki_edit(
            &db,
            Edit {
                slug: slug(index),
                text: Some(text.clone()),
                ..Edit::default()
            },
        );
        texts.push(text);
    }
    for _ in 0..edits {
        let index = rng.below(pages);
        let text = &mut texts[index as usize];
        let at = text.floor_char_boundary(rng.below(text.len() as u64 - 1000) as usize);
        let mut insert = String::new();
        insert.push_str(vocab.word(&mut rng));
        insert.push(' ');
        text.insert_str(at, &insert);
        wiki_edit(
            &db,
            Edit {
                slug: slug(index),
                text: Some(text.clone()),
                ..Edit::default()
            },
        );
    }
}

fn build_table(path: &Path, rows: u64, updates: u64) {
    let db: Db<Bank> = fast().open(path).unwrap();
    db.update(|bank| {
        for i in 0..rows {
            bank.accounts.insert(account(i));
        }
    })
    .unwrap();
    for i in 0..updates {
        let id = mix(i) % rows;
        db.update(|bank| {
            bank.accounts.get_mut(&id).unwrap().balance += 1;
        })
        .unwrap();
    }
}

struct Pinned {
    blocks: u64,
    runs: u64,
}

fn pinned(extents: &[(u64, u32)], unit: u64) -> Pinned {
    let mut blocks = 0;
    let mut runs = 0;
    let mut last = u64::MAX;
    for (offset, len) in extents {
        let first = offset / unit;
        let tail = (offset + u64::from(*len) - 1) / unit;
        let from = if last != u64::MAX && first <= last {
            last + 1
        } else {
            first
        };
        if from <= tail {
            if last == u64::MAX || from > last + 1 {
                runs += 1;
            }
            blocks += tail + 1 - from;
            last = tail;
        }
    }
    Pinned { blocks, runs }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1 << 20) as f64
}

fn analyse(layout: &Layout) {
    let file = layout.file_bytes;
    let live: u64 = layout
        .extents
        .iter()
        .map(|extent| u64::from(extent.1))
        .sum();
    let garbage = file.saturating_sub(live).max(1);
    println!(
        "file {:.1} MiB, live {:.2} MiB in {} objects, mean {} bytes",
        mib(file),
        mib(live),
        layout.extents.len(),
        live / (layout.extents.len() as u64).max(1)
    );
    println!("unit         left MiB   reclaim of garbage   runs");
    for unit in [BLOCK, 64 << 10, 1 << 20, 16 << 20, 64 << 20] {
        let pins = pinned(&layout.extents, unit);
        let left = (pins.blocks * unit).min(file);
        let name = if unit < 1 << 20 {
            format!("{} KiB", unit >> 10)
        } else {
            format!("{} MiB", unit >> 20)
        };
        println!(
            "{name:<12} {:>8.1}   {:>17.1}%   {}",
            mib(left),
            100.0 * (file - left) as f64 / garbage as f64,
            pins.runs
        );
    }
    let mut spans = Vec::new();
    let mut at = 0;
    while at < layout.extents.len() {
        let first_block = layout.extents[at].0 / BLOCK;
        let mut last_block = (layout.extents[at].0 + u64::from(layout.extents[at].1) - 1) / BLOCK;
        let mut live_bytes = u64::from(layout.extents[at].1);
        let mut next = at + 1;
        while next < layout.extents.len() {
            let start = layout.extents[next].0 / BLOCK;
            if start > last_block + 1 {
                break;
            }
            live_bytes += u64::from(layout.extents[next].1);
            last_block = last_block
                .max((layout.extents[next].0 + u64::from(layout.extents[next].1) - 1) / BLOCK);
            next += 1;
        }
        spans.push((live_bytes, (last_block + 1 - first_block) * BLOCK));
        at = next;
    }
    println!("density    moved MiB   left MiB   reclaim of garbage");
    for density in [0.0, 0.25, 0.5, 0.75, 0.9, 1.01] {
        let mut moved = 0;
        let mut kept = 0;
        for (bytes, span) in &spans {
            if (*bytes as f64) < density * *span as f64 {
                moved += bytes;
            } else {
                kept += span;
            }
        }
        let left = kept + moved;
        println!(
            "{density:<10} {:>9.1}   {:>8.1}   {:>17.1}%",
            mib(moved),
            mib(left),
            100.0 * (file - left.min(file)) as f64 / garbage as f64
        );
    }
}

struct Run {
    seconds: u64,
    keep: usize,
    fill: f64,
    every: u64,
    reclaim: bool,
}

#[derive(Default)]
struct Outcome {
    commits: u64,
    latencies: Vec<f64>,
    cycles: Vec<Compacted>,
    samples: Vec<(f64, u64, u64)>,
    reads: u64,
    read_latencies: Vec<f64>,
    cycle_secs: f64,
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .map_or(default, |text| text.parse().unwrap())
}

fn drive<T: Data>(
    db: &Db<T>,
    writers: usize,
    run: &Run,
    step: impl Fn(usize, u64) + Sync,
    read: impl Fn(u64) + Sync,
) -> Outcome {
    let stop = AtomicBool::new(false);
    let latencies = Mutex::new(Vec::new());
    let commits = AtomicU64::new(0);
    let cycles = Mutex::new(Vec::new());
    let samples = Mutex::new(Vec::new());
    let reads = AtomicU64::new(0);
    let read_latencies = Mutex::new(Vec::new());
    let cycle_secs = Mutex::new(0.0f64);
    let start = Instant::now();
    thread::scope(|scope| {
        for reader in 0..env_number("RECLAIM_READERS", 0) {
            let (read, stop, reads, read_latencies) = (&read, &stop, &reads, &read_latencies);
            scope.spawn(move || {
                let mut local = Vec::new();
                let mut index = reader << 40;
                while !stop.load(Ordering::Relaxed) {
                    if index & 63 == 0 {
                        let began = Instant::now();
                        read(index);
                        local.push(began.elapsed().as_secs_f64() * 1e6);
                    } else {
                        read(index);
                    }
                    index += 1;
                }
                reads.fetch_add(index & ((1 << 40) - 1), Ordering::Relaxed);
                read_latencies.lock().unwrap().append(&mut local);
            });
        }
        for writer in 0..writers {
            let (step, stop, latencies, commits) = (&step, &stop, &latencies, &commits);
            scope.spawn(move || {
                let mut local = Vec::new();
                let mut index = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let began = Instant::now();
                    step(writer, index);
                    local.push(began.elapsed().as_secs_f64() * 1e3);
                    index += 1;
                }
                commits.fetch_add(index, Ordering::Relaxed);
                latencies.lock().unwrap().append(&mut local);
            });
        }
        if run.reclaim {
            let (stop, cycles, cycle_secs) = (&stop, &cycles, &cycle_secs);
            scope.spawn(move || {
                let mut last = 0;
                let mut live = 0;
                while !stop.load(Ordering::Relaxed) {
                    let space = db.space().unwrap();
                    last = last.min(space.file_bytes);
                    let goal = run.every.max(live);
                    if space.file_bytes.saturating_sub(last) < goal {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    let began = Instant::now();
                    let done = db.compact(Keep::Last(run.keep)).unwrap();
                    *cycle_secs.lock().unwrap() += began.elapsed().as_secs_f64();
                    live = done.after_bytes;
                    last = done.after_bytes;
                    cycles.lock().unwrap().push(done);
                }
            });
        }
        let (stop, samples) = (&stop, &samples);
        scope.spawn(move || {
            while start.elapsed() < Duration::from_secs(run.seconds) {
                let space = db.space().unwrap();
                let mut temp = db.path().into_os_string();
                temp.push(".compacting");
                let extra = std::fs::metadata(&temp).map_or(0, |meta| meta.len());
                samples.lock().unwrap().push((
                    start.elapsed().as_secs_f64(),
                    space.file_bytes + space.retired_bytes + extra,
                    space.file_bytes,
                ));
                thread::sleep(Duration::from_millis(50));
            }
            stop.store(true, Ordering::Relaxed);
        });
    });
    Outcome {
        commits: commits.load(Ordering::Relaxed),
        latencies: latencies.into_inner().unwrap(),
        cycles: cycles.into_inner().unwrap(),
        samples: samples.into_inner().unwrap(),
        reads: reads.load(Ordering::Relaxed),
        read_latencies: read_latencies.into_inner().unwrap(),
        cycle_secs: cycle_secs.into_inner().unwrap(),
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((p / 100.0 * sorted.len() as f64) as usize).min(sorted.len() - 1)]
}

fn report<T: Data>(name: &str, db: &Db<T>, run: &Run, mut outcome: Outcome) {
    outcome.latencies.sort_by(|a, b| a.total_cmp(b));
    outcome.read_latencies.sort_by(|a, b| a.total_cmp(b));
    let lat = &outcome.latencies;
    let reads = &outcome.read_latencies;
    let mut pauses: Vec<f64> = outcome
        .cycles
        .iter()
        .map(|c| c.pause.as_secs_f64() * 1e3)
        .collect();
    pauses.sort_by(|a, b| a.total_cmp(b));
    let moved: u64 = outcome.cycles.iter().map(|c| c.after_bytes).sum();
    let ratios: Vec<f64> = outcome
        .cycles
        .iter()
        .map(|c| {
            let garbage = c.before_bytes.saturating_sub(c.after_bytes).max(1);
            c.before_bytes.saturating_sub(c.after_bytes) as f64 / garbage as f64
        })
        .collect();
    let ratio = ratios.iter().sum::<f64>() / ratios.len().max(1) as f64;
    let after = outcome.cycles.last().map_or(0, |c| c.after_bytes);
    let peak = outcome.samples.iter().map(|s| s.1).max().unwrap_or(0);
    let space = db.space().unwrap();
    let verified = db.verify().unwrap();
    let written = outcome.cycles.iter().map(|c| c.before_bytes).sum::<u64>() + space.file_bytes;
    let garbage = written.saturating_sub(verified.bytes).max(1);
    let reclaimed = written.saturating_sub(space.file_bytes);
    println!(
        "{name} fill={} keep={} commits/s={:.0} p50={:.2}ms p99={:.2}ms p999={:.2}ms max={:.1}ms cycles={} cycle_secs={:.2} pause_p50={:.3}ms pause_max={:.2}ms pause_sum={:.1}ms moved={:.1}MiB log={:.1}MiB peak={:.1}MiB alloc={:.1}MiB live={:.1}MiB reclaim={:.1}% cycle_ratio={:.1}% after_cycle={:.1}MiB reads/s={:.0} read_p50={:.2}us read_p99={:.2}us read_p999={:.1}us read_max={:.0}us",
        run.fill,
        run.keep,
        outcome.commits as f64 / run.seconds as f64,
        percentile(lat, 50.0),
        percentile(lat, 99.0),
        percentile(lat, 99.9),
        lat.last().copied().unwrap_or(0.0),
        outcome.cycles.len(),
        outcome.cycle_secs,
        percentile(&pauses, 50.0),
        pauses.iter().copied().fold(0.0, f64::max),
        pauses.iter().sum::<f64>(),
        mib(moved),
        mib(space.file_bytes),
        mib(peak),
        mib(space.file_bytes + space.retired_bytes),
        mib(verified.bytes),
        100.0 * reclaimed as f64 / garbage as f64,
        100.0 * ratio,
        mib(after),
        outcome.reads as f64 / run.seconds as f64,
        percentile(reads, 50.0),
        percentile(reads, 99.0),
        percentile(reads, 99.9),
        reads.last().copied().unwrap_or(0.0),
    );
    if std::env::var("RECLAIM_SAMPLES").is_ok() {
        for (at, allocated, log) in &outcome.samples {
            println!(
                "sample {name} {at:.2} {:.1} {:.1}",
                mib(*allocated),
                mib(*log)
            );
        }
    }
}

fn open_options(run: &Run) -> Options {
    let durability = if env_number("RECLAIM_BUFFERED", 0) == 1 {
        Durability::Buffered
    } else {
        Durability::Fsync
    };
    Options::new()
        .durability(durability)
        .cache_bytes(1 << 28)
        .history(Keep::Last(run.keep))
        .reclaim(Reclaim { every_bytes: 0 })
}

fn run_metrics(path: &Path, run: &Run) {
    let db: Db<Metrics> = open_options(run).open(path).unwrap();
    let tsdb = Tsdb { db: db.clone() };
    let keys = synthetic_series(1000);
    let writers: Vec<_> = (0..4).map(|_| Mutex::new(tsdb.writer())).collect();
    let keys = &keys;
    let outcome = drive(
        &db,
        4,
        run,
        |writer, index| {
            let mut batch = Batch::default();
            let group = 250 / 25;
            let at = (index as usize * group) % 250;
            for slot in 0..group {
                let key = &keys[writer + 4 * ((at + slot) % 250)];
                let ts = 1_700_000_000_000 + (index / 25) * 10_000;
                batch.push(key, ts, (index % 1000) as f64 + slot as f64);
            }
            writers[writer].lock().unwrap().write(batch).unwrap();
        },
        |_| {},
    );
    report("metrics", &db, run, outcome);
}

fn run_ledger(path: &Path, run: &Run) {
    let db: Db<Ledger> = open_options(run).open(path).unwrap();
    ledger::seed::run(&db, 1, 1).unwrap();
    let outcome = drive(
        &db,
        4,
        run,
        |writer, index| {
            let entry = NewEntry::transfer(
                19_500,
                "payment",
                "assets:bank-1",
                "assets:bank-2",
                100 + index as i64 + writer as i64,
            );
            db.try_commit_with::<_, _, ledger::LedgerError>(|root| {
                let id = root.post(entry, 1)?;
                Ok((format!("entry {id}"), ()))
            })
            .unwrap();
        },
        |_| {},
    );
    report("ledger", &db, run, outcome);
}

fn run_wiki(path: &Path, run: &Run) {
    let db: Db<Wiki> = open_options(run).open(path).unwrap();
    let mut rng = Rng::new(7);
    let vocab = Vocab::new(2000);
    let pages = 100u64;
    let mut texts = Vec::new();
    for index in 0..pages {
        let text = prose(&mut rng, &vocab, 64 * 1024, &mut |_| None);
        wiki_edit(
            &db,
            Edit {
                slug: slug(index),
                text: Some(text.clone()),
                ..Edit::default()
            },
        );
        texts.push(Mutex::new(text));
    }
    let outcome = drive(
        &db,
        2,
        run,
        |writer, index| {
            let page = mix(index * 2 + writer as u64) % pages;
            let mut text = texts[page as usize].lock().unwrap();
            let at = text.floor_char_boundary((mix(index) % (text.len() as u64 - 1000)) as usize);
            text.insert_str(at, "edit ");
            wiki_edit(
                &db,
                Edit {
                    slug: slug(page),
                    text: Some(text.clone()),
                    ..Edit::default()
                },
            );
        },
        |_| {},
    );
    report("wiki", &db, run, outcome);
}

fn run_table(path: &Path, run: &Run) {
    let rows = env_number("RECLAIM_ROWS", 200_000);
    {
        let db: Db<Bank> = fast().open(path).unwrap();
        db.update(|bank| {
            for i in 0..rows {
                bank.accounts.insert(account(i));
            }
        })
        .unwrap();
    }
    let db: Db<Bank> = open_options(run).open(path).unwrap();
    let branches: Vec<Db<Bank>> = (0..env_number("RECLAIM_BRANCHES", 1).max(1))
        .map(|at| match at {
            0 => db.clone(),
            _ => db.fork(&format!("b{at}")).unwrap(),
        })
        .collect();
    let outcome = drive(
        &db,
        4,
        run,
        |writer, index| {
            let pick = mix(index * 4 + writer as u64);
            let id = pick % rows;
            let db = &branches[(pick >> 32) as usize % branches.len()];
            db.update(|bank| {
                bank.accounts.get_mut(&id).unwrap().balance += 1;
            })
            .unwrap();
        },
        |index| {
            let id = mix(index) % rows;
            assert!(db.read().accounts.get(&id).is_some());
        },
    );
    report("table", &db, run, outcome);
}

fn live(args: &[String]) {
    let (workload, path) = (args[2].as_str(), Path::new(&args[3]));
    let number = |index: usize, default: f64| -> f64 {
        args.get(index)
            .map_or(default, |text| text.parse().unwrap())
    };
    let run = Run {
        seconds: number(4, 20.0) as u64,
        keep: number(5, 100.0) as usize,
        fill: number(6, 0.5),
        every: (number(7, 64.0) as u64) << 20,
        reclaim: number(6, 0.5) >= 0.0,
    };
    let _ = std::fs::remove_file(path);
    match workload {
        "metrics" => run_metrics(path, &run),
        "ledger" => run_ledger(path, &run),
        "wiki" => run_wiki(path, &run),
        "table" => run_table(path, &run),
        other => panic!("unknown workload {other}"),
    }
    let _ = std::fs::remove_file(path);
}

#[derive(Clone, Default)]
struct Opaque;

impl Data for Opaque {
    fn encode(&self, _w: &mut Encoder<'_>) -> ferro::Result<()> {
        Ok(())
    }

    fn decode(_r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Opaque)
    }
}

fn copy(path: &Path, rounds: u64, threads: &[usize]) {
    let mut name = path.as_os_str().to_os_string();
    name.push(".copy");
    let work = PathBuf::from(name);
    let bytes = fs::metadata(path).unwrap().len();
    for _ in 0..rounds {
        for &count in threads {
            fs::copy(path, &work).unwrap();
            File::open(&work).unwrap().sync_all().unwrap();
            let db: Db<Opaque> = Options::new()
                .reclaim(Reclaim { every_bytes: 0 })
                .copy_threads(count)
                .open(&work)
                .unwrap();
            let began = Instant::now();
            let done = db.compact(Keep::All).unwrap();
            let secs = began.elapsed().as_secs_f64();
            println!(
                "copy threads={count} secs={secs:.3} mb_per_s={:.0} objects={} after={:.1}MiB pause={:.2}ms",
                bytes as f64 / secs / 1e6,
                done.objects,
                mib(done.after_bytes),
                done.pause.as_secs_f64() * 1e3
            );
            drop(db);
            let _ = fs::remove_file(&work);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args[1] == "live" {
        live(&args);
        return;
    }

    let command = args[1].as_str();
    let path = Path::new(&args[2]);
    let number = |index: usize, default: u64| -> u64 {
        args.get(index)
            .map_or(default, |text| text.parse().unwrap())
    };
    match command {
        "metrics" => build_metrics(path, number(3, 60)),
        "ledger" => build_ledger(path, number(3, 200_000)),
        "wiki" => build_wiki(path, number(3, 100), number(4, 20_000)),
        "table" => build_table(path, number(3, 200_000), number(4, 200_000)),
        "copy" => {
            let threads: Vec<usize> = args[4..].iter().map(|text| text.parse().unwrap()).collect();
            copy(path, number(3, 3), &threads);
        }
        "layout" => {
            for keep in args[3..].iter().map(|text| text.parse::<usize>().unwrap()) {
                println!("--- keep last {keep}");
                analyse(&admin::layout(path, Keep::Last(keep)).unwrap());
            }
        }
        other => panic!("unknown command {other}"),
    }
}
