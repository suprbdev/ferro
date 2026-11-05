//! A time-series metrics store on ferro, with rollups in a summarized map and reclaimed history.
//!
//! [`model::Metrics`] is the database root. Each series is a [`model::SeriesRec`] row, unique by
//! metric and labels and indexed by metric and by each `name=value` label pair, so a selector
//! starts from the smallest matching index. Points live in fixed time partitions: a map from
//! partition to a map from series to a [`chunk::Chunk`] of delta-encoded timestamps and XOR-encoded
//! values held as [`ferro::Bytes`], so retention drops whole partitions with one range removal.
//! Downsampled [`model::Rollup`] rows go in a map whose summary is itself a rollup, so a series'
//! aggregate over a time range is one `range_summary`. The modules hold the parts: `model` the root
//! and rows, `chunk` the point encoding, `parse` the `metric{label="value"} value [ts]` lines,
//! selectors and durations, `ingest` batched ingestion from a reader, `tsdb` the writer, queries,
//! aggregates, downsampling, retention and verification, and `load` a synthetic write load that
//! keeps a bounded history and reclaims space as it goes. The `metrics` binary and the tests use
//! this library.
//!
//! ```sh
//! cargo run --release -p metrics -- ingest samples.txt
//! cargo run --release -p metrics -- aggregate 'http_requests{job="api"}' --fn rate --step 1m
//! ```

pub mod chunk;
pub mod ingest;
pub mod load;
pub mod model;
pub mod parse;
pub mod tsdb;
