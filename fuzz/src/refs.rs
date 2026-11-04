use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::OnceLock;

use ferro::codec::{Data, Decoder, Encoder, Ptr};
use ferro::store::{Store, StoreOptions, Txn};
use ferro::{Db, Durability, Keep, Map};

use crate::input::Input;
use crate::log_entry::Head;
use crate::types::Root;
use crate::{guarded, options};

pub const STEP: u8 = 0x7f;
pub const FOLLOW: u8 = 1;
pub const READ_ONLY: u8 = 2;
pub const WRITES: u8 = 4;
pub const ADMIN: u8 = 8;
pub const FRAMES: u8 = 16;
pub const COMPACT: u8 = 32;
pub const LONG: u8 = 64;

pub struct Base {
    pub name: &'static str,
    pub image: Vec<u8>,
}

type Table = Map<String, Head>;

fn put(db: &Db<Root>, key: u64) {
    db.update(|root| {
        root.insert(key % 11, format!("v{key}"));
    })
    .unwrap();
}

fn build(dir: &Path, name: &'static str, steps: impl FnOnce(&Db<Root>)) -> Base {
    let path = dir.join(format!("{name}.ferro"));
    let db = options(false).open::<Root>(&path).unwrap();
    steps(&db);
    drop(db);
    let image = fs::read(&path).unwrap();
    let _ = fs::remove_file(&path);
    Base { name, image }
}

fn bases_in(dir: &Path) -> Vec<Base> {
    vec![
        build(dir, "steps", |db| {
            put(db, 0);
            let branches = [db.clone(), db.fork("side").unwrap(), db.fork("x").unwrap()];
            for at in 0..140u64 {
                put(&branches[at as usize % 3], at);
            }
        }),
        build(dir, "wide", |db| {
            put(db, 0);
            let forks: Vec<Db<Root>> = (0..130)
                .map(|at| db.fork(&format!("w{at:03}")).unwrap())
                .collect();
            for at in 0..6usize {
                put(&forks[at * 17 % forks.len()], at as u64);
            }
        }),
        build(dir, "full", |db| {
            put(db, 0);
            let side = db.fork("side").unwrap();
            db.compact(Keep::All).unwrap();
            for at in 0..64u64 {
                put(if at % 2 == 0 { db } else { &side }, at);
            }
        }),
        build(dir, "merged", |db| {
            put(db, 0);
            let side = db.fork("side").unwrap();
            for at in 0..5u64 {
                put(db, at);
                put(&side, at + 50);
            }
            db.merge_with(&side, |_, _, _| ("merge", ())).unwrap();
            side.merge_with(db, |_, _, _| ("merge back", ())).unwrap();
            db.fork("gone").unwrap();
            db.drop_branch("gone").unwrap();
            db.compact(Keep::Last(3)).unwrap();
            put(&side, 99);
        }),
    ]
}

