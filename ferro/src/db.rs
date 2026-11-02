use std::collections::HashMap;
use std::fmt;
use std::mem;
use std::ops::Deref;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::admin::Keep;
use crate::codec::{Data, Decoder, Encoder, Hash, Ptr, Sink, try_hash_of};
use crate::error::{Error, Result};
use crate::history::History;
use crate::log::{Chain, Log};
use crate::map::Map;
use crate::reclaim::{self, Compacted, Marks, Reclaim, Roots, Space};
use crate::store::{Durability, Frame, Moved, Places, Store, StoreOptions, Txn};
use crate::watch::{Feed, Watcher};

const MAIN: &str = "main";
const READ_RETRIES: u32 = 32;
const SURVIVAL_ONE: f64 = (1u64 << 32) as f64;

#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub version: u64,
    pub time_ms: u64,
    pub message: String,
    pub root: Option<Ptr>,
    pub restored: Option<u64>,
}

impl Commit {
    fn genesis() -> Self {
        Commit {
            version: 0,
            time_ms: 0,
            message: String::new(),
            root: None,
            restored: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed<R> {
    pub value: R,
    pub version: u64,
    pub changed: bool,
}

impl Data for Commit {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.version.encode(w)?;
        self.time_ms.encode(w)?;
        self.message.encode(w)?;
        self.root.encode(w)?;
        self.restored.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        Ok(Commit {
            version: u64::decode(r)?,
            time_ms: u64::decode(r)?,
            message: String::decode(r)?,
            root: Option::<Ptr>::decode(r)?,
            restored: Option::<u64>::decode(r)?,
        })
    }
}

#[derive(Clone)]
pub(crate) struct Head {
    pub(crate) version: u64,
    pub(crate) root: Option<Ptr>,
    pub(crate) log: Chain,
    pub(crate) merged: Map<String, (u64, u64)>,
}

impl Data for Head {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.version.encode(w)?;
        self.log.first.encode(w)?;
        self.root.encode(w)?;
        self.log.tip.encode(w)?;
        self.merged.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let version = u64::decode(r)?;
        let first = u64::decode(r)?;
        let root = Option::<Ptr>::decode(r)?;
        let tip = Option::<Ptr>::decode(r)?;
        let merged = Map::decode(r)?;
        Ok(Head {
            version,
            root,
            log: Chain {
                tip,
                first,
                last: version,
            },
            merged,
        })
    }
}

pub(crate) type Refs = Map<String, Head>;

#[derive(Clone)]
struct RefState {
    refs: Refs,
    ptr: Option<Ptr>,
}

pub(crate) struct Held {
    commit: Arc<Commit>,
    chain: Option<Chain>,
}

impl Deref for Held {
    type Target = Commit;

    fn deref(&self) -> &Commit {
        &self.commit
    }
}

impl PartialEq for Held {
    fn eq(&self, other: &Held) -> bool {
        self.commit == other.commit
    }
}

#[derive(Default)]
struct Pins {
    held: Vec<Weak<Held>>,
    limit: usize,
}

impl Pins {
    fn add(&mut self, held: &Arc<Held>) {
        if self.held.len() >= self.limit {
            self.held.retain(|held| held.strong_count() > 0);
            self.limit = (self.held.len() * 2).max(64);
        }
        self.held.push(Arc::downgrade(held));
    }

    fn roots(&self) -> Roots {
        let mut roots = Roots {
            lenient: true,
            ..Roots::default()
        };
        for held in self.held.iter().filter_map(Weak::upgrade) {
            roots.trees.extend(held.commit.root);
            roots.chains.extend(held.chain);
        }
        roots
    }
}

pub struct Snapshot<T> {
    value: Arc<T>,
    commit: Arc<Held>,
}

impl<T> Clone for Snapshot<T> {
    fn clone(&self) -> Self {
        Snapshot {
            value: self.value.clone(),
            commit: self.commit.clone(),
        }
    }
}

impl<T> Deref for Snapshot<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> fmt::Debug for Snapshot<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("version", &self.commit.version)
            .field("message", &self.commit.message)
            .finish_non_exhaustive()
    }
}

impl<T> Snapshot<T> {
    pub fn version(&self) -> u64 {
        self.commit.version
    }

    pub fn commit(&self) -> &Commit {
        &self.commit.commit
    }
}

struct State<T> {
    snapshot: Snapshot<T>,
    log: Chain,
}

struct Intent<T> {
    restored: Option<u64>,
    merged: Option<(Arc<Branch<T>>, u64)>,
}

impl<T> Default for Intent<T> {
    fn default() -> Self {
        Intent {
            restored: None,
            merged: None,
        }
    }
}

struct Branch<T> {
    name: String,
    state: ArcSwap<State<T>>,
    tip: ArcSwap<State<T>>,
    feed: Arc<Feed<T>>,
    retired: AtomicBool,
}

