mod common;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;

use common::{cases, fast, panic_text, quiet_ferro_panics, text};
use ferro::codec::{Decoder, Encoder, Object, Pairing, Tabled};
use ferro::{
    Bundle, Bytes, Data, Db, Error, Hash, List, Map, Outcome, Ptr, Record, Set, Snapshot, Summary,
    Table,
};
use proptest::prelude::*;
use proptest::test_runner::{TestRng, TestRunner};
use tempfile::TempDir;

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u64,
    #[unique]
    email: String,
    #[index]
    city: String,
    tags: Vec<String>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Total {
    sum: u64,
    items: u64,
}

impl Summary<u64, u64> for Total {
    fn of(_key: &u64, value: &u64) -> Self {
        Total {
            sum: *value,
            items: 1,
        }
    }

    fn combine(&self, other: &Self) -> Self {
        Total {
            sum: self.sum.wrapping_add(other.sum),
            items: self.items + other.items,
        }
    }
}

#[derive(Data, Clone, Default)]
struct World {
    names: Map<u64, String>,
    nested: Map<u64, Map<u64, u64>>,
    tags: Set<String>,
    events: List<u64>,
    rows: Table<Row>,
    blobs: Map<u64, Bytes>,
    totals: Map<u64, u64, Total>,
    note: Option<Box<(u64, String)>>,
}

fn row(id: u64, round: u64) -> Row {
    Row {
        id,
        email: format!("user{id}@{round}.example"),
        city: format!("city{}", (id + round) % 7),
        tags: (0..id % 3).map(|tag| format!("t{tag}")).collect(),
    }
}

fn fill(world: &mut World, round: u64) {
    for key in (0..300).step_by(1 + round as usize * 7) {
        world
            .names
            .insert(key, text(key + round, 20 + (key % 50) as usize));
        world.totals.insert(key, key * (round + 1));
    }
    for outer in (0..30).step_by(1 + round as usize * 3) {
        let inner = world.nested.get_or_insert_with(outer, Map::new);
        for key in 0..25 {
            inner.insert(key, outer * 1000 + key + round);
        }
    }
    for tag in (0..80).step_by(1 + round as usize * 5) {
        world.tags.insert(format!("tag{tag}-{round}"));
    }
    for event in 0..60 {
        world.events.push(event * 3 + round);
    }
    for id in (0..80).step_by(1 + round as usize * 4) {
        world.rows.insert(row(id, round));
    }
    for key in (0..12).step_by(1 + round as usize) {
        world
            .blobs
            .insert(key, Bytes::from(text(key * 31 + round, 700).into_bytes()));
    }
    world.note = Some(Box::new((round, format!("round {round}"))));
}

fn read_all(world: &World) -> usize {
    let mut seen = 0;
    assert_eq!(world.names.len(), world.names.iter().count());
    for (key, value) in world.names.iter() {
        assert_eq!(*world.names.get(&key).unwrap(), *value);
        seen += 1;
    }
    assert_eq!(world.nested.len(), world.nested.iter().count());
    for (key, inner) in world.nested.iter() {
        assert_eq!(inner.len(), inner.iter().count());
        for (deep, value) in inner.iter() {
            assert_eq!(*inner.get(&deep).unwrap(), *value);
            seen += 1;
        }
        assert!(world.nested.contains_key(&key));
    }
    assert_eq!(world.tags.len(), world.tags.iter().count());
    for tag in world.tags.iter() {
        assert!(world.tags.contains(&tag));
        seen += 1;
    }
    assert_eq!(world.events.len(), world.events.iter().count());
    for index in 0..world.events.len() {
        world.events.get(index).unwrap();
        seen += 1;
    }
    assert_eq!(world.rows.len(), world.rows.iter().count());
    for found in world.rows.iter() {
        world.rows.by_email(&found.email);
        seen += world.rows.by_city(&found.city).count();
    }
    seen += world.rows.by_city_counts().len();
    assert_eq!(world.blobs.len(), world.blobs.iter().count());
    for (key, blob) in world.blobs.iter() {
        assert_eq!(world.blobs.get(&key).unwrap().len(), blob.len());
        seen += 1;
    }
    assert_eq!(world.totals.len(), world.totals.iter().count());
    let mut folded: Option<Total> = None;
    for (key, value) in world.totals.iter() {
        assert_eq!(*world.totals.get(&key).unwrap(), *value);
        let entry = Total::of(&key, &value);
        folded = Some(match folded {
            Some(total) => total.combine(&entry),
            None => entry,
        });
        seen += 1;
    }
    assert_eq!(world.totals.summary(), folded);
    if let Some((middle, _)) = world.totals.nth(world.totals.len() / 2) {
        let below = world.totals.range_summary(..middle);
        let above = world.totals.range_summary(middle..);
        let joined = match (below, above) {
            (Some(below), Some(above)) => Some(below.combine(&above)),
            (below, above) => below.or(above),
        };
        assert_eq!(joined, folded);
    }
    seen + world.note.as_ref().map_or(0, |note| note.1.len())
}

