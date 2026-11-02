# ferro design

A ferro database is one typed Rust value, `T`. There is no query language: you read `T` with
ordinary Rust and change it inside a closure. Every commit produces a new immutable version
of `T`, and all earlier versions stay readable.

## Layers

- `codec.rs`: the `Data` trait, `Encoder`, `Decoder`, primitive impls.
- `store.rs`: the append-only file, commit frames, superblocks, recovery, object reads with hash
  verification, node cache.
- `map.rs`: `Map<K, V>`, the persistent ordered tree. `Set` and `List` wrap it.
- `table.rs`: `Table<T: Record>`, a keyed collection with automatic secondary indexes.
- `db.rs`: `Db<T>`, `Snapshot<T>`, commits, branches.
- `log.rs`: the commit chain and `Log`. `history.rs`: `History`, the lazy walk behind `history_of`.
- `watch.rs`: `Watcher<T>` and the per-branch feed behind it.

## Objects and links

Everything on disk is an `Object { body, links }`. A link is a `Ptr { hash, offset }`.

- The hash is BLAKE3 over body length, link count, body, and the link hashes, truncated to 20
  bytes (`HASH_BYTES`). Offsets are not hashed, so identical content has an identical hash
  wherever it sits. The file is a Merkle DAG.
- The offset lets a reader jump straight to a child, so no hash index is needed.
- Every read re-hashes the object and compares it with the pointer it came from.
- Links are stored in a side table and consumed in order by `Decoder::link`. They take no bytes
  in the body, which keeps encoded size independent of a value's position.

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

Insert and remove use treap-style `split` and `merge`. Mutation is copy-on-write through
`Arc::make_mut`, so a node is copied once per transaction and then mutated in place. `Link::Mem`
is a dirty in-memory node, and `Link::Stored` a persisted one. `flush` writes dirty nodes bottom
up during `Data::encode` and seeds the node cache with the nodes it wrote. It clones the in-memory
node unless a value held a dirty or foreign collection, in which case it decodes the written bytes
instead, so nested collections always come back clean and a foreign store is never pinned. Values
inserted or mutated since the node was loaded are decoded once as a check before they become
durable; unchanged values are not.

`Extend` and `FromIterator` sort the batch, keep the last value per key, and for an empty map
assemble the tree bottom-up in one pass, which gives the same tree as inserting key by key.

Values over 512 encoded bytes spill to their own object, so a large value does not bloat its
node or get rewritten when a neighbour changes. Lower thresholds were measured and rejected:
spilling interior-node values over 64 bytes cut table update bytes by 19% but grew bulk size by
14% and slowed table reads by 20 to 50%, once compression was already shrinking interior nodes.

## Read path

A warm get at 1M keys walks about seven levels, and nearly all of its time is CPU cache misses, not
logic. One node cache lookup per level was 55% of the get. Reference counts on nodes near the root,
written by every reader, held eight readers to about 10M reads a second.

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
This cut peak RSS by 28% and bulk load time by 4 to 14%.

`range_by` builds each edge and its rank in one walk, and iterators read their top frame in place
instead of cloning an `Arc<Node>` per item. A node flushed by this process shares the values of the
in-memory tree, and those sit in insertion order, so a scan in key order would take one miss per
value in turn. `Node::warm_values` reads every value's reference count on entering a node, so those
misses overlap instead of queueing. That cut scans by 38% with no cost to writes, where moving
values into node order during `flush` cost 22% of bulk load speed and 16% more memory for less gain.
`Keys` and index iterators skip it, since they never touch a value.

A node miss is dominated by BLAKE3, at roughly 500 to 900 ns on an interior node against 130 to
190 ns for `pread`, and the hash check is not negotiable. Reads fetch 1 KiB at a time, which fits
most records. Measured and left out:

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
- Scan admission control. `quick_cache` starts new entries in a small cold section, so a full scan
  leaves hot-set latency unchanged. Not admitting scanned nodes saved 7 to 17% on scans larger
  than the cache, but made repeated short range pages 3 to 4 times slower, and a budget per iterator
  that avoided that saved only 6 to 8%.

