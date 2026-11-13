#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use common::{Rng, cases, fast};
use ferro::db::Clock;
use ferro::{Bundle, Db, Error, Have, Keep, Map, Outcome, Reclaim, Snapshot};
use tempfile::TempDir;

type Counts = Map<u32, i64>;

fn add(root: &mut Counts, key: u32, delta: i64) {
    let next = root.get(&key).map_or(0, |v| *v) + delta;
    if next == 0 {
        root.remove(&key);
    } else {
        root.insert(key, next);
    }
}

fn add_deltas(root: &mut Counts, base: &Snapshot<Counts>, theirs: &Snapshot<Counts>) {
    let mut keys: Vec<u32> = base.keys().chain(theirs.keys()).collect();
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        let delta = theirs.get(&key).map_or(0, |v| *v) - base.get(&key).map_or(0, |v| *v);
        if delta != 0 {
            add(root, key, delta);
        }
    }
}

fn merge(
    root: &mut Counts,
    base: &Snapshot<Counts>,
    theirs: &Snapshot<Counts>,
) -> (&'static str, ()) {
    add_deltas(root, base, theirs);
    ("merge", ())
}

fn values(snapshot: &Snapshot<Counts>) -> BTreeMap<u32, i64> {
    snapshot.iter().map(|(k, v)| (k, *v)).collect()
}

fn id_of(snapshot: &Snapshot<Counts>) -> usize {
    snapshot
        .commit()
        .message
        .strip_prefix('n')
        .map_or(0, |id| id.parse().unwrap())
}

struct Node {
    delta: BTreeMap<u32, i64>,
    ancestors: Vec<bool>,
}

struct Model(Vec<Node>);

impl Model {
    fn new() -> Self {
        Model(vec![Node {
            delta: BTreeMap::new(),
            ancestors: vec![true],
        }])
    }

    fn push(&mut self, parents: &[usize], delta: BTreeMap<u32, i64>) -> usize {
        let id = self.0.len();
        let mut ancestors = vec![false; id + 1];
        for parent in parents {
            for (at, held) in self.0[*parent].ancestors.iter().enumerate() {
                ancestors[at] |= *held;
            }
        }
        ancestors[id] = true;
        self.0.push(Node { delta, ancestors });
        id
    }

    fn holds(&self, node: usize, ancestor: usize) -> bool {
        self.0[node]
            .ancestors
            .get(ancestor)
            .copied()
            .unwrap_or(false)
    }

    fn value(&self, node: usize) -> BTreeMap<u32, i64> {
        let mut out: BTreeMap<u32, i64> = BTreeMap::new();
        for (at, held) in self.0[node].ancestors.iter().enumerate() {
            if *held {
                for (key, delta) in &self.0[at].delta {
                    *out.entry(*key).or_default() += delta;
                }
            }
        }
        out.retain(|_, v| *v != 0);
        out
    }

