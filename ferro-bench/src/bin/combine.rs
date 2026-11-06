//! Compares ways to combine two index lookups into the keys both return.
//!
//! Builds a `Table` of `--rows` rows (default 200,000) with two `#[index(each)]` fields, in a
//! `Durability::Buffered` file with a 1 GiB cache in a temporary directory under `--dir` (default
//! `target/bench`), and compacts it to its last version. Each of twelve equality cases gives its
//! value to one field of one set of rows and to the other field of another, with sets of different
//! sizes and overlaps. The strategies range from a merge of the two sorted key lists, leapfrog
//! searches through the `_after` lookups or through cursors, with or without a few steps before
//! each seek, and a hash set, to the `and` of the table's find view, read forwards, backwards, as
//! rows, or as keys followed by `get`s.
//!
//! The range cases combine `find().by_group(&g)`, whose groups hold 10, 100, 1,000 and 10,000 rows
//! picked at random and cover the other fields, with a range of the plain `score` index, a range
//! of the unique `rank` index, or the prefix of a band of the composite `(band, score)` index,
//! whose bands hold 1,000, 10,000 and 100,000 rows. Each field is a random permutation of the
//! rows, so a range's keys are scattered across the table. Their strategies are the `and` read as
//! keys from either end or as rows, collecting the range's keys and sorting them as `and` once
//! did, filtering the group's rows or covered fields, and checking the covered fields and then
//! seeking the range's index at each `(value, key)`.
//!
//! Each strategy named in `--strategies`, by default all of `merge`, `after`, `cursor`, `cursor2`,
//! `cursor4`, `cursor8`, `after8`, `after32`, `set`, `and`, `and-back`, `rows` and `gets` for the
//! equality cases and `and`, `and-back`, `rows`, `collect`, `filter`, `covered` and `seek` for the
//! range cases, is checked against the merge or the filter and then run over and over for `--ms`
//! milliseconds (default 40) in each of `--rounds` rounds (default 5). `--cases` picks `equal`,
//! `range` or both, the default, or `crossover`, which runs groups of 100, 1,000 and 10,000 rows
//! against score ranges from as many keys to 128 times as many, and `--only` runs just the cases
//! whose title holds the text it gives. For each case it prints the two sets' sizes and how many
//! keys they share, then each strategy's median microseconds and user-space instructions per run,
//! counted through `perf_event_open` on Linux.
//!
//! ```sh
//! make bench-combine ARGS='--strategies merge,and,set'
//! make bench-combine ARGS='--cases range --strategies and,collect,filter'
//! cargo run --release -p ferro-bench --bin combine -- --rows 200000 --rounds 5 --ms 40
//! ```

use std::collections::HashSet;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use ferro::table::IntoMatches;
use ferro::{Data, Durability, Keep, Options, Record, Table};

const CHUNK: u64 = 100_000;

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(band, score)]
struct Item {
    #[key]
    id: u64,
    #[index(each)]
    xs: Vec<u32>,
    #[index(each)]
    ys: Vec<u32>,
    #[index(cover(score, rank, band))]
    group: u8,
    #[index]
    score: u32,
    #[unique]
    rank: u32,
    band: u8,
    name: String,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    items: Table<Item>,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
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
        attr[5] = 1 << 5 | 1 << 6;
        // SAFETY: `attr` is an initialized 128-byte `perf_event_attr` whose `size` field says 128,
        // with every byte past the fields set here zero, and it outlives the call, which only
        // reads it. Any argument the kernel rejects makes the call return -1.
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0, -1, -1, 0) };
        if fd < 0 {
            return Counter(None);
        }
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

#[derive(Clone, Copy)]
enum Members {
    Random(u64),
    Span(u64, u64),
    Outside(usize, u64),
}

struct Case {
    name: &'static str,
    a: Members,
    b: Members,
}

