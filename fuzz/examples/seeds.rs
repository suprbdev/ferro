use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use ferro::{Bytes, Data, Db, Durability, Keep, Map, Options};
use ferro_fuzz::types::{Docs, Nested, Order, Root, Shop, Status, Summed};

const ROOT: u8 = 0;
const COUNTERS: u8 = 1;
const SHOP: u8 = 2;
const DOCS: u8 = 3;
const NESTED: u8 = 4;
const SUMMED: u8 = 5;
const OPAQUE: u8 = 6;
const LARGEST: u64 = 256 << 10;

struct Corpus {
    out: PathBuf,
    seen: HashSet<blake3::Hash>,
}

impl Corpus {
    fn emit(&mut self, kind: u8, bytes: &[u8]) {
        if !self.seen.insert(blake3::hash(&[&[kind], bytes].concat())) {
            return;
        }
        for target in ["open", "admin"] {
            let dir = self.out.join(target);
            fs::create_dir_all(&dir).unwrap();
            for mode in [kind << 2, kind << 2 | 2] {
                let mut data = vec![mode];
                data.extend_from_slice(bytes);
                let name = blake3::hash(&data).to_hex();
                fs::write(dir.join(&name[..16]), data).unwrap();
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
}

fn options() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20)
}

fn text(seed: u64, len: usize) -> String {
    (0..len)
        .map(|at| char::from(b'a' + ((seed.wrapping_mul(31).wrapping_add(at as u64 * 7)) % 26) as u8))
        .collect()
}

fn scenario<T: Data + Default>(
    corpus: &mut Corpus,
    dir: &Path,
    name: &str,
    kind: u8,
    steps: impl FnOnce(&Db<T>) -> ferro::Result<()>,
) {
    let path = dir.join(format!("{name}.ferro"));
    let db = options().open::<T>(&path).unwrap();
    steps(&db).unwrap();
    drop(db);
    corpus.emit_file(kind, &path);
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
        db.restore(&db.at(1)?)?;
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
                    customer: if id % 2 == 0 { "wile".into() } else { "road".into() },
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

fn collect(corpus: &mut Corpus, path: &Path) {
    if path.is_dir() {
        for entry in fs::read_dir(path).unwrap().flatten() {
            collect(corpus, &entry.path());
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
    if bytes.starts_with(b"FERRODB3") {
        corpus.emit(OPAQUE, &bytes);
        corpus.emit(ROOT, &bytes);
    }
}

fn main() {
    let mut args = env::args_os().skip(1);
    let out = PathBuf::from(args.next().expect("usage: seeds <corpus dir> [captured files or dirs]"));
    let mut corpus = Corpus {
        out,
        seen: HashSet::new(),
    };
    let dir = tempfile::tempdir().unwrap();
    generated(&mut corpus, dir.path());
    for extra in args {
        collect(&mut corpus, Path::new(&extra));
    }
    println!("{} seeds", corpus.seen.len());
}
