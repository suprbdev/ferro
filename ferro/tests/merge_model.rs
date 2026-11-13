mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::fast;
use ferro::db::Clock;
use ferro::{Conflict, Data, Db, Keep, Map, Options, Outcome, Reclaim, Record, Snapshot, Table};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u8,
    #[index]
    n: u8,
    #[index(each)]
    tags: Vec<String>,
}

fn row(id: u8, n: u8) -> Row {
    Row {
        id,
        n,
        tags: vec![format!("t{}", n % 3), format!("t{}", n % 2)],
    }
}

type Side = BTreeMap<u8, u8>;

fn side() -> impl Strategy<Value = Side> {
    prop::collection::btree_map(0..14u8, 0..4u8, 0..12)
}

#[derive(Clone, Copy, Debug)]
enum Policy {
    Ours,
    Theirs,
    Drop,
    Sum,
}

fn policy() -> impl Strategy<Value = Policy> {
    prop_oneof![
        Just(Policy::Ours),
        Just(Policy::Theirs),
        Just(Policy::Drop),
        Just(Policy::Sum)
    ]
}

fn pick(policy: Policy, ours: Option<u8>, theirs: Option<u8>) -> Option<u8> {
    match policy {
        Policy::Ours => ours,
        Policy::Theirs => theirs,
        Policy::Drop => None,
        Policy::Sum => Some(
            ours.unwrap_or(0)
                .wrapping_add(theirs.unwrap_or(0))
                .wrapping_add(10),
        ),
    }
}

type Seen = (u8, Option<u8>, Option<u8>, Option<u8>);

struct Reference {
    result: Side,
    written: usize,
    conflicts: Vec<Seen>,
}

fn reference(base: &Side, ours: &Side, theirs: &Side, policy: Policy) -> Reference {
    let mut keys: BTreeSet<u8> = BTreeSet::new();
    keys.extend(base.keys());
    keys.extend(ours.keys());
    keys.extend(theirs.keys());
    let mut result = ours.clone();
    let mut written = 0;
    let mut conflicts = Vec::new();
    for key in keys {
        let (b, o, t) = (
            base.get(&key).copied(),
            ours.get(&key).copied(),
            theirs.get(&key).copied(),
        );
        if t == b || o == t {
            continue;
        }
        let value = if o == b {
            t
        } else {
            conflicts.push((key, b, o, t));
            pick(policy, o, t)
        };
        match value {
            Some(v) => {
                result.insert(key, v);
            }
            None => {
                result.remove(&key);
            }
        }
        written += 1;
    }
    Reference {
        result,
        written,
        conflicts,
    }
}

fn to_map(side: &Side) -> Map<u8, u8> {
    side.iter().map(|(k, v)| (*k, *v)).collect()
}

fn to_table(side: &Side) -> Table<Row> {
    let mut table: Table<Row> = Table::new();
    for (k, v) in side {
        table.insert(row(*k, *v));
    }
    table
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(400)))]

    #[test]
    fn map_merge_matches_reference(base in side(), ours in side(), theirs in side(), policy in policy()) {
        let expected = reference(&base, &ours, &theirs, policy);
        let mut target = to_map(&ours);
        let mut seen = Vec::new();
        let written = target.merge(&to_map(&base), &to_map(&theirs), |c: Conflict<u8, u8>| {
            seen.push((c.key, c.base.as_deref().copied(), c.ours.as_deref().copied(), c.theirs.as_deref().copied()));
            pick(policy, c.ours.as_deref().copied(), c.theirs.as_deref().copied())
        });
        prop_assert_eq!(written, expected.written);
        prop_assert_eq!(seen, expected.conflicts);
        let want = to_map(&expected.result);
        prop_assert!(target == want);
        prop_assert_eq!(target.root_hash(), want.root_hash());
        let got: Side = target.iter().map(|(k, v)| (k, *v)).collect();
        prop_assert_eq!(got, expected.result);
    }

    #[test]
    fn table_merge_matches_reference(base in side(), ours in side(), theirs in side(), policy in policy()) {
        let expected = reference(&base, &ours, &theirs, policy);
        let mut target = to_table(&ours);
        let mut seen = Vec::new();
        let written = target.merge(&to_table(&base), &to_table(&theirs), |c: Conflict<u8, Row>| {
            seen.push((c.key, c.base.as_ref().map(|r| r.n), c.ours.as_ref().map(|r| r.n), c.theirs.as_ref().map(|r| r.n)));
            pick(policy, c.ours.as_ref().map(|r| r.n), c.theirs.as_ref().map(|r| r.n)).map(|n| row(c.key, n))
        });
        prop_assert_eq!(written, expected.written);
        prop_assert_eq!(seen, expected.conflicts);
        let want = to_table(&expected.result);
        prop_assert!(target == want);
        prop_assert_eq!(target.rows.root_hash(), want.rows.root_hash());
        prop_assert_eq!(target.by_n.root_hash(), want.by_n.root_hash());
        prop_assert_eq!(target.by_tags.root_hash(), want.by_tags.root_hash());
    }
}

type Counters = Map<u8, i64>;
type Mask = BTreeMap<u8, i64>;

const KEYS: u8 = 3;
const NAMES: [&str; 4] = ["main", "b1", "b2", "b3"];

#[derive(Clone, Debug)]
enum Step {
    Inc(usize, u8),
    Merge(usize, usize),
    MergeTwice(usize, usize),
    Fork(usize),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        8 => (0..4usize, 0..KEYS).prop_map(|(b, k)| Step::Inc(b, k)),
        6 => (0..4usize, 0..4usize).prop_map(|(a, b)| Step::Merge(a, b)),
        2 => (0..4usize, 0..4usize).prop_map(|(a, b)| Step::MergeTwice(a, b)),
        2 => (0..4usize).prop_map(Step::Fork),
    ]
}

struct Node {
    mask: Mask,
}

struct World {
    nodes: Vec<Node>,
    logs: Vec<Option<Vec<usize>>>,
    dbs: Vec<Option<Db<Counters>>>,
    events: u32,
}

impl World {
    fn head(&self, branch: usize) -> Option<usize> {
        self.logs[branch]
            .as_ref()
            .map(|log| log.last().copied().unwrap_or(0))
    }

    fn push(&mut self, branch: usize, mask: Mask) {
        self.nodes.push(Node { mask });
        let id = self.nodes.len() - 1;
        self.logs[branch].as_mut().unwrap().push(id);
    }
}

fn values(db: &Db<Counters>) -> Mask {
    let snapshot = db.read();
    (0..KEYS)
        .map(|k| (k, snapshot.get(&k).map(|v| *v).unwrap_or(0)))
        .collect()
}

fn mask_of(mask: &Mask) -> Mask {
    (0..KEYS)
        .map(|k| (k, mask.get(&k).copied().unwrap_or(0)))
        .collect()
}

fn counter_merge(db: &Db<Counters>, other: &Db<Counters>) -> ferro::Committed<()> {
    db.merge_with(other, |root, base, theirs| {
        for k in 0..KEYS {
            let delta =
                theirs.get(&k).map(|v| *v).unwrap_or(0) - base.get(&k).map(|v| *v).unwrap_or(0);
            if delta != 0 {
                let current = root.get(&k).map(|v| *v).unwrap_or(0);
                root.insert(k, current + delta);
            }
        }
        ("merge", ())
    })
    .unwrap()
}