    fn differs_outside(&self, ours: usize, theirs: usize) -> Vec<usize> {
        let value = self.value(ours);
        (0..self.0.len())
            .filter(|at| self.holds(theirs, *at) && !self.holds(ours, *at))
            .filter(|at| self.value(*at) != value)
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
struct Setup {
    keep: Keep,
    shared: bool,
}

fn open(path: &Path, setup: Setup, now: &Arc<AtomicU64>) -> Db<Counts> {
    let now = now.clone();
    let options = fast()
        .clock(Clock(Arc::new(move || now.fetch_add(1, Ordering::SeqCst))))
        .history(setup.keep)
        .reclaim(Reclaim::off())
        .shared(setup.shared);
    options.open(path).unwrap()
}

struct Device {
    path: PathBuf,
    setup: Setup,
    handles: Vec<Db<Counts>>,
    head: usize,
    own: Vec<(u32, i64)>,
    now: Arc<AtomicU64>,
}

impl Device {
    fn new(path: PathBuf, setup: Setup, now: Arc<AtomicU64>) -> Self {
        let mut device = Device {
            path,
            setup,
            handles: Vec::new(),
            head: 0,
            own: Vec::new(),
            now,
        };
        device.reopen();
        device
    }

    fn reopen(&mut self) {
        self.handles.clear();
        let count = if self.setup.shared { 2 } else { 1 };
        self.handles = (0..count)
            .map(|_| open(&self.path, self.setup, &self.now))
            .collect();
    }

    fn pruned(&self) -> bool {
        !matches!(self.setup.keep, Keep::All)
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Edit(usize, usize, u32, i64),
    Revert(usize, usize, usize),
    Identical(usize, usize, u32, i64),
    Pull(usize, usize, usize, usize),
    Sync(usize, usize, usize, usize),
    Cycle(usize, usize, usize),
    Reopen(usize),
    Drop(usize, usize, usize, bool),
    Local(usize, usize, u32, u32),
    Ask(usize, usize, usize),
    Deliver(usize, usize, usize),
    Redeliver(usize, usize),
    Side(usize, usize, usize, usize),
}

#[derive(Clone, Copy, Debug)]
struct Plan {
    pruned: u64,
    shared: u64,
    harder: bool,
    steps: usize,
}

const KEEP_ALL: Plan = Plan {
    pruned: 0,
    shared: 40,
    harder: false,
    steps: 120,
};

const MIXED: Plan = Plan {
    pruned: 50,
    shared: 40,
    harder: false,
    steps: 120,
};

const PRUNED_HARDER: Plan = Plan {
    pruned: 100,
    shared: 30,
    harder: true,
    steps: 120,
};

fn case(seed: u64, plan: Plan) -> (Vec<Setup>, Vec<Op>) {
    let mut rng = Rng::new(seed ^ 0x5eed);
    let count = 3 + rng.below(3) as usize;
    let setups = (0..count)
        .map(|_| Setup {
            keep: match rng.chance(plan.pruned) {
                true => Keep::Last(1 + rng.below(4) as usize),
                false => Keep::All,
            },
            shared: rng.chance(plan.shared),
        })
        .collect();
    let weights = [30, 30, 10, 7, 3, 4, 2, 3, 4, 6, 6, 3, 6];
    let total: u64 = weights.iter().sum();
    let ops = (0..plan.steps)
        .map(|_| {
            let to = rng.below(count as u64) as usize;
            let from = (to + 1 + rng.below(count as u64 - 1) as usize) % count;
            let (h1, h2) = (rng.below(2) as usize, rng.below(2) as usize);
            let key = rng.below(6) as u32;
            let delta = if rng.chance(70) { 1 } else { -1 };
            let mut roll = rng.below(total);
            let mut kind = 0;
            while roll >= weights[kind] {
                roll -= weights[kind];
                kind += 1;
            }
            let pick = rng.below(8) as usize;
            match kind {
                0 if rng.chance(35) => Op::Revert(to, h1, pick),
                0 => Op::Edit(to, h1, key, delta),
                1 => Op::Pull(to, from, h1, h2),
                2 => Op::Sync(to, from, h1, h2),
                3 => Op::Cycle(to, h1, if plan.harder { 1 + pick % 4 } else { 0 }),
                4 => Op::Reopen(to),
                5 => Op::Drop(to, h1, pick, false),
                6 => Op::Drop(to, h1, pick, true),
                7 => Op::Local(to, h1, key, pick as u32 % 6),
                8 => Op::Identical(to, from, key, delta),
                9 => Op::Ask(to, from, h1),
                10 => Op::Deliver(pick, h1, h2),
                11 => Op::Redeliver(pick, h1),
                _ => Op::Side(to, from, h1, h2),
            }
        })
        .collect();
    (setups, ops)
}

struct Mesh {
    _dir: TempDir,
    devices: Vec<Device>,
    model: Model,
    trace: Vec<String>,
    strict: bool,
    merges: usize,
    refused: usize,
    asked: Vec<(usize, usize, Have)>,
    sent: Vec<(usize, usize, Bundle, bool)>,
}

impl Mesh {
    fn new(setups: &[Setup]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000_000));
        let devices: Vec<Device> = setups
            .iter()
            .enumerate()
            .map(|(at, setup)| {
                Device::new(dir.path().join(format!("d{at}.ferro")), *setup, now.clone())
            })
            .collect();
        let strict = devices.iter().all(|device| !device.pruned());
        Mesh {
            _dir: dir,
            devices,
            model: Model::new(),
            trace: Vec::new(),
            strict,
            merges: 0,
            refused: 0,
            asked: Vec::new(),
            sent: Vec::new(),
        }
    }

    fn fail(&self, what: String) -> String {
        let from = self.trace.len().saturating_sub(40);
        format!("{what}\ntrace:\n{}", self.trace[from..].join("\n"))
    }

    fn at(&self, device: usize, handle: usize) -> usize {
        handle % self.devices[device].handles.len()
    }

    fn edit(&mut self, device: usize, handle: usize, key: u32, delta: i64) -> Result<(), String> {
        let handle = self.at(device, handle);
        let parent = self.devices[device].head;
        let id = self.model.push(&[parent], BTreeMap::from([(key, delta)]));
        self.trace.push(format!(
            "d{device}/h{handle} adds {delta:+} to k{key} as n{id}"
        ));
        self.devices[device].handles[handle]
            .commit(format!("n{id}"), |root| add(root, key, delta))
            .map_err(|error| self.fail(format!("commit on d{device}: {error}")))?;
        self.devices[device].head = id;
        self.devices[device].own.push((key, delta));
        self.check_head(device, handle)
    }

    fn check_head(&self, device: usize, handle: usize) -> Result<(), String> {
        let snapshot = self.devices[device].handles[handle].read();
        let head = self.devices[device].head;
        if id_of(&snapshot) != head {
            return Err(self.fail(format!(
                "d{device} is at n{} where the model has n{head}",
                id_of(&snapshot)
            )));
        }
        let (got, want) = (values(&snapshot), self.model.value(head));
        if got != want {
            return Err(self.fail(format!(
                "d{device} at n{head} holds {got:?}, the model {want:?}"
            )));
        }
        Ok(())
    }

    fn verify(&self) -> Result<(), String> {
        for (at, device) in self.devices.iter().enumerate() {
            for db in &device.handles {
                db.verify()
                    .map_err(|error| self.fail(format!("verify d{at}: {error}")))?;
                db.verify_typed()
                    .map_err(|error| self.fail(format!("verify_typed d{at}: {error}")))?;
            }
        }
        Ok(())
    }

    fn pull(
        &mut self,
        to: usize,
        from: usize,
        th: usize,
        fh: usize,
    ) -> Result<Option<Outcome>, String> {
        let empty = self.devices[from].handles[self.at(from, fh)]
            .head_hash()
            .is_none();
        self.bring(to, from, th, fh, None, empty)
    }

    fn bring(
        &mut self,
        to: usize,
        from: usize,
        th: usize,
        fh: usize,
        bundle: Option<&Bundle>,
        empty: bool,
    ) -> Result<Option<Outcome>, String> {
        let (th, fh) = (self.at(to, th), self.at(from, fh));
        let ours = self.devices[to].head;
        let id = self.model.0.len();
        let remote = format!("d{from}");
        let into = &self.devices[to].handles[th];
        into.refresh().ok();
        let before = (into.head_hash(), values(&into.read()));
        let merge = |root: &mut Counts, base: &Snapshot<Counts>, theirs: &Snapshot<Counts>| {
            add_deltas(root, base, theirs);
            (format!("n{id}"), ())
        };
        let pulled = match bundle {
            Some(bundle) => into.receive(&remote, bundle, merge),
            None => into.pull(&self.devices[from].handles[fh], &remote, merge),
        };
        let after = (into.head_hash(), values(&into.read()));
        let outcome = match pulled {
            Ok(pulled) => pulled.outcome,
            Err(error) => {
                self.trace
                    .push(format!("d{to}/h{th} <- d{from}/h{fh}: {error}"));
                let allowed = match error {
                    Error::StaleHave(_) => bundle.is_some(),
                    Error::NoCommonAncestor => !self.strict,
                    _ => false,
                };
                if !allowed {
                    return Err(self.fail(format!("d{to} <- d{from}: {error}")));
                }
                if after != before {
                    return Err(self.fail(format!("a refused pull d{to} <- d{from} changed d{to}")));
                }
                self.refused += 1;
                return Ok(None);
            }
        };
        let into = &self.devices[to].handles[th];
        let theirs = match into.branch(&remote) {
            _ if empty && outcome == Outcome::UpToDate => ours,
            Ok(landed) => id_of(&landed.read()),
            Err(Error::NoBranch(_)) => 0,
            Err(error) => return Err(self.fail(format!("branch {remote} on d{to}: {error}"))),
        };
        self.trace.push(format!(
            "d{to}/h{th} <- d{from}/h{fh}: n{ours} with n{theirs}: {outcome:?}"
        ));
        let model = &self.model;
        let wrong = match outcome {
            Outcome::FastForward => !model.holds(theirs, ours),
            Outcome::Ahead => !model.holds(ours, theirs),
            Outcome::UpToDate => {
                ours != theirs
                    && !model.holds(ours, theirs)
                    && (model.value(ours) != model.value(theirs)
                        || self.strict && !model.differs_outside(ours, theirs).is_empty())
            }
            Outcome::Merged => {
                model.holds(ours, theirs)
                    || self.strict
                        && model.value(ours) == model.value(theirs)
                        && model.differs_outside(ours, theirs).is_empty()
            }
        };
        if wrong {
            return Err(self.fail(format!("d{to} at n{ours} was {outcome:?} with n{theirs}")));
        }
        match outcome {
            Outcome::FastForward => self.devices[to].head = theirs,
            Outcome::Merged => {
                self.devices[to].head = self.model.push(&[ours, theirs], BTreeMap::new());
                self.merges += 1;
            }
            Outcome::Ahead | Outcome::UpToDate => {}
        }
        self.check_head(to, th)?;
        Ok(Some(outcome))
    }

    fn drop_remote(
        &mut self,
        device: usize,
        handle: usize,
        pick: usize,
        fork: bool,
    ) -> Result<(), String> {
        let handle = self.at(device, handle);
        let db = &self.devices[device].handles[handle];
        db.refresh().ok();
        let remotes: Vec<String> = db
            .branches()
            .into_iter()
            .filter(|name| name.starts_with('d'))
            .collect();
        if remotes.is_empty() {
            return Ok(());
        }
        let name = &remotes[pick % remotes.len()];
        self.trace
            .push(format!("d{device}/h{handle} drops {name}, fork {fork}"));
        db.drop_branch(name)
            .map_err(|error| self.fail(format!("drop {name} on d{device}: {error}")))?;
        if fork {
            db.fork(name)
                .map(drop)
                .map_err(|error| self.fail(format!("fork {name} on d{device}: {error}")))?;
        }
        Ok(())
    }

    fn local_merge(
        &mut self,
        device: usize,
        handle: usize,
        k1: u32,
        k2: u32,
    ) -> Result<(), String> {
        let handle = self.at(device, handle);
        let base = self.devices[device].head;
        let side = self.model.push(&[base], BTreeMap::from([(k1, 1)]));
        let main = self.model.push(&[base], BTreeMap::from([(k2, 1)]));
        let merged = self.model.0.len();
        self.trace.push(format!(
            "d{device}/h{handle} forks n{side}, commits n{main}, merges n{merged}"
        ));
        let db = &self.devices[device].handles[handle];
        let work = db
            .fork("work")
            .map_err(|error| self.fail(format!("fork on d{device}: {error}")))?;
        work.commit(format!("n{side}"), |root| add(root, k1, 1))
            .map_err(|error| self.fail(format!("commit on work of d{device}: {error}")))?;
        db.commit(format!("n{main}"), |root| add(root, k2, 1))
            .map_err(|error| self.fail(format!("commit on d{device}: {error}")))?;
        let result = db.merge_with(&work, |root, base, theirs| {
            add_deltas(root, base, theirs);
            (format!("n{merged}"), ())
        });
        drop(work);
        db.drop_branch("work")
            .map_err(|error| self.fail(format!("drop work on d{device}: {error}")))?;
        match result {
            Ok(_) => self.devices[device].head = self.model.push(&[main, side], BTreeMap::new()),
            Err(Error::NoCommonAncestor) if self.devices[device].pruned() => {
                self.model.push(&[], BTreeMap::new());
                self.devices[device].head = main;
            }
            Err(error) => return Err(self.fail(format!("merge work on d{device}: {error}"))),
        }
        self.check_head(device, handle)
    }

    fn side_pull(&mut self, to: usize, from: usize, th: usize, fh: usize) -> Result<(), String> {
        let (th, fh) = (self.at(to, th), self.at(from, fh));
        let base = self.devices[to].head;
        let remote = format!("d{from}");
        let id = self.model.0.len();
        let side = self.devices[to].handles[th]
            .fork("side")
            .map_err(|error| self.fail(format!("fork side on d{to}: {error}")))?;
        let pulled = side.pull(
            &self.devices[from].handles[fh],
            &remote,
            |root, b, theirs| {
                add_deltas(root, b, theirs);
                (format!("n{id}"), ())
            },
        );
        let mut head = base;
        match pulled {
            Ok(pulled) => {
                let theirs = side
                    .branch(&remote)
                    .map(|landed| id_of(&landed.read()))
                    .unwrap_or(0);
                self.trace.push(format!(
                    "d{to}/h{th} side <- d{from}/h{fh}: n{base} with n{theirs}: {:?}",
                    pulled.outcome
                ));
                match pulled.outcome {
                    Outcome::FastForward if self.model.holds(theirs, base) => head = theirs,
                    Outcome::FastForward => {
                        return Err(self.fail(format!("side of d{to} fast-forwarded to n{theirs}")));
                    }
                    Outcome::Merged => {
                        head = self.model.push(&[base, theirs], BTreeMap::new());
                        self.merges += 1;
                    }
                    Outcome::Ahead | Outcome::UpToDate => {}
                }
                if id_of(&side.read()) != head || values(&side.read()) != self.model.value(head) {
                    return Err(self.fail(format!("side of d{to} is not at n{head}")));
                }
            }
            Err(Error::NoCommonAncestor) if !self.strict => {}
            Err(error) => return Err(self.fail(format!("side of d{to} <- d{from}: {error}"))),
        }
        if head != base {
            let made = self.model.0.len();
            let merged = self.devices[to].handles[th].merge_with(&side, |root, b, theirs| {
                add_deltas(root, b, theirs);
                (format!("n{made}"), ())
            });
            match merged {
                Ok(_) => self.devices[to].head = self.model.push(&[base, head], BTreeMap::new()),
                Err(Error::NoCommonAncestor) if !self.strict => {}
                Err(error) => return Err(self.fail(format!("merge side on d{to}: {error}"))),
            }
        }
        drop(side);
        self.devices[to].handles[th]
            .drop_branch("side")
            .map_err(|error| self.fail(format!("drop side on d{to}: {error}")))?;
        self.check_head(to, th)
    }

    fn apply(&mut self, op: Op) -> Result<(), String> {
        match op {
            Op::Edit(d, h, key, delta) => self.edit(d, h, key, delta),
            Op::Revert(d, h, back) => {
                let own = &self.devices[d].own;
                if own.is_empty() {
                    return Ok(());
                }
                let (key, delta) = own[own.len() - 1 - back % own.len().min(4)];
                self.edit(d, h, key, -delta)
            }
            Op::Identical(a, b, key, delta) => {
                self.edit(a, 0, key, delta)?;
                self.edit(b, 0, key, delta)
            }
            Op::Pull(to, from, th, fh) => self.pull(to, from, th, fh).map(drop),
            Op::Sync(a, b, ha, hb) => {
                if self.pull(a, b, ha, hb)?.is_some()
                    && self.pull(b, a, hb, ha)? == Some(Outcome::Merged)
                {
                    self.pull(a, b, ha, hb)?;
                }
                Ok(())
            }
            Op::Cycle(d, h, harder) => {
                let h = self.at(d, h);
                let keep = match self.devices[d].setup.keep {
                    Keep::Last(n) if harder > 0 => Keep::Last(harder.min(n)),
                    keep => keep,
                };
                self.trace.push(format!("d{d}/h{h} compacts to {keep:?}"));
                self.devices[d].handles[h]
                    .compact(keep)
                    .map(drop)
                    .map_err(|error| self.fail(format!("compact d{d}: {error}")))
            }
            Op::Reopen(d) => {
                self.trace.push(format!("d{d} reopens"));
                self.devices[d].reopen();
                Ok(())
            }
            Op::Drop(d, h, pick, fork) => self.drop_remote(d, h, pick, fork),
            Op::Local(d, h, k1, k2) => self.local_merge(d, h, k1, k2),
            Op::Ask(to, from, h) => {
                let h = self.at(to, h);
                self.trace
                    .push(format!("d{to}/h{h} sends have a{}", self.asked.len()));
                let have = self.devices[to].handles[h]
                    .have()
                    .map_err(|error| self.fail(format!("have on d{to}: {error}")))?;
                self.asked.push((to, from, have));
                Ok(())
            }
            Op::Deliver(pick, fh, th) => {
                if self.asked.is_empty() {
                    return Ok(());
                }
                let (to, from, have) = self.asked.remove(pick % self.asked.len());
                let fh = self.at(from, fh);
                let empty = self.devices[from].handles[fh].head_hash().is_none();
                let bundle = self.devices[from].handles[fh]
                    .bundle(&have)
                    .map_err(|error| self.fail(format!("bundle on d{from}: {error}")))?;
                self.trace
                    .push(format!("d{from}/h{fh} answers as b{}", self.sent.len()));
                self.sent.push((to, from, bundle.clone(), empty));
                self.bring(to, from, th, fh, Some(&bundle), empty).map(drop)
            }
            Op::Redeliver(pick, th) => {
                if self.sent.is_empty() {
                    return Ok(());
                }
                let at = pick % self.sent.len();
                let (to, from, bundle, empty) = self.sent[at].clone();
                self.trace.push(format!("b{at} arrives again at d{to}"));
                self.bring(to, from, th, 0, Some(&bundle), empty).map(drop)
            }
            Op::Side(to, from, th, fh) => self.side_pull(to, from, th, fh),
        }
    }

    fn settle(&mut self, rng: &mut Rng, rounds: usize) -> Result<(), String> {
        let count = self.devices.len();
        for round in 0..rounds {
            let before = (self.merges, self.heads());
            let mut pairs: Vec<(usize, usize)> = (0..count)
                .flat_map(|a| (0..count).filter(move |b| *b != a).map(move |b| (a, b)))
                .collect();
            rng.shuffle(&mut pairs);
            for (a, b) in pairs {
                self.pull(a, b, 0, 0)?;
                self.verify()?;
            }
            if (self.merges, self.heads()) != before {
                continue;
            }
            let first = self.model.value(self.devices[0].head);
            if self.strict
                && let Some(other) = self
                    .devices
                    .iter()
                    .position(|device| self.model.value(device.head) != first)
            {
                return Err(self.fail(format!(
                    "settled after {round} rounds with d{other} unlike d0"
                )));
            }
            return Ok(());
        }
        Err(self.fail(format!("still writing after {rounds} rounds of syncs")))
    }

    fn heads(&self) -> Vec<usize> {
        self.devices.iter().map(|device| device.head).collect()
    }
}

fn replay(seed: u64, setups: &[Setup], ops: &[Op]) -> Result<Mesh, String> {
    let mut mesh = Mesh::new(setups);
    for (step, op) in ops.iter().enumerate() {
        mesh.apply(*op)
            .and_then(|()| mesh.verify())
            .map_err(|error| format!("step {step}: {error}"))?;
    }
    mesh.settle(&mut Rng::new(seed ^ 0x5e771e), 8)
        .map_err(|error| format!("settling: {error}"))?;
    Ok(mesh)
}

fn sweep(name: &str, plan: Plan, seeds: u32) -> (usize, usize) {
    let (mut merges, mut refused) = (0, 0);
    for seed in 0..u64::from(seeds) {
        let (setups, ops) = case(seed, plan);
        match replay(seed, &setups, &ops) {
            Ok(mesh) => {
                merges += mesh.merges;
                refused += mesh.refused;
            }
            Err(error) => panic!(
                "{name} seed {seed} {setups:?}\n{error}\nFERRO_SYNC_SHRINK={name}:{seed} shrinks it"
            ),
        }
    }
    (merges, refused)
}

fn plan_named(name: &str) -> Plan {
    match name {
        "keep_all" => KEEP_ALL,
        "mixed" => MIXED,
        "pruned_harder" => PRUNED_HARDER,
        _ => panic!("no plan {name}"),
    }
}

#[test]
fn files_that_keep_everything_sync_in_any_order_apply_every_change_once_and_settle() {
    let (merges, _) = sweep("keep_all", KEEP_ALL, cases(3));
    assert!(merges > 10, "{merges}");
}

#[test]
fn files_some_pruned_sync_in_any_order_apply_every_change_once_or_refuse() {
    let (merges, _) = sweep("mixed", MIXED, cases(3));
    assert!(merges > 10, "{merges}");
}

#[test]
fn pruned_files_compacted_harder_sync_in_any_order_apply_every_change_once_or_refuse() {
    let (merges, refused) = sweep("pruned_harder", PRUNED_HARDER, cases(3));
    assert!(merges > 0 && refused > 0, "{merges} {refused}");
}

#[test]
fn shrink_a_failing_sync_model_case() {
    let Ok(spec) = std::env::var("FERRO_SYNC_SHRINK") else {
        return;
    };
    let (name, seed) = spec.split_once(':').unwrap();
    let seed: u64 = seed.parse().unwrap();
    let like = std::env::var("FERRO_SYNC_SHRINK_LIKE").unwrap_or_default();
    let (setups, mut ops) = case(seed, plan_named(name));
    let fails = |ops: &[Op]| match replay(seed, &setups, ops) {
        Err(error) => error.contains(&like),
        Ok(_) => false,
    };
    assert!(fails(&ops), "{name} seed {seed} does not fail");
    let mut chunk = ops.len() / 2;
    while chunk >= 1 {
        let mut at = 0;
        while at < ops.len() {
            let mut fewer = ops.clone();
            fewer.drain(at..(at + chunk).min(ops.len()));
            if fails(&fewer) {
                ops = fewer;
            } else {
                at += chunk;
            }
        }
        chunk /= 2;
    }
    let error = replay(seed, &setups, &ops).err().unwrap_or_default();
    panic!("{setups:?}\n{ops:?}\n{error}");
}

fn sum_is_one(db: &Db<Counts>) -> Result<(), Vec<(u32, i64)>> {
    let bad: Vec<(u32, i64)> = values(&db.read())
        .into_iter()
        .filter(|(_, v)| *v != 1)
        .collect();
    if bad.is_empty() { Ok(()) } else { Err(bad) }
}

fn settle_files(devices: &[Db<Counts>], all: bool, when: &str) -> Result<(), String> {
    let check = |stage: &str| -> Result<(), String> {
        for (at, db) in devices.iter().enumerate() {
            db.refresh().ok();
            db.verify()
                .and_then(|_| db.verify_typed())
                .map_err(|error| format!("{when} {stage}: verify d{at}: {error}"))?;
            sum_is_one(db).map_err(|bad| format!("{when} {stage}: d{at} holds {bad:?}"))?;
        }
        Ok(())
    };
    check("before settling")?;
    for round in 0..8 {
        let before: Vec<_> = devices.iter().map(Db::head_hash).collect();
        for to in 0..devices.len() {
            for from in (0..devices.len()).filter(|from| *from != to) {
                match devices[to].pull(&devices[from], &format!("d{from}"), merge) {
                    Ok(_) => {}
                    Err(Error::NoCommonAncestor) if !all => {}
                    Err(error) => return Err(format!("{when}: d{to} <- d{from}: {error}")),
                }
            }
        }
        check("settling")?;
        if devices.iter().map(Db::head_hash).collect::<Vec<_>>() == before {
            let first = values(&devices[0].read());
            if all && devices.iter().any(|db| values(&db.read()) != first) {
                return Err(format!(
                    "{when}: settled after {round} rounds unlike each other"
                ));
            }
            return Ok(());
        }
    }
    Err(format!("{when}: still writing after 8 rounds"))
}

fn threads(seed: u64, keeps: &[Keep], steps: usize) -> Result<(), String> {
    let dir = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicU64::new(1_000_000));
    let all = keeps.iter().all(|keep| matches!(keep, Keep::All));
    let devices: Vec<[Db<Counts>; 2]> = keeps
        .iter()
        .enumerate()
        .map(|(at, keep)| {
            let path = dir.path().join(format!("d{at}.ferro"));
            let setup = Setup {
                keep: *keep,
                shared: true,
            };
            [open(&path, setup, &now), open(&path, setup, &now)]
        })
        .collect();
    let count = devices.len();
    let next = AtomicU32::new(0);
    let errors = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for device in 0..count {
            let (devices, next, errors) = (&devices, &next, &errors);
            let mut rng = Rng::new(seed * 64 + device as u64);
            scope.spawn(move || {
                let mut mine: Vec<u32> = Vec::new();
                for _ in 0..steps {
                    let db = &devices[device][0];
                    let result = match !mine.is_empty() && rng.chance(25) {
                        true => {
                            let key = mine.remove(rng.below(mine.len() as u64) as usize);
                            db.commit(format!("remove {key}"), |root| add(root, key, -1))
                        }
                        false => {
                            let key = next.fetch_add(1, Ordering::SeqCst);
                            mine.push(key);
                            db.commit(format!("add {key}"), |root| add(root, key, 1))
                        }
                    };
                    let compacted = match rng.chance(5) {
                        true => db.compact(keeps[device]).map(drop),
                        false => Ok(()),
                    };
                    if let Err(error) = result.and(compacted) {
                        errors.lock().unwrap().push(format!("d{device}: {error}"));
                    }
                }
            });
            for handle in 0..2 {
                let mut rng = Rng::new(seed * 64 + 16 + device as u64 * 2 + handle as u64);
                scope.spawn(move || {
                    for _ in 0..steps {
                        let from = (device + 1 + rng.below(count as u64 - 1) as usize) % count;
                        let remote = format!("d{from}");
                        if rng.chance(4) {
                            let _ = devices[device][handle].drop_branch(&remote);
                        }
                        let side = rng.below(2) as usize;
                        let pruned = !matches!(keeps[device], Keep::All);
                        match devices[device][handle].pull(&devices[from][side], &remote, merge) {
                            Ok(_) | Err(Error::NoBranch(_)) => {}
                            Err(Error::NoCommonAncestor) if !all => {}
                            Err(Error::StaleHave(_)) if pruned => {}
                            Err(error) => errors
                                .lock()
                                .unwrap()
                                .push(format!("d{device} <- d{from}: {error}")),
                        }
                    }
                });
            }
        }
    });
    let errors = errors.into_inner().unwrap();
    if !errors.is_empty() {
        return Err(format!("seed {seed} {keeps:?}: {errors:?}"));
    }
    let plain: Vec<Db<Counts>> = devices.into_iter().map(|[db, _]| db).collect();
    settle_files(&plain, all, &format!("seed {seed}"))
}

