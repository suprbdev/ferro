use std::thread;

use crate::model::{Graph, UserId};

pub const DAMPING: f64 = 0.85;

pub fn pagerank_with<S>(
    n: usize,
    threads: usize,
    iterations: usize,
    outdeg: &[u32],
    scan_in: S,
) -> Vec<f64>
where
    S: Fn(UserId, UserId, &mut dyn FnMut(UserId, &[UserId])) + Sync,
{
    if n == 0 {
        return Vec::new();
    }
    let threads = threads.max(1);
    let chunk = n.div_ceil(threads);
    let mut rank = vec![1.0 / n as f64; n];
    for _ in 0..iterations {
        let contrib: Vec<f64> = rank
            .iter()
            .zip(outdeg)
            .map(|(&r, &d)| if d > 0 { r / d as f64 } else { 0.0 })
            .collect();
        let dangling: f64 = rank
            .iter()
            .zip(outdeg)
            .filter(|(_, d)| **d == 0)
            .map(|(r, _)| r)
            .sum();
        let base = (1.0 - DAMPING) / n as f64 + DAMPING * dangling / n as f64;
        let mut next = vec![base; n];
        thread::scope(|scope| {
            for (i, slice) in next.chunks_mut(chunk).enumerate() {
                let low = i * chunk;
                let high = low + slice.len();
                let contrib = &contrib;
                let scan_in = &scan_in;
                scope.spawn(move || {
                    scan_in(low as UserId, high as UserId, &mut |v, ins| {
                        let sum: f64 = ins.iter().map(|&u| contrib[u as usize]).sum();
                        slice[v as usize - low] += DAMPING * sum;
                    });
                });
            }
        });
        rank = next;
    }
    rank
}

pub fn out_degrees(g: &Graph, threads: usize) -> Vec<u32> {
    let n = g.user_count();
    let mut deg = vec![0u32; n];
    if n == 0 {
        return deg;
    }
    let chunk = n.div_ceil(threads.max(1));
    thread::scope(|scope| {
        for (i, slice) in deg.chunks_mut(chunk).enumerate() {
            let low = i * chunk;
            let high = low + slice.len();
            scope.spawn(move || {
                for (user, ids) in g.out.scan(low as UserId, high as UserId) {
                    slice[user as usize - low] = ids.len() as u32;
                }
            });
        }
    });
    deg
}

pub fn pagerank(g: &Graph, threads: usize, iterations: usize) -> Vec<f64> {
    let deg = out_degrees(g, threads);
    pagerank_with(g.user_count(), threads, iterations, &deg, |low, high, f| {
        for (user, ids) in g.inn.scan(low, high) {
            f(user, &ids);
        }
    })
}

pub struct Csr {
    pub out_off: Vec<usize>,
    pub out_to: Vec<UserId>,
    pub in_off: Vec<usize>,
    pub in_to: Vec<UserId>,
}

impl Csr {
    pub fn from_graph(g: &Graph) -> Csr {
        let n = g.user_count();
        let build = |adj: &crate::adj::Adj| {
            let mut off = vec![0usize; n + 1];
            let mut to = Vec::new();
            let mut cursor = 0usize;
            for (user, ids) in adj.scan(0, n as UserId) {
                let user = user as usize;
                while cursor <= user {
                    off[cursor] = to.len();
                    cursor += 1;
                }
                to.extend_from_slice(&ids);
            }
            while cursor <= n {
                off[cursor] = to.len();
                cursor += 1;
            }
            (off, to)
        };
        let (out_off, out_to) = build(&g.out);
        let (in_off, in_to) = build(&g.inn);
        Csr {
            out_off,
            out_to,
            in_off,
            in_to,
        }
    }

    pub fn users(&self) -> usize {
        self.out_off.len() - 1
    }

    pub fn out(&self, u: UserId) -> &[UserId] {
        &self.out_to[self.out_off[u as usize]..self.out_off[u as usize + 1]]
    }

