# Writers and the commit path

How a write goes from a closure to a durable, published version: the per-branch lanes, the `tip`
mutex that orders frames, group commit, and drafting a commit before it takes `tip`. The user-facing
rules are in [Writers and concurrency](../guide/concurrency.md).

## Lanes, `tip` and group commit

Each branch has a writer lock of its own, its lane, and the database has one more, the `tip` mutex,
which orders frames. Writers run their closures and encode what they changed one at a time on a
branch under its lane, place their frames one at a time under `tip`, write them to the page cache
under the lane alone, and sync outside both. Each branch has two states: `state`, the published one
that readers load through `ArcSwap`, and `tip`, the newest one written. Under its lane a writer
clones the branch's tip, runs the closure and drafts its records and log entry, then under `tip` it
places them, writes the refs and queues the frame, and after releasing `tip` it writes the frame to
the page cache and releases its lane. The next writer on the branch builds on that tip even though
it is not durable yet, and a writer on another branch runs its closure meanwhile, as described under
[A lock per branch](#a-lock-per-branch) below. Object reads are bounded by `written`, the end of the
run of frames already in the page cache, rather than `tail`, the published end, so unpublished
objects stay readable to writers, and a read inside a frame that is placed but not yet written waits
for it, as described under [Writing frames after `tip`](#writing-frames-after-tip). Published
pointers never reach past `tail`.

Each writer then waits for its frame. The first to find no sync running becomes the leader: it takes
the whole queue, waits until every frame in it is in the page cache, fdatasyncs, writes one
superblock for the last frame, publishes the batch in order, and wakes the others. Writers that
arrive during the sync form the next batch, and one of them leads it. A lone writer leads its own
one-frame batch, which adds no syscall and no thread switch. `Durability::Buffered` takes the same
path without the fdatasync. A dedicated sync thread for blocking writers was measured and rejected:
it adds a wakeup to every uncontended commit and gained nothing under contention. Awaited writes,
which must not wait on a thread, do use one, as described under [Awaited writes](async-writes.md).

## What is published, and when

Under `Durability::Fsync`, nothing is published or acknowledged before it is durable, so readers,
watchers and other processes never see a commit that could be lost, and they see commits in order.
Under `Durability::Buffered` the superblock can name frames that are not yet synced, and a read-only
follower that refreshes may adopt commits that a power cut later takes back. A closure that ends
without a frame, because nothing changed or it returned `Err`, may still have read an unpublished
tip, so it waits for that tip before returning, and reports the failure instead if the tip never
became durable. That is the frame that last moved its own branch's tip, not the newest frame of any
branch. `fork` copies the tip and waits the same way, so the new branch appears in `branches` only
once durable. `branches` and `verify` use the published refs and do not touch the mutex, and
`branch` takes it only to attach a branch for the first time.

## Failed syncs

If a sync fails, the store poisons, nothing in the batch is published, the leader gets the I/O
error, and every queued writer gets `Error::Poisoned`. A failed commit's frame may still be on disk
and reappear after reopen: an error means not acknowledged, not guaranteed absent. A failed frame
write poisons the same way, but frames queued ahead of it are whole, so the leader waits for any of
them still being written, and they sync and acknowledge. Frames placed after it fail with
`Error::Poisoned` even when their own bytes were written, the leader's own among them, since
recovery keeps only a prefix of frames. A leader that panics in its sync poisons the store the same
way, fails its batch and every writer queued behind it, and wakes them all before the panic goes
on. With `coalesce`, a member whose group failed leads what is left waiting before it returns, as
described under [A group nobody led](coalescing.md#a-group-nobody-led).

An fdatasync that fails outside a leader's sync, at a Buffered checkpoint or under a `receive`
that placed its records ahead of its refs, poisons the same way, and after it no superblock is
written and no checkpoint moves the anchor. A later fdatasync can succeed though the pages the
failed one dropped never reached the disk, so the next leader fails its batch with
`Error::Poisoned` instead of acknowledging frames written before the failure.
`a_failed_checkpoint_sync_fails_the_commits_queued_behind_it` and
`a_failed_sync_under_a_large_receive_fails_the_commits_queued_behind_it` hold a commit on another
branch between its write and its sync while the fdatasync fails, and saw it acknowledged before.

## Direct I/O, rejected

Writing frames with `O_DIRECT|O_DSYNC` was also measured. On the test NVMe a forced-unit-access
write is cheaper than a cache flush, which cut single-writer latency by 26% and raised contended
throughput by 40%. It was rejected because the gain exists only where FUA beats FLUSH, and the
writes bypass the page cache: reads after a bulk commit go cold, which made buffered commits after a
reopen 27% slower, and a read-only follower's `refresh` rose from 5 µs to 117 µs. It also needs an
in-memory pending log on the read path, a fallback, and buffered reads overlapping direct writes on
the tail block.

## Snapshots and the writer lock

A `Snapshot<T>` keeps its version readable for as long as it is held, even past pruning, and so does
any `Map` or `Table` cloned from it, and a `Log` or `History` handle. All of them hold the `Store`,
and so the open file, but not the writer lock. The lock is a `flock` on the writer's file
descriptor, or on Windows a one-byte `LockFileEx` range on its handle, and closing a writer's store,
when the last `Db` handle drops or a cycle retires a generation, truncates the reservation and then
unlocks, in that order, so a new writer can open while they live. They read what they read before.
The new writer's open truncates only past the last valid frame, and every published object lies
before it. It appends after that, and a cycle writes a new file and renames it over the path, so the
old inode stays whole for whoever holds it. A closed store writes nothing more, and a second close
does nothing, so it never truncates under the new writer. A writer that opened with
`Options::shared` holds the lock shared and truncates nothing when it closes, as described under
[Writers in several processes](processes.md). A `Map` from the old writer is foreign to
the new one, since store ids differ, and is rebuilt if committed there. A closure works on a clone
of the tip, or of its group's working copy, so a panic or an `Err` discards its changes.

## A lock per branch

Every branch used to share the `tip` mutex for the whole of a write, so a closure on one branch
stalled commits on every other, and closures on different branches never overlapped though they
touch independent state (#37). Each `Branch` now has a lane, a mutex held while a closure runs on a
copy of that branch's tip, which also holds the branch's open coalescing group. `tip` is taken after
the closure and its draft, only to place the drafted records, write the refs and queue the frame.
Locks are always taken in the order lane, `tip`, `published`, the attached branches, the queue, and
nothing waits for a lane while it holds `tip`.

When the lanes were added, a commit had to know where its frame started before it encoded its first
record. A record names each link by offset, stored as the distance back from the record itself, and
the flush leaves those offsets in the cached copies of the nodes it wrote and in the new tip. So
encoding stayed under `tip`, and only the closure ran in parallel, until the draft described under
[Drafting outside `tip`](#drafting-outside-tip) below. Three ways to move encoding out were
considered first. Relocating a finished frame changes the distance to every object outside it, and
with it the length of its varint, so every record after it moves: that is encoding again, and the
cached nodes would need patching too. Encoding at the current end and starting again when another
frame got there first wastes the work under exactly the contention this is for. Reserving a region
per writer and padding what it leaves would change the format. The parallel flush already encodes,
hashes and compresses subtrees before it knows their offsets and appends them in a second pass,
which is the shape a first phase that needs no position would take, but it lives in the map's flush
and was not extended then. Sequence numbers, the dedupe table, the superblock and the queue were
already ordered by `tip` or the queue's mutex and did not change.

The queue is shared, so frames from every branch wait for one fdatasync as before, and the leader
publishes them in frame order. Coalescing is per branch. A group's working copy lives in its
branch's lane, so that lane is the lock passed between its closures, and each branch can have a
group open, which writes its own frame and version. Arrivals and gated futures are counted per
branch, since only they can join a group, and the costs that decide whether to wait for them stay
shared.

Leaving every group open until its sync ended lost throughput. The groups were sealed when the sync
ended, so their encodes queued on `tip` in the gap before the next sync instead of running during
the last one. The first version also woke a member of every open group, and the first to finish led
a sync of its own frame alone, so the rest waited a second sync: with one writer on each of 8
branches, 50 µs closures and Fsync, commits took 1.5 ms at p50 against 0.8 ms, and 38% fewer went
through than with the single group. Now a closure that leaves its own group open seals any other
branch's group that nobody is on the way to, taking that lane only if it is free, so those encodes
run during the sync, as they did when a closure on another branch flushed the single group. A group
with a writer at its lane or its gate is left to grow. A writer about to lead first seals the idle
groups of other branches, the sync thread those of every branch, and when a sync ends with no frame
waiting the leader wakes one member of the first open group, as it woke the member of the only one
before, so all the groups a sync left open go into the next one. On one branch a closure and a
leading writer find no other branch's group, in a list taken while they already hold the queue's
mutex, and do what they did before. Only the sync thread's order changed there: it closes a stranded
group before it leads rather than after, so the two share a sync.

An awaited write waits at its branch's gate, so it queues only behind writers of its branch. After
its closure it takes `tip` with a plain `lock()` in the same poll, since a guard cannot be kept
across polls. `tip` is held for one frame's placement at a time, a few microseconds, and through one
fsync by `drop_branch`, so an executor thread can wait that long behind other branches' commits, but
never behind a closure.

`fork` takes its branch's lane, which seals its open group, then `tip`, and copies the tip the next
commit would build on. `drop_branch` takes the dropped branch's lane, sealing its group, then `tip`,
and holds both through its frame's sync and until the branch is retired, so a closure on that branch
either committed before the drop or gets `Error::NoBranch`. Every commit checks again under `tip`
that its branch is live, since that is where its frame is decided. A merge runs under our lane only:
it copies the refs under `tip`, then finds the base and runs the closure without it, and its commit
checks under `tip` that the other branch is live too. So a merge from a branch that was dropped
while the closure ran fails with `NoBranch` instead of writing that name back into a merge record. A
reclamation cycle's prune and its pause, and the frame landing a `receive`, replace the tip of every
branch, so they take every attached branch's lane in name order, sealing each open group, and then
`tip`. Attaching a branch for the first time takes `tip`, so once they hold it no branch can appear
that they have not locked. They check under `tip` that the set has not changed since they listed it,
and start again if it has. With attaching left outside `tip`, a test that attaches a branch during
the pause saw it load its tip from the old file and run its closure, and its commit linked its log
entry to an offset in the old file, which came back after the swap as
`pointer 21643 outside log of 16147 bytes`.

Each frame's refs are built under `tip` from the refs of the frame before, so every frame and every
superblock names one consistent set of heads, and a read-only follower's `refresh` sees whole
commits on every branch as before. In the process, the leader publishes the refs of its batch's last
frame and each branch's state in frame order, all under the `published` lock, so a reader that takes
it finds each branch's state beside the head that names its commit, and notices still drop after
every lock is released. A merge reads the other branch's state and its merge records that way, as described
under [The other branch's state](merging.md#the-other-branchs-state).

`ferro-bench`'s `group` tool takes `--branches own`, which forks a branch for every writer. Each
writer commits single-key updates spread over 100,000 keys from a closure that spins for the given
time. On a 32-core machine shared with other work, median of three alternating runs against the tree
before, closures a second:

| Durability | Closure | 4 branches, before | After | 8 branches, before | After |
| --- | ---: | ---: | ---: | ---: | ---: |
| Buffered | none | 40,601 | 43,148 | 35,399 | 38,287 |
| Buffered | 10 µs | 30,981 | 43,936 | 28,743 | 35,370 |
| Buffered | 50 µs | 13,624 | 42,431 | 13,077 | 35,541 |
| Buffered | 200 µs | 4,414 | 17,432 | 4,223 | 30,040 |
| Fsync | none | 5,134 | 5,080 | 9,836 | 9,910 |
| Fsync | 10 µs | 5,090 | 5,214 | 9,958 | 10,226 |
| Fsync | 50 µs | 5,498 | 5,084 | 9,712 | 9,794 |
| Fsync | 200 µs | 4,208 | 5,559 | 4,154 | 9,567 |

Before, every closure queued on the one lock, so throughput was the inverse of a closure and its
encode however many branches wrote. Buffered p99 at 8 branches fell from 7.8 to 0.53 ms with 50 µs
closures and from 11.6 to 0.57 ms with 200 µs, while p50 with no closure rose from 27 to 184 µs: one
writer used to take the lock again and again while the others waited, and now each branch takes its
turn at `tip`. Encodes still queue on `tip`, so 8 branches with short closures commit more slowly
than 4, and Fsync gains only once closures are long enough that the lock, not the disk, was the
limit. A prototype that changed only the plain path measured the same before the rest was built:
3.27 and 2.62 times at 50 µs, 4.17 and 6.83 at 200 µs, and 1.34 and 2.19 for Fsync at 200 µs.

One branch, median of five alternating runs, before and after:

| Durability | Closure | 1 writer | 8 writers | 32 writers |
| --- | ---: | ---: | ---: | ---: |
| Buffered | none | 49,096 → 49,946 | 42,650 → 42,861 | 36,588 → 37,463 |
| Buffered | 50 µs | 14,451 → 14,410 | 13,807 → 13,756 | 12,355 → 12,129 |
| Fsync | none | 2,440 → 2,581 | 10,427 → 10,330 | 23,303 → 23,812 |
| Fsync | 50 µs | 2,208 → 2,246 | 9,886 → 9,937 | 11,055 → 12,686 |

The Fsync figure for 32 writers with no closure is the median of ten more alternating runs, since
the first five gave 0.79 of before while the disk swung between 25,000 and 33,000 a second for both
trees. Single-writer p50 stayed at 18 µs Buffered. `tables`, one Buffered writer, median of five:
outside the closure a hold ran 579,253 instructions before and 579,426 after, a confirm 724,272 and
724,005, and a cancel 913,797 and 913,634, and thread CPU time moved from 70.8 to 70.6, 84.3 to 84.0
and 111.6 to 111.2 µs. On one branch the second lock is never contended, and it costs nothing that
these runs can see.

Awaited writes on their own branches, Buffered, gained as blocking ones did: 13,227 to 41,727 a
second at 4 branches with 50 µs closures and 12,423 to 34,837 at 8. On one branch they stayed within
4%, and Fsync at 8 branches with 50 µs closures went from 9,961 to 10,024 over ten alternating runs.

With `coalesce` and a branch per writer, median of six alternating runs, Buffered went from 34,525
to 41,418 a second at 4 branches with no closure and from 28,552 to 35,066 at 8, and from 12,695 to
37,468 and 10,790 to 35,754 with 50 µs closures. Fsync went from 3,980 to 5,222 and 8,942 to 9,435
with no closure, and from 3,702 to 4,800 and 8,060 to 8,570 with 50 µs, where leaving every group
open to the end of its sync had given 0.62 of before at 8 branches. Coalesced awaited writes with 50
µs closures went from 12,634 to 39,240 and 10,964 to 32,224 Buffered, and from 3,787 to 4,882 and
8,985 to 9,404 under Fsync. On one branch with `coalesce`, Buffered stayed within 4% at 1, 8 and 32
writers with and without a closure, and closures per version stayed where they were, about 8 at 8
writers and 32 at 32 under Fsync. Fsync cells there ranged from 0.83 to 1.20 of before over six
runs, and the same cells, on code that is the same for one branch, came out at 1.04, 1.02 and 1.01
in five runs the hour before: a lone Fsync writer waits on a disk that other work shares.

Left open:

- A cycle, or a `receive` about to land, that waits for a long closure holds the lanes it has
  already taken, those that sort before that closure's branch, so those branches wait for the rest
  of that closure, as every branch did before.
- A closure may commit on a branch whose name sorts after its own. One that sorts before it could
  deadlock against a cycle, which takes lanes in name order, so it fails with `Error::Reentrant`, as
  described under [Writes from inside a closure](reentrancy.md).
- An awaited write takes `tip` without a gate after its closure.

## Drafting outside `tip`

A commit encodes before it takes `tip` (#18). Under its lane alone it drafts: it encodes, hashes and
compresses every new object of the working copy into a transaction laid out as if its frame began
where the log ended when the draft started, builds the cached copies of the nodes it wrote, and
decodes the new root as a check, as the flush did under `tip` before. It takes offsets from `dedupe`
only below that start, since anything above it belongs to a frame it cannot yet place itself after.
Under `tip` it checks whether another frame landed since. If none did, the draft is the frame, byte
for byte. What is left under `tip` is checking that the branch is live, the log entry, the refs, the
frame's check and its write to the page cache, and the queue. The cached copies go into the node
cache, and the frame's objects into `dedupe`, after `tip` is released, still under the lane.

If another branch's frame landed first, the draft moves. A record depends on where it sits only
through the distances of its links, so moving it copies its header and body and rewrites those
distances. A distance to an older object grows, and its varint with it, so the pass runs record by
record and keeps a table from each drafted offset to its new one. It also looks each record up in
`dedupe` again and drops it if the frames before wrote the same object, pointing its entry in the
table at that copy, as a draft made in place would have done. The root is decoded again from its
moved links. Each cached copy is then cloned with its origin, its children and its spilled values
moved through the table, and its memos pointed at its children's moved copies. A node whose values
hold links of their own, a nested collection, would have to be decoded again to move them, and so
would the nodes a parallel flush decoded from its arenas; those are left out of the cache and read
from the file when next needed.

The format did not change. A moved frame is the frame the same commit would have written in its
place: `a_frame_another_branch_moves_is_the_frame_written_in_its_place` has another branch commit,
from the hook, inside every draft on main, and compares the file byte for byte with one where those
commits came first, across nested maps, spilled values, a value both branches write and parallel
flushes. The two can only differ when `dedupe` hands out a different copy of the same object, as it
can between any two interleavings, so the test keeps each branch's edits apart and the cache large
enough that reads register nothing.

`tables`, one Buffered writer, median of five interleaved runs against the tree before, with
counters around the time `tip` is held:

| Commit | Under `tip`, before | After | Instructions under `tip`, before | After |
| --- | ---: | ---: | ---: | ---: |
| Hold | 64.7 µs | 6.5 µs (-90%) | 519,449 | 30,973 (-94%) |
| Confirm | 77.4 µs | 7.2 µs (-91%) | 643,082 | 33,041 (-95%) |
| Cancel | 102.3 µs | 8.4 µs (-92%) | 807,614 | 35,780 (-96%) |

A hold's draft runs 469,000 instructions in 51 µs under the lane, and the cache and `dedupe` inserts
after `tip` 23,000 in 6 µs. Of what stays under `tip`, the log entry is about 8,000 instructions,
the refs 7,000, and the frame's check and write about 9,000 and 3 µs, most of it the copy into the
page cache. The commit as a whole costs one writer about what it did: outside the closure a hold ran
542,230 instructions before and 544,272 after, a confirm 667,726 and 670,666, and a cancel 832,852
and 835,314, 0.3 to 0.4% more, with thread CPU time of 70.3 and 69.9, 83.5 and 83.0, and 109.9 and
109.9 µs, and bytes per write stayed at 12,208. A single-key Buffered commit into 100,000 keys ran
110,310 instructions outside its closure before and 111,315 after, 0.9% more, with CPU time at the
median of 15.2 to 15.6 µs before and 15.4 to 15.6 µs after over five runs each. Where those last
thousand instructions go was not found.

`ferro-bench`'s `group` tool with `--branches own`, single-key commits into 100,000 keys from
closures that spin for the given time, median of five alternating runs on the NVMe drive of a
16-core machine shared with other work, closures a second:

| Durability | Closure | 4 branches, before | After | 8 branches, before | After |
| --- | ---: | ---: | ---: | ---: | ---: |
| Buffered | none | 42,337 | 85,071 (2.01x) | 36,716 | 67,241 (1.83x) |
| Buffered | 10 µs | 42,145 | 79,048 (1.88x) | 36,373 | 70,582 (1.94x) |
| Buffered | 50 µs | 43,082 | 49,219 (1.14x) | 36,404 | 73,701 (2.02x) |
| Fsync | none | 5,494 | 5,292 | 10,070 | 9,617 |
| Fsync | 10 µs | 5,119 | 5,535 | 9,981 | 10,072 |
| Fsync | 50 µs | 5,184 | 5,285 | 9,736 | 9,838 |

Buffered p50 at 8 branches with no closure fell from 220 to 116 µs and p99 from 437 to 282 µs. Four
branches with 50 µs closures were already near what four threads that each spend 50 µs in a closure
and about 20 in a commit can do. Fsync waits on the disk, one fdatasync per round of branches, and
moved by no more than its runs did between themselves: two sets of the same runs, made before the
guarded reads described under [Reading from many
threads](read-scaling.md#guarded-gets-and-the-epoch-scheme), gave 0.72 to 0.99 of before and 0.95 to
1.09. Bytes per commit in the timed runs came out about 0.8% higher, because twice the commits in
the same seconds leave more of the tool's longer values in each tree and twice the file between a
node and the seed it links to; over the first 88,962 frames of a run of each, frames averaged
2,495.0 and 2,494.6 bytes, with the same records and links.

A 1M-key bulk load, median of three interleaved runs, took 706 ms before and 712 ms after, 0.4% more
instructions, all of it in the commit, and the gets, scans and ranges after it did not move. Peak
RSS rose from 373 to 377 MiB in both `bulk` and `ferro-bench`: each node a commit remembers for the
cache now carries the function that moves it and its link count, 16 bytes over 140,000 nodes, held
in a vector that doubles. Keeping those in a vector of their own put RSS back but cost a single-key
commit 1,000 instructions more, for the allocation. The rest of `ferro-bench` stayed within 3% but
for `map_diff_per_change`, a read this change does not touch, at 5%: a single-key commit took 24.2
and 24.0 µs Buffered and 411 and 405 µs with fsync, and its concurrent commits went from 10,382 to
10,501 a second.

Left open:

- The log entry, a fifth of what is left under `tip`, depends only on the branch's own chain except
  when a merge records what it merged. It is now drafted too, as described under [Drafting the log
  entry](#drafting-the-log-entry).
- The frame's write to the page cache is the largest part left. It is now written after `tip`, as
  described under [Writing frames after `tip`](#writing-frames-after-tip).
- A frame that moves loses the cached copies of nodes holding nested collections and of a parallel
  flush, so the next commit on the branch reads those back.
- The thousand instructions a single-key commit gained, and the 4 MiB of a 1M-key commit.

## Writing frames after `tip`

After the draft moved out, a booking hold still held `tip` for about 8 µs, and the largest part of
it was writing the frame: its xxh3 check and the copy into the page cache, about 4 µs of a 13 KB
frame (#51). A commit now only places its frame under `tip`. It takes the next sequence number and
the frame's range of the file, moves `placed`, the end of the last placed frame, to the frame's end,
and queues the frame, so the order of frames, their sequence numbers and the refs each one names are
still decided in one place. After releasing `tip`, still under its lane, the writer computes the
check and writes the frame. `written`, the end below which every frame is in the page cache, only
moves over a contiguous run: a frame written while one before it is still being written is noted,
and whoever writes the one before carries `written` over both. `fork`, `drop_branch`, the frames of
pruning and of `receive`, and a draft that spilled write their frames before releasing `tip`, as
before.

Nothing reads a frame before it is written. A draft lays itself out from `placed`, and takes offsets
from `dedupe` only below it, so it may link into a frame that is still being written but never reads
one. A read between `written` and `placed` waits for `written` to pass it: a commit on another
branch that loads a node of the refs the frame before named, once the cache has dropped it, or a
merge that loads the other branch's newest entry. The writer it waits for holds no lock but its own
lane and runs no user code before it writes, so the wait ends. A read past `placed` fails as before.
The next commit on the same branch needs the lane, which the writer releases only after writing.

The leader of a sync waits until `written` reaches the end of the last frame it took before it
fdatasyncs or writes a superblock, so a superblock never names a frame that is not in the page
cache, and under `Durability::Fsync` never one that is not durable. Frames reach the page cache, and
so the disk, out of order, but recovery keeps only frames that follow each other from the anchor: a
power cut that keeps a later frame and loses an earlier one rolls back to before the earlier one,
and neither was acknowledged, since acknowledging waits for the sync, which waited for both. Each
frame's refs were built from those of the frame before under `tip`, so every prefix is a consistent
set of heads. Under `Durability::Buffered` the anchor still names only what an fdatasync covered,
and a read-only follower reads only what a superblock names, all of it written before the superblock
was. A reclamation cycle and a landing `receive` take every lane before `tip`, so no frame is being
written while they copy or swap. The Fsync preallocation zero-fills past the end of the newest
placed frame, under `tip`, so it never overwrites a frame being written.

If a frame's write fails, the store notes where that frame starts and poisons, and every waiter
wakes. The leader waits until `written` reaches that point, so every frame of its batch before the
failed one is in the page cache, then syncs and acknowledges those frames and fails the rest with
`Error::Poisoned`, including frames placed after it whose bytes were written, since recovery would
not keep them. A read before that point waits for its frame as it would on a healthy store, and a
read past it fails once `written` gets there. The leader used to stop waiting as soon as the store
poisoned, so a frame placed ahead of the failed one and still being written then failed with
`Error::Poisoned`, though it was whole and reappeared after reopen
(`a_frame_still_in_its_write_when_a_later_one_fails_is_acknowledged`). A leader also took the
outcome of its sync for its own, so one whose frame followed the failed one was acknowledged when
the frames ahead synced, and a reopen did not keep its commit
(`a_leader_whose_own_frame_follows_a_failed_one_is_not_acknowledged`). It now checks its own frame
against what the sync settled.
The writer whose write failed puts its branch's tip back to the frame before and
reports the I/O error, as when the write failed under `tip`. A writer that panics between placing
and writing poisons the store the same way. The other branches keep tips in frames placed after
the failed one, past `written` for good, so a read there fails with `Error::Poisoned`, and a closure
on a poisoned store does not run. A closure on such a branch used to read its tip and panic on
`pointer ... outside log`, and an awaited commit that waited on such a read failed with
`Error::StalePointer` (`a_closure_after_a_failed_landing_is_told_of_the_poison`).
`a_power_cut_between_frames_that_landed_out_of_order_keeps_a_prefix` holds one branch's writer
between placing and writing while two other branches place and write the frames after it, so every
round writes three frames out of order, and replays prefixes of the recorded writes under both
durabilities with all, none, half, torn sectors, the superblocks only or the frames only of what was
not synced. With a leader that does not wait for `written`, it fails on the first image whose
superblock names a frame that was never written.
`a_commit_that_reads_a_frame_not_landed_yet_waits_for_it` has a commit and a merge on other branches
read through the held frame with the node cache off.

`tables`, one Buffered writer, median of three interleaved runs, with counters around the time `tip`
is held across holds, confirms and cancels: 7.86 µs at the median before and 3.70 µs after (-53%),
and 33,461 instructions before and 22,917 after (-32%). The commit as a whole costs what it did:
outside the closure a hold ran 566,604 instructions before and 567,379 after, a confirm 732,782 and
733,176, and a cancel 900,867 and 901,360, with thread CPU time of 75.6 and 75.7, 96.5 and 96.6, and
124.8 and 124.5 µs, and bytes per write stayed at 13,297.

`group` with `--branches own`, median of four alternating runs on the 32-core machine, closures a
second:

| Durability | Closure | Branches | Before | After |
| --- | ---: | ---: | ---: | ---: |
| Buffered | none | 4 | 88,493 | 91,606 (1.04x) |
| Buffered | none | 8 | 70,154 | 77,003 (1.10x) |
| Buffered | none | 16 | 58,494 | 60,496 (1.03x) |
| Buffered | 10 µs | 4 | 79,018 | 80,198 (1.01x) |
| Buffered | 10 µs | 8 | 70,437 | 74,616 (1.06x) |
| Buffered | 10 µs | 16 | 58,266 | 62,304 (1.07x) |
| Fsync | none | 4 | 5,292 | 5,472 |
| Fsync | none | 8 | 9,934 | 10,144 |
| Fsync | none | 16 | 18,038 | 17,822 |
| Fsync | 10 µs | 4 | 5,471 | 5,165 |
| Fsync | 10 µs | 8 | 10,296 | 9,720 |
| Fsync | 10 µs | 16 | 17,864 | 18,326 |

A single-key frame is about 2.5 KB, so its check and copy are about 1.5 µs, and under contention the
section shrank by less: with no closure, `tip` was held for 11.3 µs at the median before and after
at 8 branches, and 13.4 and 12.3 µs at 16, where the refs, the relayout and the log entry are most
of it. Fsync waits on the disk and moved within its runs: an earlier set of three gave 0.71 to 1.35
of before for the same cells.

## Drafting the log entry

With the frame written after `tip`, the log entry was the largest part of what a booking commit
still did under `tip`: about 9,000 instructions and 1 to 2 µs (#51). An entry depends only on its
own branch: its version, time, message and root, the entry before it, the skip pointer found by
walking back down that chain, and the merge-graph numbers carried from the entry before. All of that
is fixed under the lane, so the draft now appends the entry after the root, where the commit
appended it under `tip`, and the commit under `tip` takes it as drafted. A merge that links the
other branch's entry or records what it merged still appends under `tip`, since it reads the refs,
and so does a commit whose file a cycle swapped during its draft, with the time and message it
drafted.

The entry is written without looking in `dedupe`, as before, for the reason given under [Commits
stored twice](sync-receive.md#commits-stored-twice). So that a moved frame matches one drafted in
place, the transaction remembers where its records stopped looking in `dedupe`, and the relayout
looks up again only the records before that point. Moving the entry is moving any other record: its
links to older entries grow and its link to the root goes through the table, and the commit takes
the moved root and the moved entry. The time is now read when the draft builds the entry rather than
under `tip`. It was never ordered across branches, and on one branch the lane orders it as before.
`a_frame_another_branch_moves_is_the_frame_written_in_its_place` now moves entries as well, and the
files still match byte for byte.

`tables` as above, median of three interleaved runs of the tree before both changes, the frame
written after `tip`, and this: `tip` was held for 7.86, 3.70 and 2.04 µs at the median, -74% in all,
and for 33,461, 22,917 and 14,306 instructions, -57%. Outside the closure a hold ran 566,604,
567,379 and 567,479 instructions, a confirm 732,782, 733,176 and 733,451, and a cancel 900,867,
901,360 and 901,670, 0.1% more in all, with thread CPU time of 75.6, 75.7 and 75.8, 96.5, 96.6 and
96.7, and 124.8, 124.5 and 124.2 µs, and bytes per write stayed at 13,297.

`group` with `--branches own`, the same runs as above, closures a second:

| Durability | Closure | Branches | Before both | Frame after `tip` | And the entry drafted |
| --- | ---: | ---: | ---: | ---: | ---: |
| Buffered | none | 4 | 88,493 | 91,606 | 93,857 (1.06x) |
| Buffered | none | 8 | 70,154 | 77,003 | 89,180 (1.27x) |
| Buffered | none | 16 | 58,494 | 60,496 | 68,134 (1.16x) |
| Buffered | 10 µs | 4 | 79,018 | 80,198 | 81,502 (1.03x) |
| Buffered | 10 µs | 8 | 70,437 | 74,616 | 90,746 (1.29x) |
| Buffered | 10 µs | 16 | 58,266 | 62,304 | 64,973 (1.12x) |
| Fsync | none | 4 | 5,292 | 5,472 | 5,394 |
| Fsync | none | 8 | 9,934 | 10,144 | 9,666 |
| Fsync | none | 16 | 18,038 | 17,822 | 18,164 |
| Fsync | 10 µs | 4 | 5,471 | 5,165 | 5,545 |
| Fsync | 10 µs | 8 | 10,296 | 9,720 | 9,957 |
| Fsync | 10 µs | 16 | 17,864 | 18,326 | 17,714 |

Buffered p50 with no closure fell from 114 to 88 µs at 8 branches and from 284 to 243 µs at 16. At 4
branches a writer waited about 6 µs for `tip` at the median before, against 73 µs at 8 and 269 µs at
16, so there was less to gain. With no closure, `tip` was held at the median for 3.70, 2.84 and 1.70
µs on one branch, 11.3, 11.4 and 9.0 µs at 8 branches and 13.4, 12.3 and 11.1 µs at 16. Fsync waits
on the disk, one fdatasync per round of branches, and stayed within its runs.

One writer on one branch, against the tree before both changes: `group`'s single-key commits ran
2,576 and 2,549 a second under Fsync over eight runs in ABBA order, p50 382 and 385 µs, and 47,984
and 48,176 Buffered over five, p50 19 µs both. Bytes per commit of `ferro-bench`'s map and table
workloads stayed at 3,531.7 and 4,744.4. A 1M-key bulk load took 806 and 799 ms at the median of
eight ABBA runs, its commit 207 and 205 ms, with the same instructions per key, and the single-key
commits after it ran about 350 instructions more each, 0.2%, in 848 and 822 ms under Fsync and 55.5
and 50.7 ms Buffered.

What is left under `tip` at 16 branches, with a counter around each part: writing the refs, 4.7 µs
and 28,000 instructions, inserting into them, 1.1 µs and 5,800, moving the draft, 1.5 µs and 13,000,
queueing the frame, 1.0 µs and 3,500, and checking the branch and reading its head from the refs,
0.8 µs and 1,900. The refs grow with the branches: on one branch writing them takes 7,800
instructions, and at 16 nearly two nodes of the branch table are written for each commit, so the
leaf with the new head is encoded again in full, and both are hashed, compressed and copied for the
cache.

Measured and left out:

- Writing the refs and the entry without LZ4. A refs node of 16 branches is over the 96 bytes where
  compression starts, and compressing it took about 3,100 instructions a commit, 10% of writing the
  refs. Leaving it out gave 3 to 6% more commits a second at 8 and 16 branches over three
  alternating runs, and 1.7 to 2.1% more bytes per commit, a trade rather than a win.

Left open:

- The leaf of the branch table that holds a commit's head is encoded again on every commit, with the
  nodes above it, and at 16 branches inserting into and writing the refs are more than half of what
  a commit does under `tip`. A commit now writes one small record in their place, as described
  under [Branch-table steps](#branch-table-steps).
- Moving a draft runs 13,000 instructions for a 2.5 KB frame, about a fifth of a commit's
  instructions under `tip` at 16 branches.

## Branch-table steps

After the log entry was drafted, writing the refs was over half of what a commit did under `tip` at
16 branches (#53). The refs are the branch table, a `Map` from branch name to head, which is a
Merkle tree like any other, so a commit already rewrote only the path to its own head: the leaf
that holds it and the node above, 1.9 nodes a commit at 16 branches and 1.8 larger ones at 64. A
loop that inserts one head and writes the table as a commit does, 20,000 times, counted where that
work goes at 16 branches, per commit:

| Part | Instructions |
| --- | ---: |
| Encoding the leaf that holds the new head | 4,200 |
| Hashing the nodes | 5,600 |
| The cached copy of each node, for the next insert | 7,300 |
| Appending the records, 3,300 of it LZ4 | 8,100 |
| Handing each copy to the cache | 1,700 |

No one part is the cost. Each node on the path is encoded whole, hashed, compressed, appended and
copied for the cache, and the nodes grow with the number of branches: in the same loop writing and
placing the table took 10,700 instructions with one branch, 33,400 with 16 and 50,000 with 64. Each
alternative was measured against the work it would leave under `tip`, the prototypes with
`Durability::Buffered` on tmpfs, alternating with the tree before:

- Each head as a record of its own, drafted with the commit outside `tip`, and the table holding a
  link to it in place of the head. The nodes on the path hold one link per head instead of a head
  with two, and writing the table took 24,200 instructions instead of 33,400 at 16 branches and
  35,800 instead of 50,000 at 64. A prototype that kept such a table and wrote it under `tip`,
  without drafting or writing the head records at all, ran 110,011 and 109,178 commits a second at 8
  branches against 87,398 and 80,022 for the table of heads, and 77,326 and 75,250 at 16 against
  66,780 and 67,422. That is its ceiling, since the head records it left out cost a draft and bytes
  of their own, and a path of nodes is still written under `tip` for every commit.
- Writing the table without LZ4 saves the 3,300 instructions of compressing it, 10% at 16 branches
  and 14% at 64 in the loop. In the same runs it gave 90,812 and 81,384 commits a second at 8
  branches and 69,295 and 54,240 at 16, within what the table of heads did, for 2% more bytes per
  commit at 8 and 16 branches and 4% at 64, the trade it was when last measured.
- Moving merge records out of the head. With none, which is the case in these runs, the `merged`
  map is one byte of each head. With one record in every head writing the table took 11% more at 16
  branches and 10% more at 64, so moving them out saves that on branches that pull from peers, and
  nothing elsewhere.
- Writing under `tip` only the head that changed, linked to the table the frame before named. A
  prototype that wrote that record in place of the table ran 153,869 and 152,084 commits a second at
  8 branches against 87,416 and 86,310, and 129,321 and 127,544 at 16 against 57,001 and 68,156.
  This is what was kept.

### Steps and tables

A frame's head pointer, and the superblock's, now names either a table, the `Map` as it was, or a
step: a record of a tag byte, one branch's name and that branch's `Head` encoded as the table
encodes it, whose first link is the table or step that the frame before named. The heads a step
names are those of the object it links, with its own put in, so a chain of steps over a table gives
every branch's head, the newest step for each name winning. A commit writes a step under `tip` where
it wrote the table path: one record of about 90 bytes, three links and a body too short to compress,
and a fourth link when the branch holds merge records. It decodes the head back from what it
encoded, as the table's heads were, so a merge record map is a link and the branch's next step does
not write it again, and puts it into the table kept in memory, which every reader of `published` and
`tip` uses as before. Forks, `drop_branch`, pruning, a cycle, the landing of a `receive` and the
records a pull writes all write a table, as they did, and so does the commit that would put a 65th
step on one. That table carries every node the steps since the last one changed, in one frame, so
its cost comes back once every 64 commits: about 83,000 instructions at 16 branches and 220,000 at
64, by the difference between a step's own 3,900 and the average under `tip`.

Open, `refresh`, the swap of a cycle and the admin tools follow the steps back to their table and
apply them oldest first. A writer keeps at most 64 on a table, and a reader refuses a chain of more
than 1,024, so a damaged or hostile file cannot make it walk without end. A follower's `refresh`
stops at the head it already holds, so it reads only the steps since its last refresh. The object at
the head says which it is by its body alone: a table's body is the varint count of its branches and
nothing more, and a step's continues past its tag, so a table of 127 branches, whose count is the
tag's byte, still reads as a table (`a_table_whose_count_is_the_step_tag_reads_as_a_table`).
Applying the steps reads nodes of the table into the cache, and a cached head holds a map that holds
the store, so an open that failed on a later step kept the store, and with it the writer's lock,
alive. The `open` fuzz target found it as a leak. Open, `refresh` of a replaced file and offline
compaction now close the store before they return the error
(`an_open_that_fails_on_the_table_under_its_steps_releases_the_file`).

The format of a table did not change, and neither did the magic. A file written before this change
has a table at its head, which reads as it did, and its next commit writes a step on it
(`a_file_whose_heads_all_sit_in_one_table_opens_and_moves_on_to_steps` opens a fixture of that
format with merge records, a dropped branch and a cycle behind it, reads it read-only and
read-write, commits on it, follows it and compacts it). A build from before this change still opens
a file whose head is a table, which a fork, `drop_branch`, a pull or a cycle leaves, and refuses one
whose head is a step: it reads the step as a table whose root is the object the step links first,
which is a table's body, too short for a node, or a step, which if it reads as a node at all reads
as one at level 127, past the deepest level a node may have. So it fails with a decode error instead
of reading wrong heads (`a_reader_from_before_steps_fails_on_a_step`).

Every guarantee the table gave still holds:

- Each frame names one consistent set of heads. A step is built under `tip` on the head the frame
  before named, as the table was.
- Recovery keeps a prefix of frames. A step links only records of frames placed before its own, so
  every prefix resolves. `a_power_cut_across_branch_table_steps_keeps_one_set_of_heads` commits on
  four branches in turn, so the heads after each frame are known, crosses the 64-step bound twice
  with a fork and a drop between, and checks that every image of a cut, with all, none, half or torn
  sectors of what was not synced, opens with the heads of one prefix of the commits. The power-cut
  tests with frames written out of order now write steps too.
- `verify` walks the steps and their table as objects, and every head they hold as a chain: the
  heads in use as before, then those a later step replaced, the lowest `first` first, each down to
  its own `first` and only until it meets an entry already walked down to a `first` at or below its
  own. So a replaced head's tip and root are read and checked, every skip link it follows lands on
  an entry some walk read, and the history a cycle cut below it is not followed. Walked like any
  other object instead, a replaced head led `verify` into history a cycle had cut, and it failed on
  `pointer 0 outside log` (`verify_reads_heads_that_steps_replaced_down_to_their_own_first`).
  Stopping at the first entry already read did the same through a skip link that jumped past where
  the walk stopped, which the model of pruned devices that sync found at five times its scale, and
  `verify_holds_while_pruned_branches_fork_drop_and_cycle` finds in its first four seeds.
  `verify_typed` checks a step as a typed object, and its first link as a table or a step. `layout`
  walks the same objects.
- An object's link that names the record of an entry a chain read counts as walked only if it
  names that entry's hash. One that names another hash is read, and fails its hash check. The walk
  used to know entries by offset alone, so a flipped bit that moved a step's or a table's link to a
  root onto the entry it links as the tip, a record or two along, passed `verify`, since nothing
  else reads a replaced head's root through the step (#91,
  `a_link_redirected_to_a_sibling_is_reported_by_verify`).
- A reclamation cycle copies the table kept in memory, so the new file starts with a table and no
  steps. Entries are still copied by the offset they were read from, as described under [Commits
  stored twice](sync-receive.md#commits-stored-twice), and steps are written without looking in
  `dedupe`, as the table is.
- Sync builds haves and bundles from the published heads in memory, and its merge records and the
  merge graph live in heads and entries that did not change.

`group` with `--branches own`, single-key commits into 100,000 keys with no closure, median of four
alternating runs on the NVMe drive of the 32-core machine, commits a second, bytes per commit and
p50:

| Durability | Branches | Before | After | Bytes, before | After | p50, before | After |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Buffered | 1 | 48,106 | 50,050 (1.04x) | 2,306 | 2,301 | 19 µs | 18 µs |
| Buffered | 4 | 94,371 | 109,704 (1.16x) | 2,532 | 2,348 | 40 µs | 34 µs |
| Buffered | 8 | 89,781 | 150,017 (1.67x) | 2,726 | 2,350 | 88 µs | 50 µs |
| Buffered | 16 | 69,546 | 125,360 (1.80x) | 2,902 | 2,344 | 239 µs | 123 µs |
| Buffered | 64 | 37,909 | 55,438 (1.46x) | 3,518 | 2,372 | 1,765 µs | 1,150 µs |
| Fsync | 1 | 2,514 | 2,583 | 2,266 | 2,258 | 396 µs | 380 µs |
| Fsync | 4 | 5,396 | 5,284 | 2,471 | 2,284 | 730 µs | 758 µs |
| Fsync | 8 | 9,844 | 10,314 | 2,674 | 2,294 | 807 µs | 776 µs |
| Fsync | 16 | 18,704 | 18,238 | 2,875 | 2,306 | 838 µs | 870 µs |
| Fsync | 64 | 29,568 | 45,328 (1.53x) | 3,510 | 2,368 | 2,140 µs | 1,269 µs |

Buffered p99 at 16 branches fell from 630 to 350 µs. Fsync waits on the disk, one fdatasync per
round of branches, and moved within its runs up to 16 branches, where single runs ranged from 0.92
to 1.09 of before. At 64 branches it was `tip` again, and the gain shows. With counters around the
time `tip` is held, Buffered, the mean of two runs of each:

| Part | 1 branch, before | After | 16 branches, before | After | 64 branches, before | After |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| All of it | 3.4 µs, 16,449 | 2.5 µs, 10,354 | 13.4 µs, 58,370 | 7.3 µs, 29,523 | 21.0 µs, 85,910 | 11.3 µs, 41,395 |
| Writing the refs | 0.8 µs, 7,795 | 0.4 µs, 3,889 | 4.4 µs, 27,921 | 0.9 µs, 5,201 | 7.3 µs, 43,172 | 1.4 µs, 7,332 |
| Inserting the head | 0.3 µs, 2,781 | 0.2 µs, 1,836 | 1.2 µs, 5,600 | 1.0 µs, 4,591 | 1.9 µs, 7,836 | 1.6 µs, 7,384 |
| Moving the draft | 0.0 µs, 411 | 0.0 µs, 414 | 1.6 µs, 13,095 | 1.8 µs, 13,130 | 1.8 µs, 13,133 | 1.8 µs, 13,324 |
| Placing and queueing | 0.6 µs, 2,188 | 0.2 µs, 1,474 | 3.4 µs, 6,653 | 1.2 µs, 3,511 | 6.9 µs, 15,376 | 4.0 µs, 10,104 |
| Checking the branch, reading its head | 0.2 µs, 1,342 | 0.1 µs, 894 | 0.8 µs, 1,970 | 0.5 µs, 1,143 | 0.9 µs, 2,060 | 0.6 µs, 1,295 |

Placing got cheaper because it no longer hands the table nodes it just wrote to the cache, and
reading a head because the table in memory holds the nodes the steps changed rather than links the
cache has to resolve. `tables`, one Buffered writer, median of five interleaved runs: outside the
closure a hold ran 570,145 instructions before and 565,637 after, a confirm 736,902 and 732,681, and
a cancel 906,589 and 900,022, with thread CPU time of 74.6 and 73.0, 95.0 and 93.5, and 123.0 and
121.0 µs, and bytes per write went from 13,367 to 13,362.

What it costs is open. A read-only open read the table in one object, 5 to 7 µs here whatever the
branches, and now reads every step on it too, about half a microsecond each: 37 µs with 63 steps on
one branch, 42 µs at 16 branches and 55 µs at 64, and with every branch's state loaded 59 µs
against 26 µs at 16 branches and 131 µs against 92 µs at 64. A `refresh` after one commit took 3.2,
10.2 and 33.3 µs at 1, 16 and 64 branches, against 3.3, 11.3 and 34.6 µs before, whatever the
depth. Writing a table every 16, 32 or 128 steps instead of 64, median of three alternating runs,
gave 0.97, 0.98 and 1.00 of the commits a second at 8 branches, 0.89, 0.97 and 1.02 at 16 and 0.95,
0.97 and 1.02 at 64, and 128 would double what open pays, so the bound stays at 64.

Left open:

- Inserting the head into the table kept in memory, 1.0 µs at 16 branches and 1.6 µs at 64, is
  still under `tip`, and so is moving the draft, now the largest part at 16 branches.
- A fork, `drop_branch` and a pull write a table, with every node the steps since the last one
  changed, under `tip`.
- Open reads the steps one record at a time, up to 64 reads. They sit in the frames written since
  their table, so one read over that stretch of the log would serve them all.
- Placing and queueing grows to 4.0 µs and 10,100 instructions at 64 branches, against 1.2 µs and
  3,500 at 16, which looks like contention on the queue's mutex and was not looked into.
