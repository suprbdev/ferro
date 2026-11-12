use std::fs;
use std::path::Path;

use ferro::{Db, Durability, Error, Map, Options, Reclaim, Snapshot};

use crate::fixup::{
    BLOCK_BYTES, BLOCK_CHECK, FRAME_BYTES, FRAME_MAGIC, LOG_START, SLOT_BYTES, STAGING_MAGIC,
};
use crate::input::Input;
use crate::{fresh, scratch};

type Root = Map<u64, String>;

const SECTOR: usize = 512;
const SECTOR_MIN: usize = 8;
const MEND_BLOCKS: usize = 8;
const RAW: usize = 400_000;
const WINDOW: usize = 1024;
const PULLED: usize = 3;
const SUPER_CHECKED: usize = 112;
const SUPER_BYTES: usize = 144;
const ANCHOR_END: usize = 76;
const LONGEST: u64 = 14_000;

fn field(bytes: &[u8], at: usize) -> usize {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize
}

fn options(fsync: bool) -> Options {
    let durability = match fsync {
        true => Durability::Fsync,
        false => Durability::Buffered,
    };
    Options::new()
        .durability(durability)
        .cache_bytes(1 << 20)
        .reclaim(Reclaim { every_bytes: 0 })
}

fn text(seed: u64, len: usize) -> String {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'0' + (state % 75) as u8)
        })
        .collect()
}

fn printable(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            b' ' + (state % 95) as u8
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Recipe {
    Text,
    Sparse,
    Spill,
}

impl Recipe {
    fn of(byte: u8) -> Recipe {
        match byte >> 4 {
            12 | 13 => Recipe::Sparse,
            14 | 15 => Recipe::Spill,
            _ => Recipe::Text,
        }
    }
}

fn sparse(input: &mut Input, seed: u64) -> String {
    let mut bytes = match input.pick(3) {
        0 => {
            let mut bytes = printable(seed, RAW);
            let at = RAW / 2 - seed as usize % 4096;
            bytes[at..at + WINDOW].fill(0);
            bytes[at + WINDOW / 2] = 1 << (seed % 7);
            bytes
        }
        1 => vec![0u8; (input.num().wrapping_mul(97) % (64 << 10)) as usize],
        _ => vec![0u8; input.pick(96)],
    };
    for _ in 0..input.pick(4) {
        if !bytes.is_empty() {
            let at = input.num() as usize % bytes.len();
            bytes[at] ^= 1 << (input.byte() % 7);
        }
    }
    String::from_utf8(bytes).unwrap()
}

fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    panic!("no merge expected")
}

struct Built {
    bytes: Vec<u8>,
    starts: Vec<usize>,
    ends: Vec<usize>,
    steps: Vec<(Vec<u8>, usize)>,
    versions: Vec<usize>,
}

fn frames(bytes: &[u8]) -> Option<Vec<(usize, usize)>> {
    let mut frames = vec![(LOG_START, LOG_START)];
    let mut at = frames.last()?.1;
    loop {
        if let Some(end) = whole_frame(bytes, at) {
            frames.push((at, end));
            at = end;
        } else if let Some(end) = staging_frame(bytes, at) {
            at = end;
        } else {
            return Some(frames);
        }
    }
}

