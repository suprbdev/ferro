use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use ferro::codec::Object;
use ferro::db::{Clock, Hook};
use ferro::{Bytes, Db, Durability, Error, Hash, Keep, Options, Ptr, Reclaim, Snapshot};

use crate::catalog::{World, fill, item, person, word, world_whole};
use crate::input::Input;
use crate::types::Probe;

pub const CLOCK_MS: u64 = 1_700_000_000_000;
const MAGIC: &[u8; 8] = b"FERROBDL";
const COMPRESS_MIN: usize = 96;

pub const STRUCTURED: u8 = 0;
pub const EDITS: u8 = 1;
pub const DIRECT: u8 = 2;
pub const BOTH: u8 = 3;
pub const REHASH: u8 = 4;
pub const PACK_SMART: u8 = 8;
pub const PACK_ALWAYS: u8 = 16;
pub const REOPEN: u8 = 32;
pub const CHUNKED: u8 = 64;
pub const FAILING: u8 = 128;

pub fn options() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20)
        .reclaim(Reclaim { every_bytes: 0 })
        .clock(Clock(Arc::new(|| CLOCK_MS)))
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())?;
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Some(out)
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.take(1)?[0];
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn hash(&mut self) -> Option<Hash> {
        self.take(20)?.try_into().ok()
    }
}

#[derive(Clone, Debug)]
pub struct Wire {
    pub body: Vec<u8>,
    pub links: Vec<(Hash, u64)>,
}

impl Wire {
    pub fn hash(&self) -> Hash {
        Object {
            body: self.body.clone(),
            links: self
                .links
                .iter()
                .map(|(hash, _)| Ptr {
                    hash: *hash,
                    offset: 0,
                })
                .collect(),
        }
        .hash()
    }
}

#[derive(Clone, Debug)]
pub struct Parsed {
    pub tip: Option<(Hash, u64)>,
    pub bases: Vec<(Hash, u64)>,
    pub roots: Vec<u64>,
    pub hints: Vec<(u64, u64)>,
    pub count: Option<u64>,
    pub objects: Vec<Wire>,
}

impl Parsed {
    pub fn of(bytes: &[u8]) -> Option<Parsed> {
        let mut r = Reader { bytes, pos: 0 };
        if r.take(8)? != MAGIC || r.take(1)? != [1] {
            return None;
        }
        let tip = match r.take(1)?[0] {
            0 => None,
            1 => Some((r.hash()?, r.varint()?)),
            _ => return None,
        };
        let mut bases = Vec::new();
        for _ in 0..r.varint()? {
            bases.push((r.hash()?, r.varint()?));
        }
        let mut roots = Vec::new();
        for _ in 0..r.varint()? {
            roots.push(r.varint()?);
        }
        let mut hints = Vec::new();
        for _ in 0..r.varint()? {
            hints.push((r.varint()?, r.varint()?));
        }
        let mut objects = Vec::new();
        for _ in 0..r.varint()? {
            let flagged = r.varint()?;
            let stored = usize::try_from(flagged >> 1).ok()?;
            let body = if flagged & 1 == 1 {
                let raw = usize::try_from(r.varint()?).ok()?;
                lz4_flex::block::decompress(r.take(stored)?, raw).ok()?
            } else {
                r.take(stored)?.to_vec()
            };
            let mut links = Vec::new();
            for _ in 0..r.varint()? {
                links.push((r.hash()?, r.varint()?));
            }
            objects.push(Wire { body, links });
        }
        if r.pos != bytes.len() {
            return None;
        }
        Some(Parsed {
            tip,
            bases,
            roots,
            hints,
            count: None,
            objects,
        })
    }

