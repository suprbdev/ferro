mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::fast;
use ferro::{Conflict, Data, Db, Keep, Map, Options, Reclaim, Record, Snapshot, Table};
use proptest::prelude::*;

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

fn run_branches(steps: Vec<Step>, exact: bool) -> Result<(), TestCaseError> {
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Counters> = fast().open(dir.path().join("m.ferro")).unwrap();
    let mut world = World {
        nodes: vec![Node { mask: Mask::new() }],
        logs: vec![None; 4],
        dbs: vec![None, None, None, None],
        events: 0,
    };
    world.logs[0] = Some(vec![]);
    if exact {
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
                if !exact {
                    let mut mask = mask_of(&world.nodes[ha].mask);
                    for (k, v) in mask_of(&world.nodes[hb].mask) {
                        let slot = mask.get_mut(&k).unwrap();
                        *slot = (*slot).max(v);
                    }
                    let got = values(&da);
                    for k in 0..KEYS {
                        prop_assert!(got[&k] >= mask[&k], "lost a change merging {} <- {}", a, b);
                    }
                    if done.version != before {
                        world.push(a, got);
                    }
                    continue;
                }
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
    fn many_branch_merges_never_lose_a_change(steps in prop::collection::vec(step(), 1..30)) {
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
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0..SLOTS).prop_map(Op::Inc),
        7 => (0..SLOTS, 0..SLOTS).prop_map(|(a, b)| Op::Merge(a, b)),
        3 => (0..SLOTS).prop_map(Op::Fork),
        1 => (1..SLOTS).prop_map(Op::Drop),
        1 => Just(Op::Compact),
    ]
}

struct Version {
    generation: u64,
    counts: Counts,
    event: Option<u32>,
    exact: bool,
    ancestors: Vec<u64>,
}

fn has(bits: &[u64], id: usize) -> bool {
    bits.get(id / 64).is_some_and(|word| word & (1 << (id % 64)) != 0)
}

struct Graph(Vec<Version>);

impl Graph {
    fn push(
        &mut self,
        parents: &[usize],
        generation: u64,
        counts: Counts,
        event: Option<u32>,
        exact: bool,
    ) {
        let id = self.0.len();
        let mut ancestors = vec![0u64; id / 64 + 1];
        for parent in parents {
            for (word, bits) in ancestors.iter_mut().zip(&self.0[*parent].ancestors) {
                *word |= bits;
            }
        }
        ancestors[id / 64] |= 1 << (id % 64);
        self.0.push(Version {
            generation,
            counts,
            event,
            exact,
            ancestors,
        });
    }

    fn best(&self, a: usize, b: usize) -> Vec<usize> {
        let common: Vec<usize> = (0..self.0.len())
            .filter(|id| has(&self.0[a].ancestors, *id) && has(&self.0[b].ancestors, *id))
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
            .filter(|v| has(&self.0[id].ancestors, *v))
            .filter_map(|v| self.0[v].event)
            .map(|e| (e, 1))
            .collect()
    }
}

fn counts(db: &Db<Events>) -> Counts {
    db.read().iter().map(|(k, v)| (k, *v)).collect()
}

