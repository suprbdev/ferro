mod common;

use std::collections::BTreeMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use common::{
    BLOCK, FRAME_BYTES, Outcome, Rng, SECTOR, SLOT, assert_damaged, cases, fast, field, guarded,
    judged, quiet_ferro_panics, seal_frame, table_bytes, text, uncached,
};
use ferro::codec::{HASH_BYTES, Object, Sink};
use ferro::db::Clock;
use ferro::store::{Store, StoreOptions, record_len};
use ferro::{Db, Error, Keep, Map, Options, Ptr, admin};
use tempfile::TempDir;

type Root = Map<u64, String>;
type Model = BTreeMap<u64, String>;

const LOG_START: usize = 8192;
const SUPER: usize = 144;
const MAGIC: usize = 8;

struct Built {
    dir: TempDir,
    clock: u64,
    bytes: Vec<u8>,
    lens: Vec<usize>,
    headers: Vec<Vec<u8>>,
    states: Vec<Model>,
}

fn log_end(header: &[u8]) -> usize {
    let newest = if field(header, SLOT + 8) > field(header, 8) {
        SLOT
    } else {
        0
    };
    field(header, newest + 16)
}

fn build(commits: u64, keys_per_commit: u64) -> Built {
    build_as(fast(), commits, keys_per_commit, 600)
}

fn build_as(options: Options, commits: u64, keys_per_commit: u64, long: usize) -> Built {
    build_from(options, clock_start(), commits, keys_per_commit, long, text)
}

fn clock_start() -> u64 {
    std::env::var("FERRO_TEST_CLOCK")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        })
}

fn repetitive(seed: u64, len: usize) -> String {
    let mut rng = Rng::new(seed);
    let words: Vec<String> = (0..4)
        .map(|_| text(rng.next(), 3 + rng.below(6) as usize))
        .collect();
    let mut out = String::new();
    while out.len() < len {
        out.push_str(&words[rng.below(4) as usize]);
    }
    out.truncate(len);
    out
}

fn build_from(
    options: Options,
    clock: u64,
    commits: u64,
    keys_per_commit: u64,
    long: usize,
    value: fn(u64, usize) -> String,
) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.ferro");
    let ticks = Arc::new(AtomicU64::new(clock));
    let db = options
        .clock(Clock(Arc::new(move || {
            ticks.fetch_add(1, Ordering::Relaxed)
        })))
        .open::<Root>(&path)
        .unwrap();
    let mut rng = Rng::new(commits * 31 + keys_per_commit);
    let mut model = Model::new();
    let mut lens = vec![log_end(&fs::read(&path).unwrap())];
    let mut headers = vec![fs::read(&path).unwrap()[..LOG_START].to_vec()];
    let mut states = vec![model.clone()];
    for commit in 0..commits {
        let mut edits = Vec::new();
        for _ in 0..keys_per_commit {
            let key = rng.below(keys_per_commit * 3);
            let len = if rng.chance(25) {
                long
            } else {
                rng.below(40) as usize
            };
            edits.push((key, value(rng.next(), len)));
        }
        let remove = (commit > 0).then(|| rng.below(keys_per_commit * 3));
        db.commit(format!("c{commit}"), |root| {
            for (key, value) in &edits {
                root.insert(*key, value.clone());
            }
            if let Some(key) = remove {
                root.remove(&key);
            }
        })
        .unwrap();
        for (key, value) in edits {
            model.insert(key, value);
        }
        if let Some(key) = remove {
            model.remove(&key);
        }
        let bytes = fs::read(&path).unwrap();
        lens.push(log_end(&bytes));
        headers.push(bytes[..LOG_START].to_vec());
        states.push(model.clone());
    }
    assert_eq!(db.version(), commits);
    drop(db);
    let bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.len(), *lens.last().unwrap());
    Built {
        dir,
        clock,
        bytes,
        lens,
        headers,
        states,
    }
}

impl Built {
    fn target(&self) -> PathBuf {
        self.dir.path().join("image.ferro")
    }

    fn image(&self, commit: usize, end: usize) -> Vec<u8> {
        let mut out = self.headers[commit].clone();
        out.extend_from_slice(&self.bytes[LOG_START..end]);
        out
    }

    fn newest_slot(commit: usize) -> usize {
        (commit % 2) * SLOT
    }
}

fn matches_model(map: &Root, model: &Model) -> bool {
    map.len() == model.len()
        && map
            .iter()
            .map(|(k, v)| (k, (*v).clone()))
            .eq(model.iter().map(|(k, v)| (*k, v.clone())))
}

fn check_at_version(db: &Db<Root>, built: &Built, version: usize) {
    assert_eq!(db.version(), version as u64);
    assert!(matches_model(&db.read(), &built.states[version]));
    for v in 0..=version {
        assert!(matches_model(&db.at(v as u64).unwrap(), &built.states[v]));
    }
    assert!(db.at(version as u64 + 1).is_err());
    db.verify().unwrap();
}

fn assert_clear_error<T>(result: Result<T, Error>, context: &str) {
    match result {
        Err(Error::Corrupt(_) | Error::NotFerro(_)) => {}
        Err(other) => panic!("{context}: unexpected error {other}"),
        Ok(_) => panic!("{context}: opened a damaged file"),
    }
}

