use std::error::Error;
use std::io::Write;
use std::time::Instant;

use ferro::{Committed, Db, Snapshot};

use crate::adj::Adj;
use crate::analytics;
use crate::cli::{Algo, ReadCmd, WriteCmd};
use crate::model::{Graph, UserId, now_ms};
use crate::ops::{self, Res};
use crate::query;

pub fn resolve(g: &Graph, text: &str) -> Res<UserId> {
    if let Ok(id) = text.parse::<UserId>() {
        if id < g.next_user {
            return Ok(id);
        }
        return Err(format!("no such user {id}").into());
    }
    match g.users.by_name(text) {
        Some(user) => Ok(user.id),
        None => Err(format!("no such user {text:?}").into()),
    }
}

fn name_of(g: &Graph, id: UserId) -> String {
    g.users
        .get(&id)
        .map(|user| user.name.clone())
        .unwrap_or_default()
}

fn commit<R>(
    db: &Db<Graph>,
    message: String,
    f: impl FnOnce(&mut Graph) -> Res<R>,
) -> Res<Committed<R>> {
    db.try_commit_with::<R, String, Box<dyn Error>>(|g| f(g).map(|value| (message, value)))
}

pub fn run_write(out: &mut dyn Write, db: &Db<Graph>, cmd: WriteCmd) -> Res<()> {
    match cmd {
        WriteCmd::User { name } => {
            let now = now_ms();
            let done = commit(db, format!("user {name}"), |g| ops::add_user(g, &name, now))?;
            writeln!(out, "user {} {} (v{})", done.value, name, done.version)?;
        }
        WriteCmd::Follow { a, b } => {
            let (x, y) = pair(db, &a, &b)?;
            let done = commit(db, format!("follow {x} {y}"), |g| ops::follow(g, x, y))?;
            if done.value {
                writeln!(out, "followed {x} -> {y} (v{})", done.version)?;
            } else {
                writeln!(out, "already following {x} -> {y}")?;
            }
        }
        WriteCmd::Unfollow { a, b } => {
            let (x, y) = pair(db, &a, &b)?;
            let done = commit(db, format!("unfollow {x} {y}"), |g| ops::unfollow(g, x, y))?;
            if done.value {
                writeln!(out, "unfollowed {x} -> {y} (v{})", done.version)?;
            } else {
                writeln!(out, "was not following {x} -> {y}")?;
            }
        }
        WriteCmd::Block { a, b } => {
            let (x, y) = pair(db, &a, &b)?;
            let done = commit(db, format!("block {x} {y}"), |g| ops::block(g, x, y))?;
            if done.value {
                writeln!(out, "blocked {x} -> {y} (v{})", done.version)?;
            } else {
                writeln!(out, "already blocked {x} -> {y}")?;
            }
        }
        WriteCmd::Post {
            author,
            text,
            fanout,
        } => {
            let snap = db.read();
            let who = resolve(&snap, &author)?;
            drop(snap);
            let now = now_ms();
            let done = commit(db, format!("post by {who}"), |g| {
                ops::post(g, who, &text, now, fanout)
            })?;
            writeln!(out, "post {} by {} (v{})", done.value, who, done.version)?;
        }
        WriteCmd::Like { user, post } => {
            let snap = db.read();
            let who = resolve(&snap, &user)?;
            drop(snap);
            let done = commit(db, format!("like {who} {post}"), |g| {
                ops::like(g, who, post)
            })?;
            if done.value {
                writeln!(out, "liked post {post} by {who} (v{})", done.version)?;
            } else {
                writeln!(out, "already liked post {post}")?;
            }
        }
    }
    Ok(())
}

fn pair(db: &Db<Graph>, a: &str, b: &str) -> Res<(UserId, UserId)> {
    let snap = db.read();
    Ok((resolve(&snap, a)?, resolve(&snap, b)?))
}

fn print_page(
    out: &mut dyn Write,
    g: &Graph,
    adj: &Adj,
    user: UserId,
    after: Option<u32>,
    limit: usize,
) -> Res<()> {
    let page = query::page(adj, user, after, limit);
    for id in &page.items {
        writeln!(out, "{id}\t{}", name_of(g, *id))?;
    }
    match page.next {
        Some(cursor) => writeln!(out, "next {cursor}")?,
        None => writeln!(out, "end")?,
    }
    Ok(())
}

