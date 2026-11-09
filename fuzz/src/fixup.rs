use std::collections::HashMap;

use ferro::codec::{HASH_BYTES, Hash, Object, Ptr};
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

pub const MAGIC: &[u8; 8] = b"FERRODB5";
pub const FRAME_MAGIC: &[u8; 8] = b"FERROFRM";
pub const SLOT_BYTES: usize = 4096;
pub const LOG_START: usize = 2 * SLOT_BYTES;
pub const MARK_BYTES: usize = 24 + HASH_BYTES;
pub const FRAME_CHECKED: usize = 8 + MARK_BYTES;
pub const FRAME_BYTES: usize = FRAME_CHECKED + 16;
pub const ANCHOR_AT: usize = FRAME_CHECKED + 16;
pub const SUPER_CHECKED: usize = ANCHOR_AT + MARK_BYTES;
pub const SUPER_BYTES: usize = SUPER_CHECKED + 32;
pub const BLOCK_BYTES: usize = 4096;
pub const BLOCK_CHECK: usize = 8;

const MAX_RECORDS: usize = 1 << 14;

pub fn records_in(body: usize) -> Option<usize> {
    let blocks = body.div_ceil(BLOCK_BYTES + BLOCK_CHECK);
    let records = body.checked_sub(blocks * BLOCK_CHECK)?;
    (records.div_ceil(BLOCK_BYTES) == blocks).then_some(records)
}

pub fn seal_frame(bytes: &mut [u8], at: usize, end: usize) {
    let Some(records) = records_in(end - at - FRAME_BYTES) else {
        return;
    };
    let body = at + FRAME_BYTES;
    let mut table = Vec::with_capacity(end - body - records);
    for block in bytes[body..body + records].chunks(BLOCK_BYTES) {
        table.extend_from_slice(&xxh3_64(block).to_le_bytes());
    }
    bytes[body + records..end].copy_from_slice(&table);
    let mut hasher = Xxh3::new();
    hasher.update(&bytes[at..at + FRAME_CHECKED]);
    hasher.update(&table);
    bytes[at + FRAME_CHECKED..at + FRAME_BYTES].copy_from_slice(&hasher.digest128().to_le_bytes());
}
const MAX_BODY: usize = 1 << 22;

struct Record {
    offset: u64,
    body: Option<Vec<u8>>,
    links: Vec<(usize, u64)>,
}

