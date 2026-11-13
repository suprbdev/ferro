//! Splits the frames of single-key commits into the parts their bytes go to, and sizes them under
//! other encodings.
//!
//! Works in a temporary directory under `--dir` (default `target/bench`). `--workload` picks
//! `u64`, a `Map<u64, u64>`, `string`, a `Map<u64, String>` of short strings, `table`, the
//! `ferro-bench` table of accounts indexed on `tier` and `country`, or `all` (the default). Each
//! loads `--n` scrambled keys, or `--n` / 5 rows, (default 1,000,000) and makes `--commits`
//! single-key commits (default 1,000) with `Durability::Buffered`: an insert into a map, and for
//! the table a balance update, which changes only the rows, and an insert, which changes both
//! indexes too. For the `u64` map it first prints the mean keys and children of the nodes of the
//! load.
//!
//! It reads each commit's frame back, splits every record into its header, body and links, decodes
//! the body of every map node into its level and key count, keys, value tags, values, child counts
//! and summaries, and prints the mean bytes per commit of each part, with what LZ4 saved on the
//! node bodies it compressed, and the mean number of node records, keys and links. It then encodes
//! each record again under other encodings of nodes, links and frames, LZ4 included, and prints the
//! bytes each would save, and the time a read would spend on a whole frame to hash the records
//! that other records in it link, or to decompress the frame compressed as one block.
//!
//! [Bytes of a small commit](https://github.com/archlemon/ferro/blob/main/docs/internals/commit-bytes.md)
//! has the results.
//!
//! ```sh
//! cargo run --release -p ferro-bench --bin bytes -- --workload string --n 1000000
//! ```

use std::any::Any;
use std::collections::BTreeMap;
use std::fs::File;
use std::hint::black_box;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferro::codec::{Decoder, HASH_BYTES, Object};
use ferro::{Data, Durability, Map, Options, Ptr, Record, Table};

