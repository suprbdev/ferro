# ferro vs embedded stores

For history reads, diffs, branches, sync and verification against immudb and Dolt, see [versioned/RESULTS.md](versioned/RESULTS.md).

Run: `cd benchmarks/compare && cargo run --release` (7 engines x 3 rounds, rotated order, each in a fresh directory and fresh process, medians reported). The `ferro-extend` engine loads ferro with one `Map::extend` instead of a loop of `insert`.

Measured 2026-11-05. Hardware: AMD RYZEN AI MAX+ 395 (16 cores, 32 threads, two CCDs of 8 cores with 32 MiB of L3 each, `performance` governor), 124 GB RAM, Crucial P310 2 TB NVMe with ext4 (`noatime`), Linux 7.2.14-arch1-1. rustc 1.92.0 and cargo 1.92.0, release profile. Data dir is `benchmarks/compare/target/runs` on that disk.
Versions: ferro 0.1.0 (path), redb 2.6.4, rusqlite 0.32.1 (bundled SQLite, libsqlite3-sys 0.30.1), sled 0.34.7, fjall 2.11.2.

Medians of seven runs of the harness, each itself the median of 3 rounds, on an otherwise idle machine: the 1-minute load average was 2.0 before the first run, from the benchmark before it, and other processes kept about a tenth of a core busy during them. The harness was not pinned. Across the seven runs ferro's figures stayed within 6% of their medians, its durable commit ranging from 0.40 to 0.44 ms and sqlite-full's from 0.48 to 0.56 ms, so treat differences under about 10% as noise.

Workload: 1M u64 keys from a splitmix64 mixer, values `value-{i}`, same for every engine. Gets: 500k random existing keys. Scan: full ordered iteration. Range: 1000 reads of 1000 keys from random start keys. Commit: 1000 single-key durable commits. Concurrent: 8 writer threads x 200 durable commits each, with 8 reader threads reading random keys meanwhile, on the loaded database. Size: directory size right after bulk load. Reopen: close, open, one get.

Settings: all engines get a 1 GiB cache (ferro `cache_bytes`, redb `set_cache_size`, SQLite `cache_size`, sled `cache_capacity`, fjall `cache_size`); otherwise defaults. Bulk load is one transaction or batch (ferro `update`, redb write txn, SQLite transaction with prepared statement, sled `apply_batch`, fjall `batch`) followed by a durable flush. SQLite uses an INTEGER PRIMARY KEY rowid table (key biased to keep u64 order) and prepared statements. sled and fjall use big-endian byte keys. Concurrent SQLite uses one connection per thread.