const CASES: [Case; 12] = [
    Case {
        name: "50k and 50k",
        a: Members::Random(50_000),
        b: Members::Random(50_000),
    },
    Case {
        name: "50k and 50k, none shared",
        a: Members::Random(50_000),
        b: Members::Outside(0, 50_000),
    },
    Case {
        name: "50k and 50k, apart",
        a: Members::Span(0, 50_000),
        b: Members::Span(100_000, 150_000),
    },
    Case {
        name: "25k and 50k",
        a: Members::Random(25_000),
        b: Members::Random(50_000),
    },
    Case {
        name: "12k and 50k",
        a: Members::Random(12_500),
        b: Members::Random(50_000),
    },
    Case {
        name: "6k and 50k",
        a: Members::Random(6_250),
        b: Members::Random(50_000),
    },
    Case {
        name: "3k and 50k",
        a: Members::Random(3_125),
        b: Members::Random(50_000),
    },
    Case {
        name: "1k and 100k",
        a: Members::Random(1_000),
        b: Members::Random(100_000),
    },
    Case {
        name: "100 and 100k",
        a: Members::Random(100),
        b: Members::Random(100_000),
    },
    Case {
        name: "10 and 100k",
        a: Members::Random(10),
        b: Members::Random(100_000),
    },
    Case {
        name: "100k and 10",
        a: Members::Random(100_000),
        b: Members::Random(10),
    },
    Case {
        name: "20 and 20",
        a: Members::Random(20),
        b: Members::Random(20),
    },
];

fn members(rows: u64, case: usize, side: u64, members: Members, chosen: &[Vec<u64>]) -> Vec<u64> {
    match members {
        Members::Span(low, high) => (low..high.min(rows)).collect(),
        Members::Random(count) => {
            let mut picked: Vec<u64> = (0..rows)
                .filter(|id| mix(id ^ (case as u64) << 40 ^ side << 56) % rows < count)
                .collect();
            picked.sort_unstable();
            picked
        }
        Members::Outside(other, count) => {
            let taken: HashSet<u64> = chosen[other].iter().copied().collect();
            let free = rows - taken.len() as u64;
            (0..rows)
                .filter(|id| !taken.contains(id))
                .filter(|id| mix(id ^ (case as u64) << 40 ^ side << 56) % free < count)
                .collect()
        }
    }
}

struct Sets {
    xs: Vec<Vec<u32>>,
    ys: Vec<Vec<u32>>,
    sizes: Vec<(usize, usize)>,
}

fn sets(rows: u64) -> Sets {
    let mut xs = vec![Vec::new(); rows as usize];
    let mut ys = vec![Vec::new(); rows as usize];
    let mut sizes = Vec::new();
    for (index, case) in CASES.iter().enumerate() {
        let mut chosen = Vec::new();
        let a = members(rows, index, 0, case.a, &chosen);
        chosen.push(a);
        let b = members(rows, index, 1, case.b, &chosen);
        chosen.push(b);
        for &id in &chosen[0] {
            xs[id as usize].push(index as u32);
        }
        for &id in &chosen[1] {
            ys[id as usize].push(index as u32);
        }
        sizes.push((chosen[0].len(), chosen[1].len()));
    }
    Sets { xs, ys, sizes }
}

const GROUPS: [u64; 4] = [10, 100, 1_000, 10_000];
const BANDS: [u64; 3] = [1_000, 10_000, 100_000];

fn permutation(rows: u64, salt: u64) -> Vec<u32> {
    let mut order: Vec<u64> = (0..rows).collect();
    order.sort_by_key(|id| mix(id ^ salt << 48));
    let mut place = vec![0u32; rows as usize];
    for (position, id) in order.iter().enumerate() {
        place[*id as usize] = position as u32;
    }
    place
}

fn tier(position: u32, sizes: &[u64]) -> u8 {
    let mut start = 0u64;
    for (slot, size) in sizes.iter().enumerate() {
        start += size;
        if u64::from(position) < start {
            return slot as u8 + 1;
        }
    }
    0
}

#[derive(Clone, Copy)]
enum Span {
    Score(u32),
    Rank(u32),
    Band(u8),
}

