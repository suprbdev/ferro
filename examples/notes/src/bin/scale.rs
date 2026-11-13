//! `notes-scale`: measures what a sync sends for a small change on a large notes database.
//!
//! It fills a laptop device with `--notes` notes in batches of `--batch`, syncs them to an empty
//! phone, makes `--edits` edits on each side plus a conflicting edit and a tag on one shared note,
//! and syncs again, reporting the bytes and objects each pull sent and received and how long it
//! took. It fails if the two devices do not end with the same notes. The devices live in `--dir`,
//! or in a temporary directory that is removed afterwards.
//!
//! ```sh
//! make notes-scale
//! cargo run --release -p notes --bin notes-scale -- --notes 1000000 --edits 20
//! ```

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use notes::{Device, Synced};

const WORDS: [&str; 24] = [
    "call", "plan", "buy", "fix", "read", "write", "ship", "check", "milk", "bread", "invoice",
    "garden", "meeting", "draft", "review", "travel", "book", "ticket", "paint", "window",
    "letter", "bank", "dentist", "train",
];

#[derive(Parser)]
#[command(
    name = "notes-scale",
    about = "Measure what a sync sends for a small change on a large notes database"
)]
struct Args {
    #[arg(long, default_value_t = 100_000)]
    notes: usize,
    #[arg(long, default_value_t = 5)]
    edits: usize,
    #[arg(long, default_value_t = 10_000)]
    batch: usize,
    #[arg(long)]
    dir: Option<PathBuf>,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }

    fn sentence(&mut self) -> String {
        let words = 6 + self.below(10);
        (0..words)
            .map(|_| WORDS[self.below(WORDS.len())])
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn tags(&mut self) -> Vec<String> {
        (0..1 + self.below(3))
            .map(|_| format!("tag{}", self.below(20)))
            .collect()
    }
}

fn report(label: &str, synced: &Synced, seconds: f64) {
    println!(
        "{label}: {} bytes, {} objects, {:.1} ms",
        synced.bytes(),
        synced.objects(),
        seconds * 1000.0
    );
    for step in &synced.steps {
        println!(
            "  {} into {}: {:?}, sent {} bytes, received {} bytes in {} objects",
            step.from,
            step.into,
            step.pulled.outcome,
            step.pulled.sent_bytes,
            step.pulled.received_bytes,
            step.pulled.objects
        );
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let owned = args.dir.is_none();
    let root = args.dir.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("notes-scale-{}", std::process::id()))
    });
    fs::create_dir_all(&root)?;
    let laptop = Device::open(root.join("laptop"))?;
    let phone = Device::open(root.join("phone"))?;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let started = Instant::now();
    let mut left = args.notes;
    while left > 0 {
        let take = left.min(args.batch.max(1));
        let batch: Vec<_> = (0..take).map(|_| (rng.sentence(), rng.tags())).collect();
        laptop.add_many(batch)?;
        left -= take;
    }
    println!(
        "laptop: {} notes in {:.2} s, {} bytes on disk",
        args.notes,
        started.elapsed().as_secs_f64(),
        laptop.db().log_bytes()
    );
    let started = Instant::now();
    let cloned = phone.sync(&laptop)?;
    report(
        "first sync, phone empty",
        &cloned,
        started.elapsed().as_secs_f64(),
    );
    let ids: Vec<String> = laptop.list(None).into_iter().map(|note| note.id).collect();
    let shared = ids[rng.below(ids.len())].clone();
    for _ in 0..args.edits {
        laptop.edit(&ids[rng.below(ids.len())], &rng.sentence())?;
        phone.edit(&ids[rng.below(ids.len())], &rng.sentence())?;
    }
    laptop.edit(&shared, "edited on the laptop")?;
    phone.edit(&shared, "edited on the phone")?;
    phone.tag(&shared, "urgent")?;
    let new = phone.add("written on the phone", &["phone".to_string()])?;
    println!(
        "commits: laptop {} edits, phone {} edits, a tag and {new}; {shared} edited on both",
        args.edits + 1,
        args.edits + 1
    );
    let started = Instant::now();
    let synced = laptop.sync(&phone)?;
    report("sync after edits", &synced, started.elapsed().as_secs_f64());
    let started = Instant::now();
    let again = laptop.sync(&phone)?;
    report("sync again", &again, started.elapsed().as_secs_f64());
    let converged = laptop.everything() == phone.everything();
    let winner = laptop.get(&shared).map(|note| (note.text, note.tags));
    println!("converged: {converged}, {shared} is now {winner:?}");
    println!(
        "files: laptop {} bytes, phone {} bytes",
        laptop.db().log_bytes(),
        phone.db().log_bytes()
    );
    drop((laptop, phone));
    if owned {
        fs::remove_dir_all(&root)?;
    }
    if !converged {
        return Err("devices did not converge".into());
    }
    Ok(())
}
