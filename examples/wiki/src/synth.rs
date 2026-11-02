pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn normal(&mut self) -> f64 {
        let u = self.unit().max(1e-12);
        let v = self.unit();
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

const REAL: [&str; 121] = [
    "the",
    "of",
    "and",
    "to",
    "in",
    "a",
    "is",
    "that",
    "for",
    "it",
    "system",
    "data",
    "with",
    "as",
    "was",
    "on",
    "are",
    "by",
    "this",
    "be",
    "from",
    "or",
    "an",
    "which",
    "have",
    "not",
    "design",
    "process",
    "model",
    "network",
    "memory",
    "storage",
    "version",
    "history",
    "page",
    "index",
    "search",
    "value",
    "record",
    "table",
    "function",
    "program",
    "language",
    "service",
    "request",
    "response",
    "server",
    "client",
    "error",
    "state",
    "change",
    "update",
    "write",
    "read",
    "file",
    "block",
    "cache",
    "queue",
    "thread",
    "lock",
    "event",
    "stream",
    "message",
    "packet",
    "route",
    "address",
    "protocol",
    "security",
    "key",
    "hash",
    "tree",
    "graph",
    "node",
    "edge",
    "cluster",
    "shard",
    "replica",
    "backup",
    "restore",
    "commit",
    "branch",
    "merge",
    "conflict",
    "schema",
    "query",
    "report",
    "metric",
    "monitor",
    "alert",
    "deploy",
    "release",
    "build",
    "test",
    "debug",
    "trace",
    "profile",
    "latency",
    "throughput",
    "capacity",
    "load",
    "balance",
    "scale",
    "region",
    "zone",
    "account",
    "user",
    "group",
    "role",
    "policy",
    "audit",
    "archive",
    "document",
    "article",
    "section",
    "chapter",
    "summary",
    "overview",
    "guide",
    "reference",
    "example",
    "tutorial",
];

const SYLLABLES: [&str; 40] = [
    "ba", "be", "bi", "bo", "ka", "ke", "ki", "ko", "da", "de", "di", "do", "la", "le", "li", "lo",
    "ma", "me", "mi", "mo", "na", "ne", "ni", "no", "pa", "pe", "pi", "po", "ra", "re", "ri", "ro",
    "sa", "se", "si", "so", "ta", "te", "ti", "to",
];

pub struct Vocab {
    words: Vec<String>,
    cdf: Vec<f64>,
}

impl Vocab {
    pub fn new(size: usize) -> Self {
        let mut words: Vec<String> = REAL.iter().map(|word| word.to_string()).collect();
        let mut i = 0usize;
        while words.len() < size {
            let mut n = i;
            let mut word = String::new();
            for _ in 0..3 {
                word.push_str(SYLLABLES[n % 40]);
                n /= 40;
            }
            if i.is_multiple_of(7) {
                word.push_str(SYLLABLES[(i / 7) % 40]);
            }
            words.push(word);
            i += 1;
        }
        let mut cdf = Vec::with_capacity(words.len());
        let mut total = 0.0;
        for rank in 0..words.len() {
            total += 1.0 / (rank as f64 + 2.0).powf(1.05);
            cdf.push(total);
        }
        Vocab { words, cdf }
    }

    pub fn word(&self, rng: &mut Rng) -> &str {
        let target = rng.unit() * self.cdf[self.cdf.len() - 1];
        let index = self.cdf.partition_point(|&c| c < target);
        &self.words[index.min(self.words.len() - 1)]
    }
}

pub fn sentence(rng: &mut Rng, vocab: &Vocab, out: &mut String) {
    let words = 6 + rng.below(14);
    for i in 0..words {
        let word = vocab.word(rng);
        if i == 0 {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
        } else {
            out.push(' ');
            out.push_str(word);
        }
    }
    out.push_str(if rng.below(12) == 0 { "? " } else { ". " });
}

pub fn prose(
    rng: &mut Rng,
    vocab: &Vocab,
    target: usize,
    link: &mut dyn FnMut(&mut Rng) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(target + 256);
    while out.len() < target {
        match rng.below(9) {
            0 => {
                out.push_str("## ");
                for i in 0..2 + rng.below(3) {
                    if i > 0 {
                        out.push(' ');
                    }
                    out.push_str(vocab.word(rng));
                }
                out.push_str("\n\n");
            }
            1 => {
                for _ in 0..2 + rng.below(4) {
                    out.push_str("- ");
                    sentence(rng, vocab, &mut out);
                    out.pop();
                    out.push('\n');
                }
                out.push('\n');
            }
            _ => {
                for _ in 0..2 + rng.below(5) {
                    sentence(rng, vocab, &mut out);
                    if rng.below(6) == 0
                        && let Some(slug) = link(rng)
                    {
                        out.push_str(&format!("See [[{slug}]]. "));
                    }
                }
                if out.ends_with(' ') {
                    out.pop();
                }
                out.push_str("\n\n");
            }
        }
    }
    out
}
