mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::ops::Bound;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{Rng, cases, fast, text, uncached};
use ferro::codec::{Decoder, Encoder, HashSink};
use ferro::{Bytes, Data, Db, List, Map, Set};
use proptest::prelude::*;

static RANGE_DECODES: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq)]
struct RangeCounted(u64);

impl Data for RangeCounted {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        RANGE_DECODES.fetch_add(1, Ordering::Relaxed);
        Ok(RangeCounted(u64::decode(r)?))
    }
}

fn filled(path: &Path, n: u64) -> Db<Map<u64, u64>> {
    let db = uncached().open::<Map<u64, u64>>(path).unwrap();
    db.update(|root| root.extend((0..n).map(|key| (key, key))))
        .unwrap();
    db
}

#[test]
fn remove_range_writes_a_path_not_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let n = 200_000;
    let db = filled(&dir.path().join("w.ferro"), n);
    let before = db.size_on_disk();
    let removed = db.update(|root| root.remove_range(1_000..199_000)).unwrap();
    assert_eq!(removed, 198_000);
    let written = db.size_on_disk() - before;
    assert!(written < before / 100, "wrote {written} of {before} bytes");
    let snapshot = db.read();
    assert_eq!(snapshot.len(), 2_000);
    assert_eq!(snapshot.iter().nth(999).map(|pair| pair.0), Some(999));
    assert_eq!(snapshot.iter().nth(1000).map(|pair| pair.0), Some(199_000));
    db.verify().unwrap();
}

#[test]
fn remove_range_never_reads_the_removed_subtrees() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.ferro");
    let n = 200_000;
    let size = filled(&path, n).size_on_disk();
    let mut file = OpenOptions::new().write(true).open(&path).unwrap();
    let garbage = vec![0xa5u8; (size / 5) as usize];
    file.seek(SeekFrom::Start(size * 2 / 5)).unwrap();
    file.write_all(&garbage).unwrap();
    drop(file);
    let db = uncached().open::<Map<u64, u64>>(&path).unwrap();
    let removed = db
        .update(|root| root.remove_range(n / 8..n - n / 8))
        .unwrap();
    assert_eq!(removed as u64, n - n / 4);
    let snapshot = db.read();
    assert_eq!(snapshot.len() as u64, n / 4);
    assert_eq!(snapshot.get(&(n / 8 - 1)).as_deref(), Some(&(n / 8 - 1)));
    assert_eq!(snapshot.get(&(n / 8)), None);
    assert_eq!(snapshot.get(&(n - n / 8)).as_deref(), Some(&(n - n / 8)));
}

#[test]
fn range_moves_decode_a_path_of_values() {
    let dir = tempfile::tempdir().unwrap();
    let db = uncached()
        .open::<Map<u64, RangeCounted>>(dir.path().join("d.ferro"))
        .unwrap();
    let n = 200_000u64;
    db.update(|root| root.extend((0..n).map(|key| (key, RangeCounted(key)))))
        .unwrap();
    let start = RANGE_DECODES.load(Ordering::Relaxed);
    db.update(|root| {
        let mut tail = root.split_off(&(n / 2));
        assert_eq!(tail.len() as u64, n / 2);
        assert_eq!(root.len() as u64, n / 2);
        assert_eq!(root.remove_range(10..n / 2 - 10) as u64, n / 2 - 20);
        root.append(&mut tail);
        assert!(tail.is_empty());
    })
    .unwrap();
    let cost = RANGE_DECODES.load(Ordering::Relaxed) - start;
    assert!(cost < 2_000, "decoded {cost} values");
}

#[test]
fn empty_range_leaves_the_tree_clean() {
    let dir = tempfile::tempdir().unwrap();
    let db = filled(&dir.path().join("e.ferro"), 10_000);
    let version = db.version();
    let size = db.size_on_disk();
    db.update(|root| {
        assert_eq!(root.remove_range(20_000..), 0);
        assert_eq!(root.remove_range(..0), 0);
        assert_eq!(root.remove_range(50..50), 0);
        assert!(!root.is_dirty());
        assert_eq!(root.take_range(30_000..).len(), 0);
        assert!(!root.is_dirty());
    })
    .unwrap();
    assert_eq!(db.version(), version);
    assert_eq!(db.size_on_disk(), size);
}