    pub fn encode(&self, pack: u8) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.push(1);
        match self.tip {
            Some((hash, version)) => {
                out.push(1);
                out.extend_from_slice(&hash);
                varint(&mut out, version);
            }
            None => out.push(0),
        }
        varint(&mut out, self.bases.len() as u64);
        for (hash, version) in &self.bases {
            out.extend_from_slice(hash);
            varint(&mut out, *version);
        }
        varint(&mut out, self.roots.len() as u64);
        for root in &self.roots {
            varint(&mut out, *root);
        }
        varint(&mut out, self.hints.len() as u64);
        for (parent, index) in &self.hints {
            varint(&mut out, *parent);
            varint(&mut out, *index);
        }
        varint(&mut out, self.count.unwrap_or(self.objects.len() as u64));
        for object in &self.objects {
            let raw = object.body.len();
            let packed = (pack & (PACK_SMART | PACK_ALWAYS) != 0 && raw >= COMPRESS_MIN)
                .then(|| lz4_flex::block::compress(&object.body))
                .filter(|packed| {
                    pack & PACK_ALWAYS != 0 || packed.len() + varint_len(raw as u64) < raw
                });
            match packed {
                Some(packed) => {
                    varint(&mut out, (packed.len() as u64) << 1 | 1);
                    varint(&mut out, raw as u64);
                    out.extend_from_slice(&packed);
                }
                None => {
                    varint(&mut out, (raw as u64) << 1);
                    out.extend_from_slice(&object.body);
                }
            }
            varint(&mut out, object.links.len() as u64);
            for (hash, reference) in &object.links {
                out.extend_from_slice(hash);
                varint(&mut out, *reference);
            }
        }
        out
    }

    fn targets(&self) -> Vec<(Hash, u64)> {
        let mut targets: Vec<(Hash, u64)> = self
            .objects
            .iter()
            .flat_map(|object| object.links.iter().copied())
            .collect();
        targets.extend(self.objects.iter().map(|object| (object.hash(), 0)));
        targets.extend(self.bases.iter().map(|(hash, _)| (*hash, 0)));
        targets.sort();
        targets.dedup();
        targets
    }

    fn rehash(&mut self, old: &[Hash]) {
        let mut renamed: HashMap<Hash, Hash> = HashMap::new();
        for (object, before) in self.objects.iter_mut().zip(old) {
            for (hash, _) in &mut object.links {
                if let Some(new) = renamed.get(hash) {
                    *hash = *new;
                }
            }
            let after = object.hash();
            if after != *before {
                renamed.insert(*before, after);
            }
        }
        if let Some((hash, _)) = &mut self.tip
            && let Some(new) = renamed.get(hash)
        {
            *hash = *new;
        }
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

fn pick(input: &mut Input<'_>, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (input.num() % len as u64) as usize
    }
}

fn number(input: &mut Input<'_>, old: u64) -> u64 {
    match input.byte() % 4 {
        0 => old.wrapping_add(1),
        1 => old.wrapping_sub(1),
        2 => input.num(),
        _ => old ^ (1 << (input.byte() % 64)),
    }
}

