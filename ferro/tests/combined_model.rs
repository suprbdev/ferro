mod common;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use common::{Rng, cases, fast};
use ferro::db::Hook;
use ferro::{Db, Error, Hash, Keep, Map, Options, Outcome, Reclaim, Snapshot};
use tempfile::TempDir;

type Events = Map<u32, i64>;
type Counts = BTreeMap<u32, i64>;
type Seen = (Option<Hash>, Counts);
type Job = Box<dyn FnOnce() + Send>;
type Armed = Arc<Mutex<Option<(&'static str, Job)>>>;
type Crossed = Arc<Mutex<Option<(ferro::Result<Outcome>, Seen)>>>;

const LOCAL: [&str; 2] = ["main", "side"];
const CACHES: [u64; 3] = [1 << 20, 16 << 10, 2 << 10];
const STAGES: [&str; 3] = ["created", "copied", "synced"];

struct Flag {
    thread: Thread,
    woken: AtomicBool,
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::SeqCst);
        self.thread.unpark();
    }
}

fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut futures: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    let flag = Arc::new(Flag {
        thread: thread::current(),
        woken: AtomicBool::new(false),
    });
    let waker = Waker::from(flag.clone());
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_none()
                && let Poll::Ready(out) = future.as_mut().poll(&mut cx)
            {
                *output = Some(out);
            }
        }
        if outputs.iter().all(Option::is_some) {
            return outputs.into_iter().map(Option::unwrap).collect();
        }
        while !flag.woken.swap(false, Ordering::SeqCst) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "an awaited write was never woken");
            thread::park_timeout(left);
        }
    }
}

fn add_deltas(root: &mut Events, base: &Snapshot<Events>, theirs: &Snapshot<Events>) {
    let mut keys: Vec<u32> = base.keys().chain(theirs.keys()).collect();
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        let delta = theirs.get(&key).map_or(0, |v| *v) - base.get(&key).map_or(0, |v| *v);
        if delta == 0 {
            continue;
        }
        match root.get(&key).map_or(0, |v| *v) + delta {
            0 => {
                root.remove(&key);
            }
            next => {
                root.insert(key, next);
            }
        }
    }
}

fn counted(db: &Db<Events>) -> Counts {
    db.read().iter().map(|(k, v)| (k, *v)).collect()
}

struct Node {
    event: Option<u32>,
    ancestors: Vec<bool>,
}

#[derive(Default)]
struct Graph(Vec<Node>);

impl Graph {
    fn push(&mut self, parents: &[usize], event: Option<u32>) -> usize {
        let id = self.0.len();
        let mut ancestors = vec![false; id + 1];
        for parent in parents {
            for (at, held) in self.0[*parent].ancestors.iter().enumerate() {
                ancestors[at] |= *held;
            }
        }
        ancestors[id] = true;
        self.0.push(Node { event, ancestors });
        id
    }

    fn holds(&self, node: usize, ancestor: usize) -> bool {
        self.0[node]
            .ancestors
            .get(ancestor)
            .copied()
            .unwrap_or(false)
    }

    fn events(&self, node: usize) -> Counts {
        let mut events = Counts::new();
        for (at, held) in self.0[node].ancestors.iter().enumerate() {
            if let (true, Some(event)) = (*held, self.0[at].event) {
                events.insert(event, 1);
            }
        }
        events
    }
}

struct Device {
    path: PathBuf,
    keep: Keep,
    cache: u64,
    coalesce: bool,
    dbs: [Option<Db<Events>>; 2],
    heads: [usize; 2],
    forks: usize,
    reader: Option<Db<Events>>,
    armed: Armed,
}

impl Device {
    fn options(&self) -> Options {
        let armed = self.armed.clone();
        fast()
            .cache_bytes(self.cache)
            .history(self.keep)
            .reclaim(Reclaim { every_bytes: 0 })
            .coalesce(self.coalesce)
            .hook(Hook(Arc::new(move |step| {
                let mut slot = armed.lock().unwrap();
                if slot.as_ref().is_some_and(|(stage, _)| *stage == step) {
                    let (_, job) = slot.take().unwrap();
                    drop(slot);
                    job();
                }
            })))
    }

    fn open(&mut self) {
        let main: Db<Events> = self.options().open(&self.path).unwrap();
        let side = main.branches().iter().any(|name| name == LOCAL[1]);
        self.dbs[1] = side.then(|| main.branch(LOCAL[1]).unwrap());
        self.dbs[0] = Some(main);
        if self.reader.is_none() {
            self.reader = Some(Options::new().read_only().open(&self.path).unwrap());
        }
    }