#[test]
fn split_off_at_any_edge_writes_nothing_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = filled(&dir.path().join("s.ferro"), 10_000);
    let version = db.version();
    let size = db.size_on_disk();
    db.update(|root| {
        assert!(root.split_off(&10_000).is_empty());
    })
    .unwrap();
    assert_eq!(db.version(), version);
    assert_eq!(db.size_on_disk(), size);
    db.update(|root| {
        let mut tail = root.split_off(&5_000);
        root.append(&mut tail);
    })
    .unwrap();
    assert_eq!(db.version(), version);
}

#[test]
fn a_cut_that_moves_nothing_adds_no_bytes_beside_a_real_change() {
    type Root = (Map<u64, u64>, u64);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("o.ferro");
    let open = || fast().open::<Root>(&path).unwrap();
    open()
        .update(|root| root.0.extend((0..50_000).map(|key| (key, key))))
        .unwrap();
    let plain = {
        let db = open();
        let before = db.size_on_disk();
        db.update(|root| root.1 += 1).unwrap();
        db.size_on_disk() - before
    };
    let cut = {
        let db = open();
        let before = db.size_on_disk();
        db.update(|root| {
            root.1 += 1;
            assert!(root.0.split_off(&50_000).is_empty());
        })
        .unwrap();
        db.size_on_disk() - before
    };
    assert_eq!(cut, plain);
}

#[test]
fn append_of_a_stored_map_onto_a_new_map_keeps_its_store() {
    let dir = tempfile::tempdir().unwrap();
    let db = filled(&dir.path().join("n.ferro"), 5_000);
    let mut stored = (*db.read()).clone();
    let mut fresh: Map<u64, u64> = (5_000..5_100).map(|key| (key, key)).collect();
    let wanted: Vec<u64> = (0..5_100).collect();
    fresh.append(&mut stored);
    assert!(stored.is_empty());
    assert_eq!(fresh.keys().collect::<Vec<u64>>(), wanted);
    let mut empty = Map::new();
    empty.append(&mut fresh);
    assert!(fresh.is_empty());
    assert_eq!(empty.keys().collect::<Vec<u64>>(), wanted);
    db.update(|root| *root = empty.clone()).unwrap();
    assert_eq!(db.read().len(), 5_100);
    db.verify().unwrap();
}

fn across_stores(ours: &BTreeMap<u32, String>, theirs: &BTreeMap<u32, String>) {
    let dir = tempfile::tempdir().unwrap();
    let a = fast()
        .open::<Map<u32, String>>(dir.path().join("a.ferro"))
        .unwrap();
    let b = uncached()
        .open::<Map<u32, String>>(dir.path().join("b.ferro"))
        .unwrap();
    a.update(|root| root.extend(ours.iter().map(|(k, v)| (*k, v.clone()))))
        .unwrap();
    b.update(|root| root.extend(theirs.iter().map(|(k, v)| (*k, v.clone()))))
        .unwrap();
    let mut wanted = ours.clone();
    wanted.extend(theirs.iter().map(|(k, v)| (*k, v.clone())));
    let mut other = (*b.read()).clone();
    a.update(|root| {
        root.append(&mut other);
        assert!(other.is_empty());
    })
    .unwrap();
    let snapshot = a.read();
    let got: BTreeMap<u32, String> = snapshot.iter().map(|(k, v)| (k, (*v).clone())).collect();
    assert_eq!(got, wanted);
    let direct: Map<u32, String> = wanted.iter().map(|(k, v)| (*k, v.clone())).collect();
    assert_eq!(snapshot.root_hash(), direct.root_hash());
    a.verify().unwrap();
}

