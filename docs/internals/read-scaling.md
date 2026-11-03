# Reading from many threads

How reads scale across threads: guarded gets and the epoch scheme, iterator frames, the allocator,
and how the node cache weighs what it holds. The single-thread read path is in [The read
path](read-path.md).

## The benchmark

`ferro-bench --bin scaling` runs reader threads over shared snapshots of a 1M-key `Map<u64, String>`
and a 200,000-row table with two indexes. Its workloads are a random `get`, a full scan through
`iter().fold`, a full scan through `for_each`, ranges of about 100 keys through `range(..).fold`,
index lookups of about 20 scattered rows through `by_balance_range`, and pages of 200 rows that sit
in key order through `by_tier(..).take(200)`. Each runs with a 1 GiB cache that holds everything and
a 16 MiB one that holds about a seventh of the decoded map, with no writer and with one thread
committing single-key updates as fast as it can. The test machine has 16 cores in two CCDs of 8,
each CCD with its own L3, and two threads per core. Threads are pinned a core at a time, so 8 fill
one CCD, 16 both, and 32 add the second thread of every core. The tool reads cycles, instructions
and the cache lines a thread pulled from another core's cache through `perf_event_open`, and how
busy the rest of the machine was, since the machine is shared. The tables are medians of runs that
alternated the two builds, leaving out runs during which other work kept more than two cores busy.

Operations a second with the 1 GiB cache and no writer, and in brackets the speedup over one thread:

| Workload | | 1 thread | 2 threads | 4 threads | 8 threads | 16 threads | 32 threads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `get` | before | 3.0M | 6.1M (2.0) | 12.0M (4.0) | 23.1M (7.6) | 32.1M (10.6) | 50.4M (16.7) |
| `get` | after | 2.9M | 5.8M (2.0) | 11.9M (4.1) | 23.2M (8.0) | 41.9M (14.5) | 67.2M (23.2) |
| `iter()` scan | before | 53.1M | 57.2M (1.1) | 112.8M (2.1) | 214.1M (4.0) | 409.3M (7.7) | 834.6M (15.7) |
| `iter()` scan | after | 52.4M | 53.7M (1.0) | 104.9M (2.0) | 201.4M (3.8) | 386.5M (7.4) | 796.3M (15.2) |
| `for_each` scan | before | 68.0M | 120.9M (1.8) | 206.0M (3.0) | 400.8M (5.9) | 784.4M (11.5) | 1282.3M (18.9) |
| `for_each` scan | after | 67.8M | 120.9M (1.8) | 206.3M (3.0) | 402.9M (5.9) | 782.2M (11.5) | 1276.6M (18.8) |
| 100-key range | before | 376k | 737k (2.0) | 1.5M (3.9) | 2.8M (7.5) | 3.9M (10.4) | 6.0M (15.9) |
| 100-key range | after | 386k | 744k (1.9) | 1.5M (3.8) | 2.8M (7.2) | 4.0M (10.3) | 5.9M (15.4) |
| Index lookup | before | 235k | 434k (1.8) | 757k (3.2) | 1.4M (5.8) | 1.3M (5.6) | 2.0M (8.3) |
| Index lookup | after | 225k | 452k (2.0) | 850k (3.8) | 1.6M (6.9) | 1.8M (8.1) | 3.0M (13.3) |
| Page of 200 rows | before | 175k | 317k (1.8) | 504k (2.9) | 759k (4.3) | 707k (4.0) | 983k (5.6) |
| Page of 200 rows | after | 178k | 322k (1.8) | 530k (3.0) | 830k (4.7) | 719k (4.0) | 979k (5.5) |

With the 16 MiB cache and no writer:

