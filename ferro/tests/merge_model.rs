mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::fast;
use ferro::{Conflict, Data, Db, Map, Record, Table};
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