fn build(input: &mut Input, path: &Path, fsync: bool, recipe: Recipe) -> Option<Built> {
    let db = options(fsync).open::<Root>(path).ok()?;
    let mut steps = vec![(fs::read(path).ok()?[..LOG_START].to_vec(), 0)];
    if recipe == Recipe::Spill {
        let sender = scratch().join("sender.ferro");
        let other = options(fsync).open::<Root>(&sender).ok()?;
        let seed = input.num();
        other
            .commit("pulled", |root| {
                for key in 0..PULLED as u64 {
                    let value = printable(seed ^ key << 40, RAW);
                    root.insert(key, String::from_utf8(value).unwrap());
                }
            })
            .ok()?;
        db.pull(&other, "sender", refuse).ok()?;
        drop(other);
        let bytes = fs::read(path).ok()?;
        steps.push((bytes[..LOG_START].to_vec(), frames(&bytes)?.len() - 1));
    }
    let commits = 1 + input.pick(5);
    for commit in 0..commits {
        let keys = match recipe {
            Recipe::Sparse => 1 + input.pick(2),
            _ => 1 + input.pick(4),
        };
        let mut edits = Vec::with_capacity(keys);
        for _ in 0..keys {
            let key = input.num() % 16;
            let value = match recipe {
                Recipe::Sparse => {
                    let seed = input.num() ^ (commit as u64) << 32;
                    sparse(input, seed)
                }
                _ => {
                    let len = (input.num().wrapping_mul(97) % LONGEST) as usize;
                    text(input.num() ^ commit as u64, len)
                }
            };
            edits.push((key, value));
        }
        db.commit(format!("c{commit}"), |root| {
            for (key, value) in &edits {
                root.insert(*key, value.clone());
            }
        })
        .ok()?;
        let bytes = fs::read(path).ok()?;
        steps.push((bytes[..LOG_START].to_vec(), frames(&bytes)?.len() - 1));
    }
    drop(db);
    let bytes = fs::read(path).ok()?;
    let (starts, ends): (Vec<usize>, Vec<usize>) = frames(&bytes)?.into_iter().unzip();
    let counts: Vec<usize> = steps.iter().map(|(_, count)| *count).collect();
    if counts.windows(2).any(|pair| pair[0] >= pair[1]) || counts.last() != Some(&(ends.len() - 1))
    {
        return None;
    }
    let mut versions = vec![0];
    let prefix = scratch().join("prefix.ferro");
    for end in &ends[1..] {
        let mut image = steps[0].0.clone();
        image.extend_from_slice(&bytes[LOG_START..*end]);
        fs::write(&prefix, &image).ok()?;
        let db = options(fsync).read_only(true).open::<Root>(&prefix).ok()?;
        versions.push(db.version() as usize);
    }
    Some(Built {
        bytes,
        starts,
        ends,
        steps,
        versions,
    })
}

fn whole_frame(bytes: &[u8], start: usize) -> Option<usize> {
    if start + FRAME_BYTES > bytes.len() || &bytes[start..start + 8] != FRAME_MAGIC {
        return None;
    }
    let end = field(bytes, start + 16);
    if end < start + FRAME_BYTES || end > bytes.len() {
        return None;
    }
    let mut sealed = bytes[..end].to_vec();
    crate::fixup::seal_frame(&mut sealed, start, end);
    (sealed[start..end] == bytes[start..end]).then_some(end)
}

fn staging_frame(bytes: &[u8], start: usize) -> Option<usize> {
    if start + FRAME_BYTES > bytes.len() || &bytes[start..start + 8] != STAGING_MAGIC {
        return None;
    }
    let end = field(bytes, start + 16);
    if end < start + FRAME_BYTES || end > bytes.len() {
        return None;
    }
    let mut header = bytes[start..start + FRAME_BYTES].to_vec();
    crate::fixup::seal_staging(&mut header, 0);
    (header[..] == bytes[start..start + FRAME_BYTES]).then_some(end)
}

fn anchor(image: &[u8]) -> Option<usize> {
    let mut newest = None;
    for slot in [0, SLOT_BYTES] {
        let header = &image[slot..slot + SUPER_BYTES];
        if &header[..8] != b"FERRODB7"
            || blake3::hash(&header[..SUPER_CHECKED]).as_bytes()[..] != header[SUPER_CHECKED..]
        {
            continue;
        }
        let key = (field(header, 8), field(header, 68));
        if newest.is_none_or(|(best, _)| key > best) {
            newest = Some((key, field(header, ANCHOR_END)));
        }
    }
    newest.map(|(_, at)| at)
}

struct Frame {
    start: usize,
    end: usize,
    body: usize,
    table: usize,
}

