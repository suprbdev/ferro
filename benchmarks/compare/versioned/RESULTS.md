# ferro vs versioned stores

Run: `make compare-versioned` (5 rounds by default; `ARGS='--rounds 3 --engines ferro,dolt'` narrows it). It builds the immudb and Dolt harnesses in a pinned Go image, then runs `benchmarks/compare/src/bin/versioned.rs`, which runs ferro natively and each Go harness in a fresh container, rotating the order each round, every run in a fresh directory, and prints medians and min..max to `benchmarks/compare/target/versioned/table.md`. A full run takes about three minutes once the Go modules are downloaded. The Go module and build caches stay in `benchmarks/compare/target/versioned/go`, about 5 GB; delete that directory to get the space back.

Versions: ferro 0.1.0 (path, at d64ac77), immudb v1.11.2 (`embedded/store` as a library), Dolt 1.88.1 (embedded through `github.com/dolthub/driver` v1.88.1 and `database/sql`, no server). Go 1.26.9 in `golang:1.26.9-bookworm` pinned by digest, rustc 1.92.0. The Dolt harness builds with cgo for its zstd binding and the `gms_pure_go` tag, which swaps ICU regexes for Go's; the workload runs no regexes. Go module versions are pinned in each harness's `go.mod` and `go.sum`.

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

Medians of 5 rounds. The machine was shared with other agents' builds and benchmarks: the 1-minute load average was 5.2 to 9.4 (median 6.7) at the start of each run, on 32 threads, and nothing was pinned. Two earlier runs of the harness, at load averages of 7 to 10 and 12 to 19 (the second before it reopened the file after verifying and timed a first commit), moved ferro's medians from these by up to 28% in either direction, and its p99 commit, fork and first commit after a reopen by more, so treat differences under about 30% between engines as noise unless all three runs agree.

| workload | unit | ferro | immudb | Dolt |
| --- | --- | ---: | ---: | ---: |
| load 100k keys, 100 commits of 1000 | s | 0.526 | 0.481 | 1.41 |
| commit of 1000 keys, median | ms | 4.81 | 4.46 | 13.7 |
| single-key commit, durable, p50 | ms | 0.396 | 2.39 | 1.12 |
| single-key commit, durable, p99 | ms | 0.908 | 7.60 | 3.34 |
| single-key commit, durable, mean | ms | 0.482 | 2.61 | 1.33 |
| single-key commit, not synced, p50 | ms | 0.021 | 0.023 | n/a |
| single-key commit, not synced, mean | ms | 0.021 | 0.026 | n/a |
| get at latest | us | 0.141 | 1.36 | 18.1 |
| range of 100 keys at latest | us | 2.47 | 48.1 | 50.8 |
| get at an old version | us | 0.153 | 1.31 | 22.6 |
| range of 100 keys at an old version | us | 2.46 | 54.6 | 53.1 |
| diff of versions 1000 commits apart | ms | 4.20 | 6.30 | 3.50 |
| verify everything retained | ms | 180 | 69.6 | 636 |
| get with inclusion and dual proofs | us | n/a | 304 | n/a |
| first single-key commit after a reopen | ms | 1.12 | 2.08 | 1.48 |
| create a branch | ms | 1.39 | n/a | 1.29 |
| merge 100 commits into 100 others | ms | 3.14 | n/a | 3.32 |
| clone into an empty database | ms | 816 | n/a | 1885 |
| of which the sending side | ms | 321 | n/a | 1817 |
| clone bytes | MiB | 33.5 | n/a | 106 |
| pull 100 commits | ms | 8.82 | n/a | 105 |
| pull bytes | KiB | 270 | n/a | 1132 |
| size after load | MiB | 39.9 | 19.8 | 95.5 |
| growth per single-key commit | bytes | 3236 | 3114 | 12261 |
| size at the end | MiB | 47.0 | 25.8 | 123 |
| reopen and first get | ms | 0.143 | 8.06 | 13.6 |

Spread, min..max over the 5 rounds, for the rows that moved most: durable commit p99 0.70..7.35 ms for ferro, 3.89..57.9 for immudb and 1.61..7.19 for Dolt, as other processes' disk writes landed in some rounds; immudb's ranges 38.3..80.5 us; ferro's 1000-key commit 4.23..5.65 ms against immudb's 3.44..5.21, so the two overlap; diff 3.92..5.04 ms for ferro against 3.41..5.17 for Dolt; ferro's fork 0.61..1.63 ms against Dolt's 1.23..2.57; merge 2.93..3.49 against 2.86..4.72. Sizes and byte counts are the same in every round for ferro and immudb; Dolt's sizes vary by under 3% and its pull bytes from 1098 to 1360 KiB.

Where ferro is ahead: a durable single-key commit takes 0.40 ms against 1.12 for Dolt and 2.39 for immudb. Point reads are about 10 times faster than immudb's and over 100 times faster than Dolt's through SQL, ranges of 100 keys about 20 times faster than either, and a read at an old version costs what a read at the newest does in all three. A pull of 100 commits takes 8.8 ms and 270 KiB against Dolt's 105 ms and 1.1 MiB, and a clone moves a third of Dolt's bytes in less than half the time. ferro's file is 2.4 times smaller than Dolt's after the load, and reopening is a superblock read. Fork and merge take about what Dolt's do: ferro was ahead of Dolt on each in one run and behind in another.

