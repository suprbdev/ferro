use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use ferro::{Db, Durability, Have, Map, Options, Pulled, Snapshot};

type Root = Map<u64, String>;

const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ";

struct Counted;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static HIGH: AtomicUsize = AtomicUsize::new(0);

fn grew(size: usize) {
    let now = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    HIGH.fetch_max(now, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        grew(size);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counted = Counted;

fn heap_mib() -> f64 {
    HIGH.load(Ordering::Relaxed) as f64 / (1 << 20) as f64
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn text(&mut self, len: usize) -> String {
        (0..len)
            .map(|_| ALPHABET[(self.next() % ALPHABET.len() as u64) as usize] as char)
            .collect()
    }
}

fn peak_rss_mib() -> f64 {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| {
            rest.trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<f64>()
                .ok()
        })
        .map_or(0.0, |kib| kib / 1024.0)
}

fn options() -> Options {
    let buffered = std::env::var_os("CLONE_BUFFERED").is_some();
    let durability = if buffered {
        Durability::Buffered
    } else {
        Durability::Fsync
    };
    Options::new().durability(durability)
}

fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    panic!("a clone does not merge")
}

fn arg(args: &[String], name: &str, default: u64) -> u64 {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map_or(default, |pair| pair[1].parse().expect("a number"))
}

fn build(dir: &Path, args: &[String]) {
    let keys = arg(args, "--keys", 1_000_000);
    let value = arg(args, "--value", 320) as usize;
    let commits = arg(args, "--commits", 10).max(1);
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("source.ferro");
    let _ = fs::remove_file(&path);
    let db: Db<Root> = Options::new()
        .durability(Durability::Buffered)
        .open(&path)
        .unwrap();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let started = Instant::now();
    let per = keys.div_ceil(commits);
    for commit in 0..commits {
        let range = commit * per..((commit + 1) * per).min(keys);
        let values: Vec<(u64, String)> = range.map(|key| (key, rng.text(value))).collect();
        db.commit(format!("load {commit}"), |root| {
            for (key, text) in values {
                root.insert(key, text);
            }
        })
        .unwrap();
    }
    drop(db);
    println!(
        "built {keys} keys of {value} bytes in {commits} commits, {} bytes, {:.1} s",
        fs::metadata(&path).unwrap().len(),
        started.elapsed().as_secs_f64()
    );
}

fn report(what: &str, pulled: &Pulled<()>, seconds: f64) {
    println!(
        "{what}: {:?}, {} bytes in {} objects, {:.3} s, peak RSS {:.1} MiB, peak heap {:.1} MiB",
        pulled.outcome,
        pulled.received_bytes,
        pulled.objects,
        seconds,
        peak_rss_mib(),
        heap_mib()
    );
}

fn fresh(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let _ = fs::remove_file(&path);
    path
}

fn pull(dir: &Path) {
    let source: Db<Root> = options().open(dir.join("source.ferro")).unwrap();
    let clone: Db<Root> = options().open(fresh(dir, "clone.ferro")).unwrap();
    let started = Instant::now();
    let pulled = clone.pull(&source, "source", refuse).unwrap();
    report(
        "pull into an empty file",
        &pulled,
        started.elapsed().as_secs_f64(),
    );
    assert_eq!(clone.head_hash(), source.head_hash());
}

fn send(dir: &Path) {
    let source: Db<Root> = options().open(dir.join("source.ferro")).unwrap();
    let empty: Db<Root> = options().open(fresh(dir, "empty.ferro")).unwrap();
    let have: Have = empty.have().unwrap();
    let started = Instant::now();
    let bytes = write_bundle(&source, &have, &dir.join("bundle.bin"));
    println!(
        "bundle: {bytes} bytes, {:.3} s, peak RSS {:.1} MiB, peak heap {:.1} MiB",
        started.elapsed().as_secs_f64(),
        peak_rss_mib(),
        heap_mib()
    );
}

fn receive(dir: &Path) {
    let clone: Db<Root> = options().open(fresh(dir, "received.ferro")).unwrap();
    let started = Instant::now();
    let pulled = read_bundle(&clone, &dir.join("bundle.bin"));
    report(
        "receive into an empty file",
        &pulled,
        started.elapsed().as_secs_f64(),
    );
}

fn write_bundle(source: &Db<Root>, have: &Have, path: &Path) -> u64 {
    let out = BufWriter::new(File::create(path).unwrap());
    source.bundle_to(have, out).unwrap()
}

fn read_bundle(clone: &Db<Root>, path: &Path) -> Pulled<()> {
    let input = BufReader::new(File::open(path).unwrap());
    clone.receive_from("source", input, refuse).unwrap()
}

fn incremental(dir: &Path, args: &[String]) {
    let rounds = arg(args, "--rounds", 20);
    let source_path = fresh(dir, "inc-source.ferro");
    let clone_path = fresh(dir, "inc-clone.ferro");
    fs::copy(dir.join("source.ferro"), &source_path).unwrap();
    fs::copy(dir.join("clone.ferro"), &clone_path).unwrap();
    let source: Db<Root> = options().open(&source_path).unwrap();
    let clone: Db<Root> = options().open(&clone_path).unwrap();
    let mut rng = Rng(7);
    let keys = source.read().len() as u64;
    let mut times = Vec::new();
    let mut last = None;
    for _ in 0..rounds {
        for _ in 0..3 {
            let key = rng.next() % keys;
            let text = rng.text(320);
            source
                .commit("edit", |root| {
                    root.insert(key, text);
                })
                .unwrap();
        }
        let started = Instant::now();
        let pulled = clone.pull(&source, "source", refuse).unwrap();
        times.push(started.elapsed().as_secs_f64() * 1000.0);
        last = Some(pulled);
    }
    times.sort_by(f64::total_cmp);
    let pulled = last.unwrap();
    println!(
        "incremental pull of 3 commits: {:?}, {} bytes in {} objects, median {:.2} ms, min {:.2} ms, max {:.2} ms, peak RSS {:.1} MiB",
        pulled.outcome,
        pulled.received_bytes,
        pulled.objects,
        times[times.len() / 2],
        times[0],
        times[times.len() - 1],
        peak_rss_mib()
    );
    assert_eq!(clone.head_hash(), source.head_hash());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(command), Some(dir)) = (args.get(1), args.get(2)) else {
        eprintln!(
            "usage: clone build|pull|send|receive|incremental <dir> [--keys N] [--value BYTES] [--commits N] [--rounds N]"
        );
        std::process::exit(2);
    };
    let dir = PathBuf::from(dir);
    match command.as_str() {
        "build" => build(&dir, &args[3..]),
        "pull" => pull(&dir),
        "send" => send(&dir),
        "receive" => receive(&dir),
        "incremental" => incremental(&dir, &args[3..]),
        other => panic!("unknown command {other}"),
    }
}