`cache_bytes` bounds cache weight, which tracks on-disk size, not heap. Decoded nodes take several
times that: a 16 MiB cache holds about 71 MiB, and at 1 MiB, with pins and `Arc` allocations
included, about 4.6 MiB. Size it at a quarter of the memory you mean to spend.

## File format and durability

Two 4 KiB superblock slots, then the log. The log is a run of frames, one per commit. A frame is a
68-byte header (magic, sequence, end offset, head pointer, xxh3-128 check) followed by the records
the commit wrote. The check covers the header and every record, so a frame proves by itself that
it reached the disk whole. A commit does this:

1. Write the frame at the log tail.
2. fdatasync. This is the durability point.
3. Write the superblock to the other slot, without syncing it.

The superblock is written only after its frame is durable, so it never names anything a crash can
lose, and the next fdatasync carries it to disk. A crash leaves it at most one sync behind.

Open picks the valid slot with the highest sequence and scans forward from its log end. A frame
counts only if its magic, sequence (the previous one plus 1), end offset and check are all right,
and the first that is not ends the log. A torn tail, zeroes and garbage all stop the scan, and an
acknowledged commit survives a stale or torn superblock. A writer then syncs the frames it
accepted, truncates the rest, and rewrites the superblock, in that order. The truncation is durable
before any new frame is written, so a frame stranded past a torn one can never line up with the
log later.

A frame past the superblock may not be durable while a writer is alive, so `refresh` reads only
the superblock and never scans. A read-only open scans only when a frame follows the superblock
and it can take a shared lock, which proves no writer is alive. It holds that lock for the scan
and syncs what it accepted, since a dead writer's frames may exist only in the page cache. If a
writer holds the lock, the superblock is current and the reader uses it. A writer opening during
that short scan retries for a few milliseconds before reporting `Error::Locked`.

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
`refresh` picks up new commits.

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

Nothing is published or acknowledged before it is durable, so readers, watchers and other
processes never see a commit that could be lost, and they see commits in order. A closure
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

A `Snapshot<T>` keeps its version alive for as long as it is held, and so does any `Map` or `Table`
cloned from it. All of them also keep the writer lock held after the `Db` is dropped, so a new
writer gets `Error::Locked` until they are gone. A closure works on a clone of the tip, so a panic
or an `Err` discards its changes.

## History

Each branch head holds a commit log stored as a chain of entries, newest first. An entry holds
the `Commit`, a link to the previous entry, and for even versions a second link to the entry at
`version - lowest_set_bit(version)`. Appending a commit writes one small object and finds its
skip target by following skip links from the tip, which is O(1) amortized. Looking up a version
or a time follows skip links while they do not overshoot, which is O(log n). The head records the
tip pointer, the first retained version, and the head version. `Refs` is `Map<String, Head>` and
the superblock points at it. Forking a branch copies one `Head`, so the chain is shared and
nothing is rewritten. `restore` commits an old value as a new version, so history is never
rewritten.

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

A value's item is ready only when the walk reaches the start of that value's run. A row's creation
is followed by a run of `None` back to version 1 that reports nothing, so `Iterator::take_while`
would read the rest of the log to learn that no older item exists. `History::take_while` is an
inherent method for that reason. It tests the predicate against each older value as the walk
enters its run, paired with the commit last reported, and stops there. Its items match the
`Iterator` adaptor for a predicate on the value, on the commit, or on the conjunction of the two.
The predicate is `Fn` because it can run twice per item. The run at the tip has no commit to pair
with, so a row that is absent at the tip costs the age of that absence; check with `get` first.

A step loads one `Entry`, loads and decodes the root `T`, and runs the projection. Measured on a
50,000-commit shop in a fresh process: 0.8 µs for the entry, 1.3 µs for the root, then 1.3 µs for
a row in a quiet part of the tree and 11.5 µs for a recent row, whose nodes every commit rewrote,
so each step reads and decodes those nodes in full, values included. With the same commits cached
the whole step is 0.7 to 2 µs. Going through `at(v)` instead costs about 3 µs more per step, for
the skip-link descent from the tip. Entries hold the `Commit` behind an `Arc`, so walking the log
and `db.read()` do not copy the message.

