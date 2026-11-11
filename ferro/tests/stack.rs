#![cfg(target_os = "linux")]

mod common;

use std::cell::Cell;
use std::hint::black_box;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::Command;
use std::sync::{Arc, Barrier};

use common::fast;
use corosensei::{Coroutine, CoroutineResult, Yielder};
use ferro::codec::{Decoder, Encoder, HashSink, MAX_DEPTH};
use ferro::{Data, Db, Error, Result};

const CHILD: &str = "FERRO_STACK_CHILD";

#[derive(Clone, Default)]
struct Hand(Option<Box<Hand>>);

impl Drop for Hand {
    fn drop(&mut self) {
        let mut next = self.0.take();
        while let Some(mut node) = next {
            next = node.0.take();
        }
    }
}

impl Data for Hand {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        match &self.0 {
            None => {
                w.u8(0);
                Ok(())
            }
            Some(next) => {
                w.u8(1);
                w.nested(|w| Hand::encode(next, w))
            }
        }
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        match r.u8()? {
            0 => Ok(Hand(None)),
            _ => Ok(Hand(Some(Box::new(r.nested(Hand::decode)?)))),
        }
    }
}

#[derive(Data, Clone, Default)]
struct Heavy {
    pad: [u64; 16],
    label: String,
    next: Option<Box<Heavy>>,
}

impl Drop for Heavy {
    fn drop(&mut self) {
        let mut next = self.next.take();
        while let Some(mut node) = next {
            next = node.next.take();
        }
    }
}

fn heavy_chain(depth: usize) -> Vec<u8> {
    let mut node = Heavy::default();
    for level in 0..depth {
        let mut above = Heavy::default();
        above.pad[level % 16] = level as u64;
        above.next = Some(Box::new(node));
        node = above;
    }
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    node.encode(&mut encoder).unwrap();
    encoder.into_object().body
}

fn hand_chain(depth: usize) -> Vec<u8> {
    let mut bytes = vec![1; depth];
    bytes.push(0);
    bytes
}

fn in_child(case: &str) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, case)
        .output()
        .unwrap()
}

fn passes(case: &str) {
    let out = in_child(case);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains(&format!("case {case} passed")),
        "case {case}: {:?}\n{stdout}{stderr}",
        out.status
    );
}

fn interleaved_nested(lifo: bool) {
    let start = || {
        Coroutine::new(|yielder: &Yielder<(), ()>, ()| {
            let mut sink = HashSink;
            let mut w = Encoder::new(&mut sink);
            w.nested(|_| {
                let canary = black_box([0x5au8; 512]);
                yielder.suspend(());
                assert!(
                    black_box(canary).iter().all(|&b| b == 0x5a),
                    "the frame on the segment was overwritten"
                );
                Ok(())
            })
            .unwrap();
        })
    };
    let mut a = start();
    let mut b = start();
    assert!(matches!(a.resume(()), CoroutineResult::Yield(())));
    assert!(matches!(b.resume(()), CoroutineResult::Yield(())));
    if lifo {
        assert!(matches!(b.resume(()), CoroutineResult::Return(())));
    }
    assert!(matches!(a.resume(()), CoroutineResult::Return(())));
    let filler: Vec<Vec<u8>> = (0..4).map(|_| vec![0xa5u8; 16 << 20]).collect();
    black_box(&filler);
    if !lifo {
        assert!(matches!(b.resume(()), CoroutineResult::Return(())));
    }
}

fn interleaved_commits() {
    let dir = tempfile::tempdir().unwrap();
    let first: Arc<Db<u64>> = Arc::new(fast().open(dir.path().join("a.ferro")).unwrap());
    let second: Arc<Db<u64>> = Arc::new(fast().open(dir.path().join("b.ferro")).unwrap());
    let start = |db: Arc<Db<u64>>| {
        Coroutine::new(move |yielder: &Yielder<(), ()>, ()| {
            db.update(|value| {
                let canary = black_box([0x5au8; 512]);
                yielder.suspend(());
                assert!(
                    black_box(canary).iter().all(|&b| b == 0x5a),
                    "the frame on the segment was overwritten"
                );
                *value += 1;
            })
            .unwrap();
        })
    };
    let mut a = start(first.clone());
    let mut b = start(second.clone());
    assert!(matches!(a.resume(()), CoroutineResult::Yield(())));
    assert!(matches!(b.resume(()), CoroutineResult::Yield(())));
    assert!(matches!(a.resume(()), CoroutineResult::Return(())));
    let filler: Vec<Vec<u8>> = (0..4).map(|_| vec![0xa5u8; 16 << 20]).collect();
    black_box(&filler);
    assert!(matches!(b.resume(()), CoroutineResult::Return(())));
    assert_eq!(*first.read(), 1);
    assert_eq!(*second.read(), 1);
}

