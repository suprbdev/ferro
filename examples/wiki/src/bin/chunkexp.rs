//! `chunkexp`: compares ways to store a large page body by the cost of small edits.
//!
//! It generates a base text of `base_kib` KiB of prose and `count` small random edits, then for
//! each layout commits the base and every edit to a fresh database in `dir`: the body as one
//! `String`, as a [`ferro::List`] of fixed 4 KiB pieces, and as a list of content-defined chunks
//! either rebuilt or updated in place with [`wiki::chunk::update`] at several chunk sizes. It runs
//! each layout warm, on one open database, and cold, reopening it for every edit, and prints the
//! file growth per edit and the time per edit.
//!
//! ```sh
//! cargo run --release -p wiki --bin chunkexp
//! cargo run --release -p wiki --bin chunkexp -- target/scratch/chunkexp 1000 100
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ferro::{Data, Db, List, Options};
use wiki::chunk::{self, Chunker};
use wiki::synth::{Rng, Vocab, prose};

#[derive(Clone, Default, Data)]
struct StringRoot {
    body: String,
}

#[derive(Clone, Default, Data)]
struct ListRoot {
    body: List<String>,
}

struct Edit {
    at: usize,
    remove: usize,
    insert: String,
}

fn make_edits(rng: &mut Rng, vocab: &Vocab, text: &str, count: usize) -> Vec<Edit> {
    let mut len = text.len();
    let mut edits = Vec::new();
    for _ in 0..count {
        let at = rng.below(len as u64 - 200) as usize;
        let edit = match rng.below(3) {
            0 => Edit {
                at,
                remove: 12,
                insert: format!("{} {}", vocab.word(rng), vocab.word(rng)),
            },
            1 => {
                let mut insert = String::new();
                insert.push_str(vocab.word(rng));
                for _ in 0..6 {
                    insert.push(' ');
                    insert.push_str(vocab.word(rng));
                }
                insert.push(' ');
                Edit {
                    at,
                    remove: 0,
                    insert,
                }
            }
            _ => Edit {
                at,
                remove: 20 + rng.below(40) as usize,
                insert: String::new(),
            },
        };
        len = len + edit.insert.len() - edit.remove;
        edits.push(edit);
    }
    edits
}

fn apply(text: &mut String, edit: &Edit) {
    let mut start = edit.at.min(text.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = (start + edit.remove).min(text.len());
    while !text.is_char_boundary(end) {
        end += 1;
    }
    text.replace_range(start..end, &edit.insert);
}

fn fixed(text: &str, size: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + size).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        chunks.push(text[start..end].to_string());
        start = end;
    }
    chunks
}

fn run<T: Data + Default>(
    label: &str,
    dir: &Path,
    base: &str,
    edits: &[Edit],
    reopen: bool,
    set: impl Fn(&mut T, &str),
) {
    let path: PathBuf = dir.join(format!(
        "{label}-{}.ferro",
        if reopen { "cold" } else { "warm" }
    ));
    let _ = fs::remove_file(&path);
    let mut text = base.to_string();
    {
        let db: Db<T> = Options::new().open(&path).unwrap();
        db.commit("init", |root| set(root, &text)).unwrap();
    }
    let initial = fs::metadata(&path).unwrap().len();
    let started = Instant::now();
    if reopen {
        for edit in edits {
            apply(&mut text, edit);
            let db: Db<T> = Options::new().open(&path).unwrap();
            db.commit("edit", |root| set(root, &text)).unwrap();
        }
    } else {
        let db: Db<T> = Options::new().open(&path).unwrap();
        for edit in edits {
            apply(&mut text, edit);
            db.commit("edit", |root| set(root, &text)).unwrap();
        }
    }
    let elapsed = started.elapsed();
    let last = fs::metadata(&path).unwrap().len();
    let growth = last - initial;
    println!(
        "{label:<28} {:<5} initial {:>9}  growth {:>11}  per edit {:>8}  {:>6.1} ms/edit",
        if reopen { "cold" } else { "warm" },
        initial,
        growth,
        growth / edits.len() as u64,
        elapsed.as_secs_f64() * 1000.0 / edits.len() as f64
    );
}

fn main() {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "target/scratch/chunkexp".to_string()),
    );
    let count: usize = std::env::args()
        .nth(2)
        .and_then(|n| n.parse().ok())
        .unwrap_or(1000);
    let base_kib: usize = std::env::args()
        .nth(3)
        .and_then(|n| n.parse().ok())
        .unwrap_or(100);
    fs::create_dir_all(&dir).unwrap();
    let mut rng = Rng::new(7);
    let vocab = Vocab::new(30_000);
    let base = prose(&mut rng, &vocab, base_kib * 1024, &mut |_| None);
    let edits = make_edits(&mut rng, &vocab, &base, count);
    println!("base {} bytes, {} edits", base.len(), edits.len());
    for reopen in [false, true] {
        run::<StringRoot>("string", &dir, &base, &edits, reopen, |root, text| {
            root.body = text.to_string();
        });
        run::<ListRoot>(
            "list fixed 4096",
            &dir,
            &base,
            &edits,
            reopen,
            |root, text| {
                root.body = fixed(text, 4096).into_iter().collect();
            },
        );
        run::<ListRoot>(
            "list cdc 1k rebuild",
            &dir,
            &base,
            &edits,
            reopen,
            |root, text| {
                root.body = chunk::DEFAULT
                    .split(text)
                    .into_iter()
                    .map(str::to_string)
                    .collect();
            },
        );
        for (name, chunker) in [
            (
                "list cdc 512 update",
                Chunker {
                    min: 256,
                    bits: 9,
                    max: 2048,
                },
            ),
            ("list cdc 1k update", chunk::DEFAULT),
            (
                "list cdc 4k update",
                Chunker {
                    min: 1024,
                    bits: 12,
                    max: 16384,
                },
            ),
        ] {
            run::<ListRoot>(name, &dir, &base, &edits, reopen, move |root, text| {
                chunk::update(&mut root.body, text, &chunker);
            });
        }
    }
}
