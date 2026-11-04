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
| The writer lock, a `flock` | Shared, for the writer's life | Keeping out writers without the option |
| 2^62 + 1 | Shared, for the writer's life | Telling a read-only open that a writer is alive |
| 2^62 + 2, placement | Exclusive, per frame | Catching up, placing a frame and writing it |
| 2^62 + 3, superblock | Exclusive, per sync | Syncing and writing a superblock |
| 2^62 + 4, cycle | Exclusive, per cycle | A reclamation cycle, and the pieces of a large `receive` |
| 2^62 + 2^20 + a 60-bit xxh3 of the branch name | Exclusive, per closure | A branch's closure, across processes |

A cycle's prune and pause, the landing of a `receive` and anything else that replaces every
branch's head lock the whole range of branch bytes at once, as they take every lane within a
process. Locks are taken in the order lane, branch lock, `tip`, placement lock, and the leader of a
sync takes the superblock lock holding none of them. The cycle lock comes before all of them.

The byte-range locks are open file description locks, `fcntl` with `F_OFD_SETLK` and
`F_OFD_SETLKW`, which only Linux has, on 64-bit targets where `off_t` holds the offsets. Classic
POSIX record locks would not do: they belong to the process, so threads of one process never
conflict on them, and closing any descriptor of the file drops all of them, which a read-only handle
or a retired generation does. An open file description lock belongs to the open file, so two shared
`Db`s of one file in one process exclude each other as two processes do, and the tests use that.
The kernel releases them when a process dies, which is what lets the others go on. On every other
target, Windows included, an open with the option fails with an `Unsupported` I/O error, as
described under [Limits](#limits).

### One owner per lock in a process

Every thread of a process locks through the same open file description, and a lock it already holds
is granted again at once, as a no-op, and released by whichever thread unlocks it first. So each
lock has an owner within the process as well. The placement, superblock and cycle locks are taken
through a table of the ranges a thread of this process holds: a thread waits on a condition
variable until no other thread holds the range, takes it from the kernel, and on release unlocks it
in the kernel before it frees the entry. The first version had none. `Tip`, the guard that holds
`tip` and the placement lock, released `tip` first, so a second thread took `tip` and was granted
the placement lock as a no-op, and the first thread's release then unlocked it while the second was
placing. Another process placed a frame at the same offset, which ended in `StalePointer`, hash
mismatches and a file that no longer opened.
`threads_of_two_handles_on_branches_of_their_own_place_in_turn` runs four threads on branches of
their own in each of two handles of one file and failed on its first round. The guard now drops the
placement lock first as well, and the receive's hold on the cycle lock likewise, but neither order
matters any more.

Branch bytes are shared within a process on purpose: two branch names can hash to one byte, and a
process counts how many of its lanes hold each byte and unlocks it when the last one lets go. That
costs only parallelism, except that with nested writes across branches two such names could
deadlock two processes. With 60 bits, a thousand branch names share a byte with odds of about 4 in
10^13. Taking the whole range is exclusive with them: a thread that wants it marks it wanted, so no
thread of the process takes a branch byte, and waits until none holds one before it locks the
range. A thread that holds a byte for a branch attached after the cycle listed its lanes could be
waiting for one of those lanes, so the wait gives up when the set of attached branches changes and
the cycle lists them again. In the first version an awaited write's waiting thread was granted a
branch byte as a no-op inside its own process's cycle, then unlocked it, and another process's
closure ran during the pause and failed with `the file was replaced while a closure held its
branch`. `an_awaited_writes_wait_never_opens_a_branch_inside_a_cycles_pause` reproduces it.

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
closure move its captures into the root. `drop_branch` takes the dropped branch's lock too, so a
drop waits for a closure another process is running on that branch, and a closure that starts after
it gets `Error::NoBranch`, as it does when the drop is in its own process. Without that lock a drop
and a fork of the same name in another process, while a closure ran, left the closure's commit
failing with `Corrupt` (`a_drop_and_fork_in_another_handle_wait_for_a_closure_on_that_branch`).

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

Catching up only rebuilds branches that are attached. A branch attached later takes its state from
the published refs, so attaching first catches up and syncs what is queued, and then attaches only
if the newest head for that name is the published one, and otherwise goes round again. The first
version attached from the published refs alone, so a branch whose newer head another process had
written, and this one had caught up with but not yet synced, attached with the older state and
never moved: every commit on it failed with `another process moved branch`.
`a_branch_attached_while_a_followed_frame_waits_for_its_sync_reads_the_newest_head` holds a commit
on `main` between catching up and syncing while it attaches the branch.

The marker a cycle leaves, described under [Reclamation](#reclamation), is checked against the path,
which a store now keeps canonical. The first version kept the path it was opened by, so a handle
opened by a relative path, after the working directory changed, could not find the file, read the
marker as a cycle that failed, and went on committing to the retired file: an acknowledged commit
was lost (`a_handle_opened_by_a_relative_path_follows_a_cycle_after_the_directory_changes`). It also
treated any error checking the path as no change, and now fails the commit instead.

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

A sync that fails in one process is reported to the others, since Linux reports a writeback error
once to every open file of the inode at its next fsync, so a process whose frame followed the failed
one fails its own sync rather than acknowledging over a hole.

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
syncing. If the test itself fails, it scans, as it would with no writer alive, since scanning beside
a live writer only costs time. Read-only handles otherwise behave as before.

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
- An awaited write takes its branch's lock without waiting. If another process holds it, the
  future returns `Pending` and a thread waits for the lock, lets it go and wakes every future
  waiting on that branch, so no executor thread waits on another process's closure. There is one
  such thread per branch at a time. The first version started one for every future: 32 futures
  queued behind another process's closure took the process from 3 threads to 36, and now take it
  to 5, the waiting thread and the sync thread
  (`awaited_writes_waiting_on_another_process_share_one_waiting_thread`).
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
- A child opens the file by a relative path beside a second handle, changes directory, and commits
  after the other handle's cycle, and that commit must survive a reopen.
- A child counts its threads while 32 awaited writes wait for another handle's closure.

`ferro/tests/shared.rs` opens one file twice in one process, which locks as two processes do, for
what is easier to arrange there: forks and drops seen by the other handle, watchers fired by a
commit and by `refresh`, coalesced and awaited writes, an awaited write held up by the other
handle's closure, a pull landing beside the other handle's commits, background cycles in three
handles at once, four threads on branches of their own in each of two handles, an awaited write
waiting through the other handle's cycle pause, a branch attached between catching up and syncing,
and a drop and fork that must wait for a closure. Each test added for a finding of the review
failed before its fix, as described above. Both files build on 64-bit Linux only, and
`ferro/tests/smoke.rs` checks elsewhere that the option is refused.

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
| Fsync | one | none | 1 | 2,382 | 2,536 |
| Fsync | one | none | 2 | 2,690 | 2,575 |
| Fsync | one | none | 4 | 5,347 | 5,288 |
| Fsync | one | none | 8 | 9,727 | 10,406 |
| Fsync | own | none | 2 | 2,770 | 2,705 |
| Fsync | own | none | 4 | 4,921 | 5,278 |
| Fsync | own | none | 8 | 9,827 | 10,284 |
| Buffered | one | none | 1 | 38,280 | 43,684 |
| Buffered | one | none | 2 | 29,134 | 40,614 |
| Buffered | one | none | 4 | 24,895 | 42,027 |
| Buffered | one | none | 8 | 17,888 | 41,249 |
| Buffered | own | none | 2 | 57,459 | 62,842 |
| Buffered | own | none | 4 | 82,583 | 92,915 |
| Buffered | own | none | 8 | 54,202 | 107,160 |
| Buffered | own | 50 µs | 2 | 23,996 | 25,806 |
| Buffered | own | 50 µs | 4 | 45,038 | 48,941 |
| Buffered | own | 50 µs | 8 | 55,232 | 69,399 |

These ran with the fixes described under [One owner per lock in a
process](#one-owner-per-lock-in-a-process) and after it, and came within the spread of three rounds
run before them. p99 under Fsync was 1.5 ms at 8 processes on one branch against 1.0 ms for 8
threads, and p50 813 against 765 µs.

Under Fsync the disk is the limit, one fdatasync a round, and processes keep up with threads. Under
Buffered, writers on one branch take turns, and each turn in a process other than the last one's
starts cold: it reads the frame and steps the last writer placed, loads the branch's new root and
entry, and reads back the nodes of its path that the other process rewrote, where a thread finds
all of them in the cache the last writer filled. So two processes commit 0.72 times as fast as two
threads on one branch, and eight 0.43 times. On branches of their own the closures overlap, but
every placement reads, under the placement lock, every frame placed since this process last looked,
and the steps they wrote, and rebuilds the state of every branch it has attached that moved, here
`main` of the first process. That work grows with the processes, and at eight it is the limit,
about half of what eight threads make. With 50 µs of work in each closure four processes come within
8% of four threads.

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
- Only 64-bit Linux. Windows has byte-range locks, `LockFileEx`, but a blocking one on a handle
  opened without `FILE_FLAG_OVERLAPPED`, as `std` opens every file, holds up every other I/O call on
  that file object while it waits, so a writer waiting for another process's branch would stall this
  process's readers, and two processes could deadlock on each other's reads. Doing it right means
  a second handle for the locks, or overlapped waits, and none of it can be run here: CI only checks
  that the Windows build compiles, so the option fails with `Unsupported` there until a Windows run
  tests it. macOS and the BSDs have no open file description locks, and 32-bit Linux has an `off_t`
  too narrow for the offsets.
