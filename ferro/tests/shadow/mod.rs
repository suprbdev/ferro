#![allow(dead_code)]

use ferro::codec::{Decoder, Encoder, field_tag};
use ferro::{Data, Map, Record, Table};

#[derive(Data, Record, Clone, Debug, PartialEq)]
pub struct Row {
    #[key]
    pub id: u64,
    #[index]
    pub n: u32,
    #[unique]
    pub email: u64,
}

pub fn row(id: u64, n: u32, email: u64) -> Row {
    Row { id, n, email }
}

#[derive(Data, Clone, Default)]
pub struct Mine {
    pub rows: Table<Row>,
}

#[derive(Data, Clone, Default)]
pub struct Peer {
    pub rows: Shadow,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
pub struct Narrow {
    #[key]
    pub id: u64,
    #[index]
    pub n: u32,
    pub email: u64,
}

#[derive(Data, Clone, Default)]
pub struct MineNarrow {
    pub rows: Table<Narrow>,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
pub struct Plain {
    #[key]
    pub id: u64,
    pub n: u32,
    pub email: u64,
}

pub fn plain(id: u64, n: u32, email: u64) -> Plain {
    Plain { id, n, email }
}

#[derive(Data, Clone, Default)]
pub struct MinePlain {
    pub rows: Table<Plain>,
}

#[derive(Clone, Default)]
pub struct Unindexed {
    pub rows: Map<u64, Row>,
}

impl Data for Unindexed {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.varint(1);
        w.field(field_tag("rows"), |w| self.rows.encode(w))
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        let mut table = Unindexed::default();
        r.fields(|tag, r| {
            if tag == field_tag("rows") {
                table.rows = Map::decode(r)?;
            }
            Ok(())
        })?;
        Ok(table)
    }
}

#[derive(Data, Clone, Default)]
pub struct UnindexedPeer {
    pub rows: Unindexed,
}

#[derive(Clone, Default)]
pub struct Shadow {
    pub rows: Map<u64, Row>,
    pub by_n: Map<(u32, u64), ()>,
    pub by_email: Map<u64, u64>,
}

impl Shadow {
    pub fn of(rows: &[Row]) -> Shadow {
        let mut shadow = Shadow::default();
        for row in rows {
            shadow.put(row.clone());
        }
        shadow
    }

    pub fn put(&mut self, row: Row) {
        self.remove(row.id);
        self.by_n.insert((row.n, row.id), ());
        self.by_email.insert(row.email, row.id);
        self.rows.insert(row.id, row);
    }

    pub fn remove(&mut self, id: u64) {
        if let Some(row) = self.rows.remove(&id) {
            self.by_n.remove(&(row.n, row.id));
            self.by_email.remove(&row.email);
        }
    }
}

fn fingerprints() -> [u64; 2] {
    let prints = RowTable::__fingerprints();
    [prints.get(0), prints.get(1)]
}

impl Data for Shadow {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        let prints = fingerprints();
        let mut order = [0, 1];
        order.sort_by_key(|slot| prints[*slot]);
        let indexes = |w: &mut Encoder<'_>| -> ferro::Result<()> {
            for slot in order {
                w.field(field_tag("indexes"), |w| {
                    w.raw(&prints[slot].to_le_bytes());
                    match slot {
                        0 => self.by_n.encode(w),
                        _ => self.by_email.encode(w),
                    }
                })?;
            }
            Ok(())
        };
        let rows = |w: &mut Encoder<'_>| w.field(field_tag("rows"), |w| self.rows.encode(w));
        w.varint(3);
        if field_tag("rows") < field_tag("indexes") {
            rows(w)?;
            indexes(w)
        } else {
            indexes(w)?;
            rows(w)
        }
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        let prints = fingerprints();
        let mut shadow = Shadow::default();
        r.fields(|tag, r| {
            if tag == field_tag("rows") {
                shadow.rows = Map::decode(r)?;
            } else if tag == field_tag("indexes") {
                let print = u64::from_le_bytes(r.take(8)?.try_into().unwrap());
                if print == prints[0] {
                    shadow.by_n = Map::decode(r)?;
                } else if print == prints[1] {
                    shadow.by_email = Map::decode(r)?;
                }
            }
            Ok(())
        })?;
        Ok(shadow)
    }
}

pub fn honest() -> Vec<Row> {
    vec![row(1, 5, 100), row(2, 6, 200), row(3, 7, 400)]
}

pub fn forgeries() -> Vec<(&'static str, Shadow)> {
    let mut extra = Shadow::of(&honest());
    extra.by_n.insert((8, 77), ());
    let mut stale = Shadow::of(&honest());
    stale.by_n.remove(&(6, 2));
    stale.by_n.insert((9, 2), ());
    let mut missing = Shadow::of(&honest());
    missing.by_email.remove(&400);
    let mut shared = Shadow::of(&[row(1, 5, 100), row(3, 7, 400)]);
    shared.rows.insert(2, row(2, 6, 100));
    shared.by_n.insert((6, 2), ());
    let mut misplaced = Shadow::of(&honest());
    misplaced.rows.insert(5, row(6, 9, 600));
    misplaced.by_n.insert((9, 6), ());
    misplaced.by_email.insert(600, 6);
    vec![
        ("an entry for no row", extra),
        ("an entry under another value", stale),
        ("a row without its entry", missing),
        ("two rows sharing a unique value", shared),
        ("a row under another key", misplaced),
    ]
}

pub fn encodes_as_the_table(rows: &[Row]) -> bool {
    let mut table: Table<Row> = Table::new();
    for row in rows {
        table.insert(row.clone());
    }
    ferro::hash_of(&table) == ferro::hash_of(&Shadow::of(rows))
}