| Workload | | 1 thread | 2 threads | 4 threads | 8 threads | 16 threads | 32 threads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `get` | before | 334k | 602k (1.8) | 1.1M (3.2) | 1.9M (5.7) | 2.0M (6.1) | 2.9M (8.8) |
| `get` | after | 330k | 626k (1.9) | 1.1M (3.3) | 1.9M (5.8) | 2.0M (6.2) | 3.0M (9.2) |
| `iter()` scan | before | 7.6M | 12.2M (1.6) | 21.7M (2.9) | 38.8M (5.1) | 41.9M (5.5) | 70.6M (9.3) |
| `iter()` scan | after | 7.6M | 12.2M (1.6) | 21.7M (2.9) | 38.6M (5.1) | 41.8M (5.5) | 70.5M (9.3) |
| `for_each` scan | before | 8.0M | 12.8M (1.6) | 22.8M (2.8) | 40.7M (5.1) | 42.0M (5.2) | 73.0M (9.1) |
| `for_each` scan | after | 8.1M | 12.8M (1.6) | 22.9M (2.8) | 40.7M (5.0) | 42.1M (5.2) | 67.2M (8.3) |
| 100-key range | before | 55k | 96k (1.7) | 162k (2.9) | 277k (5.0) | 270k (4.9) | 398k (7.2) |
| 100-key range | after | 54k | 95k (1.8) | 160k (3.0) | 274k (5.1) | 274k (5.1) | 385k (7.1) |
| Index lookup | before | 14k | 24k (1.7) | 41k (2.9) | 70k (4.9) | 74k (5.2) | 107k (7.6) |
| Index lookup | after | 14k | 24k (1.7) | 40k (2.9) | 71k (5.0) | 75k (5.3) | 113k (8.0) |
| Page of 200 rows | before | 168k | 314k (1.9) | 524k (3.1) | 809k (4.8) | 742k (4.4) | 1.0M (6.1) |
| Page of 200 rows | after | 175k | 319k (1.8) | 533k (3.1) | 852k (4.9) | 708k (4.1) | 1.0M (5.7) |

Beside a writer, with the 1 GiB cache, gets went from 37.5M to 43.6M a second at 32 threads and
index lookups from 1.6M to 2.3M, and with the 16 MiB cache nothing moved beyond noise. In
`ferro-bench`'s concurrent section, eight readers beside eight writers on a 100,000-key map, reads
went from 62M to between 67M and 70M a second in the runs least disturbed by other work.

## What held scaling back

What held scaling back:

- Reference counts below the pinned levels. Every get upgraded a `Weak` memo on each of its four or
  so deeper levels and dropped the `Arc` again, two locked operations on the cache line that also
  holds the node's `keys` and `children`. With 8 readers on one CCD a get scaled with the thread
  count. At 16 a get pulled 1.6 lines a time from the other CCD where it had pulled none, its cycles
  rose from 1,730 to 2,440 for the same 1,231 instructions, and each thread lost 28% of its rate. A
  probe that read through the memo without upgrading, which is unsound under eviction, took 16
  readers from 32M to 44M gets a second and 32 from 51M to 69M, and `for_each` scans 35 to 40%
  higher.
- Index lookups. A cursor holds an `Arc` per level, so each row it seeks clones the pinned nodes of
  the first three levels again and drops them on the way back up, a few locked operations a row on
  the 73 nodes every reader shares. Lines pulled from other cores went from 100 to 209 a lookup
  between 1 and 8 threads, and 16 threads ran no faster than 8.
- A `Val` per item. `iter()` over values with a destructor hands out a `Val` that holds the node's
  column, a locked increment and decrement on a line other scanning threads write too. Two threads
  scanning the same map pulled 1.5 lines a key and each ran at 27M keys a second against 42M alone,
  where `for_each`, which lends the value, keeps 95% of its rate. Pages of the same 200 rows read by
  every thread stop at about 6 times one thread for the same reason.
- The allocator, with a cache smaller than the data. A get then decodes a node or two from the file
  and evicts as many, and the eviction frees nodes that other threads allocated. A gperftools
  profile at 16 threads put 31% of the time in those frees and 13% in the decode's allocations,
  against 8% and 5% on one thread, and at 32 threads the readers spent up to a quarter of their time
  asleep on glibc's arena locks. Preloading jemalloc took 32 threads from 2.7M to 4.2M gets a second
  and index lookups from 89,000 to 169,000. Shard locks in the cache were under 1% of the profile.

