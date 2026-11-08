mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use common::fast;
use ferro::codec::{Decoder, Encoder, HashSink, MAX_DEPTH, field_tag};
use ferro::{Bytes, Data, Db, Error, Map, Record, Result, Table};

const SMALL_STACK: usize = 2 << 20;
const HOSTILE: usize = 20_000;
const CHAINED: usize = 3_000;
const CHILD: &str = "FERRO_DEPTH_CHILD";

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u64,
    #[index]
    n: u32,
}

fn rows(seed: u32) -> Table<Row> {
    let mut table: Table<Row> = Table::new();
    for id in 0..3 {
        table.insert(Row {
            id,
            n: seed + id as u32,
        });
    }
    table
}

fn on_stack<R: Send + 'static>(size: usize, f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .stack_size(size)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

fn on_small_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    on_stack(SMALL_STACK, f)
}

fn encoded<T: Data>(value: &T) -> Vec<u8> {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder).unwrap();
    encoder.into_object().body
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[derive(Clone, Default)]
struct Raw(Vec<u8>);

impl Data for Raw {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.raw(&self.0);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        Ok(Raw(r.take(r.remaining())?.to_vec()))
    }
}

enum Part {
    Fixed(Vec<u8>),
    Inner(Vec<u8>),
}

struct Level {
    variant: Option<&'static str>,
    fields: Vec<(&'static str, Part)>,
}

impl Level {
    fn wrap(&self, inner: usize) -> (Vec<u8>, Vec<u8>) {
        let mut fields: Vec<_> = self.fields.iter().collect();
        fields.sort_by_key(|(name, _)| field_tag(name));
        let (mut before, mut after) = (Vec::new(), Vec::new());
        if let Some(variant) = self.variant {
            before.extend_from_slice(&field_tag(variant).to_le_bytes());
        }
        varint(&mut before, fields.len() as u64);
        let mut past = false;
        for (name, part) in fields {
            let out = if past { &mut after } else { &mut before };
            out.extend_from_slice(&field_tag(name).to_le_bytes());
            match part {
                Part::Fixed(bytes) => {
                    varint(out, (bytes.len() as u64) << 1);
                    out.extend_from_slice(bytes);
                }
                Part::Inner(head) => {
                    varint(out, ((head.len() + inner) as u64) << 1);
                    out.extend_from_slice(head);
                    past = true;
                }
            }
        }
        (before, after)
    }
}

trait Shape: Data + Default {
    const NAME: &'static str;

    fn chain(depth: usize, bottom: Option<Table<Row>>) -> Self;

    fn bottom(&mut self) -> &mut Option<Table<Row>>;

    fn level() -> Level;
}

fn forged<S: Shape>(depth: usize) -> Vec<u8> {
    let level = S::level();
    let bottom = encoded(&S::chain(0, None));
    let (mut befores, mut afters) = (Vec::with_capacity(depth), Vec::with_capacity(depth));
    let mut len = bottom.len();
    for _ in 0..depth {
        let (before, after) = level.wrap(len);
        len += before.len() + after.len();
        befores.push(before);
        afters.push(after);
    }
    let mut out = Vec::with_capacity(len);
    for before in befores.iter().rev() {
        out.extend_from_slice(before);
    }
    out.extend_from_slice(&bottom);
    for after in &afters {
        out.extend_from_slice(after);
    }
    out
}

fn deepest<S: Shape>(table: bool) -> usize {
    (0..=MAX_DEPTH + 2)
        .rev()
        .find(|depth| ferro::codec::try_hash_of(&S::chain(*depth, table.then(|| rows(0)))).is_ok())
        .unwrap()
}

#[derive(Data, Clone, Default)]
struct Opt {
    t: Option<Table<Row>>,
    next: Option<Box<Opt>>,
}

impl Drop for Opt {
    fn drop(&mut self) {
        let mut next = self.next.take();
        while let Some(mut node) = next {
            next = node.next.take();
        }
    }
}

impl Shape for Opt {
    const NAME: &'static str = "opt";

    fn chain(depth: usize, bottom: Option<Table<Row>>) -> Self {
        let mut node = Opt {
            t: bottom,
            next: None,
        };
        for _ in 0..depth {
            node = Opt {
                t: None,
                next: Some(Box::new(node)),
            };
        }
        node
    }

