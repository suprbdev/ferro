# ferro design

A ferro database is one typed Rust value, `T`. There is no query language: you read `T` with
ordinary Rust and change it inside a closure. Every commit produces a new immutable version
of `T`, and all earlier versions stay readable.

## Layers

- `codec.rs`: the `Data` trait, `Encoder`, `Decoder`, primitive impls.
- `store.rs`: one generation of the database: the append-only file, commit frames, superblocks,
  recovery, object reads with hash verification, node cache.
- `map.rs`: `Map<K, V>`, the persistent ordered tree, its optional summaries, `Cursor`, and `Val`,
  the handle reads return. `Set` and `List` wrap it.
- `table.rs`: `Table<T: Record>`, a keyed collection with automatic secondary indexes.
- `db.rs`: `Db<T>`, `Snapshot<T>`, commits, branches.
- `log.rs`: the commit chain and `Log`. `history.rs`: `History`, the lazy walk behind `history_of`
  and `history_of_range`.
- `watch.rs`: `Watcher<T>`, `Closer<T>` and the per-branch feed behind them.
- `reclaim.rs`: the copy into a new generation, which `Db::compact` and `admin::compact` share.
- `platform.rs`: the file calls that differ between Unix and Windows, described under Windows.
- `sync.rs`: `Have`, `Bundle`, and `Db::pull`, `push` and `receive`, which copy a branch between files.

## Objects and links

Everything on disk is an `Object { body, links }`. A link is a `Ptr { hash, offset }`.

- The hash is BLAKE3 over body length, link count, body, and the link hashes, truncated to 20
  bytes (`HASH_BYTES`). Offsets are not hashed, so identical content has an identical hash
  wherever it sits. The file is a Merkle DAG.
- The offset lets a reader jump straight to a child, so no hash index is needed. It means
  something only inside one generation of the file, as Reclamation explains.
- Every read re-hashes the object and compares it with the pointer it came from.
- Links are stored in a side table and consumed in order by `Decoder::link`. They take no bytes
  in the body, which keeps encoded size independent of a value's position.

`hash_of(&value)` gives the hash a value has as an object of its own, whether or not it is stored as
one. `Map::value_hash(&key)` returns the same hash, from the stored pointer without reading a spilled
value, or by encoding an inline value that is already in memory.

Hashes are 160 bits, not 256. A link costs 20 hash bytes plus a varint offset, about 23 bytes
instead of 40, which cut the bytes written per single-key commit by about 19%. Second-preimage
resistance, which is what tamper evidence on read needs, is 2^160. Collision resistance is 2^80
instead of 2^128. Anyone who can pick the bytes of two objects and can afford about 2^80 hash
evaluations can make two different objects share a hash, and ferro would then treat them as the
same content in `==`, `diff` and write deduplication. Nothing in ferro authenticates against such
an attacker; the superblock checksum is unkeyed. Raising `HASH_BYTES` to 32 restores the full
claim at the cost of those bytes.

## Records

A record is `varint stored_len`, `varint (link_count << 1 | compressed)`, an optional
`varint raw_len`, the body, then the links. Each link is its hash followed by a zigzag varint of
`record offset - target offset`. Children are written before parents, so the deltas are small and
positive in the common case, and nothing in a record depends on where it sits except that delta.
Bodies of 96 bytes or more are LZ4 block compressed when that makes them smaller. Compression
runs after hashing and decompression before the hash check, so the hash covers the canonical
body and two files with different physical compression still agree on every root hash. The
decoder rejects a claimed expansion beyond LZ4's 255:1 limit before allocating.

## Deduplication

`Txn::put` skips an object whose hash it finds in `dedupe`, a bounded table from hash to offset.
It holds what this process wrote and what it read, for objects of at least `SPILL_BYTES`, the size
at which a value becomes its own object. Smaller objects are cheap to write again, and registering
every read cost 3 to 11% on cold gets through a writer. Only objects read are registered, after
their hash check. Offsets are not hashed, so a link in a node proves nothing about the offset it
carries until it is followed, and registering links would let a damaged offset spread into new
commits. A read-only handle registers nothing.

So rewriting a value costs nothing if this process read the old one, whichever process wrote it.
`get_mut` on a spilled value needs no special case: the slot becomes `Fresh`, the value is encoded
again, and the hash finds the old object. Keeping the pointer in the slot would save only that one
lookup, because the encode and hash are needed to know the value did not change, and it would cost
every slot eight bytes.

It does not close the gap for code that replaces a collection without reading it. In `chunkexp`,
1000 edits to a 100 KB page as a `List<String>`:

| Update | Warm process | New process per edit, before | New process per edit, after |
| --- | --- | --- | --- |
| Rebuilt from a fresh `collect` | 2.83 MB | 87.1 MB | 87.1 MB |
| Read, then `set` the chunks that differ | 2.75 MB | 4.53 MB | 2.76 MB |

A rebuild reads nothing, so nothing is registered. Finding an unread object by hash would need an
index on disk or a scan at open, and neither is justified by one write pattern that can read first.

The table is two-way set associative. A set is one 64-byte line holding two full hashes and their
offsets behind a one-byte lock, so a lookup or an insert touches one line. A hit moves to the front
of its set when it is looked up, under the same lock, and when it is inserted again, and a new entry
pushes out the older way. It has one entry for every 4 KiB of
`cache_bytes`, between 1,024 and 65,536, and is allocated in 4 KiB chunks as entries first land in
them, so an open costs nothing and a writer that has made a few thousand commits holds about 2 MiB.
It was a `quick_cache`, and a booking commit registers and looks up about 36 objects. Once that
cache held its 65,536 entries every insert evicted through its clock lists, and every insert and
lookup took several cache misses in its shard map and slab: 14% of a booking commit's time went
there. With the table, thread CPU time outside the closure fell 7 to 12% for a booking hold and 4 to
9% for a confirm, and the hit rate held: bytes per write went from 12,210 to 12,208.

Lookups did not always promote: a hit was moved only if the commit registered the object again,
which `put_packed` does and `put` does not. Promoting every hit makes no difference at the default
size. Over 4,000 measured `tables` hold and confirm pairs with the default cache, hits went from
15,373 to 15,391 of 340,455 lookups and bytes per write stayed at 12,208. With `cache_bytes` at
4 MiB, which gives the minimum 1,024 entries, hits rose 6%, from 13,878 to 14,756, and bytes per
write fell from 12,225 to 12,214. Instructions per commit moved by less than 0.15% either way and
thread CPU time by less than the noise between runs. The map and table commits in `ferro-bench`
find nothing in the table, so promotion has nothing to do there.

## Canonical tree

`Map` is a Merkle search tree. A key's level is `trailing_zeros(xxh3_64(encoded key)) / 3`, so
about 1 in 8 keys is promoted per level. The level hash is fixed-seed and not collision resistant:
someone who can choose keys could craft many keys of a high level and unbalance the tree, which
costs performance but not correctness, because node hashes are still BLAKE3. xxh3 replaced BLAKE3
here because hashing a key was the largest per-insert cost; level frequencies on sequential
integers, strings and tuples match the ideal geometric distribution. Fanout 8 was chosen by
measurement and re-checked after links shrank, bodies were compressed and reads gained child
memos, at 1M keys: against fanout 16 it writes 27% fewer bytes per commit for 31% slower warm gets
and 29% faster cold gets, and against fanout 4 it writes 30% more bytes per commit for 12% faster
warm gets and 17% smaller bulk loads. A node holds all keys of exactly its level within its range,
and children hold the gaps. Tree shape depends only on the key set, never on insertion order.
Consequences that the rest of the system relies on:

- `a == b` for two maps compares root hashes in O(1).
- A commit that changes nothing has the same root hash and is skipped.
- `diff` skips any subtree with matching hashes, so it costs O(changes), not O(size).

Insert and remove use treap-style `split` and `merge`, and so do the range operations:
`take_range`, `remove_range` and `split_off` cut the tree along one or two paths and `append` joins
two maps whose key ranges do not overlap, so each touches O(log n) nodes. `remove_range` returns
the count stored on the cut-out subtree links and never reads those subtrees. At 1M keys it took
0.8 to 2.5 ms where collecting the keys and removing them one by one took 5 to 260 ms, for the same
bytes written. A cut that moves nothing keeps each node's `origin`, so it writes nothing back.
`Table::drain_range` has to update every index for every row, so it is O(rows) and returns them.
`append` across two stores, or with overlapping keys, copies `other` entry by entry, since
links only mean something inside their own store.

Mutation is copy-on-write through `Arc::make_mut`, so a node is copied once per transaction and
then mutated in place. `Link::Mem` is a dirty in-memory node, and `Link::Stored` a persisted one.
`flush` writes dirty nodes bottom
up during `Data::encode` and seeds the node cache with the nodes it wrote. Values inserted or
mutated since the node was loaded are decoded once as a check before they become durable;
unchanged values are not. When at least half of a node's values are fresh, the cached copy is
decoded from the written bytes, which is that check and the copy in one pass; otherwise the node is
cloned and each fresh value is the one its check decoded. A value holding a dirty or foreign
collection also forces the decode, so nested collections always come back clean and a foreign store
is never pinned.

A node that `flush` writes or decodes keeps the bytes and links of its level, keys and values while
its values are still one column, which they stop being at the first edit of a key or value, since
every such edit goes through `Cells::slots`. A node on the path to a change has a new child and the
same entries, so the next flush copies those bytes and encodes only its child links. The kept bytes
count toward the node's cache weight, and the file comes out byte for byte the same. The parallel
flush below keeps none, so a bulk load holds no more memory than before.

A dirty node of at least 4096 keys with two or more dirty children and at least 256 dirty nodes
under it is flushed in three phases.
First, every dirty subtree under 4096 keys is encoded, hashed and LZ4 compressed on up to 16 scoped
threads, each into its own buffer, and the nodes above them on the calling thread. Second, the
calling thread appends the records to the transaction in the order a one-thread flush would, so
the file comes out byte for byte the same. Third, the threads decode the cached copies from those
buffers, which also checks every value and leaves the same `Weak` memos and hints a one-thread
flush leaves, as described under Read path, and the calling thread hands them to the cache in the
same order. The first phase encodes against a sink that refuses writes and links, so a value that
holds a dirty collection or a raw `Ptr` sends the node back to the one-thread path. The threads
are scoped and the caller blocks on them: a commit holds the branch lock, and a work-stealing pool
thread waiting on a join could pick up another task that commits to the same database and
deadlock. At 1M keys the commit went from 476 to 98 ms and `root_hash` on a dirty map from 209 to
32 ms. The decoded copies land on the threads' heaps, so where they are built matters: building
them in the first phase, next to scratch buffers that are freed later, spread them over 2.5 times
as many pages and made the next scans 10 to 15% slower. Built in the third phase, after each
subtree's buffers are released, the first pass of gets, scans and ranges after a bulk load is as
fast as before or up to 7% faster, and peak memory rose 2%, from 362 to 371 MiB at 1M keys. Each
phase spawns its threads, so a 10,000-row table commit, with little to share out, gains up to 5%
for 50% more CPU time. The rule first stopped at two dirty children, so a commit that changed two
keys under different children of a large tree's root spawned threads in both phases to flush a
handful of nodes. A booking confirm, which moves a booking from one status to another in an index
of several thousand entries, did that on every commit, and lost a third of its CPU time outside the
closure and nearly half of its wall time to it. 256 dirty nodes are about a millisecond of work on
one thread.

Most of a 1M-key bulk load is still the per-key inserts, about 60%, which miss the CPU cache a few
times per level, and dropping the in-memory tree after the commit, about 25%. Measured and
rejected:

- Dropping the old tree on a background thread. It took about 300 ms off a 1M-key load, but its
  frees contend with every other allocation in the process: the gets right after the load were
  about 25% slower, reopen 0.2 ms slower, and a table loaded in 50,000-row commits up to 25%
  slower unless each commit first waited for the last drop.
- Freeing in parallel. Dropping 1M `Arc<String>` in random order took 192 ms on one thread and
  310 to 900 ms on 2 to 16, since every free takes the lock of the arena that allocated it.
- rayon. As fast as the scoped threads, but it can deadlock as above, and its `par_sort_by` did
  not beat the standard stable sort on 1M shuffled pairs, about 33 ms either way.
- Reserving the transaction buffer up front, and keeping LZ4 off when it does not pay. The first
  changed nothing measurable, and without LZ4 a 1M-key load is 17% larger and a table row 80%
  larger, so compression stays and runs on the threads.

`Extend`, `FromIterator` and `apply`, which also takes deletes, sort the batch and keep the last
edit per key. Batches of 16,384 keys or more hash their key levels on the scoped threads described
above. An empty map is assembled bottom-up in one pass, which gives the same tree as
inserting key by key. A non-empty map takes the batch in one descent: each node sorts the edits
into its gaps, applies the ones at its own level in place, and passes each group to one child, so a
subtree no edit falls in stays shared and a node is walked once however many edits it receives.
Editing the node in place, not rebuilding its vectors from the old and new entries, is what makes
this pay: the rebuilding form was up to 28% slower than per-key inserts. The gain is modest, because
a touched node has to be read and copied either way and `flush` costs three to four times the tree
work. At 1M keys the closure takes 5 to 25% less time than sorted per-key inserts from 1,000 to
100,000 edits and 30 to 37% less for 1M edits, the commit as a whole 0 to 22% less, and batches
under 1,000 edits show no difference. A 50,000 page wiki import, 20M posting entries, gained 4% in
its posting phase. The metrics load test gained nothing measurable, since chunk merging dominates
there, so its ingest keeps plain inserts. `retain` collects its deletes and applies them the same
way. A disjoint batch needs no special case, since the same descent walks one edge of the tree.

Values over 512 encoded bytes spill to their own object, so a large value does not bloat its
node or get rewritten when a neighbour changes. Lower thresholds were measured and rejected:
spilling interior-node values over 64 bytes cut table update bytes by 19% but grew bulk size by
14% and slowed table reads by 20 to 50%, once compression was already shrinking interior nodes.

`Vec<u8>` and `[u8; N]` copy their bytes in one go, through the `encode_slice` and `decode_vec` hooks
on `Data` that `u8` overrides, which took a 1 MiB value from 5.3 ms to 0.49 ms to commit and from
2.0 ms to 0.16 ms for a cold read. `Bytes` has the same encoding and adds an O(1) clone and
`Borrow<[u8]>`, so `Map<Bytes, V>` looks up by slice. It copies once on decode. Sharing the object
buffer instead gained 8% on a cold read at 1 MiB and under 4% at 64 KiB and 1 KiB, where `pread` and
BLAKE3 dominate, for a wider handle, a change to the read path, and a small value that pins the
whole node it was decoded from. `#[ferro(transparent)]` gives a newtype the encoding of its field.

## Summaries

`Map<K, V, S>` with `S: Summary<K, V>` keeps a fold of every subtree. `of` summarizes one entry and
`combine` joins two neighbours, left then right, so `S` needs only to be associative: min, max,
first, last and concatenation work, not just sums. There is no identity element, because an empty
range is `None`. `range_summary` and `summary_by` fold a range in O(log n), and `summary` reads the
root in O(1). Rank in a range is a difference of two prefixes, but a general summary has no
subtraction, so a fold walks both edges of the range and combines the links between them.

Nothing ever updates a summary. `flush` computes each node's from its entries and its children's
links, bottom up in key order, and stores it on the link in the parent next to `count`. So a summary
is a function of the tree below it, the tree is a function of the key set, and root hashes stay
independent of history. A `combine` that is not quite associative keeps that property: a float sum
rounds the same way for the same contents, whatever edits led there. `split`, `merge`, `absorb` and
the rest move links without looking at them, and any node they change is dirty, so none of them
needed to know. Applying a delta to the old summary would have been cheaper per edit and would have
broken canonical form for floats, so it is not done even for sums.

`of` must depend only on the encoded key and value, so a `#[ferro(skip)]` field must not feed it. A
read of a dirty tree has no stored summary for the nodes changed in this transaction and folds them
on demand, stopping at stored links, so it costs O(log n) plus the dirty nodes in the range. A
spilled value keeps its summary in the slot, so neither a fold nor a commit reads the value. Without
that, a commit would read every spilled neighbour on its path.

`S` defaults to `()`, a zero sized type, so the plain `Map<K, V>` has the layout, the bytes and the
speed it had. `()` encodes to nothing, `a_plain_map_keeps_its_format` pins a root hash from before
summaries existed, and alternating `ferro-bench` runs moved no byte metric and no timing beyond
noise. A default type parameter takes no part in inference, which is why `HashMap::new` exists only
for the default hasher, and `Map::new` is the same: it is defined for `S = ()` alone, so
`Map::new()` still infers, and no caller in the workspace changed. A summarized map starts from
`Map::default()` or `collect`.

Changing `S` changes a map's encoding under the same field tag, as changing `V` does. Rename the
field to rebuild it.

The summary is on the link, not in the child, because a parent can then be rebuilt and a covered
child skipped without reading that child. Inside the node a commit saves bytes, but `flush` has to
read every clean sibling on the path. At 1M `u64` keys with an `i64` sum, against 3,095 bytes for a
plain single-key commit:

| Summary stored | Bytes per commit | Commit, 1 MiB cache | Fold, 1 MiB cache |
| --- | --- | --- | --- |
| On the link | 3,333 (+7.7%) | 28 µs | 11 µs |
| In the node | 3,125 (+1.0%) | 80 µs | 57 µs |

Bytes follow the width of `S`, since a rewritten interior node repeats about nine summaries. Bulk
bytes barely move, because every node has one link either way. `ferro-bench`'s `summary` tool, at 1M
keys:

| Summary | Bytes per commit | Commit | Bulk bytes per key |
| --- | --- | --- | --- |
| None | 3,095 | 33 µs | 17.8 |
| `i64` | 3,333 (+7.7%) | 36 µs | 18.1 |
| `i128` | 3,403 (+10%) | 37 µs | 18.3 |
| Four integers in a tuple | 3,874 (+25%) | 41 µs | 19.0 |
| The same four in a `derive(Data)` struct | 4,623 (+49%) | 51 µs | 20.5 |

Field tags repeat on every link, which is the last row, so a wide summary should be a tuple. `i128`
is the natural total for `i64` values and was 16 fixed bytes, so it now encodes as two zigzag
varints, which took the ledger's file growth from 10.3% to 5.6%. Against a plain map over five
runs, full scans took 0% more with the `i64` total and 5% more with the others, and gets 1% more
with a total and 9% more with four fields. Many short scans of a map with a wide summary pay more:
56 bytes on each link cost `metrics` 25% on its scan path, which folds avoid.

Hand-kept alternatives were built into `ledger` and measured at 1M entries. Every row gave the same
balances:

| Approach | Added lines | File | Bytes per commit | Backdated commit | Balance as of | Trial balance | Cold trial balance |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Scan postings | 0 | 151 MB | 12,178 | 11,492 | 16 ms | 440 ms | 328 ms |
| Summary | 15 | 160 MB | 13,144 | 12,362 | 0.7 µs | 30 µs | 0.8 ms |
| Daily rollup | 10 | 176 MB | 15,544 | 14,905 | 16 µs | 0.7 ms | 5.6 ms |
| Running balance every 32 days | 37 | 157 MB | 14,210 | 14,017 | 85 µs | 1.6 ms | 9.3 ms |
| Rollups by 1, 32 and 1024 days | 28 | 184 MB | 17,224 | 18,037 | 2.6 µs | 65 µs | 0.8 ms |