    fn close(&mut self) {
        self.dbs = [None, None];
    }

    fn main(&self) -> &Db<Events> {
        self.dbs[0].as_ref().unwrap()
    }

    fn pruned(&self) -> bool {
        self.keep != Keep::All
    }
}

struct World {
    graph: Graph,
    devices: Vec<Device>,
    events: u32,
    tally: BTreeMap<&'static str, usize>,
    seed: u64,
    step: usize,
    pruned: bool,
    _dir: TempDir,
}

impl World {
    fn when(&self) -> String {
        format!("seed {} step {}", self.seed, self.step)
    }

    fn count(&mut self, what: &'static str) {
        *self.tally.entry(what).or_default() += 1;
    }

    fn commit(&mut self, device: usize, slot: usize) {
        let Some(db) = self.devices[device].dbs[slot].clone() else {
            return;
        };
        let event = self.events;
        self.events += 1;
        db.commit(format!("e{event}"), |root| {
            root.insert(event, 1);
        })
        .unwrap();
        let head = self.devices[device].heads[slot];
        self.devices[device].heads[slot] = self.graph.push(&[head], Some(event));
    }

    fn commit_async(&mut self, device: usize, picks: &[u64]) {
        let slots: Vec<usize> = (0..2)
            .filter(|slot| self.devices[device].dbs[*slot].is_some())
            .collect();
        let mut planned = Vec::new();
        for pick in picks {
            let slot = slots[(*pick % slots.len() as u64) as usize];
            planned.push((slot, self.events));
            self.events += 1;
        }
        let handles: Vec<Db<Events>> = planned
            .iter()
            .map(|(slot, _)| self.devices[device].dbs[*slot].clone().unwrap())
            .collect();
        let futures: Vec<_> = planned
            .iter()
            .zip(&handles)
            .map(|((_, event), db)| {
                let event = *event;
                db.commit_async(format!("e{event}"), move |root| {
                    root.insert(event, 1);
                })
            })
            .collect();
        for done in join_all(futures) {
            done.unwrap();
        }
        for (slot, event) in planned {
            let head = self.devices[device].heads[slot];
            self.devices[device].heads[slot] = self.graph.push(&[head], Some(event));
        }
        self.count("async");
    }

    fn merge(&mut self, device: usize, back: bool) {
        let when = self.when();
        let [Some(main), Some(side)] = self.devices[device].dbs.clone() else {
            return;
        };
        let (a, b) = if back { (1, 0) } else { (0, 1) };
        let (ours, theirs) = if back { (side, main) } else { (main, side) };
        let [ha, hb] = [self.devices[device].heads[a], self.devices[device].heads[b]];
        let before = (ours.head_hash(), counted(&ours));
        let id = self.graph.0.len();
        let merged = ours.merge_with(&theirs, |root, base, other| {
            add_deltas(root, base, other);
            (format!("n{id}"), ())
        });
        match merged {
            Ok(done) => {
                if self.graph.holds(ha, hb) {
                    assert!(
                        !done.changed,
                        "{when}: a merge of what we hold wrote a commit"
                    );
                    self.count("merge held");
                    return;
                }
                assert!(done.changed, "{when}: a merge wrote nothing");
                self.devices[device].heads[a] = self.graph.push(&[ha, hb], None);
                self.count("merged");
            }
            Err(Error::NoCommonAncestor) if self.pruned => {
                assert_eq!(
                    (ours.head_hash(), counted(&ours)),
                    before,
                    "{when}: refused merge"
                );
                self.count("merge refused");
            }
            Err(error) => panic!("{when}: d{device} merge back={back}: {error}"),
        }
    }

    fn pull(&mut self, a: usize, s: usize, b: usize, t: usize, remote: &str) -> Option<Outcome> {
        let (Some(ours), Some(theirs)) = (
            self.devices[a].dbs[s].clone(),
            self.devices[b].dbs[t].clone(),
        ) else {
            return None;
        };
        let (ha, hb) = (self.devices[a].heads[s], self.devices[b].heads[t]);
        let before = (ours.head_hash(), counted(&ours));
        let pulled = pull_counting(&ours, &theirs, remote, self.graph.0.len());
        let after = (ours.head_hash(), counted(&ours));
        let what = format!("d{a} {} <- d{b} {} as {remote}", LOCAL[s], LOCAL[t]);
        self.account(a, s, (ha, hb), (before, after), pulled, &what)
    }