    fn bottom(&mut self) -> &mut Option<Table<Row>> {
        let mut at = self;
        while at.next.is_some() {
            at = at.next.as_mut().unwrap();
        }
        &mut at.t
    }

    fn level() -> Level {
        Level {
            variant: None,
            fields: vec![("t", Part::Fixed(vec![0])), ("next", Part::Inner(vec![1]))],
        }
    }
}

#[derive(Data, Clone, Default)]
struct Seq {
    t: Option<Table<Row>>,
    kids: Vec<Seq>,
}

impl Shape for Seq {
    const NAME: &'static str = "vec";

    fn chain(depth: usize, bottom: Option<Table<Row>>) -> Self {
        let mut node = Seq {
            t: bottom,
            kids: Vec::new(),
        };
        for _ in 0..depth {
            node = Seq {
                t: None,
                kids: vec![node],
            };
        }
        node
    }

    fn bottom(&mut self) -> &mut Option<Table<Row>> {
        let mut at = self;
        while !at.kids.is_empty() {
            at = &mut at.kids[0];
        }
        &mut at.t
    }

    fn level() -> Level {
        Level {
            variant: None,
            fields: vec![("t", Part::Fixed(vec![0])), ("kids", Part::Inner(vec![1]))],
        }
    }
}

#[derive(Data, Clone, Default)]
struct Branches {
    t: Option<Table<Row>>,
    kids: BTreeMap<u8, Branches>,
}

impl Shape for Branches {
    const NAME: &'static str = "btreemap";

    fn chain(depth: usize, bottom: Option<Table<Row>>) -> Self {
        let mut node = Branches {
            t: bottom,
            kids: BTreeMap::new(),
        };
        for _ in 0..depth {
            node = Branches {
                t: None,
                kids: BTreeMap::from([(7, node)]),
            };
        }
        node
    }

    fn bottom(&mut self) -> &mut Option<Table<Row>> {
        let mut at = self;
        while !at.kids.is_empty() {
            at = at.kids.get_mut(&7).unwrap();
        }
        &mut at.t
    }

    fn level() -> Level {
        Level {
            variant: None,
            fields: vec![
                ("t", Part::Fixed(vec![0])),
                ("kids", Part::Inner(vec![1, 7])),
            ],
        }
    }
}

#[derive(Data, Clone)]
enum Chain {
    Leaf { t: Option<Table<Row>> },
    More(Box<Chain>),
}

impl Default for Chain {
    fn default() -> Self {
        Chain::Leaf { t: None }
    }
}

impl Shape for Chain {
    const NAME: &'static str = "enum";

    fn chain(depth: usize, bottom: Option<Table<Row>>) -> Self {
        let mut node = Chain::Leaf { t: bottom };
        for _ in 0..depth {
            node = Chain::More(Box::new(node));
        }
        node
    }

    fn bottom(&mut self) -> &mut Option<Table<Row>> {
        let mut at = self;
        loop {
            match at {
                Chain::More(next) => at = next,
                Chain::Leaf { t } => return t,
            }
        }
    }

    fn level() -> Level {
        Level {
            variant: Some("More"),
            fields: vec![("0", Part::Inner(Vec::new()))],
        }
    }
}

#[derive(Data, Clone, Default)]
struct Pairs {
    t: Option<Table<Row>>,
    next: Option<Box<(u8, Pairs)>>,
}

impl Shape for Pairs {
    const NAME: &'static str = "tuple";

    fn chain(depth: usize, bottom: Option<Table<Row>>) -> Self {
        let mut node = Pairs {
            t: bottom,
            next: None,
        };
        for _ in 0..depth {
            node = Pairs {
                t: None,
                next: Some(Box::new((3, node))),
            };
        }
        node
    }

    fn bottom(&mut self) -> &mut Option<Table<Row>> {
        let mut at = self;
        while at.next.is_some() {
            at = &mut at.next.as_mut().unwrap().1;
        }
        &mut at.t
    }

