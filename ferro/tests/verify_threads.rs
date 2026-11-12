mod common;

use std::fs;
use std::path::Path;

use common::{Rng, distance_bytes, link_slots, text};
use ferro::store::{Store, StoreOptions};
use ferro::{Db, Durability, Map, Options, Verified, admin};

type Root = Map<u64, String>;

const THREADS: [usize; 4] = [1, 2, 8, 0];

fn options(threads: usize) -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1)
        .copy_threads(threads)
}

fn build(path: &Path) {
    let db: Db<Root> = options(1).open(path).unwrap();
    let mut rng = Rng::new(7);
    for commit in 0..10 {
        db.update(|root| {
            for key in commit * 2000..(commit + 1) * 2000 {
                root.insert(key, text(rng.next(), 20 + rng.below(60) as usize));
            }
        })
        .unwrap();
    }
    let side = db.fork("side").unwrap();
    for round in 0..150 {
        let key = rng.below(20_000);
        let value = text(rng.next(), 10 + rng.below(300) as usize);
        let branch = match round % 3 {
            0 => &side,
            _ => &db,
        };
        branch
            .update(|root| {
                root.insert(key, value);
            })
            .unwrap();
    }
    db.merge_with(&side, |root, base, theirs| {
        root.merge(base, theirs, |conflict| {
            conflict.ours.map(|value| (*value).clone())
        });
        ("merge", ())
    })
    .unwrap();
    let len = fs::metadata(path).unwrap().len();
    assert!(len > 3 << 19, "{len}");
}

fn records(path: &Path) -> Vec<(usize, usize)> {
    let options = StoreOptions {
        read_only: true,
        ..StoreOptions::default()
    };
    let (store, head) = Store::open(path, options).unwrap();
    let bytes = fs::read(path).unwrap();
    let mut out = Vec::new();
    store
        .walk(head.unwrap(), |ptr, _| {
            let start = ptr.offset as usize;
            out.push((start, link_slots(&bytes, start).1));
        })
        .unwrap();
    store.close();
    out.sort_unstable();
    out
}

fn verified(path: &Path, threads: usize) -> ferro::Result<Verified> {
    options(threads)
        .read_only(true)
        .open::<Root>(path)
        .and_then(|db| db.verify())
}

fn outcomes(path: &Path) -> Vec<String> {
    THREADS
        .iter()
        .map(|threads| format!("{:?}", verified(path, *threads)))
        .chain([format!("{:?}", admin::verify(path))])
        .collect()
}

#[test]
fn verify_counts_the_same_objects_and_bytes_at_every_thread_count() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("whole.ferro");
    build(&path);
    let reports: Vec<Verified> = THREADS
        .iter()
        .map(|threads| verified(&path, *threads).unwrap())
        .chain([admin::verify(&path).unwrap()])
        .collect();
    assert!(
        reports.iter().all(|report| *report == reports[0]),
        "{reports:?}"
    );
    let records = records(&path);
    assert!(reports[0].objects >= records.len() as u64);
    assert!(reports[0].objects > 1000);
}

#[test]
fn damage_anywhere_fails_verify_with_one_error_at_every_thread_count() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let source = dir.path().join("source.ferro");
    build(&source);
    let bytes = fs::read(&source).unwrap();
    let records = records(&source);
    let path = dir.path().join("damaged.ferro");
    let step = records.len() / 40;
    let mut checked = 0;
    for (index, &(start, end)) in records.iter().enumerate().step_by(step.max(1)) {
        let mut flipped = bytes.clone();
        flipped[(start + end) / 2] ^= 0x41;
        let mut twice = flipped.clone();
        let (later, later_end) = records[(index + records.len() / 2) % records.len()];
        twice[(later + later_end) / 2] ^= 0x41;
        let mut redirected = bytes.clone();
        let (slots, _) = link_slots(&bytes, start);
        if let Some((slot, patch)) = slots.iter().find_map(|slot| {
            let target = slots
                .iter()
                .find(|other| other.target != slot.target)?
                .target;
            Some((slot, distance_bytes(start as u64, target, slot.len)?))
        }) {
            redirected[slot.at..slot.at + slot.len].copy_from_slice(&patch);
        }
        for image in [&flipped, &twice, &redirected] {
            if *image == bytes {
                continue;
            }
            fs::write(&path, image).unwrap();
            let outcomes = outcomes(&path);
            assert!(
                outcomes.iter().all(|outcome| *outcome == outcomes[0]),
                "record {start}..{end}: {outcomes:?}"
            );
            assert!(
                outcomes[0].starts_with("Err("),
                "record {start}..{end} passed"
            );
            checked += 1;
        }
    }
    assert!(checked > 80, "only {checked} images checked");
}
