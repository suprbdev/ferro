//! The branch table a file's head points at, and the steps that change it one branch at a time.
//!
//! A commit does not rewrite the whole table of branches. It writes a step: one branch's name and
//! new [`Head`], linking the table or step the head named before. Following the steps back to the
//! table they start from, the newest step for each name winning, gives every branch's head. After
//! [`STEPS`] steps the next change writes a whole table again.
//! [Branch-table steps](https://github.com/archlemon/ferro/blob/main/docs/internals/commit-path.md#branch-table-steps)
//! describes them.

use std::collections::HashSet;

use crate::codec::{Data, Decoder, Encoder, HashSink, Kind, Object, Ptr, Sink};
use crate::db::{Head, Refs};
use crate::error::{Error, Result};
use crate::map::Map;
use crate::store::{Store, Txn};

const STEP: u8 = 0x7f;
const TABLE: u8 = 0x7e;
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
    /// A whole branch table, with the root of version 0 once the file holds one.
    Table(Refs, Option<Ptr>),
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

/// Encodes the branch table `refs`, naming `genesis` as the root of version 0 when it is set.
pub(crate) fn encode_table(w: &mut Encoder<'_>, refs: &Refs, genesis: Option<Ptr>) -> Result<()> {
    if let Some(genesis) = genesis {
        w.u8(TABLE);
        w.link(genesis);
    }
    refs.encode(w)
}

impl Data for Root {
    const LOCATED: bool = true;

    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        match self {
            Root::Table(refs, genesis) => encode_table(w, refs, *genesis),
            Root::Step(step) => encode_step(w, step.prev, &step.name, &step.head),
        }
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        if r.lone_varint() {
            return Ok(Root::Table(Refs::decode(r)?, None));
        }
        let tag = r.u8()?;
        if tag == TABLE {
            let genesis = r.link()?;
            let refs = Refs::decode(r)?;
            if r.remaining() != 0 {
                return Err(Error::Decode(format!(
                    "a branch table has {} bytes after its count",
                    r.remaining()
                )));
            }
            return Ok(Root::Table(refs, Some(genesis)));
        }
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

/// A step encoded before its commit knows the table or step it goes on top of, for the head the
/// commit expects to write.
pub(crate) struct Drafted {
    head: Head,
    object: Object,
}

impl Drafted {
    /// Encodes the step that sets branch `name` to `head`, a head with no merge records, with a
    /// stand-in for its link to the table or step before it.
    pub(crate) fn new(name: &str, head: Head) -> Result<Drafted> {
        let stand_in = Ptr {
            hash: Default::default(),
            offset: 0,
        };
        let mut sink = HashSink;
        let mut encoder = Encoder::new(&mut sink);
        encode_step(&mut encoder, stand_in, name, &head)?;
        Ok(Drafted {
            head,
            object: encoder.into_object(),
        })
    }

    /// The drafted step as it encodes on top of `prev` for `head`, if `head` encodes as the head
    /// it was drafted for: the same fields, links to the same objects wherever they now lie, and
    /// no merge records.
    fn fit(self: Box<Self>, prev: Ptr, head: &Head) -> Option<Object> {
        let hash = |ptr: Option<Ptr>| ptr.map(|ptr| ptr.hash);
        let drafted = &self.head;
        let same = head.merged.is_empty()
            && drafted.version == head.version
            && drafted.born == head.born
            && drafted.log.first == head.log.first
            && hash(drafted.root) == hash(head.root)
            && hash(drafted.log.tip) == hash(head.log.tip);
        if !same {
            return None;
        }
        let mut object = self.object;
        object.links.clear();
        object.links.push(prev);
        object.links.extend(head.root);
        object.links.extend(head.log.tip);
        Some(object)
    }
}

/// Writes the step that sets branch `name` to `head` on top of the table or step at `prev`,
/// taking it from `drafted` if that was drafted for this head.
///
/// It returns the step's pointer and the head as a reader of the step finds it: `head` itself
/// when it holds no merge records, and otherwise decoded back from what was written, so that its
/// merge records are a link to the records the step wrote.
pub(crate) fn write_step(
    txn: &mut Txn,
    store: &Store,
    prev: Ptr,
    name: &str,
    head: Head,
    drafted: Option<Box<Drafted>>,
) -> Result<(Ptr, Head)> {
    let object = match drafted.and_then(|drafted| drafted.fit(prev, &head)) {
        Some(object) => object,
        None => {
            let mut encoder = Encoder::new(txn);
            encode_step(&mut encoder, prev, name, &head)?;
            encoder.into_object()
        }
    };
    let head = match head.merged.is_empty() {
        true => head,
        false => {
            let decoded =
                Root::decode(&mut Decoder::new(&object.body, &object.links, Some(store)))?;
            let Root::Step(step) = decoded else {
                return Err(Error::Corrupt(
                    "a branch table step decoded as a table".into(),
                ));
            };
            step.head
        }
    };
    let hash = object.hash();
    Ok((txn.put(object, hash)?, head))
}

/// A branch table as [`load`] reads it.
pub(crate) struct Loaded {
    /// Every branch's head by name, the steps applied.
    pub(crate) refs: Refs,
    /// The root of version 0 that the table names, or `None` if it names none.
    pub(crate) genesis: Option<Ptr>,
    /// How many steps the table lies under.
    pub(crate) depth: usize,
}

/// A head whose table is already loaded: its pointer, the table, the root of version 0 it names
/// and its depth.
pub(crate) type Since<'a> = (Ptr, &'a Refs, Option<Ptr>, usize);

