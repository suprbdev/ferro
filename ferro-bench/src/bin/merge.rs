use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferro::{Db, Durability, Map, Options, Snapshot};

type Root = Map<u64, u64>;

const ROUNDS: u32 = 100;

fn open(path: &Path) -> Db<Root> {
    Options::new()
        .durability(Durability::Buffered)
        .open::<Root>(path)
        .unwrap()
}

fn bump(db: &Db<Root>, key: u64) {
    db.update(|root| {
        let next = root.get(&key).map_or(0, |value| *value) + 1;
        root.insert(key, next);
    })
    .unwrap();
}

fn add(root: &mut Root, base: &Snapshot<Root>, theirs: &Snapshot<Root>) {
    for change in base.diff(theirs) {
        let key = *change.key();
        let before = change.before().map_or(0, |value| **value);
        let after = change.after().map_or(0, |value| **value);
        let current = root.get(&key).map_or(0, |value| *value);
        root.insert(key, current + after - before);
    }
}

fn merge(db: &Db<Root>, other: &Db<Root>) {
    db.merge_with(other, |root, base, theirs| {
        add(root, base, theirs);
        ("merge", ())
    })
    .unwrap();
}

fn micros(took: Duration) -> String {
    let us = took.as_secs_f64() * 1e6;
    if us >= 1000.0 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{us:.0} µs")
    }
}

fn time(name: &str, mut run: impl FnMut()) {
    let start = Instant::now();
    run();
    let first = start.elapsed();
    let start = Instant::now();
    for _ in 0..ROUNDS {
        run();
    }
    let then = start.elapsed() / ROUNDS;
    println!("| {name} | {} | {} |", micros(first), micros(then));
}

fn ancestor(name: &str, ours: &Db<Root>, theirs: &Db<Root>) {
    time(name, || {
        ours.common_ancestor(theirs).unwrap();
    });
}

fn refused(name: &str, ours: &Db<Root>, theirs: &Db<Root>) {
    let tip = theirs.read().commit().clone();
    time(name, || {
        let tried = ours.try_merge_with(theirs, |root, base, other| {
            add(root, base, other);
            if other.commit() == &tip {
                return Err(ferro::Error::NoCommonAncestor);
            }
            Ok(("merge", ()))
        });
        assert!(matches!(tried, Err(ferro::Error::NoCommonAncestor)));
    });
}

fn fork_far(dir: &Path) {
    let main = open(&dir.join("fork.ferro"));
    for _ in 0..10_000 {
        bump(&main, 0);
    }
    let side = main.fork("side").unwrap();
    for _ in 0..10_000 {
        bump(&main, 0);
    }
    for _ in 0..1_000 {
        bump(&side, 1);
    }
    ancestor(
        "Fork at 10,000, then 10,000 and 1,000 commits",
        &main,
        &side,
    );
}

fn took_side(dir: &Path, into_main: bool) {
    let main = open(&dir.join(format!("took-{into_main}.ferro")));
    let side = main.fork("side").unwrap();
    for _ in 0..1_000 {
        for _ in 0..10 {
            bump(&main, 0);
            bump(&side, 1);
        }
        match into_main {
            true => merge(&main, &side),
            false => merge(&side, &main),
        }
    }
    bump(&main, 0);
    bump(&side, 1);
    let name = match into_main {
        true => "`main` took a side branch 1,000 times among 21,000 commits",
        false => "A side branch took `main` 1,000 times",
    };
    ancestor(name, &main, &side);
}

fn features(dir: &Path) {
    let main = open(&dir.join("features.ferro"));
    bump(&main, 0);
    let branch = main.fork("branch").unwrap();
    bump(&branch, 1);
    for i in 0..1_000u64 {
        let feature = main.fork(&format!("f{i}")).unwrap();
        bump(&feature, 2 + i);
        merge(&main, &feature);
        main.drop_branch(&format!("f{i}")).unwrap();
    }
    ancestor(
        "1,000 features merged since a branch forked",
        &main,
        &branch,
    );
    merge(&branch, &main);
    bump(&branch, 1);
    for i in 1_000..1_200u64 {
        let feature = main.fork(&format!("f{i}")).unwrap();
        bump(&feature, 2 + i);
        merge(&main, &feature);
        main.drop_branch(&format!("f{i}")).unwrap();
    }
    ancestor(
        "The same branch after syncing, with 200 features merged since",
        &main,
        &branch,
    );
}

fn criss_cross(dir: &Path) {
    let main = open(&dir.join("cross.ferro"));
    for _ in 0..10_000 {
        bump(&main, 0);
    }
    let side = main.fork("side").unwrap();
    for _ in 0..1_000 {
        bump(&main, 0);
        bump(&side, 1);
    }
    let copy = main.fork("copy").unwrap();
    merge(&main, &side);
    merge(&side, &copy);
    main.drop_branch("copy").unwrap();
    bump(&main, 0);
    bump(&side, 1);
    ancestor("Criss-cross after 11,000 commits: the search", &main, &side);
    refused(
        "The same: the whole merge, without its commit",
        &main,
        &side,
    );
}

fn main() {
    let parent = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "target/bench".to_string()),
    );
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    println!("| Shape | First | Then |");
    println!("| --- | --- | --- |");
    fork_far(dir.path());
    took_side(dir.path(), true);
    took_side(dir.path(), false);
    features(dir.path());
    criss_cross(dir.path());
}