    fn level() -> Level {
        Level {
            variant: None,
            fields: vec![
                ("t", Part::Fixed(vec![0])),
                ("next", Part::Inner(vec![1, 3])),
            ],
        }
    }
}

#[derive(Data, Clone, Default)]
struct Wide {
    a: u64,
    b: String,
    c: Vec<u32>,
    d: Option<String>,
    e: (u32, u64),
    f: bool,
    g: f64,
    h: i32,
    i: BTreeMap<u8, u8>,
    j: u128,
    k: char,
    l: [u16; 4],
    m: Bytes,
    n: i64,
    o: Vec<String>,
    p: (bool, u8),
    q: u64,
    r: String,
    s: Vec<u32>,
    u: Option<String>,
    v: (u32, u64),
    w: bool,
    x: f64,
    y: i32,
    z: BTreeMap<u8, u8>,
    t: Option<Table<Row>>,
    next: Option<Box<Wide>>,
}

impl Shape for Wide {
    const NAME: &'static str = "wide";

    fn chain(depth: usize, bottom: Option<Table<Row>>) -> Self {
        let mut node = Wide {
            t: bottom,
            ..Wide::default()
        };
        for _ in 0..depth {
            node = Wide {
                next: Some(Box::new(node)),
                ..Wide::default()
            };
        }
        node
    }

    fn bottom(&mut self) -> &mut Option<Table<Row>> {
        let mut at = self;
        while at.next.is_some() {
            at = at.next.as_mut().unwrap();
        }
        &mut at.t
    }

