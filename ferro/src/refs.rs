//! The branch table a file's head points at, and the steps that change it one branch at a time.
//!
//! A commit does not rewrite the whole table of branches. It writes a step: one branch's name and
//! new [`Head`], linking the table or step the head named before. Following the steps back to the
//! table they start from, the newest step for each name winning, gives every branch's head. After
//! [`STEPS`] steps the next change writes a whole table again.
//! [Branch-table steps](https://github.com/archlemon/ferro/blob/main/docs/internals/commit-path.md#branch-table-steps)
//! describes them.

use std::collections::HashSet;
use std::sync::Arc;

use crate::codec::{Data, Decoder, Encoder, HASH_BYTES, Hash, Kind, Ptr, Sink};
use crate::db::{Head, Refs};
use crate::error::{Error, Result};
use crate::log::Chain;
use crate::map::Map;
use crate::store::{Store, Txn, Unchecked};

const STEP: u8 = 0x7f;
/// How many steps a writer stacks on a table before it writes a whole table again.
pub(crate) const STEPS: usize = 64;
const MAX_STEPS: usize = 1024;

/// One branch's new head, written on top of the table or step before it.
#[derive(Clone)]
pub(crate) struct Step {
    prev: Ptr,
    name: String,
    head: Head,
}

/// What a file's head points at.
#[derive(Clone)]
pub(crate) enum Root {
    /// A whole branch table.
    Table(Refs),
    /// A step on top of an earlier table or step.
    Step(Step),
}

fn encode_step(w: &mut Encoder<'_>, prev: Ptr, name: &str, head: &Head) -> Result<()> {
    w.u8(STEP);
    w.link(prev);
    w.varint(name.len() as u64);
    w.raw(name.as_bytes());
    head.encode(w)
}

impl Data for Root {
    const LOCATED: bool = true;

    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        match self {
            Root::Table(refs) => refs.encode(w),
            Root::Step(step) => encode_step(w, step.prev, &step.name, &step.head),
        }
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        if r.lone_varint() {
            return Ok(Root::Table(Refs::decode(r)?));
        }
        let tag = r.u8()?;
        if tag != STEP {
            return Err(Error::Decode(format!("invalid branch table tag {tag}")));
        }
        let prev = r.typed_link(Kind::value::<Root>)?;
        let name = String::decode(r)?;
        let head = Head::decode(r)?;
        if r.remaining() != 0 {
            return Err(Error::Decode(format!(
                "a branch table step has {} bytes after its head",
                r.remaining()
            )));
        }
        Ok(Root::Step(Step { prev, name, head }))
    }
}

/// Writes the step that sets branch `name` to `head` on top of the table or step at `prev`.
///
/// It returns the step's pointer and the head as decoded back from what was written.
pub(crate) fn write_step(
    txn: &mut Txn,
    store: &Store,
    prev: Ptr,
    name: &str,
    head: &Head,
) -> Result<(Ptr, Head)> {
    let mut encoder = Encoder::new(txn);
    encode_step(&mut encoder, prev, name, head)?;
    let object = encoder.into_object();
    let decoded = Root::decode(&mut Decoder::new(&object.body, &object.links, Some(store)))?;
    let Root::Step(step) = decoded else {
        return Err(Error::Corrupt(
            "a branch table step decoded as a table".into(),
        ));
    };
    let hash = object.hash();
    Ok((txn.put(object, hash)?, step.head))
}

struct Layers {
    steps: Vec<Step>,
    base: Refs,
    depth: usize,
}

fn layers(store: &Store, head: Ptr, since: Option<(Ptr, &Refs, usize)>) -> Result<Layers> {
    let mut steps = Vec::new();
    let mut at = head;
    loop {
        if let Some((known, refs, depth)) = since
            && known == at
        {
            return Ok(Layers {
                depth: depth + steps.len(),
                steps,
                base: refs.clone(),
            });
        }
        match store.decode_at(at, Root::decode)? {
            Root::Table(base) => {
                return Ok(Layers {
                    depth: steps.len(),
                    steps,
                    base,
                });
            }
            Root::Step(step) => {
                if steps.len() >= MAX_STEPS {
                    return Err(Error::Corrupt(format!(
                        "the branch table at {} runs back more than {MAX_STEPS} steps",
                        head.offset
                    )));
                }
                at = step.prev;
                steps.push(step);
            }
        }
    }
}