    fn account(
        &mut self,
        a: usize,
        s: usize,
        (ha, hb): (usize, usize),
        (before, after): (Seen, Seen),
        pulled: ferro::Result<Outcome>,
        what: &str,
    ) -> Option<Outcome> {
        let when = self.when();
        let outcome = match pulled {
            Ok(outcome) => outcome,
            Err(Error::NoCommonAncestor) if self.pruned => {
                assert_eq!(after, before, "{when}: refused pull");
                self.count("pull refused");
                return None;
            }
            Err(error) => panic!("{when}: {what}: {error}"),
        };
        match outcome {
            Outcome::Merged => {
                assert!(!self.graph.holds(ha, hb), "{when}: merged what it held");
                self.devices[a].heads[s] = self.graph.push(&[ha, hb], None);
                self.count("pull merged");
            }
            Outcome::FastForward => {
                assert!(self.graph.holds(hb, ha), "{when}: fast-forward");
                self.devices[a].heads[s] = hb;
                self.count("pull fast-forward");
            }
            Outcome::Ahead => {
                assert!(self.graph.holds(ha, hb), "{when}: ahead");
                self.count("pull ahead");
            }
            Outcome::UpToDate => {
                let same = self.graph.events(ha) == self.graph.events(hb);
                assert!(same || self.graph.holds(ha, hb), "{when}: up to date");
                self.count("pull up to date");
            }
        }
        Some(outcome)
    }

    fn sync(&mut self, a: usize, b: usize) {
        let when = self.when();
        let (ra, rb) = (format!("d{b}"), format!("d{a}"));
        let synced = self.pull(a, 0, b, 0, &ra).is_some()
            && match self.pull(b, 0, a, 0, &rb) {
                Some(Outcome::Merged) => self.pull(a, 0, b, 0, &ra).is_some(),
                other => other.is_some(),
            };
        if !synced {
            return;
        }
        self.check();
        for (into, out, remote) in [(a, b, &ra), (b, a, &rb)] {
            let again = self.pull(into, 0, out, 0, remote);
            assert!(
                again.is_some_and(|outcome| outcome != Outcome::Merged),
                "{when}: d{into} <- d{out} right after a sync: {again:?}"
            );
        }
        self.count("synced");
    }

    fn relay(&mut self, a: usize, b: usize, commit: bool, compact: bool) {
        let when = self.when();
        let Some(theirs) = self.devices[b].dbs[0].clone() else {
            return;
        };
        let ours = self.devices[a].main().clone();
        let hb = self.devices[b].heads[0];
        let remote = format!("d{b}");
        let have = ours.have().unwrap();
        if commit {
            self.commit(a, 0);
        }
        if compact {
            let keep = self.devices[a].keep;
            ours.compact(keep).unwrap();
        }
        let ha_now = self.devices[a].heads[0];
        let bundle = theirs.bundle(&have).unwrap();
        let before = (ours.head_hash(), counted(&ours));
        let id = self.graph.0.len();
        let received = ours.receive(&remote, &bundle, |root, base, other| {
            add_deltas(root, base, other);
            (format!("n{id}"), ())
        });
        let outcome = match received {
            Ok(pulled) => pulled.outcome,
            Err(Error::NoCommonAncestor | Error::StaleHave(_)) if self.pruned => {
                assert_eq!(
                    (ours.head_hash(), counted(&ours)),
                    before,
                    "{when}: refused receive"
                );
                self.count("receive refused");
                return;
            }
            Err(error) => panic!("{when}: d{a} receives d{b}: {error}"),
        };
        match outcome {
            Outcome::Merged => {
                self.devices[a].heads[0] = self.graph.push(&[ha_now, hb], None);
                self.count("receive merged");
            }
            Outcome::FastForward => {
                assert!(
                    self.graph.holds(hb, ha_now),
                    "{when}: received fast-forward"
                );
                self.devices[a].heads[0] = hb;
            }
            Outcome::Ahead | Outcome::UpToDate => {}
        }
    }

    fn compact(&mut self, device: usize) {
        let keep = self.devices[device].keep;
        self.devices[device].main().compact(keep).unwrap();
        self.count("compact");
    }

    fn burst(&mut self, device: usize, slot: usize, commits: usize) {
        for _ in 0..commits {
            self.commit(device, slot);
        }
        self.count("burst");
    }

