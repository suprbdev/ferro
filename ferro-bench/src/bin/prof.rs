use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use ferro::codec::{Decoder, Encoder};
use ferro::{Data, Db, Keep, Options, Reclaim};

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

const DEPTH: usize = 48;
const MAX: usize = 400_000;
static SAMPLES: [AtomicU64; MAX * DEPTH] = [const { AtomicU64::new(0) }; MAX * DEPTH];
static COUNT: AtomicUsize = AtomicUsize::new(0);
static TID: AtomicI32 = AtomicI32::new(0);
static LO: AtomicU64 = AtomicU64::new(0);
static HI: AtomicU64 = AtomicU64::new(0);
static ON: AtomicU64 = AtomicU64::new(0);
static ALL: AtomicU64 = AtomicU64::new(0);

extern "C" fn handler(_sig: libc::c_int, _info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    if ON.load(Ordering::Relaxed) == 0 {
        return;
    }
    let tid = unsafe { libc::gettid() };
    if tid != TID.load(Ordering::Relaxed) && ALL.load(Ordering::Relaxed) == 0 {
        return;
    }
    let slot = COUNT.fetch_add(1, Ordering::Relaxed);
    if slot >= MAX {
        return;
    }
    let uc = ctx as *mut libc::ucontext_t;
    let (rip, mut rbp, rsp) = unsafe {
        let g = &(*uc).uc_mcontext.gregs;
        (g[libc::REG_RIP as usize] as u64, g[libc::REG_RBP as usize] as u64, g[libc::REG_RSP as usize] as u64)
    };
    let base = slot * DEPTH;
    SAMPLES[base].store(rip, Ordering::Relaxed);
    let (lo, hi) = if tid == TID.load(Ordering::Relaxed) { (LO.load(Ordering::Relaxed), HI.load(Ordering::Relaxed)) } else { (rsp, rsp + (1 << 20)) };
    let mut i = 1;
    while i < DEPTH && rbp >= lo && rbp + 16 <= hi && rbp % 8 == 0 {
        let (next, ret) = unsafe { (*(rbp as *const u64), *((rbp + 8) as *const u64)) };
        if ret == 0 {
            break;
        }
        SAMPLES[base + i].store(ret.wrapping_sub(1), Ordering::Relaxed);
        i += 1;
        if next <= rbp {
            break;
        }
        rbp = next;
    }
}

fn install() {
    unsafe {
        TID.store(libc::gettid(), Ordering::Relaxed);
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        libc::pthread_getattr_np(libc::pthread_self(), &mut attr);
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        LO.store(addr as u64, Ordering::Relaxed);
        HI.store(addr as u64 + size as u64, Ordering::Relaxed);
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        libc::sigaction(libc::SIGPROF, &sa, std::ptr::null_mut());
        let period = std::env::var("PROF_US").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
        let tv = libc::itimerval {
            it_interval: libc::timeval { tv_sec: 0, tv_usec: period },
            it_value: libc::timeval { tv_sec: 0, tv_usec: period },
        };
        libc::setitimer(libc::ITIMER_PROF, &tv, std::ptr::null_mut());
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = Path::new(&args[1]);
    let rounds: u64 = args[2].parse().unwrap();
    let threads: usize = args[3].parse().unwrap();
    let out = PathBuf::from(&args[4]);
    let admin = args.get(5).is_some_and(|a| a == "admin");
    if std::env::var("PROF_ALL").is_ok() {
        ALL.store(1, Ordering::Relaxed);
    }
    install();
    let mut name = path.as_os_str().to_os_string();
    name.push(".prof");
    let work = PathBuf::from(name);
    for _ in 0..rounds {
        fs::copy(path, &work).unwrap();
        File::open(&work).unwrap().sync_all().unwrap();
        let began;
        if admin {
            began = Instant::now();
            ON.store(1, Ordering::Relaxed);
            ferro::admin::compact(&work, Keep::All).unwrap();
            ON.store(0, Ordering::Relaxed);
        } else {
            let db: Db<Opaque> = Options::new()
                .reclaim(Reclaim { every_bytes: 0 })
                .copy_threads(threads)
                .open(&work)
                .unwrap();
            began = Instant::now();
            ON.store(1, Ordering::Relaxed);
            db.compact(Keep::All).unwrap();
            ON.store(0, Ordering::Relaxed);
            drop(db);
        }
        eprintln!("secs={:.3}", began.elapsed().as_secs_f64());
        let _ = fs::remove_file(&work);
    }
    let n = COUNT.load(Ordering::Relaxed).min(MAX);
    let maps = fs::read_to_string("/proc/self/maps").unwrap();
    let exe = fs::read_link("/proc/self/exe").unwrap();
    let mut base = 0u64;
    for line in maps.lines() {
        if line.ends_with(exe.to_str().unwrap()) {
            let start = u64::from_str_radix(line.split('-').next().unwrap(), 16).unwrap();
            let off = u64::from_str_radix(line.split_whitespace().nth(2).unwrap(), 16).unwrap();
            if off == 0 {
                base = start;
                break;
            }
        }
    }
    let mut f = std::io::BufWriter::new(File::create(&out).unwrap());
    writeln!(f, "base {base:x}").unwrap();
    for line in maps.lines() {
        writeln!(f, "map {line}").unwrap();
    }
    for s in 0..n {
        let mut parts = Vec::new();
        for d in 0..DEPTH {
            let a = SAMPLES[s * DEPTH + d].load(Ordering::Relaxed);
            if a == 0 {
                break;
            }
            parts.push(format!("{a:x}"));
        }
        writeln!(f, "s {}", parts.join(" ")).unwrap();
    }
    eprintln!("samples={n}");
}