fn varint(bytes: &[u8], pos: &mut usize, end: usize) -> Option<u64> {
    let mut value = 0u64;
    for index in 0..10 {
        if *pos >= end {
            return None;
        }
        let byte = bytes[*pos];
        *pos += 1;
        if index == 9 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn link_delta(value: u64, len: usize) -> Option<i64> {
    if value & 1 == 0 {
        return i64::try_from(value >> 1).ok();
    }
    let top = 7 * u32::try_from(len.checked_sub(1)?).ok()?;
    if top == 0 || value.checked_shr(top)? != 1 {
        return None;
    }
    i64::try_from((value ^ (1 << top)) >> 1).ok()
}

fn record(bytes: &[u8], start: usize, end: usize) -> Option<(Record, usize)> {
    let mut pos = start;
    let stored = usize::try_from(varint(bytes, &mut pos, end)?).ok()?;
    let flagged = varint(bytes, &mut pos, end)?;
    let link_count = usize::try_from(flagged >> 1).ok()?;
    let raw = match flagged & 1 {
        1 => Some(usize::try_from(varint(bytes, &mut pos, end)?).ok()?),
        _ => None,
    };
    let body_end = pos
        .checked_add(stored)
        .filter(|body_end| *body_end <= end)?;
    let stored_body = &bytes[pos..body_end];
    let body = match raw {
        Some(raw) if raw <= MAX_BODY && raw <= stored.saturating_mul(255).saturating_add(255) => {
            lz4_flex::block::decompress(stored_body, raw).ok()
        }
        Some(_) => None,
        None => Some(stored_body.to_vec()),
    };
    pos = body_end;
    if link_count > (end - pos) / (HASH_BYTES + 1) {
        return None;
    }
    let mut links = Vec::with_capacity(link_count);
    for _ in 0..link_count {
        let at = pos;
        pos = pos.checked_add(HASH_BYTES).filter(|next| *next <= end)?;
        let begun = pos;
        let value = varint(bytes, &mut pos, end)?;
        let delta = link_delta(value, pos - begun)?;
        links.push((at, (start as u64).wrapping_sub(delta as u64)));
    }
    Some((
        Record {
            offset: start as u64,
            body,
            links,
        },
        pos,
    ))
}

fn run(bytes: &[u8], start: usize, end: usize, bare: bool, out: &mut Vec<Record>) -> usize {
    let mut pos = start;
    while pos < end && out.len() < MAX_RECORDS {
        if bare && (bytes[pos..].starts_with(FRAME_MAGIC) || bytes[pos..end.min(pos + 4)] == [0; 4])
        {
            break;
        }
        match record(bytes, pos, end) {
            Some((found, next)) => {
                out.push(found);
                pos = next;
            }
            None => break,
        }
    }
    pos
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn fix_mark(bytes: &mut [u8], at: usize, hashes: &HashMap<u64, Hash>) {
    let offset = read_u64(bytes, at + 16);
    if let Some(hash) = hashes.get(&offset) {
        bytes[at + 24..at + MARK_BYTES].copy_from_slice(hash);
    }
}

pub fn fixup(bytes: &mut [u8]) {
    let len = bytes.len();
    let mut records = Vec::new();
    let mut frames = Vec::new();
    let mut at = LOG_START;
    while at < len && records.len() < MAX_RECORDS {
        if bytes[at..].starts_with(FRAME_MAGIC) {
            if at + FRAME_BYTES > len {
                break;
            }
            let end = read_u64(bytes, at + 16);
            if end < (at + FRAME_BYTES) as u64 || end > len as u64 {
                break;
            }
            let end = end as usize;
            let held = records_in(end - at - FRAME_BYTES).unwrap_or(end - at - FRAME_BYTES);
            run(
                bytes,
                at + FRAME_BYTES,
                at + FRAME_BYTES + held,
                false,
                &mut records,
            );
            frames.push((at, end));
            at = end;
        } else {
            let next = run(bytes, at, len, true, &mut records);
            if next == at {
                break;
            }
            at = next;
        }
    }
    let index: HashMap<u64, usize> = records
        .iter()
        .enumerate()
        .map(|(position, found)| (found.offset, position))
        .collect();
    let mut hashes: Vec<Option<Hash>> = vec![None; records.len()];
    let mut state = vec![0u8; records.len()];
    for root in 0..records.len() {
        if state[root] != 0 {
            continue;
        }
        state[root] = 1;
        let mut stack = vec![(root, 0usize)];
        while let Some(top) = stack.last_mut() {
            let (current, next) = *top;
            if next < records[current].links.len() {
                top.1 += 1;
                let target = records[current].links[next].1;
                if let Some(&child) = index.get(&target)
                    && state[child] == 0
                {
                    state[child] = 1;
                    stack.push((child, 0));
                }
                continue;
            }
            stack.pop();
            let found = &records[current];
            let mut links = Vec::with_capacity(found.links.len());
            for &(at, target) in &found.links {
                if let Some(hash) = index.get(&target).and_then(|child| hashes[*child]) {
                    bytes[at..at + HASH_BYTES].copy_from_slice(&hash);
                }
                let hash: Hash = bytes[at..at + HASH_BYTES].try_into().unwrap();
                links.push(Ptr {
                    hash,
                    offset: target,
                });
            }
            hashes[current] = found.body.as_ref().map(|body| {
                Object {
                    body: body.clone(),
                    links,
                }
                .hash()
            });
            state[current] = 2;
        }
    }
    let by_offset: HashMap<u64, Hash> = records
        .iter()
        .zip(&hashes)
        .filter_map(|(found, hash)| hash.map(|hash| (found.offset, hash)))
        .collect();
    for &(at, end) in &frames {
        fix_mark(bytes, at + 8, &by_offset);
        seal_frame(bytes, at, end);
    }
    for slot in 0..2 {
        let start = slot * SLOT_BYTES;
        if start + SUPER_BYTES > len || &bytes[start..start + 8] != MAGIC {
            continue;
        }
        fix_mark(bytes, start + 8, &by_offset);
        fix_mark(bytes, start + ANCHOR_AT, &by_offset);
        let check = blake3::hash(&bytes[start..start + SUPER_CHECKED]);
        bytes[start + SUPER_CHECKED..start + SUPER_BYTES].copy_from_slice(check.as_bytes());
    }
}
