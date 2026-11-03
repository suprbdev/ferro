# Design overview

A ferro database is one typed Rust value, `T`. There is no query language: you read `T` with
ordinary Rust and change it inside a closure. Every commit produces a new immutable version of `T`,
and all earlier versions stay readable.

Each subsystem has its own file in this directory. The [documentation index](../README.md#internals)
lists them with what each covers.

## Modules

The library is `ferro/src`:

- `lib.rs`: the crate root. It declares the modules and re-exports the public API; `epoch`, `graph`,
  `platform`, `reclaim` and `reentry` are private.
- `codec.rs`: the `Data` trait, `Encoder`, `Decoder`, primitive impls, and `Object`, `Ptr`,
  `hash_of` and `Bytes`.
- `store.rs`: one generation of the database: the append-only file, commit frames, superblocks,
  recovery, object reads with hash verification, node cache, and the `dedupe` table.
- `map.rs`: `Map<K, V>`, the persistent ordered tree, its optional summaries, `Cursor`, and `Val`,
  the handle reads return.
- `set.rs` and `list.rs`: `Set<T>`, a wrapper over `Map<T, ()>`, and `List<T>`, a wrapper over
  `Map<u64, T>`.
- `table.rs`: `Table<T: Record>`, a keyed collection with automatic secondary indexes.
- `db.rs`: `Db<T>`, `Snapshot<T>`, `Options`, commits, branches, coalescing, merges and reclamation
  cycles.
- `log.rs`: the commit chain and `Log`. `history.rs`: `History`, the lazy walk behind `history_of`
  and `history_of_range`.
- `graph.rs`: the search of the commit graph for best common ancestors, described under [Finding the
  base](merging.md#finding-the-base).
- `watch.rs`: `Watcher<T>`, `Closer<T>` and the per-branch feed behind them.
- `epoch.rs`: deferred freeing of nodes lent to guarded readers, described under [Guarded gets and
  the epoch scheme](read-scaling.md#guarded-gets-and-the-epoch-scheme).
- `reentry.rs`: the per-thread record of the locks a thread holds, behind `Error::Reentrant`,
  described under [Writes from inside a closure](reentrancy.md).
- `reclaim.rs`: the copy into a new generation, which `Db::compact` and `admin::compact` share, and
  `Reclaim`, `Space` and `Compacted`.
- `admin.rs`: functions that work on a path without user types: `info`, `log`, `verify`, `layout`,
  `space` and `compact`, and `Keep`.
- `platform.rs`: the file calls that differ between Unix and Windows, described under
  [Windows](windows.md).
- `sync.rs`: `Have`, `Bundle`, and `Db::pull`, `push` and `receive`, which copy a branch between
  files.
- `error.rs`: `Error` and `Result`.

Around it:

- `ferro-derive`: the `Data` and `Record` derive macros, which `ferro` re-exports.
- `ferro-cli`: the [`ferro` tool](../guide/cli.md).
- `ferro-bench`: the benchmark tools the measurements in these files come from.
- `benchmarks/compare`: the comparison with other embedded stores, excluded from the workspace.
- `fuzz`: the `cargo-fuzz` targets, described under [Fuzzing](testing.md#fuzzing).
- `examples`: the [example apps](../guide/examples.md).
