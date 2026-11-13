use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ferro::db::Hook;
use ferro::{
    Commit, Conflict, Data, Db, Durability, Error, Keep, Map, Options, Outcome, Reclaim, Record,
    Snapshot, Table,
};
use tempfile::TempDir;

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Task {
    #[key]
    id: u64,
    #[index]
    owner: Option<String>,
    #[index]
    state: String,
    #[index(each)]
    tags: Vec<String>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Board {
    tasks: Table<Task>,
}

fn task(id: u64, owner: Option<&str>, state: &str, tags: &[&str]) -> Task {
    Task {
        id,
        owner: owner.map(str::to_string),
        state: state.to_string(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
    }
}

fn open(dir: &tempfile::TempDir) -> Db<Board> {
    Db::open(dir.path().join("b.ferro")).unwrap()
}

#[test]
fn commit_with_takes_the_message_from_the_closure_and_reports_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let done = db
        .commit_with(|board| {
            let id = board.tasks.len() as u64 + 1;
            board.tasks.insert(task(id, None, "open", &[]));
            (format!("add #{id}"), id)
        })
        .unwrap();
    assert_eq!(done.value, 1);
    assert_eq!(done.version, 1);
    assert!(done.changed);
    assert_eq!(db.log().get(1).unwrap().message, "add #1");
    let same = db.commit_with(|_| ("nothing", ())).unwrap();
    assert!(!same.changed);
    assert_eq!(same.version, 1);
    assert_eq!(db.log().len(), 1);
}

#[test]
fn a_multi_line_message_is_one_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    {
        let db = Db::<Board>::open(&path).unwrap();
        db.commit("title\n\nbody line\n", |b| {
            b.tasks.insert(task(1, None, "open", &[]))
        })
        .unwrap();
        db.commit("", |b| b.tasks.insert(task(2, None, "open", &[])))
            .unwrap();
        assert_eq!(db.read().commit().messages().count(), 0);
    }
    let db = Db::<Board>::open(&path).unwrap();
    let log = db.log();
    let first = log.get(1).unwrap();
    assert_eq!(first.message, "title\n\nbody line\n");
    assert_eq!(
        first.messages().collect::<Vec<_>>(),
        vec!["title\n\nbody line\n"]
    );
    assert_eq!(log.get(2).unwrap().messages().count(), 0);
}

#[test]
fn restore_records_its_source_and_reports_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.update(|board| board.tasks.insert(task(1, None, "open", &[])))
        .unwrap();
    db.update(|board| board.tasks.insert(task(2, None, "open", &[])))
        .unwrap();
    let done = db.restore(&db.at(1).unwrap(), "restore").unwrap();
    assert!(done.changed);
    assert_eq!(done.version, 3);
    let log = db.log();
    let commit: std::sync::Arc<Commit> = log.get(3).unwrap();
    assert_eq!(commit.restored, Some(1));
    assert!(!db.restore(&db.at(1).unwrap(), "restore").unwrap().changed);
}

#[test]
fn index_counts_and_option_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.update(|board| {
        board
            .tasks
            .insert(task(1, Some("ann"), "open", &["a", "b"]));
        board.tasks.insert(task(2, Some("ann"), "done", &["b"]));
        board.tasks.insert(task(3, Some("bob"), "open", &[]));
        board.tasks.insert(task(4, None, "open", &["c"]));
    })
    .unwrap();
    let snapshot = db.read();
    assert_eq!(
        snapshot.tasks.by_state_counts(),
        vec![("done".to_string(), 1), ("open".to_string(), 3)]
    );
    assert_eq!(
        snapshot.tasks.by_tags_counts(),
        vec![
            ("a".to_string(), 1),
            ("b".to_string(), 2),
            ("c".to_string(), 1)
        ]
    );
    assert_eq!(
        snapshot.tasks.by_owner_counts(),
        vec![
            (None, 1),
            (Some("ann".to_string()), 2),
            (Some("bob".to_string()), 1)
        ]
    );
    let ids: Vec<u64> = snapshot.tasks.by_owner_some("ann").map(|t| t.id).collect();
    assert_eq!(ids, vec![1, 2]);
    assert_eq!(snapshot.tasks.by_owner_some("nobody").len(), 0);
    assert_eq!(snapshot.tasks.by_owner(&None).len(), 1);
}

