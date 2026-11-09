#![allow(dead_code)]

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Once;

use ferro::codec::{Encoder, HashSink};
use ferro::{Data, Durability, Options};

pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

pub const SLOT: usize = 4096;
pub const FRAME_CHECKED: usize = 52;
pub const FRAME_BYTES: usize = FRAME_CHECKED + 16;
pub const BLOCK: usize = 4096;
pub const SECTOR: usize = 512;

pub fn field(bytes: &[u8], at: usize) -> usize {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize
}

pub fn table_bytes(frame: usize) -> usize {
    8 * (frame - FRAME_BYTES).div_ceil(BLOCK + 8)
}

pub fn judged(image: &[u8], at: usize, start: usize, end: usize) -> bool {
    let sector = at - at % SECTOR;
    let (from, to) = (sector.max(start), (sector + SECTOR).min(end));
    let blank = |bytes: &[u8]| bytes.iter().all(|b| *b == 0) || bytes.iter().all(|b| *b == 0xff);
    to - from >= 8 && !blank(&image[from..=at]) && !blank(&image[at..to])
}

pub fn frame_table(records: &[u8]) -> Vec<u8> {
    let mut table = Vec::new();
    for block in records.chunks(BLOCK) {
        table.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(block).to_le_bytes());
    }
    table
}

pub fn frame_check(header: &[u8], table: &[u8]) -> [u8; 16] {
    let mut check = xxhash_rust::xxh3::Xxh3::new();
    check.update(&header[..FRAME_CHECKED]);
    check.update(table);
    check.digest128().to_le_bytes()
}

pub fn seal_frame(image: &mut [u8], start: usize, end: usize) {
    let body = start + FRAME_BYTES;
    let records = end - body - table_bytes(end - start);
    let table = frame_table(&image[body..body + records]);
    image[body + records..end].copy_from_slice(&table);
    let check = frame_check(&image[start..], &table);
    image[start + FRAME_CHECKED..start + FRAME_BYTES].copy_from_slice(&check);
}

pub fn whole_frame(image: &[u8], start: usize) -> Option<usize> {
    if start + FRAME_BYTES > image.len() || &image[start..start + 8] != b"FERROFRM" {
        return None;
    }
    let end = field(image, start + 16);
    if end < start + FRAME_BYTES || end > image.len() {
        return None;
    }
    let body = start + FRAME_BYTES;
    let records = (end - body).checked_sub(table_bytes(end - start))?;
    let table = frame_table(&image[body..body + records]);
    let whole = table[..] == image[body + records..end]
        && frame_check(&image[start..], &table)
            == image[start + FRAME_CHECKED..start + FRAME_BYTES];
    whole.then_some(end)
}

pub fn frames_past_the_anchor(image: &[u8]) -> Vec<(usize, usize)> {
    let mut newest = None;
    for slot in [0, SLOT] {
        if image.len() < slot + 144 || &image[slot..slot + 8] != b"FERRODB5" {
            continue;
        }
        if blake3::hash(&image[slot..slot + 112]).as_bytes()[..] != image[slot + 112..slot + 144] {
            continue;
        }
        let key = (field(image, slot + 8), field(image, slot + 68));
        if newest.is_none_or(|(best, _)| key > best) {
            newest = Some((key, field(image, slot + 76)));
        }
    }
    let mut frames = Vec::new();
    let Some((_, mut at)) = newest else {
        return frames;
    };
    while let Some(end) = whole_frame(image, at) {
        frames.push((at, end));
        at = end;
    }
    frames
}

pub const LOG_START: usize = 2 * SLOT;
pub const HASH_BYTES: usize = 20;

pub fn varint(bytes: &[u8], pos: &mut usize) -> u64 {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let byte = bytes[*pos];
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
    }
    value
}

pub fn link_distance(value: u64, len: usize) -> u64 {
    match value & 1 {
        0 => value >> 1,
        _ => {
            let top = 7 * (len as u32 - 1);
            assert_eq!(value >> top, 1, "a padded distance ends in a byte of 1");
            (value ^ (1 << top)) >> 1
        }
    }
}

pub type Link = ([u8; HASH_BYTES], Option<(usize, usize)>);