    fn reland(&mut self, a: usize, b: usize) {
        let remote = self.remote(b, 0);
        let main = self.devices[a].main().clone();
        if main.branches().contains(&remote) {
            main.drop_branch(&remote).unwrap();
            self.count("dropped to reland");
        }
        self.pull(a, 0, b, 0, &remote);
    }

    fn cross_cycle(&mut self, a: usize, b: usize, stage: &'static str, commit: bool) {
        let remote = self.remote(b, 0);
        let ours = self.devices[a].main().clone();
        let theirs = self.devices[b].main().clone();
        let (ha, hb) = (self.devices[a].heads[0], self.devices[b].heads[0]);
        let before = (ours.head_hash(), counted(&ours));
        let id = self.graph.0.len();
        let event = self.events;
        let crossed: Crossed = Arc::new(Mutex::new(None));
        let job = {
            let (ours, remote, crossed) = (ours.clone(), remote.clone(), crossed.clone());
            move || {
                if ours.branches().contains(&remote) {
                    ours.drop_branch(&remote).unwrap();
                }
                let pulled = pull_counting(&ours, &theirs, &remote, id);
                let after = (ours.head_hash(), counted(&ours));
                if commit {
                    ours.commit(format!("e{event}"), |root| {
                        root.insert(event, 1);
                    })
                    .unwrap();
                }
                *crossed.lock().unwrap() = Some((pulled, after));
            }
        };
        *self.devices[a].armed.lock().unwrap() = Some((stage, Box::new(job)));
        let keep = self.devices[a].keep;
        ours.compact(keep).unwrap();
        self.devices[a].armed.lock().unwrap().take();
        let Some((pulled, after)) = crossed.lock().unwrap().take() else {
            return;
        };
        let what = format!("d{a} <- d{b} as {remote} while a cycle was {stage}");
        self.account(a, 0, (ha, hb), (before, after), pulled, &what);
        self.count("cycle relanded");
        if commit {
            self.events += 1;
            let head = self.devices[a].heads[0];
            self.devices[a].heads[0] = self.graph.push(&[head], Some(event));
        }
        self.count("cycle crossed");
    }

    fn reopen(&mut self, device: usize, offline: bool) {
        self.devices[device].close();
        if offline {
            let keep = self.devices[device].keep;
            ferro::admin::compact(&self.devices[device].path, keep).unwrap();
            ferro::admin::verify(&self.devices[device].path).unwrap();
        }
        self.devices[device].open();
        self.count("reopen");
    }

    fn remote(&self, device: usize, slot: usize) -> String {
        match slot {
            0 => format!("d{device}main"),
            _ => format!("d{device}side{}", self.devices[device].forks),
        }
    }

    fn fork_or_drop_side(&mut self, device: usize) {
        let dev = &mut self.devices[device];
        match dev.dbs[1].take() {
            Some(_) => {
                dev.forks += 1;
                dev.main().drop_branch(LOCAL[1]).unwrap();
            }
            None => {
                dev.dbs[1] = Some(dev.main().fork(LOCAL[1]).unwrap());
                dev.heads[1] = dev.heads[0];
            }
        }
    }

    fn drop_tracking(&mut self, device: usize, pick: u64) {
        let main = self.devices[device].main().clone();
        let tracking: Vec<String> = main
            .branches()
            .into_iter()
            .filter(|name| !LOCAL.contains(&name.as_str()))
            .collect();
        if tracking.is_empty() {
            return;
        }
        let name = &tracking[(pick % tracking.len() as u64) as usize];
        main.drop_branch(name).unwrap();
        self.count("drop tracking");
    }

