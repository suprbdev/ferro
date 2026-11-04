use std::error::Error;

use crate::model::{Degrees, Graph, Post, PostId, User, UserId};

pub type Res<T> = Result<T, Box<dyn Error>>;

fn check_user(g: &Graph, user: UserId) -> Res<()> {
    if user >= g.next_user {
        return Err(format!("no such user {user}").into());
    }
    Ok(())
}

pub fn add_user(g: &mut Graph, name: &str, now: u64) -> Res<UserId> {
    if name.is_empty() || name.parse::<u32>().is_ok() {
        return Err(format!("invalid user name {name:?}").into());
    }
    let id = g.next_user;
    g.users
        .try_insert(User {
            id,
            name: name.to_string(),
            created_ms: now,
        })
        .map_err(|taken| format!("name {name:?} is taken by user {}", taken.holder))?;
    g.next_user += 1;
    Ok(id)
}

pub fn follow(g: &mut Graph, a: UserId, b: UserId) -> Res<bool> {
    follow_with(g, a, b, true)
}

pub fn follow_with(g: &mut Graph, a: UserId, b: UserId, counts: bool) -> Res<bool> {
    check_user(g, a)?;
    check_user(g, b)?;
    if a == b {
        return Err("cannot follow yourself".into());
    }
    if g.is_blocked(a, b) {
        return Err(format!("{a} and {b} have a block between them").into());
    }
    if !g.out.insert(a, b) {
        return Ok(false);
    }
    g.inn.insert(b, a);
    g.edges += 1;
    if counts {
        g.degrees.get_or_insert_with(a, Degrees::default).following += 1;
        g.degrees.get_or_insert_with(b, Degrees::default).followers += 1;
    }
    Ok(true)
}

pub fn unfollow(g: &mut Graph, a: UserId, b: UserId) -> Res<bool> {
    check_user(g, a)?;
    check_user(g, b)?;
    if !g.out.remove(a, b) {
        return Ok(false);
    }
    g.inn.remove(b, a);
    g.edges -= 1;
    for (user, outgoing) in [(a, true), (b, false)] {
        let row = g.degrees.get_or_insert_with(user, Degrees::default);
        if outgoing {
            row.following -= 1;
        } else {
            row.followers -= 1;
        }
        if *row == Degrees::default() {
            g.degrees.remove(&user);
        }
    }
    Ok(true)
}

pub fn block(g: &mut Graph, a: UserId, b: UserId) -> Res<bool> {
    check_user(g, a)?;
    check_user(g, b)?;
    if a == b {
        return Err("cannot block yourself".into());
    }
    unfollow(g, a, b)?;
    unfollow(g, b, a)?;
    g.blocked_by.insert((b, a));
    Ok(g.blocks.insert((a, b)))
}

pub fn post(g: &mut Graph, author: UserId, text: &str, now: u64, fanout: bool) -> Res<PostId> {
    check_user(g, author)?;
    g.next_post += 1;
    let id = g.next_post;
    g.posts.insert(Post {
        id,
        author,
        at_ms: now,
        text: text.to_string(),
    });
    if fanout {
        let followers: Vec<UserId> = g.inn.iter(author).collect();
        g.inbox.extend(followers.into_iter().map(|f| (f, id)));
    }
    Ok(id)
}

pub fn like(g: &mut Graph, user: UserId, post: PostId) -> Res<bool> {
    check_user(g, user)?;
    if !g.posts.contains(&post) {
        return Err(format!("no such post {post}").into());
    }
    Ok(g.likes.insert((post, user)))
}
