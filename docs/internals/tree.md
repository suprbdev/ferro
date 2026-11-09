# The Merkle search tree

`Map` is the one tree every collection is built on: `Set` and `List` wrap it, and a `Table` is a
`Map` of rows plus one per index. This file covers its shape, how it is edited and flushed, and what
was measured on the way. Reads are in [The read path](read-path.md).

## Shape and fanout

`Map` is a Merkle search tree. A key's level is `trailing_zeros(xxh3_64(encoded key)) / 3`, so about
1 in 8 keys is promoted per level. The level hash is fixed-seed and not collision resistant: someone
who can choose keys could craft many keys of a high level and unbalance the tree, which costs
performance but not correctness, because node hashes are still BLAKE3. xxh3 replaced BLAKE3 here
because hashing a key was the largest per-insert cost; level frequencies on sequential integers,
strings and tuples match the ideal geometric distribution. Fanout 8 was chosen by measurement and
re-checked after links shrank, bodies were compressed and reads gained child memos, at 1M keys:
against fanout 16 it writes 27% fewer bytes per commit for 31% slower warm gets and 29% faster cold
gets, and against fanout 4 it writes 30% more bytes per commit for 12% faster warm gets and 17%
smaller bulk loads. A node holds all keys of exactly its level within its range, and children hold
the gaps. Tree shape depends only on the key set, never on insertion order. Consequences that the
rest of the system relies on:

- `a == b` for two maps compares root hashes in O(1).
- A commit that changes nothing has the same root hash and is skipped.
- `diff` skips any subtree with matching hashes, so it costs O(changes), not O(size).

## Range edits

Insert and remove use treap-style `split` and `merge`, and so do the range operations: `take_range`,
`remove_range` and `split_off` cut the tree along one or two paths and `append` joins two maps whose
key ranges do not overlap, so each touches O(log n) nodes. `remove_range` returns the count stored
on the cut-out subtree links and never reads those subtrees. At 1M keys it took 0.8 to 2.5 ms where
collecting the keys and removing them one by one took 5 to 260 ms, for the same bytes written. A cut
that moves nothing keeps each node's `origin`, so it writes nothing back. `Table::drain_range` has
to update every index for every row, so it is O(rows) and returns them. `append` across two stores,
or with overlapping keys, copies `other` entry by entry, since links only mean something inside
their own store.

A key that compares equal to one the map holds but encodes otherwise can hash to a higher level than
the stored key, so an insert or a batch would cut the tree under it, across the stored key. The cut
path is walked before it is cut, and a stored key found on it turns the edit into a change of that
key's value at that key's level, unless the stored key does not sit at the level it hashes to, which
is refused as corrupt. The map keeps the key it was first given, as it does when the new
key hashes to the stored key's level or below, and the tree stays the one a fresh build of its
entries gives.

## Edits that fail halfway

A cut, a join or a batch takes nodes apart as it reaches them, and a node it reaches may first have
to be read from the file, which can fail, or decoded by a `Data` impl that panics. Each edit is
arranged so that a failure leaves the map with the entries, length, summary and root hash it had:

- `split` walks its cut path first. A path held in memory throughout, as a map's new nodes are, is
  only walked. A stored node on it is loaded into its link in place, which changes no entry, and
  only then is the path cut. `remove` and a joining `append` do the same for the two edges a
  `merge` takes apart, as far as their levels lead it, and `take_range` loads both of its paths
  before the first cut.
- A batch on a stored root runs on a copy of the root, which the map keeps only once the whole
  batch has gone in. The copy costs nothing, since the nodes the batch loads are new either way and
  a failure drops them with the copy. It is dropped as well when no edit changed anything, a removal
  of an absent key or a zero-sized value stored under a key the map holds, so such a batch leaves a
  clean map clean. A copy of a root changed earlier in the transaction would make the batch copy
  each changed node it reaches rather than edit it in place, so such a batch first walks the path
  of each of its keys, and both edges beside each key it removes, loading the stored nodes there in
  place, and then edits in place with nothing left to load. The edges go all the way down, since
  removing neighbouring keys joins what earlier joins made.
- `insert` and `remove` read a value stored on its own that they replace or remove before they
  change its slot, so neither fails once the tree has changed. An edit that drops such a value
  unread, as each index edit of a table write does, and as a table does with a row it has read
  already, loads nothing for it.
