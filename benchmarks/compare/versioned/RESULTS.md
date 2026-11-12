# ferro vs versioned stores

Run: `make compare-versioned` (5 rounds by default; `ARGS='--rounds 3 --engines ferro,dolt'` narrows it, and `ARGS='--engines irmin'` runs one engine alone). It builds the immudb and Dolt harnesses in a pinned Go image and the Irmin harness in an OCaml image, then runs `benchmarks/compare/src/bin/versioned.rs`, which runs ferro natively and each other harness in a fresh container, rotating the order each round, every run in a fresh directory, and prints medians and min..max to `benchmarks/compare/target/versioned/table.md`. A full run takes about two minutes once the Go modules are downloaded and the OCaml image is built. The Go module and build caches stay in `benchmarks/compare/target/versioned/go`, about 5 GB, and the Irmin image `ferro-versioned-irmin`, about 2 GB with its base, stays in Docker; delete that directory and `docker rmi` the image to get the space back.

Versions: ferro 0.1.0 (path, at b231197), immudb v1.11.2 (`embedded/store` as a library), Dolt 1.88.1 (embedded through `github.com/dolthub/driver` v1.88.1 and `database/sql`, no server), Irmin 3.11.0 with irmin-pack 3.11.0 (`Irmin_pack_unix.KV`, as a library). Go 1.26.9 in `golang:1.26.9-bookworm` pinned by digest, OCaml 5.2.1 (no flambda) in `ocaml/opam:debian-12-ocaml-5.2` pinned by digest, rustc 1.92.0. The Dolt harness builds with cgo for its zstd binding and the `gms_pure_go` tag, which swaps ICU regexes for Go's; the workload runs no regexes. Go module versions are pinned in each harness's `go.mod` and `go.sum`; the Irmin image installs every opam package at the version in `irmin/irmin_bench.opam.locked`, from the base image's opam repository, and dune builds the harness in its release profile.

Hardware: AMD RYZEN AI MAX+ 395 (16 cores, 32 threads), 124 GB RAM, Crucial P310 2 TB NVMe with ext4 (`noatime`), Linux 7.2.15-arch1-1, Docker 29.9.3. Data dirs for all four engines are under `benchmarks/compare/target/versioned/runs` on that disk; the containers bind-mount it, run with `--memory=16g` and `--network none`, and Dolt's usage metrics are turned off.

## Workload

The same in every harness, with the same key and value sequences (splitmix64 over the index, values `value-{i}`), and each harness checks its own answers:

- Load: 100k keys in 100 commits of 1000 keys. The version after it is v0.
- Durable commits: 1000 commits that each overwrite one random existing key. The version after them is v1. File growth per commit is the size after a clean close, minus the size after the load.
- Commits not synced: 1000 more single-key commits with the engine's no-fsync mode, where it has one.
- Reads, after a reopen: 20,000 random gets and 1000 ranges of 100 keys from random start keys, at the newest version and at v0. Each runs twice and the second pass is reported. The v0 reads check every value.
- Diff of v0 and v1: 995 keys changed, and every engine has to find exactly 995.
- Verify everything retained, right after the diff, so all four verify the same 2,100 commits.
- A reopen, one get, then one timed single-key commit.
- Branch and merge (ferro, Dolt, Irmin): fork `b`, commit 100 single-key changes on `b` and 100 on main to disjoint keys, then time the merge of `b` into main.
- Clone and pull (ferro, Dolt): clone into an empty database, commit 100 more single-key changes, then pull them.

How each engine does each step:

