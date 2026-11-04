mod common;

use std::collections::BTreeMap;

use common::{Rng, cases};
use ferro::{Db, Durability, Keep, Map, Options, Reclaim};
use tempfile::TempDir;

type Root = Map<u64, u64>;

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn options() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .reclaim(Reclaim { every_bytes: 0 })
}

fn name(branch: u64) -> String {
    format!("b{branch}")
}

fn put(db: &Db<Root>, key: u64, value: u64) {
    db.update(|root| {
        root.insert(key, value);
    })
    .unwrap();
}

fn heads(db: &Db<Root>) -> BTreeMap<String, (u64, Vec<(u64, u64)>)> {
    db.branches()
        .into_iter()
        .map(|branch| {
            let handle = db.branch(&branch).unwrap();
            let snapshot = handle.read();
            let values = snapshot.iter().map(|(key, value)| (key, *value)).collect();
            (branch, (snapshot.version(), values))
        })
        .collect()
}

#[test]
fn every_set_of_heads_reopens_as_it_was_written() {
    let dir = dir();
    let path = dir.path().join("heads.ferro");
    let db: Db<Root> = options().open(&path).unwrap();
    put(&db, 0, 0);
    let mut branches: Vec<(String, Db<Root>)> = (0..5)
        .map(|b| (name(b), db.fork(&name(b)).unwrap()))
        .collect();
    let mut rng = Rng::new(7);
    let rounds = 300 * u64::from(cases(1));
    let mut extra = 0;
    for round in 1..=rounds {
        let pick = rng.below(branches.len() as u64) as usize;
        put(&branches[pick].1, rng.below(40), round);
        if rng.chance(3) {
            extra += 1;
            let forked = format!("x{extra}");
            let handle = branches[pick].1.fork(&forked).unwrap();
            branches.push((forked, handle));
        }
        if rng.chance(2) && branches.len() > 5 {
            let (doomed, _) = branches.remove(5 + rng.below(branches.len() as u64 - 5) as usize);
            db.drop_branch(&doomed).unwrap();
        }
        if round % 17 == 0 || round == rounds {
            let reader = options().read_only(true).open::<Root>(&path).unwrap();
            assert_eq!(heads(&reader), heads(&db), "round {round}");
        }
    }
    db.verify().unwrap();
    db.verify_typed().unwrap();
    let written = heads(&db);
    drop((db, branches));
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(heads(&db), written);
    db.verify().unwrap();
    db.verify_typed().unwrap();
}

#[test]
fn a_follower_refreshes_through_steps_tables_and_cycles() {
    let dir = dir();
    let path = dir.path().join("follow.ferro");
    let db: Db<Root> = options().open(&path).unwrap();
    put(&db, 0, 0);
    let branches: Vec<Db<Root>> = (0..4).map(|b| db.fork(&name(b)).unwrap()).collect();
    let follower = options().read_only(true).open::<Root>(&path).unwrap();
    let rounds = 200 * u64::from(cases(1));
    for round in 1..=rounds {
        let branch = &branches[(round % 4) as usize];
        put(branch, round % 11, round);
        match round % 50 {
            10 => {
                db.fork("passing").unwrap();
            }
            20 => db.drop_branch("passing").unwrap(),
            30 => {
                db.compact(Keep::Last(5)).unwrap();
            }
            _ => {}
        }
        if round % 3 == 0 {
            follower.refresh().unwrap();
            assert_eq!(heads(&follower), heads(&db), "round {round}");
        }
    }
    follower.refresh().unwrap();
    assert_eq!(heads(&follower), heads(&db));
    follower.verify().unwrap();
}

#[test]
fn merge_records_ride_in_the_steps_of_their_branch() {
    let dir = dir();
    let ours: Db<Root> = options().open(dir.path().join("ours.ferro")).unwrap();
    let theirs: Db<Root> = options().open(dir.path().join("theirs.ferro")).unwrap();
    put(&ours, 1, 1);
    put(&theirs, 2, 2);
    let merge = |root: &mut Root, base: &ferro::Snapshot<Root>, theirs: &ferro::Snapshot<Root>| {
        root.merge(base, theirs, |conflict| conflict.theirs.map(|value| *value));
        ("merge", ())
    };
    ours.pull(&theirs, "theirs", merge).unwrap();
    let grown: Vec<u64> = (0..20)
        .map(|round| {
            let before = ours.size_on_disk();
            put(&ours, 10 + round, round);
            ours.size_on_disk() - before
        })
        .collect();
    let plain: Db<Root> = options().open(dir.path().join("plain.ferro")).unwrap();
    put(&plain, 1, 1);
    put(&plain, 2, 2);
    let alone: Vec<u64> = (0..20)
        .map(|round| {
            let before = plain.size_on_disk();
            put(&plain, 10 + round, round);
            plain.size_on_disk() - before
        })
        .collect();
    let recorded: u64 = grown.iter().sum();
    let unrecorded: u64 = alone.iter().sum();
    assert!(
        recorded < unrecorded + 20 * 40,
        "a commit carrying merge records wrote {recorded} bytes against {unrecorded}"
    );
    put(&theirs, 3, 3);
    ours.pull(&theirs, "theirs", merge).unwrap();
    let path = dir.path().join("ours.ferro");
    drop(ours);
    let ours: Db<Root> = options().open(&path).unwrap();
    let remote = ours.branch("theirs").unwrap();
    assert_eq!(remote.version(), 2);
    assert_eq!(
        ours.common_ancestor(&remote).unwrap().commit(),
        remote.read().commit()
    );
    assert_eq!(ours.read().get(&3).map(|value| *value), Some(3));
    ours.verify().unwrap();
    ours.verify_typed().unwrap();
}