fn assert_both_slots_damaged(path: &Path, magic_kept: bool, context: &str) {
    let opens = [
        ("writable", fast().open::<Root>(path).map(drop)),
        (
            "read-only",
            fast().read_only(true).open::<Root>(path).map(drop),
        ),
        ("info", admin::info(path).map(drop)),
        ("verify", admin::verify(path).map(drop)),
        ("space", admin::space(path).map(drop)),
        ("compact", admin::compact(path, Keep::All).map(drop)),
    ];
    for (open, result) in opens {
        match result {
            Err(Error::Corrupt(message)) if magic_kept => {
                assert!(
                    message.contains("both superblocks are invalid"),
                    "{message}"
                );
            }
            Err(Error::NotFerro(_)) if !magic_kept => {}
            other => panic!("both slots damaged, {context}, {open}: {other:?}"),
        }
    }
}

#[test]
fn truncation_at_every_byte_opens_or_errors() {
    let built = build(6, 4);
    let path = built.target();
    let last = built.lens.len() - 1;
    let full = built.bytes.len();
    assert!(full < 64 * 1024, "image too large at {full} bytes");
    for end in (0..=full).chain([full + 1, full + 4096]) {
        let mut image = built.bytes[..end.min(full)].to_vec();
        image.resize(end, 0xAB);
        fs::write(&path, &image).unwrap();
        let readonly = fast().read_only(true).open::<Root>(&path);
        let writable = fast().open::<Root>(&path);
        if end >= full {
            check_at_version(&readonly.unwrap(), &built, last);
            check_at_version(&writable.unwrap(), &built, last);
        } else if end == 0 {
            assert_clear_error(readonly, "empty read-only");
            check_at_version(&writable.unwrap(), &built, 0);
        } else {
            assert_clear_error(readonly, "truncated read-only");
            assert_clear_error(writable, "truncated writable");
            assert_eq!(fs::metadata(&path).unwrap().len() as usize, end);
        }
    }
}

#[test]
fn crash_images_open_at_the_last_durable_commit() {
    let built = build(8, 3);
    let path = built.target();
    for commit in 0..built.lens.len() - 1 {
        let start = built.lens[commit];
        let end = built.lens[commit + 1];
        for tail in start..end {
            let image = built.image(commit, tail);
            fs::write(&path, &image).unwrap();
            let readonly = fast().read_only(true).open::<Root>(&path).unwrap();
            check_at_version(&readonly, &built, commit);
            drop(readonly);
            let db = fast().open::<Root>(&path).unwrap();
            check_at_version(&db, &built, commit);
            assert_eq!(fs::metadata(&path).unwrap().len() as usize, start);
        }
        fs::write(&path, built.image(commit, end)).unwrap();
        let readonly = fast().read_only(true).open::<Root>(&path).unwrap();
        check_at_version(&readonly, &built, commit + 1);
        drop(readonly);
        let db = fast().open::<Root>(&path).unwrap();
        check_at_version(&db, &built, commit + 1);
        assert_eq!(fs::metadata(&path).unwrap().len() as usize, end);
    }
}

#[test]
fn garbage_past_the_durable_tail_is_discarded() {
    let built = build(6, 3);
    let path = built.target();
    let mut rng = Rng::new(9);
    for commit in 0..built.lens.len() - 1 {
        for _ in 0..40 {
            let keep = built.lens[commit] + rng.below(64) as usize;
            let keep = keep.min(built.lens[commit + 1]);
            let mut image = built.image(commit, keep);
            for _ in 0..rng.below(9000) {
                image.push(rng.next() as u8);
            }
            fs::write(&path, &image).unwrap();
            let db = fast().open::<Root>(&path).unwrap();
            check_at_version(&db, &built, commit);
            db.update(|root| {
                root.insert(u64::MAX, "fresh".into());
            })
            .unwrap();
            drop(db);
            let db = fast().open::<Root>(&path).unwrap();
            assert_eq!(db.version(), commit as u64 + 1);
            assert_eq!(*db.read().get(&u64::MAX).unwrap(), "fresh");
            db.verify().unwrap();
        }
    }
}

#[test]
fn repeated_crashes_always_land_on_the_last_durable_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cycle.ferro");
    let mut rng = Rng::new(0xC0FFEE);
    let mut versions = vec![Model::new()];
    let mut crashes = 0;
    for cycle in 0..cases(600) {
        let db = fast().open::<Root>(&path).unwrap();
        assert_eq!(db.version(), versions.len() as u64 - 1);
        assert!(matches_model(&db.read(), versions.last().unwrap()));
        if cycle % 50 == 0 {
            for (version, model) in versions.iter().enumerate() {
                assert!(matches_model(&db.at(version as u64).unwrap(), model));
            }
            db.verify().unwrap();
        }
        let head = fs::read(&path).unwrap();
        let mut next = versions.last().unwrap().clone();
        let key = rng.below(64);
        let value = text(rng.next(), rng.below(900) as usize + 1);
        next.insert(key, value.clone());
        let unchanged = versions.last().unwrap().get(&key) == Some(&value);
        db.update(|root| {
            root.insert(key, value);
        })
        .unwrap();
        drop(db);
        if unchanged {
            continue;
        }
        let after = fs::read(&path).unwrap();
        if rng.chance(50) {
            versions.push(next);
            continue;
        }
        crashes += 1;
        let version = versions.len();
        let slot = Built::newest_slot(version);
        let torn = rng.below(SUPER as u64) as usize;
        let keep = head.len() + rng.below((after.len() - head.len()) as u64 + 1) as usize;
        if keep == after.len() {
            versions.push(next);
        }
        let mut image = after[..keep].to_vec();
        image[..LOG_START].copy_from_slice(&head[..LOG_START]);
        if rng.chance(50) {
            image[slot..slot + torn].copy_from_slice(&after[slot..slot + torn]);
            if image[slot..slot + SUPER] == after[slot..slot + SUPER] {
                image[slot + SUPER - 1] ^= 1;
            }
        }
        fs::write(&path, &image).unwrap();
    }
    assert!(crashes > 50);
}

