mod common;

use std::collections::BTreeMap;
use std::path::Path;

use common::{Rng, fast};
use ferro::codec::Hash;
use ferro::{Data, Db, Record, Table};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

#[derive(Clone, Debug, PartialEq)]
struct Line {
    id: u32,
    customer: u8,
    code: Option<u8>,
    tags: Vec<u8>,
    total: i64,
    fee: i64,
    note: String,
}

trait Version: Record<Key = u32> + Sized {
    type Root: Data + Default;
    const LAYOUT: &'static [&'static str];

    fn of(line: &Line) -> Self;
    fn line(&self) -> Line;
    fn table(root: &Self::Root) -> &Table<Self>;
    fn table_mut(root: &mut Self::Root) -> &mut Table<Self>;
    fn hashes(table: &Table<Self>) -> Vec<Option<Hash>>;
    fn dirty(table: &Table<Self>) -> bool;
    fn agree(table: &Table<Self>);
}

const CUSTOMERS: u8 = 3;

macro_rules! version {
    (
        $record:ident, $root:ident, $layout:expr,
        [$(#[$outer:meta])*],
        [$(#[$customer:meta])*],
        [$(#[$code:meta])*],
        [$(#[$tags:meta])*],
        $total:ident [$(#[$total_attr:meta])*],
        $fee:ident [$(#[$fee_attr:meta])*],
        $cover:expr,
        |$t:ident| [$($index:ident),*]
    ) => {
        #[derive(Data, Record, Clone, Debug, PartialEq)]
        $(#[$outer])*
        struct $record {
            #[key]
            id: u32,
            $(#[$customer])*
            customer: u8,
            $(#[$code])*
            code: Option<u8>,
            $(#[$tags])*
            tags: Vec<u8>,
            $(#[$total_attr])*
            $total: i64,
            $(#[$fee_attr])*
            $fee: i64,
            note: String,
        }

        #[derive(Data, Clone, Debug, Default, PartialEq)]
        struct $root {
            rows: Table<$record>,
        }

        impl Version for $record {
            type Root = $root;
            const LAYOUT: &'static [&'static str] = $layout;

            fn of(line: &Line) -> Self {
                $record {
                    id: line.id,
                    customer: line.customer,
                    code: line.code,
                    tags: line.tags.clone(),
                    $total: line.total,
                    $fee: line.fee,
                    note: line.note.clone(),
                }
            }

            fn line(&self) -> Line {
                Line {
                    id: self.id,
                    customer: self.customer,
                    code: self.code,
                    tags: self.tags.clone(),
                    total: self.$total,
                    fee: self.$fee,
                    note: self.note.clone(),
                }
            }

            fn table(root: &$root) -> &Table<Self> {
                &root.rows
            }

            fn table_mut(root: &mut $root) -> &mut Table<Self> {
                &mut root.rows
            }

            fn hashes($t: &Table<Self>) -> Vec<Option<Hash>> {
                vec![$t.rows().root_hash(), $($t.$index.root_hash()),*]
            }

            fn dirty($t: &Table<Self>) -> bool {
                false $(|| $t.$index.is_dirty())*
            }

            fn agree(table: &Table<Self>) {
                for customer in 0..CUSTOMERS {
                    let covered: Vec<String> = table
                        .by_customer(&customer)
                        .covered()
                        .map(|entry| format!("{entry:?}"))
                        .collect();
                    let rows: Vec<String> = table
                        .iter()
                        .filter(|row| row.customer == customer)
                        .map(|row| format!("{:?}", (row.id, ($cover)(&*row))))
                        .collect();
                    assert_eq!(covered, rows, "{} customer {customer}", stringify!($record));
                }
            }
        }
    };
}

version!(
    Plain, PlainRoot, &["by_customer", "unique_code", "by_tags"],
    [],
    [#[index]],
    [#[unique]],
    [#[index(each)]],
    total [],
    fee [],
    |_: &Plain| (),
    |t| [by_customer, unique_code, by_tags]
);

version!(
    Total, TotalRoot, &["by_customer cover(total)", "unique_code", "by_tags"],
    [],
    [#[index(cover(total))]],
    [#[unique]],
    [#[index(each)]],
    total [],
    fee [],
    |row: &Total| row.total,
    |t| [by_customer, unique_code, by_tags]
);

version!(
    TotalNote, TotalNoteRoot, &["by_customer cover(total,note)", "unique_code cover(total)", "by_tags"],
    [],
    [#[index(cover(total, note))]],
    [#[unique(cover(total))]],
    [#[index(each)]],
    total [],
    fee [],
    |row: &TotalNote| (row.total, row.note.clone()),
    |t| [by_customer, unique_code, by_tags]
);

version!(
    NoteTotal, NoteTotalRoot, &["by_customer cover(note,total)", "unique_code cover(total)", "by_tags cover(total)"],
    [],
    [#[index(cover(note, total))]],
    [#[unique(cover(total))]],
    [#[index(each, cover(total))]],
    total [],
    fee [],
    |row: &NoteTotal| (row.note.clone(), row.total),
    |t| [by_customer, unique_code, by_tags]
);

version!(
    Own, OwnRoot, &["by_customer", "unique_code", "by_tags"],
    [],
    [#[index(cover(customer))]],
    [#[unique]],
    [#[index(each)]],
    total [],
    fee [],
    |row: &Own| row.customer,
    |t| [by_customer, unique_code, by_tags]
);

version!(
    OwnTotal, OwnTotalRoot, &["by_customer cover(total)", "unique_code", "by_tags"],
    [],
    [#[index(cover(customer, total))]],
    [#[unique]],
    [#[index(each)]],
    total [],
    fee [],
    |row: &OwnTotal| (row.customer, row.total),
    |t| [by_customer, unique_code, by_tags]
);

version!(
    Renamed, RenamedRoot, &["by_customer cover(total)", "unique_code", "by_tags"],
    [],
    [#[index(cover(amount))]],
    [#[unique]],
    [#[index(each)]],
    amount [#[ferro(rename = "total")]],
    fee [],
    |row: &Renamed| row.amount,
    |t| [by_customer, unique_code, by_tags]
);

version!(
    Swapped, SwappedRoot, &["by_customer cover(fee)", "unique_code", "by_tags"],
    [],
    [#[index(cover(total))]],
    [#[unique]],
    [#[index(each)]],
    fee [#[ferro(rename = "total")]],
    total [#[ferro(rename = "fee")]],
    |row: &Swapped| row.total,
    |t| [by_customer, unique_code, by_tags]
);

version!(
    Fee, FeeRoot, &["by_customer cover(fee)", "unique_code", "by_tags"],
    [],
    [#[index(cover(fee))]],
    [#[unique]],
    [#[index(each)]],
    total [],
    fee [],
    |row: &Fee| row.fee,
    |t| [by_customer, unique_code, by_tags]
);

version!(
    Composite, CompositeRoot, &["by_customer", "unique_code", "by_tags", "by_customer_total cover(note)"],
    [#[index(customer, total, cover(note))]],
    [#[index]],
    [#[unique]],
    [#[index(each)]],
    total [],
    fee [],
    |_: &Composite| (),
    |t| [by_customer, unique_code, by_tags, by_customer_total]
);

version!(
    Wide, WideRoot, &["by_customer cover(total)", "unique_code cover(customer,fee)", "by_tags cover(note,fee)"],
    [],
    [#[index(cover(total))]],
    [#[unique(cover(customer, fee))]],
    [#[index(each, cover(note, fee))]],
    total [],
    fee [],
    |row: &Wide| row.total,
    |t| [by_customer, unique_code, by_tags]
);

const VERSIONS: usize = 11;

#[derive(Clone, Debug)]
enum Edit {
    Customer(u32, u8),
    Code(u32, Option<u8>),
    Tags(u32, Vec<u8>),
    Total(u32, i64),
    Fee(u32, i64),
    Note(u32, &'static str),
    Insert(u32),
    Remove(u32),
}

impl Edit {
    fn id(&self) -> u32 {
        match self {
            Edit::Customer(id, _)
            | Edit::Code(id, _)
            | Edit::Tags(id, _)
            | Edit::Total(id, _)
            | Edit::Fee(id, _)
            | Edit::Note(id, _)
            | Edit::Insert(id)
            | Edit::Remove(id) => *id,
        }
    }
}

fn rebuilt<V: Version>(lines: &BTreeMap<u32, Line>, seed: u64) -> Table<V> {
    let mut rows: Vec<&Line> = lines.values().collect();
    Rng::new(seed).shuffle(&mut rows);
    let mut table: Table<V> = Table::new();
    for line in rows {
        table.insert(V::of(line));
    }
    table
}

fn inspect<V: Version>(table: &Table<V>, lines: &BTreeMap<u32, Line>, seed: u64) {
    let rows: Vec<Line> = table.iter().map(|row| row.line()).collect();
    assert_eq!(rows, lines.values().cloned().collect::<Vec<_>>());
    assert_eq!(V::hashes(table), V::hashes(&rebuilt::<V>(lines, seed)));
    V::agree(table);
}

fn edit(lines: &mut BTreeMap<u32, Line>, edit: &Edit) {
    let free = |lines: &BTreeMap<u32, Line>, id: u32, code: Option<u8>| {
        code.is_none()
            || !lines
                .values()
                .any(|line| line.id != id && line.code == code)
    };
    match edit {
        Edit::Insert(id) if !lines.contains_key(id) => {
            lines.insert(
                *id,
                Line {
                    id: *id,
                    customer: (*id % 3) as u8,
                    code: None,
                    tags: vec![(*id % 2) as u8],
                    total: i64::from(*id) * 10,
                    fee: 1,
                    note: String::new(),
                },
            );
        }
        Edit::Remove(id) if lines.len() > 2 => {
            lines.remove(id);
        }
        Edit::Customer(id, customer) => {
            if let Some(line) = lines.get_mut(id) {
                line.customer = *customer;
            }
        }
        Edit::Code(id, code) => {
            if free(lines, *id, *code)
                && let Some(line) = lines.get_mut(id)
            {
                line.code = *code;
            }
        }
        Edit::Tags(id, tags) => {
            if let Some(line) = lines.get_mut(id) {
                line.tags = tags.clone();
            }
        }
        Edit::Total(id, total) => {
            if let Some(line) = lines.get_mut(id) {
                line.total = *total;
            }
        }
        Edit::Fee(id, fee) => {
            if let Some(line) = lines.get_mut(id) {
                line.fee = *fee;
            }
        }
        Edit::Note(id, note) => {
            if let Some(line) = lines.get_mut(id) {
                line.note = note.to_string();
            }
        }
        Edit::Insert(_) | Edit::Remove(_) => {}
    }
}

fn step<V: Version>(
    path: &Path,
    written: &mut &'static [&'static str],
    lines: &mut BTreeMap<u32, Line>,
    edits: &[Edit],
    seed: u64,
) {
    let db: Db<V::Root> = fast().open(path).unwrap();
    {
        let read = db.read();
        let table = V::table(&read);
        let rebuild = !V::LAYOUT.iter().all(|index| written.contains(index));
        assert_eq!(
            V::dirty(table),
            rebuild,
            "{:?} after {written:?}",
            V::LAYOUT
        );
        inspect(table, lines, seed);
    }
    db.update(|root| {
        let table = V::table_mut(root);
        for change in edits {
            edit(lines, change);
            let id = change.id();
            match lines.get(&id) {
                Some(line) => {
                    table.insert(V::of(line));
                }
                None => {
                    table.remove(&id);
                }
            }
        }
        if let Some(line) = lines.values_mut().next() {
            line.note = format!("step {seed}");
            table.insert(V::of(line));
        }
    })
    .unwrap();
    *written = V::LAYOUT;
    inspect(V::table(&db.read()), lines, seed + 1);
}

fn visit(
    version: usize,
    path: &Path,
    written: &mut &'static [&'static str],
    lines: &mut BTreeMap<u32, Line>,
    edits: &[Edit],
    seed: u64,
) {
    match version {
        0 => step::<Plain>(path, written, lines, edits, seed),
        1 => step::<Total>(path, written, lines, edits, seed),
        2 => step::<TotalNote>(path, written, lines, edits, seed),
        3 => step::<NoteTotal>(path, written, lines, edits, seed),
        4 => step::<Own>(path, written, lines, edits, seed),
        5 => step::<OwnTotal>(path, written, lines, edits, seed),
        6 => step::<Renamed>(path, written, lines, edits, seed),
        7 => step::<Swapped>(path, written, lines, edits, seed),
        8 => step::<Fee>(path, written, lines, edits, seed),
        9 => step::<Composite>(path, written, lines, edits, seed),
        _ => step::<Wide>(path, written, lines, edits, seed),
    }
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    let id = || 0..12u32;
    prop_oneof![
        (id(), 0..CUSTOMERS).prop_map(|(id, c)| Edit::Customer(id, c)),
        (id(), prop::option::of(0..6u8)).prop_map(|(id, c)| Edit::Code(id, c)),
        (id(), prop::collection::vec(0..3u8, 0..3)).prop_map(|(id, t)| Edit::Tags(id, t)),
        (id(), -3..300i64).prop_map(|(id, t)| Edit::Total(id, t)),
        (id(), -3..300i64).prop_map(|(id, f)| Edit::Fee(id, f)),
        (id(), prop::sample::select(vec!["", "n", "a note"])).prop_map(|(id, n)| Edit::Note(id, n)),
        id().prop_map(Edit::Insert),
        id().prop_map(Edit::Remove),
    ]
}

fn first_rows() -> BTreeMap<u32, Line> {
    (0..10u32)
        .map(|id| {
            (
                id,
                Line {
                    id,
                    customer: (id % 3) as u8,
                    code: (id % 4 != 0).then_some(id as u8),
                    tags: vec![(id % 3) as u8, 2],
                    total: i64::from(id) * 11,
                    fee: i64::from(id % 4),
                    note: format!("n{}", id % 2),
                },
            )
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        rng_seed: RngSeed::Fixed(0xC0DE_0004),
        ..ProptestConfig::with_cases(common::cases(256))
    })]

    #[test]
    fn every_index_equals_a_rebuild_whichever_version_opens_the_file(
        visits in prop::collection::vec(
            (0..VERSIONS, prop::collection::vec(edit_strategy(), 0..5)),
            1..14,
        ),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schema.ferro");
        let mut lines = first_rows();
        {
            let db: Db<PlainRoot> = fast().open(&path).unwrap();
            db.update(|root| {
                for line in lines.values() {
                    root.rows.insert(Plain::of(line));
                }
            })
            .unwrap();
        }
        let mut written = Plain::LAYOUT;
        for (at, (version, edits)) in visits.iter().enumerate() {
            visit(*version, &path, &mut written, &mut lines, edits, at as u64 + 1);
        }
        visit(0, &path, &mut written, &mut lines, &[], 99);
    }
}

fn fill_named(path: &Path) -> BTreeMap<u32, Line> {
    let lines = first_rows();
    let db: Db<TotalRoot> = fast().open(path).unwrap();
    db.update(|root| {
        for line in lines.values() {
            root.rows.insert(Total::of(line));
        }
    })
    .unwrap();
    lines
}

#[test]
fn renaming_a_covered_field_in_rust_keeps_its_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("renamed.ferro");
    let lines = fill_named(&path);
    let db: Db<RenamedRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(!read.rows.by_customer.is_dirty());
    inspect(&read.rows, &lines, 1);
}

#[test]
fn swapping_the_names_covered_fields_are_stored_under_rebuilds_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swapped.ferro");
    let lines = fill_named(&path);
    let db: Db<SwappedRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(read.rows.by_customer.is_dirty());
    inspect(&read.rows, &lines, 2);
    let fees: Vec<(u32, i64)> = read.rows.by_customer(&1).covered().collect();
    let wanted: Vec<(u32, i64)> = lines
        .values()
        .filter(|line| line.customer == 1)
        .map(|line| (line.id, line.fee))
        .collect();
    assert_eq!(fees, wanted);
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Retyped {
    #[key]
    id: u32,
    #[index(cover(total))]
    customer: u8,
    #[ferro(rename = "total_text", default)]
    total: String,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct RetypedRoot {
    rows: Table<Retyped>,
}

#[test]
fn storing_a_covered_field_under_a_new_name_and_type_rebuilds_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retyped.ferro");
    fill_named(&path);
    let db: Db<RetypedRoot> = fast().open(&path).unwrap();
    db.update(|root| {
        root.rows.update(&4, |row| row.total = "forty".into());
    })
    .unwrap();
    let read = db.read();
    let covered: Vec<(u32, String)> = read.rows.by_customer(&1).covered().collect();
    assert_eq!(
        covered,
        vec![
            (1, String::new()),
            (4, "forty".to_string()),
            (7, String::new())
        ]
    );
}