impl Frame {
    fn new(start: usize, end: usize) -> Frame {
        let body = start + FRAME_BYTES;
        let table = end - BLOCK_CHECK * (end - body).div_ceil(BLOCK_BYTES + BLOCK_CHECK);
        Frame {
            start,
            end,
            body,
            table,
        }
    }

    fn regions(&self) -> usize {
        1 + (self.table - self.body).div_ceil(BLOCK_BYTES)
    }

    fn region_of(&self, at: usize) -> usize {
        match at {
            at if at < self.body || at >= self.table => 0,
            at => 1 + (at - self.body) / BLOCK_BYTES,
        }
    }

    fn entry_of(&self, at: usize) -> Option<usize> {
        (at >= self.table).then(|| 1 + (at - self.table) / BLOCK_CHECK)
    }

    fn pick_in(&self, region: usize, input: &mut Input) -> usize {
        let spot = input.num() as usize;
        match region {
            0 => {
                let header = FRAME_BYTES;
                let span = header + self.end - self.table;
                match spot % span {
                    at if at < header => self.start + at,
                    at => self.table + at - header,
                }
            }
            block => {
                let from = self.body + (block - 1) * BLOCK_BYTES;
                let to = (from + BLOCK_BYTES).min(self.table);
                from + spot % (to - from)
            }
        }
    }

    fn judged(&self, after: &[u8], at: usize) -> bool {
        let sector = at - at % SECTOR;
        let (from, to) = (sector.max(self.start), (sector + SECTOR).min(self.end));
        let blank =
            |bytes: &[u8]| bytes.iter().all(|b| *b == 0) || bytes.iter().all(|b| *b == 0xff);
        to - from >= SECTOR_MIN && !blank(&after[from..=at]) && !blank(&after[at..to])
    }
}

fn damage(image: &mut [u8], frame: &Frame, input: &mut Input) {
    let len = frame.end - frame.start;
    for _ in 0..1 + input.pick(3) {
        match input.byte() % 5 {
            0 => {
                let at = frame.start + input.num() as usize % len;
                image[at] ^= 1 << (input.byte() % 8);
            }
            1 | 2 => {
                for region in 0..frame.regions() {
                    if input.chance(10) {
                        let at = frame.pick_in(region, input);
                        image[at] ^= 1 << (input.byte() % 8);
                    }
                }
            }
            kind => {
                let first = frame.start - frame.start % SECTOR;
                let sectors = (frame.end - first).div_ceil(SECTOR);
                let sector = first + input.pick(sectors) * SECTOR;
                let run = match kind {
                    3 => 1,
                    _ => 1 + input.pick(8),
                };
                let fill = input.byte() % 4;
                let room = frame.start - LOG_START;
                let older = LOG_START + input.num() as usize % room.saturating_sub(SECTOR).max(1);
                for index in 0..run {
                    let from = (sector + index * 2 * SECTOR).max(frame.start);
                    let to = (sector + index * 2 * SECTOR + SECTOR).min(frame.end);
                    for at in from..to {
                        image[at] = match fill {
                            0 => 0,
                            1 => 0xff,
                            2 if room >= SECTOR => image[older + at - from],
                            2 => 0,
                            _ => input.byte(),
                        };
                    }
                }
            }
        }
    }
}

enum Verdict {
    Whole,
    Torn,
    Damaged(usize),
}

fn verdict(before: &[u8], after: &[u8], frame: &Frame) -> Verdict {
    let mut bits = vec![0u32; frame.regions()];
    let mut flipped = Vec::new();
    for at in frame.start..frame.end {
        let differ = (before[at] ^ after[at]).count_ones();
        if differ > 0 {
            bits[frame.region_of(at)] += differ;
            flipped.push(at);
        }
    }
    if flipped.is_empty() {
        return Verdict::Whole;
    }
    let entry_and_block = flipped
        .iter()
        .filter_map(|at| frame.entry_of(*at))
        .any(|block| bits[block] > 0);
    let failing = bits.iter().skip(1).filter(|count| **count > 0).count();
    if bits.iter().any(|count| *count > 1)
        || entry_and_block
        || failing > MEND_BLOCKS
        || !flipped.iter().all(|at| frame.judged(after, *at))
    {
        return Verdict::Torn;
    }
    Verdict::Damaged(flipped[0])
}