type BandEntry = ((u8, u32), u64);

#[derive(Default)]
struct Probes {
    score: Option<ferro::Cursor<(u32, u64), ()>>,
    rank: Option<ferro::Cursor<u32, u64>>,
    band: Option<ferro::Cursor<BandEntry, ()>>,
}

impl Span {
    fn name(&self) -> String {
        match self {
            Span::Score(size) => format!("a score range of {size}"),
            Span::Rank(size) => format!("a unique rank range of {size}"),
            Span::Band(band) => format!("the prefix of band {band}"),
        }
    }

    fn lookup(&self, t: &Snapshot) -> ferro::table::IndexIter<Item> {
        match *self {
            Span::Score(size) => t.by_score_range(0..size),
            Span::Rank(size) => t.by_rank_range(0..size),
            Span::Band(band) => t.by_band_score_prefix(&band),
        }
    }

    fn holds(&self, score: u32, rank: u32, band: u8) -> bool {
        match *self {
            Span::Score(size) => score < size,
            Span::Rank(size) => rank < size,
            Span::Band(wanted) => band == wanted,
        }
    }

    fn indexed(&self, t: &Snapshot, probes: &mut Probes, key: u64, fields: (u32, u32, u8)) -> bool {
        let (score, rank, band) = fields;
        if !self.holds(score, rank, band) {
            return false;
        }
        match self {
            Span::Score(_) => probes
                .score
                .get_or_insert_with(|| t.by_score.cursor())
                .seek(&(score, key)),
            Span::Rank(_) => {
                let ranks = probes.rank.get_or_insert_with(|| t.unique_rank().cursor());
                ranks.seek(&rank) && ranks.read(|_, holder| *holder == key) == Some(true)
            }
            Span::Band(_) => probes
                .band
                .get_or_insert_with(|| t.by_band_score.cursor())
                .seek(&((band, score), key)),
        }
    }
}

struct RangeCase {
    group: u8,
    span: Span,
}

fn range_cases(crossover: bool) -> Vec<RangeCase> {
    let mut cases = Vec::new();
    if crossover {
        let grid: [(u8, &[u32]); 3] = [
            (2, &[100, 200, 400, 800, 1_600, 3_200, 6_400, 12_800]),
            (
                3,
                &[1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 128_000],
            ),
            (4, &[10_000, 20_000, 40_000, 80_000, 160_000]),
        ];
        for (group, sizes) in grid {
            for &size in sizes {
                cases.push(RangeCase {
                    group,
                    span: Span::Score(size),
                });
            }
        }
        return cases;
    }
    for group in 1..=4u8 {
        for size in [1_000, 10_000, 100_000] {
            cases.push(RangeCase {
                group,
                span: Span::Score(size),
            });
        }
    }
    for group in 1..=4u8 {
        for size in [10_000, 100_000] {
            cases.push(RangeCase {
                group,
                span: Span::Rank(size),
            });
        }
    }
    for group in 1..=4u8 {
        for band in [2u8, 3] {
            cases.push(RangeCase {
                group,
                span: Span::Band(band),
            });
        }
    }
    cases
}

#[derive(Clone, Copy, PartialEq)]
enum Joined {
    And,
    AndBack,
    Rows,
    Collect,
    Listed,
    Filter,
    Covered,
    Seek,
}