const FRAME_BYTES: usize = 68;
const BLOCK_BYTES: usize = 4096;
const BLOCK_CHECK: usize = 8;
const COMPRESS_MIN: usize = 96;
const CHUNK: usize = 8;
const ROUNDS: u32 = 32;

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Free,
    Pro,
    Team,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Account {
    #[key]
    id: u64,
    #[index]
    tier: Tier,
    #[index]
    country: String,
    email: String,
    name: String,
    balance: i64,
    tags: Vec<String>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Bank {
    accounts: Table<Account>,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn account(i: u64) -> Account {
    let seed = mix(i);
    Account {
        id: i,
        tier: match seed % 10 {
            0 => Tier::Team,
            1 | 2 => Tier::Pro,
            _ => Tier::Free,
        },
        country: ["nz", "au", "de", "us", "jp", "br", "ca", "fr"][(seed >> 8) as usize % 8]
            .to_string(),
        email: format!("user{i}@example.com"),
        name: format!("Account Holder {}", seed % 100_000),
        balance: (seed % 1_000_000) as i64,
        tags: vec!["beta".to_string(), format!("cohort-{}", seed % 12)],
    }
}

fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn number(name: &str, default: u64) -> u64 {
    argument(name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn varint(bytes: &[u8], pos: &mut usize) -> u64 {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = bytes[*pos];
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

fn varint_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

fn push_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push(value as u8 | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn table_bytes(frame: usize) -> usize {
    BLOCK_CHECK * (frame - FRAME_BYTES).div_ceil(BLOCK_BYTES + BLOCK_CHECK)
}

struct Rec {
    offset: u64,
    len: usize,
    header: usize,
    stored: usize,
    packed: bool,
    body: Vec<u8>,
    links: Vec<Ptr>,
    distances: Vec<usize>,
}

impl Rec {
    fn distance_bytes(&self) -> usize {
        self.distances.iter().sum()
    }

    fn link_bytes(&self) -> usize {
        self.links.len() * HASH_BYTES + self.distance_bytes()
    }
}

fn records(frame: &[u8], start: u64) -> Vec<Rec> {
    let mut pos = FRAME_BYTES;
    let mut out = Vec::new();
    while pos < frame.len() - table_bytes(frame.len()) {
        let at = pos;
        let offset = start + at as u64;
        let stored = varint(frame, &mut pos) as usize;
        let flagged = varint(frame, &mut pos);
        let raw = match flagged & 1 {
            1 => Some(varint(frame, &mut pos) as usize),
            _ => None,
        };
        let header = pos - at;
        let payload = &frame[pos..pos + stored];
        let body = match raw {
            Some(raw) => lz4_flex::block::decompress(payload, raw).unwrap(),
            None => payload.to_vec(),
        };
        pos += stored;
        let mut links = Vec::new();
        let mut distances = Vec::new();
        for _ in 0..flagged >> 1 {
            let hash = frame[pos..pos + HASH_BYTES].try_into().unwrap();
            pos += HASH_BYTES;
            let begun = pos;
            let value = varint(frame, &mut pos);
            distances.push(pos - begun);
            let delta = match value & 1 {
                0 => value >> 1,
                _ => (value ^ (1 << (7 * (pos - begun - 1)))) >> 1,
            };
            links.push(Ptr {
                hash,
                offset: offset.wrapping_sub(delta),
            });
        }
        out.push(Rec {
            offset,
            len: pos - at,
            header,
            stored,
            packed: raw.is_some(),
            body,
            links,
            distances,
        });
    }
    out
}

#[derive(Default)]
struct NodeParts {
    level: u8,
    head: usize,
    keys: Vec<Range<usize>>,
    numbers: Option<Vec<u64>>,
    tags: Vec<usize>,
    values: usize,
    value_links: usize,
    count_start: usize,
    counts: usize,
    summaries: usize,
    children: usize,
}

impl NodeParts {
    fn key_bytes(&self) -> usize {
        self.keys.iter().map(|range| range.len()).sum()
    }
}

fn parse_node<K: Data + 'static, V: Data>(body: &[u8], links: &[Ptr]) -> Option<NodeParts> {
    let mut r = Decoder::new(body, links);
    let at = |r: &Decoder<'_>| body.len() - r.remaining();
    let level = r.u8().ok()?;
    let n = usize::try_from(r.varint().ok()?).ok()?;
    if n > body.len() {
        return None;
    }
    let mut parts = NodeParts {
        level,
        head: at(&r),
        ..NodeParts::default()
    };
    let mut numbers = Vec::new();
    for _ in 0..n {
        let start = at(&r);
        let key = K::decode(&mut r).ok()?;
        if let Some(number) = (&key as &dyn Any).downcast_ref::<u64>() {
            numbers.push(*number);
        }
        parts.keys.push(start..at(&r));
    }
    if numbers.len() == n {
        parts.numbers = Some(numbers);
    }
    for _ in 0..n {
        let start = at(&r);
        parts.tags.push(start);
        match r.u8().ok()? {
            0 => {
                V::decode(&mut r).ok()?;
            }
            1 => {
                r.link().ok()?;
                parts.value_links += 1;
            }
            _ => return None,
        }
        parts.values += at(&r) - start - 1;
    }
    parts.count_start = at(&r);
    for _ in 0..=n {
        let start = at(&r);
        let count = r.varint().ok()?;
        parts.counts += at(&r) - start;
        if count != 0 {
            let start = at(&r);
            <()>::decode(&mut r).ok()?;
            parts.summaries += at(&r) - start;
            r.link().ok()?;
            parts.children += 1;
        }
    }
    r.end().ok()?;
    Some(parts)
}

type Parser = fn(&[u8], &[Ptr]) -> Option<NodeParts>;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Tree,
    Root,
    Entry,
    Refs,
}

#[derive(Default)]
struct Tally {
    commits: u64,
    total: f64,
    parts: BTreeMap<&'static str, f64>,
    counts: BTreeMap<&'static str, f64>,
    saved: BTreeMap<&'static str, f64>,
    times: BTreeMap<&'static str, Duration>,
}

impl Tally {
    fn add(&mut self, part: &'static str, bytes: usize) {
        *self.parts.entry(part).or_default() += bytes as f64;
    }

    fn less(&mut self, part: &'static str, bytes: usize) {
        *self.parts.entry(part).or_default() -= bytes as f64;
    }

    fn count(&mut self, what: &'static str, count: usize) {
        *self.counts.entry(what).or_default() += count as f64;
    }

    fn save(&mut self, encoding: &'static str, bytes: isize) {
        *self.saved.entry(encoding).or_default() += bytes as f64;
    }

    fn time(&mut self, step: &'static str, took: Duration) {
        *self.times.entry(step).or_default() += took;
    }

    fn report(&self, name: &str) {
        let n = self.commits.max(1) as f64;
        let share = |bytes: f64| 100.0 * bytes / self.total.max(1.0);
        println!(
            "{name}: {:.1} bytes per commit over {} commits",
            self.total / n,
            self.commits
        );
        for (part, bytes) in &self.parts {
            println!("  {part:<46} {:>8.1}  {:>5.1}%", bytes / n, share(*bytes));
        }
        let sum: f64 = self.parts.values().sum();
        println!("  {:<46} {:>8.1}", "sum of the parts", sum / n);
        println!("  per commit:");
        for (what, count) in &self.counts {
            println!("    {what:<44} {:>8.1}", count / n);
        }
        println!("  saved per commit:");
        for (encoding, bytes) in &self.saved {
            println!(
                "    {encoding:<44} {:>8.1}  {:>5.1}%",
                bytes / n,
                share(*bytes)
            );
        }
        println!("  read costs per frame:");
        for (step, took) in &self.times {
            println!("    {step:<44} {:>8.0} ns", took.as_nanos() as f64 / n);
        }
    }
}

fn packed_len(body: &[u8]) -> usize {
    if body.len() < COMPRESS_MIN {
        return body.len();
    }
    let packed = lz4_flex::block::compress(body).len() + varint_len(body.len() as u64);
    packed.min(body.len())
}

fn record_len(body: &[u8], links: usize, link_bytes: usize) -> usize {
    let stored = packed_len(body);
    let raw = if stored < body.len() {
        varint_len(body.len() as u64)
    } else {
        0
    };
    let payload = stored - raw;
    varint_len(payload as u64) + varint_len((links as u64) << 1) + raw + payload + link_bytes
}

#[derive(Clone, Copy, PartialEq)]
enum Keys {
    Whole,
    After,
    Packed,
}

#[derive(Clone, Copy, PartialEq)]
enum Tags {
    Each,
    Bitmap,
    WhenSpilled,
}

#[derive(Clone, Copy)]
struct Recode {
    keys: Keys,
    tags: Tags,
    leaf_counts: bool,
}

fn pack(numbers: &[u64], out: &mut Vec<u8>) {
    push_varint(out, numbers[0]);
    let gaps: Vec<u64> = numbers.windows(2).map(|w| w[1] - w[0] - 1).collect();
    let Some(widest) = gaps.iter().max() else {
        return;
    };
    let width = 64 - widest.leading_zeros() as usize;
    out.push(width as u8);
    let len = (gaps.len() * width).div_ceil(8);
    let mut word = 0u128;
    let mut held = 0;
    let mut written = 0;
    for gap in &gaps {
        word |= u128::from(*gap) << held;
        held += width;
        while held >= 8 {
            out.push(word as u8);
            word >>= 8;
            held -= 8;
            written += 1;
        }
    }
    while written < len {
        out.push(word as u8);
        word >>= 8;
        written += 1;
    }
}

fn recoded(body: &[u8], parts: &NodeParts, how: Recode) -> Vec<u8> {
    let mut out = body[..parts.head].to_vec();
    if let (Keys::Packed, Some(numbers)) = (how.keys, &parts.numbers)
        && !numbers.is_empty()
    {
        pack(numbers, &mut out);
    }
    for (index, range) in parts.keys.iter().enumerate() {
        match (how.keys, &parts.numbers, index) {
            (Keys::Packed, Some(_), _) => {}
            (Keys::After, Some(numbers), 1..) => {
                push_varint(&mut out, numbers[index] - numbers[index - 1] - 1);
            }
            (Keys::After | Keys::Packed, None, 1..) => {
                let before = &body[parts.keys[index - 1].clone()];
                let key = &body[range.clone()];
                let shared = before
                    .iter()
                    .zip(key)
                    .take_while(|(a, b)| a == b)
                    .count()
                    .min(127);
                out.push(shared as u8);
                out.extend_from_slice(&key[shared..]);
            }
            _ => out.extend_from_slice(&body[range.clone()]),
        }
    }
    let values = parts.keys.last().map_or(parts.head, |range| range.end);
    let untagged = match how.tags {
        Tags::Each => false,
        Tags::Bitmap => true,
        Tags::WhenSpilled => parts.value_links == 0,
    };
    if untagged {
        if how.tags == Tags::Bitmap {
            out.extend(std::iter::repeat_n(0u8, parts.keys.len().div_ceil(8)));
        }
        let kept = body[values..parts.count_start]
            .iter()
            .enumerate()
            .filter(|(at, _)| !parts.tags.contains(&(values + at)));
        out.extend(kept.map(|(_, byte)| *byte));
    } else {
        out.extend_from_slice(&body[values..parts.count_start]);
    }
    if how.leaf_counts || parts.level > 0 {
        out.extend_from_slice(&body[parts.count_start..]);
    }
    out
}

fn chunked(rec: &Rec, parts: &NodeParts, start: u64) -> usize {
    let children = parts.children;
    if children <= CHUNK || parts.value_links > 0 {
        return rec.len;
    }
    let n = parts.keys.len().max(1);
    let packing = rec.stored as f64 / rec.body.len() as f64;
    let entry = (parts.key_bytes() + parts.tags.len() + parts.values) as f64 / n as f64 * packing;
    let count = parts.counts as f64 / (n + 1) as f64;
    let distance = rec.distance_bytes() as f64 / rec.links.len() as f64;
    let child = HASH_BYTES as f64 + distance + count;
    let chunks = children.div_ceil(CHUNK);
    let last = children - (chunks - 1) * CHUNK;
    let width = match rec.links.iter().position(|link| link.offset >= start) {
        Some(at) if at / CHUNK == chunks - 1 => last,
        _ => CHUNK,
    };
    let head = 4.0 + (chunks - 1) as f64 * entry + chunks as f64 * (child + 1.0);
    let chunk = 4.0 + (width - 1) as f64 * entry + width as f64 * child;
    (head + chunk).round() as usize
}

fn sibling_distances(rec: &Rec, either: bool) -> usize {
    rec.links
        .iter()
        .enumerate()
        .map(|(index, link)| {
            let back = rec.offset.wrapping_sub(link.offset);
            if index == 0 {
                return varint_len(back << 1);
            }
            let gap = zigzag(link.offset.wrapping_sub(rec.links[index - 1].offset) as i64);
            if either {
                varint_len(back << 2).min(varint_len(gap << 2))
            } else {
                varint_len(gap << 1)
            }
        })
        .sum()
}

fn plain_record(rec: &Rec, out: &mut Vec<u8>) {
    push_varint(out, rec.body.len() as u64);
    push_varint(out, (rec.links.len() as u64) << 1);
    out.extend_from_slice(&rec.body);
    for link in &rec.links {
        out.extend_from_slice(&link.hash);
        push_varint(out, rec.offset.wrapping_sub(link.offset) << 1);
    }
}

fn root_of(recs: &[Rec]) -> usize {
    (0..recs.len().saturating_sub(1))
        .rev()
        .find(|&at| {
            recs[at + 1]
                .links
                .first()
                .is_some_and(|link| link.offset == recs[at].offset)
                && recs[at + 2..]
                    .iter()
                    .any(|rec| rec.links.iter().any(|l| l.offset == recs[at + 1].offset))
        })
        .expect("a commit's frame holds a root object, its log entry and the refs")
}

fn kind_of(root: usize, at: usize) -> Kind {
    match at {
        _ if at < root => Kind::Tree,
        _ if at == root => Kind::Root,
        _ if at == root + 1 => Kind::Entry,
        _ => Kind::Refs,
    }
}

fn split_frame(frame: &[u8], start: u64, parsers: &[Parser], tally: &mut Tally) {
    let recs = records(frame, start);
    let root = root_of(&recs);
    tally.commits += 1;
    tally.total += frame.len() as f64;
    tally.add("frame header", FRAME_BYTES);
    tally.add("frame check table", table_bytes(frame.len()));
    let mut plain = Vec::new();
    for (at, rec) in recs.iter().enumerate() {
        plain_record(rec, &mut plain);
        let inward = rec.links.iter().filter(|l| l.offset >= start).count();
        if inward > 0 {
            tally.save(
                "no hash on a link into the same frame",
                (inward * HASH_BYTES) as isize - 1,
            );
        }
        let distances = rec.distance_bytes() as isize;
        tally.save(
            "distances from the link before",
            distances - sibling_distances(rec, false) as isize,
        );
        tally.save(
            "distances, the shorter of the two",
            distances - sibling_distances(rec, true) as isize,
        );
        let kind = kind_of(root, at);
        let parts = match kind {
            Kind::Tree => parsers
                .iter()
                .find_map(|parse| parse(&rec.body, &rec.links)),
            _ => None,
        };
        let Some(parts) = parts else {
            let (body, hashes, distances) = match kind {
                Kind::Tree => (
                    "tree: other records",
                    "tree: other records",
                    "tree: other records",
                ),
                Kind::Root => (
                    "root object: body and header",
                    "root object: link hashes",
                    "root object: link distances",
                ),
                Kind::Entry => (
                    "log entry: body and header",
                    "log entry: link hashes",
                    "log entry: link distances",
                ),
                Kind::Refs => (
                    "refs: bodies and headers",
                    "refs: link hashes",
                    "refs: link distances",
                ),
            };
            tally.add(body, rec.len - rec.link_bytes());
            tally.add(hashes, rec.links.len() * HASH_BYTES);
            tally.add(distances, rec.distance_bytes());
            continue;
        };
        tally.add("node: record header", rec.header);
        tally.add("node: level and key count", parts.head);
        tally.add("node: keys", parts.key_bytes());
        tally.add("node: value tags", parts.tags.len());
        tally.add("node: values", parts.values);
        tally.add("node: child counts", parts.counts);
        tally.add("node: summaries", parts.summaries);
        if rec.packed {
            let raw = varint_len(rec.body.len() as u64);
            tally.less("node: saved by LZ4", rec.body.len() + raw - rec.stored);
        }
        tally.add("node: child link hashes", parts.children * HASH_BYTES);
        tally.add("node: value link hashes", parts.value_links * HASH_BYTES);
        tally.add("node: link distances", rec.distance_bytes());
        tally.count("node records", 1);
        match parts.level {
            0 => tally.count("leaves", 1),
            _ => tally.count("interior nodes", 1),
        }
        if parts.children > CHUNK {
            tally.count("nodes with more than 8 children", 1);
        }
        tally.count("keys", parts.keys.len());
        tally.count("node links", rec.links.len());
        tally.count("node links into the same frame", inward);
        let how = |keys, tags, leaf_counts| Recode {
            keys,
            tags,
            leaf_counts,
        };
        for (name, recode) in [
            (
                "keys after the key before",
                how(Keys::After, Tags::Each, true),
            ),
            (
                "u64 keys as bit-packed gaps",
                how(Keys::Packed, Tags::Each, true),
            ),
            (
                "value tags as a bitmap",
                how(Keys::Whole, Tags::Bitmap, true),
            ),
            ("untagged values", how(Keys::Whole, Tags::WhenSpilled, true)),
            ("no counts in leaves", how(Keys::Whole, Tags::Each, false)),
            ("compact nodes", how(Keys::After, Tags::WhenSpilled, false)),
            (
                "compact nodes, bit-packed",
                how(Keys::Packed, Tags::WhenSpilled, false),
            ),
        ] {
            let body = recoded(&rec.body, &parts, recode);
            let len = record_len(&body, rec.links.len(), rec.link_bytes());
            tally.save(name, rec.len as isize - len as isize);
        }
        tally.save(
            "interior nodes as chunks of 8 children",
            rec.len as isize - chunked(rec, &parts, start) as isize,
        );
    }
    let records_len: usize = recs.iter().map(|rec| rec.len).sum();
    let packed = lz4_flex::block::compress(&plain);
    tally.save(
        "LZ4 over the whole frame",
        records_len as isize - (packed.len() + varint_len(plain.len() as u64)) as isize,
    );
    time_reads(&recs, &packed, plain.len(), tally);
}

fn time_reads(recs: &[Rec], packed: &[u8], plain: usize, tally: &mut Tally) {
    let began = Instant::now();
    for _ in 0..ROUNDS {
        black_box(lz4_flex::block::decompress(black_box(packed), plain).unwrap());
    }
    tally.time(
        "LZ4 decompress of the whole frame",
        began.elapsed() / ROUNDS,
    );
    let linked: Vec<Object> = recs
        .iter()
        .filter(|rec| {
            recs.iter()
                .any(|other| other.links.iter().any(|link| link.offset == rec.offset))
        })
        .map(|rec| Object {
            body: rec.body.clone(),
            links: rec.links.clone(),
        })
        .collect();
    let began = Instant::now();
    for _ in 0..ROUNDS {
        for object in &linked {
            black_box(black_box(object).hash());
        }
    }
    tally.time(
        "BLAKE3 of the records linked in the frame",
        began.elapsed() / ROUNDS,
    );
}

fn read_frame(file: &mut File, start: u64, end: u64) -> Vec<u8> {
    let mut frame = vec![0u8; (end - start) as usize];
    file.seek(SeekFrom::Start(start)).unwrap();
    file.read_exact(&mut frame).unwrap();
    frame
}

fn commits<T: Data + Default>(
    path: &Path,
    count: u64,
    parsers: &[Parser],
    mut change: impl FnMut(&mut T, u64),
) -> Tally {
    let db = Options::new()
        .durability(Durability::Buffered)
        .open::<T>(path)
        .unwrap();
    let mut file = File::open(path).unwrap();
    let mut tally = Tally::default();
    for i in 0..count {
        let start = db.log_bytes();
        db.update(|root| change(root, i)).unwrap();
        let end = db.log_bytes();
        split_frame(
            &read_frame(&mut file, start, end),
            start,
            parsers,
            &mut tally,
        );
    }
    tally
}

fn shape(path: &Path, start: u64, end: u64, n: u64) {
    let frame = read_frame(&mut File::open(path).unwrap(), start, end);
    let (mut nodes, mut interior, mut keys, mut children) = (0, 0, 0, 0);
    for rec in records(&frame, start) {
        if let Some(parts) = parse_node::<u64, u64>(&rec.body, &rec.links) {
            nodes += 1;
            keys += parts.keys.len();
            if parts.level > 0 {
                interior += 1;
                children += parts.children;
            }
        }
    }
    println!(
        "Map<u64, u64>, {n} keys loaded: {nodes} nodes of {:.2} keys, {:.2} children an interior node",
        keys as f64 / nodes.max(1) as f64,
        children as f64 / interior.max(1) as f64
    );
}

fn main() {
    let n = number("--n", 1_000_000);
    let count = number("--commits", 1_000);
    let workload = argument("--workload").unwrap_or_else(|| "all".into());
    let parent = PathBuf::from(argument("--dir").unwrap_or_else(|| "target/bench".into()));
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let all = workload == "all";
    if all || workload == "u64" {
        let path = dir.path().join("u64.ferro");
        let db = Options::new().open::<Map<u64, u64>>(&path).unwrap();
        let start = db.log_bytes();
        db.update(|root| {
            for i in 0..n {
                root.insert(mix(i), i);
            }
        })
        .unwrap();
        let end = db.log_bytes();
        drop(db);
        shape(&path, start, end, n);
        let parsers: [Parser; 1] = [parse_node::<u64, u64>];
        commits::<Map<u64, u64>>(&path, count, &parsers, |root, i| {
            root.insert(mix(n + i), i);
        })
        .report(&format!("Map<u64, u64>, {n} keys, insert"));
    }
    if all || workload == "string" {
        let path = dir.path().join("string.ferro");
        let db = Options::new().open::<Map<u64, String>>(&path).unwrap();
        db.update(|root| {
            for i in 0..n {
                root.insert(mix(i), format!("value-{i}"));
            }
        })
        .unwrap();
        drop(db);
        let parsers: [Parser; 1] = [parse_node::<u64, String>];
        commits::<Map<u64, String>>(&path, count, &parsers, |root, i| {
            root.insert(mix(n + i), format!("value-{}", n + i));
        })
        .report(&format!("Map<u64, String>, {n} keys, insert"));
    }
    if all || workload == "table" {
        let rows = n / 5;
        let path = dir.path().join("table.ferro");
        let db = Options::new().open::<Bank>(&path).unwrap();
        for chunk in 0..rows.div_ceil(10_000) {
            db.update(|bank| {
                for i in (chunk * 10_000)..((chunk + 1) * 10_000).min(rows) {
                    bank.accounts.insert(account(i));
                }
            })
            .unwrap();
        }
        drop(db);
        let parsers: [Parser; 3] = [
            parse_node::<u64, Account>,
            parse_node::<(Tier, u64), ()>,
            parse_node::<(String, u64), ()>,
        ];
        commits::<Bank>(&path, count, &parsers, |bank, i| {
            let id = mix(i) % rows;
            bank.accounts.get_mut(&id).unwrap().balance += 1;
        })
        .report(&format!("Table<Account>, {rows} rows, balance update"));
        commits::<Bank>(&path, count, &parsers, |bank, i| {
            bank.accounts.insert(account(rows + i));
        })
        .report(&format!("Table<Account>, {rows} rows, insert"));
    }
}