| step | ferro | immudb | Dolt | Irmin |
| --- | --- | --- | --- | --- |
| data | `Db<Map<u64, String>>` | 8-byte big-endian keys | table `t (k BIGINT PRIMARY KEY, v VARCHAR(64))`, keys biased by 2^63 to keep u64 order; one held connection, so one session | string contents at path `[first 3 hex digits; all 16 hex digits]` of the key, so 4096 directories of about 24 keys; a range lists the directory of its start key and the ones after it, sorts the names and reads values until it has 100 |
| commit | `Db::update` | write-only tx, `Set`, `Commit`, which waits for indexing | `BEGIN`, `INSERT` or `REPLACE`, `CALL DOLT_COMMIT('-am', ...)`, `COMMIT` | `Store.with_tree_exn` on the root for 1000 keys, `Store.set_exn` for one: write the tree and commit, then test-and-set the branch |
| durable | `Durability::Fsync`: fdatasync per commit, group commit | `Synced` with `SyncFrequency` 0: fsync per commit | journal fsync on every root update; there is no other mode | `use_fsync` true: at the end of each commit, fsync the pack suffix and the index log, and the dict and control file when they changed; the branch file is written but never synced |
| not synced | `Durability::Buffered` | `Synced` false | none, so these commits run durably and are not reported | `use_fsync` false, the default |
| old version | `Db::at(v0)` | `GetBetween(key, 1, v0)`, key reader `ReadBetween(1, v0)` | `AS OF '<v0 hash>'` | `Commit.of_hash` v0, then its tree |
| diff | `Map::diff` of the two snapshots, counted | none built in: read the tx log from v0 + 1 to v1, collect the keys, compare `GetBetween` at both versions | `SELECT COUNT(*) FROM DOLT_DIFF(v0, v1, 't')` | `Tree.diff` of the two commits' trees, counted |
| verify | `Db::verify`: re-hash every object reachable from every retained version | read every tx with its integrity checks (entry hash tree against the header, the linear hash chain) and every value against its hash | `dolt fsck` (`commands.FsckCmd`) in a child process: hash every chunk, walk the commit graph, validate each commit's trees | `integrity_check ~auto_repair:false`: read every object the index lists and check it against its hash |
| proof per key | none | `Get`, `ReadTx`, inclusion proof and dual proof to the newest tx, both verified | none | none |
| branch, merge | `Db::fork`, `Db::merge_with` with `Map::merge` | none | `DOLT_BRANCH`, ``USE `bench/b` ``, `DOLT_MERGE` | `Store.clone ~dst:"b"`, `Store.merge_with_branch` |
| clone, pull | `have`, `bundle`, `receive` into a second file; bytes are the bundle's | none | `DOLT_PUSH` to a `file://` remote then `DOLT_CLONE`; pull is `DOLT_PUSH` then `DOLT_PULL`; bytes are the remote directory's growth | none that works on irmin-pack, as the caveats say |

Irmin's store uses the inode settings Tezos uses (32 entries, `stable_hash` 256, `Seeded_hash` child order, varint length headers), with every `Irmin_pack.config` option at its default apart from `use_fsync`. All engines keep every version (ferro's default `Keep::All`; immudb, Dolt and Irmin never prune by themselves), and none was compacted or garbage-collected. Every other option is the engine's default.

## 2026-11-12

Medians of 5 rounds, from a run started at 19:23 at b231197, the first with Irmin. The machine was quieter than for the earlier runs that day: the 1-minute load average was 2.0 when the run started and 2.4 to 2.6 (median 2.5) at the start of each engine's run, on 32 threads; nothing was pinned. Against a run at 17:22, at 081a652 without Irmin and at a load average of 6.0 to 13.8, the medians of the other three engines moved by up to 25% (ferro's diff and merge, immudb's gets), immudb's reopen by 31%, and ferro's first commit after a reopen by much more, for the reason given below. A first run on the same day, at d64ac77 and a load average of 5.2 to 9.4, gave medians that differ from the 17:22 ones by up to 26% for immudb and 18% for Dolt, apart from immudb's p99 commit and first commit after a reopen, which moved more, so treat differences under about 30% as noise.

