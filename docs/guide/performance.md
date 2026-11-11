# Performance

Measured on 2026-11-05 in release, on an AMD Ryzen AI MAX+ 395 with 16 cores in two CCDs of 8, 32
threads and 124 GB of RAM, on one NVMe SSD with ext4, under Linux 7.2.14 with rustc 1.92.0. Nothing
else was running: the 1-minute load average was under 2 before each set of runs, and other processes
kept well under one core busy during them. The workload is 1M `u64` keys with short string values,
using `ferro-bench`. Its figures are medians of seven runs, and the coalesced writers of nine runs
alternating with plain ones. The commits on 16 branches are medians of six runs of `ferro-bench`'s
`group` tool, and the booking commits of seven runs of its `tables` tool pinned to one CCD. Every
figure on this page but those under [Huge pages](#huge-pages) was measured again that day, and a
figure that came within the spread of its runs of the one measured on 2026-11-04 is given as it was.
The copy a reclamation cycle makes, under [Other tools](#other-tools), was measured again on
2026-11-07, pinned to 16 cores, once it ran on up to 8 threads by default, and the Buffered commits
of writers on their own branches on 2026-11-09, once a moved draft rewrote its links in place, as
described under [Writers and branches](#writers-and-branches). The figures under [Reads past the
cache](#reads-past-the-cache) were measured on 2026-11-09 beside other work, as that section says.

| Operation | Result |
| --- | --- |
| Bulk insert in one commit | 0.66 s |
| Random `get` | 0.31 µs |
| Full scan | 18 ns per key |
| Range read | 29 ns per key |
| Single-key commit, fsync | 410 µs, about 2,400 per second |
| Single-key commits, fsync, 8 writers with 8 readers | about 10,100 per second |
| Reads per second with those writers running | 74M |
| The same 8 writers with `coalesce` | about 14,900 per second, 9,600 without in the same runs |
| Single-key commits, `Durability::Buffered`, one writer on each of 16 branches | about 135,000 per second |
| Single-key commit, `Durability::Buffered` | 24 µs |
| Bytes written per single-key commit | 3.5 KB |
| `diff` | 3.8 µs per changed key |
| Booking hold commit, `Durability::Buffered` | 86 µs of CPU, 14 µs of it in the closure |
| Booking confirm commit, `Durability::Buffered` | 106 µs of CPU, 14 µs of it in the closure |

Of the 3.5 KB a single-key commit writes, 38% is the hashes of the children of the nodes on the
changed path and 21% those nodes' keys, as [Bytes of a small commit](../internals/commit-bytes.md)
measures, with the encodings tried to make it smaller.

The booking commits cost 4 and 14% more than the 82 and 93 µs this page gave before, measured on
2026-11-03. Since then the example's customer index covers six fields of each booking, which
[Covering indexes](../internals/schema-and-unique-indexes.md#covering-indexes) found adds 5% to the
instructions of a hold and 10% to those of a confirm, whose status it covers. Outside their closures
the two ran 72 and 92 µs.

Against 2026-11-04, the bulk insert went from 0.71 to 0.66 s and its peak RSS from 379 to 344 MiB,
and a full scan from 19 to 18 ns a key, after smaller slots, a lean insert recursion, a faster drop
of the working tree and a walk that reaches borrowed nodes through one inlined step. Seven runs
alternating with the build of 2026-11-04 found the same: the bulk insert took 661 ns and 3,932
instructions a key against 718 ns and 4,114, and a scan 17.9 ns against 19.1. A random get runs
1,025 instructions against 1,204 and a table's row get 829 against 988, but a get took 0.31 µs in
both builds, and the reads beside the writers came to 73M and 72M a second, so those two rows moved
between the days and not with the code. A range read got slower, 30 ns a key, and the fix described
under [Slower than before](#slower-than-before) took it back to 29. The other rows, the booking
commits among them, moved by less than the spread of their runs, and alternating runs gave a hold
86.4 µs against 85.5 and a confirm 106.5 against 105.4 at the same instructions within 0.3%.

## Against other embedded stores

Against other embedded stores on the same workload, with 1 GiB of cache each, medians of seven runs
of 3 rounds:

| Engine | Get | Scan | Durable commit | 8 writers, commits/s | File size |
| --- | ---: | ---: | ---: | ---: | ---: |
| ferro | 0.32 µs | 18 ns | 0.42 ms | 8,955 | 23 MiB |
| redb | 0.27 µs | 48 ns | 0.43 ms | 2,207 | 65 MiB |
| SQLite, `synchronous=FULL` | 0.87 µs | 49 ns | 0.51 ms | 1,080 | 57 MiB |
| fjall | 1.20 µs | 79 ns | 0.50 ms | 1,997 | 64 MiB |

ferro keeps every version and re-hashes objects as it reads them, and the others do neither. SQLite
bulk loads about 1.08 times faster, 0.62 s against 0.67 s, while fjall takes 1.4 times as long as
ferro and redb 2.0 times, and redb's `get` takes about 16% less time. Loaded with one `Map::extend`
instead of a loop of `insert`, ferro takes 0.38 s. These numbers come from a warm cache, on a
workload with few overwrites. Full results, including caveats about sled, are in
[`benchmarks/compare/RESULTS.md`](../../benchmarks/compare/RESULTS.md).

Against 2026-11-04, ferro's bulk load went from 0.71 to 0.67 s and its scan from 19 to 18 ns a key,
for the changes above, and its range of 1,000 keys from 27 to 29 µs, which the fix described under
[Slower than before](#slower-than-before) took back to 27. The get stayed at 0.32 µs against 0.27 µs
for redb, as [The read path](../internals/read-path.md#one-loop-for-borrowed-nodes) explains, and
[Huge pages](#huge-pages) takes about a tenth off both the get and the load. The other engines moved
by less than the spread of their runs.

## Writers and branches

`group` runs single-key commits into a map of 100,000 keys from each writer thread for three
seconds, either one writer on each of its own branches or all of them on one branch. Commits a
second, median of five runs, or of six in the first column:

| Writers | Buffered, own branches | Buffered, one branch | One branch, `coalesce` | Fsync, own branches | Fsync, one branch | One branch, `coalesce` |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 47,800 | 49,800 | 49,300 | 2,490 | 2,510 | 2,620 |
| 4 | 98,600 | 42,800 | 50,200 | 5,400 | 5,300 | 8,600 |
| 8 | 153,200 | 42,000 | 48,400 | 10,200 | 10,100 | 14,400 |
| 16 | 135,100 | 39,500 | 53,400 | 18,100 | 18,500 | 23,500 |

With `coalesce` 16 writers on one branch shared a version 13 at a time Buffered and 16 at a time
with fsync, and wrote 1,515 and 1,427 bytes a closure against 2,464 and 2,385 without it. A writer
alone on its branch has nothing to share, and `coalesce` there ran within 5% of the plain figures
of the same runs, before the first column was measured again, apart from 8 Buffered branches at
139,600 a second, 7% fewer.

Past 8 Buffered writers on their own branches, commits queue for `tip`, the one lock that places
every frame in the file. It was held about 6 µs a commit at 8 writers and 7 to 8 µs at 16, against
1.2 to 1.5 µs for one, mostly to move drafts that other frames overtook and to insert heads into a
larger branch table, and a waiter woken from it takes 1 to 2 µs more to run, so from 8 writers on
it is never free. [Where own-branch writers stop
scaling](../internals/commit-path.md#where-own-branch-writers-stop-scaling) has the measurements
and what was tried. A moved draft now rewrites only its links' distances, in place, which took the
instructions a commit runs under `tip` from 24,800 to 17,000 at 8 writers and from 28,000 to 20,200
at 16, as described under [Moving a draft in
place](../internals/commit-path.md#moving-a-draft-in-place). The own-branch Buffered column was
measured again with it on 2026-11-09, medians of six runs alternating with the build before, which
gave 46,700, 99,000, 144,800 and 112,800 a second in the same runs, with p99 at 16 writers 322 µs
against 410.

## Reads from many threads

`scaling` reads a 1M-key `Map<u64, String>` and a 200,000-row table from 1 to 32 threads pinned a
core at a time, so 8 fill one CCD, 16 both, and 32 add the second thread of every core. A lookup
reads about 20 rows by a range of an index, and a page 200 rows in key order, through a plain index
and through a twin whose index covers the field read. Operations a second, medians of five rounds
of a pass with no writer, measured again on 2026-11-05 once iterators, cursors and the row finder
borrowed nodes, roots and stores under hazards instead of counting them, as [Iterators under
hazards](../internals/read-scaling.md#iterators-under-hazards) describes, with other work under one
core busy in every cell. Measured once more that day with nothing else running, after the fixes
that followed, only gets at 32 threads with the 1 GiB cache moved by more than the spread of their
rounds, from 65.9M to 70.3M a second:

| Workload | Cache | 1 thread | 8 threads | 16 threads | 32 threads |
| --- | --- | ---: | ---: | ---: | ---: |
| `get` | 1 GiB | 3.0M | 23.1M | 42.8M | 70.3M |
| `get` | 16 MiB | 337k | 1.8M | 2.0M | 2.9M |
| `for_each` scan, keys | 1 GiB | 88M | 487M | 964M | 1,487M |
| `for_each` scan, keys | 16 MiB | 7.9M | 39M | 42M | 73M |
| 100-key range | 1 GiB | 422k | 3.1M | 5.8M | 8.6M |
| 100-key range | 16 MiB | 53k | 262k | 270k | 377k |
| Index lookup | 1 GiB | 232k | 1.7M | 2.9M | 4.4M |
| Index lookup, rows lent by `visit` | 1 GiB | 240k | 1.8M | 3.5M | 5.1M |
| Index lookup, covered | 1 GiB | 2.5M | 15.3M | 27.6M | 37.7M |
| Index lookup | 16 MiB | 14k | 67k | 76k | 108k |
| Index lookup, covered | 16 MiB | 2.2M | 14.0M | 26.0M | 35.9M |
| Page of 200 rows | 1 GiB | 183k | 914k | 1.1M | 1.5M |
| Page of 200 rows, lent by `next_ref` | 1 GiB | 305k | 1.9M | 3.4M | 4.1M |
| Page of 200 rows, covered | 1 GiB | 855k | 4.9M | 8.4M | 9.3M |

Against the figures this page gave on 2026-11-04, covered lookups at 32 threads read 37.7M a second
against 15.4M, plain lookups 4.4M against 2.8M, covered pages 9.3M against 3.4M and pages of rows
1.5M against 987,000. Covered pages at 16 and 32 threads move between about 5M and 10M a second from
one round to the next. In a pass of its own beside one thread committing single-key updates as fast
as it can, gets at 32 threads ran 66M a second with the 1 GiB cache and 2.5M with the 16 MiB one,
and index lookups 2.9M and 83,000, where the first pass gave 61M, 2.6M, 2.9M and 89,000. The last
depends on the build more than on the code: in four passes of those lookups alone, alternating the
builds, the one these figures come from ran 87,900 a second at 777,000 instructions a lookup, the
same tree built in another directory 93,000 at 751,000, and the tree of the first pass 90,800 at
762,000. Run in one pass with the writer's cells, as the tool does by default, the cells without a
writer read up to 12% slower on one thread. The fixes that followed a review of the hazards,
described in the same section, moved these cells by 2% or less in alternating runs, apart from
covered pages, 2 to 6% slower on one thread and within the swing they show between rounds on many.

The two rows read lent were measured later that day, in a pass of five rounds of every workload
with nothing else running, once lookups and iterators could lend their rows rather than hand out a
`Val`, as [Rows lent in place](../internals/read-scaling.md#rows-lent-in-place) describes. A `Val`
of a row with a destructor takes a reference count on its node's column, which every thread
reading the same rows writes, so pages of the same 200 rows read through `next_ref` ran 13 times
one thread at 32 threads where those read through `next` ran 8 times, and 65% faster on one thread.
That is about what 16 cores give a page that touches nothing shared, since the clock falls from
4.9 GHz with one core busy to 4.0 with 16 and the second thread of each core adds a tenth. In
alternating runs against the build before, the other cells ran within 2% of their rates, and the
covered ones within the swing of their rounds. [Reading rows in
place](tables.md#reading-rows-in-place) says when to use which.

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

## Reads past the cache

A read whose nodes are not in the cache reads each from the file, checks its hash and decodes it.
With a 1 MiB cache over the 1M keys, `ferro-bench`'s cold `get` reads 2.7 nodes a get, and the
cold scans of `reads` read a leaf for about every 8 keys. Measured on 2026-11-09, seven runs
alternating with the build before, pinned to one CCD while other work ran:

| Read | Before | After |
| --- | ---: | ---: |
| Cold `get`, 1 MiB cache | 5.76 µs | 5.47 µs |
| Cold `get`, 5 MiB cache | 4.11 µs | 3.90 µs |
| Cold scan, `u64` values | 106 ns per key | 85 ns |
| Cold scan, short strings | 140 ns per key | 119 ns |
| Decode of a random `u64` | 9.0 ns | 5.0 ns |

A miss now decodes the record from buffers its thread keeps rather than from copies of its own, and
a later record that lies within the bytes it read is taken from them, so a scan, which reads leaves
in the order they were written, takes most of them from bytes it already read. Warm gets, commits
and bulk loads moved by less than the spread of their runs. A miss in a get is still about a
quarter BLAKE3, which hashes a node of a few hundred bytes one block after another, a quarter
decoding, a sixth the `pread` and a ninth the cache's insert, as [Reading a node from the
file](../internals/read-path.md#reading-a-node-from-the-file) describes.

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

As with the allocator, ferro leaves this to the application. A node laid out to take fewer lines
does not do the same. A get reads about 38 lines on 13 pages, and one with each node in one
allocation, its keys inline beside its headers, read 31 on 11 and ran gets of short strings 2 to
7% faster, for slower loads, ranges and scans and 6 to 14% more heap, so it was left out. The
lines of a node arrive together, and what a get waits for is one miss a level, the leaf's from
DRAM, behind a page walk that huge pages take away and a smaller node does not. [Node
layout](../internals/read-path.md#node-layout) gives the figures.

## Other tools

The other `ferro-bench` tools, on the same machine and day, with how many runs each figure is the
median of:

| Tool, runs | Workload | Result |
| --- | --- | --- |
| `bulk`, 5 | 1M random keys inserted in one commit | 0.66 s: 0.48 s of inserts and 0.19 s to commit and drop the tree, 342 MiB peak RSS, against 0.73 s, 0.53 s, 0.20 s and 378 MiB on 2026-11-04 |
| `tables`, 7 | Booking hold, confirm and cancel, Buffered | 86, 106 and 146 µs of CPU, 14, 14 and 26 µs of it in the closure, 13,362 bytes a write |
| `covered`, 3 of 3 rounds | A balance update on 200,000 rows, a plain `tier` index against one covering `balance` | 7,660 against 10,415 bytes and 52 against 67 µs of CPU, indexes of 3.90 against 4.56 MiB |
| `merge`, 3 | Merge base of a fork at 10,000 commits followed by 10,000 and 1,000 more, of a branch that took a side branch 1,000 times, and of one with 1,000 features merged since it forked | 62 µs, 51 µs and 7.7 ms |
| `merge`, 3 | `Map<u64, u64>::merge` of 1M keys, 100,000 changed on each side | 176 ms, 302 ms with struct values and 445 ms for a `Table` with one index |
| `clone`, 5 | First `pull` of a 330 MB file of 124,937 objects, fsync | 0.82 s, 17 MiB peak RSS, 90 bytes of heap an object |
| `clone`, 5 | `bundle` of that file into another, then `receive` of it | 0.62 s at 13 MiB peak RSS, then 0.70 s at 13 MiB |
| `clone`, 5 | `pull` of three one-key commits | 1.33 ms, and 0.54 ms Buffered |
| `heap`, 3 | Live heap after scans with a 64 MiB cache | 45 MiB for short strings, 58 MiB for 300 byte strings, 65 MiB for 2 KiB values, 69 MiB for table rows |
| `watchers`, 5 | A Buffered single-key commit with 0, 1,000 and 10,000 parked watchers | 12.3, 12.0 and 12.1 µs, and 23, 57 and 375 µs at p50 to a watcher whose test passes |
| `reclaim`, 3 | 15 s of fsync commits keeping 100 versions, a cycle every 64 MiB | Longest pause 2.9 ms on `metrics`, 3.2 ms on `ledger`, 3.4 ms on `wiki` and 2.8 ms on table updates, commits a second within 2.2% of no cycles |
| `reclaim`, 3 | Updates of 20,000 rows over 64 and 256 branches, a cycle every 4 MiB | Median pause 3.1 and 4.4 ms |
| `reclaim copy`, 5 | The copy a reclamation cycle makes, keeping everything, of 189 MB of table rows and of 331 MB of 320 byte values, on the default 8 threads, measured on 2026-11-07 | 0.19 and 0.11 s, 1,000 and 2,960 MB/s, against 0.80 and 0.41 s on one thread |

## Slower than before

Measured again on 2026-11-05, one figure came out slower than on 2026-11-04 beyond the spread of the
runs, and builds of older trees run alternately with this one confirmed it: a range read, 29.8 ns a
key against 28.6 for the build of 2026-11-04 in `ferro-bench`, and 28.4 µs for 1,000 keys against
26.8 in the compare harness (#79). The harness puts the step at db07c34, which borrows nodes under
hazards in iterators, as [Iterators under
hazards](../internals/read-scaling.md#iterators-under-hazards) describes: its parent took 27.0 µs
and it took 28.1. A range runs no more instructions than before, 160.1 a key in the trees of
2026-11-04 and 2026-11-05 built the same way. The build these figures come from runs 162.1, and the
same tree built from a copy in another directory ran 160.1 and 28.7 ns, so what the compiler makes
of the range loop differs between builds of one tree by about that much. Index lookups beside a
writer with the 16 MiB cache vary between builds too, as [Reads from many
threads](#reads-from-many-threads) describes.

The range read is fixed. A borrow under a hazard read its node's flags behind the value, in a line
of their own, and the exchange that writes the slot waited for that line before the node's headers
were asked for, so each of the 130 or so nodes a range borrows cost two misses in a row where the
count had cost one. The weak count and the flags now sit at the front of each allocation, beside
the headers, as [The flags' line](../internals/read-scaling.md#the-flags-line) describes. In 15
alternating runs of the harness's ferro child a range took 27.4 µs against 28.2 before the fix and
27.3 for db07c34's parent, and in nine of `ferro-bench` 28.8 ns a key against 29.7. A get, a scan,
a bulk load and its commit and drop moved by less than the spread of their runs, a table's row get
took 157 ns against 155, and in `scaling` at 1, 8 and 32 threads no cell ran more than 0.4% slower
while gets, index lookups and `for_each` scans with the 1 GiB cache ran 3 to 6% faster. The figures
under [Reads from many threads](#reads-from-many-threads) were measured before the fix.

On 2026-11-04 four figures came out slower than before, beyond the spread of the runs, and builds of
older trees run alternately with the tree of that day confirmed each: a filtered scan of table rows,
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
