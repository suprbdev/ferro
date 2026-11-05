//! A stress test of reads that borrow nodes without counts: under epoch guards, through hazard
//! slots, and through iterators that outlive what they came from, checked against a model of every
//! version while the caches evict and the file is reclaimed.
//!
//! Eight readers do gets, `floor`, `summary_by`, row gets, index lookups, ranges both ways and
//! cursor seeks on the writer's handle and on read-only handles with caches of 12 KiB to 1 MiB,
//! which they refresh and reopen. They keep ranges, cursors and index walks from one round to the
//! next, and read nested maps through iterators whose snapshot and handle are gone, so that what
//! they hold outlives the snapshot, the handle and the store it came from. Three writers on a
//! 12 KiB cache edit values, rows and the nested maps, with a reclamation cycle every 96 KiB and
//! compactions, and the writer's `Db` is dropped between four phases and opened again. At the end
//! a fresh handle checks the last version.
//! [How the protocol is checked](https://github.com/archlemon/ferro/blob/main/docs/internals/read-scaling.md#how-the-protocol-is-checked)
//! describes the runs under AddressSanitizer and ThreadSanitizer. `FERRO_STRESS_SEED` sets the
//! seed, 40 by default, and `FERRO_STRESS_MS` how many milliseconds it runs, 4,000 by default.
//!
//! ```sh
//! make test-pkg P=ferro T=guarded_reads_match_the_model
//! FERRO_STRESS_SEED=7 FERRO_STRESS_MS=20000 cargo test --release -p ferro --test epoch_stress
//! ```

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use common::Rng;
use ferro::map::Iter;
use ferro::table::IndexIter;
use ferro::{
    Cursor, Data, Db, Durability, Keep, Map, Options, Reclaim, Record, Snapshot, Summary, Table,
    Val,
};

const KEYS: u64 = 20_000;
const HOT: u64 = 256;
const ROWS: u64 = 400;
const GROUPS: u64 = 8;
const EDITS: u64 = 1 << 21;
const TINY: u64 = 12 << 10;
const READERS: u64 = 8;
const PHASES: u64 = 4;
const NESTS: u64 = 96;
const KEPT: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Data)]
#[ferro(transparent)]
struct Total(u64);

impl Summary<u64, u64> for Total {
    fn of(_key: &u64, value: &u64) -> Self {
        Total(*value)
    }

