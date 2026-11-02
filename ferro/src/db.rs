use std::collections::HashMap;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use parking_lot::Mutex;

use crate::codec::{Data, Decoder, Encoder, Hash, HashSink, Ptr, Sink};
use crate::error::{Error, Result};
use crate::map::Map;
use crate::store::{Durability, Store, StoreOptions, Txn};

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
    pub(crate) log: Map<u64, Commit>,
    pub(crate) merged: Map<String, (u64, u64)>,
}

impl Data for Head {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        self.version.encode(w)?;
        self.root.encode(w)?;
        self.log.encode(w)?;
        self.merged.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        Ok(Head {
            version: u64::decode(r)?,
            root: Option::<Ptr>::decode(r)?,
            log: Map::decode(r)?,
            merged: Map::decode(r)?,
        })
    }
}

pub(crate) type Refs = Map<String, Head>;

struct RefState {
    refs: Refs,
    ptr: Option<Ptr>,
}

pub struct Snapshot<T> {
    value: Arc<T>,
    commit: Commit,
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
    log: Map<u64, Commit>,
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
    watchers: Mutex<Vec<Sender<Snapshot<T>>>>,
}

impl<T> Branch<T> {
    fn publish(&self, state: State<T>) {
        let snapshot = state.snapshot.clone();
        self.state.store(Arc::new(state));
        self.watchers
            .lock()
            .retain(|watcher| watcher.send(snapshot.clone()).is_ok());
    }
}

