pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    pub fn draw(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn unit(&mut self) -> f64 {
        (self.draw() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn below(&mut self, bound: u64) -> u64 {
        self.draw() % bound.max(1)
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

pub struct Scramble {
    n: u64,
    mul: u64,
    add: u64,
}

impl Scramble {
    pub fn new(n: u64, seed: u64) -> Scramble {
        let mut mul = 1_000_003 + 2 * (seed % 1000);
        while gcd(mul, n) != 1 {
            mul += 2;
        }
        Scramble {
            n,
            mul,
            add: seed.wrapping_mul(7919) % n.max(1),
        }
    }

    pub fn apply(&self, rank: u64) -> u32 {
        ((rank as u128 * self.mul as u128 + self.add as u128) % self.n as u128) as u32
    }
}

pub fn power_law_edges(users: u32, edges: u64, seed: u64) -> Vec<(u32, u32)> {
    let n = users as u64;
    if n < 2 {
        return Vec::new();
    }
    let mut rng = Rng::new(seed);
    let src_map = Scramble::new(n, seed ^ 0x51);
    let dst_map = Scramble::new(n, seed ^ 0xA3);
    let ln_n = (n as f64).ln();
    let mut packed: Vec<u64> = Vec::with_capacity(edges as usize);
    for _ in 0..edges {
        let u = rng.unit();
        let src_rank = ((n as f64) * u * u) as u64;
        let dst_rank = if rng.unit() < 0.8 {
            ((rng.unit() * ln_n).exp() as u64).saturating_sub(1)
        } else {
            rng.below(n)
        };
        let src = src_map.apply(src_rank.min(n - 1));
        let dst = dst_map.apply(dst_rank.min(n - 1));
        if src != dst {
            packed.push(((src as u64) << 32) | dst as u64);
        }
    }
    packed.sort_unstable();
    packed.dedup();
    packed
        .into_iter()
        .map(|p| ((p >> 32) as u32, p as u32))
        .collect()
}

pub fn celebrity(users: u32, seed: u64) -> u32 {
    let n = users as u64;
    Scramble::new(n, seed ^ 0xA3).apply(0)
}