fn merge(_: &mut World, _: &Snapshot<World>, _: &Snapshot<World>) -> (String, ()) {
    panic!("no merge expected")
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> &[u8] {
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        out
    }

    fn varint(&mut self) -> u64 {
        let mut value = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.take(1)[0];
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
        }
        value
    }

    fn hash(&mut self) -> Hash {
        self.take(20).try_into().unwrap()
    }
}

#[derive(Clone)]
struct Wire {
    body: Vec<u8>,
    links: Vec<(Hash, u64)>,
}

impl Wire {
    fn hash(&self) -> Hash {
        Object {
            body: self.body.clone(),
            links: self
                .links
                .iter()
                .map(|(hash, _)| Ptr {
                    hash: *hash,
                    offset: 0,
                })
                .collect(),
        }
        .hash()
    }
}

#[derive(Clone)]
struct Parsed {
    tip: (Hash, u64),
    bases: Vec<(Hash, u64)>,
    roots: Vec<u64>,
    hints: Vec<(u64, u64)>,
    objects: Vec<Wire>,
}

impl Parsed {
    fn of(bundle: &Bundle) -> Parsed {
        let mut r = Reader {
            bytes: bundle.as_bytes(),
            pos: 9,
        };
        assert_eq!(r.take(1), [1]);
        let tip = (r.hash(), r.varint());
        let bases = (0..r.varint()).map(|_| (r.hash(), r.varint())).collect();
        let roots = (0..r.varint()).map(|_| r.varint()).collect();
        let hints = (0..r.varint()).map(|_| (r.varint(), r.varint())).collect();
        let mut objects = Vec::new();
        for _ in 0..r.varint() {
            let flagged = r.varint();
            let stored = (flagged >> 1) as usize;
            let body = if flagged & 1 == 1 {
                let raw = r.varint() as usize;
                lz4_flex::block::decompress(r.take(stored), raw).unwrap()
            } else {
                r.take(stored).to_vec()
            };
            let links = (0..r.varint()).map(|_| (r.hash(), r.varint())).collect();
            objects.push(Wire { body, links });
        }
        assert_eq!(r.pos, r.bytes.len());
        Parsed {
            tip,
            bases,
            roots,
            hints,
            objects,
        }
    }

    fn encode(&self) -> Bundle {
        let mut out = b"FERROBDL".to_vec();
        out.extend_from_slice(&[1, 1]);
        out.extend_from_slice(&self.tip.0);
        varint(&mut out, self.tip.1);
        varint(&mut out, self.bases.len() as u64);
        for (hash, version) in &self.bases {
            out.extend_from_slice(hash);
            varint(&mut out, *version);
        }
        varint(&mut out, self.roots.len() as u64);
        for root in &self.roots {
            varint(&mut out, *root);
        }
        varint(&mut out, self.hints.len() as u64);
        for (parent, index) in &self.hints {
            varint(&mut out, *parent);
            varint(&mut out, *index);
        }
        varint(&mut out, self.objects.len() as u64);
        for object in &self.objects {
            varint(&mut out, (object.body.len() as u64) << 1);
            out.extend_from_slice(&object.body);
            varint(&mut out, object.links.len() as u64);
            for (hash, reference) in &object.links {
                out.extend_from_slice(hash);
                varint(&mut out, *reference);
            }
        }
        Bundle::from(out)
    }