fn run_branches(steps: Vec<Step>, paired: bool) -> Result<(), TestCaseError> {
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Counters> = fast().open(dir.path().join("m.ferro")).unwrap();
    let mut world = World {
        nodes: vec![Node { mask: Mask::new() }],
        logs: vec![None; 4],
        dbs: vec![None, None, None, None],
        events: 0,
    };
    world.logs[0] = Some(vec![]);
    if paired {
        world.dbs[1] = Some(main.fork(NAMES[1]).unwrap());
        world.logs[1] = Some(vec![]);
    }
    world.dbs[0] = Some(main);
    for step in steps {
        match step {
            Step::Inc(b, k) => {
                let Some(db) = world.dbs[b].clone() else {
                    continue;
                };
                let bit = 1i64 << world.events;
                world.events += 1;
                db.update(|root| {
                    let current = root.get(&k).map(|v| *v).unwrap_or(0);
                    root.insert(k, current + bit);
                })
                .unwrap();
                let head = world.head(b).unwrap();
                let mut mask = mask_of(&world.nodes[head].mask);
                *mask.get_mut(&k).unwrap() += bit;
                world.push(b, mask);
            }
            Step::Fork(from) => {
                let Some(db) = world.dbs[from].clone() else {
                    continue;
                };
                let Some(slot) = (1..4).find(|i| world.dbs[*i].is_none()) else {
                    continue;
                };
                world.dbs[slot] = Some(db.fork(NAMES[slot]).unwrap());
                world.logs[slot] = world.logs[from].clone();
            }
            Step::Merge(a, b) | Step::MergeTwice(a, b) => {
                if a == b {
                    continue;
                }
                let (Some(da), Some(db)) = (world.dbs[a].clone(), world.dbs[b].clone()) else {
                    continue;
                };
                let before = da.version();
                let (ha, hb) = (world.head(a).unwrap(), world.head(b).unwrap());
                let done = counter_merge(&da, &db);
                let mut mask = mask_of(&world.nodes[ha].mask);
                for (k, v) in mask_of(&world.nodes[hb].mask) {
                    *mask.get_mut(&k).unwrap() |= v;
                }
                prop_assert_eq!(values(&da), mask.clone(), "merge {} <- {}", a, b);
                if done.version != before {
                    world.push(a, mask.clone());
                }
                if matches!(step, Step::MergeTwice(..)) {
                    let again = counter_merge(&da, &db);
                    prop_assert!(!again.changed);
                    prop_assert_eq!(values(&da), mask);
                }
            }
        }
        for b in 0..4 {
            if let Some(db) = &world.dbs[b] {
                let head = world.head(b).unwrap();
                prop_assert_eq!(values(db), mask_of(&world.nodes[head].mask), "branch {}", b);
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(300)))]

    #[test]
    fn two_branch_counter_merges_apply_each_change_exactly_once(steps in prop::collection::vec(step(), 1..30)) {
        let steps = steps
            .into_iter()
            .filter_map(|s| match s {
                Step::Inc(b, k) => Some(Step::Inc(b % 2, k)),
                Step::Merge(a, b) => Some(Step::Merge(a % 2, b % 2)),
                Step::MergeTwice(a, b) => Some(Step::MergeTwice(a % 2, b % 2)),
                Step::Fork(_) => None,
            })
            .collect();
        run_branches(steps, true)?;
    }

    #[test]
    fn many_branch_counter_merges_apply_each_change_exactly_once(steps in prop::collection::vec(step(), 1..30)) {
        run_branches(steps, false)?;
    }
}

#[test]
fn ping_pong_merges_do_not_apply_a_change_twice() {
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Counters> = fast().open(dir.path().join("p.ferro")).unwrap();
    let side = main.fork("side").unwrap();
    side.update(|root| {
        root.insert(0, 1);
    })
    .unwrap();
    main.update(|root| {
        root.insert(1, 10);
    })
    .unwrap();
    counter_merge(&main, &side);
    counter_merge(&side, &main);
    main.update(|root| {
        root.insert(2, 100);
    })
    .unwrap();
    counter_merge(&main, &side);
    counter_merge(&side, &main);
    assert_eq!(values(&main), values(&side));
    assert_eq!(
        values(&main),
        [(0, 1), (1, 10), (2, 100)].into_iter().collect::<Mask>()
    );
}

fn bump(db: &Db<Counters>, key: u8, by: i64) {
    db.update(|root| {
        let current = root.get(&key).map(|v| *v).unwrap_or(0);
        root.insert(key, current + by);
    })
    .unwrap();
}

#[test]
fn forks_of_forks_merge_each_change_once() {
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Counters> = fast().open(dir.path().join("f.ferro")).unwrap();
    bump(&main, 0, 1);
    let one = main.fork("one").unwrap();
    bump(&one, 1, 10);
    let two = one.fork("two").unwrap();
    bump(&two, 2, 100);
    bump(&one, 1, 1000);
    bump(&main, 0, 10000);
    assert_eq!(main.common_ancestor(&two).unwrap().version(), 1);
    assert_eq!(one.common_ancestor(&two).unwrap().version(), 2);
    counter_merge(&main, &two);
    counter_merge(&one, &two);
    counter_merge(&two, &main);
    counter_merge(&main, &two);
    counter_merge(&one, &two);
    counter_merge(&two, &main);
    let at_main: Mask = [(0, 10001), (1, 10), (2, 100)].into_iter().collect();
    assert_eq!(values(&main), at_main);
    assert_eq!(values(&two), at_main);
    assert_eq!(
        values(&one),
        [(0, 10001), (1, 1010), (2, 100)]
            .into_iter()
            .collect::<Mask>()
    );
}

#[test]
fn common_ancestor_survives_a_restore_that_makes_the_logs_differ() {
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Counters> = fast().open(dir.path().join("r.ferro")).unwrap();
    for i in 1..=3 {
        bump(&main, 0, i);
    }
    let side = main.fork("side").unwrap();
    main.restore(&main.at(1).unwrap()).unwrap();
    bump(&side, 1, 5);
    assert_eq!(main.version(), 4);
    assert_eq!(side.version(), 4);
    assert_eq!(main.common_ancestor(&side).unwrap().version(), 3);
    assert_eq!(side.common_ancestor(&main).unwrap().version(), 3);
    counter_merge(&main, &side);
    assert_eq!(
        values(&main),
        [(0, 1), (1, 5), (2, 0)].into_iter().collect::<Mask>()
    );
}

#[test]
fn merge_after_compaction_pruned_the_record_errors_or_stays_exact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.ferro");
    {
        let main: Db<Counters> = fast().open(&path).unwrap();
        let side = main.fork("side").unwrap();
        bump(&side, 0, 1);
        bump(&main, 1, 10);
        counter_merge(&main, &side);
        for _ in 0..6 {
            bump(&main, 2, 100);
            bump(&side, 2, 1000);
        }
    }
    ferro::admin::compact(&path, ferro::admin::Keep::Last(3)).unwrap();
    let main: Db<Counters> = fast().open(&path).unwrap();
    let side = main.branch("side").unwrap();
    let merged = main.merge_with(&side, |root, base, theirs| {
        for k in 0..KEYS {
            let delta =
                theirs.get(&k).map(|v| *v).unwrap_or(0) - base.get(&k).map(|v| *v).unwrap_or(0);
            if delta != 0 {
                let current = root.get(&k).map(|v| *v).unwrap_or(0);
                root.insert(k, current + delta);
            }
        }
        ("merge", ())
    });
    match merged {
        Err(ferro::Error::NoCommonAncestor) => {}
        Ok(_) => assert_eq!(
            values(&main),
            [(0, 1), (1, 10), (2, 6600)].into_iter().collect::<Mask>()
        ),
        Err(other) => panic!("{other}"),
    }
}

type Events = Map<u32, i64>;
type Counts = BTreeMap<u32, i64>;

const SLOTS: usize = 5;

#[derive(Clone, Debug)]
enum Op {
    Inc(usize),
    Merge(usize, usize),
    Fork(usize),
    Drop(usize),
    Compact,
    Cross(usize, usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0..SLOTS).prop_map(Op::Inc),
        7 => (0..SLOTS, 0..SLOTS).prop_map(|(a, b)| Op::Merge(a, b)),
        3 => (0..SLOTS).prop_map(Op::Fork),
        1 => (1..SLOTS).prop_map(Op::Drop),
        1 => Just(Op::Compact),
        3 => (0..SLOTS, 0..SLOTS).prop_map(|(a, b)| Op::Cross(a, b)),
    ]
}

struct Version {
    generation: u64,
    event: Option<u32>,
    ancestors: Vec<u64>,
}

fn has(bits: &[u64], id: usize) -> bool {
    bits.get(id / 64)
        .is_some_and(|word| word & (1 << (id % 64)) != 0)
}

struct Graph(Vec<Version>);

impl Graph {
    fn push(&mut self, parents: &[usize], event: Option<u32>) -> usize {
        let id = self.0.len();
        let generation = parents
            .iter()
            .map(|parent| self.0[*parent].generation + 1)
            .max()
            .unwrap_or(0);
        let mut ancestors = vec![0u64; id / 64 + 1];
        for parent in parents {
            for (word, bits) in ancestors.iter_mut().zip(&self.0[*parent].ancestors) {
                *word |= bits;
            }
        }
        ancestors[id / 64] |= 1 << (id % 64);
        self.0.push(Version {
            generation,
            event,
            ancestors,
        });
        id
    }

    fn reaches(&self, from: usize, to: usize) -> bool {
        has(&self.0[from].ancestors, to)
    }

    fn best(&self, a: usize, b: usize) -> Vec<usize> {
        let common: Vec<usize> = (0..self.0.len())
            .filter(|id| self.reaches(a, *id) && self.reaches(b, *id))
            .collect();
        let mut below = vec![0u64; self.0.len() / 64 + 1];
        for d in &common {
            for (index, (word, bits)) in below.iter_mut().zip(&self.0[*d].ancestors).enumerate() {
                let own = if index == d / 64 { 1 << (d % 64) } else { 0 };
                *word |= bits & !own;
            }
        }
        common.into_iter().filter(|c| !has(&below, *c)).collect()
    }

    fn events(&self, id: usize) -> Counts {
        (0..self.0.len())
            .filter(|v| self.reaches(id, *v))
            .filter_map(|v| self.0[v].event)
            .map(|e| (e, 1))
            .collect()
    }
}

fn counts(db: &Db<Events>) -> Counts {
    db.read().iter().map(|(k, v)| (k, *v)).collect()
}