- `merge` reads the three maps and resolves the whole diff before it writes anything, then writes
  as one batch, so a load that fails, like an `Err` from `resolve`, leaves ours as it was. It
  reads ours through a finder, which keeps the leaf and parent of the key it found last, since
  the diff hands it keys in order.
- A table write changes its rows and several index maps, each edit whole on its own, and stages the
  entries of each index in `Edits`. An index that loses a lone entry is loaded for first, as far as
  a removal that drops the value unread would load, and one with more edits than a removal and an insert as a batch on a changed
  root is. An index that gains a lone entry gets it as the write goes, and if a later step fails the
  entry is taken back, which loads nothing: the insert left the path to its key in memory, and the
  two children it cut beside a new key are new nodes down the edges that a removal joins. The write
  the derive generates loads, then makes the lone inserts and writes the rows, taking the inserts
  back if either fails, and last makes the removals and batches, which have nothing left to load. A
  lone removal beside a lone insert in one index, as a changed value gives, is loaded for just after
  the insert, so that it loads what the tree it applies to needs, with no allocation for a batch. A
  removal and an insert of keys that compare equal but encode otherwise, as a covered field whose
  encoding changes while `==` holds gives, cannot be one edit, since the new key hashes to its own
  level; the removal is loaded for down both edges beside its key, and both are made last, the
  removal first, so that the insert cuts along the seam the removal joined, which is loaded.
- An insert of a row under a key the table does not hold, into a table whose every index gives a
  row at most one entry, has only lone inserts to make, so it stages nothing. It puts each entry
  straight into its index, keeping only the slot it replaced, if any, and if a later entry or a
  `Data` or `Clone` impl fails, takes back those it put in, working their keys out again from the
  row, which loads nothing for the same reason. So it clones no key to take back by. A table with
  an `each` index, whose rows can give an index several entries, writes them through `Edits`.
- `flush` refuses, with `Error::Corrupt`, a node whose count is not its keys plus its children's
  counts, so a tree damaged some other way is not committed with links that claim more entries than
  their nodes hold.

Measured against the code before in alternating runs at 1M keys: per-key inserts in `bulk` ran 2,752
instructions a key against 2,740 in the same time, single-key commits 0.05 to 0.07% more
instructions, most of them the count check in `flush`, and reads, `remove_range`, `split_off`,
`append` and batches onto a committed map did not move beyond noise. Removing keys one at a time ran
1.4 to 1.9% more instructions, for what it checks and loads before it changes a node. A batch onto a
root changed earlier in the transaction ran 11 to 20% more instructions and took 5 to 33% more time
for the walk before it. Running it on a copy of such a root, tried first, ran 37 to 168% more
instructions, from copying each changed node it reached.

The table writes and `merge` were measured the same way. The hold, confirm and cancel commits of
`bench-tables` ran within 0.3% of the instructions and the time they took before, and the main
benchmark's table updates within 0.05% of the instructions. Loading a table 10,000 rows a commit ran
4% more instructions and 2% more time, for staging each index entry. Two designs tried first cost
more: loading every node an index edit reaches before any write, which walks each path twice, and
reading the old row before an insert to stage its entries, ran 1.0 to 1.9% more instructions in
those commits and 13% more in the load. `Map::merge` of 1M keys with 110,000 written took 1 to 5%
less time than writing each key as the diff reached it, which putting back a copy on a panic kept
to; reading ours without the finder had made it up to 10% slower.

The load's extra instructions were all in the insert loop: a third of them cloning each key to take
it back by, the rest the staging around it. Putting a new row's entries straight into their indexes
won them back. Against the code before staging and the code with it, in alternating runs pinned to
16 cores, medians of eight, the main benchmark's table load ran 14,494 instructions a row against
14,389 and 15,010, in 1,645 ns against 1,645 and 1,692. Loading the same 200,000 rows into a table
in memory ran 7,556 instructions a row against 7,476 before staging, 8,057 when staging came in and
8,123 just before the change, so what is left over the code before staging came with later map
changes. The hold commits of `bench-tables` ran 1.4% fewer instructions in their closure, and its
confirms and cancels, the main benchmark's table updates and `bulk` the same instructions as with
staging, all within the noise in time. Returning the panic from the derive's insert of the entries,
rather than catching it again around it, ran 4 instructions a row more and was left out.

