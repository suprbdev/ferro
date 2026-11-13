use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ferro::codec::{Encoder, Object, Sink};
use ferro::db::Hook;
use ferro::store::{Op, Store, StoreOptions, Trace};
use ferro::{Bytes, Data, Db, Durability, Error, Keep, Map, Options, Reclaim};
use ferro_fuzz::catalog::{
    Catalog, CatalogV1, CatalogV3, Pairs, World, item, item_v1, item_v3, word,
};
use ferro_fuzz::fixup::{FRAME_BYTES, FRAME_CHECKED, LOG_START, STAGING_MAGIC, seal_staging};
use ferro_fuzz::sync::{self as fuzz_sync, BOTH, DIRECT, EDITS, PACK_SMART, REHASH, REOPEN};
use ferro_fuzz::types::{Docs, Nested, Order, Probe, Root, Shop, Status, Summed};
use ferro_fuzz::{
    CATALOG, CATALOG_V1, CATALOG_V3, COUNTERS, DAMAGE, DOCS, NESTED, OPAQUE, PAIRS, READ_ONLY,
    ROOT, SHOP, SUMMED, SYNC_OUT, WORLD, WRITES, mode,
};

const LARGEST: u64 = 256 << 10;
const FITS: usize = 69_000;

struct Corpus {
    out: PathBuf,
    seen: HashSet<blake3::Hash>,
    large: usize,
}

impl Corpus {
    fn put(&self, target: &str, data: &[u8]) {
        let dir = self.out.join(target);
        fs::create_dir_all(&dir).unwrap();
        let name = blake3::hash(data).to_hex();
        fs::write(dir.join(&name[..16]), data).unwrap();
    }

    fn emit(&mut self, kind: u8, bytes: &[u8]) {
        if !self.seen.insert(blake3::hash(&[&[kind], bytes].concat())) {
            return;
        }
        let target = if bytes.len() > FITS {
            self.large += 1;
            "open-large"
        } else {
            "open"
        };
        for flags in [READ_ONLY, WRITES, SYNC_OUT, READ_ONLY | WRITES] {
            let mut data = vec![mode(kind, flags)];
            data.extend_from_slice(bytes);
            self.put(target, &data);
        }
        if bytes.len() <= FITS {
            for admin_mode in [0, 0x0e] {
                let mut data = vec![admin_mode];
                data.extend_from_slice(bytes);
                self.put("admin", &data);
            }
            for reclaim_mode in [0, 0x04, 0x02 | ferro_fuzz::reclaim::AGAIN] {
                let mut data = vec![reclaim_mode];
                data.extend_from_slice(bytes);
                self.put("reclaim", &data);
            }
        }
    }

    fn emit_file(&mut self, kind: u8, path: &Path) {
        let bytes = fs::read(path).unwrap();
        self.emit(kind, &bytes);
        if kind != OPAQUE {
            self.emit(OPAQUE, &bytes);
        }
    }

    fn emit_sync(&mut self, data: &[u8]) {
        if self.seen.insert(blake3::hash(&[b"sync", data].concat())) {
            self.put("sync", data);
        }
    }
}

fn options() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20)
        .reclaim(Reclaim::off())
}

fn text(seed: u64, len: usize) -> String {
    (0..len)
        .map(|at| {
            char::from(b'a' + ((seed.wrapping_mul(31).wrapping_add(at as u64 * 7)) % 26) as u8)
        })
        .collect()
}

fn scenario<T: Data + Default>(
    corpus: &mut Corpus,
    dir: &Path,
    name: &str,
    kind: u8,
    steps: impl FnOnce(&Db<T>) -> ferro::Result<()>,
) -> PathBuf {
    let path = dir.join(format!("{name}.ferro"));
    let db = options().open::<T>(&path).unwrap();
    steps(&db).unwrap();
    drop(db);
    corpus.emit_file(kind, &path);
    path
}

fn history(db: &Db<Root>, commits: u64, len: usize) -> ferro::Result<()> {
    for version in 0..commits {
        db.commit(format!("c{version}"), |root| {
            root.insert(version % 7, text(version, len));
            if version % 3 == 2 {
                root.remove(&(version / 2));
            }
        })?;
    }
    Ok(())
}

