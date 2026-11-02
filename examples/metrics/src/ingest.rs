use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use ferro::Result;

use crate::model::{SeriesRef, now_ms};
use crate::parse::{parse_line, parse_tail, split_series};
use crate::tsdb::{Batch, Writer};

const CHUNK_LINES: usize = 8192;
const MAX_INTERNED: usize = 1_000_000;

#[derive(Clone, Copy, Debug)]
pub struct IngestOptions {
    pub batch_points: usize,
    pub batch_wait: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct IngestReport {
    pub points: u64,
    pub rejected: u64,
    pub first_errors: Vec<String>,
    pub commits: u64,
    pub new_series: u64,
    pub elapsed: Duration,
    pub commit_time: Duration,
}

enum Message {
    Point(SeriesRef, u64, f64),
    Bad(u64, String),
}

fn parse_interned(
    line: &str,
    interned: &mut HashMap<String, SeriesRef>,
) -> std::result::Result<Option<Message>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    if let Some((series, rest)) = split_series(line)
        && let Some(key) = interned.get(series)
        && let Ok((value, ts)) = parse_tail(rest)
    {
        return Ok(Some(Message::Point(
            key.clone(),
            ts.unwrap_or_else(now_ms),
            value,
        )));
    }
    let Some(sample) = parse_line(line)? else {
        return Ok(None);
    };
    let key = SeriesRef::new(sample.key);
    if let Some((series, _)) = split_series(line)
        && interned.len() < MAX_INTERNED
    {
        interned.insert(series.to_string(), key.clone());
    }
    Ok(Some(Message::Point(
        key,
        sample.ts.unwrap_or_else(now_ms),
        sample.value,
    )))
}

fn read_lines(input: Box<dyn Read + Send>, tx: SyncSender<Vec<Message>>) {
    let mut input = BufReader::with_capacity(1 << 20, input);
    let mut line = String::new();
    let mut interned: HashMap<String, SeriesRef> = HashMap::new();
    let mut chunk: Vec<Message> = Vec::with_capacity(CHUNK_LINES);
    let mut number = 0u64;
    loop {
        line.clear();
        let read = input.read_line(&mut line);
        number += 1;
        let finished = !matches!(read, Ok(1..));
        match read {
            Err(error) => chunk.push(Message::Bad(number, error.to_string())),
            Ok(0) => {}
            Ok(_) => match parse_interned(&line, &mut interned) {
                Ok(None) => {}
                Ok(Some(message)) => chunk.push(message),
                Err(error) => chunk.push(Message::Bad(number, error)),
            },
        }
        let drained = input.buffer().is_empty();
        if !chunk.is_empty() && (finished || drained || chunk.len() >= CHUNK_LINES) {
            let full = std::mem::replace(&mut chunk, Vec::with_capacity(CHUNK_LINES));
            if tx.send(full).is_err() {
                return;
            }
        }
        if finished {
            return;
        }
    }
}

pub fn ingest(
    input: Box<dyn Read + Send>,
    writer: &mut Writer,
    options: IngestOptions,
) -> Result<IngestReport> {
    let started = Instant::now();
    let (tx, rx) = sync_channel::<Vec<Message>>(64);
    let reader = thread::spawn(move || read_lines(input, tx));
    let mut report = IngestReport::default();
    let mut batch = Batch::default();
    let mut deadline: Option<Instant> = None;
    let mut flush = |batch: &mut Batch, report: &mut IngestReport| -> Result<()> {
        let full = std::mem::take(batch);
        let written = writer.write(full)?;
        report.points += written.points as u64;
        report.new_series += written.new_series as u64;
        report.commit_time += written.commit;
        report.commits += 1;
        Ok(())
    };
    loop {
        let wait = match deadline {
            Some(at) => at.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(3600),
        };
        match rx.recv_timeout(wait) {
            Ok(messages) => {
                for message in messages {
                    match message {
                        Message::Point(key, ts, value) => {
                            if batch.is_empty() {
                                deadline = Some(Instant::now() + options.batch_wait);
                            }
                            batch.push(&key, ts, value);
                            if batch.len() >= options.batch_points {
                                flush(&mut batch, &mut report)?;
                                deadline = None;
                            }
                        }
                        Message::Bad(line, error) => {
                            report.rejected += 1;
                            if report.first_errors.len() < 5 {
                                report.first_errors.push(format!("line {line}: {error}"));
                            }
                        }
                    }
                }
                if deadline.is_some_and(|at| Instant::now() >= at) {
                    flush(&mut batch, &mut report)?;
                    deadline = None;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !batch.is_empty() {
                    flush(&mut batch, &mut report)?;
                }
                deadline = None;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if !batch.is_empty() {
        flush(&mut batch, &mut report)?;
    }
    let _ = reader.join();
    report.elapsed = started.elapsed();
    Ok(report)
}