#[test]
fn threads_on_two_handles_of_each_shared_file_syncing_at_once_apply_every_change_once() {
    for seed in 0..u64::from(cases(2)) {
        threads(seed, &[Keep::All, Keep::All, Keep::All], 40).unwrap();
        threads(seed, &[Keep::Last(3), Keep::All, Keep::Last(6)], 40).unwrap();
    }
}

fn keep_code(keep: Keep) -> String {
    match keep {
        Keep::Last(n) => n.to_string(),
        _ => "0".to_string(),
    }
}

#[test]
fn sync_model_child() {
    let Ok(dir) = std::env::var("FERRO_SYNC_CHILD_DIR") else {
        return;
    };
    let me: u32 = std::env::var("FERRO_SYNC_CHILD_ID")
        .unwrap()
        .parse()
        .unwrap();
    let steps: usize = std::env::var("FERRO_SYNC_CHILD_STEPS")
        .unwrap()
        .parse()
        .unwrap();
    let keeps: Vec<Keep> = std::env::var("FERRO_SYNC_CHILD_KEEPS")
        .unwrap()
        .split(',')
        .map(|code| match code.parse::<usize>().unwrap() {
            0 => Keep::All,
            n => Keep::Last(n),
        })
        .collect();
    let all = keeps.iter().all(|keep| matches!(keep, Keep::All));
    let now = Arc::new(AtomicU64::new(1_000_000 + u64::from(me) * 7));
    let devices: Vec<Db<Counts>> = keeps
        .iter()
        .enumerate()
        .map(|(at, keep)| {
            let setup = Setup {
                keep: *keep,
                shared: true,
            };
            open(&Path::new(&dir).join(format!("d{at}.ferro")), setup, &now)
        })
        .collect();
    let count = devices.len();
    let mut rng = Rng::new(u64::from(me) * 1_000_003 + 17);
    let mut mine: Vec<(usize, u32)> = Vec::new();
    for step in 0..steps {
        let device = rng.below(count as u64) as usize;
        let db = &devices[device];
        let result = match rng.below(100) {
            0..40 => match !mine.is_empty() && rng.chance(20) {
                true => {
                    let (at, key) = mine.remove(rng.below(mine.len() as u64) as usize);
                    devices[at].commit(format!("remove {key}"), |root| add(root, key, -1))
                }
                false => {
                    let key = me * 1_000_000 + step as u32;
                    mine.push((device, key));
                    db.commit(format!("add {key}"), |root| add(root, key, 1))
                }
            },
            40..45 => db.compact(keeps[device]).map(drop),
            _ => {
                let from = (device + 1 + rng.below(count as u64 - 1) as usize) % count;
                let remote = format!("d{from}");
                if rng.chance(3) {
                    let _ = db.drop_branch(&remote);
                }
                let pruned = !matches!(keeps[device], Keep::All);
                match db.pull(&devices[from], &remote, merge) {
                    Err(Error::NoCommonAncestor) if !all => Ok(()),
                    Err(Error::StaleHave(_)) if pruned => Ok(()),
                    Err(Error::NoBranch(_)) => Ok(()),
                    other => other.map(drop),
                }
            }
        };
        if let Err(error) = result {
            println!("ERROR step {step} on d{device}: {error}");
        }
    }
    println!("DONE {me}");
}