| workload | unit | ferro | immudb | dolt | irmin |
| --- | --- | ---: | ---: | ---: | ---: |
| load 100k keys, 100 commits of 1000 | s | 0.314 | 0.394 | 1.31 | 5.11 |
| commit of 1000 keys, median | ms | 2.86 | 3.68 | 13.5 | 50.7 |
| single-key commit, durable, p50 | ms | 0.411 | 1.96 | 1.13 | 1.97 |
| single-key commit, durable, p99 | ms | 0.489 | 2.22 | 1.42 | 2.41 |
| single-key commit, durable, mean | ms | 0.429 | 1.97 | 1.14 | 2.00 |
| single-key commit, not synced, p50 | ms | 0.018 | 0.021 | n/a | 0.101 |
| single-key commit, not synced, mean | ms | 0.019 | 0.023 | n/a | 0.124 |
| get at latest | us | 0.115 | 1.31 | 17.8 | 3.31 |
| range of 100 keys at latest | us | 2.36 | 40.2 | 47.0 | 59.0 |
| get at an old version | us | 0.133 | 1.31 | 22.0 | 3.41 |
| range of 100 keys at an old version | us | 2.27 | 41.2 | 52.0 | 61.9 |
| diff of versions 1000 commits apart | ms | 2.27 | 4.32 | 3.34 | 44.2 |
| verify everything retained | ms | 18.4 | 56.2 | 590 | 1289 |
| get with inclusion and dual proofs | us | n/a | 284 | n/a | n/a |
| first single-key commit after a reopen | ms | 8.16 | 14.2 | 1.64 | 12.9 |
| create a branch | ms | 0.742 | n/a | 1.54 | 0.009 |
| merge 100 commits into 100 others | ms | 2.26 | n/a | 2.94 | 86.5 |
| clone into an empty database | ms | 403 | n/a | 1576 | n/a |
| of which the sending side | ms | 78.2 | n/a | 1528 | n/a |
| clone bytes | MiB | 33.5 | n/a | 105 | n/a |
| pull 100 commits | ms | 7.99 | n/a | 86.1 | n/a |
| pull bytes | KiB | 270 | n/a | 1140 | n/a |
| size after load | MiB | 39.9 | 19.8 | 95.5 | 55.7 |
| growth per single-key commit | bytes | 3236 | 3114 | 12233 | 1915 |
| size at the end | MiB | 47.0 | 25.8 | 123 | 59.7 |
| reopen and first get | ms | 0.144 | 12.8 | 11.1 | 158 |

Min..max over the 5 rounds:

| workload | unit | ferro | immudb | dolt | irmin |
| --- | --- | ---: | ---: | ---: | ---: |
| load 100k keys, 100 commits of 1000 | s | 0.298..0.342 | 0.376..0.394 | 1.23..1.35 | 4.83..5.56 |
| commit of 1000 keys, median | ms | 2.80..3.32 | 3.54..3.73 | 12.5..14.1 | 46.4..52.9 |
| single-key commit, durable, p50 | ms | 0.375..0.417 | 1.94..2.02 | 1.12..1.14 | 1.94..2.02 |
| single-key commit, durable, p99 | ms | 0.479..0.583 | 2.15..2.37 | 1.36..1.50 | 2.36..2.49 |
| single-key commit, durable, mean | ms | 0.393..0.434 | 1.94..2.03 | 1.13..1.15 | 1.98..2.06 |
| single-key commit, not synced, p50 | ms | 0.018..0.019 | 0.020..0.023 | n/a | 0.098..0.108 |
| single-key commit, not synced, mean | ms | 0.019..0.019 | 0.021..0.025 | n/a | 0.114..0.132 |
| get at latest | us | 0.114..0.137 | 1.02..1.54 | 17.3..18.1 | 2.83..3.45 |
| range of 100 keys at latest | us | 2.29..2.71 | 35.9..44.2 | 46.8..48.1 | 56.8..73.7 |
| get at an old version | us | 0.132..0.165 | 1.02..1.46 | 21.3..22.5 | 2.98..3.68 |
| range of 100 keys at an old version | us | 2.24..2.29 | 38.9..41.2 | 51.1..54.8 | 49.5..76.3 |
| diff of versions 1000 commits apart | ms | 1.88..2.71 | 3.88..4.89 | 3.31..3.71 | 43.3..46.9 |
| verify everything retained | ms | 16.0..19.5 | 53.4..58.2 | 584..636 | 1083..1303 |
| get with inclusion and dual proofs | us | n/a | 278..287 | n/a | n/a |
| first single-key commit after a reopen | ms | 1.31..8.79 | 13.9..14.3 | 1.55..1.82 | 12.6..28.2 |
| create a branch | ms | 0.655..0.807 | n/a | 1.49..1.90 | 0.008..0.017 |
| merge 100 commits into 100 others | ms | 2.14..2.34 | n/a | 2.89..3.12 | 80.9..90.2 |
| clone into an empty database | ms | 390..412 | n/a | 1551..1595 | n/a |
| of which the sending side | ms | 70.5..78.9 | n/a | 1503..1547 | n/a |
| clone bytes | MiB | 33.5..33.5 | n/a | 104..106 | n/a |
| pull 100 commits | ms | 7.30..8.54 | n/a | 85.6..96.1 | n/a |
| pull bytes | KiB | 270..270 | n/a | 1118..1235 | n/a |
| size after load | MiB | 39.9..39.9 | 19.8..19.8 | 95.5..95.5 | 55.7..55.7 |
| growth per single-key commit | bytes | 3236..3236 | 3114..3114 | 12101..12385 | 1915..1915 |
| size at the end | MiB | 47.0..47.0 | 25.8..25.8 | 123..123 | 59.7..59.7 |
| reopen and first get | ms | 0.136..0.147 | 7.06..15.1 | 10.7..13.2 | 138..167 |