    fn check(&self) {
        let when = self.when();
        for (index, device) in self.devices.iter().enumerate() {
            let main = device.main();
            main.verify()
                .unwrap_or_else(|error| panic!("{when}: d{index}: {error}"));
            main.verify_typed()
                .unwrap_or_else(|error| panic!("{when}: d{index}: {error}"));
            for name in main.branches() {
                let log = main.branch(&name).unwrap().log();
                assert_eq!(log.iter().count(), log.len(), "{when}: d{index} {name}");
            }
            let reader = device.reader.as_ref().unwrap();
            reader
                .refresh()
                .unwrap_or_else(|error| panic!("{when}: d{index} refresh: {error}"));
            reader
                .verify_typed()
                .unwrap_or_else(|error| panic!("{when}: d{index} reader: {error}"));
            let cold: Db<Events> = Options::new().read_only().open(&device.path).unwrap();
            cold.verify_typed()
                .unwrap_or_else(|error| panic!("{when}: d{index} cold: {error}"));
            assert_eq!(cold.branches(), main.branches(), "{when}: d{index}");
            assert_eq!(reader.branches(), main.branches(), "{when}: d{index}");
            for name in main.branches() {
                let (ours, theirs) = (main.branch(&name).unwrap(), reader.branch(&name).unwrap());
                assert_eq!(
                    ours.head_hash(),
                    theirs.head_hash(),
                    "{when}: d{index} {name}"
                );
                assert_eq!(counted(&ours), counted(&theirs), "{when}: d{index} {name}");
            }
            for ((db, head), name) in device.dbs.iter().zip(device.heads).zip(LOCAL) {
                if let Some(db) = db {
                    let expected = self.graph.events(head);
                    assert_eq!(counted(db), expected, "{when}: d{index} {name}");
                }
            }
        }
    }
}

fn pull_counting(
    ours: &Db<Events>,
    theirs: &Db<Events>,
    remote: &str,
    id: usize,
) -> ferro::Result<Outcome> {
    let pulled = ours.pull(theirs, remote, |root, base, other| {
        add_deltas(root, base, other);
        (format!("n{id}"), ())
    });
    pulled.map(|pulled| pulled.outcome)
}

#[derive(Clone, Debug)]
enum Act {
    Commit(usize, usize),
    Async(usize, Vec<u64>),
    Merge(usize, bool),
    Pull(usize, usize, usize, usize),
    Sync(usize, usize),
    Relay(usize, usize, bool, bool),
    Compact(usize),
    Reopen(usize, bool),
    Side(usize),
    DropTracking(usize, u64),
    Burst(usize, usize, usize),
    Reland(usize, usize),
    CrossCycle(usize, usize, &'static str, bool),
}

struct Setup {
    keeps: Vec<Keep>,
    coalesce: Vec<bool>,
}

fn around_tables(rng: &mut Rng, roll: u64, (a, b): (usize, usize), (s, t): (usize, usize)) -> Act {
    match roll {
        0..16 => Act::Commit(a, s),
        16..22 => Act::Burst(a, s, 60 + rng.below(90) as usize),
        22..30 => Act::Merge(a, rng.chance(50)),
        30..44 => Act::Pull(a, s, b, t),
        44..50 => Act::Sync(a, b),
        50..58 => Act::Relay(a, b, rng.chance(30), rng.chance(30)),
        58..62 => Act::Compact(a),
        62..72 => {
            let stage = STAGES[rng.below(STAGES.len() as u64) as usize];
            Act::CrossCycle(a, b, stage, rng.chance(50))
        }
        72..78 => Act::Reland(a, b),
        78..83 => Act::Reopen(a, rng.chance(40)),
        83..90 => Act::Side(a),
        _ => Act::DropTracking(a, rng.next()),
    }
}

fn plan(seed: u64, steps: usize, pruned: bool, tables: bool) -> (Setup, Vec<Act>) {
    let mut rng = Rng::new(seed.wrapping_mul(7919).wrapping_add(17));
    let count = 3;
    let mut setup = Setup {
        keeps: Vec::new(),
        coalesce: Vec::new(),
    };
    for _ in 0..count {
        let keep = if pruned && rng.chance(80) {
            Keep::Last(2 + rng.below(5) as usize)
        } else {
            Keep::All
        };
        setup.keeps.push(keep);
        setup.coalesce.push(rng.chance(50));
    }
    let mut acts = Vec::new();
    for _ in 0..steps {
        let a = rng.below(count as u64) as usize;
        let b = (a + 1 + rng.below(count as u64 - 1) as usize) % count;
        let (s, t) = (rng.below(2) as usize, rng.below(2) as usize);
        let roll = rng.below(100);
        if tables {
            acts.push(around_tables(&mut rng, roll, (a, b), (s, t)));
            continue;
        }
        acts.push(match roll {
            0..22 => Act::Commit(a, s),
            22..28 => {
                let count = 1 + rng.below(5) as usize;
                Act::Async(a, (0..count).map(|_| rng.next()).collect())
            }
            28..38 => Act::Merge(a, rng.chance(50)),
            38..58 => Act::Pull(a, s, b, t),
            58..66 => Act::Sync(a, b),
            66..72 => Act::Relay(a, b, rng.chance(30), rng.chance(30)),
            72..80 => Act::Compact(a),
            80..85 => Act::Reopen(a, rng.chance(40)),
            85..92 => Act::Side(a),
            _ => Act::DropTracking(a, rng.next()),
        });
    }
    (setup, acts)
}

fn execute(seed: u64, setup: &Setup, acts: &[Act]) -> BTreeMap<&'static str, usize> {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = Graph::default();
    graph.push(&[], None);
    let mut devices = Vec::new();
    for (index, keep) in setup.keeps.iter().enumerate() {
        let mut device = Device {
            path: dir.path().join(format!("d{index}.ferro")),
            keep: *keep,
            cache: CACHES[index % CACHES.len()],
            coalesce: setup.coalesce[index],
            dbs: [None, None],
            heads: [0, 0],
            forks: 0,
            reader: None,
            armed: Armed::default(),
        };
        device.open();
        devices.push(device);
    }
    let mut world = World {
        _dir: dir,
        graph,
        pruned: devices.iter().any(Device::pruned),
        devices,
        events: 0,
        tally: BTreeMap::new(),
        seed,
        step: 0,
    };
    for (step, act) in acts.iter().enumerate() {
        world.step = step;
        match act.clone() {
            Act::Commit(a, s) => world.commit(a, s),
            Act::Async(a, picks) => world.commit_async(a, &picks),
            Act::Merge(a, back) => world.merge(a, back),
            Act::Pull(a, s, b, t) => {
                let remote = world.remote(b, t);
                world.pull(a, s, b, t, &remote);
            }
            Act::Sync(a, b) => world.sync(a, b),
            Act::Relay(a, b, commit, compact) => world.relay(a, b, commit, compact),
            Act::Compact(a) => world.compact(a),
            Act::Reopen(a, offline) => world.reopen(a, offline),
            Act::Side(a) => world.fork_or_drop_side(a),
            Act::DropTracking(a, pick) => world.drop_tracking(a, pick),
            Act::Burst(a, s, commits) => world.burst(a, s, commits),
            Act::Reland(a, b) => world.reland(a, b),
            Act::CrossCycle(a, b, stage, commit) => world.cross_cycle(a, b, stage, commit),
        }
        world.check();
    }
    world.tally
}

