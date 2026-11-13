# Changelog

All notable changes to ferro are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) as it applies before 1.0: a minor release may break the
API, and a patch release only fixes.

The file format and the sync format change only in a minor release. A release that changes either
says so under **Format**, and a build refuses a file of another format with `Error::Format`, as
[Limitations](docs/guide/limitations.md) describes.

## [Unreleased]

### Changed

- **Breaking:** `Set`, `List` and `Table` return iterator types of their own rather than the `Map`
  types behind them: `set::Iter`, `set::Cursor` and `set::Diff`, `list::Iter`, and `table::Iter`,
  `table::Keys` and `table::Diff`. `Set::diff` yields a `set::Change<T>`, `Added(T)` or
  `Removed(T)`, where it yielded a `Change<T, ()>`, and a set cursor reads its value with
  `set::Cursor::value` where it called `key`.

## [0.1.0] - 2026-11-13

The first tagged release. ferro is an experimental embedded database for Rust in which the database
is one of your own types, persisted in one file, with every commit kept as an immutable version.

### Format

- File format 7, `ferro::FILE_FORMAT`, whose superblocks start with `FERRODB7`.
- Sync format 2, `ferro::SYNC_FORMAT`, the byte after the magic of each `Have` and `Bundle`.

### Features

- **Your type is the database.** Derive `Data`, open a file of your struct, read it through a
  `Snapshot`, and write it with closures over `&mut T` that commit atomically.
- **Persistent collections.** `Map`, `Set`, `List` and `Table` nest inside your type, clone in
  O(1), compare by root hash in O(1), and diff in O(changes). A `Summary` keeps range totals in
  O(log n).
- **Tables and indexes.** `#[derive(Record)]` keeps secondary, composite, unique and covering
  indexes up to date and generates their lookups. `find()` combines lookups with `and` and `or`,
  planned from their counts, with ranges and with lookups of another table or version.
- **History.** `at`, `as_of`, `history_of`, `bisect` and `diff` read old versions, and `restore`
  commits one again.
- **Branches and merges.** Forks are one small commit. Merges are three-way over the merge graph,
  criss-crosses included, with conflicts resolved in Rust.
- **Sync between files.** Two files exchange only the commits the other lacks, in one call or as
  bytes over any transport, and every object received is checked by hash and as its type.
- **Durability and integrity.** An append-only log with group commit, one fdatasync per batch.
  Every read re-hashes its object, `verify` re-hashes the file on several threads, and `head_hash`
  anchors tamper evidence.
- **Concurrency.** Readers never wait. One closure runs at a time on each branch, branches run in
  parallel, and `coalesce` lets queued closures share a version. Awaited writes and watchers run on
  any executor.
- **Watchers.** Subscribe to a branch, optionally filtered by a test, at a commit cost that does
  not grow with the number of watchers.
- **Bounded history.** Keep the last N versions or a time window. Background reclamation copies
  what is kept into a new file on several threads while writers run.
- **Schema evolution.** Fields are stored by name, so adding, removing, reordering and renaming
  them needs no migration.
- **Several processes.** Readers in other processes beside one writer, several writers with
  `Options::shared` on 64-bit Linux, and `Options::follow` to pick up other processes' commits.
- **The `ferro` tool.** Inspects, verifies and compacts any file without your types.

### Platforms

Each change is tested on Linux. The tests last ran on Windows on 2026-11-03, before this release,
and never on macOS. Windows needs NTFS. See [Limitations](docs/guide/limitations.md) for the rest.

[Unreleased]: https://github.com/archlemon/ferro/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/archlemon/ferro/releases/tag/v0.1.0
