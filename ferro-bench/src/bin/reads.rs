//! Measures reads on one thread: gets, scans, batches and walks over maps and a table, and the heap
//! a fresh read-only handle holds once it has read a map through.
//!
//! In a temporary directory under `--dir` (default `target/bench`) it loads a `Map<u64, V>` of
//! `--n` scrambled keys (default 1,000,000) with one `extend` under `Durability::Buffered`, for `V`
//! a short `String`, a `u64`, a `u32` and `()`, timing the load and 2,000 single-key commits after
//! it. A counting global allocator, which counts live bytes as `malloc` chunks, gives the heap per
//! key that a new read-only handle holds after two passes over the keys and entries, and a handle
//! with a 1 MiB cache times a cold scan. On a warm snapshot it then times scans through `for`
//! loops, folds, `count`, `filter`, `keys`, `values` and `for_each`; 500,000 random and sorted
//! `get`s, and random `contains_key`s, `floor`s, seeks and a `get_many`; and gets, `get_many` and
//! `contains_many` over every 2nd, 16th, 256th and 4096th key in key order. It also times a table
//! of `n / 5` accounts indexed on `tier` and `balance`, scanned whole, through an index in key
//! order, through scattered index rows, with `for_each` and over index keys; a `Map<String, u64>`
//! scanned three ways; decoding vectors of a million small, medium and large `u64`s, `f64`s and
//! pairs of `u32`s; and decoding and visiting a `BTreeMap<u64, u64>` and a `BTreeSet<String>` of a
//! million keys, stored in order and shuffled, and a million keys in `BTreeMap`s of four. `--only`
//! takes a comma-separated list of `string`, `u64`, `u32`, `unit`, `rows`, `text`, `decode` and
//! `btree` to run some of them.
//!
//! Each timed read runs three times and reports its fastest nanoseconds per item, and its fewest
//! user-space instructions per item as `_ins`, counted through `perf_event_open` on Linux and 0
//! elsewhere. The results are appended under `--label` (default `run`) as a JSON line to
//! `reads.jsonl` in `--dir`, and printed one to a line.
//!
//! ```sh
//! make bench-reads ARGS='--only u64,rows --label base'
//! cargo run --release -p ferro-bench --bin reads -- --n 200000 --only string,decode
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::hint::black_box;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use ferro::codec::{Decoder, Encoder, HashSink};
use ferro::{Data, Db, Durability, Map, Options, Record, Table};

struct Counting;

static COUNTING: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicUsize = AtomicUsize::new(0);

fn chunk(size: usize) -> usize {
    (size + 8).next_multiple_of(16).max(32)
}

// SAFETY: every method forwards to `System` with the caller's layout, pointer and size unchanged
// and returns what `System` returns, so `System`'s guarantees carry over. Besides that it only
// reads the atomic flag `COUNTING` and adjusts the atomic counter `LIVE`, neither of which
// allocates or panics: a layout's size is at most `isize::MAX`, so `chunk` cannot overflow, and
// the counter wraps rather than panic when a block allocated before counting began is freed.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            LIVE.fetch_add(chunk(layout.size()), Ordering::Relaxed);
        }
        // SAFETY: the caller meets `GlobalAlloc::alloc`'s contract for `layout`, which is passed
        // on to `System` unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if COUNTING.load(Ordering::Relaxed) {
            LIVE.fetch_sub(chunk(layout.size()), Ordering::Relaxed);
        }
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout`, and every block this allocator hands out came from `System` with that layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            LIVE.fetch_add(chunk(size), Ordering::Relaxed);
            LIVE.fetch_sub(chunk(layout.size()), Ordering::Relaxed);
        }
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout` and that `size` is a valid new size for it. The block came from `System` with
        // that layout, and all three are passed on unchanged.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const GETS: u64 = 500_000;
const COLD_CACHE: u64 = 1 << 20;
const STRIDES: [(&str, usize); 4] = [("half", 2), ("16th", 16), ("256th", 256), ("4096th", 4096)];

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
    email: String,
    name: String,
    #[index]
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
        email: format!("user{i}@example.com"),
        name: format!("Account Holder {}", seed % 100_000),
        balance: (seed % 1_000_000) as i64,
    }
}

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
        // write land in it, and `fd` is a descriptor `perf_event_open` returned, which this
        // `Counter` keeps open until it drops.
        unsafe { libc::read(fd, (&mut value as *mut u64).cast(), 8) };
        value
    }
}