impl<T> Branch<T> {
    fn publish(&self, state: Arc<State<T>>) {
        let snapshot = state.snapshot.clone();
        self.state.store(state);
        self.feed.publish(snapshot);
    }

    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.feed.close();
    }

    fn check_live(&self) -> Result<()> {
        if self.retired.load(Ordering::Acquire) {
            return Err(Error::NoBranch(self.name.clone()));
        }
        Ok(())
    }
}

impl<T> Drop for Branch<T> {
    fn drop(&mut self) {
        self.feed.close();
    }
}

type Update<T> = Option<(Arc<Branch<T>>, Arc<State<T>>)>;

struct Pending<T> {
    frame: Frame,
    refs: RefState,
    update: Update<T>,
}

struct Queue<T> {
    waiting: Vec<Pending<T>>,
    settled: u64,
    syncing: bool,
    failed: bool,
}

struct Shared<T> {
    store: Store,
    tip: Mutex<RefState>,
    published: Mutex<RefState>,
    queue: Mutex<Queue<T>>,
    done: Condvar,
    genesis: Arc<T>,
    genesis_hash: Hash,
    branches: Mutex<HashMap<String, Arc<Branch<T>>>>,
    history: Keep,
    reclaim: Reclaim,
    automatic: bool,
    hook: Option<Hook>,
    cycle: Mutex<()>,
    log: Mutex<CycleLog>,
    pins: Mutex<Pins>,
    plain: Arc<Commit>,
    signal: Arc<Signal>,
}

#[derive(Default)]
struct CycleLog {
    cycles: u64,
    longest_pause: Duration,
    last_error: Option<String>,
}

#[derive(Default)]
struct Signal {
    stop: AtomicBool,
    due: Mutex<bool>,
    wake: Condvar,
    grown: AtomicU64,
    threshold: AtomicU64,
}

impl Signal {
    fn raise(&self) {
        *self.due.lock() = true;
        self.wake.notify_one();
    }

    fn wait(&self) -> bool {
        let mut due = self.due.lock();
        while !*due && !self.stop.load(Ordering::Acquire) {
            self.wake.wait(&mut due);
        }
        *due = false;
        !self.stop.load(Ordering::Acquire)
    }
}

struct Keeper {
    signal: Arc<Signal>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        self.signal.stop.store(true, Ordering::Release);
        self.signal.raise();
        if let Some(thread) = self.thread.lock().take() {
            let _ = thread.join();
        }
    }
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        self.store.close();
    }
}

pub struct Db<T> {
    shared: Arc<Shared<T>>,
    branch: Arc<Branch<T>>,
    keeper: Arc<Keeper>,
}

impl<T> fmt::Debug for Db<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.shared.store.path())
            .field("branch", &self.branch.name)
            .field("version", &self.branch.state.load().snapshot.commit.version)
            .finish_non_exhaustive()
    }
}

impl<T> Clone for Db<T> {
    fn clone(&self) -> Self {
        Db {
            shared: self.shared.clone(),
            branch: self.branch.clone(),
            keeper: self.keeper.clone(),
        }
    }
}

#[doc(hidden)]
#[derive(Clone)]
pub struct Hook(pub Arc<dyn Fn(&'static str) + Send + Sync>);

impl fmt::Debug for Hook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Hook")
    }
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    store: StoreOptions,
    history: Keep,
    reclaim: Reclaim,
    hook: Option<Hook>,
}

impl Options {
    fn first_refs(store: &Store, head: Option<Ptr>) -> Result<(Refs, Option<Ptr>)> {
        let mut head = head;
        for _ in 0..READ_RETRIES {
            let Some(ptr) = head else {
                return Ok((Map::new(), None));
            };
            match store.load::<Refs>(ptr) {
                Ok(refs) => return Ok(((*refs).clone(), head)),
                Err(Error::Pruned(_)) if store.is_read_only() => head = store.refresh()?,
                Err(error) => return Err(error),
            }
        }
        Err(Error::Pruned(0))
    }

    pub fn new() -> Self {
        Options::default()
    }

    pub fn durability(mut self, durability: Durability) -> Self {
        self.store.durability = durability;
        self
    }

    pub fn cache_bytes(mut self, bytes: u64) -> Self {
        self.store.cache_bytes = bytes;
        self
    }

    pub fn read_only(mut self) -> Self {
        self.store.read_only = true;
        self
    }

    pub fn history(mut self, keep: Keep) -> Self {
        self.history = keep;
        self
    }

    pub fn reclaim(mut self, reclaim: Reclaim) -> Self {
        self.reclaim = reclaim;
        self
    }

    #[doc(hidden)]
    pub fn hook(mut self, hook: Hook) -> Self {
        self.hook = Some(hook);
        self
    }

    #[doc(hidden)]
    pub fn trace(mut self, trace: crate::store::Trace) -> Self {
        self.store.trace = Some(trace);
        self
    }

    pub fn open<T: Data + Default>(self, path: impl AsRef<Path>) -> Result<Db<T>> {
        self.open_with(path, T::default)
    }