    fn level() -> Level {
        let wide = Wide::default();
        Level {
            variant: None,
            fields: vec![
                ("a", Part::Fixed(encoded(&wide.a))),
                ("b", Part::Fixed(encoded(&wide.b))),
                ("c", Part::Fixed(encoded(&wide.c))),
                ("d", Part::Fixed(encoded(&wide.d))),
                ("e", Part::Fixed(encoded(&wide.e))),
                ("f", Part::Fixed(encoded(&wide.f))),
                ("g", Part::Fixed(encoded(&wide.g))),
                ("h", Part::Fixed(encoded(&wide.h))),
                ("i", Part::Fixed(encoded(&wide.i))),
                ("j", Part::Fixed(encoded(&wide.j))),
                ("k", Part::Fixed(encoded(&wide.k))),
                ("l", Part::Fixed(encoded(&wide.l))),
                ("m", Part::Fixed(encoded(&wide.m))),
                ("n", Part::Fixed(encoded(&wide.n))),
                ("o", Part::Fixed(encoded(&wide.o))),
                ("p", Part::Fixed(encoded(&wide.p))),
                ("q", Part::Fixed(encoded(&wide.q))),
                ("r", Part::Fixed(encoded(&wide.r))),
                ("s", Part::Fixed(encoded(&wide.s))),
                ("u", Part::Fixed(encoded(&wide.u))),
                ("v", Part::Fixed(encoded(&wide.v))),
                ("w", Part::Fixed(encoded(&wide.w))),
                ("x", Part::Fixed(encoded(&wide.x))),
                ("y", Part::Fixed(encoded(&wide.y))),
                ("z", Part::Fixed(encoded(&wide.z))),
                ("t", Part::Fixed(vec![0])),
                ("next", Part::Inner(vec![1])),
            ],
        }
    }
}

fn refused_as_too_deep(outcome: Result<()>) {
    match outcome {
        Err(Error::Bundle(message)) => assert!(
            message.contains(&format!("nests deeper than {MAX_DEPTH} levels")),
            "{message}"
        ),
        other => panic!("expected the bundle to be refused as too deep, got {other:?}"),
    }
}

fn received<S: Data + Default>(receiver: &Db<S>, bundle: ferro::Bundle) -> Result<()> {
    let receiver = receiver.clone();
    on_small_stack(move || {
        receiver
            .receive("sender", &bundle, |_: &mut S, _, _| {
                ("merge".to_string(), ())
            })
            .map(drop)
    })
}

fn forged_bundle<S: Shape>(dir: &Path, depth: usize, receiver: &Db<S>) -> ferro::Bundle {
    let forger: Db<Raw> = fast()
        .open(dir.join(format!("forger-{depth}.ferro")))
        .unwrap();
    let bytes = forged::<S>(depth);
    forger.update(|root| *root = Raw(bytes)).unwrap();
    forger.bundle(&receiver.have().unwrap()).unwrap()
}

fn over_and_at_the_limit<S: Shape>() {
    let most = deepest::<S>(false);
    assert!(most >= MAX_DEPTH / 2 - 2, "{} nests {most} deep", S::NAME);
    for depth in [0, 1, 2, most] {
        assert_eq!(
            forged::<S>(depth),
            encoded(&S::chain(depth, None)),
            "{} at {depth}",
            S::NAME
        );
    }
    assert!(matches!(
        ferro::codec::try_hash_of(&S::chain(most + 1, None)),
        Err(Error::TooDeep)
    ));
    let over = forged::<S>(most + 1);
    let decoded = S::decode(&mut Decoder::new(&over, &[], None)).map(drop);
    assert!(matches!(decoded, Err(Error::Decode(_))), "{}", S::NAME);
    assert!(matches!(
        S::visit(&mut Decoder::new(&over, &[], None)),
        Err(Error::Decode(_))
    ));

    let dir = tempfile::tempdir().unwrap();
    let receiver: Db<S> = fast().open(dir.path().join("receiver.ferro")).unwrap();
    let bundle = forged_bundle::<S>(dir.path(), most + 1, &receiver);
    refused_as_too_deep(received(&receiver, bundle));
    assert_eq!(receiver.version(), 0);

    let most = deepest::<S>(true);
    let sender: Db<S> = fast().open(dir.path().join("sender.ferro")).unwrap();
    let writer = sender.clone();
    on_small_stack(move || {
        writer
            .update(|root| *root = S::chain(most, Some(rows(0))))
            .unwrap();
    });
    received(&receiver, sender.bundle(&receiver.have().unwrap()).unwrap()).unwrap();
    let writer = sender.clone();
    on_small_stack(move || {
        writer
            .update(|root| {
                root.bottom().as_mut().unwrap().insert(Row { id: 9, n: 9 });
            })
            .unwrap();
    });
    received(&receiver, sender.bundle(&receiver.have().unwrap()).unwrap()).unwrap();
    let mut landed: S = (*receiver.read()).clone();
    let table = landed.bottom().as_ref().unwrap();
    assert_eq!(table.len(), 4);
    assert_eq!(table.by_n(&9).len(), 1);
    let checker = receiver.clone();
    on_small_stack(move || checker.verify_typed().map(drop)).unwrap();

    let over = sender.update(|root| *root = S::chain(MAX_DEPTH + 1, None));
    assert!(matches!(over, Err(Error::TooDeep)), "{}", S::NAME);
}

#[test]
fn a_bundle_just_over_the_limit_is_refused_and_one_at_it_lands() {
    over_and_at_the_limit::<Opt>();
    over_and_at_the_limit::<Seq>();
    over_and_at_the_limit::<Branches>();
    over_and_at_the_limit::<Chain>();
    over_and_at_the_limit::<Pairs>();
    over_and_at_the_limit::<Wide>();
}

#[test]
fn the_limit_counts_each_box_vec_and_map() {
    assert_eq!(deepest::<Opt>(false), MAX_DEPTH);
    assert_eq!(deepest::<Chain>(false), MAX_DEPTH);
    assert_eq!(deepest::<Pairs>(false), MAX_DEPTH);
    assert_eq!(deepest::<Seq>(false), MAX_DEPTH - 1);
    assert_eq!(deepest::<Branches>(false), MAX_DEPTH - 1);
    assert_eq!(deepest::<Wide>(false), MAX_DEPTH - 1);
}

fn file_at_the_limit<S: Shape>() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("limit.ferro");
    let most = deepest::<S>(true);
    let writer: Db<S> = fast().open(&path).unwrap();
    on_small_stack(move || {
        let over = writer.update(|root| *root = S::chain(most + 1, Some(rows(5))));
        assert!(matches!(over, Err(Error::TooDeep)), "{}", S::NAME);
        writer
            .update(|root| *root = S::chain(most, Some(rows(5))))
            .unwrap();
        writer
            .update(|root| {
                root.bottom().as_mut().unwrap().insert(Row { id: 9, n: 9 });
            })
            .unwrap();
    });
    on_small_stack(move || {
        let db: Db<S> = fast().open(&path).unwrap();
        let mut root: S = (*db.read()).clone();
        let table = root.bottom().as_ref().unwrap();
        assert_eq!(table.len(), 4);
        assert_eq!(table.by_n(&5).len(), 1);
        db.verify_typed().unwrap();
    });
}

