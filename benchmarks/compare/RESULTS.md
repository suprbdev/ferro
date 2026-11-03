# ferro vs embedded stores

Run: `cd benchmarks/compare && cargo run --release` (6 engines x 3 rounds, rotated order, each in a fresh directory and fresh process, medians reported).

Hardware: AMD RYZEN AI MAX+ 395 (32 threads), 124 GB RAM, NVMe ext4, Linux 7.2.14. rustc 1.92.0. Data dir is `benchmarks/compare/target/runs` on real disk.
Versions: ferro 0.1.0 (path), redb 2.6.4, rusqlite 0.32.1 (bundled SQLite), sled 0.34.7, fjall 2.11.2.

Medians of five runs of the harness, each itself the median of 3 rounds, on a machine shared with other build and benchmark jobs (load average 3 to 11 during the runs), after the bulk-load changes for issue #32. Every engine is slower than on an idle machine, so compare engines within the table, and treat differences under about 15% as noise.

Workload: 1M u64 keys from a splitmix64 mixer, values `value-{i}`, same for every engine. Gets: 500k random existing keys. Scan: full ordered iteration. Range: 1000 reads of 1000 keys from random start keys. Commit: 1000 single-key durable commits. Concurrent: 8 writer threads x 200 durable commits each, with 8 reader threads reading random keys meanwhile, on the loaded database. Size: directory size right after bulk load. Reopen: close, open, one get.

Settings: all engines get a 1 GiB cache (ferro `cache_bytes`, redb `set_cache_size`, SQLite `cache_size`, sled `cache_capacity`, fjall `cache_size`); otherwise defaults. Bulk load is one transaction or batch (ferro `update`, redb write txn, SQLite transaction with prepared statement, sled `apply_batch`, fjall `batch`) followed by a durable flush. SQLite uses an INTEGER PRIMARY KEY rowid table (key biased to keep u64 order) and prepared statements. sled and fjall use big-endian byte keys. Concurrent SQLite uses one connection per thread.

| engine | durability | bulk load (s) | get (us) | scan (ns/key) | range 1000 keys (us) | durable commit (ms) | 8W+8R commits/s | 8W+8R reads/s (M) | size (MiB) | reopen (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ferro | fdatasync per commit, group commit | 0.70 | 0.32 | 20 | 28 | 0.41 | 9094 | 20.23 | 23 | 0.1 |
| redb | Durability::Immediate (fsync per commit) | 1.32 | 0.27 | 49 | 50 | 0.43 | 2263 | 20.48 | 65 | 0.6 |
| sqlite-full | WAL, synchronous=FULL | 0.59 | 0.87 | 50 | 49 | 0.48 | 1085 | 0.78 | 57 | 0.2 |
| sqlite-normal | WAL, synchronous=NORMAL (commit not fsynced) | 0.59 | 0.88 | 49 | 50 | 0.02 | 9661 | 0.54 | 57 | 0.2 |
| sled | insert then flush() (fsync) | 3.44 | 5.84 | 222 | 216 | 0.04 | 99157 | 4.57 | 159 | 199.4 |
| fjall | PersistMode::SyncAll after each write | 0.95 | 1.21 | 79 | 80 | 0.51 | 1957 | 6.18 | 64 | 3.9 |

The get here is the first pass of reads right after the bulk load, in the same process. The read path changes for issue #9 took ferro's get from 0.73 to 0.33 us against 0.30 us for redb, its scan from 42 to 22 ns a key and its 1000-key range from 43 to 30 us, in 7 alternating runs of the ferro child built before and after those changes and the redb child. Bulk load, commit, size and reopen did not change beyond noise.

The bulk-load changes for issue #10 took ferro's bulk load from 1.49 to 1.25 s in runs at a load
average of 20 to 50, against 0.79 s for sqlite-full, and in 5 alternating runs of `ferro-bench`
bulk insert from 1.44 to 1.07 us a key. Those for issue #32, prefetching on the insert descent,
freeing the dropped tree in address order and `lz4_flex`'s unsafe encoder, took it from 1.04 to
0.71 s in 7 alternating runs of the harness's ferro child built before and after them and its
sqlite-full child, which took 0.60 s, at a load average of 2 to 7, and in 5 alternating runs of
`ferro-bench` bulk insert from 1.01 to 0.69 us a key. In those runs ferro's get, scan, range,
durable commit, concurrent commits and reads, size and reopen did not change beyond noise. ferro
still bulk loads about 1.2 times slower than SQLite, and now faster than fjall and redb.

Caveats: sled's 0.04 ms commit and 99k commits/s are implausibly fast for a real fsync per commit; its `flush()` durability was not independently verified, so treat the sled commit rows as unreliable. sled and fjall file sizes include uncompacted logs. SQLite NORMAL does not fsync at commit. Ferro keeps full history and verifies the hash of every object it reads; the others do neither, so ferro's get, scan and commit numbers include that work. Reopen for ferro is a superblock read; sled and fjall do recovery work.
