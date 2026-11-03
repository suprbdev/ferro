use std::collections::BTreeMap;

use ferro::map::Iter;
use ferro::{Bytes, Change, Cursor, Data, Map};

pub type UserId = u32;
pub type Edges = Vec<(UserId, UserId)>;

const FILL: usize = 128;
const SPLIT: usize = 256;

pub fn encode_block(ids: &[u32]) -> Bytes {
    let mut out = Vec::with_capacity(ids.len() * 2);
    let mut previous = ids[0];
    for &id in &ids[1..] {
        let mut delta = id - previous;
        previous = id;
        while delta >= 0x80 {
            out.push((delta & 0x7f) as u8 | 0x80);
            delta >>= 7;
        }
        out.push(delta as u8);
    }
    Bytes::from(out)
}

pub fn decode_into(first: u32, bytes: &[u8], out: &mut Vec<u32>) {
    let start = out.len();
    out.resize(start + bytes.len() + 1, first);
    let ids = &mut out[start..];
    let mut written = 1;
    let mut current = first;
    let mut delta = 0u32;
    let mut shift = 0;
    for &byte in bytes {
        delta |= ((byte & 0x7f) as u32) << shift;
        let done = byte < 0x80;
        let next = current + delta;
        ids[written] = next;
        written += usize::from(done);
        current = if done { next } else { current };
        delta = if done { 0 } else { delta };
        shift = if done { 0 } else { shift + 7 };
    }
    out.truncate(start + written);
}

fn block_len(bytes: &[u8]) -> usize {
    1 + bytes.iter().filter(|&&b| b < 0x80).count()
}

fn holds(first: u32, bytes: &[u8], wanted: u32) -> bool {
    if first == wanted {
        return true;
    }
    let mut current = first;
    let mut i = 0;
    while i < bytes.len() {
        let mut delta = 0u32;
        let mut shift = 0;
        loop {
            let byte = bytes[i];
            i += 1;
            delta |= ((byte & 0x7f) as u32) << shift;
            if byte < 0x80 {
                break;
            }
            shift += 7;
        }
        current += delta;
        if current >= wanted {
            return current == wanted;
        }
    }
    false
}

#[derive(Data, Clone, Default)]
#[ferro(transparent)]
pub struct Adj(Map<(UserId, UserId), Bytes>);

impl Adj {
    pub fn from_sorted(edges: impl Iterator<Item = (UserId, UserId)>) -> Adj {
        let mut blocks: Vec<((UserId, UserId), Bytes)> = Vec::new();
        let mut ids: Vec<u32> = Vec::new();
        let mut owner = None;
        for (a, b) in edges {
            if owner != Some(a) || ids.len() == FILL {
                if let Some(user) = owner {
                    blocks.push(((user, ids[0]), encode_block(&ids)));
                    ids.clear();
                }
                owner = Some(a);
            }
            ids.push(b);
        }
        if let Some(user) = owner {
            blocks.push(((user, ids[0]), encode_block(&ids)));
        }
        Adj(Map::from_iter(blocks))
    }

    pub fn block_count(&self) -> usize {
        self.0.len()
    }

    fn floor(&self, a: UserId, b: UserId) -> Option<(UserId, Bytes)> {
        let ((user, first), bytes) = self.0.floor(&(a, b))?;
        (user == a).then(|| (first, (*bytes).clone()))
    }

    pub fn contains(&self, a: UserId, b: UserId) -> bool {
        let Some(((user, first), bytes)) = self.0.floor(&(a, b)) else {
            return false;
        };
        user == a && holds(first, &bytes, b)
    }

    pub fn walker(&self) -> Walker {
        Walker {
            cursor: self.0.cursor(),
            ids: Vec::new(),
        }
    }

