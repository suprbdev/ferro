# The read path

How a get, a scan, a range and a cursor find their entries, and what each costs. Reads from many
threads at once, the epoch scheme and the node cache's weight are in [Reading from many
threads](read-scaling.md).

## Where a get spends its time

A warm get at 1M keys walks about seven levels, and nearly all of its time is CPU cache misses, not
logic. One node cache lookup per level was 55% of the get. Reference counts on nodes near the root,
written by every reader, held eight readers to about 10M reads a second. At 10,000 keys, which fit
in L2, a scan costs 2 ns a key and a get 67 ns. At 1M keys the same code costs 6 to 8 ns and 240 to
340 ns, so memory, not instructions, is what the read path has to save.

## Memos

So each `Link::Stored` carries a write-once memo of the node it points at, in a `OnceLock`:

- The first `PIN_DEPTH` (3) levels of a tree memoize a strong count. `Map::walk` follows those by
  reference and writes no shared memory, which took eight readers from 9M to 60M reads a second. A
  fourth level added nothing. A `Map` pins about 73 nodes whatever its size, outside `cache_bytes`,
  until the `Map` is dropped.
- Deeper links memoize a `Weak` when the child was found in the cache, or when `flush` wrote it
  beside a sibling, as described below. The cache stays the owner, so eviction still frees the node,
  and the memo keeps only the node's allocation, about 150 bytes, until its parent goes. A
  child just read from disk is not memoized, so a one-off read leaves nothing behind. A get reads
  through these memos without upgrading them, under an epoch guard, as described under [Reading from
  many threads](read-scaling.md#guarded-gets-and-the-epoch-scheme).
- A memo is never replaced. Once its child is evicted the link falls back to the cache, whose `get`
  restores the recency that memo hits skip, so eviction keeps seeing nodes that are really in use. A
  replaceable `ArcSwap` memo made cold gets 44% slower, because a small cache evicts constantly and
  each `ArcSwap` store has to settle with every reader thread.

A memo holds only what a load for that link's own `Ptr` returned, and the cache is keyed by the full
hash, so a damaged link still fails its hash check whatever else is cached. `Link::clone` starts
with an empty memo, since copying one touches the child's header, a cache miss per link that slowed
`diff` by 45%. Only `Map::clone` keeps its root's memo.

## Node layout

Leaves are 7 in 8 nodes, and their `children` vector is empty rather than a row of `Link::Empty`.
`Node::child` reads through that and `Node::children_mut` fills it in when a leaf gains a child.
This cut peak RSS by 28% and bulk load time by 4 to 14%. `Node` is `repr(C)` with `keys` and
`children` first, so both pointers a walk follows sit at the front of its allocation, which took
12% off `contains_key` and 6% off `get`.

A node is up to four allocations. The `Shared` that holds it takes 152 bytes: the strong count, the
weak count and the two flags described under [Where the flags
live](read-scaling.md#where-the-flags-live), then the 136 bytes of `Node`, which are the headers of
its key vector, its child links and its cells, its count, level, origin and prefix. Its keys, its
column or slots, and an interior node's child links, 72 bytes each, are allocations of their own.
glibc aligns each to 16 bytes, not to a cache line, so the flags and the headers a get reads in an
interior node take one line one time in four and two otherwise, and in a leaf two, or three one time
in four. With the flags behind the value, as they were until [The flags'
line](read-scaling.md#the-flags-line), the headers took as many lines in an interior node and two in
a leaf, and the flags a line of their own. A leaf of eight `u64` keys and short strings takes a
160-byte chunk for the node, 80 for its keys and 224 for its column, besides the strings.

What a get reads of that was counted by a test that replays 200,000 random gets on the 1M-key
`Map<u64, String>` of the compare harness, right after its load, and records every line each one
touches. A get reaches a node with a chance in proportion to the keys under it, so the nodes it
reads hold 12 to 15 keys where the average node holds 8, and a third of them more than 16. Lines a
get read in each node with the flags behind the value:

| Node | Headers | Flags | Keys | Link, or column and value | All | Pages | Hint prefetches, of them read |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Pinned, the first three levels | 1.0 to 1.7 | | 1.0 to 2.0 | 1.0 to 1.5 | 3.0 to 5.2 | 2.0 to 2.2 | |
| Interior, below the pins | 1.7 to 1.8 | 1 | 1.9 | 1.1 to 1.3 | 5.9 to 6.0 | 2.0 to 2.3 | 14.4 to 15.0, 3.1 |
| Leaf | 2.0 | 1 | 2.0 | 2.3 | 7.0 | 2.0 | 9.7, 4.4 |

A get reads 38 lines on 13 pages in all, and the node cache almost never, since nearly every link
below the pins carries a memo: 23 of the 200,000 reached a leaf through it. Its hardware counters on
one core of the machine described in [Performance](../guide/performance.md), medians of five runs
over the first two passes after the load, per get: 1,600 cycles and 1,035 instructions; 12 L1
misses on demand, 6.2 of them served from L2, 4.5 from L3 and 1.5 from DRAM; about 53 lines brought
in by the hints' prefetches, 15 from L2, 30 from L3 and 7 from DRAM; 13 to 16 lines from DRAM in
all, the rest of them by the hardware prefetcher; 8.5 L1 TLB misses, of which 3.9 walk the page
table; and about 9 misses in flight on average. The lines of one node are asked for together, so
whatever their number a get waits on one miss a level: the leaf's, nearly always from DRAM, and its
parent's, often, each behind a page walk. Two layouts that read fewer lines were built, measured
and left out for that reason, as described under [Measured and left out](#measured-and-left-out).
An iterator was another matter: the exchange that writes its hazard slot waited for the flags'
line before the headers' line was asked for, so a borrow's lines did not arrive together until the
flags moved to the front of the allocation, beside the headers.

## Prefetching the next level

Memos took the cache lookups out of a get and left the misses. Below the pinned levels a get still
waited, level after level, for the link, then the node header that `Weak::upgrade` reads, then the
key array behind that header, and then the link its search picked. A `Weak` memo now also carries
a `Hint`: where its node keeps its keys and, for an interior node, its child links, or for a leaf
its value column, as two 32 bit offsets from the node with a count of cache lines in their low
four bits. `reach` prefetches those lines before it upgrades, so the header, the keys and the
links of the next level arrive together, and a get waits about once a level instead of three
times. The offsets fit beside the `Weak` in space the enum already had, so `Memo` is still 16
bytes and `Link` 72. A hint comes from a live node, and once that node is evicted a stale hint
prefetches a line nobody wants and nothing more, since a prefetch never faults. That call, the
guarded borrow described under [Reading from many
threads](read-scaling.md#guarded-gets-and-the-epoch-scheme), `Shared`, the reference count that
holds nodes, described under [Where the flags live](read-scaling.md#where-the-flags-live), the
tagged pointers of iterator frames, described under [Iterator
frames](read-scaling.md#iterator-frames), and the hazard slots iterators borrow nodes under,
described under [Iterators under hazards](read-scaling.md#iterators-under-hazards), are the tree's
only `unsafe` code, and the prefetch does nothing on targets other than x86_64.

On 1M `u64` keys, median of 15 alternating rounds in one process holding two such maps, in
nanoseconds, the last two per key:

| Memo holds | `get`, string value | `contains_key` | `get`, then read it | `get`, `u64` value | Scan | 1000-key range |
| --- | --- | --- | --- | --- | --- | --- |
| The node | 549 | 471 | 642 | 578 | 33 | 48 |
| The node and a hint | 325 | 276 | 334 | 293 | 22 | 35 |

The price is a few dozen instructions a level and lines fetched that a read may not use. Gets on
10,000 keys, which stay in L2, were 6 to 12% slower until the linear search below won that back, and
walking an index's keys alone, at about 6 ns a key, is 7 to 15% slower.

Measured and left out:

- Prefetching from a node once it is reached, out of its header: every line of its links and its
  column took L1 misses from 24 to 112 a get and gained nothing, and a leaf's column alone gained at
  most 3%. The lines have to be asked for before the header arrives.
- Fewer lines. A hint of the keys alone, which fits in one word, was 1 to 6% slower than keys and
  column. Child links only for nodes of level 1, whose children are leaves, were 7 to 11% slower
  than for every interior node, and no child links at all a further 11 to 14%. The column's header
  line alone, without its values, was 13% slower for `u64` gets and 21% for gets that read a string.
- `_MM_HINT_T1`, which stops in L2: 1 to 20% slower at 1M keys and 4% at 10,000.
- Full addresses in place of offsets: no faster, and `Link` grew from 72 to 80 bytes.
- Pinning every level, which is unbounded and was only a probe: with hints it was no faster than
  `Weak` memos, so their reference counts are no longer what a get waits on, and `PIN_DEPTH` stays.

A node of at most 16 keys, each at most 16 bytes with no destructor, is searched by counting the
keys below the target instead of by binary search, which leaves no chain of dependent loads. That
took 7 to 11% off gets and `contains_key` at 10,000 keys and up to 5% at 1M. Keys that own heap keep
the binary search, since every comparison would chase a pointer.

`flush` leaves every node it writes in the cache, but it used to hand back links with empty memos,
so the first read of each node after a bulk load went through a cache lookup: the first pass of gets
after loading 1M keys took 460 ns against 365 ns warm. A link it writes now gets a `Weak` memo and
its hint when the node sits below the pinned levels and its parent rewrote more than one child, and
that first pass takes 335 ns. A single-key commit rewrites one child a level and memoizes nothing.
Memoizing every written node grew the live heap of `heap`'s commits and reads case by 4%, from 46.5
to 48.4 MiB with a 64 MiB cache, because the old paths of single-key commits kept the `Arc`
allocations of their evicted children. With the rule it is 46.7 MiB.

## Nodes a commit changes

A node that a closure reads from the file to change it, on the path of a key it inserts, updates
or removes, or in a bulk edit or a split, is not cached. The closure holds its only count and
changes it in place, and `flush` caches the node written in its place. Caching it, as such a read
did before, cost an insert that evicted another entry once the cache's cold section was full, and a
copy of the node as the closure changed it, since the cache held a count of it, for a version that
only snapshots older than the commit can still reach. A node found in the cache is handed out as
before and copied as it is changed. It matters most to writers in several processes, which read
back the nodes the others rewrote, as described under [Reading back what other processes
wrote](processes.md#reading-back-what-other-processes-wrote), where it gave two, four and eight
processes on one branch 7.3% more commits a second, 35,322, 28,499 and 23,787 against 32,921,
26,557 and 22,162. On one process `ferro-bench`'s single-key Buffered map commit ran 0.9% fewer
instructions, 178,139 against 179,743, and took 23.2 µs against 23.5, median of three alternating
rounds. The unit test `the_nodes_a_commit_reads_from_the_file_to_change_stay_out_of_the_cache`
checks that a snapshot from before a commit reads the path it changed from the file, one after it
reads the new path from the cache, and a node that a later commit finds in the cache and changes
stays there for that snapshot.

## One loop for borrowed nodes

`Map::walk` serves `get`, `contains_key`, `value_hash` and `nth`. It used to reach every node
through `reach`, which the compiler kept out of line and which handed back a `Result<Reach>`
through memory, and it pinned the epoch before the root. Each step now goes through `descend`,
which is always inlined: it takes a dirty node, a pinned memo or a `Weak` memo it can borrow
straight to a reference and leaves anything else to `reach`, which counts nodes and reads from the
file. The walk is one loop around one call of its visitor, and it pins the epoch only at the first
`Weak` memo, through a `OnceCell`, so a walk through a dirty map, or through a stored map no deeper
than the pinned levels, takes no pin at all.

Measured on 2026-11-05, alternating builds before and after on one CCD of the machine described in
[Performance](../guide/performance.md), on 1M `u64` keys with short string values unless a row says
otherwise:

| Read | Instructions before | Instructions after | Time before | Time after |
| --- | ---: | ---: | ---: | ---: |
| `get`, the compare harness's first pass after its load, seven runs | | | 0.33 µs | 0.32 µs |
| `get` in the main `ferro-bench`, its second pass, five runs | 1,205 | 1,025 | 320 ns | 312 ns |
| A table's row get in the main `ferro-bench`, 200,000 rows | 974 | 829 | 163 ns | 161 ns |
| `get` in `reads`, a reopened handle, three runs | 1,188 | 1,008 | 345 ns | 336 ns |
| `get` in `reads`, `u64` values | 1,170 | 989 | 265 ns | 261 ns |
| `get` in `reads`, `()` values | 1,162 | 1,020 | 221 ns | 219 ns |
| `contains_key` in `reads` | 1,141 | 976 | 297 ns | 290 ns |

A get stays bound by memory, not instructions: it ran about 0.7 instructions a cycle before and
after. Below the three pinned levels it reaches four nodes, each of whose header and flags, keys
and links or column arrive together but only once the link above them has been read, so the get
waits on four misses in a row. A leaf that a get touches takes about eight lines, its header and
flags, its keys and its column, so the 125,000 leaves of 1M keys want about 64 MB against the 32
MB of L3 on one CCD, and most leaf reads go to DRAM, about 140 ns on this machine against 10 ns for
L3.
Each of those misses also walks the page table: the process gets no transparent huge pages from
glibc's heap by default, and with `GLIBC_TUNABLES=glibc.malloc.hugetlb=1` gets ran 8 to 12% faster,
as [Performance](../guide/performance.md#huge-pages) describes. Fewer lines a node do not shorten
that wait: a node in one allocation, with its keys inline beside its headers and flags, read a
fifth fewer lines and ran gets of short strings 2 to 7% faster, for slower loads, ranges and scans
and more heap, as [Node layout](#node-layout) describes. Getting closer to redb's 0.27 µs would
take fewer levels, which means a wider fanout and a change of the file format, or fewer page walks,
which huge pages give.

Measured and left out:

- Three loops, one through dirty nodes and pinned memos before the pin, one through borrowed
  nodes under it and the general one: 1,142 instructions a get on 1M keys, and 930 for
  `contains_key` against 976 now, but each loop inlined the visitor, and for a table's rows the
  compiler then gave up inlining what the visitor calls, so a row get ran 998 instructions
  against 974 before.
- A loop for borrowed nodes beside the general one, each calling the visitor: 1,157 instructions a
  get.

## Values

A node holds its values in one `Arc<[V]>` column in key order, beside its keys. There used to be one
`Arc<V>` per value behind a 40 byte `Slot`, which is 72 bytes and a separate allocation for each
`u32`, and even for the `()` of a `Set` or an index entry. A `Slot` now exists only for what a
column cannot say: `Fresh` for a value changed since it was read, `Spilled` for a value in its own
object, and `Packed` for an unchanged value in a node that has one of those. `Cells::slots` turns a
column into slots at the first edit of that node's values, which takes one reference count per value
and copies none. A node on the path to an edit only gains a new child, so it keeps its column, and
copying it costs one reference count where it used to cost one per value. That is why commits got
cheaper: 34% for small values at 1M keys and 18% for short strings.

A `Slot` takes 24 bytes. `Spilled` used to hold its `Ptr`, 40 bytes, and its summary inline, which
made every slot 48 bytes though `Fresh` needs 8 and `Packed` 20. Spilled values are rare, so their
slot now holds an `Arc<(Ptr, S)>`, which a copy of the node clones with a reference count, and a
node's weight counts that allocation. A dirty leaf of a bulk load held 384 or 768 bytes of slots for
eight keys, every insert shifts them, and 1M per-key inserts of short strings ran 5 to 6% faster and
peaked 36 MiB lower, as [Inserts and prefetch hints](tree.md#inserts-and-prefetch-hints) describes.
The change also tipped the compiler out of inlining `Node::value` into `Iter::next` for a
`Map<u64, ()>`, whose `for` loop in `reads` went from 140 to 156 instructions a key and from 10.5 to
13.4 ns. `Iter::next` now reads a column's value itself and leaves slots to `Node::value`, which
brought it back to 141, with the loops over the other value types unchanged. That loop still took
12.4 ns against 9.8 in `reads`, but the same loop in a function of its own runs 140 instructions in
9.5 ns with both builds, so what is left is where it lands inside `reads`' large `probe_map`.

`flush` leaves a column behind for every node it writes, putting the values of a changed node into
key order as they decode from the written bytes. So a process reads what it wrote exactly as it
reads what it opened, a `#[ferro(skip)]` field included, and no `Fresh` slot outlives its commit.
The price is a clone, and later a free, per value in a bulk load.

A column cannot hand out an `Arc<V>`, so `get`, the iterators, `Change` and `Conflict` return a
`Val<V>`, which derefs to `V`. It is one of three things:

- The value itself, when `V` has no destructor and is at most 16 bytes. Only this case takes no
  atomic operation.
- The column and an index, for any other value. One value keeps the other values of its node alive
  for as long as it is held, which is outside `cache_bytes`. `into_owned` and `into_arc` end that.
- An `Arc<V>`, for a spilled or uncommitted value.

A `NodeRef<V>` holding the node's `Arc` was not built. It would need `K` and `S` in its type or a
virtual call on every deref, and it would pin keys and child links as well. One more allocation per
node buys a handle with a single type parameter that only pins values.

Reopened and read twice at 1M `u64` keys, median of three alternating runs, in nanoseconds per key
except for heap:

| Values held in | `u64` heap per key | `u64` fold | `u64` get | String heap per key | String fold | String get |
| --- | --- | --- | --- | --- | --- | --- |
| One `Arc<V>` each | 123 bytes | 27 | 614 | 171 bytes | 36 | 614 |
| A column, for values with no destructor | 61 bytes | 13 | 375 | 170 bytes | 29 | 656 |
| A column, for all values | 61 bytes | 13 | 383 | 109 bytes | 24 | 490 |

Half the heap matters more than the missing pointer. A million keys now nearly fit in the last level
cache, so a fold over keys alone, which never touches a value, went from 23 to 7 ns. Stopping there
left the process that wrote the data with the old slots, and `ferro-bench` reads in that process.
Median of three alternating runs:

| After `flush` | Bulk load | Get | Scan | Range | Table get | Table filter | Peak RSS |
| --- | --- | --- | --- | --- | --- | --- | --- |
| One `Arc<V>` each | 1,164 ns | 568 ns | 48 ns | 46 ns | 340 ns | 70 ns | 343 MiB |
| Slots that share each `Arc` | 1,295 ns | 673 ns | 47 ns | 55 ns | 400 ns | 41 ns | 343 MiB |
| A column | 1,500 ns | 486 ns | 39 ns | 44 ns | 207 ns | 22 ns | 362 MiB |

The bulk figure is the worst case, a short `String` per key, and building the column without copying
the slots first won most of it back. Over five alternating runs of `reads`, a bulk load through
`extend` took 24% longer for short strings and 2 to 5% longer for values with no destructor, and
`ferro-bench` could not tell its bulk loads apart from noise.

## Scans

`Iterator::fold` is overridden on `Iter`, `Keys` and `Values` to take a run of a node's entries in
one loop, which `sum`, `count`, `for_each`, `filter` and `map` reach with no new API. `try_fold`
cannot be overridden, because naming `Try` needs a nightly compiler, so `find`, `any` and `Take` go
through `next`, as do `for` loops and `collect`. A fold still builds an item, so it clones the key
and, for a value held by column, takes a reference count. `Map::for_each`, `try_for_each`,
`for_each_range` and `try_for_each_range` lend `&K` and `&V` instead and pay one reference count per
node, not per item, with a `ControlFlow` for early exit. In nanoseconds per key at 1M keys,
reopened, median of five alternating runs:

| Map | `for` before | `for` | `fold` before | `fold` | `for_each` | Keys `fold` |
| --- | --- | --- | --- | --- | --- | --- |
| `u64` to `u64` | 27 | 15 | 26 | 8 | 8 | 7 |
| `u64` to `()` | 27 | 13 | 26 | 6 | 6 | 6 |
| `u64` to short string | 36 | 37 | 35 | 24 | 19 | 12 |
| String key to `u64` | 49 | 34 | 48 | 28 | 20 | |
| Table rows | 37 | 29 | 35 | 14 | 9 | |

For values with no destructor a fold is as good as `for_each`, so the fold is the one to use.
`for_each` stays for what a fold has to clone: it takes 20 to 37% off strings and rows, and over a
value held by column it removes the atomic pair per item, which is 8 of the 11 ns at 10,000 keys.
With memory out of the picture at that size, the floor is 2 ns a key for a fold or `for_each` and 9
ns for a `for` loop. A `for` loop over strings gained nothing, because the same two atomic
operations now act on a wider handle.

A fold calls the closure it is given from one place. When each kind of run had a loop of its own,
a column held by copy, a column of values with destructors and a node's slots, the closure had two
or three call sites, and the compiler stopped inlining one that drops a `Val` of a table row, whose
drop glue then held the row's. Out of line, the closure took the 120-byte `Val` through memory and
copied it with 16-byte loads that straddled the caller's 8-byte stores, which the core cannot
forward. `ferro-bench`'s filtered scan of 200,000 rows took 101 instructions and 32.6 ns a row,
against 68 and 17.8 ns before the folds split (#56). Folds over maps kept their gain, because a
closure that drops a `Val` of a string or a `u64` is small enough to inline more than once.

A run now decides once how it reads its values, from a column it borrows or from its slots, and one
loop asks it for each value. The compiler unswitches that loop on the kind of run, so a column of
rows is read by a loop of about 20 instructions with the column's address in a register. Borrowing
the column also drops the count a clone of it took per run. Instructions per item, counted through
`perf_event_open` in a reduced copy of each fold, and for the filtered scan the time a row in
`ferro-bench`, median of seven alternating runs:

| Fold | Instructions before | Instructions after | Time before | Time after |
| --- | --- | --- | --- | --- |
| Filtered scan of 200,000 table rows | 101 | 62 | 32.6 ns | 18.7 ns |
| `range(..)` over the same rows | 102 | 59 | | |
| `iter()` over 1M `u64` to string | 63 | 58 | | |
| `iter()` over 1M `u64` to `u64` | 45 | 44 | | |
| `values()` over 1M `u64` to `u64` | 45 | 43 | | |
| `iter()` over 1M `u64` to `u64`, never flushed | 86 | 47 | | |
| Filtered scan of 200,000 rows, never flushed | 96 | 54 | | |

A map that was never flushed holds its values in slots, and those folds gained most, since the
slot path is now inlined beside the column path rather than called per value. `rows_iter_sum` in
`reads` went from 21.8 to 13.4 ns a row, and the 100-key ranges of `scaling` from 8,733 to 8,288
instructions. `iter().count()` over values held by copy is the one fold that costs more, 41
instructions a key against 38: the compiler adds up each run at once but leaves the emptied loop
behind.

## Sorted lookups and cursors

A `Cursor` keeps the path to its position, one frame per level, which holds its node as described
under [Iterator frames](read-scaling.md#iterator-frames). `seek` climbs only until a node's key span
holds the target and searches down from there, so it costs the distance moved, not the height of the
tree. Both ends of the span are checked, so a seek backwards is as cheap as one forwards and sorted
input is not an obligation. `seek_back` finds the last key at or before the target, and `advance`
steps in key order. `value` returns a `Val`. `read` lends the entry, loading a spilled value first.

`get_many` and `contains_many` run a cursor over an iterator of keys. They yield each key with its
result, so they compose like `map`, and keys may be owned or borrowed. On 1M `u64` keys, median of
five alternating runs, in nanoseconds per key:

| Keys, sorted unless noted | `get` before | `get` | `get_many` | `contains_many` |
| --- | --- | --- | --- | --- |
| Every second key | 115 | 85 | 26 | 28 |
| Every 16th key | 464 | 233 | 153 | 97 |
| Random order | 586 | 342 | 363 | |

Unsorted input costs 6% more than plain `get`, which is the price of keeping the path. A batch that
recursed through the tree, cutting a sorted slice at each node as `absorb` does, was 6% faster at
the densest and 7 to 26% slower when sparse, and needs a slice, a callback and sorted input, so the
cursor replaced it. A cursor also serves merge joins, which a batch cannot, and combined index
lookups look keys up in their larger side through one, as described under [Combining
lookups](schema-and-unique-indexes.md#combining-lookups).

`range_by` used to walk its two edges separately, adding a rank at each level to learn its length.
They now share one walk until they part and count only the links between them, so a narrow range
reads each node once and sums almost nothing, and `range(key..).next()` went from 860 to 565 ns.
`floor` is one walk that remembers the last node with a key before the target. Built on a cursor it
was 20% slower than `get`, and it would have written the reference counts of pinned nodes.
`Set::range` returns the named `Keys`, so it has `len` and `next_back`, and the set operations probe
through `contains_many`.

`IndexIter` fetched rows through a cursor. Rows of one index value sit in key order, so those went
from 163 to 60 ns each, and rows scattered across the table from 208 to 161 ns. It now fetches them
through a finder that keeps the last leaf and its parent, as described under [Reading from many
threads](read-scaling.md#index-rows). `IndexIter::keys` stops before the rows and takes 5 ns a key.

## What is left, on `examples/graph`

`bench overhead` at 1M users and 46M edges, median of seven alternating runs:

| Operation | Before | After | `Vec` |
| --- | --- | --- | --- |
| Point `contains` | 1,403 ns | 927 ns | 108 ns |
| Neighbour fetch | 1,718 ns | 1,089 ns | 128 ns |
| Sequential edge scan | 6.7 ns per edge | 3.3 ns | 0.12 ns |
| Pagerank per iteration | 0.40 s | 0.22 s | 0.042 s |
| Reach, 3 hops | 40.6 ms | 21.1 ms | 9.5 ms |
| Recommend, 20 users | 1.7 ms | 1.2 ms | 1.1 ms |
| Peak RSS, including 368 MiB of `Vec` | 955 MiB | 805 MiB | |

Half of a scan is now varint decoding, 1.6 ns per edge, which is what storing an edge in 2.9 bytes
costs, even with a decode loop that has no branch per byte. A decoder that took eight bytes at a
time was 24% slower, because each block ends in a tail it has to take byte by byte. The other half,
1.7 ns per edge or 75 ns per block of 44 edges, is walking keys and reaching each block's bytes,
which has no counterpart in a `Vec`. `Vec`'s 0.12 ns is a SIMD sum that does no work per edge:
pagerank's real work is 0.9 ns per edge on `Vec` and ferro pays the same on top of its scan. A
random seek is 1.1 µs because seven levels, in a heap far larger than the last level cache and the
reach of the TLB, take several DRAM misses where a `Vec` takes two. Neither the walk nor the
decoding goes away without a different structure. What a `Vec` cannot do at any price is take a
commit or keep an old version.

## Measured and left out

A node miss is dominated by BLAKE3, at roughly 500 to 900 ns on an interior node against 130 to 190
ns for `pread`, and the hash check is not negotiable. Reads fetch 1 KiB at a time, which fits most
records. `Decoder::varint` takes a one byte varint inline, which took `Vec<u64>` of small numbers
from 6.1 to 1.5 ns an item and cold scans down about 7%.

- Bulk decode hooks for keys. `Vec<f64>`, eight raw bytes an item, decodes at 1.4 ns, and a random
  `u64` varint at 8.8 ns, so raw keys would save at most 7 ns in a cold scan of about 100 ns a key.
  That needs fixed-width keys on disk, which would change every root hash and grow every node. For
  varints, a `decode_vec` override has nothing left to skip.
- Lazy value decoding. Decoding one value on demand made cold gets 6 to 13% faster, but cold scans
  25% slower and fully read nodes 24% larger. Decoding a node's values together on first use was
  slower than eager decoding once only leaves were missing from the cache.
- `mmap`. Copying out of a mapping before hashing, the only form that cannot be changed under the
  check, gained 3 to 9% on cold reads. That does not pay for SIGBUS when another process truncates
  the file, or for I/O errors arriving as signals instead of `Error`. It also caps what batching
  reads during scans could gain.
- Cheaper cache keys. A hash prefix in place of ahash gained 1.4% with memos in place, since memo
  hits skip the cache. Keying by offset with a hash compare was no better than that prefix even
  before memos, because the cost was misses, not hashing.
- A chunked or trie `List`. `List<T>` stays a `Map<u64, T>`, and a push rewrites the right spine
  with the values held at each level: 1.6 KB for a `u64` and 3.2 KB for a 60 byte string at 1M
  items, or 1.3 and 2.5 KB at 100k. Chunks in a `Map<u64, Vec<T>>` did not win clearly. At 1M, 16
  per chunk took the string case to 2.4 KB but the `u64` case to 2.9 KB, and 64 per chunk to 5.9 KB,
  because a node then holds several chunks just under the spill limit, while cold gets rose up to
  six times. A vector trie of fanout 8 with values only in its leaves came to about 1.0 and 1.2 KB
  in a simulation built from real record sizes, but at that fanout it has as many levels as the
  `Map`, so it would read no faster, and fanout 32, which would, gave the bytes back at 2.0 and 2.5
  KB. It helps one shape, a long list appended to one item per commit, and costs a second tree
  beside `Map` with its own nodes, flush, memos, diff and foreign store handling. Bulk appends
  already go down one edge in one descent.
- Scan admission control. `quick_cache` starts new entries in a small cold section, so a full scan
  leaves hot-set latency unchanged. Not admitting scanned nodes saved 7 to 17% on scans larger than
  the cache, but made repeated short range pages 3 to 4 times slower, and a budget per iterator that
  avoided that saved only 6 to 8%.
- Other shapes for the loop a fold runs over a run, described under [Scans](#scans). Calling the
  slot path out of line, to keep the loop small, left a call in the column's loop, and `iter()` over
  `u64` values took 61 instructions a key against 44. A `for` loop over the run took 48 and a zip of
  the keys with their offsets 46, and both made `iter().count()` dearer, 44 to 46 against 41.
- The headers beside the flags. With `origin`, `prefix`, `count` and `level` first in `Node` and
  the three vector headers last, the headers a get reads share the line of the flags, and a get on
  the map of [Node layout](#node-layout) read 36.7 lines rather than 38.3. L1 misses served from L3
  fell from 4.5 to 3.7 a get and from DRAM from 1.5 to 1.2, but lines from DRAM in all stayed at
  about 13.5, and over five alternating runs a get took 323 ns against 321 on the first pass after
  the load and 316 against 317 on the second. Built again for iterators, while the flags still
  sat behind the value, it took back only part of what borrowing cost a range right after a scan
  and slowed bulk loads by 2%, as [The flags' line](read-scaling.md#the-flags-line) describes, and
  the flags moved to the headers instead.
- A node in one allocation, with its keys inline. `Node` held a 128-byte buffer for its keys,
  right-aligned against their count, which sat before the child and cell headers, so that a node
  whose keys fit, sixteen `u64`s or five `String`s, read its keys, headers and flags as one run of
  lines in one allocation. A node read from the file or cached by `flush` kept its keys there, and
  its first edit moved them to a vector, as the first edit of a value moves a column to slots. The
  file and its hashes did not change. A get read 30.5 lines on 10.7 pages rather than 38.3 on 13.4,
  with 1.0 L1 miss from DRAM against 1.5, but lines from DRAM in all stayed at 13.5, and it ran 39
  instructions more, for the branch between the buffer and the vector. A 256-byte buffer, which
  holds the keys of nine in ten of the nodes a get reads, gained 4.8% where 128 bytes gained 3.2%.
  Every node grew by 112 bytes and only those of 16 keys or fewer lost their key vector, so the
  heap grew, and scans and folds, which read every node whole, slowed down. Alternating runs of the
  build before and the one with keys inline, pinned to the first CCD apart from `scaling`, which
  places its own threads:

| Workload | Before | Keys inline |
| --- | ---: | ---: |
| Compare harness, `get`, seven runs | 0.327 µs | 0.320 µs |
| Compare harness, bulk load | 0.667 s | 0.705 s |
| Compare harness, 1000-key range | 29.6 µs | 31.2 µs |
| Compare harness, scan, per key | 19.1 ns | 19.4 ns |
| `ferro-bench`, warm `get`, three runs | 322 ns, 1,025 instructions | 301 ns, 1,064 instructions |
| `ferro-bench`, a table's row `get` | 161 ns | 154 ns |
| `ferro-bench`, bulk load, per key | 683 ns | 726 ns |
| `ferro-bench`, 1000-key range, per key | 31.3 ns | 33.0 ns |
| `ferro-bench`, `diff`, per change | 4.61 µs | 3.32 µs |
| `ferro-bench`, peak RSS | 343 MiB | 364 MiB |
| `reads`, random `get`, strings, `u64` and `()` values, three runs | 330, 260 and 215 ns | 313, 264 and 219 ns |
| `reads`, sorted `get`, `u64` values | 61 ns | 70 ns |
| `reads`, `sum` over `u64` values, per key | 7.0 ns | 8.4 ns |
| `reads`, heap per key, `u64` values and strings | 60.5 and 108.1 bytes | 67.5 and 115.4 bytes |
| `scaling`, `get` with the 1 GiB cache at 1, 8 and 32 threads, two runs | 3.00M, 22.6M and 61.9M a second | 3.06M, 23.4M and 64.4M a second |
| `scaling`, `iter()` scan with the 1 GiB cache at 1, 8 and 32 threads | 57.4M, 267M and 807M keys a second | 54.8M, 262M and 766M keys a second |
| `scaling`, covered pages of 200 rows with the 1 GiB cache at 1 and 8 threads | 769k and 4.10M a second | 695k and 3.76M a second |

With the 16 MiB cache gets moved by under 2% at every thread count. Other work kept six hardware
threads of the second CCD busy throughout, which `scaling` shared at 32 threads. Gets of short
strings gained 2 to 7%, short of the 8% that the layout would have had to show, and gets of `u64`,
`u32` and `()` values lost 1 to 2%, while bulk loads lost 6%, ranges 5 to 6%, scans 2 to 5%, folds
over small values up to 38% and the heap 6 to 14%, so a node keeps its keys in a vector of their
own.