    pub fn open_with<T: Data>(
        self,
        path: impl AsRef<Path>,
        init: impl FnOnce() -> T,
    ) -> Result<Db<T>> {
        let read_only = self.store.read_only;
        let (store, head) = Store::open(path.as_ref(), self.store)?;
        let genesis = Arc::new(init());
        let genesis_hash = try_hash_of(&*genesis)?;
        let (refs, head) = Self::first_refs(&store, head)?;
        let settled = store.last_seq();
        let automatic = !read_only && self.history != Keep::All && self.reclaim.every_bytes > 0;
        let shared = Arc::new(Shared {
            store,
            tip: Mutex::new(RefState {
                refs: refs.clone(),
                ptr: head,
            }),
            published: Mutex::new(RefState { refs, ptr: head }),
            queue: Mutex::new(Queue {
                waiting: Vec::new(),
                settled,
                syncing: false,
                failed: false,
            }),
            done: Condvar::new(),
            genesis,
            genesis_hash,
            branches: Mutex::new(HashMap::new()),
            history: self.history,
            reclaim: self.reclaim,
            automatic,
            hook: self.hook,
            cycle: Mutex::new(()),
            log: Mutex::new(CycleLog::default()),
            pins: Mutex::new(Pins::default()),
            plain: Arc::new(Commit::genesis()),
            signal: Arc::new(Signal::default()),
        });
        let branch = {
            let state = shared.published.lock();
            shared.attach(MAIN, &state.refs)?
        };
        let thread = automatic.then(|| Shared::spawn(&shared));
        let keeper = Arc::new(Keeper {
            signal: shared.signal.clone(),
            thread: Mutex::new(thread),
        });
        Ok(Db {
            shared,
            branch,
            keeper,
        })
    }
}

fn write_value<D: Data>(value: &D, txn: &mut Txn) -> Result<Ptr> {
    let mut encoder = Encoder::new(txn);
    value.encode(&mut encoder)?;
    encoder.finish()
}

