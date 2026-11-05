# Performance

Measured on 2026-11-04 in release, on an AMD Ryzen AI MAX+ 395 with 16 cores in two CCDs of 8, 32
threads and 124 GB of RAM, on one NVMe SSD with ext4, under Linux 7.2.14 with rustc 1.92.0. Nothing
else was running: the 1-minute load average was under 2 before each set of runs, and other processes
kept well under one core busy during them. The workload is 1M `u64` keys with short string values,
using `ferro-bench`. Its figures are medians of seven runs, and the coalesced writers of nine runs
alternating with plain ones. The commits on 16 branches are medians of five runs of `ferro-bench`'s
`group` tool, and the booking commits of seven runs of its `tables` tool, pinned to one CCD.

| Operation | Result |
| --- | --- |
| Bulk insert in one commit | 0.71 s |
| Random `get` | 0.32 µs |
| Full scan | 19 ns per key |
| Range read | 28 ns per key |
| Single-key commit, fsync | 410 µs, about 2,400 per second |
| Single-key commits, fsync, 8 writers with 8 readers | about 10,100 per second |
| Reads per second with those writers running | 71M |
| The same 8 writers with `coalesce` | about 14,900 per second, 9,600 without in the same runs |
| Single-key commits, `Durability::Buffered`, one writer on each of 16 branches | about 124,000 per second |
| Single-key commit, `Durability::Buffered` | 24 µs |
| Bytes written per single-key commit | 3.5 KB |
| `diff` | 3.8 µs per changed key |
| Booking hold commit, `Durability::Buffered` | 86 µs of CPU, 14 µs of it in the closure |
| Booking confirm commit, `Durability::Buffered` | 106 µs of CPU, 14 µs of it in the closure |