fn id_of(snapshot: &Snapshot<Events>) -> usize {
    snapshot
        .commit()
        .message
        .strip_prefix('n')
        .map_or(0, |id| id.parse().unwrap())
}

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

fn ancestor_ids(db: &Db<Events>, other: &Db<Events>) -> ferro::Result<Vec<usize>> {
    Ok(db.common_ancestors(other)?.iter().map(id_of).collect())
}

fn bases(dbs: &[Db<Events>]) -> Vec<Option<Vec<usize>>> {
    let mut out = Vec::new();
    for a in dbs {
        for b in dbs {
            out.push(ancestor_ids(a, b).ok());
        }
    }
    out
}

#[derive(Default)]
struct Crossed {
    merges: usize,
    calls: usize,
}

impl Crossed {
    fn check(&mut self, bases: usize, calls: &[u64]) -> Result<(), TestCaseError> {
        if bases == 1 {
            prop_assert_eq!(calls.len(), 1);
            return Ok(());
        }
        prop_assert!(calls.len() >= 2, "{} bases, {} calls", bases, calls.len());
        prop_assert_eq!(calls.last().copied(), Some(0));
        self.merges += 1;
        self.calls = self.calls.max(calls.len());
        Ok(())
    }

    fn add(&mut self, other: Crossed) {
        self.merges += other.merges;
        self.calls = self.calls.max(other.calls);
    }
}

fn counted_merge(
    ours: &Db<Events>,
    theirs: &Db<Events>,
    id: usize,
) -> (ferro::Result<ferro::Committed<usize>>, Vec<u64>) {
    let mut calls = Vec::new();
    let merged = ours.merge_with(theirs, |root, base, other| {
        add_deltas(root, base, other);
        calls.push(base.version());
        (format!("n{id}"), id_of(base))
    });
    (merged, calls)
}

struct Run {
    main: Db<Events>,
    graph: Graph,
    heads: Vec<Option<usize>>,
    dbs: Vec<Option<Db<Events>>>,
    events: u32,
    keep: Keep,
    pruned: bool,
    crossed: Crossed,
}

impl Run {
    fn live(&self) -> Vec<usize> {
        (0..SLOTS)
            .filter(|slot| self.dbs[*slot].is_some())
            .collect()
    }

    fn inc(&mut self, slot: usize) {
        let db = self.dbs[slot].clone().unwrap();
        let (id, event, head) = (self.graph.0.len(), self.events, self.heads[slot].unwrap());
        self.events += 1;
        db.commit(format!("n{id}"), |root| {
            root.insert(event, 1);
        })
        .unwrap();
        self.graph.push(&[head], Some(event));
        self.heads[slot] = Some(id);
    }

    fn fork(&mut self, from: usize) -> Option<usize> {
        let slot = (1..SLOTS).find(|slot| self.dbs[*slot].is_none())?;
        let db = self.dbs[from].as_ref().unwrap();
        self.dbs[slot] = Some(db.fork(&format!("b{slot}")).unwrap());
        self.heads[slot] = self.heads[from];
        Some(slot)
    }

    fn drop_slot(&mut self, slot: usize) {
        self.dbs[slot] = None;
        self.heads[slot] = None;
        self.main.drop_branch(&format!("b{slot}")).unwrap();
    }

    fn compact(&mut self) -> Result<(), TestCaseError> {
        let open: Vec<Db<Events>> = self.dbs.iter().flatten().cloned().collect();
        let before = bases(&open);
        self.main.compact(self.keep).unwrap();
        prop_assert_eq!(bases(&open), before);
        Ok(())
    }

    fn merge(&mut self, a: usize, b: usize) -> Result<(), TestCaseError> {
        if a == b {
            return Ok(());
        }
        let (ours, theirs) = (self.dbs[a].clone().unwrap(), self.dbs[b].clone().unwrap());
        let (ha, hb) = (self.heads[a].unwrap(), self.heads[b].unwrap());
        let best = self.graph.best(ha, hb);
        let found = ancestor_ids(&ours, &theirs);
        let back = ancestor_ids(&theirs, &ours);
        prop_assert_eq!(found.as_ref().ok(), back.as_ref().ok());
        let seen = match found {
            Ok(seen) => seen,
            Err(ferro::Error::NoCommonAncestor) if self.pruned => {
                let failed = ours.merge_with(&theirs, |_, _, _| ("never", ()));
                prop_assert!(matches!(failed, Err(ferro::Error::NoCommonAncestor)));
                return Ok(());
            }
            Err(error) => panic!("{error}"),
        };
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        prop_assert_eq!(&sorted, &best, "bases {:?}", seen);
        let generation = |id: &usize| self.graph.0[*id].generation;
        prop_assert!(
            seen.windows(2)
                .all(|pair| generation(&pair[0]) >= generation(&pair[1]))
        );
        prop_assert_eq!(id_of(&ours.common_ancestor(&theirs).unwrap()), seen[0]);
        let id = self.graph.0.len();
        let before = ours.version();
        let (merged, calls) = counted_merge(&ours, &theirs, id);
        let done = match merged {
            Ok(done) => done,
            Err(ferro::Error::NoCommonAncestor) if self.pruned && best.len() > 1 => {
                prop_assert_eq!(ours.version(), before);
                return Ok(());
            }
            Err(error) => panic!("{error}"),
        };
        self.crossed.check(best.len(), &calls)?;
        if best.len() == 1 {
            prop_assert_eq!(done.value, seen[0]);
        }
        if best == vec![hb] {
            prop_assert!(!done.changed);
            prop_assert_eq!(ours.version(), before);
            return Ok(());
        }
        prop_assert!(done.changed);
        self.graph.push(&[ha, hb], None);
        self.heads[a] = Some(id);
        Ok(())
    }

    fn check(&self) -> Result<(), TestCaseError> {
        for slot in 0..SLOTS {
            let Some(db) = &self.dbs[slot] else {
                continue;
            };
            let head = self.heads[slot].unwrap();
            prop_assert_eq!(counts(db), self.graph.events(head), "slot {}", slot);
        }
        Ok(())
    }
}

fn run_graph(
    ops: Vec<Op>,
    options: Options,
    keep: Keep,
    pruned: bool,
) -> Result<Crossed, TestCaseError> {
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Events> = options.open(dir.path().join("g.ferro")).unwrap();
    let mut run = Run {
        main: main.clone(),
        graph: Graph(Vec::new()),
        heads: vec![None; SLOTS],
        dbs: vec![None; SLOTS],
        events: 0,
        keep,
        pruned,
        crossed: Crossed::default(),
    };
    run.graph.push(&[], None);
    run.heads[0] = Some(0);
    run.dbs[0] = Some(main);
    for op in ops {
        let live = run.live();
        let pick = |index: usize| live[index % live.len()];
        match op {
            Op::Inc(index) => run.inc(pick(index)),
            Op::Fork(index) => {
                run.fork(pick(index));
            }
            Op::Drop(index) => {
                let others: Vec<usize> = live.iter().copied().filter(|slot| *slot != 0).collect();
                if !others.is_empty() {
                    run.drop_slot(others[index % others.len()]);
                }
            }
            Op::Compact => run.compact()?,
            Op::Merge(a, b) => run.merge(pick(a), pick(b))?,
            Op::Cross(a, b) => {
                let (a, b) = (pick(a), pick(b));
                if a != b
                    && let Some(copy) = run.fork(a)
                {
                    run.merge(a, b)?;
                    run.merge(b, copy)?;
                    run.drop_slot(copy);
                }
            }
        }
        run.check()?;
    }
    run.main.verify().unwrap();
    Ok(run.crossed)
}

const DEVICES: usize = 3;
const LOCAL: [&str; 2] = ["main", "side"];

#[derive(Clone, Debug)]
enum Act {
    Inc(usize, usize),
    Fork(usize),
    Merge(usize, bool),
    Pull(usize, usize, usize, usize),
    Sync(usize, usize),
    Cross(usize, usize),
    Compact(usize),
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        6 => (0..DEVICES, 0..2usize).prop_map(|(d, s)| Act::Inc(d, s)),
        1 => (0..DEVICES).prop_map(Act::Fork),
        2 => (0..DEVICES, any::<bool>()).prop_map(|(d, back)| Act::Merge(d, back)),
        6 => (0..DEVICES, 0..2usize, 0..DEVICES, 0..2usize)
            .prop_map(|(a, s, b, t)| Act::Pull(a, s, b, t)),
        2 => (0..DEVICES, 0..DEVICES).prop_map(|(a, b)| Act::Sync(a, b)),
        2 => (0..DEVICES, 0..DEVICES).prop_map(|(a, b)| Act::Cross(a, b)),
    ]
}

fn pruning_act() -> impl Strategy<Value = Act> {
    prop_oneof![
        19 => act(),
        1 => (0..DEVICES).prop_map(Act::Compact),
    ]
}

