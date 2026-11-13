# ferro documentation

The guides describe what ferro does and how to use it. The internals describe how it works, what
each part costs, and what was measured and left out on the way. Their figures were measured when
each change landed, often on a machine shared with other work, and record the decisions they
informed. [Performance](guide/performance.md) has the current figures, measured on one idle
machine, most of them on 2026-11-05. Start with [Getting started](guide/getting-started.md).

## Guides

| File | Covers |
| --- | --- |
| [Getting started](guide/getting-started.md) | A first database, reading, writing and `Options` |
| [Collections](guide/collections.md) | `Map`, `Set`, `List`, `Bytes`, bulk reads, cursors, prefixes, range edits and summaries |
| [Tables and indexes](guide/tables.md) | `Table`, `#[derive(Record)]`, secondary, composite, unique and covering indexes, combined lookups |
| [History, versions and branches](guide/history.md) | `at`, `as_of`, `diff`, `history_of`, `bisect`, stamps, forks, dropping branches, `head_hash` |
| [Merging](guide/merging.md) | Three-way merges, which branches merge, criss-crosses and merges that can fail |
| [Watchers](guide/watchers.md) | `watch`, `watch_when`, `watch_changes`, closers and async waits |
| [Sync between files](guide/sync.md) | `pull`, `push`, `have`, `bundle` and `receive`, streaming and converging devices |
| [Durability and crash safety](guide/durability.md) | Fsync and Buffered, recovery, verification and errors on read |
| [Retention and reclamation](guide/retention.md) | `Keep`, background cycles and their threads, `compact` and what pruning means |
| [Writers and concurrency](guide/concurrency.md) | Branch locks, awaited writes, `coalesce`, reentrancy, several processes and following them |
| [Changing your types](guide/schema-evolution.md) | Adding, removing, reordering and renaming fields, and when an index rebuilds |
| [The `ferro` tool](guide/cli.md) | Inspecting, verifying and compacting a file without your types |
| [Performance](guide/performance.md) | Headline numbers, the comparison with other stores, and allocators |
| [Example apps](guide/examples.md) | The ten apps in `examples/` and their load tests |
| [Limitations](guide/limitations.md) | What it does not do, or does not do well yet |
| [Development](guide/development.md) | `Makefile` targets, benchmarks and CI |

## Internals

| File | Covers |
| --- | --- |
| [Design overview](internals/overview.md) | The modules and crates |
| [Storage](internals/storage.md) | Objects, hashes, records, deduplication, frames, superblocks and recovery |
| [The Merkle search tree](internals/tree.md) | Tree shape, flush, parallel flush, batches and spilled values |
| [Summaries](internals/summaries.md) | How a fold is kept on every link and what it costs |
| [The read path](internals/read-path.md) | Memos, prefetching, value columns, scans, cursors and cold reads |
| [Reading from many threads](internals/read-scaling.md) | Guarded gets, the epoch scheme, iterator frames, the allocator and cache weight |
| [Writers and the commit path](internals/commit-path.md) | Lanes, `tip`, group commit, a lock per branch and drafting |
| [Writers in several processes](internals/processes.md) | `Options::shared`, the locks, catching up, superblocks and cycles across processes, and followers |
| [Commits of a few rows](internals/commit-cost.md) | Where a small multi-table commit's CPU time goes |
| [Bytes of a small commit](internals/commit-bytes.md) | Where a single-key commit's bytes go, and the encodings measured to cut them |
| [Coalescing](internals/coalescing.md) | Groups of closures sharing a version |
| [Awaited writes](internals/async-writes.md) | Futures, the gate and the sync thread |
| [Writes from inside a closure](internals/reentrancy.md) | Deadlocks refused with `Error::Reentrant` |
| [Watchers](internals/watchers.md) | The feed, the watch thread and its cost |
| [History and the commit log](internals/history.md) | The chain, skip links, pruning, `head_hash`, commit times and dropping a branch |
| [Merging and the merge graph](internals/merging.md) | Merge links, the other branch's state, the base search, criss-crosses and pruning |
| [Sync](internals/sync.md) | Haves, bundles, landing, bringing the branch up, measurements and limits |
| [Receiving a bundle](internals/sync-receive.md) | Staging, commits stored twice, type checks and recomputed summaries |
| [Reclamation](internals/reclamation.md) | Cycles, the copy on several threads, generations, crashes, measurements and offline compaction |
| [Windows](internals/windows.md) | Locks, durability and renaming over an open file |
| [Schema evolution, unique and covering indexes](internals/schema-and-unique-indexes.md) | Field tags, unique constraints and ranges, covered fields and their cost, index fingerprints and combined lookups |
| [Errors and checks on local reads](internals/errors.md) | Panics, `Result` and bounds on crafted files |
| [Verifying a file](internals/verify.md) | The walk, reads, hashes checked many at once, threads, and the slow sync after an idle disk |
| [Fuzzing and testing](internals/testing.md) | Fuzz targets, the test hooks behind the `testing` feature, and where the other tests are described |
| [Regressions between measurements](internals/regressions.md) | Figures that came out slower when the performance guide was measured again, and their fixes |
| [A delta log between checkpoints](internals/delta-log.md) | A design that was built, measured and left out |
| [Alternatives measured and rejected](internals/alternatives.md) | An index of everything left out, by subsystem |
