mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use common::{Rng, cases, fast};
use ferro::db::Clock;
use ferro::{Db, Error, Keep, Map, Options, Reclaim, Snapshot};
use tempfile::TempDir;

type Root = Map<u32, u32>;

const DEVICES: usize = 3;
const NAMES: [&str; 4] = ["main", "p", "q", "r"];

fn options(cache: u64, keep: Keep) -> Options {
    fast()
        .cache_bytes(cache)
        .history(keep)
        .reclaim(Reclaim { every_bytes: 0 })
        .clock(Clock(Arc::new(|| 1_700_000_000_000)))
}

fn take_theirs(ours: &mut Root, base: &Snapshot<Root>, theirs: &Snapshot<Root>) -> (String, ()) {
    for (key, value) in theirs.iter() {
        if base.get(&key).map(|old| *old) != Some(*value) {
            ours.insert(key, *value);
        }
    }
    for (key, _) in base.iter() {
        if theirs.get(&key).is_none() {
            ours.remove(&key);
        }
    }
    ("merge".to_string(), ())
}

fn dump(root: &Root) -> BTreeMap<u32, u32> {
    root.iter().map(|(key, value)| (key, *value)).collect()
}

struct Device {
    path: PathBuf,
    db: Db<Root>,
    cache: u64,
    keep: Keep,
}

impl Device {
    fn reopen(&mut self) {
        let placeholder: Db<Root> = options(self.cache, self.keep)
            .read_only(true)
            .open(&self.path)
            .unwrap();
        drop(std::mem::replace(&mut self.db, placeholder));
        self.db = options(self.cache, self.keep).open(&self.path).unwrap();
    }

    fn pick(&self, rng: &mut Rng) -> String {
        let names = self.db.branches();
        names[rng.below(names.len() as u64) as usize].clone()
    }

    fn handle(&self, name: &str) -> Option<Db<Root>> {
        match self.db.branch(name) {
            Ok(db) => Some(db),
            Err(Error::NoBranch(_)) => None,
            Err(error) => panic!("{error}"),
        }
    }
}

type Shape = BTreeMap<String, (Option<ferro::Hash>, u64, BTreeMap<u32, u32>)>;

fn shape(db: &Db<Root>, when: &str) -> Shape {
    let mut out = Shape::new();
    for name in db.branches() {
        let branch = db.branch(&name).unwrap();
        let first = branch.log().first().map_or(1, |(version, _)| version);
        let last = branch.version();
        for version in first.max(1)..=last {
            let at = branch
                .at(version)
                .unwrap_or_else(|error| panic!("{when}: {name} at v{version}: {error}"));
            assert_eq!(at.version(), version, "{when}: {name}");
        }
        out.insert(name, (branch.head_hash(), first, dump(&branch.read())));
    }
    out
}

fn checked(devices: &[Device], when: &str) -> Vec<Shape> {
    devices
        .iter()
        .enumerate()
        .map(|(index, device)| {
            let when = format!("{when}: d{index}");
            device
                .db
                .verify()
                .unwrap_or_else(|error| panic!("{when}: {error}"));
            device
                .db
                .verify_typed()
                .unwrap_or_else(|error| panic!("{when}: {error}"));
            shape(&device.db, &when)
        })
        .collect()
}

fn run(seed: u64, steps: usize, tally: &mut BTreeMap<&'static str, usize>) {
    let mut rng = Rng::new(seed);
    let dir = TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut devices: Vec<Device> = (0..DEVICES)
        .map(|index| {
            let path = dir.path().join(format!("d{index}.ferro"));
            let cache = [1 << 20, 4 << 10, 1][index];
            let keep = [Keep::All, Keep::Last(2), Keep::Last(3)][index];
            let db = options(cache, keep).open(&path).unwrap();
            Device {
                path,
                db,
                cache,
                keep,
            }
        })
        .collect();
    let mut seen = checked(&devices, &format!("seed {seed} start"));
    for step in 0..steps {
        let when = format!("seed {seed} step {step}");
        let to = rng.below(DEVICES as u64) as usize;
        let name = devices[to].pick(&mut rng);
        let fresh = NAMES[1 + rng.below(NAMES.len() as u64 - 1) as usize];
        let mut reshaped = true;
        match rng.below(100) {
            0..40 => {
                if let Some(db) = devices[to].handle(&name) {
                    let (key, value) = (rng.below(3) as u32, rng.below(2) as u32);
                    db.commit("set", |root| {
                        root.insert(key, value);
                    })
                    .unwrap_or_else(|error| panic!("{when}: {error}"));
                    *tally.entry("commit").or_default() += 1;
                }
            }
            40..50 => {
                if let Some(db) = devices[to].handle(&name) {
                    match db.fork(fresh) {
                        Ok(_) => *tally.entry("fork").or_default() += 1,
                        Err(Error::BranchExists(_)) => {}
                        Err(error) => panic!("{when}: {error}"),
                    }
                }
            }
            50..56 => {
                if name != "main" {
                    match devices[to].db.drop_branch(&name) {
                        Ok(()) => *tally.entry("drop").or_default() += 1,
                        Err(Error::NoBranch(_)) => {}
                        Err(error) => panic!("{when}: {error}"),
                    }
                }
            }
            56..85 => {
                let from = (to + 1 + rng.below(DEVICES as u64 - 1) as usize) % DEVICES;
                let source = devices[from].pick(&mut rng);
                let remote = format!("d{from}-{}", source.replace('-', "_"));
                if remote == name {
                    continue;
                }
                let (Some(into), Some(out)) =
                    (devices[to].handle(&name), devices[from].handle(&source))
                else {
                    continue;
                };
                let pulling = rng.chance(50);
                let outcome = if pulling {
                    into.pull(&out, &remote, take_theirs)
                } else {
                    let have = into.have().unwrap();
                    let bundle = out.bundle(&have).unwrap();
                    into.receive(&remote, &bundle, take_theirs)
                };
                match outcome {
                    Ok(_) => *tally.entry("pulled").or_default() += 1,
                    Err(Error::NoCommonAncestor) => *tally.entry("refused").or_default() += 1,
                    Err(Error::StaleHave(_)) if !pulling => {
                        *tally.entry("refused").or_default() += 1;
                    }
                    Err(error) => panic!("{when}: d{to} {name} <- d{from} {source}: {error}"),
                }
            }
            85..93 => {
                let keep = match rng.below(3) {
                    0 => Keep::All,
                    1 => Keep::Last(1),
                    _ => Keep::Last(2),
                };
                devices[to]
                    .db
                    .compact(keep)
                    .unwrap_or_else(|error| panic!("{when}: compact {keep:?}: {error}"));
                *tally.entry("compact").or_default() += 1;
            }
            _ => {
                devices[to].reopen();
                let reopened = checked(&devices, &when);
                assert_eq!(reopened[to], seen[to], "{when}: d{to} reopened differently");
                *tally.entry("reopen").or_default() += 1;
                reshaped = false;
            }
        }
        if reshaped {
            seen = checked(&devices, &when);
        }
    }
}

#[test]
fn identical_commits_on_relayed_and_pruned_branches_keep_every_branch_whole() {
    let mut tally = BTreeMap::new();
    for seed in 0..u64::from(cases(120)) {
        run(seed, 80, &mut tally);
    }
    println!("{tally:?}");
    for wanted in [
        "commit", "fork", "drop", "pulled", "refused", "compact", "reopen",
    ] {
        assert!(tally.get(wanted).is_some_and(|n| *n > 10), "{tally:?}");
    }
}