#[test]
fn recovered_database_keeps_working_after_a_crash_image() {
    let built = build(8, 3);
    let path = built.target();
    for commit in 0..built.lens.len() - 1 {
        let end = (built.lens[commit] + built.lens[commit + 1]) / 2;
        fs::write(&path, built.image(commit, end)).unwrap();
        {
            let db = fast().open::<Root>(&path).unwrap();
            db.commit("after", |root| {
                root.insert(1_000_000, "after".repeat(200));
            })
            .unwrap();
        }
        let db = fast().open::<Root>(&path).unwrap();
        assert_eq!(db.version(), commit as u64 + 1);
        assert_eq!(db.read().get(&1_000_000).unwrap().len(), 1000);
        for v in 0..=commit {
            assert!(matches_model(&db.at(v as u64).unwrap(), &built.states[v]));
        }
        db.verify().unwrap();
    }
}

#[test]
fn torn_superblock_write_recovers_from_the_complete_frame() {
    let built = build(8, 3);
    let path = built.target();
    let last = built.lens.len() - 1;
    for commit in 0..last {
        let slot = Built::newest_slot(commit + 1);
        for torn in 0..=SUPER {
            let mut head = built.image(commit, built.lens[commit + 1]);
            let new = &built.headers[commit + 1][slot..slot + SUPER];
            head[slot..slot + torn].copy_from_slice(&new[..torn]);
            fs::write(&path, &head).unwrap();
            let db = fast().open::<Root>(&path).unwrap();
            check_at_version(&db, &built, commit + 1);
            drop(db);
            let mut suffix = built.image(commit, built.lens[commit + 1]);
            suffix[slot + torn..slot + SUPER].copy_from_slice(&new[torn..]);
            fs::write(&path, &suffix).unwrap();
            let db = fast().open::<Root>(&path).unwrap();
            check_at_version(&db, &built, commit + 1);
        }
    }
}

#[test]
fn torn_superblock_over_a_torn_frame_keeps_the_previous_commit() {
    let built = build(8, 3);
    let path = built.target();
    let last = built.lens.len() - 1;
    for commit in 0..last {
        let slot = Built::newest_slot(commit + 1);
        let new = &built.headers[commit + 1][slot..slot + SUPER];
        for torn in 0..SUPER {
            let mut image = built.image(commit, built.lens[commit + 1] - 1);
            image[slot..slot + torn].copy_from_slice(&new[..torn]);
            if image[slot..slot + SUPER] == *new {
                continue;
            }
            fs::write(&path, &image).unwrap();
            let db = fast().open::<Root>(&path).unwrap();
            check_at_version(&db, &built, commit);
        }
    }
}

#[test]
fn recovery_replays_every_complete_frame_past_a_stale_superblock() {
    let built = build(8, 3);
    let path = built.target();
    let last = built.lens.len() - 1;
    for base in 0..last {
        for frames in base + 1..=last {
            let full = built.lens[frames];
            fs::write(&path, built.image(base, full)).unwrap();
            let readonly = fast().read_only(true).open::<Root>(&path).unwrap();
            check_at_version(&readonly, &built, frames);
            drop(readonly);
            assert_eq!(fs::read(&path).unwrap(), built.image(base, full));
            let db = fast().open::<Root>(&path).unwrap();
            check_at_version(&db, &built, frames);
            drop(db);
            assert_eq!(fs::metadata(&path).unwrap().len() as usize, full);
            let mut image = fs::read(&path).unwrap();
            image[built.lens[frames - 1]] ^= 0xFF;
            fs::write(&path, &image).unwrap();
            let db = fast().open::<Root>(&path).unwrap();
            check_at_version(&db, &built, frames);
        }
    }
}

#[test]
fn recovery_stops_at_a_frame_two_bits_from_whole_as_at_a_torn_one() {
    let built = build(5, 3);
    let path = built.target();
    let (base, damaged, last) = (1, 3, built.lens.len() - 1);
    for position in built.lens[damaged - 1]..built.lens[damaged] {
        let mut image = built.image(base, built.lens[last]);
        image[position] ^= 0x41;
        fs::write(&path, &image).unwrap();
        let readonly = fast().read_only(true).open::<Root>(&path).unwrap();
        check_at_version(&readonly, &built, damaged - 1);
        drop(readonly);
        let db = fast().open::<Root>(&path).unwrap();
        check_at_version(&db, &built, damaged - 1);
        assert_eq!(
            fs::metadata(&path).unwrap().len() as usize,
            built.lens[damaged - 1]
        );
    }
}

fn fsync() -> Options {
    Options::new().cache_bytes(1 << 20)
}

