mod common;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use common::{Rng, cases, fast, text};
use ferro::codec::{Encoder, HashSink, Object};
use ferro::db::{Clock, Hook};
use ferro::store::{Op, Store, StoreOptions, Trace};
use ferro::{
    Bundle, Data, Db, Error, Hash, Have, Keep, Map, Options, Outcome, Ptr, Pulled, Reclaim, Record,
    Snapshot, Summary, Table,
};
use parking_lot::Mutex;
use tempfile::TempDir;

type Root = Map<u64, String>;

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Board {
    cells: Map<u64, (u64, u64, String)>,
    clock: u64,
}

const LOG_START: usize = 8192;

fn open(dir: &TempDir, name: &str) -> Db<Root> {
    fast().open(dir.path().join(name)).unwrap()
}

fn dump(root: &Root) -> BTreeMap<u64, String> {
    root.iter()
        .map(|(key, value)| (key, (*value).clone()))
        .collect()
}

fn theirs_win(root: &mut Root, base: &Snapshot<Root>, theirs: &Snapshot<Root>) -> (String, usize) {
    let applied = root.merge(base, theirs, |conflict| {
        conflict.theirs.map(|value| (*value).clone())
    });
    ("merge".to_string(), applied)
}

fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    panic!("no merge expected");
}

fn put(db: &Db<Root>, key: u64, value: &str) {
    db.commit(format!("put {key}"), |root| {
        root.insert(key, value.to_string());
    })
    .unwrap();
}

fn file_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap()
}

#[test]
fn an_empty_database_fast_forwards_to_the_remote_and_shares_its_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    for key in 0..20 {
        put(&a, key, &format!("value {key}"));
    }
    let pulled = b.pull(&a, "a", refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(pulled.value, None);
    assert_eq!(pulled.version, 20);
    assert_eq!(b.version(), 20);
    assert_eq!(b.head_hash(), a.head_hash());
    assert_eq!(dump(&b.read()), dump(&a.read()));
    for version in 1..=20 {
        assert_eq!(b.log().hash_at(&version), a.log().hash_at(&version));
        assert_eq!(
            *b.log().get(&version).unwrap(),
            *a.log().get(&version).unwrap()
        );
        assert_eq!(dump(&b.at(version).unwrap()), dump(&a.at(version).unwrap()));
    }
    assert_eq!(b.branches(), vec!["a".to_string(), "main".to_string()]);
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    b.verify().unwrap();
    drop(b);
    let b = open(&dir, "b.ferro");
    assert_eq!(b.head_hash(), a.head_hash());
    b.verify().unwrap();
}

#[test]
fn a_branch_that_has_not_moved_fast_forwards_and_sends_only_new_objects() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    a.commit("seed", |root| {
        for key in 0..5000 {
            root.insert(key, text(key, 60));
        }
    })
    .unwrap();
    let first = b.pull(&a, "a", refuse).unwrap();
    assert_eq!(first.outcome, Outcome::FastForward);
    put(&a, 17, "changed");
    put(&a, 4321, "changed too");
    let second = b.pull(&a, "a", refuse).unwrap();
    assert_eq!(second.outcome, Outcome::FastForward);
    assert_eq!(b.version(), 3);
    assert_eq!(b.head_hash(), a.head_hash());
    assert_eq!(*b.read().get(&17).unwrap(), "changed");
    println!("clone {first:?}, two commits {second:?}");
    assert!(
        second.received_bytes * 20 < first.received_bytes,
        "{second:?} against {first:?}"
    );
    assert!(second.objects < 30, "{second:?}");
    b.verify().unwrap();
}

fn object_graph(path: &Path, scratch: &Path) -> HashMap<Hash, Vec<Hash>> {
    fs::copy(path, scratch).unwrap();
    let options = StoreOptions {
        read_only: true,
        ..StoreOptions::default()
    };
    let (store, head) = Store::open(scratch, options).unwrap();
    let mut graph = HashMap::new();
    store
        .walk(head.unwrap(), |ptr, object| {
            let links = object.links.iter().map(|link| link.hash).collect();
            graph.insert(ptr.hash, links);
        })
        .unwrap();
    store.close();
    graph
}

fn reached(graph: &HashMap<Hash, Vec<Hash>>, from: Hash) -> HashSet<Hash> {
    let mut seen = HashSet::from([from]);
    let mut stack = vec![from];
    while let Some(hash) = stack.pop() {
        for link in &graph[&hash] {
            if seen.insert(*link) {
                stack.push(*link);
            }
        }
    }
    seen
}

fn depth(graph: &HashMap<Hash, Vec<Hash>>, hash: Hash) -> usize {
    let below = graph[&hash].iter().map(|link| depth(graph, *link));
    1 + below.max().unwrap_or(0)
}

#[test]
fn a_commit_that_pushes_held_nodes_deeper_sends_only_what_the_receiver_lacks() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    let scratch = dir.path().join("graph.ferro");
    let value = |key: u64| text(key, 600);
    a.commit("seed", |root| {
        for key in 0..64 {
            root.insert(key, value(key));
        }
    })
    .unwrap();
    b.pull(&a, "a", refuse).unwrap();
    let mut deeper = 0;
    for round in 0..64u64 {
        let (held_tip, held_root) = (a.head_hash().unwrap(), a.read().root_hash().unwrap());
        let copied = value(round);
        a.commit(format!("grow {round}"), |root| {
            for key in 0..32 {
                let key = (1 << 20) + round * 32 + key;
                root.insert(key, value(key));
            }
            root.insert(1 << 40 | round, copied.clone());
        })
        .unwrap();
        let pulled = b.pull(&a, "a", refuse).unwrap();
        let root = a.read().root_hash().unwrap();
        let graph = object_graph(&dir.path().join("a.ferro"), &scratch);
        if depth(&graph, root) <= depth(&graph, held_root) {
            continue;
        }
        deeper += 1;
        let held = reached(&graph, graph[&held_tip][0]);
        let tree = reached(&graph, graph[&a.head_hash().unwrap()][0]);
        let shared = ferro::hash_of(&copied);
        assert!(held.contains(&shared), "round {round}");
        let fresh = tree.iter().filter(|hash| !held.contains(*hash));
        let linked = fresh.clone().any(|hash| graph[hash].contains(&shared));
        assert!(linked, "round {round}");
        let lacked = 1 + fresh.count();
        println!("round {round} added a level: {pulled:?}, {lacked} objects lacked");
        assert_eq!(pulled.objects as usize, lacked, "round {round}");
    }
    assert!(deeper >= 1, "{deeper} rounds added a level");
    assert_eq!(dump(&b.read()), dump(&a.read()));
    b.verify().unwrap();
}

#[test]
fn diverged_branches_merge_against_the_newest_common_state() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "one");
    put(&a, 2, "two");
    b.pull(&a, "a", refuse).unwrap();
    put(&a, 1, "one from a");
    put(&b, 2, "two from b");
    put(&b, 3, "three from b");
    let mut seen = None;
    let pulled = a
        .pull(&b, "b", |root, base, theirs| {
            seen = Some((base.version(), theirs.version()));
            theirs_win(root, base, theirs)
        })
        .unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    assert_eq!(pulled.value, Some(2));
    assert_eq!(seen, Some((2, 4)));
    assert_eq!(a.version(), 4);
    let merged = dump(&a.read());
    assert_eq!(merged[&1], "one from a");
    assert_eq!(merged[&2], "two from b");
    assert_eq!(merged[&3], "three from b");
    let back = b.pull(&a, "a", theirs_win).unwrap();
    assert_eq!(back.outcome, Outcome::Merged);
    assert_eq!(dump(&b.read()), merged);
    let again = a.pull(&b, "b", refuse).unwrap();
    assert_eq!(again.outcome, Outcome::UpToDate);
    a.verify().unwrap();
    b.verify().unwrap();
}

#[test]
fn a_remote_behind_us_leaves_the_branch_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "one");
    b.pull(&a, "a", refuse).unwrap();
    put(&b, 2, "two");
    let pulled = b.pull(&a, "a", refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::Ahead);
    assert_eq!(pulled.objects, 0);
    assert_eq!(b.version(), 2);
    let pushed = b.push(&a, "b", refuse).unwrap();
    assert_eq!(pushed.outcome, Outcome::FastForward);
    assert_eq!(a.head_hash(), b.head_hash());
}

fn settle(a: &Db<Root>, an: &str, b: &Db<Root>, bn: &str) {
    a.pull(b, bn, theirs_win).unwrap();
    b.pull(a, an, theirs_win).unwrap();
    a.pull(b, bn, theirs_win).unwrap();
}

#[test]
fn repeated_sync_writes_nothing_and_sends_no_objects() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "one");
    put(&b, 2, "two");
    settle(&a, "a", &b, "b");
    assert_eq!(dump(&a.read()), dump(&b.read()));
    let (pa, pb) = (dir.path().join("a.ferro"), dir.path().join("b.ferro"));
    let (sa, sb) = (a.size_on_disk(), b.size_on_disk());
    let heads = (a.head_hash(), b.head_hash());
    for _ in 0..3 {
        let one = a.pull(&b, "b", refuse).unwrap();
        let two = b.pull(&a, "a", refuse).unwrap();
        for pulled in [&one, &two] {
            assert_ne!(pulled.outcome, Outcome::Merged);
            assert_ne!(pulled.outcome, Outcome::FastForward);
            assert_eq!(pulled.objects, 0, "{pulled:?}");
        }
        assert_eq!((a.size_on_disk(), b.size_on_disk()), (sa, sb));
        assert_eq!((a.head_hash(), b.head_hash()), heads);
    }
    drop((a, b));
    assert_eq!(fs::metadata(pa).unwrap().len(), sa);
    assert_eq!(fs::metadata(pb).unwrap().len(), sb);
}

fn stamp(board: &mut Board, device: u64, key: u64, value: String) {
    board.clock += 1;
    board.cells.insert(key, (board.clock, device, value));
}

fn join(board: &mut Board, base: &Snapshot<Board>, theirs: &Snapshot<Board>) -> (String, usize) {
    let applied = board.cells.merge(&base.cells, &theirs.cells, |conflict| {
        match (conflict.ours, conflict.theirs) {
            (Some(ours), Some(theirs)) => Some(if (ours.0, ours.1) >= (theirs.0, theirs.1) {
                (*ours).clone()
            } else {
                (*theirs).clone()
            }),
            (ours, theirs) => ours.or(theirs).map(|value| (*value).clone()),
        }
    });
    board.clock = board.clock.max(theirs.clock);
    ("join".to_string(), applied)
}

fn boards(dir: &TempDir, count: usize) -> Vec<Db<Board>> {
    (0..count)
        .map(|device| {
            fast()
                .open(dir.path().join(format!("d{device}.ferro")))
                .unwrap()
        })
        .collect()
}

fn exchange(devices: &[Db<Board>], from: usize, to: usize) -> Pulled<usize> {
    devices[to]
        .pull(&devices[from], &format!("d{from}"), join)
        .unwrap()
}

fn cells(board: &Board) -> BTreeMap<u64, (u64, u64, String)> {
    board
        .cells
        .iter()
        .map(|(key, value)| (key, (*value).clone()))
        .collect()
}

fn converge(devices: &[Db<Board>]) {
    for _ in 0..2 {
        for to in 0..devices.len() {
            for from in 0..devices.len() {
                if from != to {
                    exchange(devices, from, to);
                }
            }
        }
    }
    let first = cells(&devices[0].read());
    for device in devices {
        assert_eq!(cells(&device.read()), first);
        device.verify().unwrap();
    }
    for to in 0..devices.len() {
        for from in 0..devices.len() {
            if from != to {
                let before = devices[to].size_on_disk();
                let pulled = exchange(devices, from, to);
                assert_eq!(pulled.objects, 0, "d{from} into d{to}: {pulled:?}");
                assert_eq!(devices[to].size_on_disk(), before);
            }
        }
    }
}

#[test]
fn three_devices_converge_whatever_order_they_sync_in() {
    let orders: [&[(usize, usize)]; 4] = [
        &[(0, 1), (1, 2), (2, 0)],
        &[(2, 1), (0, 1), (1, 0), (1, 2)],
        &[(0, 2), (1, 2), (2, 0), (2, 1)],
        &[(1, 0), (2, 0), (0, 1), (0, 2), (1, 2)],
    ];
    for (round, order) in orders.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let devices = boards(&dir, 3);
        let mut rng = Rng::new(round as u64 + 1);
        for _ in 0..4 {
            for (device, db) in devices.iter().enumerate() {
                for _ in 0..rng.below(4) + 1 {
                    let key = rng.below(12);
                    let value = text(rng.next(), 8);
                    db.update(|board| stamp(board, device as u64, key, value))
                        .unwrap();
                }
            }
            for (from, to) in order.iter() {
                exchange(&devices, *from, *to);
            }
        }
        converge(&devices);
    }
}

#[test]
fn random_pairwise_syncs_between_four_devices_converge() {
    for seed in 0..cases(6) as u64 {
        let dir = tempfile::tempdir().unwrap();
        let devices = boards(&dir, 4);
        let mut rng = Rng::new(seed + 100);
        for _ in 0..40 {
            let device = rng.below(4) as usize;
            if rng.chance(55) {
                let key = rng.below(20);
                let value = text(rng.next(), 6);
                devices[device]
                    .update(|board| stamp(board, device as u64, key, value))
                    .unwrap();
            } else {
                let other = (device + 1 + rng.below(3) as usize) % 4;
                exchange(&devices, other, device);
            }
        }
        converge(&devices);
    }
}

#[test]
fn criss_cross_merges_do_not_bring_old_changes_back_as_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "base");
    put(&a, 2, "base");
    b.pull(&a, "a", refuse).unwrap();
    put(&a, 1, "a first");
    put(&b, 2, "b first");
    a.pull(&b, "b", theirs_win).unwrap();
    b.pull(&a, "a", theirs_win).unwrap();
    put(&a, 1, "a second");
    put(&b, 3, "b second");
    let mut conflicts = 0;
    let pulled = a
        .pull(&b, "b", |root, base, theirs| {
            let applied = root.merge(base, theirs, |conflict| {
                conflicts += 1;
                conflict.theirs.map(|value| (*value).clone())
            });
            ("merge", applied)
        })
        .unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    assert_eq!(conflicts, 0);
    assert_eq!(pulled.value, Some(1));
    let merged = dump(&a.read());
    assert_eq!(merged[&1], "a second");
    assert_eq!(merged[&2], "b first");
    assert_eq!(merged[&3], "b second");
}

#[test]
fn pulled_commits_keep_their_versions_times_and_messages() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "one");
    b.pull(&a, "a", refuse).unwrap();
    put(&a, 2, "two");
    put(&a, 3, "three");
    put(&b, 9, "nine");
    b.pull(&a, "a", theirs_win).unwrap();
    let remote = b.branch("a").unwrap();
    for version in 1..=3 {
        let theirs = a.log().get(&version).unwrap();
        assert_eq!(*remote.log().get(&version).unwrap(), *theirs);
        assert_eq!(remote.log().hash_at(&version), a.log().hash_at(&version));
    }
    assert_eq!(b.log().get(&1).unwrap().message, "put 1");
    assert_eq!(b.log().get(&2).unwrap().message, "put 9");
    let merge = b.log().get(&3).unwrap();
    assert_eq!(merge.message, "merge");
    assert!(merge.time_ms >= b.log().get(&2).unwrap().time_ms);
    assert_eq!(remote.version(), 3);
    assert_eq!(dump(&remote.read()), dump(&a.read()));
}

#[test]
fn a_small_change_on_a_large_database_sends_a_small_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    for batch in 0..10u64 {
        a.commit("load", |root| {
            for key in batch * 10_000..(batch + 1) * 10_000 {
                root.insert(key, text(key, 80));
            }
        })
        .unwrap();
    }
    let clone = b.pull(&a, "a", refuse).unwrap();
    put(&a, 4242, "edited");
    put(&a, 77_777, "edited too");
    put(&a, 100_001, "added");
    let small = b.pull(&a, "a", refuse).unwrap();
    assert_eq!(small.outcome, Outcome::FastForward);
    let file = a.size_on_disk();
    println!(
        "file {file} bytes, clone {} bytes in {} objects, three commits {} bytes in {} objects, have {} bytes",
        clone.received_bytes, clone.objects, small.received_bytes, small.objects, small.sent_bytes
    );
    assert!(small.received_bytes < 40_000, "{small:?}");
    assert!(
        small.received_bytes * 100 < file,
        "{small:?} against {file}"
    );
    assert_eq!(dump(&b.read()), dump(&a.read()));
}

fn corrupt_case(dir: &TempDir) -> (Db<Root>, PathBuf, Bundle) {
    let (a, b) = (open(dir, "a.ferro"), open(dir, "b.ferro"));
    a.commit("seed", |root| {
        for key in 0..300 {
            root.insert(key, text(key, 30));
        }
    })
    .unwrap();
    b.pull(&a, "a", refuse).unwrap();
    put(&a, 5, "five");
    put(&a, 250, &"long ".repeat(40));
    put(&b, 999, "local");
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    drop((a, b));
    (open(dir, "b.ferro"), dir.path().join("b.ferro"), bundle)
}

#[test]
fn every_corrupted_byte_of_a_bundle_is_rejected_and_leaves_the_target_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let (mut b, path, bundle) = corrupt_case(&dir);
    let before = (b.head_hash(), b.size_on_disk(), file_bytes(&path));
    let clean = {
        let copy = dir.path().join("clean.ferro");
        fs::write(&copy, &before.2).unwrap();
        let db = fast().open::<Root>(&copy).unwrap();
        db.receive("a", &bundle, theirs_win).unwrap();
        (dump(&db.read()), db.branch("a").unwrap().head_hash())
    };
    let bytes: Vec<u8> = bundle.clone().into();
    let (mut rejected, mut same) = (0, 0);
    for position in 0..bytes.len() {
        for mask in [0x01u8, 0x80, 0xff] {
            let mut damaged = bytes.clone();
            damaged[position] ^= mask;
            match b.receive("a", &Bundle::from(damaged), theirs_win) {
                Err(Error::Bundle(_) | Error::StaleHave(_)) => rejected += 1,
                Err(other) => panic!("byte {position} mask {mask:#x}: unexpected error {other}"),
                Ok(pulled) => {
                    assert_eq!(
                        dump(&b.read()),
                        clean.0,
                        "byte {position} mask {mask:#x}: {pulled:?}"
                    );
                    assert_eq!(b.branch("a").unwrap().head_hash(), clean.1);
                    same += 1;
                    drop(b);
                    fs::write(&path, &before.2).unwrap();
                    b = open(&dir, "b.ferro");
                }
            }
        }
        assert_eq!(b.head_hash(), before.0);
    }
    println!(
        "{} bytes, {rejected} flips rejected, {same} decoded to the same objects",
        bytes.len()
    );
    assert_eq!(rejected + same, bytes.len() * 3);
    assert!(
        same * 200 < rejected,
        "{same} flips decoded to the same objects, {rejected} rejected"
    );
    assert_eq!(b.size_on_disk(), before.1);
    drop(b);
    assert_eq!(file_bytes(&path), before.2);
    let b = open(&dir, "b.ferro");
    b.verify().unwrap();
    let pulled = b.receive("a", &bundle, theirs_win).unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    assert_eq!(*b.read().get(&5).unwrap(), "five");
    b.verify().unwrap();
}

