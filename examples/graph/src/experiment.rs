use std::path::Path;
use std::time::Instant;

use ferro::{Bytes, Data, Db, Durability, Map, Options, Set};

use crate::synth::{Rng, power_law_edges};

pub trait Rep: Data + Default + Clone + Send + Sync + 'static {
    const NAME: &'static str;
    fn build(&mut self, edges: &[(u32, u32)]);
    fn visit(&self, user: u32, f: &mut dyn FnMut(u32));
    fn has(&self, a: u32, b: u32) -> bool;
    fn add(&mut self, a: u32, b: u32);
    fn degree(&self, user: u32) -> usize;
}

#[derive(Data, Default, Clone)]
pub struct Nested {
    adj: Map<u32, Set<u32>>,
}

impl Rep for Nested {
    const NAME: &'static str = "nested Map<u32, Set<u32>>";

    fn build(&mut self, edges: &[(u32, u32)]) {
        let mut groups = Vec::new();
        let mut start = 0;
        while start < edges.len() {
            let user = edges[start].0;
            let mut end = start;
            while end < edges.len() && edges[end].0 == user {
                end += 1;
            }
            groups.push((user, Set::from_iter(edges[start..end].iter().map(|e| e.1))));
            start = end;
        }
        self.adj = Map::from_iter(groups);
    }

    fn visit(&self, user: u32, f: &mut dyn FnMut(u32)) {
        if let Some(set) = self.adj.get(&user) {
            for other in set.iter() {
                f(other);
            }
        }
    }

    fn has(&self, a: u32, b: u32) -> bool {
        self.adj.get(&a).is_some_and(|set| set.contains(&b))
    }

    fn add(&mut self, a: u32, b: u32) {
        self.adj.get_or_insert_with(a, Set::new).insert(b);
    }

    fn degree(&self, user: u32) -> usize {
        self.adj.get(&user).map_or(0, |set| set.len())
    }
}

#[derive(Data, Default, Clone)]
pub struct Flat {
    edges: Map<(u32, u32), ()>,
}

impl Rep for Flat {
    const NAME: &'static str = "flat Set<(u32, u32)> range scan";

    fn build(&mut self, edges: &[(u32, u32)]) {
        self.edges = Map::from_iter(edges.iter().map(|&e| (e, ())));
    }

    fn visit(&self, user: u32, f: &mut dyn FnMut(u32)) {
        for ((_, other), _) in self.edges.prefix(&user) {
            f(other);
        }
    }

    fn has(&self, a: u32, b: u32) -> bool {
        self.edges.contains_key(&(a, b))
    }

    fn add(&mut self, a: u32, b: u32) {
        self.edges.insert((a, b), ());
    }

    fn degree(&self, user: u32) -> usize {
        self.edges.prefix(&user).len()
    }
}

#[derive(Data, Default, Clone)]
pub struct Both {
    out: Map<(u32, u32), ()>,
    inn: Map<(u32, u32), ()>,
}

impl Rep for Both {
    const NAME: &'static str = "bidirectional pair of flat sets";

    fn build(&mut self, edges: &[(u32, u32)]) {
        self.out = Map::from_iter(edges.iter().map(|&e| (e, ())));
        let mut reversed: Vec<(u32, u32)> = edges.iter().map(|&(a, b)| (b, a)).collect();
        reversed.sort_unstable();
        self.inn = Map::from_iter(reversed.into_iter().map(|e| (e, ())));
    }

    fn visit(&self, user: u32, f: &mut dyn FnMut(u32)) {
        for ((_, other), _) in self.out.prefix(&user) {
            f(other);
        }
    }

    fn has(&self, a: u32, b: u32) -> bool {
        self.out.contains_key(&(a, b))
    }

    fn add(&mut self, a: u32, b: u32) {
        self.out.insert((a, b), ());
        self.inn.insert((b, a), ());
    }

    fn degree(&self, user: u32) -> usize {
        self.out.prefix(&user).len()
    }
}

const BLOCK: usize = 128;
const BLOCK_MAX: usize = 256;

fn encode_block(ids: &[u32]) -> Bytes {
    let mut out = Vec::with_capacity(ids.len() * 2);
    let mut previous = ids[0];
    for &id in &ids[1..] {
        let mut delta = id - previous;
        previous = id;
        while delta >= 0x80 {
            out.push((delta & 0x7f) as u8 | 0x80);
            delta >>= 7;
        }
        out.push(delta as u8);
    }
    Bytes::from(out)
}

fn decode_block(first: u32, bytes: &[u8]) -> Vec<u32> {
    let mut ids = vec![first];
    let mut current = first;
    let mut i = 0;
    while i < bytes.len() {
        let mut delta = 0u32;
        let mut shift = 0;
        loop {
            let b = bytes[i];
            i += 1;
            delta |= ((b & 0x7f) as u32) << shift;
            if b < 0x80 {
                break;
            }
            shift += 7;
        }
        current += delta;
        ids.push(current);
    }
    ids
}