The running balance has the smallest file of the hand-kept forms because a bulk commit touches one
row per account per block, but each backdated entry rewrites every later block. The hand-kept forms
answer only at day boundaries and are state that can drift from the postings. A summary works on any
key range and is computed from them. `audit` still compares the fold with the journal, because
`verify` knows no types and `verify_typed` checks a summary's type, not its value.

`metrics` keeps rollups in a summarized map and folds a bucket that spans at least 8 of them, which
is where folding starts to beat scanning. On 4.32M rollups of 1,000 series: 978 ms to 5.6 ms for one
bucket, 1,067 ms to 79 ms at one day per bucket, and 889 ms to 192 ms at six hours, for 9% more
bytes per rollup. `Rollup` is a fold of its own points, so it needed one `combine`.

Measured and left out:

- A `#[sum]` attribute on `Record`. A summary needs the value in the tree, and an index holds only
  keys, so this means a second tree with one entry per row. For total balance by country on 200k
  rows that was 7,333 bytes per update against 4,765 and a 22% larger file, and 0.75 µs a read. A
  hand-kept `Map<country, i128>` costs 4,887 bytes and 0.02 µs, against 16 ms to walk the index.
  A summary only wins on sub-ranges of an ordered key, and for those a `Map<(group, key), V, S>`
  with `summary_by` is a few lines.
- A summary on `Table` rows. It adds a type parameter to `Record`, `IndexIter` and every table
  iterator, for sums over primary key ranges that nothing here needs.
- A cached summary on dirty nodes. It would need invalidating on every path through `Arc::make_mut`,
  to speed up reads inside a transaction, which are rare.

## Read path

A warm get at 1M keys walks about seven levels, and nearly all of its time is CPU cache misses, not
logic. One node cache lookup per level was 55% of the get. Reference counts on nodes near the root,
written by every reader, held eight readers to about 10M reads a second. At 10,000 keys, which fit
in L2, a scan costs 2 ns a key and a get 67 ns. At 1M keys the same code costs 6 to 8 ns and 240 to
340 ns, so memory, not instructions, is what the read path has to save.

So each `Link::Stored` carries a write-once memo of the node it points at, in a `OnceLock`:

- The first `PIN_DEPTH` (3) levels of a tree memoize a strong `Arc`. `Map::walk` follows those by
  reference and writes no shared memory, which took eight readers from 9M to 60M reads a second. A
  fourth level added nothing. A `Map` pins about 73 nodes whatever its size, outside `cache_bytes`,
  until the `Map` is dropped.
- Deeper links memoize a `Weak` when the child was found in the cache, or when `flush` wrote it
  beside a sibling, as described below. The cache stays the owner, so eviction still frees the
  node, and the memo keeps only the node's `Arc` allocation, about 150 bytes, until its parent goes.
  A child just read from disk is not memoized, so a one-off read leaves nothing behind.
- A memo is never replaced. Once its child is evicted the link falls back to the cache, whose `get`
  restores the recency that memo hits skip, so eviction keeps seeing nodes that are really in use.
  A replaceable `ArcSwap` memo made cold gets 44% slower, because a small cache evicts constantly
  and each `ArcSwap` store has to settle with every reader thread.

A memo holds only what a load for that link's own `Ptr` returned, and the cache is keyed by the full
hash, so a damaged link still fails its hash check whatever else is cached. `Link::clone` starts
with an empty memo, since copying one touches the child's header, a cache miss per link that slowed
`diff` by 45%. Only `Map::clone` keeps its root's memo.

Leaves are 7 in 8 nodes, and their `children` vector is empty rather than a row of `Link::Empty`.
`Node::child` reads through that and `Node::children_mut` fills it in when a leaf gains a child.
This cut peak RSS by 28% and bulk load time by 4 to 14%. `Node` is `repr(C)` with `keys` and
`children` first, so the first cache line of its allocation holds both pointers a walk follows,
which took 12% off `contains_key` and 6% off `get`.

### Prefetching the next level

Memos took the cache lookups out of a get and left the misses. Below the pinned levels a get still
waited, level after level, for the link, then the node header that `Weak::upgrade` reads, then the
key array behind that header, and then the link its search picked. A `Weak` memo now also carries a
`Hint`: where its node keeps its keys and, for an interior node, its child links, or for a leaf its
value column, as two 32 bit offsets from the node with a count of cache lines in their low four
bits. `reach` prefetches those lines before it upgrades, so the header, the keys and the links of
the next level arrive together, and a get waits about once a level instead of three times. The
offsets fit beside the `Weak` in space the enum already had, so `Memo` is still 16 bytes and `Link`
72. A hint comes from a live node, and once that node is evicted a stale hint prefetches a line
nobody wants and nothing more, since a prefetch never faults. That call is the crate's one `unsafe`
block, and it does nothing on targets other than x86_64.

On 1M `u64` keys, median of 15 alternating rounds in one process holding two such maps, in
nanoseconds, the last two per key:

| Memo holds | `get`, string value | `contains_key` | `get`, then read it | `get`, `u64` value | Scan | 1000-key range |
| --- | --- | --- | --- | --- | --- | --- |
| The node | 549 | 471 | 642 | 578 | 33 | 48 |
| The node and a hint | 325 | 276 | 334 | 293 | 22 | 35 |

The price is a few dozen instructions a level and lines fetched that a read may not use. Gets on
10,000 keys, which stay in L2, were 6 to 12% slower until the linear search below won that back,
and walking an index's keys alone, at about 6 ns a key, is 7 to 15% slower.

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
took 7 to 11% off gets and `contains_key` at 10,000 keys and up to 5% at 1M. Keys that own heap
keep the binary search, since every comparison would chase a pointer.

`flush` leaves every node it writes in the cache, but it used to hand back links with empty memos,
so the first read of each node after a bulk load went through a cache lookup: the first pass of gets
after loading 1M keys took 460 ns against 365 ns warm. A link it writes now gets a `Weak` memo and
its hint when the node sits below the pinned levels and its parent rewrote more than one child, and
that first pass takes 335 ns. A single-key commit rewrites one child a level and memoizes nothing.
Memoizing every written node grew the live heap of `heap`'s commits and reads case by 4%, from 46.5
to 48.4 MiB with a 64 MiB cache, because the old paths of single-key commits kept the `Arc`
allocations of their evicted children. With the rule it is 46.7 MiB.

### Values

A node holds its values in one `Arc<[V]>` column in key order, beside its keys. There used to be one
`Arc<V>` per value behind a 40 byte `Slot`, which is 72 bytes and a separate allocation for each
`u32`, and even for the `()` of a `Set` or an index entry. A `Slot` now exists only for what a column
cannot say: `Fresh` for a value changed since it was read, `Spilled` for a value in its own object,
and `Packed` for an unchanged value in a node that has one of those. `Cells::slots` turns a column
into slots at the first edit of that node's values, which takes one reference count per value and
copies none. A node on the path to an edit only gains a new child, so it keeps its column, and
copying it costs one reference count where it used to cost one per value. That is why commits got
cheaper: 34% for small values at 1M keys and 18% for short strings.

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

The bulk figure is the worst case, a short `String` per key, and building the column without
copying the slots first won most of it back. Over five alternating runs of `reads`, a bulk load
through `extend` took 24% longer for short strings and 2 to 5% longer for values with no destructor,
and `ferro-bench` could not tell its bulk loads apart from noise.

### Scans

`Iterator::fold` is overridden on `Iter`, `Keys` and `Values` to take a run of a node's entries in
one loop, which `sum`, `count`, `for_each`, `filter` and `map` reach with no new API. `try_fold`
cannot be overridden, because naming `Try` needs a nightly compiler, so `find`, `any` and `Take` go
through `next`, as do `for` loops and `collect`. A fold still builds an item, so it clones the key
and, for a value held by column, takes a reference count. `Map::for_each`, `try_for_each`,
`for_each_range` and `try_for_each_range` lend `&K` and `&V` instead and pay one reference count per
node, not per item, with a `ControlFlow` for early exit. In nanoseconds per key at 1M keys, reopened,
median of five alternating runs:

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
With memory out of the picture at that size, the floor is 2 ns a key for a fold or `for_each` and
9 ns for a `for` loop. A `for` loop over strings gained nothing, because the same two atomic
operations now act on a wider handle.

### Sorted lookups and cursors

A `Cursor` keeps the path to its position, one `Arc<Node>` per level. `seek` climbs only until a
node's key span holds the target and searches down from there, so it costs the distance moved, not
the height of the tree. Both ends of the span are checked, so a seek backwards is as cheap as one
forwards and sorted input is not an obligation. `seek_back` finds the last key at or before the
target, and `advance` steps in key order. `value` returns a `Val`. `read` lends the entry, loading a
spilled value first.

`get_many` and `contains_many` run a cursor over an iterator of keys. They yield each key with its
result, so they compose like `map`, and keys may be owned or borrowed. On 1M `u64` keys, median of
five alternating runs, in nanoseconds per key:

| Keys, sorted unless noted | `get` before | `get` | `get_many` | `contains_many` |
| --- | --- | --- | --- | --- |
| Every second key | 115 | 85 | 26 | 28 |
| Every 16th key | 464 | 233 | 153 | 97 |
| Random order | 586 | 342 | 363 | |

Unsorted input costs 6% more than plain `get`, which is the price of keeping the path. A batch that
recursed through the tree, cutting a sorted slice at each node as `absorb` does, was 6% faster at the
densest and 7 to 26% slower when sparse, and needs a slice, a callback and sorted input, so the
cursor replaced it. A cursor also serves merge joins, which a batch cannot.

`range_by` used to walk its two edges separately, adding a rank at each level to learn its length.
They now share one walk until they part and count only the links between them, so a narrow range
reads each node once and sums almost nothing, and `range(key..).next()` went from 860 to 565 ns.
`floor` is one walk that remembers the last node with a key before the target. Built on a cursor it
was 20% slower than `get`, and it would have written the reference counts of pinned nodes. `Set::range`
returns the named `Keys`, so it has `len` and `next_back`, and the set operations probe through
`contains_many`.

`IndexIter` fetches rows through a cursor. Rows of one index value sit in key order, so those went
from 163 to 60 ns each, and rows scattered across the table from 208 to 161 ns. `IndexIter::keys`
stops before the rows and takes 5 ns a key.

### What is left, on `examples/graph`

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
costs, even with a decode loop that has no branch per byte. A decoder that took eight bytes at a time
was 24% slower, because each block ends in a tail it has to take byte by byte. The other half, 1.7 ns
per edge or 75 ns per block of 44 edges, is walking keys and reaching each block's bytes, which has
no counterpart in a `Vec`. `Vec`'s 0.12 ns is a SIMD sum that does no work per edge: pagerank's real
work is 0.9 ns per edge on `Vec` and ferro pays the same on top of its scan. A random seek is 1.1 µs
because seven levels, in a heap far larger than the last level cache and the reach of the TLB, take
several DRAM misses where a `Vec` takes two. Neither the walk nor the decoding goes away without a
different structure. What a `Vec` cannot do at any price is take a commit or keep an old version.

### Measured and left out

A node miss is dominated by BLAKE3, at roughly 500 to 900 ns on an interior node against 130 to
190 ns for `pread`, and the hash check is not negotiable. Reads fetch 1 KiB at a time, which fits
most records. `Decoder::varint` takes a one byte varint inline, which took `Vec<u64>` of small
numbers from 6.1 to 1.5 ns an item and cold scans down about 7%.

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
- A chunked or trie `List`. `List<T>` stays a `Map<u64, T>`, and a push rewrites the right spine with
  the values held at each level: 1.6 KB for a `u64` and 3.2 KB for a 60 byte string at 1M items, or
  1.3 and 2.5 KB at 100k. Chunks in a `Map<u64, Vec<T>>` did not win clearly. At 1M, 16 per chunk took
  the string case to 2.4 KB but the `u64` case to 2.9 KB, and 64 per chunk to 5.9 KB, because a node
  then holds several chunks just under the spill limit, while cold gets rose up to six times. A
  vector trie of fanout 8 with values only in its leaves came to about 1.0 and 1.2 KB in a simulation
  built from real record sizes, but at that fanout it has as many levels as the `Map`, so it would
  read no faster, and fanout 32, which would, gave the bytes back at 2.0 and 2.5 KB. It helps one
  shape, a long list appended to one item per commit, and costs a second tree beside `Map` with its
  own nodes, flush, memos, diff and foreign store handling. Bulk appends already go down one edge
  in one descent.
- Scan admission control. `quick_cache` starts new entries in a small cold section, so a full scan
  leaves hot-set latency unchanged. Not admitting scanned nodes saved 7 to 17% on scans larger
  than the cache, but made repeated short range pages 3 to 4 times slower, and a budget per iterator
  that avoided that saved only 6 to 8%.

### Cache weight

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
vector holding it, so walking history in a fresh process kept roots whose heap was nearly four
times their weight. The `history_heap` test walks 20,000 versions of a root holding a table, a map
and 200 strings. At a setting of 2 MiB the heap peaked at 7.1 MiB reading every version and at
4.4 MiB through `history_of`, and at 32 MiB at 93 and 74 MiB. It now peaks at 1.1 and 1.3 times
the setting at 2 MiB, and at 1.05 times at 32 MiB. Counting costs an add per allocation on a cache
miss and nothing on a hit.

Outside the bound are pinned nodes, including those the maps of a cached root pin, the `Arc`
allocations memos keep, the column of any `Val` still held, nodes held by live snapshots and dirty
trees, and on a writer the `dedupe` cache, which scales with the setting at about 2% of it up to
65,536 entries. A history walk that reads sixteen maps in every version peaks at 1.2 to 1.4 times
the setting, mostly from those pins.

The default rose from 256 MiB to 1 GiB, which is about what the old setting held once full.
A setting carried over from before that holds about a quarter of what it did. `map_get_cold_ns` is 5.4 µs at 1 MiB and 3.8 µs at
5 MiB, against 7.4 and 5.2 µs with one `Arc` per value, because the same bytes now hold twice the keys.

## File format and durability

Two 4 KiB superblock slots, then the log. A superblock names the head and the log end, records the
size the file had when this generation began and how much of the previous one survived, and carries
an anchor, the newest position known to be durable. The log is a run of frames, one per commit. A
frame is a 68-byte header (magic, sequence, end offset, head pointer, xxh3-128 check) followed by
the records the commit wrote. The check covers the header and every record, so a frame proves by
itself that it reached the disk whole. A commit does this:

1. Write the frame at the log tail.
2. fdatasync. This is the durability point.
3. Write the superblock to the other slot, without syncing it.

In `Durability::Fsync` the superblock is written only after its frame is durable, so it anchors
itself and never names anything a crash can lose, and the next fdatasync carries it to disk. A crash
leaves it at most one sync behind. On Windows every fdatasync here is a `FlushFileBuffers`, as
described under Windows.

Open picks the valid slot with the highest sequence and scans forward from its anchor. A frame
counts only if its magic, sequence (the previous one plus 1), end offset and check are all right,
and the first that is not ends the log. A torn tail, zeroes and garbage all stop the scan, and an
acknowledged commit survives a stale or torn superblock. A writer then syncs the frames it
accepted, truncates the rest, and rewrites the superblock, in that order. The truncation is durable
before any new frame is written, so a frame stranded past a torn one can never line up with the
log later.

`Durability::Buffered` skips step 2, so a power cut can keep a superblock and lose the frames it
names, and open used to fail on that file. It may lose recent commits by contract, but it must
always open. So a Buffered superblock keeps the anchor of the last checkpoint: a position an
fdatasync has covered. The leader of the batch that takes the log past `Options::checkpoint_bytes`
of unsynced frames, 64 MiB by default, syncs once it has published and woken that batch, so only its
own commit waits for the disk, and a clean close syncs as well. A scan from the anchor ends at the first frame the power
cut damaged, whatever the superblock claims. Only frames past the anchor can roll back, so a
superblock that anchors itself still reports a hole in its log as corruption, not as a short log.

A rollback rewrites both slots, syncing between them, so a reader that opens beside the new writer
trusts a superblock that names only surviving frames. A read-only open cannot rewrite anything. It
scans from the anchor when no writer holds the lock, and remembers the superblock it refused so
that `refresh` ignores it until a writer replaces it. Keeping the anchor in the superblock rather
than dedicating one slot to it means neither slot has to be synced on its own: every superblock
that survives names an anchor that was durable before that superblock was written.

Recovery reads what lies between the anchor and the tail. With the frames still in the page cache,
that took 13 ms for 64 MiB and 54 ms for 256 MiB, where a clean open takes 0.02 ms. The checkpoint
is the cost: the commit that crosses the line waits 27 ms for 64 MiB to reach the disk here, which
averages out to 1.4 µs on every Buffered commit of `ferro-bench`, 25.8 to 27.3 µs, while p99 does
not move. The anchor adds 44 bytes to a superblock, one more BLAKE3 block per commit. Syncing on every
Buffered commit would have closed the bug too, and made it Fsync. Writing the superblock only at
checkpoints would have left followers a checkpoint behind, because `refresh` must not scan past a
superblock while a writer is alive. The fix was tested only against modelled power cuts: every
prefix of the recorded writes with all, none, a random half or torn sectors of the unsynced ones
kept, and the superblocks kept without their frames.

A frame past the superblock may not be durable while a writer is alive, so `refresh` reads only
the superblock and never scans. A read-only open scans only when a frame follows the superblock or
the superblock does not anchor itself, and it can take a shared lock, which proves no writer is
alive. It holds that lock for the scan and syncs what it accepted, since a dead writer's frames may
exist only in the page cache. If a writer holds the lock, the superblock is current and the reader
uses it. A writer opening during that short scan retries for a few milliseconds before reporting
`Error::Locked`.

fdatasync after extending a file has to commit the ext4 journal, which took 620 µs against 360 µs
for overwriting allocated blocks on the test NVMe. So in `Durability::Fsync` the log grows in
zero-filled chunks, doubling from 64 KiB to 4 MiB, and frames overwrite blocks that are already
written, which needs only the device flush. The zeroes must be real: `posix_fallocate` leaves
unwritten extents, and converting those commits the journal just the same (640 µs). The first
commit of a process reserves nothing, so a short run pays for no zeroes. The reservation is only
an optimization. A failed reservation stops further attempts without failing the commit, open
discards it, and a clean close truncates it, so the file can be up to 4 MiB longer than
`size_on_disk` while open.

A failed write or fsync poisons the store until reopen. Only one process may hold the writer lock.
Read-only opens otherwise take no lock, since log bytes under a published head never change, and
`refresh` picks up new commits. It reads the new refs and the state of every attached branch before
it changes anything, holding the published refs so that no branch attaches in between, and only then
records the head, swaps in a replaced file and publishes. A refresh that fails partway, say on a read
error, therefore leaves the handle as it was, and the next one starts over and catches every branch
up. It used to record the head first, so the next call saw nothing new and the branches the failed
one missed stayed behind until another commit landed.

## Concurrency

Writers run their closures one at a time under the `tip` mutex and sync outside it. Each branch
has two states: `state`, the published one that readers load through `ArcSwap`, and `tip`, the
newest one written. Under the mutex a writer clones the tip, runs the closure, flushes, writes its
frame to the page cache, queues it, and releases the mutex. The next writer builds on that tip
even though it is not durable yet. Object reads are bounded by `written`, the end of the frames in
the file, rather than `tail`, the published end, so unpublished objects stay readable to writers.
Published pointers never reach past `tail`.