    fn trees(&self) -> Vec<usize> {
        let at: HashMap<Hash, usize> = self
            .objects
            .iter()
            .enumerate()
            .map(|(slot, object)| (object.hash(), slot))
            .collect();
        let mut commits = HashSet::new();
        let mut stack = vec![self.tip.0];
        while let Some(hash) = stack.pop() {
            let Some(&slot) = at.get(&hash) else {
                continue;
            };
            if commits.insert(slot) {
                stack.extend(
                    self.objects[slot]
                        .links
                        .iter()
                        .skip(1)
                        .map(|(hash, _)| *hash),
                );
            }
        }
        (0..self.objects.len())
            .filter(|slot| !commits.contains(slot))
            .collect()
    }

    fn targets(&self) -> Vec<(Hash, u64)> {
        let mut targets: Vec<(Hash, u64)> = self
            .objects
            .iter()
            .flat_map(|object| object.links.iter().copied())
            .collect();
        targets.extend(self.objects.iter().map(|object| (object.hash(), 0)));
        targets.sort();
        targets.dedup();
        targets
    }

    fn rehash(&mut self, old: Vec<Hash>) {
        let mut renamed: HashMap<Hash, Hash> = HashMap::new();
        for (object, before) in self.objects.iter_mut().zip(old) {
            for (hash, _) in &mut object.links {
                if let Some(new) = renamed.get(hash) {
                    *hash = *new;
                }
            }
            let after = object.hash();
            if after != before {
                renamed.insert(before, after);
            }
        }
        if let Some(new) = renamed.get(&self.tip.0) {
            self.tip.0 = *new;
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Mutation {
    Flip(usize, usize, u8),
    Set(usize, usize, u8),
    Cut(usize, usize),
    Grow(usize, usize, u8),
    Retarget(usize, usize, usize),
    Drop(usize, usize),
    Swap(usize, usize, usize),
}

fn mutation() -> impl Strategy<Value = Mutation> {
    let index = || 0..usize::MAX;
    prop_oneof![
        3 => (index(), index(), 1..=255u8).prop_map(|(a, b, c)| Mutation::Flip(a, b, c)),
        2 => (index(), index(), any::<u8>()).prop_map(|(a, b, c)| Mutation::Set(a, b, c)),
        1 => (index(), index()).prop_map(|(a, b)| Mutation::Cut(a, b)),
        1 => (index(), index(), any::<u8>()).prop_map(|(a, b, c)| Mutation::Grow(a, b, c)),
        3 => (index(), index(), index()).prop_map(|(a, b, c)| Mutation::Retarget(a, b, c)),
        1 => (index(), index()).prop_map(|(a, b)| Mutation::Drop(a, b)),
        1 => (index(), index(), index()).prop_map(|(a, b, c)| Mutation::Swap(a, b, c)),
    ]
}

fn apply(
    parsed: &mut Parsed,
    trees: &[usize],
    targets: &[(Hash, u64)],
    mutation: Mutation,
) -> bool {
    let pick = |n: usize| trees[n % trees.len()];
    match mutation {
        Mutation::Flip(object, at, mask) | Mutation::Set(object, at, mask) => {
            let body = &mut parsed.objects[pick(object)].body;
            if body.is_empty() {
                return false;
            }
            let at = at % body.len();
            let before = body[at];
            body[at] = match mutation {
                Mutation::Flip(..) => before ^ mask,
                _ => mask,
            };
            body[at] != before
        }
        Mutation::Cut(object, at) => {
            let body = &mut parsed.objects[pick(object)].body;
            let at = at % (body.len() + 1);
            let before = body.len();
            body.truncate(at);
            body.len() != before
        }
        Mutation::Grow(object, at, value) => {
            let body = &mut parsed.objects[pick(object)].body;
            let at = at % (body.len() + 1);
            body.insert(at, value);
            true
        }
        Mutation::Retarget(object, link, target) => {
            let links = &mut parsed.objects[pick(object)].links;
            if links.is_empty() {
                return false;
            }
            let at = link % links.len();
            let before = links[at];
            links[at] = targets[target % targets.len()];
            links[at] != before
        }
        Mutation::Drop(object, link) => {
            let links = &mut parsed.objects[pick(object)].links;
            if links.is_empty() {
                return false;
            }
            links.remove(link % links.len());
            true
        }
        Mutation::Swap(object, a, b) => {
            let links = &mut parsed.objects[pick(object)].links;
            if links.len() < 2 {
                return false;
            }
            let (a, b) = (a % links.len(), b % links.len());
            links.swap(a, b);
            links[a] != links[b]
        }
    }
}

fn mutated(honest: &Parsed, mutations: &[Mutation]) -> Option<Bundle> {
    let trees = honest.trees();
    let targets = honest.targets();
    let mut parsed = honest.clone();
    let old: Vec<Hash> = parsed.objects.iter().map(Wire::hash).collect();
    let mut changed = false;
    for mutation in mutations {
        changed |= apply(&mut parsed, &trees, &targets, *mutation);
    }
    if !changed {
        return None;
    }
    parsed.rehash(old);
    Some(parsed.encode())
}

struct Target {
    _dir: TempDir,
    path: PathBuf,
    image: Vec<u8>,
    db: Option<Db<World>>,
    accepted: usize,
    refused: usize,
}

impl Target {
    fn db(&self) -> &Db<World> {
        self.db.as_ref().unwrap()
    }

    fn restore(&mut self) {
        self.db = None;
        fs::write(&self.path, &self.image).unwrap();
        self.db = Some(fast().open(&self.path).unwrap());
    }
}

fn setup() -> (Target, Parsed) {
    let dir = tempfile::tempdir().unwrap();
    let a: Db<World> = fast().open(dir.path().join("a.ferro")).unwrap();
    let path = dir.path().join("b.ferro");
    let b: Db<World> = fast().open(&path).unwrap();
    a.update(|world| fill(world, 0)).unwrap();
    assert_eq!(
        b.pull(&a, "a", merge).unwrap().outcome,
        Outcome::FastForward
    );
    a.update(|world| fill(world, 1)).unwrap();
    a.update(|world| {
        world.names.remove(&3);
        world.nested.remove(&4);
        world.rows.remove(&5);
        world.events.truncate(50);
    })
    .unwrap();
    let bundle = a.bundle(&b.have().unwrap()).unwrap();
    drop(b);
    let image = fs::read(&path).unwrap();
    let target = Target {
        _dir: dir,
        db: Some(fast().open(&path).unwrap()),
        path,
        image,
        accepted: 0,
        refused: 0,
    };
    (target, Parsed::of(&bundle))
}

fn attempt(target: &mut Target, bundle: &Bundle) -> std::result::Result<bool, String> {
    let before = target.db().head_hash();
    match target.db().receive("a", bundle, merge) {
        Err(Error::Bundle(_)) => {
            if target.db().head_hash() != before {
                return Err("a refused bundle moved the branch".into());
            }
            target.refused += 1;
            return Ok(false);
        }
        Err(other) => return Err(format!("unexpected error {other}")),
        Ok(_) => {}
    }
    target.accepted += 1;
    let db = target.db();
    let read = panic::catch_unwind(AssertUnwindSafe(|| {
        read_all(&db.read());
        read_all(&db.branch("a").unwrap().read());
        db.verify_typed().map(drop)
    }));
    let outcome = match read {
        Ok(Ok(())) => Ok(true),
        Ok(Err(error)) => Err(format!(
            "typed verify failed after receive accepted: {error}"
        )),
        Err(payload) => Err(format!("a read panicked: {}", panic_text(&*payload))),
    };
    target.restore();
    outcome
}

#[test]
fn mutated_bundles_are_refused_or_read_back_whole() {
    quiet_ferro_panics();
    let (target, honest) = setup();
    assert!(!honest.bases.is_empty() && !honest.hints.is_empty());
    let target = RefCell::new(target);
    assert_eq!(
        attempt(&mut target.borrow_mut(), &honest.encode()),
        Ok(true)
    );
    let config = ProptestConfig {
        cases: cases(512),
        failure_persistence: None,
        ..ProptestConfig::default()
    };
    let rng = TestRng::deterministic_rng(config.rng_algorithm);
    let mut runner = TestRunner::new_with_rng(config, rng);
    runner
        .run(&prop::collection::vec(mutation(), 1..=2), |mutations| {
            let Some(bundle) = mutated(&honest, &mutations) else {
                return Ok(());
            };
            let result = attempt(&mut target.borrow_mut(), &bundle);
            prop_assert!(result.is_ok(), "{mutations:?}: {}", result.unwrap_err());
            Ok(())
        })
        .unwrap();
    let target = target.into_inner();
    println!(
        "{} mutated bundles accepted, {} refused",
        target.accepted, target.refused
    );
    assert!(target.refused > 0);
}

const MARKER: &str = "a value no other object holds";

fn received_after(value: &str, edit: impl Fn(&[u8]) -> Option<Vec<u8>>) -> ferro::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let sender: Db<Map<u64, String>> = fast().open(dir.path().join("a.ferro")).unwrap();
    let receiver: Db<Map<u64, String>> = fast().open(dir.path().join("b.ferro")).unwrap();
    sender
        .update(|map| {
            map.insert(7, value.to_string());
        })
        .unwrap();
    let mut parsed = Parsed::of(&sender.bundle(&receiver.have().unwrap()).unwrap());
    let old: Vec<Hash> = parsed.objects.iter().map(Wire::hash).collect();
    let mut edited = 0;
    for object in &mut parsed.objects {
        if let Some(body) = edit(&object.body) {
            object.body = body;
            edited += 1;
        }
    }
    assert!(edited <= 1);
    parsed.rehash(old);
    receiver.receive("a", &parsed.encode(), |_, _, _| -> (String, ()) {
        panic!("no merge expected")
    })?;
    let ours = receiver.read();
    let theirs = sender.read();
    assert_eq!(ours.root_hash(), theirs.root_hash());
    receiver.verify_typed().map(drop)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[test]
fn a_bundle_holding_a_value_in_a_form_no_encoder_writes_is_refused() {
    received_after(MARKER, |_| None).unwrap();
    let mut length = vec![MARKER.len() as u8];
    length.extend_from_slice(MARKER.as_bytes());
    let overlong = received_after(MARKER, |body| {
        let at = find(body, &length)?;
        let mut out = body[..at].to_vec();
        out.extend_from_slice(&[MARKER.len() as u8 | 0x80, 0x00]);
        out.extend_from_slice(&body[at + 1..]);
        Some(out)
    });
    assert!(matches!(overlong, Err(Error::Bundle(_))), "{overlong:?}");
    let trailing = received_after(MARKER, |body| {
        find(body, MARKER.as_bytes())?;
        let mut out = body.to_vec();
        out.push(0);
        Some(out)
    });
    assert!(matches!(trailing, Err(Error::Bundle(_))), "{trailing:?}");
}

#[test]
fn a_bundle_holding_a_spilled_value_in_a_form_no_encoder_writes_is_refused() {
    let value = MARKER.repeat(30);
    let canonical = {
        let mut sink = ferro::codec::HashSink;
        let mut encoder = ferro::codec::Encoder::new(&mut sink);
        value.encode(&mut encoder).unwrap();
        encoder.into_object().body
    };
    received_after(&value, |_| None).unwrap();
    let overlong = received_after(&value, |body| {
        (body == canonical.as_slice()).then(|| {
            let mut out = canonical.clone();
            out.splice(1..2, [canonical[1] | 0x80, 0x00]);
            out
        })
    });
    assert!(matches!(overlong, Err(Error::Bundle(_))), "{overlong:?}");
    let trailing = received_after(&value, |body| {
        (body == canonical.as_slice()).then(|| {
            let mut out = canonical.clone();
            out.push(0);
            out
        })
    });
    assert!(matches!(trailing, Err(Error::Bundle(_))), "{trailing:?}");
}

type Tagged = Map<u64, BTreeSet<String>>;

fn set_of(items: [&str; 2]) -> Vec<u8> {
    let mut out = vec![2];
    for item in items {
        out.push(item.len() as u8);
        out.extend_from_slice(item.as_bytes());
    }
    out
}

fn tags_landed(stored: &[u8]) -> (TempDir, Db<Tagged>, Db<Tagged>, ferro::Result<()>) {
    let dir = tempfile::tempdir().unwrap();
    let sender: Db<Tagged> = fast().open(dir.path().join("a.ferro")).unwrap();
    let receiver: Db<Tagged> = fast().open(dir.path().join("b.ferro")).unwrap();
    sender
        .update(|map| {
            map.insert(7, BTreeSet::from(["tag-a".into(), "tag-b".into()]));
        })
        .unwrap();
    let canonical = set_of(["tag-a", "tag-b"]);
    let mut parsed = Parsed::of(&sender.bundle(&receiver.have().unwrap()).unwrap());
    let old: Vec<Hash> = parsed.objects.iter().map(Wire::hash).collect();
    let mut edited = 0;
    for object in &mut parsed.objects {
        if let Some(at) = find(&object.body, &canonical) {
            object
                .body
                .splice(at..at + canonical.len(), stored.iter().copied());
            edited += 1;
        }
    }
    assert_eq!(edited, 1);
    parsed.rehash(old);
    let landed = receiver
        .receive("a", &parsed.encode(), |_, _, _| -> (String, ()) {
            panic!("no merge expected")
        })
        .map(drop);
    (dir, sender, receiver, landed)
}

#[test]
fn a_bundle_holding_btree_keys_out_of_order_lands_and_one_holding_a_key_twice_is_refused() {
    let (_dir, sender, receiver, landed) = tags_landed(&set_of(["tag-b", "tag-a"]));
    landed.unwrap();
    receiver.verify_typed().unwrap();
    let (ours, theirs) = (receiver.read(), sender.read());
    assert_eq!(ours.get(&7).as_deref(), theirs.get(&7).as_deref());
    assert_ne!(ours.root_hash(), theirs.root_hash());
    receiver
        .update(|map| {
            let tags = (*map.get(&7).unwrap()).clone();
            map.insert(7, tags);
        })
        .unwrap();
    assert_eq!(receiver.read().root_hash(), sender.read().root_hash());
    receiver.verify_typed().unwrap();
    let (_dir, _, receiver, landed) = tags_landed(&set_of(["tag-a", "tag-a"]));
    assert!(matches!(landed, Err(Error::Bundle(_))), "{landed:?}");
    assert!(receiver.read().is_empty());
}

thread_local! {
    static KEY_READS: Cell<u64> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CountedKey(u8);

impl Data for CountedKey {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        KEY_READS.set(KEY_READS.get() + 1);
        Ok(CountedKey(u8::decode(r)?))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Nest<const DESCENDING: bool>(BTreeMap<CountedKey, Nest<DESCENDING>>);

impl<const DESCENDING: bool> Data for Nest<DESCENDING> {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.nested(|w| {
            w.varint(self.0.len() as u64);
            let entries: Vec<_> = match DESCENDING {
                true => self.0.iter().rev().collect(),
                false => self.0.iter().collect(),
            };
            for (key, value) in entries {
                w.item(|w| {
                    key.encode(w)?;
                    value.encode(w)
                })?;
            }
            Ok(())
        })
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Nest(BTreeMap::decode(r)?))
    }

    fn visit(r: &mut Decoder<'_>) -> ferro::Result<()> {
        BTreeMap::<CountedKey, Nest<DESCENDING>>::visit(r)
    }
}

fn nest<const DESCENDING: bool>(depth: u32) -> Nest<DESCENDING> {
    if depth == 0 {
        return Nest::default();
    }
    Nest(BTreeMap::from([
        (CountedKey(0), nest(depth - 1)),
        (CountedKey(1), nest(depth - 1)),
    ]))
}

fn keys_read_landing<const DESCENDING: bool>(depth: u32) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let sender: Db<Nest<DESCENDING>> = fast().open(dir.path().join("a.ferro")).unwrap();
    let receiver: Db<Nest<DESCENDING>> = fast().open(dir.path().join("b.ferro")).unwrap();
    sender.update(|root| *root = nest(depth)).unwrap();
    let bundle = sender.bundle(&receiver.have().unwrap()).unwrap();
    KEY_READS.set(0);
    receiver
        .receive("a", &bundle, |_, _, _| -> (String, ()) {
            panic!("no merge expected")
        })
        .unwrap();
    let read = KEY_READS.get();
    assert_eq!(*receiver.read(), nest::<DESCENDING>(depth));
    read
}

#[test]
fn a_bundle_of_maps_out_of_order_at_every_level_lands_reading_each_key_a_bounded_number_of_times() {
    let ascending = keys_read_landing::<false>(10);
    let descending = keys_read_landing::<true>(10);
    assert!(
        descending <= 2 * ascending,
        "landing read {descending} keys out of order against {ascending} in order"
    );
}

#[derive(Clone, Debug, Default)]
struct Reversed<V>(BTreeMap<u8, V>);

impl<V: Data> Data for Reversed<V> {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.nested(|w| {
            w.varint(self.0.len() as u64);
            for (key, value) in self.0.iter().rev() {
                w.item(|w| {
                    key.encode(w)?;
                    value.encode(w)
                })?;
            }
            Ok(())
        })
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Reversed(BTreeMap::decode(r)?))
    }