fn mutate(parsed: &mut Parsed, input: &mut Input<'_>) {
    let targets = parsed.targets();
    let objects = parsed.objects.len();
    let op = input.byte();
    let at = pick(input, objects);
    match op % 18 {
        0..=3 if objects > 0 => {
            let body = &mut parsed.objects[at].body;
            let spot = pick(input, body.len() + 1);
            match op % 18 {
                0 if spot < body.len() => body[spot] ^= input.byte() | 1,
                1 if spot < body.len() => body[spot] = input.byte(),
                2 => body.truncate(spot),
                _ => body.insert(spot, input.byte()),
            }
        }
        4..=8 if objects > 0 => {
            let links = &mut parsed.objects[at].links;
            let slot = pick(input, links.len());
            let target = targets[pick(input, targets.len())];
            match op % 18 {
                4 if !links.is_empty() => links[slot] = target,
                5 if !links.is_empty() => {
                    links.remove(slot);
                }
                6 if links.len() > 1 => {
                    let other = pick(input, links.len());
                    links.swap(slot, other);
                }
                7 => links.insert(pick(input, links.len() + 1), target),
                _ if !links.is_empty() => links[slot].1 = number(input, links[slot].1),
                _ => {}
            }
        }
        9 if objects > 0 => {
            let copy = parsed.objects[at].clone();
            let to = pick(input, objects + 1);
            parsed.objects.insert(to, copy);
        }
        10 if objects > 0 => {
            parsed.objects.remove(at);
        }
        11 if objects > 1 => {
            let other = pick(input, objects);
            parsed.objects.swap(at, other);
        }
        12 => match &mut parsed.tip {
            Some((hash, version)) => {
                if input.chance(4) {
                    *hash = targets[pick(input, targets.len())].0;
                } else {
                    *version = number(input, *version);
                }
            }
            None => parsed.tip = Some((targets[pick(input, targets.len())].0, input.num())),
        },
        13 => {
            let len = parsed.bases.len();
            let slot = pick(input, len);
            match input.byte() % 4 {
                0 if len > 0 => parsed.bases[slot].1 = number(input, parsed.bases[slot].1),
                1 if len > 0 => parsed.bases[slot].0 = targets[pick(input, targets.len())].0,
                2 if len > 0 => {
                    parsed.bases.remove(slot);
                }
                _ if len > 0 => parsed.bases.push(parsed.bases[slot]),
                _ => parsed.tip = None,
            }
        }
        14 => {
            let len = parsed.roots.len();
            match input.byte() % 3 {
                0 if len > 0 => {
                    let slot = pick(input, len);
                    parsed.roots[slot] = number(input, parsed.roots[slot]);
                }
                1 if len > 0 => {
                    parsed.roots.remove(pick(input, len));
                }
                _ => parsed.roots.push(input.num() % 4),
            }
        }
        15 | 16 => {
            let len = parsed.hints.len();
            match input.byte() % 4 {
                0 if len > 0 => {
                    let slot = pick(input, len);
                    parsed.hints[slot].0 = number(input, parsed.hints[slot].0);
                }
                1 if len > 0 => {
                    let slot = pick(input, len);
                    parsed.hints[slot].1 = number(input, parsed.hints[slot].1);
                }
                2 if len > 0 => {
                    parsed.hints.remove(pick(input, len));
                }
                _ => parsed.hints.push((input.num() % 8, input.num() % 16)),
            }
        }
        _ => {
            let real = parsed.objects.len() as u64;
            parsed.count = Some(number(input, real));
        }
    }
}

fn edit(bytes: &mut Vec<u8>, input: &mut Input<'_>) {
    for _ in 0..1 + input.byte() % 4 {
        let at = pick(input, bytes.len() + 1);
        match input.byte() % 6 {
            0 if at < bytes.len() => bytes[at] ^= input.byte() | 1,
            1 if at < bytes.len() => bytes[at] = input.byte(),
            2 => bytes.insert(at, input.byte()),
            3 if at < bytes.len() => {
                let end = at
                    .saturating_add(1 + usize::from(input.byte()))
                    .min(bytes.len());
                bytes.drain(at..end);
            }
            4 => bytes.truncate(at),
            _ => {
                let len = usize::from(input.byte() % 16);
                let splice = input.bytes(len).to_vec();
                bytes.splice(at..at, splice);
            }
        }
    }
}

pub struct Scenario {
    pub name: &'static str,
    pub image: Vec<u8>,
    pub branch: String,
    pub remote: String,
    pub bundle: Vec<u8>,
    pub parsed: Option<Parsed>,
}

fn take_theirs(ours: &mut World, base: &Snapshot<World>, theirs: &Snapshot<World>) -> (String, ()) {
    ours.names.merge(&base.names, &theirs.names, |conflict| {
        conflict.theirs.map(|value| (*value).clone())
    });
    ours.items.merge(&base.items, &theirs.items, |conflict| {
        conflict.theirs.map(|value| (*value).clone())
    });
    ours.people.merge(&base.people, &theirs.people, |conflict| {
        conflict.ours.map(|value| (*value).clone())
    });
    ours.totals.merge(&base.totals, &theirs.totals, |conflict| {
        conflict.base.map(|value| *value)
    });
    ("merge".to_string(), ())
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }
}

fn open(path: &Path) -> Db<World> {
    options().open(path).unwrap()
}

fn scenario(
    name: &'static str,
    receiver: Db<World>,
    branch: &str,
    remote: &str,
    sender: &Db<World>,
) -> Scenario {
    let to = receiver.branch(branch).unwrap();
    let have = to.have().unwrap();
    let bundle = sender.bundle(&have).unwrap().as_bytes().to_vec();
    let path = receiver.path();
    drop(to);
    drop(receiver);
    let image = fs::read(&path).unwrap();
    let parsed = Parsed::of(&bundle);
    Scenario {
        name,
        image,
        branch: branch.to_string(),
        remote: remote.to_string(),
        parsed,
        bundle,
    }
}