#[test]
fn a_frame_one_bit_from_whole_past_the_superblock_is_reported_and_left_alone() {
    for (name, options) in [("buffered", fast()), ("fsync", fsync())] {
        let built = build_as(options.clone(), 5, 3, 600);
        let path = built.target();
        let (base, damaged, last) = (1, 3, built.lens.len() - 1);
        let (start, end) = (built.lens[damaged - 1], built.lens[damaged]);
        let mut reported = 0;
        for position in start..end {
            let mut image = built.image(base, built.lens[last]);
            image[position] ^= 1 << (position % 8);
            fs::write(&path, &image).unwrap();
            let context = format!("{name}: byte {position} of the frame at {start}..{end}");
            if !judged(&image, position, start, end) {
                let db = options.clone().open::<Root>(&path).unwrap();
                check_at_version(&db, &built, damaged - 1);
                continue;
            }
            let readonly = options.clone().read_only(true).open::<Root>(&path);
            assert_damaged(readonly, start, position, &context);
            assert_damaged(
                options.clone().open::<Root>(&path),
                start,
                position,
                &context,
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                image,
                "{context}: the file changed"
            );
            reported += 1;
        }
        assert!(
            reported > (end - start) * 9 / 10,
            "{name}: {reported} reported"
        );
    }
}

#[test]
fn a_bit_flipped_in_each_block_of_a_large_frame_is_reported() {
    let built = build_as(fast(), 3, 40, 3000);
    let path = built.target();
    let (start, end) = (built.lens[1], built.lens[2]);
    let table = table_bytes(end - start);
    let body = start + FRAME_BYTES;
    let records = end - body - table;
    assert!(records > 3 * BLOCK, "a frame of {records} bytes of records");
    let image = |flips: &[usize]| {
        let mut image = built.image(1, built.lens[3]);
        for at in flips {
            image[*at] ^= 1 << (at % 8);
        }
        image
    };
    let mut positions: Vec<usize> = (start..body).chain(end - table..end).collect();
    positions.extend((body..end - table).step_by(97));
    for position in positions {
        let image = image(&[position]);
        fs::write(&path, &image).unwrap();
        let context = format!("byte {position} of the frame at {start}..{end}");
        if judged(&image, position, start, end) {
            assert_damaged(fast().open::<Root>(&path), start, position, &context);
            assert_eq!(
                fs::read(&path).unwrap(),
                image,
                "{context}: the file changed"
            );
        } else {
            check_at_version(&fast().open::<Root>(&path).unwrap(), &built, 1);
        }
    }
    for block in 0..(records - 12) / BLOCK {
        let (first, second) = (body + block * BLOCK + 300, body + (block + 1) * BLOCK + 11);
        fs::write(&path, image(&[second, first])).unwrap();
        assert_damaged(
            fast().open::<Root>(&path),
            start,
            first,
            "flips in two blocks",
        );
        let torn = image(&[first, first + 1000]);
        fs::write(&path, &torn).unwrap();
        check_at_version(&fast().open::<Root>(&path).unwrap(), &built, 1);
        assert_eq!(fs::metadata(&path).unwrap().len() as usize, start);
        let entry = end - table + block * 8 + 3;
        fs::write(&path, image(&[first, entry])).unwrap();
        check_at_version(&fast().open::<Root>(&path).unwrap(), &built, 1);
        assert_eq!(fs::metadata(&path).unwrap().len() as usize, start);
    }
}

#[test]
fn a_frame_torn_between_sectors_that_landed_is_truncated() {
    let built = build_as(fast(), 3, 40, 3000);
    let path = built.target();
    let (start, end) = (built.lens[1], built.lens[2]);
    let older = &built.bytes[LOG_START..built.lens[1]];
    let mut sectors = 0;
    let mut sector = start.next_multiple_of(SECTOR);
    while sector + SECTOR <= end {
        for kept in [
            vec![0u8; SECTOR],
            vec![0xFFu8; SECTOR],
            older[..SECTOR].to_vec(),
        ] {
            let mut image = built.image(1, built.lens[3]);
            if image[sector..sector + SECTOR] == kept[..] {
                continue;
            }
            image[sector..sector + SECTOR].copy_from_slice(&kept);
            fs::write(&path, &image).unwrap();
            let readonly = fast().read_only(true).open::<Root>(&path).unwrap();
            check_at_version(&readonly, &built, 1);
            drop(readonly);
            let db = fast().open::<Root>(&path).unwrap();
            check_at_version(&db, &built, 1);
            assert_eq!(fs::metadata(&path).unwrap().len() as usize, start);
            sectors += 1;
        }
        sector += SECTOR;
    }
    assert!(sectors > 40, "only {sectors} torn sectors tried");
}

#[test]
fn a_hole_before_whole_frames_ends_the_log() {
    for (name, options) in [("buffered", fast()), ("fsync", fsync())] {
        let built = build_as(options.clone(), 6, 3, 600);
        let path = built.target();
        let last = built.lens.len() - 1;
        for hole in 2..last {
            let (start, end) = (built.lens[hole - 1], built.lens[hole]);
            for every in [1, 2] {
                let mut image = built.image(1, built.lens[last]);
                let mut sector = start - start % SECTOR;
                while sector < end {
                    if (sector / SECTOR).is_multiple_of(every) {
                        let from = sector.max(start);
                        image[from..(sector + SECTOR).min(end)].fill(0);
                    }
                    sector += SECTOR;
                }
                fs::write(&path, &image).unwrap();
                let context = format!("{name}: hole {hole}, every {every}");
                let readonly = options.clone().read_only(true).open::<Root>(&path).unwrap();
                assert_eq!(readonly.version(), hole as u64 - 1, "{context}");
                drop(readonly);
                let db = options.clone().open::<Root>(&path).unwrap();
                check_at_version(&db, &built, hole - 1);
                assert_eq!(
                    fs::metadata(&path).unwrap().len() as usize,
                    start,
                    "{context}"
                );
            }
        }
    }
}

