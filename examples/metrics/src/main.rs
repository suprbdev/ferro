mod cli;

use std::error::Error;
use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use ferro::Durability;
use ferro::admin::{self, Keep};
use metrics::ingest::{IngestOptions, ingest};
use metrics::load::{self, LoadOptions};
use metrics::model::{BUCKET_MS, now_ms};
use metrics::parse::{Selector, parse_duration, parse_time_now};
use metrics::tsdb::{self, AggQuery, Func, Tsdb};

use crate::cli::{Cli, Command};

type Res<T> = Result<T, Box<dyn Error>>;

fn selector(text: &str) -> Res<Selector> {
    Ok(Selector::parse(text)?)
}

fn durability(no_sync: bool) -> Durability {
    if no_sync {
        Durability::Buffered
    } else {
        Durability::Fsync
    }
}

fn human(bytes: u64) -> String {
    let value = bytes as f64;
    if value >= 1e9 {
        format!("{:.2} GB", value / 1e9)
    } else if value >= 1e6 {
        format!("{:.1} MB", value / 1e6)
    } else {
        format!("{:.1} KB", value / 1e3)
    }
}

fn run(cli: Cli, out: &mut dyn Write) -> Res<()> {
    let path = cli.db.as_path();
    match cli.command {
        Command::Ingest {
            file,
            batch_points,
            batch_ms,
            no_sync,
            strict,
        } => {
            let input: Box<dyn BufRead + Send> = match file {
                Some(file) => Box::new(BufReader::with_capacity(1 << 20, File::open(file)?)),
                None => Box::new(BufReader::with_capacity(1 << 20, io::stdin())),
            };
            let tsdb = Tsdb::open(path, durability(no_sync))?;
            let mut writer = tsdb.writer();
            let report = ingest(
                input,
                &mut writer,
                IngestOptions {
                    batch_points: batch_points.max(1),
                    batch_wait: Duration::from_millis(batch_ms),
                },
            )?;
            for error in &report.first_errors {
                eprintln!("rejected {error}");
            }
            let secs = report.elapsed.as_secs_f64().max(1e-9);
            writeln!(
                out,
                "ingested {} points in {} commits ({} new series), rejected {} lines, {:.2}s, {:.0} points/s",
                report.points,
                report.commits,
                report.new_series,
                report.rejected,
                secs,
                report.points as f64 / secs
            )?;
            if strict && report.rejected > 0 {
                return Err(format!("{} lines rejected", report.rejected).into());
            }
        }
        Command::Query {
            selector: text,
            from,
            to,
            limit,
        } => {
            let sel = selector(&text)?;
            let (from, to) = (parse_time_now(&from)?, parse_time_now(&to)?);
            let tsdb = Tsdb::open_read_only(path)?;
            let snap = tsdb.snapshot();
            for rec in tsdb::select(&snap, &sel) {
                let points = tsdb::read_series(&snap, rec.id, from, to)?;
                for (ts, value) in points.iter().take(limit.unwrap_or(usize::MAX)) {
                    writeln!(out, "{rec} {value} {ts}")?;
                }
            }
        }
        Command::Aggregate {
            selector: text,
            func,
            from,
            to,
            step,
            by,
            rollup,
        } => {
            let sel = selector(&text)?;
            let func = Func::parse(&func)?;
            let (from, to) = (parse_time_now(&from)?, parse_time_now(&to)?);
            let step = step.as_deref().map(parse_duration).transpose()?;
            let tsdb = Tsdb::open_read_only(path)?;
            let snap = tsdb.snapshot();
            let query = AggQuery {
                from,
                to,
                step,
                func,
                by: by.as_deref(),
            };
            let rows = match rollup {
                Some(rollup) => tsdb::aggregate_rollups(&snap, &sel, parse_duration(&rollup)?, query)?,
                None => tsdb::aggregate(&snap, &sel, query)?,
            };
            for row in rows {
                match &by {
                    Some(label) => writeln!(out, "{label}={}\t{}\t{}", row.group, row.bucket, row.value)?,
                    None => writeln!(out, "{}\t{}", row.bucket, row.value)?,
                }
            }
        }
        Command::Series { selector: text, count } => {
            let sel = selector(text.as_deref().unwrap_or(""))?;
            let tsdb = Tsdb::open_read_only(path)?;
            let snap = tsdb.snapshot();
            let rows = tsdb::select(&snap, &sel);
            if count {
                writeln!(out, "{}", rows.len())?;
            } else {
                for rec in rows {
                    writeln!(out, "{rec}")?;
                }
            }
        }
        Command::Labels { name } => {
            let tsdb = Tsdb::open_read_only(path)?;
            let snap = tsdb.snapshot();
            let summary = tsdb::label_summary(&snap, name.as_deref());
            for (label, values) in summary {
                if name.is_some() {
                    for (value, count) in values {
                        writeln!(out, "{value}\t{count}")?;
                    }
                } else {
                    writeln!(out, "{label}\t{}", values.len())?;
                }
            }
        }
        Command::Downsample {
            step,
            selector: text,
            from,
            to,
        } => {
            let step = parse_duration(&step)?;
            if step == 0 {
                return Err("step must be positive".into());
            }
            let sel = selector(text.as_deref().unwrap_or(""))?;
            let (from, to) = (parse_time_now(&from)?, parse_time_now(&to)?);
            let tsdb = Tsdb::open(path, Durability::Fsync)?;
            let started = Instant::now();
            let done = tsdb::downsample(&tsdb.db, &sel, step, from, to)?;
            writeln!(
                out,
                "downsampled {} points of {} series into {} rollups at {}ms in {:.2}s",
                done.points,
                done.series,
                done.rollups,
                step,
                started.elapsed().as_secs_f64()
            )?;
        }
        Command::Retain {
            older_than,
            now,
            keep_versions,
            keep_since,
            no_compact,
        } => {
            let now = now.unwrap_or_else(now_ms);
            let cutoff = now.saturating_sub(parse_duration(&older_than)?);
            let keep = match keep_since {
                Some(age) => Keep::Since(now.saturating_sub(parse_duration(&age)?)),
                None => Keep::Last(keep_versions),
            };
            let tsdb = Tsdb::open(path, Durability::Fsync)?;
            let started = Instant::now();
            let dropped = tsdb::drop_before(&tsdb.db, cutoff)?;
            let delete_secs = started.elapsed().as_secs_f64();
            writeln!(
                out,
                "dropped {} points in {} partitions in {:.3}s",
                dropped.points_dropped, dropped.partitions_dropped, delete_secs
            )?;
            drop(tsdb);
            if !no_compact {
                let started = Instant::now();
                let done = admin::compact(path, keep)?;
                writeln!(
                    out,
                    "compacted {} -> {} in {:.2}s, {} objects, {} commits dropped",
                    human(done.before_bytes),
                    human(done.after_bytes),
                    started.elapsed().as_secs_f64(),
                    done.objects,
                    done.commits_dropped
                )?;
            }
        }
        Command::Stats => {
            let tsdb = Tsdb::open_read_only(path)?;
            let stats = tsdb::stats(&tsdb.db);
            writeln!(out, "series       {}", stats.series)?;
            writeln!(out, "metrics      {}", stats.metrics)?;
            writeln!(out, "points       {}", stats.points)?;
            writeln!(out, "chunks       {}", stats.chunks)?;
            writeln!(
                out,
                "partitions   {} ({} ms each)",
                stats.partitions, BUCKET_MS
            )?;
            if let (Some(first), Some(last)) = (stats.first_bucket, stats.last_bucket) {
                writeln!(out, "range        {first}..{last}")?;
            }
            writeln!(out, "raw floor    {}", stats.raw_floor)?;
            for (step, entries) in &stats.rollup_tables {
                writeln!(out, "rollups      {step}ms: {entries}")?;
            }
            writeln!(out, "versions     {} (head v{})", stats.versions, stats.version)?;
            writeln!(out, "file bytes   {}", stats.file_bytes)?;
            if stats.points > 0 {
                writeln!(
                    out,
                    "bytes/point  {:.2}",
                    stats.file_bytes as f64 / stats.points as f64
                )?;
            }
        }
        Command::Verify => {
            let tsdb = Tsdb::open_read_only(path)?;
            let started = Instant::now();
            let report = tsdb::verify(&tsdb.db)?;
            writeln!(
                out,
                "{} objects, {} live bytes of {} file bytes, {} chunks, {} points, {:.2}s",
                report.objects,
                report.live_bytes,
                report.file_bytes,
                report.chunks,
                report.points,
                started.elapsed().as_secs_f64()
            )?;
            if report.problems.is_empty() {
                writeln!(out, "ok")?;
            } else {
                for problem in &report.problems {
                    writeln!(out, "problem: {problem}")?;
                }
                return Err(format!("{} problems found", report.problems.len()).into());
            }
        }
        Command::Load {
            rate,
            seconds,
            series,
            threads,
            batch_ms,
            interval_ms,
            start_ts,
            compact_every,
            keep_versions,
            no_sync,
        } => {
            let scrapes = rate.saturating_mul(seconds) / series.max(1) as u64;
            let start_ts = start_ts.unwrap_or_else(|| {
                now_ms().saturating_sub(scrapes.saturating_mul(interval_ms))
            });
            let options = LoadOptions {
                rate,
                seconds,
                series,
                threads,
                batch: Duration::from_millis(batch_ms.max(1)),
                interval_ms,
                start_ts,
                compact_every,
                keep: Keep::Last(keep_versions),
                durability: durability(no_sync),
            };
            let report = load::run(Path::new(path), &options)?;
            writeln!(
                out,
                "target {rate}/s, achieved {:.0}/s ({} points in {:.2}s, {} threads, {} commits)",
                report.points as f64 / report.wall_secs,
                report.points,
                report.wall_secs,
                threads,
                report.commits
            )?;
            writeln!(
                out,
                "commit latency p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
                report.percentile(50.0),
                report.percentile(99.0),
                report.percentile(100.0)
            )?;
            writeln!(
                out,
                "file {} ({:.2} bytes/point), {} versions",
                human(report.file_bytes),
                report.file_bytes as f64 / report.points.max(1) as f64,
                report.versions
            )?;
            for event in &report.compactions {
                writeln!(
                    out,
                    "compaction at {:.1}s: {} -> {}, writers stalled {:.2}s",
                    event.at_secs,
                    human(event.before_bytes),
                    human(event.after_bytes),
                    event.pause_secs
                )?;
            }
            for (at, bytes) in &report.size_samples {
                writeln!(out, "size at {at:.1}s: {}", human(*bytes))?;
            }
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let result = run(Cli::parse(), &mut out).and_then(|()| out.flush().map_err(Into::into));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let broken_pipe = error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == ErrorKind::BrokenPipe);
            if !broken_pipe {
                eprintln!("error: {error}");
            }
            ExitCode::FAILURE
        }
    }
}