Where ferro lags, as targets for later work:

- Verify: 180 ms for 151k objects against 70 ms for immudb's 102k entries in 2,100 transactions, about 1.2 us an object against 0.7 us an entry, and 2.6 to 3.2 times slower across the three runs. immudb checks less, as the caveats say, but the per-object cost is the gap to close. Dolt's fsck takes 636 ms.
- Size after the load: 39.9 MiB against immudb's 19.8. Each 1000-key commit of random keys rewrites a large share of the tree's leaves, and every version is kept, while immudb appends each entry once to its logs and writes its index out in batches rather than at each commit. Growth per single-key commit is about the same, 3.2 KB against 3.1 KB.
- Receiving a clone: 816 ms for a 33.5 MiB bundle of 143k objects, 321 ms to build it, about 2.2 us an object or 104 MiB/s, and about 500 ms to receive it, 3.5 us an object or 67 MiB/s. Dolt pushes 106 MiB to its remote at 58 MiB/s, slower per byte than ferro builds its bundle, but clones from the remote in 50 to 70 ms, which suggests it copies storage files whole rather than object by object; that was not checked.
- Commits of 1000 keys: 4.81 ms against immudb's 4.46, and the load 0.53 s against 0.48. That is within the noise of one run, but ferro was behind by 8 to 27% in all three runs.
- Diff: 4.2 ms for 995 changed keys, about 4.2 us a key, against Dolt's 3.5 ms, which includes parsing the SQL; behind by 16 to 26% in all three runs. The two `Db::at` calls take under 0.1 ms of it.
- First commit after a reopen: 1.12 ms here, 2.8 times a steady commit, against Dolt's 1.48 ms, 1.3 times. In the earlier run at a load average of 7 to 10 it took 12.2 to 12.4 ms in every round for ferro and 14 to 61 ms for immudb, while Dolt's stayed at 1.5 to 1.9 ms, and in a one-off probe the first commit after reopening a freshly loaded file took 12 to 15 ms while that after reopening a file whose last session made two commits took 0.6 to 0.8 ms. It looks like a sync that waits on the filesystem's journal when other processes are writing, perhaps from extending the file, but that was not traced. Another probe saw the first few commits after `Db::verify` take 3 to 110 ms on a file still open, before falling back to 0.4 ms, which is why the harness reopens after verifying.

## Caveats

- Docker: immudb and Dolt run in containers, ferro natively. With bind mounts, `--network none` and no CPU limit, the syscalls and the disk are the host's. The immudb binary is static, so it was also run on the host, in five pairs alternating with the container: the durable commit p50 medians were 1.79 ms on the host and 1.94 ms in the container, get 1.66 and 2.14 us, range 41 and 62 us, 1000-key commit 3.79 and 3.68 ms, verify 61 and 67 ms, but the container's gets were faster in three pairs and slower in two, so apart from perhaps the commit the difference is within this machine's noise. Dolt was not measured outside Docker.
- Durability: ferro's Fsync group-commits with fdatasync; immudb fsyncs its value, tx and commit logs one after another, three syncs a commit, and with its default `SyncFrequency` of 20 ms a lone writer would also wait 5 ms before each sync, so the harness sets 0. Dolt fsyncs its journal at each root update, and its commit here is a SQL transaction plus a Dolt commit. Not synced means page cache only for ferro and immudb; Dolt has no such mode.
- Dolt runs through SQL: every get, range, diff and merge parses and plans a statement, so its read numbers measure embedded Dolt as an application would use it, not its storage layer. The `database/sql` driver opens a new session for every pooled call, so the harness holds one connection; without that, a get took 84 us rather than 23 in a smoke run.
- Verify is not like for like. ferro re-hashes every tree node and value reachable from every version. immudb's check covers its transaction log, the hash chain linking transactions and every value, not its B-tree index, which is not authenticated; immudb proves a key with a per-key inclusion and dual proof instead, which takes 304 us and has no ferro counterpart. Dolt's fsck hashes every chunk and validates each commit's trees, including chunks no branch reaches.
- immudb has no diff: its row reads the transaction log between the two versions and compares each touched key at both. It has no branches, merges, clones or pulls.
- Clone and pull: ferro builds the bundle in memory and receives it into a second file; Dolt pushes to a `file://` remote on the same disk and clones or pulls from it, writing the data twice. Byte counts are the bundle's length for ferro and the remote directory's growth for Dolt.
- Sizes are apparent file lengths after a clean close, so ferro's preallocation is not counted; immudb's include its index files, flushed at close. No engine was compacted, and Dolt's `dolt gc` would shrink its directory.
- 100k keys fit in every engine's caches and the page cache, every phase is single-threaded, and reads are warm, the second of two passes.