pub type Shape = (u64, [u8; HASH_BYTES], Vec<(Vec<u8>, Vec<Link>)>);

pub fn frame_shapes(image: &[u8]) -> Vec<Shape> {
    let mut frames = Vec::new();
    let mut records = std::collections::HashMap::new();
    let mut start = LOG_START;
    while let Some(end) = whole_frame(image, start) {
        let seq = field(image, start + 8) as u64;
        let head: [u8; HASH_BYTES] = image[start + 32..start + 52].try_into().unwrap();
        let body_end = end - table_bytes(end - start);
        let mut pos = start + FRAME_BYTES;
        let mut shape = Vec::new();
        while pos < body_end {
            let at = pos;
            let stored = varint(image, &mut pos) as usize;
            let flagged = varint(image, &mut pos);
            if flagged & 1 == 1 {
                varint(image, &mut pos);
            }
            pos += stored;
            let header = image[at..pos].to_vec();
            let mut links = Vec::new();
            for _ in 0..flagged >> 1 {
                let hash = image[pos..pos + HASH_BYTES].try_into().unwrap();
                pos += HASH_BYTES;
                let begun = pos;
                let value = varint(image, &mut pos);
                let target = (at as u64).wrapping_sub(link_distance(value, pos - begun));
                links.push((hash, records.get(&target).copied()));
            }
            records.insert(at as u64, (frames.len(), shape.len()));
            shape.push((header, links));
        }
        frames.push((seq, head, shape));
        start = end;
    }
    frames
}

pub fn padded_links(image: &[u8]) -> usize {
    let mut padded = 0;
    let mut start = LOG_START;
    while let Some(end) = whole_frame(image, start) {
        let body_end = end - table_bytes(end - start);
        let mut pos = start + FRAME_BYTES;
        while pos < body_end {
            let stored = varint(image, &mut pos) as usize;
            let flagged = varint(image, &mut pos);
            if flagged & 1 == 1 {
                varint(image, &mut pos);
            }
            pos += stored;
            for _ in 0..flagged >> 1 {
                pos += HASH_BYTES;
                padded += (varint(image, &mut pos) & 1) as usize;
            }
        }
        start = end;
    }
    padded
}

pub fn assert_damaged<T>(result: ferro::Result<T>, frame: usize, byte: usize, context: &str) {
    match result {
        Err(ferro::Error::Corrupt(message)) => assert!(
            message.contains(&format!(" at {frame} "))
                && message.ends_with(&format!("byte {byte}")),
            "{context}: {message}"
        ),
        Err(other) => panic!("{context}: unexpected error {other}"),
        Ok(_) => panic!("{context}: opened a damaged frame"),
    }
}

pub fn text(seed: u64, len: usize) -> String {
    let mut rng = Rng::new(seed.wrapping_add(1));
    (0..len)
        .map(|_| char::from(b'a' + rng.below(26) as u8))
        .collect()
}

pub fn cases(base: u32) -> u32 {
    let scale = std::env::var("FERRO_TEST_SCALE")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(1);
    base.saturating_mul(scale.max(1))
}

pub fn level_of<K: Data>(key: &K) -> u32 {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    key.encode(&mut encoder).unwrap();
    xxhash_rust::xxh3::xxh3_64(encoder.bytes()).trailing_zeros() / 3
}

pub fn fast() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20)
}

pub fn roomy() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 28)
}

pub fn uncached() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1)
}

pub fn panic_text(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = payload.downcast_ref::<&str>() {
        text.to_string()
    } else {
        "<non-string panic>".to_string()
    }
}

pub fn quiet_ferro_panics() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !panic_text(info.payload()).starts_with("ferro:") {
                default(info);
            }
        }));
    });
}

pub enum Outcome<T> {
    Value(T),
    FerroPanic(String),
}

pub fn guarded<T>(f: impl FnOnce() -> T) -> Outcome<T> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Outcome::Value(value),
        Err(payload) => {
            let message = panic_text(&*payload);
            assert!(
                message.starts_with("ferro:"),
                "panic without ferro prefix: {message}"
            );
            Outcome::FerroPanic(message)
        }
    }
}