## Guarded gets and the epoch scheme

`epoch.rs` keeps a slot per thread, padded to its own lines, with the global epoch the thread saw
when it pinned, or zero. `Map::walk`, which serves `get`, `contains_key`, `value_hash` and `nth`,
`floor` and `summary_by` pin for the length of one descent, an exchange on the thread's own slot,
and read through a `Weak` memo by reference when its node is flagged as held by the cache, so a get
writes no shared memory at any level. A `Node` carries two flags. `cached` is set when a cache entry
takes the node and cleared when that entry is dropped by eviction, replacement or `clear`; the
entry's clones that `get` hands out are not owners, and the flag is cleared before the entry lets go
of its reference. `lent` is set by the first reader that borrows the node. An evicted node that was
never lent is freed at once, as before. One that was lent is retired with the epoch it was cleared
at and freed once every pinned slot is at a later epoch, checked when a thread has retired 128 nodes
or 64 KiB, and on `Store::close`, which waits for readers pinned before it so that retired nodes
holding nested maps do not keep the file open. A reader stores `lent` before it reads `cached`
again, and the cache clears `cached` before it reads `lent`, all sequentially consistent, so either
the reader sees the flag cleared or the cache sees the loan and defers the free. Iterators and
cursors still hold `Arc`s, since they live across the caller's code, and so does `for_each`, whose
callback could hold a pin, and with it every retired node, for as long as it likes.

The first version retired every evicted node. Gets with the 16 MiB cache fell by 44% at 16 and 32
threads, and a profile put 40% of the time in freeing the batches of 128 cold nodes, most of them
allocated by other threads. Retiring only lent nodes brought that back to within noise. An owner
beside each cache entry, holding a second `Arc`, doubled the entry from 24 to 48 bytes and grew the
heap per key by 3 to 5%; the entry now marks the copy the cache owns with a flag and gives up its
value with `Option::take` on a hit. `flush` also waits for nodes another thread has taken from a
batch and is still dropping, so that two stores closing at once cannot return before a retired node
holding the other's file is gone.

The flags made `Node` contain an `UnsafeCell`, so a `&Node` no longer tells the compiler that the
node cannot change, and loops that call out per item reloaded the node's fields after every call:
`iter()` scans went from 79 to 88 instructions a key and `for_each` stayed at 49. The folds of
`Iter` and `Values` now take the keys as a slice and a clone of the column once per run, and `scan`
walks a leaf's keys and values as two slices, which brought `iter()` scans to 64 instructions a key
and `for_each` to 30, 9 to 28% faster than before this work at 1 to 32 threads.