    pub fn insert(&mut self, a: UserId, b: UserId) -> bool {
        let held = self.floor(a, b).or_else(|| {
            self.0
                .range((a, 0)..=(a, UserId::MAX))
                .next()
                .map(|((_, first), bytes)| (first, (*bytes).clone()))
        });
        let mut ids = Vec::new();
        if let Some((first, bytes)) = &held {
            decode_into(*first, bytes, &mut ids);
        }
        match ids.binary_search(&b) {
            Ok(_) => return false,
            Err(at) => ids.insert(at, b),
        }
        if let Some((first, _)) = held
            && first != ids[0]
        {
            self.0.remove(&(a, first));
        }
        if ids.len() > SPLIT {
            let right = ids.split_off(ids.len() / 2);
            self.0.insert((a, right[0]), encode_block(&right));
        }
        self.0.insert((a, ids[0]), encode_block(&ids));
        true
    }

    pub fn remove(&mut self, a: UserId, b: UserId) -> bool {
        let Some((first, bytes)) = self.floor(a, b) else {
            return false;
        };
        let mut ids = Vec::new();
        decode_into(first, &bytes, &mut ids);
        let Ok(at) = ids.binary_search(&b) else {
            return false;
        };
        ids.remove(at);
        self.0.remove(&(a, first));
        if !ids.is_empty() {
            self.0.insert((a, ids[0]), encode_block(&ids));
        }
        true
    }

    pub fn degree(&self, a: UserId) -> usize {
        let mut total = 0;
        self.0.for_each_range((a, 0)..=(a, UserId::MAX), |_, bytes| total += block_len(bytes));
        total
    }

    pub fn degrees(&self, low: UserId, high: UserId, mut f: impl FnMut(UserId, usize)) {
        self.0.for_each_range((low, 0)..(high, 0), |&(user, _), bytes| f(user, block_len(bytes)));
    }

    pub fn neighbours(&self, a: UserId) -> Vec<UserId> {
        self.iter(a).collect()
    }

    pub fn iter(&self, a: UserId) -> Neighbours {
        self.iter_after(a, None)
    }

    pub fn iter_after(&self, a: UserId, after: Option<UserId>) -> Neighbours {
        let start = after.and_then(|b| self.floor(a, b)).map_or(0, |(first, _)| first);
        let mut cursor = self.0.cursor();
        cursor.seek(&(a, start));
        Neighbours {
            cursor,
            user: a,
            buf: Vec::new(),
            pos: 0,
            after,
        }
    }

    pub fn scan(&self, low: UserId, high: UserId) -> Scan {
        Scan {
            blocks: self.0.range((low, 0)..(high, 0)),
            pending: None,
        }
    }

    pub fn for_each_user(&self, low: UserId, high: UserId, mut f: impl FnMut(UserId, &[u32])) {
        let mut ids = Vec::new();
        let mut owner = None;
        self.0.for_each_range((low, 0)..(high, 0), |&(user, first), bytes| {
            if owner != Some(user) {
                if let Some(done) = owner.replace(user) {
                    f(done, &ids);
                }
                ids.clear();
            }
            decode_into(first, bytes, &mut ids);
        });
        if let Some(done) = owner {
            f(done, &ids);
        }
    }

    pub fn diff(&self, newer: &Adj) -> (Edges, Edges) {
        let mut before: BTreeMap<UserId, Vec<u32>> = BTreeMap::new();
        let mut after: BTreeMap<UserId, Vec<u32>> = BTreeMap::new();
        for change in self.0.diff(&newer.0) {
            let (user, first) = *change.key();
            if let Change::Removed(_, bytes) | Change::Changed(_, bytes, _) = &change {
                decode_into(first, bytes, before.entry(user).or_default());
            }
            if let Change::Added(_, bytes) | Change::Changed(_, _, bytes) = &change {
                decode_into(first, bytes, after.entry(user).or_default());
            }
        }
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let users: std::collections::BTreeSet<UserId> =
            before.keys().chain(after.keys()).copied().collect();
        for user in users {
            let mut old = before.remove(&user).unwrap_or_default();
            let mut new = after.remove(&user).unwrap_or_default();
            old.sort_unstable();
            new.sort_unstable();
            let (mut i, mut j) = (0, 0);
            while i < old.len() || j < new.len() {
                if j == new.len() || (i < old.len() && old[i] < new[j]) {
                    removed.push((user, old[i]));
                    i += 1;
                } else if i == old.len() || new[j] < old[i] {
                    added.push((user, new[j]));
                    j += 1;
                } else {
                    i += 1;
                    j += 1;
                }
            }
        }
        (added, removed)
    }
}