The booking commits cost 4 and 14% more than the 82 and 93 µs this page gave before, measured on
2026-11-03. Since then the example's customer index covers six fields of each booking, which
[Covering indexes](../internals/schema-and-unique-indexes.md#covering-indexes) found adds 5% to the
instructions of a hold and 10% to those of a confirm, whose status it covers. Outside their closures
the two ran 72 and 92 µs.

Measured again on 2026-11-05 on the second CCD while other work ran on the first, five runs of
`ferro-bench` alternating the builds before and after smaller slots, a lean insert recursion, a
faster drop of the working tree and a walk that reaches borrowed nodes through one inlined step: the
bulk insert went from 0.70 to 0.68 s and its peak RSS from 379 to 345 MiB, a random get from 0.32 to
0.31 µs for 1,025 instructions against 1,205, and a table's row get from 163 to 161 ns for 829
instructions against 974. The other rows moved by less than the spread of the runs, and the
instructions of commits, scans and ranges by under 0.3%.

## Against other embedded stores

Against other embedded stores on the same workload, with 1 GiB of cache each, medians of seven runs
of 3 rounds:

| Engine | Get | Scan | Durable commit | 8 writers, commits/s | File size |
| --- | ---: | ---: | ---: | ---: | ---: |
| ferro | 0.32 µs | 19 ns | 0.40 ms | 9,113 | 23 MiB |
| redb | 0.27 µs | 49 ns | 0.43 ms | 2,241 | 65 MiB |
| SQLite, `synchronous=FULL` | 0.89 µs | 49 ns | 0.50 ms | 1,083 | 57 MiB |
| fjall | 1.21 µs | 81 ns | 0.52 ms | 1,942 | 64 MiB |

ferro keeps every version and re-hashes objects as it reads them, and the others do neither. SQLite
bulk loads about 1.2 times faster, 0.60 s against 0.71 s, while fjall takes 1.3 times as long as
ferro and redb 1.8 times, and redb's `get` takes about 16% less time. These numbers come from a warm
cache, on a workload with few overwrites. Full results, including caveats about sled, are in
[`benchmarks/compare/RESULTS.md`](../../benchmarks/compare/RESULTS.md).

Measured again on 2026-11-05, each engine run alone on the second CCD while other work ran on the
first, after smaller slots, a lean insert recursion, a faster drop of the working tree and a walk
that reaches borrowed nodes through one inlined step: ferro's bulk load went from 0.71 to 0.67 s in
alternating runs against 0.63 s for SQLite, so SQLite now loads about 1.06 times faster. Loaded with
one `Map::extend` instead of a loop of `insert`, ferro takes 0.41 s. The get went from 0.33 to 0.32
µs against 0.27 µs for redb, as [The read
path](../internals/read-path.md#one-loop-for-borrowed-nodes) explains, and [Huge
pages](#huge-pages) takes about a tenth off both the get and the load.

## Writers and branches

`group` runs single-key commits into a map of 100,000 keys from each writer thread for three
seconds, either one writer on each of its own branches or all of them on one branch. Commits a
second, median of five runs:

| Writers | Buffered, own branches | Buffered, one branch | One branch, `coalesce` | Fsync, own branches | Fsync, one branch | One branch, `coalesce` |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 49,600 | 49,800 | 49,300 | 2,490 | 2,510 | 2,620 |
| 4 | 109,100 | 42,800 | 50,200 | 5,400 | 5,300 | 8,600 |
| 8 | 150,000 | 42,000 | 48,400 | 10,200 | 10,100 | 14,400 |
| 16 | 123,800 | 39,500 | 53,400 | 18,100 | 18,500 | 23,500 |

With `coalesce` 16 writers on one branch shared a version 13 at a time Buffered and 16 at a time
with fsync, and wrote 1,515 and 1,427 bytes a closure against 2,464 and 2,385 without it. A writer
alone on its branch has nothing to share, and `coalesce` there ran within 5% of the plain figures,
apart from 8 Buffered branches at 139,600 a second, 7% fewer.

## Reads from many threads

`scaling` reads a 1M-key `Map<u64, String>` and a 200,000-row table from 1 to 32 threads pinned a
core at a time, so 8 fill one CCD, 16 both, and 32 add the second thread of every core. A lookup
reads about 20 rows by a range of an index, and a page 200 rows in key order, through a plain index
and through a twin whose index covers the field read. Operations a second, medians of five rounds
of a pass with no writer, measured again on 2026-11-05 once iterators, cursors and the row finder
borrowed nodes, roots and stores under hazards instead of counting them, as [Iterators under
hazards](../internals/read-scaling.md#iterators-under-hazards) describes, with other work under one
core busy in every cell:

| Workload | Cache | 1 thread | 8 threads | 16 threads | 32 threads |
| --- | --- | ---: | ---: | ---: | ---: |
| `get` | 1 GiB | 3.0M | 23.1M | 42.8M | 65.9M |
| `get` | 16 MiB | 337k | 1.8M | 2.0M | 2.9M |
| `for_each` scan, keys | 1 GiB | 88M | 487M | 964M | 1,487M |
| `for_each` scan, keys | 16 MiB | 7.9M | 39M | 42M | 73M |
| 100-key range | 1 GiB | 422k | 3.1M | 5.8M | 8.6M |
| 100-key range | 16 MiB | 53k | 262k | 270k | 377k |
| Index lookup | 1 GiB | 232k | 1.7M | 2.9M | 4.4M |
| Index lookup, covered | 1 GiB | 2.5M | 15.3M | 27.6M | 37.7M |
| Index lookup | 16 MiB | 14k | 67k | 76k | 108k |
| Index lookup, covered | 16 MiB | 2.2M | 14.0M | 26.0M | 35.9M |
| Page of 200 rows | 1 GiB | 183k | 914k | 1.1M | 1.5M |
| Page of 200 rows, covered | 1 GiB | 855k | 4.9M | 8.4M | 9.3M |

Against the figures this page gave on 2026-11-04, covered lookups at 32 threads read 37.7M a second
against 15.4M, plain lookups 4.4M against 2.8M, covered pages 9.3M against 3.4M and pages of rows
1.5M against 987,000. Covered pages at 16 and 32 threads move between about 5M and 10M a second from
one round to the next. In a pass of its own beside one thread committing single-key updates as fast
as it can, gets at 32 threads ran 61M a second with the 1 GiB cache and 2.6M with the 16 MiB one,
and index lookups 2.9M and 89,000. Run in one pass with the writer's cells, as the tool does by
default, the cells without a writer read up to 12% slower on one thread. The fixes that followed a
review of the hazards, described in the same section, moved these cells by 2% or less in
alternating runs, apart from covered pages, 2 to 6% slower on one thread and within the swing they
show between rounds on many.

## Many threads and the allocator

When many threads read through a cache smaller than the data, the allocator decides. Each read then
decodes nodes and evicts others that other threads allocated, and glibc keeps only seven freed
blocks of each size per thread, returning the rest to the arena that allocated them, most under its
lock. With a 16 MiB cache over 1M keys, medians of three alternating runs of `scaling`, 32 threads
did 2.9M gets a second with the default glibc, 4.6M with
`GLIBC_TUNABLES=glibc.malloc.tcache_count=1024` in the environment, 4.5M with jemalloc preloaded and
4.0M with mimalloc, and index lookups went from 108,000 a second to between 176,000 and 189,000. One
thread gains 2 to 7%. ferro does not choose an allocator, so an application with that workload
should set one, or the tunable, itself.

## Huge pages

A large map in memory is read and built one dependent miss at a time, and each miss also walks the
page table, since 1M keys take several hundred MiB of 4 KiB pages, far more than the TLB covers.
This machine has transparent huge pages set to `always`, yet glibc's heap gets none: it grows a
little at a time, so no 2 MiB range of it is ever whole when first touched. With
`GLIBC_TUNABLES=glibc.malloc.hugetlb=1` in the environment glibc asks for huge pages and grows its
heap in whole ones. Over four alternating runs of a 1M-key load of short strings through per-key
inserts and then gets, measured on 2026-11-05 with the build before the changes above:

| | Default | `glibc.malloc.hugetlb=1` |
| --- | ---: | ---: |
| Inserts | 534 ns a key, 48,888 page faults | 459 ns a key, 501 page faults |
| Load, commit and drop | 730 ms | 641 ms |
| `get` | 332 ns | 293 to 303 ns |
| `contains_key` | 313 ns | 276 ns |
| Resident memory in huge pages | 0 of 347 MiB | 362 of 368 MiB |

As with the allocator, ferro leaves this to the application.

## Other tools

The other `ferro-bench` tools, on the same machine and day, with how many runs each figure is the
median of:

| Tool, runs | Workload | Result |
| --- | --- | --- |
| `bulk`, 5 | 1M random keys inserted in one commit, measured again on 2026-11-05 on one CCD, alternating with the build before | 0.68 s: 0.50 s of inserts and 0.19 s to commit and drop the tree, 343 MiB peak RSS, against 0.75 s, 0.54 s, 0.20 s and 379 MiB before |
| `tables`, 7 | Booking hold, confirm and cancel, Buffered | 86, 106 and 146 µs of CPU, 14, 14 and 26 µs of it in the closure, 13,362 bytes a write |
| `covered`, 3 of 3 rounds | A balance update on 200,000 rows, a plain `tier` index against one covering `balance` | 7,660 against 10,415 bytes and 52 against 67 µs of CPU, indexes of 3.90 against 4.56 MiB |
| `merge`, 3 | Merge base of a fork at 10,000 commits followed by 10,000 and 1,000 more, of a branch that took a side branch 1,000 times, and of one with 1,000 features merged since it forked | 62 µs, 51 µs and 7.7 ms |
| `merge`, 3 | `Map<u64, u64>::merge` of 1M keys, 100,000 changed on each side | 182 ms, 302 ms with struct values and 452 ms for a `Table` with one index |
| `clone`, 5 | First `pull` of a 330 MB file of 124,937 objects, fsync | 0.91 s, 17 MiB peak RSS, 90 bytes of heap an object |
| `clone`, 5 | `bundle` of that file into another, then `receive` of it | 0.62 s at 13 MiB peak RSS, then 0.78 s at 13 MiB |
| `clone`, 5 | `pull` of three one-key commits | 1.33 ms, and 0.57 ms Buffered |
| `heap`, 3 | Live heap after scans with a 64 MiB cache | 45 MiB for short strings, 58 MiB for 300 byte strings, 65 MiB for 2 KiB values, 69 MiB for table rows |
| `watchers`, 5 | A Buffered single-key commit with 0, 1,000 and 10,000 parked watchers | 12.3, 12.0 and 12.1 µs, and 23, 57 and 375 µs at p50 to a watcher whose test passes |
| `reclaim`, 3 | 15 s of fsync commits keeping 100 versions, a cycle every 64 MiB | Longest pause 2.9 ms on `metrics`, 3.8 ms on `ledger`, 4.4 ms on `wiki` and 2.8 ms on table updates, commits a second within 1% of no cycles |
| `reclaim`, 3 | Updates of 20,000 rows over 64 and 256 branches, a cycle every 4 MiB | Median pause 3.1 and 4.5 ms |
| `reclaim copy`, 5 and 15 | The copy a reclamation cycle makes, keeping everything, of 189 MB of table rows and of 331 MB of 320 byte values, on four threads since #63, measured with other work sharing the machine | 0.39 and 0.16 s, 489 and 2,067 MB/s, against 1.05 and 0.56 s before |

## Slower than before

Four figures came out slower than before, beyond the spread of the runs, and builds of older trees
run alternately with this one on the same machine confirmed each: a filtered scan of table rows,
31.6 ns a row against 17.8 (#56), `wiki` commits, 564 a second against 691 (#57), a covered page of
200 rows on one thread, 798,000 a second against 896,000 (#58), and bulk inserts, 3,092
instructions a key against 2,978 (#59).

All four have been fixed since. For #57 and #59, as [Where the flags
live](../internals/read-scaling.md#where-the-flags-live) describes, the bulk inserts ran more
instructions because `Shared::make_mut` was not inlined, and now run 2,941 a key. The store did no
more work for a `wiki` commit than before: the example's tokenizer had compiled to slower code, and
with it rewritten `wiki` commits 1,004 times a second against 548 before the fix and 638 before
`Shared`, medians of three alternating runs on a machine that other work was sharing.

The filtered scan is fixed. A fold called its closure from a loop for each kind of run, so the
compiler stopped inlining a closure over table rows, and the scan now takes 18.7 ns a row against
32.6 in alternating runs, as [Scans](../internals/read-path.md#scans) describes.

The covered page is fixed too. Its commit changed no read code, but its new functions moved the
index lookup's constructors into another codegen unit than the benchmark's read loop, which then
called the index cursor through its vtable for every row. The constructors are now always inlined
into the code that reads them, and in alternating runs a covered page ran 810,000 a second on one
thread against 742,000 and 3.27M at eight threads against 2.98M, as [Covering
indexes](../internals/schema-and-unique-indexes.md#covering-indexes) describes. The table under
[Reads from many threads](#reads-from-many-threads) was measured again after both fixes.

## More measurements

- [Example apps](examples.md) has load-test figures for `chat` and `booking`, measured on 2026-11-03
  and 2026-11-04.
- Each internals file records what its subsystem cost when it changed, and what was measured and
  left out. [Alternatives measured and rejected](../internals/alternatives.md) lists them.
- `make bench`, `make bench-reads`, `make bench-scaling`, `make bench-tables` and the other bench
  targets run the tools these figures come from, as described in [Development](development.md).