fn hints(dir: &Path) -> Scenario {
    let a = open(&dir.join("hints-a.ferro"));
    let b = open(&dir.join("hints-b.ferro"));
    a.update(|world| fill(world, 0)).unwrap();
    b.pull(&a, "a", take_theirs).unwrap();
    a.update(|world| fill(world, 1)).unwrap();
    a.update(|world| {
        world.names.remove(&3);
        world.nested.remove(&4);
        world.people.remove(&5);
        world.items.remove(&6);
        world.events.truncate(50);
    })
    .unwrap();
    scenario("hints", b, "main", "a", &a)
}

fn clone(dir: &Path) -> Scenario {
    let a = open(&dir.join("clone-a.ferro"));
    let b = open(&dir.join("clone-b.ferro"));
    a.commit("zero", |world| fill(world, 0)).unwrap();
    a.commit("one", |world| fill(world, 1)).unwrap();
    scenario("clone", b, "main", "a", &a)
}

fn merges(dir: &Path) -> Scenario {
    let a = open(&dir.join("merges-a.ferro"));
    let b = open(&dir.join("merges-b.ferro"));
    a.update(|world| fill(world, 0)).unwrap();
    b.pull(&a, "a", take_theirs).unwrap();
    a.update(|world| world.poke(11)).unwrap();
    b.update(|world| world.poke(12)).unwrap();
    b.pull(&a, "a", take_theirs).unwrap();
    a.update(|world| world.poke(13)).unwrap();
    b.update(|world| world.poke(14)).unwrap();
    let side = b.fork("side").unwrap();
    side.update(|world| world.poke(15)).unwrap();
    b.merge_with(&side, take_theirs).unwrap();
    scenario("merges", a, "main", "b", &b)
}

fn catalog(dir: &Path) -> Scenario {
    let a = open(&dir.join("catalog-a.ferro"));
    let b = open(&dir.join("catalog-b.ferro"));
    a.update(|world| fill(world, 0)).unwrap();
    b.pull(&a, "a", take_theirs).unwrap();
    a.update(|world| {
        for id in 0..40 {
            let _ = world.items.try_update(&id, |row| {
                row.price += 7;
                row.tags.push(word(u64::from(id), 3));
                row.code = None;
                row.rank = 3;
            });
        }
        for id in 60..90 {
            let _ = world.items.try_insert(item(id, 5));
        }
        for id in 0..20 {
            world.people.insert(person(id, 9));
        }
    })
    .unwrap();
    scenario("catalog", b, "main", "a", &a)
}

fn spill(dir: &Path) -> Scenario {
    let a = open(&dir.join("spill-a.ferro"));
    let b = open(&dir.join("spill-b.ferro"));
    a.update(|world| fill(world, 0)).unwrap();
    b.pull(&a, "a", take_theirs).unwrap();
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    a.update(|world| {
        for key in 100..400u64 {
            let bytes: Vec<u8> = (0..4096).map(|_| rng.next() as u8).collect();
            world.blobs.insert(key, Bytes::from(bytes));
        }
    })
    .unwrap();
    a.update(|world| world.poke(21)).unwrap();
    scenario("spill", b, "main", "a", &a)
}

