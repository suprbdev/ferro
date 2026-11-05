//! The `notes` command line: offline-first notes kept per device and synced with ferro.
//!
//! `--device`, or `NOTES_DEVICE`, names the device's directory, whose name is the device's name.
//! Notes can be added with tags, edited, tagged and untagged, deleted, listed by tag and shown with
//! the stamps of their last changes. `sync` converges this device with another device's directory
//! through [`notes::Device::sync`], merging edits made on both since they last met, and prints what
//! each pull sent.
//!
//! ```sh
//! make notes ARGS='--device phone add "buy milk" --tag shopping'
//! cargo run --release -p notes --bin notes -- --device laptop add "call the bank"
//! cargo run --release -p notes --bin notes -- --device laptop sync phone
//! cargo run --release -p notes --bin notes -- --device phone list --tag shopping
//! ```

use std::error::Error;
use std::io::{self, ErrorKind, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ferro::Outcome;
use notes::device::FILE;
use notes::{Device, Note, NotesError};

#[derive(Parser)]
#[command(
    name = "notes",
    about = "Offline-first notes kept per device and synced with ferro"
)]
struct Cli {
    #[arg(long, env = "NOTES_DEVICE")]
    device: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Add {
        text: String,
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    Edit {
        id: String,
        text: String,
    },
    Tag {
        id: String,
        tag: String,
    },
    Untag {
        id: String,
        tag: String,
    },
    Delete {
        id: String,
    },
    List {
        #[arg(long)]
        tag: Option<String>,
    },
    Show {
        id: String,
    },
    Sync {
        other: PathBuf,
    },
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

fn line(note: &Note) -> String {
    let tags: String = note.tags.iter().map(|tag| format!(" #{tag}")).collect();
    format!("{}  {}{}", note.id, note.text, tags)
}

fn outcome(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::UpToDate => "up to date",
        Outcome::Ahead => "already ahead",
        Outcome::FastForward => "fast-forward",
        Outcome::Merged => "merged",
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    let device = Device::open(&cli.device)?;
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    match cli.command {
        Command::Add { text, tags } => writeln!(out, "{}", device.add(&text, &tags)?)?,
        Command::Edit { id, text } => device.edit(&id, &text)?,
        Command::Tag { id, tag } => device.tag(&id, &tag)?,
        Command::Untag { id, tag } => device.untag(&id, &tag)?,
        Command::Delete { id } => device.delete(&id)?,
        Command::List { tag } => {
            for note in device.list(tag.as_deref()) {
                writeln!(out, "{}", line(&note))?;
            }
        }
        Command::Show { id } => {
            let note = device.get(&id).ok_or(NotesError::NoNote(id))?;
            writeln!(out, "{}", line(&note))?;
            writeln!(
                out,
                "created {} on {}",
                note.created.clock, note.created.device
            )?;
            writeln!(
                out,
                "text from {} at {}",
                note.text_at.device, note.text_at.clock
            )?;
            if note.deleted {
                writeln!(
                    out,
                    "deleted on {} at {}",
                    note.deleted_at.device, note.deleted_at.clock
                )?;
            }
        }
        Command::Sync { other } => {
            if !other.join(FILE).exists() {
                return Err(
                    NotesError::BadDevice(format!("no notes in {}", other.display())).into(),
                );
            }
            let other = Device::open(&other)?;
            let synced = device.sync(&other)?;
            for step in &synced.steps {
                writeln!(
                    out,
                    "{} into {}: {}, {} objects, {} bytes",
                    step.from,
                    step.into,
                    outcome(step.pulled.outcome),
                    step.pulled.objects,
                    step.pulled.sent_bytes + step.pulled.received_bytes
                )?;
            }
            writeln!(out, "{} bytes in all", synced.bytes())?;
        }
        Command::Log { limit } => {
            for commit in device.db().log().values().rev().take(limit) {
                writeln!(out, "v{} {}", commit.version, commit.message)?;
            }
        }
    }
    out.flush()?;
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let broken_pipe = error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == ErrorKind::BrokenPipe);
            if !broken_pipe {
                eprintln!("error: {error}");
            }
            ExitCode::FAILURE
        }
    }
}