    pub fn inn(&self, u: UserId) -> &[UserId] {
        &self.in_to[self.in_off[u as usize]..self.in_off[u as usize + 1]]
    }

    pub fn pagerank(&self, threads: usize, iterations: usize) -> Vec<f64> {
        let n = self.users();
        let deg: Vec<u32> = (0..n).map(|u| self.out(u as UserId).len() as u32).collect();
        pagerank_with(n, threads, iterations, &deg, |low, high, f| {
            for v in low..high {
                let ins = self.inn(v);
                if !ins.is_empty() {
                    f(v, ins);
                }
            }
        })
    }
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        parent[x as usize] = parent[parent[x as usize] as usize];
        x = parent[x as usize];
    }
    x
}

pub fn wcc(g: &Graph) -> Vec<u32> {
    let n = g.user_count();
    let mut parent: Vec<u32> = (0..n as u32).collect();
    for (user, ids) in g.out.scan(0, n as UserId) {
        for other in ids {
            let (a, b) = (find(&mut parent, user), find(&mut parent, other));
            if a != b {
                parent[a.max(b) as usize] = a.min(b);
            }
        }
    }
    (0..n as u32).map(|u| find(&mut parent, u)).collect()
}

fn mode(old: &[u32], neighbours: &mut Vec<u32>, own: u32) -> u32 {
    let mut labels: Vec<u32> = neighbours.iter().map(|&v| old[v as usize]).collect();
    neighbours.clear();
    labels.sort_unstable();
    let mut best = (0usize, own);
    let mut i = 0;
    while i < labels.len() {
        let mut j = i;
        while j < labels.len() && labels[j] == labels[i] {
            j += 1;
        }
        let run = j - i;
        if run > best.0 {
            best = (run, labels[i]);
        } else if run == best.0 && labels[i] == own {
            best.1 = own;
        }
        i = j;
    }
    best.1
}

pub fn lpa(g: &Graph, threads: usize, iterations: usize) -> (Vec<u32>, usize) {
    let n = g.user_count();
    let mut labels: Vec<u32> = (0..n as u32).collect();
    if n == 0 {
        return (labels, 0);
    }
    let chunk = n.div_ceil(threads.max(1));
    let mut ran = 0;
    for _ in 0..iterations {
        let mut next = labels.clone();
        let old = &labels;
        thread::scope(|scope| {
            for (i, slice) in next.chunks_mut(chunk).enumerate() {
                let low = i * chunk;
                let high = low + slice.len();
                scope.spawn(move || {
                    let mut outs = g.out.scan(low as UserId, high as UserId).peekable();
                    let mut ins = g.inn.scan(low as UserId, high as UserId).peekable();
                    let mut gathered: Vec<u32> = Vec::new();
                    loop {
                        let user = match (outs.peek(), ins.peek()) {
                            (None, None) => break,
                            (Some(a), None) => a.0,
                            (None, Some(b)) => b.0,
                            (Some(a), Some(b)) => a.0.min(b.0),
                        };
                        if outs.peek().is_some_and(|a| a.0 == user) {
                            gathered.extend(outs.next().unwrap().1);
                        }
                        if ins.peek().is_some_and(|b| b.0 == user) {
                            gathered.extend(ins.next().unwrap().1);
                        }
                        slice[user as usize - low] =
                            mode(old, &mut gathered, old[user as usize]);
                    }
                });
            }
        });
        ran += 1;
        let changed = next != labels;
        labels = next;
        if !changed {
            break;
        }
    }
    (labels, ran)
}

pub fn group_sizes(labels: &[u32]) -> Vec<(u32, usize)> {
    let mut sizes: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for &label in labels {
        *sizes.entry(label).or_default() += 1;
    }
    let mut ranked: Vec<(u32, usize)> = sizes.into_iter().collect();
    ranked.sort_unstable_by_key(|&(label, size)| (std::cmp::Reverse(size), label));
    ranked
}
