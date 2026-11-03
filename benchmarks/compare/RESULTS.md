# ferro vs embedded stores

Run: `cd benchmarks/compare && cargo run --release` (6 engines x 3 rounds, rotated order, each in a fresh directory and fresh process, medians reported).

Hardware: AMD RYZEN AI MAX+ 395 (32 threads), 124 GB RAM, NVMe ext4, Linux 7.2.14. rustc 1.92.0. Data dir is `benchmarks/compare/target/runs` on real disk.
Versions: ferro 0.1.0 (path), redb 2.6.4, rusqlite 0.32.1 (bundled SQLite), sled 0.34.7, fjall 2.11.2.

Medians of five runs of the harness, each itself the median of 3 rounds, on a machine shared with other build and benchmark jobs (load average 6 to 25 during the runs). Every engine is slower than on an idle machine, so compare engines within the table, and treat differences under about 15% as noise.

Workload: 1M u64 keys from a splitmix64 mixer, values `value-{i}`, same for every engine. Gets: 500k random existing keys. Scan: full ordered iteration. Range: 1000 reads of 1000 keys from random start keys. Commit: 1000 single-key durable commits. Concurrent: 8 writer threads x 200 durable commits each, with 8 reader threads reading random keys meanwhile, on the loaded database. Size: directory size right after bulk load. Reopen: close, open, one get.

Settings: all engines get a 1 GiB cache (ferro `cache_bytes`, redb `set_cache_size`, SQLite `cache_size`, sled `cache_capacity`, fjall `cache_size`); otherwise defaults. Bulk load is one transaction or batch (ferro `update`, redb write txn, SQLite transaction with prepared statement, sled `apply_batch`, fjall `batch`) followed by a durable flush. SQLite uses an INTEGER PRIMARY KEY rowid table (key biased to keep u64 order) and prepared statements. sled and fjall use big-endian byte keys. Concurrent SQLite uses one connection per thread.

| engine | durability | bulk load (s) | get (us) | scan (ns/key) | range 1000 keys (us) | durable commit (ms) | 8W+8R commits/s | 8W+8R reads/s (M) | size (MiB) | reopen (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ferro | fdatasync per commit, group commit | 1.25 | 0.35 | 23 | 31 | 0.43 | 8592 | 19.05 | 23 | 0.1 |
| redb | Durability::Immediate (fsync per commit) | 1.53 | 0.31 | 51 | 52 | 0.45 | 2132 | 15.91 | 65 | 0.6 |
| sqlite-full | WAL, synchronous=FULL | 0.85 | 1.23 | 58 | 60 | 0.58 | 979 | 0.69 | 57 | 0.3 |
| sqlite-normal | WAL, synchronous=NORMAL (commit not fsynced) | 0.80 | 1.32 | 61 | 62 | 0.02 | 9294 | 0.46 | 57 | 0.2 |
| sled | insert then flush() (fsync) | 4.03 | 6.85 | 343 | 256 | 0.07 | 44912 | 3.99 | 159 | 233.3 |
| fjall | PersistMode::SyncAll after each write | 1.09 | 1.42 | 97 | 98 | 0.54 | 1930 | 5.50 | 64 | 15.3 |

The get here is the first pass of reads right after the bulk load, in the same process. The read path changes for issue #9 took ferro's get from 0.73 to 0.33 us against 0.30 us for redb, its scan from 42 to 22 ns a key and its 1000-key range from 43 to 30 us, in 7 alternating runs of the ferro child built before and after those changes and the redb child. Bulk load, commit, size and reopen did not change beyond noise.

ferro's bulk load is from five later runs of the harness, after the bulk-load changes for issue #10,
at a load average of 20 to 50; before them it was the 1.49 s measured with the rest of the table.
In those runs ferro took 1.25 s against 0.79 s for sqlite-full, 1.16 s for fjall and 1.55 s for
redb, and its get, scan and range were 0.36 us, 23 ns and 32 us, within noise of its row. In 5
alternating runs of `ferro-bench` against the build before those changes, bulk insert went from
1.44 to 1.07 us a key.

Caveats: sled's 0.07 ms commit and 45k commits/s are implausibly fast for a real fsync per commit; its `flush()` durability was not independently verified, so treat the sled commit rows as unreliable. sled and fjall file sizes include uncompacted logs. SQLite NORMAL does not fsync at commit. Ferro keeps full history and verifies the hash of every object it reads; the others do neither, so ferro's get, scan and commit numbers include that work. Reopen for ferro is a superblock read; sled and fjall do recovery work.