## Copy-on-write and flush

Mutation is copy-on-write through `Shared::make_mut`, so a node is copied once per transaction and
then mutated in place. `Link::Mem` is a dirty in-memory node, and `Link::Stored` a persisted one.
`flush` writes dirty nodes bottom up during `Data::encode` and seeds the node cache with the nodes
it wrote. Values inserted or mutated since the node was loaded are decoded once as a check before
they become durable; unchanged values are not. When at least half of a node's values are fresh, the
cached copy is decoded from the written bytes, which is that check and the copy in one pass;
otherwise the node is cloned and each fresh value is the one its check decoded. A value holding a
dirty or foreign collection also forces the decode, so nested collections always come back clean
and a foreign store is never pinned.

`Map::update` keeps the value as it was if its closure panics, as `Table::update` keeps a row,
where `get_mut` hands out a plain `&mut V`. It finds the slot as `get_mut` does, runs the closure on
a copy of the value, and writes the copy into the slot once the closure returns, in place if the
slot already holds a value of its own that nothing else shares. A value still in its node's
column, or spilled, is copied out or read either way, so for it the copy costs nothing, and skipping
the `Arc::make_mut` that `get_mut` makes on the slot saves a little. A value inserted or changed
since its node was read, which `get_mut` changes where it is, is cloned, and that clone is the
cost. Measured on 2026-11-07 on one core, medians of 15 runs, on a map of 1,000 keys:

| Update of one key | Before, through `get_mut` | Copy written back | Old slot put back on a panic |
| --- | --- | --- | --- |
| In memory, a `u64` | 49.7 ns | 48.1 ns | 53.3 ns |
| In memory, a `Vec` of four `u64` | 48.5 ns | 58.7 ns | 66.0 ns |
| In memory, a 40-byte `String` | 50.7 ns | 61.9 ns | 64.3 ns |
| Committed, each key once, a `u64` | 99.9 ns | 92.1 ns | 99.6 ns |
| Committed, each key once, a `Vec` of four `u64` | 117.3 ns | 112.1 ns | 118.6 ns |
| Committed, each key once, a 40-byte `String` | 119.6 ns | 114.0 ns | 120.4 ns |
| Committed, one key in a fresh clone of the map | 755 ns | 723 ns | 736 ns |

The last column changed the value in place as `get_mut` does, kept the slot it replaced, or a
clone of a value already in a slot of its own, and put it back from a guard's `Drop` if the closure
unwound. It cost a little more everywhere, and needed the guard. A first version of the copy read
the value with `get` and walked the tree again to write it back, which cost 54 to 70 ns in memory.

A node that `flush` writes or decodes keeps the bytes and links of its level, keys and values while
its values are still one column, which they stop being at the first edit of a key or value, since
every such edit goes through `Cells::slots`. A node on the path to a change has a new child and the
same entries, so the next flush copies those bytes and encodes only its child links. The kept bytes
count toward the node's cache weight, and the file comes out byte for byte the same. The parallel
flush below keeps none, so a bulk load holds no more memory than before.

## Parallel flush

