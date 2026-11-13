//! Splits commits' frames into their records and bounds what a delta log would write instead.
//!
//! Works in a temporary directory under `--dir` (default `target/bench`), and `--workload` picks
//! `map`, `booking` or `all` (the default). Every commit is `Durability::Buffered` but the
//! `--fsync` ones. `map` loads `--n` scrambled keys (default 1,000,000) into a `Map<u64, String>`
//! and makes `--commits` single-key inserts (default 1,000). It reads each commit's frame back
//! from the file, splits its bytes into header and table, tree, root, log entry and refs, and
//! walks each new root against the one before to size the record a delta commit would write. It
//! goes on to time the commit outside the closure against `hash_of` of the dirty root inside it,
//! to size a checkpoint of 1 to 65,536 inserts, to time commits to a 16-key map, and to time
//! `--fsync` commits with `Durability::Fsync` (default 300, 0 to skip) to the large map and the
//! small one in turn. `booking` splits and walks the frames of `--pairs` (default 2,000) of the
//! `booking` example's hold and confirm pairs, with a cancel on every fourth, after `--warm`
//! warm-up pairs (default 4,000). It then times their commits against `hash_of` the same way, and
//! a commit that changes one root field alone, and sizes checkpoints of 1 to 4,096 writes. It
//! prints bytes per commit by part, and medians of wall and thread CPU microseconds with mean
//! user-space instructions, counted through `perf_event_open` on Linux.
//!
//! ```sh
//! cargo run --release -p ferro-bench --bin delta -- --workload map --n 1000000 --commits 1000
//! cargo run --release -p ferro-bench --bin delta -- --workload booking --pairs 2000 --fsync 0
//! ```

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use booking::date::DAY_MS;
use booking::seed::{Plan, Seeded, seed};
use booking::{Cents, HoldRequest, Hotels, Reservations, Why};
use ferro::codec::{HASH_BYTES, Object};
use ferro::store::{Store, StoreOptions};
use ferro::{Db, Durability, Map, Options, Ptr, hash_of};

const FRAME_BYTES: usize = 68;
const BLOCK_BYTES: usize = 4096;
const BLOCK_CHECK: usize = 8;

#[cfg(target_os = "linux")]
struct Counter(Option<i32>);

#[cfg(not(target_os = "linux"))]
struct Counter;

#[cfg(not(target_os = "linux"))]
impl Counter {
    fn instructions() -> Counter {
        Counter
    }

    fn read(&self) -> u64 {
        0
    }
}

#[cfg(target_os = "linux")]
impl Counter {
    fn instructions() -> Counter {
        let mut attr = [0u64; 16];
        attr[0] = 128 << 32;
        attr[1] = 1;
        attr[5] = 1 | 1 << 5 | 1 << 6;
        // SAFETY: `attr` is an initialized 128-byte `perf_event_attr` whose `size` field says 128,
        // with every byte past the fields set here zero, and it outlives the call, which only
        // reads it. Any argument the kernel rejects makes the call return -1.
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0, -1, -1, 0) };
        if fd < 0 {
            return Counter(None);
        }
        // SAFETY: `fd` is the descriptor `perf_event_open` just returned, checked non-negative
        // above, and `0x2400` is `PERF_EVENT_IOC_ENABLE`, whose argument is an integer the kernel
        // does not dereference.
        unsafe { libc::ioctl(fd as i32, 0x2400, 0) };
        Counter(Some(fd as i32))
    }

    fn read(&self) -> u64 {
        let Some(fd) = self.0 else {
            return 0;
        };
        let mut value = 0u64;
        // SAFETY: the pointer is to `value`, a live, writable `u64`, so all 8 bytes `read` may
        // write land in it, and `fd` came from a successful `perf_event_open` and is never closed.
        unsafe { libc::read(fd, (&mut value as *mut u64).cast(), 8) };
        value
    }
}

#[cfg(not(unix))]
fn cpu_ns() -> u64 {
    0
}

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

