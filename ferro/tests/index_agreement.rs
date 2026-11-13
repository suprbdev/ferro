mod common;
mod shadow;

use std::collections::BTreeSet;
use std::path::Path;

use common::{Rng, fast};
use ferro::codec::{Decoder, Encoder, field_tag};
use ferro::{Data, Db, Error, Keep, Map, Outcome, Record, Snapshot, Table};

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(group, rank)]
#[unique(slug, group, cover(rank))]
struct Every {
    #[key]
    id: u32,
    #[index(cover(rank, note))]
    group: u8,
    #[unique]
    code: Option<u16>,
    #[index(each)]
    tags: Vec<String>,
    #[index(each, cover(rank))]
    marks: BTreeSet<u8>,
    #[unique(cover(note))]
    slug: String,
    #[index]
    due: Option<u32>,
    rank: i64,
    note: String,
}

#[derive(Data, Clone, Default)]
struct Shelf {
    items: Table<Every>,
}

#[derive(Clone, Default)]
struct Spliced {
    rows: Table<Every>,
    indexes: Table<Every>,
}

impl Data for Spliced {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        let whole = {
            let mut whole = Encoder::new(w.sink());
            self.indexes.encode(&mut whole)?;
            whole.into_object()
        };
        let mut fields = Vec::new();
        Decoder::new(&whole.body, &whole.links).fields_sharing_tags(|tag, r| {
            let bytes = r.take(r.remaining())?.to_vec();
            let mut links = Vec::new();
            while let Ok(link) = r.link() {
                links.push(link);
            }
            fields.push((tag, bytes, links));
            Ok(())
        })?;
        w.mark_unsettled();
        w.varint(fields.len() as u64);
        for (tag, bytes, links) in fields {
            w.field(tag, |w| {
                if tag == field_tag("rows") {
                    return self.rows.rows().encode(w);
                }
                w.raw(&bytes);
                for link in links {
                    w.link(link);
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        let table: Table<Every> = Table::decode(r)?;
        Ok(Spliced {
            rows: table.clone(),
            indexes: table,
        })
    }
}

#[derive(Data, Clone, Default)]
struct SplicedShelf {
    items: Spliced,
}

fn every(id: u32, version: u32, rng: &mut Rng) -> Every {
    let tags = (0..rng.below(4))
        .map(|_| format!("t{}", rng.below(9)))
        .collect();
    let marks = (0..rng.below(4)).map(|_| rng.below(12) as u8).collect();
    Every {
        id,
        group: rng.below(6) as u8,
        code: rng.chance(70).then_some(id as u16),
        tags,
        marks,
        slug: format!("s{id}-{version}"),
        due: rng.chance(50).then(|| rng.below(20) as u32),
        rank: rng.below(1000) as i64 - 500,
        note: format!("n{}", rng.below(5)),
    }
}

fn unmerged<T>(_: &mut T, _: &Snapshot<T>, _: &Snapshot<T>) -> (String, ()) {
    panic!("no merge expected")
}

#[test]
fn every_kind_of_index_is_held_to_the_rows_it_was_built_from() {
    let dir = tempfile::tempdir().unwrap();
    let (path, other) = (dir.path().join("a.ferro"), dir.path().join("b.ferro"));
    let mut rng = Rng::new(112);
    {
        let a: Db<Shelf> = fast().open(&path).unwrap();
        let b: Db<Shelf> = fast().open(&other).unwrap();
        for version in 1..=6u32 {
            a.update(|shelf| {
                for _ in 0..400 {
                    let id = rng.below(900) as u32;
                    match rng.below(4) {
                        0 => {
                            shelf.items.remove(&id);
                        }
                        _ => {
                            shelf.items.insert(every(id, version, &mut rng));
                        }
                    }
                }
            })
            .unwrap();
            if version % 2 == 0 {
                assert_eq!(
                    b.pull(&a, "a", unmerged).unwrap().outcome,
                    Outcome::FastForward
                );
            }
        }
        a.verify_typed().unwrap();
        b.verify_typed().unwrap();
    }
    {
        let a: Db<SplicedShelf> = fast().open(&path).unwrap();
        let older = a.at(3).unwrap().items.indexes.clone();
        a.update(|shelf| {
            shelf.items.indexes = older;
        })
        .unwrap();
    }
    let a: Db<Shelf> = fast().open(&path).unwrap();
    let b: Db<Shelf> = fast().open(&other).unwrap();
    corrupt(&a, "rows of the head under the indexes of v3");
    match b.pull(&a, "a", unmerged) {
        Err(Error::Bundle(message)) => {
            assert!(message.contains("do not agree with its rows"), "{message}")
        }
        other => panic!("a spliced table landed: {other:?}"),
    }
    b.verify_typed().unwrap();
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Tagged {
    #[key]
    id: u32,
    #[index(each)]
    tags: ferro::Set<u16>,
    #[index]
    group: u8,
}

#[derive(Data, Clone, Default)]
struct Tags {
    items: Table<Tagged>,
}

#[test]
fn a_table_indexing_each_element_of_a_set_is_held_to_its_rows_after_the_walk() {
    use ferro::TableData;
    assert!(<TaggedTable as TableData<Tagged>>::indexing_reads());
    assert!(!<EveryTable as TableData<Every>>::indexing_reads());
    let dir = tempfile::tempdir().unwrap();
    let a: Db<Tags> = fast().open(dir.path().join("a.ferro")).unwrap();
    let b: Db<Tags> = fast().open(dir.path().join("b.ferro")).unwrap();
    let mut rng = Rng::new(113);
    for version in 1..=4u32 {
        a.update(|shelf| {
            for _ in 0..300 {
                let id = rng.below(500) as u32;
                let tags = (0..rng.below(5)).map(|_| rng.below(30) as u16).collect();
                let group = (rng.below(4) as u32 + version) as u8;
                shelf.items.insert(Tagged { id, tags, group });
            }
        })
        .unwrap();
        if version != 2 {
            assert_eq!(
                b.pull(&a, "a", unmerged).unwrap().outcome,
                Outcome::FastForward
            );
        }
    }
    b.verify_typed().unwrap();
    let (mine, theirs) = (b.read(), a.read());
    for tag in 0..30u16 {
        let keys: Vec<u32> = mine.items.by_tags(&tag).keys().collect();
        let expected: Vec<u32> = theirs
            .items
            .iter()
            .filter(|row| row.tags.contains(&tag))
            .map(|row| row.id)
            .collect();
        assert_eq!(keys, expected, "tag {tag}");
    }
}

fn corrupt<T: ferro::Data>(db: &Db<T>, case: &str) {
    db.verify().unwrap();
    match db.verify_typed() {
        Err(Error::Corrupt(message)) => assert!(
            message.contains("do not agree with its rows"),
            "{case}: {message}"
        ),
        other => panic!("{case}: verify_typed passed a table whose indexes disagree: {other:?}"),
    }
}

fn written(path: &Path, versions: Vec<shadow::Shadow>) {
    let peer: Db<shadow::Peer> = fast().open(path).unwrap();
    for table in versions {
        peer.update(|root| root.rows = table).unwrap();
    }
}

#[test]
fn reading_a_table_that_stores_no_index_fails_when_a_row_sits_under_another_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rows.ferro");
    {
        let peer: Db<shadow::UnindexedPeer> = fast().open(&path).unwrap();
        peer.update(|root| {
            root.rows.rows.insert(1, shadow::row(7, 5, 100));
            root.rows.rows.insert(7, shadow::row(7, 5, 100));
            root.rows.rows.insert(3, shadow::row(4, 9, 300));
        })
        .unwrap();
    }
    let failed = match fast().open::<shadow::Mine>(&path) {
        Err(error) => error,
        Ok(mine) => match mine.at(1) {
            Err(error) => error,
            Ok(read) => panic!(
                "a table with a row under another key read as {:?}",
                read.rows.by_email(&100)
            ),
        },
    };
    match failed {
        Error::Decode(message) => assert!(message.contains("other than its own"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn verify_typed_reports_a_table_without_indexes_whose_row_sits_under_another_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rows.ferro");
    {
        let peer: Db<shadow::UnindexedPeer> = fast().open(&path).unwrap();
        peer.update(|root| {
            root.rows.rows.insert(1, shadow::row(7, 5, 100));
            root.rows.rows.insert(7, shadow::row(7, 5, 100));
        })
        .unwrap();
    }
    let db: Db<shadow::MinePlain> = fast().open(&path).unwrap();
    assert_eq!(db.read().rows.get(&1).map(|row| row.id), Some(7));
    db.verify().unwrap();
    match db.verify_typed() {
        Err(Error::Corrupt(message)) => {
            assert!(message.contains("other than its own"), "{message}")
        }
        other => panic!("verify_typed passed a row under another key: {other:?}"),
    }
}

#[test]
fn verify_typed_passes_versions_of_a_table_that_hold_an_index_its_type_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dropped.ferro");
    written(&path, vec![shadow::Shadow::of(&shadow::honest())]);
    let db: Db<shadow::MinePlain> = fast().open(&path).unwrap();
    db.verify_typed().unwrap();
    db.update(|root| {
        root.rows.insert(shadow::plain(9, 1, 900));
    })
    .unwrap();
    db.verify_typed().unwrap();
    assert_eq!(db.read().rows.len(), 4);
}

#[test]
fn verify_typed_reports_a_table_whose_indexes_disagree_with_its_rows() {
    for (case, forged) in shadow::forgeries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forged.ferro");
        written(&path, vec![forged]);
        let db: Db<shadow::Mine> = fast().open(&path).unwrap();
        corrupt(&db, case);
    }
}

#[test]
fn verify_typed_reports_a_disagreeing_table_in_a_version_behind_the_head() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.ferro");
    let rows: Vec<shadow::Row> = (0..500)
        .map(|id| shadow::row(id, (id % 11) as u32, id * 3))
        .collect();
    let first = shadow::Shadow::of(&rows);
    let mut edited = first.clone();
    edited.put(shadow::row(7, 70, 7000));
    edited.remove(8);
    let mut drifted = edited.clone();
    drifted.rows.update(&9, |row| row.n = 90);
    let mut mended = drifted.clone();
    mended.by_n.remove(&(9, 9));
    mended.by_n.insert((90, 9), ());
    mended.put(shadow::row(600, 1, 1));
    written(&path, vec![first, edited, drifted, mended]);
    let db: Db<shadow::Mine> = fast().open(&path).unwrap();
    corrupt(&db, "a drifted version");
    let read = db.read();
    let nines: Vec<u64> = read.rows.by_n(&90).keys().collect();
    assert_eq!(nines, [9]);
    drop(read);
    db.compact(Keep::Last(1)).unwrap();
    db.verify_typed().unwrap();
    db.update(|root| {
        root.rows.insert(shadow::row(601, 2, 2));
    })
    .unwrap();
    db.verify_typed().unwrap();
}

#[test]
fn verify_typed_reports_a_disagreeing_table_nested_in_a_map() {
    for (case, forged) in shadow::forgeries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested.ferro");
        {
            let peer: Db<Map<u64, shadow::Shadow>> = fast().open(&path).unwrap();
            peer.update(|root| {
                for key in 0..30 {
                    root.insert(key, shadow::Shadow::of(&shadow::honest()));
                }
            })
            .unwrap();
            peer.update(|root| {
                root.insert(15, forged);
            })
            .unwrap();
        }
        let db: Db<Map<u64, Table<shadow::Row>>> = fast().open(&path).unwrap();
        corrupt(&db, case);
    }
}