pub fn run_read(out: &mut dyn Write, snap: &Snapshot<Graph>, cmd: ReadCmd) -> Res<()> {
    let g: &Graph = snap;
    match cmd {
        ReadCmd::Followers { user, after, limit } => {
            let u = resolve(g, &user)?;
            print_page(out, g, &g.inn, u, after, limit)?;
        }
        ReadCmd::Following { user, after, limit } => {
            let u = resolve(g, &user)?;
            print_page(out, g, &g.out, u, after, limit)?;
        }
        ReadCmd::Count { user, scan } => {
            let u = resolve(g, &user)?;
            let (followers, following) = if scan {
                (g.inn.degree(u), g.out.degree(u))
            } else {
                let d = g.degrees.get(&u).map(|d| *d).unwrap_or_default();
                (d.followers as usize, d.following as usize)
            };
            writeln!(out, "followers {followers} following {following}")?;
        }
        ReadCmd::Mutuals {
            a,
            b,
            followers,
            limit,
        } => {
            let (x, y) = (resolve(g, &a)?, resolve(g, &b)?);
            let common = query::mutuals(g, x, y, followers);
            writeln!(out, "count {}", common.len())?;
            for id in common.iter().take(limit) {
                writeln!(out, "{id}\t{}", name_of(g, *id))?;
            }
            let follows_ab = g.out.contains(x, y);
            let follows_ba = g.out.contains(y, x);
            writeln!(
                out,
                "{x} follows {y}: {follows_ab}, {y} follows {x}: {follows_ba}"
            )?;
        }
        ReadCmd::Path { a, b, max_depth } => {
            let (x, y) = (resolve(g, &a)?, resolve(g, &b)?);
            match query::path(g, x, y, max_depth) {
                Some(path) => {
                    let text: Vec<String> = path.iter().map(|id| id.to_string()).collect();
                    writeln!(out, "length {} path {}", path.len() - 1, text.join(" -> "))?;
                }
                None => writeln!(out, "no path within {max_depth} hops")?,
            }
        }
        ReadCmd::Recommend {
            user,
            limit,
            max_fanout,
        } => {
            let u = resolve(g, &user)?;
            for (id, mutual) in query::recommend(g, u, limit, max_fanout) {
                writeln!(out, "{id}\t{}\t{mutual}", name_of(g, id))?;
            }
        }
        ReadCmd::Timeline { user, limit, inbox } => {
            let u = resolve(g, &user)?;
            let posts = if inbox {
                query::inbox_timeline(g, u, limit)
            } else {
                query::timeline(g, u, limit)
            };
            for post in posts {
                writeln!(
                    out,
                    "{}\t{}\t{}\t{}",
                    post.id, post.author, post.at_ms, post.text
                )?;
            }
        }
        ReadCmd::Reach { user, hops, dir } => {
            let u = resolve(g, &user)?;
            let levels = query::reach(g, u, hops, dir);
            let total: usize = levels.iter().sum();
            let text: Vec<String> = levels.iter().map(|n| n.to_string()).collect();
            writeln!(out, "reach {total} levels {}", text.join(" "))?;
        }
        ReadCmd::Pagerank {
            iterations,
            threads,
            top,
        } => {
            let ranks = analytics::pagerank(g, threads, iterations);
            let sum: f64 = ranks.iter().sum();
            let mut order: Vec<usize> = (0..ranks.len()).collect();
            order.sort_unstable_by(|&a, &b| ranks[b].partial_cmp(&ranks[a]).unwrap());
            writeln!(out, "sum {sum:.6}")?;
            for &id in order.iter().take(top) {
                writeln!(out, "{id}\t{:.8}\t{}", ranks[id], name_of(g, id as UserId))?;
            }
        }
        ReadCmd::Communities {
            algo,
            iterations,
            threads,
            top,
        } => {
            let (labels, note) = match algo {
                Algo::Wcc => (analytics::wcc(g), String::new()),
                Algo::Lpa => {
                    let (labels, ran) = analytics::lpa(g, threads, iterations);
                    (labels, format!(" iterations {ran}"))
                }
            };
            let sizes = analytics::group_sizes(&labels);
            writeln!(out, "communities {}{note}", sizes.len())?;
            for (label, size) in sizes.iter().take(top) {
                writeln!(out, "{label}\t{size}")?;
            }
        }
        ReadCmd::Stats => stats(out, snap)?,
    }
    Ok(())
}

