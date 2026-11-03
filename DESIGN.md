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

`Txn::put` skips an object whose hash it finds in `dedupe`, a bounded cache from hash to offset.
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
up during `Data::encode` and seeds the node cache with the nodes it wrote. It clones the in-memory
node unless a value held a dirty or foreign collection, in which case it decodes the written bytes
instead, so nested collections always come back clean and a foreign store is never pinned. Values
inserted or mutated since the node was loaded are decoded once as a check before they become
durable; unchanged values are not.

`Extend`, `FromIterator` and `apply`, which also takes deletes, sort the batch and keep the last
edit per key. An empty map is assembled bottom-up in one pass, which gives the same tree as
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
`verify` knows no types.

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
- Deeper links memoize a `Weak`, and only when the child was found in the cache. The cache stays
  the owner, so eviction still frees the node, and the memo keeps only the node's `Arc` allocation,
  about 150 bytes, until its parent goes. A child just read from disk is not memoized, so a one-off read
  leaves nothing behind.
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

`flush` leaves a column behind for every node it writes, cloning the values of a changed node into
key order. So a process reads what it wrote exactly as it reads what it opened, and no `Fresh` slot
outlives its commit. The price is a clone, and later a free, per value in a bulk load.

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
leaves it at most one sync behind.

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
was measured and rejected: it adds a wakeup to every uncontended commit and gained nothing under
contention.

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
and forked again at a lower version.

A `Branch` closes its feed when it drops, which is when the last `Db` handle goes, because
`Shared` holds every branch. Closing wakes parked watchers and drops the held snapshot. A `Watcher`
holds only the feed, so a parked one keeps neither the file nor the writer lock. The price is that
a snapshot nobody had taken by then is lost.

`Watcher::closer` returns a `Closer`, a cloneable handle that ends that watcher and its clones from
any thread. It sets a flag under the feed's mutex and wakes the parked, so stopping a set of workers
no longer takes a commit. `recv` then fails the way it does when the feed closes, and any other
watcher of the branch carries on. A closer holds only the feed as well.

`Db::watch_when(test)` is a watcher that sleeps through commits that do not concern it. A filtered
watcher and its clones park on a condvar of their own, listed in the feed by a `Weak`. Publishing
finds the lists with someone parked and runs `test` on the new snapshot, and wakes them only if it
passes, so a commit nobody wants costs one evaluation per filter and no thread switch, and a filter
with nobody parked costs nothing. The test runs on the thread that leads the batch, after the frame
is durable, but not while that thread holds anything. Publishing stores the snapshot, bumps the
count, wakes unfiltered watchers and lists the filtered ones with someone parked, all under the
leader's locks, and returns a `Notice` that runs the tests when it drops. Every caller makes its
list of notices before it takes a lock, so the list drops after the last guard: for a group commit
once `syncing` has cleared and the batch is acknowledged, for `drop_branch` once `tip` is released,
for a reclamation cycle once `tip` and the cycle mutex are, and for a read-only `refresh` once its
`tip` is. A test may therefore commit, refresh or drop a branch. Before this, a committing test
deadlocked on the leader's own `syncing` flag, or on `tip` when `drop_branch`, a cycle or `refresh`
held it, and a slow test held up every writer behind it. Running tests late cannot reorder what a watcher
sees, since snapshots are stored in commit order and a test only decides whether to wake: two
leaders may test out of order, and the woken watcher tests the newest snapshot itself. The call that
led still returns only after its tests, so a slow test delays that call and no other. A panic is
caught there and counts as a pass, so that the watcher wakes and hits the same panic on its own
thread. A woken watcher tests the snapshot again, since what passed may be gone, and it tests
whatever it finds when it next looks, so nothing that landed while it was busy is missed.

`queue` used to wake every idle worker on every commit, and to commit a dummy `epoch` change at
shutdown. Now workers filter on a ready job in their own queues, and the reaper on something falling
due sooner than it planned, against a value it stores before it parks. With 8 and 32 idle workers a
commit used to cost 12 and 47 voluntary context switches, and now costs 2, as with no workers at
all. The time from enqueue to handler stayed within run-to-run noise, 2.0 to 2.5 ms at the median.

