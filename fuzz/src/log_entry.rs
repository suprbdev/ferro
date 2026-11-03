use std::path::Path;

use ferro::codec::{Data, Decoder, Encoder, HashSink, Object, Ptr, Sink};
use ferro::store::{Store, StoreOptions, Txn};
use ferro::{Durability, Map};

use crate::input::Input;
use crate::types::{Gen, Root};

const HAS_PREV: u8 = 1;
const HAS_SKIP: u8 = 2;
const WHOLE: u8 = 4;
const JOINED: u8 = 8;
const MERGED: u8 = 16;
const GRAPH: u8 = 32;
const BOUNDED: u8 = 64;
const EXTRA: u8 = 128;

#[derive(Clone)]
struct Head {
    version: u64,
    first: u64,
    root: Option<Ptr>,
    tip: Option<Ptr>,
    merged: Map<String, (u64, u64)>,
}

fn put_link(w: &mut Encoder<'_>, link: Option<Ptr>) {
    w.u8(u8::from(link.is_some()));
    if let Some(ptr) = link {
        w.link(ptr);
    }
}

impl Data for Head {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.version.encode(w)?;
        self.first.encode(w)?;
        put_link(w, self.root);
        put_link(w, self.tip);
        self.merged.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Head {
            version: u64::decode(r)?,
            first: u64::decode(r)?,
            root: Option::<Ptr>::decode(r)?,
            tip: Option::<Ptr>::decode(r)?,
            merged: Map::decode(r)?,
        })
    }
}

#[derive(Clone, Copy)]
struct Built {
    ptr: Ptr,
    version: u64,
    time: u64,
    root: Option<Ptr>,
    generation: u64,
    reach: u64,
    last_merge: u64,
}

fn skip_version(version: u64) -> u64 {
    version - (version & version.wrapping_neg())
}

fn choose(input: &mut Input<'_>, built: &[Built], roots: &[Ptr]) -> Ptr {
    let pick = input.byte();
    match pick % 8 {
        0 => Ptr {
            hash: [pick; 20],
            offset: 0,
        },
        1 => roots[usize::from(input.byte()) % roots.len()],
        2 => Ptr {
            hash: [pick; 20],
            offset: input.num(),
        },
        _ if built.is_empty() => roots[0],
        _ => built[usize::from(input.byte()) % built.len()].ptr,
    }
}

fn find(built: &[Built], ptr: Ptr) -> Option<&Built> {
    built.iter().find(|entry| entry.ptr == ptr)
}