fn relay(dir: &Path, seed: u64) -> Vec<Scenario> {
    let keeps = [Keep::All, Keep::Last(2), Keep::Last(1)];
    let paths: Vec<_> = (0..3)
        .map(|index| dir.join(format!("relay{seed}-{index}.ferro")))
        .collect();
    let devices: Vec<Db<World>> = paths
        .iter()
        .zip(keeps)
        .map(|(path, keep)| options().history(keep).open(path).unwrap())
        .collect();
    devices[0].update(|world| fill(world, 0)).unwrap();
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    for step in 0..40u64 {
        let to = rng.below(3) as usize;
        let names = devices[to].branches();
        let name = names[rng.below(names.len() as u64) as usize].clone();
        let Ok(handle) = devices[to].branch(&name) else {
            continue;
        };
        match rng.below(10) {
            0..=3 => {
                handle.update(|world| world.poke(rng.next())).unwrap();
            }
            4 => {
                let _ = handle.fork(["p", "q"][rng.below(2) as usize]);
            }
            5..=7 => {
                let from = (to + 1 + rng.below(2) as usize) % 3;
                let sources = devices[from].branches();
                let source = &sources[rng.below(sources.len() as u64) as usize];
                let remote = format!("d{from}-{source}");
                if remote == name {
                    continue;
                }
                let out = devices[from].branch(source).unwrap();
                match handle.pull(&out, &remote, take_theirs) {
                    Ok(_) | Err(Error::NoCommonAncestor | Error::StaleHave(_)) => {}
                    Err(error) => panic!("relay {seed} step {step}: {error}"),
                }
            }
            _ => {
                let keep = [Keep::All, Keep::Last(1), Keep::Last(2)][rng.below(3) as usize];
                devices[to].compact(keep).unwrap();
            }
        }
    }
    let mut candidates = Vec::new();
    for to in 0..3 {
        for from in (0..3).filter(|from| *from != to) {
            for branch in devices[to].branches() {
                for source in devices[from].branches() {
                    let remote = format!("d{from}-{source}");
                    if remote == branch {
                        continue;
                    }
                    let have = devices[to].branch(&branch).unwrap().have().unwrap();
                    let sending = devices[from].branch(&source).unwrap();
                    let size = sending.bundle(&have).unwrap().as_bytes().len();
                    candidates.push((size, to, from, branch.clone(), source, remote));
                }
            }
        }
    }
    candidates.sort();
    let mut out = Vec::new();
    let mut devices: Vec<Option<Db<World>>> = devices.into_iter().map(Some).collect();
    let mut used = HashSet::new();
    for (_, to, from, branch, source, remote) in candidates.into_iter().rev() {
        if out.len() == 2 || !used.insert(to) {
            continue;
        }
        let sender = devices[from].clone().unwrap();
        let receiver = devices[to].take().unwrap();
        let sending = sender.branch(&source).unwrap();
        let name = if out.is_empty() { "relay-a" } else { "relay-b" };
        let made = scenario(name, receiver, &branch, &remote, &sending);
        drop((sending, sender));
        devices[to] = Some(options().history(keeps[to]).open(&paths[to]).unwrap());
        out.push(made);
    }
    out
}

pub fn build(dir: &Path) -> Vec<Scenario> {
    let mut all = vec![
        hints(dir),
        clone(dir),
        merges(dir),
        catalog(dir),
        spill(dir),
    ];
    all.extend(relay(dir, 3));
    all.extend(relay(dir, 8));
    all
}

const NAMES: [&str; 7] = [
    "hints", "clone", "merges", "catalog", "spill", "relay-a", "relay-b",
];

fn cache_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let meta = fs::metadata(&exe).ok()?;
    let stamp = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let key = format!("{}:{}:{}", exe.display(), meta.len(), stamp.as_nanos());
    let key = blake3::hash(key.as_bytes()).to_hex();
    Some(crate::scratch_base().join(format!("ferro-fuzz-scenarios-{}.bin", &key[..16])))
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn save(all: &[Scenario], path: &Path) {
    let mut out = Vec::new();
    for scenario in all {
        for part in [
            scenario.name.as_bytes(),
            &scenario.image,
            scenario.branch.as_bytes(),
            scenario.remote.as_bytes(),
            &scenario.bundle,
        ] {
            put_bytes(&mut out, part);
        }
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    if fs::write(&temp, out).is_ok() {
        let _ = fs::rename(&temp, path);
    }
}

fn load(path: &Path) -> Option<Vec<Scenario>> {
    let bytes = fs::read(path).ok()?;
    let mut r = Reader {
        bytes: &bytes,
        pos: 0,
    };
    let part = |r: &mut Reader<'_>| -> Option<Vec<u8>> {
        let len = u64::from_le_bytes(r.take(8)?.try_into().ok()?);
        Some(r.take(usize::try_from(len).ok()?)?.to_vec())
    };
    let mut all = Vec::new();
    while r.pos < bytes.len() {
        let name = part(&mut r)?;
        let name = *NAMES.iter().find(|known| known.as_bytes() == name)?;
        let image = part(&mut r)?;
        let branch = String::from_utf8(part(&mut r)?).ok()?;
        let remote = String::from_utf8(part(&mut r)?).ok()?;
        let bundle = part(&mut r)?;
        all.push(Scenario {
            name,
            image,
            branch,
            remote,
            parsed: Parsed::of(&bundle),
            bundle,
        });
    }
    Some(all)
}

