use std::time::Instant;

use ferro::Db;

use crate::adj::Adj;
use crate::model::{Degrees, Graph, Post, User, UserId};
use crate::ops::Res;
use crate::synth::{Rng, Scramble, power_law_edges};

pub struct SeedOpts {
    pub users: u32,
    pub edges: u64,
    pub posts: u64,
    pub likes: u64,
    pub seed: u64,
}

pub fn seed(db: &Db<Graph>, opts: &SeedOpts, say: &mut dyn FnMut(String)) -> Res<()> {
    if db.read().next_user != 0 {
        return Err("seed needs an empty database".into());
    }
    let n = opts.users;
    let now = crate::model::now_ms();

    let t = Instant::now();
    db.commit("seed users", |g| {
        for id in 0..n {
            g.users.insert(User {
                id,
                name: format!("u{id}"),
                created_ms: now,
            });
        }
        g.next_user = n;
    })?;
    say(format!("users {} in {:.2}s", n, t.elapsed().as_secs_f64()));

    let t = Instant::now();
    let list = power_law_edges(n, opts.edges, opts.seed);
    say(format!(
        "generated {} edges in {:.2}s",
        list.len(),
        t.elapsed().as_secs_f64()
    ));
    let t = Instant::now();
    let mut degrees = vec![Degrees::default(); n as usize];
    for &(a, b) in &list {
        degrees[a as usize].following += 1;
        degrees[b as usize].followers += 1;
    }
    let out = Adj::from_sorted(list.iter().copied());
    let mut reversed: Vec<(UserId, UserId)> = list.iter().map(|&(a, b)| (b, a)).collect();
    reversed.sort_unstable();
    let inn = Adj::from_sorted(reversed.into_iter());
    let total = list.len() as u64;
    drop(list);
    db.commit("seed follows", |g| {
        g.out = out;
        g.inn = inn;
        g.edges = total;
        g.degrees = ferro::Map::from_iter(
            degrees
                .iter()
                .enumerate()
                .filter(|(_, d)| d.followers > 0 || d.following > 0)
                .map(|(i, d)| (i as UserId, *d)),
        );
    })?;
    say(format!("edges {} in {:.2}s", total, t.elapsed().as_secs_f64()));

    if opts.posts > 0 {
        let t = Instant::now();
        let mut rng = Rng::new(opts.seed ^ 0x77);
        let map = Scramble::new(n as u64, opts.seed ^ 0x99);
        let base = now.saturating_sub(opts.posts * 1000);
        for start in (0..opts.posts).step_by(250_000) {
            let end = (start + 250_000).min(opts.posts);
            db.commit("seed posts", |g| {
                for i in start..end {
                    let u = rng.unit();
                    let author = map.apply(((n as f64) * u * u) as u64 % n as u64);
                    let id = i + 1;
                    g.posts.insert(Post {
                        id,
                        author,
                        at_ms: base + id * 1000,
                        text: format!("post {id}"),
                    });
                }
                g.next_post = end;
            })?;
        }
        say(format!(
            "posts {} in {:.2}s",
            opts.posts,
            t.elapsed().as_secs_f64()
        ));
        if opts.likes > 0 {
            let t = Instant::now();
            let likes: Vec<((u64, UserId), ())> = (0..opts.likes)
                .map(|_| {
                    (
                        (1 + rng.below(opts.posts), rng.below(n as u64) as UserId),
                        (),
                    )
                })
                .collect();
            db.commit("seed likes", |g| g.likes.extend(likes))?;
            say(format!(
                "likes {} in {:.2}s",
                opts.likes,
                t.elapsed().as_secs_f64()
            ));
        }
    }
    Ok(())
}
