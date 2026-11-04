# Performance

Measured in release on one NVMe SSD with ext4, on 1M `u64` keys with short string values, using
`ferro-bench`. Random `get`, full scan and range read are medians of seven runs, and bulk insert of
five, on a machine shared with other jobs. The fsync commit is a median of three runs and the
coalesced writers of eight on a machine shared with other jobs, the commits on 16 branches of four
from `ferro-bench`'s `group` tool, and the booking commits come from `ferro-bench`'s `tables` tool
the same way. The rest are single runs on an otherwise idle machine.

| Operation | Result |
| --- | --- |
| Bulk insert in one commit | 0.69 s |
| Random `get` | 0.34 µs |
| Full scan | 22 ns per key |
| Range read | 33 ns per key |
| Single-key commit, fsync | 410 µs, about 2,400 per second |
| Single-key commits, fsync, 8 writers with 8 readers | about 9,600 per second |
| Reads per second with those writers running | 38M |
| The same 8 writers with `coalesce` | about 14,600 per second, 12,800 before in the same runs |
| Single-key commits, `Durability::Buffered`, one writer on each of 16 branches | about 125,000 per second |
| Single-key commit, `Durability::Buffered` | 27 µs |
| Bytes written per single-key commit | 3.5 KB |
| `diff` | 3.5 µs per changed key |
| Booking hold commit, `Durability::Buffered` | 82 µs of CPU, 13 µs of it in the closure |
| Booking confirm commit, `Durability::Buffered` | 93 µs of CPU, 12 µs of it in the closure |

## Against other embedded stores

Against other embedded stores on the same workload, with 1 GiB of cache each, medians of five runs
of 3 rounds on a machine shared with other jobs:

| Engine | Get | Scan | Durable commit | 8 writers, commits/s | File size |
| --- | ---: | ---: | ---: | ---: | ---: |
| ferro | 0.32 µs | 20 ns | 0.41 ms | 9,094 | 23 MiB |
| redb | 0.27 µs | 49 ns | 0.43 ms | 2,263 | 65 MiB |
| SQLite, `synchronous=FULL` | 0.87 µs | 50 ns | 0.48 ms | 1,085 | 57 MiB |
| fjall | 1.21 µs | 79 ns | 0.51 ms | 1,957 | 64 MiB |

ferro keeps every version and re-hashes objects as it reads them, and the others do neither. SQLite
bulk loads about 1.2 times faster, 0.59 s against 0.70 s, while fjall takes 1.4 times as long as
ferro and redb 1.9 times, and redb's `get` is about 15% faster. These numbers come from a warm
cache, on a workload with few overwrites. Full results, including caveats about sled, are in
[`benchmarks/compare/RESULTS.md`](../../benchmarks/compare/RESULTS.md).

## Many threads and the allocator

When many threads read through a cache smaller than the data, the allocator decides. Each read then
decodes nodes and evicts others that other threads allocated, and glibc keeps only seven freed
blocks of each size per thread, returning the rest to the arena that allocated them, most under its
lock. With a 16 MiB cache over 1M keys, 32 threads did 3.0M gets a second with the default glibc,
4.7M with `GLIBC_TUNABLES=glibc.malloc.tcache_count=1024` in the environment, 4.6M with jemalloc and
4.1M with mimalloc, and index lookups went from 108,000 a second to between 181,000 and 195,000. One
thread gains 9 to 13%. ferro does not choose an allocator, so an application with that workload
should set one, or the tunable, itself.

## More measurements

- [Example apps](examples.md) has load-test figures for `chat` and `booking`.
- Each internals file records what its subsystem costs and what was measured and left out.
  [Alternatives measured and rejected](../internals/alternatives.md) lists them.
- `make bench`, `make bench-reads`, `make bench-scaling`, `make bench-tables` and the other bench
  targets run the tools these figures come from, as described in [Development](development.md).
