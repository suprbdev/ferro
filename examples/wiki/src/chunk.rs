use std::sync::Arc;

use similar::{Algorithm, DiffOp, capture_diff_slices};

use crate::model::Body;

pub const STEP: u64 = 1 << 24;

#[derive(Clone, Copy, Debug)]
pub struct Chunker {
    pub min: usize,
    pub bits: u32,
    pub max: usize,
}

pub const DEFAULT: Chunker = Chunker {
    min: 512,
    bits: 10,
    max: 4096,
};

const fn gear() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
}

const GEAR: [u64; 256] = gear();

impl Chunker {
    pub fn split<'a>(&self, text: &'a str) -> Vec<&'a str> {
        let bytes = text.as_bytes();
        let mask = ((1u64 << self.bits) - 1) << (64 - self.bits);
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < bytes.len() {
            let limit = (start + self.max).min(bytes.len());
            let mut end = limit;
            if limit - start > self.min {
                let mut hash = 0u64;
                for (offset, byte) in bytes[start..limit].iter().enumerate() {
                    hash = (hash << 1).wrapping_add(GEAR[*byte as usize]);
                    if offset + 1 >= self.min && hash & mask == 0 {
                        end = start + offset + 1;
                        break;
                    }
                }
            }
            while !text.is_char_boundary(end) {
                end += 1;
            }
            chunks.push(&text[start..end]);
            start = end;
        }
        chunks
    }
}

pub fn build(text: &str, chunker: &Chunker) -> Body {
    chunker
        .split(text)
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| ((i as u64 + 1) * STEP, chunk.to_string()))
        .collect()
}

pub fn text(body: &Body) -> String {
    let mut out = String::new();
    for chunk in body.values() {
        out.push_str(&chunk);
    }
    out
}

pub fn bytes(body: &Body) -> usize {
    body.values().map(|chunk| chunk.len()).sum()
}

struct Region {
    old: (usize, usize),
    new: (usize, usize),
}

fn regions(ops: &[DiffOp]) -> Vec<Region> {
    let mut out: Vec<Region> = Vec::new();
    for op in ops {
        let (old, new) = match op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => ((*old_index, old_index + old_len), (*new_index, *new_index)),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => ((*old_index, *old_index), (*new_index, new_index + new_len)),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => (
                (*old_index, old_index + old_len),
                (*new_index, new_index + new_len),
            ),
        };
        match out.last_mut() {
            Some(last) if last.old.1 == old.0 && last.new.1 == new.0 => {
                last.old.1 = old.1;
                last.new.1 = new.1;
            }
            _ => out.push(Region { old, new }),
        }
    }
    out
}

fn bounds(old: &[(u64, Arc<String>)], region: &Region) -> (u64, u64) {
    let lower = if region.old.0 == 0 {
        0
    } else {
        old[region.old.0 - 1].0
    };
    let upper = old.get(region.old.1).map_or(u64::MAX, |(key, _)| *key);
    (lower, upper)
}

pub fn update(body: &mut Body, text: &str, chunker: &Chunker) {
    let old: Vec<(u64, Arc<String>)> = body.iter().collect();
    let old_chunks: Vec<&str> = old.iter().map(|(_, chunk)| chunk.as_str()).collect();
    let new_chunks = chunker.split(text);
    if old_chunks == new_chunks {
        return;
    }
    let ops = capture_diff_slices(Algorithm::Myers, &old_chunks, &new_chunks);
    let plan = regions(&ops);
    for region in &plan {
        let (lower, upper) = bounds(&old, region);
        let count = (region.new.1 - region.new.0) as u64;
        if count > 0 && upper - lower <= count {
            *body = build(text, chunker);
            return;
        }
    }
    for region in &plan {
        for (key, _) in &old[region.old.0..region.old.1] {
            body.remove(key);
        }
        let (lower, upper) = bounds(&old, region);
        let count = (region.new.1 - region.new.0) as u64;
        let spacing = if upper == u64::MAX {
            STEP
        } else {
            (upper - lower) / (count + 1)
        };
        for (j, chunk) in new_chunks[region.new.0..region.new.1].iter().enumerate() {
            body.insert(lower + spacing * (j as u64 + 1), chunk.to_string());
        }
    }
}
