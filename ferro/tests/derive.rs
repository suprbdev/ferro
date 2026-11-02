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
    assert_eq!(bytes(&Named { inner: raw.to_vec() }), bytes(&raw));
    roundtrip(Blob(raw.clone()));
    roundtrip(Named { inner: raw.to_vec() });
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
    let listed = |rows: ferro::table::IndexIter<Event>| -> Vec<u64> {
        rows.map(|row| row.id).collect()
    };
    assert_eq!(listed(table.by_tenant_created(&("a".to_string(), 20))), [2, 4]);
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
