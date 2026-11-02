use std::collections::HashMap;
use std::mem;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::codec::{Data, Decoder, Encoder, Hash, HashSink, Ptr, Sink};
use crate::error::{Error, Result};
use crate::history::History;
use crate::log::{Chain, Log};
use crate::map::Map;
use crate::store::{Durability, Frame, Store, StoreOptions, Txn};

const MAIN: &str = "main";

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

pub struct Snapshot<T> {
    value: Arc<T>,
    commit: Arc<Commit>,
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

impl<T> Snapshot<T> {
    pub fn version(&self) -> u64 {
        self.commit.version
    }

    pub fn commit(&self) -> &Commit {
        &self.commit
    }
}

struct State<T> {
    snapshot: Snapshot<T>,
    log: Chain,
    merged: Map<String, (u64, u64)>,
}

#[derive(Default)]
struct Intent {
    restored: Option<u64>,
    merged: Option<(String, u64)>,
}

struct Branch<T> {
    name: String,
    state: ArcSwap<State<T>>,
    tip: ArcSwap<State<T>>,
    watchers: Mutex<Vec<Sender<Snapshot<T>>>>,
}

impl<T> Branch<T> {
    fn publish(&self, state: Arc<State<T>>) {
        let snapshot = state.snapshot.clone();
        self.state.store(state);
        self.watchers
            .lock()
            .retain(|watcher| watcher.send(snapshot.clone()).is_ok());
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
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        self.store.close();
    }
}

pub struct Db<T> {
    shared: Arc<Shared<T>>,
    branch: Arc<Branch<T>>,
}

impl<T> Clone for Db<T> {
    fn clone(&self) -> Self {
        Db {
            shared: self.shared.clone(),
            branch: self.branch.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    store: StoreOptions,
}

impl Options {
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

    pub fn open<T: Data + Default>(self, path: impl AsRef<Path>) -> Result<Db<T>> {
        self.open_with(path, T::default)
    }

    pub fn open_with<T: Data>(
        self,
        path: impl AsRef<Path>,
        init: impl FnOnce() -> T,
    ) -> Result<Db<T>> {
        let (store, head) = Store::open(path.as_ref(), self.store)?;
        let genesis = Arc::new(init());
        let genesis_hash = value_hash(&*genesis)?;
        let refs = match head {
            Some(ptr) => (*store.load::<Refs>(ptr)?).clone(),
            None => Map::new(),
        };
        let settled = store.last_seq();
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
        });
        let branch = {
            let state = shared.published.lock();
            shared.attach(MAIN, &state.refs)?
        };
        Ok(Db { shared, branch })
    }
}

fn value_hash<D: Data>(value: &D) -> Result<Hash> {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder)?;
    Ok(encoder.finish()?.hash)
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

impl<T: Data> Shared<T> {
    fn state_of(&self, head: Option<&Head>) -> Result<State<T>> {
        let Some(head) = head else {
            return Ok(State {
                snapshot: Snapshot {
                    value: self.genesis.clone(),
                    commit: Arc::new(Commit::genesis()),
                },
                log: Chain::empty(),
                merged: Map::new(),
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
            .unwrap_or_else(|| Arc::new(Commit::genesis()));
        Ok(State {
            snapshot: Snapshot { value, commit },
            log: head.log,
            merged: head.merged.clone(),
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
            watchers: Mutex::new(Vec::new()),
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
        outcome
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
        self.shared.store.size()
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

    pub fn log(&self) -> Log {
        Log {
            chain: self.branch.state.load().log,
            store: self.shared.store.clone(),
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
        intent: Intent,
        f: impl FnOnce(&mut T) -> std::result::Result<(String, R), E>,
    ) -> std::result::Result<Committed<R>, E> {
        let mut tip = self.shared.tip.lock();
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
        intent: Intent,
        working: &T,
    ) -> Result<u64> {
        let store = &self.shared.store;
        let mut txn = store.begin()?;
        let root = write_root(working, &mut txn, store)?;
        let previous = state.snapshot.commit.root.map(|ptr| ptr.hash);
        let version = state.snapshot.commit.version + 1;
        let mut merged = state.merged.clone();
        let recorded = match intent.merged {
            Some((branch, theirs)) if merged.try_get(&branch)?.map(|seen| seen.0) != Some(theirs) => {
                merged.insert(branch, (theirs, version));
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
                commit: Arc::new(Commit::genesis()),
            });
        }
        let chain = self.branch.state.load().log;
        let entry = chain
            .get(&self.shared.store, version)?
            .ok_or(Error::NoVersion(version))?;
        self.snapshot_of(&entry.commit)
    }

    fn snapshot_of(&self, commit: &Arc<Commit>) -> Result<Snapshot<T>> {
        let root = commit.root.ok_or(Error::NoVersion(commit.version))?;
        Ok(Snapshot {
            value: self.shared.store.load::<T>(root)?,
            commit: commit.clone(),
        })
    }

    pub fn as_of(&self, time_ms: u64) -> Result<Snapshot<T>> {
        let chain = self.branch.state.load().log;
        match chain.at_time(&self.shared.store, time_ms)? {
            Some(entry) => self.snapshot_of(&entry.commit),
            None => self.at(0),
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
        let chain = self.branch.state.load().log;
        let genesis = (chain.first == 1).then(|| self.shared.genesis.clone());
        History::new(chain.walk(&self.shared.store), self.shared.store.clone(), genesis, project)
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
                .then(|| (other.branch.name.clone(), theirs.version())),
        };
        self.transact::<R, Error>(intent, |root| {
            let (message, value) = f(root, &base, &theirs);
            Ok((message.into(), value))
        })
    }

    pub fn common_ancestor(&self, other: &Db<T>) -> Result<Snapshot<T>> {
        let recorded = |db: &Db<T>, from: &Db<T>| -> Result<Option<(Snapshot<T>, u64)>> {
            let state = db.branch.state.load();
            let Some(seen) = state.merged.try_get(&from.branch.name)? else {
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
        self.at(low)
    }

    pub fn fork(&self, name: &str) -> Result<Db<T>> {
        let mut tip = self.shared.tip.lock();
        if name == MAIN || tip.refs.try_get(name)?.is_some() {
            return Err(Error::BranchExists(name.to_string()));
        }
        let state = self.branch.tip.load_full();
        let commit = &state.snapshot.commit;
        let mut next = tip.refs.clone();
        next.insert(
            name.to_string(),
            Head {
                version: commit.version,
                root: commit.root,
                log: state.log,
                merged: state.merged.clone(),
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
        })
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

    pub fn watch(&self) -> Receiver<Snapshot<T>> {
        let (sender, receiver) = channel();
        self.branch.watchers.lock().push(sender);
        receiver
    }

    pub fn refresh(&self) -> Result<bool> {
        if !self.shared.store.is_read_only() {
            return Ok(false);
        }
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
        let branches = self.shared.branches.lock();
        for branch in branches.values() {
            let head = tip.refs.try_get(&branch.name)?;
            let state = Arc::new(self.shared.state_of(head.as_deref())?);
            branch.tip.store(state.clone());
            if state.snapshot.version() == branch.state.load().snapshot.version() {
                branch.state.store(state);
            } else {
                branch.publish(state);
            }
        }
        Ok(true)
    }

    pub fn verify(&self) -> Result<Verified> {
        let head = self.shared.published.lock().ptr;
        let mut report = Verified::default();
        if let Some(ptr) = head {
            self.shared.store.walk(ptr, |ptr, object| {
                report.objects += 1;
                report.bytes += crate::store::record_len(ptr.offset, object) as u64;
            })?;
        }
        report.file_bytes = self.shared.store.size();
        Ok(report)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Verified {
    pub objects: u64,
    pub bytes: u64,
    pub file_bytes: u64,
}