#[test]
fn recovery_refuses_frames_that_do_not_follow_the_tail() {
    let built = build(5, 3);
    let path = built.target();
    for commit in 0..built.lens.len() - 2 {
        let (start, next, after) = (
            built.lens[commit],
            built.lens[commit + 1],
            built.lens[commit + 2],
        );
        let mut skipped = built.image(commit, start);
        skipped.extend_from_slice(&built.bytes[next..after]);
        fs::write(&path, &skipped).unwrap();
        let db = fast().open::<Root>(&path).unwrap();
        check_at_version(&db, &built, commit);
        drop(db);
        let mut replayed = built.image(commit, next);
        replayed.extend_from_slice(&built.bytes[start..next]);
        fs::write(&path, &replayed).unwrap();
        let db = fast().open::<Root>(&path).unwrap();
        check_at_version(&db, &built, commit + 1);
        assert_eq!(fs::metadata(&path).unwrap().len() as usize, next);
    }
}

#[test]
fn recovery_refuses_a_checksummed_frame_with_the_wrong_sequence() {
    let built = build(4, 3);
    let path = built.target();
    let commit = 1;
    let (start, end) = (built.lens[commit], built.lens[commit + 1]);
    let mut image = built.image(commit, end);
    let forged = commit as u64 + 2;
    image[start + 8..start + 16].copy_from_slice(&forged.to_le_bytes());
    seal_frame(&mut image, start, end);
    let mut unforged = image.clone();
    unforged[start + 8..start + 16].copy_from_slice(&(commit as u64 + 1).to_le_bytes());
    seal_frame(&mut unforged, start, end);
    assert_eq!(
        unforged,
        built.image(commit, end),
        "seal rebuilt another frame"
    );
    fs::write(&path, &image).unwrap();
    let db = fast().open::<Root>(&path).unwrap();
    check_at_version(&db, &built, commit);
    assert_eq!(fs::metadata(&path).unwrap().len() as usize, start);
}

#[test]
fn zeroed_space_past_the_last_frame_ends_the_log() {
    let built = build(6, 3);
    let path = built.target();
    let last = built.lens.len() - 1;
    for base in [last - 2, last] {
        let mut image = built.image(base, built.lens[last]);
        image.resize(built.lens[last] + (1 << 18), 0);
        fs::write(&path, &image).unwrap();
        let readonly = fast().read_only(true).open::<Root>(&path).unwrap();
        check_at_version(&readonly, &built, last);
        drop(readonly);
        assert_eq!(fs::metadata(&path).unwrap().len() as usize, image.len());
        let db = fast().open::<Root>(&path).unwrap();
        check_at_version(&db, &built, last);
        assert_eq!(
            fs::metadata(&path).unwrap().len() as usize,
            built.lens[last]
        );
    }
}

#[test]
fn corrupt_superblocks_fall_back_or_report() {
    let built = build(5, 3);
    let path = built.target();
    let last = built.lens.len() - 1;
    let newest = Built::newest_slot(last);
    let older = Built::newest_slot(last - 1);
    for byte in 0..SUPER {
        for bit in [0x01u8, 0x80] {
            let mut image = built.bytes.clone();
            image[newest + byte] ^= bit;
            fs::write(&path, &image).unwrap();
            let db = fast().read_only(true).open::<Root>(&path).unwrap();
            check_at_version(&db, &built, last);
            drop(db);

            let (start, end) = (built.lens[last - 1], built.lens[last]);
            image[start] ^= bit;
            fs::write(&path, &image).unwrap();
            let opened = fast().read_only(true).open::<Root>(&path);
            if judged(&image, start, start, end) {
                assert_damaged(opened, start, start, "a flipped magic past the older slot");
                assert_eq!(fs::read(&path).unwrap(), image);
            } else {
                check_at_version(&opened.unwrap(), &built, last - 1);
            }

            let mut image = built.bytes.clone();
            image[older + byte] ^= bit;
            fs::write(&path, &image).unwrap();
            let db = fast().read_only(true).open::<Root>(&path).unwrap();
            check_at_version(&db, &built, last);
            drop(db);

            let mut image = built.bytes.clone();
            image[newest + byte] ^= bit;
            image[older + byte] ^= bit;
            fs::write(&path, &image).unwrap();
            assert_both_slots_damaged(&path, byte >= MAGIC, &format!("byte {byte}"));
            assert_eq!(fs::read(&path).unwrap(), image);
        }
    }
    for (magic, check) in [(0, SLOT + SUPER - 1), (SLOT, SUPER - 1)] {
        let mut image = built.bytes.clone();
        image[magic] ^= 0x01;
        image[check] ^= 0x01;
        fs::write(&path, &image).unwrap();
        assert_both_slots_damaged(&path, true, &format!("magic at {magic}"));
        assert_eq!(fs::read(&path).unwrap(), image);
    }
    for byte in [SUPER, SUPER + 1, SLOT - 1, SLOT + SUPER, 2 * SLOT - 1] {
        let mut image = built.bytes.clone();
        image[byte] ^= 0xFF;
        fs::write(&path, &image).unwrap();
        let db = fast().open::<Root>(&path).unwrap();
        check_at_version(&db, &built, last);
    }
}

#[test]
fn valid_superblock_over_missing_log_reports_an_error() {
    let built = build(5, 3);
    let path = built.target();
    let last = built.lens.len() - 1;
    let mut image = built.bytes.clone();
    for byte in &mut image[built.lens[last - 1]..] {
        *byte = 0;
    }
    fs::write(&path, &image).unwrap();
    assert_clear_error(fast().open::<Root>(&path), "log hole");
}