fn generated(corpus: &mut Corpus, dir: &Path) {
    scenario::<Root>(corpus, dir, "empty", ROOT, |_| Ok(()));
    scenario::<Root>(corpus, dir, "small", ROOT, |db| history(db, 3, 4));
    scenario::<Root>(corpus, dir, "spill", ROOT, |db| history(db, 6, 700));
    scenario::<Root>(corpus, dir, "lines", ROOT, |db| {
        db.commit("one\ntwo", |root| {
            root.insert(1, "a".into());
        })?;
        db.commit_stamped(|root, stamp| {
            root.insert(stamp.version(), text(stamp.time_ms(), 3));
            ("stamped", ())
        })?;
        db.commit_with(|root| {
            root.insert(9, "with".into());
            ("with a message", ())
        })?;
        db.restore(&db.at(1)?, "restore v1")?;
        Ok(())
    });
    scenario::<Root>(corpus, dir, "branches", ROOT, |db| {
        history(db, 9, 12)?;
        let side = db.fork("side")?;
        history(&side, 5, 8)?;
        history(db, 2, 3)?;
        db.merge_with(&side, |root, _, theirs| {
            for (key, value) in theirs.iter() {
                root.insert(key + 100, (*value).clone());
            }
            ("merge side", ())
        })?;
        let gone = db.fork("gone")?;
        history(&gone, 2, 2)?;
        db.drop_branch("gone")?;
        history(&side, 1, 2)?;
        Ok(())
    });
    let path = dir.join("compacted.ferro");
    {
        let db = options().open::<Root>(&path).unwrap();
        history(&db, 20, 10).unwrap();
        let side = db.fork("side").unwrap();
        history(&side, 3, 5).unwrap();
        db.merge_with(&side, |_, _, _| ("merge", ())).unwrap();
        db.compact(Keep::Last(4)).unwrap();
        history(&db, 2, 5).unwrap();
    }
    corpus.emit_file(ROOT, &path);
    let legacy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ferro/tests/fixtures/legacy.db");
    corpus.emit_file(COUNTERS, &legacy);
    let tagged =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../ferro/tests/fixtures/tagged-indexes.db");
    corpus.emit_file(OPAQUE, &tagged);
    scenario::<Shop>(corpus, dir, "shop", SHOP, |db| {
        db.commit("stock", |shop| {
            shop.stock.insert("anvil".into(), 5);
            shop.stock.insert("rocket".into(), 2);
        })?;
        for id in 1..4 {
            db.commit(format!("order {id}"), |shop| {
                shop.next_order = id;
                shop.orders.insert(Order {
                    id,
                    customer: if id % 2 == 0 {
                        "wile".into()
                    } else {
                        "road".into()
                    },
                    status: Status::Pending,
                    total_cents: id * 1000,
                });
            })?;
        }
        db.commit("ship", |shop| {
            if let Some(mut order) = shop.orders.get_mut(&2) {
                order.status = Status::Shipped;
            }
        })?;
        Ok(())
    });
    scenario::<Docs>(corpus, dir, "docs", DOCS, |db| {
        for version in 0..4u64 {
            db.commit("doc", |docs| {
                let body = text(version, 300 * version as usize + 50).into_bytes();
                docs.insert(format!("doc{version}"), Bytes::from(body));
            })?;
        }
        Ok(())
    });
    scenario::<Nested>(corpus, dir, "nested", NESTED, |db| {
        for version in 0..5u64 {
            db.commit("nest", |nested| {
                let inner = nested.get_or_insert_with(text(version % 2, 3), Map::new);
                inner.insert(version, text(version, 5));
            })?;
        }
        Ok(())
    });
    scenario::<Summed>(corpus, dir, "summed", SUMMED, |db| {
        for version in 0..40u64 {
            db.commit("sum", |summed| {
                summed.insert(version * 7 % 23, version);
            })?;
        }
        Ok(())
    });
}

