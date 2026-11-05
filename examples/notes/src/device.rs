use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ferro::{Db, Options, Outcome, Pulled};

use crate::model::{Note, Notes, Stamp, merge};

pub const FILE: &str = "notes.ferro";

/// Every way a notes command can fail.
#[derive(Debug)]
pub enum NotesError {
    Ferro(ferro::Error),
    Io(io::Error),
    NoNote(String),
    Deleted(String),
    BadDevice(String),
}

impl fmt::Display for NotesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotesError::Ferro(error) => write!(f, "{error}"),
            NotesError::Io(error) => write!(f, "{error}"),
            NotesError::NoNote(id) => write!(f, "no note {id}"),
            NotesError::Deleted(id) => write!(f, "note {id} is deleted"),
            NotesError::BadDevice(why) => write!(f, "{why}"),
        }
    }
}

impl Error for NotesError {}

impl From<ferro::Error> for NotesError {
    fn from(error: ferro::Error) -> Self {
        NotesError::Ferro(error)
    }
}

impl From<io::Error> for NotesError {
    fn from(error: io::Error) -> Self {
        NotesError::Io(error)
    }
}

/// A result whose error is a [`NotesError`].
pub type Result<T> = std::result::Result<T, NotesError>;

/// One pull in a sync: which device pulled from which, and what the pull did and sent.
#[derive(Clone, Debug)]
pub struct Step {
    pub from: String,
    pub into: String,
    pub pulled: Pulled<usize>,
}

/// The pulls a [`Device::sync`] made, in order.
#[derive(Clone, Debug, Default)]
pub struct Synced {
    pub steps: Vec<Step>,
}

impl Synced {
    pub fn bytes(&self) -> u64 {
        self.steps
            .iter()
            .map(|step| step.pulled.sent_bytes + step.pulled.received_bytes)
            .sum()
    }

    pub fn objects(&self) -> u64 {
        self.steps.iter().map(|step| step.pulled.objects).sum()
    }
}

/// One device's notes: a directory, named for the device, holding its database.
///
/// Each write makes one commit and stamps what it changes with the device's next clock ticks, and
/// [`Device::sync`] converges it with another device.
pub struct Device {
    name: String,
    dir: PathBuf,
    db: Db<Notes>,
}

fn stamp(notes: &mut Notes, device: &str) -> Stamp {
    notes.clock += 1;
    Stamp {
        clock: notes.clock,
        device: device.to_string(),
    }
}

impl Device {
    pub fn open(dir: impl AsRef<Path>) -> Result<Device> {
        Device::open_with(dir, Options::new())
    }

    pub fn open_with(dir: impl AsRef<Path>, options: Options) -> Result<Device> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        let name = fs::canonicalize(dir)?
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .filter(|name| !name.is_empty() && name != "main")
            .ok_or_else(|| {
                NotesError::BadDevice(format!("{} cannot name a device", dir.display()))
            })?;
        let db = options.open(dir.join(FILE))?;
        Ok(Device {
            name,
            dir: dir.to_path_buf(),
            db,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn db(&self) -> &Db<Notes> {
        &self.db
    }

    pub fn add(&self, text: &str, tags: &[String]) -> Result<String> {
        let name = &self.name;
        let added = self.db.commit_with(|notes| {
            let at = stamp(notes, name);
            let id = format!("{name}-{}", at.clock);
            notes
                .notes
                .insert(Note::new(id.clone(), text.to_string(), tags, at));
            (format!("add {id}"), id)
        })?;
        Ok(added.value)
    }

    pub fn add_many(
        &self,
        items: impl IntoIterator<Item = (String, Vec<String>)>,
    ) -> Result<usize> {
        let name = &self.name;
        let added = self.db.commit_with(|notes| {
            let mut count = 0;
            for (text, tags) in items {
                let at = stamp(notes, name);
                let id = format!("{name}-{}", at.clock);
                notes.notes.insert(Note::new(id, text, &tags, at));
                count += 1;
            }
            (format!("add {count} notes"), count)
        })?;
        Ok(added.value)
    }

    fn change(&self, id: &str, what: &str, f: impl FnOnce(&mut Note, Stamp)) -> Result<()> {
        let name = &self.name;
        self.db.try_commit(format!("{what} {id}"), |notes| {
            let mut note = match notes.notes.get(id) {
                Some(note) => (*note).clone(),
                None => return Err(NotesError::NoNote(id.to_string())),
            };
            if note.deleted {
                return Err(NotesError::Deleted(id.to_string()));
            }
            let at = stamp(notes, name);
            f(&mut note, at);
            notes.notes.insert(note);
            Ok(())
        })
    }

    pub fn edit(&self, id: &str, text: &str) -> Result<()> {
        self.change(id, "edit", |note, at| {
            note.text = text.to_string();
            note.text_at = at.clone();
            note.touch(at);
        })
    }

    pub fn tag(&self, id: &str, tag: &str) -> Result<()> {
        self.change(id, "tag", |note, at| {
            note.mark(tag, true, at.clone());
            note.touch(at);
        })
    }

    pub fn untag(&self, id: &str, tag: &str) -> Result<()> {
        self.change(id, "untag", |note, at| {
            note.mark(tag, false, at.clone());
            note.touch(at);
        })
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.change(id, "delete", |note, at| {
            note.deleted = true;
            note.deleted_at = at;
        })
    }

    pub fn get(&self, id: &str) -> Option<Note> {
        self.db.read().notes.get(id).map(|note| (*note).clone())
    }

    pub fn list(&self, tag: Option<&str>) -> Vec<Note> {
        let notes = self.db.read();
        let visible = |note: &Note| !note.deleted;
        match tag {
            Some(tag) => notes
                .notes
                .by_tags(tag)
                .filter(|note| visible(note))
                .map(|note| (*note).clone())
                .collect(),
            None => notes
                .notes
                .iter()
                .filter(|note| visible(note))
                .map(|note| (*note).clone())
                .collect(),
        }
    }

    pub fn everything(&self) -> Vec<Note> {
        self.db
            .read()
            .notes
            .iter()
            .map(|note| (*note).clone())
            .collect()
    }

    fn pull(&self, other: &Device, synced: &mut Synced) -> Result<Outcome> {
        let pulled = match self.db.pull(&other.db, &other.name, merge) {
            Err(ferro::Error::StaleHave(_)) => self.db.pull(&other.db, &other.name, merge)?,
            pulled => pulled?,
        };
        let outcome = pulled.outcome;
        synced.steps.push(Step {
            from: other.name.clone(),
            into: self.name.clone(),
            pulled,
        });
        Ok(outcome)
    }

    pub fn sync(&self, other: &Device) -> Result<Synced> {
        if other.name == self.name {
            return Err(NotesError::BadDevice(format!(
                "both devices are called {}; rename one directory",
                self.name
            )));
        }
        let mut synced = Synced::default();
        self.pull(other, &mut synced)?;
        if other.pull(self, &mut synced)? == Outcome::Merged {
            self.pull(other, &mut synced)?;
        }
        Ok(synced)
    }
}
