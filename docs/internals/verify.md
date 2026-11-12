# Verifying a file

`Db::verify`, `Db::verify_typed` and `admin::verify` re-hash every object that a retained version of
a branch reaches, as [Durability and crash safety](../guide/durability.md) describes. This page is
how that walk runs, what it costs, and what was measured on the way.

## The walk

The log entries come first: each branch's retained chain, the superseded heads that steps of the
branch table still link, and the commits that merges brought in, walked as a reclamation cycle walks
them, one entry at a time. That part is 2,100 entries on the file below and takes about 2 ms. The
trees those entries hold, and the branch table, are the rest: every object they reach is read once,
counted once by its link, its hash and its offset together, and checked against that hash.

A verify walks the trees twice only on a damaged file. The first walk is fast and does not keep the
order in which it checks objects, so its error, when there is one, need not be the first one in that
order. On any error the trees are walked again one object at a time in the order the walk has always
used, reading each record with the same reader as a `get`, and that error is returned. A file that
passes the fast walk passes the ordered one, since both check the same objects, so the error a
damaged file gives does not depend on the threads or on how the hashes were batched, and the tests
in `ferro/tests` that damage a file and compare errors, `reclaim_damage`, `crash`, `fuzzed` and
`verify_threads` among them, see the errors they saw before.

### Reading records

The fast walk reads a record through `Store::check_record`, which takes the bytes from the window of
the file the thread read last when it holds the record whole, and otherwise reads 16 KiB before the
record and 4 KiB from it. A commit writes a node's children before the node, so the next records
the walk wants sit mostly behind the one it has. A record that does not fit those bytes is read in
full as any read reads it. The body is decompressed into the bytes its hash is taken over, and
nothing is allocated for an object.

| Bytes read before and from a record | Reads a verify | ms |
| --- | ---: | ---: |
| 1 KiB from it, the reader of a `get` | 159,970 | 137 to 170 |
| 1 KiB and 1 KiB | 59,191 | 127 |
| 4 KiB and 4 KiB | 24,757 | 103 |
| 16 KiB and 4 KiB, kept | 17,395 | 104 |
| 16 KiB and 16 KiB | 16,649 | 108 |
| 64 KiB and 16 KiB | 14,768 | 125 |

The window a `get` reads widens only once a later record falls inside it, and a walk down a tree
never reads forward, so it read 1 KiB for every record. A read no longer zeroes the bytes it is
about to read into, which took 4.5% of the time on eight threads.

### Hashes checked many at once

BLAKE3 compresses one 64-byte block after the one before, and each waits on the last, which took
87 to 107 ns a block here, and once the rest was cut it took 64% of the user time of a verify on
one thread. Most objects fit one chunk: on the file below 11,463 objects hash 16 to 63 bytes,
59,578 hash 64 to 255, 74,454 hash 256 to 1,023, and 5,637 hash 1 to 4 KiB.

`HashBatch` in `codec.rs` queues each object of up to one BLAKE3 chunk with the others whose hashed
bytes take as many blocks, and once a group holds as many as the CPU compresses at once, 16 with
AVX-512, it compresses every block but the last of all of them together through
`Platform::hash_many`, as BLAKE3 does for the chunks of one long input, then the last block of each
with the root flag through `Platform::compress_in_place`. Each gives the hash `hash_parts` gives,
and a unit test checks that at every length up to past a chunk, on every SIMD level the machine
has. A larger object is hashed at once.

`blake3::platform` is public but hidden from the crate's documentation, kept for its benchmarks, so
`ferro` depends on `blake3 = "=1.8.7"` exactly and a newer version has to pass that test before it
is taken. No `unsafe` code was needed: the platform calls check the CPU's features themselves.

A batched check means the walk follows an object's links before its hash is checked. On a damaged
file it can read where a damaged link points. Before it reads a record past the frames written so
far, which could wait for a frame being written, or one longer than the bytes it read, which could
claim most of the file, it checks every hash it has queued, so it never goes further than a record
that checked; and a thread checks what it has queued before it hands objects to another thread.

On one thread, in three runs alternating with the build without batches, the user-space cycles of
ten verifies fell from 5.97 to 4.03 billion, and the instructions from 4,719 an object to 3,860 in
that first version of the batch; the batch as kept runs 3,320.

### Threads

The trees are walked on as many threads as `Options::copy_threads` gives, by default one per core
up to 8 once the log holds 1 MiB, and one below that. The threads share a stack of objects to walk,
each walks depth first from the 32 or fewer it takes, and one hands the older half of its own to the
shared stack whenever that is empty and another waits. The objects reached are kept in 1,024 sets
by two bytes of their hashes, each behind a lock on lines of its own, and each thread first looks in
the last 4,096 objects it reached, which catches most of the links that newer versions share with
older ones without taking a lock.