fn stats(out: &mut dyn Write, snap: &Snapshot<Graph>) -> Res<()> {
    let g: &Graph = snap;
    writeln!(out, "version {}", snap.version())?;
    writeln!(out, "users {}", g.next_user)?;
    writeln!(out, "follows {}", g.edges)?;
    writeln!(out, "blocks {}", g.blocks.len())?;
    writeln!(out, "posts {}", g.posts.len())?;
    writeln!(out, "likes {}", g.likes.len())?;
    writeln!(
        out,
        "adjacency blocks out {} in {}",
        g.out.block_count(),
        g.inn.block_count()
    )?;
    let mut top: Vec<(u32, UserId)> = g.degrees.iter().map(|(id, d)| (d.followers, id)).collect();
    top.sort_unstable_by(|a, b| b.cmp(a));
    for (followers, id) in top.iter().take(5) {
        writeln!(out, "top {id} followers {followers}")?;
    }
    let mut busy: Vec<(u32, UserId)> = g.degrees.iter().map(|(id, d)| (d.following, id)).collect();
    busy.sort_unstable_by(|a, b| b.cmp(a));
    for (following, id) in busy.iter().take(3) {
        writeln!(out, "busiest {id} following {following}")?;
    }
    Ok(())
}

pub fn diff(out: &mut dyn Write, db: &Db<Graph>, v1: u64, v2: u64, limit: usize) -> Res<()> {
    let old = db.at(v1)?;
    let new = db.at(v2)?;
    let (added, removed) = old.out.diff(&new.out);
    writeln!(out, "added {} removed {}", added.len(), removed.len())?;
    for (a, b) in added.iter().take(limit) {
        writeln!(out, "+ {a} -> {b}")?;
    }
    for (a, b) in removed.iter().take(limit) {
        writeln!(out, "- {a} -> {b}")?;
    }
    Ok(())
}

pub fn log(out: &mut dyn Write, db: &Db<Graph>, limit: usize) -> Res<()> {
    for commit in db.log().values().rev().take(limit) {
        writeln!(out, "v{}\t{}", commit.version, commit.message)?;
    }
    Ok(())
}

pub fn verify(out: &mut dyn Write, db: &Db<Graph>, sample: usize) -> Res<()> {
    let t = Instant::now();
    let report = db.verify()?;
    writeln!(
        out,
        "objects {} bytes {} file {}",
        report.objects, report.bytes, report.file_bytes
    )?;
    let snap = db.read();
    let g: &Graph = &snap;
    let n = g.user_count();
    let mut outdeg = vec![0u32; n];
    let mut indeg = vec![0u32; n];
    let mut out_total = 0u64;
    for (user, ids) in g.out.scan(0, n as UserId) {
        if ids.windows(2).any(|w| w[0] >= w[1]) {
            return Err(format!("out list of {user} is not strictly sorted").into());
        }
        if ids.iter().any(|&v| v as usize >= n) {
            return Err(format!("out list of {user} names an unknown user").into());
        }
        outdeg[user as usize] = ids.len() as u32;
        out_total += ids.len() as u64;
    }
    let mut in_total = 0u64;
    for (user, ids) in g.inn.scan(0, n as UserId) {
        indeg[user as usize] = ids.len() as u32;
        in_total += ids.len() as u64;
    }
    if out_total != g.edges || in_total != g.edges {
        return Err(format!(
            "edge totals disagree: out {out_total} in {in_total} recorded {}",
            g.edges
        )
        .into());
    }
    let mut counted = 0usize;
    for (user, d) in g.degrees.iter() {
        if d.following != outdeg[user as usize] || d.followers != indeg[user as usize] {
            return Err(format!("cached degrees of {user} are stale").into());
        }
        counted += 1;
    }
    let busy = (0..n).filter(|&u| outdeg[u] > 0 || indeg[u] > 0).count();
    if busy != counted {
        return Err(format!("degrees has {counted} rows but {busy} users have edges").into());
    }
    let mut rng = crate::synth::Rng::new(5);
    let mut checked = 0;
    let mut tries = 0;
    while checked < sample && out_total > 0 && tries < sample * 50 {
        tries += 1;
        let user = rng.below(n as u64) as UserId;
        let neighbours = g.out.neighbours(user);
        if neighbours.is_empty() {
            continue;
        }
        let other = neighbours[rng.below(neighbours.len() as u64) as usize];
        if !g.inn.contains(other, user) {
            return Err(format!("edge {user} -> {other} is missing from the follower side").into());
        }
        checked += 1;
    }
    for (a, b) in g.blocks.iter() {
        if g.out.contains(a, b) || g.out.contains(b, a) {
            return Err(format!("follow exists across block {a} {b}").into());
        }
        if !g.blocked_by.contains(&(b, a)) {
            return Err(format!("block {a} {b} missing from blocked_by").into());
        }
    }
    writeln!(
        out,
        "ok: {} edges, {} users with edges, {} sampled mirrors, {:.2}s",
        g.edges,
        busy,
        checked,
        t.elapsed().as_secs_f64()
    )?;
    Ok(())
}
