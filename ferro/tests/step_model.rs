mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::{Rng, cases};
use ferro::{Db, Durability, Error, Hash, Keep, Map, Options, Outcome, Reclaim, Snapshot};
use tempfile::TempDir;

type Root = Map<u64, u64>;
type Heads = BTreeMap<String, (u64, Option<Hash>, Vec<(u64, u64)>)>;

fn options(keep: Keep, cache: u64) -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .history(keep)
        .reclaim(Reclaim { every_bytes: 0 })
        .cache_bytes(cache)
}

fn heads(db: &Db<Root>) -> Heads {
    db.branches()
        .into_iter()
        .map(|name| {
            let branch = db.branch(&name).unwrap();
            let snapshot = branch.read();
            let values = snapshot.iter().map(|(key, value)| (key, *value)).collect();
            (name, (snapshot.version(), branch.head_hash(), values))
        })
        .collect()
}

fn merge(root: &mut Root, base: &Snapshot<Root>, theirs: &Snapshot<Root>) -> (&'static str, ()) {
    root.merge(base, theirs, |conflict| conflict.theirs.map(|value| *value));
    ("merge", ())
}

struct World {
    dir: TempDir,
    keep: Keep,
    cache: u64,
    db: Option<Db<Root>>,
    branches: Vec<(String, Db<Root>)>,
    peer: Db<Root>,
    eager: Db<Root>,
    lazy: Db<Root>,
    forks: u64,
    rng: Rng,
    seed: u64,
    counts: BTreeMap<&'static str, usize>,
}

impl World {
    fn path(&self) -> PathBuf {
        self.dir.path().join("steps.ferro")
    }

    fn new(seed: u64, keep: Keep, cache: u64) -> World {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let path = dir.path().join("steps.ferro");
        let db: Db<Root> = options(keep, cache).open(&path).unwrap();
        db.update(|root| {
            root.insert(0, 0);
        })
        .unwrap();
        let peer: Db<Root> = options(keep, cache)
            .open(dir.path().join("peer.ferro"))
            .unwrap();
        peer.pull(&db, "origin", merge).unwrap();
        peer.update(|root| {
            root.insert(1000, 0);
        })
        .unwrap();
        let eager = options(keep, cache).read_only(true).open(&path).unwrap();
        let lazy = options(keep, cache).read_only(true).open(&path).unwrap();
        World {
            branches: vec![("main".to_string(), db.clone())],
            db: Some(db),
            dir,
            keep,
            cache,
            peer,
            eager,
            lazy,
            forks: 0,
            rng: Rng::new(seed),
            seed,
            counts: BTreeMap::new(),
        }
    }

    fn db(&self) -> &Db<Root> {
        self.db.as_ref().unwrap()
    }

    fn count(&mut self, what: &'static str) {
        *self.counts.entry(what).or_default() += 1;
    }

    fn commit(&mut self, round: u64) {
        let pick = self.rng.below(self.branches.len() as u64) as usize;
        let key = self.rng.below(24);
        let branch = &self.branches[pick].1;
        branch
            .update(|root| {
                root.insert(key, round);
            })
            .unwrap();
        self.count("commit");
    }

    fn fork(&mut self) {
        let pick = self.rng.below(self.branches.len() as u64) as usize;
        self.forks += 1;
        let name = format!("f{}", self.forks);
        let handle = self.branches[pick].1.fork(&name).unwrap();
        self.branches.push((name, handle));
        self.count("fork");
    }

    fn drop_one(&mut self) {
        let candidates: Vec<usize> = (0..self.branches.len())
            .filter(|at| self.branches[*at].0.starts_with('f'))
            .collect();
        if candidates.is_empty() {
            return self.fork();
        }
        let at = candidates[self.rng.below(candidates.len() as u64) as usize];
        let (name, _) = self.branches.remove(at);
        self.db().drop_branch(&name).unwrap();
        self.count("drop");
    }

    fn pull(&mut self, round: u64) {
        for _ in 0..1 + self.rng.below(3) {
            let key = 1000 + self.rng.below(8);
            self.peer
                .update(|root| {
                    root.insert(key, round);
                })
                .unwrap();
        }
        let local: Vec<usize> = (0..self.branches.len())
            .filter(|at| self.branches[*at].0 != "peer")
            .collect();
        let pick = local[self.rng.below(local.len() as u64) as usize];
        let into = &self.branches[pick].1;
        let pulled = into.pull(&self.peer, "peer", merge);
        if !self.branches.iter().any(|(name, _)| name == "peer")
            && let Ok(tracking) = self.db().branch("peer")
        {
            self.branches.push(("peer".to_string(), tracking));
        }
        self.count(match pulled.map(|pulled| pulled.outcome) {
            Ok(Outcome::Merged) => "pull merged",
            Ok(Outcome::FastForward) => "pull fast-forward",
            Ok(_) => "pull other",
            Err(Error::NoCommonAncestor) => "pull refused",
            Err(error) => panic!("seed {}: pull: {error}", self.seed),
        });
    }

    fn push_back(&mut self) {
        let pick = self.rng.below(self.branches.len() as u64) as usize;
        let name = self.branches[pick].0.clone();
        let remote = format!("from-{name}");
        let refused = |result: ferro::Result<_>| match result {
            Ok(_) => false,
            Err(Error::NoCommonAncestor) => true,
            Err(error) => panic!("relay: {error}"),
        };
        if refused(self.peer.pull(&self.branches[pick].1, &remote, merge)) {
            return self.count("relay refused");
        }
        let into = self.branches[pick].1.clone();
        if name == "peer" || refused(into.pull(&self.peer, "peer", merge)) {
            return self.count("relay refused");
        }
        for (name, _) in self.db().branches().into_iter().map(|name| (name, ())) {
            if !self.branches.iter().any(|(held, _)| *held == name) {
                let handle = self.db().branch(&name).unwrap();
                self.branches.push((name, handle));
            }
        }
        self.count("relay");
    }