/// The branch table at `head`, with its steps applied, and how many steps deep it is.
///
/// `since` names a pointer whose table is already loaded, with its depth, so that the walk back
/// stops there and reads only the steps written after it.
///
/// # Errors
///
/// [`Error::Corrupt`] if the steps run back further than any writer stacks them, and read
/// errors.
pub(crate) fn load(
    store: &Store,
    head: Option<Ptr>,
    since: Option<(Ptr, &Refs, usize)>,
) -> Result<(Refs, usize)> {
    let Some(head) = head else {
        return Ok((Map::new(), 0));
    };
    let Layers {
        steps,
        mut base,
        depth,
    } = layers(store, head, since)?;
    if depth > MAX_STEPS {
        return Err(Error::Corrupt(format!(
            "the branch table at {} runs back more than {MAX_STEPS} steps",
            head.offset
        )));
    }
    for step in steps.into_iter().rev() {
        base.try_insert(step.name, step.head)?;
    }
    Ok((base, depth))
}

/// Every head a branch table records, from [`heads`].
pub(crate) struct Heads {
    /// Each branch's newest head.
    pub(crate) current: Vec<Head>,
    /// The older heads that earlier steps record for the same branches.
    pub(crate) superseded: Vec<Head>,
}

/// Every head the branch table at `head` and its steps record, split into each branch's newest
/// and the older ones.
pub(crate) fn heads(store: &Store, head: Ptr) -> Result<Heads> {
    let Layers { steps, base, .. } = layers(store, head, None)?;
    let mut named = HashSet::new();
    let mut heads = Heads {
        current: Vec::new(),
        superseded: Vec::new(),
    };
    let found = steps.into_iter().map(|step| (step.name, step.head));
    for (name, head) in found.chain(base.try_entries()?) {
        match named.insert(name) {
            true => heads.current.push(head),
            false => heads.superseded.push(head),
        }
    }
    Ok(heads)
}

/// The name under which the branch table records the table values that a receive landed holding
/// an index under a fingerprint the receiving type did not know, each by the hash of its bytes and
/// links, as the keys of its head's merged map. No branch a handle opens may start with a NUL.
pub(crate) const UNCHECKED: &str = "\u{0}unchecked tables";

/// Whether `name` is kept for what the branch table records of its own, which no branch may be
/// named.
pub(crate) fn reserved(name: &str) -> bool {
    name.starts_with('\u{0}')
}

/// Takes into `store` what `refs` records of the table values that landed with an index nothing
/// checked, reading the record again only when it changed.
///
/// A record that cannot be read, in a damaged branch table, leaves `store` trusting no stored
/// index, so that every read of a table rebuilds its indexes, rather than fail the open of a
/// file whose branches may still read.
pub(crate) fn note_unchecked(store: &Store, refs: &Refs) {
    let unchecked = (|| -> Result<Option<Arc<Unchecked>>> {
        let Some(head) = refs.try_get(UNCHECKED)? else {
            return Ok(None);
        };
        let root = head.merged.root_hash();
        if let Some(known) = store
            .unchecked()
            .filter(|known| known.root == root && !known.all)
        {
            return Ok(Some(known));
        }
        let mut tables = HashSet::new();
        for (key, _) in head.merged.try_entries()? {
            tables.insert(unhex(&key).ok_or_else(|| {
                Error::Corrupt(format!("the branch table records {key:?} as a table"))
            })?);
        }
        Ok(Some(Arc::new(Unchecked {
            root,
            tables,
            all: false,
        })))
    })();
    store.set_unchecked(unchecked.unwrap_or_else(|_| {
        Some(Arc::new(Unchecked {
            root: None,
            tables: HashSet::new(),
            all: true,
        }))
    }));
}

/// Records in `refs` the table values `tables`, by hash, as landed with an index nothing checked.
///
/// # Errors
///
/// Read errors of the branch table.
pub(crate) fn record_unchecked(refs: &mut Refs, tables: &[Hash]) -> Result<()> {
    if tables.is_empty() {
        return Ok(());
    }
    let mut head = match refs.try_get(UNCHECKED)? {
        Some(head) => (*head).clone(),
        None => Head {
            version: 0,
            root: None,
            log: Chain::empty(),
            merged: Map::new(),
            born: 0,
        },
    };
    for hash in tables {
        head.merged.insert(hex(hash), (0, 0));
    }
    refs.try_insert(UNCHECKED.to_string(), head)
}