#[test]
fn index_iterators_share_one_type() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.update(|board| board.tasks.insert(task(1, Some("ann"), "open", &["a"])))
        .unwrap();
    let snapshot = db.read();
    let mut sources = vec![
        snapshot.tasks.by_state("open"),
        snapshot.tasks.by_tags("a"),
        snapshot.tasks.by_owner_some("ann"),
    ];
    sources.sort_by_key(|source| source.len());
    assert_eq!(sources.remove(0).next().unwrap().id, 1);
}

#[test]
fn history_of_reports_only_versions_where_the_projection_changed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.commit("create", |b| b.tasks.insert(task(1, None, "open", &[])))
        .unwrap();
    db.commit("unrelated", |b| b.tasks.insert(task(2, None, "open", &[])))
        .unwrap();
    db.commit("close", |b| {
        b.tasks.get_mut(&1).unwrap().state = "done".to_string();
    })
    .unwrap();
    db.commit("gone", |b| b.tasks.remove(&1)).unwrap();
    let history: Vec<_> = db.history_of(|b| b.tasks.get(&1)).collect();
    let summary: Vec<(u64, &str, Option<String>)> = history
        .iter()
        .map(|(commit, row)| {
            (
                commit.version,
                commit.message.as_str(),
                row.as_ref().map(|row| row.state.clone()),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (4, "gone", None),
            (3, "close", Some("done".to_string())),
            (1, "create", Some("open".to_string())),
        ]
    );
}

#[test]
fn common_ancestor_is_the_fork_point() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.update(|b| b.tasks.insert(task(1, None, "open", &[])))
        .unwrap();
    db.update(|b| b.tasks.insert(task(2, None, "open", &[])))
        .unwrap();
    let other = db.fork("other").unwrap();
    assert_eq!(db.common_ancestor(&other).unwrap().version(), 2);
    other
        .update(|b| b.tasks.insert(task(3, None, "open", &[])))
        .unwrap();
    db.update(|b| b.tasks.insert(task(4, None, "open", &[])))
        .unwrap();
    db.update(|b| b.tasks.insert(task(5, None, "open", &[])))
        .unwrap();
    assert_eq!(db.common_ancestor(&other).unwrap().version(), 2);
    assert_eq!(other.common_ancestor(&db).unwrap().version(), 2);
}

#[test]
fn common_ancestor_from_genesis_and_after_pruning() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.ferro");
    {
        let db = Db::<Map<u64, u64>>::open(&path).unwrap();
        let other = db.fork("other").unwrap();
        db.update(|m| m.insert(1, 1)).unwrap();
        other.update(|m| m.insert(1, 2)).unwrap();
        assert_eq!(db.common_ancestor(&other).unwrap().version(), 0);
        for i in 0..10 {
            db.update(|m| m.insert(i, i)).unwrap();
            other.update(|m| m.insert(i, i + 1)).unwrap();
        }
    }
    ferro::admin::compact(&path, ferro::admin::Keep::Last(2)).unwrap();
    let db = Db::<Map<u64, u64>>::open(&path).unwrap();
    let other = db.branch("other").unwrap();
    assert!(matches!(
        db.common_ancestor(&other).err(),
        Some(Error::NoCommonAncestor)
    ));
}

