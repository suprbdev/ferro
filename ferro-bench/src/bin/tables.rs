use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use booking::date::DAY_MS;
use booking::seed::{Plan, seed};
use booking::{BookingError, Hotels, HoldRequest, Reservations, Why};
use ferro::{Db, Durability, Options};

struct Counter(Option<i32>);

impl Counter {
    fn instructions() -> Counter {
        let mut attr = [0u64; 16];
        attr[0] = 128 << 32;
        attr[1] = 1;
        attr[5] = 1 | 1 << 5 | 1 << 6;
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0, -1, -1, 0) };
        if fd < 0 {
            return Counter(None);
        }
        unsafe { libc::ioctl(fd as i32, 0x2400, 0) };
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

fn cpu_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
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
    fn take(counter: &Counter, began: Instant) -> Sample {
        Sample {
            wall: began.elapsed().as_nanos() as u64,
            cpu: cpu_ns(),
            instructions: counter.read(),
        }
    }
}

#[derive(Default)]
struct Cost {
    whole: Vec<Sample>,
    closure: Vec<Sample>,
}

fn median(mut values: Vec<u64>) -> f64 {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or(0) as f64 / 1000.0
}

impl Cost {
    fn report(&self, name: &str) {
        let count = self.whole.len().max(1) as f64;
        let rest = |pick: fn(&Sample) -> u64| {
            median(self.whole.iter().zip(&self.closure).map(|(w, c)| pick(w) - pick(c)).collect())
        };
        let mean = |samples: &[Sample]| samples.iter().map(|s| s.instructions).sum::<u64>() as f64 / count;
        println!(
            "  {name:<8} wall {:>7.1} us (closure {:>6.1}, rest {:>7.1})  cpu {:>7.1} us (closure {:>6.1}, rest {:>7.1})  instructions {:>9.0} (closure {:>8.0}, rest {:>9.0})",
            median(self.whole.iter().map(|s| s.wall).collect()),
            median(self.closure.iter().map(|s| s.wall).collect()),
            rest(|s| s.wall),
            median(self.whole.iter().map(|s| s.cpu).collect()),
            median(self.closure.iter().map(|s| s.cpu).collect()),
            rest(|s| s.cpu),
            mean(&self.whole),
            mean(&self.closure),
            mean(&self.whole) - mean(&self.closure),
        );
    }
}

fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn timed<R>(
    db: &Db<Hotels>,
    counter: &Counter,
    cost: &mut Cost,
    f: impl FnOnce(&mut Hotels, u64) -> Result<(String, R), BookingError>,
) -> Result<R, BookingError> {
    let mut inside = Sample::default();
    let began = Instant::now();
    let start = Sample::take(counter, began);
    let done = db.try_commit_stamped(|hotels, stamp| {
        let began = Instant::now();
        let start = Sample::take(counter, began);
        let ran = f(hotels, stamp.version());
        let end = Sample::take(counter, began);
        inside = Sample {
            wall: end.wall,
            cpu: end.cpu - start.cpu,
            instructions: end.instructions - start.instructions,
        };
        ran
    });
    let end = Sample::take(counter, began);
    if done.is_ok() {
        cost.whole.push(Sample {
            wall: end.wall,
            cpu: end.cpu - start.cpu,
            instructions: end.instructions - start.instructions,
        });
        cost.closure.push(inside);
    }
    done.map(|done| done.value)
}

fn main() {
    let pairs: u64 = argument("--pairs").and_then(|v| v.parse().ok()).unwrap_or(4_000);
    let warm: u64 = argument("--warm").and_then(|v| v.parse().ok()).unwrap_or(4_000);
    let label = argument("--label").unwrap_or_else(|| "run".to_string());
    let durability = match argument("--durability").as_deref() {
        Some("fsync") => Durability::Fsync,
        _ => Durability::Buffered,
    };
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".to_string()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let db = Options::new()
        .durability(durability)
        .open::<Hotels>(dir.path().join("tables.ferro"))
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
    let seeded = seed(&svc, &plan).unwrap();
    let counter = Counter::instructions();
    let mut rng = fastrand::Rng::with_seed(7);
    let mut confirmed = Vec::new();
    let mut costs = (Cost::default(), Cost::default(), Cost::default());
    let mut bytes = 0;
    let mut writes = 0;
    for round in 0..2 {
        let measured = round == 1;
        let (mut hold, mut confirm, mut cancel) = (Cost::default(), Cost::default(), Cost::default());
        let before = db.size_on_disk();
        let mut wrote = 0;
        for i in 0..if measured { pairs } else { warm } {
            let now = start_ms + i;
            let request = HoldRequest {
                customer: seeded.customers[rng.usize(0..seeded.customers.len())],
                room: seeded.rooms[rng.usize(0..seeded.rooms.len())],
                first: (start_ms / DAY_MS) as u32 + 1 + rng.u32(0..3_000),
                nights: rng.u32(1..=3),
                ttl_ms: 600_000,
            };
            let held = timed(&db, &counter, &mut hold, |hotels, _| {
                let held = hotels.hold(now, &request)?;
                Ok((format!("hold #{} room {}", held.id, request.room), held))
            });
            let Ok(held) = held else {
                continue;
            };
            wrote += 1;
            let paid = timed(&db, &counter, &mut confirm, |hotels, version| {
                let total = hotels.confirm(now, version, held.id, Some(held.total))?;
                Ok((format!("confirm #{} paid {total}", held.id), total))
            });
            if paid.is_ok() {
                wrote += 1;
                confirmed.push(held.id);
            }
            if i % 4 == 3 && !confirmed.is_empty() {
                let id = confirmed.swap_remove(rng.usize(0..confirmed.len()));
                let done = timed(&db, &counter, &mut cancel, |hotels, _| {
                    let refund = hotels.cancel(now, id, Why::Customer)?;
                    Ok((format!("cancel #{id} refund {refund}"), refund))
                });
                wrote += u64::from(done.is_ok());
            }
        }
        if measured {
            costs = (hold, confirm, cancel);
            bytes = db.size_on_disk() - before;
            writes = wrote;
        }
    }
    println!("{label}");
    costs.0.report("hold");
    costs.1.report("confirm");
    costs.2.report("cancel");
    println!("  bytes per write {:.0}", bytes as f64 / writes.max(1) as f64);
}