    fn compact(&mut self) {
        let keep = match self.keep {
            Keep::All => Keep::All,
            _ => Keep::Last(1 + self.rng.below(4) as usize),
        };
        self.db().compact(keep).unwrap();
        self.count("cycle");
    }

    fn reopen(&mut self) {
        let names: Vec<String> = self.branches.iter().map(|(name, _)| name.clone()).collect();
        let written = heads(self.db());
        self.branches.clear();
        self.db = None;
        let db: Db<Root> = options(self.keep, self.cache).open(self.path()).unwrap();
        assert_eq!(heads(&db), written, "seed {}: reopen", self.seed);
        self.branches = names
            .into_iter()
            .map(|name| {
                let handle = db.branch(&name).unwrap();
                (name, handle)
            })
            .collect();
        self.db = Some(db);
        self.count("reopen");
    }

    fn check(&mut self, round: u64, deep: bool) {
        let context = format!("seed {} round {round}", self.seed);
        let written = heads(self.db());
        self.eager.refresh().unwrap();
        assert_eq!(heads(&self.eager), written, "{context}: eager follower");
        if !deep {
            return;
        }
        self.lazy.refresh().unwrap();
        assert_eq!(heads(&self.lazy), written, "{context}: lazy follower");
        let fresh: Db<Root> = options(self.keep, 1)
            .read_only(true)
            .open(self.path())
            .unwrap();
        assert_eq!(heads(&fresh), written, "{context}: fresh reader");
        fresh
            .verify()
            .unwrap_or_else(|error| panic!("{context}: reader verify: {error}"));
        drop(fresh);
        self.db()
            .verify()
            .unwrap_or_else(|error| panic!("{context}: verify: {error}"));
        self.db()
            .verify_typed()
            .unwrap_or_else(|error| panic!("{context}: verify_typed: {error}"));
        let info = ferro::admin::info(self.path()).unwrap();
        let versions: BTreeMap<String, u64> = info
            .branches
            .iter()
            .map(|branch| (branch.name.clone(), branch.version))
            .collect();
        let expected: BTreeMap<String, u64> = written
            .iter()
            .filter(|(name, (version, _, _))| *name != "main" || *version > 0)
            .map(|(name, (version, _, _))| (name.clone(), *version))
            .collect();
        assert_eq!(versions, expected, "{context}: admin info");
        ferro::admin::verify(self.path())
            .unwrap_or_else(|error| panic!("{context}: admin verify: {error}"));
        for (name, (_, hash, _)) in &written {
            let Some(hash) = hash else { continue };
            let log = ferro::admin::log(self.path(), name, 1).unwrap();
            let branch = self.db().branch(name).unwrap();
            assert_eq!(
                log.first().map(|commit| commit.version),
                Some(branch.version()),
                "{context}: admin log of {name}"
            );
            assert_eq!(branch.log().hash_at(&branch.version()), Some(*hash));
        }
        self.count("deep check");
    }
}

fn offline(path: &Path, keep: Keep, cache: u64, written: &Heads, context: &str) {
    ferro::admin::compact(path, Keep::All).unwrap();
    let db: Db<Root> = options(keep, cache).open(path).unwrap();
    assert_eq!(&heads(&db), written, "{context}: after offline compaction");
    db.verify().unwrap();
}

fn run(seed: u64, keep: Keep, cache: u64) -> BTreeMap<&'static str, usize> {
    let mut world = World::new(seed, keep, cache);
    let mut round = 0;
    let bursts = 30;
    for _ in 0..bursts {
        let length = match world.rng.below(4) {
            0 => 60 + world.rng.below(20),
            1 => 120 + world.rng.below(20),
            _ => world.rng.below(30),
        };
        for _ in 0..length {
            round += 1;
            world.commit(round);
            if world.rng.chance(15) {
                world.check(round, false);
            }
        }
        for _ in 0..1 + world.rng.below(3) {
            round += 1;
            match world.rng.below(100) {
                0..25 => world.fork(),
                25..40 => world.drop_one(),
                40..60 => world.pull(round),
                60..70 => world.push_back(),
                70..82 => world.compact(),
                82..92 => world.reopen(),
                _ => world.commit(round),
            }
            let deep = world.rng.chance(50);
            world.check(round, deep);
        }
        world.check(round, true);
    }
    let written = heads(world.db());
    let path = world.path();
    let context = format!("seed {seed}");
    world.branches.clear();
    world.db = None;
    let db: Db<Root> = options(keep, cache).open(&path).unwrap();
    assert_eq!(heads(&db), written, "{context}: final reopen");
    drop(db);
    offline(&path, keep, cache, &written, &context);
    world.counts
}

#[test]
fn heads_agree_across_steps_tables_followers_and_tools() {
    let mut totals: BTreeMap<&'static str, usize> = BTreeMap::new();
    for seed in 0..u64::from(cases(9)) {
        let (keep, cache) = match seed % 3 {
            0 => (Keep::All, 1 << 20),
            1 => (Keep::Last(40), 16 << 10),
            _ => (Keep::Last(2), 1),
        };
        for (what, count) in run(seed, keep, cache) {
            *totals.entry(what).or_default() += count;
        }
    }
    for what in ["commit", "fork", "deep check"] {
        assert!(totals.get(what).copied().unwrap_or(0) > 0, "{totals:?}");
    }
    eprintln!("{totals:?}");
}
