# A delta log between checkpoints

A design that was built as a prototype, measured and left out. It is kept here so that the numbers
are not measured again.

Most of a small commit's bytes are tree nodes. Writing a short record of what each commit changed,
and writing nodes only at a checkpoint every K commits, was designed against everything that relies
on a version being a root hash, bounded by measurement, built behind an option, and left out.

## The design

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

## The bound

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

## The prototype

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

## What each feature would need

The suite also showed what each feature would need before the option could ship. Streamed bundles
read raw records at delta offsets and fail. A reclamation cycle with `Keep::All` writes a larger
file than it reads. `remove_range` reads base objects in the subtrees it cuts out, and history walks
hold more than `cache_bytes`. Each is work in sync, reclamation or the read path, for a design that
loses time on every commit.

## Why it was left out

It was left out. The one gain is bytes on disk, and reclamation already bounds the file and
`coalesce` already writes 85 to 95% fewer bytes where writers contend, while gaining throughput. A
delta log costs `booking-stress` 19 to 25% of its writes a second and `ferro-bench` about half of
its Buffered commits, gives an fsync commit nothing, makes open and the first read of an old version
replay up to K records, and puts every follower to the writer's work again. The prototype is not
merged. The `delta` tool stays in `ferro-bench` for the bound.