fn window(low: u32, high: u32, count: usize) -> impl Strategy<Value = BTreeMap<u32, String>> {
    prop::collection::btree_map(low..high, (any::<u64>(), 0..700usize), 1..count).prop_map(
        |entries| {
            entries
                .into_iter()
                .map(|(key, (salt, len))| (key, text(salt, len)))
                .collect()
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(100), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn append_across_stores_copies_disjoint_maps(ours in window(0, 500, 60), theirs in window(500, 1000, 60)) {
        across_stores(&ours, &theirs);
        across_stores(&theirs, &ours);
    }

    #[test]
    fn append_across_stores_copies_overlapping_maps(ours in window(0, 300, 60), theirs in window(0, 300, 60)) {
        across_stores(&ours, &theirs);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(500), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn set_ranges_match_model(
        items in prop::collection::btree_set(0..2_000u32, 0..300),
        more in prop::collection::btree_set(0..2_000u32, 0..100),
        low in 0..2_000u32,
        span in 0..800u32,
        pivot in 0..2_000u32,
    ) {
        let mut set: Set<u32> = items.iter().copied().collect();
        let mut model: BTreeSet<u32> = items.clone();
        let high = low + span;
        let removed = set.remove_range(low..high);
        let doomed: Vec<u32> = model.range(low..high).copied().collect();
        prop_assert_eq!(removed, doomed.len());
        for value in doomed {
            model.remove(&value);
        }
        let tail = set.split_off(&pivot);
        let tail_model = model.split_off(&pivot);
        prop_assert_eq!(tail.iter().collect::<Vec<u32>>(), tail_model.iter().copied().collect::<Vec<u32>>());
        prop_assert_eq!(set.iter().collect::<Vec<u32>>(), model.iter().copied().collect::<Vec<u32>>());
        let mut extra: Set<u32> = more.iter().copied().collect();
        let mut extra_model = more.clone();
        set.append(&mut extra);
        model.append(&mut extra_model);
        prop_assert!(extra.is_empty());
        let taken = set.take_range(pivot..);
        let taken_model = model.split_off(&pivot);
        prop_assert_eq!(taken.iter().collect::<Vec<u32>>(), taken_model.iter().copied().collect::<Vec<u32>>());
        let direct: Set<u32> = model.iter().copied().collect();
        prop_assert!(set == direct);
    }

    #[test]
    fn truncate_matches_a_list_built_short(items in prop::collection::vec(any::<u16>(), 0..400), keep in 0..420usize) {
        let mut list: List<u16> = items.iter().copied().collect();
        list.truncate(keep);
        let short: List<u16> = items.iter().copied().take(keep).collect();
        prop_assert_eq!(list.len(), keep.min(items.len()));
        prop_assert!(list == short);
    }
}

#[test]
fn a_batch_of_absent_deletes_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = filled(&dir.path().join("a.ferro"), 10_000);
    let version = db.version();
    let size = db.size_on_disk();
    db.update(|root| root.apply((20_000..20_100).map(|key| (key, None))))
        .unwrap();
    assert_eq!(db.version(), version);
    assert_eq!(db.size_on_disk(), size);
    let hash = db.read().root_hash();
    let mut map = (*db.read()).clone();
    map.apply(std::iter::empty());
    assert!(!map.is_dirty());
    map.extend(std::iter::empty());
    assert!(!map.is_dirty());
    assert_eq!(map.root_hash(), hash);
}

#[test]
fn a_batch_that_changes_nothing_leaves_the_map_clean() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast()
        .open::<(Map<u32, u32>, Map<u32, ()>)>(dir.path().join("clean.ferro"))
        .unwrap();
    db.update(|(map, members)| {
        map.extend((0..1000).map(|key| (key * 2, key)));
        members.extend((0..1000).map(|key| (key * 2, ())));
    })
    .unwrap();
    let (mut map, mut members) = (*db.read()).clone();
    map.remove(&1);
    map.retain(|_, _| true);
    map.apply([(1, None), (3, None), (1999, None)]);
    assert!(!map.is_dirty());
    members.apply([(1, None), (2, Some(())), (4, Some(())), (3, None)]);
    members.extend([(6, ()), (8, ())]);
    assert!(!members.is_dirty());
    let mut rewritten = members.clone();
    rewritten.rewrite();
    assert!(rewritten.is_dirty());
    map.apply([(1, None), (5, Some(5))]);
    assert!(map.is_dirty());
    assert_eq!(map.get(&5).as_deref(), Some(&5));
    members.apply([(1, Some(()))]);
    assert!(members.is_dirty() && members.contains_key(&1));
    assert_eq!(members.len(), 1001);
}

#[test]
fn a_batch_keeps_the_last_edit_of_each_key() {
    let mut map: Map<u32, u32> = (0..100).map(|key| (key, key)).collect();
    map.apply([
        (5, None),
        (5, Some(50)),
        (6, Some(60)),
        (6, None),
        (200, Some(1)),
        (200, None),
    ]);
    assert_eq!(map.get(&5).as_deref(), Some(&50));
    assert_eq!(map.get(&6), None);
    assert_eq!(map.get(&200), None);
    assert_eq!(map.len(), 99);
    let mut set: Set<u32> = (0..10).collect();
    set.apply([(3, false), (4, false), (4, true), (20, true)]);
    assert_eq!(
        set.iter().collect::<Vec<u32>>(),
        [0, 1, 2, 4, 5, 6, 7, 8, 9, 20]
    );
}

static KEY_COMPARES: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Probe(u64);

impl PartialOrd for Probe {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Probe {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        KEY_COMPARES.fetch_add(1, Ordering::Relaxed);
        self.0.cmp(&other.0)
    }
}

impl Data for Probe {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Probe(u64::decode(r)?))
    }
}

