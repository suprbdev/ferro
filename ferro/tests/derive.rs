use std::collections::BTreeSet;

use ferro::codec::{Decoder, Encoder, HashSink};
use ferro::{Bytes, Data, Db, Record, Set, Table};

fn bytes<T: Data>(value: &T) -> Vec<u8> {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder).unwrap();
    encoder.into_object().body
}

fn roundtrip<T: Data + PartialEq + std::fmt::Debug>(value: T) {
    let body = bytes(&value);
    let mut decoder = Decoder::new(&body, &[], None);
    let back = T::decode(&mut decoder).unwrap();
    assert_eq!(back, value);
    assert_eq!(decoder.remaining(), 0);
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct Plain {
    id: u64,
    name: String,
    tags: Vec<String>,
    maybe: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct Tuple(u64, String, bool);

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct Unit;

#[derive(Clone, Debug, PartialEq, Data)]
enum Shape {
    Empty,
    Circle(f64),
    Pair(u8, String),
    Rect { w: u32, h: u32 },
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct Wrapper<T> {
    inner: T,
    extra: u8,
}

#[derive(Clone, Debug, PartialEq, Data)]
struct Renamed {
    #[ferro(rename = "old")]
    new: u64,
}

#[derive(Clone, Debug, PartialEq, Data)]
struct Old {
    old: u64,
}

fn seven() -> u32 {
    7
}

#[derive(Clone, Debug, PartialEq, Data)]
struct Defaults {
    a: u8,
    #[ferro(default)]
    b: String,
    #[ferro(default = "seven")]
    c: u32,
    #[ferro(skip)]
    d: u64,
}

#[derive(Clone, Debug, PartialEq, Data)]
struct JustA {
    a: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct OrderA {
    x: u64,
    y: String,
    z: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct OrderB {
    z: bool,
    x: u64,
    y: String,
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct V1 {
    id: u64,
    gone: String,
    kept: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct V2 {
    kept: u32,
    #[ferro(default = "seven")]
    added: u32,
    id: u64,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
#[ferro(transparent)]
struct Blob(Bytes);

#[derive(Data, Clone, Debug, Default, PartialEq)]
#[ferro(transparent)]
struct Named {
    inner: Vec<u8>,
}

#[derive(Data, Clone, Debug, PartialEq)]
struct HoldsBlob {
    kept: u8,
}

#[derive(Data, Clone, Debug, PartialEq)]
struct GrewBlob {
    kept: u8,
    blob: Blob,
    named: Named,
}

#[test]
fn transparent_newtypes_encode_as_their_field() {
    let raw = Bytes::from(vec![1u8, 2, 3, 4]);
    assert_eq!(bytes(&Blob(raw.clone())), bytes(&raw));
    assert_eq!(
        bytes(&Named {
            inner: raw.to_vec()
        }),
        bytes(&raw)
    );
    roundtrip(Blob(raw.clone()));
    roundtrip(Named {
        inner: raw.to_vec(),
    });
    let mut encoded = Vec::new();
    {
        let mut sink = HashSink;
        let mut w = Encoder::new(&mut sink);
        HoldsBlob { kept: 7 }.encode(&mut w).unwrap();
        encoded.extend_from_slice(w.bytes());
    }
    let mut r = Decoder::new(&encoded, &[], None);
    let grown = GrewBlob::decode(&mut r).unwrap();
    assert_eq!(grown.kept, 7);
    assert_eq!(grown.blob, Blob::default());
    assert_eq!(grown.named, Named::default());
}

#[test]
fn struct_roundtrip_through_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.ferro");
    let value = Plain {
        id: 9,
        name: "nine".into(),
        tags: vec!["a".into(), "b".into()],
        maybe: Some(4),
    };
    {
        let db = Db::<Plain>::open(&path).unwrap();
        db.update(|root| *root = value.clone()).unwrap();
    }
    let db = Db::<Plain>::open(&path).unwrap();
    assert_eq!(*db.read(), value);
}

#[test]
fn tuple_and_unit() {
    roundtrip(Tuple(5, "x".into(), true));
    roundtrip(Unit);
    assert_eq!(bytes(&Unit), vec![0]);
}

#[test]
fn enums() {
    roundtrip(Shape::Empty);
    roundtrip(Shape::Circle(1.5));
    roundtrip(Shape::Pair(3, "p".into()));
    roundtrip(Shape::Rect { w: 2, h: 3 });
    let mut decoder = Decoder::new(&[0xff, 0xff, 0], &[], None);
    assert!(matches!(
        Shape::decode(&mut decoder),
        Err(ferro::Error::Decode(_))
    ));
}

#[test]
fn generics() {
    roundtrip(Wrapper {
        inner: "hi".to_string(),
        extra: 1,
    });
    roundtrip(Wrapper {
        inner: Wrapper {
            inner: vec![1u32, 2],
            extra: 2,
        },
        extra: 3,
    });
}

#[test]
fn rename_keeps_old_tag() {
    assert_eq!(bytes(&Renamed { new: 5 }), bytes(&Old { old: 5 }));
    let body = bytes(&Old { old: 42 });
    let mut decoder = Decoder::new(&body, &[], None);
    assert_eq!(Renamed::decode(&mut decoder).unwrap(), Renamed { new: 42 });
}

#[test]
fn default_and_skip() {
    let body = bytes(&JustA { a: 1 });
    let mut decoder = Decoder::new(&body, &[], None);
    let decoded = Defaults::decode(&mut decoder).unwrap();
    assert_eq!(
        decoded,
        Defaults {
            a: 1,
            b: String::new(),
            c: 7,
            d: 0
        }
    );
    let full = Defaults {
        a: 1,
        b: "b".into(),
        c: 9,
        d: 100,
    };
    let body = bytes(&full);
    let mut decoder = Decoder::new(&body, &[], None);
    let back = Defaults::decode(&mut decoder).unwrap();
    assert_eq!(back, Defaults { d: 0, ..full });
    assert_eq!(
        body.len(),
        bytes(&Defaults {
            d: 0,
            ..back.clone()
        })
        .len()
    );
}

#[test]
fn missing_required_field_errors() {
    let body = bytes(&Unit);
    let mut decoder = Decoder::new(&body, &[], None);
    assert!(matches!(
        JustA::decode(&mut decoder),
        Err(ferro::Error::MissingField("a"))
    ));
}

#[test]
fn reorder_is_byte_identical() {
    let a = OrderA {
        x: 1,
        y: "y".into(),
        z: true,
    };
    let b = OrderB {
        z: true,
        x: 1,
        y: "y".into(),
    };
    assert_eq!(bytes(&a), bytes(&b));
}

#[test]
fn schema_evolution() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.ferro");
    {
        let db = Db::<V1>::open(&path).unwrap();
        db.update(|root| {
            *root = V1 {
                id: 11,
                gone: "bye".into(),
                kept: 22,
            }
        })
        .unwrap();
    }
    let db = Db::<V2>::open(&path).unwrap();
    assert_eq!(
        *db.read(),
        V2 {
            kept: 22,
            added: 7,
            id: 11
        }
    );
    db.update(|root| root.added = 99).unwrap();
    drop(db);
    let db = Db::<V1>::open(&path).unwrap_err();
    assert!(matches!(db, ferro::Error::MissingField("gone")));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Data)]
enum Status {
    Open,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
struct Issue {
    #[key]
    id: u64,
    #[index]
    status: Status,
    #[index(each)]
    labels: Vec<String>,
    title: String,
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
struct Doc {
    #[key]
    name: String,
    #[index(each)]
    tags: BTreeSet<u32>,
    #[index(each)]
    set: Set<String>,
    #[index]
    rank: u32,
}

#[derive(Clone, Default, Data)]
struct Tracker {
    issues: Table<Issue>,
    docs: Table<Doc>,
}

fn issue(id: u64, status: Status, labels: &[&str]) -> Issue {
    Issue {
        id,
        status,
        labels: labels.iter().map(|s| s.to_string()).collect(),
        title: format!("issue {id}"),
    }
}

fn ids(iter: impl Iterator<Item = ferro::Val<Issue>>) -> Vec<u64> {
    iter.map(|row| row.id).collect()
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(tenant, created)]
struct Event {
    #[key]
    id: u64,
    tenant: String,
    created: u32,
}

#[test]
fn composite_index_orders_by_each_field_in_turn() {
    let mut table: Table<Event> = Table::new();
    let event = |id, tenant: &str, created| Event {
        id,
        tenant: tenant.to_string(),
        created,
    };
    table.insert(event(1, "b", 30));
    table.insert(event(2, "a", 20));
    table.insert(event(3, "a", 10));
    table.insert(event(4, "a", 20));
    let listed =
        |rows: ferro::table::IndexIter<Event>| -> Vec<u64> { rows.map(|row| row.id).collect() };
    assert_eq!(
        listed(table.by_tenant_created(&("a".to_string(), 20))),
        [2, 4]
    );
    assert_eq!(listed(table.by_tenant_created_prefix("a")), [3, 2, 4]);
    assert_eq!(listed(table.by_tenant_created_prefix("b")), [1]);
    assert_eq!(listed(table.by_tenant_created_prefix("c")), [0u64; 0]);
    let span = ("a".to_string(), 15)..("b".to_string(), 0);
    assert_eq!(listed(table.by_tenant_created_range(span)), [2, 4]);
    assert_eq!(
        table.by_tenant_created_counts(),
        [
            (("a".to_string(), 10), 1),
            (("a".to_string(), 20), 2),
            (("b".to_string(), 30), 1),
        ]
    );
    table.update(&2, |row| row.created = 5);
    assert_eq!(listed(table.by_tenant_created_prefix("a")), [2, 3, 4]);
    table.remove(&3);
    assert_eq!(listed(table.by_tenant_created_prefix("a")), [2, 4]);
}

#[test]
fn record_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.ferro");
    {
        let db = Db::<Tracker>::open(&path).unwrap();
        db.update(|t| {
            t.issues.insert(issue(1, Status::Open, &["bug", "ui"]));
            t.issues.insert(issue(2, Status::Closed, &["bug"]));
            t.issues.insert(issue(3, Status::Open, &[]));
        })
        .unwrap();

        let snap = db.read();
        assert_eq!(snap.issues.len(), 3);
        assert_eq!(ids(snap.issues.by_status(&Status::Open)), vec![1, 3]);
        assert_eq!(ids(snap.issues.by_status(&Status::Closed)), vec![2]);
        assert_eq!(ids(snap.issues.by_status(&Status::Open).rev()), vec![3, 1]);
        assert_eq!(snap.issues.by_status(&Status::Open).len(), 2);
        assert_eq!(ids(snap.issues.by_labels("bug")), vec![1, 2]);
        assert_eq!(ids(snap.issues.by_labels("ui")), vec![1]);
        assert_eq!(
            ids(snap.issues.by_status_range(Status::Open..=Status::Closed)),
            vec![1, 3, 2]
        );
        assert_eq!(ids(snap.issues.by_status_range(Status::Closed..)), vec![2]);
        assert_eq!(
            ids(snap
                .issues
                .by_labels_range("a".to_string().."c".to_string())),
            vec![1, 2]
        );

        db.update(|t| {
            let mut row = t.issues.get_mut(&1).unwrap();
            row.status = Status::Closed;
            row.labels = vec!["ui".into(), "new".into()];
        })
        .unwrap();
        let snap = db.read();
        assert_eq!(ids(snap.issues.by_status(&Status::Open)), vec![3]);
        assert_eq!(ids(snap.issues.by_status(&Status::Closed)), vec![1, 2]);
        assert_eq!(ids(snap.issues.by_labels("bug")), vec![2]);
        assert_eq!(ids(snap.issues.by_labels("ui")), vec![1]);
        assert_eq!(ids(snap.issues.by_labels("new")), vec![1]);

        db.update(|t| {
            let mut row = t.issues.get_mut(&1).unwrap();
            row.id = 10;
        })
        .unwrap();
        let snap = db.read();
        assert!(snap.issues.get(&1).is_none());
        assert_eq!(snap.issues.get(&10).unwrap().id, 10);
        assert_eq!(ids(snap.issues.by_status(&Status::Closed)), vec![2, 10]);
        assert_eq!(ids(snap.issues.by_labels("new")), vec![10]);
        assert_eq!(snap.issues.len(), 3);

        db.update(|t| {
            assert!(t.issues.remove(&2).is_some());
            assert!(t.issues.remove(&2).is_none());
        })
        .unwrap();
        let snap = db.read();
        assert_eq!(ids(snap.issues.by_status(&Status::Closed)), vec![10]);
        assert_eq!(ids(snap.issues.by_labels("bug")), Vec::<u64>::new());
        assert_eq!(snap.issues.len(), 2);

        let before = db.version();
        db.update(|t| {
            t.issues.update(&3, |row| row.title = "same index".into());
        })
        .unwrap();
        assert_eq!(db.version(), before + 1);
    }

    let db = Db::<Tracker>::open(&path).unwrap();
    let snap = db.read();
    assert_eq!(snap.issues.len(), 2);
    assert_eq!(ids(snap.issues.by_status(&Status::Open)), vec![3]);
    assert_eq!(snap.issues.get(&3).unwrap().title, "same index");
}

#[test]
fn unchanged_index_does_not_rewrite() {
    let mut a = Table::<Issue>::new();
    a.insert(issue(1, Status::Open, &["x"]));
    let mut b = a.clone();
    b.update(&1, |row| row.title = "other".into());
    assert_ne!(a, b);
    assert_eq!(ids(b.by_status(&Status::Open)), vec![1]);
    assert_eq!(ids(b.by_labels("x")), vec![1]);
    let mut c = b.clone();
    c.update(&1, |row| row.title = a.get(&1).unwrap().title.clone());
    assert_eq!(a, c);
}

#[test]
fn each_with_set_types() {
    let mut table = Table::<Doc>::new();
    table.insert(Doc {
        name: "a".into(),
        tags: [1, 2].into_iter().collect(),
        set: ["x".to_string()].into_iter().collect(),
        rank: 5,
    });
    table.insert(Doc {
        name: "b".into(),
        tags: [2, 3].into_iter().collect(),
        set: ["x".to_string(), "y".to_string()].into_iter().collect(),
        rank: 5,
    });
    let names = |iter: Box<dyn Iterator<Item = ferro::Val<Doc>>>| -> Vec<String> {
        iter.map(|row| row.name.clone()).collect()
    };
    assert_eq!(names(Box::new(table.by_tags(&2))), vec!["a", "b"]);
    assert_eq!(names(Box::new(table.by_tags(&1))), vec!["a"]);
    assert_eq!(names(Box::new(table.by_set("x"))), vec!["a", "b"]);
    assert_eq!(names(Box::new(table.by_set("y"))), vec!["b"]);
    assert_eq!(names(Box::new(table.by_rank(&5))), vec!["a", "b"]);
    table.update("b", |row| {
        row.tags.remove(&2);
        row.set.remove("x");
    });
    assert_eq!(names(Box::new(table.by_tags(&2))), vec!["a"]);
    assert_eq!(names(Box::new(table.by_set("x"))), vec!["a"]);
    assert_eq!(names(Box::new(table.by_tags(&3))), vec!["b"]);
    table.remove("a");
    assert_eq!(names(Box::new(table.by_tags(&2))), Vec::<String>::new());
}

#[derive(Data, Clone, Debug, PartialEq)]
struct Scored {
    id: u32,
    title_score: u32,
}

#[derive(Data, Clone, Debug, PartialEq)]
#[ferro(removed("title_score"))]
struct Flagged {
    id: u32,
    #[ferro(rename = "flag_bits", default)]
    flags: u32,
}

#[derive(Data, Clone, Debug, PartialEq)]
enum Ledger {
    Open,
    RefundPremium(u32),
    Paid { at: u64, title_score: u32 },
}

#[derive(Data, Clone, Debug, PartialEq)]
#[ferro(removed("RefundPremium"))]
enum Taxed {
    Open,
    #[ferro(rename = "Levy")]
    Tax(u32),
    #[ferro(removed("title_score"))]
    Paid {
        at: u64,
        #[ferro(rename = "flag_bits", default)]
        flags: u32,
    },
}

fn reread<A: Data, B: Data>(value: &A) -> ferro::Result<B> {
    let body = bytes(value);
    B::decode(&mut Decoder::new(&body, &[], None))
}

#[test]
fn a_name_listed_as_removed_keeps_its_old_bytes_from_a_new_field_or_variant() {
    use ferro::codec::field_tag;
    assert_eq!(field_tag("title_score"), field_tag("flags"));
    assert_eq!(field_tag("RefundPremium"), field_tag("Tax"));
    let old = Scored {
        id: 1,
        title_score: 7,
    };
    let flagged: Flagged = reread(&old).unwrap();
    assert_eq!(flagged, Flagged { id: 1, flags: 0 });
    roundtrip(Flagged { id: 2, flags: 3 });
    assert_eq!(reread::<_, Taxed>(&Ledger::Open).unwrap(), Taxed::Open);
    assert!(matches!(
        reread::<_, Taxed>(&Ledger::RefundPremium(9)),
        Err(ferro::Error::Decode(_))
    ));
    let paid = Ledger::Paid {
        at: 5,
        title_score: 7,
    };
    let taxed: Taxed = reread(&paid).unwrap();
    assert_eq!(taxed, Taxed::Paid { at: 5, flags: 0 });
    roundtrip(Taxed::Tax(4));
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(r#type, r#use)]
struct Keyword {
    #[key]
    id: u32,
    #[index(cover(r#use))]
    r#type: u8,
    #[unique]
    r#use: String,
    #[index(each)]
    r#in: Vec<u8>,
}

#[test]
fn raw_identifier_fields_are_indexed_under_their_plain_names() {
    let mut table: Table<Keyword> = Table::new();
    for id in 0..4u32 {
        table.insert(Keyword {
            id,
            r#type: (id % 2) as u8,
            r#use: format!("u{id}"),
            r#in: vec![id as u8, 9],
        });
    }
    let odd: Vec<(u32, String)> = table.by_type(&1).covered().collect();
    assert_eq!(odd, [(1, "u1".to_string()), (3, "u3".to_string())]);
    assert_eq!(table.key_by_use("u2"), Some(2));
    assert_eq!(table.by_in(&9).len(), 4);
    let pair: Vec<u32> = table.by_type_use(&(0, "u2".to_string())).keys().collect();
    assert_eq!(pair, [2]);
    let find = table.find();
    let found: Vec<u32> = find.by_type(&0).and(find.by_in(&2)).keys().collect();
    assert_eq!(found, [2]);
    let taken = table.try_update(&3, |row| row.r#use = "u0".into());
    assert_eq!(taken.unwrap_err().index, "use");
}

#[derive(Data, Clone, Debug, PartialEq)]
struct RawField {
    r#type: u8,
}

#[derive(Data, Clone, Debug, PartialEq)]
struct PlainField {
    #[ferro(rename = "type")]
    kind: u8,
}

#[derive(Data, Clone, Debug, PartialEq)]
enum RawVariant {
    r#Type(u8),
}

#[derive(Data, Clone, Debug, PartialEq)]
enum PlainVariant {
    #[ferro(rename = "Type")]
    Kind(u8),
}

#[test]
fn a_raw_identifier_is_stored_under_its_name_without_the_prefix() {
    assert_eq!(
        reread::<_, PlainField>(&RawField { r#type: 7 }).unwrap(),
        PlainField { kind: 7 }
    );
    assert_eq!(
        reread::<_, PlainVariant>(&RawVariant::r#Type(4)).unwrap(),
        PlainVariant::Kind(4)
    );
}

#[derive(Clone, Debug, PartialEq, Data)]
struct Point {
    x: u32,
    y: u32,
}

fn field_record(out: &mut Vec<u8>, tag: u16, value: &[u8]) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.push((value.len() as u8) << 1);
    out.extend_from_slice(value);
}

fn refused_as<T: Data + std::fmt::Debug>(stored: &[u8]) {
    let decoded = T::decode(&mut Decoder::new(stored, &[], None));
    assert!(
        matches!(decoded, Err(ferro::Error::Decode(_))),
        "{stored:02x?} decoded as {decoded:?}"
    );
    let visited = T::visit(&mut Decoder::new(stored, &[], None));
    assert!(
        matches!(visited, Err(ferro::Error::Decode(_))),
        "{stored:02x?} passed a visit"
    );
}

#[test]
fn fields_stored_out_of_order_twice_or_with_bytes_left_over_are_refused() {
    use ferro::codec::field_tag;
    let (x, y) = (field_tag("x"), field_tag("y"));
    assert!(x < y);
    let mut canonical = vec![2];
    field_record(&mut canonical, x, &[1]);
    field_record(&mut canonical, y, &[2]);
    assert_eq!(canonical, bytes(&Point { x: 1, y: 2 }));

    let mut swapped = vec![2];
    field_record(&mut swapped, y, &[2]);
    field_record(&mut swapped, x, &[1]);
    refused_as::<Point>(&swapped);

    let mut twice = vec![3];
    field_record(&mut twice, x, &[9]);
    field_record(&mut twice, x, &[1]);
    field_record(&mut twice, y, &[2]);
    refused_as::<Point>(&twice);

    let mut padded = vec![2];
    field_record(&mut padded, x, &[1, 0xEE]);
    field_record(&mut padded, y, &[2]);
    refused_as::<Point>(&padded);

    let mut flagged = vec![2];
    flagged.extend_from_slice(&x.to_le_bytes());
    flagged.extend_from_slice(&[(1 << 1) | 1, 0, 1]);
    field_record(&mut flagged, y, &[2]);
    refused_as::<Point>(&flagged);

    let mut unknown = vec![3];
    field_record(&mut unknown, x, &[1]);
    field_record(&mut unknown, x + 1, &[0xEE, 0xEE]);
    field_record(&mut unknown, y, &[2]);
    assert_eq!(
        Point::decode(&mut Decoder::new(&unknown, &[], None)).unwrap(),
        Point { x: 1, y: 2 }
    );
    Point::visit(&mut Decoder::new(&unknown, &[], None)).unwrap();
}

#[derive(Data, Clone, Debug, PartialEq)]
enum Kind1 {
    Plain,
    Dropped(u8),
    Fancy { level: u8 },
}

#[derive(Data, Clone, Debug, PartialEq)]
struct Post1 {
    id: u64,
    title: String,
    gone: Vec<u32>,
    blob: Vec<u8>,
    cents: u32,
    boxed: (u8, String),
    kind: Kind1,
    tag: u16,
}

#[derive(Data, Clone, Copy, Debug, PartialEq)]
#[ferro(transparent)]
struct Cents(u32);

#[derive(Data, Clone, Debug, PartialEq)]
#[ferro(removed("Dropped"))]
enum Kind2 {
    Added(u8),
    Fancy {
        #[ferro(default)]
        extra: u8,
        level: u8,
    },
    Plain,
}

#[derive(Data, Clone, Debug, PartialEq)]
#[ferro(removed("gone"))]
struct Post2 {
    kind: Kind2,
    #[ferro(rename = "title")]
    heading: String,
    id: u64,
    blob: Bytes,
    cents: Cents,
    boxed: Box<(u8, String)>,
    tag: std::sync::Arc<u16>,
    note: Option<String>,
    tags: Vec<String>,
    counts: std::collections::BTreeMap<u8, u8>,
    seen: BTreeSet<u8>,
    raw: Bytes,
    index: ferro::Map<u64, u64>,
    #[ferro(default)]
    count: u32,
    #[ferro(default = "seven")]
    lucky: u32,
    #[ferro(skip)]
    cached: u64,
}

#[test]
fn every_documented_evolution_step_still_reads_old_data() {
    let old = Post1 {
        id: 4,
        title: "hello".into(),
        gone: vec![1, 2, 3],
        blob: vec![9, 8],
        cents: 250,
        boxed: (1, "inside".into()),
        kind: Kind1::Fancy { level: 3 },
        tag: 300,
    };
    let stored = bytes(&old);
    Post2::visit(&mut Decoder::new(&stored, &[], None)).unwrap();
    let new: Post2 = reread(&old).unwrap();
    assert_eq!(new.kind, Kind2::Fancy { extra: 0, level: 3 });
    assert_eq!(new.heading, "hello");
    assert_eq!(new.id, 4);
    assert_eq!(&new.blob[..], [9, 8]);
    assert_eq!(new.cents, Cents(250));
    assert_eq!(*new.boxed, (1, "inside".to_string()));
    assert_eq!(*new.tag, 300);
    assert_eq!(new.note, None);
    assert!(new.tags.is_empty() && new.counts.is_empty() && new.seen.is_empty());
    assert!(new.raw.is_empty() && new.index.is_empty());
    assert_eq!((new.count, new.lucky, new.cached), (0, 7, 0));
    for kind in [Kind1::Plain, Kind1::Fancy { level: 1 }] {
        let stored = bytes(&kind);
        Kind2::visit(&mut Decoder::new(&stored, &[], None)).unwrap();
        reread::<_, Kind2>(&kind).unwrap();
    }
    assert!(matches!(
        reread::<_, Kind2>(&Kind1::Dropped(1)),
        Err(ferro::Error::Decode(_))
    ));
    let again = bytes(&new);
    let back = Post2::decode(&mut Decoder::new(&again, &[], None)).unwrap();
    assert_eq!(bytes(&back), again);
}
