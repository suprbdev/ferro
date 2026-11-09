# Writers in several processes

How `Options::shared` lets several processes commit to one file: the locks, how a process catches
up with the others, how frames and superblocks are written, how reclamation cycles are shared, and
what was measured and left out (#60). [Followers](#followers) describes how `Options::follow` keeps
a read-only handle or a shared writer up to date with the others on a thread of its own (#84). The
user-facing rules are in [Writers and concurrency](../guide/concurrency.md#several-processes).

## The locks

Without the option one process holds the writer lock for as long as its `Db` is open. A shared
writer takes that lock shared rather than exclusive, so it and a writer without the option, from
this build or an older one, refuse each other with `Error::Locked`, and shared writers do not refuse
each other. Everything else is a byte-range lock past any real file, at 2^62 and up:

| Lock | Held | For |
| --- | --- | --- |
| The writer lock, a `flock` | Shared, for the writer's life | Keeping out writers without the option |
| 2^62 + 1 | Shared, from the end of the writer's open for its life | Telling a read-only open, and a writer that opens, that a writer has opened and is alive |
| 2^62 + 2, placement | Exclusive, per frame, or shared by a read-only open's scan | Catching up, placing a frame and writing it |
| 2^62 + 3, superblock | Exclusive, per sync | Syncing and writing a superblock |
| 2^62 + 4, cycle | Exclusive, per cycle | A reclamation cycle, and the pieces of a large `receive` |
| 2^62 + 5, gate | Exclusive, while a process wants or holds every branch lock; shared for an instant by a writer that found it held | Keeping other processes from taking a new branch lock meanwhile |
| 2^62 + 2^20 + a 60-bit xxh3 of the branch name | Exclusive, per closure | A branch's closure, across processes |

A cycle's prune and pause, the landing of a `receive` and anything else that replaces every
branch's head lock the whole range of branch bytes at once, as they take every lane within a
process, and hold the gate while they wait for it and hold it, as [The gate](#the-gate) describes.
Locks are taken in the order lane, branch lock, `tip`, placement lock, and the leader of a sync
takes the superblock lock holding none of them. A writer passes the gate before its lane, holding
nothing, and the whole range is taken after every lane and the gate. The cycle lock comes before
all of them.

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
lock has an owner within the process as well. The placement, superblock and cycle locks and the
gate are taken through a table of the ranges a thread of this process holds: a thread waits on a
condition variable until no other thread holds the range, takes it from the kernel, and on release
unlocks it in the kernel before it frees the entry. The first version had none.
`Tip`, the guard that holds
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

So the cycle's lanes must cover every branch byte the process can hold for long, and attached
branches are not all of them: a coalesced group keeps its branch's byte between closures, and a
branch can stop being attached under it. A closure that took the byte on a branch another process
had dropped, and caught up, retired the branch and then joined a group on it anyway, and the group
stayed open while a sync ran. The cycle took every attached lane, which no longer included that
branch, and waited for its byte; the group's member was parked until the sync thread sealed it,
and the sync thread was waiting for a lane the cycle held. A stress run hung about once in 25 to 50
runs of 20 rounds on two CPUs. Now a closure checks its branch is live again after taking the byte
and catching up, and lets the byte go and fails with `Error::NoBranch` if it is not, before it
runs (`a_closure_on_a_branch_another_handle_dropped_never_runs`), and a cycle also takes the lane
of every branch with a group open, attached or not, which seals the group.

### The gate

The kernel grants a lock on the whole range only at a moment when every byte of it is free, and it
holds back no byte that another process asks for while the range is wanted: a waiting
`F_OFD_SETLKW` is woken when a lock in its way goes, tries again, and sleeps again if another byte
is held by then. Within a process the `whole` flag keeps new takers out, but across processes
nothing did, and a writer holds its branch's byte for nearly all of each commit, 93% of the time
in the measurements under [Reading back what other processes
wrote](#reading-back-what-other-processes-wrote). So writers in other processes, each on a branch
of its own, kept the range from ever being free at once. With four handles of one file committing
on branches of their own, a cycle reached its pause 0.28 s after it began and then waited 10.7 s,
until the writers were stopped, and the same with four processes; a pull's landing and a prune
waited the same way. The cycling process holds every one of its own lanes while it waits, so its
own writers waited as long (#128).

So a process that wants every branch lock takes the gate, a byte of its own, exclusive, once its own
threads hold no branch byte, and only then the range, and it lets the gate go with the range. A
writer about to take its branch's lane, holding no lock, tests the gate with `F_OFD_GETLK` for a
shared lock, which reports only an exclusive lock another open file holds, and goes on if there is
none. If there is, it takes the gate shared, which the kernel grants once the holder lets it go,
and lets it go at once. Once a process holds the gate, no other process takes a branch byte it did
not hold or had not set out to take before, and the range comes free as the closures holding bytes
end: those that held one when the gate was taken, and those whose writers had already tested the
gate and wait for their lane or their byte. Each runs once, and tests the gate again before its
next. A writer that finds the gate free never holds it, so the process taking the gate waits only
for another that wants every branch lock and for writers that found the gate held, each holding
it shared for an instant once its holder let it go. Nothing in it depends on the kernel being fair.

A thread that holds any lock goes straight to its byte, as before: a nested write to a branch that
sorts after the closure's own, a write into another file from inside a closure, or anything under
the whole-file record. The process holding the gate may be waiting for that thread's byte, and the
nested write runs inside the closure that holds it, so it is bounded by that closure. The
reentrancy record of each thread, described in [Writes from inside a closure](reentrancy.md), says
whether it holds a lock. A writer waits at the gate before it takes its lane, never holding one:
a writer waiting there with the lane of `b` would hold up a closure on `a` that writes to `b`, and
the gate's holder would be waiting for the byte of `a`. An awaited write that finds the gate held
returns `Pending` as when its byte is held, and the thread that waits for the byte waits at the
gate first, unless the write came from a thread holding a lock, which gets a waiting thread of its
own that does not. The sync thread, the seal of a group and a cycle's own lanes take no new branch
byte and never pass the gate. Within a process the thread that takes the gate and those that wait
at it go through the table of [One owner per lock in a
process](#one-owner-per-lock-in-a-process): a shared lock taken through the open file description
that holds the gate exclusive would turn it shared, and its release would let it go.

The gate keeps nothing in the file, a process that dies holding it lets it go with its other locks,
and a test of it that fails lets the writer on, since it only keeps the range from waiting for
ever. A shared writer of an earlier build pays it no heed, so beside one the range can still wait as
long as it did. `a_cycle_finishes_while_other_handles_commit_on_branches_of_their_own` in
`ferro/tests/shared.rs` runs five cycles beside four handles committing on branches of their own,
and `a_handles_own_commits_wait_for_its_cycle_only_while_other_handles_finish_theirs` makes 50
commits on a branch of a handle that cycles over and over; both waited past 20 s without the gate.
`nested_and_awaited_writes_go_on_through_another_handles_cycles` runs twenty cycles beside
blocking and awaited writes from a closure on `a` into `b`, writers on `b` in the same handle and
writers on branches of their own in others, and had not finished them after 60 s without the
gate.
`a_cycle_finishes_while_other_processes_commit_on_branches_of_their_own` in
`ferro/tests/shared_process.rs` does it with four processes, or as many as `FERRO_SHARED_OWN`
says: in a debug build the five cycles took 5 to 8 ms each beside 4 processes and 10 to 12 ms beside
16, where without the gate, beside 4, the first two took 589 and 5 ms and the third had not
finished after 20 s.

The test costs a writer one `fcntl` a commit. On 2026-11-09, pinned to eight cores of the
32-thread machine while other work ran on the rest, alternating with the build before, under
Buffered: `group` with one shared writer made 44,445 single-key commits a second against 45,277,
median of five rounds, 1.8% fewer, and with eight threads 38,160 against 37,993. In `processes`,
median of the two of three rounds the other work left alone, 2, 4 and 8 processes on one branch
made 35,241, 29,757 and 24,646 commits a second against 35,776, 29,829 and 24,644, and on
branches of their own 66,422, 101,017 and 59,471 against 67,596, 101,061 and 59,540; the same
writers as threads of one process moved by at most 1.7% either way.

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
it, then the next, until it finds one that is not. It reads a header's 68 bytes and the 16 after
them, where the first record of a frame that spilled would start, so a process that is alone reads
84 bytes, a reservation of zeroes or the end of the file, each time it looks. A frame of up to 4 MiB
is read in one read together with the 84 bytes after it, which hold the next header, and checked
in memory, so following _n_ frames takes _n_ + 1 reads; a larger frame, or one that does not check
in memory, is read again from the file and judged there, as below. In this mode a frame is written
before the placement lock is released, so a frame that a live process placed is whole whenever
another can take that lock, and one that a process left half-written when it died fails its check
and ends the log there. A process killed in a write stops at a page boundary, so the rest
of its frame keeps its older bytes and reads as torn. A catch-up under the placement lock that finds
anything but zeroes in those 84 bytes past the last whole frame, part of a dead process's frame or
the records of one that spilled before its header was written, truncates the file there and syncs
before the next frame is written in its place, as recovery at open does, so that a power cut cannot
leave a sector of the next frame holding the dead one's bytes, which can be a bit from it. It does
the same when a stale cycle marker, one the path still names, has anything but zeroes in the 16
bytes past its 68. A frame that reached the disk whole and was damaged since, one bit from checking
as [Torn and damaged frames](storage.md#torn-and-damaged-frames) describes, fails a catch-up under
the placement lock with `Error::Corrupt`, so no commit is written over it. A catch-up without that
lock, on taking a branch lock, at `refresh` or before a cycle's pass, may meet a frame another
process is still writing, so it takes any frame that does not check for the end of the log, and the
catch-up under the placement lock before the next frame is placed judges it.

Nothing at or below the newest superblock is ever truncated or written over. A catch-up under the
placement lock starts from this process's `placed`, which can lie many frames behind the newest
superblock when the process has been idle, so it also reads both superblock slots and takes the
newest. Every frame that superblock names was whole in the page cache when it was named, and no
crash can have taken one back while this process is alive, so if the frames it follows stop before
the end of the log that superblock names, the frame where they stop was damaged since, however the
judgement reads it, and the catch-up fails with `Error::Corrupt` and leaves the file as it is, under
either durability. Only past that log does it judge a frame torn or truncate anything, as
[What a superblock names](storage.md#what-a-superblock-names) describes. Its own frames are placed
where the catch-up ended, so none is ever written below the newest superblock either. The read costs
about 0.9 µs of CPU on each placement, measured there. The first version judged from `placed` alone:
an idle handle's commit after another had committed four frames, with two bits flipped in the
second, read it as torn, truncated it and the two after it, synced, and acknowledged its own frame
in their place, and the other handle's next commit failed reading past the end of the file, and no
open succeeded after (#82).

The frames' head pointers name [steps](commit-path.md#branch-table-steps) on the table each process
already holds, so loading the new refs reads only the steps since the head this process last knew,
as a read-only `refresh` does. For every branch attached in this process whose head moved, it loads
the new state into the branch's tip, and a branch that is gone is retired. So is one whose name now
has a head with another `born`, the number taken when the branch was created: another process
dropped it and forked the name again before this one caught up, and the table this process loads
has only the newest of those frames' heads, as [A branch of the same
name](history.md#a-branch-of-the-same-name) describes. The first version took that head for the
old branch moved, so a write through an old handle landed on the new branch, where a process that
caught up between the drop and the fork failed it with `Error::NoBranch`. The frames go into the
queue as one entry, so the next sync
publishes those states with the process's own commits, in frame order, and fires their watchers.
Nothing another process wrote is published here before it is durable, by that process's sync or by
this one's.

Loading a state decodes its root as `T`, which runs the user's `Data::decode` under `tip`, so a
catch-up that adopts a frame or follows a cycle's marker first records the whole file, as [Writes
from inside a closure](reentrancy.md) describes, and a write, `refresh` or `Db::branch` from that
decode fails with `Error::Reentrant`. The first version recorded nothing there, and a write took
`tip` with only its lane recorded, so a `refresh`, a `Db::branch` or a write to a branch that sorts
after the lane's, made from the decode, passed the check and waited for `tip` for good (#105). The
states a catch-up replaces, and those of a branch it retires, go into notices its caller drops once
it has let `tip` and its lanes go, so a `Drop` among them runs outside the locks. A catch-up that
finds nothing records nothing, so a process alone pays nothing for either.

The bytes of the frames a catch-up followed and checked in memory, up to 4 MiB of them, stay in
memory until a later catch-up keeps the frames it follows, a catch-up truncates the file or the
store closes, and a read of an object in them takes its record from there rather than from the file:
the steps, the new root and log entry of each branch that moved, and the nodes of those frames that
the next closure reads back. A read loads the frames through an `ArcSwapOption`, so reads on many
threads take no lock and change no shared count, tests its offset against where the first frame
starts and the last ends, and finds its frame by a binary search. Every such read still hashes the
object against the pointer it came from, as a read from the file does. The bytes are those the
catch-up checked, and a frame that checks stays as it is in the file for as long as the file is in
use, since nothing is written below the end of the log a live process has placed and only bytes past
the last whole frame are ever truncated. A damaged copy on the disk is found by the next read from
the file, once a later catch-up has replaced the bytes held.

The first form kept them behind a mutex that every read of a shared writer took, found a read's
frame by a scan, and let the oldest frame go by removing it from the front of a `Vec`, which made a
long catch-up quadratic. In probes kept out of the tree, median of three alternating runs, a
follower catching up with 100,000 frames of one object each took 791 ms where it takes 17 ms with
8-byte objects, 228 and 21 ms with 300-byte ones and 89 and 51 ms with 2.3 KB ones, and each read of
an 8-byte object after it took 15.3 µs where it takes 0.25 µs. Sixteen threads reading 100,000
records of 200 bytes each from outside the kept frames took 1.8 s, in runs from 0.25 to 1.9 s, where
they take 115 ms, and four threads 68 and 55 ms, while one thread took 53 and 52 ms. Reading 50,000
each from inside some 14,000 kept frames, one thread took 166 ms where it takes 20 ms and sixteen
threads 726 where they take 29.

Catching up happens when a process takes a branch lock, before it places a frame, on each
`Db::branch`, `Db::have`, `Db::bundle` and `Db::receive`, before each pass of a cycle and on
`refresh`, and on a look of a [follower](#followers) that finds a superblock naming a frame past its
own. A process that does none of these sees nothing new, as a read-only handle that never refreshes
sees nothing new.

Catching up only rebuilds branches that are attached. A branch attached later takes its state from
the published refs, so attaching first catches up and syncs what is queued, and then attaches only
if the newest head for that name is the published one, and otherwise goes round again. The first
version attached from the published refs alone, so a branch whose newer head another process had
written, and this one had caught up with but not yet synced, attached with the older state and
never moved: every commit on it failed with `another process moved branch`.
`a_branch_attached_while_a_followed_frame_waits_for_its_sync_reads_the_newest_head` holds a commit
on `main` between catching up and syncing while it attaches the branch. `Db::branch` catches up
even for a branch this process has attached, so that one another process dropped, or dropped and
forked again, is retired first: the first version handed back the attached branch as it was, so
`branch("x")` after a drop gave the dead handle, and after a drop and fork the old branch rather
than the new one. `branch_after_another_handle_dropped_and_forked_again_gives_the_new_branch`
covers both.

The marker a cycle leaves, described under [Reclamation](#reclamation), is checked against the path,
which a shared writer's store keeps canonical; other stores keep the path they were given, as
before. The first version kept the path it was opened by, so a handle
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

If another writer that has opened is alive, which the lock at 2^62 + 1 tells, no crash can have come
since that writer opened, so the scan from the anchor must reach the end of the log the newest
superblock names, under either durability, or the open fails with `Error::Corrupt` and writes
nothing. Under Fsync the newest superblock anchors itself and the scan starts there, but under
Buffered the anchor can lie far behind, and a frame between them that does not check was damaged
since, not torn. A writer takes that lock only once it has recovered, still holding the placement
lock, so two writers that open after a power cut never take each other for live ones: the first to
recover may roll a Buffered log back past what its superblock names, and the other then finds the
log that one left. Before, an open beside a live Buffered writer that met a frame with two bits
flipped in one block rolled the log back to the frame before it, as after a crash, truncating the
live writer's acknowledged commits from under it, and the live writer's next commit failed reading
past the end of the file (#82). Checking the lock while it was still taken with the writer lock,
before recovery, would have failed one of two writers that open together after a power cut on a
Buffered log the crash cut short. A shared writer of an earlier build still takes it that way, so
beside one of those that is recovering after a power cut, a writer of this build takes it for a live
one, and on a Buffered log the crash cut short its open fails with `Error::Corrupt` and writes
nothing; opening again once that writer has closed succeeds. The format does not tell the builds
apart, so processes that share a file should run the same build.

Once it holds the placement lock it checks again that the path names the file it opened, and opens
the path again if not. A cycle renames the new file over the path under the old file's placement
lock, so a file still named by the path once that lock is held is one no cycle has retired. The
first version checked the path only before taking the lock. A writer that opened the path during
another process's cycle pause got the old file's placement lock once the cycle had renamed and let
go, recovered the retired file, and truncated the marker the cycle had left at its end as anything
past the last frame, so it never learned of the new file and acknowledged commits into one that no
longer had a name. A process that ran two cycles before another followed did the same to the file
in the middle. `a_writer_that_opens_during_another_handles_cycle_pause_writes_the_new_file` holds a
cycle at its pause while another handle opens, and lost the commit that handle acknowledged. Once
two processes were on different files, both could run a cycle, each into the same `.compacting`
and each renaming over the other's file, which is the likeliest way a stress run ended in `varint
overflow in record`.

Recovery truncates everything past the last frame, a marker included, as it always has. A marker is
only ever on a retired file or, left by a cycle that died or failed before its rename, at the end of
a live one, and the check under the placement lock keeps recovery off retired files, so the marker
it truncates is always a stale one. For a while recovery by a shared writer kept a marker as a
second guard, and that broke the promise under [File format and
durability](storage.md#file-format-and-durability) that a frame stranded past a torn one can never
line up with the log later. Frames written over a stale marker, then a power cut that kept the
second frame but not the page with the first one's header, left the marker in front of a whole
frame, and recovery stopped at the marker and kept the frame. The next frame of the same length
then lined it up again, linking into what had been rewritten, and the file failed to open with a
hash mismatch. `a_marker_left_by_a_dead_cycle_never_keeps_a_stranded_frame_after_a_power_cut` writes
a marker, writes two frames over it, puts the marker and the superblocks back as such a power cut
would, and failed that way while the marker was kept.

A shared writer does not truncate the Fsync reservation when it closes, since other writers may be
placing frames into it. The next writer to open truncates it, so the file can stay up to 4 MiB
longer than `size_on_disk` while no shared writer has it open.

A read-only open that can take the writer lock shared takes the placement lock shared too, and
trusts the superblock if another holds it, then tests the lock that shared writers hold once they
have opened, and treats a held one as a live writer, trusting the superblock instead of scanning and
syncing. If either test fails with an error rather than finding the lock held, it scans, as it
would with no writer alive, without the placement lock when that was the test that failed. It holds
the placement lock for the scan when it has it, so a shared writer that opens meanwhile waits for it
before it recovers or writes, and the scan, which judges a frame that does not check as damaged or
torn, never reads one being written. Without it, a shared writer that opens meanwhile can recover
and place frames during the scan, so the scan may meet one being written. The first version of the
placement lock trusted the superblock when its test failed with any error, so a reader whose lock
failed with `ENOLCK` missed a commit whose superblock write a crash had lost (#82). Read-only
handles otherwise behave as before.

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
does. It retires a branch whose name is gone from the new file or has a head with another `born`
there, since the frames of the old file it had not read and those of any generation between are not
read at all. If the path still names this file, the cycle failed before its rename and the marker is
written over, or, with anything but zeroes past it, truncated with what follows, as described under
[Catching up](#catching-up). Every frame placed in the old file before the marker was copied into
the new one, since the pause caught up under the placement lock and the marker went in under it, and
no process can place in the old file after the marker without first finding it. Recovery reads the
marker as the end of the log, so a crash between the marker and the rename leaves the old file as it
was.

## Watchers, coalescing, awaited writes and receive

- Watchers fire for another process's commits when this process catches up and syncs them: at its
  next commit on any branch, at `refresh`, when it attaches a branch, or when its follower finds a
  superblock naming them, as described under [Followers](#followers).
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

## Followers

`Options::follow` gives a read-only handle or a shared writer a thread, `ferro-follow`, that keeps
it up to date with what other processes commit, so `read` stays current and watchers fire without
`refresh` (#84). Every handle of one open shares the thread, which the `Keeper` owns beside the
reclamation thread. A writer without `shared` is the only writer and starts none, and a handle
opened without the option has no thread, and nothing in its commits, reads or `refresh` changes.

### A look

The thread looks at the file whenever it may have changed. On a read-only handle a look is a
`refresh`, which reads both superblocks and stats the path under `tip` and loads only what moved.
On a shared writer a look first stats the path and reads both superblock slots in one read, as a
sync does, without `tip`: only if the path names another file, or the newest superblock names a
frame past the last one this process placed or followed, does it catch up. A look that finds
nothing changes nothing and costs a few microseconds on a core that is awake. The first version
made a read-only look the same check, against the store's last sequence number. A refresh that
failed on a damaged object of the new commit, after `Store::refresh` had moved that number, then
left the next look finding nothing, so the follower cleared the error and stayed at the old
version, where `refresh` keeps failing until the damage goes
(`a_follower_whose_refresh_fails_keeps_trying_and_reporting_until_it_succeeds`). A shared writer
moves the number only once it has loaded what it followed.

A shared writer's catch-up reads on from `placed`, and a frame another process placed may not be
synced yet. A catch-up that followed one would sync it before publishing, as `refresh` on a shared
writer does: an fdatasync under the superblock lock on behalf of the other process, and watchers
that fire before that process has acknowledged its commit. So a look on a shared writer catches up
only to the frame the superblock it read names. The frames up to it were whole when they were
named, and a frame among them that does not check fails the catch-up with `Error::Corrupt`, as
under the placement lock, rather than end the log there, since no crash can have taken it back
while this process is alive.

The sync that publishes them has nothing to write either: the look notes in the store how far the
superblock it read covers, by anchor under Fsync and by name under Buffered, and a shared sync of a
frame up to there returns as the locked path would, without the superblock lock, since superblocks
only grow while this process is alive. Under Buffered the locked path could still have written a
superblock carrying this process's newer anchor; one carrying an older anchor only makes a
recovery scan from further back. The first version took the superblock lock for that sync, so a
look, and a drop waiting for it, waited as long as another process held the lock through a slow
fdatasync: `dropping_a_shared_follower_waits_for_no_other_handles_superblock_lock` holds one
handle's fdatasync under that lock, and the follower did not publish the commit a superblock had
already named, nor did its drop return, until the fdatasync did. So a look that finds a new
superblock takes no lock another process can hold. A look still leads a real sync, under the
superblock lock, when this process's own commits have queued frames past what the superblock
covers, as any leader would, and a look that meets a cycle's marker opens the new file as a shared
writer's open does, under that file's placement and superblock locks. Under Fsync a commit that a
Buffered process named is past the anchor the superblock covers, so a Fsync look that follows it
syncs under the superblock lock and waits for whoever holds it. A Buffered follower runs the
checkpoint after each look that led, as any Buffered writer does, so it syncs the file once every
`checkpoint_bytes` of other processes' commits. A look never takes a branch lock.

### Waking

On 64-bit Linux the thread waits in `ppoll` on an inotify instance and an eventfd. The instance
watches the file for `IN_MODIFY` and its directory for a file created, removed or renamed at the
file's name. A commit wakes it once for its frame and once for its superblock, and the first look
usually finds nothing yet. Events for other names in the directory, such as the `.compacting` copy
a cycle writes, are read and dropped without a look. When the name moves to another file, as at a
cycle's rename, the file's watch moves to the new file and the old one is removed, so no watch
outlives the generation it was on. A queue overflow counts as a change. The thread also looks once
a second whatever happens, for a file system that does not report another process's writes.

After a look that refreshed, the thread waits until a millisecond after that look began before it
looks again, and events meanwhile stay queued for that next look. Without the wait, a writer
committing as fast as it can kept the thread refreshing for every commit it could reach: in
`ferro-bench --bin follow`, with Buffered single-key commits from another process for five
seconds, the follower refreshed 408,649 times for the writer's 415,355 commits and used 3.26 s of
CPU, 65% of a core, and the writer made 9% fewer commits than with no follower. With it, the
follower refreshes about 960 times a second. A look that found nothing does not wait, so the look
for a commit's superblock is not held up by the one for its frame. The first version waited after
every look, so the look for a commit's frame held up the one for its superblock by up to a
millisecond: a commit reached a watcher at a median of 673 µs after the writer's closure began,
470 µs more than the commit took, where without that wait it took about 90 µs more.

Where inotify cannot be had, on every other target or when `inotify_init1` or a watch fails, as
once the user's `max_user_instances`, 128 by default, are in use, the thread looks every 10 ms,
waiting on a condvar. `Following::notified` says which it does. A watch that fails later on falls
back to looking every 10 ms, and the failure is reported in `last_error`.

### When it ends

The thread holds the handle's `Shared` by a `Weak` and upgrades it only for the length of a look, so
while it waits it keeps neither the file nor a lock, and a watcher's promise not to keep the file
open still holds. The last `Db` handle to drop drops the `Keeper`, which sets the stop flag, rings
the eventfd or wakes the condvar, and joins the thread before it stops the reclamation and sync
threads. A look in progress finishes first, and if it held the last count of `Shared`, the file
closes on the follower thread before the join returns, so either way the file is closed and the
inotify instance gone when the drop returns. So a drop waits for a look in progress. That is a few
microseconds for one that finds nothing and the length of a refresh for one that finds something; a
look that opens the file a cycle renamed waits there for that file's placement and superblock locks,
as an open would, and the drop waits with it. The drop of a Buffered shared writer already takes the
superblock lock to write its last anchor. A look that finds `Shared` gone ends the thread too. The
thread runs no closure and no watcher test, only what `refresh` runs: decoding the new states and
dropping the ones they replace. A panic there is caught and recorded, and the thread goes on.

### Errors

A look that fails changes nothing: `read` shows what it showed, watchers go on waiting, and the
thread looks again at the next change or interval. `Following::last_error` keeps the error as text
until a look succeeds, as `Space::last_error` keeps a cycle's. Ending the watchers instead would
end every subscriber over a fault that may pass, such as a file moved away and back, and a watcher
cannot carry the error.

- A path that names nothing fails with `Error::Io`. Reads go on from the file the handle has open,
  and when a file appears at the name again, created or renamed there, the directory's watch wakes
  the thread and the refresh opens it, as it opens a file a cycle renamed there.
- Superblocks that are both invalid fail with `Error::Corrupt`, and so do a damaged branch table or
  root on a read-only handle and, on a shared writer, a frame a superblock names that does not
  check.
- `Error::Replaced` never reaches it: `refresh` opens the new file when it meets that, and a shared
  writer's catch-up follows the cycle's marker to it.
- A shared writer whose write or sync failed is poisoned until it is reopened. Its catch-up then
  fails with `Error::Poisoned` before it reads, decodes or queues anything, at a look and at
  `refresh` alike, and the follower records the error and stops, so it stays. In the first version
  a poisoned writer's catch-up still adopted each frame another process named, decoded its states
  and queued it behind the failed sync, which never drained, and the follower's next look found
  nothing new and cleared the error. With 300 commits of a 256 KiB root from another handle, a
  poisoned follower grew by 75 MiB and a poisoned handle that called `refresh` after each by
  76 MiB, where a healthy follower grew by 6 MiB; with the check neither grows.
  `a_poisoned_following_shared_writer_follows_nothing_more_and_keeps_its_error` counts the frames
  it adopts after the failure, which must be none.

### Reentrancy

The thread holds no lane and runs no user closure, so nothing it could wait for waits for it. A look
takes `tip`, as `refresh` does, and on a shared writer the superblock lock only when it leads a sync
of this process's own frames or opens a file a cycle renamed, holding nothing else. A watcher test
runs on the `ferro-watch` thread holding nothing and may refresh while the follower does: they take
`tip` one after the other. A closure may refresh too, as before, since `refresh` needs only `tip`,
which a look holds briefly and never while it waits for a lane. [Writes from inside a
closure](reentrancy.md) has the rules.

### What following costs

`ferro-bench --bin follow` runs a follower in one process against a writer in another, on the
32-thread machine of the other figures here, pinned to the 16 hardware threads whose siblings
another job was using. The writer makes Buffered single-key commits. Latency runs from the moment
the writer's commit returned to a watcher's receipt, over 300 commits a random 0 to 40 ms apart;
idle is a minute with no writer; busy is five seconds of the writer committing as fast as it can,
with no follower as the baseline. Medians of three rounds, with a read-only follower beside a plain
writer:

| Follower | p50 after the ack | p99 | Skipped | Idle CPU a minute | Idle wakes a minute | Writer's busy commits | Follower's busy CPU | Busy refreshes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| None | | | | | | 478,260 | | |
| inotify | 169 µs | 926 µs | 1 | 3.0 ms | 59 | 478,565 | 153 ms | 4,808 |
| Every 1 ms | 579 µs | 1.38 ms | 3 | 140 ms | 59,706 | 487,703 | 114 ms | 4,702 |
| Every 10 ms | 4.96 ms | 10.5 ms | 35 | 254 ms | 5,920 | 476,084 | 25 ms | 487 |
| Every 100 ms | 11.0 ms | 35.6 ms | 238 | 42 ms | 598 | 469,164 | 9 ms | 51 |

And with both the follower and the writer shared writers:

| Follower | p50 after the ack | p99 | Skipped | Idle CPU a minute | Idle wakes a minute | Writer's busy commits | Follower's busy CPU | Busy refreshes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| None | | | | | | 394,418 | | |
| inotify | 116 µs | 762 µs | 1 | 4.1 ms | 59 | 370,824 | 306 ms | 4,869 |
| Every 10 ms | 5.04 ms | 10.7 ms | 35 | 274 ms | 5,917 | 346,098 | 251 ms | 471 |

A watcher skips a commit when a later one landed before it looked, and only the commits it handed
out are timed, the newest in each wait, so the figures for 100 ms understate the 50 ms a commit
waits on average there.

- Woken by inotify, a follower delivers in a fraction of a millisecond and an idle one costs
  nothing measurable: one look a second. Its median swung from 32 to 460 µs between rounds with the
  load on the sibling threads, since two wakes of cores that may be asleep, the follower's and the
  watcher's, make up most of it.
- An interval trades latency against wakes, and the wakes cost more than the looks: every 10 ms an
  idle follower used 80 to 311 ms a minute across the rounds, 13 to 53 µs a wake, where every 1 ms
  the core never slept deeply and a wake cost 2.3 µs, though it woke a thousand times a second.
- Under the busy writer the read-only follower refreshed once a millisecond, 32 µs a refresh, and
  looking every 10 ms took 25 ms. The writer's median was the same with an inotify follower as with
  none. Over these rounds and three of an earlier run, five of the six with inotify fell within the
  462,214 to 496,358 commits of the rounds with no follower and one, at 427,054, below them, so the
  `IN_MODIFY` event that each of the writer's writes queues costs it little if anything. Shared
  writers' rounds spread from 336,980 to 412,934 whatever followed.
- A shared writer's catch-up reads every frame placed since its last look, where a read-only
  refresh reads the newest head and the branch-table steps since the one it knew. So a shared
  follower took 63 µs a refresh at once a millisecond, and 530 µs when it looked every 10 ms and
  read ten milliseconds of frames each time: a longer interval saves it little under load. These
  rounds ran while a look still took the superblock lock to publish; three more once it no longer
  did gave 169 µs at the median after the ack, and 268 ms of CPU for 4,841 busy refreshes, 55 µs
  each.

`examples/shop` ran its replicas on a thread that called `refresh` every 5 ms and now opens them
with `follow`. Its `shop-load replica-lag`, three runs of 300 commits against a Buffered writer,
measured a replica reaching each commit 0.06 to 0.14 ms after the writer acknowledged it, at the
median, where the timer took 0.88 to 0.97 ms, and that only because the tool's own 3 ms pause
between commits fell in step with it.

So a follower waits on inotify where it can, looks every 10 ms elsewhere, looks once a second
besides, and waits a millisecond after a look that refreshed.

### Left out of followers

- Looking at an interval only, measured above. The cost is in the wakes, whatever the look does: a
  thread that only slept 10 ms and then stat a file used 55.7 µs of CPU a wake, 0.54% of a core,
  while its core was otherwise idle, and 1.1 µs a wake when it slept 1 ms. It is kept for targets
  without inotify.
- Watching only the directory, which reports writes to the files in it by name and needs no watch
  moved at a cycle's rename. Every write to any file beside the database, such as a log, would then
  wake the follower, where the second watch costs one more call at each rename.
- No wait after a look that refreshed: 65% of a core under a busy writer, as measured above.
- A `Db` method that starts following and returns a guard that stops it when dropped. It would let
  a library follow a handle it was given, but the thread would then have two owners, a guard
  dropped at once would follow nothing, and two guards on one file would need a rule. Following is
  a property of how a handle meets the file, like `read_only` and `shared`, so it is an option.
- Watchers that refresh while they wait. `read` would stay stale while no watcher waited, each
  waiting watcher, or the watch thread of each branch, would look on its own, and an async waiter
  would need a timer.
- One inotify instance and thread for every follower in a process. It would save instances past
  128 followed opens, at the price of one thread's lifetime shared by unrelated files. Not built.
- kqueue on macOS and the BSDs and `ReadDirectoryChangesW` on Windows: none of them can run here, so
  those targets poll.

### Tests of followers

`ferro/tests/follow.rs` opens a writer and a read-only follower of the same file, and runs each test
once woken by inotify and once looking every 2 ms:

- Watchers, filtered or not, fire for another handle's commits without `refresh`, and a handle on
  another branch shares the follower.
- The follower follows three cycles that rename a new file over the path, and reads the new file.
- A follower that finds nothing new refreshes nothing.
- With the file removed, the follower reports `Error::Io`, `refresh` fails the same way, and reads
  keep the last version; a file renamed back with one more commit is followed, and the error clears.
- Both superblocks damaged are reported as `Error::Corrupt`, and nothing moves.
- A refresh that fails on a damaged object of a new commit goes on failing and being reported, look
  after look, until the byte is put back.
- A filtered watcher whose test refreshes and attaches a branch runs while the follower publishes
  50 commits.
- A child process counts its descriptors on the file and its inotify and eventfd descriptors as soon
  as it has dropped every handle of a follower, inotify and polling, with a watcher left waiting,
  which must have ended, and then its `ferro-follow` threads; it does the same for a shared writer
  that follows, after which a writer without `shared` must open at once. It also drops a shared
  follower while a hook holds its look for 300 ms, and the drop must take that long and leave
  nothing open; a drop that did not join the thread returned at once with the watch still open.

`ferro/tests/shared.rs` has a shared writer follow another handle's commits and three cycles
without committing. It holds one handle between placing a frame and syncing it while a second
handle places the next, so the newest superblock names only the first: the follower must publish
the first once its superblock is written and the second only once the second handle syncs, where a
catch-up past the superblock would sync and publish both. It also damages the middle of three
frames that superblocks name while the follower is held in a sync, and the follower must report
frame 3 with `Error::Corrupt` and follow all of them once the byte is put back. In
`ferro/tests/shared_process.rs` a child process follows as a shared writer, without committing,
while the parent makes 60 commits and three cycles, and must see every one.

`ferro/tests/follow_fault.rs` intercepts `fdatasync`. It fails one sync of a shared writer that
follows, then has another handle make 100 commits of a 64 KiB root: the poisoned writer's
`refresh` fails with `Error::Poisoned` each time, its follower keeps that error and stops looking,
and a hook counts no frame adopted after the failure. It also holds one handle's fdatasync under
the superblock lock: a shared follower must still publish the commit a superblock already names,
and its drop must return while the lock is held.

## Crashes and killed processes

A process killed anywhere in a commit holds up the others only until the kernel releases its locks.
Killed while it holds a branch lock, its closure is lost. Killed between placing and writing its
frame, it wrote nothing, and the next frame goes where it would have. Killed while writing it, the
frame stops at a page boundary, fails its check and is written over. Killed after writing it and
before syncing, the frame is whole and stays in the log, so its commit may appear though it was
never acknowledged, as after a crash in one process. Power cuts are unchanged: frames that follow
each other from the anchor survive, and every process's acknowledged commits were synced before
they were acknowledged.

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
  go on. In a third, a process cycling every three commits is killed at a random moment eight times
  beside two writers, under Buffered.
- A process runs five cycles while four others commit on branches of their own, as
  [The gate](#the-gate) describes.
- A child opens the file by a relative path beside a second handle, changes directory, and commits
  after the other handle's cycle, and that commit must survive a reopen.
- A child counts its threads while 32 awaited writes wait for another handle's closure.
- A child runs a stress mix, adapted from the review's: two handles of one file, one coalesced, each
  with three branches of its own written by a thread that awaits every third commit and checks that
  each commit acknowledged the version after the last, writers on `main` in both, background cycles
  every 128 KiB with `Keep::Last(3)`, one handle forking and dropping four names in a loop and the
  other attaching and committing on them. After each round the file is reopened and every branch
  and every acknowledged `main` commit must be there, and the file must verify. It runs 40 rounds of
  400 commits a branch in about two seconds, `FERRO_TEST_SCALE` multiplies the rounds and
  `FERRO_SHARED_EACH` sets the commits, and the parent kills it and fails if it does not finish.
  Run on two CPUs, `taskset -c 0,1`, 20 rounds a run, the build before the fixes for opening
  during a pause and for groups on dropped branches went bad in 12 of 200 runs: 6 hung and 6
  failed, three of those with acknowledged commits missing after the reopen. With the fixes, 600
  runs, 12,000 rounds, ran clean, and so did 150 runs of 20 rounds of the review's own mix.

`ferro/tests/shared.rs` opens one file twice in one process, which locks as two processes do, for
what is easier to arrange there: forks and drops seen by the other handle, watchers fired by a
commit and by `refresh`, coalesced and awaited writes, an awaited write held up by the other
handle's closure, a pull landing beside the other handle's commits, background cycles in three
handles at once, four threads on branches of their own in each of two handles, an awaited write
waiting through the other handle's cycle pause, a branch attached between catching up and syncing, a
drop and fork that must wait for a closure, a branch the other handle dropped and forked again, with
and without a cycle between, whose old handles must stay dead, a writer opening during the other
handle's cycle pause, a closure on a branch the other handle dropped, a stale marker that a power
cut leaves in front of a whole frame, and a frame a dead writer left cut at a page boundary, which
the next writer writes over, or whole with a bit flipped, which it reports. It also has an idle
handle commit after the other committed four frames, with two bits flipped in the second's first
block, two in its magic, or its first 84 bytes zeroed, under both durabilities, and opens a handle
beside a live Buffered one over the same frames; both must fail with `Error::Corrupt`, change
nothing in the file, and leave the live handle's next commit and a reopen with every acknowledged
commit. And it leaves a dead writer's near copy behind a stale marker, which the next writer must
truncate and sync before it writes. A read-only and a shared follower each hold a snapshot and a
half-read iterator across three cycles of another handle and read them to the end.
Three handles with different `Keep`s fork, drop, merge, restore, pull and make stamped commits on
a few shared names while one of them cycles, and must meet only the errors those calls promise and
leave a file that passes `verify_typed`.
Each test added for a finding of the review failed before its fix, as described above.
A unit test in `ferro/src/store.rs` holds the placement lock while two
shared writers open a Buffered file whose superblock names a frame past its anchor that reads as
torn, then lets it go, and both must open at the frame before it, which one of them did not with the
lock at 2^62 + 1 taken before recovery. Three more check the frames a catch-up reads in one read: a
frame checked in memory must be judged as the file judges it, whole, with a bit flipped in each byte
of its header past the magic, in every 997th byte of its body and in its table, with its first block
changed and that block's entry in the table rewritten to match, which only the frame's check finds,
and cut a byte short; the frames kept must answer for offsets inside them only, up to the end of
each frame or the readable tail, across a gap between them; and a follower must read the objects of
the frames it last kept from memory after their copy on the disk is damaged, and those of a frame
larger than 4 MiB, or of one it no longer keeps, from the file. `ferro/tests/placement_fault.rs`
intercepts `fcntl` to fail a read-only open's test of the placement lock with `ENOLCK` over a
superblock one commit behind, and checks that the open scans and finds the commit, where it trusted
the superblock before. `ferro/tests/reentry_shared.rs` opens one file twice and makes reentrant
calls from a decode and from a `Drop` that run while one handle catches up with the other's commit,
cycle or `drop_branch`, as [Writes from inside a closure](reentrancy.md) describes. These files
build on 64-bit Linux only, and `ferro/tests/smoke.rs` checks elsewhere that the option is refused.

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
read off each sync with the option. The fixes from review left a writer without the option where it
was: against the commit before the second set of them, median of five interleaved runs, a hold ran
564,131 instructions outside the closure before and 564,739 after, a confirm 730,014 and 729,972,
and a cancel 897,147 and 896,960, with CPU time within its runs' spread.

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

### Reading back what other processes wrote

Before the two changes below, two processes on one branch under Buffered made 31,800 commits a
second and eight 21,900, where two and eight threads of one process made 43,000 and 38,500 (#88).
These figures and the profile below come from `ferro-bench --bin processes` on 2026-11-06,
pinned to the eight cores of one CCD of the 32-thread machine, `taskset -c 0-7`, with nothing else
running. `perf` is not installed there, so CPU time came from `getrusage` in each child, the time
each process held its branch lock from timers around the kernel lock in a build kept for the
measurement, and where the time went from a profile of the children sampled on `SIGPROF`, one
sample a millisecond of CPU at the kernel's tick, over ten-second runs:

| Before | 2 processes | 2 threads | 8 processes | 8 threads |
| --- | ---: | ---: | ---: | ---: |
| Branch lock held per commit | 29.5 µs | 21.9 µs | 43.0 µs | 23.8 µs |
| Share of the time it was held | 94% | 93% | 93% | 91% |
| Nodes read from the file per commit | 3.5 | 0.2 | 5.3 | 0.2 |
| Other objects read from the file per commit | 3.5 | 1.0 | 9.5 | 1.0 |
| Frames followed per turn | 1.0, 2.4 KB | | 7.0, 17 KB | |
| CPU per commit, all processes | 33.4 µs | | 47.3 µs | |

The lock passed from one process to the next about as fast as between two threads, the time it
was free coming to 2.0 µs a commit at two processes and 1.6 µs at two threads, and 2.9 and 2.4 µs
at eight, so the difference was nearly all work done holding it: 7.6 µs more a commit at two
processes and 19 µs at eight. Against two threads, the profile put 6.6 µs more of each
commit at two processes in reading nodes back inside the closure: 2.4 µs reading the record, 1.8
µs decoding the node and 2.8 µs putting it in the cache, which evicted another entry each time;
1.5 µs more in BLAKE3, most of it hashing what was read back; and 3.7 µs in the catch-up on taking
the lock, of which the frame took 0.9 µs, the branch's new root and log entry 1.5 µs and the step
0.4 µs. At eight processes against eight threads the reads back took 8.1 µs more, their hashes
2.8 µs and the catch-up 6.0 µs. The other objects are the root, the log entries and the steps:
threads read one log entry a commit too, as appending to the log looks one up.

A turn reads back so many nodes because a node of its path is stale whenever another process
rewrote it since this one last read or wrote it, which at every level below the root is about
(n - 1)/n of the time. A key among 100,000 lies about six levels down, so a turn reads about
1 + 5(n - 1)/n nodes, 3.5 at two processes and 5.4 at eight, as counted, and most of them were
rewritten by commits older than the frames the turn had just followed, which is why the
alternatives under [Measured and left out](#measured-and-left-out) that warm a process's cache
from those frames gain little. Two changes came of it:

- A node a closure reads from the file to change is not cached, and the closure changes it in
  place, as described under [Nodes a commit changes](read-path.md#nodes-a-commit-changes). Each
  node read back had cost a cache insert, an eviction and a copy as the closure changed it.
- A catch-up reads each frame in one read with the next header and checks it in memory, and keeps
  the bytes of the frames it followed for reads of their objects, as described under [Catching
  up](#catching-up). The steps, roots and entries of those frames, and the nodes the next closure
  reads back from them, come from memory, still hashed as they are read.

The same tool on 2026-11-07, pinned the same way with nothing else running, alternating `main`
before the changes, the first change alone and both, median of five rounds under Buffered and three
under Fsync, commits a second:

| Durability | Branches | Writers | Processes, before | First change | Both | Threads, before | Both |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Buffered | one | 1 | 45,555 | 46,201 | 46,331 | 46,173 | 46,128 |
| Buffered | one | 2 | 32,921 | 35,322 | 35,951 | 43,649 | 43,566 |
| Buffered | one | 4 | 26,557 | 28,499 | 29,407 | 40,969 | 41,046 |
| Buffered | one | 8 | 22,162 | 23,787 | 24,738 | 39,096 | 39,161 |
| Buffered | own | 2 | 65,497 | 66,331 | 66,537 | 66,260 | 66,153 |
| Buffered | own | 4 | 88,244 | 88,868 | 88,899 | 90,777 | 91,593 |
| Buffered | own | 8 | 58,393 | 59,008 | 58,669 | 105,972 | 104,797 |
| Fsync | one | 2 | 2,697 | 2,661 | 2,710 | 2,613 | 2,621 |
| Fsync | one | 4 | 5,447 | 5,090 | 5,535 | 5,060 | 5,233 |
| Fsync | one | 8 | 10,038 | 10,046 | 10,126 | 10,614 | 10,014 |
| Fsync | own | 2 | 2,782 | 2,601 | 2,752 | 2,613 | 2,649 |
| Fsync | own | 4 | 5,134 | 5,017 | 5,431 | 5,162 | 5,380 |
| Fsync | own | 8 | 10,190 | 10,279 | 9,941 | 9,900 | 9,792 |

On one branch under Buffered the first change gave 7.3% at two, four and eight processes, and the
second 1.8, 3.2 and 4.0% more, every round of each build above every round of the build before it:
9.2, 10.7 and 11.6% together. Two processes went from 0.75 of what two threads make to 0.83, four
from 0.65 to 0.72 and eight from 0.57 to 0.63, and at eight p50 went from 346 to 312 µs and p99
from 704 to 618 µs. Threads, and processes on branches of their own, which read back little, moved
by at most 1.6% either way. Under Fsync one fdatasync a round is the limit, and the figures moved
by up to 6% either way, as rounds of one build spread: four processes on their own branches made
2,315 to 5,429 commits a second in the rounds before the changes.

On one process neither change moved a figure beyond its rounds' spread. `group`, one writer making
single-key Buffered commits into 100,000 keys, median of three alternating rounds, made 51,597
commits a second before and 51,310 after, and 46,089 and 46,084 with the option. `ferro-bench`'s
single-key Buffered map commit took 23.5 µs and 179,743 instructions before and 23.4 µs and
178,156 after, and `tables`, median of seven rounds, took 86.5, 107.0 and 145.7 µs for a hold, a
confirm and a cancel before and 87.2, 107.6 and 147.8 after, with the instructions outside the
closure within 0.15% and each one's runs spreading over up to 5.3 µs.

The kept frames' first form, described under [Catching up](#catching-up), went in with those
figures. Measured again with the deque, the binary search and the `ArcSwapOption`, in a second run
the same day of five Buffered rounds alternating `main`, the first change and both, one branch made
32,452, 34,990 and 35,759 commits a second at two processes, 26,474, 28,344 and 29,291 at four and
22,099, 23,658 and 24,742 at eight: 10.2, 10.6 and 12.0% over `main`, with every round of each
build above every round of the one before. Branches of their own made 66,599, 87,512 and 57,850 at
two, four and eight processes against 65,017, 87,973 and 57,766 for `main`, and threads moved by
less than 1% but for eight threads on their own branches, 103,622 against 105,368, whose rounds
overlap. Six more Fsync rounds at eight processes spread from about 500 to 10,500 commits a second
in every build as the disk stalled, and the medians, 9,774, 9,450 and 9,422 on one branch and
9,381, 9,659 and 9,680 on branches of their own for `main`, the first change and both, moved in no
one direction.

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
- Following with `Options::follow` so that a writer's thread warms the next turn. The follower
  looks at most once a millisecond while commits keep coming, and only up to the frame a
  superblock names, so at eight processes on one branch a turn still followed 6.95 frames under
  the branch lock where it had followed 7.0, and the processes made 30,738 commits a second at two
  and 21,232 at eight, against about 32,000 and 22,000 without it.
- Catching up before waiting for the branch lock, with a try first and a catch-up when it fails. A
  writer comes back to the lock straight after its own commit, when at most one other frame has
  landed, and the frames it will need land while it waits in `F_OFD_SETLKW`, where it can read
  none of them. Not built.
- Putting every object of a followed frame in the cache as the catch-up reads it, decoded or as a
  checked record that the first read decodes. The nodes a turn reads back were rewritten by any
  commit since this process last touched them, and few of them by the frames it has just
  followed: a node of its path is stale when another process rewrote it since, about
  (n - 1)/n of the time at every level below the root, so of the 5.3 nodes a turn read at eight
  processes about 2.2 were in the seven frames it followed. The store does not know the types to
  decode the rest, a checked record would still be decoded and cached on its first read, and the
  other processes' frames hold about ten objects each, so at eight processes each would hash and
  cache some 70 objects for each commit of its own to save the read and the hash of the one or two
  of them, at levels two to four, that it reads back before the cache evicts them. A frame's xxh3
  checks do not stand in for the hash: they find a torn or damaged write, not a frame made to
  match them, and an object cached under a hash it does not have would answer every later read of
  that hash. Not built.
- Reads that skip the hash, measured only as a bound on what moving the hash off the branch lock
  could gain. With both changes of [Reading back what other processes
  wrote](#reading-back-what-other-processes-wrote), in an earlier form, and no hash on any read of
  a shared writer, two, four and eight processes on one branch made 36,942, 31,369 and 27,016
  commits a second, against 34,453, 28,275 and 24,497 with the hash, median of three alternating
  rounds: 7 to 10%. Every read hashes its object.
- The frames a catch-up keeps in one buffer, grown as it reads each frame, rather than a buffer for
  each. On branches of their own, where every placement follows a frame or two under the placement
  lock, the buffer's reallocations cost four processes 6%: 82,409 commits a second against 87,224,
  median of three alternating rounds, swapped in through an `ArcSwap`, where a buffer for each
  frame and a mutex made 87,395. Sized for the first frame and swapped under a mutex, one buffer
  still made 84,422 against 86,602 over five rounds, though at eight processes on one branch it
  made 24,807 against 23,995 for a buffer each. The buffer for each frame stays, since it costs a
  writer on its own branch nothing, and with one for each frame the `ArcSwapOption` that reads now
  load measured the same as the mutex on one branch and on branches of their own, as above.
- glibc's `trim_threshold` and `top_pad`, through `GLIBC_TUNABLES`, since the profile showed
  `mprotect` from growing a thread's arena under the 8 KiB hash table that LZ4 allocates for each
  compression: 32,306 and 22,141 commits a second at two and eight processes against 32,106 and
  22,030, within the spread of the rounds. The table is `lz4_flex`'s and the same with threads.

## Limits

- All the processes must be on one machine. The file system's page cache is what makes a frame one
  process wrote readable by another, and network file systems give no such guarantee, nor reliable
  locks.
- Each process has its own cache, so on one branch a process reads back the nodes another process
  wrote before its closure can change them.
- A process sees other processes' commits only when it commits, attaches a branch or calls
  `refresh`, unless it follows the file with `Options::follow`. One that does none of these holds
  the file a cycle replaced, as a read-only handle that never refreshes does.
- A follower is woken by inotify only on 64-bit Linux, with one inotify instance and two watches
  for each open that follows, which count against the user's `max_user_instances` and
  `max_user_watches`. Elsewhere, and past those limits, it looks every 10 ms, as described under
  [Followers](#followers).
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