fn id_of(snapshot: &Snapshot<Events>) -> usize {
    snapshot.commit().message.strip_prefix('n').map_or(0, |id| id.parse().unwrap())
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

fn event_merge(db: &Db<Events>, other: &Db<Events>, id: usize) -> ferro::Committed<usize> {
    db.merge_with(other, |root, base, theirs| {
        add_deltas(root, base, theirs);
        (format!("n{id}"), id_of(base))
    })
    .unwrap()
}

fn bases(dbs: &[Db<Events>]) -> Vec<Option<usize>> {
    let mut out = Vec::new();
    for a in dbs {
        for b in dbs {
            out.push(a.common_ancestor(b).as_ref().map(id_of).ok());
        }
    }
    out
}

fn run_graph(
    ops: Vec<Op>,
    options: Options,
    keep: Keep,
    pruned: bool,
) -> Result<(), TestCaseError> {
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Events> = options.open(dir.path().join("g.ferro")).unwrap();
    let mut graph = Graph(Vec::new());
    graph.push(&[], 0, Counts::new(), None, true);
    let mut heads: Vec<Option<usize>> = vec![None; SLOTS];
    let mut dbs: Vec<Option<Db<Events>>> = vec![None; SLOTS];
    heads[0] = Some(0);
    dbs[0] = Some(main.clone());
    let mut events = 0u32;
    for op in ops {
        let live: Vec<usize> = (0..SLOTS).filter(|slot| dbs[*slot].is_some()).collect();
        let pick = |index: usize| live[index % live.len()];
        match op {
            Op::Inc(index) => {
                let slot = pick(index);
                let db = dbs[slot].clone().unwrap();
                let (id, event, head) = (graph.0.len(), events, heads[slot].unwrap());
                events += 1;
                db.commit(format!("n{id}"), |root| {
                    root.insert(event, 1);
                })
                .unwrap();
                let mut counts = graph.0[head].counts.clone();
                counts.insert(event, 1);
                let (generation, exact) = (graph.0[head].generation + 1, graph.0[head].exact);
                graph.push(&[head], generation, counts, Some(event), exact);
                heads[slot] = Some(id);
            }
            Op::Fork(index) => {
                let from = pick(index);
                let Some(slot) = (1..SLOTS).find(|slot| dbs[*slot].is_none()) else {
                    continue;
                };
                dbs[slot] = Some(dbs[from].as_ref().unwrap().fork(&format!("b{slot}")).unwrap());
                heads[slot] = heads[from];
            }
            Op::Drop(index) => {
                let others: Vec<usize> = live.iter().copied().filter(|slot| *slot != 0).collect();
                if others.is_empty() {
                    continue;
                }
                let slot = others[index % others.len()];
                dbs[slot] = None;
                heads[slot] = None;
                main.drop_branch(&format!("b{slot}")).unwrap();
            }
            Op::Compact => {
                let open: Vec<Db<Events>> = dbs.iter().flatten().cloned().collect();
                let before = bases(&open);
                main.compact(keep).unwrap();
                prop_assert_eq!(bases(&open), before);
            }
            Op::Merge(a, b) => {
                let (a, b) = (pick(a), pick(b));
                if a == b {
                    continue;
                }
                let (ours, theirs) = (dbs[a].clone().unwrap(), dbs[b].clone().unwrap());
                let (ha, hb) = (heads[a].unwrap(), heads[b].unwrap());
                let best = graph.best(ha, hb);
                let found = ours.common_ancestor(&theirs);
                prop_assert_eq!(
                    found.as_ref().map(id_of).ok(),
                    theirs.common_ancestor(&ours).as_ref().map(id_of).ok()
                );
                let seen = match found {
                    Ok(base) => id_of(&base),
                    Err(ferro::Error::NoCommonAncestor) if pruned => {
                        let failed = ours.merge_with(&theirs, |_, _, _| ("never", ()));
                        prop_assert!(matches!(failed, Err(ferro::Error::NoCommonAncestor)));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                prop_assert!(best.contains(&seen), "base {} is not one of {:?}", seen, best);
                let top = best.iter().map(|v| graph.0[*v].generation).max().unwrap();
                prop_assert_eq!(graph.0[seen].generation, top);
                let id = graph.0.len();
                let before = ours.version();
                let done = event_merge(&ours, &theirs, id);
                prop_assert_eq!(done.value, seen);
                if best == vec![hb] {
                    prop_assert!(!done.changed);
                    prop_assert_eq!(ours.version(), before);
                    continue;
                }
                prop_assert!(done.changed);
                let mut counts = graph.0[ha].counts.clone();
                for (event, count) in &graph.0[hb].counts {
                    *counts.entry(*event).or_default() += count;
                }
                for (event, count) in &graph.0[seen].counts {
                    *counts.entry(*event).or_default() -= count;
                }
                counts.retain(|_, count| *count != 0);
                let generation = 1 + graph.0[ha].generation.max(graph.0[hb].generation);
                let exact = graph.0[ha].exact && graph.0[hb].exact && best.len() == 1;
                graph.push(&[ha, hb], generation, counts, None, exact);
                heads[a] = Some(id);
            }
        }
        for slot in 0..SLOTS {
            let Some(db) = &dbs[slot] else {
                continue;
            };
            let head = heads[slot].unwrap();
            prop_assert_eq!(counts(db), graph.0[head].counts.clone(), "slot {}", slot);
            if graph.0[head].exact {
                prop_assert_eq!(graph.0[head].counts.clone(), graph.events(head), "slot {}", slot);
            }
        }
    }
    main.verify().unwrap();
    Ok(())
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
        let options = fast().history(Keep::Last(keep)).reclaim(Reclaim { every_bytes: 0 });
        run_graph(ops, options, Keep::Last(keep), true)?;
    }
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
    let options = fast().history(Keep::Last(40)).reclaim(Reclaim { every_bytes: 0 });
    run_graph(ops, options, Keep::Last(40), false).unwrap();
}

#[test]
fn pruned_graphs_that_once_found_a_wrong_base_or_changed_after_a_cycle() {
    use Op::*;
    let cases = [
        (2, vec![Fork(0), Inc(2), Merge(1, 2), Inc(1), Inc(1), Merge(1, 2)]),
        (
            6,
            vec![
                Merge(1, 0), Merge(2, 2), Fork(2), Inc(4), Inc(4), Inc(3), Drop(3), Fork(2),
                Merge(1, 2), Drop(2), Merge(3, 3), Inc(0), Inc(2), Fork(1), Merge(3, 1), Inc(3),
                Merge(0, 2), Inc(1), Merge(0, 2), Inc(3), Compact, Inc(0), Merge(3, 4), Merge(0, 0),
                Merge(2, 0), Inc(4), Compact, Merge(0, 0), Merge(2, 1), Inc(0), Merge(4, 2), Inc(3),
                Inc(4), Inc(0), Inc(1), Fork(0), Merge(4, 2), Fork(1), Inc(3), Fork(2), Fork(2),
                Inc(2), Merge(0, 2), Inc(2), Fork(3), Fork(3), Compact, Inc(2), Merge(0, 2),
            ],
        ),
        (
            6,
            vec![
                Drop(1), Fork(1), Inc(4), Inc(0), Fork(0), Inc(1), Merge(4, 2), Merge(1, 3),
                Fork(2), Merge(0, 2), Inc(2), Inc(1), Merge(4, 1), Fork(1), Merge(4, 3), Inc(2),
                Fork(1), Compact, Merge(3, 2), Inc(2), Fork(0), Merge(2, 3), Merge(2, 2), Fork(2),
                Merge(3, 2), Inc(3), Inc(4), Inc(2), Drop(4), Merge(4, 2), Inc(1), Inc(1), Inc(4),
                Merge(1, 0), Inc(3), Merge(4, 3), Inc(0), Fork(1), Inc(0), Merge(3, 2), Inc(4),
                Merge(0, 4), Merge(0, 1), Compact, Inc(1),
            ],
        ),
        (
            7,
            vec![
                Compact, Inc(2), Fork(2), Merge(1, 4), Inc(1), Inc(1), Fork(1), Fork(4), Drop(4),
                Merge(3, 2), Merge(2, 2), Inc(3), Merge(2, 0), Compact, Fork(1), Fork(3),
                Merge(4, 1), Merge(0, 0), Merge(0, 3), Drop(2), Merge(4, 0), Compact, Inc(2),
                Merge(4, 2), Inc(4), Drop(2), Inc(1), Merge(4, 0), Inc(0), Merge(0, 1), Inc(3),
                Fork(0), Merge(3, 4), Inc(2), Merge(0, 0), Merge(4, 1), Merge(1, 3), Inc(3),
                Inc(2), Inc(0), Merge(1, 2), Fork(2), Fork(2), Merge(0, 2), Merge(1, 1), Fork(0),
                Merge(4, 2), Merge(1, 2), Merge(3, 1), Merge(3, 0), Merge(3, 4), Inc(0),
                Merge(3, 3), Drop(4), Inc(3), Fork(1), Merge(1, 0), Inc(4), Compact,
            ],
        ),
        (
            4,
            vec![
                Merge(3, 0), Fork(2), Merge(4, 4), Inc(2), Fork(0), Merge(3, 3), Fork(3), Fork(3),
                Merge(3, 0), Fork(3), Merge(4, 1), Merge(1, 3), Inc(0), Fork(1), Fork(4),
                Merge(1, 0), Inc(0), Compact, Fork(3), Inc(0), Fork(1), Merge(2, 2), Fork(4),
                Merge(1, 0), Merge(0, 1), Inc(2), Merge(3, 2), Inc(4), Fork(0), Inc(1),
                Merge(4, 1), Merge(1, 2), Inc(0), Inc(0), Merge(4, 2), Merge(4, 0), Inc(4),
                Fork(4), Inc(1), Compact, Merge(2, 2), Fork(3), Merge(2, 4), Inc(4), Merge(3, 2),
                Merge(4, 2), Inc(1), Merge(0, 3), Inc(0), Fork(1), Merge(4, 2), Fork(4),
            ],
        ),
    ];
    for _ in 0..50 {
        for (keep, ops) in &cases {
            let options = fast().history(Keep::Last(*keep)).reclaim(Reclaim { every_bytes: 0 });
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
    assert_eq!(merge_named(&main, &follow, "main <- follow again").value, "g1");
    assert_eq!(counts(&main), ones(&[0, 1, 2, 3, 4]));
    main.verify().unwrap();
}

#[test]
fn criss_cross_merges_take_the_best_base_with_the_highest_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.ferro");
    {
        let main: Db<Events> = fast().open(&path).unwrap();
        event(&main, "m0", 0);
        let side = main.fork("side").unwrap();
        event(&main, "m1", 1);
        event(&side, "s1", 2);
        event(&side, "s2", 3);
        let copy = main.fork("copy").unwrap();
        assert_eq!(merge_named(&main, &side, "main <- side").value, "m0");
        assert_eq!(merge_named(&side, &copy, "side <- copy").value, "m0");
        main.drop_branch("copy").unwrap();
        assert_eq!(main.common_ancestor(&side).unwrap().commit().message, "s2");
        assert_eq!(side.common_ancestor(&main).unwrap().commit().message, "s2");
    }
    let main: Db<Events> = fast().open(&path).unwrap();
    let side = main.branch("side").unwrap();
    assert_eq!(main.common_ancestor(&side).unwrap().commit().message, "s2");
    let merged = merge_named(&main, &side, "main <- side again");
    assert_eq!(merged.value, "s2");
    assert_eq!(counts(&main), [(0, 1), (1, 2), (2, 1), (3, 1)].into_iter().collect());
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
    let options = || fast().history(Keep::Last(4)).reclaim(Reclaim { every_bytes: 0 });
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
            (name, ())
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
    assert!(before.iter().any(|line| !line.ends_with("share no retained version")), "{before:?}");
    main.compact(Keep::Last(4)).unwrap();
    assert_eq!(pairs(&dbs), before);
    main.verify().unwrap();
    drop((main, a, b, dbs));
    let main: Db<Events> = options().open(&path).unwrap();
    let dbs = vec![main.clone(), main.branch("a").unwrap(), main.branch("b").unwrap()];
    assert_eq!(pairs(&dbs), before);
}

#[test]
fn a_later_merge_does_not_reach_history_an_earlier_merge_let_go() {
    let dir = tempfile::tempdir().unwrap();
    let options = fast().reclaim(Reclaim { every_bytes: 0 });
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
    assert!(before.iter().all(|line| !line.contains("corrupt")), "{before:?}");
    main.compact(Keep::Last(2)).unwrap();
    assert_eq!(pairs(&dbs), before);
    main.verify().unwrap();
}

#[test]
fn a_merged_branch_dropped_and_reclaimed_still_answers_later_merges() {
    let dir = tempfile::tempdir().unwrap();
    let options = Options::new().reclaim(Reclaim { every_bytes: 0 });
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