impl Joined {
    fn name(&self) -> &'static str {
        match self {
            Joined::And => "and",
            Joined::AndBack => "and, from the back",
            Joined::Rows => "and, rows",
            Joined::Collect => "collect and sort",
            Joined::Listed => "into_matches, alone",
            Joined::Filter => "lead rows, filtered",
            Joined::Covered => "lead covered, filtered",
            Joined::Seek => "covered, then seek",
        }
    }

    fn run(&self, t: &Snapshot, case: &RangeCase, out: &mut Vec<u64>) {
        let group = case.group;
        let span = case.span;
        match self {
            Joined::And => out.extend(t.find().by_group(&group).and(span.lookup(t)).keys()),
            Joined::AndBack => {
                out.extend(t.find().by_group(&group).and(span.lookup(t)).keys().rev());
                out.reverse();
            }
            Joined::Rows => out.extend(
                t.find()
                    .by_group(&group)
                    .and(span.lookup(t))
                    .map(|row| row.id),
            ),
            Joined::Collect => {
                let mut keys: Vec<u64> = span.lookup(t).keys().collect();
                keys.sort_unstable();
                out.extend(
                    t.by_group(&group)
                        .keys()
                        .filter(|key| keys.binary_search(key).is_ok()),
                );
            }
            Joined::Listed => {
                let keys: Vec<u64> = span.lookup(t).into_matches().keys().collect();
                out.extend(
                    t.by_group(&group)
                        .keys()
                        .filter(|key| keys.binary_search(key).is_ok()),
                );
            }
            Joined::Filter => t.by_group(&group).visit(|row| {
                if span.holds(row.score, row.rank, row.band) {
                    out.push(row.id);
                }
            }),
            Joined::Covered => out.extend(
                t.by_group(&group)
                    .covered()
                    .filter(|(_, (score, rank, band))| span.holds(*score, *rank, *band))
                    .map(|(key, _)| key),
            ),
            Joined::Seek => {
                let mut probes = Probes::default();
                out.extend(
                    t.by_group(&group)
                        .covered()
                        .filter(|(key, fields)| span.indexed(t, &mut probes, *key, *fields))
                        .map(|(key, _)| key),
                );
            }
        }
    }
}

type Snapshot = Table<Item>;

fn merge(t: &Snapshot, value: u32, out: &mut Vec<u64>) {
    let mut a = t.by_xs(&value).keys();
    let mut b = t.by_ys(&value).keys();
    let (mut x, mut y) = (a.next(), b.next());
    while let (Some(p), Some(q)) = (x, y) {
        match p.cmp(&q) {
            std::cmp::Ordering::Less => x = a.next(),
            std::cmp::Ordering::Greater => y = b.next(),
            std::cmp::Ordering::Equal => {
                out.push(p);
                x = a.next();
                y = b.next();
            }
        }
    }
}

fn after_x(t: &Snapshot, value: u32, from: u64) -> Option<u64> {
    match from.checked_sub(1) {
        Some(last) => t.by_xs_after(&value, &last).keys().next(),
        None => t.by_xs(&value).keys().next(),
    }
}

fn after_y(t: &Snapshot, value: u32, from: u64) -> Option<u64> {
    match from.checked_sub(1) {
        Some(last) => t.by_ys_after(&value, &last).keys().next(),
        None => t.by_ys(&value).keys().next(),
    }
}

fn leapfrog_after(t: &Snapshot, value: u32, out: &mut Vec<u64>) {
    let Some(mut x) = after_x(t, value, 0) else {
        return;
    };
    loop {
        let Some(y) = after_y(t, value, x) else {
            return;
        };
        let next = if y == x {
            out.push(x);
            x + 1
        } else {
            y
        };
        match after_x(t, value, next) {
            Some(found) => x = found,
            None => return,
        }
    }
}

fn leapfrog_cursor(t: &Snapshot, value: u32, out: &mut Vec<u64>) {
    let mut a = t.by_xs.cursor();
    let mut b = t.by_ys.cursor();
    let seek = |cursor: &mut ferro::Cursor<(u32, u64), ()>, from: u64| -> Option<u64> {
        cursor.seek(&(value, from));
        cursor
            .key()
            .filter(|entry| entry.0 == value)
            .map(|entry| entry.1)
    };
    let Some(mut x) = seek(&mut a, 0) else {
        return;
    };
    loop {
        let Some(y) = seek(&mut b, x) else {
            return;
        };
        let next = if y == x {
            out.push(x);
            x + 1
        } else {
            y
        };
        match seek(&mut a, next) {
            Some(found) => x = found,
            None => return,
        }
    }
}