fn write_root<T: Data>(value: &T, txn: &mut Txn, store: &Store) -> Result<Ptr> {
    let mut encoder = Encoder::new(txn);
    value.encode(&mut encoder)?;
    let object = encoder.into_object();
    let mut decoder = Decoder::new(&object.body, &object.links, Some(store));
    T::decode(&mut decoder)?;
    let hash = object.hash();
    txn.put(object, hash)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

impl<T: Data> Shared<T> {
    fn hold(&self, commit: Arc<Commit>, chain: Option<Chain>) -> Arc<Held> {
        let held = Arc::new(Held { commit, chain });
        if !self.store.is_read_only() {
            self.pins.lock().add(&held);
        }
        held
    }

    fn state_of(&self, head: Option<&Head>) -> Result<State<T>> {
        let Some(head) = head else {
            return Ok(State {
                snapshot: Snapshot {
                    value: self.genesis.clone(),
                    commit: self.hold(self.plain.clone(), None),
                },
                log: Chain::empty(),
            });
        };
        let value = match head.root {
            Some(ptr) => self.store.load::<T>(ptr)?,
            None => self.genesis.clone(),
        };
        let commit = head
            .log
            .tip_entry(&self.store)?
            .map(|entry| entry.commit.clone())
            .unwrap_or_else(|| self.plain.clone());
        Ok(State {
            snapshot: Snapshot {
                value,
                commit: self.hold(commit, None),
            },
            log: head.log,
        })
    }

    fn attach(&self, name: &str, refs: &Refs) -> Result<Arc<Branch<T>>> {
        let mut branches = self.branches.lock();
        if let Some(branch) = branches.get(name) {
            return Ok(branch.clone());
        }
        let head = refs.try_get(name)?;
        if head.is_none() && name != MAIN {
            return Err(Error::NoBranch(name.to_string()));
        }
        let state = Arc::new(self.state_of(head.as_deref())?);
        let branch = Arc::new(Branch {
            name: name.to_string(),
            state: ArcSwap::new(state.clone()),
            tip: ArcSwap::new(state),
            feed: Arc::new(Feed::new()),
            retired: AtomicBool::new(false),
        });
        branches.insert(name.to_string(), branch.clone());
        Ok(branch)
    }

    fn stage(
        &self,
        tip: &mut RefState,
        refs: Refs,
        mut txn: Txn,
        branch: Option<&Arc<Branch<T>>>,
    ) -> Result<()> {
        let ptr = write_value(&refs, &mut txn)?;
        let frame = txn.write(ptr)?;
        let loaded = self.load_tip(ptr, branch);
        let (refs, update) = loaded.inspect_err(|_| self.store.poison())?;
        tip.refs = refs;
        tip.ptr = Some(ptr);
        let refs = tip.clone();
        self.queue.lock().waiting.push(Pending { frame, refs, update });
        Ok(())
    }

    fn load_tip(&self, ptr: Ptr, branch: Option<&Arc<Branch<T>>>) -> Result<(Refs, Update<T>)> {
        let refs = (*self.store.load::<Refs>(ptr)?).clone();
        let Some(branch) = branch else {
            return Ok((refs, None));
        };
        let head = refs.try_get(&branch.name)?;
        let state = Arc::new(self.state_of(head.as_deref())?);
        branch.tip.store(state.clone());
        Ok((refs, Some((branch.clone(), state))))
    }

    fn settle(&self, ticket: u64) -> Result<()> {
        let mut queue = self.queue.lock();
        loop {
            if queue.settled >= ticket {
                return Ok(());
            }
            if queue.failed {
                return Err(Error::Poisoned);
            }
            if !queue.syncing {
                if queue.waiting.is_empty() {
                    return Ok(());
                }
                return self.lead(queue);
            }
            self.done.wait(&mut queue);
        }
    }

    fn lead(&self, mut queue: MutexGuard<'_, Queue<T>>) -> Result<()> {
        queue.syncing = true;
        let batch = mem::take(&mut queue.waiting);
        drop(queue);
        let last = batch.last().expect("a leader has queued commits");
        let (frame, refs) = (last.frame, last.refs.clone());
        let outcome = self.store.sync(frame);
        if outcome.is_ok() {
            *self.published.lock() = refs;
            for pending in batch {
                if let Some((branch, state)) = pending.update {
                    branch.publish(state);
                }
            }
        }
        let mut queue = self.queue.lock();
        queue.syncing = false;
        match outcome {
            Ok(()) => queue.settled = frame.seq,
            Err(_) => queue.failed = true,
        }
        drop(queue);
        self.done.notify_all();
        let grown = frame.end.saturating_sub(self.signal.grown.load(Ordering::Relaxed));
        let threshold = self.signal.threshold.load(Ordering::Relaxed);
        if self.automatic && grown >= threshold.max(self.reclaim.every_bytes) {
            self.signal.raise();
        }
        outcome
    }

    fn step(&self, name: &'static str) {
        if let Some(hook) = &self.hook {
            (hook.0)(name);
        }
    }

    fn spawn(shared: &Arc<Self>) -> JoinHandle<()> {
        let weak = Arc::downgrade(shared);
        let signal = shared.signal.clone();
        thread::spawn(move || {
            while signal.wait() {
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                let cycle = AssertUnwindSafe(|| shared.compact(shared.history, Some(&signal.stop)));
                if panic::catch_unwind(cycle).is_err() {
                    shared.log.lock().last_error = Some("reclamation panicked".to_string());
                }
            }
        })
    }

    fn prune(&self, keep: Keep) -> Result<u64> {
        if keep == Keep::All {
            return Ok(0);
        }
        let mut tip = self.tip.lock();
        let mut next = tip.refs.clone();
        let mut dropped = 0;
        let heads: Vec<_> = tip.refs.iter().collect();
        for (name, head) in heads {
            let log = head.log.pruned(&self.store, keep)?;
            if log != head.log {
                dropped += (head.log.len() - log.len()) as u64;
                let mut head = (*head).clone();
                head.log = log;
                next.insert(name, head);
            }
        }
        if dropped == 0 {
            return Ok(0);
        }
        let txn = self.store.begin()?;
        self.stage(&mut tip, next, txn, None)?;
        self.settle(self.store.last_seq())?;
        let refs = tip.refs.clone();
        for branch in self.branches.lock().values() {
            if let Some(head) = refs.try_get(&branch.name)? {
                let state = branch.tip.load_full();
                let next = Arc::new(State {
                    snapshot: state.snapshot.clone(),
                    log: head.log,
                });
                branch.tip.store(next.clone());
                branch.state.store(next);
            }
        }
        Ok(dropped)
    }

    fn gap(&self, tip: &mut RefState, bytes: u64) -> Result<u64> {
        let head = tip.ptr.ok_or(Error::Detached)?;
        let (frame, start) = self.store.begin()?.write_gap(head, bytes)?;
        let refs = tip.clone();
        self.queue.lock().waiting.push(Pending {
            frame,
            refs,
            update: None,
        });
        Ok(start)
    }

    fn write_table(&self, pairs: &[(u64, u64)], tip: Option<&mut RefState>) -> Result<Ptr> {
        let (record, hash) = reclaim::table_record(pairs);
        let bytes = record.len() as u64;
        let offset = match tip {
            Some(tip) => self.gap(tip, bytes)?,
            None => self.gap(&mut self.tip.lock(), bytes)?,
        };
        self.store.write_at(&record, offset)?;
        Ok(Ptr { hash, offset })
    }

    fn compact(&self, keep: Keep, stop: Option<&AtomicBool>) -> Result<Compacted> {
        if self.store.is_read_only() {
            return Err(Error::ReadOnly);
        }
        let _only = self.cycle.lock();
        let outcome = self.run_cycle(keep, stop);
        let mut log = self.log.lock();
        log.cycles += 1;
        match &outcome {
            Ok(report) => {
                log.longest_pause = log.longest_pause.max(report.pause);
                log.last_error = None;
            }
            Err(Error::Interrupted) => {}
            Err(error) => log.last_error = Some(error.to_string()),
        }
        drop(log);
        self.signal.grown.store(self.store.written(), Ordering::Relaxed);
        outcome
    }

    fn run_cycle(&self, keep: Keep, stop: Option<&AtomicBool>) -> Result<Compacted> {
        let store = &self.store;
        let mut report = Compacted {
            before_bytes: store.allocated()?,
            commits_dropped: self.prune(keep)?,
            ..Compacted::default()
        };
        let tip = self.tip.lock();
        self.settle(store.last_seq())?;
        let Some(head) = tip.ptr else {
            report.after_bytes = report.before_bytes;
            return Ok(report);
        };
        let refs = tip.refs.clone();
        let end = store.written();
        let pinned = self.pins.lock().roots();
        drop(tip);
        let current = Roots::of(head, &refs)?;
        let mut marks = Marks::new(end);
        let block = store.block_bytes()?;
        let mut found = marks.walk(store, &current, stop)?;
        found.extend(marks.walk(store, &pinned, stop)?);
        self.step("marked");
        marks.plan(found, self.reclaim.fill, block);
        let mut moved = mem::take(&mut marks.moved);
        moved.sort_unstable_by_key(|extent| extent.place);
        let bytes = moved.iter().map(|extent| u64::from(extent.len)).sum::<u64>();
        report.moved_bytes = bytes;
        if bytes > 0 {
            let mut tip = self.tip.lock();
            let start = self.gap(&mut tip, bytes)?;
            drop(tip);
            reclaim::copy_to(store, &mut moved, start, stop)?;
        }
        marks.moved = moved;
        self.step("copied");
        let extras = marks.rescan(store)?;
        if !extras.is_empty() {
            self.relocate(&mut marks, extras, &mut report)?;
        }
        store.sync_data()?;
        let pairs = marks.places();
        let mut table = self.write_table(&pairs, None)?;
        let mut places = Places::new(Moved::default(), pairs);
        let mut live = marks.live_bytes();
        self.settle(store.last_seq())?;
        store.sync_data()?;
        self.step("tabled");
        let paused = Instant::now();
        let mut tip = self.tip.lock();
        let extras = marks.rescan(store)?;
        if !extras.is_empty() {
            self.relocate_locked(&mut tip, &mut marks, extras, &mut report)?;
            let pairs = marks.places();
            table = self.write_table(&pairs, Some(&mut tip))?;
            places = Places::new(Moved::default(), pairs);
            live = marks.live_bytes();
        }
        let head = tip.ptr.ok_or(Error::Detached)?;
        let frame = store.begin()?.write(head)?;
        let refs = tip.clone();
        self.queue.lock().waiting.push(Pending {
            frame,
            refs,
            update: None,
        });
        self.settle(frame.seq)?;
        let before = store.places().moved;
        let grown = live.saturating_sub(before.live) as f64;
        places.moved = Moved {
            floor: end,
            table: Some(table),
            live,
            survival: (grown / (end - before.floor).max(1) as f64 * SURVIVAL_ONE) as u64,
        };
        store.publish(frame, places)?;
        drop(tip);
        report.pause = paused.elapsed();
        report.objects = marks.objects();
        report.live_bytes = live;
        self.step("published");
        self.signal.threshold.store(live, Ordering::Relaxed);
        report.punched_bytes = marks.punch(store)?;
        self.step("punched");
        report.after_bytes = store.allocated()?;
        Ok(report)
    }

    fn relocate(
        &self,
        marks: &mut Marks,
        extras: Vec<reclaim::Extent>,
        report: &mut Compacted,
    ) -> Result<()> {
        let mut tip = self.tip.lock();
        self.relocate_locked(&mut tip, marks, extras, report)
    }

    fn relocate_locked(
        &self,
        tip: &mut RefState,
        marks: &mut Marks,
        mut extras: Vec<reclaim::Extent>,
        report: &mut Compacted,
    ) -> Result<()> {
        let bytes = extras.iter().map(|extent| u64::from(extent.len)).sum::<u64>();
        report.moved_bytes += bytes;
        let start = self.gap(tip, bytes)?;
        reclaim::copy_to(&self.store, &mut extras, start, None)?;
        marks.moved.append(&mut extras);
        Ok(())
    }
}

impl<T: Data + Default> Db<T> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Options::new().open(path)
    }
}