#[test]
fn three_way_merge_applies_disjoint_changes_and_resolves_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let main = open(&dir);
    main.update(|b| {
        b.tasks.insert(task(1, None, "open", &[]));
        b.tasks.insert(task(2, None, "open", &[]));
        b.tasks.insert(task(3, None, "open", &[]));
        b.tasks.insert(task(4, None, "open", &[]));
    })
    .unwrap();
    let side = main.fork("side").unwrap();
    side.update(|b| {
        b.tasks.update(&1, |t| t.state = "side".to_string());
        b.tasks.remove(&2);
        b.tasks.insert(task(5, Some("sid"), "open", &["new"]));
        b.tasks.update(&3, |t| t.state = "both".to_string());
        b.tasks.update(&4, |t| t.state = "theirs".to_string());
    })
    .unwrap();
    main.update(|b| {
        b.tasks.update(&3, |t| t.state = "both".to_string());
        b.tasks.update(&4, |t| t.state = "ours".to_string());
    })
    .unwrap();
    let base = main.common_ancestor(&side).unwrap();
    let theirs = side.read();
    let mut conflicts: Vec<Conflict<u64, Task>> = Vec::new();
    let written = main
        .commit("merge side", |b| {
            b.tasks.merge(&base.tasks, &theirs.tasks, |conflict| {
                let keep = conflict.ours.as_deref().cloned();
                conflicts.push(conflict);
                keep
            })
        })
        .unwrap();
    assert_eq!(written, 4);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].key, 4);
    assert_eq!(conflicts[0].theirs.as_ref().unwrap().state, "theirs");
    let merged = main.read();
    assert_eq!(merged.tasks.get(&1).unwrap().state, "side");
    assert!(merged.tasks.get(&2).is_none());
    assert_eq!(merged.tasks.get(&3).unwrap().state, "both");
    assert_eq!(merged.tasks.get(&4).unwrap().state, "ours");
    assert_eq!(merged.tasks.by_owner_some("sid").len(), 1);
    assert_eq!(merged.tasks.by_tags("new").len(), 1);
    assert_eq!(merged.tasks.by_state("open").len(), 1);
}

#[test]
fn map_merge_matches_table_semantics_and_read_only_error_is_clear() {
    let dir = tempfile::tempdir().unwrap();
    let mut ours: Map<u64, u64> = [(1, 1), (2, 2)].into_iter().collect();
    let base = ours.clone();
    let mut theirs = base.clone();
    theirs.insert(3, 3);
    theirs.remove(&2);
    ours.insert(1, 9);
    assert_eq!(
        ours.merge(&base, &theirs, |_| panic!("no conflict expected")),
        2
    );
    assert_eq!(ours.len(), 2);
    assert_eq!(*ours.get(&1).unwrap(), 9);
    assert_eq!(*ours.get(&3).unwrap(), 3);
    let missing = ferro::Options::new()
        .read_only(true)
        .open::<Map<u64, u64>>(dir.path().join("nope.ferro"));
    assert!(matches!(missing.err(), Some(Error::NoDatabase(_))));
}

#[test]
fn merge_with_remembers_what_it_already_took() {
    let dir = tempfile::tempdir().unwrap();
    let main = open(&dir);
    main.update(|b| b.tasks.insert(task(1, None, "open", &[])))
        .unwrap();
    let side = main.fork("side").unwrap();
    side.update(|b| b.tasks.update(&1, |t| t.state = "side".to_string()))
        .unwrap();
    let take = |main: &Db<Board>, side: &Db<Board>| {
        main.merge_with(side, |b, base, theirs| {
            let written = b
                .tasks
                .merge(&base.tasks, &theirs.tasks, |c| c.ours.as_deref().cloned());
            ("merge", written)
        })
        .unwrap()
    };
    assert_eq!(take(&main, &side).value, 1);
    main.update(|b| b.tasks.update(&1, |t| t.state = "main".to_string()))
        .unwrap();
    let again = take(&main, &side);
    assert_eq!(again.value, 0);
    assert!(!again.changed);
    assert_eq!(main.read().tasks.get(&1).unwrap().state, "main");
    side.update(|b| b.tasks.insert(task(2, None, "open", &[])))
        .unwrap();
    assert_eq!(take(&main, &side).value, 1);
    assert_eq!(main.read().tasks.len(), 2);
    assert_eq!(main.read().tasks.get(&1).unwrap().state, "main");
}

