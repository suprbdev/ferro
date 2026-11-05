//! `booking-stress`: threads race for the same rooms and nights, then every version is checked.
//!
//! It seeds venues, rooms and customers, then runs `--threads` writers that hold, confirm and
//! cancel stays on a few hot nights while short holds lapse, a sweeper that expires them, a pricer
//! that changes rates, and `--browsers` readers that page customers' newest bookings from the
//! covering index. Writes go through [`booking::Reservations`], so each one is a
//! `try_commit_stamped` closure. By default it runs once without ferro's commit coalescing and once
//! with it and compares the write rates. Afterwards it walks the commit log with [`booking::walk`],
//! checking every transition between versions and every `--every`th version in full, and exits with
//! status 1 if any invariant is broken. `--buffered` and `--keep-last` select buffered durability
//! and bounded history.
//!
//! ```sh
//! make booking-stress ARGS='--threads 64'
//! cargo run --release -p booking --bin booking-stress -- --threads 16 --browsers 4
//! cargo run --release -p booking --bin booking-stress -- --coalesce on --secs 10 --rounds 3
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::hint::black_box;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use booking::date::{format_money, night_of};
use booking::seed::{Plan, seed};
use booking::{BookingError, Config, HoldRequest, Hotels, Line, Night, Reservations, Status, walk};
use clap::Parser;

#[derive(Parser, Clone, Debug)]
#[command(
    name = "booking-stress",
    about = "Contend for the same rooms and nights from many threads"
)]
struct Args {
    #[arg(long, default_value_t = 16)]
    threads: usize,
    #[arg(long, default_value_t = 5.0)]
    secs: f64,
    #[arg(long, default_value_t = 6)]
    venues: u32,
    #[arg(long, default_value_t = 20)]
    rooms: u32,
    #[arg(long, default_value_t = 180)]
    span: u32,
    #[arg(long, default_value_t = 3)]
    hot_nights: u32,
    #[arg(long, default_value_t = 400)]
    customers: u32,
    #[arg(long, default_value_t = 70)]
    hot: u32,
    #[arg(long, default_value_t = 15)]
    ttl_min: u64,
    #[arg(long, default_value_t = 120)]
    ttl_max: u64,
    #[arg(long, default_value_t = 500)]
    think_us: u64,
    #[arg(long, default_value_t = 10)]
    blind: u32,
    #[arg(long, default_value_t = 1)]
    late: u32,
    #[arg(long, value_parser = ["compare", "on", "off"], default_value = "compare")]
    coalesce: String,
    #[arg(long)]
    path: Option<PathBuf>,
    #[arg(long, default_value_t = 50)]
    every: u64,
    #[arg(long)]
    no_check: bool,
    #[arg(long, default_value_t = 1)]
    rounds: usize,
    #[arg(long)]
    progress: bool,
    #[arg(long)]
    buffered: bool,
    #[arg(long)]
    keep_last: Option<usize>,
    #[arg(long)]
    ack_log: Option<PathBuf>,
    #[arg(long, default_value_t = 7)]
    seed: u64,
    #[arg(long, default_value_t = 0)]
    browsers: usize,
    #[arg(long, default_value_t = 20)]
    page: usize,
}

#[derive(Default)]
struct Tally {
    attempts: u64,
    writes: u64,
    holds: u64,
    conflicts: u64,
    soldout: u64,
    confirms: u64,
    late: u64,
    broke: u64,
    abandoned: u64,
    cancels: u64,
    refunds: u64,
    takeovers: u64,
    reprices: u64,
    sweeps: u64,
    swept: u64,
    pages: u64,
    lines: u64,
    errors: BTreeMap<String, u64>,
    hold_ns: Vec<u64>,
    confirm_ns: Vec<u64>,
    sweep_ns: Vec<u64>,
    page_ns: Vec<u64>,
}

