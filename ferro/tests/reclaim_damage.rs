mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::Rng;
use ferro::store::{Store, StoreOptions};
use ferro::{Bytes, Db, Durability, Error, Keep, Map, Options, Reclaim};

struct Peak;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static HIGH: AtomicUsize = AtomicUsize::new(0);

fn grew(size: usize) {
    let now = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    HIGH.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System` with the caller's layout, pointer and size unchanged
// and returns what `System` returns, so `System`'s guarantees carry over. Besides that it only
// updates the atomic counters `LIVE` and `HIGH`, which neither allocates nor panics.
unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        // SAFETY: the caller meets `GlobalAlloc::alloc`'s contract for `layout`, which is passed on
        // to `System` unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout`, and every block this allocator hands out came from `System` with that layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        // SAFETY: the caller meets `GlobalAlloc::alloc_zeroed`'s contract for `layout`, which is
        // passed on to `System` unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        grew(size);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout` and that `size` is a valid new size for it. The block came from `System` with
        // that layout, and all three are passed on unchanged.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Peak = Peak;

type Root = Map<u64, Bytes>;

const MIB: usize = 1 << 20;
const HOSTS: u64 = 4;
const AFTER_BULK: u64 = 1 << 20;
const CLAIM: usize = 16 * MIB;

fn options(threads: usize) -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1)
        .reclaim(Reclaim { every_bytes: 0 })
        .copy_threads(threads)
}

fn noise(rng: &mut Rng, len: usize) -> Vec<u8> {
    (0..len).map(|_| rng.next() as u8).collect()
}

fn marker(kind: &[u8; 8], host: u64) -> Vec<u8> {
    let mut marker = kind.to_vec();
    marker.extend_from_slice(&host.to_le_bytes());
    marker
}

fn claims_much() -> Vec<u8> {
    let mut fake = Vec::new();
    let mut claim = CLAIM;
    while claim >= 0x80 {
        fake.push(claim as u8 | 0x80);
        claim >>= 7;
    }
    fake.push(claim as u8);
    fake.push(0);
    fake
}

fn links_to_garbage() -> Vec<u8> {
    let mut fake = vec![8, 2];
    fake.extend_from_slice(b"notabody");
    fake.extend_from_slice(&[0; 20]);
    fake.push(80);
    fake.resize(40, 0);
    fake.extend_from_slice(&[0xff; 12]);
    fake
}

fn host(rng: &mut Rng, host: u64) -> Bytes {
    let mut value = noise(rng, 64 << 10);
    for (at, kind, fake) in [
        (4096, b"CLAIMS::", claims_much()),
        (8192, b"GARBAGE:", links_to_garbage()),
    ] {
        let marker = marker(kind, host);
        value[at..at + marker.len()].copy_from_slice(&marker);
        let at = at + marker.len();
        value[at..at + fake.len()].copy_from_slice(&fake);
    }
    Bytes::from(value)
}

fn build(path: &Path) {
    let db: Db<Root> = options(1).open(path).unwrap();
    let mut rng = Rng::new(3);
    db.update(|root| {
        for key in 0..HOSTS {
            root.insert(AFTER_BULK + key, host(&mut rng, key));
        }
    })
    .unwrap();
    for round in 0..5 {
        db.update(|root| {
            for key in 0..1000 {
                root.insert(
                    1000 + round * 1000 + key,
                    Bytes::from(noise(&mut rng, 4096)),
                );
            }
        })
        .unwrap();
    }
    db.update(|root| {
        root.insert(AFTER_BULK + HOSTS, Bytes::from(b"small".to_vec()));
    })
    .unwrap();
}

fn varint(bytes: &[u8], at: &mut usize) -> u64 {
    let mut value = 0;
    let mut shift = 0;
    loop {
        let byte = bytes[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return value;
        }
    }
}

fn find(bytes: &[u8], marker: &[u8]) -> u64 {
    let at = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap();
    (at + marker.len()) as u64
}

struct Link {
    hash_at: usize,
    delta_at: usize,
    delta_len: usize,
    target: u64,
}

fn links_of(bytes: &[u8], offset: u64) -> Vec<Link> {
    let mut at = offset as usize;
    let stored = varint(bytes, &mut at) as usize;
    let flagged = varint(bytes, &mut at);
    if flagged & 1 == 1 {
        varint(bytes, &mut at);
    }
    at += stored;
    let mut links = Vec::new();
    for _ in 0..flagged >> 1 {
        let hash_at = at;
        at += 20;
        let delta_at = at;
        let delta = varint(bytes, &mut at);
        let delta = ((delta >> 1) as i64) ^ -((delta & 1) as i64);
        links.push(Link {
            hash_at,
            delta_at,
            delta_len: at - delta_at,
            target: offset.wrapping_sub(delta as u64),
        });
    }
    links
}