fn processes(seed: u64, keeps: &[Keep], children: u32, steps: usize) -> Result<(), String> {
    let dir = tempfile::tempdir().unwrap();
    let codes: Vec<String> = keeps.iter().map(|keep| keep_code(*keep)).collect();
    let spawned: Vec<_> = (0..children)
        .map(|child| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sync_model_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("FERRO_SYNC_CHILD_DIR", dir.path())
                .env(
                    "FERRO_SYNC_CHILD_ID",
                    (seed as u32 * 16 + child).to_string(),
                )
                .env("FERRO_SYNC_CHILD_STEPS", steps.to_string())
                .env("FERRO_SYNC_CHILD_KEEPS", codes.join(","))
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut errors = Vec::new();
    for child in spawned {
        let output = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        errors.extend(
            text.lines()
                .filter(|l| l.starts_with("ERROR"))
                .map(String::from),
        );
        if !output.status.success() || !text.contains("DONE") {
            errors.push(format!("a child failed: {}", output.status));
        }
    }
    if !errors.is_empty() {
        return Err(format!("seed {seed}: {errors:?}"));
    }
    let now = Arc::new(AtomicU64::new(10_000_000));
    let devices: Vec<Db<Counts>> = keeps
        .iter()
        .enumerate()
        .map(|(at, keep)| {
            let setup = Setup {
                keep: *keep,
                shared: false,
            };
            open(&dir.path().join(format!("d{at}.ferro")), setup, &now)
        })
        .collect();
    let all = keeps.iter().all(|keep| matches!(keep, Keep::All));
    settle_files(&devices, all, &format!("seed {seed}"))
}

#[test]
fn processes_syncing_shared_files_at_once_apply_every_change_once() {
    for seed in 0..u64::from(cases(1)) {
        processes(seed, &[Keep::All, Keep::All, Keep::All], 3, 150).unwrap();
        processes(seed, &[Keep::Last(3), Keep::All, Keep::Last(8)], 3, 150).unwrap();
    }
}