impl<T: Data> Db<T> {
    pub fn open_with(path: impl AsRef<Path>, init: impl FnOnce() -> T) -> Result<Self> {
        Options::new().open_with(path, init)
    }

    pub fn path(&self) -> PathBuf {
        self.shared.store.path().to_path_buf()
    }

    pub fn size_on_disk(&self) -> u64 {
        self.shared.store.on_disk()
    }

    pub fn space(&self) -> Result<Space> {
        let store = &self.shared.store;
        let log_bytes = store.size();
        let moved = store.places().moved;
        let live_bytes = if moved.floor == 0 {
            log_bytes
        } else {
            let survived = moved.survival as f64 / SURVIVAL_ONE;
            moved.live + (log_bytes.saturating_sub(moved.floor) as f64 * survived) as u64
        };
        let log = self.shared.log.lock();
        Ok(Space {
            log_bytes,
            allocated_bytes: store.allocated()?,
            live_bytes,
            cycles: log.cycles,
            longest_pause: log.longest_pause,
            last_error: log.last_error.clone(),
        })
    }

    pub fn compact(&self, keep: Keep) -> Result<Compacted> {
        self.shared.compact(keep, None)
    }

    #[doc(hidden)]
    pub fn size_log(&self) -> u64 {
        self.shared.store.size()
    }