fn schemas(corpus: &mut Corpus, dir: &Path) {
    let path = dir.join("schemas.ferro");
    {
        let db = options().open::<CatalogV1>(&path).unwrap();
        for round in 0..3u64 {
            db.commit(format!("v1 round {round}"), |catalog| {
                for id in (0..24).step_by(1 + round as usize) {
                    let _ = catalog.items.try_insert(item_v1(id, round));
                }
                catalog.notes.insert(round as u32, word(round, 6));
                catalog.stamp = round;
            })
            .unwrap();
        }
        let side = db.fork("old").unwrap();
        side.commit("old side", |catalog| catalog.poke(5)).unwrap();
    }
    let v1 = fs::read(&path).unwrap();
    for kind in [CATALOG_V1, CATALOG, CATALOG_V3] {
        corpus.emit(kind, &v1);
    }
    {
        let db = options().open::<Catalog>(&path).unwrap();
        for round in 3..6u64 {
            db.commit(format!("v2 round {round}"), |catalog| {
                for id in (0..30).step_by(1 + round as usize) {
                    let _ = catalog.items.try_insert(item(id, round));
                }
                catalog.poke(round);
            })
            .unwrap();
        }
        let side = db.fork("new").unwrap();
        side.commit("new side", |catalog| catalog.poke(17)).unwrap();
        db.merge_with(&side, |catalog, _, _| {
            catalog.poke(18);
            ("merge", ())
        })
        .unwrap();
    }
    let v2 = fs::read(&path).unwrap();
    for kind in [CATALOG_V1, CATALOG, CATALOG_V3] {
        corpus.emit(kind, &v2);
    }
    {
        let db = options().open::<CatalogV3>(&path);
        if let Ok(db) = db {
            db.commit("v3", |catalog| {
                for id in 40..50 {
                    let _ = catalog.items.try_insert(item_v3(id, 2));
                }
            })
            .unwrap();
        }
    }
    let v3 = fs::read(&path).unwrap();
    for kind in [CATALOG_V1, CATALOG, CATALOG_V3] {
        corpus.emit(kind, &v3);
    }
    scenario::<CatalogV3>(corpus, dir, "v3", CATALOG_V3, |db| {
        for round in 0..3u64 {
            db.commit("v3", |catalog| {
                for id in (0..20).step_by(1 + round as usize) {
                    let _ = catalog.items.try_insert(item_v3(id, round));
                }
                catalog.poke(round);
            })?;
        }
        Ok(())
    });
    let path = scenario::<Catalog>(corpus, dir, "covered", CATALOG, |db| {
        for round in 0..4u64 {
            db.commit("covered", |catalog| {
                for id in 0..40 {
                    let _ = catalog.items.try_insert(item(id, round));
                    let _ = catalog.items.try_update(&id, |row| {
                        row.price += round as i64;
                        row.tier = item(id, round + 1).tier;
                    });
                }
                if round == 3 {
                    for id in (0..40).step_by(3) {
                        catalog.items.remove(&id);
                    }
                }
            })?;
        }
        Ok(())
    });
    corpus.emit_file(CATALOG_V1, &path);
    corpus.emit_file(CATALOG_V3, &path);
}

fn lanes(corpus: &mut Corpus, dir: &Path) {
    for (round, writers) in [(0u64, 3usize), (1, 6)] {
        let path = dir.join(format!("lanes{round}.ferro"));
        {
            let db = options()
                .coalesce(round == 1)
                .open::<Catalog>(&path)
                .unwrap();
            db.commit("seed", |catalog| catalog.poke(1)).unwrap();
            let branches: Vec<Db<Catalog>> = (0..writers)
                .map(|at| match at {
                    0 => db.clone(),
                    _ => db.fork(&format!("lane{at}")).unwrap(),
                })
                .collect();
            let barrier = Arc::new(Barrier::new(writers));
            thread::scope(|scope| {
                for (at, branch) in branches.iter().enumerate() {
                    let barrier = barrier.clone();
                    let db = &db;
                    scope.spawn(move || {
                        barrier.wait();
                        for step in 0..12u64 {
                            branch
                                .commit(format!("lane {at} step {step}"), |catalog| {
                                    catalog.poke(step * 7 + at as u64)
                                })
                                .unwrap();
                            if step % 5 == 4 && at > 0 {
                                let _ = db.merge_with(branch, |catalog, _, _| {
                                    catalog.poke(step);
                                    ("merge lane", ())
                                });
                            }
                        }
                    });
                }
            });
            if round == 1 {
                db.drop_branch("lane2").unwrap();
                db.compact(Keep::Last(3)).unwrap();
                db.branch("lane1")
                    .unwrap()
                    .commit("after", |catalog| catalog.poke(99))
                    .unwrap();
            }
        }
        corpus.emit_file(CATALOG, &path);
    }
    let path = dir.join("lanes-pairs.ferro");
    {
        let db = options().open::<Pairs>(&path).unwrap();
        let handles: Vec<Db<Pairs>> = (0..4)
            .map(|at| match at {
                0 => db.clone(),
                _ => db.fork(&format!("p{at}")).unwrap(),
            })
            .collect();
        thread::scope(|scope| {
            for (at, branch) in handles.iter().enumerate() {
                scope.spawn(move || {
                    for step in 0..20u32 {
                        branch
                            .update(|pairs| {
                                pairs.insert(step % 5, step + at as u32);
                            })
                            .unwrap();
                    }
                });
            }
        });
    }
    corpus.emit_file(PAIRS, &path);
}