fn hostile_bundle<S: Shape>() {
    let dir = tempfile::tempdir().unwrap();
    let receiver: Db<S> = fast().open(dir.path().join("receiver.ferro")).unwrap();
    let bundle = forged_bundle::<S>(dir.path(), HOSTILE, &receiver);
    refused_as_too_deep(received(&receiver, bundle));
    assert_eq!(receiver.version(), 0);
}

#[derive(Data, Clone, Default)]
struct Shelf {
    items: Map<u32, Raw>,
}

#[derive(Data, Clone, Default)]
struct Stock {
    items: Map<u32, Opt>,
}

fn deep_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root.ferro");
    let forger: Db<Raw> = fast().open(&root).unwrap();
    forger
        .update(|raw| *raw = Raw(forged::<Opt>(HOSTILE)))
        .unwrap();
    drop(forger);
    let opened = on_small_stack(move || fast().open::<Opt>(&root).map(drop));
    match opened {
        Err(Error::Decode(message)) => assert!(message.contains("nests deeper"), "{message}"),
        other => panic!("expected the open to fail to decode, got {other:?}"),
    }

    let shelf = dir.path().join("shelf.ferro");
    let forger: Db<Shelf> = fast().open(&shelf).unwrap();
    forger
        .update(|shelf| {
            shelf.items.insert(1, Raw(encoded(&Opt::chain(100, None))));
            shelf.items.insert(2, Raw(forged::<Opt>(HOSTILE)));
        })
        .unwrap();
    drop(forger);
    on_small_stack(move || {
        let db: Db<Stock> = fast().open(&shelf).unwrap();
        let stock = db.read();
        assert!(stock.items.try_get(&1).unwrap().is_some());
        assert!(matches!(stock.items.try_get(&2), Err(Error::Decode(_))));
        let verified = db.verify_typed();
        assert!(
            verified
                .as_ref()
                .is_err_and(|error| error.to_string().contains("nests deeper")),
            "{verified:?}"
        );
    });
}

fn deep_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Opt> = fast().open(dir.path().join("commit.ferro")).unwrap();
    let writer = db.clone();
    let outcome = on_small_stack(move || writer.update(|root| *root = Opt::chain(HOSTILE, None)));
    assert!(matches!(outcome, Err(Error::TooDeep)), "{outcome:?}");
    assert_eq!(db.version(), 0);

    let nests: Db<Nest> = fast().open(dir.path().join("nests.ferro")).unwrap();
    let deep = on_stack(1 << 30, || {
        let mut node = Nest::default();
        for _ in 0..CHAINED {
            let mut kids = Map::new();
            kids.insert(1, node);
            node = Nest { t: None, kids };
        }
        node
    });
    let held = deep.clone();
    let outcome = on_small_stack(move || nests.update(|root| *root = held.clone()));
    assert!(matches!(outcome, Err(Error::TooDeep)), "{outcome:?}");
    on_stack(1 << 30, move || drop(deep));
}

#[derive(Data, Clone, Default)]
struct Nest {
    t: Option<Table<Row>>,
    kids: Map<u64, Nest>,
}

const FAR: u64 = 1 << 40;

fn grow_nests(db: &Db<Nest>, depth: usize, offset: u64, seed: u32) {
    db.update(|root| {
        root.kids.insert(
            offset,
            Nest {
                t: Some(rows(seed)),
                kids: Map::new(),
            },
        );
    })
    .unwrap();
    for level in 1..=depth as u64 {
        db.update(|root| {
            let below = (*root.kids.remove(&(offset + level - 1)).unwrap()).clone();
            let mut kids = Map::new();
            kids.insert(1, below);
            root.kids.insert(offset + level, Nest { t: None, kids });
        })
        .unwrap();
    }
}

fn nested_maps() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let sender: Db<Nest> = fast().open(dir.path().join("sender.ferro")).unwrap();
    grow_nests(&sender, CHAINED, 0, 1);
    grow_nests(&sender, CHAINED, FAR, 2);
    sender
        .update(|root| {
            let other = (*root.kids.remove(&(FAR + CHAINED as u64)).unwrap()).clone();
            root.kids.insert(CHAINED as u64, other);
        })
        .unwrap();
    let checker = sender.clone();
    on_small_stack(move || checker.verify_typed().map(drop)).unwrap();
    let receiver: Db<Nest> = fast().open(dir.path().join("receiver.ferro")).unwrap();
    received(&receiver, sender.bundle(&receiver.have().unwrap()).unwrap()).unwrap();
    assert_eq!(receiver.read().kids.len(), 1);
    let checker = receiver.clone();
    on_small_stack(move || checker.verify_typed().map(drop)).unwrap();
}

