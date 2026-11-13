mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

use common::fast;
use ferro::codec::{Decoder, Description, Encoder, field_tag};
use ferro::{Data, Db, Map, Record, Table};
use tempfile::TempDir;

macro_rules! root {
    ($root:ident, $record:ident) => {
        #[derive(Data, Clone, Debug, Default, PartialEq)]
        struct $root {
            rows: Table<$record>,
        }
    };
}

fn scratch(name: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    (dir, path)
}

fn rebuilt<T: Record + Clone>(table: &Table<T>) -> Table<T> {
    let mut fresh: Table<T> = Table::new();
    for row in table.iter() {
        fresh.insert((*row).clone());
    }
    fresh
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct CoverTotal {
    #[key]
    id: u32,
    #[index(cover(total))]
    customer: i64,
    total: i64,
    price: i64,
    speed: i64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct CoverPriceSpeed {
    #[key]
    id: u32,
    #[index(cover(price, speed))]
    customer: i64,
    total: i64,
    price: i64,
    speed: i64,
}

root!(CoverTotalRoot, CoverTotal);
root!(CoverPriceSpeedRoot, CoverPriceSpeed);

#[test]
fn a_cover_list_whose_old_tag_matched_is_rebuilt_with_its_new_fields() {
    assert_eq!(
        field_tag("by_customer cover(total)"),
        field_tag("by_customer cover(price,speed)")
    );
    let (_dir, path) = scratch("price.ferro");
    {
        let db: Db<CoverTotalRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..12u32 {
                let n = i64::from(id);
                root.rows.insert(CoverTotal {
                    id,
                    customer: n % 3,
                    total: n * 10,
                    price: n * 100,
                    speed: n + 7,
                });
            }
        })
        .unwrap();
    }
    let db: Db<CoverPriceSpeedRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(read.rows.by_customer.is_dirty());
    let covered: Vec<(u32, (i64, i64))> = read.rows.by_customer(&1).covered().collect();
    assert_eq!(
        covered,
        vec![
            (1, (100, 8)),
            (4, (400, 11)),
            (7, (700, 14)),
            (10, (1000, 17))
        ]
    );
    assert_eq!(
        read.rows.by_customer.root_hash(),
        rebuilt(&read.rows).by_customer.root_hash()
    );
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct CoverSize {
    #[key]
    id: u32,
    #[index(cover(size_id))]
    owner: i64,
    size_id: i64,
    length_v2: i64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct CoverLength {
    #[key]
    id: u32,
    #[index(cover(length_v2))]
    owner: i64,
    size_id: i64,
    length_v2: i64,
}

root!(CoverSizeRoot, CoverSize);
root!(CoverLengthRoot, CoverLength);

#[test]
fn a_same_shaped_cover_list_whose_old_tag_matched_reads_the_new_field() {
    assert_eq!(
        field_tag("by_owner cover(size_id)"),
        field_tag("by_owner cover(length_v2)")
    );
    let (_dir, path) = scratch("length.ferro");
    {
        let db: Db<CoverSizeRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..9u32 {
                let n = i64::from(id);
                root.rows.insert(CoverSize {
                    id,
                    owner: n % 2,
                    size_id: -n,
                    length_v2: n * 1000,
                });
            }
        })
        .unwrap();
    }
    let db: Db<CoverLengthRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(read.rows.by_owner.is_dirty());
    let covered: Vec<(u32, i64)> = read.rows.by_owner(&1).covered().collect();
    assert_eq!(covered, vec![(1, 1000), (3, 3000), (5, 5000), (7, 7000)]);
}

macro_rules! tagged {
    ($record:ident, $root:ident, [$($attr:tt)*]) => {
        #[derive(Data, Record, Clone, Debug, PartialEq)]
        struct $record {
            #[key]
            id: u32,
            $($attr)*
            tags: Vec<String>,
            note: String,
        }

        root!($root, $record);

        impl From<u32> for $record {
            fn from(id: u32) -> Self {
                let color = if id % 2 == 0 { "red" } else { "blue" };
                $record {
                    id,
                    tags: vec![color.to_string(), format!("size{}", id % 3)],
                    note: format!("n{id}"),
                }
            }
        }
    };
}