#[test]
fn merge_keeping_only_ours_still_records_progress_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("k.ferro");
    {
        let main = Db::<Board>::open(&path).unwrap();
        main.update(|b| b.tasks.insert(task(1, None, "open", &[])))
            .unwrap();
        let side = main.fork("side").unwrap();
        side.update(|b| b.tasks.update(&1, |t| t.state = "theirs".to_string()))
            .unwrap();
        main.update(|b| b.tasks.update(&1, |t| t.state = "ours".to_string()))
            .unwrap();
        let done = main
            .merge_with(&side, |b, base, theirs| {
                let mut seen = 0;
                b.tasks.merge(&base.tasks, &theirs.tasks, |c| {
                    seen += 1;
                    c.ours.as_deref().cloned()
                });
                ("merge", seen)
            })
            .unwrap();
        assert_eq!(done.value, 1);
        assert!(done.changed);
    }
    let main = Db::<Board>::open(&path).unwrap();
    let side = main.branch("side").unwrap();
    assert_eq!(
        main.common_ancestor(&side).unwrap().version(),
        side.version()
    );
    let done = main
        .merge_with(&side, |b, base, theirs| {
            let mut seen = 0;
            b.tasks.merge(&base.tasks, &theirs.tasks, |c| {
                seen += 1;
                c.ours.as_deref().cloned()
            });
            ("merge", seen)
        })
        .unwrap();
    assert_eq!(done.value, 0);
    assert!(!done.changed);
}

#[test]
fn merging_back_uses_the_reverse_record() {
    let dir = tempfile::tempdir().unwrap();
    let main = open(&dir);
    main.update(|b| b.tasks.insert(task(1, None, "open", &[])))
        .unwrap();
    let side = main.fork("side").unwrap();
    main.update(|b| b.tasks.insert(task(2, None, "open", &[])))
        .unwrap();
    side.merge_with(&main, |b, base, theirs| {
        let written = b
            .tasks
            .merge(&base.tasks, &theirs.tasks, |c| c.ours.as_deref().cloned());
        ("sync", written)
    })
    .unwrap();
    side.update(|b| b.tasks.insert(task(3, None, "open", &[])))
        .unwrap();
    assert_eq!(
        main.common_ancestor(&side).unwrap().version(),
        main.version()
    );
    let written = main
        .merge_with(&side, |b, base, theirs| {
            let n = b
                .tasks
                .merge(&base.tasks, &theirs.tasks, |c| c.ours.as_deref().cloned());
            ("back", n)
        })
        .unwrap();
    assert_eq!(written.value, 1);
    assert_eq!(main.read().tasks.len(), 3);
}

#[test]
fn changing_a_key_onto_an_existing_row_unindexes_the_displaced_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.update(|b| {
        b.tasks.insert(task(1, None, "one", &["x"]));
        b.tasks.insert(task(2, None, "two", &["y"]));
    })
    .unwrap();
    db.update(|b| b.tasks.get_mut(&1).unwrap().id = 2).unwrap();
    let snapshot = db.read();
    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks.get(&2).unwrap().state, "one");
    assert_eq!(snapshot.tasks.by_state("one").len(), 1);
    assert_eq!(snapshot.tasks.by_state("two").len(), 0);
    assert_eq!(snapshot.tasks.by_tags("x").len(), 1);
    assert_eq!(snapshot.tasks.by_tags("y").len(), 0);
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Lopsided(u64);

impl ferro::codec::Data for Lopsided {
    fn encode(&self, w: &mut ferro::codec::Encoder<'_>) -> ferro::Result<()> {
        w.varint(self.0);
        Ok(())
    }

