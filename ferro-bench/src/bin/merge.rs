//! Times merge-base searches over histories of different shapes, and large three-way merges.
//!
//! In a temporary directory under the directory given as the first argument (default
//! `target/bench`) it builds `Durability::Buffered` files of `Map<u64, u64>` whose branches forked
//! far back, merged one into the other many times, criss-crossed, or saw many feature branches
//! merged after they forked, and times `common_ancestor` between two of their branches: its first
//! call and the mean of the 100 after it. For the criss-cross it also times a whole
//! `try_merge_with` whose closure refuses to commit. Then it times the `merge` of a
//! `Map<u64, u64>`, a map of struct values and a `Table` with one index, each of 1,000,000 keys
//! with 100,000 changed on one side and 110,000 on the other, 10,000 of them on both: the first
//! call and the mean of the five after it. It prints a Markdown table of each shape with those two
//! times.
//!
//! ```sh
//! make bench-merge
//! cargo run --release -p ferro-bench --bin merge -- target/bench
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferro::{Data, Db, Durability, Map, Options, Record, Snapshot, Table, Val};

type Root = Map<u64, u64>;

const ROUNDS: u32 = 100;
const LARGE_ROWS: u64 = 1_000_000;
const LARGE_ROUNDS: u32 = 5;

#[derive(Data, Clone, Debug, PartialEq)]
struct Reading {
    name: String,
    score: f64,
    visits: u64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Visit {
    #[key]
    id: u64,
    #[index]
    bucket: u64,
    name: String,
    score: f64,
}

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

fn sides<V: Data>(
    path: &Path,
    build: impl Fn() -> V,
    change: impl Fn(&mut V, u64, u64),
) -> Db<Map<u8, V>> {
    let db = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 30)
        .open::<Map<u8, V>>(path)
        .unwrap();
    db.update(|root| {
        let base = build();
        let (mut ours, mut theirs) = (base.clone(), base.clone());
        for key in (0..LARGE_ROWS).filter(|key| key % 10 == 3) {
            change(&mut ours, key, 1);
        }
        for key in (0..LARGE_ROWS).filter(|key| key % 10 == 7 || key % 100 == 3) {
            change(&mut theirs, key, 2);
        }
        root.insert(0, base);
        root.insert(1, ours);
        root.insert(2, theirs);
    })
    .unwrap();
    db
}

fn timed<V: Data>(name: &str, db: &Db<Map<u8, V>>, merge: impl Fn(&mut V, &V, &V) -> usize) {
    let read = db.read();
    let (base, ours, theirs) = (
        read.get(&0).unwrap(),
        read.get(&1).unwrap(),
        read.get(&2).unwrap(),
    );
    let mut took = Vec::new();
    let mut written = 0;
    for _ in 0..=LARGE_ROUNDS {
        let mut merged = (*ours).clone();
        let start = Instant::now();
        written = merge(&mut merged, &base, &theirs);
        took.push(start.elapsed());
        drop(merged);
    }
    let then = took[1..].iter().sum::<Duration>() / LARGE_ROUNDS;
    println!(
        "| {name}, {written} written | {} | {} |",
        micros(took[0]),
        micros(then)
    );
}

fn reading(key: u64, side: u64) -> Reading {
    Reading {
        name: format!("reading {key:>12} side {side}"),
        score: key as f64 / 7.0 + side as f64,
        visits: key * 3 + side,
    }
}

fn visit(key: u64, side: u64) -> Visit {
    Visit {
        id: key,
        bucket: (key + side) % 64,
        name: format!("visit {key:>12} side {side}"),
        score: key as f64 / 7.0 + side as f64,
    }
}

fn large(dir: &Path) {
    let numbers = sides(
        &dir.join("large-u64.ferro"),
        || {
            (0..LARGE_ROWS)
                .map(|key| (key, key))
                .collect::<Map<u64, u64>>()
        },
        |map, key, side| {
            map.insert(key, key + side);
        },
    );
    timed(
        "`Map<u64, u64>::merge` of 1,000,000 keys, 100,000 changed on each side",
        &numbers,
        |ours, base, theirs| {
            ours.merge(base, theirs, |conflict| {
                conflict.theirs.map(Val::into_owned)
            })
        },
    );
    drop(numbers);
    let readings = sides(
        &dir.join("large-struct.ferro"),
        || {
            (0..LARGE_ROWS)
                .map(|key| (key, reading(key, 0)))
                .collect::<Map<u64, Reading>>()
        },
        |map, key, side| {
            map.insert(key, reading(key, side));
        },
    );
    timed(
        "`Map<u64, Reading>::merge` of the same shape",
        &readings,
        |ours, base, theirs| {
            ours.merge(base, theirs, |conflict| {
                conflict.theirs.map(Val::into_owned)
            })
        },
    );
    drop(readings);
    let visits = sides(
        &dir.join("large-table.ferro"),
        || {
            let mut table: Table<Visit> = Table::new();
            for key in 0..LARGE_ROWS {
                table.insert(visit(key, 0));
            }
            table
        },
        |table: &mut Table<Visit>, key, side| {
            table.insert(visit(key, side));
        },
    );
    timed(
        "`Table<Visit>::merge` of the same shape, with one index",
        &visits,
        |ours, base, theirs| {
            ours.merge(base, theirs, |conflict| {
                conflict.theirs.map(Val::into_owned)
            })
        },
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
    large(dir.path());
}