tagged!(TagsValue, TagsValueRoot, [#[index]]);
tagged!(TagsEach, TagsEachRoot, [#[index(each)]]);
tagged!(TagsValueNote, TagsValueNoteRoot, [#[index(cover(note))]]);
tagged!(TagsEachNote, TagsEachNoteRoot, [#[index(each, cover(note))]]);

fn fill_tags<R: Data + Default, T: Record + From<u32>>(
    path: &Path,
    table: fn(&mut R) -> &mut Table<T>,
) {
    let db: Db<R> = fast().open(path).unwrap();
    db.update(|root| {
        for id in 0..10 {
            table(root).insert(T::from(id));
        }
    })
    .unwrap();
}

#[test]
fn switching_an_index_to_each_and_back_rebuilds_it() {
    let (_dir, path) = scratch("tags.ferro");
    fill_tags::<TagsValueRoot, TagsValue>(&path, |root| &mut root.rows);
    {
        let db: Db<TagsEachRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert!(read.rows.by_tags.is_dirty());
        let red: Vec<u32> = read.rows.by_tags("red").map(|row| row.id).collect();
        assert_eq!(red, vec![0, 2, 4, 6, 8]);
        let small: Vec<u32> = read.rows.by_tags("size1").keys().collect();
        assert_eq!(small, vec![1, 4, 7]);
        drop(read);
        db.update(|root| {
            root.rows.update(&3, |row| row.tags = vec!["green".into()]);
        })
        .unwrap();
    }
    {
        let db: Db<TagsEachRoot> = fast().open(&path).unwrap();
        assert!(!db.read().rows.by_tags.is_dirty());
    }
    let db: Db<TagsValueRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(read.rows.by_tags.is_dirty());
    let whole = vec!["red".to_string(), "size0".to_string()];
    let ids: Vec<u32> = read.rows.by_tags(&whole).keys().collect();
    assert_eq!(ids, vec![0, 6]);
    let green: Vec<u32> = read
        .rows
        .by_tags(&vec!["green".to_string()])
        .keys()
        .collect();
    assert_eq!(green, vec![3]);
    assert_eq!(
        read.rows.by_tags.root_hash(),
        rebuilt(&read.rows).by_tags.root_hash()
    );
}

#[test]
fn switching_a_covered_index_to_each_rebuilds_it() {
    let (_dir, path) = scratch("tags-note.ferro");
    fill_tags::<TagsValueNoteRoot, TagsValueNote>(&path, |root| &mut root.rows);
    {
        let db: Db<TagsEachNoteRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert!(read.rows.by_tags.is_dirty());
        let red: Vec<(u32, String)> = read.rows.by_tags("red").covered().collect();
        assert_eq!(
            red,
            [0, 2, 4, 6, 8].map(|id| (id, format!("n{id}"))).to_vec()
        );
        drop(read);
        db.update(|root| {
            root.rows.update(&2, |row| row.note = "moved".into());
        })
        .unwrap();
    }
    let db: Db<TagsValueNoteRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(read.rows.by_tags.is_dirty());
    let whole = vec!["red".to_string(), "size2".to_string()];
    let covered: Vec<(u32, String)> = read.rows.by_tags(&whole).covered().collect();
    assert_eq!(
        covered,
        vec![(2, "moved".to_string()), (8, "n8".to_string())]
    );
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Totals {
    #[key]
    id: u32,
    #[index]
    total: i64,
    fee: i64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct SwappedTotals {
    #[key]
    id: u32,
    #[index]
    #[ferro(rename = "fee")]
    total: i64,
    #[ferro(rename = "total")]
    fee: i64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct RetypedTotals {
    #[key]
    id: u32,
    #[index]
    #[ferro(rename = "total_v2", default)]
    total: String,
    fee: i64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct RenamedTotals {
    #[key]
    id: u32,
    #[index]
    #[ferro(rename = "total")]
    amount: i64,
    fee: i64,
}

root!(TotalsRoot, Totals);
root!(SwappedTotalsRoot, SwappedTotals);
root!(RetypedTotalsRoot, RetypedTotals);
root!(RenamedTotalsRoot, RenamedTotals);

fn fill_totals(path: &Path) {
    let db: Db<TotalsRoot> = fast().open(path).unwrap();
    db.update(|root| {
        for id in 0..5u32 {
            root.rows.insert(Totals {
                id,
                total: i64::from(id) * 100,
                fee: i64::from(id),
            });
        }
    })
    .unwrap();
}

#[test]
fn swapping_the_stored_names_of_an_indexed_field_rebuilds_its_index() {
    let (_dir, path) = scratch("swap.ferro");
    fill_totals(&path);
    let db: Db<SwappedTotalsRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(read.rows.by_total.is_dirty());
    assert_eq!(read.rows.by_total(&300).count(), 0);
    let three: Vec<SwappedTotals> = read.rows.by_total(&3).map(|row| (*row).clone()).collect();
    assert_eq!(
        three,
        vec![SwappedTotals {
            id: 3,
            total: 3,
            fee: 300
        }]
    );
    assert_eq!(
        read.rows.by_total.root_hash(),
        rebuilt(&read.rows).by_total.root_hash()
    );
}

#[test]
fn giving_an_indexed_field_a_new_stored_name_and_type_rebuilds_its_index() {
    let (_dir, path) = scratch("retype.ferro");
    fill_totals(&path);
    let db: Db<RetypedTotalsRoot> = fast().open(&path).unwrap();
    db.update(|root| {
        root.rows
            .update(&4, |row| row.total = "four hundred".into());
    })
    .unwrap();
    let read = db.read();
    let blank: Vec<u32> = read.rows.by_total("").keys().collect();
    assert_eq!(blank, vec![0, 1, 2, 3]);
    let four: Vec<u32> = read.rows.by_total("four hundred").keys().collect();
    assert_eq!(four, vec![4]);
}

#[test]
fn renaming_an_indexed_field_in_rust_keeps_its_index() {
    let (_dir, path) = scratch("rust-rename.ferro");
    fill_totals(&path);
    let db: Db<RenamedTotalsRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(!read.rows.by_amount.is_dirty());
    let ids: Vec<u32> = read.rows.by_amount(&300).keys().collect();
    assert_eq!(ids, vec![3]);
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(customer, total)]
struct CompositeTotals {
    #[key]
    id: u32,
    customer: u8,
    total: i64,
    fee: i64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(customer, total)]
struct CompositeSwapped {
    #[key]
    id: u32,
    customer: u8,
    #[ferro(rename = "fee")]
    total: i64,
    #[ferro(rename = "total")]
    fee: i64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(client, amount)]
struct CompositeRenamed {
    #[key]
    id: u32,
    #[ferro(rename = "customer")]
    client: u8,
    #[ferro(rename = "total")]
    amount: i64,
    fee: i64,
}

root!(CompositeTotalsRoot, CompositeTotals);
root!(CompositeSwappedRoot, CompositeSwapped);
root!(CompositeRenamedRoot, CompositeRenamed);

#[test]
fn a_composite_index_follows_the_stored_names_of_its_fields() {
    let (_dir, path) = scratch("composite.ferro");
    {
        let db: Db<CompositeTotalsRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..6u32 {
                root.rows.insert(CompositeTotals {
                    id,
                    customer: (id % 2) as u8,
                    total: i64::from(id) * 100,
                    fee: i64::from(id),
                });
            }
        })
        .unwrap();
    }
    {
        let db: Db<CompositeRenamedRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert!(!read.rows.by_client_amount.is_dirty());
        let ids: Vec<u32> = read.rows.by_client_amount(&(1, 300)).keys().collect();
        assert_eq!(ids, vec![3]);
    }
    let db: Db<CompositeSwappedRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(read.rows.by_customer_total.is_dirty());
    assert_eq!(read.rows.by_customer_total(&(1, 300)).count(), 0);
    let ids: Vec<u32> = read.rows.by_customer_total(&(1, 3)).keys().collect();
    assert_eq!(ids, vec![3]);
    let prefix: Vec<u32> = read.rows.by_customer_total_prefix(&0).keys().collect();
    assert_eq!(prefix, vec![0, 2, 4]);
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Pair {
    #[key]
    id: u32,
    #[index(cover(total))]
    customer: u8,
    #[unique(cover(fee))]
    slug: String,
    #[index]
    fee: i64,
    total: i64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct PairFee {
    #[key]
    id: u32,
    #[index(cover(fee))]
    customer: u8,
    #[unique(cover(fee))]
    slug: String,
    #[index]
    fee: i64,
    total: i64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct PairBare {
    #[key]
    id: u32,
    #[index(cover(total))]
    customer: u8,
    #[unique]
    slug: String,
    #[index]
    fee: i64,
    total: i64,
}

root!(PairRoot, Pair);
root!(PairFeeRoot, PairFee);
root!(PairBareRoot, PairBare);

#[test]
fn only_the_index_whose_layout_changed_is_rebuilt() {
    let (_dir, path) = scratch("pair.ferro");
    {
        let db: Db<PairRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..8u32 {
                root.rows.insert(Pair {
                    id,
                    customer: (id % 3) as u8,
                    slug: format!("s{id}"),
                    fee: i64::from(id % 4),
                    total: i64::from(id) * 7,
                });
            }
        })
        .unwrap();
    }
    {
        let db: Db<PairFeeRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert!(read.rows.by_customer.is_dirty());
        assert!(!read.rows.unique_slug.is_dirty());
        assert!(!read.rows.by_fee.is_dirty());
        let fees: Vec<(u32, i64)> = read.rows.by_customer(&2).covered().collect();
        assert_eq!(fees, vec![(2, 2), (5, 1)]);
        drop(read);
        db.update(|root| {
            root.rows.update(&5, |row| row.fee = 9);
        })
        .unwrap();
    }
    let db: Db<PairBareRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(read.rows.by_customer.is_dirty());
    assert!(read.rows.unique_slug.is_dirty());
    assert!(!read.rows.by_fee.is_dirty());
    let fresh = rebuilt(&read.rows);
    assert_eq!(
        read.rows.by_customer.root_hash(),
        fresh.by_customer.root_hash()
    );
    assert_eq!(
        read.rows.unique_slug.root_hash(),
        fresh.unique_slug.root_hash()
    );
    assert_eq!(read.rows.by_fee.root_hash(), fresh.by_fee.root_hash());
    let nine: Vec<u32> = read.rows.by_fee(&9).keys().collect();
    assert_eq!(nine, vec![5]);
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(customer, total)]
#[unique(slug, customer)]
struct Order {
    #[key]
    id: u32,
    #[index(cover(total, note))]
    customer: u8,
    #[unique]
    code: Option<u8>,
    #[index(each)]
    tags: Vec<String>,
    #[unique(cover(total))]
    slug: String,
    #[index]
    due: Option<u32>,
    total: i64,
    note: String,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Shop {
    orders: Table<Order>,
}

fn order_hashes(table: &Table<Order>) -> Vec<Option<ferro::Hash>> {
    vec![
        table.by_customer.root_hash(),
        table.unique_code.root_hash(),
        table.by_tags.root_hash(),
        table.unique_slug.root_hash(),
        table.by_due.root_hash(),
        table.by_customer_total.root_hash(),
        table.unique_slug_customer.root_hash(),
    ]
}

fn order_dirty(table: &Table<Order>) -> Vec<bool> {
    vec![
        table.by_customer.is_dirty(),
        table.unique_code.is_dirty(),
        table.by_tags.is_dirty(),
        table.unique_slug.is_dirty(),
        table.by_due.is_dirty(),
        table.by_customer_total.is_dirty(),
        table.unique_slug_customer.is_dirty(),
    ]
}

fn tagged_fixture() -> (TempDir, PathBuf) {
    let (dir, path) = scratch("tagged.ferro");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tagged-indexes.db");
    std::fs::copy(fixture, &path).unwrap();
    (dir, path)
}

#[test]
fn a_file_with_indexes_under_their_old_tags_rebuilds_them_once() {
    let (_dir, path) = tagged_fixture();
    {
        let db: Db<Shop> = fast().open(&path).unwrap();
        db.verify_typed().unwrap();
        let read = db.read();
        let orders = &read.orders;
        assert_eq!(order_dirty(orders), vec![true; 7]);
        assert_eq!(order_hashes(orders), order_hashes(&rebuilt(orders)));
        assert_eq!(orders.len(), 24);
        let red: Vec<u32> = orders.by_tags("red").keys().collect();
        assert_eq!(red, vec![0, 3, 6, 9, 12, 15, 18, 21, 30]);
        let sale: Vec<u32> = orders.by_tags("sale").keys().collect();
        assert_eq!(sale, vec![5]);
        assert_eq!(orders.key_by_code(&200), Some(9));
        assert_eq!(orders.covered_by_slug("o5"), Some((5, 999)));
        let covered: Vec<(u32, (i64, String))> = orders.by_customer(&2).covered().collect();
        assert_eq!(covered[0], (2, (-14, "moved".to_string())));
        let first = db.at(1).unwrap();
        assert_eq!(order_dirty(&first.orders), vec![true; 7]);
        assert_eq!(
            order_hashes(&first.orders),
            order_hashes(&rebuilt(&first.orders))
        );
        assert_eq!(first.orders.by_tags("sale").count(), 0);
        drop(read);
        db.update(|shop| {
            shop.orders.update(&7, |row| row.note = "upgraded".into());
        })
        .unwrap();
    }
    let db: Db<Shop> = fast().open(&path).unwrap();
    let read = db.read();
    assert_eq!(order_dirty(&read.orders), vec![false; 7]);
    assert_eq!(
        order_hashes(&read.orders),
        order_hashes(&rebuilt(&read.orders))
    );
    db.verify_typed().unwrap();
}

#[derive(Clone, Default)]
struct Forged {
    rows: Map<u32, Totals>,
    index: Map<(i64, u32), ()>,
    padded: bool,
}

impl Data for Forged {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.varint(2);
        w.field(field_tag("indexes"), |w| {
            if self.padded {
                w.raw(&[0xab; 8]);
            }
            self.index.encode(w)
        })?;
        w.field(field_tag("rows"), |w| self.rows.encode(w))
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        let mut forged = Forged::default();
        r.fields(|tag, r| {
            if tag == field_tag("rows") {
                forged.rows = Map::decode(r)?;
            }
            Ok(())
        })?;
        Ok(forged)
    }
}

#[derive(Data, Clone, Default)]
struct ForgedRoot {
    rows: Forged,
}

#[test]
fn an_index_map_under_the_shared_tag_without_its_fingerprint_is_rebuilt() {
    assert_ne!(field_tag("indexes"), field_tag("rows"));
    for padded in [false, true] {
        let (_dir, path) = scratch("forged.ferro");
        {
            let db: Db<ForgedRoot> = fast().open(&path).unwrap();
            db.update(|root| {
                root.rows.padded = padded;
                for id in 0..5u32 {
                    let row = Totals {
                        id,
                        total: i64::from(id) * 100,
                        fee: i64::from(id),
                    };
                    root.rows.index.insert((row.fee, id), ());
                    root.rows.rows.insert(id, row);
                }
            })
            .unwrap();
        }
        let db: Db<TotalsRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert!(read.rows.by_total.is_dirty(), "padded {padded}");
        let ids: Vec<u32> = read.rows.by_total(&300).keys().collect();
        assert_eq!(ids, vec![3]);
        assert_eq!(read.rows.by_total(&3).count(), 0);
        assert_eq!(
            read.rows.by_total.root_hash(),
            rebuilt(&read.rows).by_total.root_hash()
        );
    }
}

fn described<D: Data>() -> Vec<u8> {
    let mut description = Description::new();
    description.of::<D>();
    description.bytes().to_vec()
}

type Amount = i64;

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[ferro(transparent)]
struct Cents(i64);

#[derive(Data, Clone, Debug, PartialEq)]
struct Money {
    cents: i64,
    currency: String,
}

#[derive(Data, Clone, Debug, PartialEq)]
struct Cash {
    currency: String,
    #[ferro(rename = "cents")]
    amount: i64,
    #[ferro(skip)]
    shown: bool,
}

#[derive(Data, Clone, Debug, PartialEq)]
struct MoneyRetagged {
    #[ferro(rename = "minor")]
    cents: i64,
    currency: String,
}

#[derive(Data, Clone, Debug, PartialEq)]
enum Status {
    Open,
    Paid { at: u64 },
}

#[derive(Data, Clone, Debug, PartialEq)]
enum StatusWide {
    Open,
    Paid { at: i64 },
}

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Tree {
    label: String,
    kids: Vec<Tree>,
}

#[derive(Data, Clone, Debug, PartialEq)]
struct Ping {
    pong: Option<Box<Pong>>,
}

#[derive(Data, Clone, Debug, PartialEq)]
struct Pong {
    ping: Vec<Ping>,
}

#[test]
fn a_description_follows_the_encoding_and_not_the_spelling() {
    assert_eq!(described::<Amount>(), described::<i64>());
    assert_eq!(described::<Box<i64>>(), described::<i64>());
    assert_eq!(described::<std::sync::Arc<i64>>(), described::<i64>());
    assert_eq!(described::<Cents>(), described::<i64>());
    assert_eq!(described::<Money>(), described::<Cash>());
    assert_ne!(described::<Money>(), described::<MoneyRetagged>());
    assert_ne!(described::<Status>(), described::<StatusWide>());
    assert_ne!(described::<i64>(), described::<u64>());
    assert_ne!(described::<u8>(), described::<u16>());
    assert_ne!(described::<Option<u8>>(), described::<u8>());
    assert_ne!(described::<Vec<u8>>(), described::<Option<u8>>());
    assert_ne!(described::<(i64,)>(), described::<(i64, i64)>());
    assert_ne!(described::<(i64, String)>(), described::<(String, i64)>());
    assert_ne!(described::<[u8; 2]>(), described::<[u8; 3]>());
    assert_ne!(described::<Map<u8, u8>>(), described::<Map<u8, i8>>());
    assert_ne!(described::<String>(), described::<ferro::Bytes>());
    assert_eq!(described::<Tree>(), described::<Tree>());
    assert_ne!(described::<Tree>(), described::<Vec<Tree>>());
    assert!(!described::<Ping>().is_empty());
    assert_ne!(described::<Ping>(), described::<Pong>());
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Nested {
    #[key]
    id: u32,
    #[index(cover(tree, price))]
    group: u8,
    tree: Tree,
    price: Cents,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct NestedPlain {
    #[key]
    id: u32,
    #[index(cover(tree, price))]
    group: u8,
    tree: Tree,
    price: Amount,
}

root!(NestedRoot, Nested);
root!(NestedPlainRoot, NestedPlain);

#[test]
fn a_recursive_or_wrapped_covered_type_keeps_its_index_when_spelled_another_way() {
    let (_dir, path) = scratch("nested.ferro");
    let leaf = |label: &str| Tree {
        label: label.into(),
        kids: Vec::new(),
    };
    {
        let db: Db<NestedRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..4u32 {
                root.rows.insert(Nested {
                    id,
                    group: (id % 2) as u8,
                    tree: Tree {
                        label: format!("t{id}"),
                        kids: vec![leaf("a"), leaf("b")],
                    },
                    price: Cents(i64::from(id) * 5),
                });
            }
        })
        .unwrap();
    }
    let db: Db<NestedPlainRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(!read.rows.by_group.is_dirty());
    let covered: Vec<(u32, (String, usize, i64))> = read
        .rows
        .by_group(&1)
        .covered()
        .map(|(id, (tree, price))| (id, (tree.label, tree.kids.len(), price)))
        .collect();
    assert_eq!(
        covered,
        vec![
            (1, ("t1".to_string(), 2, 5)),
            (3, ("t3".to_string(), 2, 15))
        ]
    );
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Untiered {
    #[key]
    id: u32,
    region: u8,
}

fn tier_one() -> u8 {
    1
}

fn tier_two() -> u8 {
    2
}

fn also_two() -> u8 {
    2
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct TierOne {
    #[key]
    id: u32,
    #[index(cover(tier))]
    region: u8,
    #[index]
    #[ferro(default = "tier_one")]
    tier: u8,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct TierTwo {
    #[key]
    id: u32,
    #[index(cover(tier))]
    region: u8,
    #[index]
    #[ferro(default = "tier_two")]
    tier: u8,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct AlsoTwo {
    #[key]
    id: u32,
    #[index(cover(tier))]
    region: u8,
    #[index]
    #[ferro(default = "also_two")]
    tier: u8,
}

root!(UntieredRoot, Untiered);
root!(TierOneRoot, TierOne);
root!(TierTwoRoot, TierTwo);
root!(AlsoTwoRoot, AlsoTwo);

fn fill_untiered(path: &Path) -> Option<ferro::Hash> {
    let db: Db<UntieredRoot> = fast().open(path).unwrap();
    db.update(|root| {
        for id in 0..6u32 {
            root.rows.insert(Untiered {
                id,
                region: (id % 2) as u8,
            });
        }
    })
    .unwrap();
    db.read().rows.root_hash()
}

#[test]
fn changing_the_default_of_an_indexed_or_covered_field_rebuilds_the_index() {
    let (_dir, path) = scratch("tiers.ferro");
    let untouched = fill_untiered(&path);
    {
        let db: Db<TierOneRoot> = fast().open(&path).unwrap();
        let read = db.read();
        let ones: Vec<u32> = read.rows.by_tier(&1).keys().collect();
        assert_eq!(ones, vec![0, 1, 2, 3, 4, 5]);
        let covered: Vec<(u32, u8)> = read.rows.by_region(&0).covered().collect();
        assert_eq!(covered, vec![(0, 1), (2, 1), (4, 1)]);
        db.update(|_| ()).unwrap();
    }
    {
        let db: Db<TierOneRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert_eq!(read.rows.root_hash(), untouched);
        assert!(!read.rows.by_tier.is_dirty());
        assert!(!read.rows.by_region.is_dirty());
    }
    {
        let db: Db<TierTwoRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert_eq!(read.rows.root_hash(), untouched);
        assert!(read.rows.by_tier.is_dirty());
        assert!(read.rows.by_region.is_dirty());
        assert_eq!(read.rows.by_tier(&1).count(), 0);
        let twos: Vec<u32> = read.rows.by_tier(&2).keys().collect();
        assert_eq!(twos, vec![0, 1, 2, 3, 4, 5]);
        let covered: Vec<(u32, u8)> = read.rows.by_region(&0).covered().collect();
        assert_eq!(covered, vec![(0, 2), (2, 2), (4, 2)]);
        let fresh = rebuilt(&read.rows);
        assert_eq!(read.rows.by_tier.root_hash(), fresh.by_tier.root_hash());
        assert_eq!(read.rows.by_region.root_hash(), fresh.by_region.root_hash());
        db.update(|root| {
            root.rows.update(&5, |row| row.region = 0);
            root.rows.update(&4, |row| row.tier = 7);
        })
        .unwrap();
        let read = db.read();
        let covered: Vec<(u32, u8)> = read.rows.by_region(&0).covered().collect();
        assert_eq!(covered, vec![(0, 2), (2, 2), (4, 7), (5, 2)]);
        let twos: Vec<u32> = read.rows.by_tier(&2).keys().collect();
        assert_eq!(twos, vec![0, 1, 2, 3, 5]);
        let fresh = rebuilt(&read.rows);
        assert_eq!(read.rows.by_tier.root_hash(), fresh.by_tier.root_hash());
        assert_eq!(read.rows.by_region.root_hash(), fresh.by_region.root_hash());
    }
    let db: Db<AlsoTwoRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert!(!read.rows.by_tier.is_dirty());
    assert!(!read.rows.by_region.is_dirty());
    let twos: Vec<u32> = read.rows.by_tier(&2).keys().collect();
    assert_eq!(twos, vec![0, 1, 2, 3, 5]);
}

#[derive(Data, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[ferro(transparent)]
struct GradeA(u8);

impl Default for GradeA {
    fn default() -> Self {
        GradeA(1)
    }
}

#[derive(Data, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[ferro(transparent)]
struct GradeB(u8);

impl Default for GradeB {
    fn default() -> Self {
        GradeB(2)
    }
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct GradedA {
    #[key]
    id: u32,
    region: u8,
    #[index]
    #[ferro(default)]
    grade: GradeA,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct GradedB {
    #[key]
    id: u32,
    region: u8,
    #[index]
    #[ferro(default)]
    grade: GradeB,
}

root!(GradedARoot, GradedA);
root!(GradedBRoot, GradedB);

#[test]
fn changing_what_default_returns_for_an_indexed_type_rebuilds_its_index() {
    let (_dir, path) = scratch("grades.ferro");
    let untouched = fill_untiered(&path);
    {
        let db: Db<GradedARoot> = fast().open(&path).unwrap();
        let ones: Vec<u32> = db.read().rows.by_grade(&GradeA(1)).keys().collect();
        assert_eq!(ones, vec![0, 1, 2, 3, 4, 5]);
        db.update(|_| ()).unwrap();
    }
    let db: Db<GradedBRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert_eq!(read.rows.root_hash(), untouched);
    assert!(read.rows.by_grade.is_dirty());
    assert_eq!(read.rows.by_grade(&GradeB(1)).count(), 0);
    let twos: Vec<u32> = read.rows.by_grade(&GradeB(2)).keys().collect();
    assert_eq!(twos, vec![0, 1, 2, 3, 4, 5]);
}

#[test]
fn a_default_is_described_by_the_value_it_returns() {
    let fingerprint = |value: u8| {
        ferro::__derive::index_fingerprint(|d| {
            d.word("value");
            d.value(&value);
        })
    };
    assert_eq!(fingerprint(2), fingerprint(2));
    assert_ne!(fingerprint(1), fingerprint(2));
}

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Badge {
    kind: u8,
}

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BadgeOne {
    kind: u8,
    #[ferro(default = "tier_one")]
    level: u8,
}

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BadgeTwo {
    kind: u8,
    #[ferro(default = "tier_two")]
    level: u8,
}

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BadgeAlso {
    kind: u8,
    #[ferro(default = "also_two")]
    level: u8,
}

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Idle,
    Busy { job: u8 },
}

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PhaseOne {
    Idle,
    Busy {
        job: u8,
        #[ferro(default = "tier_one")]
        level: u8,
    },
}

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PhaseTwo {
    Idle,
    Busy {
        job: u8,
        #[ferro(default = "tier_two")]
        level: u8,
    },
}

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PhaseAlso {
    Idle,
    Busy {
        job: u8,
        #[ferro(default = "also_two")]
        level: u8,
    },
}

macro_rules! badged {
    ($row:ident, $root:ident, $badge:ident, $phase:ident) => {
        #[derive(Data, Record, Clone, Debug, PartialEq)]
        struct $row {
            #[key]
            id: u32,
            #[index(cover(badge))]
            region: u8,
            #[index]
            badge: $badge,
            #[index]
            phase: Option<$phase>,
            #[index(each)]
            parts: Vec<$badge>,
        }

        root!($root, $row);

        impl $row {
            fn dirty(table: &Table<$row>) -> [bool; 4] {
                [
                    table.by_region.is_dirty(),
                    table.by_badge.is_dirty(),
                    table.by_phase.is_dirty(),
                    table.by_parts.is_dirty(),
                ]
            }
        }
    };
}

badged!(Badged, BadgedRoot, Badge, Phase);
badged!(BadgedOne, BadgedOneRoot, BadgeOne, PhaseOne);
badged!(BadgedTwo, BadgedTwoRoot, BadgeTwo, PhaseTwo);
badged!(BadgedAlso, BadgedAlsoRoot, BadgeAlso, PhaseAlso);

#[derive(Data, Clone, Debug, PartialEq)]
struct GradedHolderA {
    #[ferro(default)]
    grade: GradeA,
}

#[derive(Data, Clone, Debug, PartialEq)]
struct GradedHolderB {
    #[ferro(default)]
    grade: GradeB,
}

#[test]
fn a_description_holds_the_defaults_of_the_types_it_reaches() {
    assert_eq!(described::<GradeA>(), described::<GradeB>());
    assert_ne!(described::<GradedHolderA>(), described::<GradedHolderB>());
    assert_ne!(described::<BadgeOne>(), described::<BadgeTwo>());
    assert_eq!(described::<BadgeTwo>(), described::<BadgeAlso>());
    assert_ne!(described::<PhaseOne>(), described::<PhaseTwo>());
    assert_eq!(described::<PhaseTwo>(), described::<PhaseAlso>());
    assert_ne!(
        described::<Option<BadgeOne>>(),
        described::<Option<BadgeTwo>>()
    );
    assert_ne!(described::<Vec<BadgeOne>>(), described::<Vec<BadgeTwo>>());
    assert_ne!(
        described::<std::collections::BTreeMap<u8, BadgeOne>>(),
        described::<std::collections::BTreeMap<u8, BadgeTwo>>()
    );
    assert_ne!(
        described::<Map<u8, BadgeOne>>(),
        described::<Map<u8, BadgeTwo>>()
    );
    assert_ne!(
        described::<(u8, Box<BadgeOne>)>(),
        described::<(u8, Box<BadgeTwo>)>()
    );
}

#[test]
fn changing_a_default_nested_in_an_indexed_or_covered_type_rebuilds_the_index() {
    let (_dir, path) = scratch("badges.ferro");
    {
        let db: Db<BadgedRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..4u8 {
                root.rows.insert(Badged {
                    id: u32::from(id),
                    region: id % 2,
                    badge: Badge { kind: 0 },
                    phase: Some(Phase::Busy { job: id }),
                    parts: vec![Badge { kind: id }],
                });
            }
        })
        .unwrap();
        assert_eq!(Badged::dirty(&db.read().rows), [false; 4]);
    }
    let untouched = {
        let db: Db<BadgedOneRoot> = fast().open(&path).unwrap();
        let ones: Vec<u32> = db
            .read()
            .rows
            .by_badge(&BadgeOne { kind: 0, level: 1 })
            .keys()
            .collect();
        assert_eq!(ones, vec![0, 1, 2, 3]);
        db.update(|_| ()).unwrap();
        db.read().rows.root_hash()
    };
    {
        let db: Db<BadgedOneRoot> = fast().open(&path).unwrap();
        assert_eq!(BadgedOne::dirty(&db.read().rows), [false; 4]);
    }
    {
        let db: Db<BadgedTwoRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert_eq!(read.rows.root_hash(), untouched);
        assert_eq!(BadgedTwo::dirty(&read.rows), [true; 4]);
        assert!(read.rows.iter().all(|row| row.badge.level == 2));
        let two = BadgeTwo { kind: 0, level: 2 };
        assert_eq!(
            read.rows.by_badge(&BadgeTwo { kind: 0, level: 1 }).count(),
            0
        );
        let twos: Vec<u32> = read.rows.by_badge(&two).keys().collect();
        assert_eq!(twos, vec![0, 1, 2, 3]);
        let covered: Vec<(u32, BadgeTwo)> = read.rows.by_region(&0).covered().collect();
        assert_eq!(covered, vec![(0, two.clone()), (2, two)]);
        let busy: Vec<u32> = read
            .rows
            .by_phase(&Some(PhaseTwo::Busy { job: 1, level: 2 }))
            .keys()
            .collect();
        assert_eq!(busy, vec![1]);
        let parts: Vec<u32> = read
            .rows
            .by_parts(&BadgeTwo { kind: 3, level: 2 })
            .keys()
            .collect();
        assert_eq!(parts, vec![3]);
        let fresh = rebuilt(&read.rows);
        assert_eq!(read.rows.by_region.root_hash(), fresh.by_region.root_hash());
        assert_eq!(read.rows.by_badge.root_hash(), fresh.by_badge.root_hash());
        assert_eq!(read.rows.by_phase.root_hash(), fresh.by_phase.root_hash());
        assert_eq!(read.rows.by_parts.root_hash(), fresh.by_parts.root_hash());
        db.update(|_| ()).unwrap();
    }
    let db: Db<BadgedAlsoRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert_eq!(BadgedAlso::dirty(&read.rows), [false; 4]);
    let twos: Vec<u32> = read
        .rows
        .by_badge(&BadgeAlso { kind: 0, level: 2 })
        .keys()
        .collect();
    assert_eq!(twos, vec![0, 1, 2, 3]);
}

static MISSING: [AtomicU8; 2] = [AtomicU8::new(1), AtomicU8::new(1)];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank<const SLOT: usize>(u8);

impl<const SLOT: usize> Data for Rank<SLOT> {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.u8(self.0);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Rank(r.u8()?))
    }

    fn missing() -> Option<Self> {
        Some(Rank(MISSING[SLOT].load(Ordering::Relaxed)))
    }
}

#[derive(Data, Clone, Debug, PartialEq)]
struct Ranked {
    rank: Rank<1>,
}

#[test]
fn the_default_description_holds_the_value_missing_returns() {
    let mut named = Description::new();
    named.word("rust");
    named.word(std::any::type_name::<Forged>());
    assert_eq!(described::<Forged>(), named.bytes());
    let all = || {
        [
            described::<Rank<1>>(),
            described::<Ranked>(),
            described::<Option<Box<Rank<1>>>>(),
        ]
    };
    MISSING[1].store(1, Ordering::Relaxed);
    let ones = all();
    MISSING[1].store(2, Ordering::Relaxed);
    let twos = all();
    for (one, two) in ones.iter().zip(&twos) {
        assert_ne!(one, two);
    }
    MISSING[1].store(1, Ordering::Relaxed);
    assert_eq!(all(), ones);
}

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Crest {
    kind: u8,
}

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RankedCrest {
    kind: u8,
    rank: Rank<0>,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Unranked {
    #[key]
    id: u32,
    region: u8,
    crest: Crest,
}

root!(UnrankedRoot, Unranked);

macro_rules! ranked {
    ($row:ident, $root:ident) => {
        #[derive(Data, Record, Clone, Debug, PartialEq)]
        struct $row {
            #[key]
            id: u32,
            #[index(cover(rank))]
            region: u8,
            #[index]
            rank: Rank<0>,
            #[index]
            crest: RankedCrest,
        }

        root!($root, $row);

        impl $row {
            fn dirty(table: &Table<$row>) -> [bool; 3] {
                [
                    table.by_region.is_dirty(),
                    table.by_rank.is_dirty(),
                    table.by_crest.is_dirty(),
                ]
            }
        }
    };
}

ranked!(RankedOne, RankedOneRoot);
ranked!(RankedTwo, RankedTwoRoot);
ranked!(RankedAlso, RankedAlsoRoot);

#[test]
fn changing_what_missing_returns_for_an_indexed_hand_written_type_rebuilds_the_index() {
    let (_dir, path) = scratch("ranks.ferro");
    MISSING[0].store(1, Ordering::Relaxed);
    {
        let db: Db<UnrankedRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..4u8 {
                root.rows.insert(Unranked {
                    id: u32::from(id),
                    region: id % 2,
                    crest: Crest { kind: id },
                });
            }
        })
        .unwrap();
    }
    let untouched = {
        let db: Db<RankedOneRoot> = fast().open(&path).unwrap();
        let ones: Vec<u32> = db.read().rows.by_rank(&Rank(1)).keys().collect();
        assert_eq!(ones, vec![0, 1, 2, 3]);
        db.update(|_| ()).unwrap();
        assert_eq!(RankedOne::dirty(&db.read().rows), [false; 3]);
        db.read().rows.root_hash()
    };
    MISSING[0].store(2, Ordering::Relaxed);
    {
        let db: Db<RankedTwoRoot> = fast().open(&path).unwrap();
        let read = db.read();
        assert_eq!(read.rows.root_hash(), untouched);
        assert_eq!(RankedTwo::dirty(&read.rows), [true; 3]);
        assert!(
            read.rows
                .iter()
                .all(|row| row.rank == Rank(2) && row.crest.rank == Rank(2))
        );
        assert_eq!(read.rows.by_rank(&Rank(1)).count(), 0);
        let twos: Vec<u32> = read.rows.by_rank(&Rank(2)).keys().collect();
        assert_eq!(twos, vec![0, 1, 2, 3]);
        let covered: Vec<(u32, Rank<0>)> = read.rows.by_region(&0).covered().collect();
        assert_eq!(covered, vec![(0, Rank(2)), (2, Rank(2))]);
        let crest = RankedCrest {
            kind: 3,
            rank: Rank(2),
        };
        let crests: Vec<u32> = read.rows.by_crest(&crest).keys().collect();
        assert_eq!(crests, vec![3]);
        let fresh = rebuilt(&read.rows);
        assert_eq!(read.rows.by_region.root_hash(), fresh.by_region.root_hash());
        assert_eq!(read.rows.by_rank.root_hash(), fresh.by_rank.root_hash());
        assert_eq!(read.rows.by_crest.root_hash(), fresh.by_crest.root_hash());
        db.update(|_| ()).unwrap();
    }
    let db: Db<RankedAlsoRoot> = fast().open(&path).unwrap();
    let read = db.read();
    assert_eq!(RankedAlso::dirty(&read.rows), [false; 3]);
    let twos: Vec<u32> = read.rows.by_rank(&Rank(2)).keys().collect();
    assert_eq!(twos, vec![0, 1, 2, 3]);
}

type MaybeCode = Option<u32>;

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[ferro(transparent)]
struct WrappedCode(Option<u32>);

macro_rules! coded {
    ($record:ident, $root:ident, $code:ty, $wrap:expr, $lookup:ty, $look:expr) => {
        #[derive(Data, Record, Clone, Debug, PartialEq)]
        struct $record {
            #[key]
            id: u32,
            #[unique]
            code: $code,
            #[index]
            label: $code,
        }

        root!($root, $record);

        impl Coded for $root {
            fn fill(&mut self, id: u32, code: Option<u32>) {
                let wrap: fn(Option<u32>) -> $code = $wrap;
                self.rows.insert($record {
                    id,
                    code: wrap(code),
                    label: wrap(code.map(|code| code % 2)),
                });
            }

            fn holder(&self, code: u32) -> Option<u32> {
                let wrap: fn(Option<u32>) -> $code = $wrap;
                let look: fn(u32) -> $lookup = $look;
                let by_unique = self.rows.key_by_code(&look(code));
                let by_label: Vec<u32> = self.rows.by_label(&wrap(Some(code % 2))).keys().collect();
                assert!(by_unique.is_none_or(|id| by_label.contains(&id)));
                by_unique
            }

            fn coded(&self) -> usize {
                self.rows.unique_code().len()
            }

            fn dirty(&self) -> [bool; 2] {
                [
                    self.rows.unique_code.is_dirty(),
                    self.rows.by_label.is_dirty(),
                ]
            }
        }
    };
}

trait Coded: Data + Default {
    fn fill(&mut self, id: u32, code: Option<u32>);
    fn holder(&self, code: u32) -> Option<u32>;
    fn coded(&self) -> usize;
    fn dirty(&self) -> [bool; 2];
}

coded!(
    SpelledRow,
    SpelledRoot,
    Option<u32>,
    |code| code,
    u32,
    |code| code
);
coded!(
    AliasedRow,
    AliasedRoot,
    MaybeCode,
    |code| code,
    MaybeCode,
    Some
);
coded!(
    BoxedRow,
    BoxedRoot,
    Box<Option<u32>>,
    Box::new,
    Box<Option<u32>>,
    |code| Box::new(Some(code))
);
coded!(
    WrappedRow,
    WrappedRoot,
    WrappedCode,
    WrappedCode,
    WrappedCode,
    |code| WrappedCode(Some(code))
);

fn reopened<R: Coded>(path: &Path, next: u32) {
    let db: Db<R> = fast().open(path).unwrap();
    assert_eq!(
        db.read().dirty(),
        [false, false],
        "{}",
        std::any::type_name::<R>()
    );
    assert_eq!(db.read().coded(), next as usize - 3);
    for code in 0..next - 3 {
        assert_eq!(db.read().holder(code), Some(code + 3));
    }
    db.update(|root| {
        root.fill(next + 100, None);
        root.fill(next, Some(next - 3));
    })
    .unwrap();
    assert_eq!(db.read().holder(next - 3), Some(next));
}

#[test]
fn an_option_written_through_an_alias_a_box_or_a_wrapper_keeps_its_indexes() {
    let (_dir, path) = scratch("spelled.ferro");
    {
        let db: Db<SpelledRoot> = fast().open(&path).unwrap();
        db.update(|root| {
            for id in 0..3 {
                root.fill(id, None);
            }
            root.fill(3, Some(0));
        })
        .unwrap();
    }
    reopened::<AliasedRoot>(&path, 4);
    reopened::<BoxedRoot>(&path, 5);
    reopened::<WrappedRoot>(&path, 6);
    reopened::<SpelledRoot>(&path, 7);
}
