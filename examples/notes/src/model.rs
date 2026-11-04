use std::collections::BTreeMap;

use ferro::{Data, Record, Snapshot, Table, Val};

#[derive(Data, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp {
    pub clock: u64,
    pub device: String,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
pub struct Note {
    #[key]
    pub id: String,
    pub text: String,
    pub text_at: Stamp,
    #[index(each)]
    pub tags: Vec<String>,
    pub tag_marks: BTreeMap<String, (bool, Stamp)>,
    pub deleted: bool,
    pub deleted_at: Stamp,
    pub created: Stamp,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
pub struct Notes {
    pub notes: Table<Note>,
    pub clock: u64,
}

impl Note {
    pub fn new(id: String, text: String, tags: &[String], at: Stamp) -> Note {
        let mut note = Note {
            id,
            text,
            text_at: at.clone(),
            tags: Vec::new(),
            tag_marks: BTreeMap::new(),
            deleted: false,
            deleted_at: at.clone(),
            created: at.clone(),
        };
        for tag in tags {
            note.mark(tag, true, at.clone());
        }
        note
    }

    pub fn mark(&mut self, tag: &str, present: bool, at: Stamp) {
        self.tag_marks.insert(tag.to_string(), (present, at));
        self.retag();
    }

    fn retag(&mut self) {
        self.tags = self
            .tag_marks
            .iter()
            .filter(|(_, (present, _))| *present)
            .map(|(tag, _)| tag.clone())
            .collect();
    }

    pub fn touch(&mut self, at: Stamp) {
        self.deleted = false;
        self.deleted_at = at;
    }

    pub fn join(&self, other: &Note) -> Note {
        let mut out = self.clone();
        if other.text_at > out.text_at {
            out.text = other.text.clone();
            out.text_at = other.text_at.clone();
        }
        for (tag, mark) in &other.tag_marks {
            let newer = match out.tag_marks.get(tag) {
                Some(current) => mark.1 > current.1,
                None => true,
            };
            if newer {
                out.tag_marks.insert(tag.clone(), mark.clone());
            }
        }
        out.retag();
        if other.deleted_at > out.deleted_at {
            out.deleted = other.deleted;
            out.deleted_at = other.deleted_at.clone();
        }
        out.created = out.created.clone().min(other.created.clone());
        out
    }
}

pub fn merge(
    notes: &mut Notes,
    base: &Snapshot<Notes>,
    theirs: &Snapshot<Notes>,
) -> (String, usize) {
    let applied = notes.notes.merge(&base.notes, &theirs.notes, |conflict| {
        match (conflict.ours, conflict.theirs) {
            (Some(ours), Some(theirs)) => Some(ours.join(&theirs)),
            (ours, theirs) => ours.or(theirs).map(Val::into_owned),
        }
    });
    notes.clock = notes.clock.max(theirs.clock);
    (
        format!("sync: {applied} notes from another device"),
        applied,
    )
}