Each writer then waits for its frame. The first to find no sync running becomes the leader: it
takes the whole queue, fdatasyncs, writes one superblock for the last frame, publishes the batch
in order, and wakes the others. Writers that arrive during the sync form the next batch, and one
of them leads it. A lone writer leads its own one-frame batch, which adds no syscall and no thread
switch. `Durability::Buffered` takes the same path without the fdatasync. A dedicated sync thread
for blocking writers was measured and rejected: it adds a wakeup to every uncontended commit and
gained nothing under contention. Awaited writes, which must not wait on a thread, do use one, as
described under awaited writes below.

Under `Durability::Fsync`, nothing is published or acknowledged before it is durable, so readers,
watchers and other processes never see a commit that could be lost, and they see commits in order.
Under `Durability::Buffered` the superblock can name frames that are not yet synced, and a
read-only follower that refreshes may adopt commits that a power cut later takes back. A closure
that ends without a frame, because nothing changed or it returned `Err`, may still have read an
unpublished tip, so it waits for that tip before returning, and reports the failure instead if the
tip never became durable. `fork` copies the tip and waits the same way, so the new branch appears
in `branches` only once durable. `branch`, `branches` and `verify` use the published refs and do
not touch the mutex.

If a sync fails, the store poisons, nothing in the batch is published, the leader gets the I/O
error, and every queued writer gets `Error::Poisoned`. A failed commit's frame may still be on disk
and reappear after reopen: an error means not acknowledged, not guaranteed absent. A failed frame
write poisons the same way, but frames queued ahead of it are whole, so they still sync and
acknowledge.

Writing frames with `O_DIRECT|O_DSYNC` was also measured. On the test NVMe a forced-unit-access
write is cheaper than a cache flush, which cut single-writer latency by 26% and raised contended
throughput by 40%. It was rejected because the gain exists only where FUA beats FLUSH, and the
writes bypass the page cache: reads after a bulk commit go cold, which made buffered commits after
a reopen 27% slower, and a read-only follower's `refresh` rose from 5 µs to 117 µs. It also needs
an in-memory pending log on the read path, a fallback, and buffered reads overlapping direct writes
on the tail block.

Each branch has one `Feed`: a mutex over the newest published snapshot and a publish count, and a
condvar. Publishing stores one snapshot and wakes waiters whatever the number of watchers. A
`Watcher` remembers the count it last saw, so a snapshot is never queued, and a consumer that falls
behind gets the newest one and uses `diff` for what it skipped, which costs O(changes). With the
channel this replaced, a watcher that never read grew RSS by 147 MB over 100,000 order commits
against 53 MB with no watcher. Now it is 53 MB either way, and commit throughput matches. The count
is compared rather than versions so that a read-only handle stays right when a branch is dropped
and forked again at a lower version. The feed starts out holding the branch's snapshot from when
the branch was attached, so a watcher made before any commit has a snapshot to compare against.

A `Branch` closes its feed when it drops, which is when the last `Db` handle goes, because
`Shared` holds every branch. Closing wakes parked watchers, the filtered and async ones through the
watch thread described below, and drops the held snapshot. A `Watcher` holds only the feed, so a
parked one keeps neither the file nor the writer lock. The price is that a snapshot nobody had taken
by then is lost.

`Watcher::closer` returns a `Closer`, a cloneable handle that ends that watcher and its clones from
any thread. It sets a flag under the feed's mutex and wakes that watcher's parked threads and
futures, so stopping a set of workers no longer takes a commit. `recv` then fails the way it does
when the feed closes, and any other watcher of the branch carries on. A closer holds only the feed
as well. A wake with no close has no user here: a timeout already covers what it would do, and the
queue only ever needed the end.

`Db::watch_when(test)` is a watcher that sleeps through commits that do not concern it. Its tests
run on a `ferro-watch` thread that the feed starts when a filtered watcher first parks or any
watcher first waits as a future. A watcher that waits is listed in the feed once, by a `Weak`, and
counts as waiting while a thread is parked on its condvar or a future holds a waker in it.
Publishing stores the snapshot, bumps the count and wakes unfiltered threads under the feed's
mutex, and returns a `Notice`. When the notice drops, after the publisher has let go of every lock,
it sets a `due` flag and unparks the watch thread, but only if some watcher is waiting. That is a
load, a swap and at most one unpark whatever the number of watchers, and nothing at all while nobody
waits. The watch thread clears `due`, takes the newest snapshot and the list, tests that snapshot
for each listed watcher that is waiting, wakes those that pass, and forgets the entries whose
watcher is gone. Commits that land during a pass set `due` again, so one pass stands for all of
them. A pass marks a passing watcher approved for that count, so the woken thread skips its own
test unless something newer has landed. A `watch_changes` watcher always tests again on its own
side, because its reference may have moved after the pass read it. A panic is caught on the watch
thread and counts as a pass, so that the watcher wakes and hits the same panic on its own thread.

Testing on another thread cannot reorder what a watcher sees: snapshots are stored in commit order,
a pass only decides whom to wake, and a woken watcher takes the newest snapshot under the mutex and
tests it if it is not the approved one, so nothing that landed while it was busy is missed. Since no
test runs on a committing thread, a test may commit, refresh or drop a branch: its own commit only
sets `due` again. Before tests moved to the notice, a committing test deadlocked on the leader's
own `syncing` flag or on `tip`, and while they ran in the notice, the call that published returned
only after them, so every commit paid for every parked filter. Now a slow test holds up no commit,
only the other filtered watchers of its branch.

Measured with `ferro-bench --bin watchers`: buffered single-key commits into a 100k-entry map with
N watchers parked in `recv` whose test `snapshot.get(&key).is_some()` never passes, then the time
from starting a commit to a passing watcher's return, on a 32-core machine shared with other work.
Before and after ran back to back. A second pair gave the same picture: commits of 14 to 16 µs
after, against 64 and 547 µs before at 1,000 and 10,000 watchers, and delivery within 1 µs of
before with no other watchers, 13 µs slower at 1,000 and 45 µs slower at 10,000.

| Parked watchers | Commit, before | Commit, after | Delivery p50, before | Delivery p50, after |
| ---: | ---: | ---: | ---: | ---: |
| 0 | 14.5 µs | 14.5 µs | 21.5 µs | 21.1 µs |
| 1,000 | 80.2 µs | 16.8 µs | 98.8 µs | 82.1 µs |
| 10,000 | 591 µs | 14.5 µs | 572 µs | 526 µs |

Delivery stays where it was because the work is the same, one test per waiting watcher, done by
one thread either way. What changes is who waits for it.

Waking every waiting watcher to test on its own thread was measured on the same bench and rejected,
both waking from the committing thread and from the watch thread. The wakes cost as much as the tests did, and the herd
then fights the publisher for the feed's mutex: at 1,000 watchers a commit took 614 µs and 432 µs
and delivery 1.7 ms at p50, and at 10,000 a commit took 10.9 ms and 5.5 ms and delivery 19 ms and
24 ms.

A long pass is shared. Once a pass alone takes over a millisecond and at least 1,024 watchers are
listed, the watch thread hands equal shares of at least 512 watchers to helper threads over
channels, one per four cores and between one and seven, and tests one share itself. It keeps a running cost per watcher for passes alone and shared, uses whichever
is cheaper, and tries the other every 64th pass. Sharing pays for heavy tests: with 3,000 chat
feeds at 1,000 posts a second, delivery fell from 4.4 ms at p50 alone to 0.9 ms shared. It loses for
cheap tests that read the same nodes of one snapshot from every thread: 10,000 single-key `get`s
took 0.58 ms alone and 1.2 ms over eight threads. With tests that read nothing it halved, so the
choice is measured rather than fixed.

`Watcher::changed` is `recv` as a future, built on `std::task::Waker` alone, so it runs on any
executor. A future that finds nothing new stores its waker in the watcher's gate while it still
holds the feed's mutex, so a commit between its look and its registration cannot slip past, and
lists the gate for the watch thread, which wakes it as it wakes a parked thread. Dropping the future
removes its waker. A `Watcher` keeps its place in an atomic, so that it is `Sync` and the future is
`Send`. A plain `watch` waited on as a future is woken through the watch thread too, so a commit
never runs an executor's wake code.

`Db::watch_changes(test)` passes the test the snapshot the watcher returned last as well as the new
one. The first reference is the feed's snapshot, taken in the same mutex hold as the count, so it
cannot disagree with the place, and `Watcher::seen` returns it. The chat example used to keep its
marks in a mutex shared with its test and hold that mutex across `db.read()` for the same
guarantee, and to refresh them before each `recv`. The reference lives in the gate, so clones share
it and a return by one moves it for all, only ever forward by count. It holds its snapshot, and so
its file across a cycle, until the next return, like the snapshot a consumer keeps to diff against.

`queue` used to wake every idle worker on every commit, and to commit a dummy `epoch` change at
shutdown. Now workers filter on a ready job in their own queues, and the reaper on something falling
due sooner than it planned, against a value it stores before it parks. With 8 and 32 idle workers a
commit used to cost 12 and 47 voluntary context switches, and then cost 2, as with no workers at
all. The time from enqueue to handler stayed within run-to-run noise, 2.0 to 2.5 ms at the median.
The watch thread adds one switch per commit while workers are idle, its own park after each pass:
in `ferro-bench --bin watchers --watchers 0,8,32 --test false`, a buffered commit costs no
voluntary switch with no watchers and one with 8 or 32.


A `Snapshot<T>` keeps its version readable for as long as it is held, even past pruning, and so
does any `Map` or `Table` cloned from it, and a `Log` or `History` handle. All of them hold the
`Store`, and so the open file, but not the writer lock. The lock is a `flock` on the writer's file
descriptor, or on Windows a one-byte `LockFileEx` range on its handle, and closing a writer's
store, when the last `Db` handle drops or a cycle retires a generation, truncates the reservation
and then unlocks, in that order, so a new writer can open while they live. They read what they read before. The new writer's open truncates only past the last
valid frame, and every published object lies before it. It appends after that, and a cycle writes a
new file and renames it over the path, so the old inode stays whole for whoever holds it. A closed
store writes nothing more, and a second close does nothing, so it never truncates under the new
writer. A `Map` from the old writer is foreign to the new one, since store ids differ, and is
rebuilt if committed there.
A closure works on a clone of the tip, or of its group's working copy, so a panic or an `Err`
discards its changes.

### Coalescing

Closures run one at a time, but each still encoded, hashed and compressed a whole tree path and wrote a
frame, though the next closure rewrote the same paths. On `queue` at 8 writers the lock was held for
37 µs a commit: 5 for the closure, 20 to encode and hash the tree, and 12 for the log entry, the
refs, the frame and reloading the tip. At 32 writers, with more cache misses, it was 64 µs, which is
what capped commits at about 15,000 a second, and bytes per job grew with the writers because more
rows were in flight.

`Options::coalesce(true)` lets closures that queue up behind one another share a flush, a frame and
a version. It is off by default, because it gives up what ferro otherwise promises, a version for
every closure:

- A version is a group. `Committed::version` is the group's, so several closures can return the same
  one. `at` cannot show the state between two closures of a group, because it was never written.
- `Commit::message` holds the group's messages joined by newlines, and `Commit::messages` lists
  them. A message may contain newlines itself, so the log entry records where each one ends: one
  flag bit marks a commit made alone, whose message is one message whatever it contains, and
  another marks a group, followed by the byte length of each message. Entries written before
  these bits existed have neither, and `messages` splits them on newlines as it always did, which
  is the only reading they allow.
- `history_of` and `Log` see groups. A row changed twice in a group shows once, with its last value,
  and a change undone inside a group does not show at all.
- Closures still run in order, each seeing what the ones before it did.
- `Committed::changed` says the group moved the version. A closure that changed nothing, in a group
  where another did, reports `true`. Telling them apart needs a hash of the dirty tree after every
  closure, which is the cost this removes. A group of no-ops writes nothing and reports `false`.
- A `Stamp` is the group's. Its version is the group's base version plus one, and its time lives in
  the group: the first closure to call `time_ms` fixes it, and later closures read the same value.
  A closure that fails or panics puts the time back unset if it was the one that set it, as it puts
  back the working copy, so a group whose only reader failed keeps no claim on its millisecond.
- `restore` and `merge_with` commit alone, since `restored` and `merged` belong to one version.
  They, `fork`, `drop_branch` and a reclamation cycle flush the open group before they start, and a
  closure on another branch flushes it before starting its own.

The combiner is a lock passed between waiters. The group's working copy lives in `Shared` between
closures and each closure takes it under the `tip` mutex on its own thread. So there is no `Send`
bound, `update` closures keep borrowing local state, and a panic unwinds where it should. Cloning
the working copy first is the checkpoint, O(1) per collection, and an `Err` or a panic puts it back.
That closure still waits for the group, since it may have read state that never becomes durable,
except that a panic does not wait, as it never did.

A closure ending under the lock does not flush if someone else will finish the group: either a sync
is running, whose leader wakes one member when it ends, or a thread has announced it is on its way
to the lock and waiting for it has lately been worth it, which is described below. Otherwise it
flushes. So a group closes when nobody is coming or a sync ends, which gives one flush per fsync
however many writers there are, and a lone writer flushes at once, down the same path as before. A
group stops at 256 closures, which bounds what the first of them waits for.

Members park on the group's seal and the leader unparks each one. The first version waited on a
condvar, and notifying a condvar moves its waiters to the mutex and wakes them one at a time: that
chain alone held 32 `queue` writers to 11,600 jobs a second with a p99 of 1,107 µs, against 15,600
and 753 µs once members were unparked directly, median of three. The same change on the plain path
gained 3% at 8 writers and nothing at 32, where the writer lock is the bottleneck, so it was left
out.