impl Tally {
    fn absorb(&mut self, other: Tally) {
        self.attempts += other.attempts;
        self.writes += other.writes;
        self.holds += other.holds;
        self.conflicts += other.conflicts;
        self.soldout += other.soldout;
        self.confirms += other.confirms;
        self.late += other.late;
        self.broke += other.broke;
        self.abandoned += other.abandoned;
        self.cancels += other.cancels;
        self.refunds += other.refunds;
        self.takeovers += other.takeovers;
        self.reprices += other.reprices;
        self.sweeps += other.sweeps;
        self.swept += other.swept;
        self.pages += other.pages;
        self.lines += other.lines;
        for (kind, count) in other.errors {
            *self.errors.entry(kind).or_default() += count;
        }
        self.hold_ns.extend(other.hold_ns);
        self.confirm_ns.extend(other.confirm_ns);
        self.sweep_ns.extend(other.sweep_ns);
        self.page_ns.extend(other.page_ns);
    }

    fn error(&mut self, error: &BookingError) {
        let kind = format!("{error:?}");
        let kind = kind
            .split([' ', '(', '{'])
            .next()
            .unwrap_or("?")
            .to_string();
        *self.errors.entry(kind).or_default() += 1;
    }
}

fn pct(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index] as f64 / 1e6
}

struct World {
    rooms: Vec<Vec<u32>>,
    customers: Vec<u32>,
    first: Night,
    acks: Option<Mutex<fs::File>>,
}

impl World {
    fn ack(&self, kind: char, id: u64) {
        if let Some(file) = &self.acks {
            let mut file = file.lock().expect("ack log");
            file.write_all(format!("{kind} {id}\n").as_bytes())
                .expect("write ack log");
        }
    }
}

fn world(svc: &Reservations, args: &Args) -> Result<World, BookingError> {
    if svc.read().rooms.is_empty() {
        let plan = Plan {
            venues: args.venues,
            rooms: args.rooms,
            customers: args.customers,
            credit: 150_000,
            rate: 9_000,
        };
        seed(svc, &plan)?;
    }
    let h = svc.read();
    let rooms = h
        .venues
        .iter()
        .map(|venue| h.rooms.by_venue(&venue.id).keys().collect())
        .collect();
    let customers = h.customers.keys().collect();
    let acks = match &args.ack_log {
        Some(path) => Some(Mutex::new(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|error| BookingError::Db(ferro::Error::Io(error)))?,
        )),
        None => None,
    };
    Ok(World {
        rooms,
        customers,
        first: night_of(svc.now()) + 3,
        acks,
    })
}

fn free(h: &Hotels, room: u32, first: Night, nights: u32, now: u64) -> bool {
    (first..first + nights).all(|night| h.holder(room, night).is_none_or(|id| h.lapsed(id, now)))
}

fn choose(
    svc: &Reservations,
    world: &World,
    args: &Args,
    rng: &mut fastrand::Rng,
) -> Option<HoldRequest> {
    let hot = rng.u32(0..100) < args.hot;
    let nights = rng.u32(1..=3).min(args.span);
    let start = if hot {
        rng.u32(
            0..=args
                .hot_nights
                .saturating_sub(nights)
                .min(args.span - nights),
        )
    } else {
        rng.u32(0..=args.span - nights)
    };
    let first = world.first + start;
    let home = if hot {
        0
    } else {
        rng.usize(0..world.rooms.len())
    };
    let h = svc.read();
    let now = svc.now();
    let mut room = None;
    for step in 0..world.rooms.len() {
        let venue = &world.rooms[(home + step) % world.rooms.len()];
        let mut open = venue
            .iter()
            .copied()
            .filter(|room| free(&h, *room, first, nights, now));
        room = if rng.u32(0..100) < 80 {
            open.next()
        } else {
            open.nth(rng.usize(0..venue.len()))
        };
        if room.is_some() {
            break;
        }
    }
    let room = match room {
        Some(room) => room,
        None if rng.u32(0..100) < args.blind => {
            let venue = &world.rooms[home];
            venue[rng.usize(0..venue.len())]
        }
        None => return None,
    };
    Some(HoldRequest {
        customer: world.customers[rng.usize(0..world.customers.len())],
        room,
        first,
        nights,
        ttl_ms: rng.u64(args.ttl_min..=args.ttl_max),
    })
}

