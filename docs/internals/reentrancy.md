# Writes from inside a closure

A write called from inside a closure used to wait for a lock its own thread held, and so never
returned (#41). These are the cases, each reproduced with a timeout in `ferro/tests/reentry.rs`,
`reentry_sync.rs`, `reentry_shared.rs` and `reentry_locks.rs`, and each hung before this change:

## The cases

- A write that needs the closure's own lane: the commit family on that branch, blocking or awaited,
  `restore`, a merge into it, `fork` from it, and `drop_branch` of it from any handle. An awaited
  one never got past the lane's gate, so `block_on` parked for good.
- A write that takes every lane of the file: `compact`, and `pull`, `push` and `receive` into any
  branch of it.
- A write to a branch that sorts before the closure's own. Alone it went through, but a cycle or
  `receive` that holds the lanes below the closure's branch waits for that one, and the write waits
  for the cycle.
- With `coalesce`, a write to any other branch of the file. The closure may be running in a group
  that an earlier member left open. If its write leaves its own group open while a sync runs and
  waits for it, the leader of that sync wakes one member of the first open group, the closure's, and
  that member waits for the closure's lane to seal it, so nobody seals the write's group. Waiting on
  a write that was started outside the closure, by awaiting its future, hangs the same way: the
  group it waits for may be the closure's own.
- Two closures on two threads that write into each other's files, each holding the lane the other
  needs.
- A write from user code that runs under `tip` rather than a lane. It took its lane and then waited
  for `tip`, held by its own thread. Since commits draft outside `tip`, as described under [Drafting
  outside `tip`](commit-path.md#drafting-outside-tip), `Data::encode` and the decode of the new root
  run under the lane only, so a write from them is judged as one from the closure. What still runs
  under `tip` is the drop of the drafted root when nothing changed; when another frame landed during
  the draft, `decode` of the moved root and the drop of the drafted one it replaces; when a cycle
  swapped the file during the draft, the whole draft again; and in a cycle's swap, the decode of
  every attached branch's root from the new file. The tests cover the drops and the moved root's
  `decode`.
- With `shared`, the decode of another process's roots while catching up with it, which also runs
  under `tip`. A write takes its lane and then catches up under `tip`, and does so again before it
  places its frame, and `fork`, `drop_branch`, `Db::branch`, `refresh`, a cycle and a follower's
  look all catch up too, as [Catching up](processes.md#catching-up) describes. A frame it adopts
  loads the new state of every attached branch it moved, and a cycle's marker loads every attached
  branch from the new file, so `Data::decode` ran under `tip` with only the lane recorded, and a
  `refresh`, a `Db::branch` or a write to a branch that sorts after the lane's passed the check and
  then waited for `tip` (#105). `reentry_shared.rs` makes those calls, and writes to an earlier
  branch, `fork`, `drop_branch` and a read, from a decode during `update`, `fork`, `drop_branch`,
  `compact`, `Db::branch`, `refresh` and a read-only handle's `refresh`, with and without
  `coalesce`, after another handle's commit and after its cycle. Before the fix 18 of each test's
  98 cases hung: `refresh`, `branch`, the later write and `drop_branch` of a later branch inside
  `update`, `fork` and `drop_branch`, and with `coalesce`, which refuses those writes under any
  lane, `refresh` and `branch`.
- User code that ran under a lock no record covered (#107). Attaching a branch, as `Db::branch` and
  `fork` do, a read-only handle's `refresh` and a cycle's swap decoded the states they loaded while
  they held the published refs, which `branches`, `verify`, `have`, a bundle and a merge base take
  to read the branch table, and the cache dropped what one insert evicted past the first two
  entries, and all it held when its store closed, while it held a shard's lock, which every read of
  the shard takes, so a `decode` or a `Drop` that made such a read waited for itself. A cycle holds the cycle lock without any lane
  while it copies, and closed the file it replaced under it, and a `receive` whose bundle spilled
  holds it while its pieces are placed, so a `Drop` run there that called `compact`, or a `pull`,
  `push` or `receive` whose bundle spilled, waited for it too. `reentry_locks.rs` makes those
  reads, reads through the cache, `pull`, `push`, a small and a spilled `receive` and `compact`
  from a `decode` and from a `Drop` that the cache runs as it evicts a value or closes, during a
  read, `Db::branch`, `fork`, a read-only handle's `refresh`, `compact` and a spilled `receive`,
  plain and shared, with a cache small enough that one large root evicts several values. Before the
  fix 51 of its 198 cases hung, the reads through the cache only when they reached the shard being
  written; after it every one returns at once with the result the check below gives.

A merge closure, pull's merge, an awaited write's closure, a watcher test run on the thread that
waits for it, and a `Drop` of anything a closure captured all run under the lane, so they are the
same cases.

## The check

Each write now checks, before it takes any lock, a thread-local record of what its thread holds, and
fails with `Error::Reentrant` where waiting could deadlock. Taking a lane records its file and its
branch's name, and dropping the lane removes them. A file is the `Shared` behind every handle of one
`open`, numbered when it opens, since two opens of one path have separate locks. Each place that
holds `tip` while user code may run records the whole file: a commit's hold of `tip` after its
draft, a cycle's or `receive`'s hold of every lane, each piece of a bundle that `receive` places,
attaching a branch, a read-only handle's `refresh`, `fork` and `drop_branch`, and a catch-up that
adopts a frame or follows a cycle's marker, wherever it runs. The commit's record is taken by its
callers rather than inside `commit_locked`, so it also covers the draft that `commit_locked` drops
when it returns early. The catch-up's is taken in `follow_to` only once the log names something
new, so a commit in a process alone, which finds nothing, records nothing more. `fork` decodes
nothing, but the cache of decoded objects may drop a value it evicts at any read under `tip`. A
cycle records the cycle lock from when it takes it until it lets it go, and so does a `receive`
whose bundle spilled while its pieces are placed. A write is refused when its thread holds:

- the whole file it writes to;
- with `coalesce`, any lane of that file;
- any lane of that file, for `compact`, `pull`, `push` and `receive`, which take every lane;
- the cycle lock of that file, for `compact`, and for `pull`, `push` and `receive` when their
  bundle spilled, which take it;
- a lane of that file whose name sorts at or after the one the write takes.

Two shared opens of one file lock it separately, so a lock that one holds is one the other waits for
in the kernel, and a write through one is refused as well when its thread holds, through another
open of the same file:

- the lane of the branch the write takes, or of one whose name hashes to the same byte;
- any lane, for `compact`, `pull`, `push` and `receive`, which take every byte;
- the cycle lock, for `compact`, and for `pull`, `push` and `receive` when their bundle spilled.

Its other locks through that open count only as a crossing, as locks in other files do, so a write
to another branch through it, or into another file, goes on as before. The record names each lock's
file by an xxh3 of its canonical path, the same for every open of it. Until #142 a write to `a`
through a second open, from inside a closure on `a`, waited for good for the byte its own thread
held, and so did a `compact` through it.

With `shared`, a write the check lets through also reads the record to decide whether it may wait
at the gate: one whose thread holds anything on that file, through any handle, goes straight to its
branch's byte, as [The gate](processes.md#the-gate) describes. Each record names the file as well as
the handle, by an xxh3 of its canonical path, and locks on other files do not count.

`pull`, `push` and `receive` take the lanes only to land a bundle, after it has streamed into a
scratch file, as described under [Where checked objects
wait](sync-receive.md#where-checked-objects-wait). They make the check before they read anything, so
a call that will be refused reads no stream, but the edge into the receiving file described below is
listed only from the end of the stream to the merge, and a pull's edge into the sending file only
while it plans the bundle. A bundle spills only as it streams, so the check for the cycle lock comes
at its landing.

Reads are not checked, and need not be, since no user code runs under the locks they take. They
take the published refs only to copy the branch table. Attaching a branch, a read-only `refresh`, a
catch-up that follows a cycle and a cycle's swap decode the states they load before they take the
published refs, and a sync, a swap and a refresh drop the table they replace, and a catch-up the
branches it retires, once they have let them go. A read through the cache of decoded objects takes
a shard's lock, and the cache keeps every entry an insert evicts until the insert has let that lock
go, the first two in the insert and the rest in a list each thread lends from one insert to the
next, while a close takes the entries out one at a time. A value the cache drops therefore runs its
`Drop` under whatever the reader holds: a lane, the whole file or the cycle lock, which the record
covers, or nothing.

Without `coalesce`, then, a closure may write to a branch whose name sorts after its own, including
`fork` from it and `drop_branch` of it, and those writes may nest further up the order. Cycles and
`receive` take lanes in that order and `tip` after them, and nothing waits for a lane while it holds
`tip`, so each thread waits only for a lock above everything it holds, and no cycle of waits can
form. A closure on `main` therefore cannot write to `b`, while one on `b` can write to `main`.
`branch` and `refresh` take only `tip`, so they are refused only under the whole file.

## Writes into another file

Writes into another file deadlock only against writes that come back, directly or through more
files, since the library never holds the locks of two files at once. A cycle on a file can stand
between any of its lanes and any other, so the file is the unit. A write made while its thread holds
a lane of another file adds an edge from each file it holds to the one it writes, to a process-wide
list, for as long as it runs, and is refused if the file it writes already reaches one it holds
through the edges listed. Of two closures that write into each other's files at once, exactly one
fails. While a closure on one file receives into another and its peer is slow, a closure on the
receiving file may write into the first, because the receive holds nothing there yet. If that write
still runs when the stream ends, the receive fails with `Reentrant` at its landing, having read its
bundle for nothing. Only a write made while its thread holds something takes that list's mutex, and
a closure that only ever writes one way between two files is never refused.

## Awaited writes, watchers and drops

An awaited write checks at every poll. Before its closure has run it needs the lane, as a blocking
write does. After, it only waits for an acknowledgement, which a closure on its file may do without
`coalesce`, since the sync thread and blocking leaders take no lane. With `coalesce` a poll from a
thread that holds a lane of the file fails. The closure ran, so the write lands as if the future had
been dropped, and the error means what any error from a write means: not acknowledged. The future
keeps its edges to other files between polls, since a thread blocked on it still holds its lanes.

A watcher's test on the `ferro-watch` thread holds nothing, so it may write anywhere, as before. A
test run on the watcher's own thread, by `recv` or `try_recv` inside a closure, is checked like the
closure. The `ferro-follow` thread that `Options::follow` starts holds nothing either and runs no
closure or test. Its look takes `tip` as `refresh` does, and on a shared writer the superblock lock
only to lead a sync of this process's own frames or to open a file a cycle renamed, and waits for no
lane, so a test or a closure that refreshes waits at most for one look, as it would for another
thread's `refresh`, and the follower never waits for it. In `ferro/tests/follow.rs` a filtered
watcher whose test refreshes and attaches a branch runs while the follower publishes 50 commits. The
`Drop` impls that run under a lane or `tip` are covered by the same record: values a closure
captured, a coalesced group's working copy, and a commit's drafted root when nothing changed or when
it moves. The library's own drop that writes, the flush when the last handle of a file goes, takes
that file's lanes, but no closure can run on a file once its last handle is gone, so nothing can
hold them.

The states that a catch-up, a cycle's swap or a read-only `refresh` replace, and those of a branch
they retire, are not dropped under `tip` at all. Each call keeps their snapshots in notices, as a
sync keeps the snapshot a publish replaces, and drops them once it has released its locks, a cycle
after its cycle locks as well, so a `Drop` there may write anywhere, as one after the call may.
`reentry_shared.rs` makes the calls above from such a `Drop` after another handle's cycle and after
it dropped a branch this one had attached, and `reentry.rs` writes, awaits a write, attaches and
forks from one after the handle's own cycle. Before, those under the catch-up of a write, `fork` or
`drop_branch` hung as the decodes did, and most of the rest failed with `Error::Reentrant`. The
cache of decoded objects may hold the last copy of a value too, and drops it when it evicts it or
the store closes, under whatever the call holds then, though never under the cache's own lock, as
described under [The check](#the-check). A cycle closes the file it replaced once it has let its
cycle locks go, so a `Drop` that close runs may write anywhere, `compact` included.

## What stays undetected

What stays undetected is a wait that ferro does not see. A closure that waits on a channel, a
condvar or a `Watcher` for another thread's commit can still deadlock: on its own branch, since that
commit needs the lane; without `shared`, on another branch of its file, while a cycle of the same
handle that holds that branch's lane waits for this one; and with `coalesce`, for a commit whose
closure has already run, since that commit may be in this closure's group. With `shared` a cycle or
landing takes every lane of its handle and every branch byte at once or none, so it holds such a
commit up only at its gate, through any handle, its own included: until the end of the phase the
commit finds, a round at most 4 ms longer than the hold had lasted when it came, and at most a
second, and one more round if the commit's branch is busy then, at most 2 s in all, as [The
gate](processes.md#the-gate) describes. The first version of the gate waited there for good, and a
closure on `a` that joined a thread committing on `b` hung beside another handle's cycle (#136);
until #142 a commit through the cycling handle waited for its lane for good, and until #145 a cycle
that held some of its lanes while it waited for the rest could hang beside such a commit.

## Cost

A commit pays the check before its lane and four updates of the record: the lane, and the whole file
around `tip`, each added and removed. On their own, in a loop pinned to one core, those take 18 ns,
against about 24 µs for a single-key Buffered commit. On a 32-core machine shared with other work,
the binaries before and after this change, both drafting outside `tip`, were compared two ways,
twenty rounds each:

- `tables`, before and after running at the same time on two pinned cores, swapped each round: CPU
  time outside the closure went from 78.5 to 78.3 µs for a hold, 92.4 to 92.1 for a confirm and
  122.5 to 122.1 for a cancel, with paired ratios at medians of 1.001 to 1.004 and every round
  between 0.972 and 1.018. Instructions outside the closure moved by 10, 95 and 230 at the median,
  under 0.03%, and inside it by none.
- `ferro-bench`'s single-key commits on tmpfs, one after the other on four pinned cores in ABBA
  order: paired ratios at medians of 1.000 for Buffered, 0.994 for Fsync and 0.994 for a table row
  update, though single rounds ranged from 0.63 to 1.72 while other work loaded the machine.

The same comparisons against the tree before drafting moved out of `tip` gave the same picture:
`tables` within 0.6% of before, single-key commits on tmpfs at paired medians of 1.001 to 1.010, and
on NVMe, twelve rounds, Buffered at 1.003, with the Fsync commit and row update waiting on the disk
at 0.991 and 0.951. There the instructions inside the closure, which runs none of the check, had
moved by 0.7 to 1.0% with the rest of the code generation.

Run one after the other without pinning, whichever binary ran second in a pair took about 25%
longer, so an unpinned first try showed `tables` 10 to 16% slower after. That is why these runs were
pinned, and why `tables` ran side by side.

The record a catch-up takes, and the notices that hold what it replaces, cost a process alone
nothing, since it finds nothing to adopt. A probe kept out of the tree made single-key Buffered
commits through a shared handle, one build after the other on one pinned core in alternating
order: alone, 50,000 a run, 10.70 µs a commit before the change and 10.68 after, with paired ratios
at a median of 1.000 over twelve runs, from 0.973 to 1.036; with two handles of the file committing
in turn, so that each commit adopts the other's frame, 15.45 and 15.51 µs, at 1.008, from 0.977 to
1.033, over ten; and a `fork` and `drop_branch` together 9.95 and 9.96 µs, at 1.000.

Moving user code out from under the published refs and the cycle lock changed nothing a commit or a
warm read does: the decodes and drops it moved run when a branch attaches, a read-only handle
refreshes or a cycle swaps the file, and recording the cycle lock costs a cycle or a spilled bundle
two updates of the record. What a read can pay for is the cache keeping what an insert evicts until
it has let the shard's lock go, where it used to drop all but two of them under it. A probe kept out
of the tree, the build before and after this change one after the other on one pinned core in
alternating order, on a machine shared with other work: a warm single-key get on a 1M-key map, as
under [Where a get spends its time](read-path.md#where-a-get-spends-its-time), took 252.4 ns before
and 250.4 after, at a paired median of 0.997 over twelve rounds, from 0.963 to 1.017, and 1.004 and
1.011 over two more runs of sixteen; a single-key Buffered commit on tmpfs 12.61 and 12.61 µs, at
0.998, from 0.989 to 1.020, then 1.002 and 0.997. A get through a 4 MiB cache, which misses and
evicts on nearly every read, took 2.13 µs before and about 1% longer after, at paired medians of
1.002 to 1.011 over four runs of twelve to sixteen rounds, against 0.998 for the build before run
against itself and for this one with the cache's old handling of evictions put back.