fn retarget(bytes: &mut [u8], offset: u64, link: &Link, target: u64) {
    let delta = offset as i64 - target as i64;
    let mut value = ((delta << 1) ^ (delta >> 63)) as u64;
    for index in 0..link.delta_len {
        let more = if index + 1 < link.delta_len { 0x80 } else { 0 };
        bytes[link.delta_at + index] = (value & 0x7f) as u8 | more;
        value >>= 7;
    }
    assert_eq!(
        value, 0,
        "the link at {} cannot reach {target}",
        link.delta_at
    );
}

struct Hosts {
    parent: u64,
    hosts: Vec<(u64, u64)>,
}

fn layout(path: &Path) -> Hosts {
    let options = StoreOptions {
        read_only: true,
        ..StoreOptions::default()
    };
    let (store, head) = Store::open(path, options).unwrap();
    let mut hosts = Vec::new();
    let mut parents = Vec::new();
    store
        .walk(head.unwrap(), |ptr, object| {
            if object.body.len() > 60 << 10 {
                let key = (0..HOSTS)
                    .find(|key| {
                        let marker = marker(b"CLAIMS::", *key);
                        object.body.windows(16).any(|window| window == marker)
                    })
                    .unwrap();
                hosts.push((ptr.offset, key));
            }
            parents.push((
                ptr.offset,
                object
                    .links
                    .iter()
                    .map(|link| link.offset)
                    .collect::<Vec<_>>(),
            ));
        })
        .unwrap();
    store.close();
    let parent = parents
        .iter()
        .filter(|(_, links)| {
            links
                .iter()
                .any(|link| hosts.iter().any(|host| host.0 == *link))
        })
        .map(|(offset, _)| *offset)
        .max()
        .unwrap();
    Hosts { parent, hosts }
}

enum Damage {
    MidRecord,
    LargeClaims,
    ParentDamagedToo,
}

fn damaged(source: &[u8], layout: &Hosts, damage: &Damage) -> (Vec<u8>, String) {
    let mut bytes = source.to_vec();
    let parent = layout.parent;
    let links = links_of(&bytes, parent);
    let (link, key) = links
        .iter()
        .find_map(|link| {
            let (_, key) = layout.hosts.iter().find(|host| host.0 == link.target)?;
            Some((link, *key))
        })
        .unwrap();
    let kind = match damage {
        Damage::MidRecord => b"GARBAGE:",
        _ => b"CLAIMS::",
    };
    let fake = find(source, &marker(kind, key));
    retarget(&mut bytes, parent, link, fake);
    let wrong = match damage {
        Damage::ParentDamagedToo => {
            bytes[link.hash_at] ^= 1;
            parent
        }
        _ => fake,
    };
    (bytes, format!("hash mismatch for object at {wrong}"))
}

fn peak_of<R>(run: impl FnOnce() -> R) -> (R, usize) {
    let start = LIVE.load(Ordering::Relaxed);
    HIGH.store(start, Ordering::Relaxed);
    let out = run();
    (out, HIGH.load(Ordering::Relaxed).saturating_sub(start))
}

#[test]
fn a_link_damaged_to_land_mid_record_fails_alike_at_every_thread_count() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let source = dir.path().join("source.ferro");
    build(&source);
    let layout = layout(&source);
    let bytes = fs::read(&source).unwrap();
    for damage in [
        Damage::MidRecord,
        Damage::LargeClaims,
        Damage::ParentDamagedToo,
    ] {
        let (damaged, wrong) = damaged(&bytes, &layout, &damage);
        let mut peaks = Vec::new();
        for threads in [1, 4] {
            let path = dir.path().join(format!("damaged-{threads}.ferro"));
            fs::write(&path, &damaged).unwrap();
            let db: Db<Root> = options(threads).open(&path).unwrap();
            let (outcome, peak) = peak_of(|| db.compact(Keep::Last(1)));
            println!("{threads} threads: {outcome:?}, peak {peak}");
            match outcome {
                Err(Error::Corrupt(message)) => assert_eq!(message, wrong, "{threads} threads"),
                other => panic!("{threads} threads: {other:?}"),
            }
            peaks.push(peak);
        }
        assert!(
            peaks[1] < peaks[0] + CLAIM / 4 * 3,
            "4 threads peaked at {} bytes, 1 thread at {}",
            peaks[1],
            peaks[0]
        );
    }
}