| engine | durability | bulk load (s) | get (us) | scan (ns/key) | range 1000 keys (us) | durable commit (ms) | 8W+8R commits/s | 8W+8R reads/s (M) | size (MiB) | reopen (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ferro | fdatasync per commit, group commit | 0.67 | 0.32 | 18 | 29 | 0.42 | 8955 | 23.07 | 23 | 0.1 |
| ferro-extend | as ferro, bulk load through `extend` | 0.38 | 0.32 | 18 | 29 | 0.42 | 9123 | 22.90 | 23 | 0.1 |
| redb | Durability::Immediate (fsync per commit) | 1.32 | 0.27 | 48 | 49 | 0.43 | 2207 | 20.57 | 65 | 0.6 |
| sqlite-full | WAL, synchronous=FULL | 0.62 | 0.87 | 49 | 49 | 0.51 | 1080 | 0.78 | 57 | 0.2 |
| sqlite-normal | WAL, synchronous=NORMAL (commit not fsynced) | 0.61 | 0.87 | 49 | 49 | 0.02 | 12819 | 0.52 | 57 | 0.2 |
| sled | insert then flush() (fsync) | 3.47 | 5.81 | 220 | 214 | 0.04 | 113301 | 4.45 | 159 | 189.4 |
| fjall | PersistMode::SyncAll after each write | 0.96 | 1.20 | 79 | 81 | 0.50 | 1997 | 6.28 | 64 | 4.0 |

Against the previous table, seven runs on the same idle machine on 2026-11-04, ferro's bulk load went from 0.71 to 0.67 s, with runs from 0.66 to 0.68 s against 0.71 to 0.72 s, and its scan from 19 to 18 ns, after the changes described under 2026-11-05 below, and its 1000-key range went the other way, from 27 to 29 us, since db07c34 borrows nodes under hazards in iterators (#79), as `docs/guide/performance.md` describes under Slower than before. A later change moved each node's flags to the front of its allocation, beside its headers, and in 15 alternating runs of the ferro child alone the range took 27.4 us against 28.2 before it and 27.3 for db07c34's parent; the table above was not run again. Its get stayed at 0.32 us, and its durable commit, 0.40 to 0.42 ms, and concurrent commits and reads, 9,113 to 8,955 and 22.3M to 23.1M a second, moved by less than the spread of their runs. The other engines moved by as little, apart from sqlite-normal's concurrent commits, 11,012 to 12,819 a second with runs from 10,517 to 14,952, and sled's. ferro bulk loads about 1.08 times slower than SQLite, 1.4 times faster than fjall and 2.0 times faster than redb, and through `extend` 1.6 times faster than SQLite, and redb's get takes about 16% less time.

The get here is the first pass of reads right after the bulk load, in the same process. The read path changes for issue #9 took ferro's get from 0.73 to 0.33 us against 0.30 us for redb, its scan from 42 to 22 ns a key and its 1000-key range from 43 to 30 us, in 7 alternating runs of the ferro child built before and after those changes and the redb child. Bulk load, commit, size and reopen did not change beyond noise.

The bulk-load changes for issue #10 took ferro's bulk load from 1.49 to 1.25 s in runs at a load
average of 20 to 50, against 0.79 s for sqlite-full, and in 5 alternating runs of `ferro-bench`
bulk insert from 1.44 to 1.07 us a key. Those for issue #32, prefetching on the insert descent,
freeing the dropped tree in address order and `lz4_flex`'s unsafe encoder, took it from 1.04 to
0.71 s in 7 alternating runs of the harness's ferro child built before and after them and its
sqlite-full child, which took 0.60 s, at a load average of 2 to 7, and in 5 alternating runs of
`ferro-bench` bulk insert from 1.01 to 0.69 us a key. In those runs ferro's get, scan, range,
durable commit, concurrent commits and reads, size and reopen did not change beyond noise.

## 2026-11-05: slots, inserts and the walk

Smaller slots, a lean recursion for inserts, a teardown that prefetches each node's flag line and a walk that follows borrowed nodes in one loop, described in `docs/internals/tree.md` and `docs/internals/read-path.md`. Each row is the harness's child for one engine run on its own, `compare --child <engine> <dir> 1000000`, pinned with `taskset` to the second CCD, cores 8 to 15 and their second threads, while another agent's benchmarks ran on the first: the ferro rows are medians of seven runs that alternated the builds before and after, and the others of three runs of the build after. Each run started only once the second CCD had been idle for three seconds.

| engine | durability | bulk load (s) | get (us) | scan (ns/key) | range 1000 keys (us) | durable commit (ms) | 8W+8R commits/s | 8W+8R reads/s (M) | size (MiB) | reopen (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ferro, before (2026-11-05) | fdatasync per commit, group commit | 0.71 | 0.33 | 19 | 29 | 0.44 | 8504 | 22.58 | 23 | 0.1 |
| ferro, after (2026-11-05) | fdatasync per commit, group commit | 0.67 | 0.32 | 18 | 29 | 0.41 | 8876 | 22.94 | 23 | 0.1 |
| ferro-extend (2026-11-05) | as ferro, bulk load through `extend` | 0.41 | 0.35 | 19 | 30 | 0.45 | 8508 | 21.97 | 23 | 0.1 |
| redb (2026-11-05) | Durability::Immediate (fsync per commit) | 1.36 | 0.27 | 50 | 50 | 0.42 | 2262 | 20.60 | 65 | 0.5 |
| sqlite-full (2026-11-05) | WAL, synchronous=FULL | 0.63 | 0.87 | 49 | 50 | 0.49 | 1150 | 0.77 | 57 | 0.3 |

ferro's bulk load through per-key inserts went from 0.71 to 0.67 s, against 0.63 s for sqlite-full measured the same way, so SQLite now loads 1.06 times faster rather than 1.2. Two earlier sets of seven alternating runs, one with the walk not yet in its final form and one while the workspace's tests ran on the first CCD, gave 0.73 to 0.69 s and 0.74 to 0.70 s. The `bulk` tool split the gain into inserts from 543 to 496 ms, the rest of the commit from 95 to 92 ms and the drop of the working tree from 108 to 96 ms, and peak RSS fell from 379 to 343 MiB. Through `extend` ferro loads in 0.41 s, 1.5 times faster than SQLite. The get moved from 0.33 to 0.32 us, and in the two earlier sets from 0.34 to 0.34 and 0.35 to 0.33: it runs 15% fewer instructions, 1,024 against 1,205, but waits on four dependent misses below the pinned levels, most of them in DRAM, and redb's 0.27 us stays ahead. With `GLIBC_TUNABLES=glibc.malloc.hugetlb=1`, which gives glibc's heap transparent huge pages, ferro's gets ran 8 to 12% faster and its load 12% faster in alternating runs of a smaller harness, as `docs/guide/performance.md` describes; the other engines were not measured that way. The durable commit went from 0.44 to 0.41 ms and the concurrent commits from 8,504 to 8,876 a second, within the spread of the three sets for commits that run within 0.3% of the instructions they did, and every other ferro column moved by 2% or less.

Caveats: sled's 0.04 ms commit and 113k commits/s are implausibly fast for a real fsync per commit; its `flush()` durability was not independently verified, so treat the sled commit rows as unreliable. sled and fjall file sizes include uncompacted logs. SQLite NORMAL does not fsync at commit. Ferro keeps full history and verifies the hash of every object it reads; the others do neither, so ferro's get, scan and commit numbers include that work. Reopen for ferro is a superblock read; sled and fjall do recovery work, and fjall's reopen ranged from 3.3 to 16.0 ms between runs.