#[derive(Data, Record, Clone, Debug)]
struct Twig {
    #[key]
    id: u64,
    #[index]
    n: u64,
    sub: Table<Twig>,
}

#[derive(Data, Clone, Default)]
struct Woods {
    tree: Table<Twig>,
}

fn grow_twigs(db: &Db<Woods>, depth: usize, offset: u64) {
    db.update(|root| {
        let mut leaf: Table<Twig> = Table::new();
        leaf.insert(Twig {
            id: 1,
            n: offset,
            sub: Table::new(),
        });
        root.tree.insert(Twig {
            id: offset,
            n: offset,
            sub: leaf,
        });
    })
    .unwrap();
    for level in 1..=depth as u64 {
        db.update(|root| {
            let below = root.tree.remove(&(offset + level - 1)).unwrap();
            let mut sub: Table<Twig> = Table::new();
            sub.insert(Twig {
                id: 1,
                n: offset + level,
                sub: below.sub.clone(),
            });
            root.tree.insert(Twig {
                id: offset + level,
                n: level,
                sub,
            });
        })
        .unwrap();
    }
}

fn nested_tables() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let sender: Db<Woods> = fast().open(dir.path().join("sender.ferro")).unwrap();
    grow_twigs(&sender, CHAINED, 0);
    grow_twigs(&sender, CHAINED, FAR);
    sender
        .update(|root| {
            let other = root.tree.remove(&(FAR + CHAINED as u64)).unwrap();
            root.tree.insert(Twig {
                id: CHAINED as u64,
                n: 0,
                sub: other.sub.clone(),
            });
        })
        .unwrap();
    let checker = sender.clone();
    on_small_stack(move || checker.verify_typed().map(drop)).unwrap();
    let receiver: Db<Woods> = fast().open(dir.path().join("receiver.ferro")).unwrap();
    received(&receiver, sender.bundle(&receiver.have().unwrap()).unwrap()).unwrap();
    assert_eq!(receiver.read().tree.len(), 1);
    let checker = receiver.clone();
    on_small_stack(move || checker.verify_typed().map(drop)).unwrap();
}

#[derive(Data, Record, Clone, Debug)]
struct Bare {
    #[key]
    id: u64,
    n: u64,
    sub: Table<Bare>,
}

#[derive(Data, Clone, Default)]
struct BareWoods {
    tree: Table<Bare>,
}

fn stale_tables() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("stale.ferro");
    let db: Db<BareWoods> = fast().open(&path).unwrap();
    for level in 0..CHAINED as u64 {
        db.update(|root| {
            let mut tree: Table<Bare> = Table::new();
            tree.insert(Bare {
                id: 1,
                n: level,
                sub: root.tree.clone(),
            });
            root.tree = tree;
        })
        .unwrap();
    }
    drop(db);
    on_small_stack(move || {
        let db: Db<Woods> = fast().open(&path).unwrap();
        let mut tree = db.read().tree.clone();
        for level in (CHAINED as u64 - 40..CHAINED as u64).rev() {
            assert_eq!(tree.by_n(&level).len(), 1, "level {level}");
            assert_eq!(tree.by_n(&(level + 1)).len(), 0, "level {level}");
            tree = tree.get(&1).unwrap().sub.clone();
        }
    });
}

#[derive(Clone)]
struct Hand(Option<Box<Hand>>);

impl Data for Hand {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        match &self.0 {
            None => {
                w.u8(0);
                Ok(())
            }
            Some(next) => {
                w.u8(1);
                w.nested(|w| Hand::encode(next, w))
            }
        }
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        match r.u8()? {
            0 => Ok(Hand(None)),
            _ => Ok(Hand(Some(Box::new(r.nested(Hand::decode)?)))),
        }
    }
}

#[derive(Data, Clone)]
#[ferro(transparent)]
struct Through(Option<Box<Through>>);

fn hand_chain(depth: usize) -> Vec<u8> {
    let mut bytes = vec![1; depth];
    bytes.push(0);
    bytes
}