struct Devices {
    graph: Graph,
    dbs: Vec<[Option<Db<Events>>; 2]>,
    heads: Vec<[usize; 2]>,
    events: u32,
    crossed: Crossed,
    keep: Option<Keep>,
}

impl Devices {
    fn inc(&mut self, device: usize, slot: usize) {
        let Some(db) = &self.dbs[device][slot] else {
            return;
        };
        let (id, event) = (self.graph.0.len(), self.events);
        self.events += 1;
        db.commit(format!("n{id}"), |root| {
            root.insert(event, 1);
        })
        .unwrap();
        self.graph.push(&[self.heads[device][slot]], Some(event));
        self.heads[device][slot] = id;
    }

    fn merge(&mut self, device: usize, back: bool) -> Result<(), TestCaseError> {
        let [Some(main), Some(side)] = self.dbs[device].clone() else {
            return Ok(());
        };
        let (a, b) = if back { (1, 0) } else { (0, 1) };
        let (ours, theirs) = if back { (side, main) } else { (main, side) };
        let (ha, hb) = (self.heads[device][a], self.heads[device][b]);
        let best = self.graph.best(ha, hb);
        let found = ancestor_ids(&ours, &theirs);
        let back = ancestor_ids(&theirs, &ours);
        prop_assert_eq!(found.as_ref().ok(), back.as_ref().ok());
        let answered = match found {
            Ok(mut seen) => {
                seen.sort_unstable();
                prop_assert_eq!(&seen, &best);
                true
            }
            Err(ferro::Error::NoCommonAncestor) if self.keep.is_some() => {
                prop_assert_ne!(ha, hb);
                false
            }
            Err(error) => panic!("{error}"),
        };
        let id = self.graph.0.len();
        let (merged, calls) = counted_merge(&ours, &theirs, id);
        let done = match merged {
            Ok(done) => done,
            Err(ferro::Error::NoCommonAncestor) if self.keep.is_some() => {
                prop_assert!(!answered || best.len() > 1);
                return Ok(());
            }
            Err(error) => panic!("{error}"),
        };
        self.crossed.check(best.len(), &calls)?;
        if best == vec![hb] {
            prop_assert!(!done.changed);
            return Ok(());
        }
        prop_assert!(done.changed);
        self.graph.push(&[ha, hb], None);
        self.heads[device][a] = id;
        Ok(())
    }

    fn pull(&mut self, a: usize, s: usize, b: usize, t: usize) -> Result<bool, TestCaseError> {
        let (Some(ours), Some(theirs)) = (self.dbs[a][s].clone(), self.dbs[b][t].clone()) else {
            return Ok(false);
        };
        if a == b {
            return Ok(false);
        }
        let (ha, hb) = (self.heads[a][s], self.heads[b][t]);
        let best = self.graph.best(ha, hb);
        let id = self.graph.0.len();
        let mut calls = Vec::new();
        let remote = format!("d{b}{}", LOCAL[t]);
        let pulled = ours.pull(&theirs, &remote, |root, base, other| {
            add_deltas(root, base, other);
            calls.push(base.version());
            (format!("n{id}"), ())
        });
        let pulled = match pulled {
            Ok(pulled) => pulled,
            Err(ferro::Error::NoCommonAncestor) if self.keep.is_some() => return Ok(false),
            Err(error) => panic!("{error}"),
        };
        match pulled.outcome {
            Outcome::Merged => {
                prop_assert!(!self.graph.reaches(ha, hb));
                self.crossed.check(best.len(), &calls)?;
                self.graph.push(&[ha, hb], None);
                self.heads[a][s] = id;
            }
            Outcome::FastForward => {
                prop_assert!(self.graph.reaches(hb, ha));
                prop_assert!(calls.is_empty());
                self.heads[a][s] = hb;
            }
            Outcome::Ahead => {
                prop_assert!(self.graph.reaches(ha, hb));
                prop_assert!(calls.is_empty());
            }
            Outcome::UpToDate => {
                let same = self.graph.events(ha) == self.graph.events(hb);
                prop_assert!(self.graph.reaches(ha, hb) || same);
                prop_assert!(calls.is_empty());
            }
        }
        Ok(pulled.outcome == Outcome::Merged)
    }

    fn check(&self) -> Result<(), TestCaseError> {
        for (device, dbs) in self.dbs.iter().enumerate() {
            for (slot, db) in dbs.iter().enumerate() {
                let Some(db) = db else {
                    continue;
                };
                let head = self.heads[device][slot];
                prop_assert_eq!(
                    counts(db),
                    self.graph.events(head),
                    "d{} {}",
                    device,
                    LOCAL[slot]
                );
            }
        }
        Ok(())
    }
}

fn run_devices(acts: Vec<Act>, keep: Option<Keep>) -> Result<Crossed, TestCaseError> {
    let dir = tempfile::tempdir().unwrap();
    let mut devices = Devices {
        graph: Graph(Vec::new()),
        dbs: Vec::new(),
        heads: vec![[0, 0]; DEVICES],
        events: 0,
        crossed: Crossed::default(),
        keep,
    };
    devices.graph.push(&[], None);
    for device in 0..DEVICES {
        let options = match keep {
            Some(keep) => fast().history(keep).reclaim(Reclaim::off()),
            None => fast(),
        };
        let db: Db<Events> = options
            .open(dir.path().join(format!("d{device}.ferro")))
            .unwrap();
        devices.dbs.push([Some(db), None]);
    }
    for act in acts {
        match act {
            Act::Inc(device, slot) => devices.inc(device, slot),
            Act::Fork(device) => {
                if devices.dbs[device][1].is_none() {
                    let main = devices.dbs[device][0].clone().unwrap();
                    devices.dbs[device][1] = Some(main.fork(LOCAL[1]).unwrap());
                    devices.heads[device][1] = devices.heads[device][0];
                }
            }
            Act::Merge(device, back) => devices.merge(device, back)?,
            Act::Pull(a, s, b, t) => {
                devices.pull(a, s, b, t)?;
            }
            Act::Sync(a, b) => {
                devices.pull(a, 0, b, 0)?;
                if devices.pull(b, 0, a, 0)? {
                    devices.pull(a, 0, b, 0)?;
                }
            }
            Act::Cross(a, b) => {
                let c = (0..DEVICES).find(|c| *c != a && *c != b).unwrap();
                devices.pull(c, 0, a, 0)?;
                devices.pull(a, 0, b, 0)?;
                devices.pull(b, 0, c, 0)?;
            }
            Act::Compact(device) => {
                if let (Some(keep), Some(db)) = (keep, &devices.dbs[device][0]) {
                    db.compact(keep).unwrap();
                }
            }
        }
        devices.check()?;
    }
    for db in devices.dbs.iter().flatten().flatten() {
        db.verify().unwrap();
    }
    Ok(devices.crossed)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(300)))]

    #[test]
    fn merge_graphs_apply_every_delta_exactly_once(ops in prop::collection::vec(op(), 1..45)) {
        run_graph(ops, fast(), Keep::All, false)?;
    }

    #[test]
    fn pruned_merge_graphs_find_the_true_base_or_refuse(
        ops in prop::collection::vec(op(), 1..60),
        keep in 2..8usize,
    ) {
        let options = fast().history(Keep::Last(keep)).reclaim(Reclaim::off());
        run_graph(ops, options, Keep::Last(keep), true)?;
    }

    #[test]
    fn devices_that_branch_merge_and_sync_apply_every_delta_exactly_once(
        acts in prop::collection::vec(act(), 1..60),
    ) {
        run_devices(acts, None)?;
    }

    #[test]
    fn devices_that_sync_with_bounded_history_find_the_true_base_or_refuse(
        acts in prop::collection::vec(pruning_act(), 1..60),
        keep in 2..8usize,
    ) {
        run_devices(acts, Some(Keep::Last(keep)))?;
    }
}

#[test]
fn random_histories_cross_often_and_each_delta_is_still_applied_once() {
    let mut runner = TestRunner::deterministic();
    let (mut graphs, mut devices) = (Crossed::default(), Crossed::default());
    for _ in 0..common::cases(100) {
        let ops = prop::collection::vec(op(), 1..80)
            .new_tree(&mut runner)
            .unwrap()
            .current();
        graphs.add(run_graph(ops, fast(), Keep::All, false).unwrap());
        let acts = prop::collection::vec(act(), 1..80)
            .new_tree(&mut runner)
            .unwrap()
            .current();
        devices.add(run_devices(acts, None).unwrap());
    }
    assert!(graphs.merges >= 50, "{} criss-cross merges", graphs.merges);
    assert!(
        devices.merges >= 50,
        "{} criss-cross merges",
        devices.merges
    );
    assert!(graphs.calls >= 3 && devices.calls >= 3);
}