#[test]
fn a_dense_batch_walks_the_tree_once() {
    let n = 100_000u64;
    let mut map: Map<Probe, u64> = (0..n).map(|key| (Probe(key * 2), key)).collect();
    let batch: Vec<(Probe, u64)> = (0..n).map(|key| (Probe(key * 2 + 1), key)).collect();
    let start = KEY_COMPARES.load(Ordering::Relaxed);
    map.extend(batch);
    let cost = KEY_COMPARES.load(Ordering::Relaxed) - start;
    assert_eq!(map.len() as u64, 2 * n);
    assert!(cost < 12 * n, "{cost} comparisons for {n} edits");
}

#[derive(Clone, Debug, PartialEq)]
struct Tally(u8);

static SLICE_ENCODES: AtomicU64 = AtomicU64::new(0);
static VEC_DECODES: AtomicU64 = AtomicU64::new(0);

impl Data for Tally {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.u8(self.0);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Tally(r.u8()?))
    }

    fn encode_slice(items: &[Self], w: &mut Encoder<'_>) -> ferro::Result<()> {
        SLICE_ENCODES.fetch_add(1, Ordering::Relaxed);
        for item in items {
            item.encode(w)?;
        }
        Ok(())
    }

    fn decode_vec(r: &mut Decoder<'_>, len: usize) -> ferro::Result<Vec<Self>> {
        VEC_DECODES.fetch_add(1, Ordering::Relaxed);
        (0..len).map(|_| Self::decode(r)).collect()
    }
}

fn encoded<T: Data>(value: &T) -> Vec<u8> {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder).unwrap();
    encoder.bytes().to_vec()
}

fn decoded<T: Data>(bytes: &[u8]) -> ferro::Result<T> {
    T::decode(&mut Decoder::new(bytes, &[], None))
}

#[test]
fn vectors_and_arrays_go_through_the_element_slice_hooks() {
    let items = vec![Tally(1), Tally(2)];
    let bytes = encoded(&items);
    assert_eq!(SLICE_ENCODES.load(Ordering::Relaxed), 1);
    assert_eq!(decoded::<Vec<Tally>>(&bytes).unwrap(), items);
    assert_eq!(VEC_DECODES.load(Ordering::Relaxed), 1);
    let array = [Tally(3), Tally(4), Tally(5)];
    let bytes = encoded(&array);
    assert_eq!(SLICE_ENCODES.load(Ordering::Relaxed), 2);
    assert_eq!(decoded::<[Tally; 3]>(&bytes).unwrap(), array);
    assert_eq!(VEC_DECODES.load(Ordering::Relaxed), 2);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(500), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn byte_buffers_share_one_encoding(raw in prop::collection::vec(any::<u8>(), 0..2_000)) {
        let bytes = Bytes::from(raw.clone());
        let wire = encoded(&raw);
        prop_assert_eq!(encoded(&bytes), wire.clone());
        prop_assert_eq!(decoded::<Bytes>(&wire).unwrap(), bytes.clone());
        prop_assert_eq!(decoded::<Vec<u8>>(&wire).unwrap(), raw.clone());
        prop_assert_eq!(&*bytes, raw.as_slice());
        prop_assert!(decoded::<Bytes>(&wire[..wire.len() - 1]).is_err() || raw.is_empty());
        prop_assert!(decoded::<Vec<u8>>(&wire[..wire.len() - 1]).is_err() || raw.is_empty());
    }
}