pub fn bases() -> &'static [Base] {
    static BASES: OnceLock<Vec<Base>> = OnceLock::new();
    BASES.get_or_init(|| {
        let dir = crate::scratch_base().join(format!("ferro-fuzz-refs-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let all = bases_in(&dir);
        let _ = fs::remove_dir_all(&dir);
        all
    })
}

#[derive(Default)]
struct Known {
    chain: Vec<Ptr>,
    heads: Vec<(Vec<u8>, Head)>,
    objects: Vec<Ptr>,
}

fn lone_varint(body: &[u8]) -> bool {
    let end = body.iter().position(|byte| byte & 0x80 == 0);
    end.is_some() && end == body.len().checked_sub(1)
}

fn parse_step(store: &Store, ptr: Ptr) -> Option<(Ptr, Vec<u8>, Head)> {
    let object = store.read_object(ptr).ok()?;
    if lone_varint(&object.body) {
        return None;
    }
    let mut r = Decoder::new(&object.body, &object.links, Some(store));
    if r.u8().ok()? != STEP {
        return None;
    }
    let prev = r.link().ok()?;
    let len = usize::try_from(r.varint().ok()?).ok()?;
    let name = r.take(len).ok()?.to_vec();
    let head = Head::decode(&mut r).ok()?;
    Some((prev, name, head))
}

fn table_heads(store: &Store, ptr: Ptr, out: &mut Vec<(Vec<u8>, Head)>) {
    let Ok(table) = store.load::<Table>(ptr) else {
        return;
    };
    guarded(|| {
        table.try_for_each(|name, head| {
            out.push((name.as_bytes().to_vec(), head.clone()));
            match out.len() >= 400 {
                true => ControlFlow::Break(()),
                false => ControlFlow::Continue(()),
            }
        })
    });
}

fn survey(store: &Store, top: Ptr) -> Known {
    let mut known = Known::default();
    let mut at = top;
    for _ in 0..80 {
        known.chain.push(at);
        match parse_step(store, at) {
            Some((prev, name, head)) => {
                known.heads.push((name, head));
                at = prev;
            }
            None => {
                table_heads(store, at, &mut known.heads);
                break;
            }
        }
    }
    for (_, head) in &known.heads {
        known.objects.extend(head.root);
        known.objects.extend(head.tip);
    }
    if known.heads.is_empty() {
        known.heads.push((
            b"main".to_vec(),
            Head {
                version: 0,
                first: 1,
                root: None,
                tip: None,
                merged: Map::new(),
            },
        ));
    }
    known
}

fn number(input: &mut Input<'_>, old: u64) -> u64 {
    match input.byte() % 6 {
        0 => old.wrapping_add(1),
        1 => old.wrapping_sub(1),
        2 => 0,
        3 => old.wrapping_add(64),
        _ => input.num(),
    }
}

fn pick(input: &mut Input<'_>, known: &Known) -> Ptr {
    let control = input.byte();
    let from = |list: &[Ptr], at: u8| list.get(usize::from(at) % list.len().max(1)).copied();
    let chosen = match control % 6 {
        0 => from(&known.chain, input.byte()),
        1 | 2 => from(&known.objects, input.byte()),
        3 => known.chain.last().copied(),
        _ => None,
    };
    let base = chosen.unwrap_or(Ptr {
        hash: [control; 20],
        offset: input.num(),
    });
    match control / 8 % 4 {
        0 => Ptr {
            hash: base.hash,
            offset: number(input, base.offset),
        },
        1 => {
            let mut hash = base.hash;
            hash[usize::from(input.byte()) % hash.len()] ^= 1 << (input.byte() % 8);
            Ptr { hash, ..base }
        }
        _ => base,
    }
}

fn perturb(input: &mut Input<'_>, known: &Known, head: &Head) -> Head {
    let mut head = head.clone();
    let control = input.byte();
    if control & 1 != 0 {
        head.version = number(input, head.version);
    }
    if control & 2 != 0 {
        head.first = number(input, head.first);
    }
    if control & 4 != 0 {
        head.root = input.chance(12).then(|| pick(input, known));
    }
    if control & 8 != 0 {
        head.tip = input.chance(12).then(|| pick(input, known));
    }
    if control & 16 != 0 {
        head.merged.insert(input.text(), (input.num(), input.num()));
    }
    if control & 32 != 0 {
        let other = &known.heads[input.pick(known.heads.len())].1;
        head.tip = other.tip;
        head.version = other.version;
    }
    head
}

fn name(input: &mut Input<'_>, known: &Known) -> Vec<u8> {
    match input.byte() % 8 {
        0 => input.text().into_bytes(),
        1 => {
            let len = usize::from(input.byte() % 12);
            input.bytes(len).to_vec()
        }
        2 => vec![b'n'; 64 + usize::from(input.byte()) * 4],
        3 => b"main".to_vec(),
        _ => known.heads[input.pick(known.heads.len())].0.clone(),
    }
}

fn put_step(
    txn: &mut Txn,
    tag: u8,
    prev: Ptr,
    name: &[u8],
    head: &Head,
    tail: &[u8],
) -> Option<Ptr> {
    let mut w = Encoder::new(txn);
    w.u8(tag);
    w.link(prev);
    w.varint(name.len() as u64);
    w.raw(name);
    head.encode(&mut w).ok()?;
    w.raw(tail);
    w.finish().ok()
}

fn honest(input: &mut Input<'_>, known: &Known) -> (Vec<u8>, Head) {
    let (name, head) = &known.heads[input.pick(known.heads.len())];
    (name.clone(), head.clone())
}

fn step(txn: &mut Txn, input: &mut Input<'_>, known: &Known, prev: Ptr) -> Option<Ptr> {
    let control = input.byte();
    let prev = match control & 3 {
        0 => pick(input, known),
        _ => prev,
    };
    let (mut name_bytes, mut head) = honest(input, known);
    if control & 4 != 0 {
        name_bytes = name(input, known);
        head = perturb(input, known, &head);
    }
    let tag = match control & 8 {
        0 => STEP,
        _ => input.byte(),
    };
    let tail = match control & 16 {
        0 => &[][..],
        _ => input.bytes(2),
    };
    put_step(txn, tag, prev, &name_bytes, &head, tail)
}

fn run_of(txn: &mut Txn, input: &mut Input<'_>, known: &Known, mut prev: Ptr) -> Option<Ptr> {
    let count = match input.byte() % 8 {
        0 => 62,
        1 => 63,
        2 => 64,
        3 => 65,
        4 => 1022,
        5 => 1023,
        6 => 1024,
        _ => 1025,
    };
    let spoiled = input.chance(4).then(|| input.pick(count));
    for at in 0..count {
        let (name, mut head) = honest(input, known);
        if spoiled == Some(at) {
            head = perturb(input, known, &head);
        }
        prev = put_step(txn, STEP, prev, &name, &head, &[])?;
    }
    Some(prev)
}

fn table(txn: &mut Txn, input: &mut Input<'_>, known: &Known) -> Option<Ptr> {
    let count = match input.byte() % 8 {
        0 => 0,
        1 => 126,
        2 => 127,
        3 => 128,
        4 => 300,
        _ => usize::from(input.byte() % 16),
    };
    let mut refs = Table::new();
    for at in 0..count {
        let (name, head) = honest(input, known);
        let name = match at < known.heads.len() {
            true => String::from_utf8_lossy(&known.heads[at].0).into_owned(),
            false => format!("{}{at}", String::from_utf8_lossy(&name)),
        };
        let head = match input.chance(2) {
            true => perturb(input, known, &head),
            false => head,
        };
        refs.insert(name, head);
    }
    let mut w = Encoder::new(txn);
    refs.encode(&mut w).ok()?;
    w.finish().ok()
}

fn raw(txn: &mut Txn, input: &mut Input<'_>, known: &Known) -> Option<Ptr> {
    let mut w = Encoder::new(txn);
    let len = usize::from(input.byte());
    w.raw(input.bytes(len));
    for _ in 0..input.byte() % 4 {
        w.link(pick(input, known));
    }
    w.finish().ok()
}

fn forge_in(input: &mut Input<'_>, store: &Store, top: Ptr, frames: bool) -> Option<()> {
    let mut known = survey(store, top);
    let mut txn = store.begin().ok()?;
    let mut top = top;
    for _ in 0..1 + input.byte() % 4 {
        top = match input.byte() % 8 {
            0..=2 => step(&mut txn, input, &known, top)?,
            3 => run_of(&mut txn, input, &known, top)?,
            4 => table(&mut txn, input, &known)?,
            5 => raw(&mut txn, input, &known)?,
            6 => {
                let older = known.chain[input.pick(known.chain.len())];
                step(&mut txn, input, &known, older)?
            }
            _ => {
                let base = table(&mut txn, input, &known)?;
                step(&mut txn, input, &known, base)?
            }
        };
        known.chain.insert(0, top);
        if frames && input.chance(6) {
            txn.commit(top).ok()?;
            txn = store.begin().ok()?;
        }
    }
    txn.commit(top).ok()
}

fn forge(input: &mut Input<'_>, path: &Path, frames: bool) -> Option<()> {
    let options = StoreOptions {
        durability: Durability::Buffered,
        cache_bytes: 1 << 20,
        ..StoreOptions::default()
    };
    let (store, top) = Store::open(path, options).ok()?;
    let forged = top.and_then(|top| forge_in(input, &store, top, frames));
    store.close();
    forged
}

fn writes(input: &mut Input<'_>, db: &Db<Root>, long: bool, compact: bool) {
    let names = guarded(|| db.branches()).unwrap_or_default();
    let rounds = match long {
        true => 70,
        false => 1 + usize::from(input.byte() % 8),
    };
    for round in 0..rounds as u64 {
        let handle = match names.is_empty() {
            true => db.branch(db.branch_name()),
            false => db.branch(&names[input.pick(names.len())]),
        };
        let Ok(handle) = handle else {
            continue;
        };
        let wrote = guarded(|| {
            handle.update(|root| {
                root.insert(round % 7, format!("w{round}"));
            })
        });
        if wrote.is_none_or(|done| done.is_err()) && input.chance(8) {
            break;
        }
    }
    if let Ok(side) = db.fork("refs-side") {
        guarded(|| side.update(|root| root.insert(3, "side".into())));
        guarded(|| db.merge_with(&side, |_, _, _| ("merge side", ())));
        drop(side);
        let _ = db.drop_branch("refs-side");
    }
    if let Some(name) = names.iter().find(|name| name.as_str() != db.branch_name())
        && let Ok(other) = db.branch(name)
    {
        guarded(|| db.merge_with(&other, |_, _, _| ("merge other", ())));
        if input.chance(4) {
            drop(other);
            let _ = db.drop_branch(name);
        }
    }
    let _ = db.verify();
    if compact && db.compact(Keep::Last(2)).is_ok() {
        guarded(|| db.update(|root| root.insert(5, "after".into())));
        let _ = db.verify();
    }
}

fn start(input: &mut Input<'_>) -> (u8, &'static Base) {
    let control = input.byte();
    let all = bases();
    (control, &all[usize::from(input.byte()) % all.len()])
}

pub fn write(data: &[u8], path: &Path) -> Option<(u8, &'static str)> {
    let mut input = Input::new(data);
    let (control, base) = start(&mut input);
    fs::write(path, &base.image).ok()?;
    forge(&mut input, path, control & FRAMES != 0)?;
    Some((control, base.name))
}

pub fn run(data: &[u8]) {
    let mut input = Input::new(data);
    let (control, base) = start(&mut input);
    let path = crate::stage(&base.image);
    let follower = match control & FOLLOW {
        0 => None,
        _ => options(true).open::<Root>(&path).ok(),
    };
    let forged = forge(&mut input, &path, control & FRAMES != 0);
    if let Some(db) = &follower {
        let _ = db.refresh();
        crate::exercise(db, true);
    }
    if forged.is_none() {
        return;
    }
    let read_only = control & READ_ONLY != 0;
    if let Ok(db) = options(read_only).open::<Root>(&path) {
        crate::exercise(&db, read_only);
        let _ = db.verify_typed();
        if control & WRITES != 0 && !read_only {
            writes(&mut input, &db, control & LONG != 0, control & COMPACT != 0);
        }
    }
    if let Some(db) = &follower {
        let _ = db.refresh();
        crate::exercise(db, true);
        let _ = db.verify();
    }
    drop(follower);
    if control & ADMIN != 0 {
        crate::run_admin(&path, control & COMPACT != 0);
    }
}