#[test]
fn branches_that_keep_syncing_merge_exactly_with_bounded_history() {
    let mut ops = Vec::new();
    for round in 0..120 {
        ops.extend([Op::Inc(0), Op::Inc(1), Op::Inc(2)]);
        ops.extend([Op::Merge(1, 0), Op::Merge(0, 1)]);
        if round % 2 == 0 {
            ops.push(Op::Merge(2, 0));
        }
        if round % 3 == 0 {
            ops.push(Op::Merge(0, 2));
        }
        if round % 10 == 9 {
            ops.push(Op::Compact);
        }
        if round == 0 {
            ops.extend([Op::Fork(0), Op::Fork(0)]);
        }
    }
    let options = fast().history(Keep::Last(40)).reclaim(Reclaim::off());
    run_graph(ops, options, Keep::Last(40), false).unwrap();
}

#[test]
fn pruned_graphs_that_once_found_a_wrong_base_or_changed_after_a_cycle() {
    use Op::*;
    let cases = [
        (
            2,
            vec![Fork(0), Inc(2), Merge(1, 2), Inc(1), Inc(1), Merge(1, 2)],
        ),
        (
            6,
            vec![
                Merge(1, 0),
                Merge(2, 2),
                Fork(2),
                Inc(4),
                Inc(4),
                Inc(3),
                Drop(3),
                Fork(2),
                Merge(1, 2),
                Drop(2),
                Merge(3, 3),
                Inc(0),
                Inc(2),
                Fork(1),
                Merge(3, 1),
                Inc(3),
                Merge(0, 2),
                Inc(1),
                Merge(0, 2),
                Inc(3),
                Compact,
                Inc(0),
                Merge(3, 4),
                Merge(0, 0),
                Merge(2, 0),
                Inc(4),
                Compact,
                Merge(0, 0),
                Merge(2, 1),
                Inc(0),
                Merge(4, 2),
                Inc(3),
                Inc(4),
                Inc(0),
                Inc(1),
                Fork(0),
                Merge(4, 2),
                Fork(1),
                Inc(3),
                Fork(2),
                Fork(2),
                Inc(2),
                Merge(0, 2),
                Inc(2),
                Fork(3),
                Fork(3),
                Compact,
                Inc(2),
                Merge(0, 2),
            ],
        ),
        (
            6,
            vec![
                Drop(1),
                Fork(1),
                Inc(4),
                Inc(0),
                Fork(0),
                Inc(1),
                Merge(4, 2),
                Merge(1, 3),
                Fork(2),
                Merge(0, 2),
                Inc(2),
                Inc(1),
                Merge(4, 1),
                Fork(1),
                Merge(4, 3),
                Inc(2),
                Fork(1),
                Compact,
                Merge(3, 2),
                Inc(2),
                Fork(0),
                Merge(2, 3),
                Merge(2, 2),
                Fork(2),
                Merge(3, 2),
                Inc(3),
                Inc(4),
                Inc(2),
                Drop(4),
                Merge(4, 2),
                Inc(1),
                Inc(1),
                Inc(4),
                Merge(1, 0),
                Inc(3),
                Merge(4, 3),
                Inc(0),
                Fork(1),
                Inc(0),
                Merge(3, 2),
                Inc(4),
                Merge(0, 4),
                Merge(0, 1),
                Compact,
                Inc(1),
            ],
        ),
        (
            7,
            vec![
                Compact,
                Inc(2),
                Fork(2),
                Merge(1, 4),
                Inc(1),
                Inc(1),
                Fork(1),
                Fork(4),
                Drop(4),
                Merge(3, 2),
                Merge(2, 2),
                Inc(3),
                Merge(2, 0),
                Compact,
                Fork(1),
                Fork(3),
                Merge(4, 1),
                Merge(0, 0),
                Merge(0, 3),
                Drop(2),
                Merge(4, 0),
                Compact,
                Inc(2),
                Merge(4, 2),
                Inc(4),
                Drop(2),
                Inc(1),
                Merge(4, 0),
                Inc(0),
                Merge(0, 1),
                Inc(3),
                Fork(0),
                Merge(3, 4),
                Inc(2),
                Merge(0, 0),
                Merge(4, 1),
                Merge(1, 3),
                Inc(3),
                Inc(2),
                Inc(0),
                Merge(1, 2),
                Fork(2),
                Fork(2),
                Merge(0, 2),
                Merge(1, 1),
                Fork(0),
                Merge(4, 2),
                Merge(1, 2),
                Merge(3, 1),
                Merge(3, 0),
                Merge(3, 4),
                Inc(0),
                Merge(3, 3),
                Drop(4),
                Inc(3),
                Fork(1),
                Merge(1, 0),
                Inc(4),
                Compact,
            ],
        ),
        (
            4,
            vec![
                Merge(3, 0),
                Fork(2),
                Merge(4, 4),
                Inc(2),
                Fork(0),
                Merge(3, 3),
                Fork(3),
                Fork(3),
                Merge(3, 0),
                Fork(3),
                Merge(4, 1),
                Merge(1, 3),
                Inc(0),
                Fork(1),
                Fork(4),
                Merge(1, 0),
                Inc(0),
                Compact,
                Fork(3),
                Inc(0),
                Fork(1),
                Merge(2, 2),
                Fork(4),
                Merge(1, 0),
                Merge(0, 1),
                Inc(2),
                Merge(3, 2),
                Inc(4),
                Fork(0),
                Inc(1),
                Merge(4, 1),
                Merge(1, 2),
                Inc(0),
                Inc(0),
                Merge(4, 2),
                Merge(4, 0),
                Inc(4),
                Fork(4),
                Inc(1),
                Compact,
                Merge(2, 2),
                Fork(3),
                Merge(2, 4),
                Inc(4),
                Merge(3, 2),
                Merge(4, 2),
                Inc(1),
                Merge(0, 3),
                Inc(0),
                Fork(1),
                Merge(4, 2),
                Fork(4),
            ],
        ),
        (
            4,
            vec![
                Fork(0),
                Inc(1),
                Fork(1),
                Fork(0),
                Inc(1),
                Inc(1),
                Fork(0),
                Drop(3),
                Cross(2, 4),
                Fork(1),
                Merge(4, 0),
                Inc(2),
                Inc(1),
                Inc(4),
                Merge(1, 2),
                Merge(4, 1),
            ],
        ),
        (
            4,
            vec![
                Inc(2),
                Inc(0),
                Inc(0),
                Fork(1),
                Fork(0),
                Inc(0),
                Fork(0),
                Merge(2, 0),
                Cross(0, 1),
                Inc(4),
                Fork(4),
                Inc(4),
                Inc(0),
                Merge(4, 2),
                Merge(0, 1),
                Inc(4),
                Drop(1),
                Inc(4),
                Inc(1),
                Fork(4),
                Merge(4, 2),
            ],
        ),
        (
            3,
            vec![
                Drop(1),
                Fork(0),
                Inc(1),
                Merge(0, 1),
                Fork(0),
                Inc(0),
                Inc(0),
                Inc(4),
                Fork(0),
                Inc(2),
                Cross(0, 2),
                Merge(3, 1),
                Merge(3, 0),
                Inc(0),
            ],
        ),
    ];
    for _ in 0..50 {
        for (keep, ops) in &cases {
            let options = fast().history(Keep::Last(*keep)).reclaim(Reclaim::off());
            run_graph(ops.clone(), options, Keep::Last(*keep), true).unwrap();
        }
    }
}

fn event(db: &Db<Events>, name: &str, event: u32) {
    db.commit(name, |root| {
        root.insert(event, 1);
    })
    .unwrap();
}

fn merge_named(db: &Db<Events>, other: &Db<Events>, name: &str) -> ferro::Committed<String> {
    db.merge_with(other, |root, base, theirs| {
        add_deltas(root, base, theirs);
        (name.to_string(), base.commit().message.clone())
    })
    .unwrap()
}

fn ones(events: &[u32]) -> Counts {
    events.iter().map(|e| (*e, 1)).collect()
}

#[test]
fn a_change_that_arrives_through_a_third_branch_is_merged_once() {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Events> = fast().open(dir.path().join("t.ferro")).unwrap();
    event(&a, "a0", 0);
    let b = a.fork("b").unwrap();
    let c = a.fork("c").unwrap();
    event(&c, "c1", 1);
    event(&b, "b1", 2);
    assert_eq!(merge_named(&b, &c, "b <- c").value, "a0");
    assert_eq!(merge_named(&a, &b, "a <- b").value, "a0");
    let again = merge_named(&a, &c, "a <- c");
    assert_eq!(again.value, "c1");
    assert!(!again.changed);
    assert_eq!(counts(&a), ones(&[0, 1, 2]));
    event(&c, "c2", 3);
    assert_eq!(merge_named(&a, &c, "a <- c again").value, "c1");
    assert_eq!(counts(&a), ones(&[0, 1, 2, 3]));
    assert_eq!(merge_named(&c, &a, "c <- a").value, "c2");
    assert_eq!(counts(&c), ones(&[0, 1, 2, 3]));
}