#[inline(never)]
fn recurse(n: u64) -> u64 {
    let pad = black_box([n as u8; 1024]);
    if black_box(n) == u64::MAX {
        return 0;
    }
    recurse(n + 1).wrapping_add(pad[(n % 1024) as usize] as u64)
}

fn overflow(in_commit: bool) {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<u64> = fast().open(dir.path().join("o.ferro")).unwrap();
    std::thread::Builder::new()
        .name("overflowing".into())
        .stack_size(2 << 20)
        .spawn(move || match in_commit {
            true => db.update(|value| *value = recurse(0)).unwrap(),
            false => {
                db.update(|value| *value = 1).unwrap();
                black_box(recurse(0));
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

fn rss_kb() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap();
    let field = |name: &str| {
        text.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    (field("Rss:"), field("AnonHugePages:"))
}

fn parked_threads(
    threads: usize,
    stack: usize,
    work: impl Fn() + Send + Sync + 'static,
) -> (u64, u64) {
    let work = Arc::new(work);
    let ready = Arc::new(Barrier::new(threads + 1));
    let release = Arc::new(Barrier::new(threads + 1));
    let before = rss_kb();
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let (work, ready, release) = (work.clone(), ready.clone(), release.clone());
            std::thread::Builder::new()
                .stack_size(stack)
                .spawn(move || {
                    work();
                    ready.wait();
                    release.wait();
                })
                .unwrap()
        })
        .collect();
    ready.wait();
    let after = rss_kb();
    release.wait();
    for handle in handles {
        handle.join().unwrap();
    }
    (after.0 - before.0, after.1.saturating_sub(before.1))
}

fn memory() {
    const THREADS: usize = 32;
    let dir = tempfile::tempdir().unwrap();
    let db: Arc<Db<u64>> = Arc::new(fast().open(dir.path().join("m.ferro")).unwrap());
    db.update(|value| *value += 1).unwrap();
    let committing = |db: &Arc<Db<u64>>| {
        let db = db.clone();
        move || db.update(|value| *value += 1).unwrap()
    };
    let big = parked_threads(THREADS, 64 << 20, committing(&db));
    let small = parked_threads(THREADS, 2 << 20, committing(&db));
    let chain = Arc::new(hand_chain(MAX_DEPTH));
    let deep = {
        let chain = chain.clone();
        parked_threads(THREADS, 2 << 20, move || {
            drop(Hand::decode(&mut Decoder::new(&chain, &[], None)).unwrap())
        })
    };
    let heavy = Arc::new(heavy_chain(MAX_DEPTH - 1));
    let heavier = {
        let heavy = heavy.clone();
        parked_threads(THREADS, 2 << 20, move || {
            drop(Heavy::decode(&mut Decoder::new(&heavy, &[], None)).unwrap())
        })
    };
    let used = {
        let heavy = heavy.clone();
        parked_threads(THREADS, 64 << 20, move || {
            drop(Heavy::decode(&mut Decoder::new(&heavy, &[], None)).unwrap())
        })
    };
    let per = |(rss, thp): (u64, u64)| (rss / THREADS as u64, thp / THREADS as u64);
    println!(
        "rss kB per thread, and of it in huge pages: commit on 64 MiB stack {:?}, commit on 2 MiB \
         stack {:?}, chain at the limit on 2 MiB stack {:?}, heavy chain at the limit on 2 MiB \
         stack {:?}, the same on 64 MiB stack {:?}",
        per(big),
        per(small),
        per(deep),
        per(heavier),
        per(used),
    );
    assert!(
        small.0 <= big.0 + (THREADS as u64) * 256,
        "a small commit on a 2 MiB thread keeps {} kB more a thread than on a 64 MiB one",
        (small.0 - big.0) / THREADS as u64
    );
    assert!(
        used.0 >= heavier.0 + (THREADS as u64) * (3 << 10),
        "a thread that read deep on segments keeps {} kB a thread, where one that read as deep on \
         its own stack keeps {}",
        heavier.0 / THREADS as u64,
        used.0 / THREADS as u64
    );
}

thread_local! {
    static LATE: Late = const { Late };
}

struct Late;

impl Drop for Late {
    fn drop(&mut self) {
        let chain = hand_chain(MAX_DEPTH);
        let read = Hand::decode(&mut Decoder::new(&chain, &[], None)).map(drop);
        println!("decode in a thread-local destructor: {read:?}");
    }
}

fn tls_destructor(late_first: bool) {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let chain = hand_chain(MAX_DEPTH);
            if late_first {
                LATE.with(|_| {});
            }
            Hand::decode(&mut Decoder::new(&chain, &[], None)).unwrap();
            if !late_first {
                LATE.with(|_| {});
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

fn reentrant_commits() {
    let dir = tempfile::tempdir().unwrap();
    let outer: Db<u64> = fast().open(dir.path().join("outer.ferro")).unwrap();
    let inner: Db<u64> = fast().open(dir.path().join("inner.ferro")).unwrap();
    let chain = hand_chain(MAX_DEPTH);
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            for round in 0..3u64 {
                outer
                    .update(|value| {
                        inner
                            .update(|value| {
                                Hand::decode(&mut Decoder::new(&chain, &[], None)).unwrap();
                                *value += 1
                            })
                            .unwrap();
                        *value += 1
                    })
                    .unwrap();
                assert_eq!(*outer.read(), round + 1);
                assert_eq!(*inner.read(), round + 1);
                let caught = catch_unwind(AssertUnwindSafe(|| {
                    outer.update(|_| -> () {
                        let depth = Cell::new(0);
                        let _ = inner.update(|_| depth.set(1));
                        std::panic::panic_any(format!("payload {}", depth.get()))
                    })
                }));
                let payload = caught.expect_err("the closure should have panicked");
                assert_eq!(
                    payload.downcast_ref::<String>().map(String::as_str),
                    Some("payload 1")
                );
            }
            let over = hand_chain(MAX_DEPTH + 1);
            assert!(matches!(
                Hand::decode(&mut Decoder::new(&over, &[], None)),
                Err(Error::Decode(_))
            ));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn child() {
    let Ok(case) = std::env::var(CHILD) else {
        return;
    };
    match case.as_str() {
        "interleaved-nested" => interleaved_nested(false),
        "nested-lifo" => interleaved_nested(true),
        "interleaved-commits" => interleaved_commits(),
        "overflow-in-commit" => overflow(true),
        "overflow-outside" => overflow(false),
        "memory" => memory(),
        "tls-late-first" => tls_destructor(true),
        "tls-late-last" => tls_destructor(false),
        "reentrant-commits" => reentrant_commits(),
        other => panic!("unknown case {other}"),
    }
    println!("case {case} passed");
}

#[test]
#[cfg_attr(
    ferro_asan,
    ignore = "corosensei tells AddressSanitizer of its switches only with its sanitizer feature"
)]
fn coroutines_that_interleave_nested_levels_keep_their_segments() {
    passes("nested-lifo");
    passes("interleaved-nested");
}

#[test]
#[cfg_attr(
    ferro_asan,
    ignore = "corosensei tells AddressSanitizer of its switches only with its sanitizer feature"
)]
fn coroutines_that_interleave_commits_keep_their_segments() {
    passes("interleaved-commits");
}

#[test]
#[cfg_attr(
    ferro_sanitized,
    ignore = "a sanitizer reports a stack overflow in its own words, or as a bare SIGSEGV"
)]
fn a_stack_overflow_in_a_commit_closure_is_reported_as_one() {
    let control = in_child("overflow-outside");
    let control_err = String::from_utf8_lossy(&control.stderr);
    assert!(
        control_err.contains("has overflowed its stack"),
        "control: {:?}\n{control_err}",
        control.status
    );
    let out = in_child("overflow-in-commit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("has overflowed its stack"),
        "in commit: {:?}\n{stderr}",
        out.status
    );
}

#[test]
#[cfg_attr(
    ferro_asan,
    ignore = "AddressSanitizer's shadow memory weighs on every stack"
)]
fn a_small_commit_on_a_small_stack_keeps_little_memory() {
    passes("memory");
}

#[test]
fn a_deep_decode_in_a_thread_local_destructor_does_not_crash() {
    passes("tls-late-first");
    passes("tls-late-last");
}

#[test]
fn commits_nest_and_panics_cross_the_segments() {
    passes("reentrant-commits");
}

#[test]
fn a_shallow_box_decodes() {
    let bytes = hand_chain(1);
    assert!(Hand::decode(&mut Decoder::new(&bytes, &[], None)).is_ok());
}
