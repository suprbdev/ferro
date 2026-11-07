# Reclamation

How a reclamation cycle copies what is retained into a new file while writers run. Retention itself
is in [Retention and reclamation](../guide/retention.md).

## A cycle

Retention says what to keep, and reclamation gives the rest back while writers run. A cycle copies
what is retained into a new file and renames it over the old one. Each file is a generation.
`Db::compact` runs one cycle on the caller's thread. `Options::history`, with anything but
`Keep::All`, runs them on a background thread once the file has grown by `Reclaim::every_bytes`, set
through `Options::reclaim`, or by the size it had after the last cycle, whichever is larger, so the
copying stays proportional to what was written.

1. Prune. One frame raises `first` on every branch. Branch state changes before anything else, so
   `at` stops serving a version before its objects are left behind.
2. Copy, with no lock. Walk the published refs and write each reachable object to
   `<file>.compacting`, re-emitting links with new offsets behind each body as it is stored. The
   walk copies log entries itself and hands each tree it reaches to up to seven other threads,
   which check every object's hash and write the tree where one thread would have, as described
   under [The copy on several threads](#the-copy-on-several-threads). Chain entries are copied like anything else, but a link
   to an entry that is past `first` and was not brought in by a retained merge, as described under
   [Merges and pruning](merging.md#merges-and-pruning), is written with offset 0 instead of being
   followed, and so is a skip link below where the entry's `prev` links reach. Copies are keyed by
   hash, and entries by the offset they were read from, as described under [Commits stored
   twice](sync-receive.md#commits-stored-twice), so a later pass from newer refs visits only what is
   new. An entry's copy also records how much history it was made for, and a walk that needs more
   copies the record again, as described under [Copies made for less
   history](#copies-made-for-less-history). Data is immutable, so no write barrier is needed. Passes
   repeat, up to six, until one copies less than 256 KiB.
3. Sync, with no lock, so the pause does not pay for it.
4. Swap, under every branch's lane and the `tip` mutex. This is the only pause. Wait for queued
   commits, copy what is left, write the branch table whole and both superblock slots, and sync, so
   the new file starts with no [steps](commit-path.md#branch-table-steps) on its table. Open the new
   file, which takes its writer lock, and read every branch's tip and root from it. Rename it over
   the old one, sync the directory (on Windows, flush the renamed file), and rebind every branch.
5. Retire, with no lock. Clear the old generation's cache. Its file goes when the last handle does.

The new file continues the frame sequence, so group commit tickets stay in order. It has no frames
yet, only records and a superblock naming them, the same shape offline compaction produces.

An entry's hash covers its version, time, message, root and links, so a branch can make a commit
identical to one pruned from another branch, in the same millisecond or when the clock has stepped
back. Pruned hashes are therefore kept apart from copied ones and drop out of the set when a chain
retains that hash again, and an entry's `prev`, skip or merge link already at offset 0 is never
followed. Only those links are ever written as offset 0. Anywhere else, including an entry's root,
a link at offset 0 names nothing in the file, so the copy fails on it with the error `verify` gives,
even where an object with its hash was copied, and a link to a pruned entry fails it with "link to
the pruned log entry at <offset>", rather than writing a file whose trees cannot be read. Which
records are entries at all is decided by how the copy reached them, as described under [Records
reached as entries](#records-reached-as-entries).

## Copies made for less history

A walk stops at an entry an earlier pass copied, and the copy then stands for everything below it.
That copy was cut for the heads of its own pass, and a head that lands between passes can need more
of the same records (#55). A remote kept from v2 when a pass copied it was dropped and landed again
kept from v1. The landing found v2 and v1 by hash in `dedupe` and linked them as they were, since
the old file holds them whole, so the new head ran through the record the pass had copied with its
`prev` link cut. The pause took that copy, and the cycle's file failed `verify` with `commit log
keeps versions from 1 but v2 links pruned history`. Seed 73 of the pruned combined model found it
at a scale of 30, in most runs but not all, since the model reads the system clock.

What a walk needs below a record comes down to two numbers. A head walks down to its `first` and
bounds the merges it finds by the time of its first entry, and a merge's closure walks down to its
floor while commits are no older than its bound, as described under [Merges and
pruning](merging.md#merges-and-pruning). Times never go back along a chain, so a walk with a lower
floor and an earlier bound visits everything one with a higher floor and a later bound does, seeds
each merge with a bound no later, and keeps every skip link the other keeps. A copy made for one
need therefore serves every need it covers. Each entry's copy now records the needs of the walks
that reached its record in its pass, which is one need unless walks that neither covers met there.
A walk stops at a copied record only if one of those covers its own. Otherwise it goes on through
the record, which is copied again with whatever `prev`, skip and merge links the walk needs below
it, and later links name the new copy. The needs the old copy served are walked again from the
record, so the new copy serves them too, and a walk that stopped there earlier in the same pass is
still served. Copies already written that name the old copy keep it, and the heads that reach it
through them need no more than it kept. Merge closures in the copier are keyed by record rather
than hash, so another copy of a commit is walked and not skipped because one with its hash was.

A record copied again has two copies in the new file, and a skip link has to name the one on its
own `prev` path. A new entry whose walk stopped at an older copy above its skip target links that
copy as `prev`, and the older copy's chain leads to the first copy of the target, while the map
from offset names the second. `verify` reads a retained skip link nothing walked to as an ordinary
object, so once the head that needed the second copy was dropped, it followed that copy's cut
`prev` link and failed with `pointer 0 outside log`. The pruned model found it at seed 8 step 48,
once in ten runs at a scale of 30 and every time with its clock replaced by a counter. The copier
now keeps the `prev` and skip offsets of each entry it copies, with the offset it read the entry
from, and finds a skip target by walking down them from the new entry's `prev` link, a version
lookup's O(log n) steps in memory. It links the copy it reaches when that copy was read from the
record the skip link names, and the copy the map names otherwise, which is what it did before.

Four tests in `ferro/tests/sync.rs` land remotes again from a hook at `created`, `copied` and
`synced` with a fixed clock.
`a_remote_landed_again_with_more_history_than_a_pass_copied_keeps_its_links` needs a skip link and a
`prev` link a pass cut, and `a_remote_landed_again_past_a_merge_a_pass_cut_by_time_keeps_its_links`
needs records a pass copied only for a merge's closure; both failed `verify` before the change.
`a_remote_landed_again_through_a_merge_keeps_the_merged_history_a_pass_cut_by_time` needs merged
history further down than a pass kept, which `verify` does not follow, so it pulls a device that
forked from the first commit of that history; with closures stopping at any copy the pull did not
merge. `a_skip_link_copied_after_its_target_was_copied_again_names_the_copy_on_its_chain` failed
with `pointer 0 outside log` when skip links took the copy the map names.

What it costs is memory and some bookkeeping in each pass. An entry the copier writes now carries a
need, 16 bytes keyed by the offset it was read from, and three offsets keyed by its own where one
used to say how far its `prev` links reach, about 40 bytes more. `ferro-bench --bin reclaim live
table` gained `RECLAIM_BRANCHES`, which spreads the writers' updates over that many forks. With
20,000 rows, `Keep::Last(10)` and a cycle every 4 MiB for 20 seconds, on the test NVMe with fsync,
the median of three alternating runs' median pauses went from 4.96 to 5.06 ms at 64 branches and
from 9.12 to 8.92 ms at 256. On tmpfs the writers commit three times as fast and a run's median
pause varied by up to 1.8 times between runs: from 7.2 to 5.9 ms over four alternating runs each at
64 branches, and from 12.2 to 12.9 ms over ten at 256. A run at 256 branches that counted them
copied no entry twice, so only the bookkeeping differs.

## Records reached as entries

The copy treats a log entry unlike any other object. It checks the record on the walking thread,
decodes it to find its `prev` and skip links, and may write those and its merge link with offset 0.
It used to tell an entry by its hash, so a record with an entry's hash was copied as one wherever it
was linked from (#76). But another record can hold the same bytes: a commit can be stored twice, as
described under [Commits stored twice](sync-receive.md#commits-stored-twice), and a damaged tree or
branch-table node can link a second record of an entry that no chain reaches. `verify` reads such a
record as a tree and follows every link it has, while the copy cut its links and wrote it where
`verify` would read it as a tree. The reclaim fuzz target found this through a branch-table node
whose keys were out of order, which linked a second record of an entry no head named. A test builds
the shape directly: a commit at v3 whose root links a second record of v2. With `Keep::Last(2)` the
cycle wrote that record with its `prev` link cut to offset 0, and `verify` refused the new file with
`pointer 0 outside log`.

A record is now an entry only when the copy reaches it as one: a record that the walk down a
branch's chain or a merge's closure read, in this pass or an earlier one, or the record that the
`prev`, skip or merge link of an entry being copied names. Reached any other way, it is an object,
copied with every link in full as `verify` reads it, so the copy fails where one of those links
names pruned history or offset 0. A tree that links the very record a chain read gets that record's
copy, which keeps the branch table's links to each branch's tip one record each. On the test's file
a cycle that keeps everything now writes the second record beside the entry's own copy, as a tree,
into a file that verifies, the same at 1 and 4 threads. `Keep::Last(2)` fails with "link to the
pruned log entry at <offset>", as for a tree that links pruned history itself, and with the second
record's `prev` already at offset 0 every cycle fails with the error `verify` gives on the source,
where one used to write a file that `verify` refused the same way.

The map of copies marks each hash a chain retains before the copy starts, so a link from a tree
takes the one lookup by hash it took before, and only a link whose hash is an entry's also looks up
the records the chains read, kept by offset with their hashes in 32 bytes a slot, where the set of
entry hashes it replaced took 20. From a file whose trees link no entry, which is every file ferro
writes, the copy makes the same choices as before: `cmp` found the copies of the two files under
[Limits](#limits) identical to those the build before made, keeping everything, the last 100
versions and the last one. On 2026-11-05, on a machine other work kept at load averages of 78 to 136
on 32 cores, 23 rounds of `ferro-bench --bin reclaim copy` alternating the two builds took medians
of 2.19 s against 2.09 before at 1 thread and 1.61 against 1.60 s at 4 on the table rows, and 0.94
against 1.16 and 0.89 against 0.86 s on the 320 byte values, with runs spread over a factor of
three. The fastest runs took 1.38 against 1.37 s, 0.73 against 0.73, 0.65 against 0.66 and 0.27
against 0.26. The calling thread's user-space instructions, which load does not move, fell from 6.04
to 6.00 billion at 1 thread and from 2.11 to 2.07 at 4 on the table rows, and by 0.4% and 2.1% on
the values, since a record read no longer looks its hash up in a set of entry hashes.

Three tests in `ferro/tests/fuzzed.rs` build the shape. A tree that links a second record of v2
under v3 must compact with `Keep::All` into the same bytes at 1 and 4 threads and a file that
verifies, and fail with `Keep::Last(2)` naming v1's offset. With that record's `prev` at offset 0,
both must fail with the error `verify` then gives. Both failed before the change, which wrote files
that `verify` refused. A tree that links v2's own record must compact into as many objects as the
source holds less the versions pruned, so that the record is written once.

## The copy on several threads

Until #63 the copy did everything for an object on the thread that walked to it: it read the
record, decompressed the body, checked the hash, encoded the links with their new offsets,
compressed the body again and appended the record. Timers around each step of `ferro compact`,
keeping everything, of the two files under [Limits](#limits), on 2026-11-04 on a machine other work
was sharing:

| Step | 189 MB of table rows | 331 MB of 320 byte values |
| --- | ---: | ---: |
| Walking the chains before the copy | 36 ms | 0 ms |
| Reading records, in 467,728 and 218,384 `pread`s | 113 ms | 48 ms |
| Decompressing, 241 MB and none | 63 ms | 0 ms |
| Checking hashes, over 241 and 327 MB | 318 ms | 278 ms |
| Compressing again | 179 ms | 46 ms |
| Relinking and the rest of the walk | 279 ms | 56 ms |
| Writing | 18 ms | 32 ms |
| Syncing the new file | 40 ms | 61 ms |
| All of it | 1.07 s | 0.53 s |

None of the 320 byte values compresses, so their 46 ms went on attempts that failed. Four changes
take most of the rest away.

- A body is copied as it is stored. A record keeps its links after the body and compresses only the
  body, and the hash covers the body before compression, so a new offset in a link never changes
  the body's bytes. The copy writes the stored bytes, compressed or not, behind the new links. LZ4
  is deterministic, so a file written by this version copies to the same bytes as before: `cmp`
  found the copies of both files identical to those the old copy made. A body stored raw that would
  compress, which no writer in this version produces, stays raw. A wrong decoded length would be
  copied as stored too, since LZ4 decodes into a larger buffer without complaint and the hash
  covers only the bytes decoded, so the check, `verify` and every read compare the decoded length
  with the one the record gives, as `receive` does. With the length of a 20 MB body set to 268 MB,
  the copy kept it and touched all 268 MB on each check, 280 MB of RSS a cycle, where the copy
  before #63 wrote the true length back. Now the cycle, `verify` and a read of the value fail with
  `compressed body at 8260 has the wrong length`, and the cycle peaks at 45 MB: a buffer larger
  than the one the check keeps is allocated zeroed, so only the pages decoded into are touched.
- Hashes are checked on other threads. The walk reads a record without decompressing it, takes its
  links and appends its bytes to a batch. A batch of 256 KiB or 4,096 records goes to one of up to
  three threads, which decompress each record in it into a buffer of their own, hash it and compare
  the hash with the pointer the walk reached it by. The walking thread still writes every record,
  in the order it finishes them, so it assigns offsets as before, each parent after its children,
  and the file is the same byte for byte at any number of threads. A pass returns only once every
  batch is checked, and the pause checks what it copied before it seals, so no record reaches a
  named file unchecked. The first failure fails the pass with the error the check found, and a
  panic on a checking thread is raised again on the walking thread.
- The new file is written back as it grows. After each 1 MiB it writes, the copy asks Linux to start
  writing that range to disk with `sync_file_range`, which waits only for room in the device's
  queue and makes nothing durable. The `fdatasync` before the rename is left with the last part,
  and fell from 40 and 61 ms to about 1 ms. Other targets have no such call and sync as before.
- The walk does less for each object. It parses a record in its read buffer when the record fits
  there, writes an object whose links are all copied without holding it on its stack, reuses the
  buffers of the objects it does hold, and keys copies by the first 8 bytes of the hash, which
  BLAKE3 has already mixed, instead of SipHash.

Checking later means the walk follows the links of objects it has not checked yet. A damaged link
can only send it to a wrong offset, which it reads with the usual bounds and record checks, and it
looks for a failed batch before every read. It runs ahead of the checks by at most the batches
queued, two for each checking thread, one being checked on each and the one it fills, ten batches
with three threads. A record that claims more than 256 KiB goes in no batch: the walk waits for
every check in flight before it reads the record, and checks it on its own thread before it
follows its links, as one thread does. So no batch holds more than 512 KiB, the walk follows damage
no further than 5 MiB, and a loop in damaged links ends there too. Chain entries are checked on the
walking thread as well: the walk decodes an entry to find its `prev` and skip links, so it checks
each entry before decoding it, as it did before, which took 11 ms for 40,000 entries.

That bound used to count batches only, and a batch took a record of any size. A link damaged to
point into the middle of a value can parse as a record claiming most of the log. In a 21 MB file
whose link was pointed at bytes claiming 16 MiB, one thread read those 16 MiB and failed their hash
check at 19 MB of heap; four read them, batched them and wrote them before the check came back, at
58 MB. With the link's parent damaged too, one thread failed the parent's check at 3 MB and never
followed the link, and four still read the 16 MiB, at 58 MB. Both now peak within 5 MB of one
thread. In `ferro-bench --bin reclaim copy`, medians of five runs alternating with the build before
on 2026-11-05, the table rows took 0.82 s at 1 thread against 0.83 and 0.40 s at 4 against 0.40,
and the 320 byte values 0.42 and 0.14 s, as before. A 600 MB file of 600 KB values, whose records
are now each checked on the walking thread, took 0.19 s at 1 thread as before and 0.19 at 4 against
0.17.

A failure is reported in walk order, as on one thread. Each batch carries its place in the walk, a
failed check skips only the batches after it, and an error the walk meets itself, such as a record
that does not parse, waits for the checks of everything read before it, and gives way to the first
of those that failed. A link pointed at bytes that parse as a small record with a link of its own
failed on one thread with `hash mismatch for object at 16475`, the record it landed on, and on four
with `varint overflow in record`, from the garbage the walk followed out of that record before its
check came back. Both now report the hash mismatch.

`Options::copy_threads` sets how many threads a cycle copies with, the caller's included, and 0, the
default, takes the number of cores, at most 8. With 1 the copy checks each record as it reads it
and starts no thread. With more, it copies trees as described under [Trees on several
threads](#trees-on-several-threads), which replaced the batches above for a cycle on several
threads, and starts its threads with the first tree it hands out: always when `copy_threads` asks
for more than one, and with the default only in a file of more than 256 KiB. Otherwise the calling
thread does that work itself. The threads stop when the cycle ends.
`admin::compact` uses the same copy with the default.

`ferro-bench --bin reclaim copy` copies a file aside and times `Db::compact` keeping everything on
it, one thread count after another, against the same tool built on the tree before. On 2026-11-04,
medians of five rounds for the table rows, of fifteen in three sets for the values and of three for
the large files:

| File | Before | 1 thread | 2 | 4 | 8 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 189 MB of table rows, 383,364 objects | 1.05 s | 0.80 s | 0.48 s | 0.39 s | 0.38 s |
| 331 MB of 320 byte values, 124,939 objects | 0.56 s | 0.44 s | 0.33 s | 0.16 s | 0.15 s |
| 1.14 GB of table rows, 2,130,227 objects | 6.81 s | 5.29 s | 3.17 s | 2.55 s | 2.58 s |
| 1.32 GB of 320 byte values, 500,168 objects | 2.42 s | 1.67 s | 1.25 s | 0.91 s | 0.58 s |

That is 180 against 489 MB/s on the first file at 4 threads, 596 against 2,067 MB/s on the
second, 167 against 446 on the third and 548 against 1,457 on the fourth. The 1-minute load average
was 1.1 for the table rows and 1 to 11 for the others, from other work, and runs spread as other
work used the disk: the values from 0.13 to 1.05 s at 4 threads and the last file from 0.56 to
1.30 s, whose fastest runs took 2.13 s before and 0.56 s at 4 threads. The large files are a
million rows with 200,000 updates and four million values, built under `target/` with `ferro-bench
--bin reclaim table` and `--bin clone build`.

At 4 threads the copy of the table rows spent its time like this, the walking thread's share in
order:

| Step | Time |
| --- | ---: |
| Walking the chains before the copy | 36 ms |
| Reading records, in 383,362 `pread`s | 139 ms |
| Looking up 2,852,005 links | 48 ms |
| Writing records and recording where they went | 66 ms |
| Checking 40,001 chain entries before decoding them | 11 ms |
| Copying bodies of objects it holds, and filling batches | 18 ms |
| The rest of the walk | 92 ms |
| Syncing the new file | 1 ms |
| All of it | 0.42 s |
| Checking batches, on the other threads | 0.43 s of CPU |

So the walking thread is now the limit, and more than four threads change nothing measurable. A
record it reads takes 360 ns of the walk, where the same `pread`s in a loop of nothing else take
215 ns: most of history is a path of nine or so objects for each version, each found from the one
above it and scattered across the file, and a lookup or a read that misses the cache waits for
memory.

The map of copies is as big as before, about 50 bytes an object and 70 a commit. The threads add up
to twenty batches with three checking threads, ten in flight and as many spare, each of 256 to 512
KiB, and a buffer of up to 1 MiB on each thread for decompressing. The
walk keeps up to 64 sets of buffers for the objects it holds, at most 4 MiB.

The pause is unchanged and a cycle is shorter. Alternating runs of `ferro-bench --bin reclaim
live` as under [Measurements](#measurements), on 2026-11-04 with the load average at 1 to 2,
medians of six runs each on table updates and three on `ledger`: the longest pause in a run was
2.96 ms before and 2.98 ms after on table updates, and 3.14 and 2.95 ms on `ledger`. A cycle took
0.19 s before and 0.12 s after on table updates, and 0.16 and 0.15 s on `ledger`, which retains far
less. Commits a second were 4,437 and 4,831 on table updates and 4,929 and 4,937 on `ledger`, and
p99 on `ledger` 1.22 and 1.24 ms. Other work's syncs on the disk halved the commits of four of the
twelve table runs, two of each build.

`reclaim_threads.rs` compares the files copied at 1, 2 and 4 threads byte for byte, checks that
every record follows its children and that the records tile the copy, damages a record only old
history reaches and expects the cycle and `admin::compact` to fail at its offset with no temp file
left and the database still working, and counts the checking threads at a small and a large cycle.
`reclaim_crash.rs` cuts the power at every third operation of two cycles whose copies span several
batches, `reclaim_fault.rs` shuts a writer down while three threads copy with every write slowed,
and the unit tests damage one record in 30,000 at each batch boundary, damage two and
expect the first at every thread count, and drop a verifier with batches queued.
`reclaim_damage.rs` points a link of a node into a value three ways, at bytes that parse as a small
record linking garbage, at bytes claiming 16 MiB, and at those bytes with the node damaged too, and
expects a cycle at 1 and 4 threads to fail with the same hash mismatch and four to peak within 12
MiB of one. `a_damaged_raw_length_fails_verify_a_read_and_a_cycle` in `compress.rs`
gives a compressed body a length 100,000 bytes too long and expects `verify`, a read, a cycle at 1
and 4 threads and `admin::compact` to name its offset and leave the file as it was.
`epoch_stress.rs`, the unit tests and `reclaim_threads.rs` pass under AddressSanitizer and
ThreadSanitizer.

### Trees on several threads

Checking hashes elsewhere left the walking thread doing everything else for every object (#89). A
profile of it, sampled on `SIGPROF` at each millisecond of its CPU over ten copies of the table rows
under [Limits](#limits) at 4 threads, on 2026-11-07, 3,260 samples:

| Where the walking thread's time went | Share |
| --- | ---: |
| Reading records with `pread`, and again past the first 1 KiB | 27% |
| Looking up links in the map of copies | 10% |
| Recording copies in it | 8% |
| Walking the chains before the copy, reading and hashing each entry | 10% |
| Allocating and freeing the buffers of the objects it held | 10% |
| Writing the copy and starting its writeback | 5% |
| Relinking, parsing, the needs of entries and the rest | 30% |

On the 320 byte values, reading took 34%, writing and writeback 30%, appending records to the copy
17% and filling batches 5%. No one step was most of it, so making each cheaper on the same thread
could not do much better than its share, and the copy now takes the walk itself apart.

The walk copies log entries and the branch table as before, and hands the root of each entry it
reaches to the copy's threads as a unit, then goes on. Any other tree it reaches, from the branch
table or from a tree it copies itself, it reads first, and hands out only if none of its links names
a log entry, pruned history or offset 0; otherwise it copies the tree itself and treats its links the
same way. A node of the branch table links each branch's tip, so it stays with the walk, and the
current tree it links goes out. A thread discovers a unit: it reads each record, checks its hash before following its links, and
lists the records the unit copies in the order one thread writes them, children first. Every object
a unit reaches is claimed for it by hash, under one of 256 locks, with the unit's key, which orders
units as one thread reaches them: the order the walk handed them out in, then the index of each link
a unit was split at. The smaller key keeps the claim, so of two units that reach one object, the one
that comes first copies it, whichever thread got there first, and a thread that finds an object
claimed by a later unit takes it over and copies what it reaches below it again. A tree whose root
has two or more children no earlier unit holds is split into a unit for each such child and one for
the root, down to three levels below the tree's root, and a chain of nodes with one link each counts
as one level, so the current tree of a file becomes hundreds of units while each version's path in
history stays one.

Units are placed in key order, by whichever thread finds the next one discovered. Each record its
unit still holds takes the next offset, and its length follows from that offset and its links', all
placed already since a record follows its children, so the second pass the varint offsets need is a
sum over lengths, done one unit at a time behind the discovery. Placed records are written in jobs of
256 KiB or more, at their offsets: a thread reads each record again, takes its links' new offsets
and appends the record as the walk would, and writes the job with `pwrite`, one job at a time, since
the file system takes the file's lock for each buffered write. The walk waits for units only when it
writes an entry or a node of the branch table, which come after every tree handed out before them,
or when 16,384 units wait to be placed, until half of them are. A pass returns once every job is
written.

The copy is the same file byte for byte at any number of threads: the claims give each object the
first unit one thread would reach it from, a unit lists its records in one thread's order, and an
object two units listed is placed only by the one that holds it. `reclaim_threads.rs` compares the
copies made at 1, 2 and 4 threads, and the `reclaim` fuzz target those at 1 and 4. Only the cost of
the work changes with the threads: a unit taken over is discovered twice, 3% more records than are
copied on the table rows at 4 threads and 11 to 42% more at 16, and all of it costs more CPU than one thread, 1.0 to 1.3 s against
0.8 s, of which checking hashes is 38%, claims 13% and writing 15%.

A record the walk is about to write that a unit's fallback copied meanwhile is not written again: the
walk takes that copy, as it would have found it before going on on one thread. So the walk can go
down a chain of entries that a tree handed out before it links, and the bytes stay the same; the
first version wrote such entries twice. `fuzzed.rs` copies a tree that links a log entry at 1, 4 and
8 threads and expects the same bytes.

A unit stops at anything the walk treats otherwise: a link to a log entry or to pruned history, a
link at offset 0, or a record that fails to read, parse or check. It is tainted, and the walk copies
it itself, with the code it uses on one thread, when its turn comes in the order. So a failure is
reported in walk order with the error one thread gives, the threads follow no link of a record they
have not checked, and the walk checks what it reads itself before following it, so a cycle on
several threads no longer follows damage into the batches of 256 KiB above. Checking the walk's own
reads in those batches on more threads, alongside the units, put a later error first on the damaged
file in `fuzzed.rs`, so the walk checks them itself. With the branch table read by the walk, a unit
falls back to it only where a commit's tree links a log entry or the file is damaged, and there the
walk checks as one thread does. A thread reads a record of any length it claims, within the file,
against the budget below. An error the walk meets itself waits
for the units handed out before it and gives way to the first of their failures, as it waited for
the batches. The `reclaim` fuzz target found that it once did not: a damaged tree failed one thread
with `bad compressed body at 8192`, while on four the walk went on past the tree's unit and failed
on its own with `varint runs off the end of its object`. `fuzzed.rs` keeps the file, and expects
the first error at 1, 2 and 4 threads. The walk also no longer checks an entry again that the walk
down the chains read and checked in the same cycle.

Memory changes in three ways. The claims take about 60 bytes an object, where the map of copies
took about 50, and the map of copies now holds only entries and what the walk writes itself. A
unit's list takes 24 bytes a record and 4 a link from its discovery until it is placed, for at most
16,384 units the walk has handed out and every unit split from them, and the largest of those is the
largest tree below the third level of a split. And the records the threads read, the jobs they write
and the records over 1 KiB the walk reads share a budget of 16 MiB: a thread that would pass it
waits for the others, unless it would hold the bytes alone, so one record larger than that is read
by itself, and every buffer shrinks back once its record is done. Copying the table rows peaked at
68 to 70 MB of RSS at 4 and 8 threads, against 155 MB before, which held every object's copy in one
map, and the 320 byte values at 21 to 22 MB either way. Measured on the heap, 48 values of 8 MiB
peaked at 18 MiB at 4 and 8 threads against 32 MiB on one, and a link pointed at bytes claiming 16
MiB, as in `reclaim_damage.rs`, at 18 to 20 MiB at 1 to 8 threads, where before the budget it took
52, 68 and 116 MiB at 2, 4 and 8. Crash safety is unchanged: the threads write only the temp file,
in any order, and the pass waits for every write before the sync that comes before the rename.

`ferro-bench --bin reclaim copy`, one copy per run, alternating with the build before on 2026-11-07,
pinned to the 16 cores 0-15 of the 32-thread machine with nothing else running, `sync` and a pause
between runs, medians of three:

| File | Threads | Before | After |
| --- | ---: | ---: | ---: |
| 189 MB of table rows, 383,364 objects | 1 | 0.80 s | 0.80 s |
| | 4 | 0.38 s | 0.31 s |
| | 8 | 0.38 s | 0.26 s |
| | 16 | 0.38 s | 0.22 s |
| 331 MB of 320 byte values, 124,939 objects | 1 | 0.41 s | 0.41 s |
| | 4 | 0.14 s | 0.15 s |
| | 8 | 0.15 s | 0.11 s |
| | 16 | 0.14 s | 0.12 s |
| 601 MB of 600 KB values, 1,183 objects | 1 | 0.19 s | 0.19 s |
| | 4 | 0.19 s | 0.14 s |
| | 8 | 0.19 s | 0.14 s |
| | 16 | 0.19 s | 0.13 s |

The fastest runs of the table rows took 0.21 s at 8 threads and 0.22 s at 16, 899 and 870 MB/s,
against 0.38 s, 500 MB/s, at any count before. Runs that met writeback of an earlier run took up to
seconds, on the large values most of all, whose figures are the fastest of three. On the values, 4
threads lose 0.02 s to the extra CPU, and 8 or more gain 0.02 to 0.04 s. `ferro compact`, which
takes the default threads, 4 before and 8 now on this machine, took 0.23 s against 0.39 s on the
table rows and 0.14 against 0.18 s on the values. At 16 threads the walk is the limit again: the
chains before the copy take 37 ms, and its 40,000 entries about 100 ms more. The longest pause of
`ferro-bench --bin reclaim live table`, as under [Measurements](#measurements), medians of three
alternating runs, was 2.91 ms against 2.93 ms before, at 5,114 and 5,190 commits a second.

A review found that the first version of this copy handed a node of the branch table that was still
a stored record, as it is after a prune or in any file a cycle wrote, to the threads as a unit. Its
links to the tips tainted it, and the walk then copied everything below it, entries and trees, on
one thread and with no checking threads, so every background cycle, which always prunes, ran at one
thread's speed: on a 423 MB file of 6,000 commits of 8 updates, `Keep::Last(3000)` took 0.40 s at 1,
4 and 8 threads against 0.16 s before. Reading such a node first and handing out what lies below it,
as above, the fastest of three on 2026-11-07:

| Copy of the 423 MB file | Threads | Before | After |
| --- | ---: | ---: | ---: |
| `Keep::All` | 1 | 0.70 s | 0.69 s |
| | 4 | 0.32 s | 0.26 s |
| | 8 | 0.31 s | 0.19 s |
| `Keep::Last(3000)` | 1 | 0.40 s | 0.40 s |
| | 4 | 0.17 s | 0.15 s |
| | 8 | 0.21 s | 0.10 s |
| `Keep::All`, of a file a cycle wrote | 4 | 0.28 s | 0.29 s |
| | 8 | 0.32 s | 0.18 s |
| `Keep::Last(3000)`, of a file a cycle wrote | 4 | 0.17 s | 0.15 s |
| | 8 | 0.18 s | 0.11 s |

`Compacted::handed_out` counts the trees a cycle handed out, and `reclaim_threads.rs` checks that
copies at 2, 4 and 8 threads, keeping everything and the last 20 versions, and the copy of the file a
cycle wrote, hand out trees and make the same bytes as one thread.

### Measured and left out

- Reading a node's children in one read. When the children still to copy lay within 256 KiB and
  averaged at most 16 KiB apart, the walk read the whole span once and parsed them from it, which
  meant looking up an object's links when it was read rather than when the walk reached them. On
  the values 110,105 of 124,937 records came from such a read, and reading fell from 59 to 40 ms,
  but copying each record out of the span cost as much again, and alternating runs took 0.21 to
  0.27 s either way. On the table rows only 6,100 spans formed, the early lookups took 55 to 61 ms
  against 51 to 53, and the copy took 0.48 to 0.51 s against 0.45 to 0.46.
- Reading the file from memory. With the whole file read into memory first, which a mapping would
  approach, reading the table rows' records took 141 to 146 ms against 169 to 172 with `pread`s,
  loading the file included, because a record far from the last one misses the cache either way.
  That is under a tenth of the walk, for unsafe code and a mapping that a file truncated under it
  turns into `SIGBUS`.
- Fetching ahead on the checking threads. Not built. The walk needs an object's links before it can
  go on, and most objects in the table rows' history lie on the path of one version from its root,
  each found only from the one above, so a thread fetching ahead would have nothing to fetch until
  the walk got there, and the walk would wait for it instead of for a read. Where a node's children
  could be fetched together, the walk would still copy each record out of the other thread's
  buffer, at a cost close to the `pread`'s.
- Writing on another thread. The walk as it was, handing each 1 MiB of the copy to a thread that
  wrote it and started its writeback, took 0.36 s against 0.39 at 4 threads and 0.78 against 0.80
  at 1 on the table rows, and 0.12 against 0.13 and 0.38 against 0.41 on the 320 byte values,
  medians of three alternating runs on 2026-11-07. Units take the writing off the walk as well, and
  the reading and the map of copies with it.
- A first read of 4 KiB instead of 1 KiB, so that a record of a few KiB takes one `pread`: 0.391
  against 0.387 s at 4 threads on the table rows, and 0.128 against 0.133 s on the values.
- Units for the roots of entries only, with the walk waiting for every unit after each 4,096. The
  branch table reaches the current tree before any entry, so the walk still copied its 75,221
  objects itself, the values' tree, whose root has one link, was never split, and each wait left
  the threads idle for 5 to 9 ms. The table rows took 0.33 s at 4 threads and 0.23 s at 8, and the
  values 0.15 s at any count.

## Generations

A `Store` is one generation: a file, a node cache, a dedupe cache and an id. Offsets mean nothing
outside it. Decoded nodes, memos and cached entries all hold offsets, and each belongs to the
generation it was read from, so none of them needs rewriting. A cycle replaces the whole set, and
the first reads after it are cold. The page cache is warm, because the copy just wrote those bytes,
so a miss costs a decode and not a disk read.

A branch's state holds its chain and its `Store` together, because a chain offset read through the
wrong file would fail its hash check and look like corruption. `at`, `as_of`, `log` and `history_of`
all take both from one state. Commits compare root hashes, not offsets, so the same commit in two
generations is equal.

Whatever holds a `Store` keeps reading the file it came from: a `Snapshot`, a `Map` cloned from one,
a `Log`, a `History`, or a handle on a dropped branch. Nothing old ever fails, because nothing old
changes. A cycle frees no disk space while such a handle lives, and it holds the whole old file, not
only its version. `Space::retired_bytes` shows that. A branch's watcher feed would hold the file
too, so a swap rebinds its latest snapshot without firing it. Old generations read uncached, because
a cached node can hold a `Map` that holds the `Store`, which would keep the file open forever.

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
A handle opened with `Options::follow` does this on its own as soon as it sees the rename, as
described under [Followers](processes.md#followers).

## Crashes

Until the rename is durable, the name points at the old file, which is complete and has every
acknowledged commit, and a writer's open removes the temp file. The new file is synced before the
rename, so once the name points at it, it is complete. Reading each branch's tip and root from it
first means a bug in the copy fails the cycle while the old file still has the name. Writers stay
paused until the directory sync returns, so no commit is acknowledged in a file whose name could
roll back. If that sync fails the store poisons, because the new file may or may not be the one
found after a crash. Both slots are written so that one damaged sector cannot lose a new generation
with no commits yet.

A failed copy, a full disk or a failed rename removes the temp file and reports the error from
`Db::compact` or in `Space::last_error`, and commits never notice. So does an object that fails its
hash check on any of the copy's threads, since a pass waits for every check before it returns. The
writeback the copy starts as it goes makes nothing durable, so the order above is unchanged. A
store poisoned while a cycle copied stays poisoned, because the swap checks first. On Unix the new
file takes the old one's mode and owner, and a path through a symlink stays a symlink. Windows
changes how the rename and its sync work, but not this order, as described under
[Windows](windows.md#renaming-over-an-open-file).

## Measurements

Four workloads ran for 15 seconds each on the test NVMe with ext4, with fsync, keeping the last 100
versions and starting a cycle every 64 MiB. These are medians of three runs of `ferro-bench --bin
reclaim live`, on 2026-11-04 with nothing else running, before the copy ran on several threads; how
that changed pauses and commits is under [The copy on several
threads](#the-copy-on-several-threads):

| Workload | Commits/s without cycles | With cycles | Longest pause | File after a cycle | Live | Grows to without cycles |
| --- | --- | --- | --- | --- | --- | --- |
| `metrics`, one commit per scrape | 5,242 | 5,282 | 2.9 ms | 1.5 MiB | 1.5 MiB | 254 MiB |
| `ledger`, single entries | 4,945 | 4,935 | 3.8 ms | 5.2 MiB | 4.6 MiB | 460 MiB |
| `wiki`, edits of 64 KiB pages | 565 | 566 | 4.4 ms | 7.2 MiB | 7.2 MiB | 90 MiB |
| Table updates, 200,000 rows | 5,151 | 5,184 | 2.8 ms | 13.2 MiB | 12.4 MiB | 338 MiB |

p99 commit latency moved from 0.92 to 0.93 ms, 1.14 to 1.33 ms, 5.17 to 5.20 ms and 0.93 to 0.98 ms.
The file saws between what is live and 64 MiB above it. While both files exist the peak is higher by
what has been copied, which came to 77 MiB on `ledger` and 94 MiB on table updates. The same table
measured on 2026-11-03, on a machine other work was sharing, had `wiki` at 670 and 680 commits a
second. The drop, #57, was in the example's tokenizer and not in the store, as [Where the flags
live](read-scaling.md#where-the-flags-live) describes, and with the tokenizer rewritten `wiki` ran
1,004 commits a second without cycles against 548, medians of three alternating runs on a machine
other work was sharing.

The rest of this section was measured on 2026-11-03. `metrics load` at 20,000 points a second with
one commit per scrape grew to 190 MB without cycles. With them the file sawed between 3 and 62 MB
with two cycles in 30 seconds, and the longest pause was 1.8 ms. It used to stall writers for 0.34
to 0.74 s, dropping the database each time.

With 8 readers on a 1M row table and 60 MiB live, at 4,200 commits a second with or without cycles,
reads went from 1.67M a second to 1.54M, because every swap empties the node cache. p99 read latency
went from 53 to 55 µs. Each cycle took about 1.1 s in the background and the longest pause was 5 ms.
In `Durability::Buffered` the longest pause was 7.2 ms at 18,000 commits a second on `metrics`, 7.6
ms at 11,700 on `ledger`, and 8.5 ms at 15,800 on table updates.

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
  blocks, and one live record pins its block. Over a whole run of 0.2 to 2.6 GB it freed 99.9% of
  the garbage in `metrics`, whose live data clusters by time, but 70 to 90% elsewhere, leaving 8 to
  17 times the live size. Per 64 MiB cycle it freed 44 to 91%, because the same live records sit in
  less garbage. Each commit scatters survivors across the log, so this decides it.
- Copying only the records in sparse runs to the tail and punching the rest copies a quarter as much
  on the 1M row table. Offsets stay valid through a table from offset to place, so caches stay warm.
  It was built in full and is in the history at `c384c6c`. It reads 3% faster than swapping and
  needs less spare disk. It lost on the rest. It ran 13 to 21% fewer Buffered commits, with a table
  check on every link and a pin for every snapshot in the path. The log's apparent size only grows,
  so ext4 would stop it at 16 TiB, which at 10 MB/s of log is 20 days, and a tool that does not
  preserve holes would copy the apparent size. A `Map` that outlived its snapshot and a read-only
  handle that lagged both needed a new error, and a filesystem that cannot punch freed nothing.
  Review found that a head rebuilt by dedupe could be reclaimed, which left the file unopenable.
  Liveness there was a property every path had to get right, and here an old object is simply never
  freed while anything can reach it.
- A segmented log drops a segment only when nothing in it is live. At 1 MiB or more that freed 0 to
  1% of the garbage in `ledger`, `wiki` and table updates, and 90 to 99.8% in `metrics`. Live data
  would have to move, which needs the same table or rewritten parents, and a directory gives up the
  single file.

## Limits

A cycle walks the log on one thread and copies the trees it reaches on up to seven others, checking
every object's hash as it goes, as described under [The copy on several
threads](#the-copy-on-several-threads). On 2026-11-07 the copy of a file in the page cache, keeping
everything, ran at 870 MB/s on 189 MB of table rows, 200,000 rows and 40,000 updates in 383,364
objects, and at 2,780 MB/s on 331 MB of a million 320 byte values in 124,939 objects, with 16
threads, where it ran at 500 and 2,450 MB/s before with 4 or more. With 4 threads it now runs at
620 and 2,150 MB/s, with 8 at 720 and 2,900, and on one thread at 237 and 806 MB/s as before. So a
10 GB database of table rows takes about 12 seconds and needs 10 GB free. The trigger keeps that to
one extra byte copied per byte written, at worst. Past 8 threads the walking thread is the limit
again on table rows, since it reads each log entry twice, walking the chains and copying them.
`Keep::All` copies all of history, so `history` is what makes background cycles worthwhile. The
copy holds its claims, about 60 bytes per object, about 80 bytes per commit for where each entry
was read and copied and what its copy keeps, the lists of the trees waiting to be placed, 24 bytes
a record and 4 a link, and 16 MiB of records the threads and the walk hold at once, or one larger
record alone, besides a buffer of up to 1 MiB on each thread.

The pause copies what the last pass left, so it depends on how much reachable data changed in that
pass, not on the size of the database. Writers that outrun the copier for all six passes make the
pause longer. That did not happen in any run here, at up to 19,000 commits a second, but nothing
bounds it.

`Space::live_bytes` is an estimate: the size of this generation when it began, plus the bytes since
then scaled by the share that survived the last cycle. Before the first cycle it is the file size.
`verify` still gives the exact number.

## Offline compaction

`admin::compact` is the same copy in one pass, on the same threads, with no writer running. It
prunes by raising `first` in a private copy of the refs and needs no user types. The copy is
iterative, because a chain is as deep as the number of commits. A read-only handle follows it on
`refresh` the same way it follows a cycle.