fn worker(
    svc: Reservations,
    world: Arc<World>,
    args: Args,
    stop: Arc<AtomicBool>,
    index: usize,
) -> Tally {
    let mut rng =
        fastrand::Rng::with_seed(args.seed.wrapping_mul(1_000_003).wrapping_add(index as u64));
    let mut tally = Tally::default();
    let mut confirmed: Vec<u64> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if args.think_us > 0 {
            thread::sleep(Duration::from_micros(rng.u64(0..=args.think_us)));
        }
        let Some(request) = choose(&svc, &world, &args, &mut rng) else {
            tally.soldout += 1;
            continue;
        };
        tally.attempts += 1;
        let began = Instant::now();
        let held = svc.hold(&request);
        tally.hold_ns.push(began.elapsed().as_nanos() as u64);
        let held = match held {
            Ok(held) => held,
            Err(BookingError::Unavailable { .. }) => {
                tally.conflicts += 1;
                continue;
            }
            Err(error) => {
                tally.error(&error);
                continue;
            }
        };
        tally.writes += 1;
        tally.holds += 1;
        world.ack('H', held.id);
        tally.takeovers += held.released.len() as u64;
        if args.think_us > 0 {
            thread::sleep(Duration::from_micros(rng.u64(0..=args.think_us)));
        }
        let roll = rng.u32(0..100);
        if roll < 60 {
            tally.attempts += 1;
            let began = Instant::now();
            let paid = svc.confirm(held.id, Some(held.total));
            tally.confirm_ns.push(began.elapsed().as_nanos() as u64);
            match paid {
                Ok(_) => {
                    tally.writes += 1;
                    tally.confirms += 1;
                    confirmed.push(held.id);
                    world.ack('C', held.id);
                }
                Err(BookingError::HoldExpired { .. })
                | Err(BookingError::WrongStatus {
                    status: Status::Expired,
                    ..
                }) => tally.late += 1,
                Err(BookingError::InsufficientFunds { .. }) => {
                    tally.broke += 1;
                    tally.attempts += 1;
                    match svc.cancel(held.id) {
                        Ok(_) => tally.writes += 1,
                        Err(BookingError::WrongStatus { .. }) => tally.late += 1,
                        Err(error) => tally.error(&error),
                    }
                }
                Err(error) => tally.error(&error),
            }
        } else if roll < 60 + args.late {
            thread::sleep(Duration::from_millis(
                held.expires_ms.saturating_sub(svc.now()) + 1,
            ));
            tally.attempts += 1;
            match svc.confirm(held.id, Some(held.total)) {
                Err(BookingError::HoldExpired { .. }) | Err(BookingError::WrongStatus { .. }) => {
                    tally.late += 1
                }
                Ok(_) => panic!("confirmed #{} after its hold ran out", held.id),
                Err(error) => tally.error(&error),
            }
        } else if roll < 88 {
            tally.abandoned += 1;
        } else {
            tally.attempts += 1;
            match svc.cancel(held.id) {
                Ok(_) => {
                    tally.writes += 1;
                    tally.cancels += 1;
                }
                Err(BookingError::WrongStatus { .. }) => tally.late += 1,
                Err(error) => tally.error(&error),
            }
        }
        if !confirmed.is_empty() && rng.u32(0..100) < 20 {
            let id = confirmed.swap_remove(rng.usize(0..confirmed.len()));
            tally.attempts += 1;
            match svc.cancel(id) {
                Ok(refund) => {
                    tally.writes += 1;
                    tally.cancels += 1;
                    tally.refunds += u64::from(refund > 0);
                    world.ack('X', id);
                }
                Err(error) => tally.error(&error),
            }
        }
    }
    tally
}

fn sweeper(svc: Reservations, stop: Arc<AtomicBool>) -> Tally {
    let mut tally = Tally::default();
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(5));
        tally.sweeps += 1;
        let began = Instant::now();
        let swept = svc.sweep();
        tally.sweep_ns.push(began.elapsed().as_nanos() as u64);
        match swept {
            Ok(expired) if !expired.is_empty() => {
                tally.writes += 1;
                tally.swept += expired.len() as u64;
            }
            Ok(_) => {}
            Err(error) => tally.error(&error),
        }
    }
    tally
}