struct Shared<T> {
    store: Store,
    refs: Mutex<RefState>,
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
        let shared = Arc::new(Shared {
            store,
            refs: Mutex::new(RefState { refs, ptr: head }),
            genesis,
            genesis_hash,
            branches: Mutex::new(HashMap::new()),
        });
        let branch = {
            let state = shared.refs.lock();
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
                    commit: Commit::genesis(),
                },
                log: Map::new(),
                merged: Map::new(),
            });
        };
        let value = match head.root {
            Some(ptr) => self.store.load::<T>(ptr)?,
            None => self.genesis.clone(),
        };
        let commit = head
            .log
            .try_get(&head.version)?
            .map(|commit| (*commit).clone())
            .unwrap_or_else(Commit::genesis);
        Ok(State {
            snapshot: Snapshot { value, commit },
            log: head.log.clone(),
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
        let state = self.state_of(head.as_deref())?;
        let branch = Arc::new(Branch {
            name: name.to_string(),
            state: ArcSwap::from_pointee(state),
            watchers: Mutex::new(Vec::new()),
        });
        branches.insert(name.to_string(), branch.clone());
        Ok(branch)
    }

    fn write_refs(&self, state: &mut RefState, refs: Refs, mut txn: Txn) -> Result<()> {
        let ptr = write_value(&refs, &mut txn)?;
        txn.commit(ptr)?;
        state.refs = (*self.store.load::<Refs>(ptr)?).clone();
        state.ptr = Some(ptr);
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

    pub fn log(&self) -> Map<u64, Commit> {
        self.branch.state.load().log.clone()
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
        let mut refs = self.shared.refs.lock();
        let state = self.branch.state.load_full();
        let mut working = (*state.snapshot.value).clone();
        let (message, value) = f(&mut working)?;
        let before = state.snapshot.commit.version;
        let version = self.commit_locked(&mut refs, &state, message, intent, &working)?;
        Ok(Committed {
            value,
            version,
            changed: version != before,
        })
    }

    fn commit_locked(
        &self,
        refs: &mut RefState,
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
        let mut log = state.log.clone();
        log.insert(version, commit);
        let mut next = refs.refs.clone();
        next.insert(
            self.branch.name.clone(),
            Head {
                version,
                root: Some(root),
                log,
                merged,
            },
        );
        self.shared.write_refs(refs, next, txn)?;
        let head = refs.refs.try_get(&self.branch.name)?;
        self.branch.publish(self.shared.state_of(head.as_deref())?);
        Ok(version)
    }

    pub fn at(&self, version: u64) -> Result<Snapshot<T>> {
        if version == 0 {
            return Ok(Snapshot {
                value: self.shared.genesis.clone(),
                commit: Commit::genesis(),
            });
        }
        let log = self.log();
        let commit = log.try_get(&version)?.ok_or(Error::NoVersion(version))?;
        let root = commit.root.ok_or(Error::NoVersion(version))?;
        Ok(Snapshot {
            value: self.shared.store.load::<T>(root)?,
            commit: (*commit).clone(),
        })
    }

    pub fn as_of(&self, time_ms: u64) -> Result<Snapshot<T>> {
        let log = self.log();
        let mut low = 0usize;
        let mut high = log.len();
        while low < high {
            let middle = low + (high - low) / 2;
            match log.nth(middle) {
                Some((_, commit)) if commit.time_ms <= time_ms => low = middle + 1,
                _ => high = middle,
            }
        }
        match low.checked_sub(1).and_then(|index| log.nth(index)) {
            Some((version, _)) => self.at(version),
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

    pub fn history_of<V: PartialEq + Clone>(
        &self,
        project: impl Fn(&T) -> V,
    ) -> Result<Vec<(Commit, V)>> {
        let log = self.log();
        let mut previous = None;
        if log.first().is_some_and(|(version, _)| version == 1) {
            previous = Some(project(&self.shared.genesis));
        }
        let mut changes = Vec::new();
        for (version, commit) in log.iter() {
            let value = project(&*self.at(version)?);
            if previous.as_ref() != Some(&value) {
                changes.push(((*commit).clone(), value.clone()));
                previous = Some(value);
            }
        }
        Ok(changes)
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
        let (ours, theirs) = (self.log(), other.log());
        let (Some((first_a, _)), Some((first_b, _))) = (ours.first(), theirs.first()) else {
            return self.at(0);
        };
        let start = first_a.max(first_b);
        let same = |version: &u64| {
            matches!(
                (ours.get(version), theirs.get(version)),
                (Some(a), Some(b)) if a == b
            )
        };
        if !same(&start) {
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
            if same(&middle) {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        self.at(low)
    }

    pub fn fork(&self, name: &str) -> Result<Db<T>> {
        let mut refs = self.shared.refs.lock();
        if name == MAIN || refs.refs.try_get(name)?.is_some() {
            return Err(Error::BranchExists(name.to_string()));
        }
        let state = self.branch.state.load_full();
        let commit = &state.snapshot.commit;
        let mut next = refs.refs.clone();
        next.insert(
            name.to_string(),
            Head {
                version: commit.version,
                root: commit.root,
                log: state.log.clone(),
                merged: state.merged.clone(),
            },
        );
        let txn = self.shared.store.begin()?;
        self.shared.write_refs(&mut refs, next, txn)?;
        let branch = self.shared.attach(name, &refs.refs)?;
        Ok(Db {
            shared: self.shared.clone(),
            branch,
        })
    }

    pub fn branch(&self, name: &str) -> Result<Db<T>> {
        let refs = self.shared.refs.lock();
        let branch = self.shared.attach(name, &refs.refs)?;
        Ok(Db {
            shared: self.shared.clone(),
            branch,
        })
    }

    pub fn branches(&self) -> Vec<String> {
        let refs = self.shared.refs.lock();
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
        let mut refs = self.shared.refs.lock();
        let head = self.shared.store.refresh()?;
        if head == refs.ptr {
            return Ok(false);
        }
        let Some(ptr) = head else {
            return Ok(false);
        };
        refs.refs = (*self.shared.store.load::<Refs>(ptr)?).clone();
        refs.ptr = Some(ptr);
        let branches = self.shared.branches.lock();
        for branch in branches.values() {
            let head = refs.refs.try_get(&branch.name)?;
            let state = self.shared.state_of(head.as_deref())?;
            if state.snapshot.version() == branch.state.load().snapshot.version() {
                branch.state.store(Arc::new(state));
            } else {
                branch.publish(state);
            }
        }
        Ok(true)
    }

    pub fn verify(&self) -> Result<Verified> {
        let refs = self.shared.refs.lock();
        let mut report = Verified::default();
        if let Some(ptr) = refs.ptr {
            self.shared.store.walk(ptr, |_, object| {
                report.objects += 1;
                report.bytes += (object.body.len() + object.links.len() * 40 + 8) as u64;
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
