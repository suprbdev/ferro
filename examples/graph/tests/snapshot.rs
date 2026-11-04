use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use ferro::{Db, Durability, Options};
use graph::analytics;
use graph::model::{Graph, UserId};
use graph::ops;
use graph::seed::{SeedOpts, seed};
use graph::synth::Rng;

fn fingerprint(g: &Graph) -> (u64, u64, usize) {
    let n = g.user_count() as UserId;
    let mut sum = 0u64;
    let mut edges = 0u64;
    for (user, ids) in g.out.scan(0, n) {
        for id in ids {
            sum = sum
                .wrapping_mul(31)
                .wrapping_add(user as u64 * 1_000_003 + id as u64);
            edges += 1;
        }
    }
    (sum, edges, g.inn.block_count())
}

#[test]
fn long_analytic_run_sees_a_frozen_graph_while_writers_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Graph> = Options::new()
        .durability(Durability::Buffered)
        .open(dir.path().join("s.ferro"))
        .unwrap();
    let opts = SeedOpts {
        users: 4000,
        edges: 40_000,
        posts: 0,
        likes: 0,
        seed: 9,
    };
    seed(&db, &opts, &mut |_| {}).unwrap();

    let snapshot = db.read();
    let version = snapshot.version();
    let before = fingerprint(&snapshot);
    let ranks = analytics::pagerank(&snapshot, 4, 8);
    let stop = AtomicBool::new(false);

    let (commits, adds) = thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let mut rng = Rng::new(77);
            let mut adds = Vec::new();
            let mut commits = 0;
            while !stop.load(Ordering::Relaxed) && commits < 5000 {
                let a = rng.below(4000) as UserId;
                let b = rng.below(4000) as UserId;
                if a == b {
                    continue;
                }
                let done = db
                    .try_commit_with::<_, String, Box<dyn std::error::Error>>(|g| {
                        ops::follow(g, a, b).map(|v| ("follow".to_string(), v))
                    })
                    .unwrap();
                if done.value {
                    adds.push((a, b));
                }
                commits += 1;
            }
            (commits, adds)
        });
        let mut rounds = 0;
        while rounds < 5 || !writer.is_finished() && rounds < 40 {
            assert_eq!(analytics::pagerank(&snapshot, 4, 8), ranks);
            assert_eq!(fingerprint(&snapshot), before);
            assert_eq!(snapshot.version(), version);
            rounds += 1;
            if db.read().version() > version + 50 && rounds >= 5 {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap()
    });

    assert!(commits > 0 && !adds.is_empty());
    let live = db.read();
    assert!(live.version() > version);
    assert_eq!(live.edges, before.1 + adds.len() as u64);
    assert_eq!(fingerprint(&snapshot), before);
    assert_eq!(snapshot.edges, before.1);
    assert_ne!(analytics::pagerank(&live, 4, 8), ranks);

    let (added, removed) = snapshot.out.diff(&live.out);
    let mut expected = adds.clone();
    expected.sort_unstable();
    let mut got = added;
    got.sort_unstable();
    assert_eq!(got, expected);
    assert!(removed.is_empty());
    assert_eq!(analytics::pagerank(&snapshot, 1, 8), ranks);
}

#[test]
fn adjacency_matches_a_btreeset_under_random_edits() {
    use std::collections::BTreeSet;
    let mut adj = graph::adj::Adj::default();
    let mut model: BTreeSet<(u32, u32)> = BTreeSet::new();
    let mut rng = Rng::new(5);
    for step in 0..30_000 {
        let a = rng.below(6) as u32;
        let b = rng.below(1200) as u32;
        if rng.below(4) == 0 && step > 100 {
            assert_eq!(adj.remove(a, b), model.remove(&(a, b)));
        } else {
            assert_eq!(adj.insert(a, b), model.insert((a, b)));
        }
        if step % 5000 == 0 {
            for u in 0..6 {
                let want: Vec<u32> = model.range((u, 0)..=(u, u32::MAX)).map(|e| e.1).collect();
                assert_eq!(adj.neighbours(u), want);
                assert_eq!(adj.degree(u), want.len());
            }
        }
    }
    for u in 0..6 {
        let want: Vec<u32> = model.range((u, 0)..=(u, u32::MAX)).map(|e| e.1).collect();
        assert_eq!(adj.neighbours(u), want);
        for probe in [0u32, 17, 600, 1199] {
            assert_eq!(adj.contains(u, probe), want.contains(&probe));
            let after: Vec<u32> = adj.iter_after(u, Some(probe)).collect();
            let expect: Vec<u32> = want.iter().copied().filter(|&x| x > probe).collect();
            assert_eq!(after, expect);
        }
    }
    let scanned: Vec<(u32, u32)> = adj
        .scan(0, 6)
        .flat_map(|(u, ids)| ids.into_iter().map(move |v| (u, v)))
        .collect();
    assert_eq!(scanned, model.iter().copied().collect::<Vec<_>>());
}

#[test]
fn descending_inserts_do_not_grow_one_block_without_bound() {
    let mut adj = graph::adj::Adj::default();
    for b in (0..5000u32).rev() {
        adj.insert(1, b);
    }
    assert_eq!(adj.degree(1), 5000);
    assert!(adj.block_count() >= 5000 / 256);
}