    fn visit(r: &mut Decoder<'_>) -> ferro::Result<()> {
        BTreeMap::<u8, V>::visit(r)
    }

    fn tables(t: &mut Tabled) {
        BTreeMap::<u8, V>::tables(t)
    }

    fn pair(
        old: &mut Decoder<'_>,
        new: &mut Decoder<'_>,
        pairing: &mut Pairing<'_>,
    ) -> ferro::Result<()> {
        BTreeMap::<u8, V>::pair(old, new, pairing)
    }
}

fn reversed_rows(rng: &mut common::Rng, slot: u8, count: u64) -> Table<Row> {
    let mut table: Table<Row> = Table::new();
    for id in 0..count {
        table.insert(Row {
            id,
            email: format!("{slot}-{id}@x"),
            city: format!("c{}", rng.below(4)),
            tags: vec![text(rng.next(), 1 + rng.below(700) as usize)],
        });
    }
    table
}

fn landed_whole<T: Data>(sender: &Db<T>, receiver: &Db<T>) {
    let bundle = sender.bundle(&receiver.have().unwrap()).unwrap();
    receiver
        .receive("a", &bundle, |_, _, _| -> (String, ()) {
            panic!("no merge expected")
        })
        .unwrap();
    receiver.verify_typed().unwrap();
}