#[cfg(target_os = "linux")]
impl Drop for Counter {
    fn drop(&mut self) {
        if let Some(fd) = self.0 {
            // SAFETY: `fd` is a descriptor `perf_event_open` returned for this `Counter`, which
            // owns it, and nothing uses it after this one close.
            unsafe { libc::close(fd) };
        }
    }
}

struct Report(BTreeMap<String, f64>);

impl Report {
    fn set(&mut self, name: &str, value: f64) {
        self.0.insert(name.to_string(), value);
    }

    fn timed<R>(&mut self, name: &str, count: u64, mut f: impl FnMut() -> R) {
        let counter = Counter::instructions();
        let mut best = f64::MAX;
        let mut fewest = u64::MAX;
        for _ in 0..3 {
            let before = counter.read();
            let start = Instant::now();
            black_box(f());
            best = best.min(start.elapsed().as_nanos() as f64 / count as f64);
            fewest = fewest.min(counter.read() - before);
        }
        self.set(name, best);
        self.set(&format!("{name}_ins"), fewest as f64 / count as f64);
    }
}

trait Weigh {
    fn weigh(&self) -> u64;
}

impl Weigh for String {
    #[inline]
    fn weigh(&self) -> u64 {
        self.len() as u64
    }
}

impl Weigh for u64 {
    #[inline]
    fn weigh(&self) -> u64 {
        *self
    }
}

impl Weigh for u32 {
    #[inline]
    fn weigh(&self) -> u64 {
        u64::from(*self)
    }
}

impl Weigh for () {
    #[inline]
    fn weigh(&self) -> u64 {
        1
    }
}

fn probe_map<V: Data + Weigh>(report: &mut Report, tag: &str, map: &Map<u64, V>, keys: &[u64]) {
    let n = map.len() as u64;
    report.timed(&format!("{tag}_for"), n, || {
        let mut total = 0u64;
        for (key, value) in map.iter() {
            total = total.wrapping_add(key ^ value.weigh());
        }
        total
    });
    report.timed(&format!("{tag}_sum"), n, || {
        map.iter()
            .map(|(key, value)| key ^ value.weigh())
            .fold(0u64, u64::wrapping_add)
    });
    report.timed(&format!("{tag}_count"), n, || map.iter().count());
    report.timed(&format!("{tag}_keys_for"), n, || {
        let mut total = 0u64;
        for key in map.keys() {
            total = total.wrapping_add(key);
        }
        total
    });
    report.timed(&format!("{tag}_keys_sum"), n, || {
        map.keys().fold(0u64, u64::wrapping_add)
    });
    report.timed(&format!("{tag}_values_sum"), n, || {
        map.values()
            .map(|value| value.weigh())
            .fold(0u64, u64::wrapping_add)
    });
    report.timed(&format!("{tag}_filter_count"), n, || {
        map.iter().filter(|(key, _)| key % 3 == 0).count()
    });
    let sample = &keys[..keys.len().min(GETS as usize)];
    let mut sorted_all = keys.to_vec();
    sorted_all.sort_unstable();
    let mut sorted = sample.to_vec();
    sorted.sort_unstable();
    report.timed(&format!("{tag}_get_random"), sample.len() as u64, || {
        let mut total = 0u64;
        for key in sample {
            total = total.wrapping_add(map.get(key).map_or(0, |value| value.weigh()));
        }
        total
    });
    report.timed(&format!("{tag}_get_sorted"), sorted.len() as u64, || {
        let mut total = 0u64;
        for key in &sorted {
            total = total.wrapping_add(map.get(key).map_or(0, |value| value.weigh()));
        }
        total
    });
    report.timed(
        &format!("{tag}_contains_random"),
        sample.len() as u64,
        || sample.iter().filter(|key| map.contains_key(*key)).count(),
    );
    for (name, step) in STRIDES {
        let picked: Vec<u64> = sorted_all.iter().step_by(step).copied().collect();
        report.timed(&format!("{tag}_{name}_get"), picked.len() as u64, || {
            let mut total = 0u64;
            for key in &picked {
                total = total.wrapping_add(map.get(key).map_or(0, |value| value.weigh()));
            }
            total
        });
    }
    report.timed(&format!("{tag}_seek_next"), sample.len() as u64, || {
        let mut total = 0u64;
        for key in sample {
            total = total.wrapping_add(map.range(*key..).next().map_or(0, |(key, _)| key));
        }
        total
    });
    map_extras(report, tag, map, sample, &sorted_all);
}