pub struct Walker {
    cursor: Cursor<(UserId, UserId), Bytes>,
    ids: Vec<u32>,
}

impl Walker {
    pub fn neighbours(&mut self, user: UserId, limit: usize) -> &[u32] {
        let Walker { cursor, ids } = self;
        ids.clear();
        cursor.seek(&(user, 0));
        while ids.len() < limit {
            let decoded = cursor.read(|&(owner, first), bytes| {
                if owner == user {
                    decode_into(first, bytes, ids);
                }
                owner == user
            });
            if decoded != Some(true) {
                break;
            }
            cursor.advance();
        }
        &ids[..ids.len().min(limit)]
    }

    pub fn contains(&mut self, a: UserId, b: UserId) -> bool {
        self.cursor.seek_back(&(a, b));
        let held = self.cursor.read(|&(user, first), bytes| user == a && holds(first, bytes, b));
        held == Some(true)
    }
}

pub struct Neighbours {
    cursor: Cursor<(UserId, UserId), Bytes>,
    user: UserId,
    buf: Vec<u32>,
    pos: usize,
    after: Option<UserId>,
}

impl Iterator for Neighbours {
    type Item = UserId;

    fn next(&mut self) -> Option<UserId> {
        loop {
            if self.pos < self.buf.len() {
                let id = self.buf[self.pos];
                self.pos += 1;
                return Some(id);
            }
            let Neighbours {
                cursor,
                user,
                buf,
                pos,
                after,
            } = self;
            buf.clear();
            *pos = 0;
            let decoded = cursor.read(|&(owner, first), bytes| {
                if owner == *user {
                    decode_into(first, bytes, buf);
                }
                owner == *user
            });
            if decoded != Some(true) {
                return None;
            }
            cursor.advance();
            if let Some(after) = after {
                *pos = buf.partition_point(|x| x <= after);
            }
        }
    }
}

pub struct Scan {
    blocks: Iter<(UserId, UserId), Bytes>,
    pending: Option<(UserId, Vec<u32>)>,
}

impl Iterator for Scan {
    type Item = (UserId, Vec<u32>);

    fn next(&mut self) -> Option<(UserId, Vec<u32>)> {
        loop {
            match self.blocks.next() {
                Some(((user, first), bytes)) => match &mut self.pending {
                    Some((owner, ids)) if *owner == user => decode_into(first, &bytes, ids),
                    _ => {
                        let mut ids = Vec::new();
                        decode_into(first, &bytes, &mut ids);
                        if let Some(done) = self.pending.replace((user, ids)) {
                            return Some(done);
                        }
                    }
                },
                None => return self.pending.take(),
            }
        }
    }
}

pub fn intersect(
    left: &Adj,
    a: UserId,
    left_degree: usize,
    right: &Adj,
    b: UserId,
    right_degree: usize,
) -> Vec<UserId> {
    let (small, small_user, large, large_user, small_degree, large_degree) =
        if left_degree <= right_degree {
            (left, a, right, b, left_degree, right_degree)
        } else {
            (right, b, left, a, right_degree, left_degree)
        };
    if small_degree == 0 {
        return Vec::new();
    }
    if small_degree * 16 < large_degree {
        let mut walker = large.walker();
        return small.iter(small_user).filter(|&id| walker.contains(large_user, id)).collect();
    }
    let mut out = Vec::new();
    let mut x = small.iter(small_user);
    let mut y = large.iter(large_user);
    let (mut p, mut q) = (x.next(), y.next());
    while let (Some(i), Some(j)) = (p, q) {
        match i.cmp(&j) {
            std::cmp::Ordering::Less => p = x.next(),
            std::cmp::Ordering::Greater => q = y.next(),
            std::cmp::Ordering::Equal => {
                out.push(i);
                p = x.next();
                q = y.next();
            }
        }
    }
    out
}