    #[doc(hidden)]
    pub fn trace_op(&self, op: crate::store::Op) {
        self.shared.store.trace(op);
    }

    pub fn branch_name(&self) -> &str {
        &self.branch.name
    }

    pub fn read(&self) -> Snapshot<T> {
        self.branch.state.load().snapshot.clone()
    }

    pub fn version(&self) -> u64 {
        self.branch.state.load().snapshot.commit.version
    }

    pub fn head_hash(&self) -> Option<Hash> {
        self.branch.state.load().log.tip.map(|ptr| ptr.hash)
    }

    pub fn log(&self) -> Log {
        let (chain, pin) = self.pinned_chain();
        Log {
            chain,
            store: self.shared.store.clone(),
            pin,
        }
    }

    fn pinned_chain(&self) -> (Chain, Arc<Held>) {
        loop {
            let chain = self.branch.state.load().log;
            let pin = self.shared.hold(self.shared.plain.clone(), Some(chain));
            if self.branch.state.load().log.first <= chain.first {
                return (chain, pin);
            }
        }
    }

    pub fn update<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R> {
        self.commit("", f)
    }

    pub fn commit<R>(&self, message: impl Into<String>, f: impl FnOnce(&mut T) -> R) -> Result<R> {
        let message = message.into();
        let done = self.transact::<R, Error>(Intent::default(), |root| Ok((message, f(root))))?;
        Ok(done.value)
    }

    pub fn commit_with<R, S: Into<String>>(
        &self,
        f: impl FnOnce(&mut T) -> (S, R),
    ) -> Result<Committed<R>> {
        self.transact::<R, Error>(Intent::default(), |root| {
            let (message, value) = f(root);
            Ok((message.into(), value))
        })
    }

