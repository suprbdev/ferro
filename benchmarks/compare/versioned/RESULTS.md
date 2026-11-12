# ferro vs versioned stores

Run: `make compare-versioned` (5 rounds by default; `ARGS='--rounds 3 --engines ferro,dolt'` narrows it). It builds the immudb and Dolt harnesses in a pinned Go image, then runs `benchmarks/compare/src/bin/versioned.rs`, which runs ferro natively and each Go harness in a fresh container, rotating the order each round, every run in a fresh directory, and prints medians and min..max to `benchmarks/compare/target/versioned/table.md`. A full run takes about three minutes once the Go modules are downloaded. The Go module and build caches stay in `benchmarks/compare/target/versioned/go`, about 5 GB; delete that directory to get the space back.

Versions: ferro 0.1.0 (path, at 081a652), immudb v1.11.2 (`embedded/store` as a library), Dolt 1.88.1 (embedded through `github.com/dolthub/driver` v1.88.1 and `database/sql`, no server). Go 1.26.9 in `golang:1.26.9-bookworm` pinned by digest, rustc 1.92.0. The Dolt harness builds with cgo for its zstd binding and the `gms_pure_go` tag, which swaps ICU regexes for Go's; the workload runs no regexes. Go module versions are pinned in each harness's `go.mod` and `go.sum`.

Hardware: AMD RYZEN AI MAX+ 395 (16 cores, 32 threads), 124 GB RAM, Crucial P310 2 TB NVMe with ext4 (`noatime`), Linux 7.2.15-arch1-1, Docker 29.9.3. Data dirs for all three engines are under `benchmarks/compare/target/versioned/runs` on that disk; the containers bind-mount it, run with `--memory=16g` and `--network none`, and Dolt's usage metrics are turned off.

## Workload

The same in every harness, with the same key and value sequences (splitmix64 over the index, values `value-{i}`), and each harness checks its own answers:

- Load: 100k keys in 100 commits of 1000 keys. The version after it is v0.
- Durable commits: 1000 commits that each overwrite one random existing key. The version after them is v1. File growth per commit is the size after a clean close, minus the size after the load.
- Commits not synced: 1000 more single-key commits with the engine's no-fsync mode, where it has one.
- Reads, after a reopen: 20,000 random gets and 1000 ranges of 100 keys from random start keys, at the newest version and at v0. Each runs twice and the second pass is reported. The v0 reads check every value.
- Diff of v0 and v1: 995 keys changed, and every engine has to find exactly 995.
- Verify everything retained, right after the diff, so all three verify the same 2,100 commits.
- A reopen, one get, then one timed single-key commit.
- Branch and merge (ferro, Dolt): fork `b`, commit 100 single-key changes on `b` and 100 on main to disjoint keys, then time the merge of `b` into main.
- Clone and pull (ferro, Dolt): clone into an empty database, commit 100 more single-key changes, then pull them.

How each engine does each step:

| step | ferro | immudb | Dolt |
| --- | --- | --- | --- |
| data | `Db<Map<u64, String>>` | 8-byte big-endian keys | table `t (k BIGINT PRIMARY KEY, v VARCHAR(64))`, keys biased by 2^63 to keep u64 order; one held connection, so one session |
| commit | `Db::update` | write-only tx, `Set`, `Commit`, which waits for indexing | `BEGIN`, `INSERT` or `REPLACE`, `CALL DOLT_COMMIT('-am', ...)`, `COMMIT` |
| durable | `Durability::Fsync`: fdatasync per commit, group commit | `Synced` with `SyncFrequency` 0: fsync per commit | journal fsync on every root update; there is no other mode |
| not synced | `Durability::Buffered` | `Synced` false | none, so these commits run durably and are not reported |
| old version | `Db::at(v0)` | `GetBetween(key, 1, v0)`, key reader `ReadBetween(1, v0)` | `AS OF '<v0 hash>'` |
| diff | `Map::diff` of the two snapshots, counted | none built in: read the tx log from v0 + 1 to v1, collect the keys, compare `GetBetween` at both versions | `SELECT COUNT(*) FROM DOLT_DIFF(v0, v1, 't')` |
| verify | `Db::verify`: re-hash every object reachable from every retained version | read every tx with its integrity checks (entry hash tree against the header, the linear hash chain) and every value against its hash | `dolt fsck` (`commands.FsckCmd`) in a child process: hash every chunk, walk the commit graph, validate each commit's trees |
| proof per key | none | `Get`, `ReadTx`, inclusion proof and dual proof to the newest tx, both verified | none |
| branch, merge | `Db::fork`, `Db::merge_with` with `Map::merge` | none | `DOLT_BRANCH`, ``USE `bench/b` ``, `DOLT_MERGE` |
| clone, pull | `have`, `bundle`, `receive` into a second file; bytes are the bundle's | none | `DOLT_PUSH` to a `file://` remote then `DOLT_CLONE`; pull is `DOLT_PUSH` then `DOLT_PULL`; bytes are the remote directory's growth |

