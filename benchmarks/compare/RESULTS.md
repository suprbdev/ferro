# ferro vs embedded stores

Run: `cd benchmarks/compare && cargo run --release` (6 engines x 3 rounds, rotated order, each in a fresh directory and fresh process, medians reported).

Measured 2026-11-04. Hardware: AMD RYZEN AI MAX+ 395 (16 cores, 32 threads, two CCDs of 8 cores with 32 MiB of L3 each, `performance` governor), 124 GB RAM, Crucial P310 2 TB NVMe with ext4 (`noatime`), Linux 7.2.14-arch1-1. rustc 1.92.0 and cargo 1.92.0, release profile. Data dir is `benchmarks/compare/target/runs` on that disk.
Versions: ferro 0.1.0 (path), redb 2.6.4, rusqlite 0.32.1 (bundled SQLite, libsqlite3-sys 0.30.1), sled 0.34.7, fjall 2.11.2.

Medians of seven runs of the harness, each itself the median of 3 rounds, on an otherwise idle machine: the 1-minute load average was 0.8 before the first run and no other process used a meaningful share of a core during them. The harness was not pinned. Across the seven runs ferro's figures stayed within 6% of their medians, its durable commit ranging from 0.39 to 0.43 ms and sqlite-full's from 0.46 to 0.55 ms, so treat differences under about 10% as noise.

Workload: 1M u64 keys from a splitmix64 mixer, values `value-{i}`, same for every engine. Gets: 500k random existing keys. Scan: full ordered iteration. Range: 1000 reads of 1000 keys from random start keys. Commit: 1000 single-key durable commits. Concurrent: 8 writer threads x 200 durable commits each, with 8 reader threads reading random keys meanwhile, on the loaded database. Size: directory size right after bulk load. Reopen: close, open, one get.

Settings: all engines get a 1 GiB cache (ferro `cache_bytes`, redb `set_cache_size`, SQLite `cache_size`, sled `cache_capacity`, fjall `cache_size`); otherwise defaults. Bulk load is one transaction or batch (ferro `update`, redb write txn, SQLite transaction with prepared statement, sled `apply_batch`, fjall `batch`) followed by a durable flush. SQLite uses an INTEGER PRIMARY KEY rowid table (key biased to keep u64 order) and prepared statements. sled and fjall use big-endian byte keys. Concurrent SQLite uses one connection per thread.

| engine | durability | bulk load (s) | get (us) | scan (ns/key) | range 1000 keys (us) | durable commit (ms) | 8W+8R commits/s | 8W+8R reads/s (M) | size (MiB) | reopen (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ferro | fdatasync per commit, group commit | 0.71 | 0.32 | 19 | 27 | 0.40 | 9113 | 22.34 | 23 | 0.1 |
| redb | Durability::Immediate (fsync per commit) | 1.31 | 0.27 | 49 | 48 | 0.43 | 2241 | 20.52 | 65 | 0.6 |
| sqlite-full | WAL, synchronous=FULL | 0.60 | 0.89 | 49 | 48 | 0.50 | 1083 | 0.78 | 57 | 0.2 |
| sqlite-normal | WAL, synchronous=NORMAL (commit not fsynced) | 0.61 | 0.90 | 50 | 49 | 0.02 | 11012 | 0.53 | 57 | 0.2 |
| sled | insert then flush() (fsync) | 3.46 | 5.81 | 223 | 218 | 0.04 | 123620 | 4.50 | 159 | 190.6 |
| fjall | PersistMode::SyncAll after each write | 0.94 | 1.21 | 81 | 81 | 0.52 | 1942 | 6.19 | 64 | 4.6 |

Against the previous table, five runs on a machine shared with other build and benchmark jobs at a load average of 3 to 11, ferro moved by no more than that spread: bulk load 0.70 to 0.71 s, get unchanged at 0.32 us, scan 20 to 19 ns, range 28 to 27 us, durable commit 0.41 to 0.40 ms, concurrent commits 9,094 to 9,113 and reads 20.2M to 22.3M a second. The other engines moved by as little, apart from sqlite-normal's concurrent commits, 9,661 to 11,012 a second with runs from 8,810 to 13,121, and sled's. ferro bulk loads about 1.2 times slower than SQLite, 1.3 times faster than fjall and 1.8 times faster than redb, and redb's get takes about 16% less time.

The get here is the first pass of reads right after the bulk load, in the same process. The read path changes for issue #9 took ferro's get from 0.73 to 0.33 us against 0.30 us for redb, its scan from 42 to 22 ns a key and its 1000-key range from 43 to 30 us, in 7 alternating runs of the ferro child built before and after those changes and the redb child. Bulk load, commit, size and reopen did not change beyond noise.

The bulk-load changes for issue #10 took ferro's bulk load from 1.49 to 1.25 s in runs at a load
average of 20 to 50, against 0.79 s for sqlite-full, and in 5 alternating runs of `ferro-bench`
bulk insert from 1.44 to 1.07 us a key. Those for issue #32, prefetching on the insert descent,
freeing the dropped tree in address order and `lz4_flex`'s unsafe encoder, took it from 1.04 to
0.71 s in 7 alternating runs of the harness's ferro child built before and after them and its
sqlite-full child, which took 0.60 s, at a load average of 2 to 7, and in 5 alternating runs of
`ferro-bench` bulk insert from 1.01 to 0.69 us a key. In those runs ferro's get, scan, range,
durable commit, concurrent commits and reads, size and reopen did not change beyond noise.

Caveats: sled's 0.04 ms commit and 124k commits/s are implausibly fast for a real fsync per commit; its `flush()` durability was not independently verified, so treat the sled commit rows as unreliable. sled and fjall file sizes include uncompacted logs. SQLite NORMAL does not fsync at commit. Ferro keeps full history and verifies the hash of every object it reads; the others do neither, so ferro's get, scan and commit numbers include that work. Reopen for ferro is a superblock read; sled and fjall do recovery work, and fjall's reopen ranged from 3.3 to 15.9 ms between runs.