struct Layers {
    steps: Vec<Step>,
    base: Refs,
    genesis: Option<Ptr>,
    depth: usize,
}

fn layers(store: &Store, head: Ptr, since: Option<Since<'_>>) -> Result<Layers> {
    let mut steps = Vec::new();
    let mut at = head;
    loop {
        if let Some((known, refs, genesis, depth)) = since
            && known == at
        {
            return Ok(Layers {
                depth: depth + steps.len(),
                steps,
                base: refs.clone(),
                genesis,
            });
        }
        match store.decode_at(at, Root::decode)? {
            Root::Table(base, genesis) => {
                return Ok(Layers {
                    depth: steps.len(),
                    steps,
                    base,
                    genesis,
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

/// The branch table at `head`, with its steps applied, the root of version 0 it names, and how
/// many steps deep it is.
///
/// `since` names a pointer whose table is already loaded, with its depth, so that the walk back
/// stops there and reads only the steps written after it.
///
/// # Errors
///
/// [`Error::Corrupt`] if the steps run back further than any writer stacks them, and read
/// errors.
pub(crate) fn load(store: &Store, head: Option<Ptr>, since: Option<Since<'_>>) -> Result<Loaded> {
    let Some(head) = head else {
        return Ok(Loaded {
            refs: Map::new(),
            genesis: None,
            depth: 0,
        });
    };
    let Layers {
        steps,
        mut base,
        genesis,
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
    Ok(Loaded {
        refs: base,
        genesis,
        depth,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Chain;
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
        let (ptr, _) = write_step(&mut txn, store, prev, name, head(version), None).unwrap();
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
        let Loaded { refs, depth, .. } = load(&store, Some(new), None).unwrap();
        assert_eq!(depth, 4);
        let expected: Vec<_> = [("b0", 20), ("b1", 30), ("b2", 40)]
            .map(|(name, version)| (name.to_string(), version))
            .into();
        assert_eq!(versions(&refs), expected);
        let Loaded {
            refs: older, depth, ..
        } = load(&store, Some(two), None).unwrap();
        assert_eq!(depth, 2);
        let Loaded {
            refs: caught_up,
            depth,
            ..
        } = load(&store, Some(new), Some((two, &older, None, 2))).unwrap();
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
            let Loaded { refs, depth, .. } = load(&store, Some(base), None).unwrap();
            assert_eq!((refs.len(), depth), (branches as usize, 0));
            let top = step(&store, base, "b5", 99);
            let Loaded { refs, depth, .. } = load(&store, Some(top), None).unwrap();
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
        assert_eq!(load(&store, Some(top), None).unwrap().depth, MAX_STEPS);
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
        for (tag, extra) in [(STEP, true), (TABLE - 1, false), (STEP + 1, false)] {
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

    #[test]
    fn a_table_names_version_0_for_the_steps_above_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir);
        let mut refs: Refs = Map::new();
        refs.insert("b0".to_string(), head(1));
        let mut txn = store.begin().unwrap();
        let mut encoder = Encoder::new(&mut txn);
        7u64.encode(&mut encoder).unwrap();
        let genesis = encoder.finish().unwrap();
        let mut encoder = Encoder::new(&mut txn);
        encode_table(&mut encoder, &refs, Some(genesis)).unwrap();
        let base = encoder.finish().unwrap();
        txn.commit(base).unwrap();
        let top = step(&store, base, "b1", 2);
        let loaded = load(&store, Some(top), None).unwrap();
        assert_eq!(
            (loaded.genesis, loaded.depth, loaded.refs.len()),
            (Some(genesis), 1, 2)
        );
        let older = load(&store, Some(base), None).unwrap();
        let since = Some((base, &older.refs, older.genesis, older.depth));
        assert_eq!(
            load(&store, Some(top), since).unwrap().genesis,
            Some(genesis)
        );
        assert_eq!(
            load(&store, Some(table(&store, 1)), None).unwrap().genesis,
            None
        );
        let mut txn = store.begin().unwrap();
        let mut encoder = Encoder::new(&mut txn);
        encode_table(&mut encoder, &refs, Some(genesis)).unwrap();
        encoder.u8(0);
        let trailing = encoder.finish().unwrap();
        txn.commit(trailing).unwrap();
        assert!(load(&store, Some(trailing), None).is_err());
    }

    #[test]
    fn a_drafted_step_is_the_step_encoded_on_its_table() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir);
        let base = table(&store, 2);
        let moved = Ptr {
            hash: base.hash,
            offset: base.offset + 1,
        };
        let linked = |root: Ptr| Head {
            version: 3,
            root: Some(root),
            log: Chain {
                tip: Some(root),
                first: 1,
                last: 3,
            },
            merged: Map::new(),
            born: 7,
        };
        let written = |drafted: Option<Box<Drafted>>| {
            let mut txn = store.begin().unwrap();
            let (ptr, head) =
                write_step(&mut txn, &store, base, "b1", linked(moved), drafted).unwrap();
            txn.commit(ptr).unwrap();
            let Root::Step(step) = store.decode_at(ptr, Root::decode).unwrap() else {
                panic!("a step read back as a table");
            };
            assert_eq!(
                (step.prev, step.head.root, step.head.log),
                (base, Some(moved), head.log)
            );
            ptr.hash
        };
        let fresh = written(None);
        let drafted = Drafted::new("b1", linked(base)).unwrap();
        assert_eq!(written(Some(Box::new(drafted))), fresh);
        let mut other = linked(base);
        other.version = 4;
        other.log.last = 4;
        let unfit = Drafted::new("b1", other).unwrap();
        assert_eq!(written(Some(Box::new(unfit))), fresh);
    }
}
