//! A social graph service on ferro: follows, blocks, posts and likes, with graph analytics.
//!
//! [`model::Graph`] is the database root. Follow edges are kept in both directions as an
//! [`adj::Adj`], a map from `(user, first neighbour)` to a sorted, delta-encoded block of neighbour
//! ids held as [`ferro::Bytes`], so a page of followers decodes only the blocks it reads and
//! [`adj::Adj::diff`] only the blocks that changed between two versions. Users and posts are
//! tables, indexed by unique name and by author, and degree counts, blocks, likes and fan-out
//! inboxes are maps and sets. The modules hold the parts: `model` the root and rows, `adj` the
//! adjacency blocks, `ops` the writes, `query` paging, mutuals, paths, reach, recommendations and
//! timelines, `analytics` pagerank, connected components and label propagation, `seed` and `synth`
//! a power-law graph generator, `cli` and `commands` the command line, `bench` its benchmarks and
//! `experiment` a comparison of other adjacency layouts. The `graph` binary and the tests use this
//! library.
//!
//! ```sh
//! cargo run --release -p graph -- seed --users 10000 --edges 100000
//! cargo run --release -p graph -- followers u1
//! ```

pub mod adj;
pub mod analytics;
pub mod bench;
pub mod cli;
pub mod commands;
pub mod experiment;
pub mod model;
pub mod ops;
pub mod query;
pub mod seed;
pub mod synth;
