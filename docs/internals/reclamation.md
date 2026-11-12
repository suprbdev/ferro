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
   walk copies log entries itself, which up to four other threads read ahead of it, and hands each
   tree it reaches to up to seven other threads, which check every object's hash and write the
   tree where one thread would have, as described under [The copy on several
   threads](#the-copy-on-several-threads). Chain entries are copied like anything else, but a link
   to an entry that is past `first` and was not brought in by a retained merge, as described under
   [Merges and pruning](merging.md#merges-and-pruning), is written with offset 0 instead of being
   followed, and so is a skip link below where the entry's `prev` links reach.
   Copies are keyed by hash, and entries by the offset they were read from, as described under
   [Commits stored twice](sync-receive.md#commits-stored-twice), so a later pass from newer refs
   visits only what is new. An entry's copy also records how much history it was made for, and a
   walk that needs more copies the record again, as described under [Copies made for less
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
record the skip link names, and the copy the map names otherwise, which is what it did before. A
copy it reaches that was read from a record of another hash fails the cycle with `skips to another
entry`, since the walk of a fork's chain stops where another branch's walk took over, before it
meets the target of a skip link into the history the two share. That search counts one version for
each `prev` step, which holds because the walk of the chains refuses a `prev` link that reaches
another version than the one below, as [What readers rely on](history.md#what-readers-rely-on)
describes.

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
`admin::compact` uses the same copy with the default. Once units took the trees, no cycle started
the checking threads of the batches above any more, so #92 removed them and their unit tests: the
walk checks each record it reads itself, at any number of threads.

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
left and the database still working, and counts the copy's threads at a small and a large cycle.
`reclaim_crash.rs` cuts the power at every third operation of two cycles whose copies span several
batches, and `reclaim_fault.rs` shuts a writer down while three threads copy with every write
slowed. Unit tests damaged one record in 30,000 at each batch boundary, damaged two and expected
the first at every thread count, and dropped a verifier with batches queued, until the batches
went.
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
a log entry, pruned history or offset 0; otherwise it copies the tree itself and treats its links
the same way. A node of the branch table links each branch's tip, so it stays with the walk, and the
current tree it links goes out. A thread discovers a unit: it reads each record, checks its hash
before following its links, and lists the records the unit copies in the order one thread writes
them, children first. Every object a unit reaches is claimed for it by hash, its slot found under
one of 256 locks or in a table of the hashes the thread met lately, as described under [Past eight
threads again](#past-eight-threads-again), with the unit's key, which orders units as one thread
reaches them: the order the walk handed them out in, then the index of each link a unit was split
at. The smaller key keeps the claim, so of two units that reach one object, the one that comes
first copies it, whichever thread got there first, and a thread that finds an object claimed by a
later unit takes it over and copies what it reaches below it again. A tree whose root has two or
more children no earlier unit holds is split into a unit for each such child and one for the root,
down to three levels below the tree's root, and a chain of nodes with one link each counts as one
level, so the current tree of a file becomes hundreds of units while each version's path in history
stays one.

Units are placed in key order, by whichever thread finds the next one discovered. Each record its
unit still holds takes the next offset, and its length follows from that offset and its links', all
placed already since a record follows its children, so the second pass the varint offsets need is a
sum over lengths, done one unit at a time behind the discovery. Placed records are written in jobs
of 256 KiB or more, at their offsets: a thread reads each record again, takes its links' new offsets
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
copied on the table rows at 4 threads and 11 to 42% more at 16, and all of it costs more CPU than
one thread, 1.0 to 1.3 s against 0.8 s, of which checking hashes is 38%, claims 13% and writing 15%.

A record the walk is about to write that a unit's fallback copied meanwhile is not written again:
the walk takes that copy, as it would have found it before going on on one thread. So the walk can
go down a chain of entries that a tree handed out before it links, and the bytes stay the same; the
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
and the records over 1 KiB the walk reads share a budget of 16 MiB, a compressed record a thread
checks counted at its stored and decompressed sizes together, and the walk holding its share only
while it reads, never while it hands out or waits for units: a thread that would pass it
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
chains before the copy take 37 ms, and its 40,000 entries about 100 ms more, as described under
[The walk past eight threads](#the-walk-past-eight-threads). The longest pause of
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
copies at 2, 4 and 8 threads, keeping everything and the last 20 versions, hand out trees and make
the same bytes as one thread, both in the first copy and in the copy of the file a cycle wrote. It
does the same at 1, 2, 3 and 8 threads for a file of eight branches with merges, restores, nested
maps and 20 KB values, keeping everything, the last 15 versions and the last 300 ms by its clock.

### The walk past eight threads

With units, the walk was the limit again past eight threads (#92): the table rows took 0.22 s at 8
threads and no less at 16. Timers around the walking thread's steps, copying the table rows under
[Limits](#limits) on 2026-11-07, pinned to cores 0-15 with nothing else running, medians of three
copies in a build the timers slow to 0.24 s:

| The walking thread | 8 threads | 16 threads |
| --- | ---: | ---: |
| Walking the chains before the copy | 46 ms | 43 ms |
| of which reading and checking 40,001 entries | 21 ms | 22 ms |
| of which recording what the copy needs of them | 13 ms | 12 ms |
| Reading and decoding the same entries again in the copy | 34 ms | 35 ms |
| Handing out units, besides waiting | 4 ms | 22 ms |
| Writing the entries | 14 ms | 14 ms |
| Waiting for units, at the limit of 16,384 and at the end | 99 ms | 74 ms |
| All of it | 228 ms | 225 ms |

So the other threads sat idle while the walk went down the chains, and the walk then waited for
them. Sampling every thread on `SIGPROF` at each millisecond of its CPU, over eight copies at 16
threads, put 36% of all their CPU in checking hashes, decompressing and running BLAKE3, 33% in
discovering units, reading records and claiming them, and 12% in writing. With other work on the
sibling cores 16-31, the threads spent 23% of their samples on the lock of the byte budget, which
every compressed record takes and gives back, and most copies at 16 threads took 0.4 to 0.9 s.

Four changes went in. Each was measured on the table rows on top of those above it, medians of
five alternating runs for the first five rows and of six for the last two, in seconds:

| Build | 1 thread | 4 | 8 | 16 |
| --- | ---: | ---: | ---: | ---: |
| Before | 0.815 | 0.318 | 0.229 | 0.346 |
| Entries kept from the walk down the chains | 0.810 | 0.321 | 0.238 | 0.242 |
| And the budget counted without a lock | 0.804 | 0.316 | 0.225 | 0.225 |
| The budget alone | 0.806 | 0.319 | 0.221 | 0.237 |
| And cheaper maps | 0.789 | 0.308 | 0.211 | 0.221 |
| The same, in a second set of runs | 0.800 | 0.308 | 0.206 | 0.216 |
| And hashes checked ahead | 0.790 | 0.288 | 0.186 | 0.192 |

- Entries are read once. The walk down the chains appends the bytes of each entry's record it
  reads, as stored, to one buffer, and keeps their place by offset with the entry's hash and which
  of its links it has. When the copy reaches that record as an entry it parses it from the buffer,
  takes it as checked, as it already did for an entry the walk had read, and decodes nothing.
  Reading entries in the copy fell from 35 to 12 ms at 16 threads and from 34 to 11 ms at 8. What
  the buffer and the map of places have allocated is counted against 16 MiB, and once a read takes
  it past that the cache keeps nothing more for the pass, so it holds at most 16 MiB and the last
  growth of its buffer or map; past that the copy reads entries from the file as before. The first
  form counted each entry at its length and 64 bytes, which held 125,927 entries of about 69 bytes
  at 27.7 MB, 1.65 times what it counted. It is freed when the pass ends.
- The budget counts without a lock while the bytes fit. A thread adds its bytes with a
  compare-and-swap, and only one that would pass the budget takes the lock, counts itself and
  waits, so a thread giving bytes back takes the lock to wake waiters only when there are any. On
  its own it took the copies at 16 threads, which ran from 0.22 to 0.72 s, to 0.22 to 0.24 s.
- The maps keyed by offset hash it with one multiply, folded, where they used SipHash, and the walk
  keeps the hashes of the entries it retains in a set that takes them as they are. Writing the
  entries fell from 14 to 10 ms, and the walk down the chains by 2 to 4 ms.
- Hashes are checked ahead. While the walk goes down the chains it hands the root of each entry it
  will copy to the copy's threads, which start then. A thread with no unit or job to take checks
  the records below that root, each before the links it follows, from a stack of its own, gives
  half of the stack back to the queue past 128 records, and gives it all back and stops once a unit
  or a job waits or the pass has written its units. It claims nothing and writes nothing: the slot
  of each record's hash records the offset at which the record was checked, 8 bytes more for each
  object, and a unit that reaches the record at that offset reads and parses it without
  decompressing or hashing it again. A unit records the records it checks the same way, so no
  thread checks them again. Of about 350,000 reads of records in units on the table rows, 53,000
  took the record as checked at 4 threads, 143,000 at 8 and 161,000 at 16. The threads would
  otherwise wait, but the walk slows as more cores run beside it: it took 37 ms down the chains on
  one thread, and 42, 46 and 51 ms beside 3, 7 and 15 threads checking. A record that fails its
  check is left for its unit, which fails it in walk order as before.

The copy is the same bytes at any number of threads: the entries kept, the budget and the checks
change only where a record's bytes come from and whether its hash is computed again, never which
unit holds a record or where it goes. `reclaim_threads.rs`, `fuzzed.rs`, `reclaim_damage.rs` and
the `reclaim` fuzz target pass unchanged.

`ferro-bench --bin reclaim copy`, alternating with the build before on 2026-11-07, pinned to cores
0-15 with nothing else running, `sync` and a pause of a second between runs, medians of five runs
for the first two files and the fastest of three for the large values, which writeback of earlier
runs slowed up to tenfold in both builds:

| File | Threads | Before | After |
| --- | ---: | ---: | ---: |
| 189 MB of table rows, 383,364 objects | 1 | 0.82 s | 0.80 s |
| | 4 | 0.32 s | 0.30 s |
| | 8 | 0.22 s | 0.19 s |
| | 16 | 0.55 s | 0.19 s |
| 331 MB of 320 byte values, 124,939 objects | 1 | 0.41 s | 0.41 s |
| | 4 | 0.15 s | 0.16 s |
| | 8 | 0.11 s | 0.11 s |
| | 16 | 0.16 s | 0.11 s |
| 601 MB of 600 KB values, 1,183 objects | 1 | 0.18 s | 0.19 s |
| | 4 | 0.13 s | 0.13 s |
| | 8 | 0.13 s | 0.13 s |
| | 16 | 0.12 s | 0.12 s |

Before, four of the five copies of the table rows at 16 threads took 0.44 to 0.85 s on the lock of
the budget, and the fastest 0.23 s. The values have ten commits, so they have no chain to speak of
and gain only where the budget's lock cost them. `ferro-bench --bin reclaim table` takes a third
number now, the updates in each commit. A file of a million rows and 6,000 commits of 8 updates,
255 MB in 501,635 objects, copied as `RECLAIM_KEEP` and `RECLAIM_AGAIN` set, medians of three:

| Copy of the 255 MB file | Threads | Before | After |
| --- | ---: | ---: | ---: |
| `Keep::All` | 1 | 1.04 s | 1.04 s |
| | 4 | 0.38 s | 0.37 s |
| | 8 | 0.28 s | 0.23 s |
| | 16 | 0.31 s | 0.23 s |
| `Keep::Last(3000)` | 1 | 0.68 s | 0.67 s |
| | 4 | 0.27 s | 0.25 s |
| | 8 | 0.18 s | 0.16 s |
| | 16 | 0.43 s | 0.16 s |
| `Keep::All`, of a file a cycle wrote | 4 | 0.36 s | 0.37 s |
| | 8 | 0.27 s | 0.21 s |
| | 16 | 0.27 s | 0.23 s |
| `Keep::Last(3000)`, of a file a cycle wrote | 4 | 0.24 s | 0.24 s |
| | 8 | 0.17 s | 0.15 s |
| | 16 | 0.19 s | 0.16 s |

Peak RSS copying the table rows rose from 158 to 162 MB on one thread and from 70 to 83 MB at 4
threads: the entries kept, the slots' checked offsets and the roots waiting to be checked. An
entry's rungs, which links of its record are its root, `prev` and skip links, now take three flags
where they took five words, on the copy's stack as well. The longest pause of `ferro-bench --bin
reclaim live table`, as under [Measurements](#measurements), medians of three alternating runs, was
2.69 ms against 2.74 ms before, at 5,277 and 5,267 commits a second, and a cycle took 0.53 s
against 0.54 s.

16 threads still copy no faster than 8. The walk down the chains takes 51 ms beside 15 threads
checking against 46 beside 7, and handing out units cost the walk 22 ms at 16 threads against 4 at
8, on the lock of the queue the threads take units from.

### Past eight threads again

On 2026-11-11 the copy of the table rows took 0.191 s at 8 threads, 0.200 s at 16 and 0.319 s at
32, medians of five runs pinned as above, cores 0-15 up to 16 threads and all 32 at 32. Stamps
along the first pass, the copy's CPU time and stacks of every thread, taken by attaching `gdb`
200 times during repeated copies at 16 threads, put the time in three places, the first and last
on one thread. The stamps slow the copy by a few percent.

- The walk down the chains ended at 46 to 49 ms at 4 and 8 threads, 64 to 70 ms at 16 and 134 ms
  at 32, against 50 ms on one thread, of which reading and checking the 40,001 entries took 27. It
  reads each entry from the one above it, so it waits for each `pread` in turn, 37% of its samples
  at 16 threads, and it slows as more threads check ahead beside it, most at 32, where one of them
  shares its core. The other threads only check ahead meanwhile, and two fifths of the samples at
  16 threads fell in this part.
- Units were placed at about 5,000 every 14 ms at 8 threads and at 16 alike. Placing them kept
  its thread busy 21 to 33 ms at 4 and 8 threads, 60 to 64 ms at 16 and 85 ms at 32, most of it
  reading the offsets of the records each record links from slots that the threads claiming and
  checking were writing. With the first change below in place, the copy used 1.89 s of CPU in
  0.226 s at 16 threads, 8.4 of 16 cores, against 1.25 s in 0.225 s at 8, and discovering units
  took 1,050 to 1,430 ms of the threads' time at 16 threads against 650 to 680 ms of their CPU,
  where at 8 it took 709 ms against 690: the threads waited on the locks of the claims and of the
  queue, whose waiters spin, yield and park, and on whatever else ran on their cores.
- Once every unit was placed, the walk wrote the 40,001 entries, from 29 ms at 8 threads to 38 and
  40 ms at 16 and 32.

Three changes went in. In one set of alternating runs on the table rows, medians of five, in
seconds, each of the middle rows leaves one of them out, the third by turning the reading ahead
off in the build of the last:

| Build | 1 thread | 4 | 8 | 16 | 32 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Before | 0.809 | 0.309 | 0.196 | 0.202 | 0.312 |
| Fewer locks, chains read ahead | 0.803 | 0.303 | 0.184 | 0.204 | 0.235 |
| Fewer locks, offsets apart | 0.804 | 0.292 | 0.190 | 0.198 | 0.264 |
| All three | 0.807 | 0.290 | 0.178 | 0.203 | 0.223 |

- Claims take fewer locks. Each thread keeps a table of 4,096 hashes it met lately with their
  slots, indexed by the hash's first bytes, so a claim of one of them finds its slot without the
  lock of its shard, and a claim lowers the slot's key with a compare-and-swap, and only when its
  key is lower than the one it reads, so a claim that loses writes nothing. A thread numbers the
  slots of hashes it meets first from a block of 1,024 it takes at once, rather than one at a time
  from a counter every thread shares. Of 3.9 million lookups at 16 threads, 1.5 million found
  their slot in the thread's table. The end of each item is counted with an atomic add rather than
  under the queue's lock, the walk waits for it on a lock of its own, a thread is woken for each
  item queued rather than every thread for any, and the queue orders units by a key held beside
  each rather than read through its pointer.
- The offsets placed records take are kept apart from the slots, so the thread placing them reads
  them from lines the others do not write, and the root of each unit the walk dispatched is found
  by the unit's number rather than looked up by hash: a link to a unit holds that number, taken
  from `u64::MAX`, until it is placed. Placing kept its thread busy 25 to 29 ms at 8 threads, 28 to
  40 ms at 16 and 43 to 59 ms at 32.
- The chains are read ahead. Before the walk goes down a branch's chain of 2,048 entries or more,
  it splits the chain's versions into up to four stretches, one for each of up to four of the
  copy's other threads. A thread reads its stretch down the `prev` links, from the tip or from the
  entry the skip links lead to from the tip, checks each record's hash and decodes it to find the
  next, and hands the records to the walk in two batches of 4 and then in batches of 32, marking
  its stretch read with the last of them. The walk takes each entry it reads from the stretch that
  holds it next, waiting for that stretch's thread if it is on its way, and reads it itself
  otherwise: an entry no thread has reached, one a damaged link sent elsewhere, one reached
  through a merge, or one past where a stretch's share of 16 MiB filled. So the walk sees the same
  records in the same order, and meets any error on its own read with the error one thread gives.
  The walk stops at an entry an earlier pass walked, so a later pass and the pause read ahead only
  the versions above the newest one an earlier pass walked on that branch, and only when there are
  2,048 of them or more; a branch no earlier pass walked, or whose tip one did, is not read ahead
  then. All 40,001 entries of the table rows came from the stretches, and the walk waited 3 ms for
  them in all. The walk down the chains now ends at 22 to 29 ms at 8 threads, 36 to 50 ms at 16
  and 72 to 94 ms at 32: its bookkeeping, six maps and the closures' marks for each entry, is what
  is left, and it slows as the other threads run.

Against the build before, on 2026-11-11, pinned as above, medians of five alternating runs:

| File | Threads | Before | After |
| --- | ---: | ---: | ---: |
| 189 MB of table rows, 383,364 objects | 1 | 0.809 s | 0.793 s |
| | 4 | 0.295 s | 0.291 s |
| | 8 | 0.191 s | 0.176 s |
| | 16 | 0.200 s | 0.191 s |
| | 32 | 0.319 s | 0.229 s |
| 331 MB of 320 byte values, 124,939 objects | 1 | 0.418 s | 0.417 s |
| | 4 | 0.150 s | 0.152 s |
| | 8 | 0.112 s | 0.112 s |
| | 16 | 0.114 s | 0.109 s |
| | 32 | 0.102 s | 0.107 s |

The values have ten commits, so their chain is not read ahead, and they move within the spread
of the runs. On cores 0-15, 16 threads lose time to something else that runs on cores 8-15 on this
machine: 12 threads on cores 0-11 and 16 on 0-15 made 16,000 to 45,000 involuntary context
switches a copy, where 8 on cores 0-7, 16 on the eight cores 0-7 and their siblings 16-23, or 16
on all 32 made a few hundred. On all 32, 16 threads copied the table rows in 0.167 s against 0.199
before, and 8 in 0.186 against 0.194; on cores 0-7 and 16-23, 16 threads took 0.166 against 0.189.
So 16 threads now copy faster than 8, by about a tenth, where before they did not, and 32 no longer
lose a third to 8. The default stays at 8.

The 255 MB file of 6,000 commits of 8 updates, as above, medians of three:

| Copy of the 255 MB file | Threads | Before | After |
| --- | ---: | ---: | ---: |
| `Keep::All` | 1 | 1.065 s | 1.060 s |
| | 8 | 0.308 s | 0.221 s |
| | 16 | 0.252 s | 0.227 s |
| `Keep::Last(3000)` | 1 | 0.685 s | 0.706 s |
| | 8 | 0.175 s | 0.155 s |
| | 16 | 0.167 s | 0.155 s |
| `Keep::All`, of a file a cycle wrote | 8 | 0.226 s | 0.215 s |
| | 16 | 0.229 s | 0.224 s |
| `Keep::Last(3000)`, of a file a cycle wrote | 8 | 0.151 s | 0.149 s |
| | 16 | 0.155 s | 0.146 s |

The fastest of the three took 0.239 against 0.220 s at 8 threads keeping everything, and 0.685
against 0.683 s on one thread keeping the last 3,000, whose code does not change. The copy is the
same bytes at any number of threads: `cmp` found the copies of the table rows and of the values
made at 3, 8, 16 and 32 threads identical to the one made on one, and the reclamation tests pass
unchanged. Peak RSS copying the table rows went from 160 to 162 MiB on one thread, 88 to 92 MiB at
8 and 90 to 98 MiB at 16, and copying the values from 24 to 28 MiB at 8 threads and 28 to 33 MiB at
16: the stretches read ahead, up to 16 MiB while the walk goes down a chain, the threads' tables of
hashes, 96 KiB each, and the number of each unit's root, 4 bytes a unit. The longest pause of
`ferro-bench --bin reclaim live table`, as under [Measurements](#measurements), medians of three
alternating runs, was 2.75 ms against 2.69 ms before, within the runs' spread of 2.59 to 2.87 ms,
at 5,160 and 5,159 commits a second, and a cycle took 0.58 s against 0.58 to 0.61 s.

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
- Keeping each entry's decoded body and links for the copy instead of its record's bytes, moved
  into the copy's stack whole, with a compressed body kept as stored. With the budget and the
  cheaper maps under both, the table rows took 0.782, 0.306, 0.206 and 0.222 s at 1, 4, 8 and 16
  threads against 0.789, 0.308, 0.211 and 0.221 with the buffer, within the runs' spread, for
  three allocations an entry where the buffer takes about 130 bytes.
- Checking ahead one record at a time, each record a turn of the queue's lock. The threads queued
  on the lock and checked 88,000 to 97,000 records while the walk went down the chains, and the
  table rows took 0.299, 0.207 and 0.209 s at 4, 8 and 16 threads against 0.313, 0.207 and 0.214.
- Checking ahead without units recording what they checked. At 16 threads 91,000 records were
  checked after their unit had checked them, and the table rows took 0.293, 0.192 and 0.203 s at 4,
  8 and 16 threads against 0.288, 0.186 and 0.192.
- Reading wider around each record. A thread whose next read lay within 64 KiB of its last read
  12 KiB before the record and 4 KiB after, and 32 KiB ahead for a write job, since a commit writes
  a version's path together, children first. Reading then took 39% of the threads' running samples
  against 29%, copying 16 KiB where it had copied 1, and the table rows took 0.183 and 0.191 s at 8
  and 16 threads against 0.179 and 0.192.
- Taking up to eight units from the queue at once while many waited. Each thread then discovered
  units further out of order, so later units claimed what earlier ones took over, and the threads
  discovered 500,000 to 580,000 records where 351,000 are copied.
- 4,096 shards of claims instead of 256. The table rows took 0.184, 0.193 and 0.225 s at 8, 16 and
  32 threads against 0.179, 0.193 and 0.227, and discovery still spent half its time off the CPU at
  16 threads.
- Making room in the walk's maps for every entry of a chain before going down it. The walk down
  the chains took 22 to 24 ms against 27 to 29 at 8 threads, but 48 to 54 against 37 to 40 at 16
  and 88 to 98 against 78 to 88 at 32.
- Letting only half or a quarter of the threads check ahead, so that the walk had cores to itself.
  At 32 threads copies took 0.200 to 0.220 s against 0.223 to 0.232, and at 16 0.197 to 0.207
  against 0.177 to 0.198, single runs within the spread of either.
- Writing each job's buffer from whichever thread holds the file, so that the others queue it and
  go on rather than wait for the lock. Waiting for the file fell from 313 ms of the threads' time
  at 16 threads to none, but the table rows took 0.182, 0.194 and 0.218 s at 8, 16 and 32 threads
  against 0.180, 0.194 and 0.223.
- Handing out the units the walk dispatched without the queue's lock, each taken by a
  compare-and-swap on a count: 0.179, 0.193 and 0.227 s at 8, 16 and 32 threads against 0.182,
  0.194 and 0.218.
- A format that lists each frame's objects in its header, or copying a frame whole when nothing in
  it is garbage. Not built. A file a cycle wrote has no frames, so the background cycles that
  follow the first would gain nothing, and with the chains read ahead the walk no longer waits to
  find entries but on its own bookkeeping.

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
a `Log`, a `History`, a handle on a dropped branch, or a `watch_changes` watcher whose last return
the new file no longer retains. Nothing old ever fails, because nothing old changes. A cycle frees
no disk space while such a handle lives, and it holds the whole old file, not only its version.
`Space::retired_bytes` shows that, on a writer and, once `refresh` has followed the new file, on a
read-only handle too. A branch's watcher feed would hold the file too, so a swap rebinds its latest
snapshot without firing it. Old generations read uncached, because a cached node can hold a `Map`
that holds the `Store`, which would keep the file open forever.

A `watch_changes` watcher holds the snapshot it returned last, to compare new ones with, and an
idle one on a branch that other branches' commits outgrow would hold every file it had seen. So the
feed lists those watchers by a `Weak`, and after each swap, whether a cycle's, a catch-up's or a
read-only `refresh`'s, each moves to the new file's snapshot of the same commit, read once for each
commit the watchers saw, while the new file retains that version. One whose version was pruned
keeps the old file until it next returns, like any snapshot a program holds. A cycle does this once
writers are let go, so the pause is unchanged, and a catch-up or a `refresh` once it has released
`tip`, in the notice that frees what it replaced. A watcher that returns meanwhile keeps what it
returned. Before, a watcher that never returned again kept the file of its last return on disk for
as long as it lived (#127), and a read-only handle reported `retired_bytes` of 0 while a snapshot
held from before its `refresh` kept the replaced file open. In `ferro/tests/reclaim.rs`,
`a_changes_watcher_moves_to_the_new_file_while_it_retains_what_was_seen` holds watchers at a pruned,
an older retained and the newest version across a cycle and a read-only `refresh`, and
`a_changes_watcher_follows_another_handles_cycle_to_the_new_file` across a shared writer's
catch-up; `a_read_only_handle_counts_the_file_a_held_snapshot_keeps_after_refresh` checks the count.

A move waits in its notice, and a later swap's move can run first: a cycle that began by catching up
with another process's cycle moved the watchers into its own new file, and the catch-up's notice,
dropped when the cycle returned, then moved them back into the file the cycle had just replaced,
which they kept on disk; a `fork` that caught up with one cycle and, on attaching the new branch,
with a second did the same. So a move puts a snapshot in place only while the branch still reads
the file it was found in, asking under each watcher's lock: a later swap changes the file the
branch reads before it reads the watchers, so it either stops the earlier move or moves on what
that move put in place (#137).
`a_changes_watcher_stays_in_the_new_file_after_a_cycle_that_caught_up_with_another_handles_cycle`
and `a_changes_watcher_stays_in_the_newest_file_when_one_call_catches_up_with_two_cycles` check
`retired_bytes` after each.

Moving them costs the cycle, not the writers. With 10,000 `watch_changes` watchers on a 100,000-key
map, measured with a probe kept out of the tree on 2026-11-08 on a machine shared with other work,
the pause stayed at 24 to 36 µs with or without them, as in the build before. The cycle around it
took about 0.5 ms more with all of them at one version, and about 5.5 ms more with them spread over
1,000 versions, 9.6 to 10.2 ms against 3.7 to 4.6 ms before, a lookup down the chain and a root read
for each version. A read-only `refresh` that followed a cycle took 0.7 ms with them at one version
and 4.3 ms spread over 1,000 versions, against 0.05 ms with none; the moving in it runs once `tip`
is let go.

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
from the real layout of live records in each workload's file, from `ferro-bench`'s `reclaim
layout`. Until 2026-11-07, `admin::layout` behind it counted as live the branch table, the steps
written on top of it and the history the older heads in those steps keep, none of which a cycle
copies. Run again on 2026-11-07 on a file of each workload as `reclaim` builds it, 0.18 to 1.75
GB, it now counts 2 to 7% fewer live bytes, at the last 1 or 100 versions, so what is left is up
to 4% more times the live size, 16.7 to 17.3 times for `ledger` with 4 KiB holes. The share of the
garbage that each unit or density reclaims moved by at most 1.2 points, 8.2 to 9.4% for `wiki` at
64 KiB, and by under 0.6 otherwise, so the figures below stand. Relocation ran live, and that
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

A cycle walks the log on one thread, with up to four others reading the chains ahead of it, and
copies the trees it reaches on up to seven others, checking every object's hash as it goes, as
described under [The copy on several threads](#the-copy-on-several-threads). On 2026-11-11 the
copy of a file in the page cache, keeping everything, ran at 1,078 MB/s on 189 MB of table rows,
200,000 rows and 40,000 updates in 383,364 objects, and at 2,958 MB/s on 331 MB of a million 320
byte values in 124,939 objects, with 8 threads, medians of five runs, against 993 MB/s on the table
rows before and as before on the values. With 4 threads it runs at 652 and 2,180 MB/s, with 16 at 993 and 3,040, and on one thread
at 239 and 794 MB/s, as before. So a 10 GB database of table rows takes about 9 seconds and needs
10 GB free. The trigger keeps that to one extra byte copied per byte written, at worst. Past 8
threads the copy of table rows gains about a tenth at 16 threads, 1,136 MB/s with them spread over
all 32 CPUs, and runs at 828 MB/s at 32, since the walk down the chains and the writing of the
entries after every tree stay on one thread, and the walk's bookkeeping slows as more cores run,
as described under [Past eight threads again](#past-eight-threads-again). `Keep::All` copies all of
history, so `history` is what makes background cycles worthwhile. The copy holds its claims, about
68 bytes per object, about 80 bytes per commit for where each entry was read and copied and what
its copy keeps, up to 16 MiB of the entries the walk down the chains read, counted at what their
buffer and map allocate, until the pass ends, and up to 16 MiB more of entries read ahead of it
while it goes down a chain, the lists of the trees waiting to be placed, 24 bytes a record and 4 a
link, and 16 MiB of records and decompressed bodies the threads and the walk hold at once, or one
larger record alone, besides a buffer of up to 1 MiB and a table of 4,096 hashes on each thread and
the one record the walk is copying.

The pause copies what the last pass left, so it depends on how much reachable data changed in that
pass, not on the size of the database. Writers that outrun the copier for all six passes make the
pause longer. That did not happen in any run here, at up to 19,000 commits a second, but nothing
bounds it.

`Space::live_bytes` is an estimate: the size of this generation when it began, plus the bytes since
then scaled by the share that survived the last cycle, all measured on the log, which ends before
any zeroes a writer preallocated. Before the first cycle it is the log's size, `Space::log_bytes`.
`verify` still gives the exact number.

## Offline compaction

`admin::compact` is the same copy in one pass, on the same threads, with no writer running. It
prunes by raising `first` in a private copy of the refs and needs no user types. The copy is
iterative, because a chain is as deep as the number of commits. A read-only handle follows it on
`refresh` the same way it follows a cycle.