fn hex(hash: &Hash) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<Hash> {
    let mut hash = [0u8; HASH_BYTES];
    if text.len() != 2 * HASH_BYTES {
        return None;
    }
    for (at, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * at..2 * at + 2)?, 16).ok()?;
    }
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Closing, Durability, StoreOptions};

    fn open(dir: &tempfile::TempDir) -> Closing {
        let options = StoreOptions {
            durability: Durability::Buffered,
            ..StoreOptions::default()
        };
        Closing(
            Store::open(&dir.path().join("refs.ferro"), options)
                .unwrap()
                .0,
        )
    }

    fn head(mark: u64) -> Head {
        Head {
            version: 0,
            root: None,
            log: Chain {
                first: mark,
                ..Chain::empty()
            },
            merged: Map::new(),
            born: 0,
        }
    }

    fn table(store: &Store, branches: u64) -> Ptr {
        let mut refs: Refs = Map::new();
        for branch in 0..branches {
            refs.insert(format!("b{branch}"), head(branch));
        }
        let mut txn = store.begin().unwrap();
        let mut encoder = Encoder::new(&mut txn);
        refs.encode(&mut encoder).unwrap();
        let ptr = encoder.finish().unwrap();
        txn.commit(ptr).unwrap();
        ptr
    }

    fn step(store: &Store, prev: Ptr, name: &str, version: u64) -> Ptr {
        let mut txn = store.begin().unwrap();
        let (ptr, _) = write_step(&mut txn, store, prev, name, &head(version)).unwrap();
        txn.commit(ptr).unwrap();
        ptr
    }

    fn versions(refs: &Refs) -> Vec<(String, u64)> {
        let entries = refs.try_entries().unwrap().into_iter();
        entries.map(|(name, head)| (name, head.log.first)).collect()
    }

    #[test]
    fn steps_resolve_over_their_table_newest_last() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir);
        let base = table(&store, 2);
        let one = step(&store, base, "b1", 10);
        let two = step(&store, one, "b0", 20);
        let three = step(&store, two, "b1", 30);
        let new = step(&store, three, "b2", 40);
        let (refs, depth) = load(&store, Some(new), None).unwrap();
        assert_eq!(depth, 4);
        let expected: Vec<_> = [("b0", 20), ("b1", 30), ("b2", 40)]
            .map(|(name, version)| (name.to_string(), version))
            .into();
        assert_eq!(versions(&refs), expected);
        let (older, depth) = load(&store, Some(two), None).unwrap();
        assert_eq!(depth, 2);
        let (caught_up, depth) = load(&store, Some(new), Some((two, &older, 2))).unwrap();
        assert_eq!(depth, 4);
        assert_eq!(versions(&caught_up), expected);
    }

    #[test]
    fn a_reader_from_before_steps_fails_on_a_step() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir);
        let base = table(&store, 3);
        let one = step(&store, base, "b0", 7);
        let two = step(&store, one, "b1", 8);
        for head in [one, two] {
            let old = store.load::<Refs>(head).unwrap();
            assert!(old.try_entries().is_err());
            assert!(old.try_get("b0").is_err());
        }
        assert_eq!(versions(&store.load::<Refs>(base).unwrap()).len(), 3);
    }

    #[test]
    fn a_table_whose_count_is_the_step_tag_reads_as_a_table() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir);
        for branches in [126, 127, 128] {
            let base = table(&store, branches);
            let (refs, depth) = load(&store, Some(base), None).unwrap();
            assert_eq!((refs.len(), depth), (branches as usize, 0));
            let top = step(&store, base, "b5", 99);
            let (refs, depth) = load(&store, Some(top), None).unwrap();
            assert_eq!((refs.len(), depth), (branches as usize, 1));
            assert_eq!(refs.try_get("b5").unwrap().unwrap().log.first, 99);
        }
    }

    #[test]
    fn a_chain_of_steps_past_the_limit_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir);
        let mut top = table(&store, 1);
        for version in 1..=MAX_STEPS as u64 {
            top = step(&store, top, "b0", version);
        }
        assert_eq!(load(&store, Some(top), None).unwrap().1, MAX_STEPS);
        top = step(&store, top, "b0", 0);
        assert!(matches!(
            load(&store, Some(top), None),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn a_step_must_carry_its_tag_and_nothing_after_its_head() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir);
        let base = table(&store, 1);
        for (tag, extra) in [(STEP, true), (STEP - 1, false), (STEP + 1, false)] {
            let mut txn = store.begin().unwrap();
            let mut encoder = Encoder::new(&mut txn);
            encoder.u8(tag);
            encoder.link(base);
            encoder.varint(2);
            encoder.raw(b"b0");
            head(3).encode(&mut encoder).unwrap();
            if extra {
                encoder.u8(0);
            }
            let ptr = encoder.finish().unwrap();
            txn.commit(ptr).unwrap();
            assert!(load(&store, Some(ptr), None).is_err());
        }
    }
}