#[test]
fn a_fork_of_a_merged_branch_does_not_bring_its_changes_back() {
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Events> = fast().open(dir.path().join("f.ferro")).unwrap();
    event(&main, "m0", 0);
    let feature = main.fork("feature").unwrap();
    event(&feature, "f1", 1);
    event(&main, "m1", 2);
    assert_eq!(merge_named(&main, &feature, "main <- feature").value, "m0");
    let follow = feature.fork("follow").unwrap();
    event(&follow, "g1", 3);
    main.drop_branch("feature").unwrap();
    assert_eq!(merge_named(&main, &follow, "main <- follow").value, "f1");
    assert_eq!(counts(&main), ones(&[0, 1, 2, 3]));
    main.compact(Keep::All).unwrap();
    event(&follow, "g2", 4);
    assert_eq!(
        merge_named(&main, &follow, "main <- follow again").value,
        "g1"
    );
    assert_eq!(counts(&main), ones(&[0, 1, 2, 3, 4]));
    main.verify().unwrap();
}

fn messages(snapshots: &[Snapshot<Events>]) -> Vec<String> {
    snapshots
        .iter()
        .map(|s| s.commit().message.clone())
        .collect()
}

fn sorted_messages(snapshots: &[Snapshot<Events>]) -> Vec<String> {
    let mut out = messages(snapshots);
    out.sort();
    out
}

type Call = (u64, String, String);

fn traced_merge(
    db: &Db<Events>,
    other: &Db<Events>,
    name: &str,
) -> (ferro::Committed<()>, Vec<Call>) {
    let mut calls = Vec::new();
    let merged = db
        .merge_with(other, |root, base, theirs| {
            add_deltas(root, base, theirs);
            let message = |s: &Snapshot<Events>| s.commit().message.clone();
            calls.push((base.version(), message(base), message(theirs)));
            (name.to_string(), ())
        })
        .unwrap();
    (merged, calls)
}

fn call(version: u64, base: &str, theirs: &str) -> Call {
    (version, base.to_string(), theirs.to_string())
}

fn criss_cross(path: &std::path::Path) -> (Db<Events>, Db<Events>) {
    let main: Db<Events> = fast().open(path).unwrap();
    event(&main, "m0", 0);
    let side = main.fork("side").unwrap();
    event(&main, "m1", 1);
    event(&side, "s1", 2);
    event(&side, "s2", 3);
    let copy = main.fork("copy").unwrap();
    assert_eq!(merge_named(&main, &side, "main <- side").value, "m0");
    assert_eq!(merge_named(&side, &copy, "side <- copy").value, "m0");
    main.drop_branch("copy").unwrap();
    (main, side)
}

#[test]
fn criss_cross_merges_merge_the_best_ancestors_into_a_virtual_base_first() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.ferro");
    {
        let (main, side) = criss_cross(&path);
        assert_eq!(main.common_ancestor(&side).unwrap().commit().message, "s2");
        assert_eq!(
            messages(&main.common_ancestors(&side).unwrap()),
            ["s2", "m1"]
        );
        assert_eq!(
            messages(&side.common_ancestors(&main).unwrap()),
            ["s2", "m1"]
        );
    }
    let main: Db<Events> = fast().open(&path).unwrap();
    let side = main.branch("side").unwrap();
    assert_eq!(
        messages(&main.common_ancestors(&side).unwrap()),
        ["s2", "m1"]
    );
    let before = main.version();
    let (merged, calls) = traced_merge(&main, &side, "main <- side again");
    assert_eq!(
        calls,
        [
            call(1, "m0", "m1"),
            call(0, "main <- side again", "side <- copy")
        ]
    );
    assert_eq!(counts(&main), ones(&[0, 1, 2, 3]));
    assert_eq!(merged.version, before + 1);
    let log: Vec<String> = main
        .log()
        .values()
        .map(|commit| commit.message.clone())
        .collect();
    assert_eq!(log, ["m0", "m1", "main <- side", "main <- side again"]);
    let (_, calls) = traced_merge(&side, &main, "side <- main");
    assert_eq!(calls, [call(4, "side <- copy", "main <- side again")]);
    assert_eq!(counts(&side), ones(&[0, 1, 2, 3]));
    main.verify().unwrap();
}

#[test]
fn a_revert_of_what_only_one_best_ancestor_carries_survives_a_criss_cross_merge() {
    let dir = tempfile::tempdir().unwrap();
    let (main, side) = criss_cross(&dir.path().join("v.ferro"));
    main.commit("revert m1", |root| {
        root.remove(&1);
    })
    .unwrap();
    main.merge_with(&side, |root, base, theirs| {
        root.merge(base, theirs, |conflict| conflict.ours.map(|v| *v));
        ("main <- side again", ())
    })
    .unwrap();
    assert_eq!(counts(&main), ones(&[0, 2, 3]));
}

#[test]
fn an_error_while_building_the_virtual_base_discards_the_merge() {
    let dir = tempfile::tempdir().unwrap();
    let (main, side) = criss_cross(&dir.path().join("e.ferro"));
    let (before, head) = (main.version(), main.head_hash());
    let mut calls = 0;
    let failed = main.try_merge_with(&side, |root, base, theirs| {
        calls += 1;
        if base.version() != 0 && theirs.commit().message == "m1" {
            return Err(ferro::Error::BranchInUse("an ancestor".into()));
        }
        add_deltas(root, base, theirs);
        Ok(("never", ()))
    });
    assert!(matches!(failed, Err(ferro::Error::BranchInUse(_))));
    assert_eq!(calls, 1);
    assert_eq!((main.version(), main.head_hash()), (before, head));
    assert_eq!(
        messages(&main.common_ancestors(&side).unwrap()),
        ["s2", "m1"]
    );
    let (_, calls) = traced_merge(&main, &side, "main <- side again");
    assert_eq!(calls.len(), 2);
    assert_eq!(counts(&main), ones(&[0, 1, 2, 3]));
}

#[test]
fn a_criss_cross_of_criss_crosses_builds_its_virtual_base_recursively() {
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Events> = fast().open(dir.path().join("r.ferro")).unwrap();
    event(&main, "m0", 0);
    let side = main.fork("side").unwrap();
    event(&main, "m1", 1);
    event(&side, "s1", 2);
    let copy = main.fork("copy").unwrap();
    merge_named(&main, &side, "a1");
    merge_named(&side, &copy, "b1");
    main.drop_branch("copy").unwrap();
    event(&main, "m2", 3);
    event(&side, "s2", 4);
    let copy = main.fork("copy").unwrap();
    merge_named(&main, &side, "a2");
    merge_named(&side, &copy, "b2");
    main.drop_branch("copy").unwrap();
    assert_eq!(
        sorted_messages(&main.common_ancestors(&side).unwrap()),
        ["m2", "s2"]
    );
    let (_, calls) = traced_merge(&main, &side, "a3");
    assert_eq!(calls.len(), 3, "{calls:?}");
    let first = [call(1, "m0", "m1"), call(1, "m0", "s1")];
    assert!(first.contains(&calls[0]), "{calls:?}");
    assert!(
        [call(0, "a3", "m2"), call(0, "a3", "s2")].contains(&calls[1]),
        "{calls:?}"
    );
    assert_eq!(calls[2], call(0, "a3", "b2"));
    assert_eq!(counts(&main), ones(&[0, 1, 2, 3, 4]));
    let (_, calls) = traced_merge(&side, &main, "b3");
    assert_eq!(calls, [call(5, "b2", "a3")]);
    assert_eq!(counts(&side), ones(&[0, 1, 2, 3, 4]));
}

#[test]
fn a_merge_after_rounds_of_three_branches_merging_each_other_builds_each_virtual_base_once() {
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Events> = fast().open(dir.path().join("w.ferro")).unwrap();
    event(&main, "x0", 0);
    let branches = [
        main.clone(),
        main.fork("b").unwrap(),
        main.fork("c").unwrap(),
    ];
    let rounds = 10u32;
    let mut events = vec![0];
    for round in 0..rounds {
        for (at, branch) in branches.iter().enumerate() {
            let key = 1 + round * 3 + at as u32;
            event(branch, &format!("e{key}"), key);
            events.push(key);
        }
        let copies: Vec<Db<Events>> = branches
            .iter()
            .enumerate()
            .map(|(at, branch)| branch.fork(&format!("r{round}-{at}")).unwrap())
            .collect();
        for (at, branch) in branches.iter().enumerate() {
            for (other, copy) in copies.iter().enumerate() {
                if other != at {
                    merge_named(branch, copy, &format!("r{round}: {at} <- {other}"));
                }
            }
        }
        for (at, copy) in copies.into_iter().enumerate() {
            drop(copy);
            main.drop_branch(&format!("r{round}-{at}")).unwrap();
        }
    }
    assert_eq!(main.common_ancestors(&branches[1]).unwrap().len(), 3);
    let (_, calls) = traced_merge(&main, &branches[1], "last");
    assert_eq!(calls.len(), 2 * rounds as usize + 1);
    assert_eq!(counts(&main), ones(&events));
    main.verify_typed().unwrap();
}