#[test]
fn truncated_extended_and_foreign_bundles_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (b, _, bundle) = corrupt_case(&dir);
    let bytes: Vec<u8> = bundle.into();
    for end in 0..bytes.len() {
        let result = b.receive("a", &Bundle::from(bytes[..end].to_vec()), theirs_win);
        assert!(matches!(result, Err(Error::Bundle(_))), "cut at {end}");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(matches!(
        b.receive("a", &Bundle::from(longer), theirs_win),
        Err(Error::Bundle(_))
    ));
    let have: Vec<u8> = b.have().unwrap().into();
    assert!(matches!(
        b.receive("a", &Bundle::from(have), theirs_win),
        Err(Error::Bundle(_))
    ));
    assert_eq!(b.version(), 2);
}

#[test]
fn a_bundle_built_for_another_database_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        open(&dir, "a.ferro"),
        open(&dir, "b.ferro"),
        open(&dir, "c.ferro"),
    );
    put(&a, 1, "one");
    b.pull(&a, "a", refuse).unwrap();
    put(&a, 2, "two");
    let for_b = a.bundle(&b.have().unwrap()).unwrap();
    put(&c, 7, "seven");
    let size = c.size_on_disk();
    match c.receive("a", &for_b, theirs_win) {
        Err(Error::StaleHave(message)) => assert!(message.contains("does not have"), "{message}"),
        other => panic!("accepted a foreign bundle: {other:?}"),
    }
    assert_eq!(c.size_on_disk(), size);
    assert_eq!(c.branches(), vec!["main".to_string()]);
}

#[test]
fn a_bundle_survives_local_commits_made_after_its_have() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "one");
    b.pull(&a, "a", refuse).unwrap();
    let have = b.have().unwrap();
    put(&a, 2, "two");
    let bundle = a.bundle(&have).unwrap();
    put(&b, 3, "three");
    let pulled = b.receive("a", &bundle, theirs_win).unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    assert_eq!(dump(&b.read()).len(), 3);
}

#[test]
fn remote_names_cannot_be_main_or_the_pulling_branch() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "one");
    assert!(matches!(
        b.pull(&a, "main", refuse),
        Err(Error::BranchInUse(_))
    ));
    let side = b.fork("side").unwrap();
    assert!(matches!(
        side.pull(&a, "side", refuse),
        Err(Error::BranchInUse(_))
    ));
    assert_eq!(
        side.pull(&a, "a", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    assert_eq!(b.version(), 0);
    assert_eq!(side.version(), 1);
    drop((side, b));
    let reader = fast()
        .read_only(true)
        .open::<Root>(dir.path().join("b.ferro"))
        .unwrap();
    assert!(matches!(reader.pull(&a, "a", refuse), Err(Error::ReadOnly)));
    let have = reader.have().unwrap();
    let from_reader = reader.bundle(&a.have().unwrap()).unwrap();
    assert!(!have.as_bytes().is_empty());
    assert_eq!(
        a.receive("b", &from_reader, refuse).unwrap().outcome,
        Outcome::UpToDate
    );
}

#[test]
fn watchers_and_held_handles_see_what_a_pull_lands() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "one");
    b.pull(&a, "a", refuse).unwrap();
    let remote = b.branch("a").unwrap();
    let watcher = b.watch();
    let remote_watcher = remote.watch();
    put(&a, 2, "two");
    assert_eq!(
        b.pull(&a, "a", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    assert_eq!(
        watcher.try_recv().map(|snapshot| snapshot.version()).ok(),
        Some(2)
    );
    assert_eq!(
        remote_watcher
            .try_recv()
            .map(|snapshot| snapshot.version())
            .ok(),
        Some(2)
    );
    assert_eq!(remote.version(), 2);
    put(&a, 3, "three");
    b.pull(&a, "a", refuse).unwrap();
    remote
        .commit("local on the remote branch", |root| {
            root.insert(50, "x".into());
        })
        .unwrap();
    assert_eq!(remote.version(), 4);
    assert_eq!(dump(&remote.read()).len(), 4);
}

#[test]
fn pruned_histories_still_sync() {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Root> = fast()
        .history(Keep::Last(8))
        .open(dir.path().join("a.ferro"))
        .unwrap();
    let b: Db<Root> = fast()
        .history(Keep::Last(8))
        .open(dir.path().join("b.ferro"))
        .unwrap();
    for key in 0..10 {
        put(&a, key, "a");
    }
    assert_eq!(
        b.pull(&a, "a", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    assert_eq!(b.log().len(), 8);
    a.commit("drop 0", |root| {
        root.remove(&0);
    })
    .unwrap();
    b.commit("drop 1", |root| {
        root.remove(&1);
    })
    .unwrap();
    for key in 10..13 {
        put(&a, key, "a");
        put(&b, key + 100, "b");
    }
    b.compact(Keep::Last(8)).unwrap();
    a.compact(Keep::Last(8)).unwrap();
    assert!(b.log().first().unwrap().0 > 1);
    let pulled = b.pull(&a, "a", theirs_win).unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    let merged = dump(&b.read());
    assert_eq!(merged.len(), 14);
    assert!(!merged.contains_key(&0) && !merged.contains_key(&1));
    b.verify().unwrap();
    b.compact(Keep::Last(8)).unwrap();
    b.verify().unwrap();
    let back = a.pull(&b, "b", theirs_win).unwrap();
    assert_eq!(back.outcome, Outcome::Merged);
    assert_eq!(dump(&a.read()), merged);
    drop((a, b));
    let b = open(&dir, "b.ferro");
    b.verify().unwrap();
    assert_eq!(dump(&b.read()), merged);
}

#[test]
fn a_pull_from_a_remote_pruned_past_our_tip_refuses_instead_of_bringing_deletions_back() {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Root> = fast()
        .history(Keep::Last(3))
        .open(dir.path().join("a.ferro"))
        .unwrap();
    let b = open(&dir, "b.ferro");
    for key in 0..5 {
        put(&a, key, "a");
    }
    assert_eq!(
        b.pull(&a, "a", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    a.commit("drop 0", |root| {
        root.remove(&0);
    })
    .unwrap();
    for key in 5..10 {
        put(&a, key, "a");
    }
    let before = (b.head_hash(), dump(&b.read()));
    match b.pull(&a, "a", theirs_win) {
        Err(Error::NoCommonAncestor) => {}
        other => panic!("merged a remote that shares nothing retained: {other:?}"),
    }
    assert!(matches!(
        b.pull(&a, "a", refuse),
        Err(Error::NoCommonAncestor)
    ));
    assert_eq!((b.head_hash(), dump(&b.read())), before);
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    b.verify().unwrap();
}

#[test]
fn a_revert_routed_through_a_third_device_survives_the_first_pull() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        open(&dir, "a.ferro"),
        open(&dir, "b.ferro"),
        open(&dir, "c.ferro"),
    );
    put(&a, 1, "1");
    b.pull(&a, "a", refuse).unwrap();
    c.pull(&a, "a", refuse).unwrap();
    put(&a, 2, "9");
    put(&b, 1, "2");
    assert_eq!(
        a.pull(&b, "b", theirs_win).unwrap().outcome,
        Outcome::Merged
    );
    assert_eq!(*a.read().get(&1).unwrap(), "2");
    assert_eq!(
        c.pull(&b, "b", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    put(&c, 1, "1");
    assert_eq!(c.read().root_hash(), b.at(1).unwrap().root_hash());
    let pulled = a.pull(&c, "c", theirs_win).unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    assert_eq!(*a.read().get(&1).unwrap(), "1");
    assert_eq!(*a.read().get(&2).unwrap(), "9");
}

type Events = Map<u32, i64>;

fn add_deltas(
    root: &mut Events,
    base: &Snapshot<Events>,
    theirs: &Snapshot<Events>,
) -> (String, ()) {
    let mut keys: Vec<u32> = base.keys().chain(theirs.keys()).collect();
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        let delta = theirs.get(&key).map_or(0, |v| *v) - base.get(&key).map_or(0, |v| *v);
        if delta != 0 {
            let next = root.get(&key).map_or(0, |v| *v) + delta;
            root.insert(key, next);
        }
    }
    ("merge".to_string(), ())
}

fn event(db: &Db<Events>, event: u32) {
    db.commit(format!("e{event}"), |root| {
        root.insert(event, 1);
    })
    .unwrap();
}

struct Node {
    counts: BTreeMap<u32, i64>,
    ancestors: Vec<bool>,
    generation: u64,
}

struct Model(Vec<Node>);

impl Model {
    fn push(&mut self, parents: &[usize], counts: BTreeMap<u32, i64>) -> usize {
        let id = self.0.len();
        let mut ancestors = vec![false; id + 1];
        for parent in parents {
            for (at, held) in self.0[*parent].ancestors.iter().enumerate() {
                ancestors[at] |= *held;
            }
        }
        ancestors[id] = true;
        let generation = parents
            .iter()
            .map(|parent| self.0[*parent].generation + 1)
            .max();
        self.0.push(Node {
            counts,
            ancestors,
            generation: generation.unwrap_or(0),
        });
        id
    }

    fn holds(&self, node: usize, ancestor: usize) -> bool {
        self.0[node]
            .ancestors
            .get(ancestor)
            .copied()
            .unwrap_or(false)
    }

    fn best(&self, a: usize, b: usize) -> Vec<usize> {
        let common: Vec<usize> = (0..self.0.len())
            .filter(|c| self.holds(a, *c) && self.holds(b, *c))
            .collect();
        common
            .iter()
            .copied()
            .filter(|c| !common.iter().any(|d| d != c && self.holds(*d, *c)))
            .collect()
    }

    fn events(&self, node: usize) -> BTreeMap<u32, i64> {
        let mut events = BTreeMap::new();
        for (at, held) in self.0[node].ancestors.iter().enumerate() {
            if *held {
                for event in self.0[at].counts.keys() {
                    events.insert(*event, 1);
                }
            }
        }
        events
    }
}

fn id_in(snapshot: &Snapshot<Events>) -> usize {
    snapshot
        .commit()
        .message
        .strip_prefix('n')
        .map_or(0, |id| id.parse().unwrap())
}

fn counted(db: &Db<Events>) -> BTreeMap<u32, i64> {
    db.read().iter().map(|(k, v)| (k, *v)).collect()
}

#[test]
fn random_syncs_with_a_counting_merge_apply_every_change_exactly_once() {
    for seed in 0..cases(40) as u64 {
        let dir = tempfile::tempdir().unwrap();
        let devices: Vec<Db<Events>> = (0..4)
            .map(|device| {
                fast()
                    .open(dir.path().join(format!("d{device}.ferro")))
                    .unwrap()
            })
            .collect();
        let mut model = Model(Vec::new());
        model.push(&[], BTreeMap::new());
        let mut heads = [0usize; 4];
        let mut rng = Rng::new(seed + 7);
        let mut next_event = 0u32;
        for _ in 0..60 {
            let to = rng.below(4) as usize;
            if rng.chance(40) {
                let id = model.0.len();
                let event = next_event;
                next_event += 1;
                devices[to]
                    .commit(format!("n{id}"), |root| {
                        root.insert(event, 1);
                    })
                    .unwrap();
                let mut counts = model.0[heads[to]].counts.clone();
                counts.insert(event, 1);
                heads[to] = model.push(&[heads[to]], counts);
                continue;
            }
            let from = (to + 1 + rng.below(3) as usize) % 4;
            let (ours, theirs) = (heads[to], heads[from]);
            let best = model.best(ours, theirs);
            let id = model.0.len();
            let mut seen = Vec::new();
            let pulled = devices[to]
                .pull(
                    &devices[from],
                    &format!("d{from}"),
                    |root, base, incoming| {
                        seen.push((base.version(), id_in(base)));
                        add_deltas(root, base, incoming);
                        (format!("n{id}"), ())
                    },
                )
                .unwrap();
            match pulled.outcome {
                Outcome::FastForward => {
                    assert!(model.holds(theirs, ours), "seed {seed}");
                    heads[to] = theirs;
                }
                Outcome::Ahead => assert!(model.holds(ours, theirs), "seed {seed}"),
                Outcome::UpToDate => {
                    let same = model.0[ours].counts == model.0[theirs].counts;
                    assert!(same || model.holds(ours, theirs), "seed {seed}");
                }
                Outcome::Merged => {
                    let (version, base) = *seen.last().unwrap();
                    if let [only] = best[..] {
                        assert_eq!((seen.len(), base), (1, only), "seed {seed}");
                    } else {
                        assert!(seen.len() >= 2 && version == 0, "seed {seed}: {seen:?}");
                    }
                    let mut counts = model.0[ours].counts.clone();
                    counts.extend(model.0[theirs].counts.clone());
                    heads[to] = model.push(&[ours, theirs], counts);
                }
            }
            for (device, db) in devices.iter().enumerate() {
                let node = &model.0[heads[device]];
                assert_eq!(counted(db), node.counts, "seed {seed} device {device}");
                assert_eq!(node.counts, model.events(heads[device]), "seed {seed}");
            }
        }
        for db in &devices {
            db.verify().unwrap();
        }
    }
}

#[test]
fn a_merged_chain_the_sender_cut_short_reads_as_pruned_history() {
    let dir = tempfile::tempdir().unwrap();
    let pause = || thread::sleep(Duration::from_millis(3));
    let r = open(&dir, "r.ferro");
    let b = open(&dir, "b.ferro");
    put(&r, 100, "r");
    assert_eq!(
        b.pull(&r, "r", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    put(&r, 101, "r");
    put(&r, 102, "r");
    pause();
    let a: Db<Root> = fast()
        .history(Keep::Last(5))
        .open(dir.path().join("a.ferro"))
        .unwrap();
    for key in 0..5 {
        put(&a, key, "a");
    }
    assert_eq!(
        b.pull(&a, "a", theirs_win).unwrap().outcome,
        Outcome::Merged
    );
    assert_eq!(
        a.pull(&r, "r", theirs_win).unwrap().outcome,
        Outcome::Merged
    );
    put(&a, 5, "a");
    put(&a, 6, "a");
    assert_eq!(a.log().first().unwrap().0, 4);
    let tracking = b.branch("a").unwrap();
    let before = (b.head_hash(), dump(&b.read()));
    match b.pull(&a, "a", theirs_win) {
        Ok(pulled) => assert_eq!(pulled.outcome, Outcome::Merged),
        Err(Error::NoCommonAncestor) => assert_eq!((b.head_hash(), dump(&b.read())), before),
        Err(other) => panic!("{other}"),
    }
    let answer = |db: &Db<Root>| match db.common_ancestor(&tracking) {
        Ok(base) => Some(base.version()),
        Err(Error::NoCommonAncestor) => None,
        Err(other) => panic!("{other}"),
    };
    let found = answer(&b);
    b.verify().unwrap();
    b.compact(Keep::All).unwrap();
    b.verify().unwrap();
    assert_eq!(answer(&b), found);
}

#[test]
fn a_merge_whose_history_the_sender_had_let_go_still_merges() {
    let dir = tempfile::tempdir().unwrap();
    let pause = || thread::sleep(Duration::from_millis(3));
    let r = open(&dir, "r.ferro");
    for key in 100..103 {
        put(&r, key, "r");
    }
    pause();
    let a: Db<Root> = fast()
        .history(Keep::Last(5))
        .open(dir.path().join("a.ferro"))
        .unwrap();
    let b = open(&dir, "b.ferro");
    for key in 0..5 {
        put(&a, key, "a");
    }
    assert_eq!(
        b.pull(&a, "a", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    assert_eq!(
        a.pull(&r, "r", theirs_win).unwrap().outcome,
        Outcome::Merged
    );
    put(&a, 5, "a");
    put(&a, 6, "a");
    assert_eq!(a.log().first().unwrap().0, 4);
    put(&b, 50, "b");
    let pulled = b.pull(&a, "a", theirs_win).unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    let merged = dump(&b.read());
    assert_eq!(merged.len(), 11);
    assert_eq!(merged[&101], "r");
    b.verify().unwrap();
    b.compact(Keep::All).unwrap();
    b.verify().unwrap();
    put(&a, 7, "a");
    assert_eq!(
        b.pull(&a, "a", theirs_win).unwrap().outcome,
        Outcome::Merged
    );
    assert_eq!(dump(&b.read()).len(), 12);
}

#[test]
fn a_change_that_reaches_us_through_a_third_device_is_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    let devices: Vec<Db<Events>> = ["a", "b", "c"]
        .iter()
        .map(|name| {
            fast()
                .open(dir.path().join(format!("{name}.ferro")))
                .unwrap()
        })
        .collect();
    let (a, b, c) = (&devices[0], &devices[1], &devices[2]);
    event(a, 0);
    b.pull(a, "a", add_deltas).unwrap();
    c.pull(a, "a", add_deltas).unwrap();
    event(c, 1);
    assert_eq!(
        b.pull(c, "c", add_deltas).unwrap().outcome,
        Outcome::FastForward
    );
    event(b, 2);
    event(a, 3);
    assert_eq!(a.pull(b, "b", add_deltas).unwrap().outcome, Outcome::Merged);
    assert_eq!(a.pull(c, "c", add_deltas).unwrap().outcome, Outcome::Ahead);
    event(c, 4);
    assert_eq!(a.pull(c, "c", add_deltas).unwrap().outcome, Outcome::Merged);
    let counts: Vec<(u32, i64)> = a.read().iter().map(|(k, v)| (k, *v)).collect();
    assert_eq!(counts, vec![(0, 1), (1, 1), (2, 1), (3, 1), (4, 1)]);
    for device in [b, c] {
        device.pull(a, "a", add_deltas).unwrap();
        assert_eq!(device.read().root_hash(), a.read().root_hash());
        device.verify().unwrap();
    }
    let fresh: Db<Events> = fast().open(dir.path().join("d.ferro")).unwrap();
    fresh.pull(b, "b", add_deltas).unwrap();
    event(&fresh, 5);
    event(c, 6);
    fresh.pull(c, "c", add_deltas).unwrap();
    let counts: Vec<(u32, i64)> = fresh.read().iter().map(|(k, v)| (k, *v)).collect();
    assert_eq!(counts, (0..7).map(|event| (event, 1)).collect::<Vec<_>>());
    fresh.verify().unwrap();
}

#[test]
fn reclamation_between_have_and_bundle_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    for key in 0..50 {
        put(&a, key, "x");
    }
    b.pull(&a, "a", refuse).unwrap();
    put(&a, 3, "y");
    let have = b.have().unwrap();
    b.compact(Keep::All).unwrap();
    a.compact(Keep::Last(5)).unwrap();
    let bundle = a.bundle(&have).unwrap();
    let pulled = b.receive("a", &bundle, refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(b.head_hash(), a.head_hash());
    assert!(pulled.objects < 12, "{pulled:?}");
    b.verify().unwrap();
}

fn image(before: &[u8], after: &[u8], end: usize) -> Vec<u8> {
    let mut out = before[..LOG_START].to_vec();
    out.extend_from_slice(&after[LOG_START..end]);
    out
}

#[test]
fn a_crash_while_receiving_leaves_the_target_at_its_previous_or_new_head() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    a.commit("seed", |root| {
        for key in 0..400 {
            root.insert(key, text(key, 40));
        }
    })
    .unwrap();
    b.pull(&a, "a", refuse).unwrap();
    put(&a, 7, "seven");
    put(&a, 300, &"long ".repeat(30));
    put(&b, 1000, "local");
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    let old = (
        b.head_hash(),
        dump(&b.read()),
        b.branch("a").unwrap().head_hash(),
    );
    let path = dir.path().join("b.ferro");
    drop(b);
    let before = file_bytes(&path);
    let b = open(&dir, "b.ferro");
    let pulled = b.receive("a", &bundle, theirs_win).unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    let new = (b.head_hash(), dump(&b.read()), a.head_hash());
    drop(b);
    let after = file_bytes(&path);
    assert!(after.len() > before.len());
    assert_eq!(after[LOG_START..before.len()], before[LOG_START..]);
    let target = dir.path().join("image.ferro");
    let mut seen = BTreeMap::new();
    for end in before.len()..=after.len() {
        fs::write(&target, image(&before, &after, end)).unwrap();
        let db = fast().open::<Root>(&target).unwrap();
        db.verify().unwrap();
        let main = (db.head_hash(), dump(&db.read()));
        let remote = db.branch("a").unwrap().head_hash();
        let state = if main == (old.0, old.1.clone()) && remote == old.2 {
            "previous"
        } else if main == (old.0, old.1.clone()) && remote == new.2 {
            "landed"
        } else {
            assert_eq!(main, (new.0, new.1.clone()), "cut at {end}");
            assert_eq!(remote, new.2, "cut at {end}");
            "merged"
        };
        *seen.entry(state).or_insert(0) += 1;
        drop(db);
        let reopened = fast().open::<Root>(&target).unwrap();
        let resumed = reopened.receive("a", &bundle, theirs_win).unwrap();
        assert_eq!(dump(&reopened.read()), new.1, "resumed from cut at {end}");
        assert_ne!(resumed.outcome, Outcome::FastForward);
        reopened.verify().unwrap();
    }
    assert_eq!(seen.len(), 3, "{seen:?}");
    for slot in [0, 4096] {
        if after[slot..slot + 144] == before[slot..slot + 144] {
            continue;
        }
        for torn in 0..144 {
            let mut damaged = image(&before, &after, after.len());
            damaged[slot..slot + torn].copy_from_slice(&after[slot..slot + torn]);
            fs::write(&target, &damaged).unwrap();
            let db = fast().open::<Root>(&target).unwrap();
            assert_eq!(db.head_hash(), new.0, "slot {slot} torn at {torn}");
            db.verify().unwrap();
        }
    }
}

fn apply(image: &mut Vec<u8>, op: &Op) {
    match op {
        Op::Write(_, at, bytes) => {
            let end = *at as usize + bytes.len();
            if image.len() < end {
                image.resize(end, 0);
            }
            image[*at as usize..end].copy_from_slice(bytes);
        }
        Op::SetLen(_, len) => image.resize(*len as usize, 0),
        _ => {}
    }
}

#[test]
fn a_power_cut_at_any_write_of_a_pull_keeps_a_database_that_opens() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    a.commit("seed", |root| {
        for key in 0..300 {
            root.insert(key, text(key, 50));
        }
    })
    .unwrap();
    b.pull(&a, "a", refuse).unwrap();
    put(&a, 3, &"three ".repeat(30));
    put(&a, 200, "two hundred");
    put(&b, 7, "seven");
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    let old = (b.head_hash(), b.branch("a").unwrap().head_hash());
    drop(b);
    let path = dir.path().join("b.ferro");
    let base = file_bytes(&path);
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let b: Db<Root> = Options::new().trace(trace.clone()).open(&path).unwrap();
    assert_eq!(
        b.receive("a", &bundle, theirs_win).unwrap().outcome,
        Outcome::Merged
    );
    b.trace_op(Op::Acked(1));
    let new = (b.head_hash(), a.head_hash(), dump(&b.read()));
    drop(b);
    let ops = trace.lock().clone();
    let acked = ops
        .iter()
        .position(|op| matches!(op, Op::Acked(_)))
        .unwrap();
    let target = dir.path().join("cut.ferro");
    let mut rng = Rng::new(31);
    let mut states = BTreeMap::new();
    for cut in 0..=ops.len() {
        for loss in 0..4 {
            let mut image = base.clone();
            let mut pending = Vec::new();
            for op in &ops[..cut] {
                match op {
                    Op::Write(..) | Op::SetLen(..) => pending.push(op),
                    Op::Sync(_) => {
                        for done in pending.drain(..) {
                            apply(&mut image, done);
                        }
                    }
                    _ => {}
                }
            }
            for op in pending {
                let keep = match loss {
                    0 => false,
                    1 => true,
                    _ => rng.chance(50),
                };
                if keep {
                    apply(&mut image, op);
                }
            }
            fs::write(&target, &image).unwrap();
            let db = fast()
                .open::<Root>(&target)
                .unwrap_or_else(|error| panic!("cut {cut} loss {loss}: {error}"));
            db.verify()
                .unwrap_or_else(|error| panic!("cut {cut} loss {loss}: {error}"));
            let remote = db.branch("a").unwrap().head_hash();
            let state = if db.head_hash() == old.0 {
                assert!(remote == old.1 || remote == new.1, "cut {cut} loss {loss}");
                assert!(
                    cut < acked,
                    "cut {cut} loss {loss} lost an acknowledged pull"
                );
                if remote == old.1 {
                    "previous"
                } else {
                    "landed"
                }
            } else {
                assert_eq!(db.head_hash(), new.0, "cut {cut} loss {loss}");
                assert_eq!(remote, new.1, "cut {cut} loss {loss}");
                assert_eq!(dump(&db.read()), new.2);
                "merged"
            };
            *states.entry(state).or_insert(0) += 1;
        }
    }
    assert_eq!(states.len(), 3, "{states:?}");
}

#[test]
fn a_merge_that_panics_leaves_the_remote_landed_and_the_next_pull_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "one");
    b.pull(&a, "a", refuse).unwrap();
    put(&a, 2, "two");
    put(&b, 3, "three");
    let before = (b.head_hash(), b.version());
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        b.pull(
            &a,
            "a",
            |_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>| -> (String, ()) {
                panic!("merge gave up")
            },
        )
    }));
    assert!(crashed.is_err());
    assert_eq!((b.head_hash(), b.version()), before);
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    let have: Vec<u8> = b.have().unwrap().into();
    let bundle: Vec<u8> = a.bundle(&Have::from(have)).unwrap().into();
    let pulled = b.receive("a", &Bundle::from(bundle), theirs_win).unwrap();
    assert_eq!(pulled.objects, 0);
    assert_eq!(pulled.outcome, Outcome::Merged);
    assert_eq!(dump(&b.read()).len(), 3);
    b.verify().unwrap();
}