| Eight threads | ms | User-space cycles, ten verifies |
| --- | ---: | ---: |
| 64 sets, packed together | 29.5 | 5.44 billion |
| 1,024 sets, a line each | 23.9 | 4.91 billion |
| and the last 4,096 objects | 18.5 | 3.57 billion |

Those were measured at a load average of 19 to 25 on 32 threads, where one thread took 58 ms, so
eight gave 3.1 times the speed; with the machine at 40 to 55 they gave 36 against 112 ms.

## Measurements

On the file `make compare-versioned` builds, 100,000 keys in 100 commits of 1,000, then 1,000
single-key commits with `Durability::Fsync` and 1,000 with `Durability::Buffered`, 151,133 objects in
47.9 MB, from `ferro-bench`'s `verify` tool, five runs of ten verifies alternating with the build
before these changes, on 2026-11-12 with the 1-minute load average at 37 to 56 on 32 threads:

| | Before | One thread | Default, 8 threads |
| --- | ---: | ---: | ---: |
| ms, median | 279 | 112 | 36 |
| Instructions an object | 8,642 | 3,266 | 3,522 |
| User-space cycles, ten verifies | 8.3 billion | 3.1 billion | 5.7 billion |

Before, 26% of the user time went to the set of objects reached, a `HashSet<Ptr>` with SipHash,
most of it to cache misses in its compares, 6% to zeroing a new 1 KiB buffer for every record, and
a fifth of the wall time to 160,000 reads in the kernel. In the comparison harness's ferro child,
alternating four times at a load of 37 to 42, a verify took 28 to 60 ms against 204 to 368 before.

`verify_typed` decodes everything as well, and on this file took 420 to 470 ms and 25,000
instructions an object, nearly all of it in the typed checks, which this did not change.

## Commits after a verify

The comparison saw the first few `Durability::Fsync` commits after a `Db::verify` on an open file
take 3 to 110 ms before falling back to 0.4 ms. `ferro-bench`'s `verify stall` reproduces it: on one
thread before these changes the two commits after a verify took 2.9 and 9.5 ms in every round. A
`thread::sleep` of 300 ms in place of the verify gives the same 2.9 and 9.5 ms, and so does a spin
of 300 ms, while instructions, system time, page faults and context switches of those commits were
those of any other. A Python loop that writes 3,000 bytes and calls `fdatasync` sees one sync of 12
ms after 300 ms of nothing, 15 ms after a second, and about 1 ms after 100 ms; reading 4 KiB with
`O_DIRECT` every 20 ms through the pause takes it back to 0.6 ms, every 200 ms does not.

The disk causes it, not `ferro`: this machine's NVMe SSD, a Crucial P310, drops into a
power-saving state once it has been idle for between 30 and 100 ms, as Linux allows with
`nvme_core.default_ps_max_latency_us` at 100,000, and the next write waits for it to come out. A
verify of a file in the page cache does no I/O, so a verify of 200 ms left the disk idle for long
enough. After these changes a verify of this file takes 20 to 60 ms: verifies of 36 to 57 ms were
followed by commits of 0.6 to 1.6 ms and then 0.4, and one of 64 ms by a commit of 11.8 ms, while
`--pause 30` gives 0.6 to 0.7 ms, `--pause 100` one commit of 7 to 8 ms and `--pause 150` 2.9 and
9.5 ms, as before.

The harness's first commit after a reopen measures the same thing: the reads and the verify before
it leave the disk idle for a second or more. In its ferro child that commit took 12.3 to 14.8 ms in
8 of 11 runs and 1.4 to 1.6 ms in the other three, likely while other processes used the disk, and
0.9 to 1.3 ms in three runs with a 4 KiB `O_DIRECT` read every 20 ms from another process. A lower
`nvme_core.default_ps_max_latency_us`, which keeps the drive out of its deepest states at the cost of
the power they save, should remove it; that was not tried here.

## Left out

- A sorted pass over the file, reading each record once in order, would need the links of every
  object before it knows which are reachable.
- A set without locks, by offset with the hash beside it, would save most of the lock traffic left
  on eight threads, about a fifth of their cycles, at the cost of a fixed table sized up front.
- Objects of more than one chunk, 4% of this file's. A reclamation cycle's copy checks its hashes
  in batches that take objects of up to 16 chunks, as [Checking many hashes at
  once](reclamation.md#checking-many-hashes-at-once) describes, and `verify` could share them.