A dirty node of at least 4096 keys with two or more dirty children and at least 256 dirty nodes
under it is flushed in three phases. First, every dirty subtree under 4096 keys is encoded, hashed
and LZ4 compressed on up to 16 scoped threads, each into its own buffer, and the nodes above them on
the calling thread. Second, the calling thread appends the records to the transaction in the order a
one-thread flush would, so the file comes out byte for byte the same. Third, the threads decode the
cached copies from those buffers, which also checks every value and leaves the same `Weak` memos and
hints a one-thread flush leaves, as described under [The read path](read-path.md#memos), and the
calling thread hands them to the cache in the same order. The first phase encodes against a sink
that refuses writes and links, so a value that holds a dirty collection or a raw `Ptr` sends the
node back to the one-thread path. The threads are scoped and the caller blocks on them: a commit
holds the branch lock, and a work-stealing pool thread waiting on a join could pick up another task
that commits to the same database and deadlock. At 1M keys the commit went from 476 to 98 ms and
`root_hash` on a dirty map from 209 to 32 ms. The decoded copies land on the threads' heaps, so
where they are built matters: building them in the first phase, next to scratch buffers that are
freed later, spread them over 2.5 times as many pages and made the next scans 10 to 15% slower.
Built in the third phase, after each subtree's buffers are released, the first pass of gets, scans
and ranges after a bulk load is as fast as before or up to 7% faster, and peak memory rose 2%, from
362 to 371 MiB at 1M keys. Each phase spawns its threads, so a 10,000-row table commit, with little
to share out, gains up to 5% for 50% more CPU time. The rule first stopped at two dirty children, so
a commit that changed two keys under different children of a large tree's root spawned threads in
both phases to flush a handful of nodes. A booking confirm, which moves a booking from one status to
another in an index of several thousand entries, did that on every commit, and lost a third of its
CPU time outside the closure and nearly half of its wall time to it. 256 dirty nodes are about a
millisecond of work on one thread.

## Inserts and prefetch hints

A 1M-key bulk load of random keys then spent about 60% of its time in the per-key inserts and 25%
dropping the in-memory tree after the commit. `ferro-bench`'s `bulk` tool times the inserts, the
rest of the commit and, with `--split`, the drop apart, with user-space instructions counted through
`perf_event_open`.

Below the top levels an insert waited on memory three times per level: for the child's header, then
its keys, then the line of its children, or of its slots in a leaf, that the search picked. A dirty
link now carries a hint like a memo's, the offsets of its node's keys and of its children or slots.
It is computed for nodes below level 3 when they are built and again when an insert moves their
buffers, a node copied for a write starts without one, and a parent at level 3 or below prefetches
all of it as it steps into the child, so the three arrive together. Over three sets of seven
interleaved runs the inserts took 22 to 31% less time, for 11% more instructions. Ascending keys
keep their path in cache, and took 1 to 5% more time for 9% more instructions, until new leaves and
the right parts of splits started with room for eight keys instead of reallocating at the second and
fifth: 42% fewer reallocations, inserts 1.7% faster on random keys and 5.7% on ascending ones, for 2
to 3% more peak RSS. A booking closure, which copies a few paths from the file, runs 5% more
instructions and 2.5% more CPU time, about 0.3 µs.

A slot used to take 48 bytes, for the spilled variant's inline `Ptr`, and now takes 24, as
[Values](read-path.md#values) describes, so a leaf's slots are half the size an insert shifts and
half the memory a bulk load faults in. The recursion also got lean: `insert_at` used to inline the
paths that build a new leaf, split a subtree under a new key and load a stored node, and call
`dirty_mut` out of line, so each level pushed a 728-byte frame. It now makes its node unique in
place and recurses, and `insert_outside`, `insert_above` and `insert_between` take the rare paths,
in a 216-byte frame. In `bulk`, median of five alternating runs on one CCD, 1M random keys:

| | Before | After |
| --- | ---: | ---: |
| Inserts | 543 ms, 2,977 instructions a key | 496 ms, 2,728 instructions a key |
| Rest of the commit | 95 ms | 92 ms |
| Drop | 108 ms | 96 ms |
| Peak RSS | 379 MiB | 343 MiB |

Most of what is left of an insert is waiting: about a third of its samples sit on the
compare-and-swap that makes each node on the path unique, which is the first touch of a node
whose address the level above has only just read, so no prefetch issued from there arrives
sooner. Another 14% is the page table and page faults: the inserts fault in about 190 MB of 4 KiB
pages, 49,000 faults and 64 ms of system time, since glibc's heap gets no transparent huge pages by
default, and with `GLIBC_TUNABLES=glibc.malloc.hugetlb=1` they took 501 faults and ran 14% faster.

## Dropping a large tree

Dropping the tree freed 2.4M allocations in key order: a box and a string for each value and up to
four for each node. Random keys leave the values in memory in an order unrelated to the keys, so
every free missed the cache on its chunk and on the neighbours glibc merges it with. Since glibc
2.42 every free past the per-thread cache of 16 also takes the arena lock once the process has a
second thread, which keeps those misses from overlapping. A `Map` of 4096 keys or more that is
dropped with a dirty root samples the fresh values on four root-to-leaf paths. If more than one step
in five between neighbouring values goes down in memory, it walks the dirty nodes it alone holds
breadth first, prefetching ahead, collects the values, key vectors, slot vectors, child vectors and
nodes, sorts each by address and frees all of them in one ascending pass that merges the five lists,
so each chunk merges with the one freed just before it. At 1M keys the drop went from 286 to 110 ms,
and the gets, scans and ranges right after the load did not change. A tree built in key order, by
ascending inserts or `Extend`, already frees in address order and keeps the plain drop.

The walk prefetched the first two lines of each node 16 ahead, but `Shared::get_mut` reads the weak
count and `lent`, which sit behind the value on a line of their own, so every node still waited on
a miss there. It prefetches that line too, and the walk of 1M keys went from 16.3 to 9.6 ms, median
of four alternating runs, out of a drop of about 95 ms: sorting the five lists takes 17 ms and the
ascending pass 67. That pass used to work out all five heads after every free and now refreshes
only the one it advanced, 30 instructions a key fewer, with no change in time beyond noise. The
counts and flags have since moved to the front of the allocation, as [The flags'
line](read-scaling.md#the-flags-line) describes, so the walk prefetches their line and the first
two of the node, and the drop took the same 103 ms in seven alternating runs of `bulk`.

## Bulk load figures

With the LZ4 change described under [Commits of a few rows](commit-cost.md#the-lz4-encoder), a
1M-key bulk load in `bulk`, median of five interleaved runs on a quiet machine, went from 1,018 to
703 ms: the inserts from 649 to 514 ms, the drop from 286 to 103 ms, and the rest of the commit
stayed at 87 ms of wall time for 7% less CPU time. Peak RSS rose from 365 to 374 MiB, and the `heap`
tool's live heap after reads moved by at most 0.2 MiB. Ascending keys and `Extend` did not move
beyond 3%. In the compare harness it went from 1.04 to 0.71 s against 0.60 s for SQLite, as
`benchmarks/compare/RESULTS.md` describes.

Smaller slots, the lean insert recursion and the drop's flag-line prefetch, described above, took
the harness's ferro child from 0.71 to 0.67 s, median of seven alternating runs on one CCD of the
machine, and from 0.73 to 0.69 s and 0.74 to 0.70 s in two noisier sets, where SQLite took 0.63 s
and redb 1.36 s in three runs each the same way. Loading the same
keys with one `extend`, which sorts the batch and builds the tree bottom-up as described under
[Batches](#batches-extend-fromiterator-and-apply), took 0.41 s. A `Map::from_sorted` for input
already in key order would save only the sort, about 33 ms at 1M pairs, so it was not added.

## Measured and rejected

- Prefetching the child's header, count and flag lines with its hint as an insert steps into it:
  inserts 1% faster in one set of six alternating runs and 7% slower in a set of five. The
  compare-and-swap on the header follows a few instructions later, so the prefetch gains it nothing.
- Room for 16 keys in new leaves now that slots take 24 bytes: inserts 6% slower than with room for
  eight over five alternating runs, and the gets after the load 3 to 7% slower, from the larger
  heap.
- Sorting the teardown's five lists on threads. Not built: the sort is 17 ms of a 95 ms drop, and
  sorting in a destructor on threads needs `K` and `V` to be `Send`, which `Map`'s `Drop` cannot
  ask.
- Prefetching a node's keys and its children or slots after reading its header, without hints in the
  links: inserts 12% faster for 20% more instructions, against 22 to 31% with the hints.
- Prefetching from the hints at every level: 1 to 6% faster inserts on random keys than stopping at
  level 3, for 8% more instructions, but ascending keys lost 6 to 8% where the gate lost 1 to 5%.
- Skipping the prefetch into the last child, the one ascending keys always take: ascending keys as
  fast as before, random keys 2.5 to 6% slower than with it.
- Splitting a child in place for an insert, keeping its node as the left part: 12% fewer allocations
  and almost no frees during the inserts, but no faster, for 4 to 5% more instructions.
- Linear search up to 32 keys instead of 16: inserts unchanged, gets 1.4% slower for 4.6% more
  instructions.
- Checking that a node is unshared with plain loads, which takes unsafe code, instead of the
  compare-and-swap in `Arc::make_mut`: no faster in a repeated run. The samples on the swap were the
  cache miss on the header.
- Sorting the frees by address whatever the order: 30% slower drops after ascending inserts and 10
  to 16% after `Extend`, since freeing every value before the vectors around it leaves holes that
  later frees merge one by one.
- Freeing each kind in turn, values first: 155 ms against 111, since a free often landed next to a
  chunk of another kind freed earlier, which glibc had to unlink from wherever its bin had put it.
- Collecting every piece into one array and sorting that: 10% slower than freeing kind by kind, from
  sorting 60 MB of pieces.
- A per-commit arena or bump allocation for working-tree nodes. Not built: `Arc` takes no allocator
  on stable Rust, a global allocator is the application's choice, and the values and what they own
  are allocated by the caller's types anyway.
- Buffering per-key inserts to build leaves from a sorted batch. Not built: `insert` returns the
  value it replaced and later reads in the closure must see the new one, so each insert needs its
  lookup either way. `Extend` and `apply` already sort a batch and build or absorb it directly, in
  about a quarter of the time of per-key inserts of 1M random keys.
- Dropping the old tree on a background thread. It took about 300 ms off a 1M-key load, but its
  frees contend with every other allocation in the process: the gets right after the load were about
  25% slower, reopen 0.2 ms slower, and a table loaded in 50,000-row commits up to 25% slower unless
  each commit first waited for the last drop.
- Freeing in parallel. Dropping 1M `Arc<String>` in random order took 192 ms on one thread and 310
  to 900 ms on 2 to 16, since every free takes the lock of the arena that allocated it.
- rayon. As fast as the scoped threads, but it can deadlock as above, and its `par_sort_by` did not
  beat the standard stable sort on 1M shuffled pairs, about 33 ms either way.
- Reserving the transaction buffer up front, and keeping LZ4 off when it does not pay. The first
  changed nothing measurable, and without LZ4 a 1M-key load is 17% larger and a table row 80%
  larger, so compression stays and runs on the threads.

## Batches: `Extend`, `FromIterator` and `apply`

`Extend`, `FromIterator` and `apply`, which also takes deletes, sort the batch and keep the last
edit per key. Batches of 16,384 keys or more hash their key levels on the scoped threads described
above. An empty map is assembled bottom-up in one pass, which gives the same tree as inserting key
by key. A non-empty map takes the batch in one descent: each node sorts the edits into its gaps,
applies the ones at its own level in place, and passes each group to one child, so a subtree no edit
falls in stays shared and a node is walked once however many edits it receives, or twice on a root
changed earlier in the transaction, as [Edits that fail halfway](#edits-that-fail-halfway)
describes. Editing the node in place, not rebuilding its vectors from the old and new entries, is
what makes this pay: the rebuilding form was up to 28% slower than per-key inserts. The gain is
modest, because a touched node has to be read and copied either way and `flush` costs three to four
times the tree work. At 1M keys the closure takes 5 to 25% less time than sorted per-key inserts
from 1,000 to 100,000 edits and 30 to 37% less for 1M edits, the commit as a whole 0 to 22% less,
and batches under 1,000 edits show no difference. A 50,000 page wiki import, 20M posting entries,
gained 4% in its posting phase. The metrics load test gained nothing measurable, since chunk merging
dominates there, so its ingest keeps plain inserts. `retain` collects its deletes and applies them
the same way. A disjoint batch needs no special case, since the same descent walks one edge of the
tree.

## Spilled values

Values over 512 encoded bytes spill to their own object, so a large value does not bloat its node or
get rewritten when a neighbour changes. Lower thresholds were measured and rejected: spilling
interior-node values over 64 bytes cut table update bytes by 19% but grew bulk size by 14% and
slowed table reads by 20 to 50%, once compression was already shrinking interior nodes.

## Byte buffers and newtypes

`Vec<u8>` and `[u8; N]` copy their bytes in one go, through the `encode_slice` and `decode_vec`
hooks on `Data` that `u8` overrides, which took a 1 MiB value from 5.3 ms to 0.49 ms to commit and
from 2.0 ms to 0.16 ms for a cold read. `Bytes` has the same encoding and adds an O(1) clone and
`Borrow<[u8]>`, so `Map<Bytes, V>` looks up by slice. It copies once on decode. Sharing the object
buffer instead gained 8% on a cold read at 1 MiB and under 4% at 64 KiB and 1 KiB, where `pread` and
BLAKE3 dominate, for a wider handle, a change to the read path, and a small value that pins the
whole node it was decoded from. `#[ferro(transparent)]` gives a newtype the encoding of its field.