fn within<R: Send + 'static>(what: &str, run: impl FnOnce() -> R + Send + 'static) -> R {
    let (done, finished) = channel();
    thread::spawn(move || {
        let _ = done.send(run());
    });
    match finished.recv_timeout(Duration::from_secs(20)) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => panic!("{what} deadlocked"),
        Err(RecvTimeoutError::Disconnected) => panic!("{what} panicked"),
    }
}

fn a_test_commits_while_a_pull_publishes(watch_remote: bool, diverge: bool) -> Outcome {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "one");
    b.pull(&a, "a", refuse).unwrap();
    if diverge {
        put(&b, 5, "five");
    }
    let watched = if watch_remote {
        b.branch("a").unwrap()
    } else {
        b.clone()
    };
    let fired = Arc::new(AtomicUsize::new(0));
    let watcher = {
        let (b, a, fired) = (b.clone(), a.clone(), fired.clone());
        watched.watch_when(move |snapshot| {
            let arrived = snapshot.contains_key(&2);
            if arrived && fired.fetch_add(1, Ordering::SeqCst) == 0 {
                put(&b, 9, "from the test");
                b.pull(&a, "a", theirs_win).unwrap();
            }
            arrived
        })
    };
    let parked_thread =
        thread::spawn(move || watcher.recv().map(|snapshot| snapshot.contains_key(&2)));
    thread::sleep(Duration::from_millis(30));
    put(&a, 2, "two");
    let (puller, source) = (b.clone(), a.clone());
    let outcome = within("a pull whose test commits and pulls", move || {
        puller.pull(&source, "a", theirs_win).unwrap().outcome
    });
    assert!(within("the watcher", move || parked_thread.join().unwrap()).unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !b.read().contains_key(&9) {
        assert!(
            Instant::now() < deadline,
            "the test's own commit never landed"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(fired.load(Ordering::SeqCst) >= 1);
    let ours = dump(&b.read());
    assert_eq!(ours[&2], "two");
    assert_eq!(ours.contains_key(&5), diverge);
    b.verify().unwrap();
    outcome
}

#[test]
fn a_watch_test_may_commit_and_pull_while_a_fast_forward_publishes() {
    assert_eq!(
        a_test_commits_while_a_pull_publishes(false, false),
        Outcome::FastForward
    );
}

#[test]
fn a_watch_test_on_the_remote_branch_may_commit_and_pull_while_it_lands() {
    let outcome = a_test_commits_while_a_pull_publishes(true, false);
    assert!(
        matches!(
            outcome,
            Outcome::FastForward | Outcome::Merged | Outcome::Ahead
        ),
        "{outcome:?}"
    );
}

#[test]
fn a_watch_test_may_commit_and_pull_while_a_merge_publishes() {
    assert_eq!(
        a_test_commits_while_a_pull_publishes(false, true),
        Outcome::Merged
    );
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn link_to(object: &Object) -> Ptr {
    Ptr {
        hash: object.hash(),
        offset: 0,
    }
}

fn object_of<D: Data>(value: &D) -> Object {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder).unwrap();
    encoder.into_object()
}

fn entry_of(version: u64, root: &Object, prev: Option<Ptr>, skip: Option<Ptr>) -> Object {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    version.encode(&mut encoder).unwrap();
    version.encode(&mut encoder).unwrap();
    String::new().encode(&mut encoder).unwrap();
    Some(link_to(root)).encode(&mut encoder).unwrap();
    None::<u64>.encode(&mut encoder).unwrap();
    encoder.u8(u8::from(prev.is_some()) + 2 * u8::from(skip.is_some()));
    for chained in prev.into_iter().chain(skip) {
        encoder.link(chained);
    }
    encoder.into_object()
}

fn forged(tip: &Object, version: u64, objects: &[&Object]) -> Bundle {
    let mut out = b"FERROBDL".to_vec();
    out.extend_from_slice(&[1, 1]);
    out.extend_from_slice(&tip.hash());
    varint(&mut out, version);
    for _ in 0..3 {
        varint(&mut out, 0);
    }
    varint(&mut out, objects.len() as u64);
    for object in objects {
        varint(&mut out, (object.body.len() as u64) << 1);
        out.extend_from_slice(&object.body);
        varint(&mut out, object.links.len() as u64);
        for linked in &object.links {
            out.extend_from_slice(&linked.hash);
            varint(&mut out, 0);
        }
    }
    Bundle::from(out)
}

fn untouched(_: &mut u64, _: &Snapshot<u64>, _: &Snapshot<u64>) -> (String, ()) {
    panic!("no merge expected");
}

#[test]
fn a_bundle_whose_commit_skips_to_another_commit_is_rejected() {
    for into_a_value in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let b: Db<u64> = fast().open(dir.path().join("b.ferro")).unwrap();
        let size = b.size_on_disk();
        let roots: Vec<Object> = (0..=10u64).map(|value| object_of(&value)).collect();
        let other_root = object_of(&40u64);
        let mut entries: Vec<Object> = Vec::new();
        let mut impostor = None;
        for version in 1..=10u64 {
            let prev = entries.last().map(link_to);
            let skip = match version {
                6 if !into_a_value => {
                    let other = entry_of(4, &other_root, Some(link_to(&entries[2])), None);
                    let skip = link_to(&other);
                    impostor = Some(other);
                    Some(skip)
                }
                10 if into_a_value => Some(link_to(&roots[8])),
                10 => Some(link_to(&entries[7])),
                _ => None,
            };
            entries.push(entry_of(version, &roots[version as usize], prev, skip));
        }
        let mut objects: Vec<&Object> = roots[1..].iter().collect();
        objects.push(&other_root);
        objects.extend(&entries[..3]);
        objects.extend(&impostor);
        objects.extend(&entries[3..]);
        let bundle = forged(&entries[9], 10, &objects);
        match b.receive("evil", &bundle, untouched) {
            Err(Error::Bundle(_)) => {}
            other => panic!("accepted a forged skip link: {other:?}"),
        }
        assert_eq!(b.version(), 0);
        assert_eq!(b.branches(), vec!["main".to_string()]);
        assert_eq!(b.size_on_disk(), size);
    }
}

struct Forged {
    version: u64,
    time: u64,
    prev: Option<Ptr>,
    merged: Option<(Ptr, u64)>,
    graph: Option<(u64, u64, u64)>,
}

fn forged_entry(root: &Object, forged: Forged) -> Object {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    forged.version.encode(&mut encoder).unwrap();
    forged.time.encode(&mut encoder).unwrap();
    String::new().encode(&mut encoder).unwrap();
    Some(link_to(root)).encode(&mut encoder).unwrap();
    None::<u64>.encode(&mut encoder).unwrap();
    let flags = u8::from(forged.prev.is_some())
        + 16 * u8::from(forged.merged.is_some())
        + 32 * u8::from(forged.graph.is_some());
    encoder.u8(flags);
    if let Some((_, floor)) = forged.merged {
        encoder.varint(floor);
        encoder.varint(0);
    }
    if let Some((lift, reach, last_merge)) = forged.graph {
        encoder.varint(lift);
        encoder.varint(reach);
        encoder.varint(last_merge);
    }
    for linked in forged
        .prev
        .into_iter()
        .chain(forged.merged.map(|(ptr, _)| ptr))
    {
        encoder.link(linked);
    }
    encoder.into_object()
}

#[test]
fn a_bundle_with_a_forged_merge_link_or_generation_is_rejected() {
    let roots: Vec<Object> = (0..4u64).map(|value| object_of(&value)).collect();
    let side_root = object_of(&50u64);
    let plain = |version: u64, prev: Option<Ptr>| Forged {
        version,
        time: version,
        prev,
        merged: None,
        graph: None,
    };
    let first = forged_entry(&roots[1], plain(1, None));
    let second = forged_entry(&roots[2], plain(2, Some(link_to(&first))));
    let side = forged_entry(&side_root, plain(1, None));
    let honest = Forged {
        version: 3,
        time: 3,
        prev: Some(link_to(&second)),
        merged: Some((link_to(&side), 1)),
        graph: Some((0, 1, 3)),
    };
    let cases: Vec<(&str, Forged)> = vec![
        ("honest", Forged { ..honest }),
        (
            "generation",
            Forged {
                graph: Some((4, 1, 3)),
                ..honest
            },
        ),
        (
            "reach",
            Forged {
                graph: Some((0, 2, 3)),
                ..honest
            },
        ),
        (
            "floor",
            Forged {
                merged: Some((link_to(&side), 2)),
                ..honest
            },
        ),
        (
            "value",
            Forged {
                merged: Some((link_to(&roots[1]), 1)),
                ..honest
            },
        ),
        (
            "unlinked",
            Forged {
                merged: None,
                ..honest
            },
        ),
    ];
    for (name, claim) in cases {
        let dir = tempfile::tempdir().unwrap();
        let b: Db<u64> = fast().open(dir.path().join("b.ferro")).unwrap();
        let size = b.size_on_disk();
        let merge = forged_entry(&roots[3], Forged { time: 60, ..claim });
        let objects: Vec<&Object> = vec![
            &roots[1], &first, &roots[2], &second, &side_root, &side, &roots[3], &merge,
        ];
        let bundle = forged(&merge, 3, &objects);
        let received = b.receive("evil", &bundle, untouched);
        if name == "honest" {
            let pulled = received.unwrap();
            assert_eq!(pulled.outcome, Outcome::FastForward);
            assert_eq!(*b.read(), 3);
            b.verify().unwrap();
            continue;
        }
        match received {
            Err(Error::Bundle(_)) => {}
            other => panic!("accepted a forged {name} merge: {other:?}"),
        }
        assert_eq!(b.version(), 0);
        assert_eq!(b.size_on_disk(), size);
    }
}

#[test]
fn a_bundle_whose_head_is_past_the_last_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let b: Db<u64> = fast().open(dir.path().join("b.ferro")).unwrap();
    let root = object_of(&7u64);
    let parent = Ptr {
        hash: [9; 20],
        offset: 0,
    };
    let tip = entry_of(u64::MAX, &root, Some(parent), None);
    match b.receive("evil", &forged(&tip, u64::MAX, &[&root, &tip]), untouched) {
        Err(Error::Bundle(_)) => {}
        other => panic!("accepted a head at the last version: {other:?}"),
    }
    assert_eq!(b.branches(), vec!["main".to_string()]);
    b.update(|value| *value += 1).unwrap();
    assert_eq!(b.version(), 1);
}