fn expect_damaged<T>(result: ferro::Result<T>, frame: &Frame, byte: usize, what: &str) {
    match result {
        Err(Error::Corrupt(message))
            if message.contains(&format!(" at {} ", frame.start))
                && message.ends_with(&format!("byte {byte}")) => {}
        Err(other) => panic!("{what}: a frame damaged at byte {byte} gave {other}"),
        Ok(_) => panic!("{what}: a frame damaged at byte {byte} opened"),
    }
}

fn expect_version(result: ferro::Result<Db<Root>>, version: usize, what: &str) -> Db<Root> {
    match result {
        Ok(db) if db.version() == version as u64 => db,
        Ok(db) => panic!("{what}: opened at version {}, not {version}", db.version()),
        Err(error) => panic!("{what}: failed to open at version {version}: {error}"),
    }
}

pub fn run(data: &[u8]) {
    let mut input = Input::new(data);
    let first = input.byte();
    let fsync = first % 16 < 8;
    let source = fresh("source.ferro");
    let Some(built) = build(&mut input, &source, fsync, Recipe::of(first)) else {
        return;
    };
    let frames = built.ends.len() - 1;
    let (header, named) = &built.steps[input.pick(built.steps.len() - 1)];
    let mut image = header.clone();
    let Some(anchor) = anchor(&image) else {
        return;
    };
    let last = named + 1 + input.pick(frames - named);
    let past: Vec<usize> = (1..=last)
        .filter(|commit| built.starts[*commit] >= anchor)
        .collect();
    image.extend_from_slice(&built.bytes[LOG_START..built.ends[last]]);
    let whole = image.clone();
    let target = (!past.is_empty()).then(|| past[input.pick(past.len())]);
    let chosen = match target {
        Some(commit) => {
            let frame = Frame::new(built.starts[commit], built.ends[commit]);
            damage(&mut image, &frame, &mut input);
            Some((commit, frame))
        }
        None => None,
    };
    if input.chance(4) {
        image.resize(image.len() + SECTOR * (1 + input.pick(16)), 0);
    }
    let path = scratch().join("damaged.ferro");
    fs::write(&path, &image).unwrap();
    let opened = || options(fsync).open::<Root>(&path);
    let read_only = || options(fsync).read_only(true).open::<Root>(&path);
    let (version, start) = match &chosen {
        Some((commit, frame)) => match verdict(&whole, &image, frame) {
            Verdict::Whole => (built.versions[last], None),
            Verdict::Torn => (built.versions[commit - 1], Some(built.ends[commit - 1])),
            Verdict::Damaged(byte) => {
                expect_damaged(read_only(), frame, byte, "read-only");
                assert_eq!(fs::read(&path).unwrap(), image, "a read-only open wrote");
                expect_damaged(opened(), frame, byte, "writer");
                assert_eq!(fs::read(&path).unwrap(), image, "a writer's open wrote");
                return;
            }
        },
        None => (built.versions[last], None),
    };
    drop(expect_version(read_only(), version, "read-only"));
    assert_eq!(fs::read(&path).unwrap(), image, "a read-only open wrote");
    let db = expect_version(opened(), version, "writer");
    if let Some(start) = start {
        assert_eq!(fs::metadata(&path).unwrap().len() as usize, start);
    }
    assert!(db.verify().is_ok(), "the kept log fails verify");
    db.commit("after", |root| {
        root.insert(99, "after".into());
    })
    .unwrap();
    drop(db);
    let db = expect_version(opened(), version + 1, "reopen");
    assert!(db.verify().is_ok(), "the log after a commit fails verify");
}