fn relayed(corpus: &mut Corpus, dir: &Path) {
    for seed in 0..4u64 {
        let keeps = [Keep::All, Keep::Last(2), Keep::Last(1)];
        let paths: Vec<PathBuf> = (0..3)
            .map(|at| dir.join(format!("relay{seed}-{at}.ferro")))
            .collect();
        let devices: Vec<Db<Pairs>> = paths
            .iter()
            .zip(keeps)
            .map(|(path, keep)| options().history(keep).open(path).unwrap())
            .collect();
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move |limit: u64| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D) % limit
        };
        for _ in 0..60 {
            let to = next(3) as usize;
            let names = devices[to].branches();
            let name = names[next(names.len() as u64) as usize].clone();
            let Ok(handle) = devices[to].branch(&name) else {
                continue;
            };
            match next(10) {
                0..=3 => {
                    let (key, value) = (next(4) as u32, next(3) as u32);
                    handle
                        .update(|pairs| {
                            pairs.insert(key, value);
                        })
                        .unwrap();
                }
                4 => {
                    let _ = handle.fork(["p", "q"][next(2) as usize]);
                }
                5..=7 => {
                    let from = (to + 1 + next(2) as usize) % 3;
                    let sources = devices[from].branches();
                    let source = sources[next(sources.len() as u64) as usize].clone();
                    let remote = format!("d{from}-{source}");
                    if remote == name {
                        continue;
                    }
                    let out = devices[from].branch(&source).unwrap();
                    let merge = |ours: &mut Pairs,
                                 _: &ferro::Snapshot<Pairs>,
                                 theirs: &ferro::Snapshot<Pairs>| {
                        for (key, value) in theirs.iter() {
                            ours.insert(key, *value);
                        }
                        ("merge", ())
                    };
                    match handle.pull(&out, &remote, merge) {
                        Ok(_) | Err(Error::NoCommonAncestor | Error::StaleHave(_)) => {}
                        Err(error) => panic!("{error}"),
                    }
                }
                _ => {
                    let keep = [Keep::All, Keep::Last(1), Keep::Last(2)][next(3) as usize];
                    devices[to].compact(keep).unwrap();
                }
            }
        }
        drop(devices);
        for path in &paths {
            corpus.emit_file(PAIRS, path);
        }
    }
}

fn small(db: &Db<Root>, key: u64, seed: u64) {
    db.update(|root| {
        root.insert(key, text(seed, 6));
    })
    .unwrap();
}

fn steps(corpus: &mut Corpus, dir: &Path) {
    let path = dir.join("steps.ferro");
    {
        let db = options().open::<Root>(&path).unwrap();
        small(&db, 0, 0);
        let side = db.fork("side").unwrap();
        for count in 1..=130u64 {
            small(if count % 3 == 0 { &db } else { &side }, count % 9, count);
            if matches!(
                count,
                1 | 2 | 62 | 63 | 64 | 65 | 66 | 127 | 128 | 129 | 130
            ) {
                corpus.emit(ROOT, &fs::read(&path).unwrap());
            }
        }
    }
    for (branches, commits) in [(126u64, 1u64), (127, 2), (40, 90)] {
        let path = dir.join(format!("branches{branches}.ferro"));
        {
            let db = options().open::<Root>(&path).unwrap();
            small(&db, 0, 0);
            let forks: Vec<Db<Root>> = (0..branches)
                .map(|at| db.fork(&format!("b{at}")).unwrap())
                .collect();
            for count in 0..commits {
                small(&forks[(count * 7 % branches) as usize], count % 5, count);
            }
            db.drop_branch("b3").unwrap();
            small(&forks[1], 1, 1);
        }
        let bytes = fs::read(&path).unwrap();
        corpus.emit(ROOT, &bytes);
        corpus.emit(OPAQUE, &bytes);
    }
}

fn wide_steps(corpus: &mut Corpus, dir: &Path) {
    let path = dir.join("covered-steps.ferro");
    {
        let db = options().open::<Catalog>(&path).unwrap();
        db.commit("seed", |catalog| {
            for id in 0..12 {
                let _ = catalog.items.try_insert(item(id, 0));
            }
        })
        .unwrap();
        let forks: Vec<Db<Catalog>> = (0..20)
            .map(|at| db.fork(&format!("c{at:02}")).unwrap())
            .collect();
        for count in 1..=140u64 {
            let branch = &forks[(count * 7 % 20) as usize];
            branch
                .commit(format!("covered {count}"), |catalog| {
                    let id = (count % 12) as u32;
                    let _ = catalog.items.try_update(&id, |row| {
                        row.price += count as i64;
                        row.tier = item(id, count).tier;
                        row.label = word(count, 3);
                    });
                })
                .unwrap();
            if matches!(count, 43 | 44 | 45 | 107 | 108 | 109 | 140) {
                corpus.emit(CATALOG, &fs::read(&path).unwrap());
            }
        }
        db.drop_branch("c05").unwrap();
    }
    corpus.emit_file(CATALOG, &path);
    let path = dir.join("branches200.ferro");
    {
        let db = options().open::<Root>(&path).unwrap();
        small(&db, 0, 0);
        let forks: Vec<Db<Root>> = (0..200u64)
            .map(|at| db.fork(&format!("wide-{at:03}")).unwrap())
            .collect();
        for count in 0..70u64 {
            small(&forks[(count * 13 % 200) as usize], count % 5, count);
        }
    }
    let bytes = fs::read(&path).unwrap();
    corpus.emit(ROOT, &bytes);
    corpus.emit(OPAQUE, &bytes);
}

const FRAME_MAGIC: &[u8; 8] = b"FERROFRM";