pub fn scenarios() -> &'static [Scenario] {
    static ALL: OnceLock<Vec<Scenario>> = OnceLock::new();
    ALL.get_or_init(|| {
        let cache = cache_path();
        if let Some(found) = cache.as_deref().and_then(load) {
            return found;
        }
        let dir = tempfile::tempdir().unwrap();
        let all = build(dir.path());
        if let Some(path) = &cache {
            save(&all, path);
        }
        all
    })
}

pub struct Prepared {
    pub scenario: &'static Scenario,
    pub mode: u8,
    pub interfere: u8,
    pub chunk: usize,
    pub fail_at: usize,
    pub interrupt: usize,
    pub bundle: Vec<u8>,
}

pub fn prepare(data: &[u8]) -> Option<Prepared> {
    let all = scenarios();
    let mut input = Input::new(data);
    let scenario = &all[usize::from(input.byte()) % all.len()];
    let mode = input.byte();
    let interfere = input.byte();
    let chunk = match mode & CHUNKED {
        0 => usize::MAX,
        _ => 1 + (input.num() % 8192) as usize,
    };
    let interrupt = match mode & (CHUNKED | FAILING) {
        flags if flags == CHUNKED | FAILING => 2 + usize::from(input.byte() % 6),
        _ => 0,
    };
    let fail_at = match mode & (CHUNKED | FAILING) {
        FAILING => input.num() as usize,
        _ => usize::MAX,
    };
    let form = mode & 3;
    let bundle = match form {
        DIRECT => input.bytes(usize::MAX).to_vec(),
        EDITS => {
            let mut bytes = scenario.bundle.clone();
            edit(&mut bytes, &mut input);
            bytes
        }
        _ => {
            let mut parsed = scenario.parsed.clone()?;
            let old: Vec<Hash> = parsed.objects.iter().map(Wire::hash).collect();
            for _ in 0..input.byte() % 6 {
                mutate(&mut parsed, &mut input);
            }
            if mode & REHASH != 0 && old.len() == parsed.objects.len() {
                parsed.rehash(&old);
            }
            let mut bytes = parsed.encode(mode);
            if form == BOTH {
                edit(&mut bytes, &mut input);
            }
            bytes
        }
    };
    Some(Prepared {
        scenario,
        mode,
        interfere,
        chunk,
        fail_at,
        interrupt,
        bundle,
    })
}

struct Feed<'a> {
    data: &'a [u8],
    pos: usize,
    chunk: usize,
    fail_at: usize,
    interrupt: usize,
    calls: usize,
}

impl Read for Feed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.calls += 1;
        if self.interrupt > 0 && self.calls % self.interrupt == 0 {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.pos >= self.fail_at && self.pos < self.data.len() {
            return Err(io::Error::other("the stream broke"));
        }
        let end = self
            .data
            .len()
            .min(self.fail_at)
            .min(self.pos.saturating_add(self.chunk.min(buf.len())));
        let n = end.saturating_sub(self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..end]);
        self.pos += n;
        Ok(n)
    }
}

static ACTION: Mutex<Option<(Db<World>, u8)>> = Mutex::new(None);

fn interfere(step: &'static str) {
    let mut action = ACTION.lock().unwrap();
    let wanted = match action.as_ref() {
        Some((_, what)) if what & 8 != 0 => "placed",
        Some(_) => "received",
        None => return,
    };
    if step != wanted {
        return;
    }
    let Some((db, what)) = action.take() else {
        return;
    };
    drop(action);
    match what % 8 {
        4 => {
            let _ = db.update(|world| world.poke(31));
        }
        5 => {
            if let Ok(other) = db.branch("main") {
                let _ = other.update(|world| world.poke(32));
            }
        }
        6 if step == "received" => {
            let _ = db.compact(Keep::Last(1));
        }
        6 => {
            if let Ok(other) = db.branch(&db.branches()[0]) {
                let _ = other.update(|world| world.poke(34));
            }
        }
        7 => {
            if let Ok(side) = db.fork("fuzz-side") {
                let _ = side.update(|world| world.poke(33));
            }
        }
        _ => {}
    }
}