    fn decode(r: &mut ferro::codec::Decoder<'_>) -> ferro::Result<Self> {
        let value = r.varint()?;
        if value == 7 {
            return Err(Error::Decode("seven is unreadable".into()));
        }
        Ok(Lopsided(value))
    }
}

#[test]
fn a_root_that_cannot_be_read_back_is_refused_before_it_becomes_durable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("l.ferro");
    {
        let db = Db::<Lopsided>::open(&path).unwrap();
        db.update(|root| root.0 = 1).unwrap();
        assert!(db.update(|root| root.0 = 7).is_err());
        assert_eq!(db.read().0, 1);
    }
    let db = Db::<Lopsided>::open(&path).unwrap();
    assert_eq!(db.read().0, 1);
    assert_eq!(db.version(), 1);
}

type Events = Map<u32, i64>;

const PATIENCE: Duration = Duration::from_secs(30);

fn add_deltas(root: &mut Events, base: &Events, theirs: &Events) {
    let mut keys: BTreeSet<u32> = base.keys().collect();
    keys.extend(theirs.keys());
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

fn add(
    root: &mut Events,
    base: &Snapshot<Events>,
    theirs: &Snapshot<Events>,
) -> (&'static str, ()) {
    add_deltas(root, base, theirs);
    ("merge", ())
}

fn events(db: &Db<Events>) -> BTreeMap<u32, i64> {
    db.read().iter().map(|(k, v)| (k, *v)).collect()
}

fn event(db: &Db<Events>, message: &str, key: u32) {
    db.commit(message, |root| {
        root.insert(key, 1);
    })
    .unwrap();
}

struct Gate {
    at: &'static str,
    landed: AtomicBool,
    held: Mutex<(bool, bool)>,
    moved: Condvar,
}

impl Gate {
    fn new(at: &'static str) -> Self {
        Gate {
            at,
            landed: AtomicBool::new(false),
            held: Mutex::new((false, false)),
            moved: Condvar::new(),
        }
    }

    fn step(&self, step: &str) {
        if thread::current().name() != Some("side") {
            return;
        }
        if step == "landed" {
            self.landed.store(true, Ordering::SeqCst);
            return;
        }
        if step != self.at || !self.landed.load(Ordering::SeqCst) {
            return;
        }
        let mut held = self.held.lock().unwrap();
        held.0 = true;
        self.moved.notify_all();
        while !held.1 {
            held = self.moved.wait(held).unwrap();
        }
    }

    fn wait_held(&self) {
        let mut held = self.held.lock().unwrap();
        while !held.0 {
            let (next, waited) = self.moved.wait_timeout(held, PATIENCE).unwrap();
            assert!(
                !waited.timed_out(),
                "the side's commit never reached its sync"
            );
            held = next;
        }
    }

    fn release(&self) {
        self.held.lock().unwrap().1 = true;
        self.moved.notify_all();
    }
}

struct Held {
    gate: Arc<Gate>,
    main: Db<Events>,
    side: Db<Events>,
    want: BTreeMap<u32, i64>,
    published: u64,
    syncing: JoinHandle<ferro::Result<()>>,
    _dir: TempDir,
}

fn hold_a_side_commit(durability: Durability, pulls: bool, at: &'static str) -> Held {
    let gate = Arc::new(Gate::new(at));
    let hook = {
        let gate = gate.clone();
        Hook(Arc::new(move |step| gate.step(step)))
    };
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Events> = Options::new()
        .durability(durability)
        .hook(hook)
        .open(dir.path().join("m.ferro"))
        .unwrap();
    event(&main, "x", 1);
    let side = main.fork("side").unwrap();
    event(&main, "a", 2);
    event(&side, "b", 3);
    let remote: Db<Events> = Db::open(dir.path().join("g.ferro")).unwrap();
    event(&remote, "r", 10);
    let mut want = BTreeMap::from([(1, 1), (2, 1), (3, 1)]);
    if pulls {
        main.pull(&remote, "g", add).unwrap();
        want.insert(10, 1);
    }
    let published = side.version();
    let syncing = {
        let (side, main) = (side.clone(), main.clone());
        thread::Builder::new()
            .name("side".into())
            .spawn(move || match pulls {
                true => side.pull(&remote, "g", add).map(|_| ()),
                false => side.merge_with(&main, add).map(|_| ()),
            })
            .unwrap()
    };
    gate.wait_held();
    Held {
        gate,
        main,
        side,
        want,
        published,
        syncing,
        _dir: dir,
    }
}

fn merging(held: &Held, seen: mpsc::Sender<(u64, u64)>) -> JoinHandle<ferro::Result<()>> {
    let (main, side) = (held.main.clone(), held.side.clone());
    thread::spawn(move || {
        main.merge_with(&side, |root, base, theirs| {
            seen.send((base.version(), theirs.version())).unwrap();
            add(root, base, theirs)
        })
        .map(|_| ())
    })
}

fn settle_and_merge_again(held: Held, merging: JoinHandle<ferro::Result<()>>) {
    held.gate.release();
    held.syncing.join().unwrap().unwrap();
    merging.join().unwrap().unwrap();
    let Held {
        main, side, want, ..
    } = held;
    assert_eq!(events(&main), want);
    main.merge_with(&side, add).unwrap();
    assert_eq!(events(&main), want);
    side.merge_with(&main, add).unwrap();
    assert_eq!(events(&side), want);
    main.verify().unwrap();
}

fn merge_beside_a_commit_still_syncing(durability: Durability, pulls: bool) {
    let held = hold_a_side_commit(durability, pulls, "leading");
    let found = held.main.common_ancestors(&held.side).unwrap();
    assert_eq!(found.iter().map(Snapshot::version).collect::<Vec<_>>(), [1]);
    let (seen, runs) = mpsc::channel();
    let merge = merging(&held, seen);
    let first = runs.recv_timeout(PATIENCE).unwrap();
    assert_eq!(first, (1, held.published));
    settle_and_merge_again(held, merge);
    assert_eq!(runs.try_iter().count(), 0, "the merge built a virtual base");
}

fn merge_beside_a_commit_being_published(durability: Durability, pulls: bool) {
    let held = hold_a_side_commit(durability, pulls, "published");
    assert!(
        held.side.version() > held.published,
        "the branch's state is published before the lock is let go"
    );
    let (seen, runs) = mpsc::channel();
    let merge = merging(&held, seen);
    assert!(
        runs.recv_timeout(Duration::from_millis(100)).is_err(),
        "a merge ran while the refs and states were being published"
    );
    settle_and_merge_again(held, merge);
}

#[test]
fn a_merge_leaves_a_pull_still_syncing_on_the_other_branch_for_later() {
    for durability in [Durability::Buffered, Durability::Fsync] {
        merge_beside_a_commit_still_syncing(durability, true);
    }
}

#[test]
fn a_merge_leaves_a_merge_still_syncing_on_the_other_branch_for_later() {
    for durability in [Durability::Buffered, Durability::Fsync] {
        merge_beside_a_commit_still_syncing(durability, false);
    }
}

#[test]
fn a_merge_waits_for_the_other_branch_to_be_published_with_its_merge_records() {
    for durability in [Durability::Buffered, Durability::Fsync] {
        merge_beside_a_commit_being_published(durability, true);
        merge_beside_a_commit_being_published(durability, false);
    }
}

fn messages(found: ferro::Result<Vec<Snapshot<Events>>>) -> ferro::Result<Vec<String>> {
    Ok(found?.iter().map(|s| s.commit().message.clone()).collect())
}

#[test]
fn branch_pairs_refuse_a_handle_on_another_open_database() {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Events> = Db::open(dir.path().join("a.ferro")).unwrap();
    let b: Db<Events> = Db::open(dir.path().join("b.ferro")).unwrap();
    let c: Db<Events> = Db::open(dir.path().join("c.ferro")).unwrap();
    event(&c, "c1", 1);
    event(&b, "b1", 2);
    let pulled = a.pull(&c, "c", add).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    let reader: Db<Events> = Options::new()
        .read_only(true)
        .open(dir.path().join("a.ferro"))
        .unwrap();
    for (ours, theirs) in [(&a, &b), (&b, &a), (&a, &reader), (&reader, &a)] {
        assert!(matches!(
            ours.common_ancestor(theirs),
            Err(Error::ForeignHandle)
        ));
        assert!(matches!(
            messages(ours.common_ancestors(theirs)),
            Err(Error::ForeignHandle)
        ));
    }
    let d = a.fork("d").unwrap();
    assert_eq!(messages(a.common_ancestors(&d)).unwrap(), ["c1"]);

    let mut runs = 0;
    let merged = b.merge_with(&a, |root, base, theirs| {
        runs += 1;
        add(root, base, theirs)
    });
    assert!(matches!(merged, Err(Error::ForeignHandle)));
    let tried = b.try_merge_with(&a, |_, _, _| {
        runs += 1;
        Ok::<_, Error>(("never", ()))
    });
    assert!(matches!(tried, Err(Error::ForeignHandle)));
    assert_eq!(runs, 0);
    assert_eq!((b.version(), events(&b)), (1, BTreeMap::from([(2, 1)])));
    b.verify().unwrap();
}

#[test]
fn branch_pairs_refuse_a_dropped_branch_even_once_its_name_is_reused() {
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Events> = Db::open(dir.path().join("m.ferro")).unwrap();
    let remote: Db<Events> = Db::open(dir.path().join("r.ferro")).unwrap();
    event(&remote, "r1", 1);
    let old = main.fork("x").unwrap();
    event(&old, "x1", 2);
    let pulled = main.pull(&remote, "r", add).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    let refused = |found: ferro::Result<Vec<String>>| matches!(found, Err(Error::NoBranch(name)) if name == "x");
    main.drop_branch("x").unwrap();
    assert!(refused(messages(old.common_ancestors(&main))));
    assert!(refused(messages(main.common_ancestors(&old))));
    let new = main.fork("x").unwrap();
    assert_eq!(old.read().commit().message, "x1");
    assert!(refused(messages(old.common_ancestors(&main))));
    assert!(refused(messages(main.common_ancestors(&old))));
    assert!(matches!(
        old.common_ancestor(&main),
        Err(Error::NoBranch(_))
    ));
    let mut runs = 0;
    let merged = main.merge_with(&old, |_, _, _| {
        runs += 1;
        ("never", ())
    });
    assert!(matches!(merged, Err(Error::NoBranch(name)) if name == "x"));
    assert_eq!(runs, 0);
    assert_eq!(messages(main.common_ancestors(&new)).unwrap(), ["r1"]);
}

#[derive(Default)]
struct Queued {
    held: bool,
    placed: bool,
    released: bool,
    done: bool,
}

struct Leader {
    state: Mutex<Queued>,
    moved: Condvar,
}

impl Leader {
    fn step(&self, step: &str) {
        let name = thread::current().name().map(str::to_string);
        let mut state = self.state.lock().unwrap();
        match (name.as_deref(), step) {
            (Some("placed"), "landed") => {
                state.placed = true;
                self.moved.notify_all();
            }
            (Some("held"), "leading") if !state.done => {
                state.held = true;
                self.moved.notify_all();
                while !state.released {
                    state = self.moved.wait(state).unwrap();
                }
                state.done = true;
            }
            _ => {}
        }
    }

    fn wait(&self, ready: impl Fn(&Queued) -> bool) {
        let mut state = self.state.lock().unwrap();
        while !ready(&state) {
            let (next, waited) = self.moved.wait_timeout(state, PATIENCE).unwrap();
            assert!(!waited.timed_out(), "the side's commits never queued");
            state = next;
        }
    }

    fn release(&self) {
        self.state.lock().unwrap().released = true;
        self.moved.notify_all();
    }
}

fn commit_on(db: &Db<Events>, thread: &str, message: &'static str, key: u32) -> JoinHandle<()> {
    let db = db.clone();
    thread::Builder::new()
        .name(thread.into())
        .spawn(move || event(&db, message, key))
        .unwrap()
}

fn merge_beside_pruning_commits_in_their_sync(durability: Durability) {
    let leader = Arc::new(Leader {
        state: Mutex::new(Queued::default()),
        moved: Condvar::new(),
    });
    let hook = {
        let leader = leader.clone();
        Hook(Arc::new(move |step| leader.step(step)))
    };
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Events> = Options::new()
        .durability(durability)
        .history(Keep::Last(2))
        .reclaim(Reclaim::off())
        .hook(hook)
        .open(dir.path().join("m.ferro"))
        .unwrap();
    event(&main, "x", 1);
    let side = main.fork("side").unwrap();
    event(&side, "b", 2);
    event(&main, "a", 3);
    let held = commit_on(&side, "held", "c", 4);
    leader.wait(|state| state.held);
    let placed = commit_on(&side, "placed", "d", 5);
    leader.wait(|state| state.placed);
    let (seen, runs) = mpsc::channel();
    let merging = {
        let (main, side) = (main.clone(), side.clone());
        thread::spawn(move || {
            main.merge_with(&side, |root, base, theirs| {
                seen.send((base.version(), theirs.version())).unwrap();
                add(root, base, theirs)
            })
            .unwrap();
        })
    };
    assert_eq!(runs.recv_timeout(PATIENCE).unwrap(), (1, 2));
    leader.release();
    held.join().unwrap();
    placed.join().unwrap();
    merging.join().unwrap();
    assert_eq!(side.version(), 4);
    assert_eq!(side.log().first().unwrap().0, 3);
    let merged = BTreeMap::from([(1, 1), (2, 1), (3, 1)]);
    assert_eq!(events(&main), merged);
    assert_eq!(messages(main.common_ancestors(&side)).unwrap(), ["b"]);

    main.compact(Keep::Last(2)).unwrap();
    main.verify().unwrap();
    main.verify_typed().unwrap();
    assert_eq!(messages(main.common_ancestors(&side)).unwrap(), ["b"]);
    assert_eq!(messages(side.common_ancestors(&main)).unwrap(), ["b"]);
    let log: Vec<String> = main.log().values().map(|c| c.message.clone()).collect();
    assert_eq!(log, ["a", "merge"]);
    let sizes: Vec<(u64, usize)> = main
        .history_of(|root| root.len())
        .map(|(commit, len)| (commit.version, len))
        .collect();
    assert_eq!(sizes, [(3, 3), (2, 2)]);
    assert_eq!(*main.at(3).unwrap(), *main.read());

    let mut bases = Vec::new();
    main.merge_with(&side, |root, base, theirs| {
        bases.push(base.commit().message.clone());
        add(root, base, theirs)
    })
    .unwrap();
    assert_eq!(bases, ["b"]);
    let all = BTreeMap::from([(1, 1), (2, 1), (3, 1), (4, 1), (5, 1)]);
    assert_eq!(events(&main), all);
    side.merge_with(&main, add).unwrap();
    assert_eq!(events(&side), all);
    main.compact(Keep::Last(2)).unwrap();
    main.verify().unwrap();
    main.verify_typed().unwrap();
}

#[test]
fn a_merge_beside_pruning_commits_still_in_the_other_branchs_sync_links_what_it_merged() {
    for durability in [Durability::Buffered, Durability::Fsync] {
        merge_beside_pruning_commits_in_their_sync(durability);
    }
}
