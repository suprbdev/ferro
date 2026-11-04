use std::collections::HashSet;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use ferro::{Data, Durability, Keep, Options, Record, Table};

const CHUNK: u64 = 100_000;

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Item {
    #[key]
    id: u64,
    #[index(each)]
    xs: Vec<u32>,
    #[index(each)]
    ys: Vec<u32>,
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
    for chunk in 0..rows.div_ceil(CHUNK) {
        let ids = chunk * CHUNK..((chunk + 1) * CHUNK).min(rows);
        db.update(|root| {
            for id in ids.clone() {
                root.items.insert(Item {
                    id,
                    xs: std::mem::take(&mut xs[id as usize]),
                    ys: std::mem::take(&mut ys[id as usize]),
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
    let chosen = argument("--strategies").unwrap_or_else(|| {
        all.iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(",")
    });
    let strategies: Vec<Strategy> = chosen
        .split(',')
        .map(|name| {
            all.iter()
                .find(|(known, _)| *known == name)
                .map(|(_, strategy)| *strategy)
                .unwrap_or_else(|| panic!("unknown strategy {name}"))
        })
        .collect();
    let counter = Counter::instructions();
    let mut out = Vec::new();
    for (index, case) in CASES.iter().enumerate() {
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
    drop(snapshot);
    drop(db);
}