fn browser(
    svc: Reservations,
    world: Arc<World>,
    args: Args,
    stop: Arc<AtomicBool>,
    index: usize,
) -> Tally {
    let mut rng =
        fastrand::Rng::with_seed(args.seed ^ 0xb0b ^ (index as u64).wrapping_mul(0x9e37_79b9));
    let mut tally = Tally::default();
    while !stop.load(Ordering::Relaxed) {
        let customer = world.customers[rng.usize(0..world.customers.len())];
        let timed = tally.pages % 16 == 0;
        let began = Instant::now();
        let h = svc.read();
        let page: Vec<Line> = h.lines_of(customer).rev().take(args.page).collect();
        drop(h);
        if timed {
            tally.page_ns.push(began.elapsed().as_nanos() as u64);
        }
        tally.pages += 1;
        tally.lines += page.len() as u64;
        black_box(page);
    }
    tally
}

fn pricer(svc: Reservations, world: Arc<World>, args: Args, stop: Arc<AtomicBool>) -> Tally {
    let mut rng = fastrand::Rng::with_seed(args.seed ^ 0x5eed);
    let mut tally = Tally::default();
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(25));
        let rooms = &world.rooms[rng.usize(0..world.rooms.len())];
        let room = rooms[rng.usize(0..rooms.len())];
        let done = if rng.bool() {
            let rate = 8_000 + rng.i64(0..40) * 250;
            svc.set_rate(room, rate).map(|_| ())
        } else {
            let night = world.first + rng.u32(0..args.span.min(args.hot_nights * 4).max(1));
            let rate = (rng.u32(0..3) > 0).then(|| 15_000 + rng.i64(0..40) * 250);
            svc.set_night_rate(room, night, rate)
        };
        match done {
            Ok(()) => {
                tally.writes += 1;
                tally.reprices += 1;
            }
            Err(error) => tally.error(&error),
        }
    }
    tally
}

struct Outcome {
    label: String,
    secs: f64,
    tally: Tally,
    versions: u64,
    bytes: u64,
    walked: Option<booking::Walked>,
    held_left: usize,
    occupancy: String,
    space: Option<ferro::Space>,
}