fn pull_named(db: &Db<Events>, from: &Db<Events>, remote: &str, name: &str) -> (Outcome, usize) {
    let mut calls = 0;
    let pulled = db
        .pull(from, remote, |root, base, theirs| {
            calls += 1;
            add_deltas(root, base, theirs);
            (name.to_string(), ())
        })
        .unwrap();
    (pulled.outcome, calls)
}

#[test]
fn a_criss_cross_made_by_syncs_between_three_files_applies_each_change_once() {
    let dir = tempfile::tempdir().unwrap();
    let open = |name: &str| -> Db<Events> { fast().open(dir.path().join(name)).unwrap() };
    let (a, b, c) = (open("a.ferro"), open("b.ferro"), open("c.ferro"));
    event(&a, "a0", 0);
    assert_eq!(pull_named(&b, &a, "a", "b <- a"), (Outcome::FastForward, 0));
    event(&a, "a1", 1);
    event(&b, "b1", 2);
    assert_eq!(pull_named(&c, &a, "a", "c <- a"), (Outcome::FastForward, 0));
    assert_eq!(pull_named(&a, &b, "b", "a <- b"), (Outcome::Merged, 1));
    assert_eq!(pull_named(&b, &c, "c", "b <- c"), (Outcome::Merged, 1));
    assert_eq!(counts(&a), ones(&[0, 1, 2]));
    assert_eq!(counts(&b), ones(&[0, 1, 2]));
    event(&a, "a2", 3);
    event(&b, "b2", 4);
    assert_eq!(
        pull_named(&a, &b, "b", "a <- b again"),
        (Outcome::Merged, 2)
    );
    assert_eq!(counts(&a), ones(&[0, 1, 2, 3, 4]));
    assert_eq!(
        pull_named(&b, &a, "a", "b <- a again"),
        (Outcome::Merged, 1)
    );
    assert_eq!(counts(&b), ones(&[0, 1, 2, 3, 4]));
    assert_eq!(
        pull_named(&a, &b, "b", "a <- b last"),
        (Outcome::UpToDate, 0)
    );
    assert_eq!(
        pull_named(&b, &a, "a", "b <- a last"),
        (Outcome::UpToDate, 0)
    );
}

fn pairs(dbs: &[Db<Events>]) -> Vec<String> {
    let mut out = Vec::new();
    for a in dbs {
        for b in dbs {
            let names = format!("{} {}", a.branch_name(), b.branch_name());
            out.push(match a.common_ancestor(b) {
                Ok(base) => format!("{names} {}", base.commit().message),
                Err(error) => format!("{names} {error}"),
            });
        }
    }
    out
}

#[test]
fn merge_bases_survive_pruning_and_do_not_change_when_a_cycle_reclaims() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.ferro");
    let options = || fast().history(Keep::Last(4)).reclaim(Reclaim::off());
    let main: Db<Events> = options().open(&path).unwrap();
    let mut next = 0;
    let mut bump = |db: &Db<Events>| {
        event(db, &format!("e{next}"), next);
        next += 1;
    };
    bump(&main);
    let a = main.fork("a").unwrap();
    let b = main.fork("b").unwrap();
    let merge = |db: &Db<Events>, other: &Db<Events>, name: String| {
        let merged = db.merge_with(other, |root, base, theirs| {
            add_deltas(root, base, theirs);
            (name.clone(), ())
        });
        match merged {
            Ok(_) | Err(ferro::Error::NoCommonAncestor) => {}
            Err(other) => panic!("{other}"),
        }
    };
    for round in 0..12 {
        bump(&a);
        bump(&b);
        bump(&main);
        if round % 3 == 0 {
            merge(&main, &a, format!("main <- a {round}"));
        }
        if round % 4 == 1 {
            merge(&b, &main, format!("b <- main {round}"));
        }
        if round % 5 == 2 {
            merge(&a, &b, format!("a <- b {round}"));
        }
    }
    let dbs = vec![main.clone(), a.clone(), b.clone()];
    let before = pairs(&dbs);
    assert!(
        before
            .iter()
            .any(|line| !line.ends_with("share no retained version")),
        "{before:?}"
    );
    main.compact(Keep::Last(4)).unwrap();
    assert_eq!(pairs(&dbs), before);
    main.verify().unwrap();
    drop((main, a, b, dbs));
    let main: Db<Events> = options().open(&path).unwrap();
    let dbs = vec![
        main.clone(),
        main.branch("a").unwrap(),
        main.branch("b").unwrap(),
    ];
    assert_eq!(pairs(&dbs), before);
}

fn crossed_under_pruning(path: &std::path::Path, keep: usize, step: u64) -> [Db<Events>; 3] {
    let ticks = Arc::new(AtomicU64::new(1_700_000_000_000));
    let clock = Clock(Arc::new(move || ticks.fetch_add(step, Ordering::Relaxed)));
    let options = fast().history(Keep::Last(keep)).reclaim(Reclaim::off());
    let main: Db<Events> = options.clock(clock).open(path).unwrap();
    let b1 = main.fork("b1").unwrap();
    event(&b1, "n1", 1);
    let b2 = b1.fork("b2").unwrap();
    event(&b1, "n2", 2);
    event(&b1, "n3", 3);
    let copy = b2.fork("copy").unwrap();
    merge_named(&main, &copy, "n4");
    main.drop_branch("copy").unwrap();
    let b4 = b1.fork("b4").unwrap();
    assert_eq!(merge_named(&b4, &main, "n5").value, "n1");
    event(&b2, "n6", 6);
    event(&b1, "n7", 7);
    event(&b4, "n8", 8);
    assert_eq!(merge_named(&b1, &b2, "n9").value, "n1");
    [main, b1, b4]
}

#[test]
fn a_pruned_search_does_not_report_a_base_below_another_and_the_merge_goes_through() {
    for step in [0, 1] {
        let dir = tempfile::tempdir().unwrap();
        let [main, b1, b4] = crossed_under_pruning(&dir.path().join("b.ferro"), 4, step);
        assert_eq!(
            messages(&b4.common_ancestors(&b1).unwrap()),
            ["n3"],
            "step {step}"
        );
        assert_eq!(
            messages(&b1.common_ancestors(&b4).unwrap()),
            ["n3"],
            "step {step}"
        );
        let (_, calls) = traced_merge(&b4, &b1, "n10");
        assert_eq!(calls, [call(3, "n3", "n9")], "step {step}");
        assert_eq!(counts(&b4), ones(&[1, 2, 3, 6, 7, 8]));
        main.verify().unwrap();
    }
}

#[test]
fn a_base_pruned_from_our_branch_but_kept_on_theirs_is_found() {
    let dir = tempfile::tempdir().unwrap();
    let options = fast().history(Keep::Last(3)).reclaim(Reclaim::off());
    let main: Db<Events> = options.open(dir.path().join("k.ferro")).unwrap();
    event(&main, "m1", 1);
    let side = main.fork("side").unwrap();
    for (name, key) in [("m2", 2), ("m3", 3), ("m4", 4)] {
        event(&main, name, key);
    }
    event(&side, "s1", 5);
    assert_eq!(main.log().first().unwrap().0, 2);
    assert_eq!(messages(&main.common_ancestors(&side).unwrap()), ["m1"]);
    assert_eq!(messages(&side.common_ancestors(&main).unwrap()), ["m1"]);
    assert_eq!(merge_named(&main, &side, "main <- side").value, "m1");
    assert_eq!(counts(&main), ones(&[1, 2, 3, 4, 5]));
    main.compact(Keep::Last(3)).unwrap();
    assert_eq!(merge_named(&side, &main, "side <- main").value, "s1");
    assert_eq!(counts(&side), ones(&[1, 2, 3, 4, 5]));
    main.verify().unwrap();
}

#[test]
fn a_pruned_search_refuses_when_a_base_could_lie_under_history_no_branch_keeps() {
    for step in [0, 1] {
        let dir = tempfile::tempdir().unwrap();
        let [main, b1, b4] = crossed_under_pruning(&dir.path().join("h.ferro"), 3, step);
        let refused = |found: ferro::Result<Vec<Snapshot<Events>>>| {
            matches!(found, Err(ferro::Error::NoCommonAncestor))
        };
        assert!(refused(b4.common_ancestors(&b1)), "step {step}");
        assert!(refused(b1.common_ancestors(&b4)), "step {step}");
        let (before, head) = (b4.version(), b4.head_hash());
        let merged = b4.merge_with(&b1, |root, base, theirs| {
            add_deltas(root, base, theirs);
            ("n10", ())
        });
        assert!(
            matches!(merged, Err(ferro::Error::NoCommonAncestor)),
            "step {step}"
        );
        assert_eq!((b4.version(), b4.head_hash()), (before, head));
        main.verify().unwrap();
    }
}

fn ticking() -> Clock {
    let ticks = Arc::new(AtomicU64::new(1_700_000_000_000));
    Clock(Arc::new(move || ticks.fetch_add(1, Ordering::Relaxed)))
}