Evaluating the test on each watcher's own thread was rejected, since that is a wake per commit per
watcher, which is the cost this removes. Waking one member of a filter to test for the rest would
keep user code off the commit path, at one wake per commit forever. A wake with no close has no user
here: a timeout already covers what it would do, and the queue only ever needed the end.

A `Snapshot<T>` keeps its version readable for as long as it is held, even past pruning, and so
does any `Map` or `Table` cloned from it, and a `Log` or `History` handle. All of them hold the
`Store`, and so the open file, but not the writer lock. The lock is a `flock` on the writer's file
descriptor, and closing a writer's store, when the last `Db` handle drops or a cycle retires a
generation, truncates the reservation and then unlocks, in that order, so a new writer can open while
they live. They read what they read before. The new writer's open truncates only past the last
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
- `restore` and `merge_with` commit alone, since `restored` and `merged` belong to one version.
  They, `fork`, `drop_branch` and a reclamation cycle flush the open group before they start, and a
  closure on another branch flushes it before starting its own.

The combiner is a lock passed between waiters. The group's working copy lives in `Shared` between
closures and each closure takes it under the `tip` mutex on its own thread. So there is no `Send`
bound, `update` closures keep borrowing local state, and a panic unwinds where it should. Cloning
the working copy first is the checkpoint, O(1) per collection, and an `Err` or a panic puts it back.
That closure still waits for the group, since it may have read state that never becomes durable,
except that a panic does not wait, as it never did.

A closure ending under the lock does not flush if someone else will finish the group: either a
thread has announced it is on its way to the lock, or a sync is running, whose leader wakes one
member when it ends. Otherwise it flushes. So a group closes when nobody is coming or a sync ends,
which gives one flush per fsync however many writers there are, and a lone writer flushes at once,
down the same path as before. A group stops at 256 closures, which bounds what the first of them
waits for.

Members park on the group's seal and the leader unparks each one. The first version waited on a
condvar, and notifying a condvar moves its waiters to the mutex and wakes them one at a time: that
chain alone held 32 `queue` writers to 11,600 jobs a second with a p99 of 1,107 µs, against 15,600
and 753 µs once members were unparked directly, median of three. The same change on the plain path
gained 3% at 8 writers and nothing at 32, where the writer lock is the bottleneck, so it was left
out.

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

## Merging

`Map::merge` and `Table::merge` do a three-way merge driven by `diff(base, theirs)`, costing
O(changes). A key changed only by theirs is applied. A key changed the same way on both sides is
left alone. A key changed differently on both sides goes to a `resolve` closure. Merging runs
inside an ordinary commit, so a root with several collections merges each one in plain Rust.

`Db::merge_with` finds the base, runs the closure on our tip with the base and their snapshot, and
commits. The log entry it writes has a second link, to the entry of the other branch's tip: the
merge's second parent. Commits are then a graph, as in git, and `common_ancestor` returns a best
common ancestor in it: a commit both tips reach through `prev` and merge links that is not an
ancestor of another such commit. Because it follows merge links, a change counts as shared however
it arrived, directly, through a third branch (A merges B after B merged C, then A merges C), or
through a fork of a branch that was already merged, so a closure that adds counter deltas applies
each delta once. The base is computed under the writer lock against the tip the closure runs on.
A merge that keeps only our side still commits, so the link is not lost, and merging a branch
whose tip we already contain writes nothing.

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

When the tips have several best common ancestors, a criss-cross, `common_ancestor` returns the one
with the highest generation, then the latest `time_ms`, then the highest entry hash. The rule
depends only on the set, so both sides and every run agree, and a stale candidate can never win,
since it is below a best one and so has a lower generation. What the other best ancestors have and
the chosen one lacks is then seen again, so a closure that is not idempotent applies it twice.
Git's recursive strategy first merges the best ancestors into a virtual base. That needs the
closure more than once, which `FnOnce` does not allow, and it was left out.

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
side packs what that lacks, and `receive` checks and applies it. `Have` and `Bundle` are byte
buffers, so the same code runs over a file, a socket or HTTP. `pull` runs the three in one process,
and `push` is `pull` called from the other side.

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