fn leapfrog_cursor_steps(t: &Snapshot, value: u32, steps: usize, out: &mut Vec<u64>) {
    let mut a = t.by_xs.cursor();
    let mut b = t.by_ys.cursor();
    let seek = |cursor: &mut ferro::Cursor<(u32, u64), ()>, from: u64| -> Option<u64> {
        for _ in 0..steps {
            match cursor.key() {
                Some(entry) if entry.0 == value && entry.1 < from => {
                    cursor.advance();
                }
                Some(entry) if entry.0 == value => return Some(entry.1),
                _ => return None,
            }
        }
        cursor.seek(&(value, from));
        cursor
            .key()
            .filter(|entry| entry.0 == value)
            .map(|entry| entry.1)
    };
    a.seek(&(value, 0));
    b.seek(&(value, 0));
    let Some(mut x) = a
        .key()
        .filter(|entry| entry.0 == value)
        .map(|entry| entry.1)
    else {
        return;
    };
    loop {
        let Some(y) = seek(&mut b, x) else {
            return;
        };
        let next = if y == x {
            out.push(x);
            x + 1
        } else {
            y
        };
        match seek(&mut a, next) {
            Some(found) => x = found,
            None => return,
        }
    }
}

struct Gallop<'a> {
    t: &'a Snapshot,
    xs: bool,
    value: u32,
    keys: ferro::table::IndexKeys<Item>,
    current: Option<u64>,
    steps: usize,
}

impl<'a> Gallop<'a> {
    fn new(t: &'a Snapshot, xs: bool, value: u32, steps: usize) -> Self {
        let mut keys = if xs {
            t.by_xs(&value).keys()
        } else {
            t.by_ys(&value).keys()
        };
        let current = keys.next();
        Gallop {
            t,
            xs,
            value,
            keys,
            current,
            steps,
        }
    }

    fn seek(&mut self, target: u64) -> Option<u64> {
        for _ in 0..self.steps {
            match self.current {
                Some(key) if key < target => self.current = self.keys.next(),
                other => return other,
            }
        }
        if self.current.is_some_and(|key| key < target) {
            self.keys = if self.xs {
                self.t.by_xs_after(&self.value, &(target - 1)).keys()
            } else {
                self.t.by_ys_after(&self.value, &(target - 1)).keys()
            };
            self.current = self.keys.next();
        }
        self.current
    }
}

fn gallop(t: &Snapshot, value: u32, steps: usize, out: &mut Vec<u64>) {
    let mut a = Gallop::new(t, true, value, steps);
    let mut b = Gallop::new(t, false, value, steps);
    let Some(mut x) = a.current else {
        return;
    };
    loop {
        let Some(y) = b.seek(x) else {
            return;
        };
        let next = if y == x {
            out.push(x);
            x + 1
        } else {
            y
        };
        match a.seek(next) {
            Some(found) => x = found,
            None => return,
        }
    }
}

fn set(t: &Snapshot, value: u32, out: &mut Vec<u64>) {
    let a = t.by_xs(&value);
    let b = t.by_ys(&value);
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let held: HashSet<u64> = small.keys().collect();
    out.extend(large.keys().filter(|key| held.contains(key)));
}

#[derive(Clone, Copy, PartialEq)]
enum Strategy {
    Merge,
    After,
    Cursor,
    CursorSteps(usize),
    Gallop(usize),
    Set,
    And,
    AndBack,
    Rows,
    Gets,
}

impl Strategy {
    fn name(&self) -> String {
        match self {
            Strategy::Merge => "merge".to_string(),
            Strategy::After => "leapfrog, _after".to_string(),
            Strategy::Cursor => "leapfrog, cursor".to_string(),
            Strategy::CursorSteps(steps) => format!("{steps} steps, then cursor"),
            Strategy::Gallop(steps) => format!("{steps} steps, then _after"),
            Strategy::Set => "set".to_string(),
            Strategy::And => "and".to_string(),
            Strategy::AndBack => "and, from the back".to_string(),
            Strategy::Rows => "and, rows".to_string(),
            Strategy::Gets => "and, keys then get".to_string(),
        }
    }