## Merging

`Db::common_ancestor` finds the newest commit two branches share. Forking shares the log, so the
logs agree up to the fork and differ after it, which makes this a binary search that compares
commits at a version. Each probe is a version lookup, so it costs O(log^2 n). `Map::merge` and
`Table::merge` then do a three-way merge driven by `diff(base, theirs)`, costing O(changes). A
key changed only by theirs is applied. A key changed the same way on both sides is left alone.
A key changed differently on both sides goes to a `resolve` closure. Merging runs inside an
ordinary commit, so a root with several collections merges each one in plain Rust.

`Db::merge_with` wraps this and records in the head the other branch's merged version and the
version the merge became. `common_ancestor` prefers a recorded version over the fork point. When
each branch has recorded a merge of the other, it takes the one that contains the other. So
repeated merges, and merges back and forth between two branches, do not apply a change twice. A
merge that keeps only our side still commits, so the record is not lost.

Only direct merges are recorded. A change that reaches a branch through a third branch (A merges
B after B merged C, then A merges C), or through a fork of a branch that was already merged, is
not recognised. A merge closure that is not idempotent, such as one that adds a counter delta,
applies it again, and the built-in merges may report a conflict for it. If compaction pruned the
recorded version, `common_ancestor` falls back to the fork point, or returns `NoCommonAncestor`
if that was pruned too.

## Dropping a branch

`Db::drop_branch` writes one frame: a `Refs` without the branch, and with its name removed from the
`merged` map of every other head, including records a fork inherited. Records are keyed by name,
so one left behind would match any later branch of that name. Its version would then be read
against the new branch's log, which gives a base older than the real fork point, and merges would
report conflicts and re-apply old changes. The branch's own head and records go with it. Commits
read `merged` from the head in `tip.refs` rather than from a copy in the branch state, so nothing
in memory can write a removed record back.

A commit writes its head by name, so a handle on a dropped branch would quietly recreate it. Each
`Branch` therefore has a `retired` flag, which commits, forks and merges from that branch check
under the `tip` mutex, returning `Error::NoBranch`. `drop_branch` holds the mutex through its sync
and until the flag is set, so no commit fits between the frame and the flag. That stalls writers for
one fsync, on an operation that is rare. Retiring also closes the feed, which ends the branch's
watchers. Reads through an old handle still work, because state is a value and the objects are
there until compaction. A later fork of the same name is a new `Branch`, so old handles stay dead.

Counting handles and refusing the drop while others exist was rejected: a clone parked on another
thread would block it forever, and handles in other processes cannot be counted anyway. A read-only
handle learns of the drop on `refresh`, and retires its `Branch` the same way instead of falling
back to the genesis value.

## Compaction

Compaction is offline and needs no user types. It prunes each branch's log by rebuilding the
chain from the retained commits, since dropping the oldest entries changes every later entry's
link, then copies every object still reachable into a new file. The copy is iterative, because a
chain is as deep as the number of commits. Hashes do not cover offsets, so objects are re-emitted
with new link offsets and compressed again; every object is re-verified on the way through. The
new file replaces the old with an atomic rename. A read-only handle holding the old inode keeps
reading its old snapshot, and `refresh` reports `Error::Replaced`.

## Schema evolution

The derive macro writes structs as `(tag, length, payload)` fields, where the tag is a 16-bit
FNV-1a hash of the field name. Fields are sorted by tag, so reordering a struct does not change
its hash. Unknown fields are skipped. Missing fields use `Data::missing`, or `#[ferro(default)]`.
`#[ferro(rename = "old")]` keeps the old tag.

## Errors

Collections read like native types, so `get` and iterators do not return `Result`. An I/O failure
or corruption during a read panics. `try_get` exists for callers that need to handle it. Inside
`update` a panic discards the transaction and leaves the database unchanged.
