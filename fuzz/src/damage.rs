use std::fs;
use std::path::Path;

use ferro::{Db, Durability, Error, Map, Options, Reclaim};

use crate::fixup::{BLOCK_BYTES, BLOCK_CHECK, FRAME_BYTES, FRAME_MAGIC, LOG_START, SLOT_BYTES};
use crate::input::Input;
use crate::{fresh, scratch};

type Root = Map<u64, String>;

const SECTOR: usize = 512;
const SECTOR_MIN: usize = 8;
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

struct Built {
    bytes: Vec<u8>,
    ends: Vec<usize>,
    headers: Vec<Vec<u8>>,
}

fn build(input: &mut Input, path: &Path, fsync: bool) -> Option<Built> {
    let db = options(fsync).open::<Root>(path).ok()?;
    let commits = 1 + input.pick(5);
    let mut headers = vec![fs::read(path).ok()?[..LOG_START].to_vec()];
    for commit in 0..commits {
        let keys = 1 + input.pick(4);
        let mut edits = Vec::with_capacity(keys);
        for _ in 0..keys {
            let key = input.num() % 16;
            let len = (input.num().wrapping_mul(97) % LONGEST) as usize;
            edits.push((key, text(input.num() ^ commit as u64, len)));
        }
        db.commit(format!("c{commit}"), |root| {
            for (key, value) in &edits {
                root.insert(*key, value.clone());
            }
        })
        .ok()?;
        headers.push(fs::read(path).ok()?[..LOG_START].to_vec());
    }
    drop(db);
    let bytes = fs::read(path).ok()?;
    let mut ends = vec![LOG_START];
    while let Some(end) = whole_frame(&bytes, *ends.last()?) {
        ends.push(end);
    }
    (ends.len() == commits + 1).then_some(Built {
        bytes,
        ends,
        headers,
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

fn anchor(image: &[u8]) -> Option<usize> {
    let mut newest = None;
    for slot in [0, SLOT_BYTES] {
        let header = &image[slot..slot + SUPER_BYTES];
        if &header[..8] != b"FERRODB4"
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

    fn judged(&self, at: usize) -> bool {
        let sector = at - at % SECTOR;
        (sector + SECTOR).min(self.end) - sector.max(self.start) >= SECTOR_MIN
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
    if bits.iter().any(|count| *count > 1)
        || entry_and_block
        || !flipped.iter().all(|at| frame.judged(*at))
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
    let fsync = input.chance(8);
    let source = fresh("source.ferro");
    let Some(built) = build(&mut input, &source, fsync) else {
        return;
    };
    let commits = built.ends.len() - 1;
    let base = input.pick(commits);
    let mut image = built.headers[base].clone();
    let Some(anchor) = anchor(&image) else {
        return;
    };
    let last = base + 1 + input.pick(commits - base);
    let past: Vec<usize> = (1..=last)
        .filter(|commit| built.ends[commit - 1] >= anchor)
        .collect();
    image.extend_from_slice(&built.bytes[LOG_START..built.ends[last]]);
    let whole = image.clone();
    let target = (!past.is_empty()).then(|| past[input.pick(past.len())]);
    let chosen = match target {
        Some(commit) => {
            let frame = Frame::new(built.ends[commit - 1], built.ends[commit]);
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
            Verdict::Whole => (last, None),
            Verdict::Torn => (commit - 1, Some(frame.start)),
            Verdict::Damaged(byte) => {
                expect_damaged(read_only(), frame, byte, "read-only");
                assert_eq!(fs::read(&path).unwrap(), image, "a read-only open wrote");
                expect_damaged(opened(), frame, byte, "writer");
                assert_eq!(fs::read(&path).unwrap(), image, "a writer's open wrote");
                return;
            }
        },
        None => (last, None),
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