fn wait_until(ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "a held frame never moved");
        thread::sleep(Duration::from_micros(200));
    }
}

#[derive(Default)]
struct Held {
    placed: AtomicBool,
    released: AtomicBool,
    landed: AtomicUsize,
}

fn out_of_order(corpus: &mut Corpus, dir: &Path) {
    let path = dir.join("out-of-order.ferro");
    let writers = 4u64;
    {
        let db = options().open::<Catalog>(&path).unwrap();
        db.commit("seed", |catalog| catalog.poke(4)).unwrap();
        for at in 0..writers {
            db.fork(&format!("o{at}")).unwrap();
        }
    }
    let base = fs::read(&path).unwrap();
    let trace: Trace = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let held = Arc::new(Held::default());
    let seen = held.clone();
    let hook = Hook(Arc::new(move |step| {
        match (thread::current().name(), step) {
            (Some("held"), "landing") => {
                seen.placed.store(true, Ordering::SeqCst);
                wait_until(|| seen.released.load(Ordering::SeqCst));
            }
            (Some("ahead"), "landed") => {
                seen.landed.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }));
    {
        let db = options()
            .checkpoint_bytes(4096)
            .trace(trace.clone())
            .hook(hook)
            .open::<Catalog>(&path)
            .unwrap();
        let commit = |at: u64, round: u64, name: &str| {
            let handle = db.branch(&format!("o{at}")).unwrap();
            thread::Builder::new()
                .name(name.to_string())
                .spawn(move || {
                    handle
                        .commit(format!("o{at} {round}"), |catalog| {
                            catalog.poke(round * 4 + at)
                        })
                        .unwrap();
                })
                .unwrap()
        };
        for round in 0..24u64 {
            held.placed.store(false, Ordering::SeqCst);
            held.released.store(false, Ordering::SeqCst);
            held.landed.store(0, Ordering::SeqCst);
            let first = commit(round % writers, round, "held");
            wait_until(|| held.placed.load(Ordering::SeqCst));
            let later: Vec<_> = (1..writers)
                .map(|step| commit((round + step) % writers, round, "ahead"))
                .collect();
            wait_until(|| held.landed.load(Ordering::SeqCst) == later.len());
            held.released.store(true, Ordering::SeqCst);
            first.join().unwrap();
            for handle in later {
                handle.join().unwrap();
            }
        }
    }
    let ops = trace.lock().clone();
    let frames: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| matches!(op, Op::Write(_, _, bytes) if bytes.starts_with(FRAME_MAGIC)))
        .map(|(at, _)| at)
        .collect();
    let offset = |at: usize| match &ops[at] {
        Op::Write(_, offset, _) => *offset,
        _ => 0,
    };
    let early: Vec<usize> = frames
        .windows(2)
        .filter(|pair| offset(pair[1]) < offset(pair[0]))
        .map(|pair| pair[0])
        .collect();
    println!(
        "out of order: {} frames, {} landed before a frame below them",
        frames.len(),
        early.len()
    );
    let image = |skip: &[usize], upto: usize| {
        let mut image = base.clone();
        for (at, op) in ops[..upto].iter().enumerate() {
            if !skip.contains(&at) {
                replay(&mut image, op, &mut || true, false);
            }
        }
        image
    };
    let mut by_offset = frames.clone();
    by_offset.sort_by_key(|at| offset(*at));
    for hole in by_offset
        .iter()
        .step_by(by_offset.len().div_ceil(10).max(1))
    {
        corpus.emit(CATALOG, &image(&[*hole], ops.len()));
        corpus.emit(CATALOG, &image(&[*hole], hole + 3));
    }
    for late in early.iter().take(8) {
        let below = frames
            .iter()
            .copied()
            .filter(|at| at > late && offset(*at) < offset(*late))
            .collect::<Vec<_>>();
        corpus.emit(CATALOG, &image(&[], late + 1));
        corpus.emit(CATALOG, &image(&below, ops.len()));
    }
}

fn damage_seeds(corpus: &mut Corpus) {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for seed in 0..64u64 {
        let recipe = match seed {
            0..48 => 0,
            48..56 => 0xc0,
            _ => 0xe0,
        };
        let mut data = vec![mode(DAMAGE, 0), recipe | (seed % 16) as u8];
        for _ in 0..64 + seed * 3 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.push(state as u8);
        }
        corpus.put("open", &data);
    }
}