    fn combine(&self, other: &Self) -> Self {
        Total(self.0.wrapping_add(other.0))
    }
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u64,
    #[index]
    group: u64,
    stamp: u64,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Nest {
    version: u64,
    blob: String,
    inner: Map<u64, u64>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Root {
    values: Map<u64, u64, Total>,
    rows: Table<Row>,
    nests: Map<u64, Nest>,
    value_edits: u64,
    row_edits: u64,
    nest_edits: u64,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn blob(key: u64, version: u64) -> String {
    let size = if (key + version).is_multiple_of(3) {
        8
    } else {
        600 + (mix(key ^ version) % 400) as usize
    };
    format!("{key}:{version}:").repeat(size / 4 + 1)[..size].to_string()
}

fn inner(key: u64, version: u64) -> Vec<(u64, u64)> {
    (0..4)
        .map(|slot| (slot, key * 1_000_000 + version * 10 + slot))
        .collect()
}

fn nest(key: u64, version: u64) -> Nest {
    Nest {
        version,
        blob: blob(key, version),
        inner: inner(key, version).into_iter().collect(),
    }
}

fn nest_key(seed: u64, edit: u64) -> u64 {
    mix(seed ^ edit ^ 0x6666) % NESTS
}

fn check_nest(seed: u64, key: u64, nest: &Nest, edits: Option<u64>, context: &str) {
    let version = nest.version;
    if let Some(edits) = edits {
        assert!(
            version <= edits,
            "{context}: nest {key} at {version} after {edits} edits"
        );
    }
    assert!(
        version == 0 || nest_key(seed, version - 1) == key,
        "{context}: nest {key} holds edit {version} of another key"
    );
    assert_eq!(
        nest.blob,
        blob(key, version),
        "{context}: blob of nest {key}"
    );
    let got: Vec<(u64, u64)> = nest
        .inner
        .iter()
        .map(|(slot, value)| (slot, *value))
        .collect();
    assert_eq!(
        got,
        inner(key, version),
        "{context}: inner map of nest {key}"
    );
    let probe = mix(key ^ version) % 4;
    assert_eq!(
        nest.inner.get(&probe).map(|value| *value),
        Some(key * 1_000_000 + version * 10 + probe),
        "{context}: inner get {probe} of nest {key}"
    );
}

fn seed() -> u64 {
    std::env::var("FERRO_STRESS_SEED")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(40)
}

fn millis() -> u64 {
    std::env::var("FERRO_STRESS_MS")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(4_000)
}

struct Model {
    seed: u64,
    values: Vec<Vec<u32>>,
    rows: Vec<Vec<u32>>,
}

impl Model {
    fn new(seed: u64) -> Model {
        let mut values = vec![Vec::new(); KEYS as usize];
        let mut rows = vec![Vec::new(); ROWS as usize];
        for edit in 0..EDITS {
            values[Model::value_key(seed, edit) as usize].push(edit as u32);
            rows[Model::row_id(seed, edit) as usize].push(edit as u32);
        }
        Model { seed, values, rows }
    }

    fn value_key(seed: u64, edit: u64) -> u64 {
        let spread = mix(seed ^ edit);
        if spread.is_multiple_of(2) {
            (spread >> 1) % HOT
        } else {
            (spread >> 1) % KEYS
        }
    }

    fn row_id(seed: u64, edit: u64) -> u64 {
        mix(seed ^ edit ^ 0xa5a5) % ROWS
    }

    fn first_value(&self, key: u64) -> u64 {
        mix(self.seed ^ key ^ 0x1111) >> 20
    }

    fn value_edit(&self, edit: u64) -> (u64, Option<u64>) {
        let key = Model::value_key(self.seed, edit);
        if mix(self.seed ^ edit ^ 0x2222).is_multiple_of(5) {
            (key, None)
        } else {
            (key, Some(mix(self.seed ^ edit ^ 0x3333) >> 20))
        }
    }

    fn first_row(&self, id: u64) -> Row {
        Row {
            id,
            group: id % GROUPS,
            stamp: (1 << 40) | id,
        }
    }

    fn row_edit(&self, edit: u64) -> (u64, Option<Row>) {
        let id = Model::row_id(self.seed, edit);
        if mix(self.seed ^ edit ^ 0x4444).is_multiple_of(4) {
            (id, None)
        } else {
            let group = mix(self.seed ^ edit ^ 0x5555) % GROUPS;
            (
                id,
                Some(Row {
                    id,
                    group,
                    stamp: edit,
                }),
            )
        }
    }

    fn value_at(&self, key: u64, applied: u64) -> Option<u64> {
        let touched = &self.values[key as usize];
        let before = touched.partition_point(|edit| u64::from(*edit) < applied);
        match before {
            0 => Some(self.first_value(key)),
            n => self.value_edit(u64::from(touched[n - 1])).1,
        }
    }

    fn row_at(&self, id: u64, applied: u64) -> Option<Row> {
        let touched = &self.rows[id as usize];
        let before = touched.partition_point(|edit| u64::from(*edit) < applied);
        match before {
            0 => Some(self.first_row(id)),
            n => self.row_edit(u64::from(touched[n - 1])).1,
        }
    }
}

fn writer_options() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(TINY)
        .reclaim(Reclaim {
            every_bytes: 96 << 10,
        })
}

fn reader_options(cache: u64) -> Options {
    Options::new().read_only(true).cache_bytes(cache)
}

fn populate(db: &Db<Root>, model: &Model) {
    db.update(|root| {
        root.values
            .extend((0..KEYS).map(|key| (key, model.first_value(key))));
        for id in 0..ROWS {
            root.rows.insert(model.first_row(id));
        }
        root.nests.extend((0..NESTS).map(|key| (key, nest(key, 0))));
    })
    .unwrap();
}

fn pick(rng: &mut Rng, hot: u64) -> u64 {
    match rng.below(10) {
        0..4 => rng.below(HOT),
        4..7 => (hot + rng.below(HOT)) % KEYS,
        _ => rng.below(KEYS + 8),
    }
}

fn check(model: &Model, root: &Root, rng: &mut Rng, hot: u64, context: &str) -> u64 {
    let values = root.value_edits;
    let rows = root.row_edits;
    let mut checked = 0;
    for _ in 0..24 {
        match rng.below(9) {
            0 | 1 => {
                let key = pick(rng, hot);
                let got = root.values.get(&key).map(|value| *value);
                let want = if key < KEYS {
                    model.value_at(key, values)
                } else {
                    None
                };
                assert_eq!(got, want, "{context}: get {key} after {values} edits");
                assert_eq!(
                    root.values.contains_key(&key),
                    want.is_some(),
                    "{context}: contains {key}"
                );
            }
            2 => {
                let key = pick(rng, hot);
                let got = root
                    .values
                    .floor(&key)
                    .map(|(found, value)| (found, *value));
                let want = (0..=key.min(KEYS - 1))
                    .rev()
                    .find_map(|probe| model.value_at(probe, values).map(|value| (probe, value)));
                assert_eq!(got, want, "{context}: floor {key} after {values} edits");
            }
            3 => {
                let low = pick(rng, hot).min(KEYS - 1);
                let high = low + 1 + rng.below(80);
                let got = root.values.range_summary(low..high).map(|total| total.0);
                let mut want: Option<u64> = None;
                for key in low..high.min(KEYS) {
                    if let Some(value) = model.value_at(key, values) {
                        want = Some(want.unwrap_or(0).wrapping_add(value));
                    }
                }
                assert_eq!(
                    got, want,
                    "{context}: summary of {low}..{high} after {values} edits"
                );
            }
            4 => {
                let id = rng.below(ROWS + 4);
                let got = root.rows.get(&id).map(|row| (*row).clone());
                let want = if id < ROWS {
                    model.row_at(id, rows)
                } else {
                    None
                };
                assert_eq!(got, want, "{context}: row {id} after {rows} edits");
            }
            8 => {
                let key = rng.below(NESTS + 2);
                let got = root.nests.get(&key);
                assert_eq!(got.is_some(), key < NESTS, "{context}: nest {key}");
                if let Some(got) = got {
                    check_nest(model.seed, key, &got, Some(root.nest_edits), context);
                }
            }
            5 => {
                let low = pick(rng, hot).min(KEYS - 1);
                let high = low + 1 + rng.below(80);
                let want: Vec<(u64, u64)> = (low..high.min(KEYS))
                    .filter_map(|key| model.value_at(key, values).map(|value| (key, value)))
                    .collect();
                let got: Vec<(u64, u64)> = root
                    .values
                    .range(low..high)
                    .map(|(key, value)| (key, *value))
                    .collect();
                assert_eq!(
                    got, want,
                    "{context}: range {low}..{high} after {values} edits"
                );
                let mut back: Vec<(u64, u64)> = root
                    .values
                    .range(low..high)
                    .rev()
                    .map(|(key, value)| (key, *value))
                    .collect();
                back.reverse();
                assert_eq!(back, want, "{context}: reversed range {low}..{high}");
                let mut cursor = root.values.cursor();
                for (key, value) in &want {
                    assert!(cursor.seek(key), "{context}: cursor seek {key}");
                    assert_eq!(cursor.value().map(|found| *found), Some(*value));
                }
            }
            _ => {
                let group = rng.below(GROUPS);
                let got: Vec<Row> = root
                    .rows
                    .by_group(&group)
                    .map(|row| (*row).clone())
                    .collect();
                let want: Vec<Row> = (0..ROWS)
                    .filter_map(|id| model.row_at(id, rows))
                    .filter(|row| row.group == group)
                    .collect();
                assert_eq!(got, want, "{context}: group {group} after {rows} edits");
            }
        }
        checked += 1;
    }
    checked
}

type Shared = Arc<RwLock<Option<Db<Root>>>>;

struct Walk {
    values: Iter<u64, u64, Total>,
    backwards: bool,
    want: std::vec::IntoIter<(u64, u64)>,
    context: String,
}

struct Seeks {
    cursor: Cursor<u64, u64, Total>,
    want: std::vec::IntoIter<(u64, u64)>,
    context: String,
}

struct Rows {
    rows: IndexIter<Row>,
    want: std::vec::IntoIter<Row>,
    context: String,
}

struct Nests {
    nests: Iter<u64, Nest>,
    backwards: bool,
    edits: u64,
    context: String,
}

#[derive(Default)]
struct Walks {
    values: Option<Walk>,
    seeks: Option<Seeks>,
    rows: Option<Rows>,
    nests: Option<Nests>,
    kept: Vec<(u64, Val<Nest>, String)>,
}

fn values_in(model: &Model, low: u64, high: u64, applied: u64) -> Vec<(u64, u64)> {
    (low..high.min(KEYS))
        .filter_map(|key| model.value_at(key, applied).map(|value| (key, value)))
        .collect()
}

impl Walks {
    fn start(&mut self, model: &Model, root: &Root, rng: &mut Rng, hot: u64, context: &str) {
        let low = pick(rng, hot).min(KEYS - 1);
        let high = low + 50 + rng.below(800);
        if self.values.is_none() && rng.chance(30) {
            let backwards = rng.chance(30);
            let mut want = values_in(model, low, high, root.value_edits);
            if backwards {
                want.reverse();
            }
            self.values = Some(Walk {
                values: root.values.range(low..high),
                backwards,
                want: want.into_iter(),
                context: format!("{context}: held range {low}..{high}"),
            });
        }
        if self.seeks.is_none() && rng.chance(20) {
            self.seeks = Some(Seeks {
                cursor: root.values.cursor(),
                want: values_in(model, low, high, root.value_edits).into_iter(),
                context: format!("{context}: held cursor over {low}..{high}"),
            });
        }
        if self.rows.is_none() && rng.chance(20) {
            let group = rng.below(GROUPS);
            let want: Vec<Row> = (0..ROWS)
                .filter_map(|id| model.row_at(id, root.row_edits))
                .filter(|row| row.group == group)
                .collect();
            self.rows = Some(Rows {
                rows: root.rows.by_group(&group),
                want: want.into_iter(),
                context: format!("{context}: held group {group}"),
            });
        }
        if self.nests.is_none() && rng.chance(25) {
            let from = rng.below(NESTS);
            let backwards = rng.chance(30);
            self.nests = Some(Nests {
                nests: match backwards {
                    true => root.nests.range(..from),
                    false => root.nests.range(from..),
                },
                backwards,
                edits: root.nest_edits,
                context: format!("{context}: held nests from {from}"),
            });
        }
    }

    fn advance(&mut self, seed: u64, rng: &mut Rng) -> u64 {
        let mut checked = 0;
        if let Some(walk) = &mut self.values {
            let mut done = rng.chance(4);
            for _ in 0..rng.below(48) {
                let got = if walk.backwards {
                    walk.values.next_back()
                } else {
                    walk.values.next()
                };
                let got = got.map(|(key, value)| (key, *value));
                let want = walk.want.next();
                assert_eq!(got, want, "{}", walk.context);
                checked += 1;
                if want.is_none() {
                    done = true;
                    break;
                }
            }
            if done {
                self.values = None;
            }
        }
        if let Some(seeks) = &mut self.seeks {
            let mut done = rng.chance(4);
            for _ in 0..rng.below(32) {
                let Some((key, value)) = seeks.want.next() else {
                    done = true;
                    break;
                };
                assert!(seeks.cursor.seek(&key), "{}: seek {key}", seeks.context);
                let got = seeks.cursor.value().map(|found| *found);
                assert_eq!(got, Some(value), "{}: value at {key}", seeks.context);
                checked += 1;
            }
            if done {
                self.seeks = None;
            }
        }
        if let Some(rows) = &mut self.rows {
            let mut done = rng.chance(4);
            for _ in 0..rng.below(24) {
                let got = rows.rows.next().map(|row| (*row).clone());
                let want = rows.want.next();
                assert_eq!(got, want, "{}", rows.context);
                checked += 1;
                if want.is_none() {
                    done = true;
                    break;
                }
            }
            if done {
                self.rows = None;
            }
        }
        if let Some(walk) = &mut self.nests {
            let mut done = rng.chance(4);
            for _ in 0..rng.below(12) {
                let got = if walk.backwards {
                    walk.nests.next_back()
                } else {
                    walk.nests.next()
                };
                let Some((key, value)) = got else {
                    done = true;
                    break;
                };
                check_nest(seed, key, &value, Some(walk.edits), &walk.context);
                checked += 1;
                if self.kept.len() < KEPT && rng.chance(30) {
                    self.kept.push((key, value, walk.context.clone()));
                }
            }
            if done {
                self.nests = None;
            }
        }
        if !self.kept.is_empty() && rng.chance(25) {
            let at = rng.below(self.kept.len() as u64) as usize;
            let (key, value, context) = self.kept.swap_remove(at);
            check_nest(seed, key, &value, None, &format!("{context}, kept"));
            checked += 1;
        }
        checked
    }
}

fn orphaned(model: &Model, path: &std::path::Path, rng: &mut Rng, context: &str) -> u64 {
    let Ok(db) = reader_options(TINY).open::<Root>(path) else {
        return 0;
    };
    let snapshot = db.read();
    let edits = snapshot.nest_edits;
    let from = rng.below(NESTS);
    let mut walk = snapshot.nests.range(from..);
    let mut cursor = snapshot.nests.cursor();
    drop(snapshot);
    drop(db);
    let mut held = Vec::new();
    for _ in 0..rng.below(8) {
        let Some((key, value)) = walk.next() else {
            break;
        };
        check_nest(model.seed, key, &value, Some(edits), context);
        held.push((key, value));
    }
    let key = rng.below(NESTS);
    if cursor.seek(&key)
        && let Some(value) = cursor.value()
    {
        check_nest(model.seed, key, &value, Some(edits), context);
        held.push((key, value));
    }
    drop(walk);
    drop(cursor);
    let context = format!("{context}: orphaned");
    for (key, value) in &held {
        check_nest(model.seed, *key, value, Some(edits), &context);
    }
    held.len() as u64
}

fn reader(
    model: Arc<Model>,
    path: std::path::PathBuf,
    shared: Shared,
    stop: Arc<AtomicBool>,
    seed: u64,
) -> u64 {
    let start = Instant::now();
    let mut rng = Rng::new(seed);
    let cache = [TINY, 96 << 10, 1 << 20][rng.below(3) as usize];
    let mut own: Option<Db<Root>> = None;
    let mut kept: Option<Snapshot<Root>> = None;
    let mut walks = Walks::default();
    let mut checked = 0;
    let mut rounds = 0u64;
    while !stop.load(Ordering::Relaxed) {
        rounds += 1;
        if own.is_none() || rng.chance(2) {
            own = reader_options(cache).open::<Root>(&path).ok();
        }
        if rng.chance(20)
            && let Some(db) = &own
        {
            db.refresh().unwrap();
        }
        checked += walks.advance(model.seed, &mut rng);
        if rng.chance(3) {
            let context = format!("reader {seed} round {rounds}");
            checked += orphaned(&model, &path, &mut rng, &context);
        }
        let snapshot = match rng.below(3) {
            0 => shared.read().unwrap().as_ref().map(Db::read),
            1 => kept.clone(),
            _ => own.as_ref().map(Db::read),
        };
        let Some(snapshot) = snapshot else {
            continue;
        };
        let hot = (start.elapsed().as_millis() as u64 / 50).wrapping_mul(7_919) % KEYS;
        let context = format!("reader {seed} round {rounds}");
        checked += check(&model, &snapshot, &mut rng, hot, &context);
        walks.start(&model, &snapshot, &mut rng, hot, &context);
        if rng.chance(10) {
            kept = Some(snapshot);
        }
    }
    checked
}

fn write_values(db: Db<Root>, model: Arc<Model>, stop: Arc<AtomicBool>, commits: Arc<AtomicU64>) {
    let mut rng = Rng::new(model.seed ^ 0x77);
    while !stop.load(Ordering::Relaxed) {
        let batch = 1 + rng.below(6);
        db.update(|root| {
            for _ in 0..batch {
                let edit = root.value_edits;
                assert!(edit < EDITS);
                match model.value_edit(edit) {
                    (key, Some(value)) => {
                        root.values.insert(key, value);
                    }
                    (key, None) => {
                        root.values.remove(&key);
                    }
                }
                root.value_edits = edit + 1;
            }
        })
        .unwrap();
        commits.fetch_add(1, Ordering::Relaxed);
    }
}

fn write_rows(db: Db<Root>, model: Arc<Model>, stop: Arc<AtomicBool>, commits: Arc<AtomicU64>) {
    let mut rng = Rng::new(model.seed ^ 0x99);
    while !stop.load(Ordering::Relaxed) {
        let batch = 1 + rng.below(3);
        db.update(|root| {
            for _ in 0..batch {
                let edit = root.row_edits;
                assert!(edit < EDITS);
                match model.row_edit(edit) {
                    (_, Some(row)) => {
                        root.rows.insert(row);
                    }
                    (id, None) => {
                        root.rows.remove(&id);
                    }
                }
                root.row_edits = edit + 1;
            }
        })
        .unwrap();
        commits.fetch_add(1, Ordering::Relaxed);
        if rng.chance(1) {
            db.compact(Keep::Last(1 + rng.below(50) as usize)).unwrap();
        }
    }
}

fn write_nests(db: Db<Root>, model: Arc<Model>, stop: Arc<AtomicBool>, commits: Arc<AtomicU64>) {
    while !stop.load(Ordering::Relaxed) {
        db.update(|root| {
            let edit = root.nest_edits;
            let key = nest_key(model.seed, edit);
            root.nests.insert(key, nest(key, edit + 1));
            root.nest_edits = edit + 1;
        })
        .unwrap();
        commits.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn guarded_reads_match_the_model_under_eviction_and_reopening() {
    let seed = seed();
    let model = Arc::new(Model::new(seed));
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("stress.ferro");
    let shared: Shared = Arc::new(RwLock::new(None));
    {
        let db = writer_options().open::<Root>(&path).unwrap();
        populate(&db, &model);
        let snapshot = db.read();
        check(&model, &snapshot, &mut Rng::new(seed), 0, "populated");
        *shared.write().unwrap() = Some(db);
    }
    let stop_readers = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..READERS)
        .map(|index| {
            let (model, path, shared, stop) = (
                model.clone(),
                path.clone(),
                shared.clone(),
                stop_readers.clone(),
            );
            thread::spawn(move || reader(model, path, shared, stop, seed ^ (index << 8)))
        })
        .collect();
    let commits = Arc::new(AtomicU64::new(0));
    let phase = Duration::from_millis(millis() / PHASES);
    for round in 0..PHASES {
        if round > 0 {
            *shared.write().unwrap() = Some(writer_options().open::<Root>(&path).unwrap());
        }
        let db = shared.read().unwrap().as_ref().unwrap().clone();
        let stop_writers = Arc::new(AtomicBool::new(false));
        let writers = [
            {
                let (db, model, stop, commits) = (
                    db.clone(),
                    model.clone(),
                    stop_writers.clone(),
                    commits.clone(),
                );
                thread::spawn(move || write_values(db, model, stop, commits))
            },
            {
                let (db, model, stop, commits) = (
                    db.clone(),
                    model.clone(),
                    stop_writers.clone(),
                    commits.clone(),
                );
                thread::spawn(move || write_rows(db, model, stop, commits))
            },
            {
                let (db, model, stop, commits) = (
                    db.clone(),
                    model.clone(),
                    stop_writers.clone(),
                    commits.clone(),
                );
                thread::spawn(move || write_nests(db, model, stop, commits))
            },
        ];
        drop(db);
        let start = Instant::now();
        while start.elapsed() < phase {
            thread::sleep(Duration::from_millis(5));
        }
        stop_writers.store(true, Ordering::Relaxed);
        for writer in writers {
            writer.join().unwrap();
        }
        let closing = shared.write().unwrap().take();
        drop(closing);
        thread::sleep(Duration::from_millis(20));
    }
    stop_readers.store(true, Ordering::Relaxed);
    let checked: u64 = readers
        .into_iter()
        .map(|reader| reader.join().unwrap())
        .sum();
    let reopened = reader_options(TINY).open::<Root>(&path).unwrap();
    let last = reopened.read();
    check(&model, &last, &mut Rng::new(seed ^ 1), 0, "reopened");
    for (key, nest) in last.nests.iter() {
        check_nest(seed, key, &nest, Some(last.nest_edits), "reopened");
    }
    assert!(
        commits.load(Ordering::Relaxed) > PHASES,
        "writers committed too little"
    );
    assert!(
        checked > READERS * 100,
        "readers checked only {checked} reads"
    );
}