#[derive(Data, Default, Clone)]
pub struct Bucketed {
    blocks: Map<(u32, u32), Bytes>,
}

impl Bucketed {
    fn find(&self, a: u32, b: u32) -> Option<((u32, u32), Vec<u32>)> {
        let ((user, first), bytes) = self.blocks.range((a, 0)..=(a, b)).next_back()?;
        Some(((user, first), decode_block(first, &bytes)))
    }
}

impl Rep for Bucketed {
    const NAME: &'static str = "bucketed Map<(u32, u32), Bytes>";

    fn build(&mut self, edges: &[(u32, u32)]) {
        let mut blocks = Vec::new();
        let mut start = 0;
        while start < edges.len() {
            let user = edges[start].0;
            let mut end = start;
            while end < edges.len() && edges[end].0 == user {
                end += 1;
            }
            let ids: Vec<u32> = edges[start..end].iter().map(|e| e.1).collect();
            for chunk in ids.chunks(BLOCK) {
                blocks.push(((user, chunk[0]), encode_block(chunk)));
            }
            start = end;
        }
        self.blocks = Map::from_iter(blocks);
    }

    fn visit(&self, user: u32, f: &mut dyn FnMut(u32)) {
        for ((_, first), bytes) in self.blocks.prefix(&user) {
            for id in decode_block(first, &bytes) {
                f(id);
            }
        }
    }

    fn has(&self, a: u32, b: u32) -> bool {
        self.find(a, b)
            .is_some_and(|(_, ids)| ids.binary_search(&b).is_ok())
    }

    fn add(&mut self, a: u32, b: u32) {
        match self.find(a, b) {
            None => {
                let next = self.blocks.prefix(&a).next();
                match next {
                    Some(((_, first), bytes)) => {
                        let mut ids = decode_block(first, &bytes);
                        ids.insert(0, b);
                        self.blocks.remove(&(a, first));
                        self.blocks.insert((a, b), encode_block(&ids));
                    }
                    None => {
                        self.blocks.insert((a, b), encode_block(&[b]));
                    }
                }
            }
            Some((key, mut ids)) => {
                if let Err(at) = ids.binary_search(&b) {
                    ids.insert(at, b);
                    if ids.len() > BLOCK_MAX {
                        let right = ids.split_off(ids.len() / 2);
                        self.blocks.insert((a, right[0]), encode_block(&right));
                    }
                    self.blocks.insert(key, encode_block(&ids));
                }
            }
        }
    }

    fn degree(&self, user: u32) -> usize {
        let mut count = 0;
        self.visit(user, &mut |_| count += 1);
        count
    }
}

fn rss_mib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kib: u64 = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .unwrap_or(0);
            return kib / 1024;
        }
    }
    0
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