Waiting for a thread on its way to the lock used to be unconditional, and where a sync is nearly
free that made `coalesce` far slower than plain group commit (#29). The thread on its way is
usually parked in the mutex, so the group can close only once the scheduler runs it, and the
closure that waits for it parks too, so nothing uses the lock in between. Every closure then pays
at least one wake-up to save a version. On a quiet machine a wake-up costs a few µs and the trade
still pays, since writing a version of one closure took about 16 µs in the benchmark below. When
the cores are busy a wake-up waits for a time slice, about a millisecond, while a sync on tmpfs or
with `Buffered` takes 1 to 2 µs. Counters in a build of the `group` benchmark, `Buffered` on tmpfs
with 8 writers on 4 cores shared with other work, showed it: 29% of closures waited for a
follower, a parked member waited 2.5 ms on average, the writer lock was held 19% of the time, and
a sync took 1.6 µs. Sampling `/proc` put the coalesced writers asleep in `futex_wait` 53% of the
time against 16% without the option, and `schedstat` gave about 1 ms of run-queue wait per time
slice.

`Shared` now keeps three costs: a running mean of how long a sync takes, one of how long a group's
version takes to write under the lock, and an estimate of how long a thread on its way takes to
pick up a group handed to it, timed from the handover to its `join_group`. A closure waits for a
thread on its way only while that estimate is below the other two together, which is what the
thread costs if it writes a version of its own instead. A running sync is waited for as before,
since its leader wakes a member when it ends and the members would wait for it anyway. The means
move an eighth of the way to each sample. The estimate jumps to any slower sample and moves an
eighth of the way to a faster one, so one slow handover stops the waiting. While it stops it,
each closure that declines to wait takes 1/1024 off it, so on a busy machine about one closure in
four thousand waits again and measures, and a machine that has gone quiet is noticed within as
many closures. Each of these updates is a compare-and-swap loop on the current value, so a sample
from a thread that finishes at the same moment as another is not lost. Taking 1/256 off instead let
the waits come back four times as often, and at 2 writers on busy cores raised p99 from 78 to
682 µs. On NVMe a sync takes about 400 µs, so groups still hold every writer.

`ferro-bench --concurrent-only`, 8 writers of 200 commits beside 8 readers that never sleep, commits
a second, median of 15 alternating runs:

| Where | Plain | `coalesce` before | After |
| --- | ---: | ---: | ---: |
| tmpfs, the readers and writers on 8 cores | 34,974 | 2,549 | 31,486 |
| tmpfs, all 32 cores | 41,716 | 78,310 | 67,546 |
| NVMe, all 32 cores | 9,866 | 15,729 | 15,710 |

The `group` benchmark, closures a second, median of three alternating runs, with the closures per
version before and after:

| Where | Writers | Plain | `coalesce` before | After | Per version |
| --- | ---: | ---: | ---: | ---: | --- |
| tmpfs, 4 cores beside 4 busy loops | 2 | 26,575 | 4,455 | 29,012 | 1.1 → 1.0 |
| | 8 | 28,698 | 7,610 | 31,758 | 1.3 → 1.0 |
| | 32 | 24,284 | 25,388 | 26,808 | 1.3 → 1.0 |
| NVMe, `Buffered`, the same | 2 | 32,798 | 7,541 | 33,978 | 1.1 → 1.0 |
| | 8 | 29,970 | 8,133 | 28,664 | 1.3 → 1.0 |
| | 32 | 29,469 | 22,788 | 28,674 | 1.4 → 1.0 |
| NVMe, the same | 8 | 5,041 | 6,773 | 6,499 | 4.6 → 4.1 |
| | 32 | 7,228 | 20,570 | 19,229 | 16.6 → 16.4 |
| tmpfs, all 32 cores | 8 | 42,942 | 55,672 | 54,532 | 8.0 → 6.2 |
| | 32 | 35,656 | 49,257 | 54,960 | 32.0 → 31.9 |
| NVMe, `Buffered`, the same | 8 | 42,299 | 55,809 | 55,309 | 8.0 → 6.2 |
| | 32 | 35,816 | 49,339 | 49,348 | 31.9 → 31.4 |
| NVMe, the same | 8 | 10,111 | 14,884 | 14,267 | 8.0 → 8.0 |
| | 32 | 32,354 | 34,949 | 34,874 | 31.4 → 31.9 |

One writer, and 2 writers on NVMe, moved by less than the noise. What the rule gives up is some of
the gain on a quiet machine, where one slow handover, a thread that happened to be descheduled,
stops the waiting for some thousands of closures. Of 12 counted runs of the concurrent benchmark
on 32 cores, the four that came out under 53,000 commits a second each had exactly one handover
over 100 µs, and the other eight had none. On 4 idle cores the `group` benchmark gave 62,985 and
72,145 closures a second at 8 and 32 writers before, against 55,719 and 62,418 after, though later
runs put the after figures within 6%. Feeding the estimate the lesser of the last two handovers,
so that one slow one is ignored, kept that gain but fell to 54% of plain with the concurrent
benchmark on 8 cores, which suggests that handovers there come fast and slow by turns. A running
mean of the handovers fell to 79% of plain at 8 writers on 4 busy cores with a weight of an eighth,
and to 56% with a weight of a sixty-fourth.

If the group cannot be written, every closure in it gets `Error::GroupFailed`, or `Error::Poisoned`
if the store is, and its changes are gone, including those of closures that did nothing wrong. A
panic in `Data::encode` is one of those failures: it cannot be charged to one closure, so it is
caught and reported to all of them. Nothing else about failure changes: a failed sync fails the
group behind it, and a closure that waits for another thread's commit to be acknowledged now
deadlocks, because that commit may be waiting for this closure to end.

`queue-bench throughput`, fsync, each job an enqueue, a lease and an ack, median of five
alternating runs against the tree before this change:

| Jobs waiting | Writers | Jobs/s | Bytes per job | Ack p50 | Ack p99 |
| --- | --- | --- | --- | --- | --- |
| 1 | 1 | 862 → 894 | 1,185 → 1,173 | 386 → 354 µs | 454 → 425 µs |
| 1 | 8 | 3,457 → 5,707 | 1,848 → 283 | 763 → 453 µs | 862 → 797 µs |
| 1 | 32 | 9,721 → 16,926 | 2,691 → 146 | 959 → 611 µs | 2,270 → 796 µs |
| 1,000 | 8 | 3,529 → 5,306 | 4,293 → 647 | 742 → 485 µs | 882 → 615 µs |
| 1,000 | 32 | 5,930 → 15,286 | 4,972 → 234 | 1,786 → 659 µs | 3,755 → 857 µs |

That is one version for every 8 closures at 8 writers and every 32 at 32, so bytes per job fall by
85 to 95%, against 93% for batching ten jobs a commit by hand. `shop-load` against `shop --coalesce`, with its 32 writer
threads, median of three:

| Mix of reads, cart adds and orders | Connections | Writes/s | Write p99 | Bytes per write |
| --- | --- | --- | --- | --- |
| 90, 8, 2 | 32 | 10,267 → 12,531 | 5.4 → 3.7 ms | 2,905 → 1,104 |
| 90, 8, 2 | 128 | 10,909 → 12,592 | 16.3 → 14.3 ms | 2,891 → 900 |
| 0, 70, 30 | 32 | 12,034 → 14,223 | 5.4 → 3.4 ms | 2,967 → 973 |
| 0, 70, 30 | 128 | 14,616 → 14,579 | 14.7 → 13.4 ms | 3,041 → 970 |

At 128 connections throughput stays where it was, so something besides the writer lock bounds it
there. Read p99 rose from 0.15 to 0.21 ms at 32 connections. `ferro-bench`'s concurrent metric, 8
writers and 8 readers on 1M keys, went from 10,300 to 15,650 commits a second, median of five, and
with the option off it measured 9,900, within that run-to-run spread. A writer alone costs the same
with the option on: 2,550 fsync commits a second against 2,390 with it off and 2,590 before, and
46,000 Buffered against 45,900 and 47,900, a difference the Buffered checkpoint accounts for.

`ferro-bench`'s `group` tool, single-key updates spread over 100,000 keys, median of three:

| Durability | Closure | Writers | Closures/s | Bytes per closure | p99 |
| --- | --- | --- | --- | --- | --- |
| fsync | empty | 8 | 10,300 → 14,800 | 2,308 → 1,536 | 995 → 699 µs |
| fsync | empty | 32 | 31,300 → 36,200 | 2,427 → 1,279 | 2,783 → 2,550 µs |
| fsync | 50 µs | 8 | 9,980 → 8,350 | 2,306 → 1,510 | 1,214 → 1,151 µs |
| fsync | 50 µs | 32 | 12,360 → 12,100 | 2,345 → 1,209 | 4,769 → 4,782 µs |
| Buffered | empty | 8 | 40,900 → 49,900 | 2,400 → 1,610 | 3,204 → 292 µs |
| Buffered | empty | 32 | 32,700 → 48,200 | 2,435 → 1,304 | 8,673 → 818 µs |
| Buffered | 50 µs | 32 | 11,400 → 13,000 | 2,343 → 1,212 | 33,287 → 2,679 µs |

Random keys share only the top of each path, so a flush saves less than on `queue`. With a 50 µs
closure and fsync it does not win: 8 writers lost 16% of their throughput. Every writer ends up in
the one group, so all of them run their closures, 400 µs between them, while the disk sits idle,
and then all of them wait for the same sync, where the plain path runs one writer's closure while
another's frame syncs. Buffered commits gain at every size, and their p99 falls by an order of
magnitude, because the leader no longer serializes a flush per writer.

Measured and left out:

- One version per closure in a shared frame. That keeps history and needs no option, but only the
  frame write, the refs and the reload of the tip, 10 of those 37 µs, happen once per frame, and the
  tree path is the rest. Bytes barely move, since a frame header and the refs are about 130 of the
  233 bytes that a commit of one field costs. An upper bound of +37% on throughput, where coalescing
  gave 50 to 158% on `queue`, did not justify an open transaction that has to read its own unwritten
  objects.
- Sharing intermediate nodes between versions. There is nothing left to share: objects are
  addressed by hash, so a commit already writes only what differs from the version before. Every
  intermediate version must reference a root, and that root every node on the paths it changed. A
  root cannot be recovered without writing it, since a closure is code, not data, and `T` has no
  generic diff, so the only way to keep the bytes down is to store them smaller. Compressing each
  group's frames as one LZ4 block instead of object by object, simulated over 60,000 real `queue`
  frames, took 1,537 bytes per closure to 464 at 8 closures a block and 356 at 32. That keeps history
  and saves no CPU, because each intermediate root must still be hashed, and a read would have to
  decompress the block, so offsets would stop naming records. It was not built.
- A combiner thread that runs other threads' closures. It saves the lock hand-over, which costs
  more than most closures here, but needs `Send` on every closure and its result.
- Spinning on arrival before parking. 2,000 spins, about 25 µs, gained 8% on `queue` at 8 writers
  and 6% at 32, and 3 and 10% on `group`, median of four alternating runs, with p99 better in three
  cases and 30% worse in one. That is not clear enough to burn a core in every waiting writer.
- Other rules for closing a group. Sealing whenever no other thread is on its way, even with a
  sync running, cost `queue` 39% of its throughput at 8 writers and 14% at 32, with 53 to 72% more
  bytes. Sealing as soon as no sync is running, unless Buffered, cost it 37 and 21%. Sealing once a
  group had been open for half the last sync, though more closures were coming, helped 50 µs
  closures by 10% at 8 writers and 38% at 32, and cost `queue` 7% and 19% of its throughput and up
  to 75% more bytes. Nothing tried won everywhere, so the simplest rule stays.

### Awaited writes

A blocking write holds its thread through the closure, the encode and the sync, so the async
examples wrapped every write in `spawn_blocking` or a pool of writer threads and paid a thread for
each commit in flight (#31). `update_async`, `commit_async`, `commit_with_async` and their `try_`
forms return a future that resolves when the commit is published, which under Fsync is when it is
durable, and that holds no thread while it waits for the lock or the sync. It uses
`std::task::Waker` alone, as `Watcher::changed` does, so the library depends on no runtime.

The closure could run in three places. On a committer thread per database or branch, every closure
and its result would need `Send` and `'static`, which rules out borrowing locals, a panic would have
to be caught and carried back to the right future, and every write would pay a thread switch before
its closure started. That is the combiner thread that coalescing already left out, for the same
bound. On the group leader, a future would run other callers' closures inside its own poll, with
the same bounds, and a future dropped before the leader reached its closure would leave a commit
that may or may not happen depending on timing. It runs on the caller instead, in the poll that
gets the writer lock: that poll runs the closure, encodes and writes the frame exactly as a
blocking write does, and lets go. So the closure needs no `Send` and the future is `Send` exactly
when the closure and its result are, an `Err` behaves as in the blocking call, a panic unwinds out
of `poll` on the caller's thread, and with `coalesce` an awaited closure joins a group like any
other.

The first version took the lock with a plain `lock()` in that poll. The lock is held for one
closure and its encode, a few tens of microseconds, but under load an executor thread waited for
it behind a dozen others, and every request queued on that thread waited too. At 128 connections
of `shop-load`'s 90/8/2 mix, reads went from 0.05 ms at p50 with the writer pool to 0.67 ms, and
2.4 ms at p99. Now a future that finds the lock taken queues its waker at a gate and returns
`Pending`. Every holder of the lock, blocking or not, holds it through a guard whose drop wakes the
first future in the queue, usually from an executor thread that has just finished another write,
so on tokio the next writer runs right after it on the same worker. A woken future that finds the
lock taken again, by a blocking writer or by a future that has just arrived, goes back to the head
of the queue. Futures therefore keep their order among themselves, and the lock never sits idle
waiting for a woken one to be polled. Blocking writers still park in `parking_lot` and can take
the lock between a release and that poll. A future waiting at the gate counts as on its way to the
lock, so a coalescing group is handed to it as to a thread blocked in `arrive`. The waiter count is
stored before a second `try_lock`, and read after the release, with a `SeqCst` fence on each side,
so a release and a future queuing at the same moment cannot both miss each other.

The sync goes to a `ferro-sync` thread, one per open database, started by the first awaited write
and holding only a `Weak` to the database. A future never calls fdatasync. After its closure runs it
waits on what a blocking writer would wait on: a non-coalesced write registers its waker in the
queue with the sequence number of the frame it needs, and a coalesced one adds its waker to its
group's seal beside any parked threads. If no sync is running it rouses the sync thread, which
leads exactly as a blocking leader does. Whoever leads, that thread or a blocking writer, wakes the
futures whose frames it settled after letting go of the queue's mutex, and if frames or an open
group are left, rouses the sync thread, since no future will lead them. Under coalescing, the
blocking path relies on one woken member to close a group that was left open while a sync ran. A
woken future may be dropped or polled late, so the sync thread also closes any group left open
once no sync runs and nobody is on the way to the lock, and a writer that closes another's group
rouses it as well. The sync thread uses the same `syncing` flag as everyone else, so there is never
a second sync at once, and an acknowledgement means what it did: a future resolves only once the
queue's settled sequence has reached its frame, which under Fsync happens after the fdatasync.

A future does nothing until a poll finds the lock free. Dropped before that, polled or not, it
writes nothing: it leaves the gate, and if a release had already chosen it, it wakes the next
future in its place. The poll that gets the lock runs the closure to the end and queues the frame
with no await point in between, so after it the outcome is fixed: whether a dropped write lands is
exactly whether its closure ran. Dropping the future then only removes its waker. The frame stays
queued, or the closure stays in its group, and the sync thread writes and syncs it as if the future
were still waiting. If the last `Db` handle goes first, closing seals any group still open and
leads whatever is queued before it settles the anchor. `Drop` for the shared state cannot require
`T: Data`, so it calls that flush through a function pointer stored when the database opened. The
keeper joins the sync thread, so the file is closed and the writer lock released by the time the
last handle's drop returns. A clean close therefore keeps every write whose closure ran, and a
crash keeps every write that was acknowledged.

The tests drive the futures with a small `block_on` and a `join_all` over a park and unpark waker,
on Fsync and Buffered, with and without `coalesce`, alongside blocking writers on other threads. A
recorded trace of awaited writes, some dropped after their closures ran, is cut at every point with
the unsynced writes kept, lost, half kept or torn, and every image must open at a version no older
than the newest one acknowledged under Fsync. Another trace closes the database right after
dropping polled futures and loses everything unsynced, and every write whose closure ran must still
be there. `examples/chat` tests the same under tokio: hundreds of tasks on a current-thread runtime,
`timeout(Duration::ZERO, ..)` and `abort` landing exactly the writes whose closures ran, and awaited
tasks beside `spawn_blocking` writers.

`shop` now writes with `try_commit_with_async` by default, where it used a pool of 32 writer
threads, and `--writes pool:32` and `--writes blocking`, which uses `spawn_blocking`, still select
the old ways. `shop-load` against each, fsync on NVMe, median of three alternating 5 second runs on
32 cores shared with other work. The `spawn_blocking` rows come from an earlier set of runs
alternated with the same pool, which measured within 5% of the figures here except 84,790 ops/s at
32 connections of the read-heavy mix. Threads is the most the server had during a run.

| Mix of reads, cart adds and orders | Connections | Writer | Ops/s | p50 | p99 | Read p99 | Write p50 | Write p99 | Threads |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0, 70, 30 | 32 | pool of 32 | 12,772 | 2.46 ms | 5.17 ms | | 2.46 ms | 5.21 ms | 66 |
| | | `spawn_blocking` | 12,919 | 2.39 ms | 5.15 ms | | 2.40 ms | 5.15 ms | 67 |
| | | awaited | 15,835 | 1.82 ms | 4.68 ms | | 1.85 ms | 4.78 ms | 35 |
| 0, 70, 30 | 128 | pool of 32 | 12,365 | 9.78 ms | 17.95 ms | | 9.79 ms | 17.90 ms | 66 |
| | | `spawn_blocking` | 11,022 | 11.42 ms | 23.35 ms | | 11.43 ms | 23.37 ms | 183 |
| | | awaited | 15,034 | 8.19 ms | 17.70 ms | | 8.21 ms | 17.80 ms | 35 |
| 90, 8, 2 | 32 | pool of 32 | 103,818 | 0.05 ms | 3.41 ms | 0.16 ms | 2.48 ms | 5.38 ms | 66 |
| | | `spawn_blocking` | 86,357 | 0.06 ms | 3.91 ms | 0.47 ms | 2.75 ms | 6.34 ms | 66 |
| | | awaited | 126,371 | 0.06 ms | 2.55 ms | 0.35 ms | 1.68 ms | 4.74 ms | 35 |
| 90, 8, 2 | 128 | pool of 32 | 102,397 | 0.05 ms | 16.13 ms | 0.19 ms | 11.53 ms | 25.46 ms | 66 |
| | | `spawn_blocking` | 94,763 | 0.05 ms | 15.74 ms | 0.17 ms | 12.87 ms | 26.39 ms | 162 |
| | | awaited | 130,183 | 0.06 ms | 13.19 ms | 0.39 ms | 8.87 ms | 17.86 ms | 35 |

Writes are the cart adds, and the orders match them within a few percent. The pool adds its 32
threads to tokio's 32 workers and the server's own, and `spawn_blocking` adds one for every write in
flight, where awaiting adds only the sync thread. The awaited server did 21 to 27% more work than
the pool at every mix and connection count, and its writes were faster at p50 everywhere and no
slower at p99. Reads at p99 rose by about 0.2 ms, because a write's closure and encode now run on
the executor between reads. The version that blocked on the lock did 3 to 15% less work than the
gate and had faster writes at 128 connections of the read-heavy mix, 3.65 ms at p50, but the slow
reads above.

`chat-load` flat out against `chat`, 300 feeds and 32 posters, median of three:

| Writer | Posts/s | Post p50 | Post p99 | Delivery p50 | Delivery p99 | Threads |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `spawn_blocking` | 3,913 | 8.17 ms | 15.66 ms | 8.55 ms | 16.03 ms | 131 |
| awaited, blocking on the lock | 4,674 | 6.57 ms | 14.07 ms | 6.91 ms | 14.40 ms | 35 |
| awaited | 5,157 | 6.50 ms | 10.40 ms | 6.88 ms | 10.86 ms | 35 |

`ferro-bench`'s `group --async 1` awaits each write with `block_on` on a thread per writer, which is
not how an async server runs but isolates the cost of the sync thread. Medians of four runs on the
shared machine: a lone writer cost the same under Fsync, and under Buffered took 21 to 22 µs at p50
on quiet runs against 19 µs blocking, since its commit now goes through a second thread. At 8 and
32 writers awaited writes did 4 to 10% fewer commits a second, and 21% fewer at 32 under Fsync,
25,950 against 32,857, because the sync thread wakes every waiting writer itself before it starts
the next sync, and here each wake unparks a thread where on an executor it queues a task. Buffered
p99 at 8 writers fell from 1.4 to 2.1 ms to 0.4 ms. Leading a Buffered batch from the poll itself
would save the lone writer's 2 µs, but its checkpoint can fdatasync 64 MiB on an executor thread,
so it was left out.

### Commits of a few rows

A booking write in `examples/booking` changes about ten small trees, the rows and their indexes,
and spent six to ten times as long outside its closure as in it, all under the writer lock.
`ferro-bench`'s `tables` tool runs the example's hold, confirm and cancel closures one commit at a
time against a Buffered file, after 4,000 warm-up pairs, and reads thread CPU time and user-space
instructions, through `perf_event_open`, for the commit and for its closure. A hold writes 36
objects, 7.3 KB of bodies before LZ4 and 5.1 KB after, and encodes 274 keys on 34 nodes, of which
20 to 23 only gained a new child.

Instructions for a hold, in thousands, by phase. The phases were taken with counters around each
one, which add about 6%; the totals are without them.

| Phase | Before | After |
| --- | ---: | ---: |
| Closure | 63 | 64 |
| Encoding keys, values and child links | 324 | 200 |
| BLAKE3 | 142 | 96 |
| LZ4 | 168 | 167 |
| Appending records and looking up `dedupe` | 43 | 41 |
| Registering in `dedupe` | 20 | 4 |
| Node cache inserts | 16 | 19 |
| Building the cached copies | 45 | 42 |
| Decoding the root as a check | 12 | 12 |
| Reading the refs, root and log entry back | 26 | 0 |
| Log entry, refs, frame, superblock and publish | 17 | 17 |
| Dropping the old tree | 14 | 15 |
| Setup and the decode of fresh values as a check | 17 | 19 |
| Outside the closure, total | 794 | 575 |

Time did not follow instructions. In a sampling profile of the same run before, leaving out time
inside libc, encoding took 30% of a commit, inserts into the node cache and `dedupe` 21%, the
closure 15%, LZ4 11%, appending records and `dedupe` lookups 7% and reading the tip back 4%: the
inserts and lookups run few instructions and wait on cache misses. Five changes, each measured on
its own:

- A node on the path to a change copies the bytes of its unchanged entries, as described under
  Canonical tree: 16% fewer instructions outside the closure. A single-key insert into a
  20,000-key map encoded 58 values and now fewer than 24.
- The new tip is the root the commit decoded as a check and the commit it wrote, and the refs are
  decoded from their encoded bytes, instead of three reads, hash checks and decodes from the file:
  4% fewer instructions and 6 to 10% less CPU time.
- `dedupe` became the set-associative table described under Deduplication: 3% fewer instructions and
  7 to 12% less CPU time for a hold.
- `Object::hash` copies an object of up to 4 KiB and its link hashes into a per-thread buffer and
  hashes it in one call. Each small update had cost about 140 instructions of buffering, so a node
  with nine links paid more for its links than for its body: a 100-byte node with nine links took
  2,285 instructions against 3,542. The bytes hashed are the same. 6% fewer instructions.
- The parallel flush needs 256 dirty nodes, as described under Canonical tree: a third less CPU time
  for a confirm and a quarter less for a cancel.

Median of five interleaved runs on a 32-core machine while other work on it was light, in thread
CPU time:

| Commit | Closure | Outside the closure, before | After | Instructions outside, before | After |
| --- | ---: | ---: | ---: | ---: | ---: |
| Hold | 13 to 15 µs | 90 µs | 71 µs (-21%) | 794,000 | 575,000 (-28%) |
| Confirm | 12 to 13 µs | 136 µs | 85 µs (-37%) | 1,024,000 | 718,000 (-30%) |
| Cancel | 24 to 26 µs | 163 µs | 112 µs (-31%) | 1,257,000 | 908,000 (-28%) |

The figures before for a confirm and a cancel leave out the threads the parallel flush spawned, so
their real fall is larger, and a confirm's wall time outside the closure fell from 140 to 85 µs.
With heavy other work on the machine the same comparison gave -24%, -43% and -36%. Bytes per write
went from 12,210 to 12,208. `booking-stress --buffered` at 64 threads, median of three interleaved
rounds, went from 5,097 to 6,055 writes a second alone, with hold p99 from 23 to 20 ms, and from
14,459 to 15,551 with `coalesce`. Under heavy other work it went from 2,031 to 2,858 alone and from
3,418 to 4,833 with `coalesce`, and at 16 threads from 2,366 to 3,142 and from 2,906 to 4,780.

What is left is spread out. LZ4 is now the largest phase by instructions, 29% of what is outside the
closure, then encoding at 35%, which is mostly the leaves that a change lands in and the
per-node work of building records and cached copies, and BLAKE3 at 17%. Of the nodes a commit puts
in the node cache, 98.3% are read again, 61% by the very next commit, so the inserts stay. The
`heap` tool's commits and reads case ends with 154.8 MiB of live heap against 170.9 before at a
256 MiB setting, and its other cases moved by under 1%.

Measured and left out:

- Encoding, hashing and compressing outside the writer lock. A closure is `FnOnce` and has to run
  on the tip, so it cannot run before the lock is taken and again if the tip moved. What is left is
  to let the next closure run on this one's dirty tree while this one flushes, which bounds the gain
  by the closure's share of the lock: (13 + 71) / 71, at most 1.18 times the writes a second for
  a hold. It needs a flushed-link memo on every shared dirty node so the next flush does not encode
  it again, a second lock to keep frames in order, and a failed flush would fail closures that
  already ran on it, which is what `coalesce` already does for 2.6 times as many writes.
- Copying the unchanged entries of a node whose values changed too, through offsets kept per
  entry and the column they came from. 3 to 5% fewer instructions outside the closure, but a
  cancel's closure took 21% more instructions and CPU time came out 3 to 5% worse, since most
  leaves a commit touches were last written by a bulk load or read from the file and keep nothing.
- An LZ4 block compressor of our own, with a table sized to the input and reused per thread, which
  saves `lz4_flex`'s zeroed 8 KiB table and output vector on every call. 7% more instructions per
  commit, the same CPU time over five rounds, and 0.1% more bytes.
- `lz4_flex` without `safe-encode`: 27% fewer LZ4 instructions, 6% fewer per commit and the same
  bytes, but it brings unsafe code into the write path.
- No LZ4 under 128 or 192 bytes: 2 or 5% fewer instructions for 0.35 or 1.2% more bytes, and CPU
  time within noise. Bodies under 128 bytes compressed in 53 to 70% of cases, saving 11 to 16% of
  their bytes.
- Compressing into a reused buffer, and leaving a leaf's link vector unallocated: 1% fewer
  instructions, CPU time within noise.
- Hashing several nodes in one SIMD pass. BLAKE3's multi-input path is only reachable through an
  unstable hidden module, takes inputs of one block-aligned length, and a parent needs its
  children's hashes first, so at most BLAKE3's 17% could shrink. Not built.
- Decoding just-written nodes for the cache, and the log entry itself, are about 1% each.
- Cheaper index maintenance: the derived `reindex` already skips an index whose fields did not
  change, and the whole closure is a tenth of a commit's instructions and under a fifth of its
  time.

### A delta log between checkpoints

Most of a small commit's bytes are tree nodes. Writing a short record of what each commit changed,
and writing nodes only at a checkpoint every K commits, was designed against everything that relies
on a version being a root hash, bounded by measurement, built behind an option, and left out.

A version has to stay a root hash. Hashing the chain of records instead would give the same contents
different versions in two files, and `==`, the no-op skip, `diff`, deduplication and sync by hashes
would all stop working. So a delta commit still encodes and hashes every changed node, as `hash_of`
does for a dirty tree, and skips only compressing and writing them. Its log entry links the root by
its hash at the offset of the commit's record. Offsets are not hashed, so entry hashes, `head_hash`
and commit identity across files are what a plain file has. The record needs no types: for each new
object it holds a copy-and-insert script against the object at the same place in the previous
version, and a code per link, which is the base's link at that index, another of its links, a new
object, or a pointer written out. Replaying a record rebuilds the objects and hashes them, so a read
still checks every object against the hash that named it. Between checkpoints:

- `at(v)` replays the records since the checkpoint before `v`, up to K of them, and `diff` between
  two stored versions pays for both replays before its O(changes) walk.
- Open replays the records since the last checkpoint. A record is part of its commit's frame, so a
  crash loses what it loses today: nothing under `Fsync`, frames past the anchor under `Buffered`.
- Watchers in the writing process get snapshots as before. A follower in another process replays the
  records between its version and the new head on `refresh`, so every follower repeats the writer's
  encoding and hashing.
- Sync sends objects, so a sender has to materialise every commit it sends, or bundles carry records
  and the receiver replays them before its typed check.
- A reclamation cycle copies objects, so it materialises every retained version unless it learns to
  copy records and checkpoints, and `verify` replays the whole log.

`ferro-bench`'s `delta` tool measured the bound on the tree as it is. It splits each commit's frame
into its records, walks the new root against the old one to size the record a delta commit would
write, sizes a checkpoint as one commit of K changes, and times `hash_of` of the dirty root inside
the closure against the commit outside it, all with `Durability::Buffered`. Bytes per commit:

| Commit | Bytes | Nodes and root | Header, entry and refs | Record | Delta frame |
| --- | ---: | ---: | ---: | ---: | ---: |
| Insert one key into 1M keys | 3,533 | 3,312 | 221 | 83 | 304 |
| Booking hold | 11,236 | 11,008 | 229 | 478 | 706 |
| Booking confirm | 13,049 | 12,818 | 231 | 341 | 573 |
| Booking cancel | 16,272 | 16,042 | 230 | 443 | 673 |

A checkpoint writes every node the commits since the last one left on the tip, per commit:

| Commits per checkpoint | 16 | 64 | 256 | 1,024 | 4,096 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Random keys into 1M keys | 2,568 | | 1,680 | | 859 |
| Booking writes | 3,457 | 2,089 | 1,175 | 673 | 347 |

Random keys in a large tree share only the tops of their paths, so the map writes 1.8 times fewer
bytes at a checkpoint every 256 commits and 3.0 times at 4,096. Booking trees are small and their
indexes hot, so a booking write gains 4.6 times at 64 and 9.6 times at 1,024. Thread CPU time
outside the closure, median, with mean instructions:

| Commit | Commit as it is | `hash_of` alone | A commit that writes almost nothing | Bound on time |
| --- | ---: | ---: | ---: | ---: |
| Insert one key into 1M keys | 21.1 µs, 162,000 | 7.8 µs, 55,000 | 6.2 µs, 40,000 | 1.5 times |
| Booking hold | 95.1 µs, 564,000 | 32.7 µs, 232,000 | 11.2 µs, 68,000 | 2.2 times |
| Booking confirm | 113.4 µs, 704,000 | 39.3 µs, 279,000 | 11.2 µs, 68,000 | 2.2 times |
| Booking cancel | 160.3 µs, 976,000 | 56.7 µs, 388,000 | 11.2 µs, 68,000 | 2.4 times |

The commit that writes almost nothing, a 16-key map or one field of the booking root, stands for the
log entry, refs, frame, superblock and publish that a delta commit still pays. Fsync does not move:
a 1M-key commit of 3,684 bytes took 427.5 µs at the median and a 16-key one of 355 bytes 394.0 µs,
so a frame the size of a record saves at most 8% of an fsync commit. With the closures counted, the
bound promised 1.8 to 9.6 times fewer bytes and 1.5 to 2.0 times the Buffered commits of one writer,
so it was built.

The prototype, `Options::deltas(K)`, drafts a commit's objects in memory, writes the record as the
first record of the frame and makes every Kth commit a checkpoint. Forcing it on for the whole test
suite found three faults. A node re-encoded to the bytes it was loaded from keeps its old pointer,
but its spilled values had been drafted again under the new record, which then never named them, and
the cached copy still pointed there: the file failed after a reopen. In-memory pointers carried a
checkpoint's draft offset, which names no record, so a checkpoint has to reload the tip from the
written root and drop every cached node that links a delta offset. And a flipped byte made a record
its own base, so replay followed the chain without end; one crash test grew to 79 GB before it was
killed. Replay now refuses a record it is already applying, and offsets alone cannot order it, since
an object found by hash can come from a later checkpoint whose links point forward. With those
fixed, every crash image test passes, and 3,000 random byte flips in a delta file were each reported
or read back right. Median of interleaved runs, two or three each:

| | Plain | K = 64 | K = 1,024 |
| --- | ---: | ---: | ---: |
| 100k keys, 5,000 single-key commits: bytes per commit | 3,135 | 1,806 | 907 |
| Same, Buffered commit p50 | 19.0 to 21.1 µs | 27.0 to 29.6 µs | 23.2 to 27.8 µs |
| Same, p99 | 33 to 34 µs | 542 to 554 µs | 48 to 58 µs |
| Same, open | 0.10 to 0.15 ms | 0.22 to 0.37 ms | 8.2 to 10.2 ms |
| Same, first `at(v)` of a random version, mean | 18 to 32 µs | 140 to 176 µs | 127 to 162 µs |
| `tables`: bytes per write | 12,208 | 2,827 | 1,590 |
| `tables`: hold, whole commit | 80 to 81 µs | 102 to 103 µs | 116 to 117 µs |
| `tables`: cancel, whole commit | 130 to 131 µs | 179 to 181 µs | 188 to 191 µs |
| `booking-stress --buffered`, 64 threads: writes a second | 5,947 | 4,809 | 4,470 |
| Same: bytes per write | 11,158 | 2,645 | 1,406 |
| Same: hold p99 | 20.4 ms | 23.3 ms | 51.1 ms |
| `ferro-bench`: Buffered commit at 1M keys | 25.0 to 25.9 µs | 46.4 µs | 53.3 to 54.0 µs |
| `ferro-bench`: fsync commit | 411 to 430 µs | 446 to 485 µs | 417 to 427 µs |
| `ferro-bench`: open | 0.1 ms | 0.8 ms | 11.4 ms |
| `ferro-bench`: peak RSS | 365 to 366 MiB | 442 to 443 MiB | 441 to 445 MiB |

Bytes came close to the bound: 2,827 and 1,590 per booking write against 2,732 and 1,316. Time went
the other way. In runs of 3,000 commits with the record replaced by a stub, a 100k-key commit took
19.1 to 19.4 µs at the median against 17.5 to 17.6 µs plain, and 21.5 to 24.3 µs with the real
record. The bound counted only encoding and hashing, but a delta commit keeps the rest of a flush:
the decode of fresh values as a check, the cached copies and their node cache inserts. It adds
holding every drafted object until the checkpoint and the bytes of every object it reads as a diff
base, and diffing against them, which together cost more than LZ4 and the appends they replace. A
checkpoint then writes the union in one commit, the p99 above, and drops the cached nodes, so the
closures after it read and encode those nodes again: a booking cancel's closure went from 23 µs to
68 µs at K = 64 and 48 µs at K = 1,024. The replayed objects sit outside `cache_bytes`.

The suite also showed what each feature would need before the option could ship. Streamed bundles
read raw records at delta offsets and fail. A reclamation cycle with `Keep::All` writes a larger
file than it reads. `remove_range` reads base objects in the subtrees it cuts out, and history walks
hold more than `cache_bytes`. Each is work in sync, reclamation or the read path, for a design that
loses time on every commit.

It was left out. The one gain is bytes on disk, and reclamation already bounds the file and
`coalesce` already writes 85 to 95% fewer bytes where writers contend, while gaining throughput. A
delta log costs `booking-stress` 19 to 25% of its writes a second and `ferro-bench` about half of
its Buffered commits, gives an fsync commit nothing, makes open and the first read of an old version
replay up to K records, and puts every follower to the writer's work again. The prototype is not
merged. The `delta` tool stays in `ferro-bench` for the bound.

## History

Each branch head holds a commit log stored as a chain of entries, newest first. An entry holds
the `Commit`, a link to the previous entry, and for even versions a second link to the entry at
`version - lowest_set_bit(version)`. A merge's entry also links the other branch's tip entry, as
described under Merging. Appending a commit writes one small object and finds its
skip target by following skip links from the tip, which is O(1) amortized. Looking up a version
or a time follows skip links while they do not overshoot, which is O(log n). The head records the
tip pointer, the first retained version, and the head version. `Refs` is `Map<String, Head>` and
the superblock points at it. Forking a branch copies one `Head`, so the chain is shared and
nothing is rewritten. `restore` commits an old value as a new version, so history is never
rewritten.

Pruning only raises `first`. Walks stop there, and lookups below it return `NoVersion`. Retained
entries are not rewritten, so the entry at `first` still links `prev` and perhaps `skip` entries that
are gone. Those links are hashes nothing follows, and a copy into a new file writes them with
offset 0.
`Options::history(Keep::Last(n))` raises `first` as part of each commit, which costs no extra bytes.
`Keep::Since` and `Keep::Within` need a lookup by time, so they apply at each reclamation cycle.

Everything that names a version of a branch follows one rule: a version below `first` is gone.
`at` returns `NoVersion` for it, `as_of` a time before the oldest retained commit returns
`NoVersion`, `history_of` and `bisect_changes` report the oldest retained commit as a change
because there is nothing left to compare it with, and a fork copies `first`, so it answers the
same. Version 0, the value before the first commit, is not stored, and it counts as retained for
exactly as long as version 1 is. `at(0)` used to return the current `Default` even after pruning,
while `as_of` for the same moment returned `NoVersion`. The rule depends only on `first`, never on
whether a reclamation cycle has removed the bytes yet, so an answer does not change when one runs.

The log used to be a `Map<u64, Commit>`, which rewrote a tree path per commit and was about 20%
of the bytes of a single-key commit. `Db::log()` now returns a `Log` handle with `len`, `get`,
`first`, `last`, `iter` and `values`. `iter` runs in version order, like a `Map`, and is double
ended. `next_back` follows `prev` links from the tip and reads only what it yields, so
`.rev().take(n)` costs O(n). The chain has no forward links, so the first `next` reads what is
left of it once.

`Db::history_of` walks `prev` links newest first and yields `(commit, value)` for each version
whose projection differs from the version before it. Only adjacent versions are compared, so a
value that comes back is reported again, and skip links cannot speed this up: a change and its
reversal could both fall between two probes. Version 1 is compared with the projection of the
genesis value, and a pruned chain reports its oldest retained commit, since what came before is
gone. `History` is not double ended, because reversing it would need the whole walk. Callers that
show changes oldest first collect and reverse, so the cost is in plain sight.

`Db::history_of_range(versions, project)` is the same walk between two versions. It finds its top
through skip links and reads one version below its bottom as a baseline, so it yields exactly what
filtering `history_of` by version would, for O(log n) plus the span. `history_of` is that with no
bounds. Both load nothing until the first `next`.

`Db::bisect(versions, test)` finds where a monotone test turns true, in about log2(n) probes of a
root each. A probe is a version lookup, so that is O(log^2 n) entry loads. Following skip links from
the tip was worked out and not built: it takes two probes a level for the sake of the entry loads,
and the probes are what cost.

`Db::bisect_changes(versions, project)` uses the probes the walk cannot trust. It compares the ends
of a span, stops if they agree, and otherwise cuts the span in two, so it costs O(changes × log n)
probes, not the span, and it can miss a change and its reversal that fall between two probes. It is
for a projection that never returns to an earlier value in that range, such as a row with a
revision or an attempt count, or `value_hash` of one. A row that appears and later goes is not one:
its absence comes back, so the range has to stop short of the row's end. A pruned chain and genesis
behave as in `history_of`, and results come oldest first.

`queue` carried 90 lines of its own bisection and now has none. On a 305,000 commit file, for one job
out of 100,000, `queue history` takes 1.2 ms, as the old bisection did, against 2.0 to 3.9 s for
`history_of`, which walks back to the start of the log because the job is absent at the tip. The two
`bisect` calls that find where the job begins and ends take 0.1 to 0.2 ms. For a job alive through
the whole file, `bisect_changes` takes 0.26 ms and `history_of_range` over its 304,734 versions takes
1.0 s. A job that stays alive is where the walk still costs, which is why `queue`
uses `bisect_changes` and not `history_of_range`. `shop` starts `GET /orders/:id/history` from the
request's snapshot, so it no longer walks commits newer than the one it answers for.

A value's item is ready only when the walk reaches the start of that value's run. A row's creation
is followed by a run of `None` back to version 1 that reports nothing, so `Iterator::take_while`
would read the rest of the log to learn that no older item exists. `History::take_while` is an
inherent method for that reason. It tests the predicate against each older value as the walk
enters its run, paired with the commit last reported, and stops there. Its items match the
`Iterator` adaptor for a predicate on the value, on the commit, or on the conjunction of the two.
The predicate is `Fn` because it can run twice per item. The run at the tip has no commit to pair
with, so a row that is absent at the tip costs the age of that absence; find where it ended with
`bisect` and start `history_of_range` there.

A step loads one `Entry`, loads and decodes the root `T`, and runs the projection. Measured on a
50,000-commit shop in a fresh process: 0.8 µs for the entry, 1.3 µs for the root, then 1.3 µs for
a row in a quiet part of the tree and 11.5 µs for a recent row, whose nodes every commit rewrote,
so each step reads and decodes those nodes in full, values included. With the same commits cached
the whole step is 0.7 to 2 µs. Going through `at(v)` instead costs about 3 µs more per step, for
the skip-link descent from the tip. Entries hold the `Commit` behind an `Arc`, so walking the log
and `db.read()` do not copy the message.

`Db::head_hash` is the hash of the tip entry, and `Log::hash_at` that of any retained entry. An
entry links its commit's root and its `prev` entry, and the hash covers link hashes, so one hash
covers every commit's version, time, message and `restored` field, and every root value, back to
the oldest retained entry, and commits to everything older as well, though that can no longer be
checked. Pruning and reclamation keep it, because offsets are not hashed and retained entries are
never rewritten.

Published where the writer of the file cannot change it, the hash is a tamper-evidence anchor. If
`hash_at(version)` matches an anchor for that version and `verify` passes, then the history up to
that version is what it was when the anchor was taken, subject to the limits of 160-bit hashes
described under Objects and links. That is all it shows:

- It is not a signature. Hashes are unkeyed, so anyone who can write the file can rewrite history
  and get a consistent chain with a new hash. Only comparing with the anchor reveals that.
- It proves nothing about commits after the anchored version, or about history before the anchor
  was taken: a chain forged first and anchored afterwards verifies.
- `time_ms` and `message` are what the writer recorded. The hash shows they were not changed, not
  that they were true.
- It covers one branch's chain and, through merge links, the commits merged into it. Other
  branches, and the old `merged` records in the head, are outside it, and `verify` is what checks
  those objects.
- An anchor for a pruned version can no longer be checked, since `hash_at` returns `None` for it.
  An anchor for a retained version still matches.

### Commit times

A commit's `time_ms` is the later of the clock and a floor: the previous commit's time, plus one if
that commit is stamped, and for a merge also the time of the entry it links. So times never go back
along a chain, a clock that steps back makes commits share a millisecond until it catches up, and a
merge is never older than either parent, which the merge-base search relies on. `as_of(t)` follows
skip links from the tip to the newest entry whose time is at most `t`, so of several commits in one
millisecond it returns the last.

A time a closure reads from the clock says little about its commit. The commit takes its time after
the closure returns, and later commits can share that millisecond, so `as_of` of the reading can
land before the commit, on it, or on a later commit. `examples/booking` recorded the time of each
payment that way and found all three among 1,500 payments, 773 of them on a later commit.

`commit_stamped` and `try_commit_stamped` pass the closure a `Stamp`. Its version is the tip's plus
one, the version the commit gets since it is made under the same lock. The first call to `time_ms`
fixes the commit's time at the later of the clock and the floor, and the commit is then stamped:
its entry carries a flag, and the next commit's floor is one past its time, so no later commit on
the chain shares its millisecond and `as_of` of that time returns it. A commit whose closure reads
only the version is not stamped, so writers that do not ask keep the times they had. The flag lives
in the entry, so the rule holds after a reopen, on a fork, which shares the entry, and after a pull
that fast-forwards onto it. A closure that returns `Err`, panics or changes nothing writes no entry
and claims nothing.

The cost is drift. A branch that stamps more than 1,000 commits a second takes times ahead of the
clock by a millisecond for each one past that rate, and the clock catches up once the rate falls.
Stamping every commit would make `as_of` exact for any commit's time without asking, but a plain
commit takes 27 µs with buffered durability, so a busy writer would run seconds ahead within a
minute and `as_of(now)` would stop returning the head.

The flag byte of an entry had one bit left. Bit 128 now says a varint of further flags follows the
byte, and bit 1 of that varint is the stamp, so the next flag needs no change to the byte. A varint
of 0, or one with an unknown bit, is rejected, so an entry has one encoding. Entries written before
have no such bit and read as not stamped. A build from before this change rejects a stamped entry.

## Merging

`Map::merge` and `Table::merge` do a three-way merge driven by `diff(base, theirs)`, costing
O(changes). A key changed only by theirs is applied. A key changed the same way on both sides is
left alone. A key changed differently on both sides goes to a `resolve` closure. Merging runs
inside an ordinary commit, so a root with several collections merges each one in plain Rust.

`Db::merge_with` finds the base, or builds a virtual one after a criss-cross as described below,
runs the closure on our tip with the base and their snapshot, and commits. The log entry it writes has a second link, to the entry of the other branch's tip: the
merge's second parent. Commits are then a graph, as in git, and `common_ancestor` returns a best
common ancestor in it: a commit both tips reach through `prev` and merge links that is not an
ancestor of another such commit. Because it follows merge links, a change counts as shared however
it arrived, directly, through a third branch (A merges B after B merged C, then A merges C), or
through a fork of a branch that was already merged, so a closure that adds counter deltas applies
each delta once. The base is computed under the writer lock against the tip the closure runs on.
A merge that keeps only our side still commits, so the link is not lost, and merging a branch
whose tip we already contain writes nothing.

`merge_with` had no error channel, so a merge closure that found it could not go ahead had to
panic, and `examples/booking` caught the panic and turned its payload back into a message.
`try_merge_with` is the same merge with a closure that returns `Result<(S, R), E>` for any
`E: From<Error>`, like `try_commit_with`, and `merge_with` is now that with a closure that cannot
fail. The closure runs on a copy of our tip under the writer lock, and the merge link and the
`merged` record are only written by the commit after it returns, so an `Err` drops the copy and
nothing reaches the file: the next merge finds the same base and runs again. The errors of the merge
itself, such as `NoBranch` for a dropped branch, come back through `E::from`. `Map::try_merge` and
`Table::try_merge` do the same for `resolve`, which matters when one conflict decides that the merge
cannot happen. They stop at the first `Err` and put back an O(1) clone of the collection taken
before they started, since a half merged collection is of no use to anyone but a closure that is
about to fail.

Only direct merges used to be recorded, as `(their version, our version)` per branch name in the
head, and the base was the fork point found by binary search unless a record said otherwise. A
change that came through a third branch was seen again.

### Finding the base

Each entry carries three numbers for the search. Its generation is one more than the larger of
its parents' generations, so a parent always has a lower one. Its reach is the highest generation
among the merged parents of the merge entries on its chain, at or below it. Its last merge is the
version of the nearest merge entry on its chain. On a chain with no merges they are the version, 0
and 0, which entries written before merge links existed already mean, so they are stored only once
a chain has a merge, as three varints.

The search paints down from both tips, as git's merge-base does, but in jumps. An item is a place
on a chain and the sides that reach it. Items are taken in generation order, so when one is taken
every path into it has arrived and its sides are final. A chain item does not step one commit at a
time. It moves down to the highest place where anything could still join it: where its chain meets
another item's, found by a binary search on versions that compares entry hashes, or the highest
entry whose generation is at most the reach of any item, since nothing that has not yet been
opened can arrive above that. The merge entries it passes become a separate item whose level is
their reach, opened from the top only when nothing higher is left. A place reached from both sides
is a candidate. It and everything below it is stale, and the search stops once one side has
nothing left that is not stale.

So a merge whose chains have no merges since they parted costs two binary searches, O(log^2 n),
like the old fork-point search. Merge entries are opened only while their merged parents could
still matter: when `main` has taken a side branch a thousand times, the next base is found after
opening the latest merge, not all of them. What is opened is roughly the merges made since the
base, since each of those could have brought in something the other side has. Measured with
`Durability::Buffered`, the first call and then the mean of 100 more:

| Shape | First | Then |
| --- | --- | --- |
| Fork at 10,000, then 10,000 and 1,000 commits | 0.15 ms | 49 µs |
| `main` took a side branch 1,000 times among 21,000 commits | 46 µs | 44 µs |
| A side branch took `main` 1,000 times | 46 µs | 44 µs |
| 1,000 features merged since a branch forked | 31 ms | 30 ms |
| The same branch after syncing, with 200 features merged since | 1.3 ms | 1.3 ms |

### Criss-crosses

When two branches have each merged an older state of the other, the tips have several best common
ancestors. The search returns every candidate it found, ordered by generation, then `time_ms`, then
entry hash, highest first. The order depends only on the set, so both sides and every run agree.
`common_ancestor` returns the first, which is the one every merge used to take, and
`common_ancestors` returns them all. The model tests compare that list with the true set of best
common ancestors on every merge, with and without pruning, and whenever the search answers they
match.

Merging against the first alone made what only the others carry look new: a closure adding counter
deltas applied it twice, and a three-way merge could undo a revert, since the base held the value
from before the change that was reverted. Merging is now recursive, as in git. With best ancestors
B1 to Bk in that order, the virtual base starts as B1. For each later Bi, the search runs again
with B1 to Bi-1 together as one side and Bi as the other, which is the ancestry a commit merging
B1 to Bi-1 would have. If Bi is among the answers, it is below something already merged and is
skipped. Otherwise the answers become the base for this step, virtual themselves if there are
several, and the closure merges Bi into a working copy of the virtual base built so far. The merge
then runs against the result. For counter deltas this is inclusion and exclusion over ancestor
sets, so each delta is counted once whatever the shape.

The closure is the user's, so it must be callable more than once, and the merge methods and the sync
methods take `FnMut` instead of `FnOnce`. Each step calls it as a merge with the virtual base built
so far as `ours`, so it resolves conflicts between the ancestors as it resolves any. Git writes
conflict markers into a virtual base, so that the final merge conflicts again. Ferro has no conflict
state to write, and a closure that resolves the same way whichever side is ours, such as
last-writer-wins, makes the virtual base agree with what each side already chose. What the steps
return is dropped. An `Err` from any step discards the whole merge, as an `Err` from the merge
itself does, and nothing is written. The virtual base is never written either. It lives in memory
for the merge, its commit has version 0, no root, the latest `time_ms` of what it merged and the
message the closure returned for it, and the merge commit still links only our tip and theirs, so
later searches see the same graph.

The steps start from commits below the tips, with the limits the first search reached them with, so
they read only what pruning keeps, and when the base of the ancestors could be in pruned history the
merge refuses with `NoCommonAncestor`. A record in the old `merged` map says that a version of the
named branch is below the head's commit at the recorded version, so a step that starts on that
head's chain at or above that version also starts from the recorded commit. Without that, a
criss-cross over a merge recorded before merge links lost the recorded branch's changes, because the
step found the fork point instead.

Other shapes were considered. Handing the closure every best ancestor leaves it unable to build
the right base, which needs the ancestors' own bases, recursively, so each caller would redo this.
A merge trait on the root type that ferro calls recursively has no room for what closures capture,
such as a policy, a clock or a device name, or for a value returned to the caller, and it would be
a second merge API. Keeping `FnOnce` with an opt-in recursive variant would leave the default
wrong, and the sync API alone has eight methods that take a merge closure. Whether a closure can run
twice is a static bound in Rust, so it cannot depend on whether a criss-cross occurs. Most closures
are `FnMut` already. In this workspace one test closure moved a captured string into its message
and needed a clone.

Without a criss-cross, the closure runs once and the only added work is ordering the candidates.
`cargo run --release -p ferro-bench --bin merge` builds the shapes above, its feature merges smaller
than those of the table, and a criss-cross of two branches after 11,000 commits, merged without its
commit:

| Shape | Before | After |
| --- | --- | --- |
| Fork at 10,000, then 10,000 and 1,000 commits | 48 µs | 49 µs |
| `main` took a side branch 1,000 times among 21,000 commits | 40 µs | 41 µs |
| A side branch took `main` 1,000 times | 40 µs | 41 µs |
| 1,000 features merged since a branch forked | 5.9 ms | 6.0 ms |
| The same branch after syncing, with 200 features merged since | 1.8 ms | 1.8 ms |
| The criss-cross, its search | 120 µs | 120 µs |
| The criss-cross, the whole merge | 120 µs | 166 µs |

The merge costs one more search, from the two ancestors to their fork point, and one more call of
the closure.

### Merges and pruning

A branch keeps its retained versions and, through every merge entry among them, the commits that
merge brought in: the merged chain from the merged tip down to the oldest version the other branch
had retained when the merge was made, and no older than either that version's commit or the oldest
retained commit of the branch that keeps the merge. The version and its time are stored in the
merge entry. Merge entries reached that way bring in theirs under the later of the two time
limits. A reclamation cycle copies exactly that. Every limit only rises, and a merge never claims
more than the other branch still had, so a commit that has gone never comes back through a later
merge: a nested merge does not take the looser limit of whichever branch reached it, which would
let a branch that never pruned reach, through a merge of a pruned branch, commits that branch had
already let go. The merged tip itself is kept whatever its age, with its value but nothing below
it, because it is the base the next merge of that branch is most likely to need, and it was
retained when the merge was made. Without pruning nothing is cut, and the commits of a dropped
branch stay as long as a merge brought them in.

The search reads only what that rule keeps, never whatever bytes are still on disk, so its answer
does not change when a cycle runs, and every path it follows was copied. When a path that is not
stale runs into pruned history, what is hidden there is older than where it stopped, since a merge
commit takes the later of its parents' times, and of lower generation. The search then runs on
until nothing that is not stale is left, and returns `NoCommonAncestor` if the other side reached
anything that old and that low, or if both sides ran into pruned history, because the base could be
hidden there. Otherwise its answer is the one the whole history would give.

### Format

Log entries have three more flag bits: the merge link, followed by its floor version and time; the
three numbers; and a bit saying that some merge below has a limit, which tells the search when it
may stop early.
Entries written before carry none of them and read as a chain without merges. The head still
holds the old `merged` map, so files written before stay readable, but nothing new is recorded in
it. A record left from before still counts: the recorded version of the named branch is treated as
reachable from the head that holds it, which is what the old merge meant. A build from before this
change rejects entries with the new bits.

## Dropping a branch

`Db::drop_branch` writes one frame: a `Refs` without the branch. Merge links name entries, not
branches, so a merge of the dropped branch still knows what it took, and those commits stay as
described under Merges and pruning. The frame also removes the branch's name from the old
`merged` map of every other head, including records a fork inherited. Those records are keyed by
name, so one left behind would match any later branch of that name. Its version would then be read
against the new branch's log, which gives a wrong base, and merges would report conflicts and
re-apply old changes. The branch's own head and records go with it. Commits read `merged` from the
head in `tip.refs` rather than from a copy in the branch state, so nothing in memory can write a
removed record back.

A commit writes its head by name, so a handle on a dropped branch would quietly recreate it. Each
`Branch` therefore has a `retired` flag, which commits, forks and merges from that branch check
under the `tip` mutex, returning `Error::NoBranch`. `drop_branch` holds the mutex through its sync
and until the flag is set, so no commit fits between the frame and the flag. That stalls writers for
one fsync, on an operation that is rare. Retiring also closes the feed, which ends the branch's
watchers. Reads through an old handle still work, because state is a value and its objects are
there at least until a cycle, and after one the handle keeps the old file open. A later fork of the same name is a new `Branch`, so old handles stay dead.

Counting handles and refusing the drop while others exist was rejected: a clone parked on another
thread would block it forever, and handles in other processes cannot be counted anyway. A read-only
handle learns of the drop on `refresh`, and retires its `Branch` the same way instead of falling
back to the genesis value.

## Sync

`Db::pull(&other, remote, merge)` copies the branch of `other`, usually a handle on another file,
into this file as the branch `remote`, then brings this handle's branch up to it. It is three steps
that a transport can split: `have` on the pulling side lists what it holds, `bundle` on the other
side packs what that lacks, and `receive` checks and applies it. A have is a byte buffer and a
bundle a byte stream, so the same code runs over a file, a socket or HTTP: `bundle_to` writes to an
`io::Write` and `receive_from` reads from an `io::Read`, and `bundle` and `receive` are those two
over a `Vec<u8>`. `pull` runs the three in one process, and `push` is `pull` called from the other
side.

### Negotiation

A have lists commits as entry hash and version: the tip of every branch, then the pulling branch at
`last - 1`, `last - 3`, `last - 7` and so on down to `first`, about log2 of its length. The two-branch
file in the measurements below sends 137 bytes. The sender looks each one up in each of its own
chains at that version, O(log n) through skip links, and keeps those whose hash matches. Entries
link their parents, so a commit with the same hash in both files has the same history below it.
For each recognised commit the sender binary-searches the newest version its branch shares with
that commit's chain, O(log^2 n), and the newest of those is the floor: its commits above the floor
are new. Commits are matched by hash. A version alone means nothing across files.

### What is sent

The new commits' entries, and the objects their roots reach that the receiver lacks. ferro has no
index from hash to offset, so what the receiver lacks is found by comparing trees. The roots of the
recognised tips, and of the commit that set the floor, are anchors: trees the receiver is known to
hold. Each new root, oldest first, is compared with the root before it, and the first with the
anchors, one level at a time. The old side expands a level and records each object it reaches as
known. Then the new side expands a level, and a new object that is known or already sent is not
followed. An old object that the new tree still links is common to both and is not expanded.

The first version compared every new root with the anchors at once, and sent 185 objects and 95 KB
for two commits that changed one key each in a 5,000 key map. An old node that the first commit kept
and the second replaced counted as common, so nothing under it was known and the second commit's
copy went whole. Root by root it sends 12 objects and 7 KB.

A `Map`'s shape depends only on its contents, so consecutive roots line up level by level. A change
that adds or removes a level moves the unchanged subtrees one level up or down. The old side expands
before the new side looks, so a subtree that sits one level deeper in the old tree is still found,
and only one moved further is sent again.

A sent merge commit links the commit it merged, and the receiver needs that commit to find later
merge bases. If the receiver named it in its have, its mark goes into the bundle's list of commits
the receiver holds, and the link resolves to it there. Otherwise the merged commit is sent, with its
chain down to the newest entry a recognised commit's chain shares with it, and the same for the
merges inside it, so far as the sender keeps them under the retention rule for merges. A merged
commit the sender has let go is not sent, and the receiver writes the link with offset 0, which the
merge-base search treats as pruned history. The receiver may keep more of its history than the
sender did, so a merge can claim older commits than arrived. The search finds where a merged chain
ends by its links, not only by the merge's limits, and a chain cut short reads as pruned there.

A sent object that links a known one names it with a hint: the anchor or earlier hint it hangs from,
and the index of the link there. The receiver follows the path in its own file and checks that it
ends at the same hash, which costs one read of each parent on a changed path. A link to a sent object
needs nothing, because objects go children first. Bodies are LZ4 compressed when that is smaller,
as in the file.

The sender plans without bodies. The tree comparison reads only the links of each record, and keeps
the hashes it will send, with the links of the first 64K or so objects and links so that a small
bundle reads nothing twice. The sent commits are then visited in generation order, each depth first
through what it links that is to be sent, so a commit's trees and parents are out before it and the
stack is as deep as a tree, not as a history. That walk names the hints, so the header, with the
anchors, the hints and the object count, goes first. Then each object is read once more, its hash
checked, and written in 64 KiB chunks with the LZ4 bytes the file already holds for it, so nothing
is compressed twice. Planning holds about 110 bytes for each object sent. `pull` runs the sender on
a second thread once it sends 256 objects, with eight chunks in flight between the two.

### Applying

`receive` checks everything before anything it writes can be found:

- Every object is hashed as the bundle is read. Nothing is reserved from a count the bundle
  claims: lists grow as entries are read, and a body is read into memory as it arrives. The bodies,
  after LZ4, may come to at most 16 times the bytes read so far plus 16 MiB, checked before a body
  is read, and no body may pass the 4 GiB a record can hold. The sender sends a body uncompressed
  when compressing it would break that rule, so a bundle from `bundle` always passes. The stream must
  end after the last object.
- The new commits are walked from the head through `prev`, and their versions must descend one at a
  time. A head at 2^63 or above is rejected, so a fast-forward to it cannot leave the next commit to
  wrap the version past `u64::MAX`. The commit below the oldest one sent is looked up in the
  receiver's chains by hash and version. Found, the landed branch continues into the receiver's copy
  of it and keeps the lowest `first` among the chains that hold it, so a fast-forward does not cut
  our history short to where another branch was pruned. Not found, the link is written with offset 0
  and the branch starts at the oldest sent commit, the way a pruned chain does.
- Every commit the bundle says the receiver holds must still be in one of its chains. If the
  receiver pruned or dropped one after it sent its have, `receive` returns `Error::StaleHave`,
  writes nothing, and a bundle built from a fresh have lands. `pull` returns it as well rather than
  retrying, so a caller sees the same error whether it pulls in one process or over a network, and
  decides itself whether to try again.
- Every link resolves to an object earlier in the bundle, to a hint whose path ends at the same
  hash, or, for a commit's `prev` or `skip`, to the receiver's entry at that version, checked by
  hash, or to offset 0 below the landed branch's `first`, which nothing follows. A sent commit's
  `prev` or `skip` that lands on a sent commit must name the one at that version, so a lookup through
  skip links cannot reach a commit the `prev` chain does not hold. Anything else rejects the bundle,
  and so does an object nothing links to.
- A sent commit's merge link resolves to a commit in the bundle, which must decode as a commit, to a
  commit the bundle says the receiver holds, or to offset 0. Each merged chain in the bundle is
  checked like the landed branch: versions descend one at a time, and every `prev` and `skip`
  names the commit at that version, in the bundle or in the receiver's chain it joins. A commit
  must use exactly the links it declares.
- Every sent commit's generation, reach, last merge and bounded bit are recomputed from its parents
  and must match, its time must not be before either parent's, and its merge floor must not be past
  the merged commit's version. Where a parent lies in pruned history, only what can be checked is:
  the generation must exceed the known parent's, and so on. The numbers are a function of the
  graph alone, so a commit hashes the same in every file, and a forged generation or merge link is
  rejected rather than landed.
- No sent commit may be dated more than a day past the receiver's clock. Commit times only go up
  along a chain and a merge takes the later of its parents' times, so a forged time of `u64::MAX`
  would have pinned every later commit there. A device whose clock runs more than a day fast cannot
  be pulled from until the puller's clock catches up.
- Every object the bundle sends is checked as the type it will be read as, below.

All the objects and a `Refs` that points `remote` at the new head are then one frame, synced like a
commit, so a crash leaves the previous head: a torn frame is not replayed. Branch handles on
`remote` are rebound and its watchers fire. Their notices drop once the pull has released `tip`, so
a `watch_when` test may commit or pull. A bad bundle is `Error::Bundle` and leaves the file as it
was. Flipping every byte of a 3,358 byte bundle with three masks gave 10,073 rejections and one flip
inside an LZ4 body that decoded to the same bytes, and so landed the same commit.

### Where checked objects wait

A bundle is read once and not kept, so its objects have to be somewhere between being hashed and
being published. They go into the frame that will publish them, before that frame has a header.

`receive` reads the bundle's header, then takes the `tip` mutex and resolves the anchors and hints
against this file, so a stale have fails before any object is read. A frame is a header at the end
of the log followed by records, and the header names the frame's end, the refs it publishes and a
hash over the whole frame. The transaction that will become the landing frame starts at the end of
the log with its header left as zeros. Each object is appended to it as it is hashed, with its links
resolved to offsets: to an object sent earlier, to an anchor or hint, or, where a link names
something not sent, to offset 0 for now. Whenever 1 MiB has built up, the transaction writes it to
the file past the end of the log. The checks above read the objects back, from the file or from what
is still buffered, without hashing them again, since they were hashed on the way in.

Only a commit may link to something not sent, and an object that does and is not one of the new
commits rejects the bundle. Which commit such a link names is only known once the chains are walked,
so after the checks each sent commit whose links differ from what the walk resolved is appended
again with the right ones, in generation order so that its parents' final copies come first. A clone
into an empty file rewrites none, and an incremental pull rewrites its few commits. The first copies
stay in the frame with nothing linking to them, and the next reclamation cycle drops them. The refs
that point `remote` at the head go last. Then the header is written over the zeros, with a hash that
reads the part already on disk back in 1 MiB chunks. A bundle smaller than 1 MiB never reaches the
file before that, and its frame is written in one piece like a commit's.

Until the header lands, nothing can find the objects. Readers bound every offset by the published
end of the log, nothing outside the frame links into it, and the dedupe cache learns its objects
only once the frame is written. A crash leaves zeros where the header goes, or a header whose hash
does not cover what reached the disk, so `open` stops scanning there and truncates the rest: the
previous head comes back and the partial bundle is gone. A refused or failed receive, including one
that filled the disk, cuts the file back to where the frame began and does not poison the store. A
power cut at every write of a receive that spilled several times, keeping or dropping each write not
yet synced, always opened at the previous head, the landed remote or the forwarded branch, with
nothing past the end of the log.

The `tip` mutex is held from the bundle's header to the end of the stream. Commits on the receiving
file wait for the transfer, and so does reclamation, which needs the mutex to prune and to swap: a
cycle waits at whichever it reaches first, so the offsets the frame hands out stay in this
generation, and pruning cannot take away a commit the frame links to. A slow peer therefore stalls
writers on the receiving file. A caller that cannot have that writes the download to a file and
receives from it.

The receiver holds a 64 KiB read buffer, up to 1 MiB of frame not yet written, the object being
read, and for each object its hash, its offset in the frame and the types it was checked as, under
500 bytes an object. An object is held whole, about three times over while it is read, hashed and
copied into the frame, so the largest value sent bounds memory too. A hostile stream gets no more:
48 distinct objects of 1 MiB took 2.1 MiB of heap, and were cut off the file when the stream ended
early.

### Checking trees

A `Map` decodes lazily, so decoding a root as `T` says nothing about the nodes under it. A bundle
whose root decoded and whose node did not used to land, pass `verify`, and panic on the first read
that reached the node. `receive` now checks each object it is sent, and no object it holds, as the
type that will read it.

`Data` has a `visit` method that walks one encoded value without building it. The derive implements
it field by field, `Option`, `Vec`, tuples and the other containers forward it, and the default
decodes and drops the value, so a hand-written `Data` gets a correct visit for free. A decoder can
carry a list of typed links. `Map::decode` adds its root link to that list as a node of its `K`, `V`
and `S` with the count it claims, so any value that holds a map, through a derived struct, a hand
written impl, a `Table`, `Set` or `List`, reports where its nodes are. A node check decodes the
keys, checks that they ascend, that each hashes to the node's level and that they fall inside the
range the parent gives the node, visits the inline values and summaries, and reports each spilled
value as a `V` and each child as a node one level down at most, with the separators around it as its
range. It returns the node's count and level, which must match what the link claimed.

The receive check is a worklist that starts at each new commit's root as `T`. A link to a sent object
checks that object as the kind the link names, at most once for each type, and pushes its own links.
A link that resolves to an object the receiver holds is not walked into: what the receiver holds is
trusted. Its type comes from the hint path. The anchor root is a `T`, and each hint step checks the
held parent as its kind to learn the kind of the link the step follows, one read per parent on a
changed path, which the hint resolution already paid. A held child agrees with a sent parent when it
has the same type and count, its trusted range lies inside the new one and its trusted parent's level
is no higher. Otherwise the held node is loaded and its leftmost and rightmost keys are found down
its edges, O(depth). A link that names held data of another type is accepted only if that one object
checks as the new type and holds no typed links of its own: a string spilled in one map and the same
bytes as `Bytes` in another are one object. Anything else rejects the bundle.

The check costs the bundle, not the database: every sent object is decoded once, plus one read per
parent on a hint path and the edges of a reused subtree whose range narrowed. A node reached again
under a range its first check does not cover is checked again, and those rechecks may not exceed
twice the bundle's body bytes. Cloning a `Map<u64, String>` of 400,000 keys, 36 MB in 50,143 objects,
took 164 ms against 102 ms without the check, and a bundle editing 1,008 keys spread over it, 3.2 MB
in 2,480 objects, took 18.8 ms against 15.5 ms.

Links a type does not name are not checked: a raw `Ptr`, or a field this schema no longer knows.
Nothing reads them as a type. Summaries are checked as `S` but not recomputed, because a summary is
the sender's data like any value, and spilled values are not checked for the size that made them
spill.

`Db::verify_typed` runs the same check over the whole file: the refs, and the roots of every retained
commit, as `T`, each object checked once per type. It costs a decode of everything reachable, so it
is a separate call from `verify`, which knows no types and only re-hashes.

A fuzz test parses an honest bundle with hints, applies one or two mutations to its tree objects,
flipping, setting, cutting or inserting a byte, or dropping, swapping or retargeting a link,
including to held objects through the hints, and re-hashes up to the head so the hash checks pass.
Every result must be refused, or read back whole: each map's `len` equal to its iteration, every
iterated key found by `get`, every table row found through its indexes, and `verify_typed` passing.
Before the check, a single flipped byte landed and a read panicked. With it, 131,000 changed
bundles over random seeds landed and read back whole one time in seven, and were refused otherwise.
The first version checked levels and order inside a node but not the range a parent gives it, and
the fuzz test found a flipped key byte that put a key where iteration saw it and `get` did not.

### Bringing the branch up

A second step compares the pulling branch with `remote`:

- The same tip, or the same root: `UpToDate`.
- Our tip is the remote chain's entry at our version: `FastForward`. Our head takes the remote chain
  in a frame with no commit. The chains share our entries, so versions carry on from ours, and
  `head_hash` is now the same in both files.
- Their tip is our entry at their version: `Ahead`.
- Otherwise the merge-base search of `merge_with` runs under the writer lock against our tip and
  the landed `remote`. If their tip is the base, we already hold it through a merge: `Ahead`.
  Otherwise `merge` runs against that base, or against the virtual base after a criss-cross, as a
  commit on our branch that links their tip as its second parent.

Each outcome that shows their tip is an ancestor of ours records `(their version, our version)`
under `remote` in our head's `merged` map. The merge link says the same thing, but a record survives
our side of it being pruned, and the search treats it as a known ancestor of our tip, so a branch
pruned past its last sync with an idle remote still finds itself `Ahead`. Equal roots without that
ancestry record nothing, since a record there would make the remote's history look like ours. A crash between the landing and this step leaves `remote` landed and
our branch as it was. The next pull's have names the landed tip, so its bundle is empty and the step
runs again. `try_pull`, `try_push` and `try_receive` take a merge closure that returns a `Result`,
and an `Err` from it leaves the file as that crash would, with the error returned in the caller's
type. Undoing the landing as well would take a frame of its own and make the next pull send the
same commits again.

### Commit identity

An entry's hash covers its version, time, message and root, and its parent's hash, so a pulled
commit can only be the same commit in both files if it keeps its version. It lands verbatim, with
the version and time the other file gave it, and `remote` mirrors the other file's branch. A pulled
commit joins our branch only by fast-forward, and then sits at the version it has there. A merge
adds one commit at our next version, timed by our clock and never before our previous commit, and
the commits it merged stay on `remote` under their own numbers. So a version never changes meaning on
a branch: a pull appends to our chain or adopts one that extends it. After a divergence, `at(7)` on
main and on `remote` can be different commits, so compare across files by hash, with `head_hash`
and `Log::hash_at`.

Each commit's time is at least its previous commit's, as described under Commit times, so time
never goes back along a chain and `as_of` works after a fast-forward. A pulled entry keeps its stamp
flag, so the next commit after a fast-forward onto a stamped commit takes a later millisecond. A
device whose clock runs ahead holds the times of our later commits at its reading until our clock
passes it. `receive` refuses commits more than a day past our clock, so that hold lasts a day at
most.

### The merge base

A sync merge links the remote commit it merged, and pulled commits carry their merge links, so after
A merges B and B merges A each side's merge is an ancestor of the other's next commit, in every file
that holds them. The base is therefore the one `merge_with` finds, a best common ancestor over the
whole merge graph, with the records under our head as known ancestors of our tip. It replaces a
search by content: our roots since the last record went into a set, and the remote chain was walked
for the first root in it, then the record, then the shared prefix, then genesis. Matching by content
lost a revert routed through a third device. If A took B's change by a merge, and C took it by
fast-forward and set the value back, C's commit had the same root as one of A's older commits, so
it looked like a state A already held, and the revert was dropped on A's first pull from C. The
graph base is B's commit, which both have, and the revert applies. A change that reaches us through
a third device is a common ancestor once it is, so it is merged once, and a remote that has
nothing we lack is found to be an ancestor of our tip and reported `Ahead`.

Devices that pull from each other while a third relays make criss-crosses routinely: when A merges
B's commit and B merges one of A's that C passed on, each has merged an older state of the other,
and the next pull between them has two best common ancestors. The pull then merges against their
virtual base, as `merge_with` does. `examples/notes` needs no change for that: its merge is a
function, so it is `FnMut`, and since every note only moves forward under its last-writer-wins
join, a three-way merge against any common ancestor gives the join. The virtual base spares it
conflicts over changes both sides already hold, which it counted among the notes it applied.

When the base could lie in pruned history, the search returns `NoCommonAncestor` and the pull
leaves our branch alone, with `remote` landed. That happens when the remote has pruned past the
commit we last shared with it: its bundle no longer connects to anything we hold, and merging
against genesis, as before, made every key on our side look like our own addition and brought
back what the remote had deleted since. A device with bounded history therefore needs to keep what
it commits between syncs.

A pull is not symmetric: the closure gets `ours` and `theirs`. Devices converge only if it gives the
same result whichever side is ours, as last-writer-wins on a Lamport clock does. `examples/notes`
syncs with a pull each way, and a third pull when the second merged, so that each side has recorded
the other's last commit. A sync after that sends two empty bundles and writes nothing.

### Measurements

On one NVMe SSD with ext4, in release:

| Case | Sent |
| --- | --- |
| `Map<u64, String>`, 100,000 keys, 9.0 MB file: first pull into an empty file | 8.95 MB, 12,680 objects |
| The same, three commits of one key each | 15.2 KB, 22 objects, and a 137 byte have |
| `notes-scale`, 100,000 notes, 14.4 MB file: first sync | 14.3 MB, 41,093 objects, 220 to 240 ms |
| The same after 6 edits on one device and 8 commits on the other, one note edited on both | 106 KB, 139 objects, 11 to 13 ms, both ways and a merge on each side |
| The same, synced again | 430 bytes, no objects, nothing written |

A notes edit rewrites a path of four or five nodes in the rows and, for a tag, in the index, 6 to
8 KB a commit, and every commit since the last sync is sent, not only the final state.

Streaming was measured with `cargo run --release -p ferro-bench --bin clone`, whose `build`, `pull`,
`send`, `receive` and `incremental` steps each run in a process of their own, on a
`Map<u64, String>` of a million keys with 320 byte values, loaded in ten commits: a 330 MB file
whose clone sends 330 MB in 124,937 objects. Each case ran with fsync three times, alternating with
the in-memory version on a machine other work was sharing, and these are the medians:

| Case | In memory | Streaming |
| --- | --- | --- |
| `pull` into an empty file | 2.63 s, 1,085 MiB RSS, 1,494 MiB heap | 2.25 s, 65 MiB RSS, 56 MiB heap |
| `bundle` into a file | 1.37 s, 677 MiB RSS, 1,125 MiB heap | 1.20 s, 19 MiB RSS, 13.5 MiB heap |
| `receive` from that file into an empty one | 1.69 s, 1,071 MiB RSS, 1,276 MiB heap | 1.40 s, 63 MiB RSS, 56 MiB heap |
| `pull` of three one-key commits into the clone, 200 times | median 2.78 ms, fastest 1.57 ms | median 2.58 ms, fastest 1.54 ms |
| The same with `Durability::Buffered` | median 0.98 ms, fastest 0.65 ms | median 0.93 ms, fastest 0.60 ms |

The heap that is left is the receiver's bookkeeping, 467 bytes an object, and the sender's, 113. It
got faster mostly because neither side compresses a body the file already holds compressed, and
`pull` runs the two sides on two threads. A small pull costs what it did: its bundle is below 1 MiB,
so the frame is written in one piece, and the planning reads that streaming added are served from
the links the tree comparison kept.

### Limits

- A bundle streams, but the receiver keeps under 500 bytes for each object and the sender about 110,
  so the first pull of a file of tens of millions of objects still needs gigabytes. Each object is
  held whole, so one huge value needs a few times its size. A hostile stream costs the same, for what
  it carries and not for what it claims.
- `receive` holds the `tip` mutex for the whole transfer, so a slow or stalled peer stalls commits on
  the receiving file until the reader errors or times out.
- Files that share no commit share no anchors, so their first pull sends everything, even trees both
  happen to hold.
- Each commit in a have is looked up in every branch of the sender, so recognising them costs the
  receiver's branches plus log n, times the sender's branches, times log n.
- A subtree that moved more than one level deeper than where the receiver holds it is sent again.
- A value that holds a `Ptr` into pruned history cannot be sent.
- Each pull that brings a head `remote` does not hold replaces `remote`, including commits made on it
  here. If the remote branch's history no longer contains what was landed before, records under that
  name are dropped, and if nothing retained connects the two branches the pull refuses with
  `NoCommonAncestor`.
- A merged commit the receiver holds only inside another commit's merged history, not on a chain
  its have names, is sent again.
- The receiver checks every sent object as its own `T`, so peers need schemas that decode each
  other's trees, not only their roots. A table without one of its index fields passes, because the
  check visits fields and does not rebuild the index. Reading it rebuilds the index, and if two rows
  share a value of a unique index that read fails, so a peer can still send a table that does not
  read. Checking it at receive would mean reading every row.
- A summary is checked as an `S`, not recomputed, so a peer can send a subtree whose summary is not
  the fold of its entries, and `summary` returns what it sent.

## Reclamation

Retention says what to keep, and reclamation gives the rest back while writers run. A cycle copies
what is retained into a new file and renames it over the old one. Each file is a generation.
`Db::compact` runs one cycle on the caller's thread. `Options::history` runs them on a background
thread once the file has grown by `every_bytes` or by the size it had after the last cycle, whichever
is larger, so the copying stays proportional to what was written.

1. Prune. One frame raises `first` on every branch. Branch state changes before anything else, so
   `at` stops serving a version before its objects are left behind.
2. Copy, with no lock. Walk the published refs and write each reachable object to
   `<file>.compacting`, re-emitting links with new offsets, verifying each object's hash and
   compressing again. Chain entries are copied like anything else, but a link to an entry that is
   past `first` and was not brought in by a retained merge, as described under Merges and pruning,
   is written with offset 0 instead of being followed. Copies are keyed by hash, so a later pass from newer refs
   visits only what is new. Data is immutable, so no write barrier is needed. Passes repeat, up to
   six, until one copies less than 256 KiB.
3. Sync, with no lock, so the pause does not pay for it.
4. Swap, under the `tip` mutex. This is the only pause. Wait for queued commits, copy what is left,
   write both superblock slots and sync. Open the new file, which takes its writer lock, and read
   every branch's tip and root from it. Rename it over the old one, sync the directory (on Windows,
   flush the renamed file), and rebind every branch.
5. Retire, with no lock. Clear the old generation's cache. Its file goes when the last handle does.

The new file continues the frame sequence, so group commit tickets stay in order. It has no frames
yet, only records and a superblock naming them, the same shape offline compaction produces.

An entry's hash covers its version, time, message, root and links, so a branch can make a commit
identical to one pruned from another branch, in the same millisecond or when the clock has stepped
back. Pruned hashes are therefore kept apart from copied ones and drop out of the set when a chain
retains that hash again, and a link already at offset 0 is never followed.

### Generations

A `Store` is one generation: a file, a node cache, a dedupe cache and an id. Offsets mean nothing
outside it. Decoded nodes, memos and cached entries all hold offsets, and each belongs to the
generation it was read from, so none of them needs rewriting. A cycle replaces the whole set, and
the first reads after it are cold. The page cache is warm, because the copy just wrote those bytes,
so a miss costs a decode and not a disk read.

A branch's state holds its chain and its `Store` together, because a chain offset read through the
wrong file would fail its hash check and look like corruption. `at`, `as_of`, `log` and `history_of`
all take both from one state. Commits compare root hashes, not offsets, so the same commit in two
generations is equal.

Whatever holds a `Store` keeps reading the file it came from: a `Snapshot`, a `Map` cloned from one, a
`Log`, a `History`, or a handle on a dropped branch. Nothing old ever fails, because nothing old
changes. A cycle frees no disk space while such a handle lives, and it holds the whole old file, not
only its version. `Space::retired_bytes` shows that. A branch's watcher feed would hold the file too,
so a swap rebinds its latest snapshot without firing it. Old generations read uncached, because a
cached node can hold a `Map` that holds the `Store`, which would keep the file open forever.

Writing is where a generation can go wrong. A `Map` from an older generation put into a commit must
not write its old offsets into the new file. `Map::encode` already compares store ids for maps from
another database, so such a map is rebuilt and written by value, and a dirty one is too. This takes
time proportional to the map's size, under the `tip` mutex, so `restore` of a snapshot held across a
cycle stalls writers for that long. Stale maps are the only way to reach it: the working copy in a
commit closure comes from the rebound tip.

`Ptr` and `Commit` are `Data` too, so a program can put one in its root. Those carry no store to
compare, so encoding a `Ptr` reads the object it names, unless the same commit wrote it, and fails
with `Error::StalePointer` if it is not there. A pointer read back from the current generation is
fine, because the copy rewrote it. One held in memory across a cycle is refused.

A read-only handle keeps its view, in the old file if need be, and `refresh` notices that the path
names a new inode, or a new file index on Windows, opens it, and rebinds. Its watchers fire only if
the version moved. A process that never refreshes never fails, and holds the old file open instead.

### Crashes

Until the rename is durable, the name points at the old file, which is complete and has every
acknowledged commit, and a writer's open removes the temp file. The new file is synced before the
rename, so once the name points at it, it is complete. Reading each branch's tip and root from it
first means a bug in the copy fails the cycle while the old file still has the name. Writers stay
paused until the directory sync returns, so no commit is acknowledged in a file whose name could roll
back. If that sync fails the store poisons, because the new file may or may not be the one found
after a crash. Both slots are written so that one damaged sector cannot lose a new generation with no
commits yet.

A failed copy, a full disk or a failed rename removes the temp file and reports the error from
`Db::compact` or in `Space::last_error`, and commits never notice. A store poisoned while a cycle
copied stays poisoned, because the swap checks first. On Unix the new file takes the old one's mode
and owner, and a path through a symlink stays a symlink. Windows changes how the rename and its sync
work, but not this order, as described under Windows.

### Measurements

Four workloads ran for 15 seconds each on the test NVMe with ext4, with fsync, keeping the last 100
versions and starting a cycle every 64 MiB. These are medians of three runs, alternated on a machine
other work was sharing:

| Workload | Commits/s without cycles | With cycles | Longest pause | File after a cycle | Live | Grows to without cycles |
| --- | --- | --- | --- | --- | --- | --- |
| `metrics`, one commit per scrape | 5,240 | 5,190 | 2.8 ms | 1.3 MiB | 1.4 MiB | 248 MiB |
| `ledger`, single entries | 4,810 | 4,820 | 3.5 ms | 4.6 MiB | 4.5 MiB | 443 MiB |
| `wiki`, edits of 64 KiB pages | 670 | 680 | 4.8 ms | 7.2 MiB | 7.1 MiB | 100 MiB |
| Table updates, 200,000 rows | 5,090 | 5,090 | 3.2 ms | 13.2 MiB | 12.2 MiB | 332 MiB |

p99 commit latency moved from 0.93 to 0.94 ms, 1.21 to 1.33 ms, 4.51 to 4.59 ms and 0.95 to 1.01 ms.
The file saws between what is live and 64 MiB above it. While both files exist the peak is higher by
what has been copied, which came to 77 MiB on `ledger` and 95 MiB on table updates.

`metrics load` at 20,000 points a second with one commit per scrape grew to 190 MB without cycles.
With them the file sawed between 3 and 62 MB with two cycles in 30 seconds, and the longest pause was
1.8 ms. It used to stall writers for 0.34 to 0.74 s, dropping the database each time.

With 8 readers on a 1M row table and 60 MiB live, at 4,200 commits a second with or without cycles,
reads went from 1.67M a second to 1.54M, because every swap empties the node cache. p99 read latency
went from 53 to 55 µs. Each cycle took about 1.1 s in the background and the longest pause was 5 ms.
In `Durability::Buffered` the longest pause was 7.2 ms at 18,000 commits a second on `metrics`, 7.6 ms
at 11,700 on `ledger`, and 8.5 ms at 15,800 on table updates.

Three alternatives were built or simulated and left out. Hole punching and segments were worked out
from the real layout of live records in each workload's file. Relocation ran live, and that
comparison was made on one base, before value columns landed:

| | Reclaimed per cycle | File after a cycle, 1M rows | Pause | Buffered commits/s | Reads/s, 1M rows | Copied, 1M rows | Lines in `ferro/src` |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Copy and swap | 100% | 62 MiB | 2.9 to 5.6 ms | 11,900 to 19,400 | 0.96M | 564 MiB | +1,149 -299 |
| Hole punching alone | 44 to 91% | not run | 2 to 3.5 ms | not run | not run | 0 | not separate |
| Copying sparse runs to the tail, then punching | 64 to 96% | 87 MiB | 2 to 3.9 ms | 10,500 to 16,000 | 0.99M | 136 MiB | +1,516 -173 |
| Fixed segments, dropped when dead | 0 to 90% | simulated | none | not run | not run | 0 | not built |

- Hole punching needs no pointer changes at all, because offsets stay valid. But a hole frees whole
  blocks, and one live record pins its block. Over a whole run of 0.2 to 2.6 GB it freed 99.9% of the
  garbage in `metrics`, whose live data clusters by time, but 70 to 90% elsewhere, leaving 8 to 17
  times the live size. Per 64 MiB cycle it freed 44 to 91%, because the same live records sit in less
  garbage. Each commit scatters survivors across the log, so this decides it.
- Copying only the records in sparse runs to the tail and punching the rest copies a quarter as much
  on the 1M row table. Offsets stay valid through a table from offset to place, so caches stay warm.
  It was built in full and is in the history at `c384c6c`. It reads 3% faster than swapping and needs
  less spare disk. It lost on the rest. It ran 13 to 21% fewer Buffered commits, with a table check
  on every link and a pin for every snapshot in the path. The log's apparent size only grows, so ext4
  would stop it at 16 TiB, which at 10 MB/s of log is 20 days, and a tool that does not preserve holes
  would copy the apparent size. A `Map` that outlived its snapshot and a read-only handle that lagged
  both needed a new error, and a filesystem that cannot punch freed nothing. Review found that a head
  rebuilt by dedupe could be reclaimed, which left the file unopenable. Liveness there was a property
  every path had to get right, and here an old object is simply never freed while anything can reach
  it.
- A segmented log drops a segment only when nothing in it is live. At 1 MiB or more that freed 0 to
  1% of the garbage in `ledger`, `wiki` and table updates, and 90 to 99.8% in `metrics`. Live data
  would have to move, which needs the same table or rewritten parents, and a directory gives up the
  single file.

### Limits

A cycle reads, verifies, recompresses and writes everything retained, in one thread, at about 50 MB/s,
so a 10 GB database takes minutes and needs 10 GB free. The trigger keeps that to one extra byte
copied per byte written, at worst. `Keep::All` copies all of history, so `history` is what makes
background cycles worthwhile. The copy holds a map from hash to offset, about 50 bytes per object.

The pause copies what the last pass left, so it depends on how much reachable data changed in that
pass, not on the size of the database. Writers that outrun the copier for all six passes make the
pause longer. That did not happen in any run here, at up to 19,000 commits a second, but nothing
bounds it.

`Space::live_bytes` is an estimate: the size of this generation when it began, plus the bytes since
then scaled by the share that survived the last cycle. Before the first cycle it is the file size.
`verify` still gives the exact number.

## Offline compaction

`admin::compact` is the same copy in one pass, with no writer running. It prunes by raising `first`
in a private copy of the refs and needs no user types. The copy is iterative, because a chain is as
deep as the number of commits. A read-only handle follows it on `refresh` the same way it follows a
cycle.

## Windows

`platform.rs` holds everything the store asks of the OS that differs between Unix and Windows:
positioned reads and writes, the writer lock, the identity of a file, a reader's sync, the directory
sync and the owner of a new generation. The rest of the store is shared.

### Reads, writes and the lock

Positioned I/O loops over `seek_read` and `seek_write`, which can return short counts as `pread`
and `pwrite` can. They also move the handle's file pointer, which nothing reads. Windows serialises
requests on a handle opened without `FILE_FLAG_OVERLAPPED`, which `std` never sets, so readers in one
process queue behind each other and behind frame writes, where `pread` lets them run side by side.
That costs throughput, not correctness.

`File::lock` would shut readers out. On Windows it is `LockFileEx` over the whole file, and Windows
byte-range locks are mandatory: any other handle, in this process or another, fails to read a locked
range with `ERROR_LOCK_VIOLATION`, so no read-only open could read beside a writer. The writer lock is
`LockFileEx` on the single byte at offset 2^62 instead, past the end of any real file, exclusive for
a writer and shared for a reader's scan, and always with `LOCKFILE_FAIL_IMMEDIATELY`, so it behaves
as `try_lock` does. A lock belongs to its handle, so a second writer in the same process conflicts,
as with `flock`, and closing the handle releases it. `Store::close` unlocks explicitly, because
Windows documents that it releases the locks of a process that dies after a delay that "depends upon
available system resources", so a writer that opens right after another was killed can get
`Error::Locked`.

The identity that `open_locked` checks after locking, and that `refresh` compares to notice a cycle,
is the 64-bit file index from `GetFileInformationByHandle`, read through a handle opened with no
access rights, as `std::fs::metadata` does. `std` keeps the index behind the unstable
`windows_by_handle` feature. On NTFS it is unique on the volume and survives a rename, which is all
both checks need. ReFS ids are 128 bits, and this 64-bit index is not guaranteed to tell two files
apart there.

### Durability

`sync_data` and `sync_all` both call `FlushFileBuffers`, which writes the file's cached data and
metadata and then flushes the device's write cache. Every commit therefore pays for an fsync, not an
fdatasync, and the durability point does not move: once the flush returns, the frame survives a
power cut, unless the disk ignores flushes or Windows was told not to send them ("Turn off Windows
write-cache buffer flushing on the device"), the same caveat as a Linux disk mounted without
barriers.

The flush needs a handle with write access. A read-only open that has to sync the frames a dead
writer left, as described under File format and durability, first checks that the path still names
its file. If it does, the reader opens the path for writing and flushes that. A process that may not
write the file gets `PermissionDenied` there, rather than reading frames that a power cut could
still take back, and opens normally once a writer has recovered the file. If the path names another
file, a cycle replaced this one and there is nothing to sync. A writer lets go of a generation only
after the rename that replaced it is durable, so no crash names it again, unless the writer died or
poisoned between the rename and its flush. Even then, `Durability::Fsync` synced every frame before
the swap, and `Buffered` may lose commits by contract. The reader keeps the old view, and its next
`refresh` moves to the new file.

The reader used to reopen its own file with `ReOpenFile` instead. That failed with access denied
whenever it had opened the path just before a swap and taken the lock just after the writer retired
the old file, because Windows refuses every new handle to a file that has lost its name. Checking
the path also means the reader never needs write access to a file that is not the database any more.

Windows has no directory sync. A directory opens only with `FILE_FLAG_BACKUP_SEMANTICS`, and nothing
documents what `FlushFileBuffers` does to one. Where Unix syncs the parent directory, after creating
a file and after a cycle's rename, Windows opens the path for writing and flushes the file it names.
NTFS logs a create or a rename as one transaction in its metadata journal, so after a crash the name
points at the old file or the new one, never at neither. The rename also changed the renamed file's
own record, and ferro relies on a flush of that file committing the journal through the rename.
PostgreSQL's `durable_rename` relies on the same thing, since it too skips the directory on Windows
and flushes the file under its new name, but no Win32 document promises it. With the directory sync
read as this flush, the argument under Reclamation holds unchanged: writers stay paused until it
returns, and a failed flush poisons the store. FAT32 and exFAT keep no journal and are not
supported.

### Renaming over an open file

A cycle renames a new file over one that is still open, since the old generation stays open for
every snapshot, `Log` and follower that holds it, and Windows normally refuses that. It works when
two things hold. Every handle on the file must share delete: `std` opens files with
`FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` unless told otherwise, and ferro never
tells it otherwise. And the rename must use POSIX semantics: when `MoveFileExW` is refused,
`std::fs::rename` retries with `SetFileInformationByHandle(FileRenameInfoEx)` and
`FILE_RENAME_FLAG_POSIX_SEMANTICS`, on Windows 10 1607 and later where the file system supports it,
as NTFS does. The name then moves to the new file at once, and the old file loses its name but stays
whole for every handle until the last one closes, as an unlinked inode does on Unix. Snapshots keep
reading the old generation, its space comes back when the last of them drops, and `refresh` finds a
new file index at the path. `admin::compact` renames the same way.

If either does not hold, because another program holds the file open without `FILE_SHARE_DELETE` or
the file system has no POSIX rename, the rename fails before anything has changed. The cycle then
fails like any failed rename: the temp file goes, the error lands in `Db::compact` or
`Space::last_error`, commits carry on in the old file, and the next trigger tries again. Nothing is
lost, but the file does not shrink. The cycle is the retry, so the rename does not loop on its own,
which would hold the `tip` mutex and stall commits.

Two other designs were weighed. Generation-numbered files, with a small file naming the current
one, never rename over an open file and work on any file system, but the path would stop being the
database, so copying or backing up one file would no longer work, and the small file needs its own
atomic, durable update, which is a rename again. Deleting the old file and then renaming the new one
into place needs no POSIX semantics, but a crash between the two leaves no database at the path.

A new generation keeps the old file's read-only attribute, which is all `set_permissions` carries on
Windows, and takes its ACL from the directory rather than from the old file. The owner is not
copied.

### Tests

CI runs the library's tests on `windows-latest`. Tests that interpose libc symbols through `dlsym`
(`fdatasync`, `fsync`, `pread64`, `pwrite64`, `rename`, `clock_gettime`), run a child under `sh`
with `ulimit -f`, kill or abort a child process, or make symlinks or check mode bits are
`#[cfg(unix)]`. The traced crash simulations replay ferro's own writes and run on both. So on Windows
failed syncs, failed renames and process death go unexercised, and as on Unix, power loss was never
tested on hardware.

## Schema evolution

The derive macro writes structs as `(tag, length, payload)` fields, where the tag is a 16-bit
FNV-1a hash of the field name. Fields are sorted by tag, so reordering a struct does not change
its hash. Unknown fields are skipped. Missing fields use `Data::missing`, or `#[ferro(default)]`.
`#[ferro(rename = "old")]` keeps the old tag.

## Unique indexes

`#[unique]` on a field, or `#[unique(a, b)]` on the struct for a combination of fields, adds a
`Map<value, key>` to the generated table, with `by_x(&value)` for the row, `key_by_x(&value)` for
the key alone, and `unique_x()` for the whole map. On an `Option<T>` field `None` is not indexed, so
any number of rows can leave it empty.

A clash cannot be repaired where it is found. `Table::insert` returns the old row and has no error
channel, and the two ways of carrying on are both wrong: last writer wins deletes a row nobody
named, and overwriting the index leaves a row that `by_x` cannot find. So `try_insert` and
`try_update` return `Taken { index, holder }` and leave the table as it was, and `insert`, `update`
and dropping a `RowMut` panic before changing anything. Inside a commit that discards the
transaction, which is the honest result when a constraint the schema states does not hold. A
`RowMut` dropped while its thread is already panicking keeps the old row, since a second panic would
abort the process. A row may replace itself, and a row changing key may keep the value it holds.

`Table::merge` treats a clash as data, not a bug, and calls `resolve` once at most for each key. An
answer is the row for that key: `None` removes it, and a row with another key keeps our row. The
merge holds a clashing row back, and the key keeps its current row and values, until the rest of
the diff is applied, since a chain of renames clashes only on the way through. Held rows then go in
lowest key first as their values come free. A held row taken from theirs that still clashes goes to
`resolve` with `ours` set to the row holding the value, so returning `ours` keeps both of our rows.
Two rows that swap values reach `resolve` too, because neither can move first. A row that `resolve`
has already answered for, because both sides changed the key or because it answered a clash, is not
offered again. If it still clashes when nothing else can move, the key keeps our row. So a merge
never panics on a clash, the indexes always match the rows, and a row is removed only when theirs
removed it or `resolve` returned `None` for its key.

The index field is named `unique_x`, not `by_x`, so turning an `#[index]` into a `#[unique]` rebuilds
it instead of reading the old map with new types. The rebuild on decode covers unique indexes. If
two existing rows share a value it fails with `Error::Decode` naming the index, because a constraint
the data breaks cannot be honoured, so that version does not open until the data or the attribute
changes. The rebuild reads the rows through `table::stored_rows`, which returns a damaged row node as
an error, so `Db::open` fails with it instead of panicking.

## Errors

Collections read like native types, so `get` and iterators do not return `Result`. An I/O failure
or corruption during a read panics. `try_get` exists for callers that need to handle it. Inside
`update` a panic discards the transaction and leaves the database unchanged.

A decode error on read stays a panic. Every object a file holds was either written by this process,
which decodes each new key, value and root as it writes them, or checked as its type by `receive`,
so an object
that does not decode is a damaged file or a schema change that broke stored data, the same classes
as a bad hash. Turning it into an `Error` would put a `Result` on every `get`, iterator step and
comparison for a case a healthy file cannot reach, and the read would have nothing better to do than
stop. `try_get` and `try_for_each` surface it as an `Error` for a caller reading a file it does not
trust, and `verify_typed` finds it before any read does.

`Db::verify`, `common_ancestor`, `Db::have`, the bundle planner and the `admin` functions return
`Result`, so they walk the branch table and merge records through a crate-internal `try_entries`
that returns a read or decode error, not through `Map::iter`, which would panic on one.

A hash proves that an object is the one its link named, not that a file that names it was written by
`ferro`. Bundles get the full node check described under sync. Local reads trust more, but each node
they take from the file passes three checks that cost a few compares, so a crafted or damaged file
gives an error where it used to overflow a count, the stack, or the time a scan takes:

- Decoding a node sums its child counts with checked arithmetic.
- Each child link keeps its parent's level, and following a link checks that the node it reaches
  sits below that level and holds the count the link claims. A root link allows level 21, the
  highest `trailing_zeros / 3` gives. Levels fall by at least one a step, so no path is longer than
  22 nodes, which bounds every recursion over a tree: `scan`, `gather`, `flush` and the edits. A
  memo is set only after the check, so memo hits skip it, and a link that finds its node in the
  cache pays two compares. `Link::Stored` keeps the level in padding, so a link without a summary
  is still 72 bytes. On `ferro-bench`'s `reads` at 1M keys, the best of 18 runs moved gets by 1 to
  2% and scans by less than 1%, inside the spread between runs, and a cold scan, which takes every
  node through the checks, by 2%.
- A node read from the file may not claim more entries than the log could hold. Counts that add up
  and levels that fall still allow a DAG that links one child from many places: 21 nodes that each
  link the same child three times claim 10^10 entries in 10 KB, and wider nodes claim up to 2^64.
  Iteration visits exactly what the root claims, so in practice it would not end. In a tree `ferro`
  wrote no node appears twice, since sibling ranges are disjoint, so every entry costs at least its
  one byte slot tag in a node body of its own. A stored body decompresses to at most 255 times its
  length plus 255, or the store rejects it, and a record adds at least two header bytes, so a log of
  `n` bytes holds fewer than `255 n` entries. That is the bound, checked against the bytes written
  so far: a crafted file can make a scan as long as an honest file of its size could, and no
  longer. Checking that each node falls between its parent's separators would rule sharing out
  entirely, as the bundle check does, but it would cost two key comparisons a level and carry the
  separators down every path into `reach`, for a shape no file `ferro` wrote can have.

Key order, the level each key hashes to and key ranges are not checked on local reads. `Map::len`
returns the count the root link claims before anything is read. `verify_typed` checks the rest.

## Fuzzing

`fuzz/` holds `cargo-fuzz` targets. It is its own workspace, excluded from the main one, and needs a
nightly toolchain:

```
cargo install cargo-fuzz
cd fuzz
cargo run --release --example seeds -- corpus/seeds [files or directories of captured databases]
cargo +nightly fuzz run open corpus/seeds/open -- -max_len=70000 -timeout=10 -malloc_limit_mb=1024
```

- `open` and `admin` take one mode byte and a file image. The mode picks the root type (one of
  six, or a type that decodes nothing so that any file opens), read-only or read-write, and whether
  to repair the image first: recompute every object hash bottom-up, then the frame checks and the
  superblock checksums, so that a mutation gets past the hash checks to the code behind them.
  `open` then reads the value, the log, `at`, `as_of`, `history_of`, `bisect`, branches,
  `common_ancestor`, `verify`, `space` and `refresh`, and scans every map it reaches. `admin` runs
  `info`, `log`, `verify`, `space`, `layout` and, for one mode, `compact`.
- `log_entry` writes a log of crafted entries with valid hashes, mostly coherent and with fields
  perturbed by the input, plus branch heads that point into it, then runs the `open` reads over it
  and, for half the inputs, the `admin` functions.
- `map_node` writes crafted map nodes with valid hashes for six key, value and summary types,
  including a value with a `Table`, loads the map and reads, scans and edits it.

The harness treats a panic whose message starts with `ferro:` as expected inside the infallible
reads, which panic on corruption by design, and as a failure anywhere else. The `seeds` example
writes a corpus from databases it builds and from any files passed to it; files captured while
`cargo test --workspace` and the examples ran make a broader one. `cargo run --example fixture --
<target> <crash> <out.db>` turns a crashing input into the file it opens, which is how the fixtures
behind `ferro/tests/fuzzed.rs` were made.