All engines keep every version (ferro's default `Keep::All`; immudb and Dolt never prune by themselves), and none was compacted or garbage-collected. Every other option is the engine's default.

## 2026-11-12

Medians of 5 rounds, from a run started at 17:22. The machine was shared with other agents' builds and tests: the 1-minute load average was about 4 when the run started, rose to about 33 for a short time while it built the harnesses, and was 6.0 to 13.8 (median 8.3) at the start of each engine's run, falling through the run, on 32 threads; nothing was pinned. A first run on the same day, at d64ac77 and a load average of 5.2 to 9.4, gave medians that differ from these by up to 26% for immudb and 18% for Dolt, apart from immudb's p99 commit and first commit after a reopen, which moved more, so treat differences under about 30% as noise.

| workload | unit | ferro | immudb | dolt |
| --- | --- | ---: | ---: | ---: |
| load 100k keys, 100 commits of 1000 | s | 0.294 | 0.383 | 1.20 |
| commit of 1000 keys, median | ms | 2.73 | 3.66 | 11.9 |
| single-key commit, durable, p50 | ms | 0.371 | 1.83 | 1.03 |
| single-key commit, durable, p99 | ms | 0.487 | 2.17 | 1.35 |
| single-key commit, durable, mean | ms | 0.391 | 1.87 | 1.05 |
| single-key commit, not synced, p50 | ms | 0.018 | 0.020 | n/a |
| single-key commit, not synced, mean | ms | 0.018 | 0.021 | n/a |
| get at latest | us | 0.112 | 1.05 | 17.8 |
| range of 100 keys at latest | us | 2.35 | 38.4 | 48.5 |
| get at an old version | us | 0.129 | 1.06 | 21.9 |
| range of 100 keys at an old version | us | 2.33 | 40.2 | 52.0 |
| diff of versions 1000 commits apart | ms | 1.86 | 4.69 | 3.63 |
| verify everything retained | ms | 19.2 | 55.8 | 589 |
| get with inclusion and dual proofs | us | n/a | 287 | n/a |
| first single-key commit after a reopen | ms | 1.02 | 14.3 | 1.55 |
| create a branch | ms | 0.658 | n/a | 1.34 |
| merge 100 commits into 100 others | ms | 1.81 | n/a | 2.85 |
| clone into an empty database | ms | 380 | n/a | 1553 |
| of which the sending side | ms | 76.4 | n/a | 1501 |
| clone bytes | MiB | 33.5 | n/a | 105 |
| pull 100 commits | ms | 7.45 | n/a | 86.3 |
| pull bytes | KiB | 270 | n/a | 1162 |
| size after load | MiB | 39.9 | 19.8 | 95.5 |
| growth per single-key commit | bytes | 3236 | 3114 | 12225 |
| size at the end | MiB | 47.0 | 25.8 | 123 |
| reopen and first get | ms | 0.136 | 9.77 | 11.5 |

Min..max over the 5 rounds:

| workload | unit | ferro | immudb | dolt |
| --- | --- | ---: | ---: | ---: |
| load 100k keys, 100 commits of 1000 | s | 0.283..0.339 | 0.356..0.410 | 1.19..1.29 |
| commit of 1000 keys, median | ms | 2.59..3.28 | 3.31..3.88 | 11.7..13.3 |
| single-key commit, durable, p50 | ms | 0.369..0.420 | 1.76..1.96 | 1.01..1.09 |
| single-key commit, durable, p99 | ms | 0.425..0.701 | 2.00..2.22 | 1.25..1.42 |
| single-key commit, durable, mean | ms | 0.376..0.441 | 1.78..1.97 | 1.02..1.09 |
| single-key commit, not synced, p50 | ms | 0.017..0.018 | 0.019..0.022 | n/a |
| single-key commit, not synced, mean | ms | 0.018..0.019 | 0.020..0.025 | n/a |
| get at latest | us | 0.111..0.120 | 1.03..1.13 | 17.2..18.5 |
| range of 100 keys at latest | us | 2.27..2.44 | 37.7..39.7 | 46.8..50.4 |
| get at an old version | us | 0.127..0.135 | 1.04..1.08 | 21.5..22.4 |
| range of 100 keys at an old version | us | 2.25..2.36 | 38.4..41.0 | 50.3..56.9 |
| diff of versions 1000 commits apart | ms | 1.82..2.03 | 4.36..4.79 | 3.38..4.00 |
| verify everything retained | ms | 15.6..20.1 | 54.8..57.3 | 578..623 |
| get with inclusion and dual proofs | us | n/a | 282..288 | n/a |
| first single-key commit after a reopen | ms | 0.943..1.07 | 14.0..15.5 | 1.43..1.57 |
| create a branch | ms | 0.636..0.674 | n/a | 1.33..1.46 |
| merge 100 commits into 100 others | ms | 1.77..2.32 | n/a | 2.77..3.23 |
| clone into an empty database | ms | 377..394 | n/a | 1545..1576 |
| of which the sending side | ms | 72.4..80.8 | n/a | 1495..1525 |
| clone bytes | MiB | 33.5..33.5 | n/a | 104..106 |
| pull 100 commits | ms | 7.24..7.51 | n/a | 80.5..98.5 |
| pull bytes | KiB | 270..270 | n/a | 1111..1272 |
| size after load | MiB | 39.9..39.9 | 19.8..19.8 | 95.4..95.5 |
| growth per single-key commit | bytes | 3236..3236 | 3114..3114 | 12035..12335 |
| size at the end | MiB | 47.0..47.0 | 25.8..25.8 | 123..123 |
| reopen and first get | ms | 0.130..0.149 | 7.36..13.0 | 10.2..12.9 |

Since that first run, ferro's verify takes 89% less time, 19.2 ms against 180, after 14152da re-hashed on several threads and many hashes at once. Its clone takes 53% less, 380 ms against 816, and the sending side 76% less, 76.4 ms against 321, after 35f3465 built bundles on threads. Commits of 1000 keys take 43% less and the load 44% less after 3d4b272 kept the flush's threads, against 18% and 20% less for immudb between the two runs. The diff takes 56% less, 1.86 ms against 4.20, after c0e5b10 walked the nodes it opens in place, as [Diffs](../../../docs/internals/read-path.md#diffs) describes. Fork and merge take 53% and 42% less, which was not traced to one commit. The p99 durable commit fell by 46% for ferro and 71% for immudb; the first run had other processes' disk writes landing in some rounds. ferro's other rows moved by 5 to 21%, within the noise, and its sizes did not change. The reclamation work in between does not run in this workload.

Where ferro is ahead: the load takes 0.29 s against 0.38 for immudb and 1.20 for Dolt, and a commit of 1000 keys 2.73 ms against 3.66 and 11.9. A durable single-key commit takes 0.37 ms against 1.03 for Dolt and 1.83 for immudb, with a p99 of 0.49 ms against 1.35 and 2.17, and a commit not synced 0.018 ms against immudb's 0.020. Point reads are 8 to 9 times faster than immudb's and over 150 times faster than Dolt's through SQL, ranges of 100 keys 16 to 22 times faster than either, and a read at an old version costs about what a read at the newest does in all three. The diff takes 1.86 ms against 3.63 for Dolt and 4.69 for immudb. Verify takes 19.2 ms on its default 8 threads against 55.8 for immudb and 589 for Dolt's fsck, while checking more than immudb does, as the caveats say. The first commit after a reopen takes 1.02 ms against Dolt's 1.55 and immudb's 14.3. A fork takes half of Dolt's time, and a merge 1.81 ms against 2.85. A clone takes a quarter of Dolt's time and moves a third of its bytes; Dolt's is almost all its push to the remote, and it clones from the remote in about 50 ms, which suggests it copies storage files whole rather than object by object; that was not checked. A pull of 100 commits takes 7.5 ms and 270 KiB against Dolt's 86 ms and 1.1 MiB. ferro's file is 2.4 times smaller than Dolt's after the load, and reopening is a superblock read, 0.14 ms against 9.8 for immudb and 11.5 for Dolt. On each of these rows the rounds' ranges for ferro and the next engine do not overlap.

Where ferro lags, as a target for later work: the size after the load, 39.9 MiB against immudb's 19.8. Each 1000-key commit of random keys rewrites a large share of the tree's leaves, and every version is kept, while immudb appends each entry once to its logs and writes its index out in batches rather than at each commit. Growth per single-key commit is about the same, 3.2 KB against 3.1 KB.

The first commit after a reopen depends on the drive. Here ferro's took 0.94 to 1.07 ms in every round, 2.7 times a steady commit, and Dolt's 1.43 to 1.57 ms, while immudb's took 14.0 to 15.5 ms. In a run before the first, at a load average of 7 to 10, ferro's took 12.2 to 12.4 ms in every round and immudb's 14 to 61 ms, while Dolt's stayed at 1.5 to 1.9 ms, and in a one-off probe the first commit after reopening a freshly loaded file took 12 to 15 ms while that after reopening a file whose last session made two commits took 0.6 to 0.8 ms. Another probe saw the first few commits after `Db::verify` take 3 to 110 ms on a file still open, before falling back to 0.4 ms, which is why the harness reopens after verifying. Both come from the NVMe drive, not from any engine: with `nvme_core.default_ps_max_latency_us` at 100000 on this machine, the drive enters a low-power state after 30 to 100 ms without I/O, and the next sync waits for it to wake. A 300 ms sleep in place of the verify gives the same stall, a plain pwrite and fdatasync in Python after 300 ms idle takes 12 ms, and an `O_DIRECT` 4 KiB read every 20 ms during the pause brings the first commit after a reopen to 0.9 to 1.3 ms, as described in [Verify](../../../docs/internals/verify.md).

## Caveats

- Docker: immudb and Dolt run in containers, ferro natively. With bind mounts, `--network none` and no CPU limit, the syscalls and the disk are the host's. The immudb binary is static, so it was also run on the host, in five pairs alternating with the container: the durable commit p50 medians were 1.79 ms on the host and 1.94 ms in the container, get 1.66 and 2.14 us, range 41 and 62 us, 1000-key commit 3.79 and 3.68 ms, verify 61 and 67 ms, but the container's gets were faster in three pairs and slower in two, so apart from perhaps the commit the difference is within this machine's noise. Dolt was not measured outside Docker.
- Durability: ferro's Fsync group-commits with fdatasync; immudb fsyncs its value, tx and commit logs one after another, three syncs a commit, and with its default `SyncFrequency` of 20 ms a lone writer would also wait 5 ms before each sync, so the harness sets 0. Dolt fsyncs its journal at each root update, and its commit here is a SQL transaction plus a Dolt commit. Not synced means page cache only for ferro and immudb; Dolt has no such mode.
- Dolt runs through SQL: every get, range, diff and merge parses and plans a statement, so its read numbers measure embedded Dolt as an application would use it, not its storage layer. The `database/sql` driver opens a new session for every pooled call, so the harness holds one connection; without that, a get took 84 us rather than 23 in a smoke run.
- Verify is not like for like. ferro re-hashes every tree node and value reachable from every version. immudb's check covers its transaction log, the hash chain linking transactions and every value, not its B-tree index, which is not authenticated; immudb proves a key with a per-key inclusion and dual proof instead, which takes 287 us and has no ferro counterpart. Dolt's fsck hashes every chunk and validates each commit's trees, including chunks no branch reaches.
- immudb has no diff: its row reads the transaction log between the two versions and compares each touched key at both. It has no branches, merges, clones or pulls.
- Clone and pull: ferro builds the bundle in memory and receives it into a second file; Dolt pushes to a `file://` remote on the same disk and clones or pulls from it, writing the data twice. Byte counts are the bundle's length for ferro and the remote directory's growth for Dolt.
- Sizes are apparent file lengths after a clean close, so ferro's preallocation is not counted; immudb's include its index files, flushed at close. No engine was compacted, and Dolt's `dolt gc` would shrink its directory.
- 100k keys fit in every engine's caches and the page cache, every phase is single-threaded, and reads are warm, the second of two passes.