fn refs_seeds(corpus: &mut Corpus) {
    use ferro_fuzz::refs::{ADMIN, COMPACT, FOLLOW, FRAMES, LONG, READ_ONLY, WRITES};
    let controls = [
        0,
        FOLLOW,
        WRITES,
        WRITES | LONG,
        FOLLOW | WRITES | FRAMES,
        READ_ONLY | ADMIN,
        WRITES | COMPACT | ADMIN,
        FOLLOW | WRITES | LONG | COMPACT,
    ];
    let forges: [&[u8]; 10] = [
        &[0, 0, 1],
        &[0, 0, 4, 0],
        &[0, 3, 2],
        &[0, 3, 6],
        &[0, 4, 2],
        &[1, 4, 4, 1, 1],
        &[2, 6, 1, 0, 7, 3, 0, 0],
        &[0, 7, 1, 4, 4, 8, 2],
        &[3, 0, 0, 2, 3, 0, 0, 5, 9, 1, 1, 0, 1],
        &[
            0, 5, 40, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 2, 1, 0,
        ],
    ];
    for base in 0..ferro_fuzz::refs::bases().len() as u8 {
        for (at, control) in controls.iter().enumerate() {
            for forge in forges.iter().skip(at % 2).step_by(2) {
                let mut data = vec![*control, base];
                data.extend_from_slice(forge);
                data.extend_from_slice(&[3, 1, 4, 1, 5, 9, 2, 6]);
                corpus.put("refs", &data);
            }
        }
    }
    let named_past_its_tip = [
        0xad, 0x00, 0x08, 0x00, 0x05, 0x00, 0x93, 0x83, 0x8d, 0x93, 0x93, 0x93, 0x93, 0x93, 0x77,
        0x93, 0x93, 0x6c, 0x00, 0x45,
    ];
    corpus.put("refs", &named_past_its_tip);
}

struct Lcg(u64);

impl Lcg {
    fn chance(&mut self) -> bool {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 63 == 1
    }
}

fn replay(image: &mut Vec<u8>, op: &Op, keep: &mut impl FnMut() -> bool, torn: bool) {
    match op {
        Op::Write(_, at, bytes) => {
            let start = *at as usize;
            let end = start + bytes.len();
            if image.len() < end {
                image.resize(end, 0);
            }
            if !torn {
                image[start..end].copy_from_slice(bytes);
                return;
            }
            let mut offset = start;
            while offset < end {
                let stop = ((offset / 512 + 1) * 512).min(end);
                if keep() {
                    image[offset..stop].copy_from_slice(&bytes[offset - start..stop - start]);
                }
                offset = stop;
            }
        }
        Op::SetLen(_, len) => image.resize(*len as usize, 0),
        _ => {}
    }
}

fn crashes(corpus: &mut Corpus, dir: &Path) {
    let path = dir.join("crash.ferro");
    let options = || options().checkpoint_bytes(2048);
    let writers = 5u64;
    {
        let db = options().open::<Root>(&path).unwrap();
        small(&db, 0, 0);
        for branch in 0..writers {
            db.fork(&format!("w{branch}")).unwrap();
        }
    }
    let base = fs::read(&path).unwrap();
    let trace: Trace = Arc::new(parking_lot::Mutex::new(Vec::new()));
    {
        let db = options().trace(trace.clone()).open::<Root>(&path).unwrap();
        thread::scope(|scope| {
            for branch in 0..writers {
                let handle = db.branch(&format!("w{branch}")).unwrap();
                scope.spawn(move || {
                    for count in 0..28 {
                        small(&handle, count % 4, branch * 100 + count);
                    }
                });
            }
            let forker = db.clone();
            scope.spawn(move || {
                for round in 0..3 {
                    let name = format!("extra{round}");
                    forker.fork(&name).unwrap();
                    forker.drop_branch(&name).unwrap();
                }
            });
        });
    }
    let ops = trace.lock().clone();
    let mut rng = Lcg(17);
    let cuts = 14;
    for at in 1..=cuts {
        let cut = ops.len() * at / cuts;
        for loss in 0..5 {
            let mut image = base.clone();
            let mut pending: Vec<&Op> = Vec::new();
            for op in &ops[..cut] {
                match op {
                    Op::Write(..) | Op::SetLen(..) => pending.push(op),
                    Op::Sync(_) => {
                        for done in pending.drain(..) {
                            replay(&mut image, done, &mut || true, false);
                        }
                    }
                    _ => {}
                }
            }
            for op in pending {
                let superblock = matches!(op, Op::Write(_, at, _) if *at < 8192);
                match loss {
                    0 => {}
                    1 => replay(&mut image, op, &mut || true, false),
                    2 => replay(&mut image, op, &mut || rng.chance(), true),
                    3 if superblock => replay(&mut image, op, &mut || true, false),
                    4 if !superblock => replay(&mut image, op, &mut || true, false),
                    _ => {}
                }
            }
            corpus.emit(ROOT, &image);
        }
    }
}

fn value(sink: &mut dyn Sink, value: &impl Data) -> ferro::Ptr {
    let mut encoder = Encoder::new(sink);
    value.encode(&mut encoder).unwrap();
    encoder.finish().unwrap()
}

