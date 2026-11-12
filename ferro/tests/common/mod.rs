#![allow(dead_code)]

use std::any::Any;
use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Once;

use ferro::codec::{Decoder, Encoder, HashSink, Object};
use ferro::store::{Store, StoreOptions, Txn};
use ferro::{Bundle, Data, Db, Durability, Hash, Map, Options, Ptr, Summary};

pub struct Tripwire<'a> {
    bytes: &'a [u8],
    pos: usize,
    at: usize,
    trip: Option<Box<dyn FnOnce() + 'a>>,
}

impl<'a> Tripwire<'a> {
    pub fn new(bytes: &'a [u8], at: usize, trip: impl FnOnce() + 'a) -> Tripwire<'a> {
        Tripwire {
            bytes,
            pos: 0,
            at,
            trip: Some(Box::new(trip)),
        }
    }
}

impl std::io::Read for Tripwire<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.at
            && let Some(trip) = self.trip.take()
        {
            trip();
        }
        let mut n = out.len().min(self.bytes.len() - self.pos).min(64 << 10);
        if self.trip.is_some() {
            n = n.min(self.at - self.pos);
        }
        out[..n].copy_from_slice(&self.bytes[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

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

pub fn staging_frame(image: &[u8], start: usize) -> Option<usize> {
    if start + FRAME_BYTES > image.len() || &image[start..start + 8] != b"FERROSTG" {
        return None;
    }
    let end = field(image, start + 16);
    if end < start + FRAME_BYTES || end > image.len() {
        return None;
    }
    let check = frame_check(&image[start..], &[]);
    (check == image[start + FRAME_CHECKED..start + FRAME_BYTES]).then_some(end)
}

pub fn frames_past_the_anchor(image: &[u8]) -> Vec<(usize, usize)> {
    let mut newest = None;
    for slot in [0, SLOT] {
        if image.len() < slot + 144 || &image[slot..slot + 8] != b"FERRODB7" {
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
    loop {
        if let Some(end) = whole_frame(image, at) {
            frames.push((at, end));
            at = end;
        } else if let Some(end) = staging_frame(image, at) {
            at = end;
        } else {
            return frames;
        }
    }
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

pub struct LinkSlot {
    pub hash_at: usize,
    pub at: usize,
    pub len: usize,
    pub target: u64,
}

pub fn link_slots(bytes: &[u8], start: usize) -> (Vec<LinkSlot>, usize) {
    let mut pos = start;
    let stored = varint(bytes, &mut pos) as usize;
    let flagged = varint(bytes, &mut pos);
    if flagged & 1 == 1 {
        varint(bytes, &mut pos);
    }
    pos += stored;
    let mut slots = Vec::new();
    for _ in 0..flagged >> 1 {
        let hash_at = pos;
        pos += HASH_BYTES;
        let at = pos;
        let value = varint(bytes, &mut pos);
        slots.push(LinkSlot {
            hash_at,
            at,
            len: pos - at,
            target: (start as u64).wrapping_sub(link_distance(value, pos - at)),
        });
    }
    (slots, pos)
}

pub fn distance_bytes(offset: u64, target: u64, len: usize) -> Option<Vec<u8>> {
    let plain = offset.checked_sub(target)?.checked_mul(2)?;
    let mut need = 1;
    let mut rest = plain;
    while rest >= 0x80 {
        rest >>= 7;
        need += 1;
    }
    if need > len || len > 10 {
        return None;
    }
    let (mut value, last) = match need == len {
        true => (plain, None),
        false => (plain | 1, Some(1)),
    };
    let mut out = Vec::with_capacity(len);
    for _ in 1..len {
        out.push(value as u8 & 0x7f | 0x80);
        value >>= 7;
    }
    out.push(last.unwrap_or(value as u8));
    Some(out)
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

pub type Spread = Map<u64, String>;

pub fn spread_into(db: &Db<Spread>, model: &mut BTreeMap<u64, String>, rng: &mut Rng, lane: u64) {
    let count = match rng.below(10) {
        0 => 2_000,
        1 | 2 => 200,
        _ => 1 + rng.below(30),
    };
    let writes: Vec<(u64, String)> = (0..count)
        .map(|_| {
            let key = lane << 32 | rng.below(5_000);
            let value = match rng.below(8) {
                0 => text(rng.next(), 2_000),
                1 => text(key % 3, 1_500),
                _ => format!("{lane}-{}", rng.next()),
            };
            (key, value)
        })
        .collect();
    model.extend(writes.iter().cloned());
    db.update(|root| {
        for (key, value) in writes {
            root.insert(key, value);
        }
    })
    .unwrap();
}

pub fn spread_of(db: &Db<Spread>) -> BTreeMap<u64, String> {
    db.read()
        .iter()
        .map(|(key, value)| (key, (*value).clone()))
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

#[derive(Clone)]
pub struct RawHead {
    pub version: u64,
    pub first: u64,
    pub flags: u8,
    pub root: Option<Ptr>,
    pub tip: Option<Ptr>,
    pub merged: Map<String, (u64, u64)>,
    pub born: Option<u64>,
}

impl Data for RawHead {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.version.encode(w)?;
        self.first.encode(w)?;
        w.u8(self.flags);
        if let Some(root) = self.root {
            w.link(root);
        }
        self.tip.encode(w)?;
        self.merged.encode(w)?;
        if let Some(born) = self.born {
            born.encode(w)?;
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        let version = u64::decode(r)?;
        let first = u64::decode(r)?;
        let flags = r.u8()?;
        let root = match flags & 1 {
            0 => None,
            _ => Some(r.link()?),
        };
        let tip = Option::<Ptr>::decode(r)?;
        let merged = Map::decode(r)?;
        let born = match flags & 2 {
            0 => None,
            _ => Some(u64::decode(r)?),
        };
        Ok(RawHead {
            version,
            first,
            flags,
            root,
            tip,
            merged,
            born,
        })
    }
}

#[derive(Clone)]
struct RawTable(Map<String, RawHead>);

impl Data for RawTable {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        match r.u8()? {
            0x7e => {
                r.link()?;
                Ok(RawTable(Map::decode(r)?))
            }
            tag => Err(ferro::Error::Decode(format!("no table tag but {tag}"))),
        }
    }
}

pub fn rewrite_branch_table(
    path: &Path,
    write: impl FnOnce(Map<String, RawHead>, &mut Txn) -> Ptr,
) {
    ferro::admin::compact(path, ferro::Keep::All).unwrap();
    let (store, head) = Store::open(path, StoreOptions::default()).unwrap();
    let refs = store.load::<RawTable>(head.unwrap()).unwrap().0.clone();
    let mut txn = store.begin().unwrap();
    let table = write(refs, &mut txn);
    txn.commit(table).unwrap();
    store.close();
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

thread_local! {
    static FUSE: std::cell::Cell<Option<(u32, bool)>> = const { std::cell::Cell::new(None) };
    static DECODES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub fn fuse(after: u32, fails: bool) {
    FUSE.set(Some((after, fails)));
}

pub fn defuse() {
    FUSE.set(None);
}

pub fn decodes() -> u64 {
    DECODES.get()
}

pub fn count_decodes() {
    DECODES.set(0);
}

fn tick() -> ferro::Result<()> {
    DECODES.set(DECODES.get() + 1);
    match FUSE.get() {
        Some((0, fails)) => {
            FUSE.set(None);
            if fails {
                return Err(ferro::Error::Decode("the fuse blew".into()));
            }
            panic!("the fuse blew");
        }
        Some((left, fails)) => FUSE.set(Some((left - 1, fails))),
        None => {}
    }
    Ok(())
}

pub fn interrupted<R>(rng: &mut Rng, op: impl FnOnce() -> R) -> Option<R> {
    if rng.chance(80) {
        let decodes = if rng.chance(50) { 40 } else { 2500 };
        fuse(rng.below(decodes) as u32, rng.chance(50));
    }
    let outcome = panic::catch_unwind(AssertUnwindSafe(op));
    defuse();
    outcome.ok()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fragile(pub u64, pub String);

impl Data for Fragile {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.0.encode(w)?;
        self.1.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        let value = Fragile(u64::decode(r)?, String::decode(r)?);
        tick()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Brittle(pub u64);

impl Data for Brittle {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        let key = Brittle(u64::decode(r)?);
        tick()?;
        Ok(key)
    }
}

pub struct Sent {
    pub body: Vec<u8>,
    pub links: Vec<(u64, Option<Hash>)>,
    pub uses: u64,
}

pub fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub fn sent_objects(bundle: &Bundle) -> (Vec<u8>, Vec<Sent>) {
    let bytes = bundle.as_bytes();
    let mut pos = 10 + HASH_BYTES;
    varint(bytes, &mut pos);
    for _ in 0..varint(bytes, &mut pos) {
        pos += HASH_BYTES;
        varint(bytes, &mut pos);
    }
    assert_eq!(varint(bytes, &mut pos), 0, "a bundle that names no roots");
    assert_eq!(varint(bytes, &mut pos), 0, "a bundle that names no hints");
    let head = bytes[..pos].to_vec();
    let mut objects = Vec::new();
    for _ in 0..varint(bytes, &mut pos) {
        let flagged = varint(bytes, &mut pos);
        let stored = (flagged >> 1) as usize;
        let body = match flagged & 1 {
            1 => {
                let raw = varint(bytes, &mut pos) as usize;
                lz4_flex::block::decompress(&bytes[pos..pos + stored], raw).unwrap()
            }
            _ => bytes[pos..pos + stored].to_vec(),
        };
        pos += stored;
        let links = (0..varint(bytes, &mut pos))
            .map(|_| match varint(bytes, &mut pos) {
                0 => {
                    let hash: Hash = bytes[pos..pos + HASH_BYTES].try_into().unwrap();
                    pos += HASH_BYTES;
                    (0, Some(hash))
                }
                back => (back, None),
            })
            .collect();
        let uses = varint(bytes, &mut pos);
        objects.push(Sent { body, links, uses });
    }
    assert_eq!(pos, bytes.len());
    (head, objects)
}

pub fn resent(head: &[u8], objects: &[Sent]) -> Bundle {
    let mut hashes: Vec<Hash> = Vec::with_capacity(objects.len());
    for (at, object) in objects.iter().enumerate() {
        let links = object
            .links
            .iter()
            .map(|(back, hash)| Ptr {
                hash: hash.unwrap_or_else(|| hashes[at - *back as usize]),
                offset: 0,
            })
            .collect();
        hashes.push(
            Object {
                body: object.body.clone(),
                links,
            }
            .hash(),
        );
    }
    let mut out = head.to_vec();
    out[10..10 + HASH_BYTES].copy_from_slice(hashes.last().unwrap());
    push_varint(&mut out, objects.len() as u64);
    for object in objects {
        push_varint(&mut out, (object.body.len() as u64) << 1);
        out.extend_from_slice(&object.body);
        push_varint(&mut out, object.links.len() as u64);
        for (back, hash) in &object.links {
            push_varint(&mut out, *back);
            out.extend(hash.iter().flatten());
        }
        push_varint(&mut out, object.uses);
    }
    Bundle::from(out)
}

pub fn node_body(keys: &[u64], summaries: Option<&[u64]>) -> Vec<u8> {
    let mut body = vec![0];
    push_varint(&mut body, keys.len() as u64);
    for key in keys {
        push_varint(&mut body, *key);
    }
    for index in 0..keys.len() {
        body.push(1);
        if let Some(summaries) = summaries {
            push_varint(&mut body, summaries[index]);
        }
    }
    body.resize(body.len() + keys.len() + 1, 0);
    body
}

pub fn sharing_one_value<S: Summary<u64, String>>(count: usize) -> (Vec<u8>, Vec<Sent>, Vec<u64>) {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Map<u64, String, S>> = fast().open(dir.path().join("a.ferro")).unwrap();
    let fresh: Db<Map<u64, String, S>> = fast().open(dir.path().join("fresh.ferro")).unwrap();
    let keys: Vec<u64> = (1..).filter(|key| level_of(key) == 0).take(count).collect();
    let value = text(9, 600);
    a.update(|root| {
        root.insert(keys[0], value.clone());
        root.insert(keys[1], value.clone());
    })
    .unwrap();
    let (head, objects) = sent_objects(&a.bundle(&fresh.have().unwrap()).unwrap());
    assert_eq!(objects.len(), 4, "a value, a node, a root and a commit");
    assert_eq!(objects[1].links, vec![(1, None); 2]);
    assert_eq!(objects[0].uses, 2);
    (head, objects, keys)
}