#[test]
fn verify_reads_heads_that_steps_replaced_down_to_their_own_first() {
    let dir = dir();
    let path = dir.path().join("pruned.ferro");
    let db: Db<Root> = options().history(Keep::Last(2)).open(&path).unwrap();
    let side = db.fork("side").unwrap();
    for round in 0..6 {
        put(&db, round, round);
        put(&side, round, round);
    }
    db.compact(Keep::Last(2)).unwrap();
    for round in 0..12 {
        put(&db, 100 + round, round);
        if round % 4 == 0 {
            put(&side, 100 + round, round);
        }
    }
    db.verify().unwrap();
    db.verify_typed().unwrap();
    ferro::admin::verify(&path).unwrap();
    ferro::admin::layout(&path, Keep::All).unwrap();
    ferro::admin::layout(&path, Keep::Last(1)).unwrap();
    let written = heads(&db);
    drop((db, side));
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(heads(&db), written);
    db.verify().unwrap();
}

#[test]
fn verify_holds_while_pruned_branches_fork_drop_and_cycle() {
    for seed in 0..u64::from(cases(20)) {
        let dir = dir();
        let path = dir.path().join("pruned.ferro");
        let db: Db<Root> = options().history(Keep::Last(3)).open(&path).unwrap();
        put(&db, 0, 0);
        let mut branches: Vec<(String, Db<Root>)> = vec![("main".to_string(), db.clone())];
        let mut rng = Rng::new(seed);
        let mut forks = 0;
        for round in 0..150 {
            let pick = rng.below(branches.len() as u64) as usize;
            match rng.below(100) {
                0..70 => put(&branches[pick].1, rng.below(8), round),
                70..82 => {
                    forks += 1;
                    let name = format!("f{forks}");
                    let handle = branches[pick].1.fork(&name).unwrap();
                    branches.push((name, handle));
                }
                82..90 if branches.len() > 1 => {
                    let (name, _) =
                        branches.remove(1 + rng.below(branches.len() as u64 - 1) as usize);
                    db.drop_branch(&name).unwrap();
                }
                90..94 => {
                    db.compact(Keep::Last(1 + rng.below(3) as usize)).unwrap();
                }
                _ => {}
            }
            if round % 5 == 0 {
                db.verify()
                    .unwrap_or_else(|error| panic!("seed {seed} round {round}: {error}"));
            }
        }
        db.verify_typed().unwrap();
        let written = heads(&db);
        drop((db, branches));
        let db: Db<Root> = options().open(&path).unwrap();
        assert_eq!(heads(&db), written, "seed {seed}");
        db.verify().unwrap();
    }
}

#[test]
fn an_open_that_fails_on_the_table_under_its_steps_releases_the_file() {
    let dir = dir();
    let path = dir.path().join("damaged.ferro");
    let (start, end) = {
        let db: Db<Root> = options().open(&path).unwrap();
        put(&db, 0, 0);
        let branches: Vec<Db<Root>> = (0..24).map(|b| db.fork(&name(b)).unwrap()).collect();
        let start = db.size_on_disk();
        db.fork("table").unwrap();
        db.drop_branch("table").unwrap();
        let end = db.size_on_disk();
        for branch in &branches {
            put(branch, 1, 1);
        }
        (start, end)
    };
    let clean = std::fs::read(&path).unwrap();
    let mut failed = 0;
    for at in start as usize..end as usize {
        let mut image = clean.clone();
        image[at] ^= 0x41;
        std::fs::write(&path, &image).unwrap();
        if options().open::<Root>(&path).is_err() {
            failed += 1;
            let again = options().open::<Root>(&path);
            assert!(
                !matches!(again, Err(ferro::Error::Locked)),
                "a failed open left the file locked, byte {at}"
            );
        }
    }
    assert!(failed > 0, "no damaged byte failed the open");
}

#[test]
fn many_branches_reopen_whole() {
    let dir = dir();
    let path = dir.path().join("many.ferro");
    let db: Db<Root> = options().open(&path).unwrap();
    put(&db, 0, 0);
    let branches: Vec<Db<Root>> = (0..140).map(|b| db.fork(&name(b)).unwrap()).collect();
    for round in 0..3 {
        for (index, branch) in (0..).zip(&branches) {
            put(branch, round, index);
        }
    }
    let written = heads(&db);
    assert_eq!(written.len(), 141);
    drop((db, branches));
    let db: Db<Root> = options().open(&path).unwrap();
    assert_eq!(heads(&db), written);
    db.verify().unwrap();
    db.verify_typed().unwrap();
    let info = ferro::admin::info(&path).unwrap();
    assert_eq!(info.branches.len(), 141);
}