#[test]
fn blank_file_from_a_crash_during_creation_opens_as_new() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blank.ferro");
    for len in [0usize, 1, 95, 96, 4096, LOG_START] {
        fs::write(&path, vec![0u8; len]).unwrap();
        let db = fast().open::<Root>(&path).unwrap();
        assert_eq!(db.version(), 0);
        db.update(|root| {
            root.insert(1, "x".into());
        })
        .unwrap();
        drop(db);
        let db = fast().open::<Root>(&path).unwrap();
        assert_eq!(db.version(), 1);
    }
    fs::write(&path, vec![0u8; LOG_START + 1]).unwrap();
    assert_clear_error(fast().open::<Root>(&path), "blank header with a log");
}

#[test]
fn foreign_files_are_rejected_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("other.txt");
    for body in [
        b"hello world".to_vec(),
        vec![7u8; 100],
        vec![7u8; 20_000],
        b"FERRODB3".to_vec(),
    ] {
        fs::write(&path, &body).unwrap();
        assert_clear_error(fast().open::<Root>(&path), "foreign file");
        assert_clear_error(fast().read_only(true).open::<Root>(&path), "foreign file");
        assert_eq!(fs::read(&path).unwrap(), body);
    }
}

#[test]
fn truncated_database_is_never_reinitialised() {
    let built = build(3, 3);
    let path = built.target();
    for end in 8..LOG_START {
        fs::write(&path, &built.bytes[..end]).unwrap();
        assert_clear_error(fast().open::<Root>(&path), "short database");
        assert_eq!(fs::read(&path).unwrap(), &built.bytes[..end]);
    }
    let mut torn = built.bytes[..4096].to_vec();
    torn[20] ^= 0xFF;
    fs::write(&path, &torn).unwrap();
    assert_clear_error(fast().open::<Root>(&path), "torn short database");
    assert_eq!(fs::read(&path).unwrap(), torn);
}

fn reachable(path: &Path) -> Vec<(usize, usize, Vec<Ptr>)> {
    let options = StoreOptions {
        read_only: true,
        ..StoreOptions::default()
    };
    let (store, head) = Store::open(path, options).unwrap();
    let mut out = Vec::new();
    store
        .walk(head.unwrap(), |ptr, object| {
            let start = ptr.offset as usize;
            let end = start + record_len(ptr.offset, object);
            out.push((start, end, object.links.clone()));
        })
        .unwrap();
    out.sort_by_key(|entry| entry.0);
    out
}

