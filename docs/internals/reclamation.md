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
   `<file>.compacting`, re-emitting links with new offsets, verifying each object's hash and
   compressing again. Chain entries are copied like anything else, but a link to an entry that is
   past `first` and was not brought in by a retained merge, as described under [Merges and
   pruning](merging.md#merges-and-pruning), is written with offset 0 instead of being followed, and
   so is a skip link below where the entry's `prev` links reach. Copies are keyed by hash, and
   entries by the offset they were read from, as described under [Commits stored
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
retains that hash again, and a link already at offset 0 is never followed.

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
`Db::compact` or in `Space::last_error`, and commits never notice. A store poisoned while a cycle
copied stays poisoned, because the swap checks first. On Unix the new file takes the old one's mode
and owner, and a path through a symlink stays a symlink. Windows changes how the rename and its sync
work, but not this order, as described under [Windows](windows.md#renaming-over-an-open-file).

## Measurements

Four workloads ran for 15 seconds each on the test NVMe with ext4, with fsync, keeping the last 100
versions and starting a cycle every 64 MiB. These are medians of three runs of `ferro-bench --bin
reclaim live`, on 2026-11-04 with nothing else running:

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
second, and issue #57 tracks the drop.

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

A cycle reads, verifies, recompresses and writes everything retained, in one thread. `ferro
compact` of a file in the page cache, keeping everything, ran at 180 MB/s on 189 MB of table rows,
200,000 rows and 40,000 updates in 383,364 objects, and at 570 MB/s on 331 MB of a million 320 byte
values in 124,939 objects, on 2026-11-04. So a 10 GB database takes up to a minute and needs 10 GB
free. The trigger keeps that to one extra byte copied per byte written, at worst. `Keep::All` copies
all of history, so `history` is what makes background cycles worthwhile. The copy holds a map from
hash to offset, about 50 bytes per object, and about 70 bytes per commit for where each entry was
copied and what its copy keeps.

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
