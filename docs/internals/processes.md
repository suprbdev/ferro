# Writers in several processes

How `Options::shared` lets several processes commit to one file: the locks, how a process catches
up with the others, how frames and superblocks are written, how reclamation cycles are shared, and
what was measured and left out (#60). The user-facing rules are in [Writers and
concurrency](../guide/concurrency.md#several-processes).

## The locks

Without the option one process holds the writer lock for as long as its `Db` is open. A shared
writer takes that lock shared rather than exclusive, so it and a writer without the option, from
this build or an older one, refuse each other with `Error::Locked`, and shared writers do not refuse
each other. Everything else is a byte-range lock past any real file, at 2^62 and up:

| Lock | Held | For |
| --- | --- | --- |
| The writer lock, a `flock` on Unix and the byte at 2^62 on Windows | Shared, for the writer's life | Keeping out writers without the option |
| 2^62 + 1 | Shared, for the writer's life | Telling a read-only open that a writer is alive |
| 2^62 + 2, placement | Exclusive, per frame | Catching up, placing a frame and writing it |
| 2^62 + 3, superblock | Exclusive, per sync | Syncing and writing a superblock |
| 2^62 + 4, cycle | Exclusive, per cycle | A reclamation cycle, and the pieces of a large `receive` |
| 2^62 + 2^20 + a 60-bit xxh3 of the branch name | Exclusive, per closure | A branch's closure, across processes |

A cycle's prune and pause, the landing of a `receive` and anything else that replaces every
branch's head lock the whole range of branch bytes at once, as they take every lane within a
process. Locks are taken in the order lane, branch lock, `tip`, placement lock, and the leader of a
sync takes the superblock lock holding none of them. The cycle lock comes before all of them. A
process that locks the whole range first lets go of the branch bytes it holds itself, since on
Windows its own locks conflict with a range over them, and on Linux it unlocks the range around any
byte another of its threads took meanwhile, since unlocking a range there unlocks every byte in it.

On Linux the byte-range locks are open file description locks, `fcntl` with `F_OFD_SETLK` and
`F_OFD_SETLKW`. Classic POSIX record locks would not do: they belong to the process, so threads of
one process never conflict on them, and closing any descriptor of the file drops all of them, which
a read-only handle or a retired generation does. An open file description lock belongs to the open
file, so two shared `Db`s of one file in one process exclude each other as two processes do, and the
tests use that. macOS and the BSDs have no such locks, and an open with the option fails there with
an `Unsupported` I/O error. On Windows they are `LockFileEx` ranges, as the writer lock already was,
taken without `LOCKFILE_FAIL_IMMEDIATELY` where a writer waits. Both kinds are released by the
kernel when a process dies, which is what lets the others go on.

Two branch names can hash to one byte. That costs only parallelism, since a process counts how many
of its lanes hold each byte and unlocks it when the last one lets go, except that with nested writes
across branches two such names could deadlock two processes. With 60 bits, a thousand branch names
share a byte with odds of about 4 in 10^13.

## A commit

1. Under its lane the writer takes its branch's lock, waiting while another process runs a closure
   on that branch, and catches up, as described below. The closure then starts from the newest
   commit on its branch from any process, and nothing can move that head until the lock is
   released.
2. It runs the closure and drafts the commit outside `tip`, as before.
3. It takes `tip` and then the placement lock and catches up again, this time with frames other
   processes placed on other branches while it drafted. If any did, the draft moves, as it does when
   another branch's frame lands first within a process. It checks that its branch's head is the one
   it started from, then places the frame and writes it before releasing the placement lock.
4. The leader of the sync takes the superblock lock, syncs and writes a superblock, as described
   under [Superblocks and group commit](#superblocks-and-group-commit).

A closure never runs against a head another process has since moved, so it never runs again. The
other way round, optimistic closures checked at placement and run again on a conflict, would need
them to be `FnMut`, and `update` and every other write take an `FnOnce`, which is what lets a
closure move its captures into the root. A closure whose branch another process dropped while it
ran gets `Error::NoBranch`, as it does when the drop is in its own process.

## Catching up

A process knows where the log ended when it last looked, `placed`. To catch up it reads the frame
header there. If it is a frame with the next sequence number, it reads the whole frame and checks
it, then the next, until it finds one that is not. So a process that is alone reads one header of
68 bytes, a reservation of zeroes or the end of the file, each time it looks. In this mode a frame
is written before the placement lock is released, so a frame that a live process placed is whole
whenever another can take that lock, and one that a process left half-written when it died fails
its check, ends the log there, and is written over by the next frame.

The frames' head pointers name [steps](commit-path.md#branch-table-steps) on the table each process
already holds, so loading the new refs reads only the steps since the head this process last knew,
as a read-only `refresh` does. For every branch attached in this process whose head moved, it loads
the new state into the branch's tip, and a branch that is gone is retired. The frames go into the
queue as one entry, so the next sync publishes those states with the process's own commits, in
frame order, and fires their watchers. Nothing another process wrote is published here before it
is durable, by that process's sync or by this one's.

Catching up happens when a process takes a branch lock, before it places a frame, when it attaches a
branch it has not seen, before each pass of a cycle and on `refresh`. A process that does none of
these sees nothing new, as a read-only handle that never refreshes sees nothing new.

## Superblocks and group commit

Within a process one leader syncs every frame queued behind it. Across processes each leader used
to sync for itself, so 8 processes made 8 fdatasyncs where 8 threads made one. A superblock under
`Durability::Fsync` anchors itself, and every frame up to its anchor was written before the
fdatasync that preceded it, so a leader that finds a superblock anchored at or past its own frame
has nothing to sync. So the leader takes the superblock lock, waiting out any sync another process
is running, and reads both slots. If the newest one covers its frame, it publishes at once.
Otherwise it reads past its own frame to the newest whole frame of any process, syncs, and writes a
superblock naming that frame to the slot that does not hold the newest. Every process that queued a
frame meanwhile then finds it covered. A superblock is written only when it is newer than the
newest, so slots alternate and only one superblock write is ever in flight, as with one process.

Under `Durability::Buffered` nothing is synced and the lock only orders superblock writes. Each
process checkpoints for itself, and a superblock carries the anchor of the process that wrote it,
which is never past what an fdatasync covered.

A sync that fails in one process is reported to the others on Linux, which reports a writeback error
once to every open file of the inode at its next fsync, so a process whose frame followed the failed
one fails its own sync rather than acknowledging over a hole. Windows documents no such thing for
`FlushFileBuffers`.

## Opening and closing

A shared writer opens under the placement and superblock locks and recovers as a writer always has:
it scans from the anchor, syncs what it accepted and truncates the rest. Other writers may be alive,
but none can be placing a frame, and every frame they placed is whole, so the scan finds exactly the
log. A Buffered file with live writers may have up to `checkpoint_bytes` past its anchor to scan. It
removes a stale `.compacting` file only if it can take the cycle lock, since another process's cycle
may be writing it.

A shared writer does not truncate the Fsync reservation when it closes, since other writers may be
placing frames into it. The next writer to open truncates it, so the file can stay up to 4 MiB
longer than `size_on_disk` while no shared writer has it open.

A read-only open that can take the writer lock shared tests the lock that shared writers hold for
their lives, and treats a held one as a live writer, trusting the superblock instead of scanning and
syncing. Read-only handles otherwise behave as before.

## Reclamation

A cycle runs in one process at a time, under the cycle lock. A background cycle that finds it taken
skips its turn, and one that gets it after another process's cycle follows the new file first and
runs only if the file has grown enough since. `Db::compact` waits for the lock. Each pass catches up
first, so it copies the other processes' commits too, and the pause takes every branch lock and the
placement lock, catches up once more and copies what is left. Before it renames the new file over
the old one, the cycle writes a marker at the end of the old log, a frame header with the magic
`FERROGEN`. It holds the new file's placement lock until it has rebound its own branches.

A process that catches up and reads the marker checks the path. If the path names another file, it
syncs and publishes what it had queued in the old one, opens the new one, loads every attached
branch's state from it and rebinds its watchers' feeds, as a read-only `refresh` of a replaced file
does. If the path still names this file, the cycle failed before its rename and the marker is
written over. Every frame placed in the old file before the marker was copied into the new one,
since the pause caught up under the placement lock and the marker went in under it, and no
process can place in the old file after the marker without first finding it. Recovery reads the
marker as the end of the log, so a crash between the marker and the rename leaves the old file as
it was.

## Watchers, coalescing, awaited writes and receive

- Watchers fire for another process's commits when this process catches up and syncs them: at its
  next commit on any branch, at `refresh`, or when it attaches a branch.
- With `coalesce`, a group holds its branch's lock from its first closure until it is sealed, so
  another process's closures on that branch wait until the group is written. Groups are per process:
  closures in two processes never share a version.
- An awaited write takes its branch's lock without waiting. If another process holds it, the future
  returns `Pending` and a thread waits for the lock, lets it go and wakes the future, so no executor
  thread waits on another process's closure.
- `receive` places a large bundle's pieces under the cycle lock and lands it under every branch
  lock, so its checks run against the newest heads of every process.

## Crashes and killed processes

A process killed anywhere in a commit holds up the others only until the kernel releases its locks.
Killed while it holds a branch lock, its closure is lost. Killed between placing and writing its
frame, it wrote nothing, and the next frame goes where it would have. Killed while writing it, the
frame fails its check and is written over. Killed after writing it and before syncing, the frame is
whole and stays in the log, so its commit may appear though it was never acknowledged, as after a
crash in one process. Power cuts are unchanged: frames that follow each other from the anchor
survive, and every process's acknowledged commits were synced before they were acknowledged.

## Tests

`ferro/tests/shared_process.rs` spawns children of the test binary that open one file with the
option. Each commits `n += 1` with a counter of its own and prints a line for every commit that
returned, and the parent checks that versions run from 1 to `n` with no gap, that each child's
counters rise in order, that every acknowledged commit sits at the version it was acknowledged at,
and that the file verifies:

- Four processes under Fsync and six under Buffered write at once.
- A process aborts after drafting, between placing and writing its frame, after writing it, and on
  its first commit, while another writes throughout and a third takes its place.
- A process is killed at random eight times beside a steady writer.
- One process runs a cycle every 25 commits beside three writers and a read-only follower that must
  see every commit, and in another test aborts at each stage of its second cycle while two writers
  go on.

`ferro/tests/shared.rs` opens one file twice in one process, which locks as two processes do, for
what is easier to arrange there: forks and drops seen by the other handle, watchers fired by a
commit and by `refresh`, coalesced and awaited writes, an awaited write held up by the other
handle's closure, a pull landing beside the other handle's commits, and background cycles in three
handles at once. The tests that only spawn processes or open a file twice run on Windows too, and
those that kill or abort a process on Unix only, as elsewhere.

## Measurements

Without the option a writer takes no new lock and reads nothing past its own end: a commit only
tests a flag on its way. `tables`, one Buffered writer, median of six interleaved runs against the
tree before shared writers, on a machine other work was sharing:

| Commit | Instructions outside the closure, before | After | Shared, one process | CPU time, before | After | Shared |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Hold | 563,684 | 564,306 | 569,393 | 99.2 µs | 97.3 µs | 99.8 µs |
| Confirm | 729,967 | 730,246 | 734,852 | 127.1 µs | 124.2 µs | 127.4 µs |
| Cancel | 898,249 | 897,894 | 902,830 | 164.5 µs | 160.2 µs | 164.7 µs |

Bytes per write stayed at 13,362 in all three. The counter counts user-space instructions only, so
the shared column's 0.5 to 0.9% more leaves out the locks and reads themselves, which the CPU time
holds: 2.5 to 4.5 µs a commit over the build without the option. `group`, one writer making
single-key Buffered commits into 100,000 keys, median of seven alternating three-second runs: 47,940
commits a second before, 47,488 after and 42,718 with the option, p50 19, 19 and 21 µs. With fsync,
median of five: 2,604, 2,554 and 2,453, p50 377, 377 and 402 µs, where single runs of each ranged
over 20%. These ran before a later change read both superblock slots in one read, which takes one
read off each sync with the option.

`ferro-bench`, median of three alternating runs against the tree before: no figure that does not
wait on fsync was more than 1.4% slower, and several were faster, but `map_diff_per_change`, a read
this change does not touch, at 8.3 and 8.9 µs, whose runs spread from 6.8 to 9.2 µs. A map commit
took 30.3 and 30.1 µs Buffered and 404 and 410 µs with fsync, and a table update with fsync 417 and
442 µs, within what their runs spread over. Seven more alternating runs of the concurrent workload,
eight fsync writers beside readers, gave 9,456 and 9,566 commits a second and 64.0M and 64.3M reads.

The `processes` tool in `ferro-bench` spawns one process per writer, each committing single-key
updates over 100,000 keys from one thread, on `main` or with `--branches own` on a branch of its
own, and then runs the same number of writers as threads of one process. Median of three rounds on
the NVMe drive of the 32-core machine, shared with other work, commits a second:

| Durability | Branches | Closure | Writers | Processes | Threads of one process |
| --- | --- | ---: | ---: | ---: | ---: |
| Fsync | one | none | 1 | 2,418 | 2,381 |
| Fsync | one | none | 2 | 2,669 | 2,741 |
| Fsync | one | none | 4 | 5,359 | 5,268 |
| Fsync | one | none | 8 | 9,963 | 10,292 |
| Fsync | own | none | 2 | 2,618 | 2,628 |
| Fsync | own | none | 4 | 5,165 | 5,053 |
| Fsync | own | none | 8 | 9,752 | 10,455 |
| Buffered | one | none | 1 | 41,383 | 39,719 |
| Buffered | one | none | 2 | 29,231 | 39,639 |
| Buffered | one | none | 4 | 23,386 | 34,154 |
| Buffered | one | none | 8 | 19,805 | 38,232 |
| Buffered | own | none | 2 | 63,780 | 58,643 |
| Buffered | own | none | 4 | 77,106 | 88,848 |
| Buffered | own | none | 8 | 50,086 | 103,432 |
| Buffered | own | 50 µs | 2 | 24,769 | 26,941 |
| Buffered | own | 50 µs | 4 | 46,423 | 45,892 |
| Buffered | own | 50 µs | 8 | 51,352 | 70,118 |

p99 under Fsync was 1.6 ms at 8 processes against 1.0 ms for 8 threads, and p50 within 3%.

Under Fsync the disk is the limit, one fdatasync a round, and processes keep up with threads. Under
Buffered, writers on one branch take turns, and each turn in a process other than the last one's
starts cold: it reads the frame and steps the last writer placed, loads the branch's new root and
entry, and reads back the nodes of its path that the other process rewrote, where a thread finds
all of them in the cache the last writer filled. So two processes commit 0.74 times as fast as two
threads on one branch, and eight 0.52 times. On branches of their own the closures overlap, but
every placement reads, under the placement lock, every frame placed since this process last looked,
and the steps they wrote, and rebuilds the state of every branch it has attached that moved, here
`main` of the first process. That work grows with the processes, and at eight it is the limit,
about half of what eight threads make. With 50 µs of work in each closure four processes keep up
with four threads.

## Measured and left out

- Shared writers by default. On one process the option costs what its locks and reads cost, 2.5 to
  4.5 µs of CPU a booking commit and 10% of single-key Buffered commits a second, measured above,
  and a file opened without it keeps every promise it made before: a writer keeps the file to
  itself, a read-only follower is the only other process that can open it, and nothing in a commit
  takes a lock or reads past the writer's own end. So it stays an option.
- One lock across processes for every closure, in place of a lock per branch, built by giving every
  branch name the same byte. It is simpler, but closures on different branches in different
  processes then run one at a time. With a branch per process, median of three rounds alternating
  the two, before a followed frame's header was read only once, commits a second:

  | Durability | Closure | Writers | One lock | A lock per branch |
  | --- | ---: | ---: | ---: | ---: |
  | Buffered | none | 2 | 38,373 | 57,146 |
  | Buffered | none | 4 | 32,005 | 72,640 |
  | Buffered | none | 8 | 23,284 | 44,840 |
  | Buffered | 50 µs | 2 | 13,020 | 23,809 |
  | Buffered | 50 µs | 4 | 12,106 | 42,306 |
  | Buffered | 50 µs | 8 | 10,558 | 47,390 |
  | Fsync | none | 2 | 2,600 | 2,661 |
  | Fsync | none | 4 | 5,004 | 5,133 |
  | Fsync | none | 8 | 9,746 | 9,347 |

  Under Fsync the disk hides it. Elsewhere only one closure runs at a time across all the
  processes, so with 50 µs closures they make about 13,000 commits a second however many there
  are, and fewer as they add handoffs.
- Closures checked at placement and run again when another process moved their branch. Every write
  takes an `FnOnce` closure, so running one twice would change the API, and a closure on a busy
  branch could run any number of times. Not built.
- Each process syncing for itself, which the first version did. Single-key commits with fsync,
  median of three alternating runs, commits a second:

  | Branches | Writers | Each syncing alone | One sync for all |
  | --- | ---: | ---: | ---: |
  | one | 2 | 2,778 | 2,654 |
  | one | 4 | 3,179 | 5,094 |
  | one | 8 | 3,532 | 9,922 |
  | own | 2 | 2,629 | 2,617 |
  | own | 4 | 3,172 | 5,334 |
  | own | 8 | 3,487 | 9,972 |

  Alone, each process's fdatasync covered little but its own frames, so 8 processes made about a
  quarter more commits than 2, where 8 threads made 3.8 times as many as 2.
- A mark in the file's header naming the last frame placed, which each placer would read and
  rewrite. It saves reading another process's frames to find their end, but costs a write on every
  commit where reading the header at `placed` costs a read, and a process that dies between writing
  its frame and the mark leaves a whole frame for the next one to write over. Not built.
- Writing frames after the placement lock, as within a process. Another process cannot wait for this
  one's `written`, so it would have to poll for frames it can see placed but not yet whole, and a
  sync would have to as well. Writing the frame under the lock costs its check and copy there, about
  1.5 µs for a 2.5 KB frame. Not built.
- A small shared memory file beside the database, as SQLite's WAL mode keeps, for the locks and the
  end of the log. It would make an uncontended lock an atomic instruction, but a mutex in shared
  memory is left locked by a process that dies holding it unless it is robust, which only Linux
  offers, and it is a second file to keep beside the database. Kernel locks are released on death.
  Not built.

## Limits

- All the processes must be on one machine. The file system's page cache is what makes a frame one
  process wrote readable by another, and network file systems give no such guarantee, nor reliable
  locks.
- Each process has its own cache, so on one branch a process reads back the nodes another process
  wrote before its closure can change them.
- A process sees other processes' commits only when it commits, attaches a branch or calls
  `refresh`. One that does none of these holds the file a cycle replaced, as a read-only follower
  does.
- Two shared `Db`s of one file in one process behave as two processes. A closure that writes through
  the other one to the same branch waits for itself, and `Error::Reentrant` cannot see it.
- An open of a Buffered file with live writers scans up to `checkpoint_bytes`.