fn pieces(corpus: &mut Corpus, dir: &Path) {
    let path = dir.join("pieces.ferro");
    {
        let db = options().open::<Root>(&path).unwrap();
        history(&db, 5, 20).unwrap();
        db.fork("side").unwrap();
    }
    let store_options = StoreOptions {
        durability: Durability::Buffered,
        cache_bytes: 1 << 20,
        ..StoreOptions::default()
    };
    for round in 0..2u64 {
        let (store, head) = Store::open(&path, store_options.clone()).unwrap();
        let head = head.unwrap();
        let mut txn = store.begin().unwrap();
        let mut root = Root::new();
        for key in 0..30 {
            root.insert(key, text(key + round * 100, 40));
        }
        let tree = value(&mut txn, &root);
        let body = [&[round as u8 + 9], &b"unlinked commit"[..]].concat();
        let object = Object {
            body,
            links: vec![tree, head],
        };
        let hash = object.hash();
        txn.put(object, hash).unwrap();
        txn.commit(head).unwrap();
        store.close();
    }
    corpus.emit_file(ROOT, &path);
    let mut torn = fs::read(&path).unwrap();
    let keep = torn.len() - 40;
    torn.truncate(keep);
    corpus.emit(ROOT, &torn);
}

fn word_at(bytes: &[u8], at: usize) -> usize {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize
}

fn append_staging(path: &Path, records: impl FnOnce(&[u8]) -> Vec<u8>) -> usize {
    let bytes = fs::read(path).unwrap();
    let (mut at, mut last, mut before) = (LOG_START, None, None);
    while at + FRAME_BYTES <= bytes.len() {
        if bytes[at..].starts_with(FRAME_MAGIC) {
            last = Some(at);
        } else if !bytes[at..].starts_with(STAGING_MAGIC) {
            break;
        }
        before = Some(at);
        at = word_at(&bytes, at + 16);
    }
    assert_eq!(at, bytes.len(), "the log ends the file");
    let (last, before) = last.zip(before).expect("a frame before the staging frame");
    let body = &bytes[last + FRAME_BYTES..word_at(&bytes, last + 16)];
    let records = records(body);
    let mut frame = vec![0u8; FRAME_BYTES];
    frame[..8].copy_from_slice(STAGING_MAGIC);
    let seq = word_at(&bytes, before + 8) as u64 + 1;
    frame[8..16].copy_from_slice(&seq.to_le_bytes());
    let end = (at + FRAME_BYTES + records.len()) as u64;
    frame[16..24].copy_from_slice(&end.to_le_bytes());
    frame[24..FRAME_CHECKED].copy_from_slice(&bytes[before + 24..before + FRAME_CHECKED]);
    seal_staging(&mut frame, 0);
    frame.extend_from_slice(&records);
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, &frame, at as u64).unwrap();
    at
}

fn staging_frames(corpus: &mut Corpus, dir: &Path) {
    let path = dir.join("staging.ferro");
    let db = options().shared(true).open::<Root>(&path).unwrap();
    small(&db, 0, 0);
    small(&db, 1, 1);
    let first = append_staging(&path, |body| body.to_vec());
    let ending = fs::read(&path).unwrap();
    corpus.emit(ROOT, &ending);
    corpus.emit(ROOT, &ending[..first + FRAME_BYTES / 2]);
    small(&db, 2, 2);
    let side = db.fork("side").unwrap();
    small(&side, 3, 3);
    corpus.emit(ROOT, &fs::read(&path).unwrap());
    let second = append_staging(&path, |body| vec![0; body.len().min(512)]);
    let refused = fs::read(&path).unwrap();
    corpus.emit(ROOT, &refused);
    let mut flipped = refused.clone();
    flipped[second + 20] ^= 0x04;
    corpus.emit(ROOT, &flipped);
    small(&db, 4, 4);
    drop((side, db));
    corpus.emit_file(ROOT, &path);
}

fn mid_receive(corpus: &mut Corpus, dir: &Path) {
    let scenario = fuzz_sync::scenarios()
        .iter()
        .find(|scenario| scenario.name == "spill")
        .unwrap();
    let path = dir.join("mid-receive.ferro");
    fs::write(&path, &scenario.image).unwrap();
    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let watch = {
        let captured = captured.clone();
        let path = path.clone();
        move |step: &'static str| {
            if step == "placed" || step == "received" {
                captured.lock().unwrap().push(fs::read(&path).unwrap());
            }
        }
    };
    let db: Db<World> = fuzz_sync::options()
        .hook(Hook(Arc::new(watch)))
        .open(&path)
        .unwrap();
    db.receive(
        &scenario.remote,
        &ferro::Bundle::from(scenario.bundle.clone()),
        |_, _, _| ("merge", ()),
    )
    .unwrap();
    drop(db);
    for image in captured.lock().unwrap().iter() {
        corpus.emit(WORLD, image);
    }
    corpus.emit_file(WORLD, &path);
}

