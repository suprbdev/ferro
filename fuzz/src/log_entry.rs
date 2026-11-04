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
const STEP: u8 = 0x7f;
const FOLLOW: u8 = 8;

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
        let links = (0..input.byte() % 4)
            .map(|_| choose(input, built, roots))
            .collect();
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
        0 => below.map_or(1000, |entry| {
            entry.time.wrapping_add(u64::from(input.byte()))
        }),
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
    for (set, bit) in [
        (prev.is_some(), HAS_PREV),
        (skip.is_some(), HAS_SKIP),
        (merged.is_some(), MERGED),
        (wide, GRAPH),
        (control & 64 != 0, BOUNDED),
        (stamped, EXTRA),
    ] {
        if set {
            flags |= bit;
        }
    }
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
    for (bit, link) in [
        (HAS_PREV, prev),
        (HAS_SKIP, skip),
        (MERGED, merged.map(|m| m.0)),
    ] {
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

struct Made {
    built: Vec<Built>,
    roots: Vec<Ptr>,
    names: Vec<String>,
    tables: Vec<Ptr>,
}

fn head(input: &mut Input<'_>, built: &[Built], roots: &[Ptr]) -> Head {
    let tip = match input.byte() % 4 {
        0 => built[usize::from(input.byte()) % built.len()],
        _ => *built.last().unwrap(),
    };
    let control = input.byte();
    let mut merged = Map::new();
    if control & 1 != 0 {
        merged.insert(input.text(), (input.num(), input.num()));
    }
    Head {
        version: if control & 2 != 0 {
            input.num()
        } else {
            tip.version
        },
        first: if control & 4 != 0 { input.num() } else { 1 },
        root: if control & 8 != 0 {
            Some(choose(input, built, roots))
        } else {
            tip.root
        },
        tip: if control & 16 != 0 {
            None
        } else {
            Some(tip.ptr)
        },
        merged,
    }
}

fn build(input: &mut Input<'_>, store: &Store) -> ferro::Result<Made> {
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
    let mut names = Vec::new();
    for name in ["main", "side", "other"]
        .iter()
        .take(1 + usize::from(input.byte() % 3))
    {
        refs.insert(name.to_string(), head(input, &built, &roots));
        names.push(name.to_string());
    }
    let table = value(&mut txn, &refs)?;
    txn.commit(table)?;
    Ok(Made {
        built,
        roots,
        names,
        tables: vec![table],
    })
}

fn steps(input: &mut Input<'_>, store: &Store, top: Ptr, made: &mut Made) -> ferro::Result<()> {
    let count = match input.byte() % 8 {
        0 => 0,
        7 => 60 + usize::from(input.byte() % 16),
        pick => usize::from(pick) * 2,
    };
    if count == 0 {
        return Ok(());
    }
    let mut txn = store.begin()?;
    let mut prev = top;
    for _ in 0..count {
        let control = input.byte();
        let link = match control % 16 {
            0 => made.tables[usize::from(input.byte()) % made.tables.len()],
            1 => choose(input, &made.built, &made.roots),
            _ => prev,
        };
        let name = match control / 16 % 8 {
            0 => input.text(),
            pick => made.names[usize::from(pick) % made.names.len()].clone(),
        };
        let head = head(input, &made.built, &made.roots);
        let mut w = Encoder::new(&mut txn);
        let forged = control & 128 != 0 && input.chance(2);
        w.u8(if forged { input.byte() } else { STEP });
        w.link(link);
        w.varint(name.len() as u64);
        w.raw(name.as_bytes());
        head.encode(&mut w)?;
        if control & 128 != 0 && input.chance(2) {
            w.raw(input.bytes(2));
        }
        prev = w.finish()?;
        made.tables.push(prev);
        if !made.names.contains(&name) {
            made.names.push(name);
        }
    }
    txn.commit(prev)
}

fn open_store(path: &Path) -> Option<(Store, Option<Ptr>)> {
    let options = StoreOptions {
        durability: Durability::Buffered,
        cache_bytes: 1 << 20,
        ..StoreOptions::default()
    };
    Store::open(path, options).ok()
}

fn more_steps(input: &mut Input<'_>, path: &Path, made: &mut Made) -> Option<()> {
    let (store, top) = open_store(path)?;
    let stepped = match top {
        Some(top) => steps(input, &store, top, made),
        None => Ok(()),
    };
    store.close();
    stepped.ok()
}

fn first_phase(input: &mut Input<'_>, path: &Path) -> Option<Made> {
    let (store, _) = open_store(path)?;
    let built = build(input, &store);
    store.close();
    let mut made = built.ok()?;
    more_steps(input, path, &mut made)?;
    Some(made)
}

pub fn write(data: &[u8], path: &Path) -> Option<u8> {
    let mut input = Input::new(data);
    let mode = input.byte();
    let mut made = first_phase(&mut input, path)?;
    if mode & FOLLOW != 0 {
        more_steps(&mut input, path, &mut made)?;
    }
    Some(mode)
}

pub fn run(data: &[u8]) {
    let path = crate::fresh("log.ferro");
    let mut input = Input::new(data);
    let mode = input.byte();
    let Some(mut made) = first_phase(&mut input, &path) else {
        return;
    };
    if mode & FOLLOW != 0 {
        let follower = crate::options(true).open::<Root>(&path);
        let appended = more_steps(&mut input, &path, &mut made);
        if let Ok(db) = follower {
            let _ = db.refresh();
            crate::exercise(&db, true);
        }
        if appended.is_none() {
            return;
        }
    }
    crate::open_as::<Root>(&path, mode & 1 != 0);
    if mode & 2 != 0 {
        crate::run_admin(&path, mode & 4 != 0);
    }
}
