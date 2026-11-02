use std::fs;
use std::path::PathBuf;

use wiki::synth::{Rng, Vocab, prose};

const CATEGORIES: [&str; 24] = [
    "ops", "dev", "design", "security", "data", "network", "people", "finance", "legal", "support",
    "sales", "research", "infra", "mobile", "web", "ml", "qa", "release", "hardware", "training",
    "policy", "archive", "howto", "incident",
];

fn slug_of(n: usize) -> String {
    format!("{}/p{n:05}", CATEGORIES[n % CATEGORIES.len()])
}

fn main() {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: gencorpus <dir> [pages]"),
    );
    let pages: usize = std::env::args()
        .nth(2)
        .and_then(|n| n.parse().ok())
        .unwrap_or(50_000);
    let vocab = Vocab::new(60_000);
    let mut total = 0usize;
    let mut biggest = 0usize;
    for n in 0..pages {
        let mut rng = Rng::new(n as u64 + 1);
        let (slug, target) = match n {
            0 => ("bench/large-100k".to_string(), 100 * 1024),
            1 => ("bench/huge-1m".to_string(), 1024 * 1024),
            _ => {
                let size = if n % 2500 == 0 {
                    300_000 + rng.below(700_000) as usize
                } else if n % 100 == 0 {
                    40_000 + rng.below(110_000) as usize
                } else {
                    let z = rng.normal();
                    ((2000.0 * (1.1 * z).exp()) as usize).clamp(600, 60_000)
                };
                (slug_of(n), size)
            }
        };
        let mut link = |rng: &mut Rng| {
            let skew = rng.unit().powi(3);
            Some(slug_of((skew * pages as f64) as usize % pages.max(1)))
        };
        let mut title = String::new();
        for i in 0..2 {
            let word = vocab.word(&mut rng);
            if i > 0 {
                title.push(' ');
            }
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                title.extend(first.to_uppercase());
                title.push_str(chars.as_str());
            }
        }
        let mut text = format!("# {title} {n}\n\n");
        text.push_str(&prose(&mut rng, &vocab, target, &mut link));
        if n == 31_337 % pages {
            text.push_str("The zorblax anomaly was first recorded here.\n");
        }
        if n % 5000 == 7 {
            text.push_str("Notes on the quuxel subsystem.\n");
        }
        let path = dir.join(format!("{slug}.md"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &text).unwrap();
        total += text.len();
        biggest = biggest.max(text.len());
    }
    println!("{pages} pages, {total} bytes, largest {biggest}");
}
