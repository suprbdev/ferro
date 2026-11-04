mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use common::Rng;
use ferro::{Data, Db, Durability, Keep, Map, Options, Reclaim, Record, Snapshot, Summary, Table};

const KEYS: u64 = 20_000;
const HOT: u64 = 256;
const ROWS: u64 = 400;
const GROUPS: u64 = 8;
const EDITS: u64 = 1 << 21;
const TINY: u64 = 12 << 10;
const READERS: u64 = 8;
const PHASES: u64 = 4;

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
struct Root {
    values: Map<u64, u64, Total>,
    rows: Table<Row>,
    value_edits: u64,
    row_edits: u64,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
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
        match rng.below(8) {
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
        let snapshot = match rng.below(3) {
            0 => shared.read().unwrap().as_ref().map(Db::read),
            1 => kept.clone(),
            _ => own.as_ref().map(Db::read),
        };
        let Some(snapshot) = snapshot else {
            continue;
        };
        let hot = (start.elapsed().as_millis() as u64 / 50).wrapping_mul(7_919) % KEYS;
        checked += check(
            &model,
            &snapshot,
            &mut rng,
            hot,
            &format!("reader {seed} round {rounds}"),
        );
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
    assert!(
        commits.load(Ordering::Relaxed) > PHASES,
        "writers committed too little"
    );
    assert!(
        checked > READERS * 100,
        "readers checked only {checked} reads"
    );
}