pub fn run<R: Rep>(dir: &Path, edges: &[(u32, u32)], celebrity: u32, users: u32) -> String {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("exp-{}.ferro", R::NAME.split(' ').next().unwrap()));
    let _ = std::fs::remove_file(&path);
    let mut out = format!("== {}\n", R::NAME);
    let mut rng = Rng::new(99);

    let db: Db<R> = Db::open(&path).unwrap();
    let t = Instant::now();
    db.commit("bulk", |r| r.build(edges)).unwrap();
    let bulk = t.elapsed().as_secs_f64();
    let size = db.log_bytes();
    out += &format!(
        "bulk insert   {:.2}s  {:.2} M edges/s   file {:.1} MiB  {:.2} bytes/edge\n",
        bulk,
        edges.len() as f64 / bulk / 1e6,
        size as f64 / 1048576.0,
        size as f64 / edges.len() as f64
    );
    drop(db);

    let db: Db<R> = Db::open(&path).unwrap();
    let snap = db.read();
    let t = Instant::now();
    let mut seen = 0u64;
    snap.visit(celebrity, &mut |_| seen += 1);
    let cold = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut seen2 = 0u64;
    snap.visit(celebrity, &mut |_| seen2 += 1);
    let warm = t.elapsed().as_secs_f64();
    out += &format!(
        "celebrity scan ({} edges)  cold {:.1} ns/edge  warm {:.1} ns/edge\n",
        seen,
        cold * 1e9 / seen as f64,
        warm * 1e9 / seen2 as f64
    );
    let sample: Vec<u32> = (0..3000).map(|_| rng.below(users as u64) as u32).collect();
    let t = Instant::now();
    let mut total = 0u64;
    for &u in &sample {
        snap.visit(u, &mut |_| total += 1);
    }
    let cold = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut total2 = 0u64;
    for &u in &sample {
        snap.visit(u, &mut |_| total2 += 1);
    }
    let warm = t.elapsed().as_secs_f64();
    out += &format!(
        "3000 user scans ({} edges)  cold {:.1} ns/edge {:.1} us/user  warm {:.1} ns/edge {:.1} us/user\n",
        total,
        cold * 1e9 / total as f64,
        cold * 1e6 / sample.len() as f64,
        warm * 1e9 / total2 as f64,
        warm * 1e6 / sample.len() as f64
    );
    let t = Instant::now();
    let mut dsum = 0usize;
    for &u in &sample {
        dsum += snap.degree(u);
    }
    out += &format!(
        "degree of 3000 users  {:.2} us/user (sum {})\n",
        t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64,
        dsum
    );
    let probes: Vec<(u32, u32)> = (0..200_000)
        .map(|i| {
            if i % 2 == 0 {
                edges[rng.below(edges.len() as u64) as usize]
            } else {
                (
                    rng.below(users as u64) as u32,
                    rng.below(users as u64) as u32,
                )
            }
        })
        .collect();
    let t = Instant::now();
    let hits = probes.iter().filter(|&&(a, b)| snap.has(a, b)).count();
    out += &format!(
        "point lookup  {:.0} ns each ({} hits of {})\n",
        t.elapsed().as_secs_f64() * 1e9 / probes.len() as f64,
        hits,
        probes.len()
    );
    drop(snap);
    drop(db);

    let db: Db<R> = Options::new()
        .durability(Durability::Buffered)
        .open(&path)
        .unwrap();
    let fresh: Vec<(u32, u32)> = (0..50_000)
        .map(|_| {
            (
                rng.below(users as u64) as u32,
                rng.below(users as u64) as u32,
            )
        })
        .collect();
    let t = Instant::now();
    db.commit("incremental", |r| {
        for &(a, b) in &fresh {
            r.add(a, b);
        }
    })
    .unwrap();
    let inc = t.elapsed().as_secs_f64();
    out += &format!(
        "incremental 50k random adds in one commit  {:.2}s  {:.0} k edges/s\n",
        inc,
        50.0 / inc
    );

    let before = db.log_bytes();
    let mut next = users;
    let mut lat = Vec::new();
    for _ in 0..300 {
        next += 1;
        let t = Instant::now();
        db.commit("celeb", |r| r.add(celebrity, next)).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let bytes = (db.log_bytes() - before) as f64 / 300.0;
    out += &format!(
        "celebrity row update (buffered)  median {:.0} us  {:.0} bytes/commit\n",
        median(&mut lat),
        bytes
    );
    let before = db.log_bytes();
    let mut lat = Vec::new();
    for _ in 0..300 {
        let a = rng.below(users as u64) as u32;
        let b = rng.below(users as u64) as u32;
        let t = Instant::now();
        db.commit("ordinary", |r| r.add(a, b)).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let bytes_ordinary = (db.log_bytes() - before) as f64 / 300.0;
    out += &format!(
        "ordinary random add (buffered)  median {:.0} us  {:.0} bytes/commit\n",
        median(&mut lat),
        bytes_ordinary
    );
    drop(db);

    let db: Db<R> = Db::open(&path).unwrap();
    let mut lat = Vec::new();
    for _ in 0..200 {
        next += 1;
        let t = Instant::now();
        db.commit("celeb", |r| r.add(celebrity, next)).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
    }
    out += &format!(
        "celebrity row update (fsync)  median {:.0} us\n",
        median(&mut lat)
    );
    let mut lat = Vec::new();
    for _ in 0..200 {
        let a = rng.below(users as u64) as u32;
        let b = rng.below(users as u64) as u32;
        let t = Instant::now();
        db.commit("ordinary", |r| r.add(a, b)).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
    }
    out += &format!(
        "ordinary random add (fsync)  median {:.0} us\n",
        median(&mut lat)
    );
    out += &format!("peak RSS {} MiB\n", rss_mib());
    out
}

pub fn experiment(dir: &Path, which: &str, users: u32, edges: u64, celebrity_followers: u32) {
    let mut list = power_law_edges(users, edges, 7);
    let celebrity = users - 1;
    list.extend((0..celebrity_followers.min(users - 1)).map(|x| (celebrity, x)));
    list.sort_unstable();
    list.dedup();
    println!(
        "{} users, {} edges (celebrity row {} edges)",
        users,
        list.len(),
        celebrity_followers
    );
    let text = match which {
        "nested" => run::<Nested>(dir, &list, celebrity, users),
        "flat" => run::<Flat>(dir, &list, celebrity, users),
        "both" => run::<Both>(dir, &list, celebrity, users),
        "bucketed" => run::<Bucketed>(dir, &list, celebrity, users),
        other => panic!("unknown rep {other}"),
    };
    print!("{text}");
}