fn map_extras<V: Data + Weigh>(
    report: &mut Report,
    tag: &str,
    map: &Map<u64, V>,
    sample: &[u64],
    sorted_all: &[u64],
) {
    let n = map.len() as u64;
    report.timed(&format!("{tag}_each"), n, || {
        let mut total = 0u64;
        map.for_each(|key, value| total = total.wrapping_add(key ^ value.weigh()));
        total
    });
    for (name, step) in STRIDES {
        let picked: Vec<u64> = sorted_all.iter().step_by(step).copied().collect();
        report.timed(&format!("{tag}_{name}_many"), picked.len() as u64, || {
            let mut total = 0u64;
            for (_, value) in map.get_many(&picked) {
                total = total.wrapping_add(value.map_or(0, |value| value.weigh()));
            }
            total
        });
        report.timed(
            &format!("{tag}_{name}_contains"),
            picked.len() as u64,
            || {
                map.contains_many(&picked)
                    .filter(|(_, found)| *found)
                    .count()
            },
        );
    }
    report.timed(&format!("{tag}_floor_random"), sample.len() as u64, || {
        let mut total = 0u64;
        for key in sample {
            total = total.wrapping_add(map.floor(key).map_or(0, |(_, value)| value.weigh()));
        }
        total
    });
    report.timed(&format!("{tag}_many_random"), sample.len() as u64, || {
        let mut total = 0u64;
        for (_, value) in map.get_many(sample) {
            total = total.wrapping_add(value.map_or(0, |value| value.weigh()));
        }
        total
    });
}

fn heap_per_key<T: Data>(db: &Db<T>, n: u64, read: impl Fn(&T)) -> f64 {
    let before = LIVE.load(Ordering::Relaxed);
    let snapshot = db.read();
    read(&snapshot);
    read(&snapshot);
    LIVE.load(Ordering::Relaxed).saturating_sub(before) as f64 / n as f64
}

fn map_case<V: Data + Default + Weigh>(
    report: &mut Report,
    dir: &Path,
    tag: &str,
    n: u64,
    value: impl Fn(u64) -> V,
) {
    let path = dir.join(format!("{tag}.ferro"));
    let keys: Vec<u64> = (0..n).map(mix).collect();
    let db = Options::new()
        .durability(Durability::Buffered)
        .open::<Map<u64, V>>(&path)
        .unwrap();
    let start = Instant::now();
    db.update(|root| {
        root.extend((0..n).map(|i| (mix(i), value(i))));
    })
    .unwrap();
    report.set(
        &format!("{tag}_bulk"),
        start.elapsed().as_nanos() as f64 / n as f64,
    );
    let start = Instant::now();
    for i in 0..2_000 {
        db.update(|root| {
            root.insert(mix(i * 7), value(i + 1));
        })
        .unwrap();
    }
    report.set(
        &format!("{tag}_commit"),
        start.elapsed().as_nanos() as f64 / 2_000.0,
    );
    drop(db);

    COUNTING.store(true, Ordering::Relaxed);
    let reader = Options::new()
        .read_only(true)
        .open::<Map<u64, V>>(&path)
        .unwrap();
    let heap = heap_per_key(&reader, n, |root| {
        black_box(root.keys().count());
        for (key, value) in root.iter() {
            black_box((key, value));
        }
    });
    COUNTING.store(false, Ordering::Relaxed);
    report.set(&format!("{tag}_heap_per_key"), heap);
    let snapshot = reader.read();
    let cold = Options::new().read_only(true).cache_bytes(COLD_CACHE);
    let cold = cold.open::<Map<u64, V>>(&path).unwrap();
    let cold_view = cold.read();
    report.timed(&format!("{tag}_cold_scan"), n, || {
        cold_view
            .iter()
            .map(|(key, value)| key ^ value.weigh())
            .fold(0u64, u64::wrapping_add)
    });
    probe_map(report, tag, &snapshot, &keys);
}