fn pulled_then_forked(
    dir: &std::path::Path,
    history: Keep,
    compact: Option<Keep>,
    after: u32,
) -> [Db<Events>; 3] {
    let clock = ticking();
    let main: Db<Events> = fast()
        .history(history)
        .reclaim(Reclaim::off())
        .clock(clock.clone())
        .open(dir.join("a.ferro"))
        .unwrap();
    let remote: Db<Events> = fast().clock(clock).open(dir.join("b.ferro")).unwrap();
    for key in 0..3 {
        event(&main, &format!("m{key}"), key);
    }
    assert_eq!(
        pull_named(&remote, &main, "a", "never").0,
        Outcome::FastForward
    );
    event(&remote, "r1", 10);
    event(&main, "m3", 3);
    assert_eq!(
        pull_named(&main, &remote, "remote", "pull"),
        (Outcome::Merged, 1)
    );
    for key in 0..after {
        event(&main, &format!("x{key}"), 20 + key);
    }
    if let Some(keep) = compact {
        main.compact(keep).unwrap();
    }
    let side = main.fork("side").unwrap();
    event(&side, "s1", 30);
    event(&main, "m4", 31);
    [main, side, remote]
}

#[test]
fn a_fork_after_a_pull_whose_merge_left_the_kept_window_merges_against_the_fork_point() {
    let kept = (2..=4).map(|keep| (Keep::Last(keep), None));
    let compacted = (1..=3).map(|keep| (Keep::All, Some(Keep::Last(keep))));
    for (history, compact) in kept.chain(compacted) {
        for after in 0..4 {
            let shape = format!("{history:?} compacted to {compact:?}, {after} after the pull");
            let dir = tempfile::tempdir().unwrap();
            let [main, side, remote] = pulled_then_forked(dir.path(), history, compact, after);
            let fork = match after {
                0 => "pull".to_string(),
                _ => format!("x{}", after - 1),
            };
            assert_eq!(
                messages(&main.common_ancestors(&side).unwrap()),
                [fork.as_str()],
                "{shape}"
            );
            assert_eq!(
                messages(&side.common_ancestors(&main).unwrap()),
                [fork.as_str()],
                "{shape}"
            );
            let mut all: Vec<u32> = vec![0, 1, 2, 3, 10, 30, 31];
            all.extend((0..after).map(|key| 20 + key));
            assert_eq!(merge_named(&main, &side, "main <- side").value, fork);
            assert_eq!(counts(&main), ones(&all), "{shape}");
            assert_eq!(merge_named(&side, &main, "side <- main").value, "s1");
            assert_eq!(counts(&side), ones(&all), "{shape}");
            event(&remote, "r2", 11);
            event(&main, "m5", 32);
            all.extend([11, 32]);
            assert_eq!(
                pull_named(&main, &remote, "remote", "pull again"),
                (Outcome::Merged, 1),
                "{shape}"
            );
            assert_eq!(counts(&main), ones(&all), "{shape}");
            assert_eq!(
                pull_named(&remote, &main, "a", "pull back"),
                (Outcome::Merged, 1),
                "{shape}"
            );
            assert_eq!(counts(&remote), ones(&all), "{shape}");
            main.verify().unwrap();
            remote.verify().unwrap();
        }
    }
}

#[test]
fn a_criss_cross_after_a_pull_pruned_past_its_merge_builds_its_virtual_base() {
    let dir = tempfile::tempdir().unwrap();
    let [main, side, _remote] = pulled_then_forked(dir.path(), Keep::Last(3), None, 1);
    let copy = main.fork("copy").unwrap();
    assert_eq!(merge_named(&main, &side, "main <- side").value, "x0");
    assert_eq!(merge_named(&side, &copy, "side <- copy").value, "x0");
    main.drop_branch("copy").unwrap();
    assert_eq!(main.log().first().unwrap().0, 6);
    assert_eq!(
        sorted_messages(&main.common_ancestors(&side).unwrap()),
        ["m4", "s1"]
    );
    let (_, calls) = traced_merge(&main, &side, "main <- side again");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1, "x0");
    assert_eq!(counts(&main), ones(&[0, 1, 2, 3, 10, 20, 30, 31]));
    main.verify().unwrap();
}

fn refuses_to_merge(main: &Db<Events>, side: &Db<Events>) {
    for (ours, theirs) in [(main, side), (side, main)] {
        assert!(matches!(
            ours.common_ancestors(theirs),
            Err(ferro::Error::NoCommonAncestor)
        ));
    }
    let (before, head) = (main.version(), main.head_hash());
    let merged = main.merge_with(side, |root, base, theirs| {
        add_deltas(root, base, theirs);
        ("never", ())
    });
    assert!(matches!(merged, Err(ferro::Error::NoCommonAncestor)));
    assert_eq!((main.version(), main.head_hash()), (before, head));
}

#[test]
fn a_fork_point_pruned_after_a_pull_is_refused_and_not_answered_with_the_pulled_commit() {
    let dir = tempfile::tempdir().unwrap();
    let [main, side, _remote] = pulled_then_forked(dir.path(), Keep::All, None, 1);
    main.compact(Keep::Last(1)).unwrap();
    assert_eq!(main.log().first().unwrap().1.message, "m4");
    assert_eq!(side.log().first().unwrap().1.message, "s1");
    refuses_to_merge(&main, &side);
    main.verify().unwrap();
}

#[test]
fn a_commit_two_branches_pulled_apart_is_refused_when_pruned_history_could_hold_it() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ticking();
    let open = |name: &str| -> Db<Events> {
        fast()
            .reclaim(Reclaim::off())
            .clock(clock.clone())
            .open(dir.path().join(name))
            .unwrap()
    };
    let (main, remote) = (open("a.ferro"), open("b.ferro"));
    for key in 0..3 {
        event(&main, &format!("m{key}"), key);
    }
    assert_eq!(
        pull_named(&remote, &main, "a", "never").0,
        Outcome::FastForward
    );
    event(&remote, "r1", 10);
    for key in 3..6 {
        event(&main, &format!("m{key}"), key);
    }
    let side = main.fork("side").unwrap();
    assert_eq!(
        pull_named(&main, &remote, "remote", "main pull"),
        (Outcome::Merged, 1)
    );
    assert_eq!(
        pull_named(&side, &remote, "remote", "side pull"),
        (Outcome::Merged, 1)
    );
    event(&main, "m6", 6);
    event(&side, "s1", 7);
    assert_eq!(
        messages(&main.common_ancestors(&side).unwrap()),
        ["m5", "r1"]
    );
    main.compact(Keep::Last(3)).unwrap();
    assert_eq!(main.log().first().unwrap().1.message, "m5");
    refuses_to_merge(&main, &side);
    main.verify().unwrap();
}

#[test]
fn a_later_merge_does_not_reach_history_an_earlier_merge_let_go() {
    let dir = tempfile::tempdir().unwrap();
    let options = fast().reclaim(Reclaim::off());
    let main: Db<Events> = options.open(dir.path().join("l.ferro")).unwrap();
    let pause = || std::thread::sleep(std::time::Duration::from_millis(3));
    let fresh = main.fork("fresh").unwrap();
    event(&main, "m1", 1);
    pause();
    event(&main, "m2", 2);
    let side = main.fork("side").unwrap();
    pause();
    event(&side, "s1", 3);
    pause();
    event(&main, "m3", 4);
    pause();
    merge_named(&side, &main, "side <- main");
    main.compact(Keep::Last(2)).unwrap();
    event(&fresh, "f1", 5);
    merge_named(&fresh, &side, "fresh <- side");
    let dbs = vec![main.clone(), side.clone(), fresh.clone()];
    let before = pairs(&dbs);
    assert!(
        before.iter().all(|line| !line.contains("corrupt")),
        "{before:?}"
    );
    main.compact(Keep::Last(2)).unwrap();
    assert_eq!(pairs(&dbs), before);
    main.verify().unwrap();
}

#[test]
fn a_merged_branch_dropped_and_reclaimed_still_answers_later_merges() {
    let dir = tempfile::tempdir().unwrap();
    let options = Options::new().reclaim(Reclaim::off());
    let main: Db<Events> = options.open(dir.path().join("d.ferro")).unwrap();
    event(&main, "m0", 0);
    let late = main.fork("late").unwrap();
    let feature = main.fork("feature").unwrap();
    event(&feature, "f1", 1);
    event(&feature, "f2", 2);
    event(&late, "l1", 3);
    merge_named(&late, &feature, "late <- feature");
    merge_named(&main, &feature, "main <- feature");
    main.drop_branch("feature").unwrap();
    main.compact(Keep::All).unwrap();
    main.verify().unwrap();
    assert_eq!(main.common_ancestor(&late).unwrap().commit().message, "f2");
    assert_eq!(merge_named(&main, &late, "main <- late").value, "f2");
    assert_eq!(counts(&main), ones(&[0, 1, 2, 3]));
}
