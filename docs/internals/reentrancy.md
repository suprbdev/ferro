# Writes from inside a closure

A write called from inside a closure used to wait for a lock its own thread held, and so never
returned (#41). These are the cases, each reproduced with a timeout in `ferro/tests/reentry.rs` and
`reentry_sync.rs`, and each hung before this change:

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
  group it waits for may be the closure's own, and the sync thread that would close another may be
  parked on the closure's lane.
- Two closures on two threads that write into each other's files, each holding the lane the other
  needs.
- A write from user code that runs under `tip` rather than a lane. It took its lane and then waited
  for `tip`, held by its own thread. Since commits draft outside `tip`, as described under [Drafting
  outside `tip`](commit-path.md#drafting-outside-tip), `Data::encode` and the decode of the new root
  run under the lane only, so a write from them is judged as one from the closure. What still runs
  under `tip` is the drop of the drafted root when nothing changed; when another frame landed during
  the draft, `decode` of the moved root and the drop of the drafted one it replaces; when a cycle
  swapped the file during the draft, the whole draft again; and in a cycle's swap, the drop of the
  states it replaces. The tests cover the drops and the moved root's `decode`.

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
attaching a branch, `refresh` and `drop_branch`. The commit's record is taken by its callers rather
than inside `commit_locked`, so it also covers the draft that `commit_locked` drops when it returns
early. A write is refused when its thread holds:

- the whole file it writes to;
- with `coalesce`, any lane of that file;
- any lane of that file, for `compact`, `pull`, `push` and `receive`, which take every lane;
- a lane of that file whose name sorts at or after the one the write takes.

`pull`, `push` and `receive` take the lanes only to land a bundle, after it has streamed into a
scratch file, as described under [Where checked objects
wait](sync-receive.md#where-checked-objects-wait). They make the check before they read anything, so
a call that will be refused reads no stream, but the edge into the receiving file described below is
listed only from the end of the stream to the merge, and a pull's edge into the sending file only
while it plans the bundle.

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
closure. The `Drop` impls that run under a lane or `tip` are covered by the same record: values a
closure captured, a coalesced group's working copy, a commit's drafted root when nothing changed or
when it moves, and the states a cycle's swap replaces. The library's own drop that writes, the flush
when the last handle of a file goes, takes that file's lanes, but no closure can run on a file once
its last handle is gone, so nothing can hold them.

## What stays undetected

What stays undetected is a wait that ferro does not see. A closure that waits on a channel, a
condvar or a `Watcher` for another thread's commit can still deadlock: on its own branch, since that
commit needs the lane; on another branch of its file, while a cycle that holds that branch's lane
waits for this one; and with `coalesce`, for a commit whose closure has already run, since that
commit may be in this closure's group. Reads that take the published refs, such as `branches`,
`verify` and `have`, are not checked either, so a `Drop` that reads during a cycle's swap still
waits for itself.

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