The protocol is checked four ways. `tests/epoch_stress.rs` runs eight readers doing gets, `floor`,
`summary_by`, row gets, index lookups through the `Finder`, and ranges both ways and cursor seeks
through the frames described under [Iterator frames](#iterator-frames), against a model of every
version, on handles with caches of 12 KiB to 1 MiB that read-only readers refresh and reopen, beside
two writers on a 12 KiB cache with reclamation cycles every 96 KiB and compaction, while the
writer's `Db` is dropped and reopened four times. It runs under AddressSanitizer and ThreadSanitizer
clean, and with `release` changed to free lent nodes at once both report the use after free within
the default four seconds. Unit tests in `epoch.rs`, `map.rs` and `store.rs` race a reader's borrow
with an eviction, check that only the cache's own entry releases its value, and walk iterators and a
cursor over pinned and borrowed nodes after their map is dropped or edited. They pass under Miri
with tree borrows over 16 schedules. Under stacked borrows all but one pass over 16 schedules:
`epoch_borrows_survive_an_eviction_that_races_them` fails on one schedule in 16, as it did before
the frames, because an eviction that finds a node not yet lent drops it in place while a reader
holding its `Weak` makes its first load of `cached`, and stacked borrows count the drop's `&mut` as
a write to every field.

## Index rows

`IndexIter` fetches rows through a `Finder` that keeps the last leaf it found and that leaf's
parent. A key within the leaf's first and last keys is in that leaf or nowhere, so rows in key order
are found in it without a descent; a key within the parent's span descends one level, and any other
key descends from the root under the guard. Only a new leaf and parent are upgraded, so a scattered
row takes two upgrades where the cursor took about five.

## Measured and left out

- Pinning every level. Pinned memos carry no hint, so a get on one thread was 38% slower, and 15%
  faster at 32.
- Holding the pinned levels of iterators and cursors by child index instead of by `Arc`, resolved
  from the root when needed. Index lookups gained 18% at 16 threads and 22% at 32, but lost 26% on
  one thread, ranges 17% and `iter()` scans 14%.
- Prefetching the node's header and flag lines along with the hint: no faster, a get on one thread
  3.3% slower than base where the guarded get alone was 2% slower.
- A plain store in place of the exchange on pin, which is unsound: no faster on one thread, so the
  fence is not what the guard costs.
- Moving `cached` and `lent` ahead of `keys`, so that they share a line with the header a walk reads
  anyway: no faster.
- A per-thread read buffer in place of the 1 KiB vector each disk read allocates: within 3% either
  way, with the 16 MiB cache and the 1 GiB one.
- 512 cache shards sized for four times the items, against the default of 128: gets and index
  lookups with the 16 MiB cache 11 to 18% slower.
- Aligning the store's shared state to 128 bytes, so that the `Store` clone every iterator takes
  does not share a line with the cache's shard table: within noise.

## What was left

With the 16 MiB cache the allocator still decides: the library cannot choose one, and the decode
allocates a vector, a column and a string per value whatever the reader does. A `Val` for a value
with a destructor costs a shared reference count per item, which `for_each` avoids but `iter()`
cannot without lending, so scans through `iter()` and hot pages of rows scale with it. Each iterator
still clones the store and the root, two counts every reader of a map shares, which holds ranges
near 16 times one thread at 32. A get on one thread is 1 to 4% slower than before, about what the
pin costs, since the same build with an unguarded walk was as fast as before. `iter()` scans from
two threads up still came out 4 to 8% slower than before in a quiet run after the loops were
hoisted, with fewer instructions a key than before, and equal in a rerun on a busier machine, so a
remaining cause is not found. [Iterators, rows and the allocator](#iterators-rows-and-the-allocator)
takes these up.

## Iterators, rows and the allocator

Measured again once commits were encoded outside `tip` and nodes carried `cached` and `lent`, on the
same machine and data, alternating the two builds of this section in one quiet session, medians of
three runs. Operations a second with the 1 GiB cache and no writer, and in brackets the speedup over
one thread:

| Workload | | 1 thread | 2 threads | 4 threads | 8 threads | 16 threads | 32 threads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `get` | before | 3.0M | 6.1M (2.0) | 12.2M (4.1) | 23.6M (7.8) | 42.5M (14.1) | 69.6M (23.1) |
| `get` | after | 3.0M | 6.1M (2.0) | 12.2M (4.1) | 23.5M (7.9) | 43.0M (14.4) | 69.2M (23.2) |
| `iter()` scan | before | 53.2M | 52.4M (1.0) | 101.7M (1.9) | 191.4M (3.6) | 374.1M (7.0) | 806.7M (15.2) |
| `iter()` scan | after | 52.7M | 52.2M (1.0) | 101.3M (1.9) | 189.0M (3.6) | 370.1M (7.0) | 818.9M (15.5) |
| `for_each` scan | before | 86.3M | 153.2M (1.8) | 235.1M (2.7) | 480.6M (5.6) | 939.1M (10.9) | 1465.5M (17.0) |
| `for_each` scan | after | 86.5M | 153.7M (1.8) | 238.3M (2.8) | 483.9M (5.6) | 952.5M (11.0) | 1462.9M (16.9) |
| 100-key range | before | 387k | 745k (1.9) | 1.5M (3.8) | 2.8M (7.1) | 3.8M (9.9) | 5.8M (15.0) |
| 100-key range | after | 385k | 740k (1.9) | 1.5M (3.8) | 2.8M (7.3) | 4.7M (12.3) | 7.4M (19.2) |
| Index lookup | before | 226k | 455k (2.0) | 856k (3.8) | 1.5M (6.8) | 1.8M (8.1) | 2.8M (12.4) |
| Index lookup | after | 228k | 452k (2.0) | 841k (3.7) | 1.5M (6.6) | 1.9M (8.3) | 2.9M (12.6) |
| Page of 200 rows | before | 176k | 317k (1.8) | 514k (2.9) | 815k (4.6) | 707k (4.0) | 970k (5.5) |
| Page of 200 rows | after | 170k | 308k (1.8) | 496k (2.9) | 785k (4.6) | 704k (4.1) | 985k (5.8) |

With the 16 MiB cache and no writer:

| Workload | | 1 thread | 2 threads | 4 threads | 8 threads | 16 threads | 32 threads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `get` | before | 342k | 630k (1.8) | 1.1M (3.2) | 1.9M (5.5) | 2.1M (6.1) | 3.0M (8.6) |
| `get` | after | 342k | 629k (1.8) | 1.1M (3.1) | 1.9M (5.5) | 2.1M (6.1) | 2.9M (8.6) |
| `iter()` scan | before | 7.4M | 12.0M (1.6) | 21.2M (2.9) | 37.6M (5.1) | 41.2M (5.5) | 70.4M (9.5) |
| `iter()` scan | after | 7.4M | 12.0M (1.6) | 21.2M (2.9) | 37.7M (5.1) | 41.4M (5.6) | 69.8M (9.4) |
| `for_each` scan | before | 7.9M | 12.8M (1.6) | 22.5M (2.8) | 40.1M (5.1) | 42.0M (5.3) | 72.8M (9.2) |
| `for_each` scan | after | 7.9M | 12.8M (1.6) | 22.6M (2.8) | 40.1M (5.0) | 42.3M (5.3) | 73.0M (9.2) |
| 100-key range | before | 54k | 95k (1.8) | 158k (3.0) | 269k (5.0) | 277k (5.2) | 382k (7.1) |
| 100-key range | after | 53k | 95k (1.8) | 158k (3.0) | 268k (5.0) | 279k (5.2) | 384k (7.2) |
| Index lookup | before | 14k | 24k (1.7) | 40k (2.9) | 68k (4.9) | 77k (5.5) | 109k (7.8) |
| Index lookup | after | 14k | 24k (1.7) | 40k (2.9) | 68k (4.9) | 76k (5.5) | 108k (7.9) |
| Page of 200 rows | before | 128k | 220k (1.7) | 374k (2.9) | 601k (4.7) | 482k (3.8) | 734k (5.7) |
| Page of 200 rows | after | 127k | 220k (1.7) | 374k (2.9) | 601k (4.7) | 509k (4.0) | 777k (6.1) |

### Iterator frames

An iterator or cursor held an `Arc` for each node on its path, so building the two edges of a range
took three counts on the root and two on each pinned node below it, and gave them back when it
dropped: locked operations on the nodes every reader shares, on the line that holds their `keys` and
`children`. A frame now holds a tagged pointer. A node the frame reached by upgrading a memo or by a
load is an `Arc`, as before. A node reached through a pinned memo or an in-memory link of a node the
iterator already borrows this way is a plain pointer with its low bit set, and the iterator keeps
one `Arc` of the root, which owns those links. A shared node is never changed in place, since
`Arc::make_mut` copies it first, and a memo is never replaced, so those pointers stay valid for as
long as the iterator holds the root. Frames drop before it. A cursor anchors to the root link it
already owns. An iterator over a dirty map takes no count at all, and one over a stored map takes
one on the root and one on each node below the pinned levels. Ranges went from 5.8M to 7.4M a second
at 32 threads and from 3.8M to 4.7M at 16, with one thread unchanged, and pages of rows on one
thread were within 3% either way across sessions. Index lookups at 32 threads fell into two modes in
both builds, about 2.8M a second and 3.2M to 3.4M, over nine runs of each, with medians of 2.85M and
2.89M.

The flag has to live in the pointer. A frame holding the pointer beside an `Option<Arc>` grew from
16 to 24 bytes and made `iter()` scans on one thread 9% slower and ranges 6%, with fewer
instructions than before, and padding the old frame to 24 bytes alone cost them 14% and 12%.

The step that `next` takes keeps its descent into a child in a function of its own. Inlined, the
step grew from 0x220 to 0x4f0 bytes of code and `for` loops over keys and rows were 3 to 4% slower
on one thread; out of line they are 0 to 10% faster than before the frames. Bulk load, commits, cold
gets, the heap of `heap`'s cases and peak RSS did not move.

### Measured and left out

- A `Val` that counts on a lease private to the iterator instead of on the column. `Iter`, its folds
  and the row finder kept one `Arc` holding a clone of the current column, renewed in place when no
  `Val` still held it, and handed out a clone of that `Arc` with the index. Two threads scanning
  together went from 49M to 61M keys a second, with lines pulled from other cores from 1.75 to 1.38
  a key, and 8 and 16 threads gained 9 to 10%, but one thread lost 10 to 14% and 32 threads 13 to
  19%: the fourth variant stopped the compiler inlining a `Val`'s drop into the caller's loop, and a
  scan went from 64 to between 85 and 89 instructions a key. A 100-key range lost 28% on one thread.
  Pages of the same 200 rows lost 19% on one thread and gained 15% at 32, and pulled as many lines
  from other cores as before, 214 a page at 8 threads: rows of a page sit a few to a leaf, so the
  count that moved off each row came back as the lease's count on each leaf's column, beside the
  count the finder takes on the leaf itself.
- A row finder that holds a leaf only once a second row falls in the span of the last one it
  descended to, so that scattered rows take no count at all. Index lookups gained 3 to 5% at 16 and
  32 threads, and pages of rows lost 20% on one thread and 13% at 32, and 36 to 43% with the 16 MiB
  cache, because each new leaf took two descents.
- Building the value column in its `Arc` as values decode, without the vector it is copied from. A
  probe that could not fail and assumed no spills was 1 to 3% faster with the 16 MiB cache, within
  noise, and the real thing needs `unsafe` to drop a half-built column on a decode error.
- Decoding from the read buffer instead of copying the body out of it: the copy and its allocation
  were under 1% of the profile below, so it was not built.

### The allocator

A gperftools profile of gets with the 16 MiB cache at 16 threads put 36% of the time in dropping
evicted nodes, called from the thread that inserted the node it had just decoded, and 21% in
decoding, two thirds of that in glibc's `malloc`. The evicted nodes are ones no guarded reader had
borrowed, so they are freed at once, not in the epoch's batches. A leaf of short strings frees a
dozen blocks of one size, glibc keeps seven of each size in a thread's cache, and the rest go back
to the arena that allocated them, most under that arena's lock. A larger cache per thread, set in
the environment, does as well as another allocator. Operations a second with the 16 MiB cache and no
writer, medians of three alternating runs:

| Workload | Threads | glibc | glibc, `tcache_count=1024` | jemalloc | mimalloc |
| --- | --- | --- | --- | --- | --- |
| `get` | 1 | 342k | 372k | 380k | 386k |
| `get` | 16 | 2.1M | 3.3M | 3.3M | 2.7M |
| `get` | 32 | 3.0M | 4.7M | 4.6M | 4.1M |
| Index lookup | 1 | 14k | 15k | 15k | 15k |
| Index lookup | 16 | 73k | 133k | 130k | 120k |
| Index lookup | 32 | 108k | 188k | 195k | 181k |
| 100-key range | 1 | 53k | 55k | 54k | 57k |
| 100-key range | 16 | 271k | 470k | 462k | 387k |
| 100-key range | 32 | 387k | 632k | 622k | 553k |

The tunable is `GLIBC_TUNABLES=glibc.malloc.tcache_count=1024`, and jemalloc was preloaded.
tcmalloc, preloaded in an earlier run on the build before the new frames, matched jemalloc: 4.7M
gets, 209,000 index lookups and 668,000 ranges a second at 32 threads. The library does not choose
an allocator, and a tunable is the application's to set, so
[Performance](../guide/performance.md#many-threads-and-the-allocator) says what to do. `ferro-bench`
builds `scaling` with mimalloc as its global allocator under `--features mimalloc`, which is off by
default and was used only for the last column.

### What is left

A `Val` for a value with a destructor still costs a shared count per item from `iter()`, and any
iterator, cursor or finder holds an `Arc` on each node below the pinned levels, a shared count per
leaf it passes. That is what pages of the same 200 rows meet: each page passes about 25 index leaves
and 50 row leaves that every thread also passes. Each iterator still clones the store and the root.
With a cache smaller than the data the allocator decides, and the library leaves that to the
application.

## Cache weight

`cache_bytes` is an approximate bound on the decoded heap of cached objects. An entry weighs its
body bytes, which stand for the strings and vectors inside it, plus what decoding allocates around
them: the `size_of` of the object, of each key and child link, and of the column with its `Arc`
header. A key or value that owns heap adds a flat allowance for the rounding of small allocations,
and a value also counts its `size_of` twice, because a wider value holds more allocations. One with
no destructor adds nothing, so a million `u64` keys now take half the budget they did. There is no
size hook on `Data`, because walking every value on load would cost more than the accuracy is worth.
Heap measured by `ferro-bench`'s `heap` tool with a counting allocator, after two scans of data
larger than the cache, as a multiple of the setting at 1 to 256 MiB:

| Values | Weight tracking disk size | With one `Arc` per value | Now |
| --- | --- | --- | --- |
| Short strings | 3.8 to 3.9 | 0.76 to 0.83 | 0.70 to 0.80 |
| 300 byte strings | 1.4 to 1.5 | 0.89 to 0.93 | 0.90 to 1.0 |
| 2 KiB spilled strings | 1.0 to 1.7 | 1.0 to 1.1 | 1.0 to 1.1 |
| Table rows and their indexes | 3.1 to 4.6 | 1.25 to 1.3 | 1.05 to 1.1 |

A root, a log entry or a spilled value is one decoded value rather than a node, so it weighs what
decoding it allocated, or its body bytes if those are more. The decoder counts that as it builds
each string, vector, `Box`, `Arc`, `Bytes` and std tree, rounded the way the counting allocator
rounds. Weighing a root by its body alone counted a short string at its length and nothing for the
vector holding it, so walking history in a fresh process kept roots whose heap was nearly four times
their weight. The `history_heap` test walks 20,000 versions of a root holding a table, a map and 200
strings. At a setting of 2 MiB the heap peaked at 7.1 MiB reading every version and at 4.4 MiB
through `history_of`, and at 32 MiB at 93 and 74 MiB. It now peaks at 1.1 and 1.3 times the setting
at 2 MiB, and at 1.05 times at 32 MiB. The test itself runs settings of 2 and 8 MiB, and asserts
that the heap left after the walk stays under 1.5 times the setting and the peak under twice it.
Counting costs an add per allocation on a cache miss and nothing on a hit.

Outside the bound are pinned nodes, including those the maps of a cached root pin, the `Arc`
allocations memos keep, the column of any `Val` still held, nodes held by live snapshots and dirty
trees, evicted nodes a guarded read may still be using, which wait in batches of up to 128 nodes or
64 KiB a thread, and on a writer the `dedupe` table, 32 bytes for every 4 KiB of the setting,
rounded up to a power of two sets, about 0.8 to 1.6% of it and at most 2 MiB. A history walk that
reads sixteen maps in every version peaks at 1.2 to 1.4 times the setting, mostly from those pins.

The default rose from 256 MiB to 1 GiB, which is about what the old setting held once full. A
setting carried over from before that holds about a quarter of what it did. `map_get_cold_ns` is 5.4
µs at 1 MiB and 3.8 µs at 5 MiB, against 7.4 and 5.2 µs with one `Arc` per value, because the same
bytes now hold twice the keys.
