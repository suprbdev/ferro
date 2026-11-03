mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use common::{Rng, cases, fast, text};
use ferro::codec::{Encoder, HashSink, Object};
use ferro::store::{Op, Trace};
use ferro::{
    Bundle, Data, Db, Error, Have, Keep, Map, Options, Outcome, Ptr, Pulled, Snapshot,
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
                Err(Error::Bundle(_)) => rejected += 1,
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
        Err(Error::Bundle(message)) => assert!(message.contains("does not have"), "{message}"),
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
        .read_only()
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
    let a: Db<Root> = fast().history(Keep::Last(3)).open(dir.path().join("a.ferro")).unwrap();
    let b = open(&dir, "b.ferro");
    for key in 0..5 {
        put(&a, key, "a");
    }
    assert_eq!(b.pull(&a, "a", refuse).unwrap().outcome, Outcome::FastForward);
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
    assert!(matches!(b.pull(&a, "a", refuse), Err(Error::NoCommonAncestor)));
    assert_eq!((b.head_hash(), dump(&b.read())), before);
    assert_eq!(b.branch("a").unwrap().head_hash(), a.head_hash());
    b.verify().unwrap();
}

#[test]
fn a_revert_routed_through_a_third_device_survives_the_first_pull() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (open(&dir, "a.ferro"), open(&dir, "b.ferro"), open(&dir, "c.ferro"));
    put(&a, 1, "1");
    b.pull(&a, "a", refuse).unwrap();
    c.pull(&a, "a", refuse).unwrap();
    put(&a, 2, "9");
    put(&b, 1, "2");
    assert_eq!(a.pull(&b, "b", theirs_win).unwrap().outcome, Outcome::Merged);
    assert_eq!(*a.read().get(&1).unwrap(), "2");
    assert_eq!(c.pull(&b, "b", refuse).unwrap().outcome, Outcome::FastForward);
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
    exact: bool,
}

struct Model(Vec<Node>);

impl Model {
    fn push(&mut self, parents: &[usize], counts: BTreeMap<u32, i64>, exact: bool) -> usize {
        let id = self.0.len();
        let mut ancestors = vec![false; id + 1];
        for parent in parents {
            for (at, held) in self.0[*parent].ancestors.iter().enumerate() {
                ancestors[at] |= *held;
            }
        }
        ancestors[id] = true;
        let generation = parents.iter().map(|parent| self.0[*parent].generation + 1).max();
        self.0.push(Node {
            counts,
            ancestors,
            generation: generation.unwrap_or(0),
            exact,
        });
        id
    }

    fn holds(&self, node: usize, ancestor: usize) -> bool {
        self.0[node].ancestors.get(ancestor).copied().unwrap_or(false)
    }

    fn best(&self, a: usize, b: usize) -> Vec<usize> {
        let common: Vec<usize> =
            (0..self.0.len()).filter(|c| self.holds(a, *c) && self.holds(b, *c)).collect();
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
    snapshot.commit().message.strip_prefix('n').map_or(0, |id| id.parse().unwrap())
}

fn counted(db: &Db<Events>) -> BTreeMap<u32, i64> {
    db.read().iter().map(|(k, v)| (k, *v)).collect()
}

#[test]
fn random_syncs_with_a_counting_merge_apply_every_change_exactly_once() {
    for seed in 0..cases(40) as u64 {
        let dir = tempfile::tempdir().unwrap();
        let devices: Vec<Db<Events>> = (0..4)
            .map(|device| fast().open(dir.path().join(format!("d{device}.ferro"))).unwrap())
            .collect();
        let mut model = Model(Vec::new());
        model.push(&[], BTreeMap::new(), true);
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
                let exact = model.0[heads[to]].exact;
                heads[to] = model.push(&[heads[to]], counts, exact);
                continue;
            }
            let from = (to + 1 + rng.below(3) as usize) % 4;
            let (ours, theirs) = (heads[to], heads[from]);
            let best = model.best(ours, theirs);
            let id = model.0.len();
            let mut seen = None;
            let pulled = devices[to]
                .pull(&devices[from], &format!("d{from}"), |root, base, incoming| {
                    seen = Some(id_in(base));
                    add_deltas(root, base, incoming);
                    (format!("n{id}"), ())
                })
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
                    let base = seen.unwrap();
                    assert!(best.contains(&base), "seed {seed}: base {base} not in {best:?}");
                    let top = best.iter().map(|node| model.0[*node].generation).max();
                    assert_eq!(Some(model.0[base].generation), top, "seed {seed}");
                    let mut counts = model.0[ours].counts.clone();
                    for (event, count) in &model.0[theirs].counts {
                        *counts.entry(*event).or_default() += count;
                    }
                    for (event, count) in &model.0[base].counts {
                        *counts.entry(*event).or_default() -= count;
                    }
                    counts.retain(|_, count| *count != 0);
                    let exact = model.0[ours].exact && model.0[theirs].exact && best.len() == 1;
                    heads[to] = model.push(&[ours, theirs], counts, exact);
                }
            }
            for (device, db) in devices.iter().enumerate() {
                let node = &model.0[heads[device]];
                assert_eq!(counted(db), node.counts, "seed {seed} device {device}");
                if node.exact {
                    assert_eq!(node.counts, model.events(heads[device]), "seed {seed}");
                }
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
    assert_eq!(b.pull(&r, "r", refuse).unwrap().outcome, Outcome::FastForward);
    put(&r, 101, "r");
    put(&r, 102, "r");
    pause();
    let a: Db<Root> = fast().history(Keep::Last(5)).open(dir.path().join("a.ferro")).unwrap();
    for key in 0..5 {
        put(&a, key, "a");
    }
    assert_eq!(b.pull(&a, "a", theirs_win).unwrap().outcome, Outcome::Merged);
    assert_eq!(a.pull(&r, "r", theirs_win).unwrap().outcome, Outcome::Merged);
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
    let a: Db<Root> = fast().history(Keep::Last(5)).open(dir.path().join("a.ferro")).unwrap();
    let b = open(&dir, "b.ferro");
    for key in 0..5 {
        put(&a, key, "a");
    }
    assert_eq!(b.pull(&a, "a", refuse).unwrap().outcome, Outcome::FastForward);
    assert_eq!(a.pull(&r, "r", theirs_win).unwrap().outcome, Outcome::Merged);
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
    assert_eq!(b.pull(&a, "a", theirs_win).unwrap().outcome, Outcome::Merged);
    assert_eq!(dump(&b.read()).len(), 12);
}

#[test]
fn a_change_that_reaches_us_through_a_third_device_is_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    let devices: Vec<Db<Events>> = ["a", "b", "c"]
        .iter()
        .map(|name| fast().open(dir.path().join(format!("{name}.ferro"))).unwrap())
        .collect();
    let (a, b, c) = (&devices[0], &devices[1], &devices[2]);
    event(a, 0);
    b.pull(a, "a", add_deltas).unwrap();
    c.pull(a, "a", add_deltas).unwrap();
    event(c, 1);
    assert_eq!(b.pull(c, "c", add_deltas).unwrap().outcome, Outcome::FastForward);
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
    for linked in forged.prev.into_iter().chain(forged.merged.map(|(ptr, _)| ptr)) {
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
        ("generation", Forged { graph: Some((4, 1, 3)), ..honest }),
        ("reach", Forged { graph: Some((0, 2, 3)), ..honest }),
        ("floor", Forged { merged: Some((link_to(&side), 2)), ..honest }),
        ("value", Forged { merged: Some((link_to(&roots[1]), 1)), ..honest }),
        ("unlinked", Forged { merged: None, ..honest }),
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
