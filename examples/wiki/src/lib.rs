//! A wiki and document store on ferro, with drafts on branches and three-way merges of pages.
//!
//! [`model::Wiki`] is the database root. Each [`model::Page`] is a row unique by slug and indexed
//! by author and by each tag, and its body is a `List` of content-defined chunks, so an edit
//! rewrites only the chunks it touches and versions of a large page share the rest. Beside the
//! pages sit a map from each linked slug to the pages that link to it, for backlinks and orphans,
//! and a map from each search term to the pages that contain it and how often. The [`index::Tx`]
//! keeps both in step with every page it writes. A draft is a forked branch; publishing it is a
//! `merge_with` that merges each page three ways, line by line for the body, leaving conflict
//! markers where both sides changed the same lines. Page history comes from `history_of`, and diffs
//! and reverts read past versions with `at`. The modules hold the parts: `model` the rows, `chunk`
//! the chunker, `index` the transaction that maintains the indexes, `ops` the writes, `query`
//! lookups, search, backlinks and stats, `merge` and `merge3` the page and line merges, `text`
//! tokens, links and slugs, `render` output formatting, `synth` generated prose, and `cli` and
//! `commands` the command line. The `wiki`, `chunkexp` and `gencorpus` binaries and the tests use
//! this library.
//!
//! ```sh
//! cargo run --release -p wiki --bin wiki -- put home --file home.md --title Home
//! cargo run --release -p wiki --bin wiki -- search ferro merge
//! ```

pub mod chunk;
pub mod cli;
pub mod commands;
pub mod index;
pub mod merge;
pub mod merge3;
pub mod model;
pub mod ops;
pub mod query;
pub mod render;
pub mod synth;
pub mod text;

use std::error::Error;

/// The result the wiki's operations and commands return, with any error boxed.
pub type Res<T> = Result<T, Box<dyn Error>>;