#[test]
fn a_revert_made_after_adopting_our_commits_is_not_lost() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "0");
    b.pull(&a, "a", refuse).unwrap();
    assert_eq!(a.pull(&b, "b", refuse).unwrap().outcome, Outcome::UpToDate);
    put(&a, 1, "1");
    assert_eq!(
        b.pull(&a, "a", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    put(&b, 1, "0");
    put(&a, 2, "x");
    assert_eq!(
        a.pull(&b, "b", theirs_win).unwrap().outcome,
        Outcome::Merged
    );
    assert_eq!(*a.read().get(&1).unwrap(), "0");
    for _ in 0..2 {
        b.pull(&a, "a", theirs_win).unwrap();
        a.pull(&b, "b", theirs_win).unwrap();
    }
    assert_eq!(dump(&a.read()), dump(&b.read()));
    assert_eq!(*b.read().get(&1).unwrap(), "0");
}

#[test]
fn a_pruned_branch_ahead_of_an_idle_remote_stays_ahead() {
    let dir = tempfile::tempdir().unwrap();
    let a = open(&dir, "a.ferro");
    let b: Db<Root> = fast()
        .history(Keep::Last(3))
        .open(dir.path().join("b.ferro"))
        .unwrap();
    for key in 0..5 {
        put(&a, key, "a");
    }
    assert_eq!(
        b.pull(&a, "a", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    b.commit("drop 0", |root| {
        root.remove(&0);
    })
    .unwrap();
    for key in 10..15 {
        put(&b, key, "b");
    }
    assert_eq!(b.pull(&a, "a", refuse).unwrap().outcome, Outcome::Ahead);
    assert!(!b.read().contains_key(&0));
    assert_eq!(dump(&b.read()).len(), 9);
}

#[test]
fn a_fast_forward_keeps_history_that_another_branch_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let b = open(&dir, "b.ferro");
    for key in 1..=10 {
        put(&b, key, "b");
    }
    let a = open(&dir, "a.ferro");
    a.pull(&b, "b", refuse).unwrap();
    put(&a, 11, "a");
    put(&a, 12, "a");
    drop(b);
    let pruning: Db<Root> = fast()
        .history(Keep::Last(2))
        .open(dir.path().join("b.ferro"))
        .unwrap();
    let side = pruning.fork("aside").unwrap();
    put(&side, 100, "side");
    assert_eq!(side.log().len(), 2);
    drop((side, pruning));
    let b = open(&dir, "b.ferro");
    assert_eq!(
        b.pull(&a, "a", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    assert_eq!(b.log().len(), 12);
    assert_eq!(dump(&b.at(3).unwrap()).len(), 3);
    b.verify().unwrap();
}

#[test]
fn a_stale_bundle_does_not_move_the_remote_back() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"));
    put(&a, 1, "one");
    b.pull(&a, "a", refuse).unwrap();
    put(&b, 9, "nine");
    let have = b.have().unwrap();
    put(&a, 1, "two");
    let stale = a.bundle(&have).unwrap();
    put(&a, 1, "three");
    assert_eq!(
        b.pull(&a, "a", theirs_win).unwrap().outcome,
        Outcome::Merged
    );
    let before = (b.head_hash(), b.size_on_disk());
    let replayed = b.receive("a", &stale, theirs_win).unwrap();
    assert_ne!(replayed.outcome, Outcome::Merged);
    assert_eq!(*b.read().get(&1).unwrap(), "three");
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    assert_eq!((b.head_hash(), b.size_on_disk()), before);
}

fn forged_with(
    tip: &Object,
    version: u64,
    bases: &[(ferro::Hash, u64)],
    roots: &[u64],
    hints: &[(u64, u64)],
    objects: &[(&Object, &[u64])],
) -> Bundle {
    let mut out = b"FERROBDL".to_vec();
    out.extend_from_slice(&[1, 1]);
    out.extend_from_slice(&tip.hash());
    varint(&mut out, version);
    varint(&mut out, bases.len() as u64);
    for (hash, at) in bases {
        out.extend_from_slice(hash);
        varint(&mut out, *at);
    }
    varint(&mut out, roots.len() as u64);
    for root in roots {
        varint(&mut out, *root);
    }
    varint(&mut out, hints.len() as u64);
    for (parent, index) in hints {
        varint(&mut out, *parent);
        varint(&mut out, *index);
    }
    varint(&mut out, objects.len() as u64);
    for (object, references) in objects {
        varint(&mut out, (object.body.len() as u64) << 1);
        out.extend_from_slice(&object.body);
        varint(&mut out, object.links.len() as u64);
        for (linked, reference) in object.links.iter().zip(references.iter()) {
            out.extend_from_slice(&linked.hash);
            varint(&mut out, *reference);
        }
    }
    Bundle::from(out)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn key_at_level(level: u32) -> u64 {
    (0..).find(|key| common::level_of(key) == level).unwrap()
}

fn node_of(key: u64, slot: &[u8], links: Vec<Ptr>) -> Object {
    let mut body = vec![common::level_of(&key) as u8, 1];
    varint(&mut body, key);
    body.extend_from_slice(slot);
    body.extend_from_slice(&[0, 0]);
    Object { body, links }
}

fn map_root(count: u64, node: &Object) -> Object {
    let mut body = Vec::new();
    varint(&mut body, count);
    let links = if count == 0 {
        Vec::new()
    } else {
        vec![link_to(node)]
    };
    Object { body, links }
}

fn inline_string(text: &str) -> Vec<u8> {
    let mut slot = vec![0];
    varint(&mut slot, text.len() as u64);
    slot.extend_from_slice(text.as_bytes());
    slot
}

type Received<T> = (Db<T>, ferro::Result<Pulled<()>>, TempDir);

fn receive_tree<T: Data + Default>(objects: &[&Object]) -> Received<T> {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<T> = fast().open(dir.path().join("b.ferro")).unwrap();
    let root = objects.last().unwrap();
    let tip = entry_of(1, root, None, None);
    let mut all = objects.to_vec();
    all.push(&tip);
    let result = db.receive(
        "evil",
        &forged(&tip, 1, &all),
        |_: &mut T, _, _| -> (String, ()) { panic!("no merge expected") },
    );
    (db, result, dir)
}

fn assert_refused<T: Data>(db: &Db<T>, result: ferro::Result<Pulled<()>>) {
    match result {
        Err(Error::Bundle(_)) => {}
        other => panic!("accepted a tree that does not read: {other:?}"),
    }
    assert_eq!(db.version(), 0);
    assert_eq!(db.branches(), vec!["main".to_string()]);
    db.verify_typed().unwrap();
}

#[test]
fn a_bundle_whose_tree_does_not_decode_is_rejected() {
    let node = Object {
        body: vec![0xff, 0xff, 0xff],
        links: Vec::new(),
    };
    let root = map_root(1, &node);
    let (b, result, _dir) = receive_tree::<Root>(&[&node, &root]);
    assert_refused(&b, result);
    assert!(b.read().is_empty());
}

#[test]
fn a_bundle_whose_map_count_disagrees_with_its_nodes_is_rejected() {
    let key = key_at_level(0);
    let node = node_of(key, &inline_string("one"), Vec::new());
    let honest = map_root(1, &node);
    let (b, result, _dir) = receive_tree::<Root>(&[&node, &honest]);
    assert_eq!(result.unwrap().outcome, Outcome::FastForward);
    assert_eq!(dump(&b.read()), BTreeMap::from([(key, "one".to_string())]));
    b.verify_typed().unwrap();
    for count in [2, 1 << 40] {
        let lying = map_root(count, &node);
        let (b, result, _dir) = receive_tree::<Root>(&[&node, &lying]);
        assert_refused(&b, result);
    }
}

#[test]
fn a_bundle_whose_keys_sit_at_the_wrong_level_or_out_of_order_is_rejected() {
    let high = key_at_level(1);
    let mut misplaced = node_of(high, &inline_string("x"), Vec::new());
    misplaced.body[0] = 0;
    let root = map_root(1, &misplaced);
    let (b, result, _dir) = receive_tree::<Root>(&[&misplaced, &root]);
    assert_refused(&b, result);

    let low: Vec<u64> = (0..)
        .filter(|key| common::level_of(key) == 0)
        .take(2)
        .collect();
    let mut body = vec![0, 2];
    varint(&mut body, low[1]);
    varint(&mut body, low[0]);
    body.extend_from_slice(&inline_string("a"));
    body.extend_from_slice(&inline_string("b"));
    body.extend_from_slice(&[0, 0, 0]);
    let unordered = Object {
        body,
        links: Vec::new(),
    };
    let root = map_root(2, &unordered);
    let (b, result, _dir) = receive_tree::<Root>(&[&unordered, &root]);
    assert_refused(&b, result);
}

#[test]
fn a_bundle_whose_spilled_value_or_nested_map_does_not_decode_is_rejected() {
    let key = key_at_level(0);
    let spill = Object {
        body: vec![2, 0xff, 0xfe],
        links: Vec::new(),
    };
    let node = node_of(key, &[1], vec![link_to(&spill)]);
    let root = map_root(1, &node);
    let (b, result, _dir) = receive_tree::<Root>(&[&spill, &node, &root]);
    assert_refused(&b, result);

    let inner = Object {
        body: vec![0, 1, 0x80],
        links: Vec::new(),
    };
    let outer = node_of(key, &[0, 1], vec![link_to(&inner)]);
    let root = map_root(1, &outer);
    let (b, result, _dir) = receive_tree::<Map<u64, Map<u64, u64>>>(&[&inner, &outer, &root]);
    assert_refused(&b, result);
}

#[test]
fn a_bundle_that_reuses_held_data_as_another_type_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let b = open(&dir, "b.ferro");
    let key = key_at_level(0);
    let long = "x".repeat(600);
    put(&b, key, &long);
    let spill = object_of(&long);
    let node = node_of(key, &[1], vec![link_to(&spill)]);
    assert_eq!(b.read().root_hash(), Some(node.hash()));
    let v1 = b.log().hash_at(&1).unwrap();
    let size = b.size_on_disk();
    let before = b.head_hash();
    for (count, held_as) in [(1u8, 3u64), (4, 2), (1, 2)] {
        let target = if held_as == 3 { &spill } else { &node };
        let root = Object {
            body: vec![count],
            links: vec![link_to(target)],
        };
        let tip = forged_entry(
            &root,
            Forged {
                version: 2,
                time: now_ms(),
                prev: Some(Ptr {
                    hash: v1,
                    offset: 0,
                }),
                merged: None,
                graph: None,
            },
        );
        let bundle = forged_with(
            &tip,
            2,
            &[(v1, 1)],
            &[0],
            &[(0, 0), (1, 0)],
            &[(&root, &[held_as]), (&tip, &[0, 0])],
        );
        let result = b.receive("evil", &bundle, refuse);
        if (count, held_as) == (1, 2) {
            assert_eq!(result.unwrap().outcome, Outcome::FastForward);
            assert_eq!(*b.read().get(&key).unwrap(), long);
            continue;
        }
        match result {
            Err(Error::Bundle(_)) => {}
            other => panic!("accepted a link to held data of another type: {other:?}"),
        }
        assert_eq!(b.head_hash(), before);
        assert_eq!(b.size_on_disk(), size);
    }
    b.verify_typed().unwrap();
}

#[test]
fn verify_typed_finds_nodes_that_do_not_decode_as_the_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("numbers.ferro");
    let numbers: Db<Map<u64, u64>> = fast().open(&path).unwrap();
    numbers
        .commit("seed", |root| {
            for key in 0..100 {
                root.insert(key, 200 + key);
            }
        })
        .unwrap();
    numbers.verify_typed().unwrap();
    drop(numbers);
    let words: Db<Root> = fast().open(&path).unwrap();
    words.verify().unwrap();
    match words.verify_typed() {
        Err(Error::Corrupt(message)) => assert!(message.contains("decode"), "{message}"),
        other => panic!("typed verify passed a file whose nodes do not decode: {other:?}"),
    }
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct Sum(u64);

impl Summary<u64, u64> for Sum {
    fn of(key: &u64, value: &u64) -> Self {
        Sum(key.wrapping_mul(3).wrapping_add(*value))
    }

    fn combine(&self, other: &Self) -> Self {
        Sum(self.0.wrapping_add(other.0))
    }
}

impl Summary<u64, String> for Sum {
    fn of(key: &u64, value: &String) -> Self {
        Sum(key.wrapping_mul(3).wrapping_add(value.len() as u64))
    }

    fn combine(&self, other: &Self) -> Self {
        Sum(self.0.wrapping_add(other.0))
    }
}

type Summed = Map<u64, u64, Sum>;
type Spilled = Map<u64, String, Sum>;

enum Slot<'a> {
    Inline(u64),
    Spill(&'a Object, u64),
}

fn summed_node(entries: &[(u64, Slot<'_>)], children: &[(usize, u64, u64, &Object)]) -> Object {
    let mut body = vec![common::level_of(&entries[0].0) as u8];
    varint(&mut body, entries.len() as u64);
    for (key, _) in entries {
        varint(&mut body, *key);
    }
    let mut links = Vec::new();
    for (_, slot) in entries {
        match slot {
            Slot::Inline(value) => {
                body.push(0);
                varint(&mut body, *value);
            }
            Slot::Spill(value, sum) => {
                body.push(1);
                varint(&mut body, *sum);
                links.push(link_to(value));
            }
        }
    }
    for position in 0..=entries.len() {
        match children.iter().find(|(at, ..)| *at == position) {
            Some((_, count, sum, child)) => {
                varint(&mut body, *count);
                varint(&mut body, *sum);
                links.push(link_to(child));
            }
            None => body.push(0),
        }
    }
    Object { body, links }
}

fn summed_root(count: u64, sum: u64, node: &Object) -> Object {
    let mut body = Vec::new();
    varint(&mut body, count);
    varint(&mut body, sum);
    Object {
        body,
        links: vec![link_to(node)],
    }
}

fn unmerged<T>(_: &mut T, _: &Snapshot<T>, _: &Snapshot<T>) -> (String, ()) {
    panic!("no merge expected")
}

fn level_zero_keys(count: usize) -> Vec<u64> {
    (0..)
        .filter(|key| common::level_of(key) == 0)
        .take(count)
        .collect()
}

fn assert_forged<T: Data>(db: &Db<T>, result: ferro::Result<Pulled<()>>) {
    match &result {
        Err(Error::Bundle(message)) => assert!(message.contains("summary"), "{message}"),
        other => panic!("accepted a forged summary: {other:?}"),
    }
    assert_refused(db, result);
}

#[test]
fn a_bundle_whose_summary_is_not_the_fold_of_its_entries_is_rejected() {
    let key = key_at_level(0);
    let leaf = summed_node(&[(key, Slot::Inline(7))], &[]);
    let honest = Sum::of(&key, &7u64).0;
    let root = summed_root(1, honest, &leaf);
    let (b, result, _dir) = receive_tree::<Summed>(&[&leaf, &root]);
    assert_eq!(result.unwrap().outcome, Outcome::FastForward);
    assert_eq!(b.read().summary(), Some(Sum(honest)));
    b.verify_typed().unwrap();
    for forged in [honest + 1, 0, u64::MAX] {
        let root = summed_root(1, forged, &leaf);
        let (b, result, _dir) = receive_tree::<Summed>(&[&leaf, &root]);
        assert_forged(&b, result);
    }

    let high = key_at_level(1);
    let low = (0..high).find(|key| common::level_of(key) == 0).unwrap();
    let below = summed_node(&[(low, Slot::Inline(5))], &[]);
    let truth = Sum::of(&low, &5u64).0;
    let entry = Sum::of(&high, &9u64).0;
    for claimed in [truth, truth + 1, truth.wrapping_sub(1)] {
        let top = summed_node(&[(high, Slot::Inline(9))], &[(0, 1, claimed, &below)]);
        let root = summed_root(2, claimed.wrapping_add(entry), &top);
        let (b, result, _dir) = receive_tree::<Summed>(&[&below, &top, &root]);
        if claimed != truth {
            assert_forged(&b, result);
            continue;
        }
        assert_eq!(result.unwrap().outcome, Outcome::FastForward);
        let read = b.read();
        assert_eq!(read.summary(), Some(Sum(truth + entry)));
        assert_eq!(read.range_summary(..high), Some(Sum(truth)));
        b.verify_typed().unwrap();
    }
}

#[test]
fn a_bundle_whose_spilled_value_carries_another_entrys_summary_is_rejected() {
    let keys = level_zero_keys(2);
    let long = "x".repeat(600);
    let spill = object_of(&long);
    let of = |key: u64| Sum::of(&key, &long).0;
    let leaf = |first: u64, second: u64| {
        summed_node(
            &[
                (keys[0], Slot::Spill(&spill, first)),
                (keys[1], Slot::Spill(&spill, second)),
            ],
            &[],
        )
    };
    let honest = leaf(of(keys[0]), of(keys[1]));
    let root = summed_root(2, of(keys[0]).wrapping_add(of(keys[1])), &honest);
    let (b, result, _dir) = receive_tree::<Spilled>(&[&spill, &honest, &root]);
    assert_eq!(result.unwrap().outcome, Outcome::FastForward);
    assert_eq!(*b.read().get(&keys[1]).unwrap(), long);
    assert_eq!(b.read().range_summary(keys[1]..), Some(Sum(of(keys[1]))));
    b.verify_typed().unwrap();
    for (first, second) in [
        (of(keys[0]), of(keys[0])),
        (of(keys[1]), of(keys[1])),
        (of(keys[1]), of(keys[0])),
        (of(keys[0]), of(keys[1]) + 1),
    ] {
        let forged = leaf(first, second);
        let root = summed_root(2, first.wrapping_add(second), &forged);
        let (b, result, _dir) = receive_tree::<Spilled>(&[&spill, &forged, &root]);
        assert_forged(&b, result);
    }
}

#[test]
fn a_bundle_that_links_one_node_under_two_summaries_is_rejected() {
    let key = key_at_level(0);
    let leaf = summed_node(&[(key, Slot::Inline(3))], &[]);
    let honest = Sum::of(&key, &3u64).0;
    let pair = |first: u64, second: u64| {
        let mut body = Vec::new();
        for sum in [first, second] {
            varint(&mut body, 1);
            varint(&mut body, sum);
        }
        Object {
            body,
            links: vec![link_to(&leaf), link_to(&leaf)],
        }
    };
    let root = pair(honest, honest);
    let (b, result, _dir) = receive_tree::<(Summed, Summed)>(&[&leaf, &root]);
    assert_eq!(result.unwrap().outcome, Outcome::FastForward);
    assert_eq!(b.read().1.summary(), Some(Sum(honest)));
    b.verify_typed().unwrap();
    for (first, second) in [(honest, honest + 1), (honest + 1, honest)] {
        let root = pair(first, second);
        let (b, result, _dir) = receive_tree::<(Summed, Summed)>(&[&leaf, &root]);
        assert_forged(&b, result);
    }
}

#[test]
fn a_bundle_that_reuses_held_data_under_a_forged_summary_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let b: Db<Spilled> = fast().open(dir.path().join("b.ferro")).unwrap();
    let keys = level_zero_keys(2);
    let long = "y".repeat(600);
    let of = |key: u64| Sum::of(&key, &long).0;
    b.update(|root| {
        root.insert(keys[0], long.clone());
    })
    .unwrap();
    let spill = object_of(&long);
    let held = summed_node(&[(keys[0], Slot::Spill(&spill, of(keys[0])))], &[]);
    assert_eq!(b.read().root_hash(), Some(held.hash()));
    let v1 = b.log().hash_at(&1).unwrap();
    let base = Ptr {
        hash: v1,
        offset: 0,
    };
    let tip_of = |root: &Object| {
        forged_entry(
            root,
            Forged {
                version: 2,
                time: now_ms(),
                prev: Some(base),
                merged: None,
                graph: None,
            },
        )
    };
    let before = (b.head_hash(), b.size_on_disk());
    for forged in [of(keys[0]) + 1, of(keys[1])] {
        let root = summed_root(1, forged, &held);
        let tip = tip_of(&root);
        let bundle = forged_with(
            &tip,
            2,
            &[(v1, 1)],
            &[0],
            &[(0, 0)],
            &[(&root, &[2]), (&tip, &[0, 0])],
        );
        match b.receive("evil", &bundle, unmerged) {
            Err(Error::Bundle(message)) => assert!(message.contains("summary"), "{message}"),
            other => panic!("accepted a held node under a forged summary: {other:?}"),
        }
        assert_eq!((b.head_hash(), b.size_on_disk()), before);
    }
    for (claimed, lands) in [(of(keys[0]), false), (of(keys[1]), true)] {
        let leaf = summed_node(&[(keys[1], Slot::Spill(&spill, claimed))], &[]);
        let root = summed_root(1, claimed, &leaf);
        let tip = tip_of(&root);
        let bundle = forged_with(
            &tip,
            2,
            &[(v1, 1)],
            &[0],
            &[(0, 0), (1, 0)],
            &[(&leaf, &[3]), (&root, &[0]), (&tip, &[0, 0])],
        );
        let result = b.receive("evil", &bundle, unmerged);
        if lands {
            assert_eq!(result.unwrap().outcome, Outcome::FastForward);
            let read = b.read();
            assert_eq!(*read.get(&keys[1]).unwrap(), long);
            assert_eq!(read.summary(), Some(Sum(of(keys[1]))));
            continue;
        }
        match result {
            Err(Error::Bundle(message)) => assert!(message.contains("summary"), "{message}"),
            other => panic!("accepted a held value under another entry's summary: {other:?}"),
        }
        assert_eq!((b.head_hash(), b.size_on_disk()), before);
    }
    b.verify_typed().unwrap();
}

#[derive(Data, Clone, Debug, PartialEq)]
struct Tally {
    count: u64,
    bytes: u64,
    first: u64,
    last: u64,
    weight: f64,
}

impl Summary<u64, String> for Tally {
    fn of(key: &u64, value: &String) -> Self {
        Tally {
            count: 1,
            bytes: value.len() as u64,
            first: *key,
            last: *key,
            weight: *key as f64 * 0.1 + value.len() as f64 / 3.0,
        }
    }

    fn combine(&self, other: &Self) -> Self {
        Tally {
            count: self.count + other.count,
            bytes: self.bytes + other.bytes,
            first: self.first,
            last: other.last,
            weight: self.weight + other.weight,
        }
    }
}

type Tallied = Map<u64, String, Tally>;

fn same_tallies(ours: &Tallied, theirs: &Tallied) {
    assert_eq!(ours.root_hash(), theirs.root_hash());
    assert_eq!(ours.summary(), theirs.summary());
    let (mut count, mut bytes) = (0, 0);
    for (_, value) in ours.iter() {
        count += 1;
        bytes += value.len() as u64;
    }
    let summary = ours.summary().unwrap();
    assert_eq!((summary.count, summary.bytes), (count, bytes));
    for (low, high) in [
        (0, 9_000),
        (1_234, 40_000),
        (30_001, 30_002),
        (59_000, u64::MAX),
    ] {
        assert_eq!(
            ours.range_summary(low..high),
            theirs.range_summary(low..high)
        );
    }
}

#[test]
fn summarized_maps_land_whole_through_clones_and_scattered_edits() {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Tallied> = fast().open(dir.path().join("a.ferro")).unwrap();
    let b: Db<Tallied> = fast().open(dir.path().join("b.ferro")).unwrap();
    let mut rng = Rng::new(83);
    let shared = text(1, 900);
    let value = |rng: &mut Rng, key: u64| match rng.below(10) {
        0 => shared.clone(),
        1 => text(key ^ rng.next(), 520 + rng.below(400) as usize),
        _ => text(key ^ rng.next(), rng.below(60) as usize),
    };
    a.update(|root| {
        for key in 0..20_000u64 {
            root.insert(key * 3, value(&mut rng, key));
        }
    })
    .unwrap();
    assert_eq!(
        b.pull(&a, "a", unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    same_tallies(&b.read(), &a.read());
    for _ in 0..4 {
        a.update(|root| {
            for _ in 0..300 {
                let key = rng.below(64_000);
                match rng.below(4) {
                    0 => {
                        root.remove(&key);
                    }
                    _ => {
                        root.insert(key, value(&mut rng, key));
                    }
                }
            }
        })
        .unwrap();
        assert_eq!(
            b.pull(&a, "a", unmerged).unwrap().outcome,
            Outcome::FastForward
        );
        same_tallies(&b.read(), &a.read());
    }
    b.verify_typed().unwrap();
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct Doubled(u64);

impl Summary<u64, u64> for Doubled {
    fn of(key: &u64, value: &u64) -> Self {
        Doubled(Sum::of(key, value).0.wrapping_mul(2))
    }

    fn combine(&self, other: &Self) -> Self {
        Doubled(self.0.wrapping_add(other.0))
    }
}

impl Summary<u64, String> for Doubled {
    fn of(key: &u64, value: &String) -> Self {
        Doubled(Sum::of(key, value).0.wrapping_mul(2))
    }

    fn combine(&self, other: &Self) -> Self {
        Doubled(self.0.wrapping_add(other.0))
    }
}

fn assert_stale_summary<T: Data>(db: &Db<T>) {
    db.verify().unwrap();
    match db.verify_typed() {
        Err(Error::Corrupt(message)) => assert!(message.contains("summary"), "{message}"),
        other => panic!("typed verify passed summaries that are not the fold: {other:?}"),
    }
}

#[test]
fn verify_typed_finds_summaries_that_are_not_the_fold_of_their_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("summed.ferro");
    let summed: Db<Summed> = fast().open(&path).unwrap();
    summed
        .update(|root| root.extend((0..2_000u64).map(|key| (key, key * 7))))
        .unwrap();
    summed.verify_typed().unwrap();
    drop(summed);
    assert_stale_summary(&fast().open::<Map<u64, u64, Doubled>>(&path).unwrap());

    let path = dir.path().join("spilled.ferro");
    let spilled: Db<Spilled> = fast().open(&path).unwrap();
    let values: Vec<String> = (0..4u64)
        .map(|seed| text(seed, 600 + seed as usize))
        .collect();
    spilled
        .update(|root| root.extend((0..400u64).map(|key| (key, values[key as usize % 4].clone()))))
        .unwrap();
    spilled.verify_typed().unwrap();
    drop(spilled);
    assert_stale_summary(&fast().open::<Map<u64, String, Doubled>>(&path).unwrap());
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct Wrapping(u64);

impl Summary<u64, u64> for Wrapping {
    fn of(_key: &u64, value: &u64) -> Self {
        Wrapping(*value)
    }

    fn combine(&self, other: &Self) -> Self {
        Wrapping(self.0.wrapping_add(other.0))
    }
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct Strict(u64);

impl Summary<u64, u64> for Strict {
    fn of(_key: &u64, value: &u64) -> Self {
        Strict(*value)
    }

    fn combine(&self, other: &Self) -> Self {
        Strict(
            self.0
                .checked_add(other.0)
                .expect("ferro: test summary overflowed"),
        )
    }
}

#[test]
fn a_bundle_whose_summary_panics_to_recompute_is_refused() {
    common::quiet_ferro_panics();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ferro");
    let a: Db<Map<u64, u64, Wrapping>> = fast().open(&path).unwrap();
    a.update(|root| root.extend((0..500u64).map(|key| (key, u64::MAX / 100))))
        .unwrap();
    let b: Db<Map<u64, u64, Strict>> = fast().open(dir.path().join("b.ferro")).unwrap();
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    match b.receive("a", &bundle, unmerged) {
        Err(Error::Bundle(message)) => assert!(message.contains("panicked"), "{message}"),
        other => panic!("accepted a bundle whose summary overflows: {other:?}"),
    }
    assert_eq!(b.branches(), vec!["main".to_string()]);
    assert_eq!(b.version(), 0);
    b.update(|root| {
        root.insert(1, 2);
    })
    .unwrap();
    assert_eq!(b.read().summary(), Some(Strict(2)));
    drop(a);
    let reread: Db<Map<u64, u64, Strict>> = fast().open(&path).unwrap();
    reread.verify().unwrap();
    match reread.verify_typed() {
        Err(Error::Corrupt(message)) => assert!(message.contains("panicked"), "{message}"),
        other => panic!("typed verify passed a summary that overflows: {other:?}"),
    }
}

type Inner = Map<u64, u64>;
type Note = (String, Inner);

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct InnerSum(u64);

impl Summary<u64, Inner> for InnerSum {
    fn of(key: &u64, value: &Inner) -> Self {
        let inner = value.iter().fold(0u64, |sum, (_, v)| sum.wrapping_add(*v));
        InnerSum(key.wrapping_add(inner))
    }

    fn combine(&self, other: &Self) -> Self {
        InnerSum(self.0.wrapping_add(other.0))
    }
}

impl Summary<u64, Note> for InnerSum {
    fn of(key: &u64, value: &Note) -> Self {
        let inner = value
            .1
            .iter()
            .fold(0u64, |sum, (_, v)| sum.wrapping_add(*v));
        InnerSum(key.wrapping_add(inner).wrapping_add(value.0.len() as u64))
    }

    fn combine(&self, other: &Self) -> Self {
        InnerSum(self.0.wrapping_add(other.0))
    }
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct InnerCount(u64);

impl Summary<u64, Inner> for InnerCount {
    fn of(_key: &u64, value: &Inner) -> Self {
        InnerCount(value.len() as u64)
    }

    fn combine(&self, other: &Self) -> Self {
        InnerCount(self.0.wrapping_add(other.0))
    }
}

impl Summary<u64, Note> for InnerCount {
    fn of(_key: &u64, value: &Note) -> Self {
        InnerCount(value.1.len() as u64)
    }

    fn combine(&self, other: &Self) -> Self {
        InnerCount(self.0.wrapping_add(other.0))
    }
}

#[derive(Data, Clone)]
struct Nested<S: Summary<u64, Inner> + Summary<u64, Note>> {
    inline: Map<u64, Inner, S>,
    spilled: Map<u64, Note, S>,
}

impl<S: Summary<u64, Inner> + Summary<u64, Note>> Default for Nested<S> {
    fn default() -> Self {
        Nested {
            inline: Map::default(),
            spilled: Map::default(),
        }
    }
}

fn inner(seed: u64, len: u64) -> Inner {
    (0..len).map(|key| (key, seed * 1_000 + key)).collect()
}

fn same_nested(ours: &Nested<InnerSum>, theirs: &Nested<InnerSum>) {
    assert_eq!(ours.inline.root_hash(), theirs.inline.root_hash());
    assert_eq!(ours.inline.summary(), theirs.inline.summary());
    assert_eq!(
        ours.inline.range_summary(5..60),
        theirs.inline.range_summary(5..60)
    );
    assert_eq!(ours.spilled.root_hash(), theirs.spilled.root_hash());
    assert_eq!(ours.spilled.summary(), theirs.spilled.summary());
}

#[test]
fn a_summary_whose_of_reads_a_nested_map_lands_and_verifies() {
    common::quiet_ferro_panics();
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Nested<InnerSum>> = fast().open(dir.path().join("a.ferro")).unwrap();
    a.update(|root| {
        for key in 0..120u64 {
            root.inline.insert(key, inner(key, 1 + key % 40));
            let note = (
                text(key, 520 + key as usize % 30),
                inner(key + 7, 3 + key % 5),
            );
            root.spilled.insert(key, note);
        }
        root.inline.insert(1_000, Map::new());
    })
    .unwrap();
    a.verify_typed().unwrap();
    let b: Db<Nested<InnerSum>> = fast().open(dir.path().join("b.ferro")).unwrap();
    assert_eq!(
        b.pull(&a, "a", unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    same_nested(&b.read(), &a.read());
    b.verify_typed().unwrap();
    for round in 0..3u64 {
        a.update(|root| {
            for key in (round..120).step_by(11) {
                root.inline.update(&key, |value| {
                    value.insert(500 + round, round);
                });
                root.spilled.update(&key, |value| {
                    value.1.insert(900 + round, key);
                });
            }
            root.inline.insert(2_000 + round, inner(round, 50));
        })
        .unwrap();
        assert_eq!(
            b.pull(&a, "a", unmerged).unwrap().outcome,
            Outcome::FastForward
        );
        same_nested(&b.read(), &a.read());
    }
    b.verify_typed().unwrap();

    let path = dir.path().join("c.ferro");
    let c: Db<Nested<InnerCount>> = fast().open(&path).unwrap();
    match c.receive("a", &a.bundle(&c.have().unwrap()).unwrap(), unmerged) {
        Err(Error::Bundle(message)) => assert!(message.contains("summary"), "{message}"),
        other => panic!("accepted nested summaries another `of` made: {other:?}"),
    }
    assert_eq!(c.version(), 0);
    drop((a, c));
    let reread: Db<Nested<InnerCount>> = fast().open(dir.path().join("a.ferro")).unwrap();
    match reread.verify_typed() {
        Err(Error::Corrupt(message)) => assert!(message.contains("summary"), "{message}"),
        other => panic!("typed verify passed nested summaries another `of` made: {other:?}"),
    }
}

type Outer<S> = Map<u64, Inner, S>;

fn deep_inner() -> Inner {
    (0..20_000u64).map(|key| (key, key * 3)).collect()
}

fn keep_ours<T>(_: &mut T, _: &Snapshot<T>, _: &Snapshot<T>) -> (String, ()) {
    ("keep ours".to_string(), ())
}

fn holding_deep_inner(dir: &TempDir) -> Db<Outer<InnerSum>> {
    let path = dir.path().join("b.ferro");
    {
        let b: Db<Outer<InnerSum>> = common::roomy().open(&path).unwrap();
        b.update(|root| {
            root.insert(1, deep_inner());
        })
        .unwrap();
    }
    let b: Db<Outer<InnerSum>> = common::roomy().open(&path).unwrap();
    assert_eq!(*b.read().get(&1).unwrap().get(&0).unwrap(), 0);
    b
}

fn sender_of_deep_inner<S: Summary<u64, Inner>>(dir: &TempDir) -> Db<Outer<S>> {
    let a: Db<Outer<S>> = fast().open(dir.path().join("a.ferro")).unwrap();
    a.update(|root| {
        root.insert(1, deep_inner());
        root.insert(2, inner(9, 5));
    })
    .unwrap();
    a
}

fn assert_deep_inner_whole(b: &Db<Outer<InnerSum>>) {
    let read = b.read();
    let inner = read.get(&1).unwrap();
    let sum = inner
        .iter()
        .fold(0u64, |sum, (_, value)| sum.wrapping_add(*value));
    let want = (0..20_000u64).fold(0u64, |sum, key| sum.wrapping_add(key * 3));
    assert_eq!(sum, want);
    for key in [19_999u64, 10_001, 5_001, 15_000, 2_745] {
        assert_eq!(*inner.get(&key).unwrap(), key * 3);
    }
}

#[test]
fn a_receive_leaves_the_receivers_own_nested_maps_whole() {
    let dir = tempfile::tempdir().unwrap();
    let b = holding_deep_inner(&dir);
    let a = sender_of_deep_inner::<InnerSum>(&dir);
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    assert_eq!(
        b.receive("a", &bundle, keep_ours).unwrap().outcome,
        Outcome::Merged
    );
    assert_deep_inner_whole(&b);
    b.update(|root| {
        root.update(&1, |inner| {
            for key in [19_999u64, 10_001, 5_001, 2_745] {
                inner.insert(key, 7);
            }
        });
    })
    .unwrap();
    assert_eq!(*b.read().get(&1).unwrap().get(&10_001).unwrap(), 7);
    drop(b);
    let reopened: Db<Outer<InnerSum>> = fast().open(dir.path().join("b.ferro")).unwrap();
    reopened.verify().unwrap();
    reopened.verify_typed().unwrap();
    assert_eq!(reopened.read().get(&1).unwrap().iter().count(), 20_000);
    assert_eq!(
        reopened
            .branch("a")
            .unwrap()
            .read()
            .get(&1)
            .unwrap()
            .iter()
            .count(),
        20_000
    );
}

#[test]
fn a_refused_bundle_leaves_the_receivers_own_nested_maps_whole() {
    let dir = tempfile::tempdir().unwrap();
    let b = holding_deep_inner(&dir);
    let a = sender_of_deep_inner::<InnerCount>(&dir);
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    match b.receive("a", &bundle, keep_ours) {
        Err(Error::Bundle(message)) => assert!(message.contains("summary"), "{message}"),
        other => panic!("accepted summaries another `of` made: {other:?}"),
    }
    assert_eq!(b.branches(), vec!["main".to_string()]);
    assert_deep_inner_whole(&b);
    b.verify_typed().unwrap();
}

#[test]
fn a_nested_map_larger_than_the_receivers_file_could_hold_lands() {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Outer<InnerSum>> = fast().open(dir.path().join("a.ferro")).unwrap();
    a.update(|root| {
        root.insert(1, (0..2_200_000u64).map(|key| (key, key)).collect());
    })
    .unwrap();
    let b: Db<Outer<InnerSum>> = fast().open(dir.path().join("b.ferro")).unwrap();
    assert_eq!(
        b.pull(&a, "a", unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    assert_eq!(b.read().summary(), a.read().summary());
    assert_eq!(b.read().get(&1).unwrap().len(), 2_200_000);
}

#[test]
fn one_nested_map_summarized_under_many_keys_lands_or_is_refused_within_a_budget() {
    let dir = tempfile::tempdir().unwrap();
    let shared: Inner = (0..40_000u64).map(|key| (key, key)).collect();
    let keys = level_zero_keys(1_500);
    let a: Db<Outer<InnerSum>> = fast().open(dir.path().join("a.ferro")).unwrap();
    a.update(|root| {
        for key in &keys[..20] {
            root.insert(*key, shared.clone());
        }
    })
    .unwrap();
    let b: Db<Outer<InnerSum>> = fast().open(dir.path().join("b.ferro")).unwrap();
    assert_eq!(
        b.pull(&a, "a", unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    assert_eq!(b.read().summary(), a.read().summary());
    let cheap: Db<Outer<InnerCount>> = fast().open(dir.path().join("c.ferro")).unwrap();
    cheap
        .update(|root| {
            for key in &keys {
                root.insert(*key, shared.clone());
            }
        })
        .unwrap();
    let d: Db<Outer<InnerSum>> = fast().open(dir.path().join("d.ferro")).unwrap();
    let bundle = cheap.bundle(&d.have().unwrap()).unwrap();
    let started = Instant::now();
    match d.receive("a", &bundle, unmerged) {
        Err(Error::Bundle(message)) => assert!(message.contains("size allows"), "{message}"),
        other => panic!("read one nested map 1500 times for a small bundle: {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(d.version(), 0);
}

#[test]
fn small_spilled_values_shared_by_very_many_keys_land() {
    for (keys, size) in [(500_000u64, 600usize), (100_000, 4_096)] {
        let dir = tempfile::tempdir().unwrap();
        let a: Db<Spilled> = fast().open(dir.path().join("a.ferro")).unwrap();
        let shared = "v".repeat(size);
        for chunk in 0..10 {
            a.update(|root| {
                for key in chunk * keys / 10..(chunk + 1) * keys / 10 {
                    root.insert(key, shared.clone());
                }
            })
            .unwrap();
        }
        let b: Db<Spilled> = fast().open(dir.path().join("b.ferro")).unwrap();
        assert_eq!(
            b.pull(&a, "a", unmerged).unwrap().outcome,
            Outcome::FastForward,
            "{keys} keys sharing one value of {size} bytes"
        );
        assert_eq!(b.read().summary(), a.read().summary());
    }
}

#[test]
fn a_small_held_value_put_under_very_many_keys_lands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    let shared = "h".repeat(600);
    let b: Db<Spilled> = fast().open(&path).unwrap();
    b.update(|root| {
        root.insert(0, shared.clone());
    })
    .unwrap();
    drop(b);
    fs::copy(&path, dir.path().join("a.ferro")).unwrap();
    let a: Db<Spilled> = fast().open(dir.path().join("a.ferro")).unwrap();
    let b: Db<Spilled> = fast().open(&path).unwrap();
    a.update(|root| {
        for key in 1..100_000u64 {
            root.insert(key, shared.clone());
        }
    })
    .unwrap();
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    assert_eq!(
        b.receive("a", &bundle, unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    assert_eq!(b.read().summary(), a.read().summary());
}

type Pages = Map<u64, String>;

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct PageBytes(u64);

impl Summary<u64, Pages> for PageBytes {
    fn of(key: &u64, value: &Pages) -> Self {
        let bytes = value
            .iter()
            .fold(0u64, |sum, (_, page)| sum.wrapping_add(page.len() as u64));
        PageBytes(key.wrapping_add(bytes))
    }

    fn combine(&self, other: &Self) -> Self {
        PageBytes(self.0.wrapping_add(other.0))
    }
}

type Books = Map<u64, Pages, PageBytes>;

const PAGE_BYTES: usize = 32 << 10;

fn book(seed: u64, pages: u64) -> Pages {
    (0..pages)
        .map(|at| {
            let mut page = format!("{seed}:{at}:");
            page.extend(std::iter::repeat_n('p', PAGE_BYTES - page.len()));
            (at, page)
        })
        .collect()
}

fn shelved(dir: &TempDir, keys: &[u64], pages: u64) -> (Db<Books>, Db<Books>) {
    let a: Db<Books> = fast().open(dir.path().join("a.ferro")).unwrap();
    for (seed, key) in keys.iter().enumerate() {
        a.update(|root| {
            root.insert(*key, book(seed as u64, pages));
        })
        .unwrap();
    }
    let b: Db<Books> = fast().open(dir.path().join("b.ferro")).unwrap();
    assert_eq!(
        b.pull(&a, "a", unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    (a, b)
}

#[test]
fn small_edits_beside_held_nested_maps_past_the_allowance_land() {
    let dir = tempfile::tempdir().unwrap();
    let keys = level_zero_keys(13);
    let (a, b) = shelved(&dir, &keys[..10], 1_024);
    let edits: [&[usize]; 2] = [&[10], &[11, 12, 12]];
    for edit in edits {
        for (commit, at) in edit.iter().enumerate() {
            a.update(|root| {
                root.insert(keys[*at], book(*at as u64, 2 + commit as u64));
            })
            .unwrap();
        }
        let bundle = a.bundle(&b.have().unwrap()).unwrap();
        assert!(
            bundle.as_bytes().len() < 64 << 10,
            "{}",
            bundle.as_bytes().len()
        );
        assert_eq!(
            b.receive("a", &bundle, unmerged).unwrap().outcome,
            Outcome::FastForward,
            "{} commits beside 320 MiB of pages the receiver holds",
            edit.len()
        );
        assert_eq!(b.read().summary(), a.read().summary());
    }
    b.verify_typed().unwrap();
}

#[test]
fn a_small_edit_beside_held_nested_maps_of_numbers_lands() {
    let dir = tempfile::tempdir().unwrap();
    let keys = level_zero_keys(15);
    let entries = 180_000 * u64::from(cases(1));
    let a: Db<Outer<InnerSum>> = fast().open(dir.path().join("a.ferro")).unwrap();
    for (seed, key) in keys[..14].iter().enumerate() {
        a.update(|root| {
            root.insert(
                *key,
                (0..entries).map(|at| (at, at ^ seed as u64)).collect(),
            );
        })
        .unwrap();
    }
    let b: Db<Outer<InnerSum>> = fast().open(dir.path().join("b.ferro")).unwrap();
    assert_eq!(
        b.pull(&a, "a", unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    a.update(|root| {
        root.insert(keys[14], inner(3, 5));
    })
    .unwrap();
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    assert!(
        bundle.as_bytes().len() < 4 << 10,
        "{}",
        bundle.as_bytes().len()
    );
    assert_eq!(
        b.receive("a", &bundle, unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    assert_eq!(b.read().summary(), a.read().summary());
}

thread_local! {
    static SUMMARIZED: std::cell::RefCell<BTreeMap<(u64, usize), usize>> =
        const { std::cell::RefCell::new(BTreeMap::new()) };
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct Counted(u64);

impl Summary<u64, Inner> for Counted {
    fn of(key: &u64, value: &Inner) -> Self {
        let len = value.iter().count();
        SUMMARIZED.with(|summarized| *summarized.borrow_mut().entry((*key, len)).or_insert(0) += 1);
        Counted(key.wrapping_add(len as u64))
    }

    fn combine(&self, other: &Self) -> Self {
        Counted(self.0.wrapping_add(other.0))
    }
}

#[test]
fn an_entry_in_every_version_of_a_node_a_bundle_rewrites_is_summarized_once() {
    let dir = tempfile::tempdir().unwrap();
    let keys = level_zero_keys(12);
    let a: Db<Outer<Counted>> = fast().open(dir.path().join("a.ferro")).unwrap();
    a.update(|root| {
        for (seed, key) in keys[..6].iter().enumerate() {
            root.insert(*key, inner(seed as u64, 10 + seed as u64));
        }
    })
    .unwrap();
    let b: Db<Outer<Counted>> = fast().open(dir.path().join("b.ferro")).unwrap();
    assert_eq!(
        b.pull(&a, "a", unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    for (seed, key) in keys[6..].iter().enumerate() {
        a.update(|root| {
            root.insert(*key, inner(seed as u64, 20 + seed as u64));
        })
        .unwrap();
    }
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    SUMMARIZED.with(|summarized| summarized.borrow_mut().clear());
    assert_eq!(
        b.receive("a", &bundle, unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    let summarized = SUMMARIZED.with(|summarized| summarized.borrow().clone());
    assert_eq!(summarized.len(), 12, "{summarized:?}");
    assert!(
        summarized.values().all(|calls| *calls == 1),
        "{summarized:?}"
    );
    assert_eq!(b.read().summary(), a.read().summary());
}

type Notebook = Map<u64, (String, Pages), PageBytes>;

impl Summary<u64, (String, Pages)> for PageBytes {
    fn of(key: &u64, value: &(String, Pages)) -> Self {
        let pages = <PageBytes as Summary<u64, Pages>>::of(key, &value.1).0;
        PageBytes(pages.wrapping_add(value.0.len() as u64))
    }

    fn combine(&self, other: &Self) -> Self {
        PageBytes(self.0.wrapping_add(other.0))
    }
}

#[test]
fn a_held_value_whose_summary_reads_a_large_map_lands_under_a_few_keys_and_not_many() {
    for (keys, lands) in [(4u64, true), (400, false)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.ferro");
        let b: Db<Notebook> = fast().open(&path).unwrap();
        b.update(|root| {
            root.insert(0, ("n".repeat(600), book(0, 512)));
        })
        .unwrap();
        drop(b);
        fs::copy(&path, dir.path().join("a.ferro")).unwrap();
        let a: Db<Notebook> = fast().open(dir.path().join("a.ferro")).unwrap();
        let b: Db<Notebook> = fast().open(&path).unwrap();
        a.update(|root| {
            let note = (*root.get(&0).unwrap()).clone();
            for key in 1..=keys {
                root.insert(key, note.clone());
            }
        })
        .unwrap();
        let bundle = a.bundle(&b.have().unwrap()).unwrap();
        assert!(
            bundle.as_bytes().len() < 64 << 10,
            "{}",
            bundle.as_bytes().len()
        );
        let started = Instant::now();
        let result = b.receive("a", &bundle, unmerged);
        if lands {
            assert_eq!(result.unwrap().outcome, Outcome::FastForward);
            assert_eq!(b.read().summary(), a.read().summary());
            continue;
        }
        match result {
            Err(Error::Bundle(message)) => assert!(message.contains("size allows"), "{message}"),
            other => panic!("read a held 16 MiB map once for each of {keys} keys: {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(b.branches(), vec!["main".to_string()]);
    }
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct Aside(u64);

impl Summary<u64, Inner> for Aside {
    fn of(key: &u64, value: &Inner) -> Self {
        let inner = thread::scope(|scope| {
            scope
                .spawn(|| value.iter().fold(0u64, |sum, (_, v)| sum.wrapping_add(*v)))
                .join()
                .unwrap()
        });
        Aside(key.wrapping_add(inner))
    }

    fn combine(&self, other: &Self) -> Self {
        Aside(self.0.wrapping_add(other.0))
    }
}

#[test]
fn a_summary_that_reads_a_sent_nested_map_on_another_thread_lands() {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Outer<Aside>> = fast().open(dir.path().join("a.ferro")).unwrap();
    a.update(|root| {
        for key in 0..40u64 {
            root.insert(key, inner(key, 1 + key * 50));
        }
    })
    .unwrap();
    let b: Db<Outer<Aside>> = fast().open(dir.path().join("b.ferro")).unwrap();
    assert_eq!(
        b.pull(&a, "a", unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    a.update(|root| {
        root.update(&7, |value| {
            value.insert(9_000, 1);
        });
        root.insert(100, inner(100, 3_000));
    })
    .unwrap();
    assert_eq!(
        b.pull(&a, "a", unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    assert_eq!(b.read().summary(), a.read().summary());
    b.verify_typed().unwrap();
}

#[derive(Clone, Default)]
struct Lattice(u64);

impl Data for Lattice {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        if self.0 == 0 {
            w.varint(0);
            return Ok(());
        }
        let mut rng = Rng::new(self.0);
        w.child(|w| {
            for _ in 0..(64 << 20) / 8 {
                w.raw(&rng.next().to_le_bytes());
            }
            Ok(())
        })?;
        let (mut below, mut count) = (None, 0u64);
        for level in 0..self.0 {
            below = Some(w.child(|w| {
                w.u8(level as u8);
                w.varint(2);
                w.varint(2 * level);
                w.varint(2 * level + 1);
                for _ in 0..2 {
                    w.u8(0);
                    w.varint(level);
                }
                for _ in 0..3 {
                    match below {
                        Some(ptr) => {
                            w.varint(count);
                            w.link(ptr);
                        }
                        None => w.varint(0),
                    }
                }
                Ok(())
            })?);
            count = 3 * count + 2;
        }
        let key = key_at_level(0);
        let node = w.child(|w| {
            w.u8(0);
            w.varint(1);
            w.varint(key);
            w.u8(0);
            w.varint(count);
            w.link(below.expect("the lattice has levels"));
            w.varint(0);
            w.varint(0);
            Ok(())
        })?;
        w.varint(1);
        w.varint(0);
        w.link(node);
        Ok(())
    }

    fn decode(r: &mut ferro::codec::Decoder<'_>) -> ferro::Result<Self> {
        if r.varint()? == 0 {
            return Ok(Lattice(0));
        }
        r.varint()?;
        r.link()?;
        Ok(Lattice(1))
    }
}

#[test]
fn verify_typed_checks_a_nested_map_before_a_summary_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lattice.ferro");
    let lattice: Db<Lattice> = fast().open(&path).unwrap();
    lattice.update(|root| *root = Lattice(21)).unwrap();
    drop(lattice);
    let outer: Db<Outer<InnerSum>> = fast().open(&path).unwrap();
    outer.verify().unwrap();
    let started = Instant::now();
    match outer.verify_typed() {
        Err(Error::Corrupt(_)) => {}
        other => panic!("typed verify passed a nested map whose nodes share children: {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Tagged {
    #[key]
    id: u32,
    tag: u32,
}

mod indexed {
    use ferro::{Data, Record};

    #[derive(Data, Record, Clone, Debug, PartialEq)]
    pub struct Tagged {
        #[key]
        pub id: u32,
        #[index]
        pub tag: u32,
    }
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct Rows(u64);

impl Summary<u64, Table<Tagged>> for Rows {
    fn of(_key: &u64, value: &Table<Tagged>) -> Self {
        Rows(value.len() as u64)
    }

    fn combine(&self, other: &Self) -> Self {
        Rows(self.0.wrapping_add(other.0))
    }
}

impl Summary<u64, Table<indexed::Tagged>> for Rows {
    fn of(_key: &u64, value: &Table<indexed::Tagged>) -> Self {
        Rows(value.len() as u64)
    }

    fn combine(&self, other: &Self) -> Self {
        Rows(self.0.wrapping_add(other.0))
    }
}

#[test]
fn a_summarized_table_whose_index_the_receiver_added_lands_and_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ferro");
    let a: Db<Map<u64, Table<Tagged>, Rows>> = fast().open(&path).unwrap();
    a.update(|root| {
        for key in 0..20u64 {
            let mut table: Table<Tagged> = Table::new();
            for id in 0..3u32 {
                table.insert(Tagged {
                    id,
                    tag: id + key as u32,
                });
            }
            root.insert(key, table);
        }
    })
    .unwrap();
    let b: Db<Map<u64, Table<indexed::Tagged>, Rows>> =
        fast().open(dir.path().join("b.ferro")).unwrap();
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    assert_eq!(
        b.receive("a", &bundle, unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    assert_eq!(b.read().summary(), Some(Rows(60)));
    assert_eq!(b.read().get(&3).unwrap().by_tag(&4).count(), 1);
    b.verify_typed().unwrap();
    drop(a);
    let reread: Db<Map<u64, Table<indexed::Tagged>, Rows>> = fast().open(&path).unwrap();
    assert_eq!(reread.read().get(&3).unwrap().by_tag(&4).count(), 1);
    reread.verify_typed().unwrap();
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Person {
    #[key]
    id: u32,
    #[index]
    email: String,
}

mod unique {
    use ferro::{Data, Record};

    #[derive(Data, Record, Clone, Debug, PartialEq)]
    pub struct Person {
        #[key]
        pub id: u32,
        #[unique]
        pub email: String,
    }
}

fn person(id: u32, email: &str) -> Person {
    Person {
        id,
        email: email.into(),
    }
}

fn keep_theirs(
    people: &mut Table<unique::Person>,
    base: &Snapshot<Table<unique::Person>>,
    theirs: &Snapshot<Table<unique::Person>>,
) -> (String, usize) {
    let applied = people.merge(base, theirs, |conflict| conflict.keep_theirs());
    ("merge".to_string(), applied)
}

#[test]
fn a_head_the_receiver_cannot_read_lands_as_the_remote_and_moves_no_branch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    let a: Db<Table<Person>> = fast().open(dir.path().join("a.ferro")).unwrap();
    let b: Db<Table<unique::Person>> = fast().open(&path).unwrap();
    a.update(|people| {
        people.insert(person(1, "x"));
        people.insert(person(2, "x"));
    })
    .unwrap();
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    let refused = b.receive("a", &bundle, unmerged);
    assert!(matches!(refused, Err(Error::Decode(_))), "{refused:?}");
    assert_eq!(b.version(), 0);
    assert_eq!(b.branches(), ["a", "main"]);
    drop(b);
    let b: Db<Table<unique::Person>> = fast().open(&path).unwrap();
    assert_eq!(b.version(), 0);
    assert_eq!(b.read().len(), 0);
    b.update(|people| {
        people.insert(unique::Person {
            id: 3,
            email: "z".into(),
        });
    })
    .unwrap();
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    let refused = b.receive("a", &bundle, keep_theirs);
    assert!(matches!(refused, Err(Error::Decode(_))), "{refused:?}");
    assert_eq!(b.version(), 1);
    drop(b);
    let b: Db<Table<unique::Person>> = fast().open(&path).unwrap();
    let keys: Vec<u32> = b.read().keys().collect();
    assert_eq!(keys, [3]);
    a.update(|people| {
        people.remove(&2);
    })
    .unwrap();
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    let pulled = b.receive("a", &bundle, keep_theirs).unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    let keys: Vec<u32> = b.read().keys().collect();
    assert_eq!(keys, [1, 3]);
    assert_eq!(b.read().key_by_email("x"), Some(1));
    b.verify_typed().unwrap();
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct Scan(u64);

impl Summary<u64, String> for Scan {
    fn of(key: &u64, value: &String) -> Self {
        let xs = value.bytes().filter(|byte| *byte == b'x').count() as u64;
        Scan(key.wrapping_mul(3).wrapping_add(xs))
    }

    fn combine(&self, other: &Self) -> Self {
        Scan(self.0.wrapping_add(other.0))
    }
}

fn one_value_under(keys: &[u64], spill: &Object, size: usize) -> Received<Map<u64, String, Scan>> {
    let sums: Vec<u64> = keys
        .iter()
        .map(|key| key.wrapping_mul(3).wrapping_add(size as u64))
        .collect();
    let entries: Vec<(u64, Slot<'_>)> = keys
        .iter()
        .zip(&sums)
        .map(|(key, sum)| (*key, Slot::Spill(spill, *sum)))
        .collect();
    let total = sums
        .iter()
        .fold(0u64, |total, sum| total.wrapping_add(*sum));
    let leaf = summed_node(&entries, &[]);
    let root = summed_root(keys.len() as u64, total, &leaf);
    receive_tree(&[spill, &leaf, &root])
}

#[test]
fn one_spilled_value_under_many_keys_lands_within_a_budget() {
    let size = 1 << 20;
    let long = "x".repeat(size);
    let spill = object_of(&long);
    let keys = level_zero_keys(4_000);
    let started = Instant::now();
    let (b, result, _dir) = one_value_under(&keys[..200], &spill, size);
    assert_eq!(result.unwrap().outcome, Outcome::FastForward);
    assert_eq!(*b.read().get(&keys[199]).unwrap(), long);
    b.verify_typed().unwrap();
    let (b, result, _dir) = one_value_under(&keys, &spill, size);
    match result {
        Err(Error::Bundle(message)) => assert!(message.contains("too many keys"), "{message}"),
        other => panic!("recomputed one value's summary under 4000 keys: {other:?}"),
    }
    assert_eq!(b.version(), 0);
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );
}

thread_local! {
    static SUMMED: std::cell::RefCell<BTreeMap<(u64, usize), usize>> =
        const { std::cell::RefCell::new(BTreeMap::new()) };
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct Noted(u64);

impl Summary<u64, String> for Noted {
    fn of(key: &u64, value: &String) -> Self {
        SUMMED.with(|summed| *summed.borrow_mut().entry((*key, value.len())).or_insert(0) += 1);
        Noted(key.wrapping_add(value.len() as u64))
    }

    fn combine(&self, other: &Self) -> Self {
        Noted(self.0.wrapping_add(other.0))
    }
}

#[test]
fn a_spilled_value_reused_under_a_key_is_summarized_once_for_it() {
    let dir = tempfile::tempdir().unwrap();
    type Noting = Map<u64, String, Noted>;
    type Five = (Noting, Noting, Noting, Noting, Noting);
    let a: Db<Five> = fast().open(dir.path().join("a.ferro")).unwrap();
    let shared = "s".repeat(700);
    let keys = level_zero_keys(6);
    a.update(|root| {
        root.4.insert(keys[0], shared.clone());
        for (at, map) in [&mut root.0, &mut root.1, &mut root.2, &mut root.3]
            .into_iter()
            .enumerate()
        {
            map.insert(keys[1], shared.clone());
            map.insert(keys[2 + at], "o".repeat(600 + at));
        }
    })
    .unwrap();
    let b: Db<Five> = fast().open(dir.path().join("b.ferro")).unwrap();
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    SUMMED.with(|summed| summed.borrow_mut().clear());
    assert_eq!(
        b.receive("a", &bundle, unmerged).unwrap().outcome,
        Outcome::FastForward
    );
    let summed = SUMMED.with(|summed| summed.borrow().clone());
    assert_eq!(summed.get(&(keys[0], 700)), Some(&1), "{summed:?}");
    assert_eq!(summed.get(&(keys[1], 700)), Some(&1), "{summed:?}");
    assert_eq!(summed.len(), 6, "{summed:?}");
    b.verify_typed().unwrap();
}

#[test]
fn a_bundle_dated_far_past_this_clock_is_rejected() {
    let now = now_ms();
    let day = 24 * 60 * 60 * 1000;
    for (time, accepted) in [
        (u64::MAX, false),
        (now + 2 * day, false),
        (now + 60_000, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let b: Db<u64> = fast().open(dir.path().join("b.ferro")).unwrap();
        let root = object_of(&7u64);
        let tip = forged_entry(
            &root,
            Forged {
                version: 1,
                time,
                prev: None,
                merged: None,
                graph: None,
            },
        );
        let result = b.receive("evil", &forged(&tip, 1, &[&root, &tip]), untouched);
        if accepted {
            assert_eq!(result.unwrap().outcome, Outcome::FastForward);
            assert_eq!(*b.read(), 7);
            continue;
        }
        match result {
            Err(Error::Bundle(message)) => assert!(message.contains("clock"), "{message}"),
            other => panic!("accepted a commit dated {time}: {other:?}"),
        }
        assert_eq!(b.branches(), vec!["main".to_string()]);
        b.update(|value| *value += 1).unwrap();
        assert!(b.log().get(&1).unwrap().time_ms <= now_ms());
    }
}

#[test]
fn a_bundle_whose_base_was_pruned_since_its_have_is_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let a = open(&dir, "a.ferro");
    let b: Db<Root> = fast()
        .history(Keep::Last(1))
        .open(dir.path().join("b.ferro"))
        .unwrap();
    put(&b, 1, "one");
    a.pull(&b, "b", refuse).unwrap();
    let have = b.have().unwrap();
    put(&b, 2, "two");
    put(&a, 3, "three");
    let bundle = a.bundle(&have).unwrap();
    let before = (b.head_hash(), b.size_on_disk());
    match b.receive("a", &bundle, theirs_win) {
        Err(Error::StaleHave(message)) => assert!(message.contains("does not have"), "{message}"),
        other => panic!("a bundle whose base was pruned was not retryable: {other:?}"),
    }
    assert_eq!((b.head_hash(), b.size_on_disk()), before);
    let fresh = a.bundle(&b.have().unwrap()).unwrap();
    match b.receive("a", &fresh, theirs_win) {
        Ok(received) => assert_eq!(received.outcome, Outcome::Merged),
        other => panic!("a bundle from a fresh have did not land: {other:?}"),
    }
    let landed = b.branch("a").unwrap();
    assert_eq!(landed.head_hash(), a.head_hash());
    assert_eq!(landed.log().first().unwrap().0, 1);
    assert_eq!(
        b.common_ancestor(&landed).unwrap().commit().message,
        "put 3"
    );
    let merged = [(1, "one"), (2, "two"), (3, "three")];
    let merged: BTreeMap<u64, String> = merged.map(|(k, v)| (k, v.to_string())).into();
    assert_eq!(dump(&b.read()), merged);
    b.verify_typed().unwrap();
}

type Pruning = Arc<Mutex<Option<Db<Root>>>>;

fn prunes_once(at: &'static str, pruning: &Pruning, steps: &Arc<AtomicUsize>) -> Hook {
    let (pruning, steps) = (pruning.clone(), steps.clone());
    Hook(Arc::new(move |name| {
        if name != at {
            return;
        }
        steps.fetch_add(1, Ordering::SeqCst);
        let db = pruning.lock().take();
        if let Some(db) = db {
            db.compact(Keep::Last(3)).unwrap();
        }
    }))
}

fn diverged_over_a_prunable_base(dir: &TempDir, hook: Hook) -> (Db<Root>, Db<Root>) {
    let a = open(dir, "a.ferro");
    let b: Db<Root> = fast().hook(hook).open(dir.path().join("b.ferro")).unwrap();
    for (key, value) in [(1, "one"), (2, "two"), (3, "three")] {
        put(&a, key, value);
    }
    assert_eq!(
        b.pull(&a, "a", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    b.drop_branch("a").unwrap();
    put(&b, 4, "four");
    put(&b, 5, "five");
    put(&a, 6, "six");
    (a, b)
}

fn keys(root: &Root) -> Vec<u64> {
    root.iter().map(|(key, _)| key).collect()
}

#[derive(Debug)]
enum Refused {
    Ferro(Error),
    Merge(usize),
}

impl From<Error> for Refused {
    fn from(error: Error) -> Self {
        Refused::Ferro(error)
    }
}

#[test]
fn a_try_pull_retries_a_stale_have_before_its_error_type_and_returns_its_merge_error_once() {
    let dir = tempfile::tempdir().unwrap();
    let (pruning, bundled) = (Pruning::default(), Arc::new(AtomicUsize::new(0)));
    let (a, b) = diverged_over_a_prunable_base(&dir, prunes_once("bundled", &pruning, &bundled));
    let (head, before) = (b.head_hash(), bundled.load(Ordering::SeqCst));
    *pruning.lock() = Some(b.clone());
    let mut merges = Vec::new();
    let refused = b.try_pull(&a, "a", |root, base, theirs| {
        merges.push(base.version());
        Err::<(String, ()), _>(Refused::Merge(theirs_win(root, base, theirs).1))
    });
    assert!(pruning.lock().is_none(), "the hook never pruned");
    assert!(matches!(refused, Err(Refused::Merge(1))), "{refused:?}");
    assert_eq!(merges, [3]);
    assert_eq!(bundled.load(Ordering::SeqCst) - before, 2);
    assert_eq!(b.head_hash(), head);
    assert_eq!(b.log().first().map(|(version, _)| version), Some(3));
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    let pulled = b
        .try_pull(&a, "a", |root, base, theirs| -> Result<_, Refused> {
            Ok(theirs_win(root, base, theirs))
        })
        .unwrap();
    assert_eq!(pulled.outcome, Outcome::Merged);
    assert_eq!(pulled.objects, 0);
    assert_eq!(bundled.load(Ordering::SeqCst) - before, 3);
    assert_eq!(keys(&b.read()), [1, 2, 3, 4, 5, 6]);
    b.verify_typed().unwrap();
}

#[test]
fn a_push_whose_base_goes_before_it_lands_tries_again_and_merges_once() {
    let dir = tempfile::tempdir().unwrap();
    let (pruning, received) = (Pruning::default(), Arc::new(AtomicUsize::new(0)));
    let (a, b) = diverged_over_a_prunable_base(&dir, prunes_once("received", &pruning, &received));
    let before = received.load(Ordering::SeqCst);
    *pruning.lock() = Some(b.clone());
    let mut merges = Vec::new();
    let pushed = a
        .push(&b, "a", |root, base, theirs| {
            merges.push((keys(base), keys(theirs)));
            theirs_win(root, base, theirs)
        })
        .unwrap();
    assert!(pruning.lock().is_none(), "the hook never pruned");
    assert_eq!(received.load(Ordering::SeqCst) - before, 2);
    assert_eq!(merges, [(vec![1, 2, 3], vec![1, 2, 3, 6])]);
    assert_eq!(pushed.outcome, Outcome::Merged);
    assert_eq!(pushed.value, Some(1));
    assert_eq!(pushed.version, 6);
    assert_eq!(b.log().first().map(|(version, _)| version), Some(3));
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    assert_eq!(keys(&b.read()), [1, 2, 3, 4, 5, 6]);
    b.verify_typed().unwrap();
}

#[test]
fn a_pull_whose_have_goes_stale_on_every_attempt_gives_up_after_three() {
    let dir = tempfile::tempdir().unwrap();
    let moving: Arc<Mutex<Option<[Db<Root>; 2]>>> = Arc::default();
    let attempts = Arc::new(AtomicUsize::new(0));
    let hook = {
        let (moving, attempts) = (moving.clone(), attempts.clone());
        Hook(Arc::new(move |name| {
            if name != "bundled" {
                return;
            }
            let pair = moving.lock().clone();
            let Some([a, b]) = pair else {
                return;
            };
            let at = attempts.fetch_add(1, Ordering::SeqCst) as u64;
            put(&b, 100 + at, "moved on");
            let pulled = a.pull(&b, "b", refuse).unwrap();
            assert_eq!(pulled.outcome, Outcome::FastForward);
        }))
    };
    let a = open(&dir, "a.ferro");
    let b: Db<Root> = fast()
        .history(Keep::Last(1))
        .reclaim(Reclaim { every_bytes: 0 })
        .hook(hook)
        .open(dir.path().join("b.ferro"))
        .unwrap();
    put(&b, 1, "one");
    a.pull(&b, "b", refuse).unwrap();
    *moving.lock() = Some([a.clone(), b.clone()]);
    let mut merges = 0;
    let pulled = b.try_pull(&a, "a", |root, base, theirs| -> Result<_, Refused> {
        merges += 1;
        Ok(theirs_win(root, base, theirs))
    });
    moving.lock().take();
    match pulled {
        Err(Refused::Ferro(Error::StaleHave(message))) => {
            assert!(message.contains("does not have"), "{message}");
        }
        other => panic!("a pull whose have went stale on every attempt: {other:?}"),
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(merges, 0);
    assert_eq!(b.branches(), vec!["main".to_string()]);
    assert_eq!(b.version(), 4);
    assert_eq!(a.head_hash(), b.head_hash());
    b.verify_typed().unwrap();
    let pulled = b.pull(&a, "a", refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::UpToDate);
    assert_eq!(b.branch("a").unwrap().head_hash(), b.head_hash());
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

fn pruned_events(path: &Path, keep: usize) -> Db<Events> {
    fast()
        .history(Keep::Last(keep))
        .reclaim(Reclaim { every_bytes: 0 })
        .open(path)
        .unwrap()
}

fn checked(devices: &[Db<Events>], when: &str) {
    for (device, db) in devices.iter().enumerate() {
        db.verify()
            .unwrap_or_else(|error| panic!("{when}: d{device}: {error}"));
        db.verify_typed()
            .unwrap_or_else(|error| panic!("{when}: d{device}: {error}"));
        for name in db.branches() {
            let branch = db.branch(&name).unwrap();
            let log = branch.log();
            assert_eq!(log.iter().count(), log.len(), "{when}: d{device} {name}");
        }
    }
}

fn relayed_pruned_syncs(compact: bool) {
    let dir = tempfile::tempdir().unwrap();
    let devices: Vec<Db<Events>> = (0..3)
        .map(|device| pruned_events(&dir.path().join(format!("d{device}.ferro")), 3))
        .collect();
    let mut next = 0u32;
    let mut commit = |device: usize| {
        event(&devices[device], next);
        next += 1;
    };
    let pull = |to: usize, from: usize| {
        let pulled = devices[to].pull(&devices[from], &format!("d{from}main"), add_deltas);
        let merged = match pulled {
            Ok(pulled) => pulled.outcome == Outcome::Merged,
            Err(Error::NoCommonAncestor) => false,
            Err(error) => panic!("d{to} <- d{from}: {error}"),
        };
        if compact {
            for db in &devices {
                db.compact(Keep::Last(3)).unwrap();
            }
        }
        checked(&devices, &format!("d{to} <- d{from}"));
        for db in &devices {
            assert!(
                counted(db).values().all(|count| *count == 1),
                "{:?}",
                counted(db)
            );
        }
        merged
    };
    commit(0);
    commit(1);
    pull(2, 0);
    commit(2);
    commit(2);
    pull(1, 0);
    pull(0, 2);
    pull(2, 1);
    commit(0);
    pull(0, 1);
    if pull(1, 0) {
        pull(0, 1);
    }
    pull(1, 2);
    pull(2, 0);
    pull(0, 1);
}

#[test]
fn a_landed_chain_holds_every_version_it_claims_when_devices_relay_pruned_history() {
    relayed_pruned_syncs(false);
}

#[test]
fn reclamation_keeps_a_landed_chain_that_shares_commits_with_a_shorter_one() {
    relayed_pruned_syncs(true);
}

#[test]
fn a_remote_whose_head_we_pruned_below_a_commit_it_knows_still_lands() {
    let dir = tempfile::tempdir().unwrap();
    let (a, s, y) = (
        open(&dir, "a.ferro"),
        open(&dir, "s.ferro"),
        open(&dir, "y.ferro"),
    );
    put(&y, 1, "y");
    assert_eq!(
        s.pull(&y, "y", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    put(&y, 2, "y");
    put(&y, 3, "y");
    assert_eq!(
        a.pull(&y, "y", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    put(&a, 4, "a");
    a.compact(Keep::Last(2)).unwrap();
    match s.pull(&a, "a", theirs_win) {
        Err(Error::NoCommonAncestor) => {}
        other => panic!("s merged a branch it shares nothing retained with: {other:?}"),
    }
    assert_eq!(
        s.pull(&y, "y", refuse).unwrap().outcome,
        Outcome::FastForward
    );
    a.drop_branch("y").unwrap();
    put(&a, 5, "a");
    a.compact(Keep::Last(2)).unwrap();
    assert_eq!(a.log().first().unwrap().0, 4);
    let before = (a.head_hash(), dump(&a.read()));
    for _ in 0..2 {
        match a.pull(&s, "s", theirs_win) {
            Ok(pulled) => assert_eq!(pulled.outcome, Outcome::Ahead),
            other => panic!("a pull that a fresh have cannot fix: {other:?}"),
        }
    }
    assert_eq!(a.branch("s").unwrap().head_hash(), s.head_hash());
    assert_eq!(a.branch("s").unwrap().log().first().unwrap().0, 3);
    assert_eq!((a.head_hash(), dump(&a.read())), before);
    a.verify_typed().unwrap();
}

struct Mesh {
    dir: TempDir,
    keeps: Vec<usize>,
    devices: Vec<Db<Events>>,
    model: Model,
    heads: Vec<usize>,
    tally: BTreeMap<&'static str, usize>,
}

impl Mesh {
    fn pull(&mut self, to: usize, from: usize, when: &str) -> Option<Outcome> {
        let (ours, theirs) = (self.heads[to], self.heads[from]);
        let id = self.model.0.len();
        let before = (self.devices[to].head_hash(), counted(&self.devices[to]));
        let pulled = self.devices[to].pull(
            &self.devices[from],
            &format!("d{from}"),
            |root, base, incoming| {
                add_deltas(root, base, incoming);
                (format!("n{id}"), ())
            },
        );
        let outcome = match pulled {
            Ok(pulled) => pulled.outcome,
            Err(Error::NoCommonAncestor) => {
                let after = (self.devices[to].head_hash(), counted(&self.devices[to]));
                assert_eq!(after, before, "{when}: a refused pull changed d{to}");
                *self.tally.entry("refused").or_default() += 1;
                return None;
            }
            Err(error) => panic!("{when}: d{to} <- d{from}: {error}"),
        };
        match outcome {
            Outcome::FastForward => {
                assert!(self.model.holds(theirs, ours), "{when}: fast-forward");
                self.heads[to] = theirs;
            }
            Outcome::Ahead => assert!(self.model.holds(ours, theirs), "{when}: ahead"),
            Outcome::UpToDate => {
                let same = self.model.0[ours].counts == self.model.0[theirs].counts;
                assert!(same || self.model.holds(ours, theirs), "{when}: up to date");
            }
            Outcome::Merged => {
                let mut counts = self.model.0[ours].counts.clone();
                counts.extend(self.model.0[theirs].counts.clone());
                self.heads[to] = self.model.push(&[ours, theirs], counts);
            }
        }
        let name = match outcome {
            Outcome::UpToDate => "up to date",
            Outcome::Ahead => "ahead",
            Outcome::FastForward => "fast-forward",
            Outcome::Merged => "merged",
        };
        *self.tally.entry(name).or_default() += 1;
        Some(outcome)
    }

    fn check(&self, when: &str) {
        checked(&self.devices, when);
        for (device, db) in self.devices.iter().enumerate() {
            let node = &self.model.0[self.heads[device]];
            assert_eq!(counted(db), node.counts, "{when}: d{device}");
            assert_eq!(node.counts, self.model.events(self.heads[device]), "{when}");
        }
    }
}

#[test]
fn pruned_devices_syncing_in_any_order_apply_every_change_once_or_refuse() {
    let mut tally: BTreeMap<&'static str, usize> = BTreeMap::new();
    for seed in 0..cases(60) as u64 {
        let mut rng = Rng::new(seed + 36);
        let count = 3 + rng.below(2) as usize;
        let keeps: Vec<usize> = (0..count).map(|_| 2 + rng.below(4) as usize).collect();
        let dir = tempfile::tempdir().unwrap();
        let devices = (0..count)
            .map(|device| {
                pruned_events(&dir.path().join(format!("d{device}.ferro")), keeps[device])
            })
            .collect();
        let mut model = Model(Vec::new());
        model.push(&[], BTreeMap::new());
        let mut mesh = Mesh {
            dir,
            keeps,
            devices,
            model,
            heads: vec![0; count],
            tally: BTreeMap::new(),
        };
        let mut next_event = 0u32;
        for step in 0..80 {
            let when = format!("seed {seed} step {step}");
            let to = rng.below(count as u64) as usize;
            let from = (to + 1 + rng.below(count as u64 - 1) as usize) % count;
            match rng.below(100) {
                0..35 => {
                    let id = mesh.model.0.len();
                    let event = next_event;
                    next_event += 1;
                    mesh.devices[to]
                        .commit(format!("n{id}"), |root| {
                            root.insert(event, 1);
                        })
                        .unwrap();
                    let mut counts = mesh.model.0[mesh.heads[to]].counts.clone();
                    counts.insert(event, 1);
                    mesh.heads[to] = mesh.model.push(&[mesh.heads[to]], counts);
                }
                35..45 => {
                    mesh.devices[to]
                        .compact(Keep::Last(mesh.keeps[to]))
                        .unwrap();
                }
                45..50 => {
                    let path = mesh.dir.path().join(format!("d{to}.ferro"));
                    drop(mesh.devices.remove(to));
                    mesh.devices
                        .insert(to, pruned_events(&path, mesh.keeps[to]));
                }
                50..75 => {
                    mesh.pull(to, from, &when);
                }
                _ => {
                    let synced = mesh.pull(to, from, &when).is_some()
                        && match mesh.pull(from, to, &when) {
                            Some(Outcome::Merged) => mesh.pull(to, from, &when).is_some(),
                            other => other.is_some(),
                        };
                    if synced {
                        mesh.check(&when);
                        for (into, out) in [(to, from), (from, to)] {
                            let again = mesh.pull(into, out, &when);
                            assert!(
                                again.is_some_and(|outcome| outcome != Outcome::Merged),
                                "{when}: d{into} <- d{out} right after a sync: {again:?}"
                            );
                        }
                        *mesh.tally.entry("synced").or_default() += 1;
                    }
                }
            }
            mesh.check(&when);
        }
        for (name, seen) in mesh.tally {
            *tally.entry(name).or_default() += seen;
        }
    }
    assert!(tally["merged"] > 100, "{tally:?}");
    assert!(tally["fast-forward"] > 100, "{tally:?}");
    assert!(tally["refused"] > 100, "{tally:?}");
    assert!(tally["synced"] > 100, "{tally:?}");
}

#[test]
fn a_remote_landed_again_after_a_drop_keeps_the_links_it_landed_with() {
    let dir = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicU64::new(1_000_000));
    let open = |name: &str, keep: Keep| -> Db<Events> {
        let now = now.clone();
        fast()
            .history(keep)
            .reclaim(Reclaim { every_bytes: 0 })
            .clock(Clock(Arc::new(move || now.fetch_add(1, Ordering::SeqCst))))
            .open(dir.path().join(name))
            .unwrap()
    };
    let (a, b, c) = (
        open("a.ferro", Keep::Last(2)),
        open("b.ferro", Keep::All),
        open("c.ferro", Keep::All),
    );
    let pull = |to: &Db<Events>, from: &Db<Events>, remote: &str| match to
        .pull(from, remote, add_deltas)
    {
        Ok(pulled) => Some(pulled.outcome),
        Err(Error::NoCommonAncestor) => None,
        Err(error) => panic!("{error}"),
    };
    event(&b, 1);
    assert_eq!(pull(&a, &b, "b"), Some(Outcome::FastForward));
    event(&a, 2);
    event(&a, 3);
    event(&c, 4);
    assert_eq!(pull(&c, &b, "b"), Some(Outcome::Merged));
    assert_eq!(pull(&b, &c, "c"), Some(Outcome::Merged));
    assert_eq!(pull(&a, &b, "b"), Some(Outcome::Merged));
    event(&a, 5);
    assert_eq!(pull(&c, &a, "a"), None);
    c.drop_branch("a").unwrap();
    assert_eq!(pull(&a, &c, "c"), Some(Outcome::Ahead));
    assert_eq!(pull(&c, &a, "a"), None);
    c.verify_typed().unwrap();
    let landed = c.branch("a").unwrap().head_hash();
    drop(c);
    let c = open("c.ferro", Keep::All);
    c.verify_typed().unwrap();
    assert_eq!(c.branch("a").unwrap().head_hash(), landed);
}

#[test]
fn a_reader_verifies_a_remote_landed_again_after_a_drop() {
    let dir = tempfile::tempdir().unwrap();
    let open = |name: &str, keep: Keep| -> Db<Events> {
        fast()
            .history(keep)
            .reclaim(Reclaim { every_bytes: 0 })
            .open(dir.path().join(name))
            .unwrap()
    };
    let (a, b) = (open("a.ferro", Keep::All), open("b.ferro", Keep::Last(2)));
    let reader: Db<Events> = Options::new()
        .read_only(true)
        .open(dir.path().join("a.ferro"))
        .unwrap();
    event(&b, 0);
    assert_eq!(
        a.pull(&b, "b", add_deltas).unwrap().outcome,
        Outcome::FastForward
    );
    for next in 1..4 {
        event(&b, next);
    }
    assert!(matches!(
        a.pull(&b, "b", add_deltas),
        Err(Error::NoCommonAncestor)
    ));
    a.compact(Keep::All).unwrap();
    reader.refresh().unwrap();
    reader.verify().unwrap();
    a.drop_branch("b").unwrap();
    assert!(matches!(
        a.pull(&b, "b", add_deltas),
        Err(Error::NoCommonAncestor)
    ));
    a.verify().unwrap();
    reader.refresh().unwrap();
    reader.verify().unwrap();
    reader.verify_typed().unwrap();
}

#[test]
fn a_writer_with_a_small_cache_verifies_a_remote_landed_again_after_a_drop() {
    for cache in (1..=64).map(|step| step * 256) {
        let dir = tempfile::tempdir().unwrap();
        let open = |name: &str, keep: Keep, cache: u64| -> Db<Events> {
            fast()
                .cache_bytes(cache)
                .history(keep)
                .reclaim(Reclaim { every_bytes: 0 })
                .open(dir.path().join(name))
                .unwrap()
        };
        let a = open("a.ferro", Keep::All, cache);
        let b = open("b.ferro", Keep::Last(2), 1 << 20);
        event(&b, 0);
        assert_eq!(
            a.pull(&b, "b", add_deltas).unwrap().outcome,
            Outcome::FastForward
        );
        for next in 1..4 {
            event(&b, next);
        }
        assert!(matches!(
            a.pull(&b, "b", add_deltas),
            Err(Error::NoCommonAncestor)
        ));
        a.compact(Keep::All).unwrap();
        a.verify().unwrap();
        a.drop_branch("b").unwrap();
        assert!(matches!(
            a.pull(&b, "b", add_deltas),
            Err(Error::NoCommonAncestor)
        ));
        a.verify()
            .unwrap_or_else(|error| panic!("cache_bytes {cache}: {error}"));
        a.verify_typed()
            .unwrap_or_else(|error| panic!("cache_bytes {cache}: {error}"));
    }
}

#[test]
fn a_remote_landed_again_during_every_stage_of_a_cycle_verifies_and_reopens() {
    for stage in ["created", "copied", "synced"] {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000_000));
        let slot: Arc<Mutex<Option<[Db<Events>; 2]>>> = Arc::new(Mutex::new(None));
        let open = |name: &str, keep: Keep, hooked: bool| -> Db<Events> {
            let now = now.clone();
            let mut options = fast()
                .history(keep)
                .reclaim(Reclaim { every_bytes: 0 })
                .clock(Clock(Arc::new(move || now.fetch_add(1, Ordering::SeqCst))));
            if hooked {
                let slot = slot.clone();
                options = options.hook(Hook(Arc::new(move |name: &'static str| {
                    if name != stage {
                        return;
                    }
                    let Some([a, c]) = slot.lock().take() else {
                        return;
                    };
                    c.drop_branch("a").unwrap();
                    assert_eq!(
                        a.pull(&c, "c", add_deltas)
                            .map(|pulled| pulled.outcome)
                            .ok(),
                        Some(Outcome::Ahead)
                    );
                    assert!(matches!(
                        c.pull(&a, "a", add_deltas),
                        Err(Error::NoCommonAncestor)
                    ));
                })));
            }
            options.open(dir.path().join(name)).unwrap()
        };
        let (a, b, c) = (
            open("a.ferro", Keep::Last(2), false),
            open("b.ferro", Keep::All, false),
            open("c.ferro", Keep::All, true),
        );
        let pull = |to: &Db<Events>, from: &Db<Events>, remote: &str| match to
            .pull(from, remote, add_deltas)
        {
            Ok(pulled) => Some(pulled.outcome),
            Err(Error::NoCommonAncestor) => None,
            Err(error) => panic!("{error}"),
        };
        event(&b, 1);
        assert_eq!(pull(&a, &b, "b"), Some(Outcome::FastForward));
        event(&a, 2);
        event(&a, 3);
        event(&c, 4);
        assert_eq!(pull(&c, &b, "b"), Some(Outcome::Merged));
        assert_eq!(pull(&b, &c, "c"), Some(Outcome::Merged));
        assert_eq!(pull(&a, &b, "b"), Some(Outcome::Merged));
        event(&a, 5);
        assert_eq!(pull(&c, &a, "a"), None);
        *slot.lock() = Some([a.clone(), c.clone()]);
        c.compact(Keep::All).unwrap();
        assert!(slot.lock().is_none(), "{stage}");
        c.verify()
            .unwrap_or_else(|error| panic!("{stage}: {error}"));
        c.verify_typed()
            .unwrap_or_else(|error| panic!("{stage}: {error}"));
        let landed = c.branch("a").unwrap().head_hash();
        drop(c);
        let c = open("c.ferro", Keep::All, false);
        c.verify()
            .unwrap_or_else(|error| panic!("{stage} reopened: {error}"));
        c.verify_typed()
            .unwrap_or_else(|error| panic!("{stage} reopened: {error}"));
        assert_eq!(c.branch("a").unwrap().head_hash(), landed, "{stage}");
        let log = c.branch("a").unwrap().log();
        assert_eq!(log.iter().count(), log.len(), "{stage}");
        event(&a, 6);
        assert_eq!(pull(&c, &a, "a"), None);
        c.compact(Keep::All).unwrap();
        c.verify_typed()
            .unwrap_or_else(|error| panic!("{stage} after another cycle: {error}"));
        drop(c);
        ferro::admin::verify(dir.path().join("c.ferro")).unwrap();
    }
}

type Armed = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

fn clocked(
    dir: &TempDir,
    name: &str,
    now: &Arc<AtomicU64>,
    keep: Keep,
    hook: Option<(&str, Armed)>,
) -> Db<Events> {
    let now = now.clone();
    let mut options = fast()
        .history(keep)
        .reclaim(Reclaim { every_bytes: 0 })
        .clock(Clock(Arc::new(move || now.fetch_add(1, Ordering::SeqCst))));
    if let Some((stage, armed)) = hook {
        let stage = stage.to_string();
        options = options.hook(Hook(Arc::new(move |name: &'static str| {
            if name == stage
                && let Some(job) = armed.lock().take()
            {
                job();
            }
        })));
    }
    options.open(dir.path().join(name)).unwrap()
}

fn land_aside(db: &Db<Events>, from: &Db<Events>, remote: &str) {
    let aside = db.fork("aside").unwrap();
    let pulled = aside.pull(from, remote, add_deltas).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    drop(aside);
    db.drop_branch("aside").unwrap();
}

fn reland(db: &Db<Events>, from: &Db<Events>, remote: &str) -> Box<dyn FnOnce() + Send> {
    let (db, from, remote) = (db.clone(), from.clone(), remote.to_string());
    Box::new(move || {
        if db.branches().contains(&remote) {
            db.drop_branch(&remote).unwrap();
        }
        db.pull(&from, &remote, add_deltas).unwrap();
    })
}

fn whole(db: &Db<Events>, name: &str, versions: u64, when: &str) {
    db.verify()
        .unwrap_or_else(|error| panic!("{when}: {error}"));
    db.verify_typed()
        .unwrap_or_else(|error| panic!("{when}: {error}"));
    let log = db.branch(name).unwrap().log();
    assert_eq!(log.len() as u64, versions, "{when}");
    assert_eq!(log.iter().count(), log.len(), "{when}");
    for version in 1..=versions {
        assert!(log.get(&version).is_some(), "{when}: v{version}");
    }
}

#[test]
fn a_remote_landed_again_with_more_history_than_a_pass_copied_keeps_its_links() {
    for stage in ["created", "copied", "synced"] {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000_000));
        let armed = Armed::default();
        let a = clocked(&dir, "a.ferro", &now, Keep::All, None);
        let c = clocked(
            &dir,
            "c.ferro",
            &now,
            Keep::All,
            Some((stage, armed.clone())),
        );
        for at in 1..=7 {
            event(&a, at);
        }
        land_aside(&c, &a, "a");
        *armed.lock() = Some(reland(&c, &a, "a"));
        c.compact(Keep::Last(3)).unwrap();
        assert!(armed.lock().is_none(), "{stage}");
        whole(&c, "a", 7, stage);
        whole(&c, "main", 7, stage);
        drop(c);
        let c = clocked(&dir, "c.ferro", &now, Keep::All, None);
        whole(&c, "a", 7, &format!("{stage} reopened"));
        c.compact(Keep::All).unwrap();
        whole(&c, "a", 7, &format!("{stage} after another cycle"));
        drop(c);
        ferro::admin::verify(dir.path().join("c.ferro")).unwrap();
    }
}

#[test]
fn a_skip_link_copied_after_its_target_was_copied_again_names_the_copy_on_its_chain() {
    for stage in ["created", "copied", "synced"] {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000_000));
        let armed = Armed::default();
        let a = clocked(&dir, "a.ferro", &now, Keep::All, None);
        let b = clocked(&dir, "b.ferro", &now, Keep::All, None);
        let c = clocked(
            &dir,
            "c.ferro",
            &now,
            Keep::Last(4),
            Some((stage, armed.clone())),
        );
        for at in 1..=4 {
            event(&a, at);
        }
        assert_eq!(
            b.pull(&a, "a", add_deltas).unwrap().outcome,
            Outcome::FastForward
        );
        event(&a, 5);
        let side = c.fork("side").unwrap();
        land_aside(&c, &a, "a");
        *armed.lock() = Some(Box::new({
            let (a, b, c, side) = (a.clone(), b.clone(), c.clone(), side.clone());
            move || {
                event(&a, 6);
                c.pull(&a, "a", add_deltas).unwrap();
                let pulled = side.pull(&b, "b", add_deltas).unwrap();
                assert_eq!(pulled.outcome, Outcome::FastForward);
                event(&side, 50);
                c.drop_branch("b").unwrap();
            }
        }));
        c.compact(Keep::Last(2)).unwrap();
        assert!(armed.lock().is_none(), "{stage}");
        assert_eq!(side.log().first().map(|(version, _)| version), Some(2));
        drop(side);
        c.drop_branch("side").unwrap();
        c.verify()
            .unwrap_or_else(|error| panic!("{stage}: {error}"));
        c.verify_typed()
            .unwrap_or_else(|error| panic!("{stage}: {error}"));
        let log = c.branch("a").unwrap().log();
        assert_eq!(log.len(), 3, "{stage}");
        for version in 4..=6 {
            assert!(log.get(&version).is_some(), "{stage}: v{version}");
        }
        drop(c);
        ferro::admin::verify(dir.path().join("c.ferro")).unwrap();
    }
}

#[test]
fn a_remote_landed_again_past_a_merge_a_pass_cut_by_time_keeps_its_links() {
    for stage in ["created", "copied", "synced"] {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000_000));
        let armed = Armed::default();
        let a = clocked(&dir, "a.ferro", &now, Keep::All, None);
        let c = clocked(
            &dir,
            "c.ferro",
            &now,
            Keep::All,
            Some((stage, armed.clone())),
        );
        for at in 1..=5 {
            event(&a, at);
        }
        event(&c, 10);
        assert_eq!(
            c.pull(&a, "a", add_deltas).unwrap().outcome,
            Outcome::Merged
        );
        event(&c, 11);
        c.drop_branch("a").unwrap();
        *armed.lock() = Some(reland(&c, &a, "a"));
        c.compact(Keep::Last(2)).unwrap();
        assert!(armed.lock().is_none(), "{stage}");
        whole(&c, "a", 5, stage);
        drop(c);
        let c = clocked(&dir, "c.ferro", &now, Keep::All, None);
        whole(&c, "a", 5, &format!("{stage} reopened"));
    }
}

#[test]
fn a_remote_landed_again_through_a_merge_keeps_the_merged_history_a_pass_cut_by_time() {
    for stage in ["created", "copied", "synced"] {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000_000));
        let armed = Armed::default();
        let a = clocked(&dir, "a.ferro", &now, Keep::All, None);
        let b = clocked(&dir, "b.ferro", &now, Keep::All, None);
        let c = clocked(
            &dir,
            "c.ferro",
            &now,
            Keep::All,
            Some((stage, armed.clone())),
        );
        let d = clocked(&dir, "d.ferro", &now, Keep::All, None);
        event(&a, 1);
        assert_eq!(
            d.pull(&a, "a", add_deltas).unwrap().outcome,
            Outcome::FastForward
        );
        event(&d, 20);
        event(&a, 2);
        event(&a, 3);
        event(&b, 30);
        assert_eq!(
            b.pull(&a, "a", add_deltas).unwrap().outcome,
            Outcome::Merged
        );
        event(&b, 31);
        event(&b, 32);
        land_aside(&c, &b, "b");
        *armed.lock() = Some(reland(&c, &b, "b"));
        c.compact(Keep::Last(3)).unwrap();
        assert!(armed.lock().is_none(), "{stage}");
        whole(&c, "b", 4, stage);
        let pulled = c.pull(&d, "d", add_deltas).map(|pulled| pulled.outcome);
        assert!(matches!(pulled, Ok(Outcome::Merged)), "{stage}: {pulled:?}");
        let counts: BTreeMap<u32, i64> =
            c.read().iter().map(|(key, value)| (key, *value)).collect();
        let expected: BTreeMap<u32, i64> = [1, 2, 3, 20, 30, 31, 32].map(|key| (key, 1)).into();
        assert_eq!(counts, expected, "{stage}");
        c.verify_typed()
            .unwrap_or_else(|error| panic!("{stage} merged: {error}"));
    }
}
