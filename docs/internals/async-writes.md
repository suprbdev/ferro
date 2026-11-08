# Awaited writes

How `update_async`, `commit_async` and the other awaited writes hold no thread while they wait. The
API is in [Writers and concurrency](../guide/concurrency.md#awaited-writes).

A blocking write holds its thread through the closure, the encode and the sync, so the async
examples wrapped every write in `spawn_blocking` or a pool of writer threads and paid a thread for
each commit in flight (#31). `update_async`, `commit_async`, `commit_with_async`,
`commit_stamped_async` and their `try_` forms return a future that resolves when the commit is
published, which under Fsync is when it is durable, and that holds no thread while it waits for the
lock or the sync. It uses `std::task::Waker` alone, as `Watcher::changed` does, so the library
depends on no runtime.

## Where the closure runs

The closure could run in three places. On a committer thread per database or branch, every closure
and its result would need `Send` and `'static`, which rules out borrowing locals, a panic would have
to be caught and carried back to the right future, and every write would pay a thread switch before
its closure started. That is the combiner thread that coalescing already left out, for the same
bound. On the group leader, a future would run other callers' closures inside its own poll, with the
same bounds, and a future dropped before the leader reached its closure would leave a commit that
may or may not happen depending on timing. It runs on the caller instead, in the poll that gets the
writer lock: that poll runs the closure, encodes and writes the frame exactly as a blocking write
does, and lets go. So the closure needs no `Send` and the future is `Send` exactly when the closure
and its result are, an `Err` behaves as in the blocking call, a panic unwinds out of `poll` on the
caller's thread, and with `coalesce` an awaited closure joins a group like any other.

## Waiting for the lock

The first version took the lock with a plain `lock()` in that poll. The lock is held for one closure
and its encode, a few tens of microseconds, but under load an executor thread waited for it behind a
dozen others, and every request queued on that thread waited too. At 128 connections of
`shop-load`'s 90/8/2 mix, reads went from 0.05 ms at p50 with the writer pool to 0.67 ms, and 2.4 ms
at p99. Now a future that finds the lock taken queues its waker at a gate and returns `Pending`.
Every holder of the lock, blocking or not, holds it through a guard whose drop wakes the first
future in the queue, usually from an executor thread that has just finished another write, so on
tokio the next writer runs right after it on the same worker. A woken future that finds the lock
taken again, by a blocking writer or by a future that has just arrived, goes back to the head of the
queue. Futures polled only when woken therefore get the lock in about the order they queued, and the
lock never sits idle waiting for a woken one to be polled. The order is not exact, because any poll
that finds the lock free takes it: a future polled for the first time between a release and the
woken one's poll goes first, and so does a queued future that its executor polls without a wake, as
a `join` of several writes polls them all. Keeping the order exactly would mean refusing a free lock
to every future but the first in line, which leaves the lock idle from each release until that
future's poll, the wait the gate was built to remove. Blocking writers still park in `parking_lot`
and can take the lock between a release and that poll. A future waiting at the gate counts as on its
way to the lock, so a coalescing group is handed to it as to a thread blocked in `arrive`. The
waiter count is stored before a second `try_lock`, and read after the release, with a `SeqCst` fence
on each side, so a release and a future queuing at the same moment cannot both miss each other. The
lock a future waits for is now its branch's lane, so each branch has a gate of its own, as described
under [A lock per branch](commit-path.md#a-lock-per-branch).

## The sync thread

The sync goes to a `ferro-sync` thread, one per open database, started by the first awaited write
and holding only a `Weak` to the database. A future never calls fdatasync. After its closure runs it
waits on what a blocking writer would wait on: a non-coalesced write registers its waker in the
queue with the sequence number of the frame it needs, and a coalesced one adds its waker to its
group's seal beside any parked threads. If no sync is running it rouses the sync thread, which leads
exactly as a blocking leader does. Whoever leads, that thread or a blocking writer, wakes the
futures whose frames it settled after letting go of the queue's mutex, and if frames or an open
group are left, rouses the sync thread, since no future will lead them. Under coalescing, the
blocking path relies on one woken member to close a group that was left open while a sync ran. A
woken future may be dropped or polled late, so the sync thread also closes any group left open once
no sync runs and nobody is on the way to the lock, taking its lane only if it is free, as described
under [A sync thread that waited for a
closure](coalescing.md#a-sync-thread-that-waited-for-a-closure). A writer that closes another's
group rouses it as well, and so does a future whose group failed, since that group's leader may
have been left another branch's group to sync, as described under [A group nobody
led](coalescing.md#a-group-nobody-led). The sync thread uses the same `syncing` flag as everyone
else, so there is never a second sync at once, and an acknowledgement means what it did: a future
resolves only once the queue's settled sequence has reached its frame, which under Fsync happens
after the fdatasync.

## Dropped futures and closing

A future does nothing until a poll finds the lock free. Dropped before that, polled or not, it
writes nothing: it leaves the gate, and if a release had already chosen it, it wakes the next future
in its place. The poll that gets the lock runs the closure to the end and queues the frame with no
await point in between, so after it the outcome is fixed: whether a dropped write lands is exactly
whether its closure ran. Dropping the future then only removes its waker. The frame stays queued, or
the closure stays in its group, and the sync thread writes and syncs it as if the future were still
waiting. If the last `Db` handle goes first, closing seals any group still open and leads whatever
is queued before it settles the anchor. `Drop` for the shared state cannot require `T: Data`, so it
calls that flush through a function pointer stored when the database opened. The keeper joins the
sync thread, so the file is closed and the writer lock released by the time the last handle's drop
returns. A clean close therefore keeps every write whose closure ran, and a crash keeps every write
that was acknowledged.

## Tests

The tests drive the futures with a small `block_on` and a `join_all` over a park and unpark waker,
on Fsync and Buffered, with and without `coalesce`, alongside blocking writers on other threads. A
recorded trace of awaited writes, some dropped after their closures ran, is cut at every point with
the unsynced writes kept, lost, half kept or torn, and every image must open at a version no older
than the newest one acknowledged under Fsync. Another trace closes the database right after dropping
polled futures and loses everything unsynced, and every write whose closure ran must still be there.
Polled from one thread, no write is on its way to the lane while another's closure runs, so awaited
writes share a group only while a sync is running. `coalesced_async_writes_share_versions` used to
leave that to the sync thread, and under heavy load, when that thread got no CPU while the writes
were first polled, it saw a version for each of its 200 writes. It now holds a blocking writer at
the hook's `leading` step, so a sync is running while every write is polled, and all 200 must share
one version. `examples/chat` tests the same under tokio: hundreds of tasks on a current-thread
runtime, `timeout(Duration::ZERO, ..)` and `abort` landing exactly the writes whose closures ran,
and awaited tasks beside `spawn_blocking` writers.

## Measurements

`shop` now writes with `try_commit_with_async` by default, where it used a pool of 32 writer
threads, and `--writes pool:32` and `--writes blocking`, which uses `spawn_blocking`, still select
the old ways. `shop-load` against each, fsync on NVMe, median of three alternating 5 second runs on
32 cores shared with other work. The `spawn_blocking` rows come from an earlier set of runs
alternated with the same pool, which measured within 5% of the figures here except 84,790 ops/s at
32 connections of the read-heavy mix. Threads is the most the server had during a run.

| Mix of reads, cart adds and orders | Connections | Writer | Ops/s | p50 | p99 | Read p99 | Write p50 | Write p99 | Threads |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0, 70, 30 | 32 | pool of 32 | 12,772 | 2.46 ms | 5.17 ms | | 2.46 ms | 5.21 ms | 66 |
| | | `spawn_blocking` | 12,919 | 2.39 ms | 5.15 ms | | 2.40 ms | 5.15 ms | 67 |
| | | awaited | 15,835 | 1.82 ms | 4.68 ms | | 1.85 ms | 4.78 ms | 35 |
| 0, 70, 30 | 128 | pool of 32 | 12,365 | 9.78 ms | 17.95 ms | | 9.79 ms | 17.90 ms | 66 |
| | | `spawn_blocking` | 11,022 | 11.42 ms | 23.35 ms | | 11.43 ms | 23.37 ms | 183 |
| | | awaited | 15,034 | 8.19 ms | 17.70 ms | | 8.21 ms | 17.80 ms | 35 |
| 90, 8, 2 | 32 | pool of 32 | 103,818 | 0.05 ms | 3.41 ms | 0.16 ms | 2.48 ms | 5.38 ms | 66 |
| | | `spawn_blocking` | 86,357 | 0.06 ms | 3.91 ms | 0.47 ms | 2.75 ms | 6.34 ms | 66 |
| | | awaited | 126,371 | 0.06 ms | 2.55 ms | 0.35 ms | 1.68 ms | 4.74 ms | 35 |
| 90, 8, 2 | 128 | pool of 32 | 102,397 | 0.05 ms | 16.13 ms | 0.19 ms | 11.53 ms | 25.46 ms | 66 |
| | | `spawn_blocking` | 94,763 | 0.05 ms | 15.74 ms | 0.17 ms | 12.87 ms | 26.39 ms | 162 |
| | | awaited | 130,183 | 0.06 ms | 13.19 ms | 0.39 ms | 8.87 ms | 17.86 ms | 35 |

Writes are the cart adds, and the orders match them within a few percent. The pool adds its 32
threads to tokio's 32 workers and the server's own, and `spawn_blocking` adds one for every write in
flight, where awaiting adds only the sync thread. The awaited server did 21 to 27% more work than
the pool at every mix and connection count, and its writes were faster at p50 everywhere and no
slower at p99. Reads at p99 rose by about 0.2 ms, because a write's closure and encode now run on
the executor between reads. The version that blocked on the lock did 3 to 15% less work than the
gate and had faster writes at 128 connections of the read-heavy mix, 3.65 ms at p50, but the slow
reads above.

`chat-load` flat out against `chat`, 300 feeds and 32 posters, median of three:

| Writer | Posts/s | Post p50 | Post p99 | Delivery p50 | Delivery p99 | Threads |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `spawn_blocking` | 3,913 | 8.17 ms | 15.66 ms | 8.55 ms | 16.03 ms | 131 |
| awaited, blocking on the lock | 4,674 | 6.57 ms | 14.07 ms | 6.91 ms | 14.40 ms | 35 |
| awaited | 5,157 | 6.50 ms | 10.40 ms | 6.88 ms | 10.86 ms | 35 |

`ferro-bench`'s `group --async 1` awaits each write with `block_on` on a thread per writer, which is
not how an async server runs but isolates the cost of the sync thread. Medians of four runs on the
shared machine: a lone writer cost the same under Fsync, and under Buffered took 21 to 22 µs at p50
on quiet runs against 19 µs blocking, since its commit now goes through a second thread. At 8 and 32
writers awaited writes did 4 to 10% fewer commits a second, and 21% fewer at 32 under Fsync, 25,950
against 32,857, because the sync thread wakes every waiting writer itself before it starts the next
sync, and here each wake unparks a thread where on an executor it queues a task. Buffered p99 at 8
writers fell from 1.4 to 2.1 ms to 0.4 ms. Leading a Buffered batch from the poll itself would save
the lone writer's 2 µs, but its checkpoint can fdatasync 64 MiB on an executor thread, so it was
left out.