    pub fn try_update<R, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        self.try_commit("", f)
    }

    pub fn try_commit<R, E: From<Error>>(
        &self,
        message: impl Into<String>,
        f: impl FnOnce(&mut T) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        let message = message.into();
        let done = self.transact::<R, E>(Intent::default(), |root| {
            f(root).map(|value| (message, value))
        })?;
        Ok(done.value)
    }

    pub fn try_commit_with<R, S: Into<String>, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut T) -> std::result::Result<(S, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        self.transact(Intent::default(), |root| {
            let (message, value) = f(root)?;
            Ok((message.into(), value))
        })
    }

    fn transact<R, E: From<Error>>(
        &self,
        intent: Intent<T>,
        f: impl FnOnce(&mut T) -> std::result::Result<(String, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        let mut tip = self.shared.tip.lock();
        self.branch.check_live()?;
        if let Some((other, _)) = &intent.merged {
            other.check_live()?;
        }
        let state = self.branch.tip.load_full();
        let mut working = (*state.snapshot.value).clone();
        let before = state.snapshot.commit.version;
        let result = f(&mut working).and_then(|(message, value)| {
            let version = self.commit_locked(&mut tip, &state, message, intent, &working)?;
            Ok((value, version))
        });
        let ticket = self.shared.store.last_seq();
        drop(tip);
        self.shared.settle(ticket)?;
        let (value, version) = result?;
        Ok(Committed {
            value,
            version,
            changed: version != before,
        })
    }

    fn commit_locked(
        &self,
        tip: &mut RefState,
        state: &State<T>,
        message: String,
        intent: Intent<T>,
        working: &T,
    ) -> Result<u64> {
        let store = &self.shared.store;
        let mut txn = store.begin()?;
        let root = write_root(working, &mut txn, store)?;
        let previous = state.snapshot.commit.root.map(|ptr| ptr.hash);
        let version = state.snapshot.commit.version + 1;
        let mut merged = tip
            .refs
            .try_get(&self.branch.name)?
            .map(|head| head.merged.clone())
            .unwrap_or_default();
        let recorded = match intent.merged {
            Some((branch, theirs))
                if merged.try_get(&branch.name)?.map(|seen| seen.0) != Some(theirs) =>
            {
                merged.insert(branch.name.clone(), (theirs, version));
                true
            }
            _ => false,
        };
        if !recorded && previous.unwrap_or(self.shared.genesis_hash) == root.hash {
            return Ok(state.snapshot.commit.version);
        }
        let commit = Commit {
            version,
            time_ms: now_ms().max(state.snapshot.commit.time_ms),
            message,
            root: Some(root),
            restored: intent.restored,
        };
        let log = state.log.append(store, &mut txn, commit)?;
        let log = match self.shared.history {
            Keep::Last(_) => log.pruned(store, self.shared.history)?,
            _ => log,
        };
        let mut next = tip.refs.clone();
        next.insert(
            self.branch.name.clone(),
            Head {
                version,
                root: Some(root),
                log,
                merged,
            },
        );
        self.shared.stage(tip, next, txn, Some(&self.branch))?;
        Ok(version)
    }

    pub fn at(&self, version: u64) -> Result<Snapshot<T>> {
        if version == 0 {
            return Ok(Snapshot {
                value: self.shared.genesis.clone(),
                commit: self.shared.hold(self.shared.plain.clone(), None),
            });
        }
        let chain = self.branch.state.load().log;
        let entry = match chain.get(&self.shared.store, version) {
            Err(Error::Pruned(_)) => return Err(Error::NoVersion(version)),
            found => found?.ok_or(Error::NoVersion(version))?,
        };
        self.snapshot_of(&entry.commit)
    }

    fn snapshot_of(&self, commit: &Arc<Commit>) -> Result<Snapshot<T>> {
        let root = commit.root.ok_or(Error::NoVersion(commit.version))?;
        let held = self.shared.hold(commit.clone(), None);
        if commit.version < self.branch.state.load().log.first {
            return Err(Error::NoVersion(commit.version));
        }
        Ok(Snapshot {
            value: self.shared.store.load::<T>(root)?,
            commit: held,
        })
    }

    pub fn as_of(&self, time_ms: u64) -> Result<Snapshot<T>> {
        let chain = self.branch.state.load().log;
        match chain.at_time(&self.shared.store, time_ms) {
            Ok(Some(entry)) => self.snapshot_of(&entry.commit),
            Ok(None) if chain.first > 1 => Err(Error::NoVersion(chain.first - 1)),
            Ok(None) => self.at(0),
            Err(Error::Pruned(_)) => Err(Error::NoVersion(chain.first.saturating_sub(1))),
            Err(error) => Err(error),
        }
    }

    pub fn restore(&self, snapshot: &Snapshot<T>) -> Result<Committed<()>> {
        let message = format!("restore v{}", snapshot.commit.version);
        self.restore_with(snapshot, message)
    }

    pub fn restore_with(
        &self,
        snapshot: &Snapshot<T>,
        message: impl Into<String>,
    ) -> Result<Committed<()>> {
        let value = (*snapshot.value).clone();
        let message = message.into();
        let intent = Intent {
            restored: Some(snapshot.commit.version),
            merged: None,
        };
        self.transact::<(), Error>(intent, move |root| {
            *root = value;
            Ok((message, ()))
        })
    }

    pub fn history_of<V, F>(&self, project: F) -> History<T, V, F>
    where
        V: PartialEq,
        F: Fn(&T) -> V,
    {
        let (chain, pin) = self.pinned_chain();
        let genesis = (chain.first == 1).then(|| self.shared.genesis.clone());
        History::new(chain.walk(&self.shared.store), self.shared.store.clone(), genesis, project, pin)
    }

    pub fn merge_with<R, S: Into<String>>(
        &self,
        other: &Db<T>,
        f: impl FnOnce(&mut T, &Snapshot<T>, &Snapshot<T>) -> (S, R),
    ) -> Result<Committed<R>> {
        let base = self.common_ancestor(other)?;
        let theirs = other.read();
        let advanced = base.commit.root.map(|ptr| ptr.hash) != theirs.commit.root.map(|ptr| ptr.hash);
        let intent = Intent {
            restored: None,
            merged: (advanced && !Arc::ptr_eq(&self.branch, &other.branch))
                .then(|| (other.branch.clone(), theirs.version())),
        };
        self.transact::<R, Error>(intent, |root| {
            let (message, value) = f(root, &base, &theirs);
            Ok((message.into(), value))
        })
    }

    pub fn common_ancestor(&self, other: &Db<T>) -> Result<Snapshot<T>> {
        let recorded = |db: &Db<T>, from: &Db<T>| -> Result<Option<(Snapshot<T>, u64)>> {
            let head = db.shared.published.lock().refs.try_get(&db.branch.name)?;
            let seen = match head {
                Some(head) => head.merged.try_get(&from.branch.name)?,
                None => None,
            };
            let Some(seen) = seen else {
                return Ok(None);
            };
            match from.at(seen.0) {
                Ok(snapshot) => Ok(Some((snapshot, seen.1))),
                Err(Error::NoVersion(_)) => Ok(None),
                Err(error) => Err(error),
            }
        };
        match (recorded(self, other)?, recorded(other, self)?) {
            (Some((mine, merged_at)), Some((yours, _))) => {
                return Ok(if merged_at <= yours.version() { yours } else { mine });
            }
            (Some((snapshot, _)), None) | (None, Some((snapshot, _))) => return Ok(snapshot),
            (None, None) => {}
        }
        let (ours, theirs) = (self.branch.state.load().log, other.branch.state.load().log);
        if ours.tip.is_none() || theirs.tip.is_none() {
            return self.at(0);
        }
        let start = ours.first.max(theirs.first);
        let same = |version: &u64| -> Result<bool> {
            let (a, b) = (
                ours.get(&self.shared.store, *version)?,
                theirs.get(&other.shared.store, *version)?,
            );
            Ok(matches!((a, b), (Some(a), Some(b)) if a.commit == b.commit))
        };
        if !same(&start)? {
            return if start == 1 {
                self.at(0)
            } else {
                Err(Error::NoCommonAncestor)
            };
        }
        let mut low = start;
        let mut high = self.version().min(other.version());
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            if same(&middle)? {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        self.at(low).map_err(|error| match error {
            Error::NoVersion(_) => Error::NoCommonAncestor,
            other => other,
        })
    }

    pub fn fork(&self, name: &str) -> Result<Db<T>> {
        let mut tip = self.shared.tip.lock();
        self.branch.check_live()?;
        if name == MAIN || tip.refs.try_get(name)?.is_some() {
            return Err(Error::BranchExists(name.to_string()));
        }
        let state = self.branch.tip.load_full();
        let commit = &state.snapshot.commit;
        let merged = tip
            .refs
            .try_get(&self.branch.name)?
            .map(|head| head.merged.clone())
            .unwrap_or_default();
        let mut next = tip.refs.clone();
        next.insert(
            name.to_string(),
            Head {
                version: commit.version,
                root: commit.root,
                log: state.log,
                merged,
            },
        );
        let txn = self.shared.store.begin()?;
        self.shared.stage(&mut tip, next, txn, None)?;
        let ticket = self.shared.store.last_seq();
        drop(tip);
        self.shared.settle(ticket)?;
        self.branch(name)
    }

    pub fn branch(&self, name: &str) -> Result<Db<T>> {
        let refs = self.shared.published.lock();
        let branch = self.shared.attach(name, &refs.refs)?;
        Ok(Db {
            shared: self.shared.clone(),
            branch,
            keeper: self.keeper.clone(),
        })
    }

    pub fn drop_branch(&self, name: &str) -> Result<()> {
        if name == MAIN || name == self.branch.name {
            return Err(Error::BranchInUse(name.to_string()));
        }
        let mut tip = self.shared.tip.lock();
        if tip.refs.try_get(name)?.is_none() {
            return Err(Error::NoBranch(name.to_string()));
        }
        let txn = self.shared.store.begin()?;
        let mut next = tip.refs.clone();
        next.remove(name);
        let branches: Vec<_> = next.iter().collect();
        for (branch, head) in branches {
            if head.merged.try_get(name)?.is_some() {
                let mut head = (*head).clone();
                head.merged.remove(name);
                next.insert(branch, head);
            }
        }
        self.shared.stage(&mut tip, next, txn, None)?;
        let ticket = self.shared.store.last_seq();
        self.shared.settle(ticket)?;
        let published = self.shared.published.lock();
        let retired = self.shared.branches.lock().remove(name);
        drop(published);
        if let Some(branch) = &retired {
            branch.retire();
        }
        drop(tip);
        Ok(())
    }

    pub fn branches(&self) -> Vec<String> {
        let refs = self.shared.published.lock();
        let mut names: Vec<String> = refs.refs.keys().collect();
        if !names.iter().any(|name| name == MAIN) {
            names.push(MAIN.to_string());
            names.sort();
        }
        names
    }

    pub fn watch(&self) -> Watcher<T> {
        Watcher::new(self.branch.feed.clone())
    }

    pub fn refresh(&self) -> Result<bool> {
        if !self.shared.store.is_read_only() {
            return Ok(false);
        }
        let mut last = Ok(false);
        for _ in 0..READ_RETRIES {
            last = self.refresh_once();
            if !matches!(last, Err(Error::Pruned(_))) {
                break;
            }
        }
        last
    }

    fn refresh_once(&self) -> Result<bool> {
        let mut tip = self.shared.tip.lock();
        let head = self.shared.store.refresh()?;
        if head == tip.ptr {
            return Ok(false);
        }
        let Some(ptr) = head else {
            return Ok(false);
        };
        tip.refs = (*self.shared.store.load::<Refs>(ptr)?).clone();
        tip.ptr = Some(ptr);
        *self.shared.published.lock() = tip.clone();
        let mut branches = self.shared.branches.lock();
        let mut gone = Vec::new();
        for branch in branches.values() {
            let head = tip.refs.try_get(&branch.name)?;
            if head.is_none() && branch.name != MAIN {
                gone.push(branch.name.clone());
                continue;
            }
            let state = Arc::new(self.shared.state_of(head.as_deref())?);
            branch.tip.store(state.clone());
            if state.snapshot.commit == branch.state.load().snapshot.commit {
                branch.state.store(state);
            } else {
                branch.publish(state);
            }
        }
        for name in gone {
            if let Some(branch) = branches.remove(&name) {
                branch.retire();
            }
        }
        Ok(true)
    }

    pub fn verify(&self) -> Result<Verified> {
        let mut last = Ok(Verified::default());
        for _ in 0..READ_RETRIES {
            let head = self.shared.published.lock().ptr;
            last = crate::admin::verified(&self.shared.store, head);
            if !matches!(last, Err(Error::Pruned(_))) {
                break;
            }
        }
        last
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Verified {
    pub objects: u64,
    pub bytes: u64,
    pub file_bytes: u64,
}
