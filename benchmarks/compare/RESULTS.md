# ferro vs embedded stores

Run: `cd benchmarks/compare && cargo run --release` (6 engines x 3 rounds, rotated order, each in a fresh directory and fresh process, medians reported).

Hardware: AMD RYZEN AI MAX+ 395 (32 threads), 124 GB RAM, NVMe ext4, Linux 7.2.14. rustc 1.92.0. Data dir is `benchmarks/compare/target/runs` on real disk.
Versions: ferro 0.1.0 (path), redb 2.6.4, rusqlite 0.32.1 (bundled SQLite), sled 0.34.7, fjall 2.11.2.

Medians of 3 rounds on an otherwise idle machine. Treat differences under about 10% as noise.

Workload: 1M u64 keys from a splitmix64 mixer, values `value-{i}`, same for every engine. Gets: 500k random existing keys. Scan: full ordered iteration. Range: 1000 reads of 1000 keys from random start keys. Commit: 1000 single-key durable commits. Concurrent: 8 writer threads x 200 durable commits each, with 8 reader threads reading random keys meanwhile, on the loaded database. Size: directory size right after bulk load. Reopen: close, open, one get.

Settings: all engines get a 1 GiB cache (ferro `cache_bytes`, redb `set_cache_size`, SQLite `cache_size`, sled `cache_capacity`, fjall `cache_size`); otherwise defaults. Bulk load is one transaction or batch (ferro `update`, redb write txn, SQLite transaction with prepared statement, sled `apply_batch`, fjall `batch`) followed by a durable flush. SQLite uses an INTEGER PRIMARY KEY rowid table (key biased to keep u64 order) and prepared statements. sled and fjall use big-endian byte keys. Concurrent SQLite uses one connection per thread.

| engine | durability | bulk load (s) | get (us) | scan (ns/key) | range 1000 keys (us) | durable commit (ms) | 8W+8R commits/s | 8W+8R reads/s (M) | size (MiB) | reopen (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ferro | fdatasync per commit, group commit | 1.39 | 0.53 | 35 | 34 | 0.40 | 9502 | 18.76 | 23 | 0.1 |
| redb | Durability::Immediate (fsync per commit) | 1.31 | 0.27 | 49 | 48 | 0.42 | 2155 | 19.65 | 65 | 0.6 |
| sqlite-full | WAL, synchronous=FULL | 0.60 | 0.90 | 51 | 49 | 0.47 | 1027 | 0.78 | 57 | 0.2 |
| sqlite-normal | WAL, synchronous=NORMAL (commit not fsynced) | 0.61 | 0.89 | 50 | 50 | 0.01 | 10170 | 0.52 | 57 | 0.2 |
| sled | insert then flush() (fsync) | 3.39 | 5.83 | 218 | 212 | 0.03 | 106118 | 4.81 | 159 | 188.4 |
| fjall | PersistMode::SyncAll after each write | 0.95 | 1.19 | 83 | 82 | 0.50 | 1982 | 7.04 | 64 | 3.9 |

Caveats: sled's 0.03 ms commit and 106k commits/s are implausibly fast for a real fsync per commit; its `flush()` durability was not independently verified, so treat the sled commit rows as unreliable. sled and fjall file sizes include uncompacted logs. SQLite NORMAL does not fsync at commit. Ferro keeps full history and verifies the hash of every object it reads; the others do neither, so ferro's get, scan and commit numbers include that work. Reopen for ferro is a superblock read; sled and fjall do recovery work.