fn run(path: &Path, coalesce: bool, args: &Args, progress: bool) -> Result<Outcome, BookingError> {
    let config = Config {
        coalesce,
        buffered: args.buffered,
        keep_last: args.keep_last,
        ..Config::default()
    };
    let svc = Reservations::open(path, config)?;
    let world = Arc::new(world(&svc, args)?);
    let stop = Arc::new(AtomicBool::new(false));
    let version_before = svc.db().version();
    let bytes_before = svc.db().size_on_disk();
    let started = Instant::now();
    let mut handles = Vec::new();
    for index in 0..args.threads {
        let (svc, world, args, stop) = (svc.clone(), world.clone(), args.clone(), stop.clone());
        handles.push(thread::spawn(move || worker(svc, world, args, stop, index)));
    }
    let quiet = Arc::new(AtomicBool::new(false));
    let sweeping = {
        let (svc, quiet) = (svc.clone(), quiet.clone());
        thread::spawn(move || sweeper(svc, quiet))
    };
    {
        let (svc, world, args, stop) = (svc.clone(), world.clone(), args.clone(), stop.clone());
        handles.push(thread::spawn(move || pricer(svc, world, args, stop)));
    }
    for index in 0..args.browsers {
        let (svc, world, args, stop) = (svc.clone(), world.clone(), args.clone(), stop.clone());
        handles.push(thread::spawn(move || {
            browser(svc, world, args, stop, index)
        }));
    }
    let deadline = Duration::from_secs_f64(args.secs);
    while started.elapsed() < deadline {
        thread::sleep(Duration::from_millis(200).min(deadline.saturating_sub(started.elapsed())));
        if progress {
            println!(
                "progress v{} {:.1}s",
                svc.db().version(),
                started.elapsed().as_secs_f64()
            );
            io::stdout().flush().ok();
        }
    }
    stop.store(true, Ordering::Relaxed);
    let mut tally = Tally::default();
    for handle in handles {
        tally.absorb(handle.join().expect("stress thread panicked"));
    }
    let secs = started.elapsed().as_secs_f64();
    let versions = svc.db().version() - version_before;
    let bytes = svc.db().size_on_disk() - bytes_before;
    thread::sleep(Duration::from_millis(args.ttl_max + 20));
    quiet.store(true, Ordering::Relaxed);
    tally.absorb(sweeping.join().expect("sweeper panicked"));
    svc.sweep()?;
    let snapshot = svc.read();
    let held_left = snapshot.bookings.by_status(&Status::Held).len();
    let occupancy = (0..args.span.min(args.hot_nights + 3))
        .map(|offset| {
            let night = world.first + offset;
            let sold: u32 = snapshot
                .venues
                .iter()
                .filter_map(|v| snapshot.loads.get(&(v.id, night)))
                .map(|l| l.sold)
                .sum();
            sold.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ");
    drop(snapshot);
    let walked = if args.no_check {
        None
    } else {
        Some(walk(svc.db(), args.every)?)
    };
    let space = args.keep_last.map(|_| svc.db().space()).transpose()?;
    if args.keep_last.is_some() {
        svc.db().verify()?;
    }
    Ok(Outcome {
        label: if coalesce {
            "coalesce".to_string()
        } else {
            "alone".to_string()
        },
        secs,
        tally,
        versions,
        bytes,
        walked,
        held_left,
        occupancy,
        space,
    })
}

fn print(outcome: &mut Outcome, svc: &Reservations) {
    let t = &mut outcome.tally;
    t.hold_ns.sort_unstable();
    t.confirm_ns.sort_unstable();
    t.sweep_ns.sort_unstable();
    t.page_ns.sort_unstable();
    let s = outcome.secs;
    println!("== {} ({:.1}s)", outcome.label, s);
    println!(
        "  writes {:>7} ({:>7.0}/s)  attempts {:>7} ({:>7.0}/s)  versions {:>6} ({:.2} writes each)  {:.0} bytes/write",
        t.writes,
        t.writes as f64 / s,
        t.attempts,
        t.attempts as f64 / s,
        outcome.versions,
        t.writes as f64 / outcome.versions.max(1) as f64,
        outcome.bytes as f64 / t.writes.max(1) as f64
    );
    println!(
        "  holds {} lost races {} sold out {} confirms {} late {} broke {} abandoned {} cancels {} refunds {} reprices {}",
        t.holds,
        t.conflicts,
        t.soldout,
        t.confirms,
        t.late,
        t.broke,
        t.abandoned,
        t.cancels,
        t.refunds,
        t.reprices
    );
    println!(
        "  lapsed holds taken over in place {}, released by the sweeper {} in {} sweeps",
        t.takeovers, t.swept, t.sweeps
    );
    println!(
        "  hold p50 {:.2} ms p99 {:.2} ms max {:.2} ms   confirm p50 {:.2} ms p99 {:.2} ms   sweep p50 {:.2} ms p99 {:.2} ms max {:.2} ms",
        pct(&t.hold_ns, 0.5),
        pct(&t.hold_ns, 0.99),
        pct(&t.hold_ns, 1.0),
        pct(&t.confirm_ns, 0.5),
        pct(&t.confirm_ns, 0.99),
        pct(&t.sweep_ns, 0.5),
        pct(&t.sweep_ns, 0.99),
        pct(&t.sweep_ns, 1.0)
    );
    if t.pages > 0 {
        println!(
            "  customer pages {} ({:.0}/s, {:.1} lines each)   page p50 {:.1} us p99 {:.1} us",
            t.pages,
            t.pages as f64 / s,
            t.lines as f64 / t.pages as f64,
            pct(&t.page_ns, 0.5) * 1000.0,
            pct(&t.page_ns, 0.99) * 1000.0
        );
    }
    if let Some(space) = &outcome.space {
        println!(
            "  file {:.1} MiB, about {:.1} MiB live, {} reclamation cycles, longest pause {:?}, last error {:?}",
            space.file_bytes as f64 / 1048576.0,
            space.live_bytes as f64 / 1048576.0,
            space.cycles,
            space.longest_pause,
            space.last_error
        );
    }
    if !t.errors.is_empty() {
        println!("  other errors {:?}", t.errors);
    }
    let h = svc.read();
    println!(
        "  rooms sold per night {}   holds left after the final sweep {}   charged {} refunded {}",
        outcome.occupancy,
        outcome.held_left,
        format_money(h.till.charged),
        format_money(h.till.refunded)
    );
    if let Some(walked) = &outcome.walked {
        println!(
            "  check v{}..v{}: {} transitions, {} full checks, {} takeovers, {} expiries (lag mean {:.1} ms, max {} ms), {} violations in {:.2?}",
            walked.first,
            walked.last,
            walked.steps,
            walked.full,
            walked.takeovers,
            walked.expired,
            walked.expiry_lag_mean_ms(),
            walked.expiry_lag_max_ms,
            walked.violations.len(),
            walked.elapsed
        );
        if let Some((lag, version, id)) = walked.slowest_expiry {
            let message = svc
                .db()
                .at(version)
                .map(|s| s.commit().message.replace('\n', " | "))
                .unwrap_or_default();
            let message: String = message.chars().take(160).collect();
            println!("  slowest release: #{id} {lag} ms late, in v{version}: {message}");
        }
        for violation in walked.violations.iter().take(10) {
            println!("  VIOLATION v{}: {}", violation.version, violation.message);
        }
    }
}

fn scratch() -> PathBuf {
    let path = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/booking-stress"
    ));
    fs::create_dir_all(&path).expect("create target/booking-stress");
    path
}