#[derive(Clone, Copy, Default)]
struct Sample {
    wall: u64,
    cpu: u64,
    instructions: u64,
}

impl Sample {
    fn now(counter: &Counter, began: Instant) -> Sample {
        Sample {
            wall: began.elapsed().as_nanos() as u64,
            cpu: cpu_ns(),
            instructions: counter.read(),
        }
    }

    fn since(self, start: Sample) -> Sample {
        Sample {
            wall: self.wall - start.wall,
            cpu: self.cpu - start.cpu,
            instructions: self.instructions - start.instructions,
        }
    }

    fn minus(self, other: Sample) -> Sample {
        Sample {
            wall: self.wall.saturating_sub(other.wall),
            cpu: self.cpu.saturating_sub(other.cpu),
            instructions: self.instructions.saturating_sub(other.instructions),
        }
    }
}

fn median(mut values: Vec<u64>) -> f64 {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or(0) as f64 / 1000.0
}

fn summary(samples: &[Sample]) -> String {
    let mean =
        samples.iter().map(|s| s.instructions).sum::<u64>() as f64 / samples.len().max(1) as f64;
    format!(
        "cpu {:>6.1} us  wall {:>6.1} us  instructions {:>8.0}",
        median(samples.iter().map(|s| s.cpu).collect()),
        median(samples.iter().map(|s| s.wall).collect()),
        mean
    )
}

fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn number(name: &str, default: u64) -> u64 {
    argument(name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn varint_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

fn varint(bytes: &[u8], pos: &mut usize) -> u64 {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = bytes[*pos];
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

struct Record {
    offset: u64,
    len: usize,
    links: Vec<Ptr>,
}

fn table_bytes(frame: usize) -> usize {
    BLOCK_CHECK * (frame - FRAME_BYTES).div_ceil(BLOCK_BYTES + BLOCK_CHECK)
}

fn records(frame: &[u8], start: u64) -> Vec<Record> {
    let mut pos = FRAME_BYTES;
    let mut out = Vec::new();
    while pos < frame.len() - table_bytes(frame.len()) {
        let at = pos;
        let offset = start + at as u64;
        let stored = varint(frame, &mut pos) as usize;
        let flagged = varint(frame, &mut pos);
        if flagged & 1 == 1 {
            varint(frame, &mut pos);
        }
        pos += stored;
        let mut links = Vec::new();
        for _ in 0..flagged >> 1 {
            let hash = frame[pos..pos + HASH_BYTES].try_into().unwrap();
            pos += HASH_BYTES;
            let begun = pos;
            let value = varint(frame, &mut pos);
            let delta = match value & 1 {
                0 => value >> 1,
                _ => (value ^ (1 << (7 * (pos - begun - 1)))) >> 1,
            };
            links.push(Ptr {
                hash,
                offset: offset.wrapping_sub(delta),
            });
        }
        out.push(Record {
            offset,
            len: pos - at,
            links,
        });
    }
    out
}

#[derive(Clone, Copy, Default)]
struct Parts {
    commits: u64,
    header: u64,
    tree: u64,
    root: u64,
    entry: u64,
    refs: u64,
    objects: u64,
    delta: u64,
}

impl Parts {
    fn add(&mut self, other: Parts) {
        self.commits += other.commits;
        self.header += other.header;
        self.tree += other.tree;
        self.root += other.root;
        self.entry += other.entry;
        self.refs += other.refs;
        self.objects += other.objects;
        self.delta += other.delta;
    }

    fn total(&self) -> u64 {
        self.header + self.tree + self.root + self.entry + self.refs
    }

    fn report(&self, name: &str) {
        let n = self.commits.max(1) as f64;
        let per = |v: u64| v as f64 / n;
        let floor = per(self.header + self.entry + self.refs + self.delta);
        println!(
            "  {name:<8} bytes {:>7.0} = header {:>3.0} + tree {:>6.0} + root {:>5.0} + entry {:>4.0} + refs {:>4.0}  objects {:>5.1}  object delta {:>5.0}  delta frame {:>5.0} ({:.1}x)",
            per(self.total()),
            per(self.header),
            per(self.tree),
            per(self.root),
            per(self.entry),
            per(self.refs),
            per(self.objects),
            per(self.delta),
            floor,
            per(self.total()) / floor.max(1.0),
        );
    }
}

struct Framed {
    start: u64,
    end: u64,
    root: Ptr,
    parts: Parts,
}

fn frame_of(mut file: &File, start: u64, end: u64) -> Framed {
    let mut frame = vec![0u8; (end - start) as usize];
    file.seek(SeekFrom::Start(start)).unwrap();
    file.read_exact(&mut frame).unwrap();
    let recs = records(&frame, start);
    let n = recs.len();
    assert!(n >= 4, "a commit frame holds at least four records");
    let entry = &recs[n - 3];
    let root = entry.links[0];
    assert_eq!(
        root.offset,
        recs[n - 4].offset,
        "the record before the entry is the root"
    );
    assert!(
        recs[n - 2]
            .links
            .iter()
            .any(|link| link.offset == entry.offset)
    );
    let tree = recs[..n - 4].iter().map(|r| r.len as u64).sum();
    Framed {
        start,
        end,
        root,
        parts: Parts {
            commits: 1,
            header: (FRAME_BYTES + table_bytes(frame.len())) as u64,
            tree,
            root: recs[n - 4].len as u64,
            entry: entry.len as u64,
            refs: (recs[n - 2].len + recs[n - 1].len) as u64,
            objects: (n - 3) as u64,
            delta: 0,
        },
    }
}

fn body_diff(old: &[u8], new: &[u8]) -> usize {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    let middle = new.len() - prefix - suffix;
    let ends =
        varint_len(prefix as u64) + varint_len(suffix as u64) + varint_len(middle as u64) + middle;
    ends.min(copies(old, new))
}

fn copies(old: &[u8], new: &[u8]) -> usize {
    let mut index: HashMap<[u8; 4], Vec<usize>> = HashMap::new();
    for at in 0..old.len().saturating_sub(3) {
        index
            .entry(old[at..at + 4].try_into().unwrap())
            .or_default()
            .push(at);
    }
    let (mut cost, mut literal, mut at) = (0, 0, 0);
    while at < new.len() {
        let mut best = (0, 0);
        if at + 4 <= new.len()
            && let Some(starts) = index.get(&new[at..at + 4])
        {
            for &from in starts.iter().take(16) {
                let len = old[from..]
                    .iter()
                    .zip(&new[at..])
                    .take_while(|(a, b)| a == b)
                    .count();
                if len > best.1 {
                    best = (from, len);
                }
            }
        }
        if best.1 >= 4 {
            if literal > 0 {
                cost += varint_len(literal as u64) + literal;
                literal = 0;
            }
            cost += varint_len(best.0 as u64) + varint_len(best.1 as u64);
            at += best.1;
        } else {
            literal += 1;
            at += 1;
        }
    }
    if literal > 0 {
        cost += varint_len(literal as u64) + literal;
    }
    cost
}

struct Walker {
    store: Store,
    objects: HashMap<Ptr, Arc<Object>>,
}

impl Walker {
    fn get(&mut self, ptr: Ptr) -> Arc<Object> {
        if let Some(object) = self.objects.get(&ptr) {
            return object.clone();
        }
        let object = Arc::new(self.store.read_object(ptr).unwrap());
        if self.objects.len() > 1 << 16 {
            self.objects.clear();
        }
        self.objects.insert(ptr, object.clone());
        object
    }

    fn delta(&mut self, new: Ptr, base: Option<Ptr>, fresh: (u64, u64)) -> usize {
        let object = self.get(new);
        let old = base.map(|ptr| self.get(ptr));
        let mut bytes = match &old {
            Some(old) => body_diff(&old.body, &object.body),
            None => varint_len(object.body.len() as u64) + object.body.len(),
        };
        bytes += varint_len(object.links.len() as u64) + object.links.len().div_ceil(8);
        let old_links: Vec<Ptr> = old.as_ref().map(|o| o.links.clone()).unwrap_or_default();
        for (index, link) in object.links.iter().enumerate() {
            if old_links.get(index).is_some_and(|l| l.hash == link.hash) {
                continue;
            }
            if (fresh.0..fresh.1).contains(&link.offset) {
                let child = self.get(*link);
                let mut best: Option<(usize, Ptr)> = None;
                for candidate in [index.checked_sub(1), Some(index), Some(index + 1)]
                    .into_iter()
                    .flatten()
                {
                    let Some(&candidate) = old_links.get(candidate) else {
                        continue;
                    };
                    let cost = body_diff(&self.get(candidate).body, &child.body);
                    if best.is_none_or(|(b, _)| cost < b) {
                        best = Some((cost, candidate));
                    }
                }
                bytes += self.delta(*link, best.map(|(_, ptr)| ptr), fresh);
            } else if old_links.iter().any(|l| l.hash == link.hash) {
                bytes += 1;
            } else {
                bytes += HASH_BYTES + 1;
            }
        }
        bytes
    }
}

fn deltas(path: &Path, framed: &mut [(usize, Framed)], before: Ptr) {
    let (store, _) = Store::open(
        path,
        StoreOptions {
            read_only: true,
            ..StoreOptions::default()
        },
    )
    .unwrap();
    let mut walker = Walker {
        store,
        objects: HashMap::new(),
    };
    let mut previous = before;
    for (_, frame) in framed.iter_mut() {
        frame.parts.delta =
            walker.delta(frame.root, Some(previous), (frame.start, frame.end)) as u64;
        previous = frame.root;
    }
}

fn open_map(path: &Path, durability: Durability) -> Db<Map<u64, String>> {
    Options::new().durability(durability).open(path).unwrap()
}

fn root_ptr(path: &Path, start: u64, end: u64) -> Ptr {
    let file = File::open(path).unwrap();
    frame_of(&file, start, end).root
}

fn map_workload(dir: &Path, n: u64, commits: u64, counter: &Counter) {
    let path = dir.join("map.ferro");
    let started = Instant::now();
    {
        let db = open_map(&path, Durability::Buffered);
        db.update(|root| {
            for i in 0..n {
                root.insert(mix(i), format!("value-{i}"));
            }
        })
        .unwrap();
    }
    println!(
        "map, {n} keys, loaded in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    let mut next = n;
    let mut framed = Vec::new();
    let before_root;
    {
        let db = open_map(&path, Durability::Buffered);
        let file = File::open(&path).unwrap();
        let start = db.log_bytes();
        db.update(|root| {
            root.insert(mix(next), "x".to_string());
        })
        .unwrap();
        next += 1;
        before_root = root_ptr(&path, start, db.log_bytes());
        for _ in 0..commits {
            let start = db.log_bytes();
            db.update(|root| {
                root.insert(mix(next), "x".to_string());
            })
            .unwrap();
            next += 1;
            framed.push((0, frame_of(&file, start, db.log_bytes())));
        }
    }
    deltas(&path, &mut framed, before_root);
    let mut parts = Parts::default();
    for (_, frame) in &framed {
        parts.add(frame.parts);
    }
    parts.report("insert");
    {
        let db = open_map(&path, Durability::Buffered);
        let file = File::open(&path).unwrap();
        let mut rest = Vec::new();
        let mut hashed = Vec::new();
        for i in 0..2 * commits + 2_000 {
            let measured = i >= 2_000;
            let hash = i % 2 == 1;
            let key = mix(next);
            next += 1;
            let mut inside = Sample::default();
            let mut hashing = Sample::default();
            let began = Instant::now();
            let start = Sample::now(counter, began);
            db.update(|root| {
                let began = Instant::now();
                let start = Sample::now(counter, began);
                root.insert(key, "x".to_string());
                if hash {
                    let began = Instant::now();
                    let start = Sample::now(counter, began);
                    std::hint::black_box(hash_of(&*root));
                    hashing = Sample::now(counter, began).since(start);
                }
                inside = Sample::now(counter, began).since(start);
            })
            .unwrap();
            let whole = Sample::now(counter, began).since(start);
            if measured && hash {
                hashed.push(hashing);
            } else if measured {
                rest.push(whole.minus(inside));
            }
        }
        println!("  outside the closure, commit        {}", summary(&rest));
        println!("  hash_of the dirty root, no write   {}", summary(&hashed));
        for k in [1u64, 16, 256, 4096, 65536] {
            let start = db.log_bytes();
            db.update(|root| {
                for _ in 0..k {
                    root.insert(mix(next), "x".to_string());
                    next += 1;
                }
            })
            .unwrap();
            let frame = frame_of(&file, start, db.log_bytes());
            println!(
                "  checkpoint of {k:>5} commits: tree {:>9} bytes, {:>6.0} per commit",
                frame.parts.tree + frame.parts.root,
                (frame.parts.tree + frame.parts.root) as f64 / k as f64
            );
        }
    }
    let tiny = dir.join("tiny.ferro");
    let small = Options::new()
        .durability(Durability::Buffered)
        .open::<Map<u64, u64>>(&tiny)
        .unwrap();
    small
        .update(|root| {
            for i in 0..16 {
                root.insert(i, i);
            }
        })
        .unwrap();
    let mut fixed = Vec::new();
    for i in 0..commits + 1_000 {
        let mut inside = Sample::default();
        let began = Instant::now();
        let start = Sample::now(counter, began);
        small
            .update(|root| {
                let began = Instant::now();
                let start = Sample::now(counter, began);
                root.insert(i % 16, i);
                inside = Sample::now(counter, began).since(start);
            })
            .unwrap();
        if i >= 1_000 {
            fixed.push(Sample::now(counter, began).since(start).minus(inside));
        }
    }
    let start = small.log_bytes();
    small.update(|root| root.insert(3, 3)).unwrap();
    println!(
        "  a 16-key map, outside the closure  {}  bytes {}",
        summary(&fixed),
        small.log_bytes() - start
    );
    drop(small);
    let fsync_rounds = number("--fsync", 300);
    if fsync_rounds > 0 {
        let large = open_map(&path, Durability::Fsync);
        let small = Options::new().open::<Map<u64, u64>>(&tiny).unwrap();
        let (mut big, mut little) = (Vec::new(), Vec::new());
        let (big_start, little_start) = (large.log_bytes(), small.log_bytes());
        for i in 0..fsync_rounds {
            let began = Instant::now();
            large
                .update(|root| root.insert(mix(next), "x".to_string()))
                .unwrap();
            next += 1;
            big.push(began.elapsed().as_nanos() as u64);
            let began = Instant::now();
            small.update(|root| root.insert(i % 16, i + 1)).unwrap();
            little.push(began.elapsed().as_nanos() as u64);
        }
        println!(
            "  fsync commit p50: 1M-key map {:.1} us at {} bytes, 16-key map {:.1} us at {} bytes",
            median(big),
            (large.log_bytes() - big_start) / fsync_rounds,
            median(little),
            (small.log_bytes() - little_start) / fsync_rounds
        );
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    Hold,
    Confirm,
    Cancel,
}

const KINDS: [(Kind, &str); 3] = [
    (Kind::Hold, "hold"),
    (Kind::Confirm, "confirm"),
    (Kind::Cancel, "cancel"),
];

enum Step {
    Hold(HoldRequest, u64),
    Confirm(u64, Cents, u64),
    Cancel(u64, u64),
}

impl Step {
    fn kind(&self) -> Kind {
        match self {
            Step::Hold(..) => Kind::Hold,
            Step::Confirm(..) => Kind::Confirm,
            Step::Cancel(..) => Kind::Cancel,
        }
    }
}

#[derive(Clone)]
struct Script {
    rng: fastrand::Rng,
    seeded: Seeded,
    confirmed: Vec<u64>,
    start_ms: u64,
    round: u64,
}

impl Script {
    fn drive(&mut self, pairs: u64, mut run: impl FnMut(&Step) -> Option<(u64, Cents)>) {
        for _ in 0..pairs {
            let i = self.round;
            self.round += 1;
            let now = self.start_ms + i;
            let request = HoldRequest {
                customer: self.seeded.customers[self.rng.usize(0..self.seeded.customers.len())],
                room: self.seeded.rooms[self.rng.usize(0..self.seeded.rooms.len())],
                first: (self.start_ms / DAY_MS) as u32 + 1 + self.rng.u32(0..3_000),
                nights: self.rng.u32(1..=3),
                ttl_ms: 600_000,
            };
            let Some((id, total)) = run(&Step::Hold(request, now)) else {
                continue;
            };
            if run(&Step::Confirm(id, total, now)).is_some() {
                self.confirmed.push(id);
            }
            if i % 4 == 3 && !self.confirmed.is_empty() {
                let id = self
                    .confirmed
                    .swap_remove(self.rng.usize(0..self.confirmed.len()));
                run(&Step::Cancel(id, now));
            }
        }
    }
}

fn apply(hotels: &mut Hotels, step: &Step, version: u64) -> Option<(u64, Cents)> {
    let kept = hotels.clone();
    let done = match step {
        Step::Hold(request, now) => hotels.hold(*now, request).map(|held| (held.id, held.total)),
        Step::Confirm(id, total, now) => hotels
            .confirm(*now, version, *id, Some(*total))
            .map(|_| (*id, 0)),
        Step::Cancel(id, now) => hotels.cancel(*now, *id, Why::Customer).map(|_| (*id, 0)),
    };
    if done.is_err() {
        *hotels = kept;
    }
    done.ok()
}

fn commit(db: &Db<Hotels>, step: &Step, inside: impl FnOnce(&mut Hotels)) -> Option<(u64, Cents)> {
    db.try_commit_stamped(|hotels, stamp| {
        let done = apply(hotels, step, stamp.version());
        inside(hotels);
        done.map(|done| (format!("{:?}", step.kind()), done))
            .ok_or(ferro::Error::Detached)
    })
    .ok()
    .map(|done| done.value)
}

fn booking_workload(dir: &Path, pairs: u64, warm: u64, counter: &Counter) {
    let path = dir.join("booking.ferro");
    let db = Options::new()
        .durability(Durability::Buffered)
        .open::<Hotels>(&path)
        .unwrap();
    let start_ms = 20_000 * DAY_MS;
    let svc = Reservations::new(db.clone()).with_clock(Arc::new(move || start_ms));
    let plan = Plan {
        venues: 6,
        rooms: 20,
        customers: 400,
        credit: 1 << 40,
        rate: 9_000,
    };
    let mut script = Script {
        rng: fastrand::Rng::with_seed(7),
        seeded: seed(&svc, &plan).unwrap(),
        confirmed: Vec::new(),
        start_ms,
        round: 0,
    };
    script.drive(warm, |step| commit(&db, step, |_| {}));
    println!("booking, after {warm} warm-up pairs");
    let file = File::open(&path).unwrap();
    let before = db.read().commit().root.unwrap();
    let mut framed: Vec<(usize, Framed)> = Vec::new();
    script.drive(pairs, |step| {
        let start = db.log_bytes();
        let done = commit(&db, step, |_| {});
        if done.is_some() {
            framed.push((step.kind() as usize, frame_of(&file, start, db.log_bytes())));
        }
        done
    });
    deltas(&path, &mut framed, before);
    let mut by_kind: HashMap<usize, Parts> = HashMap::new();
    let mut all = Parts::default();
    for (kind, frame) in &framed {
        by_kind.entry(*kind).or_default().add(frame.parts);
        all.add(frame.parts);
    }
    for (kind, name) in KINDS {
        by_kind
            .get(&(kind as usize))
            .copied()
            .unwrap_or_default()
            .report(name);
    }
    all.report("all");
    let mut rest: HashMap<Kind, Vec<Sample>> = HashMap::new();
    let mut hashed: HashMap<Kind, Vec<Sample>> = HashMap::new();
    let mut flip: HashMap<Kind, bool> = HashMap::new();
    script.drive(pairs * 2, |step| {
        let kind = step.kind();
        let hash = *flip.entry(kind).and_modify(|f| *f = !*f).or_insert(false);
        let mut inside = Sample::default();
        let mut hashing = Sample::default();
        let began = Instant::now();
        let start = Sample::now(counter, began);
        let done = db
            .try_commit_stamped(|hotels, stamp| {
                let began = Instant::now();
                let start = Sample::now(counter, began);
                let done = apply(hotels, step, stamp.version());
                if hash {
                    let began = Instant::now();
                    let start = Sample::now(counter, began);
                    std::hint::black_box(hash_of(&*hotels));
                    hashing = Sample::now(counter, began).since(start);
                }
                inside = Sample::now(counter, began).since(start);
                done.map(|done| (format!("{kind:?}"), done))
                    .ok_or(ferro::Error::Detached)
            })
            .ok()
            .map(|done| done.value);
        let whole = Sample::now(counter, began).since(start);
        if done.is_some() && hash {
            hashed.entry(kind).or_default().push(hashing);
        } else if done.is_some() {
            rest.entry(kind).or_default().push(whole.minus(inside));
        }
        done
    });
    for (kind, name) in KINDS {
        println!("  {name:<8} outside the closure  {}", summary(&rest[&kind]));
        println!(
            "  {name:<8} hash_of, no write    {}",
            summary(&hashed[&kind])
        );
    }
    let mut bumps = Vec::new();
    for i in 0..pairs + 500 {
        let mut inside = Sample::default();
        let began = Instant::now();
        let start = Sample::now(counter, began);
        db.update(|hotels| {
            let began = Instant::now();
            let start = Sample::now(counter, began);
            hotels.next_payment += 1;
            inside = Sample::now(counter, began).since(start);
        })
        .unwrap();
        if i >= 500 {
            bumps.push(Sample::now(counter, began).since(start).minus(inside));
        }
    }
    let start = db.log_bytes();
    db.update(|hotels| hotels.next_payment += 1).unwrap();
    println!(
        "  a root field alone, outside the closure  {}  bytes {}",
        summary(&bumps),
        db.log_bytes() - start
    );
    for k in [1u64, 4, 16, 64, 256, 1024, 4096] {
        let mut ops = 0;
        let start = db.log_bytes();
        let mut group = script.clone();
        group.rng = fastrand::Rng::with_seed(k);
        db.update(|hotels| {
            while ops < k {
                group.drive(1, |step| {
                    if ops >= k {
                        return None;
                    }
                    let done = apply(hotels, step, 0);
                    ops += u64::from(done.is_some());
                    done
                });
            }
        })
        .unwrap();
        let frame = frame_of(&file, start, db.log_bytes());
        println!(
            "  checkpoint of {ops:>5} writes: tree {:>9} bytes, {:>6.0} per write",
            frame.parts.tree + frame.parts.root,
            (frame.parts.tree + frame.parts.root) as f64 / ops as f64
        );
    }
}

fn main() {
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let counter = Counter::instructions();
    let workload = argument("--workload").unwrap_or_else(|| "all".to_string());
    if workload == "all" || workload == "map" {
        map_workload(
            dir.path(),
            number("--n", 1_000_000),
            number("--commits", 1_000),
            &counter,
        );
    }
    if workload == "all" || workload == "booking" {
        booking_workload(
            dir.path(),
            number("--pairs", 2_000),
            number("--warm", 4_000),
            &counter,
        );
    }
}