fn run(seed: u64, steps: usize, pruned: bool, tables: bool) -> BTreeMap<&'static str, usize> {
    let (setup, acts) = plan(seed, steps, pruned, tables);
    execute(seed, &setup, &acts)
}

fn sweep(seeds: u64, pruned: bool, tables: bool) -> BTreeMap<&'static str, usize> {
    let mut tally = BTreeMap::new();
    for seed in 0..seeds {
        for (name, seen) in run(seed, 70, pruned, tables) {
            *tally.entry(name).or_default() += seen;
        }
    }
    tally
}

#[test]
fn devices_with_branches_merges_syncs_and_reclamation_count_each_event_once() {
    let tally = sweep(cases(100) as u64, false, false);
    assert!(tally["merged"] > 50, "{tally:?}");
    assert!(tally["pull merged"] > 50, "{tally:?}");
    assert!(tally["synced"] > 20, "{tally:?}");
}

#[test]
fn pruned_devices_with_branches_count_each_event_once_or_refuse() {
    let tally = sweep(cases(100) as u64, true, false);
    assert!(tally["merged"] > 30, "{tally:?}");
    assert!(tally["pull merged"] > 30, "{tally:?}");
    assert!(tally["pull refused"] > 10, "{tally:?}");
}

#[test]
fn devices_relanding_remotes_across_cycles_and_step_tables_count_each_event_once() {
    let tally = sweep(cases(30) as u64, false, true);
    assert!(tally["burst"] > 20, "{tally:?}");
    assert!(tally["cycle relanded"] > 20, "{tally:?}");
    assert!(tally["dropped to reland"] > 20, "{tally:?}");
}

#[test]
fn pruned_devices_relanding_remotes_across_cycles_and_step_tables_count_each_event_once() {
    let tally = sweep(cases(30) as u64, true, true);
    assert!(tally["burst"] > 20, "{tally:?}");
    assert!(tally["cycle relanded"] > 20, "{tally:?}");
    assert!(tally["dropped to reland"] > 20, "{tally:?}");
    assert!(tally["pull refused"] > 10, "{tally:?}");
}
