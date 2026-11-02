use std::io::BufRead;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use ferro::Result;

use crate::model::{SeriesKey, now_ms};
use crate::parse::parse_line;
use crate::tsdb::{Batch, Writer};

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
}

enum Message {
    Point(SeriesKey, u64, f64),
    Bad(u64, String),
}

pub fn ingest(
    input: Box<dyn BufRead + Send>,
    writer: &mut Writer,
    options: IngestOptions,
) -> Result<IngestReport> {
    let started = Instant::now();
    let (tx, rx) = sync_channel::<Message>(131_072);
    let reader = thread::spawn(move || {
        for (index, line) in input.lines().enumerate() {
            let message = match line {
                Err(error) => Message::Bad(index as u64 + 1, error.to_string()),
                Ok(line) => match parse_line(&line) {
                    Ok(None) => continue,
                    Ok(Some(sample)) => {
                        Message::Point(sample.key, sample.ts.unwrap_or_else(now_ms), sample.value)
                    }
                    Err(error) => Message::Bad(index as u64 + 1, error),
                },
            };
            if tx.send(message).is_err() {
                return;
            }
        }
    });
    let mut report = IngestReport::default();
    let mut batch = Batch::default();
    let mut deadline: Option<Instant> = None;
    let mut flush = |batch: &mut Batch, report: &mut IngestReport| -> Result<()> {
        let full = std::mem::take(batch);
        let written = writer.write(full)?;
        report.points += written.points as u64;
        report.new_series += written.new_series as u64;
        report.commits += 1;
        Ok(())
    };
    loop {
        let wait = match deadline {
            Some(at) => at.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(3600),
        };
        match rx.recv_timeout(wait) {
            Ok(Message::Point(key, ts, value)) => {
                if batch.is_empty() {
                    deadline = Some(Instant::now() + options.batch_wait);
                }
                batch.push(key, ts, value);
                let due = deadline.is_some_and(|at| Instant::now() >= at);
                if batch.len() >= options.batch_points || due {
                    flush(&mut batch, &mut report)?;
                    deadline = None;
                }
            }
            Ok(Message::Bad(line, error)) => {
                report.rejected += 1;
                if report.first_errors.len() < 5 {
                    report.first_errors.push(format!("line {line}: {error}"));
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