fn table_case(report: &mut Report, dir: &Path, n: u64) {
    let path = dir.join("rows.ferro");
    let db = Options::new()
        .durability(Durability::Buffered)
        .open::<Bank>(&path)
        .unwrap();
    for chunk in 0..n.div_ceil(100_000) {
        db.update(|bank| {
            for i in chunk * 100_000..((chunk + 1) * 100_000).min(n) {
                bank.accounts.insert(account(i));
            }
        })
        .unwrap();
    }
    drop(db);
    let reader = Options::new().read_only(true).open::<Bank>(&path).unwrap();
    let snapshot = reader.read();
    let bank: &Bank = &snapshot;
    black_box(bank.accounts.iter().count());
    report.timed("rows_iter_for", n, || {
        let mut total = 0i64;
        for row in bank.accounts.iter() {
            total = total.wrapping_add(row.balance);
        }
        total
    });
    report.timed("rows_iter_sum", n, || {
        bank.accounts
            .iter()
            .map(|row| row.balance)
            .fold(0i64, i64::wrapping_add)
    });
    let pro = bank.accounts.by_tier(&Tier::Pro).len() as u64;
    report.timed("rows_index_for", pro, || {
        let mut total = 0i64;
        for row in bank.accounts.by_tier(&Tier::Pro) {
            total = total.wrapping_add(row.balance);
        }
        total
    });
    let band = bank.accounts.by_balance_range(0..200_000).len() as u64;
    report.timed("rows_scattered_for", band, || {
        let mut total = 0i64;
        for row in bank.accounts.by_balance_range(0..200_000) {
            total = total.wrapping_add(row.balance);
        }
        total
    });
    rows_extras(report, bank, n, pro);
}

fn rows_extras(report: &mut Report, bank: &Bank, n: u64, pro: u64) {
    report.timed("rows_each", n, || {
        let mut total = 0i64;
        bank.accounts
            .for_each(|row| total = total.wrapping_add(row.balance));
        total
    });
    report.timed("rows_index_keys", pro, || {
        bank.accounts
            .by_tier(&Tier::Pro)
            .keys()
            .fold(0u64, u64::wrapping_add)
    });
}

fn text_keys_case(report: &mut Report, dir: &Path, n: u64) {
    let path = dir.join("text.ferro");
    let db = Options::new()
        .durability(Durability::Buffered)
        .open::<Map<String, u64>>(&path)
        .unwrap();
    db.update(|root| {
        root.extend((0..n).map(|i| (format!("user-{:016x}", mix(i)), i)));
    })
    .unwrap();
    drop(db);
    let reader = Options::new()
        .read_only(true)
        .open::<Map<String, u64>>(&path)
        .unwrap();
    let snapshot = reader.read();
    let map: &Map<String, u64> = &snapshot;
    black_box(map.keys().count());
    report.timed("text_for", n, || {
        let mut total = 0u64;
        for (key, value) in map.iter() {
            total = total.wrapping_add(key.len() as u64 ^ *value);
        }
        total
    });
    report.timed("text_sum", n, || {
        map.iter()
            .map(|(key, value)| key.len() as u64 ^ *value)
            .fold(0u64, u64::wrapping_add)
    });
    text_extras(report, map, n);
}

fn text_extras(report: &mut Report, map: &Map<String, u64>, n: u64) {
    report.timed("text_each", n, || {
        let mut total = 0u64;
        map.for_each(|key, value| total = total.wrapping_add(key.len() as u64 ^ *value));
        total
    });
}

fn decoded<T: Data>(report: &mut Report, name: &str, items: Vec<T>) {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    items.encode(&mut encoder).unwrap();
    let body = encoder.into_object().body;
    report.timed(name, items.len() as u64, || {
        let mut decoder = Decoder::new(&body, &[]);
        Vec::<T>::decode(&mut decoder).unwrap()
    });
}

