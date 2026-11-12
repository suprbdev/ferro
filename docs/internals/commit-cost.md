# Commits of a few rows

Where the CPU time of a small multi-table commit goes, and what was done about it. The lock
structure around it is in [Writers and the commit path](commit-path.md), and where its bytes go in
[Bytes of a small commit](commit-bytes.md).

A booking write in `examples/booking` changes about ten small trees, the rows and their indexes, and
spent six to ten times as long outside its closure as in it, all under the writer lock.
`ferro-bench`'s `tables` tool runs the example's hold, confirm and cancel closures one commit at a
time against a Buffered file, after 4,000 warm-up pairs, and reads thread CPU time and user-space
instructions, through `perf_event_open`, for the commit and for its closure. A hold writes 36
objects, 7.3 KB of bodies before LZ4 and 5.1 KB after, and encodes 274 keys on 34 nodes, of which 20
to 23 only gained a new child.

## Instructions by phase

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

## Five changes

Time did not follow instructions. In a sampling profile of the same run before, leaving out time
inside libc, encoding took 30% of a commit, inserts into the node cache and `dedupe` 21%, the
closure 15%, LZ4 11%, appending records and `dedupe` lookups 7% and reading the tip back 4%: the
inserts and lookups run few instructions and wait on cache misses. Five changes, each measured on
its own:

- A node on the path to a change copies the bytes of its unchanged entries, as described under
  [Copy-on-write and flush](tree.md#copy-on-write-and-flush): 16% fewer instructions outside the
  closure. A single-key insert into a 20,000-key map encoded 58 values and now fewer than 24.
- The new tip is the root the commit decoded as a check and the commit it wrote, and the refs are
  decoded from their encoded bytes, instead of three reads, hash checks and decodes from the file:
  4% fewer instructions and 6 to 10% less CPU time.
- `dedupe` became the set-associative table described under
  [Deduplication](storage.md#deduplication): 3% fewer instructions and 7 to 12% less CPU time for a
  hold.
- `Object::hash` copies an object of up to 4 KiB and its link hashes into a per-thread buffer and
  hashes it in one call. Each small update had cost about 140 instructions of buffering, so a node
  with nine links paid more for its links than for its body: a 100-byte node with nine links took
  2,285 instructions against 3,542. The bytes hashed are the same. 6% fewer instructions.
- The parallel flush needs 256 dirty nodes, as described under [Parallel
  flush](tree.md#parallel-flush): a third less CPU time for a confirm and a quarter less for a
  cancel.

## Results

Median of five interleaved runs on a 32-core machine while other work on it was light, in thread CPU
time:

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
closure, then encoding at 35%, which is mostly the leaves that a change lands in and the per-node
work of building records and cached copies, and BLAKE3 at 17%. Of the nodes a commit puts in the
node cache, 98.3% are read again, 61% by the very next commit, so the inserts stay. The `heap`
tool's commits and reads case ends with 154.8 MiB of live heap against 170.9 before at a 256 MiB
setting, and its other cases moved by under 1%.

## The LZ4 encoder

LZ4 compression later dropped `lz4_flex`'s `safe-encode`, which had been left out for the unsafe
code it brings into the write path; the decoder stays safe, as described under
[Records](storage.md#records). Median of seven interleaved runs of `tables`, a hold, a confirm and a
cancel ran 7.7, 8.8 and 9.7% fewer instructions outside the closure, with the same 12,208 bytes per
write. CPU time there was 0 to 4% lower across three sets of runs, not clearly beyond noise.
Single-key commits ran 5 to 6% fewer instructions.

## A bulk load's commit

A commit of 1M fresh keys is a different shape: 140,000 dirty nodes, a 24 MB frame and a working
tree of about 190 MB to drop. Timed in one run on 2026-11-05, with timers around each phase of a
`Map<u64, String>` loaded by per-key inserts:

| Phase | Wall time | On |
| --- | ---: | --- |
| Encode, hash and compress subtrees, the [parallel flush](tree.md#parallel-flush)'s first phase | 17 ms | 16 threads |
| Append the records to the transaction | 24 ms | the committing thread |
| Decode the cached copies | 12 ms | 16 threads |
| Hand the copies to the transaction | 7 ms | the committing thread |
| Write and sync the frame, and publish | 5 ms | the committing thread |
| Register the records for deduplication and fill the node cache | 13 ms | the committing thread |
| Drop the working tree | 109 ms | the committing thread |

The append and the decode of the cached copies now overlap, on threads a crew keeps between
flushes, as [Threads kept between flushes](tree.md#threads-kept-between-flushes) describes, which
took the commit in `bulk` from 103 to 95 ms.

The drop is most of it, and freeing about 2.4M allocations in address order is most of the drop, as
[Dropping a large tree](tree.md#dropping-a-large-tree) describes. Smaller slots and a prefetch of
each node's flag line in the drop's walk took the drop to 96 ms in `bulk`, median of five
alternating runs, and the rest of the commit from 95 to 92 ms.

## Measured and left out

- Sizing a bulk load's `seen` table for its 140,000 records up front: the append took 21.4 ms
  against 23.6 in one run each. It needs the count before the first record, through a new method on
  `Sink`, and was not kept.
- Filling the node cache on a thread while the committing thread drops the working tree, which
  would hide its 13 ms. The cache and deduplication table are filled under the branch's lane so the
  next commit on the branch finds them, while the drop runs after the lane is released, so the two
  cannot overlap without holding the lane through the drop or handing it to the helper thread.
- Encoding, hashing and compressing outside the writer lock. A closure is `FnOnce` and has to run on
  the tip, so it cannot run before the lock is taken and again if the tip moved. What is left is to
  let the next closure run on this one's dirty tree while this one flushes, which bounds the gain by
  the closure's share of the lock: (13 + 71) / 71, at most 1.18 times the writes a second for a
  hold. It needs a flushed-link memo on every shared dirty node so the next flush does not encode it
  again, a second lock to keep frames in order, and a failed flush would fail closures that already
  ran on it, which is what `coalesce` already does for 2.6 times as many writes.
- Copying the unchanged entries of a node whose values changed too, through offsets kept per entry
  and the column they came from. 3 to 5% fewer instructions outside the closure, but a cancel's
  closure took 21% more instructions and CPU time came out 3 to 5% worse, since most leaves a commit
  touches were last written by a bulk load or read from the file and keep nothing.
- An LZ4 block compressor of our own, with a table sized to the input and reused per thread, which
  saves `lz4_flex`'s zeroed 8 KiB table and output vector on every call. 7% more instructions per
  commit, the same CPU time over five rounds, and 0.1% more bytes.
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
  change, and the whole closure is a tenth of a commit's instructions and under a fifth of its time.