#[test]
fn btrees_out_of_order_holding_tables_and_maps_land_and_verify_through_many_commits() {
    type Shelves = Map<u64, Reversed<(Table<Row>, Map<u32, String>, Reversed<Table<Row>>)>>;
    let dir = tempfile::tempdir().unwrap();
    let sender: Db<Shelves> = fast().open(dir.path().join("a.ferro")).unwrap();
    let receiver: Db<Shelves> = fast().open(dir.path().join("b.ferro")).unwrap();
    let rooted: Db<Reversed<Table<Row>>> = fast().open(dir.path().join("c.ferro")).unwrap();
    let rooted_receiver: Db<Reversed<Table<Row>>> =
        fast().open(dir.path().join("d.ferro")).unwrap();
    let mut rng = common::Rng::new(5);
    for round in 0..6u64 {
        sender
            .update(|shelves| {
                for shelf in 0..4u64 {
                    let mut held = shelves
                        .get(&shelf)
                        .map(|held| (*held).clone())
                        .unwrap_or_default();
                    let slot = rng.below(5) as u8 * 3;
                    let (table, map, inner) = held.0.entry(slot).or_default();
                    let count = rng.below(30);
                    *table = reversed_rows(&mut rng, slot, count);
                    table.retain(|row| row.id % 5 != round % 5);
                    map.insert(rng.below(50) as u32, text(rng.next(), 600));
                    let at = rng.below(3) as u8;
                    let count = rng.below(8);
                    inner.0.insert(at, reversed_rows(&mut rng, at, count));
                    shelves.insert(shelf, held);
                }
            })
            .unwrap();
        rooted
            .update(|root| {
                let slot = rng.below(10) as u8;
                match rng.below(3) {
                    0 => {
                        root.0.remove(&slot);
                    }
                    _ => {
                        let count = rng.below(30);
                        root.0.insert(slot, reversed_rows(&mut rng, slot, count));
                    }
                }
            })
            .unwrap();
        sender.verify_typed().unwrap();
        landed_whole(&sender, &receiver);
        landed_whole(&rooted, &rooted_receiver);
        for (sent, got) in sender.read().iter().zip(receiver.read().iter()) {
            assert_eq!(sent.0, got.0, "round {round}");
            for ((a, x), (b, y)) in sent.1.0.iter().zip(got.1.0.iter()) {
                assert_eq!(a, b, "round {round}");
                assert_eq!((&x.0, &x.1), (&y.0, &y.1), "round {round}");
            }
        }
        let (sent, got) = (rooted.read(), rooted_receiver.read());
        assert!(sent.0.iter().eq(got.0.iter()), "round {round}");
    }
}