### Applying

`receive` checks everything before it writes anything:

- Every object is hashed as the bundle is parsed.
- The new commits are walked from the head through `prev`, and their versions must descend one at a
  time. A head at 2^63 or above is rejected, so a fast-forward to it cannot leave the next commit to
  wrap the version past `u64::MAX`. The commit below the oldest one sent is looked up in the
  receiver's chains by hash and version. Found, the landed branch continues into the receiver's copy
  of it and keeps the lowest `first` among the chains that hold it, so a fast-forward does not cut
  our history short to where another branch was pruned. Not found, the link is written with offset 0
  and the branch starts at the oldest sent commit, the way a pruned chain does.
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
- The root of every new commit decodes as `T`.

All the objects and a `Refs` that points `remote` at the new head are then one frame, written under
the `tip` mutex and synced like a commit, so a crash leaves the previous head: a torn frame is not
replayed. Branch handles on `remote` are rebound and its watchers fire. Their notices drop once
the pull has released `tip`, so a `watch_when` test may commit or pull. A bad bundle is
`Error::Bundle` and writes nothing. Flipping every byte of a 3,358 byte bundle with three masks gave
10,073 rejections and one flip inside an LZ4 body that decoded to the same bytes, and so landed the
same commit.

A bundle whose head `remote` already holds at that version writes nothing. Bundles are plain bytes,
so one can arrive late, from a slow transfer or a second pull running at the same time, or be sent
again. Landing it would move `remote` back, drop the records under its name, and merge older state
over newer.

### Bringing the branch up

A second step compares the pulling branch with `remote`:

- The same tip, or the same root: `UpToDate`.
- Our tip is the remote chain's entry at our version: `FastForward`. Our head takes the remote chain
  in a frame with no commit. The chains share our entries, so versions carry on from ours, and
  `head_hash` is now the same in both files.
- Their tip is our entry at their version: `Ahead`.
- Otherwise the merge-base search of `merge_with` runs under the writer lock against our tip and
  the landed `remote`. If their tip is the base, we already hold it through a merge: `Ahead`.
  Otherwise `merge` runs against that base as a commit on our branch that links their tip as its
  second parent.

Each outcome that shows their tip is an ancestor of ours records `(their version, our version)`
under `remote` in our head's `merged` map. The merge link says the same thing, but a record survives
our side of it being pruned, and the search treats it as a known ancestor of our tip, so a branch
pruned past its last sync with an idle remote still finds itself `Ahead`. Equal roots without that
ancestry record nothing, since a record there would make the remote's history look like ours. A crash between the landing and this step leaves `remote` landed and
our branch as it was. The next pull's have names the landed tip, so its bundle is empty and the step
runs again.

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

Each commit's time is `max(now, previous)`, so time never goes back along a chain and `as_of` works
after a fast-forward. A device whose clock runs ahead holds the times of our later commits at its
reading until our clock passes it.

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

### Limits

- A bundle is built and applied in memory, and the first pull into an empty file carries all of the
  retained history, so it needs that much memory on both sides.
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
- The receiver decodes each new root as its own `T`, so peers need schemas that decode each other's
  roots. A root that has to rebuild a table index as it decodes, because the sender's schema lacks
  that index, can be rejected, since the rows it would read are not written yet.

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
   every branch's tip and root from it. Rename it over the old one, sync the directory, and rebind
   every branch.
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
names a new inode, opens it, and rebinds. Its watchers fire only if the version moved. A process that
never refreshes never fails, and holds the old file open instead.

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
copied stays poisoned, because the swap checks first. The new file takes the old one's mode and owner,
and a path through a symlink stays a symlink.

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
changes.

## Errors

Collections read like native types, so `get` and iterators do not return `Result`. An I/O failure
or corruption during a read panics. `try_get` exists for callers that need to handle it. Inside
`update` a panic discards the transaction and leaves the database unchanged.