#[test]
fn byte_keys_look_up_by_slice() {
    let mut map: Map<Bytes, u32> = Map::new();
    let mut model = BTreeMap::new();
    for n in 0..2_000u32 {
        let key = Bytes::from(n.to_be_bytes().to_vec());
        map.insert(key, n);
        model.insert(n.to_be_bytes().to_vec(), n);
    }
    assert_eq!(map.get(&5u32.to_be_bytes()[..]).as_deref(), Some(&5));
    let keys: Vec<Vec<u8>> = map.keys().map(|key| key.to_vec()).collect();
    assert_eq!(keys, model.keys().cloned().collect::<Vec<_>>());
    let vectors: Map<Vec<u8>, u32> = model.into_iter().collect();
    assert_eq!(map.root_hash(), vectors.root_hash());
    assert_eq!(Bytes::new().len(), 0);
    assert_eq!(Bytes::default(), Bytes::from(Vec::new()));
}

#[test]
fn byte_values_survive_the_store_at_every_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    let sizes = [0usize, 1, 511, 512, 513, 4_096, 1 << 16, 1 << 20];
    let sample = |size: usize| -> Vec<u8> { text(size as u64, size).into_bytes() };
    let db = uncached().open::<Map<u64, Bytes>>(&path).unwrap();
    db.update(|root| {
        root.extend(
            sizes
                .iter()
                .map(|size| (*size as u64, Bytes::from(sample(*size)))),
        );
    })
    .unwrap();
    drop(db);
    let db = uncached().open::<Map<u64, Bytes>>(&path).unwrap();
    let snapshot = db.read();
    for size in sizes {
        assert_eq!(
            &**snapshot.get(&(size as u64)).unwrap(),
            sample(size).as_slice()
        );
    }
    db.verify().unwrap();
}

#[test]
fn inverted_ranges_remove_nothing() {
    let mut map: Map<u32, u32> = (0..1000).map(|key| (key, key)).collect();
    let hash = map.root_hash();
    let inverted = (Bound::Included(800), Bound::Excluded(100));
    assert_eq!(map.remove_range::<u32, _>(inverted), 0);
    let touching = (Bound::Excluded(50), Bound::Excluded(50));
    assert_eq!(map.remove_range::<u32, _>(touching), 0);
    assert_eq!(map.root_hash(), hash);
    assert_eq!(map.remove_range(..), 1000);
    assert!(map.is_empty());
    assert_eq!(map.root_hash(), None);
}

#[test]
fn borrowed_string_ranges() {
    let mut map: Map<String, u32> = (0..500u32).map(|n| (format!("{n:03}"), n)).collect();
    let bounds: (Bound<&str>, Bound<&str>) = (Bound::Excluded("100"), Bound::Included("105"));
    assert_eq!(map.remove_range::<str, _>(bounds), 5);
    assert!(map.contains_key("100"));
    assert!(!map.contains_key("101"));
    let tail = map.split_off("400");
    assert_eq!(tail.len(), 100);
    assert_eq!(tail.first().map(|pair| pair.1).as_deref(), Some(&400));
}

#[test]
fn large_range_churn_matches_model() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast()
        .open::<Map<u32, u32>>(dir.path().join("c.ferro"))
        .unwrap();
    let mut model: BTreeMap<u32, u32> = BTreeMap::new();
    let mut rng = Rng::new(21);
    for round in 0..200u32 {
        let low = rng.below(50_000) as u32;
        let high = low + rng.below(20_000) as u32;
        let batch: Vec<(u32, u32)> = (0..500)
            .map(|_| (rng.below(70_000) as u32, round))
            .collect();
        model.extend(batch.iter().copied());
        let grown = model.len();
        model.retain(|key, _| !(low..high).contains(key));
        let wanted = grown - model.len();
        db.update(|root| {
            for (key, value) in &batch {
                root.insert(*key, *value);
            }
            assert_eq!(root.remove_range(low..high), wanted);
        })
        .unwrap();
        assert_eq!(db.read().len(), model.len());
    }
    let snapshot = db.read();
    let got: Vec<(u32, u32)> = snapshot.iter().map(|(k, v)| (k, *v)).collect();
    assert_eq!(got, model.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>());
    let direct: Map<u32, u32> = model.iter().map(|(k, v)| (*k, *v)).collect();
    assert_eq!(snapshot.root_hash(), direct.root_hash());
    db.verify().unwrap();
}