fn main() {
    let args = Args::parse();
    let modes: Vec<bool> = match args.coalesce.as_str() {
        "on" => vec![true],
        "off" => vec![false],
        _ => vec![false, true],
    };
    let mut failed = false;
    let mut summary: Vec<(String, Vec<f64>)> = Vec::new();
    for round in 0..args.rounds {
        for &coalesce in &modes {
            let path = match &args.path {
                Some(path) => path.clone(),
                None => {
                    let path = scratch().join(format!(
                        "stress-{}.ferro",
                        if coalesce { "on" } else { "off" }
                    ));
                    let _ = fs::remove_file(&path);
                    path
                }
            };
            let mut outcome = match run(&path, coalesce, &args, args.progress) {
                Ok(outcome) => outcome,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(2);
                }
            };
            let config = Config {
                coalesce,
                read_only: true,
                ..Config::default()
            };
            let svc = Reservations::open(&path, config).expect("reopen");
            if args.rounds > 1 {
                outcome.label = format!("{} round {}", outcome.label, round + 1);
            }
            print(&mut outcome, &svc);
            failed |= outcome.walked.as_ref().is_some_and(|walked| !walked.ok());
            let key = if coalesce { "coalesce" } else { "alone" }.to_string();
            let rate = outcome.tally.writes as f64 / outcome.secs;
            match summary.iter_mut().find(|(label, _)| *label == key) {
                Some((_, rates)) => rates.push(rate),
                None => summary.push((key, vec![rate])),
            }
        }
    }
    if summary.len() == 2 {
        let median = |rates: &mut Vec<f64>| {
            rates.sort_by(f64::total_cmp);
            rates[rates.len() / 2]
        };
        let alone = median(&mut summary[0].1.clone());
        let together = median(&mut summary[1].1.clone());
        println!(
            "== writes/s with {} threads: alone {:.0}, coalesce {:.0} ({:+.0}%)",
            args.threads,
            alone,
            together,
            (together / alone - 1.0) * 100.0
        );
    }
    if failed {
        eprintln!("invariant violations found");
        std::process::exit(1);
    }
}