fn sync_seeds(corpus: &mut Corpus, dir: &Path) {
    let all = fuzz_sync::scenarios();
    for (at, scenario) in all.iter().enumerate() {
        let s = at as u8;
        println!(
            "sync scenario {at} {}: image {} bytes, bundle {} bytes, parsed {}",
            scenario.name,
            scenario.image.len(),
            scenario.bundle.len(),
            scenario.parsed.is_some()
        );
        if scenario.image.len() <= FITS {
            corpus.emit(WORLD, &scenario.image);
        }
        corpus.emit_sync(&[s, 0, 0, 0]);
        corpus.emit_sync(&[s, REOPEN, 4, 0]);
        corpus.emit_sync(&[s, PACK_SMART, 6, 0]);
        corpus.emit_sync(&[s, REHASH | PACK_SMART, 0, 1, 0, 3, 5, 2]);
        corpus.emit_sync(&[s, REHASH, 0, 2, 4, 9, 1, 0, 12, 0, 1]);
        corpus.emit_sync(&[s, 0, 5, 1, 17, 1, 1]);
        corpus.emit_sync(&[s, EDITS, 0, 1, 40, 0, 7]);
        corpus.emit_sync(&[s, BOTH | REHASH, 7, 1, 3, 2, 1, 0, 9, 0]);
        corpus.emit_sync(&[s, 0x40, 0, 3, 0]);
        corpus.emit_sync(&[s, 0xc0, 0, 9, 3, 0]);
        corpus.emit_sync(&[s, 0x80, 0, 200, 0]);
        if scenario.bundle.len() <= FITS {
            let mut data = vec![s, DIRECT, 0];
            data.extend_from_slice(&scenario.bundle);
            corpus.emit_sync(&data);
        }
        let path = dir.join(format!("landed-{at}.ferro"));
        fs::write(&path, &scenario.image).unwrap();
        let db: Db<World> = fuzz_sync::options().open(&path).unwrap();
        let to = db.branch(&scenario.branch).unwrap();
        let bundle = ferro::Bundle::from(scenario.bundle.clone());
        if to
            .receive(&scenario.remote, &bundle, |_, _, _| ("merge", ()))
            .is_ok()
        {
            drop(to);
            drop(db);
            corpus.emit(WORLD, &fs::read(&path).unwrap());
        }
    }
}

fn collect(corpus: &mut Corpus, path: &Path) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = fs::read_dir(path)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        for entry in entries {
            collect(corpus, &entry);
        }
        return;
    }
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len() > LARGEST {
        return;
    }
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    if !bytes.starts_with(ferro_fuzz::fixup::MAGIC) {
        return;
    }
    let kinds: &[u8] = if bytes.len() > FITS {
        &[OPAQUE]
    } else {
        &[OPAQUE, ROOT, PAIRS]
    };
    for &kind in kinds {
        if !corpus
            .seen
            .insert(blake3::hash(&[&[kind], &bytes[..]].concat()))
        {
            continue;
        }
        let target = if bytes.len() > FITS {
            "open-large"
        } else {
            "open"
        };
        let flags = match kind {
            OPAQUE => WRITES | SYNC_OUT,
            ROOT => READ_ONLY,
            _ => WRITES,
        };
        let mut data = vec![mode(kind, flags)];
        data.extend_from_slice(&bytes);
        corpus.put(target, &data);
        if kind == OPAQUE && bytes.len() <= FITS {
            data[0] = 0;
            corpus.put("admin", &data);
            corpus.put("reclaim", &data);
        }
    }
}

fn main() {
    let mut args = env::args_os().skip(1);
    let out = PathBuf::from(
        args.next()
            .expect("usage: seeds <corpus dir> [captured files or dirs]"),
    );
    let mut corpus = Corpus {
        out,
        seen: HashSet::new(),
        large: 0,
    };
    let dir = tempfile::tempdir().unwrap();
    generated(&mut corpus, dir.path());
    schemas(&mut corpus, dir.path());
    lanes(&mut corpus, dir.path());
    relayed(&mut corpus, dir.path());
    pieces(&mut corpus, dir.path());
    steps(&mut corpus, dir.path());
    wide_steps(&mut corpus, dir.path());
    crashes(&mut corpus, dir.path());
    out_of_order(&mut corpus, dir.path());
    mid_receive(&mut corpus, dir.path());
    staging_frames(&mut corpus, dir.path());
    sync_seeds(&mut corpus, dir.path());
    refs_seeds(&mut corpus);
    damage_seeds(&mut corpus);
    let generated = corpus.seen.len();
    for extra in args {
        collect(&mut corpus, Path::new(&extra));
    }
    println!(
        "{} seeds, {generated} generated, {} too large for the open corpus",
        corpus.seen.len(),
        corpus.large
    );
}