fn decode_case(report: &mut Report) {
    decoded(
        report,
        "decode_u64_small",
        (0..1_000_000u64).map(|i| i % 100).collect(),
    );
    decoded(
        report,
        "decode_u64_medium",
        (0..1_000_000u64).map(|i| mix(i) >> 44).collect(),
    );
    decoded(
        report,
        "decode_u64_large",
        (0..1_000_000u64).map(mix).collect(),
    );
    decoded(
        report,
        "decode_f64",
        (0..1_000_000u64).map(|i| i as f64).collect(),
    );
    decoded(
        report,
        "decode_pair",
        (0..1_000_000u64)
            .map(|i| (i as u32, mix(i) as u32))
            .collect(),
    );
}

fn decoded_whole<T: Data>(report: &mut Report, name: &str, count: u64, value: &impl Data) {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder).unwrap();
    let body = encoder.into_object().body;
    if T::decode(&mut Decoder::new(&body, &[])).is_err() {
        return;
    }
    report.timed(&format!("decode_{name}"), count, || {
        T::decode(&mut Decoder::new(&body, &[])).unwrap()
    });
    report.timed(&format!("visit_{name}"), count, || {
        T::visit(&mut Decoder::new(&body, &[])).unwrap()
    });
}

fn btree_case(report: &mut Report) {
    let pairs: Vec<(u64, u64)> = (0..1_000_000u64).map(|i| (mix(i), i)).collect();
    let mut sorted = pairs.clone();
    sorted.sort_unstable();
    decoded_whole::<BTreeMap<u64, u64>>(report, "btreemap", 1_000_000, &sorted);
    decoded_whole::<BTreeMap<u64, u64>>(report, "btreemap_shuffled", 1_000_000, &pairs);
    let texts: Vec<String> = (0..1_000_000u64)
        .map(|i| format!("user-{:016x}", mix(i)))
        .collect();
    let mut sorted = texts.clone();
    sorted.sort_unstable();
    decoded_whole::<BTreeSet<String>>(report, "btreeset_text", 1_000_000, &sorted);
    decoded_whole::<BTreeSet<String>>(report, "btreeset_text_shuffled", 1_000_000, &texts);
    let small: Vec<BTreeMap<u16, u32>> = (0..250_000u64)
        .map(|i| {
            let base = (mix(i) as u16) & !3;
            (0..4).map(|j| (base + j as u16, j)).collect()
        })
        .collect();
    decoded_whole::<Vec<BTreeMap<u16, u32>>>(report, "btreemap_small", 1_000_000, &small);
}

fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn main() {
    let n: u64 = argument("--n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let label = argument("--label").unwrap_or_else(|| "run".to_string());
    let only = argument("--only");
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let wanted = |tag: &str| {
        only.as_ref()
            .is_none_or(|list| list.split(',').any(|item| item == tag))
    };
    let mut report = Report(BTreeMap::new());
    if wanted("string") {
        map_case(&mut report, dir.path(), "string", n, |i| {
            format!("value-{i}")
        });
    }
    if wanted("u64") {
        map_case(&mut report, dir.path(), "u64", n, mix);
    }
    if wanted("u32") {
        map_case(&mut report, dir.path(), "u32", n, |i| mix(i) as u32);
    }
    if wanted("unit") {
        map_case(&mut report, dir.path(), "unit", n, |_| ());
    }
    if wanted("rows") {
        table_case(&mut report, dir.path(), n / 5);
    }
    if wanted("text") {
        text_keys_case(&mut report, dir.path(), n);
    }
    if wanted("decode") {
        decode_case(&mut report);
    }
    if wanted("btree") {
        btree_case(&mut report);
    }
    let body: Vec<String> = report
        .0
        .iter()
        .map(|(k, v)| format!("\"{k}\":{v:.2}"))
        .collect();
    let mut out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(parent.join("reads.jsonl"))
        .unwrap();
    writeln!(out, "{{\"label\":\"{label}\",{}}}", body.join(",")).unwrap();
    for (name, value) in &report.0 {
        println!("{name:<28} {value:>12.1}");
    }
}
