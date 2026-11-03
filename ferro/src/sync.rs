use std::collections::{HashMap, HashSet, hash_map};

use crate::codec::{Data, Decoder, HASH_BYTES, Hash, Object, Ptr, Sink, push_varint};
use crate::db::{Db, Head, Refs, Snapshot};
use crate::error::{Error, Result};
use crate::log::{Chain, Entry, Merged, Node};
use crate::map::Map;
use crate::store::{Store, Txn};

const HAVE_MAGIC: &[u8; 8] = b"FERROHAV";
const BUNDLE_MAGIC: &[u8; 8] = b"FERROBDL";
const FORMAT: u8 = 1;
const COMPRESS_MIN: usize = 96;
const MAX_RATIO: usize = 255;
const MAIN: &str = "main";
const MAX_VERSION: u64 = u64::MAX >> 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Have(Vec<u8>);

impl Have {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Have {
    fn from(bytes: Vec<u8>) -> Self {
        Have(bytes)
    }
}

impl From<Have> for Vec<u8> {
    fn from(have: Have) -> Self {
        have.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle(Vec<u8>);

impl Bundle {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Bundle {
    fn from(bytes: Vec<u8>) -> Self {
        Bundle(bytes)
    }
}

impl From<Bundle> for Vec<u8> {
    fn from(bundle: Bundle) -> Self {
        bundle.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    Ahead,
    FastForward,
    Merged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pulled<R> {
    pub outcome: Outcome,
    pub value: Option<R>,
    pub version: u64,
    pub objects: u64,
    pub sent_bytes: u64,
    pub received_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mark {
    hash: Hash,
    version: u64,
}

struct Marks {
    marks: Vec<Mark>,
    tips: usize,
}

struct Wire {
    hash: Hash,
    body: Vec<u8>,
    links: Vec<(Hash, usize)>,
}

#[derive(Default)]
struct Plan {
    tip: Option<Mark>,
    bases: Vec<Mark>,
    roots: Vec<usize>,
    hints: Vec<(usize, usize)>,
    objects: Vec<Wire>,
}

fn bad(what: impl Into<String>) -> Error {
    Error::Bundle(what.into())
}

fn skip_version(version: u64) -> u64 {
    version - (version & version.wrapping_neg())
}

fn varint_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn open(bytes: &'a [u8], magic: &[u8; 8]) -> Result<Reader<'a>> {
        if bytes.len() < magic.len() + 1 || &bytes[..magic.len()] != magic {
            return Err(bad("not a ferro sync message"));
        }
        if bytes[magic.len()] != FORMAT {
            return Err(bad(format!(
                "format {} is not supported",
                bytes[magic.len()]
            )));
        }
        Ok(Reader {
            bytes,
            pos: magic.len() + 1,
        })
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(bad("truncated"));
        }
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for index in 0..10 {
            let byte = self.u8()?;
            if index == 9 && byte > 1 {
                break;
            }
            value |= u64::from(byte & 0x7f) << (7 * index);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(bad("varint overflow"))
    }

    fn size(&mut self) -> Result<usize> {
        usize::try_from(self.varint()?).map_err(|_| bad("size overflow"))
    }

    fn count(&mut self, each: usize) -> Result<usize> {
        let count = self.size()?;
        if count > self.remaining() / each.max(1) {
            return Err(bad("count runs past the end"));
        }
        Ok(count)
    }

    fn below(&mut self, limit: usize) -> Result<usize> {
        let value = self.size()?;
        if value >= limit {
            return Err(bad("index out of range"));
        }
        Ok(value)
    }

    fn hash(&mut self) -> Result<Hash> {
        Ok(self
            .take(HASH_BYTES)?
            .try_into()
            .expect("slice length is HASH_BYTES"))
    }

    fn mark(&mut self) -> Result<Mark> {
        Ok(Mark {
            hash: self.hash()?,
            version: self.varint()?,
        })
    }

    fn body(&mut self) -> Result<Vec<u8>> {
        let flagged = self.varint()?;
        let stored = usize::try_from(flagged >> 1).map_err(|_| bad("size overflow"))?;
        if flagged & 1 == 0 {
            return Ok(self.take(stored)?.to_vec());
        }
        let raw = self.size()?;
        if raw > stored.saturating_mul(MAX_RATIO).saturating_add(MAX_RATIO) {
            return Err(bad("implausible compression"));
        }
        let body = lz4_flex::block::decompress(self.take(stored)?, raw)
            .map_err(|error| bad(format!("bad compressed body: {error}")))?;
        if body.len() != raw {
            return Err(bad("compressed body has the wrong length"));
        }
        Ok(body)
    }

    fn finish(&self) -> Result<()> {
        if self.remaining() != 0 {
            return Err(bad("trailing bytes"));
        }
        Ok(())
    }
}

fn put_mark(out: &mut Vec<u8>, mark: Mark) {
    out.extend_from_slice(&mark.hash);
    push_varint(out, mark.version);
}

fn put_body(out: &mut Vec<u8>, body: &[u8]) {
    let packed = (body.len() >= COMPRESS_MIN)
        .then(|| lz4_flex::block::compress(body))
        .filter(|packed| packed.len() + varint_len(body.len() as u64) < body.len());
    match packed {
        Some(packed) => {
            push_varint(out, (packed.len() as u64) << 1 | 1);
            push_varint(out, body.len() as u64);
            out.extend_from_slice(&packed);
        }
        None => {
            push_varint(out, (body.len() as u64) << 1);
            out.extend_from_slice(body);
        }
    }
}

impl Marks {
    fn encode(&self) -> Vec<u8> {
        let mut out = HAVE_MAGIC.to_vec();
        out.push(FORMAT);
        push_varint(&mut out, self.tips as u64);
        push_varint(&mut out, self.marks.len() as u64);
        for mark in &self.marks {
            put_mark(&mut out, *mark);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Marks> {
        let mut reader = Reader::open(bytes, HAVE_MAGIC)?;
        let tips = reader.size()?;
        let count = reader.count(HASH_BYTES + 1)?;
        if tips > count {
            return Err(bad("more tips than commits"));
        }
        let marks = (0..count)
            .map(|_| reader.mark())
            .collect::<Result<Vec<_>>>()?;
        reader.finish()?;
        Ok(Marks { marks, tips })
    }
}

impl Plan {
    fn encode(&self) -> Vec<u8> {
        let mut out = BUNDLE_MAGIC.to_vec();
        out.push(FORMAT);
        match self.tip {
            Some(tip) => {
                out.push(1);
                put_mark(&mut out, tip);
            }
            None => out.push(0),
        }
        push_varint(&mut out, self.bases.len() as u64);
        for base in &self.bases {
            put_mark(&mut out, *base);
        }
        push_varint(&mut out, self.roots.len() as u64);
        for root in &self.roots {
            push_varint(&mut out, *root as u64);
        }
        push_varint(&mut out, self.hints.len() as u64);
        for (parent, index) in &self.hints {
            push_varint(&mut out, *parent as u64);
            push_varint(&mut out, *index as u64);
        }
        push_varint(&mut out, self.objects.len() as u64);
        for object in &self.objects {
            put_body(&mut out, &object.body);
            push_varint(&mut out, object.links.len() as u64);
            for (hash, reference) in &object.links {
                out.extend_from_slice(hash);
                push_varint(&mut out, *reference as u64);
            }
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Plan> {
        let mut reader = Reader::open(bytes, BUNDLE_MAGIC)?;
        let tip = match reader.u8()? {
            0 => None,
            1 => Some(reader.mark()?),
            _ => return Err(bad("bad head flag")),
        };
        let count = reader.count(HASH_BYTES + 1)?;
        let bases = (0..count)
            .map(|_| reader.mark())
            .collect::<Result<Vec<_>>>()?;
        let count = reader.count(1)?;
        let roots = (0..count)
            .map(|_| reader.below(bases.len()))
            .collect::<Result<Vec<_>>>()?;
        let count = reader.count(2)?;
        let mut hints = Vec::with_capacity(count);
        for hint in 0..count {
            let parent = reader.below(roots.len() + hint)?;
            hints.push((parent, reader.size()?));
        }
        let references = 1 + roots.len() + hints.len();
        let count = reader.count(2)?;
        let mut objects = Vec::with_capacity(count);
        for _ in 0..count {
            let body = reader.body()?;
            let links = reader.count(HASH_BYTES + 1)?;
            let mut wire = Wire {
                hash: [0; HASH_BYTES],
                body,
                links: Vec::with_capacity(links),
            };
            for _ in 0..links {
                let hash = reader.hash()?;
                wire.links.push((hash, reader.below(references)?));
            }
            let object = Object {
                body: wire.body,
                links: wire
                    .links
                    .iter()
                    .map(|(hash, _)| Ptr {
                        hash: *hash,
                        offset: 0,
                    })
                    .collect(),
            };
            wire.hash = object.hash();
            wire.body = object.body;
            objects.push(wire);
        }
        reader.finish()?;
        if tip.is_none() && !(bases.is_empty() && objects.is_empty()) {
            return Err(bad("it carries objects but no head"));
        }
        Ok(Plan {
            tip,
            bases,
            roots,
            hints,
            objects,
        })
    }
}

fn corrupt(what: &str) -> Error {
    Error::Corrupt(format!("commit log {what}"))
}

fn entry_at(store: &Store, chain: &Chain, version: u64) -> Result<Option<Ptr>> {
    let Some(mut ptr) = chain.tip else {
        return Ok(None);
    };
    if version < chain.first || version > chain.last {
        return Ok(None);
    }
    loop {
        let entry = store.load::<Entry>(ptr)?;
        let at = entry.commit.version;
        if at == version {
            return Ok(Some(ptr));
        }
        if at < version {
            return Err(corrupt("skipped a version"));
        }
        ptr = match entry.skip {
            Some(skip) if skip_version(at) >= version => skip,
            _ => entry.prev.ok_or_else(|| corrupt("chain ends early"))?,
        };
    }
}

fn hash_at(store: &Store, chain: &Chain, version: u64) -> Result<Option<Hash>> {
    Ok(entry_at(store, chain, version)?.map(|ptr| ptr.hash))
}

fn shared_prefix(store: &Store, a: &Chain, b: &Chain) -> Result<Option<u64>> {
    if a.tip.is_none() || b.tip.is_none() {
        return Ok(None);
    }
    let (low, high) = (a.first.max(b.first), a.last.min(b.last));
    if low > high {
        return Ok(None);
    }
    let same = |version: u64| -> Result<bool> {
        let ours = hash_at(store, a, version)?;
        Ok(ours.is_some() && ours == hash_at(store, b, version)?)
    };
    if same(high)? {
        return Ok(Some(high));
    }
    if !same(low)? {
        return Ok(None);
    }
    let (mut low, mut high) = (low, high);
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if same(middle)? {
            low = middle;
        } else {
            high = middle;
        }
    }
    Ok(Some(low))
}

fn locate(store: &Store, refs: &Refs, mark: Mark) -> Result<Option<(Ptr, Chain)>> {
    let mut found: Option<(Ptr, Chain)> = None;
    for (_, head) in refs.iter() {
        if found.is_some_and(|(_, chain)| chain.first <= head.log.first) {
            continue;
        }
        if let Some(ptr) = entry_at(store, &head.log, mark.version)?
            && ptr.hash == mark.hash
        {
            let chain = Chain {
                tip: Some(ptr),
                first: head.log.first,
                last: mark.version,
            };
            found = Some((ptr, chain));
        }
    }
    Ok(found)
}

fn read_entry(object: &Object) -> Result<Entry> {
    Entry::decode(&mut Decoder::new(&object.body, &object.links, None))
}

#[derive(Clone, Copy)]
enum Origin {
    Root(usize),
    Child(Hash, usize),
}

struct Difference {
    known: HashMap<Hash, Origin>,
    send: HashMap<Hash, Object>,
}

fn difference(store: &Store, old_roots: &[Ptr], new_roots: Vec<Ptr>) -> Result<Difference> {
    let mut found = Difference {
        known: HashMap::new(),
        send: HashMap::new(),
    };
    for (slot, root) in old_roots.iter().enumerate() {
        found.known.entry(root.hash).or_insert(Origin::Root(slot));
    }
    let mut previous = old_roots.to_vec();
    for root in new_roots {
        found.step(store, previous, root)?;
        previous = vec![root];
    }
    Ok(found)
}

impl Difference {
    fn classified(&self, hash: &Hash) -> bool {
        self.known.contains_key(hash) || self.send.contains_key(hash)
    }

    fn step(&mut self, store: &Store, mut old: Vec<Ptr>, root: Ptr) -> Result<()> {
        let mut seen = HashSet::from([root.hash]);
        let mut expanded = HashSet::new();
        let mut fresh = vec![root];
        loop {
            fresh.retain(|ptr| !self.classified(&ptr.hash));
            if fresh.is_empty() {
                return Ok(());
            }
            let mut deeper = Vec::new();
            for ptr in old.drain(..) {
                if seen.contains(&ptr.hash) || !expanded.insert(ptr.hash) {
                    continue;
                }
                let links = match self.send.get(&ptr.hash) {
                    Some(object) => object.links.clone(),
                    None => store.read_object(ptr)?.links,
                };
                for (index, link) in links.into_iter().enumerate() {
                    if link.offset == 0 {
                        continue;
                    }
                    if !self.classified(&link.hash) {
                        self.known.insert(link.hash, Origin::Child(ptr.hash, index));
                    }
                    deeper.push(link);
                }
            }
            old = deeper;
            let mut next = Vec::new();
            for ptr in fresh.drain(..) {
                if self.classified(&ptr.hash) {
                    continue;
                }
                if ptr.offset == 0 {
                    return Err(bad(
                        "the branch links to pruned history outside its commit log",
                    ));
                }
                let object = store.read_object(ptr)?;
                for link in &object.links {
                    if seen.insert(link.hash) {
                        next.push(*link);
                    }
                }
                self.send.insert(ptr.hash, object);
            }
            fresh = next;
        }
    }
}

fn post_order(pool: &HashMap<Hash, Object>, start: Hash) -> Vec<Hash> {
    let mut order = Vec::with_capacity(pool.len());
    if !pool.contains_key(&start) {
        return order;
    }
    let mut done = HashSet::new();
    let mut open = HashSet::from([start]);
    let mut stack = vec![(start, 0usize)];
    while let Some((hash, next)) = stack.last_mut() {
        let links = &pool[&*hash].links;
        if *next < links.len() {
            let link = links[*next].hash;
            *next += 1;
            if pool.contains_key(&link) && !done.contains(&link) && open.insert(link) {
                stack.push((link, 0));
            }
            continue;
        }
        let hash = *hash;
        stack.pop();
        open.remove(&hash);
        done.insert(hash);
        order.push(hash);
    }
    order
}

struct Known {
    mark: Mark,
    ptr: Ptr,
    chain: Chain,
    tip: bool,
}

fn plan(store: &Store, refs: &Refs, name: &str, have: &Marks) -> Result<Plan> {
    let chain = refs.try_get(name)?.map(|head| head.log);
    let Some(chain) = chain.filter(|chain| chain.tip.is_some()) else {
        return Ok(Plan::default());
    };
    let tip = chain.tip.expect("chain has a tip");
    let mut known = Vec::new();
    for (index, mark) in have.marks.iter().enumerate() {
        if let Some((ptr, base)) = locate(store, refs, *mark)? {
            known.push(Known {
                mark: *mark,
                ptr,
                chain: base,
                tip: index < have.tips,
            });
        }
    }
    let mut prefix: Option<(u64, usize)> = None;
    for (slot, base) in known.iter().enumerate() {
        if let Some(version) = shared_prefix(store, &chain, &base.chain)?
            && prefix.is_none_or(|(best, _)| version > best)
        {
            prefix = Some((version, slot));
        }
    }
    let floor = prefix
        .map_or(0, |(version, _)| version)
        .max(chain.first.saturating_sub(1));
    let mut entries = Vec::new();
    let mut next = Some(tip);
    while let Some(ptr) = next {
        let object = store.read_object(ptr)?;
        let entry = read_entry(&object)?;
        let at = entry.commit.version;
        if at <= floor {
            break;
        }
        next = entry.prev.filter(|_| at > chain.first);
        entries.push((ptr, object, entry));
    }
    let mut out = Plan {
        tip: Some(Mark {
            hash: tip.hash,
            version: chain.last,
        }),
        ..Plan::default()
    };
    let mut slots: HashMap<usize, usize> = HashMap::new();
    let mut base_slot = |slot: usize, bases: &mut Vec<Mark>| -> usize {
        *slots.entry(slot).or_insert_with(|| {
            bases.push(known[slot].mark);
            bases.len() - 1
        })
    };
    let mut old_roots: Vec<Ptr> = Vec::new();
    for (slot, base) in known.iter().enumerate() {
        if !base.tip && prefix.map(|(_, chosen)| chosen) != Some(slot) {
            continue;
        }
        let entry = store.load::<Entry>(base.ptr)?;
        let Some(root) = entry.commit.root else {
            continue;
        };
        if old_roots.iter().any(|old| old.hash == root.hash) {
            continue;
        }
        out.roots.push(base_slot(slot, &mut out.bases));
        old_roots.push(root);
    }
    let extra = closures(store, &chain, &entries, &known, &mut out.bases)?;
    let mut new_roots = Vec::new();
    for (_, _, entry) in entries.iter().rev().chain(&extra) {
        new_roots.extend(entry.commit.root);
    }
    let Difference { known: old, send } = difference(store, &old_roots, new_roots)?;
    let mut pool = send;
    for (ptr, object, _) in entries.into_iter().chain(extra) {
        pool.insert(ptr.hash, object);
    }
    let order = post_order(&pool, tip.hash);
    if order.len() != pool.len() {
        return Err(Error::Corrupt(
            "sync found objects its head does not reach".into(),
        ));
    }
    let mut ids: HashMap<Hash, usize> = HashMap::new();
    let mut emitted: HashSet<Hash> = HashSet::with_capacity(order.len());
    for hash in order {
        let object = pool.remove(&hash).expect("ordered objects are pooled");
        let mut links = Vec::with_capacity(object.links.len());
        for link in &object.links {
            let reference = if emitted.contains(&link.hash) {
                0
            } else {
                match old.get(&link.hash) {
                    Some(Origin::Root(slot)) => 1 + slot,
                    Some(Origin::Child(..)) => {
                        let id =
                            hint_for(link.hash, &old, &mut ids, &mut out.hints, old_roots.len());
                        1 + old_roots.len() + id
                    }
                    None => 0,
                }
            };
            links.push((link.hash, reference));
        }
        emitted.insert(hash);
        out.objects.push(Wire {
            hash,
            body: object.body,
            links,
        });
    }
    Ok(out)
}

fn closures(
    store: &Store,
    chain: &Chain,
    entries: &[(Ptr, Object, Entry)],
    known: &[Known],
    bases: &mut Vec<Mark>,
) -> Result<Vec<(Ptr, Object, Entry)>> {
    let after = chain.after(store)?;
    let mut sending: HashSet<Hash> = entries.iter().map(|(ptr, _, _)| ptr.hash).collect();
    let mut seeds: Vec<(Merged, Option<u64>)> = entries
        .iter()
        .filter_map(|(_, _, entry)| entry.merged.map(|merged| (merged, after)))
        .collect();
    let mut extra = Vec::new();
    while let Some((merged, path)) = seeds.pop() {
        let (ptr, after) = (merged.ptr, merged.after(path));
        let marked = |hash: &Hash| bases.iter().any(|mark| mark.hash == *hash);
        if ptr.offset == 0 || sending.contains(&ptr.hash) || marked(&ptr.hash) {
            continue;
        }
        let node = Node::load(store, ptr)?;
        let version = node.version();
        if known.iter().any(|held| held.mark.hash == ptr.hash) {
            bases.push(Mark {
                hash: ptr.hash,
                version,
            });
            continue;
        }
        let bare = after.is_some_and(|after| node.time() < after);
        let low = match after {
            _ if bare => version,
            Some(after) => {
                node.lowest_kept(store, merged.floor, |entry| entry.commit.time_ms >= after)?
            }
            None => node.lowest_kept(store, merged.floor, |_| true)?,
        };
        let reach = Chain {
            tip: Some(ptr),
            first: low,
            last: version,
        };
        let mut held = None;
        for base in known {
            held = held.max(shared_prefix(store, &reach, &base.chain)?);
        }
        let stop = held.map_or(low, |held| low.max(held + 1)).min(version);
        let mut next = Some(ptr);
        while let Some(at) = next.take() {
            if at.offset == 0 || sending.contains(&at.hash) {
                break;
            }
            let object = store.read_object(at)?;
            let entry = read_entry(&object)?;
            let version = entry.commit.version;
            if version < stop {
                break;
            }
            sending.insert(at.hash);
            if let Some(nested) = entry.merged.filter(|_| !bare) {
                seeds.push((nested, after));
            }
            next = entry.prev.filter(|_| version > stop);
            extra.push((at, object, entry));
        }
    }
    Ok(extra)
}

fn hint_for(
    hash: Hash,
    old: &HashMap<Hash, Origin>,
    ids: &mut HashMap<Hash, usize>,
    hints: &mut Vec<(usize, usize)>,
    roots: usize,
) -> usize {
    let mut path = Vec::new();
    let mut at = hash;
    let parent = loop {
        if let Some(id) = ids.get(&at) {
            break roots + id;
        }
        match old[&at] {
            Origin::Root(slot) => break slot,
            Origin::Child(parent, index) => {
                path.push((at, index));
                at = parent;
            }
        }
    };
    let mut parent = parent;
    for (child, index) in path.into_iter().rev() {
        hints.push((parent, index));
        let id = hints.len() - 1;
        ids.insert(child, id);
        parent = roots + id;
    }
    ids[&hash]
}

#[derive(Clone, Copy)]
struct Facts {
    version: u64,
    time: u64,
    generation: u64,
    reach: u64,
    last_merge: u64,
    bounded: bool,
}

impl Facts {
    fn of(entry: &Entry) -> Facts {
        Facts {
            version: entry.commit.version,
            time: entry.commit.time_ms,
            generation: entry.generation(),
            reach: entry.graph.reach,
            last_merge: entry.graph.last_merge,
            bounded: entry.graph.bounded,
        }
    }
}

fn check_graph(
    entry: &Entry,
    prev: Option<Option<Facts>>,
    merged: Option<Option<Facts>>,
) -> Result<()> {
    let own = Facts::of(entry);
    let known = prev.is_none_or(|facts| facts.is_some()) && merged.is_none_or(|f| f.is_some());
    let (prev, linked) = (prev.flatten(), merged.flatten());
    let generation = |facts: Option<Facts>| facts.map_or(0, |facts| facts.generation);
    let parents = generation(prev).max(generation(linked));
    let reach = prev.map_or(0, |facts| facts.reach).max(linked.map_or(0, |facts| facts.generation));
    let link = entry.merged;
    let bounded = prev.is_some_and(|facts| facts.bounded)
        || link.is_some_and(|link| link.floor > 1 || link.after.is_some())
        || linked.is_some_and(|facts| facts.bounded);
    let last_merge = match (link, prev) {
        (Some(_), _) => Some(own.version),
        (None, Some(prev)) => Some(prev.last_merge),
        (None, None) if known => Some(0),
        (None, None) => None,
    };
    let consistent = if known {
        own.generation == parents + 1 && own.reach == reach && own.bounded == bounded
    } else {
        own.generation > parents && own.reach >= reach && (own.bounded || !bounded)
    };
    let later = prev.is_none_or(|facts| own.time >= facts.time)
        && linked.is_none_or(|facts| own.time >= facts.time);
    let floored = match (link, linked) {
        (Some(link), Some(facts)) => link.floor <= facts.version,
        _ => true,
    };
    if !consistent || !later || !floored || last_merge.is_some_and(|last| own.last_merge != last) {
        return Err(bad(format!(
            "v{} does not agree with the commits it links to",
            own.version
        )));
    }
    Ok(())
}

enum Below {
    Joins(Hash),
    Chain(Chain),
    Gone,
}

struct Segment {
    versions: HashMap<u64, Hash>,
    bottom: u64,
    below: Below,
}

enum Found {
    Sent(Hash),
    Held(Ptr),
    Gone,
}

struct Landing<'a> {
    store: &'a Store,
    segments: Vec<Segment>,
    decoded: HashMap<Hash, (usize, usize)>,
    entries: HashMap<usize, Entry>,
}

impl Landing<'_> {
    fn find(&self, mut segment: usize, version: u64) -> Result<Found> {
        loop {
            let seg = &self.segments[segment];
            if version >= seg.bottom {
                return seg
                    .versions
                    .get(&version)
                    .map(|hash| Found::Sent(*hash))
                    .ok_or_else(|| bad("a commit links past the top of its chain"));
            }
            match &seg.below {
                Below::Joins(hash) => segment = self.decoded[hash].1,
                Below::Chain(chain) => {
                    let found = entry_at(self.store, chain, version)?;
                    return Ok(found.map_or(Found::Gone, Found::Held));
                }
                Below::Gone => return Ok(Found::Gone),
            }
        }
    }

    fn facts(&self, ptr: Ptr) -> Result<Option<Facts>> {
        if ptr.offset == 0 {
            return Ok(None);
        }
        match self.decoded.get(&ptr.hash) {
            Some((slot, _)) => Ok(Some(Facts::of(&self.entries[slot]))),
            None => Ok(Some(Facts::of(&*self.store.load::<Entry>(ptr)?))),
        }
    }

    fn segment(
        &mut self,
        refs: &Refs,
        plan: &Plan,
        index: &HashMap<Hash, usize>,
        top: Hash,
    ) -> Result<Vec<Hash>> {
        let id = self.segments.len();
        let mut versions = HashMap::new();
        let mut merged = Vec::new();
        let (mut at, mut expected) = (top, None);
        let below = loop {
            let slot = index[&at];
            let entry = decode_entry(plan, slot)?;
            let version = entry.commit.version;
            let skipped = expected.is_some_and(|want| want != version);
            if version == 0 || version > MAX_VERSION || skipped {
                return Err(bad("a merged chain's versions do not descend one at a time"));
            }
            versions.insert(version, at);
            self.decoded.insert(at, (slot, id));
            merged.extend(entry.merged.map(|link| link.ptr.hash));
            let prev = entry.prev;
            self.entries.insert(slot, entry);
            let Some(prev) = prev else {
                if version != 1 {
                    return Err(bad(format!("v{version} has no parent")));
                }
                break Below::Gone;
            };
            if let Some((slot, _)) = self.decoded.get(&prev.hash) {
                if self.entries[slot].commit.version != version - 1 {
                    return Err(bad("a merged chain joins another at the wrong version"));
                }
                break Below::Joins(prev.hash);
            }
            if index.contains_key(&prev.hash) {
                (at, expected) = (prev.hash, Some(version - 1));
                continue;
            }
            let mark = Mark {
                hash: prev.hash,
                version: version - 1,
            };
            break match locate(self.store, refs, mark)? {
                Some((_, chain)) => Below::Chain(chain),
                None => Below::Gone,
            };
        };
        let bottom = versions.keys().copied().min().unwrap_or(1);
        self.segments.push(Segment {
            versions,
            bottom,
            below,
        });
        Ok(merged)
    }
}

fn decode_entry(plan: &Plan, slot: usize) -> Result<Entry> {
    let wire = &plan.objects[slot];
    let links: Vec<Ptr> = wire
        .links
        .iter()
        .map(|(hash, _)| Ptr {
            hash: *hash,
            offset: 0,
        })
        .collect();
    let entry = Entry::decode(&mut Decoder::new(&wire.body, &links, None))
        .map_err(|error| bad(format!("an object it links as a commit is not one: {error}")))?;
    let used = usize::from(entry.commit.root.is_some())
        + usize::from(entry.prev.is_some())
        + usize::from(entry.skip.is_some())
        + usize::from(entry.merged.is_some());
    if used != links.len() {
        return Err(bad("a commit carries links it does not use"));
    }
    Ok(entry)
}

fn land<T: Data>(
    store: &Store,
    txn: &mut Txn,
    refs: &Refs,
    remote: &str,
    plan: Plan,
) -> Result<Option<Refs>> {
    let Some(tip) = plan.tip else {
        return Ok(None);
    };
    if tip.version > MAX_VERSION {
        return Err(bad(format!(
            "its head is at v{}, past what a commit log can reach",
            tip.version
        )));
    }
    let mut bases = Vec::with_capacity(plan.bases.len());
    for mark in &plan.bases {
        let found = locate(store, refs, *mark)?.ok_or_else(|| {
            bad(format!(
                "it builds on v{} {}, which this database does not have",
                mark.version,
                hex(&mark.hash)
            ))
        })?;
        bases.push(found);
    }
    let mut index: HashMap<Hash, usize> = HashMap::with_capacity(plan.objects.len());
    for (slot, wire) in plan.objects.iter().enumerate() {
        if index.insert(wire.hash, slot).is_some() {
            return Err(bad("it repeats an object"));
        }
    }
    let mut entries: HashMap<usize, Entry> = HashMap::new();
    let mut decoded: HashMap<Hash, (usize, usize)> = HashMap::new();
    let mut versions: HashMap<u64, Hash> = HashMap::new();
    let mut below = Some(tip);
    while let Some(mark) = below {
        let Some(&slot) = index.get(&mark.hash) else {
            break;
        };
        let entry = decode_entry(&plan, slot)
            .map_err(|error| bad(format!("v{} is not a commit: {error}", mark.version)))?;
        if mark.version == 0 || entry.commit.version != mark.version {
            return Err(bad("its commit versions do not descend one at a time"));
        }
        below = match entry.prev {
            Some(prev) => Some(Mark {
                hash: prev.hash,
                version: mark.version - 1,
            }),
            None if mark.version == 1 => None,
            None => return Err(bad(format!("v{} has no parent", mark.version))),
        };
        versions.insert(mark.version, mark.hash);
        decoded.insert(mark.hash, (slot, 0));
        entries.insert(slot, entry);
    }
    let low = tip
        .version
        .saturating_sub(entries.len() as u64)
        .saturating_add(1);
    let mut prefix = None;
    let mut first = low;
    let mut tip_ptr = None;
    if let Some(mark) = below {
        match locate(store, refs, mark)? {
            Some((ptr, chain)) => {
                first = chain.first;
                prefix = Some(chain);
                if entries.is_empty() {
                    tip_ptr = Some(ptr);
                }
            }
            None if entries.is_empty() => {
                return Err(bad("its head is neither in it nor in this database"));
            }
            None => {}
        }
    }
    let mut landing = Landing {
        store,
        segments: vec![Segment {
            versions,
            bottom: low,
            below: prefix.map_or(Below::Gone, Below::Chain),
        }],
        decoded,
        entries,
    };
    let mut pending: Vec<Hash> = landing
        .entries
        .values()
        .filter_map(|entry| entry.merged.map(|link| link.ptr.hash))
        .collect();
    while let Some(top) = pending.pop() {
        if landing.decoded.contains_key(&top) || !index.contains_key(&top) {
            continue;
        }
        pending.extend(landing.segment(refs, &plan, &index, top)?);
    }
    let mut roots = Vec::with_capacity(plan.roots.len());
    for slot in &plan.roots {
        let entry = store.load::<Entry>(bases[*slot].0)?;
        roots.push(
            entry
                .commit
                .root
                .ok_or_else(|| bad("a base it names has no root"))?,
        );
    }
    let mut hinted: Vec<Ptr> = Vec::with_capacity(plan.hints.len());
    let mut parents: HashMap<Ptr, Vec<Ptr>> = HashMap::new();
    for (parent, slot) in &plan.hints {
        let from = if *parent < roots.len() {
            roots[*parent]
        } else {
            hinted[*parent - roots.len()]
        };
        let links = match parents.entry(from) {
            hash_map::Entry::Occupied(found) => found.into_mut(),
            hash_map::Entry::Vacant(empty) => empty.insert(store.read_object(from)?.links),
        };
        let link = *links
            .get(*slot)
            .ok_or_else(|| bad("a hint names a link that does not exist"))?;
        if link.offset == 0 {
            return Err(bad("a hint leads into pruned history"));
        }
        hinted.push(link);
    }
    let commit_roots: HashSet<Hash> = landing
        .entries
        .values()
        .filter_map(|entry| entry.commit.root.map(|root| root.hash))
        .collect();
    let based: HashMap<Hash, Ptr> = bases.iter().map(|(ptr, _)| (ptr.hash, *ptr)).collect();
    let mut written: HashMap<Hash, Ptr> = HashMap::with_capacity(plan.objects.len());
    let mut used = vec![false; plan.objects.len()];
    let mut tip_root = None;
    for (slot, wire) in plan.objects.into_iter().enumerate() {
        let mut links = Vec::with_capacity(wire.links.len());
        let sent = |hash: &Hash, used: &mut Vec<bool>| -> Result<Ptr> {
            let ptr = written
                .get(hash)
                .copied()
                .ok_or_else(|| bad("an object links to one that was neither sent nor known"))?;
            used[index[hash]] = true;
            Ok(ptr)
        };
        let value = |hash: &Hash, reference: usize, used: &mut Vec<bool>| -> Result<Ptr> {
            let ptr = match reference {
                0 => sent(hash, used)?,
                _ if reference <= roots.len() => roots[reference - 1],
                _ => hinted[reference - 1 - roots.len()],
            };
            if ptr.hash != *hash {
                return Err(bad("a link does not match the object it names"));
            }
            Ok(ptr)
        };
        let commit = landing.decoded.get(&wire.hash).filter(|(found, _)| *found == slot);
        let Some(&(_, segment)) = commit else {
            for (hash, reference) in &wire.links {
                links.push(value(hash, *reference, &mut used)?);
            }
            let object = Object {
                body: wire.body,
                links,
            };
            if commit_roots.contains(&wire.hash) {
                T::decode(&mut Decoder::new(&object.body, &object.links, Some(store))).map_err(
                    |error| bad(format!("a commit's root is not this database's type: {error}")),
                )?;
            }
            written.insert(wire.hash, txn.put(object, wire.hash)?);
            continue;
        };
        let entry = &landing.entries[&slot];
        let version = entry.commit.version;
        let mut wires = wire.links.iter();
        if entry.commit.root.is_some() {
            let (hash, reference) = wires.next().ok_or_else(|| bad("a commit lost its root"))?;
            links.push(value(hash, *reference, &mut used)?);
        }
        let chained = |want: Ptr, at: u64, used: &mut Vec<bool>| -> Result<Ptr> {
            match landing.find(segment, at)? {
                Found::Sent(hash) if hash == want.hash => {
                    let ptr = written
                        .get(&hash)
                        .copied()
                        .ok_or_else(|| bad("a commit links to one sent after it"))?;
                    used[index[&hash]] = true;
                    Ok(ptr)
                }
                Found::Held(ptr) if ptr.hash == want.hash => Ok(ptr),
                Found::Gone => Ok(Ptr {
                    hash: want.hash,
                    offset: 0,
                }),
                Found::Held(_) if segment == 0 && at < low => Err(bad(format!(
                    "v{at}, which it builds on, does not match this database"
                ))),
                _ => Err(bad(
                    "a commit links to one that is not the commit at that version",
                )),
            }
        };
        let mut prev_facts = None;
        if let Some(prev) = entry.prev {
            let ptr = chained(prev, version - 1, &mut used)?;
            prev_facts = Some(landing.facts(ptr)?);
            links.push(ptr);
        }
        if let Some(skip) = entry.skip {
            links.push(chained(skip, skip_version(version), &mut used)?);
        }
        let mut merged_facts = None;
        if let Some(link) = entry.merged {
            let hash = link.ptr.hash;
            let ptr = if landing.decoded.contains_key(&hash) {
                let ptr = written
                    .get(&hash)
                    .copied()
                    .ok_or_else(|| bad("a merge links to a commit sent after it"))?;
                used[index[&hash]] = true;
                ptr
            } else {
                based.get(&hash).copied().unwrap_or(Ptr { hash, offset: 0 })
            };
            merged_facts = Some(landing.facts(ptr)?);
            links.push(ptr);
        }
        check_graph(entry, prev_facts, merged_facts)?;
        if wire.hash == tip.hash {
            tip_root = entry
                .commit
                .root
                .and_then(|root| links.iter().copied().find(|link| link.hash == root.hash));
        }
        let object = Object {
            body: wire.body,
            links,
        };
        let ptr = txn.put(object, wire.hash)?;
        written.insert(wire.hash, ptr);
    }
    if let Some(slot) = index.get(&tip.hash) {
        used[*slot] = true;
    }
    if used.iter().any(|used| !used) {
        return Err(bad("it carries objects nothing links to"));
    }
    let tip_ptr = match tip_ptr {
        Some(ptr) => ptr,
        None => written[&tip.hash],
    };
    let root = if index.contains_key(&tip.hash) {
        tip_root
    } else {
        store.load::<Entry>(tip_ptr)?.commit.root
    };
    let head = Head {
        version: tip.version,
        root,
        log: Chain {
            tip: Some(tip_ptr),
            first,
            last: tip.version,
        },
        merged: Map::new(),
    };
    let existing = refs.try_get(remote)?;
    if let Some(old) = &existing
        && (old.log == head.log || hash_at(store, &old.log, tip.version)? == Some(tip.hash))
    {
        return Ok(None);
    }
    let mut next = refs.clone();
    let continues = match existing
        .as_ref()
        .and_then(|old| old.log.tip.map(|ptr| (ptr.hash, old.log.last)))
    {
        None => false,
        Some((hash, version)) => match landing.find(0, version) {
            Ok(Found::Sent(found)) => found == hash,
            Ok(Found::Held(ptr)) => ptr.hash == hash,
            _ => false,
        },
    };
    if !continues {
        let heads: Vec<_> = next.iter().collect();
        for (name, head) in heads {
            if head.merged.try_get(remote)?.is_some() {
                let mut head = (*head).clone();
                head.merged.remove(remote);
                next.insert(name, head);
            }
        }
    }
    next.insert(remote.to_string(), head);
    Ok(Some(next))
}

fn hex(hash: &Hash) -> String {
    hash.iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    UpToDate,
    Equal,
    Ahead,
    FastForward,
    Diverged,
}

fn decide(store: &Store, ours: Option<&Head>, theirs: &Head) -> Result<Decision> {
    let Some(their_tip) = theirs.log.tip else {
        return Ok(Decision::UpToDate);
    };
    let Some(ours) = ours.filter(|ours| ours.log.tip.is_some()) else {
        return Ok(Decision::FastForward);
    };
    let our_tip = ours.log.tip.expect("filtered on a tip");
    if our_tip.hash == their_tip.hash {
        return Ok(Decision::UpToDate);
    }
    if hash_at(store, &theirs.log, ours.log.last)? == Some(our_tip.hash) {
        return Ok(Decision::FastForward);
    }
    if hash_at(store, &ours.log, theirs.log.last)? == Some(their_tip.hash) {
        return Ok(Decision::Ahead);
    }
    if ours.root.map(|ptr| ptr.hash) == theirs.root.map(|ptr| ptr.hash) {
        return Ok(Decision::Equal);
    }
    Ok(Decision::Diverged)
}

fn fast_forward(ours: Option<&Head>, theirs: &Head) -> Head {
    let first = match ours.filter(|ours| ours.log.tip.is_some()) {
        Some(ours) => ours.log.first.max(theirs.log.first),
        None => theirs.log.first,
    };
    Head {
        version: theirs.version,
        root: theirs.root,
        log: Chain {
            first,
            ..theirs.log
        },
        merged: ours.map(|ours| ours.merged.clone()).unwrap_or_default(),
    }
}

fn check_remote(remote: &str, branch: &str) -> Result<()> {
    if remote == MAIN || remote == branch {
        return Err(Error::BranchInUse(remote.to_string()));
    }
    Ok(())
}

impl<T: Data> Db<T> {
    pub fn have(&self) -> Result<Have> {
        let (store, refs) = self.published_refs();
        let mut marks = Vec::new();
        for (_, head) in refs.iter() {
            if let Some(tip) = head.log.tip {
                marks.push(Mark {
                    hash: tip.hash,
                    version: head.log.last,
                });
            }
        }
        let tips = marks.len();
        if let Some(head) = refs.try_get(self.branch_name())?
            && head.log.tip.is_some()
        {
            let chain = head.log;
            let (mut version, mut step) = (chain.last, 1u64);
            while version > chain.first {
                version = version.saturating_sub(step).max(chain.first);
                if let Some(ptr) = entry_at(&store, &chain, version)? {
                    marks.push(Mark {
                        hash: ptr.hash,
                        version,
                    });
                }
                step = step.saturating_mul(2);
            }
        }
        Ok(Have(Marks { marks, tips }.encode()))
    }

    pub fn bundle(&self, have: &Have) -> Result<Bundle> {
        let have = Marks::decode(have.as_bytes())?;
        let (store, refs) = self.published_refs();
        Ok(Bundle(
            plan(&store, &refs, self.branch_name(), &have)?.encode(),
        ))
    }

    pub fn pull<R, S: Into<String>>(
        &self,
        from: &Db<T>,
        remote: &str,
        merge: impl FnOnce(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Pulled<R>> {
        check_remote(remote, self.branch_name())?;
        let have = self.have()?;
        let bundle = from.bundle(&have)?;
        let mut pulled = self.receive(remote, &bundle, merge)?;
        pulled.sent_bytes = have.as_bytes().len() as u64;
        Ok(pulled)
    }

    pub fn push<R, S: Into<String>>(
        &self,
        to: &Db<T>,
        remote: &str,
        merge: impl FnOnce(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Pulled<R>> {
        to.pull(self, remote, merge)
    }

    pub fn receive<R, S: Into<String>>(
        &self,
        remote: &str,
        bundle: &Bundle,
        merge: impl FnOnce(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Pulled<R>> {
        check_remote(remote, self.branch_name())?;
        let plan = Plan::decode(bundle.as_bytes())?;
        let objects = plan.objects.len() as u64;
        let landing = plan.tip.is_some();
        if landing {
            self.rewrite_refs(|store, txn, refs| {
                Ok((land::<T>(store, txn, refs, remote, plan)?, ()))
            })?;
        }
        let (outcome, value) = if landing {
            self.integrate(remote, merge)?
        } else {
            (Outcome::UpToDate, None)
        };
        Ok(Pulled {
            outcome,
            value,
            version: self.version(),
            objects,
            sent_bytes: 0,
            received_bytes: bundle.as_bytes().len() as u64,
        })
    }

    fn integrate<R, S: Into<String>>(
        &self,
        remote: &str,
        merge: impl FnOnce(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<(Outcome, Option<R>)> {
        let name = self.branch_name().to_string();
        let decision = self.rewrite_refs(|store, _, refs| {
            let theirs = refs
                .try_get(remote)?
                .ok_or_else(|| Error::NoBranch(remote.to_string()))?;
            let ours = refs.try_get(&name)?;
            let decision = decide(store, ours.as_deref(), &theirs)?;
            let head = match decision {
                Decision::Diverged | Decision::Equal => return Ok((None, decision)),
                Decision::FastForward => fast_forward(ours.as_deref(), &theirs),
                Decision::UpToDate | Decision::Ahead => match ours {
                    Some(ours) => (*ours).clone(),
                    None => return Ok((None, decision)),
                },
            };
            Ok((
                recorded(refs, &name, head, remote, theirs.version)?,
                decision,
            ))
        })?;
        let outcome = match decision {
            Decision::UpToDate | Decision::Equal => Outcome::UpToDate,
            Decision::Ahead => Outcome::Ahead,
            Decision::FastForward => Outcome::FastForward,
            Decision::Diverged => return self.merge_remote(remote, merge),
        };
        Ok((outcome, None))
    }

    fn merge_remote<R, S: Into<String>>(
        &self,
        remote: &str,
        merge: impl FnOnce(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<(Outcome, Option<R>)> {
        let tracking = self.branch(remote)?;
        let seen = match self.merge_tracking(&tracking, merge)? {
            Ok(committed) => return Ok((Outcome::Merged, Some(committed.value))),
            Err(seen) => seen,
        };
        let name = self.branch_name().to_string();
        self.rewrite_refs(|_, _, refs| {
            let Some(ours) = refs.try_get(&name)? else {
                return Ok((None, ()));
            };
            Ok((recorded(refs, &name, (*ours).clone(), remote, seen)?, ()))
        })?;
        Ok((Outcome::Ahead, None))
    }
}

fn recorded(
    refs: &Refs,
    name: &str,
    mut head: Head,
    remote: &str,
    theirs: u64,
) -> Result<Option<Refs>> {
    let record = (theirs, head.version);
    if let Some(current) = refs.try_get(name)?
        && current.log == head.log
        && current.merged.try_get(remote)?.as_deref() == Some(&record)
    {
        return Ok(None);
    }
    head.merged.insert(remote.to_string(), record);
    let mut next = refs.clone();
    next.insert(name.to_string(), head);
    Ok(Some(next))
}