fn hand_written() {
    let bytes = hand_chain(HOSTILE);
    on_small_stack(move || {
        assert!(matches!(
            Hand::decode(&mut Decoder::new(&bytes, &[], None)),
            Err(Error::Decode(_))
        ));
        assert!(matches!(
            Through::decode(&mut Decoder::new(&bytes, &[], None)),
            Err(Error::Decode(_))
        ));
        assert!(matches!(
            Through::visit(&mut Decoder::new(&bytes, &[], None)),
            Err(Error::Decode(_))
        ));
    });
}

#[test]
fn hand_written_and_transparent_types_count_their_levels() {
    let at = hand_chain(MAX_DEPTH);
    assert!(Hand::decode(&mut Decoder::new(&at, &[], None)).is_ok());
    assert!(Through::decode(&mut Decoder::new(&at, &[], None)).is_ok());
    let over = hand_chain(MAX_DEPTH + 1);
    assert!(Hand::decode(&mut Decoder::new(&over, &[], None)).is_err());
    assert!(Through::decode(&mut Decoder::new(&over, &[], None)).is_err());
    let value = Through::decode(&mut Decoder::new(&at, &[], None)).unwrap();
    assert_eq!(encoded(&value), at);
    let deeper = Through(Some(Box::new(value)));
    assert!(matches!(
        ferro::codec::try_hash_of(&deeper),
        Err(Error::TooDeep)
    ));
    let value = Hand::decode(&mut Decoder::new(&at, &[], None)).unwrap();
    assert_eq!(encoded(&value), at);
    let deeper = Hand(Some(Box::new(value)));
    assert!(matches!(
        ferro::codec::try_hash_of(&deeper),
        Err(Error::TooDeep)
    ));
}

fn in_child(case: &str) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, case)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "case {case}: {:?}\n{stdout}{stderr}",
        out.status
    );
    assert!(
        stdout.contains(&format!("case {case} passed")),
        "{stdout}{stderr}"
    );
}

#[test]
fn child() {
    let Ok(case) = std::env::var(CHILD) else {
        return;
    };
    match case.as_str() {
        "bundle-opt" => hostile_bundle::<Opt>(),
        "bundle-vec" => hostile_bundle::<Seq>(),
        "bundle-btreemap" => hostile_bundle::<Branches>(),
        "bundle-enum" => hostile_bundle::<Chain>(),
        "bundle-tuple" => hostile_bundle::<Pairs>(),
        "bundle-wide" => hostile_bundle::<Wide>(),
        "file-opt" => file_at_the_limit::<Opt>(),
        "file-vec" => file_at_the_limit::<Seq>(),
        "file-btreemap" => file_at_the_limit::<Branches>(),
        "file-enum" => file_at_the_limit::<Chain>(),
        "file-tuple" => file_at_the_limit::<Pairs>(),
        "file-wide" => file_at_the_limit::<Wide>(),
        "deep-file" => deep_file(),
        "deep-commit" => deep_commit(),
        "nested-maps" => nested_maps(),
        "nested-tables" => nested_tables(),
        "stale-tables" => stale_tables(),
        "hand-written" => hand_written(),
        other => panic!("unknown case {other}"),
    }
    println!("case {case} passed");
}

const SHAPES: [&str; 6] = ["opt", "vec", "btreemap", "enum", "tuple", "wide"];

#[test]
fn a_deeply_nested_bundle_is_refused_without_running_out_of_stack() {
    for shape in SHAPES {
        in_child(&format!("bundle-{shape}"));
    }
}

#[test]
fn a_file_at_the_limit_opens_reads_and_verifies_on_a_small_stack() {
    for shape in SHAPES {
        in_child(&format!("file-{shape}"));
    }
}

#[test]
fn a_deeply_nested_file_fails_to_read_and_verify_without_running_out_of_stack() {
    in_child("deep-file");
}

#[test]
fn a_deeply_nested_commit_fails_with_too_deep() {
    in_child("deep-commit");
}

#[test]
fn maps_nested_in_map_values_pair_one_map_at_a_time() {
    in_child("nested-maps");
}

#[test]
fn tables_nested_in_rows_pair_one_table_at_a_time() {
    in_child("nested-tables");
}

#[test]
fn stale_indexes_of_nested_tables_rebuild_one_level_at_a_time() {
    in_child("stale-tables");
}

#[test]
fn hand_written_and_transparent_types_refuse_deep_input() {
    in_child("hand-written");
}
