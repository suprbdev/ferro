# Alternatives measured and rejected

Each internals file records what was tried and left out next to the design it lost to, with the
numbers. This index lists them by subsystem.

## Storage

- 256-bit hashes, kept at 160 bits: [Objects and links](storage.md#objects-and-links).
- Registering every read in `dedupe`, finding unread objects by hash, a `quick_cache` for `dedupe`,
  and promoting only re-registered hits: [Deduplication](storage.md#deduplication).
- Syncing on every Buffered commit, and writing the superblock only at checkpoints: [Buffered
  durability and checkpoints](storage.md#buffered-durability-and-checkpoints).
- `posix_fallocate` for the log's reservation: [Preallocating the
  log](storage.md#preallocating-the-log).
- Frames written with `O_DIRECT|O_DSYNC`: [Direct I/O, rejected](commit-path.md#direct-io-rejected).

## The tree

- Fanout 4 and 16, and BLAKE3 for key levels: [Shape and fanout](tree.md#shape-and-fanout).
- Prefetch variants, splitting in place, longer linear search, unsafe uniqueness checks, other free
  orders, arenas, buffered inserts, dropping on a background thread, freeing in parallel, rayon, and
  turning LZ4 off: [Measured and rejected](tree.md#measured-and-rejected).
- Rebuilding node vectors for a batch: [Batches](tree.md#batches-extend-fromiterator-and-apply).
- Lower spill thresholds: [Spilled values](tree.md#spilled-values).
- `Bytes` sharing the object buffer: [Byte buffers and newtypes](tree.md#byte-buffers-and-newtypes).

## Summaries

- A summary stored in the node, and updating summaries by delta: [How a summary is
  kept](summaries.md#how-a-summary-is-kept) and [Where the summary is
  stored](summaries.md#where-the-summary-is-stored).
- Hand-kept rollups and running balances: [Against hand-kept
  rollups](summaries.md#against-hand-kept-rollups).
- A `#[sum]` attribute, summaries on `Table` rows, and cached summaries on dirty nodes: [Measured
  and left out](summaries.md#measured-and-left-out).

## Reads

- A replaceable `ArcSwap` memo: [Memos](read-path.md#memos).
- Prefetching after the header, fewer lines, `_MM_HINT_T1`, full addresses and pinning every level:
  [Prefetching the next level](read-path.md#prefetching-the-next-level).
- A `NodeRef` handle: [Values](read-path.md#values).
- A recursive batch get, and `floor` on a cursor: [Sorted lookups and
  cursors](read-path.md#sorted-lookups-and-cursors).
- A wider varint decoder: [What is left, on
  `examples/graph`](read-path.md#what-is-left-on-examplesgraph).
- Bulk decode hooks, lazy value decoding, `mmap`, cheaper cache keys, a chunked or trie `List`, and
  scan admission control: [Measured and left out](read-path.md#measured-and-left-out).
- Pinning every level, index-based iterator frames, more prefetching, a plain store on pin, moved
  flags, a per-thread read buffer, more cache shards and aligned shared state: [Measured and left
  out](read-scaling.md#measured-and-left-out).
- Retiring every evicted node, and an owner beside each cache entry: [Guarded gets and the epoch
  scheme](read-scaling.md#guarded-gets-and-the-epoch-scheme).
- Deferring every memoized eviction, the flags in an allocation of their own, moving nodes out with
  `Arc::try_unwrap`, and a `!Unpin` node: [Where the flags
  live](read-scaling.md#where-the-flags-live).
- A frame flag beside the pointer, and an inlined step: [Iterator
  frames](read-scaling.md#iterator-frames).
- A private lease for `Val`, a lazier row finder, building the column in place, and decoding from
  the read buffer: [Measured and left out](read-scaling.md#measured-and-left-out-1).
- Choosing an allocator in the library: [The allocator](read-scaling.md#the-allocator).
- Weighing entries by disk size, and a size hook on `Data`: [Cache
  weight](read-scaling.md#cache-weight).

## Writes

- A dedicated sync thread for blocking writers: [Lanes, `tip` and group
  commit](commit-path.md#lanes-tip-and-group-commit).
- Relocating a finished frame, encoding at the current end and retrying, and a region per writer: [A
  lock per branch](commit-path.md#a-lock-per-branch).
- Leaving every group open until its sync ends: [A lock per
  branch](commit-path.md#a-lock-per-branch).
- Keeping moved-node records in a vector of their own: [Drafting outside
  `tip`](commit-path.md#drafting-outside-tip).
- Encoding outside the writer lock, copying unchanged entries of changed nodes, an LZ4 compressor of
  our own, no LZ4 for small bodies, reused buffers, SIMD hashing and cheaper index maintenance:
  [Measured and left out](commit-cost.md#measured-and-left-out).
- A delta log between checkpoints, built and measured: [A delta log between
  checkpoints](delta-log.md).
- One version per closure in a shared frame, sharing intermediate nodes, a combiner thread,
  spinning, and other rules for closing a group: [Measured and left
  out](coalescing.md#measured-and-left-out).
- A condvar for group members: [Waking members](coalescing.md#waking-members).
- The lesser of two handovers, and running means of handovers: [Waiting for a thread on its
  way](coalescing.md#waiting-for-a-thread-on-its-way).
- A committer thread, running closures on the group leader, a plain `lock()` in the poll, and
  leading a Buffered batch from the poll: [Awaited writes](async-writes.md).
- Waking every watcher to test on its own thread, and a wake with no close:
  [Measurements](watchers.md#measurements) and [Closing a feed or a
  watcher](watchers.md#closing-a-feed-or-a-watcher).

## History, merges and branches

- Skip links for `bisect`, and a double-ended `History`: [`bisect` and
  `bisect_changes`](history.md#bisect-and-bisect_changes) and [`history_of` and
  ranges](history.md#history_of-and-ranges).
- Stamping every commit: [Commit times](history.md#commit-times).
- Counting handles before dropping a branch: [Dropping a branch](history.md#dropping-a-branch).
- Handing the closure every best ancestor, a merge trait, and an opt-in recursive merge:
  [Criss-crosses](merging.md#criss-crosses).

## Sync

- Comparing every new root with the anchors at once: [What is sent](sync.md#what-is-sent).
- Finding the merge base by content: [The merge base](sync.md#the-merge-base).
- Undoing the landing after an `Err`: [Bringing the branch up](sync.md#bringing-the-branch-up).
- Writing staged objects as frames while the bundle streams, copying in the landing frame, and
  holding checked objects in memory: [Where checked objects
  wait](sync-receive.md#where-checked-objects-wait).
- Landing onto the existing record, keying the cache by hash and offset, and resolving links by
  hash: [Commits stored twice](sync-receive.md#commits-stored-twice).

## Reclamation and Windows

- Hole punching, copying sparse runs to the tail, and a segmented log:
  [Measurements](reclamation.md#measurements).
- `File::lock`, `ReOpenFile`, generation-numbered files, and deleting before renaming:
  [Windows](windows.md).

## Errors and tables

- Decode errors as `Error`, and checking separators on local reads: [Errors and checks on local
  reads](errors.md).
- Last writer wins or an overwritten index on a unique clash: [Unique
  indexes](schema-and-unique-indexes.md#unique-indexes).
- Trusting indexes written before fingerprints, a wider fingerprint, storing the description, and a
  tag per index: [Index fingerprints](schema-and-unique-indexes.md#index-fingerprints).
