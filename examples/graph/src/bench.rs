use std::cmp::Reverse;
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ferro::Db;

use crate::analytics::{self, Csr};
use crate::cli::BenchCmd;
use crate::model::{Graph, UserId, now_ms};
use crate::ops::{self, Res};
use crate::query::{self, Bits, Dir};
use crate::synth::Rng;

pub fn rss_mib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| {
            rest.trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok()
        })
        .map_or(0, |kib| kib / 1024)
}

fn timed<R>(f: impl FnOnce() -> R) -> (f64, R) {
    let t = Instant::now();
    let r = f();
    (t.elapsed().as_secs_f64(), r)
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

fn csr_reach(csr: &Csr, start: UserId, hops: usize) -> (usize, usize, usize) {
    let mut seen = Bits::new(csr.users());
    seen.set(start);
    let mut frontier = vec![start];
    let (mut total, mut lookups, mut edges) = (0, 0, 0);
    for _ in 0..hops {
        let mut next = Vec::new();
        for &u in &frontier {
            lookups += 1;
            for &v in csr.out(u) {
                edges += 1;
                if seen.set(v) {
                    next.push(v);
                }
            }
        }
        total += next.len();
        frontier = next;
    }
    (total, lookups, edges)
}

fn csr_recommend(csr: &Csr, user: UserId, limit: usize, max_fanout: usize) -> Vec<(UserId, u32)> {
    let following = csr.out(user);
    let mut counts: HashMap<UserId, u32> = HashMap::new();
    for &friend in following {
        for &candidate in csr.out(friend).iter().take(max_fanout) {
            if candidate != user && following.binary_search(&candidate).is_err() {
                *counts.entry(candidate).or_default() += 1;
            }
        }
    }
    let mut ranked: Vec<(UserId, u32)> = counts.into_iter().collect();
    ranked.sort_unstable_by_key(|&(id, n)| (Reverse(n), id));
    ranked.truncate(limit);
    ranked
}

fn overhead(out: &mut dyn Write, db: &Db<Graph>, lookups: usize) -> Res<()> {
    let snap = db.read();
    let g: &Graph = &snap;
    let n = g.user_count() as u64;
    let (build, csr) = timed(|| Csr::from_graph(g));
    writeln!(
        out,
        "csr build {:.2}s ({} users, {} edges, {} MiB of Vecs)",
        build,
        csr.users(),
        csr.out_to.len(),
        (csr.out_to.len() + csr.in_to.len()) * 4 / 1048576 + (csr.out_off.len() * 16) / 1048576
    )?;
    let mut rng = Rng::new(1);
    let users: Vec<UserId> = (0..lookups).map(|_| rng.below(n) as UserId).collect();
    let edges_walked: usize = users.iter().map(|&u| csr.out(u).len()).sum();

    for pass in ["cold", "warm"] {
        let mut walker = g.out.walker();
        let (seek, s1) = timed(|| {
            let mut s = 0u64;
            for &u in &users {
                s += walker.neighbours(u, 1).first().copied().unwrap_or(0) as u64;
            }
            s
        });
        let (full, s2) = timed(|| {
            let mut s = 0u64;
            for &u in &users {
                for &v in walker.neighbours(u, usize::MAX) {
                    s += v as u64;
                }
            }
            s
        });
        let (vec_full, s3) = timed(|| {
            let mut s = 0u64;
            for &u in &users {
                for &v in csr.out(u) {
                    s += v as u64;
                }
            }
            s
        });
        std::hint::black_box((s1, s2, s3));
        writeln!(
            out,
            "{pass}: {} random users ({} edges)  ferro seek-only {:.0} ns/user  ferro full list {:.0} ns/user ({:.1} ns/edge)  Vec {:.0} ns/user  ferro/Vec {:.0}x",
            lookups,
            edges_walked,
            seek * 1e9 / lookups as f64,
            full * 1e9 / lookups as f64,
            full * 1e9 / edges_walked.max(1) as f64,
            vec_full * 1e9 / lookups as f64,
            full / vec_full.max(1e-12)
        )?;
    }

    let probes: Vec<(UserId, UserId)> = (0..lookups)
        .map(|i| {
            let u = loop {
                let u = rng.below(n) as UserId;
                if !csr.out(u).is_empty() {
                    break u;
                }
            };
            if i % 2 == 0 {
                let list = csr.out(u);
                (u, list[rng.below(list.len() as u64) as usize])
            } else {
                (u, rng.below(n) as UserId)
            }
        })
        .collect();
    let (ferro_has, h1) = timed(|| {
        probes
            .iter()
            .filter(|&&(a, b)| g.out.contains(a, b))
            .count()
    });
    let (ferro_has2, _) = timed(|| {
        probes
            .iter()
            .filter(|&&(a, b)| g.out.contains(a, b))
            .count()
    });
    let (vec_has, h2) = timed(|| {
        probes
            .iter()
            .filter(|&&(a, b)| csr.out(a).binary_search(&b).is_ok())
            .count()
    });
    assert_eq!(h1, h2);
    writeln!(
        out,
        "contains x{}: ferro cold {:.0} ns  warm {:.0} ns  Vec binary search {:.0} ns  ratio {:.0}x",
        lookups,
        ferro_has * 1e9 / lookups as f64,
        ferro_has2 * 1e9 / lookups as f64,
        vec_has * 1e9 / lookups as f64,
        ferro_has2 / vec_has.max(1e-12)
    )?;

    let (scan_t, scan_n) = timed(|| {
        let mut total = 0;
        g.out
            .for_each_user(0, n as UserId, |_, ids| total += ids.len());
        total
    });
    let (walk_t, walked) = timed(|| {
        let mut total = 0;
        g.out.degrees(0, n as UserId, |_, count| total += count);
        total
    });
    assert_eq!(walked, scan_n);
    let (vec_scan, vs) = timed(|| {
        std::hint::black_box(&csr.out_to)
            .iter()
            .map(|&v| v as u64)
            .sum::<u64>()
    });
    std::hint::black_box(vs);
    writeln!(
        out,
        "out-edge walk without varint decode: ferro {:.3}s ({:.1} ns/edge, {} blocks)",
        walk_t,
        walk_t * 1e9 / scan_n as f64,
        g.out.block_count()
    )?;
    writeln!(
        out,
        "full out-edge scan: ferro {:.3}s ({:.1} ns/edge)  Vec {:.3}s ({:.2} ns/edge)  ratio {:.0}x",
        scan_t,
        scan_t * 1e9 / scan_n as f64,
        vec_scan,
        vec_scan * 1e9 / csr.out_to.len() as f64,
        scan_t / vec_scan.max(1e-12)
    )?;

    let starts: Vec<UserId> = (0..20)
        .map(|_| {
            loop {
                let u = rng.below(n) as UserId;
                if csr.out(u).len() >= 5 {
                    break u;
                }
            }
        })
        .collect();
    for hops in [2usize, 3] {
        let (t_vec, stats) = timed(|| {
            let mut total = (0, 0, 0);
            for &u in &starts {
                let r = csr_reach(&csr, u, hops);
                total = (total.0 + r.0, total.1 + r.1, total.2 + r.2);
            }
            total
        });
        let (t_cold, reached) = timed(|| {
            starts
                .iter()
                .map(|&u| query::reach(g, u, hops, Dir::Out).iter().sum::<usize>())
                .sum::<usize>()
        });
        let (t_warm, _) = timed(|| {
            starts
                .iter()
                .map(|&u| query::reach(g, u, hops, Dir::Out).iter().sum::<usize>())
                .sum::<usize>()
        });
        assert_eq!(reached, stats.0);
        let seek_only = {
            let mut frontier_all = Vec::new();
            for &u in &starts {
                let mut frontier = vec![u];
                let mut seen = Bits::new(csr.users());
                seen.set(u);
                for _ in 0..hops {
                    let mut next = Vec::new();
                    for &x in &frontier {
                        frontier_all.push(x);
                        for &v in csr.out(x) {
                            if seen.set(v) {
                                next.push(v);
                            }
                        }
                    }
                    frontier = next;
                }
            }
            let mut walker = g.out.walker();
            let (t, _) = timed(|| {
                let mut s = 0u64;
                for &x in &frontier_all {
                    s += walker.neighbours(x, 1).first().copied().unwrap_or(0) as u64;
                }
                s
            });
            t
        };
        writeln!(
            out,
            "reach hops {hops} x{}: reached {} lookups {} edges {}  Vec {:.1} ms  ferro cold {:.1} ms  warm {:.1} ms ({:.0}x Vec)  seek-only share of warm {:.0}%",
            starts.len(),
            stats.0,
            stats.1,
            stats.2,
            t_vec * 1e3,
            t_cold * 1e3,
            t_warm * 1e3,
            t_warm / t_vec.max(1e-12),
            seek_only / t_warm * 100.0
        )?;
    }
    let (t_vec, picks) = timed(|| {
        starts
            .iter()
            .map(|&u| csr_recommend(&csr, u, 10, 10_000).len())
            .sum::<usize>()
    });
    let (t_cold, cold_picks) = timed(|| {
        starts
            .iter()
            .map(|&u| query::recommend(g, u, 10, 10_000).len())
            .sum::<usize>()
    });
    let (t_warm, _) = timed(|| {
        starts
            .iter()
            .map(|&u| query::recommend(g, u, 10, 10_000).len())
            .sum::<usize>()
    });
    assert_eq!(picks, cold_picks);
    writeln!(
        out,
        "recommend x{}: Vec {:.1} ms  ferro cold {:.1} ms  warm {:.1} ms ({:.1}x Vec)",
        starts.len(),
        t_vec * 1e3,
        t_cold * 1e3,
        t_warm * 1e3,
        t_warm / t_vec.max(1e-12)
    )?;
    writeln!(out, "peak RSS {} MiB", rss_mib())?;
    Ok(())
}

fn pagerank_bench(
    out: &mut dyn Write,
    db: &Db<Graph>,
    iterations: usize,
    threads: usize,
) -> Res<()> {
    let snap = db.read();
    let g: &Graph = &snap;
    let (build, csr) = timed(|| Csr::from_graph(g));
    writeln!(out, "csr build {build:.2}s")?;
    for t in [1, threads] {
        let (ferro, r1) = timed(|| analytics::pagerank(g, t, iterations));
        let (vec, r2) = timed(|| csr.pagerank(t, iterations));
        let diff = r1
            .iter()
            .zip(&r2)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        writeln!(
            out,
            "pagerank {iterations} iterations, {t} threads: ferro {ferro:.2}s ({:.2}s/iter)  Vec {vec:.2}s ({:.3}s/iter)  ratio {:.1}x  max diff {diff:.2e}",
            ferro / iterations as f64,
            vec / iterations as f64,
            ferro / vec
        )?;
    }
    writeln!(out, "peak RSS {} MiB", rss_mib())?;
    Ok(())
}

fn writers_bench(
    out: &mut dyn Write,
    db: &Db<Graph>,
    writers: usize,
    iterations: usize,
    seconds: usize,
) -> Res<()> {
    let n = db.read().user_count() as u64;
    let run_writers = |stop: &AtomicBool, count: &AtomicU64, phase: u64| {
        thread::scope(|scope| {
            for w in 0..writers {
                scope.spawn(move || {
                    let mut rng = Rng::new(1000 + w as u64 + phase * 100);
                    while !stop.load(Ordering::Relaxed) {
                        let a = rng.below(n) as UserId;
                        let b = rng.below(n) as UserId;
                        if a == b {
                            continue;
                        }
                        let done =
                            db.try_commit_with::<_, String, Box<dyn std::error::Error>>(|g| {
                                ops::follow(g, a, b).map(|v| ("follow".to_string(), v))
                            });
                        if done.is_ok_and(|d| d.changed) {
                            count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
    };

    let warm = db.read();
    analytics::pagerank(&warm, 8, 1);
    drop(warm);
    let stop = AtomicBool::new(false);
    let count = AtomicU64::new(0);
    let alone = thread::scope(|scope| {
        let t = Instant::now();
        scope.spawn(|| {
            thread::sleep(Duration::from_secs(seconds as u64));
            stop.store(true, Ordering::Relaxed);
        });
        run_writers(&stop, &count, 1);
        count.load(Ordering::Relaxed) as f64 / t.elapsed().as_secs_f64()
    });
    writeln!(out, "{writers} writers alone: {alone:.0} commits/s")?;

    let snap = db.read();
    let g: &Graph = &snap;
    let (v0, edges0, blocks0) = (snap.version(), g.edges, g.out.block_count());
    let (idle, ranks0) = timed(|| analytics::pagerank(g, 8, iterations));
    let stop = AtomicBool::new(false);
    let count = AtomicU64::new(0);
    let (busy, during, ranks1) = thread::scope(|scope| {
        let t = Instant::now();
        let handle = scope.spawn(|| {
            let (secs, ranks) = timed(|| analytics::pagerank(g, 8, iterations));
            stop.store(true, Ordering::Relaxed);
            (secs, ranks)
        });
        run_writers(&stop, &count, 2);
        let during = count.load(Ordering::Relaxed) as f64 / t.elapsed().as_secs_f64();
        let (secs, ranks) = handle.join().unwrap();
        (secs, during, ranks)
    });
    let live = db.read();
    writeln!(
        out,
        "pagerank {iterations} iterations on a held snapshot: idle {idle:.2}s, with {writers} writers running {busy:.2}s"
    )?;
    writeln!(
        out,
        "writers during pagerank: {during:.0} commits/s ({:.0}% of alone), {} commits landed",
        during / alone * 100.0,
        count.load(Ordering::Relaxed)
    )?;
    writeln!(
        out,
        "held snapshot v{} edges {} blocks {} -> after: v{} edges {} blocks {}; live graph now v{} edges {}",
        v0,
        edges0,
        blocks0,
        snap.version(),
        g.edges,
        g.out.block_count(),
        live.version(),
        live.edges
    )?;
    let identical = ranks0 == ranks1;
    writeln!(
        out,
        "pagerank results identical idle vs during writes: {identical}"
    )?;
    Ok(())
}

fn hottest(g: &Graph) -> (UserId, u32) {
    g.degrees
        .iter()
        .map(|(id, d)| (id, d.followers))
        .max_by_key(|&(_, f)| f)
        .unwrap_or((0, 0))
}

fn counts_bench(out: &mut dyn Write, db: &Db<Graph>, commits: usize) -> Res<()> {
    let snap = db.read();
    let g: &Graph = &snap;
    let (celebrity, followers) = hottest(g);
    let n = g.user_count() as u64;
    writeln!(out, "celebrity {celebrity} with {followers} followers")?;
    let mut rng = Rng::new(3);
    for (label, with_counts) in [
        ("without cached counts", false),
        ("with cached counts", true),
    ] {
        for (who, celeb) in [("celebrity followed", true), ("ordinary followed", false)] {
            let before = db.size_on_disk();
            let mut lat = Vec::new();
            for _ in 0..commits {
                let a = rng.below(n) as UserId;
                let b = if celeb {
                    celebrity
                } else {
                    rng.below(n) as UserId
                };
                if a == b {
                    continue;
                }
                let t = Instant::now();
                db.try_commit_with::<_, String, Box<dyn std::error::Error>>(|g| {
                    ops::follow_with(g, a, b, with_counts).map(|v| ("follow".to_string(), v))
                })?;
                lat.push(t.elapsed().as_secs_f64() * 1e6);
            }
            let bytes = (db.size_on_disk() - before) as f64 / lat.len() as f64;
            writeln!(
                out,
                "follow {who}, {label}: median {:.0} us, {:.0} bytes/commit",
                median(&mut lat),
                bytes
            )?;
        }
    }
    let fresh = db.read();
    let g: &Graph = &fresh;
    let (cached, _) = timed(|| {
        let mut s = 0u64;
        for _ in 0..100_000 {
            s += g.degrees.get(&celebrity).map_or(0, |d| d.followers as u64);
        }
        s
    });
    let (scanned, s) = timed(|| g.inn.degree(celebrity));
    let (scan_warm, _) = timed(|| g.inn.degree(celebrity));
    writeln!(
        out,
        "follower count of celebrity: cached {:.0} ns, scan {:.2} ms cold / {:.2} ms warm ({} counted)  ratio {:.0}x",
        cached * 1e9 / 100_000.0,
        scanned * 1e3,
        scan_warm * 1e3,
        s,
        scan_warm / (cached / 100_000.0)
    )?;
    let sample: Vec<UserId> = (0..2000).map(|_| rng.below(n) as UserId).collect();
    let (cached, _) = timed(|| {
        sample
            .iter()
            .map(|&u| g.degrees.get(&u).map_or(0, |d| d.followers as usize))
            .sum::<usize>()
    });
    let (scanned, _) = timed(|| sample.iter().map(|&u| g.inn.degree(u)).sum::<usize>());
    writeln!(
        out,
        "follower count of 2000 random users: cached {:.0} ns, scan {:.0} ns each",
        cached * 1e9 / 2000.0,
        scanned * 1e9 / 2000.0
    )?;
    Ok(())
}

fn fanout_bench(out: &mut dyn Write, db: &Db<Graph>, commits: usize) -> Res<()> {
    let snap = db.read();
    let g: &Graph = &snap;
    let mut by_followers: Vec<(u32, UserId)> =
        g.degrees.iter().map(|(id, d)| (d.followers, id)).collect();
    by_followers.sort_unstable();
    let pick = |target: u32| {
        by_followers
            .iter()
            .find(|&&(f, _)| f >= target)
            .copied()
            .unwrap_or(*by_followers.last().unwrap())
    };
    let now = now_ms();
    for target in [10u32, 1_000, 100_000, u32::MAX] {
        let (followers, author) = if target == u32::MAX {
            *by_followers.last().unwrap()
        } else {
            pick(target)
        };
        for fanout in [false, true] {
            let mut lat = Vec::new();
            let before = db.size_on_disk();
            let reps = if followers > 50_000 {
                commits.min(3)
            } else {
                commits
            };
            for _ in 0..reps {
                let t = Instant::now();
                db.try_commit_with::<_, String, Box<dyn std::error::Error>>(|g| {
                    ops::post(g, author, "bench", now, fanout).map(|v| ("post".to_string(), v))
                })?;
                lat.push(t.elapsed().as_secs_f64() * 1e3);
            }
            writeln!(
                out,
                "post by user with {followers} followers, fanout-on-write={fanout}: median {:.2} ms, {:.0} KiB/commit",
                median(&mut lat),
                (db.size_on_disk() - before) as f64 / reps as f64 / 1024.0
            )?;
        }
    }

    let snap = db.read();
    let g: &Graph = &snap;
    let mut by_following: Vec<(u32, UserId)> =
        g.degrees.iter().map(|(id, d)| (d.following, id)).collect();
    by_following.sort_unstable();
    for target in [10u32, 200, 2_000] {
        let Some(&(following, user)) = by_following.iter().find(|&&(f, _)| f >= target) else {
            continue;
        };
        let (cold, feed) = timed(|| query::timeline(g, user, 20));
        let (warm, _) = timed(|| query::timeline(g, user, 20));
        let wanted: Vec<_> = query::timeline(g, user, 200).iter().map(|p| p.id).collect();
        db.commit("materialize inbox", |g| {
            g.inbox.extend(wanted.iter().map(|&id| (user, id)));
        })?;
        let snap2 = db.read();
        let g2: &Graph = &snap2;
        let _ = query::inbox_timeline(g2, user, 20);
        let (inbox, cached) = timed(|| query::inbox_timeline(g2, user, 20));
        assert_eq!(
            feed.iter().map(|p| p.id).collect::<Vec<_>>(),
            cached.iter().map(|p| p.id).collect::<Vec<_>>()
        );
        writeln!(
            out,
            "timeline of user following {following}: compute on read cold {:.2} ms warm {:.2} ms, inbox read {:.3} ms  ({:.0}x)",
            cold * 1e3,
            warm * 1e3,
            inbox * 1e3,
            warm / inbox.max(1e-12)
        )?;
    }
    Ok(())
}

pub fn run(out: &mut dyn Write, db: &Db<Graph>, which: BenchCmd) -> Res<()> {
    match which {
        BenchCmd::Overhead { lookups } => overhead(out, db, lookups),
        BenchCmd::Pagerank {
            iterations,
            threads,
        } => pagerank_bench(out, db, iterations, threads),
        BenchCmd::Writers {
            writers,
            iterations,
            seconds,
        } => writers_bench(out, db, writers, iterations, seconds),
        BenchCmd::Counts { commits } => counts_bench(out, db, commits),
        BenchCmd::Fanout { commits } => fanout_bench(out, db, commits),
    }
}
