use ferro::Val;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use clap::ValueEnum;

use crate::adj::{Adj, intersect};
use crate::model::{Graph, Post, UserId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Dir {
    Out,
    In,
    Both,
}

pub struct Page {
    pub items: Vec<UserId>,
    pub next: Option<UserId>,
}

pub fn page(adj: &Adj, user: UserId, after: Option<UserId>, limit: usize) -> Page {
    let mut items: Vec<UserId> = adj.iter_after(user, after).take(limit + 1).collect();
    let next = if items.len() > limit {
        items.truncate(limit);
        items.last().copied()
    } else {
        None
    };
    Page { items, next }
}

pub fn degree(g: &Graph, adj: &Adj, user: UserId, outgoing: bool) -> usize {
    match g.degrees.get(&user) {
        Some(d) if outgoing => d.following as usize,
        Some(d) => d.followers as usize,
        None => adj.degree(user),
    }
}

pub fn mutuals(g: &Graph, a: UserId, b: UserId, followers: bool) -> Vec<UserId> {
    let adj = if followers { &g.inn } else { &g.out };
    intersect(
        adj,
        a,
        degree(g, adj, a, !followers),
        adj,
        b,
        degree(g, adj, b, !followers),
    )
}

pub struct Bits(Vec<u64>);

impl Bits {
    pub fn new(n: usize) -> Bits {
        Bits(vec![0; n.div_ceil(64)])
    }

    pub fn set(&mut self, i: UserId) -> bool {
        let (word, bit) = (i as usize / 64, i % 64);
        let was = self.0[word] >> bit & 1 == 1;
        self.0[word] |= 1 << bit;
        !was
    }

    pub fn has(&self, i: UserId) -> bool {
        self.0[i as usize / 64] >> (i % 64) & 1 == 1
    }
}

pub fn reach(g: &Graph, start: UserId, hops: usize, dir: Dir) -> Vec<usize> {
    let mut seen = Bits::new(g.user_count());
    seen.set(start);
    let mut frontier = vec![start];
    let mut levels = Vec::new();
    let mut out = g.out.walker();
    let mut inn = g.inn.walker();
    for _ in 0..hops {
        let mut next = Vec::new();
        frontier.sort_unstable();
        for &user in &frontier {
            if matches!(dir, Dir::Out | Dir::Both) {
                next.extend(
                    out.neighbours(user, usize::MAX)
                        .iter()
                        .filter(|&&id| seen.set(id)),
                );
            }
            if matches!(dir, Dir::In | Dir::Both) {
                next.extend(
                    inn.neighbours(user, usize::MAX)
                        .iter()
                        .filter(|&&id| seen.set(id)),
                );
            }
        }
        levels.push(next.len());
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    levels
}

pub fn path(g: &Graph, a: UserId, b: UserId, max_depth: usize) -> Option<Vec<UserId>> {
    if a == b {
        return Some(vec![a]);
    }
    let mut forward: HashMap<UserId, UserId> = HashMap::new();
    let mut backward: HashMap<UserId, UserId> = HashMap::new();
    forward.insert(a, a);
    backward.insert(b, b);
    let mut front = vec![a];
    let mut back = vec![b];
    let (mut df, mut db) = (0usize, 0usize);
    while df + db < max_depth && !front.is_empty() && !back.is_empty() {
        let expand_forward = front.len() <= back.len();
        let (frontier, mine, other, adj) = if expand_forward {
            (&mut front, &mut forward, &backward, &g.out)
        } else {
            (&mut back, &mut backward, &forward, &g.inn)
        };
        let mut next = Vec::new();
        let mut meet = None;
        'layer: for &user in frontier.iter() {
            for neighbour in adj.iter(user) {
                if mine.contains_key(&neighbour) {
                    continue;
                }
                mine.insert(neighbour, user);
                if other.contains_key(&neighbour) {
                    meet = Some(neighbour);
                    break 'layer;
                }
                next.push(neighbour);
            }
        }
        if let Some(mid) = meet {
            let mut left = vec![mid];
            let mut cursor = mid;
            while forward[&cursor] != cursor {
                cursor = forward[&cursor];
                left.push(cursor);
            }
            left.reverse();
            cursor = mid;
            while backward[&cursor] != cursor {
                cursor = backward[&cursor];
                left.push(cursor);
            }
            return Some(left);
        }
        *frontier = next;
        if expand_forward {
            df += 1;
        } else {
            db += 1;
        }
    }
    None
}

pub fn recommend(g: &Graph, user: UserId, limit: usize, max_fanout: usize) -> Vec<(UserId, u32)> {
    let following = g.out.neighbours(user);
    let blocked = g.blocked_either_way(user);
    let mut counts: HashMap<UserId, u32> = HashMap::new();
    let mut walker = g.out.walker();
    for &friend in &following {
        for &candidate in walker.neighbours(friend, max_fanout) {
            if candidate == user
                || following.binary_search(&candidate).is_ok()
                || blocked.binary_search(&candidate).is_ok()
            {
                continue;
            }
            *counts.entry(candidate).or_default() += 1;
        }
    }
    let mut ranked: Vec<(UserId, u32)> = counts.into_iter().collect();
    ranked.sort_unstable_by_key(|&(id, n)| (Reverse(n), id));
    ranked.truncate(limit);
    ranked
}

pub fn timeline(g: &Graph, user: UserId, limit: usize) -> Vec<Val<Post>> {
    let mut streams = Vec::new();
    let mut heads: Vec<Option<Val<Post>>> = Vec::new();
    let mut heap = BinaryHeap::new();
    for followed in g.out.iter(user) {
        let mut stream = g.posts.by_author(&followed).rev();
        if let Some(post) = stream.next() {
            heap.push((post.id, streams.len()));
            heads.push(Some(post));
            streams.push(stream);
        }
    }
    let mut out = Vec::with_capacity(limit);
    while out.len() < limit {
        let Some((_, which)) = heap.pop() else {
            break;
        };
        out.extend(heads[which].take());
        if let Some(next) = streams[which].next() {
            heap.push((next.id, which));
            heads[which] = Some(next);
        }
    }
    out
}

pub fn inbox_timeline(g: &Graph, user: UserId, limit: usize) -> Vec<Val<Post>> {
    g.inbox
        .range((user, 0)..=(user, u64::MAX))
        .rev()
        .filter_map(|(_, id)| g.posts.get(&id))
        .take(limit)
        .collect()
}

pub fn edge_set(adj: &Adj) -> HashSet<(UserId, UserId)> {
    adj.scan(0, UserId::MAX)
        .flat_map(|(u, ids)| ids.into_iter().map(move |v| (u, v)))
        .collect()
}