fn record_varint(bytes: &[u8], at: &mut usize) -> u64 {
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

fn compressed_bodies(bytes: &[u8], ranges: &[(usize, usize, Vec<Ptr>)]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    for (start, _, _) in ranges {
        let mut at = *start;
        let stored = record_varint(bytes, &mut at) as usize;
        if record_varint(bytes, &mut at) & 1 == 1 {
            record_varint(bytes, &mut at);
            out.push(at..at + stored);
        }
    }
    out
}

enum Reads {
    Correct,
    Wrong(String),
}

fn read_everything(db: &Db<Root>, built: &Built) -> ferro::Result<Reads> {
    let last = built.states.len() - 1;
    if db.version() != last as u64 {
        return Ok(Reads::Wrong(format!("opened at v{}", db.version())));
    }
    for version in (0..=last).rev() {
        let snapshot = db.at(version as u64)?;
        let model = &built.states[version];
        if !matches_model(&snapshot, model) {
            return Ok(Reads::Wrong(format!("iter of v{version}")));
        }
        let reversed: Vec<u64> = snapshot.keys().rev().collect();
        if !reversed.iter().copied().eq(model.keys().rev().copied()) {
            return Ok(Reads::Wrong(format!("rev of v{version}")));
        }
        for (key, value) in model {
            if snapshot.try_get(key)?.as_deref() != Some(value) {
                return Ok(Reads::Wrong(format!("get {key} of v{version}")));
            }
        }
        if snapshot.nth(model.len() / 2).map(|p| p.0) != model.keys().nth(model.len() / 2).copied()
        {
            return Ok(Reads::Wrong(format!("nth of v{version}")));
        }
        let mut edited = (*snapshot).clone();
        for key in model.keys().take(3) {
            edited.remove(key);
        }
        if edited.len() != model.len().saturating_sub(3) {
            return Ok(Reads::Wrong(format!("remove of v{version}")));
        }
        if version > 0 {
            let before = db.at(version as u64 - 1)?;
            let changes = before.diff(&snapshot).count();
            let wanted = built.states[version - 1]
                .iter()
                .filter(|(k, v)| model.get(*k) != Some(*v))
                .count()
                + model
                    .keys()
                    .filter(|k| !built.states[version - 1].contains_key(*k))
                    .count();
            if changes != wanted {
                return Ok(Reads::Wrong(format!("diff at v{version}")));
            }
        }
    }
    Ok(Reads::Correct)
}

#[test]
fn flipped_log_bytes_never_read_back_as_wrong_data() {
    quiet_ferro_panics();
    let built = build(40, 6);
    flip_log_bytes(&built, 0xFACE);
}

#[test]
fn flipped_bytes_of_compressed_bodies_never_read_back_as_wrong_data() {
    quiet_ferro_panics();
    let built = build_from(fast(), clock_start(), 40, 6, 600, repetitive);
    let compressed = flip_log_bytes(&built, 0xFADE);
    assert!(compressed > 20, "only {compressed} bodies compressed");
}

fn flip_log_bytes(built: &Built, seed: u64) -> usize {
    let path = built.target();
    fs::write(&path, &built.bytes).unwrap();
    let ranges = reachable(&path);
    let compressed = compressed_bodies(&built.bytes, &ranges);
    let mut rng = Rng::new(seed);
    let mut detected = 0u64;
    let mut dead = 0u64;
    for trial in 0..cases(4000) {
        let position = LOG_START + rng.below((built.bytes.len() - LOG_START) as u64) as usize;
        let mask = if rng.chance(50) {
            1u8 << rng.below(8)
        } else {
            1 + rng.below(255) as u8
        };
        let mut image = built.bytes.clone();
        image[position] ^= mask;
        fs::write(&path, &image).unwrap();
        let live = ranges
            .iter()
            .any(|(start, end, _)| (*start..*end).contains(&position));
        let options = if trial % 2 == 0 { uncached() } else { fast() };
        let context = format!(
            "FERRO_TEST_CLOCK={} byte {position} mask {mask:#x} live {live}",
            built.clock
        );
        match guarded(|| {
            let db = match options.open::<Root>(&path) {
                Ok(db) => db,
                Err(error) => return Some(error.to_string()),
            };
            match read_everything(&db, built) {
                Ok(Reads::Correct) => {}
                Ok(Reads::Wrong(what)) => panic!("ferro: WRONG DATA {what}"),
                Err(error) => return Some(error.to_string()),
            }
            db.verify().err().map(|error| error.to_string())
        }) {
            Outcome::Value(Some(_)) => {
                assert!(live, "{context}: reported damage in dead bytes");
                detected += 1;
            }
            Outcome::Value(None) => {
                let recoded = compressed.iter().any(|body| body.contains(&position));
                assert!(
                    !live || recoded,
                    "{context}: reachable damage went unnoticed"
                );
                dead += u64::from(!live);
            }
            Outcome::FerroPanic(message) => {
                assert!(!message.contains("WRONG DATA"), "{context}: {message}");
                detected += 1;
            }
        }
    }
    assert!(detected > 100, "only {detected} flips detected");
    assert!(dead > 0, "no flip landed in unreachable bytes");
    compressed.len()
}

#[test]
fn a_record_read_on_into_the_byte_after_it_is_reported() {
    let built = build(5, 3);
    let path = built.target();
    fs::write(&path, &built.bytes).unwrap();
    let ranges = reachable(&path);
    let live = |at: usize| {
        ranges
            .iter()
            .any(|(start, end, _)| (*start..*end).contains(&at))
    };
    let opens_whole = |image: &[u8]| {
        fs::write(&path, image).unwrap();
        fast()
            .open::<Root>(&path)
            .and_then(|db| db.verify().map(|_| ()))
    };
    let mut checked = 0;
    for (start, end, links) in &ranges {
        if links.is_empty() || *end == built.bytes.len() || live(*end) {
            continue;
        }
        let mut image = built.bytes.clone();
        image[*end] = 0;
        if opens_whole(&image).is_err() {
            continue;
        }
        image[end - 1] ^= 0x80;
        assert!(
            opens_whole(&image).is_err(),
            "FERRO_TEST_CLOCK={} record {start}..{end}: an overlong last link was read",
            built.clock
        );
        checked += 1;
    }
    assert!(checked > 0, "no record ends before a dead byte");
}

#[test]
fn every_live_byte_flip_is_reported_by_verify() {
    quiet_ferro_panics();
    let built = build(4, 3);
    let path = built.target();
    fs::write(&path, &built.bytes).unwrap();
    let ranges = reachable(&path);
    for (start, end, _) in &ranges {
        for position in *start..*end {
            let mut image = built.bytes.clone();
            image[position] ^= 0x41;
            fs::write(&path, &image).unwrap();
            let opened = fast().open::<Root>(&path);
            let report = match opened {
                Ok(db) => db.verify().map(|_| ()),
                Err(error) => Err(error),
            };
            assert!(report.is_err(), "flip at {position} passed verify");
        }
    }
}

fn wide_database() -> (TempDir, PathBuf, u64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wide.ferro");
    let db = fast().open::<Root>(&path).unwrap();
    db.update(|root| {
        for key in 0..20_000 {
            root.insert(key, format!("value-{key}"));
        }
    })
    .unwrap();
    drop(db);
    (dir, path, 20_000)
}

fn zigzag_len(offset: u64, target: u64) -> usize {
    let delta = offset.wrapping_sub(target) as i64;
    let mut value = ((delta << 1) ^ (delta >> 63)) as u64;
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

fn zigzag_of_len(offset: u64, target: u64, len: usize) -> Option<Vec<u8>> {
    if zigzag_len(offset, target) != len {
        return None;
    }
    let delta = offset.wrapping_sub(target) as i64;
    let mut value = ((delta << 1) ^ (delta >> 63)) as u64;
    let mut out = Vec::new();
    for position in 0..len {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if position + 1 < len {
            byte |= 0x80;
        }
        out.push(byte);
    }
    Some(out)
}

fn redirect_sibling_link(path: &Path) -> usize {
    let ranges = reachable(path);
    let (start, end, links) = ranges
        .iter()
        .max_by_key(|entry| entry.2.len())
        .expect("database has objects");
    assert!(links.len() > 3);
    let lens: Vec<usize> = links
        .iter()
        .map(|link| HASH_BYTES + zigzag_len(*start as u64, link.offset))
        .collect();
    let mut image = fs::read(path).unwrap();
    let mut position = end - lens.iter().sum::<usize>();
    for index in 0..links.len() - 1 {
        let delta_len = lens[index] - HASH_BYTES;
        if let Some(patch) = zigzag_of_len(*start as u64, links[index + 1].offset, delta_len)
            && links[index + 1].offset != links[index].offset
        {
            let delta_at = position + HASH_BYTES;
            image[delta_at..delta_at + delta_len].copy_from_slice(&patch);
            fs::write(path, image).unwrap();
            return index;
        }
        position += lens[index];
    }
    panic!("no link could be redirected in place");
}

#[test]
fn redirected_link_is_caught_even_when_the_wrong_target_is_cached() {
    quiet_ferro_panics();
    let (_dir, path, keys) = wide_database();
    redirect_sibling_link(&path);
    let db = fast().cache_bytes(64 << 20).open::<Root>(&path).unwrap();
    let snapshot = db.read();
    let order: Vec<u64> = (0..keys).rev().collect();
    let mut failures = 0;
    for key in order {
        match guarded(|| snapshot.get(&key)) {
            Outcome::Value(Some(value)) => assert_eq!(*value, format!("value-{key}")),
            Outcome::Value(None) => panic!("key {key} silently vanished"),
            Outcome::FerroPanic(_) => failures += 1,
        }
    }
    assert!(failures > 0, "the redirected link was never followed");
}

#[test]
fn redirected_link_stays_caught_once_child_memos_are_warm() {
    quiet_ferro_panics();
    let (_dir, path, keys) = wide_database();
    redirect_sibling_link(&path);
    let db = fast().cache_bytes(64 << 20).open::<Root>(&path).unwrap();
    let snapshot = db.read();
    let mut failed = None;
    for _ in 0..3 {
        let mut failures = Vec::new();
        for key in 0..keys {
            match guarded(|| snapshot.get(&key)) {
                Outcome::Value(Some(value)) => assert_eq!(*value, format!("value-{key}")),
                Outcome::Value(None) => panic!("key {key} silently vanished"),
                Outcome::FerroPanic(_) => failures.push(key),
            }
        }
        assert!(
            !failures.is_empty(),
            "the redirected link was never followed"
        );
        assert_eq!(*failed.get_or_insert_with(|| failures.clone()), failures);
    }
}

#[test]
fn a_link_redirected_to_a_sibling_is_reported_by_verify() {
    let built = build(5, 3);
    let path = built.target();
    fs::write(&path, &built.bytes).unwrap();
    let ranges = reachable(&path);
    let mut redirected = 0;
    for (start, end, links) in &ranges {
        let lens: Vec<usize> = links
            .iter()
            .map(|link| HASH_BYTES + zigzag_len(*start as u64, link.offset))
            .collect();
        let mut at = end - lens.iter().sum::<usize>();
        for (index, link) in links.iter().enumerate() {
            let delta_len = lens[index] - HASH_BYTES;
            let delta_at = at + HASH_BYTES;
            at += lens[index];
            for other in links.iter().filter(|other| other.hash != link.hash) {
                let Some(patch) = zigzag_of_len(*start as u64, other.offset, delta_len) else {
                    continue;
                };
                let mut image = built.bytes.clone();
                image[delta_at..delta_at + delta_len].copy_from_slice(&patch);
                fs::write(&path, &image).unwrap();
                let report = fast()
                    .open::<Root>(&path)
                    .and_then(|db| db.verify().map(|_| ()));
                assert!(
                    report.is_err(),
                    "FERRO_TEST_CLOCK={} record {start}..{end}: link {index} named {} and passed",
                    built.clock,
                    other.offset
                );
                redirected += 1;
            }
        }
    }
    assert!(redirected > 10, "only {redirected} links redirected");
}

#[test]
fn verify_reports_a_link_redirected_to_a_visited_object() {
    let (_dir, path, _) = wide_database();
    redirect_sibling_link(&path);
    let db = uncached().open::<Root>(&path).unwrap();
    assert!(db.verify().is_err());
}

#[test]
fn failed_open_of_a_damaged_head_releases_the_lock() {
    let built = build(3, 3);
    let path = built.target();
    let last = built.lens.len() - 1;
    let mut image = built.bytes.clone();
    let tail = built.lens[last] - 1 - table_bytes(built.lens[last] - built.lens[last - 1]);
    image[tail] ^= 0xFF;
    fs::write(&path, &image).unwrap();
    for _ in 0..3 {
        match fast().open::<Root>(&path) {
            Err(Error::Locked) => panic!("failed open leaked the lock"),
            Err(_) => {}
            Ok(db) => {
                assert!(db.verify().is_err());
            }
        }
    }
}

#[test]
fn oversized_object_is_refused_before_it_reaches_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.ferro");
    let (store, _) = Store::open(&path, StoreOptions::default()).unwrap();
    let mut txn = store.begin().unwrap();
    let object = Object {
        body: vec![0u8; (1usize << 32) + 1],
        links: Vec::new(),
    };
    assert!(matches!(
        txn.put(object, [0; HASH_BYTES]),
        Err(Error::TooLarge(_))
    ));
}

#[test]
fn wrong_root_type_reports_or_panics_cleanly() {
    quiet_ferro_panics();
    let built = build(3, 3);
    let path = built.target();
    fs::write(&path, &built.bytes).unwrap();
    let outcome = guarded(|| {
        fast()
            .open::<Map<String, u64>>(&path)
            .map(|db| db.read().iter().count())
    });
    match outcome {
        Outcome::Value(Ok(_)) => panic!("wrong type read cleanly"),
        Outcome::Value(Err(_)) | Outcome::FerroPanic(_) => {}
    }
}