fn whole_branch(db: &Db<World>, when: &str) {
    let snapshot = db.read();
    world_whole(&snapshot);
    let first = db.log().first().map_or(1, |(version, _)| version);
    let last = db.version();
    for version in [first, first.saturating_add(1), last.saturating_sub(1), last] {
        if version == 0 || version < first || version > last {
            continue;
        }
        if let Err(error) = db.at(version) {
            panic!(
                "{when}: {} at v{version} after an accepted bundle: {error}",
                db.branch_name()
            );
        }
    }
    let _ = db.log().iter().count();
}

fn whole_file(db: &Db<World>, remote: &str, when: &str) {
    whole_branch(db, when);
    if let Ok(landed) = db.branch(remote) {
        whole_branch(&landed, when);
        let _ = db.common_ancestor(&landed);
    }
    if let Err(error) = db.verify() {
        panic!("{when}: verify failed after an accepted bundle: {error}");
    }
    if let Err(error) = db.verify_typed() {
        panic!("{when}: typed verify failed after an accepted bundle: {error}");
    }
}

pub fn run(data: &[u8]) {
    let Some(prepared) = prepare(data) else {
        return;
    };
    receive(&prepared, &crate::fresh("sync.ferro"));
}

pub fn receive(prepared: &Prepared, path: &Path) {
    let scenario = prepared.scenario;
    fs::write(path, &scenario.image).unwrap();
    let hook = Hook(Arc::new(interfere));
    let db: Db<World> = options().hook(hook).open(path).unwrap();
    let to = db.branch(&scenario.branch).unwrap();
    let before = to.head_hash();
    let landed_before = db
        .branch(&scenario.remote)
        .ok()
        .map(|landed| landed.head_hash());
    if prepared.interfere % 8 >= 4 {
        *ACTION.lock().unwrap() = Some((to.clone(), prepared.interfere));
    }
    let feed = Feed {
        data: &prepared.bundle,
        pos: 0,
        chunk: prepared.chunk,
        fail_at: prepared.fail_at,
        interrupt: prepared.interrupt,
        calls: 0,
    };
    let result = to.receive_from(&scenario.remote, feed, take_theirs);
    ACTION.lock().unwrap().take();
    if std::env::var_os("FERRO_FUZZ_TRACE").is_some() {
        eprintln!(
            "{}: {:?}",
            scenario.name,
            result.as_ref().map(|pulled| pulled.outcome)
        );
    }
    match result {
        Ok(_) => {
            whole_file(&to, &scenario.remote, "received");
            if prepared.mode & REOPEN != 0 {
                drop(to);
                drop(db);
                let db: Db<World> = options().open(path).unwrap_or_else(|error| {
                    panic!("reopening after an accepted bundle failed: {error}")
                });
                let to = db.branch(&scenario.branch).unwrap();
                whole_file(&to, &scenario.remote, "reopened");
                if let Err(error) = to.update(|world| world.poke(41)) {
                    panic!("a commit after an accepted bundle failed: {error}");
                }
                whole_file(&to, &scenario.remote, "committed");
            }
        }
        Err(Error::Bundle(_) | Error::StaleHave(_)) => {
            if prepared.interfere % 8 < 4 {
                assert_eq!(to.head_hash(), before, "a refused bundle moved the branch");
                let landed = db
                    .branch(&scenario.remote)
                    .ok()
                    .map(|landed| landed.head_hash());
                assert_eq!(landed, landed_before, "a refused bundle moved the remote");
            }
        }
        Err(Error::Io(_)) if prepared.fail_at < prepared.bundle.len() => {}
        Err(Error::NoCommonAncestor) => {}
        Err(error) => panic!("unexpected receive error: {error:?}"),
    }
}