fn entry(input: &mut Input<'_>, built: &[Built], roots: &[Ptr]) -> (Object, Built) {
    let control = input.byte();
    let below = built.last().copied();
    if control == 0xff {
        let len = usize::from(input.byte());
        let body = input.bytes(len).to_vec();
        let links = (0..input.byte() % 4).map(|_| choose(input, built, roots)).collect();
        let object = Object { body, links };
        let made = Built {
            ptr: Ptr {
                hash: object.hash(),
                offset: 0,
            },
            version: 0,
            time: 0,
            root: None,
            generation: 0,
            reach: 0,
            last_merge: 0,
        };
        return (object, made);
    }
    let version = match control & 1 {
        0 => below.map_or(1, |entry| entry.version.wrapping_add(1)),
        _ => input.num(),
    };
    let time = match control & 2 {
        0 => below.map_or(1000, |entry| entry.time.wrapping_add(u64::from(input.byte()))),
        _ => input.num(),
    };
    let prev = match control & 4 {
        0 => below.map(|entry| entry.ptr),
        _ => input.chance(12).then(|| choose(input, built, roots)),
    };
    let skip = match control & 8 {
        0 => {
            let target = skip_version(version);
            built
                .iter()
                .find(|entry| entry.version == target && target.wrapping_add(1) != version)
                .map(|entry| entry.ptr)
        }
        _ => input.chance(8).then(|| choose(input, built, roots)),
    };
    let merged = (control & 16 != 0).then(|| {
        let ptr = choose(input, built, roots);
        let floor = match input.byte() % 4 {
            0 => input.num(),
            low => u64::from(low),
        };
        let after = match input.chance(4) {
            true => input.num(),
            false => 0,
        };
        (ptr, floor, after)
    });
    let root = match input.byte() % 8 {
        0 => None,
        1 => Some(choose(input, built, roots)),
        pick => Some(roots[usize::from(pick) % roots.len()]),
    };
    let restored = input.chance(2).then(|| input.num());
    let mut generation = below.map_or(0, |entry| entry.generation).wrapping_add(1);
    let mut reach = below.map_or(0, |entry| entry.reach);
    let mut last_merge = below.map_or(0, |entry| entry.last_merge);
    if let Some((ptr, _, _)) = merged {
        let other = find(built, ptr).map_or(0, |entry| entry.generation);
        generation = generation.max(other.wrapping_add(1));
        reach = reach.max(other);
        last_merge = version;
    }
    let mut lift = generation.saturating_sub(version);
    if control & 32 != 0 {
        lift = input.num();
        reach = input.num();
        last_merge = input.num();
    }
    let mut message = input.text();
    let mut flags = 0u8;
    let mut lengths = Vec::new();
    match input.byte() % 4 {
        0 => {}
        1 => flags |= WHOLE,
        2 => {
            flags |= JOINED;
            message = format!("{message}\n{}", input.text());
            lengths = message.split('\n').map(|part| part.len() as u64).collect();
        }
        _ => {
            flags |= JOINED;
            lengths = (0..input.byte() % 5).map(|_| input.num()).collect();
        }
    }
    let stamped = input.chance(4);
    let wide = (lift, reach, last_merge) != (0, 0, 0);
    flags |= u8::from(prev.is_some()) * HAS_PREV
        | u8::from(skip.is_some()) * HAS_SKIP
        | u8::from(merged.is_some()) * MERGED
        | u8::from(wide) * GRAPH
        | u8::from(control & 64 != 0) * BOUNDED
        | u8::from(stamped) * EXTRA;
    if control & 128 != 0 {
        flags ^= input.byte();
    }
    let mut sink = HashSink;
    let mut w = Encoder::new(&mut sink);
    w.varint(version);
    w.varint(time);
    w.varint(message.len() as u64);
    w.raw(message.as_bytes());
    put_link(&mut w, root);
    match restored {
        Some(at) => {
            w.u8(1);
            w.varint(at);
        }
        None => w.u8(0),
    }
    w.u8(flags);
    if flags & EXTRA != 0 {
        w.varint(if control & 128 != 0 { input.num() } else { 1 });
    }
    if flags & JOINED != 0 {
        w.varint(lengths.len() as u64);
        for len in &lengths {
            w.varint(*len);
        }
    }
    if flags & MERGED != 0 {
        let (_, floor, after) = merged.unwrap_or((roots[0], input.num(), input.num()));
        w.varint(floor);
        w.varint(after);
    }
    if flags & GRAPH != 0 {
        w.varint(lift);
        w.varint(reach);
        w.varint(last_merge);
    }
    for (bit, link) in [(HAS_PREV, prev), (HAS_SKIP, skip), (MERGED, merged.map(|m| m.0))] {
        if flags & bit != 0 {
            w.link(link.unwrap_or_else(|| choose(input, built, roots)));
        }
    }
    if input.chance(1) {
        w.raw(input.bytes(3));
    }
    let object = w.into_object();
    let made = Built {
        ptr: Ptr {
            hash: object.hash(),
            offset: 0,
        },
        version,
        time,
        root,
        generation: version.wrapping_add(lift),
        reach,
        last_merge,
    };
    (object, made)
}

fn value(txn: &mut Txn, value: &impl Data) -> ferro::Result<Ptr> {
    let mut encoder = Encoder::new(txn);
    value.encode(&mut encoder)?;
    encoder.finish()
}

fn build(input: &mut Input<'_>, store: &Store) -> ferro::Result<()> {
    let mut txn = store.begin()?;
    let mut roots = Vec::new();
    for _ in 0..2 {
        let root = Root::generate(input);
        roots.push(value(&mut txn, &root)?);
    }
    let mut built: Vec<Built> = Vec::new();
    for _ in 0..1 + input.byte() % 24 {
        let (object, mut made) = entry(input, &built, &roots);
        let hash = made.ptr.hash;
        made.ptr = txn.put(object, hash)?;
        built.push(made);
    }
    let mut refs: Map<String, Head> = Map::new();
    for name in ["main", "side", "other"].iter().take(1 + usize::from(input.byte() % 3)) {
        let tip = match input.byte() % 4 {
            0 => built[usize::from(input.byte()) % built.len()],
            _ => *built.last().unwrap(),
        };
        let control = input.byte();
        let mut merged = Map::new();
        if control & 1 != 0 {
            merged.insert(input.text(), (input.num(), input.num()));
        }
        let head = Head {
            version: if control & 2 != 0 { input.num() } else { tip.version },
            first: if control & 4 != 0 { input.num() } else { 1 },
            root: if control & 8 != 0 { Some(choose(input, &built, &roots)) } else { tip.root },
            tip: if control & 16 != 0 { None } else { Some(tip.ptr) },
            merged,
        };
        refs.insert(name.to_string(), head);
    }
    let head = value(&mut txn, &refs)?;
    txn.commit(head)
}

pub fn write(data: &[u8], path: &Path) -> Option<u8> {
    let mut input = Input::new(data);
    let mode = input.byte();
    let options = StoreOptions {
        durability: Durability::Buffered,
        cache_bytes: 1 << 20,
        ..StoreOptions::default()
    };
    let (store, _) = Store::open(path, options).ok()?;
    let built = build(&mut input, &store);
    store.close();
    built.ok().map(|()| mode)
}

pub fn run(data: &[u8]) {
    let path = crate::fresh("log.ferro");
    let Some(mode) = write(data, &path) else {
        return;
    };
    crate::open_as::<Root>(&path, mode & 1 != 0);
    if mode & 2 != 0 {
        crate::run_admin(&path, mode & 4 != 0);
    }
}
