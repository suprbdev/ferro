# Coalescing

How `Options::coalesce` lets closures share a flush, a frame and a version. The user-facing
trade-off is in [Writers and
concurrency](../guide/concurrency.md#sharing-a-version-between-writers).

Closures run one at a time, but each still encoded, hashed and compressed a whole tree path and
wrote a frame, though the next closure rewrote the same paths. On `queue` at 8 writers the lock was
held for 37 µs a commit: 5 for the closure, 20 to encode and hash the tree, and 12 for the log
entry, the refs, the frame and reloading the tip. At 32 writers, with more cache misses, it was 64
µs, which is what capped commits at about 15,000 a second, and bytes per job grew with the writers
because more rows were in flight.

## What a group gives up

`Options::coalesce(true)` lets closures that queue up behind one another share a flush, a frame and
a version. It is off by default, because it gives up what ferro otherwise promises, a version for
every closure:

- A version is a group. `Committed::version` is the group's, so several closures can return the same
  one. `at` cannot show the state between two closures of a group, because it was never written.
- `Commit::message` holds the group's messages joined by newlines, and `Commit::messages` lists
  them. A message may contain newlines itself, so the log entry records where each one ends: one
  flag bit marks a commit made alone, whose message is one message whatever it contains, and another
  marks a group, followed by the byte length of each message. Entries written before these bits
  existed have neither, and `messages` splits them on newlines as it always did, which is the only
  reading they allow.
- `history_of` and `Log` see groups. A row changed twice in a group shows once, with its last value,
  and a change undone inside a group does not show at all.
- Closures still run in order, each seeing what the ones before it did.
- `Committed::changed` says the group moved the version. A closure that changed nothing, in a group
  where another did, reports `true`. Telling them apart needs a hash of the dirty tree after every
  closure, which is the cost this removes. A group of no-ops writes nothing and reports `false`.
- A `Stamp` is the group's. Its version is the group's base version plus one, and its time lives in
  the group: the first closure to call `time_ms` fixes it, and later closures read the same value. A
  closure that fails or panics puts the time back unset if it was the one that set it, as it puts
  back the working copy, so a group whose only reader failed keeps no claim on its millisecond.
- `restore` and `merge_with` commit alone, since `restored` and `merged` belong to one version. They
  and `fork` flush their branch's open group before they start, `drop_branch` flushes the dropped
  branch's, and a reclamation cycle flushes every branch's.
- A group belongs to one branch, and each branch can have one open. A closure on another branch that
  leaves its own group open seals any other branch's group that nobody is on the way to, as
  described under [A lock per branch](commit-path.md#a-lock-per-branch).

## Passing the group between closures

The combiner is a lock passed between waiters. The group's working copy lives in its branch's lane
between closures and each closure takes it under that lane on its own thread. So there is no `Send`
bound, `update` closures keep borrowing local state, and a panic unwinds where it should. Cloning
the working copy first is the checkpoint, O(1) per collection, and an `Err` or a panic puts it back.
That closure still waits for the group, since it may have read state that never becomes durable,
except that a panic does not wait, as it never did.

A closure ending under the lock does not flush if someone else will finish the group: either a sync
is running, whose leader wakes one member when it ends, or a thread has announced it is on its way
to the lock and waiting for it has lately been worth it, which is described below. Otherwise it
flushes. So a group closes when nobody is coming or a sync ends, which gives one flush per fsync
however many writers there are, and a lone writer flushes at once, down the same path as before. A
group stops at 256 closures, which bounds what the first of them waits for.

## Waking members

Members park on the group's seal and the leader unparks each one. The first version waited on a
condvar, and notifying a condvar moves its waiters to the mutex and wakes them one at a time: that
chain alone held 32 `queue` writers to 11,600 jobs a second with a p99 of 1,107 µs, against 15,600
and 753 µs once members were unparked directly, median of three. The same change on the plain path
gained 3% at 8 writers and nothing at 32, where the writer lock is the bottleneck, so it was left
out.

A member that wakes with its group still open, no sync running and nobody on the way takes the lane
to seal the group itself, and while it waits for the lane it counts as on its way. In that time the
closure under the lane can seal the group without it and lead its sync, and a later closure can
start a new group and leave it open for the member, parked because the member is on its way. The
member seals the group it finds, as it should, since it is not a closure that would join it. It
used to then find its own group settled and return, so nobody led the new group's sync, and its
closures slept until another commit came. At the end of a run none did, and the booking example's
`a_coalesced_receipt_lands_on_the_group_that_paid` hung (#78). Copies of its test binary run at once
on 32 cores beside 16 busy loops, 16 of them free to move and 8 each pinned to one core, hung 4
times in 65,618 runs over 35 minutes. Each copy was started with `prctl(PR_SET_PTRACER,
PR_SET_PTRACER_ANY)`, so that `gdb -p` could attach under Yama's ptrace scope 1, and in every hung
process it found each thread in a futex wait: the rate writer parked in `await_group`, once with a
buyer beside it, the other buyers done, and the test joining one of the two.

A member that seals a group other than its own now wakes one of that group's members, as `fork`,
`restore` and a panicking closure do when they seal a group they will not wait for, and that member
leads the sync. The same copies of the fixed binary, 8 pinned for 60 minutes and 16 free for 35,
ran 73,375 times without a hang. `a_member_that_seals_a_newer_group_on_its_way_wakes_that_group`
stops a member on its way to the lane, seals and syncs its group with a `fork`, and starts the new
group behind it. Without the wake the new group's closure never returned, in 6 runs of 6.

## Waiting for a thread on its way

Waiting for a thread on its way to the lock used to be unconditional, and where a sync is nearly
free that made `coalesce` far slower than plain group commit (#29). The thread on its way is usually
parked in the mutex, so the group can close only once the scheduler runs it, and the closure that
waits for it parks too, so nothing uses the lock in between. Every closure then pays at least one
wake-up to save a version. On a quiet machine a wake-up costs a few µs and the trade still pays,
since writing a version of one closure took about 16 µs in the benchmark below. When the cores are
busy a wake-up waits for a time slice, about a millisecond, while a sync on tmpfs or with `Buffered`
takes 1 to 2 µs. Counters in a build of the `group` benchmark, `Buffered` on tmpfs with 8 writers on
4 cores shared with other work, showed it: 29% of closures waited for a follower, a parked member
waited 2.5 ms on average, the writer lock was held 19% of the time, and a sync took 1.6 µs. Sampling
`/proc` put the coalesced writers asleep in `futex_wait` 53% of the time against 16% without the
option, and `schedstat` gave about 1 ms of run-queue wait per time slice.

`Shared` now keeps three costs: a running mean of how long a sync takes, one of how long a group's
version takes to write under the lock, and an estimate of how long a thread on its way takes to pick
up a group handed to it, timed from the handover to its `join_group`. A closure waits for a thread
on its way only while that estimate is below the other two together, which is what the thread costs
if it writes a version of its own instead. A running sync is waited for as before, since its leader
wakes a member when it ends and the members would wait for it anyway. The means move an eighth of
the way to each sample. The estimate jumps to any slower sample and moves an eighth of the way to a
faster one, so one slow handover stops the waiting. While it stops it, each closure that declines to
wait takes 1/1024 off it, so on a busy machine about one closure in four thousand waits again and
measures, and a machine that has gone quiet is noticed within as many closures. Each of these
updates is a compare-and-swap loop on the current value, so a sample from a thread that finishes at
the same moment as another is not lost. Taking 1/256 off instead let the waits come back four times
as often, and at 2 writers on busy cores raised p99 from 78 to 682 µs. On NVMe a sync takes about
400 µs, so groups still hold every writer.

`ferro-bench --concurrent-only`, 8 writers of 200 commits beside 8 readers that never sleep, commits
a second, median of 15 alternating runs:

| Where | Plain | `coalesce` before | After |
| --- | ---: | ---: | ---: |
| tmpfs, the readers and writers on 8 cores | 34,974 | 2,549 | 31,486 |
| tmpfs, all 32 cores | 41,716 | 78,310 | 67,546 |
| NVMe, all 32 cores | 9,866 | 15,729 | 15,710 |

The `group` benchmark, closures a second, median of three alternating runs, with the closures per
version before and after:

| Where | Writers | Plain | `coalesce` before | After | Per version |
| --- | ---: | ---: | ---: | ---: | --- |
| tmpfs, 4 cores beside 4 busy loops | 2 | 26,575 | 4,455 | 29,012 | 1.1 → 1.0 |
| | 8 | 28,698 | 7,610 | 31,758 | 1.3 → 1.0 |
| | 32 | 24,284 | 25,388 | 26,808 | 1.3 → 1.0 |
| NVMe, `Buffered`, the same | 2 | 32,798 | 7,541 | 33,978 | 1.1 → 1.0 |
| | 8 | 29,970 | 8,133 | 28,664 | 1.3 → 1.0 |
| | 32 | 29,469 | 22,788 | 28,674 | 1.4 → 1.0 |
| NVMe, the same | 8 | 5,041 | 6,773 | 6,499 | 4.6 → 4.1 |
| | 32 | 7,228 | 20,570 | 19,229 | 16.6 → 16.4 |
| tmpfs, all 32 cores | 8 | 42,942 | 55,672 | 54,532 | 8.0 → 6.2 |
| | 32 | 35,656 | 49,257 | 54,960 | 32.0 → 31.9 |
| NVMe, `Buffered`, the same | 8 | 42,299 | 55,809 | 55,309 | 8.0 → 6.2 |
| | 32 | 35,816 | 49,339 | 49,348 | 31.9 → 31.4 |
| NVMe, the same | 8 | 10,111 | 14,884 | 14,267 | 8.0 → 8.0 |
| | 32 | 32,354 | 34,949 | 34,874 | 31.4 → 31.9 |

One writer, and 2 writers on NVMe, moved by less than the noise. What the rule gives up is some of
the gain on a quiet machine, where one slow handover, a thread that happened to be descheduled,
stops the waiting for some thousands of closures. Of 12 counted runs of the concurrent benchmark on
32 cores, the four that came out under 53,000 commits a second each had exactly one handover over
100 µs, and the other eight had none. On 4 idle cores the `group` benchmark gave 62,985 and 72,145
closures a second at 8 and 32 writers before, against 55,719 and 62,418 after, though later runs put
the after figures within 6%. Feeding the estimate the lesser of the last two handovers, so that one
slow one is ignored, kept that gain but fell to 54% of plain with the concurrent benchmark on 8
cores, which suggests that handovers there come fast and slow by turns. A running mean of the
handovers fell to 79% of plain at 8 writers on 4 busy cores with a weight of an eighth, and to 56%
with a weight of a sixty-fourth.

## Failures

If the group cannot be written, every closure in it gets `Error::GroupFailed`, or `Error::Poisoned`
if the store is, and its changes are gone, including those of closures that did nothing wrong. A
panic in `Data::encode` is one of those failures: it cannot be charged to one closure, so it is
caught and reported to all of them. Nothing else about failure changes: a failed sync fails the
group behind it, and a closure that waits for another thread's commit to be acknowledged now
deadlocks, because that commit may be waiting for this closure to end. Awaiting a write's future
there fails with `Error::Reentrant` instead, as described under [Writes from inside a
closure](reentrancy.md).

## Measurements

`queue-bench throughput`, fsync, each job an enqueue, a lease and an ack, median of five alternating
runs against the tree before this change:

| Jobs waiting | Writers | Jobs/s | Bytes per job | Ack p50 | Ack p99 |
| --- | --- | --- | --- | --- | --- |
| 1 | 1 | 862 → 894 | 1,185 → 1,173 | 386 → 354 µs | 454 → 425 µs |
| 1 | 8 | 3,457 → 5,707 | 1,848 → 283 | 763 → 453 µs | 862 → 797 µs |
| 1 | 32 | 9,721 → 16,926 | 2,691 → 146 | 959 → 611 µs | 2,270 → 796 µs |
| 1,000 | 8 | 3,529 → 5,306 | 4,293 → 647 | 742 → 485 µs | 882 → 615 µs |
| 1,000 | 32 | 5,930 → 15,286 | 4,972 → 234 | 1,786 → 659 µs | 3,755 → 857 µs |

That is one version for every 8 closures at 8 writers and every 32 at 32, so bytes per job fall by
85 to 95%, against 93% for batching ten jobs a commit by hand. `shop-load` against
`shop --coalesce`, with its 32 writer threads, median of three:

| Mix of reads, cart adds and orders | Connections | Writes/s | Write p99 | Bytes per write |
| --- | --- | --- | --- | --- |
| 90, 8, 2 | 32 | 10,267 → 12,531 | 5.4 → 3.7 ms | 2,905 → 1,104 |
| 90, 8, 2 | 128 | 10,909 → 12,592 | 16.3 → 14.3 ms | 2,891 → 900 |
| 0, 70, 30 | 32 | 12,034 → 14,223 | 5.4 → 3.4 ms | 2,967 → 973 |
| 0, 70, 30 | 128 | 14,616 → 14,579 | 14.7 → 13.4 ms | 3,041 → 970 |

At 128 connections throughput stays where it was, so something besides the writer lock bounds it
there. Read p99 rose from 0.15 to 0.21 ms at 32 connections. `ferro-bench`'s concurrent metric, 8
writers and 8 readers on 1M keys, went from 10,300 to 15,650 commits a second, median of five, and
with the option off it measured 9,900, within that run-to-run spread. A writer alone costs the same
with the option on: 2,550 fsync commits a second against 2,390 with it off and 2,590 before, and
46,000 Buffered against 45,900 and 47,900, a difference the Buffered checkpoint accounts for.

`ferro-bench`'s `group` tool, single-key updates spread over 100,000 keys, median of three:

| Durability | Closure | Writers | Closures/s | Bytes per closure | p99 |
| --- | --- | --- | --- | --- | --- |
| fsync | empty | 8 | 10,300 → 14,800 | 2,308 → 1,536 | 995 → 699 µs |
| fsync | empty | 32 | 31,300 → 36,200 | 2,427 → 1,279 | 2,783 → 2,550 µs |
| fsync | 50 µs | 8 | 9,980 → 8,350 | 2,306 → 1,510 | 1,214 → 1,151 µs |
| fsync | 50 µs | 32 | 12,360 → 12,100 | 2,345 → 1,209 | 4,769 → 4,782 µs |
| Buffered | empty | 8 | 40,900 → 49,900 | 2,400 → 1,610 | 3,204 → 292 µs |
| Buffered | empty | 32 | 32,700 → 48,200 | 2,435 → 1,304 | 8,673 → 818 µs |
| Buffered | 50 µs | 32 | 11,400 → 13,000 | 2,343 → 1,212 | 33,287 → 2,679 µs |

Random keys share only the top of each path, so a flush saves less than on `queue`. With a 50 µs
closure and fsync it does not win: 8 writers lost 16% of their throughput. Every writer ends up in
the one group, so all of them run their closures, 400 µs between them, while the disk sits idle, and
then all of them wait for the same sync, where the plain path runs one writer's closure while
another's frame syncs. Buffered commits gain at every size, and their p99 falls by an order of
magnitude, because the leader no longer serializes a flush per writer.

## Measured and left out

- One version per closure in a shared frame. That keeps history and needs no option, but only the
  frame write, the refs and the reload of the tip, 10 of those 37 µs, happen once per frame, and the
  tree path is the rest. Bytes barely move, since a frame header and the refs are about 130 of the
  233 bytes that a commit of one field costs. An upper bound of +37% on throughput, where coalescing
  gave 50 to 158% on `queue`, did not justify an open transaction that has to read its own unwritten
  objects.
- Sharing intermediate nodes between versions. There is nothing left to share: objects are addressed
  by hash, so a commit already writes only what differs from the version before. Every intermediate
  version must reference a root, and that root every node on the paths it changed. A root cannot be
  recovered without writing it, since a closure is code, not data, and `T` has no generic diff, so
  the only way to keep the bytes down is to store them smaller. Compressing each group's frames as
  one LZ4 block instead of object by object, simulated over 60,000 real `queue` frames, took 1,537
  bytes per closure to 464 at 8 closures a block and 356 at 32. That keeps history and saves no CPU,
  because each intermediate root must still be hashed, and a read would have to decompress the
  block, so offsets would stop naming records. It was not built.
- A combiner thread that runs other threads' closures. It saves the lock hand-over, which costs more
  than most closures here, but needs `Send` on every closure and its result.
- Spinning on arrival before parking. 2,000 spins, about 25 µs, gained 8% on `queue` at 8 writers
  and 6% at 32, and 3 and 10% on `group`, median of four alternating runs, with p99 better in three
  cases and 30% worse in one. That is not clear enough to burn a core in every waiting writer.
- Other rules for closing a group. Sealing whenever no other thread is on its way, even with a sync
  running, cost `queue` 39% of its throughput at 8 writers and 14% at 32, with 53 to 72% more bytes.
  Sealing as soon as no sync is running, unless Buffered, cost it 37 and 21%. Sealing once a group
  had been open for half the last sync, though more closures were coming, helped 50 µs closures by
  10% at 8 writers and 38% at 32, and cost `queue` 7% and 19% of its throughput and up to 75% more
  bytes. Nothing tried won everywhere, so the simplest rule stays.
