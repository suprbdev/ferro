# Reading from many threads

How reads scale across threads: guarded gets and the epoch scheme, iterator frames, the allocator,
iterators under hazards, rows lent in place, and how the node cache weighs what it holds. The
single-thread read path is in [The read path](read-path.md).

## The benchmark

`ferro-bench --bin scaling` runs reader threads over shared snapshots of a 1M-key `Map<u64, String>`
and a 200,000-row table with two indexes. Its workloads are a random `get`, a full scan through
`iter().fold`, a full scan through `for_each`, ranges of about 100 keys through `range(..).fold`,
index lookups of about 20 scattered rows through `by_balance_range`, and pages of 200 rows that sit
in key order through `by_tier(..).take(200)`, and the same two read through `.covered()` from a twin
table whose indexes cover `balance`. Each runs with a 1 GiB cache that holds everything and a 16 MiB
one that holds about a seventh of the decoded map, with no writer and with one thread
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
writes no shared memory at any level. Nodes are counted by `Shared`, a reference count of the
crate's own, and each node's allocation carries two flags behind the node, as described under
[Where the flags live](#where-the-flags-live). `cached` is set when a cache entry
takes the node and cleared when that entry is dropped by eviction, replacement or `clear`; the
entry's clones that `get` hands out are not owners, and the flag is cleared before the entry lets go
of its reference. `lent` is set by the first reader that borrows the node. An evicted node that was
never lent is freed at once, as before. One that was lent is retired with the epoch it was cleared
at and freed once every pinned slot is at a later epoch, checked when a thread has retired 128 nodes
or 64 KiB more than it kept at its last check, and on `Store::close`, which waits for readers
pinned before it so that retired nodes holding nested maps do not keep the file open. A reader
stores `lent` before it reads `cached` again, and the cache clears `cached` before it reads `lent`,
all sequentially consistent, so either the reader sees the flag cleared or the cache sees the loan
and defers the free. Iterators, cursors and the row finder live across the caller's code, so they
borrow under hazards rather than a pin, as described under [Iterators under
hazards](#iterators-under-hazards), and `for_each`, whose callback could hold a pin, and with it
every retired node, for as long as it likes, still holds counts.

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
`Iter` and `Values` then took the keys as a slice and a clone of the column once per run, and `scan`
walks a leaf's keys and values as two slices, which brought `iter()` scans to 64 instructions a key
and `for_each` to 30, 9 to 28% faster than before this work at 1 to 32 threads. The folds now borrow
the column and call their closure from one place, as described under [Scans](read-path.md#scans),
and `iter()` scans take 58.

### Where the flags live

The flags used to be fields of `Node`, and under Miri's stacked borrows
`epoch_borrows_survive_an_eviction_that_races_them` failed on about one schedule in 16. An eviction
that found a node not yet lent dropped it in place while a reader holding its `Weak` made its first
load of `cached`. `Arc` drops its value through a `&mut Node`, and under stacked borrows a `&mut`
claims every byte it covers, `UnsafeCell` fields included, so the compiler may read or write them
speculatively; Miri reported that retag as a write racing the reader's atomic load. Tree borrows let
a `&mut` to interior-mutable bytes coexist with other accesses, so they passed.

Ordering the eviction differently could not fix it. A reader loads `cached` before it knows anything
about the node, and a `Weak` memo outlives the node for as long as its parent does, so a reader that
pins after an eviction can be making that load while any later drop of the node runs, including the
epoch's free of a node that was lent and retired.
`epoch_readers_after_an_eviction_race_the_free_of_what_it_retired` does that, and with the flags in
`Node` it failed under stacked borrows on 4 of 32 schedules, inside `epoch::flush`. Deferring every
drop would have cost the 44% above and still raced.

So the flags left the node. `Shared<T>` in `shared.rs` holds the strong count, a 32-bit weak count
and the two flags at the front of its allocation and the value right after them, so a node's
allocation stays 152 bytes. A reader reaches the flags from its `Weak` without making a
reference to the node, and no reference to them is ever exclusive: the last owner drops only the
value, through a `&mut` to the value alone, and `get_mut`, `make_mut` and `unwrap_or_clone` lend or
move the value alone. The allocation is freed when the last `Weak` goes, and a reader's `Weak` sits
in a parent that its own guard or count keeps alive. That leaves the node itself, which a reader
touches only once the protocol above has shown it safe: it borrows only after it stored or saw
`lent` and then still saw `cached`, so the cache had not yet cleared `cached`, the cache's later
load of `lent` sees the loan, and the node waits in the epoch until the reader unpins. A node
dropped in place, at eviction or by whoever holds its last count, is one that no reader has borrowed
or can still borrow, and a reader that looks at it afterwards reads only its flags. The argument
does not depend on what a `&mut` claims about interior-mutable bytes, so it holds under stacked and
tree borrows alike, and the flags are only ever accessed atomically, so there is no data race in the
C++ memory model either.

The first layout kept `Arc`'s order, with the flags beside the counts in a 16-byte header. Index
lookups then pulled 22 to 26 lines a lookup from the other CCX at 16 and 32 threads, against 18 to
22, because the flags every guarded descent reads shared a line with the strong count that the row
finder's upgrades write. Behind the value the flags sat 148 bytes from the count, where before they
sat 97 bytes into the allocation, and lookups pulled 19 to 21 lines from the other CCX against 18 to
22, at the price of about 20 more lines a lookup from the local L3, 127 against 105 on one thread,
which did not show in the rate. Once iterators, cursors and the row finder borrowed under hazards,
the finder no longer wrote that count, and the line the flags had to themselves cost iterators more
than the sharing had cost lookups, so they went back to the front, as [The flags'
line](#the-flags-line) describes.

`Shared` takes the place of `Arc` for nodes everywhere. A cache entry holds a node as `Erased`, a
pointer and a static table of its type's offsets and drop, and any other object as
`Arc<dyn Cached>`, in an enum that is still 24 bytes. Before freeing, `Shared` acquires the counts
with a load rather than a fence, as `Arc` does when built for ThreadSanitizer, which does not model
fences: with the fence, ThreadSanitizer reported the final drop of a node that an iterator had just
read.

Measured on the machine and data of the tables above, alternating the two builds six times in one
session, medians of the runs during which other work kept at most two cores busy. Operations a
second with the 1 GiB cache and no writer:

| Workload | | 1 thread | 16 threads | 32 threads |
| --- | --- | --- | --- | --- |
| `get` | before | 2.9M | 43.6M | 69.7M |
| `get` | after | 3.0M | 44.4M | 69.6M |
| `iter()` scan | before | 52.2M | 374.9M | 818.0M |
| `iter()` scan | after | 53.7M | 377.9M | 825.3M |
| `for_each` scan | before | 86.1M | 946.0M | 1423.3M |
| `for_each` scan | after | 88.7M | 957.7M | 1436.4M |
| 100-key range | before | 385k | 4.9M | 7.6M |
| 100-key range | after | 396k | 5.0M | 7.5M |
| Index lookup | before | 224k | 1.9M | 2.8M |
| Index lookup | after | 226k | 2.1M | 3.2M |

With the 16 MiB cache and no writer:

| Workload | | 1 thread | 16 threads | 32 threads |
| --- | --- | --- | --- | --- |
| `get` | before | 349k | 2.1M | 2.9M |
| `get` | after | 350k | 2.1M | 2.9M |
| `iter()` scan | before | 7.4M | 41.5M | 69.6M |
| `iter()` scan | after | 7.4M | 41.6M | 69.7M |
| `for_each` scan | before | 7.9M | 42.4M | 71.3M |
| `for_each` scan | after | 7.9M | 42.0M | 71.9M |
| 100-key range | before | 54k | 276k | 380k |
| 100-key range | after | 54k | 274k | 381k |
| Index lookup | before | 14k | 75k | 102k |
| Index lookup | after | 14k | 76k | 106k |

Index lookups at 16 and 32 threads still fall into the two modes described under [Iterator
frames](#iterator-frames) in both builds, about 1.8M and 2.1M a second at 16 threads. The base build
landed in the slower one in four of six runs here and the new one in two, and in the session with
the first layout the other way round, so the gap between the medians is that and not the change.
With no `UnsafeCell` left in `Node`, one thread with the 1 GiB cache runs 1,223 instructions a get
where it ran 1,281, 63 a key in an `iter()` scan where it ran 65, 29 in `for_each` where it ran 30,
and 3% fewer in ranges and index lookups. In `reads`, medians of four alternating runs, the heap per
key did not move in any case, 60.5 bytes for `u64` values and 108.5 for strings, bulk loads, commits
and cold scans stayed within 4%, and random and sorted gets, scans and walks over keys on one thread
were up to 10% faster. Gets and batches over every 256th or 4096th key varied by up to a factor of
two between runs of the same build. One of them, `get_many` over every 4096th key of the string map,
came out 24% slower in every run, but with the same counters added to both builds it took 99 to 103
ns a key against 100 to 101, on 4% fewer instructions, so that was where the code landed in that
binary.

Two regressions found later were bisected to this change. A 1M-key `bulk` load ran 3,092
instructions a key in its inserts against 2,978 (#59). `Arc::make_mut` is inlined and
`Shared::make_mut` was not, so every level of an insert called out to it and spilled around the
call. Its fast path, the exchange on the strong count, the load of the weak count and the store
back, is now inlined, and the copy for a shared node and the move for one with a `Weak` stay out of
line, marked cold. The inserts run 2,941 instructions a key. With the values formatted before the
loop they run 2,204 against 2,202 before `Shared` and 2,342 with `make_mut` out of line, at 2,375
cycles a key against 2,414 and 2,420, medians of seven alternating runs. Gets, scans, ranges and
the commit and drop after the inserts did not move.

`wiki` commits in `reclaim live` fell from about 691 a second to 564 at this change (#57), but the
store did no more work for them. With counters around each step of a single-writer edit, the slug
lookup, the update of the page's body and the comparison took the same cycles before and after and
the commit the same instructions, and nearly all of the 2.1M extra cycles a commit were in the
example's tokenizer, which runs twice an edit. It ran 8.9M instructions and 3.3 to 3.6M cycles over
a 64 KiB page where it had run 8.6M and 2.6M, even when timed before the database was opened. The
wiki's source had not changed, but the code rustc built for it had, most likely because its codegen
units also hold the `ferro` generics it instantiates, which this change resized.
`token.extend(c.to_lowercase())` was no longer inlined, and the out-of-line `String::extend` read
the lowercase characters with a 16-byte load from an 8-byte and a 4-byte store, which store
forwarding cannot serve, once for every letter and digit. The tokenizer now pushes ASCII letters
and digits lowercased itself and leaves other characters to `to_lowercase`, and takes 5.3M
instructions and 1.5M cycles in either tree. With it in both trees an edit costs the same
instructions before and after `Shared`, at cycles within 2%, and `wiki` commits 1,004 times a second
against 548 without it, medians of three alternating runs on a machine other work was sharing.

Measured and left out:

- Moving the weak count to the front beside the strong count, both 32-bit so that the allocation
  stays 152 bytes, so that `make_mut` reads one line of the allocation rather than two. Inserts ran
  2,433 cycles a key against 2,375 with the weak count behind the value, medians of seven
  alternating runs. The weak count and the flags have since moved to the front beside a 64-bit
  strong count, as [The flags' line](#the-flags-line) describes, and inserts there ran within 1% of
  the layout before.
- Deferring the drop of every evicted node that has a `Weak` memo, not only the lent ones, with the
  flags still in `Node`. With the 16 MiB cache memoized evictions outnumbered lent ones 3 and 9
  times for gets on 1 and 16 threads and 9 and 13 times for index lookups, and ranges, which never
  lend, evicted a memoized node 11 to 12% of the time. Gets were 4% slower at 16 threads and 7% at
  32, and index lookups 6% and 7%, medians of four and three alternating runs, and drops by other
  owners would still race.
- An `Arc` of the two flags beside each `Weak` memo. It is sound under both models, but every
  memoized node gains a 32-byte allocation and every `Link` grows from 72 to 80 bytes, about 6 bytes
  a key on a 1M-key `Map<u64, u64>` held in the cache, against 61. Not built.
- Moving a node out with `Arc::try_unwrap` instead of dropping it in place, so that no `&mut` covers
  it. The current `try_unwrap` reads through a shared reference, but `into_inner` and `Drop` take a
  `&mut`, and `Weak::as_ptr` promises nothing once no strong count is left, so it would rest on
  `std`'s internals.
- Making `Node` `!Unpin`, which Miri's stacked borrows exempt from the uniqueness of a `&mut`. That
  silences the report without giving a reason the code is sound, and drops `noalias` from every
  `&mut Node`.

### How the protocol is checked

The protocol is checked five ways. `tests/epoch_stress.rs` runs eight readers doing gets, `floor`,
`summary_by`, row gets, index lookups through the `Finder`, and ranges both ways and cursor seeks
through the frames described under [Iterator frames](#iterator-frames), against a model of every
version, on handles with caches of 12 KiB to 1 MiB that read-only readers refresh and reopen, beside
two writers on a 12 KiB cache with reclamation cycles every 96 KiB and compaction, while the
writer's `Db` is dropped and reopened four times. Each reader also keeps a range, read forwards or
backwards, a cursor and an index walk from one round to the next and advances each by a few dozen
items a round, so that they outlive the snapshot they came from and, once the reader reopens its
handle, the `Db` and store they read, while the writers commit and the caches evict; one round in 25
drops each mid-walk. The root also keeps a map of values that each hold a nested map, most of them
large enough to spill, which a third writer replaces: readers get them, walk them with iterators
held across rounds and keep some of what they read past the iterator, the snapshot and the handle it
came from, and one round in 33 opens a handle, takes a range and a cursor over them and drops the
snapshot and the handle before it reads, so that a decoder clones a store that only the iterator's
slot holds. It runs under AddressSanitizer and ThreadSanitizer clean with its default seed and three
others, and two copies at once ran clean with eight seeds. With `Shared::release` changed to report
no loan, so that lent nodes are freed at once, with a lent value's last drop no longer parked while
a slot holds it, or with the retired list releasing a lent value's last count without reading the
records again, as described under [Iterators under hazards](#iterators-under-hazards),
AddressSanitizer reports a use after free with each of the four seeds, and ThreadSanitizer 85 data
races for the first and 8 for the second within the default four seconds. Against the version that
left a lent store dead, AddressSanitizer reports one with two of the four seeds. With `free`
ignoring the records as it picks what to free, only the unit tests fail, under Miri, since its
second reading still parks what a slot holds. `tests/reads.rs` reads nested maps from iterators and
cursors that outlived every handle on their store, checks on Linux that the file closes as the last
of them drops, and edits a stored map beside an iterator and a cursor over it, clean under
AddressSanitizer and ThreadSanitizer. Unit tests in `epoch.rs`, `map.rs`, `store.rs` and `shared.rs`
race a reader's borrow with an eviction, race readers that pin after an eviction with the epoch's
free of the node it retired, check that only the cache's own entry releases its value, walk
iterators and a cursor over pinned and borrowed nodes after their map is dropped or edited, and
drive `Shared`'s counts, moves, erasure, layout and flags, including flags read after the value is
dropped. For the hazards they hold iterators, a cursor and the row finder across an eviction and a
flush, drop iterators mid-walk, keep a stored root for the iterators that borrow it after its map is
dropped, keep a leaf the finder holds once the borrowed node that pinned it is freed, borrow a root
through its map after a free has scanned the records, run a flush between a free's scan and its
pass over the lists, so that the free meets an item another found held through a slot written
after its scan, run a flush, a loan and a retire between a free's second reading of the slots and
its pick, so that the pick meets an item another found held since at an address it found loose,
and move records between threads. They also check that `make_mut`, `get_mut` and
`unwrap_or_clone` copy a value a slot holds, that each end of a narrow range holds its nodes in its
own slots and that a finder whose descent panicked holds only what its slots hold, race a lent
value's last owner with the release of its slot, which must leave the value on no retired list,
and with a clone through it, and run a free inside a borrow through a memo and inside the copy of
a slot, between the slot's write and the second read of `cached`, and check that the record's drop
lets go of what the free kept once the borrow has cleared its slot. `Shared`'s and `Erased`'s
clones assert in debug builds that the count they add to is not zero. They pass under Miri with
stacked borrows and with tree borrows over 32 schedules each. Under Miri the epoch's lists lock with
`std::sync::Mutex` rather than `parking_lot`, whose futex call Miri's shim rejects when two threads
contend for a lock, which stopped one schedule in 32 under tree borrows. Under Miri their assertions
fail for the sabotages of the loan, the records and the retired list's second reading, and Miri
reports a data race on the freed value when a last drop is not parked. The uniqueness check made to
ignore slots and the finder made to keep its fields through a descent fail their unit tests on
every run. The decoder's clone made to take a count from zero is reported as a dangling reference in
3 of 64 schedules, the owner made to read its mark before clearing its slots fails the race in 2 of
64, and the parker made to trust its first reading of the slots in 1 of 256. The drop made to read
its mark only when a slot is set fails the free run inside a borrow on every run. Leaving the fence
out of `epoch::retract` passes that test, which runs on one thread, and also passed a race of the
same borrows beside a free on another thread over 64 schedules, in which Miri never let the free
read the slot's old address once the clear had run; the fence rests on the argument under [The root
and the store](#the-root-and-the-store). The forms that lend rows and entries in place are checked
as [How it is checked](#how-it-is-checked) describes.

The unit tests share the process's epoch and records, so a free that one test makes acts on every
other test's items, and `make test-stress` runs them that way: twelve copies of the library's test
binary at once, 16 test threads each, 25 times over, which keeps every core busy and preempts
threads between a free's scan and its pass over the lists. `make ci` runs it after the tests, in
about 30 seconds. Before the fixes described under [The root and the
store](#the-root-and-the-store) 16 of 1,500 runs failed, and 21 of 600 built with each sanitizer,
neither of which reported anything. With the first fix alone none of 1,200 runs failed, and with
both none of 3,000, nor of 600 under AddressSanitizer, while under ThreadSanitizer 4 of 600 failed,
all in the race the same section describes, the tests' own doing; once that test was fixed none of
900 failed under ThreadSanitizer, 600 under AddressSanitizer or 1,500 in a release build. The unit
test that runs a flush between a free's scan and its pass failed every run before the first fix.
Once the counts and flags moved to the front of the allocation, as described under [The flags'
line](#the-flags-line), which changed no step of the protocol, the unit tests passed under Miri
with stacked and with tree borrows over 32 schedules each, the library's tests, `epoch_stress` with
four seeds and `tests/reads.rs` passed under each sanitizer, none of 1,200 runs of `make
test-stress` failed, nor of 600 of the library's tests built with each sanitizer, and `make ci`
passed.

Each of these runs inside `systemd-run --user --scope -q -p MemoryMax=16G -p MemorySwapMax=0`, as
the `Makefile`'s targets do, and `FERRO_STRESS_SEED` and `FERRO_STRESS_MS` change the stress test's
seed and its four seconds. `STRESS_ROUNDS` and `STRESS_COPIES` size `make test-stress`, and the
unit test binary that a sanitizer build leaves under its target directory runs the same way, a dozen
copies at a time with `--test-threads 16`:

```
make test-stress STRESS_ROUNDS=250
MIRIFLAGS="-Zmiri-many-seeds=0..32" cargo +nightly miri test -p ferro --lib -- epoch_ shared_
MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-many-seeds=0..32" \
    cargo +nightly miri test -p ferro --lib -- epoch_ shared_
RUSTFLAGS="-Zsanitizer=address" CARGO_TARGET_DIR=target/asan cargo +nightly test -Zbuild-std \
    --target x86_64-unknown-linux-gnu --release -p ferro --lib --test epoch_stress --test reads
RUSTFLAGS="-Zsanitizer=thread" CARGO_TARGET_DIR=target/tsan cargo +nightly test -Zbuild-std \
    --target x86_64-unknown-linux-gnu --release -p ferro --lib --test epoch_stress --test reads
```

## Index rows

`IndexIter` fetches rows through a `Finder` that keeps the last leaf it found and that leaf's
parent. A key within the leaf's first and last keys is in that leaf or nowhere, so rows in key order
are found in it without a descent; a key within the parent's span descends one level, and any other
key descends from the root under the guard. Only a new leaf and parent are upgraded, so a scattered
row takes two upgrades where the cursor took about five. The finder now borrows them under hazards
instead, as described under [Iterators under hazards](#iterators-under-hazards).

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
load holds a count, as before. A node reached through a pinned memo or an in-memory link of a node
the iterator already borrows this way is a plain pointer with its low bit set, and the iterator
keeps one count on the root, which owns those links. A shared node is never changed in place, since
`Shared::make_mut` copies it first, and a memo is never replaced, so those pointers stay valid for
as long as the iterator holds the root. Frames drop before it. A cursor anchors to the root link it
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
iterator, cursor or finder holds a count on each node below the pinned levels, a shared count per
leaf it passes. That is what pages of the same 200 rows meet: each page passes about 25 index leaves
and 50 row leaves that every thread also passes. A covering index skips the row leaves, as described
under [Covering indexes](schema-and-unique-indexes.md#covering-indexes). Each iterator still clones
the store and the root. With a cache smaller than the data the allocator decides, and the library
leaves that to the application. [Iterators under hazards](#iterators-under-hazards) takes up the
counts.

## Iterators under hazards

An iterator, cursor or row finder took a count on each node below the pinned levels that it passed,
and every reader of a map passes the same nodes. With two threads on one CCX, a page of the same 200
rows pulled 77 lines a page from the other core's cache where one thread pulled none, about one for
each of the 25 index leaves and 50 row leaves the page passes, and a covered page, which passes only
the index leaves, pulled 25. A short lookup passes few leaves, but its index iterator and its row
finder each cloned the store and the root and dropped them again, eight locked operations a lookup
on three lines that every reader of the table writes: a covered lookup pulled 10 lines at two
threads against 5 on one, and 4 a lookup from the other CCX at 16 threads, where it ran slower than
at 8.

Three probes bounded what removing the counts could gain. Each skipped them in a way that is unsound
once nodes are evicted, and leaked one count on every node it borrowed so that the cache's evictions
could not free what it read. Operations a second at 32 threads with the 1 GiB cache and no writer,
medians of two alternating runs of two rounds:

| Workload | Before | No counts below the pinned levels | Nor on the root and store | Nor on a row's `Val` |
| --- | --- | --- | --- | --- |
| Index lookup | 2.8M | 3.1M | 4.4M | 4.9M |
| Index lookup, covered | 15.1M | 17.3M | 44.7M | 45.2M |
| Page of 200 rows | 1.0M | 1.5M | 1.6M | 3.6M |
| Page of 200 rows, covered | 3.5M | 8.0M | 11.5M | |
| 100-key range | 7.7M | 8.0M | 8.5M | 8.4M |

### Hazards

`epoch.rs` keeps hazard records, eight pointer slots in a line of their own, in a list that `free`
reads. An iterator, cursor or row finder takes a record when it is built, from a spare list kept per
thread, so taking one is a pop from thread-local memory, and clears it and hands it back when it is
dropped, on whichever thread drops it. With no record to be had, because the thread is exiting or
4,096 records are in use, it counts as before.

To borrow a node through a `Weak` memo, an iterator runs the protocol of a guarded get with a slot
written first: it reads `cached`, stores the node's address in a slot, sets `lent` if it is not set
and reads `cached` again, all sequentially consistent. If `cached` is still set, the cache had not
cleared it when the slot was written, so the cache's release sees `lent` and retires the node, and
`free` frees a retired node only once no slot holds it. It reads the slots after the epoch it bumps
and frees only what was retired before that bump, so a slot written before a release is always
seen. A node it finds a slot holding waits only for slots from then on, and any free lets such a
node go only once it has read the slots again, as described under [The root and the
store](#the-root-and-the-store). If `cached` was cleared, the iterator clears the slot and takes a
count as before. A frame holding a borrowed node is a tagged pointer with no count, like one
anchored to the root, and so are frames below it reached through pinned memos or in-memory links,
which live as long as it does.

A borrowed node is not changed in place. The cache or the retired list usually holds a count on it
while a slot can hold it, and an iterator over a dirty map counts its root, which keeps every node
below it shared, but a writer that fetched a node from the cache just as the cache let go of it
holds that node's only count while a slot may still hold it. So `make_mut`, `get_mut` and
`unwrap_or_clone` count a value whose `lent` is set as shared while a slot holds it, reading the
records to find out, and clear `lent` when no slot does, so that a writer's next edits of the node
read no records. `lent` sits on the line of the weak count, which `make_mut` reads already.

The slots of a path are chosen by level: the node a frame enters takes the slot of its parent's
level minus one, so the leaf, its parent and its grandparent each have one, frames on one path never
share one, and the front and back paths of an iterator have three each. Where the two paths of a
range share their top frames, a frame whose node a slot holds is copied into a slot of the back
path's, and the copy then reads `cached` again, sequentially consistent: if it is still set, the
cache clears it only after the new slot is written, so any free of the node, which comes after that,
reads the new slot. If it was cleared, the back path takes a count instead, through the clone that
waits out a count passing through zero, described below. A first version copied the slot and read
nothing after, which was sound only because a free reads a record's slots in order, front before
back: read the other way, a front slot overwritten after the copy could have been read after its
change and the back slot before it. Borrowing each shared node a second time through its parent's
link also made the copy independent of that order, but cost a covered lookup 490 instructions and 6
to 10% of its rate, where reading `cached` after the copy costs 6 instructions. A frame records in
its pointer's second bit whether a slot holds its node, beside the first bit that says it holds no
count. Nodes higher than that but below the pinned levels are counted as before; a scan of the
1M-key map enters one about every 4,096 keys. The row finder keeps its leaf and the leaf's parent in
two slots and also sets `lent` on them, since nothing above them stays borrowed once its descent is
over, and a leaf that only a borrowed node's pinned memo held would otherwise be dropped in place
with that node. It lends them through the link it reached them by: Miri's stacked borrows rejected a
first version that set `lent` through a pointer taken from the `&Node`, whose permission covers the
node and not the flags behind it. It takes its leaf and parent out of its fields before a descent
and puts back what the descent kept. A first version replaced them only after the descent returned,
so a descent that rewrote the parent's slot and then failed to read a leaf, which panics, left a
finder that a caller catching the panic could use with a parent no slot held.

### The root and the store

The root of a stored map is borrowed through the map that builds the iterator: the iterator writes
the root's address in a slot and sets `lent` while the map still holds its count, so whoever drops
the root's last count later sees the flag. `Shared`'s last drop checks `lent`, and a value that is
lent and held by a slot, which it learns by reading the records, is parked: the thread that took its
count to zero takes the count back and leaves it on the retired list, which lets it go once no slot
holds the value. `Store` keeps its state in a `Shared` for this. A dirty map's root is still
counted: only its writer reads it, and a borrowed root would make the writer's next edit copy it.
The cursor and the row finder borrow the root only once its pinned memo is set, so that building one
reads nothing from the file.

A first version left such a value dead, its count at zero, until no slot held it. A borrowed store
could then be cloned back to life: an iterator that outlived its map, its snapshot and its `Db`
decoded a value holding a nested map, `Map::decode` cloned the store through the iterator's
uncounted handle, and the count went from zero to one and back, so the store was dropped under the
iterator and again by the retired list. A review reproduced it under AddressSanitizer. A parked
value keeps a count, so a clone through a borrowed handle finds one. `Decoder::store`, the one clone
of a store that can start from a borrowed handle, waits out a count that is passing through zero,
between a last owner's decrement and its taking the count back, rather than taking one from zero;
every other clone adds to a count its handle holds, since a loop of compare-and-swaps on the line
that readers and committers all write costs more than an add. Keeping a counted store in iterators,
cursors and the row finder would also have fixed it, at the cost measured below.

An iterator, cursor or row finder whose slots kept something from being let go flushes the epoch
when it is dropped, so a store's file closes when its last iterator goes, as it did when the
iterator held a count. The thread that parks a value, and a free that skips an item because a slot
holds it, mark each record that holds it in any slot and then read that slot again, both
sequentially consistent. A record's owner clears its slots and reads its mark behind a sequentially
consistent fence, so either the owner sees the mark and flushes or the marker finds the slot cleared
and lets the item go itself; one fence ran covered lookups 3% faster than clearing the slots with
sequentially consistent stores. An item a free skips for a slot has passed its epoch, so it is
flagged as waiting only for slots, and a marker that lets one go picks that item alone. A first
version had the owner check whether its store or root had died before clearing its slots, and missed
one that died between the two, which then waited for the next free. A second marked records only
when it parked a value, in their root and store slots: when `Db::close` cleared the cache, a root an
iterator borrowed was retired with its count and skipped by the close's flush, and its inline nested
maps held the store's count, so the store was never parked and the iterator's drop saw no mark. The
root, the store and its file stayed until an unrelated flush. A test now counts the process's
descriptors on a file as its last iterator, cursor and nested map's iterator drop. Marking for every
slot cost the cells with the 16 MiB cache 0.3 to 0.7% more instructions and 1 to 4% of their rate
across two alternating runs, and moved nothing with the 1 GiB cache.

Two borrows write a slot and then clear it again: a borrow through a `Weak` memo whose second read
of `cached` finds it cleared, and the back path's copy of a shared frame when the copy's read of
`cached` does (#75). The clear was a release store, and the owner read its mark only if some slot
was set when it dropped the record. A free could mark the record between the borrow's write and its
clear, read the slot again before the clear reached it and keep the item, and an owner whose other
slots were all clear when it dropped the record never read the mark, so the item, a store's file
with it, waited for an unrelated free. The map's iterators, cursors and row finder hold their store
in a slot until they drop whenever a node slot can be written, so their drops read the mark, but the
protocol did not promise it. The clear now goes through `epoch::retract`, a release store and then a
sequentially consistent fence, the handshake of the clear at drop, and the drop reads the mark
whether or not a slot is set, behind its own fence only when one is. Either the free's second
reading comes after the borrow's fence and finds the slot clear, so the free lets the item go
itself, or the mark comes before that fence, and the drop sees it and flushes. The clear is rare:
over a round of every workload of `scaling` at 1, 8 and 32 threads with both caches, the back path
copied a frame more than 67M times and cleared one copy, and borrows through a memo cleared 8 to 15
slots.

The first version freed a lent value from a retired list as soon as the list's own count was the
last, trusting the scan of the records that the free began with. A root the cache had retired could
then be borrowed through its map after that scan, and the map dropped before the list let go, so the
root was freed under the iterator: `epoch_stress` crashed in release builds when two copies ran at
once, and AddressSanitizer reported the use after free. When the list's count is the last on a lent
value, `free` now reads the records again, after the decrement that orders it behind the owner that
let go, and parks whatever a slot holds. That costs one more scan a batch, and only for batches that
free lent values.

A free still trusted the scan it began with for items that already waited only for slots (#77).
That scan shows every slot an item retired before the free's bump can be held by, but an item can
come to wait only for slots after the bump: a later free finds it held by a slot written after the
first free's scan, or the value's last owner parks it for such a slot. A free that was slow between
its scan and its pass over a list, preempted or busy with other lists, then let such an item go on a
scan that showed no slot. For a lent value the release read the records again and parked what a
slot held, so nothing was freed under a reader, but the count went to zero and back while a slot
held the value, and an iterator that upgraded the node's memo in between found it gone and read the
node from the file again. A free now reads the slots again for every item that waits only for
slots, marking the records that hold one as before, and lets go of only those that none holds; the
scan it began with decides only for items still waiting on their epoch. That second reading is one
scan of the records for all such items on the list, where the old code made one for each item its
first scan found held.

A release that revives a value takes it out of the list, counted in `FREEING`, which a flush waits
for once its own pass over the lists is done, and puts it back, reads its slots again and releases
it a second time if none still holds it. It gave up its count in `FREEING` before that second
release. A flush that passed the list while the value was out, such as the flush of the record
whose drop had just cleared the slot, then found nothing left to wait for and returned with the
value back on a list and no slot holding it, until the releasing thread got to it: a store's file
could stay open past the drop of the last iterator that read it. The release now keeps its count
until the second release is done.

The library's unit tests share the process's epoch and records, and it was they that showed both,
with twelve copies of their binary running at once at 16 test threads each: 16 of 1,500 runs
failed. A value that a test retired without lending it, so that only the free's own decision kept
it, was dropped under its slot; a retired count went while the test still held the slot; a parked
root's count read zero; and an iterator over a test map with no store panicked when its upgrade
failed in that window, where a map with a store would have read the node from the file. A build
that logged every release of an item that a slot still held, and every failed upgrade that a retry
then won, found 279 such releases in 1,200 runs, made by other tests' flushes, by stores' closes
and by the test hook that frees every list with a scan made before its callback runs, and each of
the 7 iterator panics in those runs came after an upgrade that won again within 81 yields, at an
address a release had just revived. Built with AddressSanitizer, 21 of 600 runs failed the same
ways, and with ThreadSanitizer 21 of 600, 6 of them in the race of a lent owner's last drop with its
record's drop, which asserted that one of the two had dropped the value by the time both returned.
Neither sanitizer reported a use after free or a data race.

That race still failed under ThreadSanitizer with both fixes, in 4 of 600 runs, and was the tests'
own doing. Logged in a failing run, the owner's drop parked the count, the record's drop returned in
2 µs without a flush, no slot held the value, and the value was dropped 6 ms later: another test's
flush had found the parked item with no slot holding it and taken it between the owner's park and
its own release, which then found nothing left to take. No mark had been set, since every reading
after the park found the slot clear, so neither drop had anything to wait for, and nothing need
wait: the item is off the lists, and the thread that took it drops it. The test now holds a `Weak`
so that the allocation's address stays its own, checks that once both drops have returned no
retired list still holds the value, and then that the value is dropped once. With the record's drop
made to skip its flush, it fails in 7 of 10 runs on its own, where the item stays on the owner's
list. The release that revives a value, two paragraphs up, was found while reading the protocol for
this race, not from a failure.

The second reading still picked what to let go by address (#102). A free gathered the items on a
list that waited only for slots, read the slots for their addresses, and then took from the list
every item waiting only for slots at an address that no slot held. Between that reading and the
pick, another thread could let go of one of those items, the value's allocation be freed and a new
value take its address, and an item of the new value come to wait only for slots, found held by a
free that read a slot written after the first free's reading. The pick then took the new value's
item too, and let go of its count while the slot held the value. A park and a release that revives
a value picked the same way. As under #77, a release that took a lent value's last count read the
records again and parked what a slot held, so nothing was freed under a reader, but the count could
go to zero and back while a slot held the value. `make test-stress` failed once in 300 runs, in
`shared_a_borrow_that_clears_its_slot_after_a_free_marked_it_flushes_at_drop`, which borrows twice:
the allocator gave the second value the first one's address, and another test's flush, which had
read the slots for the first value's item and found none holding it, took the second value's item
from inside the borrow, between its retire and the check that the retired count was kept. Each item
now has a number that no other item on its list has had, and a free, a park and a revive let go of
only the items they read the slots for, picked by number. An item that comes to wait only for slots
afterwards is read for by the free, park or revive that made it so. With a sleep of 300 µs between
the reading and the pick, that test failed in 13 of 300 runs of the library's tests and in 70 of 600
runs of the `shared` and `epoch` tests alone, each time after a log line from a pick that took an
item it had not read for, at that item's address, and in none of 600 runs of the test on its own or
300 runs at one test thread, where no other test frees. With the fix and the same sleep none of 900
and none of 1,200 failed. A unit test runs a flush, a new loan and a retire of the same value
between a free's second reading and its pick, so that the value has a second item, found held, at
an address the free found loose; it failed on every run with the pick by address. Without the
sleep, none of 1,800 runs of the library's tests failed before the fix, nor of 3,000 after it.

### Measurements

Measured on the machine and data of the tables above, alternating the two builds, two runs of two
rounds each, with other work under one core busy in every cell. Operations a second with the 1 GiB
cache and no writer, and in brackets the speedup over one thread:

| Workload | | 1 thread | 2 threads | 4 threads | 8 threads | 16 threads | 32 threads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `get` | before | 3.0M | 6.2M (2.0) | 12.2M (4.1) | 23.6M (7.9) | 43.0M (14.3) | 68.1M (22.6) |
| `get` | after | 3.0M | 6.1M (2.1) | 12.2M (4.1) | 23.5M (7.9) | 43.6M (14.5) | 68.0M (22.7) |
| `iter()` scan | before | 58.8M | 58.8M (1.0) | 114.2M (1.9) | 215.4M (3.7) | 417.9M (7.1) | 888.9M (15.1) |
| `iter()` scan | after | 59.1M | 71.9M (1.2) | 144.4M (2.4) | 278.7M (4.7) | 536.6M (9.1) | 873.7M (14.8) |
| `for_each` scan | before | 88.7M | 156.6M (1.8) | 243.0M (2.7) | 491.0M (5.5) | 957.8M (10.8) | 1459.4M (16.5) |
| `for_each` scan | after | 88.5M | 156.7M (1.8) | 243.9M (2.8) | 492.3M (5.6) | 966.0M (10.9) | 1467.7M (16.6) |
| 100-key range | before | 422k | 817k (1.9) | 1.6M (3.8) | 3.1M (7.3) | 5.2M (12.4) | 7.9M (18.6) |
| 100-key range | after | 423k | 822k (1.9) | 1.6M (3.8) | 3.2M (7.4) | 5.9M (13.8) | 8.6M (20.3) |
| Index lookup | before | 228k | 458k (2.0) | 850k (3.7) | 1.6M (6.8) | 1.9M (8.1) | 2.8M (12.1) |
| Index lookup | after | 234k | 476k (2.0) | 896k (3.8) | 1.7M (7.3) | 2.9M (12.4) | 4.5M (19.4) |
| Index lookup, covered | before | 2.4M | 4.4M (1.8) | 7.1M (2.9) | 11.1M (4.6) | 9.6M (4.0) | 15.0M (6.2) |
| Index lookup, covered | after | 2.5M | 5.0M (2.0) | 8.6M (3.5) | 15.4M (6.2) | 27.5M (11.1) | 37.9M (15.2) |
| Page of 200 rows | before | 180k | 324k (1.8) | 518k (2.9) | 815k (4.5) | 728k (4.0) | 1.0M (5.6) |
| Page of 200 rows | after | 182k | 348k (1.9) | 567k (3.1) | 912k (5.0) | 1.1M (6.0) | 1.5M (8.3) |
| Page of 200 rows, covered | before | 817k | 1.4M (1.7) | 2.1M (2.6) | 3.3M (4.0) | 2.7M (3.4) | 3.4M (4.2) |
| Page of 200 rows, covered | after | 849k | 1.6M (1.9) | 2.7M (3.2) | 4.7M (5.5) | 6.9M (8.2) | 7.4M (8.8) |

With the 16 MiB cache and no writer:

| Workload | | 1 thread | 2 threads | 4 threads | 8 threads | 16 threads | 32 threads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `get` | before | 337k | 613k (1.8) | 1.1M (3.1) | 1.9M (5.5) | 2.1M (6.2) | 2.9M (8.5) |
| `get` | after | 336k | 612k (1.8) | 1.1M (3.2) | 1.8M (5.5) | 2.1M (6.1) | 2.9M (8.5) |
| `iter()` scan | before | 7.5M | 12.2M (1.6) | 21.3M (2.8) | 37.7M (5.0) | 41.8M (5.6) | 70.5M (9.4) |
| `iter()` scan | after | 7.4M | 12.1M (1.6) | 21.1M (2.9) | 37.4M (5.1) | 43.6M (5.9) | 74.5M (10.1) |
| `for_each` scan | before | 7.9M | 12.7M (1.6) | 22.3M (2.8) | 39.2M (5.0) | 42.4M (5.4) | 72.6M (9.2) |
| `for_each` scan | after | 7.9M | 12.7M (1.6) | 22.2M (2.8) | 39.1M (5.0) | 42.1M (5.4) | 71.9M (9.2) |
| 100-key range | before | 54k | 95k (1.7) | 158k (2.9) | 268k (4.9) | 281k (5.2) | 378k (7.0) |
| 100-key range | after | 54k | 94k (1.7) | 156k (2.9) | 263k (4.9) | 272k (5.1) | 371k (6.9) |
| Index lookup | before | 14k | 24k (1.7) | 40k (2.8) | 68k (4.9) | 77k (5.5) | 107k (7.6) |
| Index lookup | after | 14k | 24k (1.7) | 40k (2.8) | 68k (4.9) | 77k (5.5) | 106k (7.6) |
| Index lookup, covered | before | 2.1M | 4.0M (1.9) | 6.7M (3.1) | 10.5M (4.9) | 9.7M (4.6) | 15.2M (7.2) |
| Index lookup, covered | after | 2.2M | 4.4M (2.0) | 7.8M (3.6) | 14.1M (6.4) | 26.1M (12.0) | 35.1M (16.1) |
| Page of 200 rows | before | 138k | 242k (1.8) | 404k (2.9) | 639k (4.6) | 524k (3.8) | 775k (5.6) |
| Page of 200 rows | after | 134k | 239k (1.8) | 402k (3.0) | 652k (4.9) | 570k (4.3) | 815k (6.1) |
| Page of 200 rows, covered | before | 790k | 1.3M (1.6) | 2.0M (2.5) | 3.1M (3.9) | 2.6M (3.3) | 3.6M (4.5) |
| Page of 200 rows, covered | after | 707k | 1.4M (2.0) | 2.4M (3.4) | 4.2M (5.9) | 6.3M (8.9) | 7.1M (10.0) |

At 32 threads with the 1 GiB cache, covered lookups went from 6.2 to 15.2 times one thread, index
lookups from 12.1 to 19.4, covered pages from 4.2 to 8.8 and pages of rows from 5.6 to 8.3, and
`iter()` scans gained 22 to 28% from 2 to 16 threads. Gets and `for_each`, which take no slots, did
not move, and with the 16 MiB cache only the covered reads, whose index the cache holds whole,
moved by more than 6%.

On one thread, medians of three alternating runs of three rounds, operations a second and the
instructions an operation counted by `scaling`:

| Workload | Cache | Before | After | Instructions before | Instructions after |
| --- | --- | ---: | ---: | ---: | ---: |
| `get` | 1 GiB | 3.0M | 3.0M | 1,222 | 1,221 |
| `iter()` scan | 1 GiB | 59.0M | 59.4M | 61 | 62 |
| `for_each` scan | 1 GiB | 88.9M | 89.0M | 29 | 29 |
| 100-key range | 1 GiB | 425k | 426k | 8,610 | 8,876 |
| Index lookup | 1 GiB | 230k | 235k | 28,046 | 27,609 |
| Index lookup, covered | 1 GiB | 2.47M | 2.52M | 5,054 | 5,311 |
| Page of 200 rows | 1 GiB | 184k | 187k | 79,491 | 80,904 |
| Page of 200 rows, covered | 1 GiB | 908k | 879k | 29,845 | 30,450 |
| `get` | 16 MiB | 341k | 343k | 24,171 | 24,251 |
| `iter()` scan | 16 MiB | 7.56M | 7.51M | 1,224 | 1,229 |
| 100-key range | 16 MiB | 56k | 56k | 156,392 | 157,733 |
| Index lookup | 16 MiB | 14k | 14k | 748,427 | 746,003 |
| Index lookup, covered | 16 MiB | 2.14M | 2.19M | 4,995 | 5,251 |
| Page of 200 rows | 16 MiB | 140k | 139k | 88,475 | 91,151 |
| Page of 200 rows, covered | 16 MiB | 904k | 867k | 29,132 | 29,680 |

A short lookup takes and hands back two records and writes a slot for each node it borrows where it
took and dropped counts, about 250 instructions more and fewer locked operations, and runs 2%
faster. A plain lookup also stops upgrading a leaf and a parent for each row and runs 440 fewer. A
covered page passes 25 leaves for 200 cheap rows and runs 3 to 4% slower, the one cell on one
thread that lost.

Beside one thread committing single-key updates as fast as it can, medians of two alternating runs
of two rounds, 32 threads with the 1 GiB cache read 61.2M gets a second against 61.1M, 2.9M index
lookups against 2.6M, 34.9M covered lookups against 15.8M and 7.1M covered pages against 3.4M, and
with the 16 MiB cache 18.2M covered lookups against 12.5M and 4.1M covered pages against 3.1M.
One thread beside the writer ran covered lookups 5% slower with either cache, at 5,624 instructions
against 5,208 with the 1 GiB one, and covered pages 4% and 8% slower. Every commit gives the readers
new roots, which the first iterator on each lends, and when the last snapshot holding an old root
lets go of a root that was lent, its drop reads the records.

In `reads` and `ferro-bench`'s main tool, which now count instructions an item beside the time,
medians of three alternating runs, the heap per key did not move, 56.7 bytes for `u32` values and
108.5 for strings, nor did peak RSS, 378 MiB, the bytes a commit writes or a commit's instructions,
179,659 Buffered against 179,609. Bulk loads, commits, warm and cold gets and diffs stayed within 5%
either way. Scans run one or two more instructions a key, the slot a leaf takes spread over its
keys, and their times at 5 to 28 ns a key moved by up to 9% either way, as did `for_each`, whose
code did not change. Batches through a cursor, `get_many` and `contains_many`, run 12 to 38 more
instructions a key, 3 to 4.5%, at times within the factor of two those cells vary by between runs
of one build, and a table's page of index rows and its group counts run 2.8% and 7% more
instructions and 5% faster.

The fixes after the review, described above, were measured against the build before them, with the
1 GiB cache and no writer, medians of three alternating runs of two rounds. Covered lookups ran
2.51M a second on one thread against 2.52M, 27.8M at 16 threads against 28.4M and 37.6M at 32
against 38.1M, at 5,311 instructions each, and index lookups 4.44M at 32 threads against 4.45M.
Pages of rows ran 1.52M at 32 threads against 1.49M but 83,972 instructions against 81,392: the row
finder takes its leaf and parent out of their fields around each descent, about 13 instructions a
row it does not find in the leaf it holds, and index lookups, which read 20 rows, run 28,135 against
27,612. Covered pages stayed within the swing they show between rounds. One thread, medians of three
runs of three rounds with both caches, moved by 2% or less except covered pages, 2% slower with the
1 GiB cache and 6% with the 16 MiB one, and beside a writer every cell stayed within 1% except
covered pages, 4% and 5% slower. In `reads` and the main tool the heap per key, peak RSS, the bytes
a commit writes and the instructions of a bulk load or a commit did not move, and only row reads
through the finder ran 2 to 3% more instructions.

The fenced clear of a slot, described under [The root and the store](#the-root-and-the-store), was
measured against the build before it with no writer at 1, 8 and 32 threads. Over two alternating
runs of every workload, five rounds each, every cell ran the instructions it ran before, within the
spread that evictions give with the 16 MiB cache. Over six alternating runs of three rounds of
lookups and pages, leaving out cells during which other work kept more than two cores busy, every
cell with either cache ran within 2% of its rate before, except index lookups at 32 threads and
covered pages at 8 with the 16 MiB cache, 4% and 3% slower. With the 1 GiB cache covered lookups
ran 2.48M a second on one thread against 2.52M, 15.7M at 8 threads against 15.5M and 37.4M at 32
against 37.7M, at 5,307 instructions against 5,309, and covered pages 844,000 against 847,000, 4.81M
against 4.88M and 8.99M against 8.79M, at 30,376 instructions against 30,380.

The free that reads the slots again for items waiting only for slots, and the release that keeps
its count in `FREEING` until its second release, described under [The root and the
store](#the-root-and-the-store), were measured against the build before them with the 16 MiB cache
and no writer, where evictions retire lent nodes and frees run, at 1, 8 and 32 threads, medians of
three alternating runs of three rounds. Scans, 100-key ranges, index lookups and covered pages ran
the instructions they ran before, within 0.5%, at rates within 2% either way: at 32 threads scans
ran 74.0M keys a second against 74.7M, ranges 372,000 against 370,000, index lookups 107,900
against 106,900 and covered pages 9.40M against 9.22M.

### The flags' line

A range of 1,000 keys, `snap.range(start..).take(1000).count()` on the compare harness's 1M-key
`Map<u64, String>`, took about 4% longer once iterators borrowed nodes under hazards, 28.1 µs
against 27.0 in the harness, for the same 160 instructions a key (#79). Such a range borrows about
130 nodes: a build that counted its borrows found 129,500 in 1,000 ranges, all through a `Weak`
memo, since a range with no end has no back path to copy into. A probe built the harness's map the
same way, ran its gets and its full scan, and then timed rounds of 1,000 ranges on one pinned core,
counting the thread's cycles and instructions through `perf_event_open`, either one round after
another or each right after a full scan, as the harness runs them. Each tree was copied out with
`git archive` and built in a directory of its own, and the builds alternated. Cycles a range against
the build before the hazards, a643acb, as the median of each run's ratio to the a643acb run beside
it, and the range of those medians over sessions of 10 to 30 runs:

| Build | One round after another | Right after a scan |
| --- | ---: | ---: |
| Hazards, flags behind the value, as at db07c34 | +1.0 to +3.0% | +1.9 to +4.6% |
| The same, writing the slot with a plain store, which is unsound | +0.6 to +1.1% | +2.7% |
| The same, with the asymmetric fence described below | +1.5 to +1.7% | |
| The same, reading neither flag, which is unsound | -2.2% | -1.6% |
| The node's headers moved behind its other fields, beside the flags | -0.6 to +0.1% | +0.6 to +4.3% |
| The same, with the asymmetric fence | -0.6 to -0.2% | +2.4% |
| The flags moved to the front, beside the headers | -0.1 to 0.0% | -0.3 to +0.2% |
| The same, with the asymmetric fence | -0.2% | -1.6% |

The cycles went to the flags' line. The count a frame used to take wrote the strong count at the
front of the allocation, in the line of the node's headers, which the reader goes on to read anyway.
A borrow read `cached` and `lent` 148 bytes into the allocation, in a line that held nothing else a
read wants, and on x86 the exchange that writes the slot completes only once the load of `cached`
before it has, and no load after it is performed before it completes, so the node's headers were
asked for only once the flags' line had arrived: two misses in a row where the count had one. That
cost a borrow about 20 cycles once the ranges had warmed L3 and about 45 right after a scan, when
both lines come from DRAM. Writing the slot with a plain store, which leaves the line to be fetched
but lets the loads after it go ahead, took back more than half of the first and less than half of
the second, and a borrow that kept the exchange but read neither flag ran faster than the count.

`Shared` now holds the strong count, the weak count and the flags in the first 16 bytes of its
allocation and the value after them, as described under [Where the flags
live](#where-the-flags-live), so the flags share a line with the front of the node, where `Node`
keeps the headers of its keys, child links and cells. The exchange then waits for the line the
reader needs next anyway. The other way round, moving the headers behind the node's other fields so
that they shared the line of the flags behind the value, took back the cost of ranges one round
after another but only part of it right after a scan, and slowed bulk loads, as [Measured and left
out](read-path.md#measured-and-left-out) records.

In 15 alternating runs of the compare harness's ferro child, a 1,000-key range took 27.4 µs against
28.2 with the flags behind the value and 27.3 at a643acb. In nine alternating runs of `ferro-bench`,
whose ranges also follow a scan, `map_range_1k_ns` went from 29.7 to 28.8 ns, against 28.9 for
a643acb in another session, and in seven runs of `bulk` the range phase took 27.1 ms against 28.2. A
get did not move: 0.317 µs against 0.316 in the harness and 310.8 ns against 311.2 in `ferro-bench`,
at 1,039 instructions against 1,025, and a table's row get took 157 ns against 155. Bulk loads, the
CPU of the commit after them and the drop of the working tree stayed within 1%, and peak RSS did not
move. In `scaling`, two alternating runs of three rounds with no writer at 1, 8 and 32 threads, no
cell with either cache ran more than 0.4% slower than with the flags behind the value. With the 1
GiB cache gets ran 3 to 4% faster, index lookups 3 to 5%, `for_each` scans 3 to 6% and covered pages
at 32 threads 5%, at 9.4M a second, and covered lookups at 32 threads ran 37.6M a second against
37.1M and pages of rows 1.52M against 1.50M.

### Measured and left out

- A counted store in iterators, cursors and the row finder, in place of the store's slot, as the
  fix for a store cloned back to life. Medians of two alternating runs of two rounds with the
  1 GiB cache: one thread did not move, but covered lookups fell from 27.6M a second to 14.5M at
  16 threads and from 37.4M to 18.2M at 32, covered pages from 6.7M to 5.5M and from 8.2M to
  6.8M, and index lookups, pages of rows and 100-key ranges lost 2 to 4% at 16 and 32 threads.
  Each iterator and finder a lookup builds then took and dropped a count on the store's line,
  which every reader writes.
- Hazards for the root and the store alone, with the nodes below the pinned levels counted as
  before. One thread ran covered pages at 910,000 a second against 901,000 before and 885,000 with
  every hazard, medians of four alternating runs, but 32 threads ran them at 3.5M against 5.2M with
  every hazard, covered lookups at 28.3M against 35.5M and pages of rows at 1.23M against 1.39M.
- An epoch lease per iterator, a slot holding the epoch it started at and renewed every few dozen
  leaves with counts taken at the boundary, in place of a slot per node. Not built. A build that
  wrote the slots with a plain store, which is unsound and costs a leaf what a lease would, ran
  covered pages 1% faster on one thread than the exchange, 883,000 a second against 876,000. A lease
  held by an iterator that sits unused between calls would keep every thread's retired nodes from
  being freed and make `Store::close` wait out its second, where a record holds back eight nodes at
  most.
- Sixteen slots chosen by depth below the pinned levels and cleared one by one: a covered lookup on
  one thread ran 5,597 instructions against 5,304 with eight slots chosen by level, a covered page
  32,000 against 30,436 and a 100-key range 9,082 against 8,861.
- Writing the slot before reading `cached`, so that the exchange need not wait on the flag line: no
  faster, 869,000 covered pages a second on one thread against 866,000.
- More prefetching. A node's flag line is prefetched with its hint, which took covered pages on one
  thread from 860,000 a second to 874,000. Prefetching the node's first line as well made them
  853,000, and asking for the next child's flag line when an interior frame hands out the key
  before it put the request in every step and cost a covered page 4,000 instructions and 19%.
- The weak count and both flags in the allocation's first line beside the strong count, so that a
  borrow reads no line a walk does not: no faster, 858,000 covered pages a second on one thread
  against 879,000. Measured later on ranges, whose nodes come from DRAM rather than L3, it took
  back what the flags' own line cost them and became the layout, as [The flags'
  line](#the-flags-line) describes.
- Records aligned to 64 bytes rather than 128: within noise in one session, 5.7M covered pages a
  second at 32 threads against 5.9M. The records keep 128, which keeps two of them off the pair of
  lines the adjacent-line prefetcher fetches together, as the epoch's slots do.
- Counts striped per thread for the root and the store. Not built: a stripe would still be a locked
  operation for every iterator, its count would have to follow the iterator to whichever thread
  drops it, and a store's file closes when its count reaches zero, which a stripe holding a count
  for its thread would delay. The probe without these counts bounds what striping could gain, and
  the hazards come within a sixth of it for covered lookups, 37.9M a second at 32 threads against
  44.7M in another session.
- A fence in every drop of a record, whatever its slots hold, in place of the fence in
  `epoch::retract`, as the fix for a slot cleared early. Where a slot holds the store it changes
  nothing, but the iterators, cursors and row finders of a map without a store set no slot and pay
  it on every drop. In a 1,000-key in-memory map, medians of six alternating runs of the best of
  five, a cursor's seek took 36.7 ns against 36.0 before and 36.2 with the fence in `retract`, and a
  two-key range 68.5 ns against 68.9 and 69.5, differences the size of what moving the code around
  gives. The fence stayed in `retract`, which runs only when a borrow clears its slot.
- `epoch::retract` out of line and marked cold. The code around each inlined borrow changed: a
  100-key range ran 8,972 instructions against 8,928, a covered lookup 5,328 against 5,309 and a
  two-key range of an in-memory map 1,383 against 1,371. Inlined, they run what they ran before.
- An asymmetric fence, as folly's hazard pointers use, in place of the exchange that writes a slot.
  The two borrows that write a slot and then read `cached`, through a `Weak` memo and the back
  path's copy, wrote it with a release store and a compiler fence, and a free called
  `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED)`, registered once when the first record was taken,
  between its bump of the epoch and its reading of the slots. Where registration failed, borrows
  kept the exchange and frees made no call. The call acts as a sequentially consistent fence run on
  every other thread of the process at some point while it runs, and the compiler fence keeps the
  borrow's store before its read in the code the thread runs. If that fence falls after the store,
  it orders the store before the free's reading of the slots, which sees it. If it falls before, it
  comes after the free's bump, which follows the read of the epoch that retired the node and so the
  cache's clear of `cached` before it, and the borrow's read of `cached`, which follows the fence,
  sees the clear and gives the slot back. A slot written through a count or under a pin is ordered
  by the count's release or the unpin and needs neither, and a free's later readings meet only items
  that a reading after such a call found held, or that an owner parked, so they need no call of
  their own. With the flags behind the value it took a range's cycles from 1.5 to 1.7% above
  a643acb's where the exchange left them 2.5 to 2.7% above, one round after another, and with the
  headers beside those flags it changed them by 0.4% or less. With the flags at the front it left
  them where they were one round after another and took 1.3% off them right after a scan, in 14 of
  20 runs. Once the exchange waits on nothing the reader does not need next, there is little left to
  take away, and the call would interrupt every core running a thread of the process in every batch
  a free makes. Miri and ThreadSanitizer cannot model it, so they would check only the fallback,
  leaving the stress tests and AddressSanitizer, and those cannot see the call go missing: a build
  that wrote the slot with a plain store and no barrier at all, which is unsound, passed 300 runs of
  `make test-stress`, `epoch_stress` under AddressSanitizer with four seeds, and the unit tests
  under Miri's stacked borrows over 32 schedules.
- Fewer slot writes, such as keeping one slot across the leaves of a parent and writing the next
  leaf's only at a parent's boundary. A leaf is evicted on its own, whatever its parent, so every
  borrow has to read its own node's `cached` after its slot is written, and that line was the
  cost; a slot write saves at most what the plain store above saves. Not built.
- Counts in place of slots while only one thread reads. A build that counted every node below the
  pinned levels, as before the hazards, ran the ranges above at a643acb's cycles, within the 1%
  its runs varied by, which is what a fast path could reach, and the flags at the front reach it
  too. Knowing that only one thread reads would take a count of readers that every
  iterator writes, the shared line the hazards took away. Not built.

### What is left

With the flags at the front a borrowed node costs a range on one thread what the count did, as [The
flags' line](#the-flags-line) describes. Before that a slot cost a borrowed leaf about 1.5 ns more
than the count, which covered pages, 25 leaves for 200 cheap rows, showed as 2 to 5% across
alternating runs, and they have not been run against a counted build since. Covered pages at 16 and
32 threads also move between about 5M and 10M a second from one round of a process to the next,
where the counted build was steady at its lower rate. Each round reopens the files and starts new
threads, so the rounds differ in where each thread's nodes and allocations land. With mimalloc as
the global allocator a covered page pulled 13 lines from the local CCX and 3.3 from the other
against 25 and 7.5, and the rounds still split. A `Val` for a row still clones its leaf's column,
which the third probe puts at more than half of what pages of rows could gain. `for_each` still
counts, and so do nodes below the pinned levels that sit above a leaf's grandparent.
`Space::retired_bytes` counts an old file only while a handle counts its store, so a file that only
an iterator still reads is left out of it. With a cache smaller than the data the allocator decides,
as before. [Rows lent in place](#rows-lent-in-place) takes up the `Val`.

## Rows lent in place

Each row an index lookup yields is a `Val`, and for a row with a destructor that is a share of the
column of the node that holds it: an `Arc` increment when the row finder hands the row out and a
decrement when the caller drops it, on the column's first line, which every thread reading the same
rows writes. A page of the same 200 rows passes about 50 row leaves, four rows to a leaf, and with
the 1 GiB cache it pulled about 160 lines a page from the local CCX and 15 from the other at 16
threads, where one thread pulled none, and ran 8 times one thread at 32. The third probe under
[Iterators under hazards](#iterators-under-hazards), which leaked that count, took pages from 1.6M
to 3.6M a second at 32 threads.

### Lending

`IndexIter::next_ref` steps to the next row and lends `&T`, and `IndexIter::visit` lends every row
left to a closure; `Matches` has the same pair. The row finder's `lend` finds the row as `get` does,
but where `get` makes a `Val` of it, `lend` notes whether the node it found the row in is the leaf
or the parent it keeps, which its hazard slots hold, or a count when it got no record, and lends the
row from there. The reference borrows the lookup mutably, so neither the lookup nor its finder can
move while it lives, and the finder lets go of a leaf only at a later lookup. A row found in a node
above the parent, about one row in 64, is read as a `Val` and kept in the finder until its next
lookup, and so is a row stored on its own. `get` takes the same descent, made generic over what it
makes of the row it finds, so a lookup through `next` builds its `Val` where it did and runs the
instructions it ran before, within 0.4%.

`Iter` and `Values` have the same pair, so `Table::iter`, `range` and `prefix` and a map's iterators
lend too. `next_ref` lends the entry from the frame that holds it and keeps a value stored on its
own in the iterator, an `Arc` of 8 bytes, until the next step. `visit` takes a run of a node's
entries at a time, as `fold` does.

A lent row is a borrow of a node that the lookup or iterator already holds, by a slot, an anchor or
a count, for as long as the reference can live, so no step of the protocol changed and nothing new
is unsafe. The borrow checker ends the reference at the iterator's next step, which is the first
point at which the iterator may let the node go.

### How it is checked

The unit test `epoch_lent_entries_outlive_the_eviction_of_their_nodes` lends an entry of a range, a
value of another and rows that a finder found in its leaf, in its parent and in a node above them,
evicts and flushes every node they came from, reads the lent references afterwards, and checks that
the nodes go once the iterators and finders do. `tests/reads.rs` compares `next_ref` and `visit`
with the `Val` forms, mixed with `next` and `next_back`, over maps with spilled values in memory,
stored, edited and reopened read-only, and over a table whose notes spill, through its lookups, its
`find()` lookups and its ranges, and past an index entry whose row is gone. In `epoch_stress` the
readers read rows, value ranges and nested maps, most of whose values spill, lent both ways, and
keep lent walks of values, rows and nested maps from one round to the next, against the model while
the caches evict and the handles reopen.

The unit tests passed under Miri with stacked borrows and with tree borrows over 32 schedules each,
the library's tests, `tests/reads.rs` and `epoch_stress` with its default seed and three others
under AddressSanitizer and ThreadSanitizer, 600 runs of the library's tests built with each
sanitizer, twelve at a time, and 1,200 runs of `make test-stress`. With `Shared` and `Weak` made to
write nothing to the slot they lend through, Miri reports the new unit test's read of a lent value
as a dangling reference on each of four schedules, and `epoch_stress` crashes in a release build.
With the finder made to keep its old leaf when a lookup finds a new one, whose slot the new leaf
then holds, `epoch_stress` and two tests of `tests/reads.rs` fail on an index past the end of the
leaf a row is lent from. With `Shared::release` made to report no loan, the new unit test still
passes, since a lent node's last owner parks it while a slot holds it.

### Measurements

Measured on the machine and data of the tables above, with nothing else running, in one pass of five
rounds of every workload with no writer; the `next` rows are the same workloads as `index` and
`tier` above. Operations a second with the 1 GiB cache, and in brackets the speedup over one thread
of the same form:

| Workload | Read by | 1 thread | 2 threads | 4 threads | 8 threads | 16 threads | 32 threads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Index lookup | `next` | 236k | 475k (2.0) | 904k (3.8) | 1.7M (7.3) | 2.9M (12.3) | 4.5M (19.2) |
| Index lookup | `visit` | 240k | 489k (2.0) | 923k (3.9) | 1.8M (7.4) | 3.5M (14.4) | 5.1M (21.4) |
| Page of 200 rows | `next` | 182k | 348k (1.9) | 581k (3.2) | 947k (5.2) | 1.1M (5.9) | 1.5M (8.3) |
| Page of 200 rows | `next_ref` | 305k | 605k (2.0) | 1.0M (3.4) | 1.9M (6.3) | 3.4M (11.1) | 4.1M (13.3) |

With the 16 MiB cache:

| Workload | Read by | 1 thread | 2 threads | 4 threads | 8 threads | 16 threads | 32 threads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Index lookup | `next` | 14k | 24k (1.7) | 40k (2.9) | 70k (5.0) | 76k (5.5) | 109k (7.8) |
| Index lookup | `visit` | 14k | 24k (1.7) | 40k (2.9) | 70k (5.0) | 76k (5.5) | 109k (7.8) |
| Page of 200 rows | `next` | 130k | 229k (1.8) | 386k (3.0) | 619k (4.7) | 529k (4.1) | 791k (6.1) |
| Page of 200 rows | `next_ref` | 181k | 311k (1.7) | 506k (2.8) | 792k (4.4) | 685k (3.8) | 921k (5.1) |

On one thread with the 1 GiB cache a lent page runs 77,080 instructions and 16,150 cycles, against
83,740 and 27,190 through `next`, which pays for each row the column's locked increment and
decrement and the 120-byte `Val` the caller moves and drops. At 16 threads a lent page pulls 7.6
lines from the local CCX and 3.5 from the other, against 157 and 15, and takes 18,940 cycles, 17%
more than on one thread, where a page through `next` takes 68,140, two and a half times as many. At
32 threads, two to a core, it takes 29,350 a thread, so the second thread of a core adds a tenth to
a page that runs 4.8 instructions a cycle alone.

What a thread does a second falls further than its cycles rise, because the clock falls with the
cores kept busy: rate times cycles gives 4.93 GHz for a lent page on one thread, 3.97 GHz at 8
threads, 4.01 at 16 and 3.72 at 32, where pages through `next`, which spend their cycles waiting on
lines, kept the clock at 4.3 to 4.6 GHz. So 13.3 times one thread at 32 is 16 cores at three
quarters of the clock and a tenth more for the second threads, and what contention still costs a
lent page is the 17% more cycles it takes at 16 threads than on one. Index lookups, whose 20
scattered rows each take a descent of their own, read 1 to 2% faster on one thread, 18% faster at 16
and 13% at 32, where lines from the other CCX fell from 1.9 a lookup to 0.5. With the 16 MiB cache
pages read 39% faster on one thread and 16% at 32, and index lookups, which the allocator decides as
[The allocator](#the-allocator) describes, did not move.

Run alternately with the build before the change, three runs of two rounds of the existing workloads
at 1, 8, 16 and 32 threads with both caches, every cell of `get`, scans, `for_each`, ranges and
plain index lookups ran within 2% of its rate before, at the instructions it ran before within 0.5%.
Pages of rows through `next` ran within 1% on one thread, at 0.8% more instructions in those runs
and the same in the pass above, and within the 4% their runs spread by on more threads. Covered
lookups at 16 and 32 threads with the 1 GiB cache fall into the two modes described under [Iterator
frames](#iterator-frames), about 28M and 31M a second at 16 threads with 3.8 and 3.0 lines a lookup
from the local CCX, and in four more alternating runs of them and of covered pages the new build ran
4 to 5% faster at 16 and 32 threads and covered pages 2 to 3%, all within the spread of their runs.
In `reads`, three alternating runs, no item ran more than 0.5% more instructions, and times moved by
up to 5% either way, apart from the batches over every 256th key, which vary by a factor of two
between runs of one build. In the main tool, eight alternating runs, every step ran the instructions
it ran before, within 0.3%, and no time ran more than 2% slower except a warm get, 315 ns against
308 at the same 1,038.5 instructions. A copy of the new tree built in another directory took 315 ns
too, but the get's code did not change, `reads` ran random gets within 1% and `scaling` ran gets on
one thread within 0.3%, so it is where the get's code landed in that binary, as under [Where the
flags live](#where-the-flags-live).

### Measured and left out

- Closures alone, with `try_visit` taking a `ControlFlow` to stop, on `IndexIter`, `Matches`, `Iter`
  and `Values`. In one session, built side by side, pages through `try_visit` ran 79,700
  instructions and 291,000 a second on one thread and 3.87M at 32, against 76,860, 305,000 and 4.09M
  through a loop over `next_ref`. `next_ref` lets a page stop, return or use `?` as plain control
  flow, and `try_visit` added nothing a loop over it does not do, so it was dropped; `visit` stays
  for a closure over every row, and for an iterator's runs.
- A guard type, a `Val` borrowed for one step that takes the count only when `to_val` keeps it. Not
  built: it lends what `next_ref` lends, and `next` already hands out the `Val` of a row that is
  kept.
- A third slot in the row finder for the node above its parent in which one row in 64 sits. A probe
  that cloned those rows instead of counting them ran pages 2% faster at 16 threads and no faster at
  32, pulling three fewer lines a page from the local CCX and one fewer from the other, and 3%
  slower on one thread for the clones. Not built.
- Ten or twelve slots a record, so that an index iterator borrows the level below the pinned ones,
  which it counts with eight. Across two sessions, alternating builds, 100-key ranges ran 2 to 5%
  faster at 16 and 32 threads, and lines a range from the other CCX fell from 2.2 to 1.2. Covered
  pages ran up to 12% faster, within what their rounds swing by as [What is left](#what-is-left-1)
  describes, pages through `next_ref` within the swing of their rounds, and covered lookups 7%
  slower in one session and the same in the other. Left out until a run can tell them apart.
- A fourth pinned level, as a probe of the same counted nodes: pages through `next_ref` 3 to 4%
  faster at 16 threads and covered pages 8 to 11%, at eight times the nodes a map pins outside the
  cache's budget. Not built.
- A snapshot every 256 pages rather than every 16: no faster, so the two counts a snapshot's clone
  and drop write are not what the pages wait on.
- Striped, deferred or biased counts for columns. Not built. The rows are read by every thread, so a
  count striped per thread still has to follow a `Val` to the thread that drops it and be summed to
  know when the column can go, deferring the decrements leaves the increments on the shared line,
  and biased counting makes only the decoding thread's counts cheap, which is not a reader. The
  lease per iterator measured under [Measured and left out](#measured-and-left-out-1) cost one
  thread 10 to 14%. Lending takes the count away instead.

### What is left

A lent row is gone at the next step, so a row that is kept, sent or returned still comes through
`next` as a `Val` and pays the count, and so does a lent row found above the row finder's parent. An
index iterator still counts the level of the index below the pinned ones, and the extra slots that
would borrow it are left out above. A lent page now runs at what the clock and the second threads of
the cores allow, so taking more from it on this machine means fewer instructions a page, which helps
one thread as much as 32. Covered pages read no `Val` and moved by no more than their rounds swing
by, which they still do.

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

Outside the bound are pinned nodes, including those the maps of a cached root pin, the node
allocations memos keep, the column of any `Val` still held, nodes held by live snapshots and dirty
trees, evicted nodes a guarded read may still be using, which wait in batches of up to 128 nodes or
64 KiB a thread, and on a writer the `dedupe` table, 32 bytes for every 4 KiB of the setting,
rounded up to a power of two sets, about 0.8 to 1.6% of it and at most 2 MiB. A history walk that
reads sixteen maps in every version peaks at 1.2 to 1.4 times the setting, mostly from those pins.

The default rose from 256 MiB to 1 GiB, which is about what the old setting held once full. A
setting carried over from before that holds about a quarter of what it did. `map_get_cold_ns` is 5.4
µs at 1 MiB and 3.8 µs at 5 MiB, against 7.4 and 5.2 µs with one `Arc` per value, because the same
bytes now hold twice the keys.