    fn run(&self, t: &Snapshot, value: u32, out: &mut Vec<u64>) {
        match *self {
            Strategy::Merge => merge(t, value, out),
            Strategy::After => leapfrog_after(t, value, out),
            Strategy::Cursor => leapfrog_cursor(t, value, out),
            Strategy::CursorSteps(steps) => leapfrog_cursor_steps(t, value, steps, out),
            Strategy::Gallop(steps) => gallop(t, value, steps, out),
            Strategy::Set => set(t, value, out),
            Strategy::And => {
                let find = t.find();
                out.extend(find.by_xs(&value).and(find.by_ys(&value)).keys());
            }
            Strategy::AndBack => {
                let find = t.find();
                out.extend(find.by_xs(&value).and(find.by_ys(&value)).keys().rev());
                out.reverse();
            }
            Strategy::Rows => {
                let find = t.find();
                out.extend(find.by_xs(&value).and(find.by_ys(&value)).map(|row| row.id));
            }
            Strategy::Gets => {
                let find = t.find();
                for key in find.by_xs(&value).and(find.by_ys(&value)).keys() {
                    out.extend(t.get(&key).map(|row| row.id));
                }
            }
        }
    }
}

fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

fn main() {
    let rows: u64 = argument("--rows")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);
    let rounds: usize = argument("--rounds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let budget_ms: f64 = argument("--ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(40.0);
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let db = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 30)
        .open::<Root>(dir.path().join("combine.ferro"))
        .unwrap();
    let Sets {
        mut xs,
        mut ys,
        sizes,
    } = sets(rows);
    let scores = permutation(rows, 1);
    let ranks = permutation(rows, 2);
    let groups = permutation(rows, 3);
    let bands = permutation(rows, 4);
    for chunk in 0..rows.div_ceil(CHUNK) {
        let ids = chunk * CHUNK..((chunk + 1) * CHUNK).min(rows);
        db.update(|root| {
            for id in ids.clone() {
                let at = id as usize;
                root.items.insert(Item {
                    id,
                    xs: std::mem::take(&mut xs[at]),
                    ys: std::mem::take(&mut ys[at]),
                    group: tier(groups[at], &GROUPS),
                    score: scores[at],
                    rank: ranks[at],
                    band: tier(bands[at], &BANDS),
                    name: format!("item {}", mix(id) % 1_000_000),
                });
            }
        })
        .unwrap();
    }
    db.compact(Keep::Last(1)).unwrap();
    let snapshot = db.read();
    let table = &snapshot.items;
    let all = [
        ("merge", Strategy::Merge),
        ("after", Strategy::After),
        ("cursor", Strategy::Cursor),
        ("cursor2", Strategy::CursorSteps(2)),
        ("cursor4", Strategy::CursorSteps(4)),
        ("cursor8", Strategy::CursorSteps(8)),
        ("after8", Strategy::Gallop(8)),
        ("after32", Strategy::Gallop(32)),
        ("set", Strategy::Set),
        ("and", Strategy::And),
        ("and-back", Strategy::AndBack),
        ("rows", Strategy::Rows),
        ("gets", Strategy::Gets),
    ];
    let joins = [
        ("and", Joined::And),
        ("and-back", Joined::AndBack),
        ("rows", Joined::Rows),
        ("collect", Joined::Collect),
        ("listed", Joined::Listed),
        ("filter", Joined::Filter),
        ("covered", Joined::Covered),
        ("seek", Joined::Seek),
    ];
    let chosen = argument("--strategies").unwrap_or_else(|| {
        all.iter()
            .map(|(name, _)| *name)
            .chain(joins.iter().map(|(name, _)| *name))
            .collect::<Vec<_>>()
            .join(",")
    });
    let mut names: Vec<&str> = Vec::new();
    for name in chosen.split(',') {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    for name in &names {
        let known = all.iter().any(|(known, _)| known == name)
            || joins.iter().any(|(known, _)| known == name);
        assert!(known, "unknown strategy {name}");
    }
    let strategies: Vec<Strategy> = names
        .iter()
        .filter_map(|name| all.iter().find(|(known, _)| known == name))
        .map(|(_, strategy)| *strategy)
        .collect();
    let joined: Vec<Joined> = names
        .iter()
        .filter_map(|name| joins.iter().find(|(known, _)| known == name))
        .map(|(_, join)| *join)
        .collect();
    let kinds = argument("--cases").unwrap_or_else(|| "equal,range".to_string());
    let kinds: Vec<&str> = kinds.split(',').collect();
    let only = argument("--only").unwrap_or_default();
    let counter = Counter::instructions();
    let mut out = Vec::new();
    let equal = kinds.contains(&"equal");
    for (index, case) in CASES.iter().enumerate().filter(|_| equal) {
        if !case.name.contains(&only) {
            continue;
        }
        let value = index as u32;
        let mut wanted = Vec::new();
        Strategy::Merge.run(table, value, &mut wanted);
        let mut samples = vec![(Vec::new(), Vec::new()); strategies.len()];
        for _ in 0..rounds {
            for (slot, strategy) in strategies.iter().enumerate() {
                out.clear();
                strategy.run(table, value, &mut out);
                assert_eq!(out, wanted, "{} on {}", strategy.name(), case.name);
                let began = Instant::now();
                let mut runs = 0u64;
                let before = counter.read();
                while began.elapsed().as_secs_f64() * 1000.0 < budget_ms {
                    out.clear();
                    strategy.run(table, value, &mut out);
                    black_box(&out);
                    runs += 1;
                }
                let instructions = counter.read() - before;
                let ns = began.elapsed().as_nanos() as f64 / runs as f64;
                samples[slot].0.push(ns);
                samples[slot].1.push(instructions as f64 / runs as f64);
            }
        }
        println!(
            "{} ({} and {}, {} shared)",
            case.name,
            sizes[index].0,
            sizes[index].1,
            wanted.len()
        );
        for (slot, strategy) in strategies.iter().enumerate() {
            let (ns, instructions) = samples[slot].clone();
            println!(
                "  {:<22} {:>11.1} us {:>13.0} instructions",
                strategy.name(),
                median(ns) / 1000.0,
                median(instructions)
            );
        }
    }
    let crossover = kinds.contains(&"crossover");
    let ranged = crossover || kinds.contains(&"range");
    for case in range_cases(crossover).iter().filter(|_| ranged) {
        let lead = table.by_group(&case.group).len();
        let title = format!("{lead} keys and {}", case.span.name());
        if !title.contains(&only) {
            continue;
        }
        let mut wanted = Vec::new();
        Joined::Filter.run(table, case, &mut wanted);
        let mut samples = vec![(Vec::new(), Vec::new()); joined.len()];
        for _ in 0..rounds {
            for (slot, join) in joined.iter().enumerate() {
                out.clear();
                join.run(table, case, &mut out);
                assert_eq!(out, wanted, "{} on {}", join.name(), case.span.name());
                let began = Instant::now();
                let mut runs = 0u64;
                let before = counter.read();
                while began.elapsed().as_secs_f64() * 1000.0 < budget_ms {
                    out.clear();
                    join.run(table, case, &mut out);
                    black_box(&out);
                    runs += 1;
                }
                let instructions = counter.read() - before;
                let ns = began.elapsed().as_nanos() as f64 / runs as f64;
                samples[slot].0.push(ns);
                samples[slot].1.push(instructions as f64 / runs as f64);
            }
        }
        println!(
            "{title} ({lead} and {}, {} shared)",
            case.span.lookup(table).len(),
            wanted.len()
        );
        for (slot, join) in joined.iter().enumerate() {
            let (ns, instructions) = samples[slot].clone();
            println!(
                "  {:<22} {:>11.1} us {:>13.0} instructions",
                join.name(),
                median(ns) / 1000.0,
                median(instructions)
            );
        }
    }
    drop(snapshot);
    drop(db);
}
