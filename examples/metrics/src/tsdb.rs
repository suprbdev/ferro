use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ferro::table::IndexIter;
use ferro::{Db, Durability, Error, Map, Options, Result, Snapshot};

use crate::chunk::{Chunk, merge_points};
use crate::model::{BUCKET_MS, Metrics, Rollup, SeriesKey, SeriesRec, SeriesRef, bucket_of};
use crate::parse::{Op, Selector};

pub struct Tsdb {
    pub db: Db<Metrics>,
}

impl Tsdb {
    pub fn open(path: impl AsRef<Path>, durability: Durability) -> Result<Tsdb> {
        let db = Options::new().durability(durability).open(path)?;
        Ok(Tsdb { db })
    }

    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Tsdb> {
        let db = Options::new().read_only().open(path)?;
        Ok(Tsdb { db })
    }

    pub fn snapshot(&self) -> Snapshot<Metrics> {
        self.db.read()
    }

    pub fn writer(&self) -> Writer {
        Writer {
            db: self.db.clone(),
            cache: HashMap::new(),
        }
    }
}

#[derive(Default)]
pub struct Batch {
    groups: HashMap<SeriesRef, Vec<(u64, f64)>>,
    points: usize,
}

impl Batch {
    pub fn push(&mut self, key: &SeriesRef, ts: u64, value: f64) {
        self.points += 1;
        match self.groups.get_mut(key) {
            Some(points) => points.push((ts, value)),
            None => {
                self.groups.insert(key.clone(), vec![(ts, value)]);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.points
    }

    pub fn is_empty(&self) -> bool {
        self.points == 0
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Written {
    pub points: usize,
    pub new_series: usize,
    pub commit: Duration,
}

pub struct Writer {
    db: Db<Metrics>,
    cache: HashMap<SeriesRef, u64>,
}

type Work = (u64, u64, Vec<(u64, f64)>);

fn normalized(points: &[(u64, f64)]) -> Vec<(u64, f64)> {
    let mut out = Vec::new();
    merge_points(&mut out, points);
    out
}

fn register(m: &mut Metrics, key: &SeriesKey) -> u64 {
    let series = (key.metric.clone(), key.labels.clone());
    if let Some(id) = m.series.key_by_metric_labels(&series) {
        return id;
    }
    m.next_id += 1;
    let id = m.next_id;
    m.series.insert(SeriesRec::new(id, key));
    id
}

impl Writer {
    pub fn write(&mut self, batch: Batch) -> Result<Written> {
        if batch.is_empty() {
            return Ok(Written::default());
        }
        let points = batch.points;
        let started = Instant::now();
        let cache = &self.cache;
        let groups = batch.groups;
        let learned = self.db.try_commit::<_, Error>("ingest", |m| {
            let mut learned = Vec::new();
            let mut work: Vec<Work> = Vec::new();
            for (key, mut pts) in groups {
                let sid = match cache.get(&key) {
                    Some(id) => *id,
                    None => {
                        let id = register(m, &key.0);
                        learned.push((key, id));
                        id
                    }
                };
                pts.sort_by_key(|point| point.0);
                let mut start = 0;
                while start < pts.len() {
                    let bucket = bucket_of(pts[start].0);
                    let mut end = start;
                    while end < pts.len() && bucket_of(pts[end].0) == bucket {
                        end += 1;
                    }
                    work.push((bucket, sid, pts[start..end].to_vec()));
                    start = end;
                }
            }
            work.sort_by_key(|item| (item.0, item.1));
            let mut added = 0i64;
            let mut index = 0;
            while index < work.len() {
                let bucket = work[index].0;
                let part = m.chunks.get_or_insert_with(bucket, Map::new);
                while index < work.len() && work[index].0 == bucket {
                    let (_, sid, pts) = &work[index];
                    let (chunk, before) = match part.get(sid) {
                        Some(old) => (old.merged(pts)?, old.len()),
                        None => (Chunk::encode_points(&normalized(pts)), 0),
                    };
                    added += chunk.len() as i64 - before as i64;
                    part.insert(*sid, chunk);
                    index += 1;
                }
            }
            m.points = (m.points as i64 + added) as u64;
            Ok(learned)
        })?;
        let new_series = learned.len();
        for (key, id) in learned {
            self.cache.insert(key, id);
        }
        Ok(Written {
            points,
            new_series,
            commit: started.elapsed(),
        })
    }
}

pub fn select(m: &Metrics, selector: &Selector) -> Vec<Arc<SeriesRec>> {
    let mut best: Option<IndexIter<SeriesRec>> = None;
    let mut consider = |candidate: IndexIter<SeriesRec>| {
        if best.as_ref().is_none_or(|b| candidate.len() < b.len()) {
            best = Some(candidate);
        }
    };
    if let Some(metric) = &selector.metric {
        consider(m.series.by_metric(metric.as_str()));
    }
    for matcher in &selector.matchers {
        if matcher.op == Op::Eq && !matcher.value.is_empty() {
            let pair = format!("{}={}", matcher.name, matcher.value);
            consider(m.series.by_pairs(pair.as_str()));
        }
    }
    let mut rows: Vec<Arc<SeriesRec>> = match best {
        Some(rows) => rows.filter(|rec| selector.matches(rec)).collect(),
        None => m.series.iter().filter(|rec| selector.matches(rec)).collect(),
    };
    rows.sort_by_key(|rec| rec.id);
    rows
}

pub fn scan(
    m: &Metrics,
    sids: &[u64],
    from: u64,
    to: u64,
    mut visit: impl FnMut(usize, &[(u64, f64)]),
) -> Result<()> {
    if from >= to {
        return Ok(());
    }
    for (_, part) in m.chunks.range(bucket_of(from)..to) {
        for (index, sid) in sids.iter().enumerate() {
            let Some(chunk) = part.get(sid) else { continue };
            let points = chunk.decode_points()?;
            let low = points.partition_point(|point| point.0 < from);
            let high = points.partition_point(|point| point.0 < to);
            if low < high {
                visit(index, &points[low..high]);
            }
        }
    }
    Ok(())
}

pub fn read_series(m: &Metrics, sid: u64, from: u64, to: u64) -> Result<Vec<(u64, f64)>> {
    let mut out = Vec::new();
    scan(m, &[sid], from, to, |_, points| out.extend_from_slice(points))?;
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Func {
    Avg,
    Sum,
    Min,
    Max,
    Count,
    Rate,
    Pct(u32),
}

impl Func {
    pub fn parse(text: &str) -> std::result::Result<Func, String> {
        Ok(match text {
            "avg" => Func::Avg,
            "sum" => Func::Sum,
            "min" => Func::Min,
            "max" => Func::Max,
            "count" => Func::Count,
            "rate" => Func::Rate,
            "p50" => Func::Pct(50),
            "p95" => Func::Pct(95),
            "p99" => Func::Pct(99),
            other => return Err(format!("unknown function '{other}'")),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AggRow {
    pub group: String,
    pub bucket: u64,
    pub value: f64,
}

#[derive(Default)]
struct Cell {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    values: Vec<f64>,
}

impl Cell {
    fn add(&mut self, value: f64, keep: bool) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.count += 1;
        self.sum += value;
        if keep {
            self.values.push(value);
        }
    }

    fn merge(&mut self, other: Cell) {
        if other.count == 0 {
            return;
        }
        if self.count == 0 {
            self.min = other.min;
            self.max = other.max;
        } else {
            self.min = self.min.min(other.min);
            self.max = self.max.max(other.max);
        }
        self.count += other.count;
        self.sum += other.sum;
        self.values.extend(other.values);
    }

    fn finish(&mut self, func: Func) -> f64 {
        match func {
            Func::Avg => self.sum / self.count as f64,
            Func::Sum | Func::Rate => self.sum,
            Func::Min => self.min,
            Func::Max => self.max,
            Func::Count => self.count as f64,
            Func::Pct(p) => {
                self.values.sort_by(|a, b| a.total_cmp(b));
                let rank = (f64::from(p) / 100.0 * self.values.len() as f64).ceil() as usize;
                self.values[rank.max(1) - 1]
            }
        }
    }
}

#[derive(Clone, Copy)]
struct RateState {
    bucket: usize,
    first_ts: u64,
    last_ts: u64,
    last_value: f64,
    increase: f64,
    live: bool,
}

const MAX_CELLS: u64 = 5_000_000;

struct Grid {
    from: u64,
    step: u64,
    first_bucket: u64,
    buckets: usize,
    groups: Vec<String>,
    cells: Vec<Cell>,
}

impl Grid {
    fn new(from: u64, to: u64, step: Option<u64>, groups: Vec<String>) -> std::result::Result<Grid, String> {
        let (step, first_bucket, buckets) = match step {
            Some(step) => {
                if step == 0 {
                    return Err("step must be positive".to_string());
                }
                let first = from / step * step;
                let last = (to - 1) / step * step;
                (step, first, ((last - first) / step + 1) as usize)
            }
            None => (to - from, from, 1),
        };
        let total = buckets as u64 * groups.len().max(1) as u64;
        if total > MAX_CELLS {
            return Err(format!("{total} result cells; use a larger --step or a narrower range"));
        }
        let cells = (0..total).map(|_| Cell::default()).collect();
        Ok(Grid {
            from,
            step,
            first_bucket,
            buckets,
            groups,
            cells,
        })
    }

    fn empty_like(&self) -> Grid {
        Grid {
            from: self.from,
            step: self.step,
            first_bucket: self.first_bucket,
            buckets: self.buckets,
            groups: self.groups.clone(),
            cells: (0..self.cells.len()).map(|_| Cell::default()).collect(),
        }
    }

    fn bucket_index(&self, ts: u64) -> usize {
        if self.buckets == 1 {
            0
        } else {
            ((ts / self.step * self.step - self.first_bucket) / self.step) as usize
        }
    }

    fn bucket_start(&self, index: usize) -> u64 {
        if self.buckets == 1 && self.first_bucket == self.from {
            self.from
        } else {
            self.first_bucket + index as u64 * self.step
        }
    }

    fn into_rows(mut self, func: Func) -> Vec<AggRow> {
        let mut rows = Vec::new();
        for group in 0..self.groups.len().max(1) {
            for bucket in 0..self.buckets {
                let start = self.bucket_start(bucket);
                let cell = &mut self.cells[group * self.buckets + bucket];
                if cell.count == 0 {
                    continue;
                }
                let name = self.groups.get(group).cloned().unwrap_or_default();
                rows.push(AggRow {
                    group: name,
                    bucket: start,
                    value: cell.finish(func),
                });
            }
        }
        rows
    }
}

fn group_assignment(rows: &[Arc<SeriesRec>], by: Option<&str>) -> (Vec<String>, Vec<usize>) {
    let Some(by) = by else {
        return (vec![String::new()], vec![0; rows.len()]);
    };
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    for rec in rows {
        names.entry(rec.label(by).to_string()).or_insert(0);
    }
    for (index, slot) in names.values_mut().enumerate() {
        *slot = index;
    }
    let assignment = rows.iter().map(|rec| names[rec.label(by)]).collect();
    (names.into_keys().collect(), assignment)
}

#[derive(Clone, Copy, Debug)]
pub struct AggQuery<'a> {
    pub from: u64,
    pub to: u64,
    pub step: Option<u64>,
    pub func: Func,
    pub by: Option<&'a str>,
}

pub fn aggregate(
    m: &Metrics,
    selector: &Selector,
    query: AggQuery<'_>,
) -> std::result::Result<Vec<AggRow>, String> {
    let AggQuery {
        from,
        to,
        step,
        func,
        by,
    } = query;
    let Some((from, to)) = clamp_range(m, from, to) else {
        return Ok(Vec::new());
    };
    let rows = select(m, selector);
    let sids: Vec<u64> = rows.iter().map(|rec| rec.id).collect();
    let (names, assignment) = group_assignment(&rows, by);
    let grid = Grid::new(from, to, step, names)?;
    let shards = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(sids.len().div_ceil(64))
        .max(1);
    let grid = if shards == 1 {
        let mut grid = grid;
        fill(m, &sids, &assignment, &mut grid, from, to, func)?;
        grid
    } else {
        let per = sids.len().div_ceil(shards);
        let template = &grid;
        let results: Vec<std::result::Result<Grid, String>> = thread::scope(|scope| {
            let handles: Vec<_> = sids
                .chunks(per)
                .zip(assignment.chunks(per))
                .map(|(sids, assignment)| {
                    scope.spawn(move || {
                        let mut shard = template.empty_like();
                        fill(m, sids, assignment, &mut shard, from, to, func)?;
                        Ok(shard)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("aggregate shard panicked"))
                .collect()
        });
        let mut merged = grid.empty_like();
        for shard in results {
            for (cell, other) in merged.cells.iter_mut().zip(shard?.cells) {
                cell.merge(other);
            }
        }
        merged
    };
    Ok(grid.into_rows(func))
}

fn fill(
    m: &Metrics,
    sids: &[u64],
    assignment: &[usize],
    grid: &mut Grid,
    from: u64,
    to: u64,
    func: Func,
) -> std::result::Result<(), String> {
    let keep = matches!(func, Func::Pct(_));
    let buckets = grid.buckets;
    if func == Func::Rate {
        let mut states = vec![
            RateState {
                bucket: 0,
                first_ts: 0,
                last_ts: 0,
                last_value: 0.0,
                increase: 0.0,
                live: false,
            };
            sids.len()
        ];
        let flush = |grid: &mut Grid, group: usize, state: &RateState| {
            if state.last_ts > state.first_ts {
                let seconds = (state.last_ts - state.first_ts) as f64 / 1000.0;
                let cell = &mut grid.cells[group * buckets + state.bucket];
                cell.count += 1;
                cell.sum += state.increase / seconds;
            }
        };
        scan(m, sids, from, to, |index, points| {
            let group = assignment[index];
            for (ts, value) in points {
                let bucket = grid.bucket_index(*ts);
                let state = &mut states[index];
                if state.live && state.bucket == bucket {
                    state.increase += if *value >= state.last_value {
                        *value - state.last_value
                    } else {
                        *value
                    };
                    state.last_ts = *ts;
                    state.last_value = *value;
                } else {
                    if state.live {
                        let done = *state;
                        flush(grid, group, &done);
                    }
                    *state = RateState {
                        bucket,
                        first_ts: *ts,
                        last_ts: *ts,
                        last_value: *value,
                        increase: 0.0,
                        live: true,
                    };
                }
            }
        })
        .map_err(|e| e.to_string())?;
        for (index, state) in states.iter().enumerate() {
            if state.live {
                flush(grid, assignment[index], state);
            }
        }
    } else {
        scan(m, sids, from, to, |index, points| {
            let group = assignment[index];
            for (ts, value) in points {
                let bucket = grid.bucket_index(*ts);
                grid.cells[group * buckets + bucket].add(*value, keep);
            }
        })
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn aggregate_rollups(
    m: &Metrics,
    selector: &Selector,
    rollup_step: u64,
    query: AggQuery<'_>,
) -> std::result::Result<Vec<AggRow>, String> {
    let AggQuery {
        from,
        to,
        step,
        func,
        by,
    } = query;
    if matches!(func, Func::Rate | Func::Pct(_)) {
        return Err("rollups hold count, sum, min and max only; rate and percentiles need raw points".to_string());
    }
    if let Some(step) = step
        && step % rollup_step != 0
    {
        return Err(format!("step must be a multiple of the rollup step {rollup_step}"));
    }
    let Some(table) = m.rollups.get(&rollup_step) else {
        return Ok(Vec::new());
    };
    let rows = select(m, selector);
    let mut low = u64::MAX;
    let mut high = 0;
    for rec in &rows {
        if let Some(((_, start), _)) = table.range((rec.id, 0)..=(rec.id, u64::MAX)).next() {
            low = low.min(start);
        }
        if let Some(((_, start), _)) = table.range((rec.id, 0)..=(rec.id, u64::MAX)).next_back() {
            high = high.max(start + rollup_step);
        }
    }
    let (from, to) = (from.max(low), to.min(high));
    if from >= to {
        return Ok(Vec::new());
    }
    let (names, assignment) = group_assignment(&rows, by);
    let mut grid = Grid::new(from, to, step, names)?;
    let buckets = grid.buckets;
    let mut merged: Vec<Cell> = (0..grid.cells.len()).map(|_| Cell::default()).collect();
    for (index, rec) in rows.iter().enumerate() {
        let group = assignment[index];
        for ((_, start), roll) in table.range((rec.id, bucket_floor(from, rollup_step))..(rec.id, to)) {
            if start < from {
                continue;
            }
            let cell = &mut merged[group * buckets + grid.bucket_index(start)];
            if cell.count == 0 {
                cell.min = roll.min;
                cell.max = roll.max;
            } else {
                cell.min = cell.min.min(roll.min);
                cell.max = cell.max.max(roll.max);
            }
            cell.count += roll.count;
            cell.sum += roll.sum;
        }
    }
    grid.cells = merged;
    let mut rows = Vec::new();
    for group in 0..grid.groups.len().max(1) {
        for bucket in 0..grid.buckets {
            let cell = &grid.cells[group * buckets + bucket];
            if cell.count == 0 {
                continue;
            }
            let value = match func {
                Func::Avg => cell.sum / cell.count as f64,
                Func::Sum => cell.sum,
                Func::Min => cell.min,
                Func::Max => cell.max,
                _ => cell.count as f64,
            };
            rows.push(AggRow {
                group: grid.groups.get(group).cloned().unwrap_or_default(),
                bucket: grid.bucket_start(bucket),
                value,
            });
        }
    }
    Ok(rows)
}

pub fn clamp_range(m: &Metrics, from: u64, to: u64) -> Option<(u64, u64)> {
    let first = m.chunks.first()?.0;
    let last = m.chunks.last()?.0 + BUCKET_MS;
    let from = from.max(first);
    let to = to.min(last);
    (from < to).then_some((from, to))
}

fn bucket_floor(ts: u64, step: u64) -> u64 {
    ts / step * step
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Downsampled {
    pub series: usize,
    pub rollups: usize,
    pub points: u64,
}

pub fn downsample(
    db: &Db<Metrics>,
    selector: &Selector,
    step: u64,
    from: u64,
    to: u64,
) -> Result<Downsampled> {
    let snapshot = db.read();
    let Some((from, to)) = clamp_range(&snapshot, from, to) else {
        return Ok(Downsampled::default());
    };
    let rows = select(&snapshot, selector);
    let sids: Vec<u64> = rows.iter().map(|rec| rec.id).collect();
    let floor = snapshot.raw_floor;
    let mut per_series: Vec<Vec<((u64, u64), Rollup)>> = vec![Vec::new(); sids.len()];
    let mut points = 0u64;
    scan(&snapshot, &sids, from, to, |index, pts| {
        let sid = sids[index];
        let out = &mut per_series[index];
        for (ts, value) in pts {
            let start = bucket_floor(*ts, step);
            if start < floor {
                continue;
            }
            points += 1;
            match out.last_mut() {
                Some(((_, last), roll)) if *last == start => roll.add(*value),
                _ => out.push(((sid, start), Rollup::of(*value))),
            }
        }
    })?;
    drop(snapshot);
    let entries: Vec<((u64, u64), Rollup)> = per_series.into_iter().flatten().collect();
    let rollups = entries.len();
    db.commit(format!("downsample {step}ms"), |m| {
        m.rollups.get_or_insert_with(step, Map::new).extend(entries);
    })?;
    Ok(Downsampled {
        series: sids.len(),
        rollups,
        points,
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Retained {
    pub partitions_dropped: usize,
    pub points_dropped: u64,
}

pub fn drop_before(db: &Db<Metrics>, cutoff: u64) -> Result<Retained> {
    db.try_commit::<_, Error>("retain", |m| {
        let mut report = Retained::default();
        let boundary = bucket_of(cutoff);
        let dropped = m.chunks.take_range(..boundary);
        report.partitions_dropped = dropped.len();
        for part in dropped.values() {
            report.points_dropped += part.values().map(|chunk| chunk.len() as u64).sum::<u64>();
        }
        if cutoff > boundary
            && let Some(part) = m.chunks.get_mut(&boundary)
        {
            let mut trimmed = Vec::new();
            for (sid, chunk) in part.iter() {
                let kept: Vec<(u64, f64)> = chunk
                    .decode_points()?
                    .into_iter()
                    .filter(|point| point.0 >= cutoff)
                    .collect();
                report.points_dropped += (chunk.len() - kept.len()) as u64;
                trimmed.push((sid, (!kept.is_empty()).then(|| Chunk::encode_points(&kept))));
            }
            part.apply(trimmed);
            if part.is_empty() {
                m.chunks.remove(&boundary);
            }
        }
        m.points -= report.points_dropped.min(m.points);
        m.raw_floor = m.raw_floor.max(cutoff);
        Ok(report)
    })
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub series: usize,
    pub metrics: usize,
    pub partitions: usize,
    pub chunks: usize,
    pub points: u64,
    pub rollup_tables: Vec<(u64, usize)>,
    pub first_bucket: Option<u64>,
    pub last_bucket: Option<u64>,
    pub raw_floor: u64,
    pub versions: usize,
    pub version: u64,
    pub file_bytes: u64,
}

pub fn stats(db: &Db<Metrics>) -> Stats {
    let m = db.read();
    Stats {
        series: m.series.len(),
        metrics: m.series.by_metric_counts().len(),
        partitions: m.chunks.len(),
        chunks: m.chunks.values().map(|part| part.len()).sum(),
        points: m.points,
        rollup_tables: m.rollups.iter().map(|(step, table)| (step, table.len())).collect(),
        first_bucket: m.chunks.first().map(|(bucket, _)| bucket),
        last_bucket: m.chunks.last().map(|(bucket, _)| bucket + BUCKET_MS),
        raw_floor: m.raw_floor,
        versions: db.log().len(),
        version: db.version(),
        file_bytes: db.size_on_disk(),
    }
}

#[derive(Clone, Debug, Default)]
pub struct Verification {
    pub objects: u64,
    pub live_bytes: u64,
    pub file_bytes: u64,
    pub chunks: u64,
    pub points: u64,
    pub problems: Vec<String>,
}

pub fn verify(db: &Db<Metrics>) -> Result<Verification> {
    let walked = db.verify()?;
    let mut report = Verification {
        objects: walked.objects,
        live_bytes: walked.bytes,
        file_bytes: walked.file_bytes,
        ..Verification::default()
    };
    let m = db.read();
    for (bucket, part) in m.chunks.iter() {
        for (sid, chunk) in part.iter() {
            report.chunks += 1;
            if !m.series.contains(&sid) {
                report.problems.push(format!("chunk for unknown series {sid}"));
            }
            match chunk.decode_points() {
                Err(error) => report
                    .problems
                    .push(format!("bucket {bucket} series {sid}: {error}")),
                Ok(points) => {
                    report.points += points.len() as u64;
                    if points.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
                        report
                            .problems
                            .push(format!("bucket {bucket} series {sid}: timestamps not increasing"));
                    }
                    if points.iter().any(|p| bucket_of(p.0) != bucket) {
                        report
                            .problems
                            .push(format!("bucket {bucket} series {sid}: point outside bucket"));
                    }
                }
            }
        }
    }
    if report.points != m.points {
        report.problems.push(format!(
            "point counter says {} but chunks hold {}",
            m.points, report.points
        ));
    }
    Ok(report)
}

pub fn label_summary(m: &Metrics, name: Option<&str>) -> Vec<(String, Vec<(String, usize)>)> {
    let mut out: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
    if name.is_none_or(|n| n == "__name__") {
        out.insert("__name__".to_string(), m.series.by_metric_counts());
    }
    for (pair, count) in m.series.by_pairs_counts() {
        if let Some((k, v)) = pair.split_once('=')
            && name.is_none_or(|n| n == k)
        {
            out.entry(k.to_string()).or_default().push((v.to_string(), count));
        }
    }
    out.into_iter().collect()
}