Between that first run and the one at 17:22, ferro's verify came to take 89% less time, 19.2 ms against 180, after 14152da re-hashed on several threads and many hashes at once. Its clone came to take 53% less, 380 ms against 816, and the sending side 76% less, 76.4 ms against 321, after 35f3465 built bundles on threads. Commits of 1000 keys took 43% less and the load 44% less after 3d4b272 kept the flush's threads, against 18% and 20% less for immudb between the two runs. The diff took 56% less, 1.86 ms against 4.20, after c0e5b10 walked the nodes it opens in place, as [Diffs](../../../docs/internals/read-path.md#diffs) describes. Fork and merge took 53% and 42% less, which was not traced to one commit. The p99 durable commit fell by 46% for ferro and 71% for immudb; the first run had other processes' disk writes landing in some rounds. ferro's other rows moved by 5 to 21%, within the noise, and its sizes did not change. The reclamation work in between does not run in this workload, and the superblock fixes between 081a652 and b231197 left the reopen at 0.14 ms.

Where ferro is ahead: the load takes 0.31 s against 0.39 for immudb, 1.31 for Dolt and 5.11 for Irmin, and a commit of 1000 keys 2.86 ms against 3.68, 13.5 and 50.7. A durable single-key commit takes 0.41 ms against 1.13 for Dolt, 1.96 for immudb and 1.97 for Irmin, with a p99 of 0.49 ms against 1.42, 2.22 and 2.41, and a commit not synced 0.018 ms against immudb's 0.021 and Irmin's 0.101. Point reads are 10 to 11 times faster than immudb's, 26 to 29 times faster than Irmin's and over 150 times faster than Dolt's through SQL, ranges of 100 keys 17 to 27 times faster than any of the three, and a read at an old version costs about what a read at the newest does in all four. The diff takes 2.27 ms against 3.34 for Dolt, 4.32 for immudb and 44.2 for Irmin. Verify takes 18.4 ms on its default 8 threads against 56.2 for immudb, 590 for Dolt's fsck and 1289 for Irmin's integrity check, while checking more than immudb does, as the caveats say. A merge takes 2.26 ms against 2.94 for Dolt and 86.5 for Irmin. A clone takes a quarter of Dolt's time and moves a third of its bytes; Dolt's is almost all its push to the remote, and it clones from the remote in about 50 ms, which suggests it copies storage files whole rather than object by object; that was not checked. A pull of 100 commits takes 8.0 ms and 270 KiB against Dolt's 86 ms and 1.1 MiB. ferro's file is 2.4 times smaller than Dolt's and 1.4 times smaller than Irmin's after the load, and reopening is a superblock read, 0.14 ms against 11.1 for Dolt, 12.8 for immudb and 158 for Irmin, which reads its index log into memory on open. On each of these rows the rounds' ranges for ferro and the next engine do not overlap.

Where ferro lags, as a target for later work: the size after the load, 39.9 MiB against immudb's 19.8. Each 1000-key commit of random keys rewrites a large share of the tree's leaves, and every version is kept, while immudb appends each entry once to its logs and writes its index out in batches rather than at each commit. Growth per single-key commit is about the same as immudb's, 3.2 KB against 3.1 KB, and Irmin's is 1.9 KB, 41% less than ferro's; Irmin rewrites the inodes on the path to the changed key, each of at most 32 entries. Creating a branch takes 0.74 ms against 1.54 for Dolt but 0.009 ms for Irmin, whose fork writes one entry to its branch file and does not sync it.

The first commit after a reopen depends on the drive. In this run ferro's took 1.31 ms in the first round and 6.7 to 8.8 ms in the other four, against 0.41 ms for a steady commit, immudb's 13.9 to 14.3 ms and Irmin's 12.6 to 28.2 ms, while Dolt's took 1.55 to 1.82 ms. In the 17:22 run, at a higher load, ferro's took 0.94 to 1.07 ms in every round and Dolt's 1.43 to 1.57 ms, while immudb's took 14.0 to 15.5 ms. In a run before the first, at a load average of 7 to 10, ferro's took 12.2 to 12.4 ms in every round and immudb's 14 to 61 ms, while Dolt's stayed at 1.5 to 1.9 ms, and in a one-off probe the first commit after reopening a freshly loaded file took 12 to 15 ms while that after reopening a file whose last session made two commits took 0.6 to 0.8 ms. Another probe saw the first few commits after `Db::verify` take 3 to 110 ms on a file still open, before falling back to 0.4 ms, which is why the harness reopens after verifying. Both come from the NVMe drive, not from any engine: with `nvme_core.default_ps_max_latency_us` at 100000 on this machine, the drive enters a low-power state after 30 to 100 ms without I/O, and the next sync waits for it to wake. A 300 ms sleep in place of the verify gives the same stall, a plain pwrite and fdatasync in Python after 300 ms idle takes 12 ms, and an `O_DIRECT` 4 KiB read every 20 ms during the pause brings the first commit after a reopen to 0.9 to 1.3 ms, as described in [Verify](../../../docs/internals/verify.md). The quieter machine in this run, with less of other processes' I/O to keep the drive awake, fits the slower first commits.

## Caveats

- Docker: immudb, Dolt and Irmin run in containers, ferro natively. With bind mounts, `--network none` and no CPU limit, the syscalls and the disk are the host's. The immudb binary is static, so it was also run on the host, in five pairs alternating with the container: the durable commit p50 medians were 1.79 ms on the host and 1.94 ms in the container, get 1.66 and 2.14 us, range 41 and 62 us, 1000-key commit 3.79 and 3.68 ms, verify 61 and 67 ms, but the container's gets were faster in three pairs and slower in two, so apart from perhaps the commit the difference is within this machine's noise. Dolt and Irmin were not measured outside Docker.
- Durability: ferro's Fsync group-commits with fdatasync; immudb fsyncs its value, tx and commit logs one after another, three syncs a commit, and with its default `SyncFrequency` of 20 ms a lone writer would also wait 5 ms before each sync, so the harness sets 0. Dolt fsyncs its journal at each root update, and its commit here is a SQL transaction plus a Dolt commit. Irmin with `use_fsync` calls fsync, not fdatasync, on its pack suffix and its index log at the end of each commit, and on its dict and control file when they changed, so two syncs for a commit that adds no new names, as the single-key commits here do not; the branch file that names the new head is written in place and never synced, so after a crash a branch can name an older commit although the newer commits' objects are on disk, a weaker promise than the other three make. Not synced means page cache only for ferro, immudb and Irmin; Dolt has no such mode.
- Irmin runs as native OCaml 5.2.1 code with the default GC settings, so minor and major collections land inside the timed sections, as Go's collector does for immudb and Dolt, and every call goes through Lwt promises, as Irmin's API requires.
- Irmin's layout and ranges are the harness's choice, so its read rows are not like for like either. irmin-pack stores a directory of more than 32 entries as a tree of inodes ordered by a hash of each name, so `Tree.list` returns names in no useful order and nothing seeks to a key; the harness shards keys into 4096 directories by their first three hex digits and sorts each directory it lists, which an application wanting ordered ranges would also have to do. A flat directory of 100k keys would make each range list all of them.
- Irmin's reads build a fresh tree from the commit on each pass, as ferro takes a fresh snapshot, and so read through irmin-pack's LRU of decoded objects. A store handle also keeps the tree it has read, values included: through `Store.tree` on one branch handle, in a separate probe, the second pass took 0.45 us a get and 42 us a range at the newest version, and a v0 tree kept between the passes 0.60 us a get.
- Irmin's options are the defaults, and irmin-pack's default indexing strategy, `always`, puts every object's hash in its index, whose log the store reads into memory on open. Tezos uses `minimal`, which indexes commits only. In one probe run with `minimal`, against one with the default, the load took 3.67 s against 5.25, a 1000-key commit 33.6 ms against 53.7, the size after the load was 36.8 MiB against 55.7, growth per commit 1523 bytes against 1915 and reopen 43 ms against 148, while single-key commits, reads, diff and merge stayed within 10%. Its integrity check then walks only the trees of the branch heads, in 244 ms, so it no longer covers every retained version.
- The inode settings irmin-pack's documentation recommends for new stores, `Hash_bits` child order and `stable_hash` 0, raised `Invalid_depth` partway through the load with irmin-pack 3.11.0, so the harness uses those of Tezos.
- Dolt runs through SQL: every get, range, diff and merge parses and plans a statement, so its read numbers measure embedded Dolt as an application would use it, not its storage layer. The `database/sql` driver opens a new session for every pooled call, so the harness holds one connection; without that, a get took 84 us rather than 23 in a smoke run.
- Verify is not like for like. ferro re-hashes every tree node and value reachable from every version. immudb's check covers its transaction log, the hash chain linking transactions and every value, not its B-tree index, which is not authenticated; immudb proves a key with a per-key inclusion and dual proof instead, which takes 287 us and has no ferro counterpart. Dolt's fsck hashes every chunk and validates each commit's trees, including chunks no branch reaches. Irmin's integrity check, with the default indexing strategy, reads every object its index lists, reachable or not, and checks it against its hash, but does not walk the commit graph or check that a node's children are present.
- immudb has no diff: its row reads the transaction log between the two versions and compares each touched key at both. It has no branches, merges, clones or pulls.
- Irmin's merge is a three-way merge of the trees from the commits' common ancestor, which it finds by walking the history. Irmin has no clone or pull for irmin-pack: its store-to-store `Irmin.Sync` with `Irmin.remote_store` exported the history, 197,802 objects, in 62 s, then failed within 0.1 s of importing them into a second irmin-pack store, with `Encountered unkeyed hash but expected key`; Irmin's native sync is for its Git backend only.
- Clone and pull: ferro builds the bundle in memory and receives it into a second file; Dolt pushes to a `file://` remote on the same disk and clones or pulls from it, writing the data twice. Byte counts are the bundle's length for ferro and the remote directory's growth for Dolt.
- Sizes are apparent file lengths after a clean close, so ferro's preallocation is not counted; immudb's include its index files, flushed at close, and Irmin's its index directory, which holds every object's hash. No engine was compacted, and Dolt's `dolt gc` would shrink its directory; irmin-pack's GC refuses a store that has used the `always` indexing strategy.
- 100k keys fit in every engine's caches and the page cache, every phase is single-threaded, and reads are warm, the second of two passes.
