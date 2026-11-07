# Watchers

How a branch's feed hands snapshots to watchers, where filtered and async watchers run their tests,
and what that costs a commit. The API is in [Watchers](../guide/watchers.md).

## The feed

Each branch has one `Feed`: a mutex over the newest published snapshot and a publish count, and a
condvar. Publishing stores one snapshot and wakes waiters whatever the number of watchers. A
`Watcher` remembers the count it last saw, so a snapshot is never queued, and a consumer that falls
behind gets the newest one and uses `diff` for what it skipped, which costs O(changes). With the
channel this replaced, a watcher that never read grew RSS by 147 MB over 100,000 order commits
against 53 MB with no watcher. Now it is 53 MB either way, and commit throughput matches. The count
is compared rather than versions so that a read-only handle stays right when a branch is dropped and
forked again at a lower version. The feed starts out holding the branch's snapshot from when the
branch was attached, so a watcher made before any commit has a snapshot to compare against.

Another process's commits reach the feed when this process catches up and syncs them, at `refresh`,
or, with `Options::follow`, when its `ferro-follow` thread does either, as described under
[Followers](processes.md#followers). Publishing from that thread is the same store and wake as from
a commit, so the watch thread and the waiting watchers cannot tell the difference.

## Closing a feed or a watcher

A `Branch` closes its feed when it drops, which is when the last `Db` handle goes, because `Shared`
holds every branch. Closing wakes parked watchers, the filtered and async ones through the watch
thread described below, and drops the held snapshot. A `Watcher` holds only the feed and its gate,
so a parked one keeps neither the file nor the writer lock, except through the snapshot a
`watch_changes` watcher keeps to compare against, described under [Comparing against what was
seen](#comparing-against-what-was-seen). The price is that a snapshot nobody had taken by then is
lost.

`Watcher::closer` returns a `Closer`, a cloneable handle that ends that watcher and its clones from
any thread. It sets a flag under the feed's mutex and wakes that watcher's parked threads and
futures, so stopping a set of workers no longer takes a commit. `recv` then fails the way it does
when the feed closes, and any other watcher of the branch carries on. A closer holds only the feed
and the watcher's gate as well. A wake with no close has no user here: a timeout already covers what
it would do, and the queue only ever needed the end.

## Filtered watchers and the watch thread

`Db::watch_when(test)` is a watcher that sleeps through commits that do not concern it. Its tests
run on a `ferro-watch` thread that the feed starts when a filtered watcher first parks or any
watcher first waits as a future. A watcher that waits is listed in the feed once, by a `Weak`, and
counts as waiting while a thread is parked on its condvar or a future holds a waker in it.
Publishing stores the snapshot, bumps the count and wakes unfiltered threads under the feed's mutex,
and returns a `Notice`. When the notice drops, after the publisher has let go of every lock, it sets
a `due` flag and unparks the watch thread, but only if some watcher is waiting. That is a load, a
swap and at most one unpark whatever the number of watchers, and nothing at all while nobody waits.
The watch thread clears `due`, takes the newest snapshot and the list, tests that snapshot for each
listed watcher that is waiting, wakes those that pass, and forgets the entries whose watcher is
gone. Commits that land during a pass set `due` again, so one pass stands for all of them. A pass
marks a passing watcher approved for that count, so the woken thread skips its own test unless
something newer has landed. A `watch_changes` watcher always tests again on its own side, because
its reference may have moved after the pass read it. A panic is caught on the watch thread and
counts as a pass, so that the watcher wakes and hits the same panic on its own thread.

Testing on another thread cannot reorder what a watcher sees: snapshots are stored in commit order,
a pass only decides whom to wake, and a woken watcher takes the newest snapshot under the mutex and
tests it if it is not the approved one, so nothing that landed while it was busy is missed. Since no
test runs on a committing thread, a test may commit, refresh or drop a branch: its own commit only
sets `due` again. Before tests moved to the notice, a committing test deadlocked on the leader's own
`syncing` flag or on `tip`, and while they ran in the notice, the call that published returned only
after them, so every commit paid for every parked filter. Now a slow test holds up no commit, only
the other filtered watchers of its branch.

## Measurements

Measured with `ferro-bench --bin watchers`: buffered single-key commits into a 100k-entry map with N
watchers parked in `recv` whose test `snapshot.get(&key).is_some()` never passes, then the time from
starting a commit to a passing watcher's return, on a 32-core machine shared with other work. Before
and after ran back to back. A second pair gave the same picture: commits of 14 to 16 µs after,
against 64 and 547 µs before at 1,000 and 10,000 watchers, and delivery within 1 µs of before with
no other watchers, 13 µs slower at 1,000 and 45 µs slower at 10,000.

| Parked watchers | Commit, before | Commit, after | Delivery p50, before | Delivery p50, after |
| ---: | ---: | ---: | ---: | ---: |
| 0 | 14.5 µs | 14.5 µs | 21.5 µs | 21.1 µs |
| 1,000 | 80.2 µs | 16.8 µs | 98.8 µs | 82.1 µs |
| 10,000 | 591 µs | 14.5 µs | 572 µs | 526 µs |

Delivery stays where it was because the work is the same, one test per waiting watcher, done by one
thread either way. What changes is who waits for it.

Waking every waiting watcher to test on its own thread was measured on the same bench and rejected,
both waking from the committing thread and from the watch thread. The wakes cost as much as the
tests did, and the herd then fights the publisher for the feed's mutex: at 1,000 watchers a commit
took 614 µs and 432 µs and delivery 1.7 ms at p50, and at 10,000 a commit took 10.9 ms and 5.5 ms
and delivery 19 ms and 24 ms.

## Sharing a long pass

A long pass is shared. Once a pass alone takes over a millisecond and at least 1,024 watchers are
listed, the watch thread hands equal shares of at least 512 watchers to helper threads over
channels, one per four cores and between one and seven, and tests one share itself. It keeps a
running cost per watcher for passes alone and shared, uses whichever is cheaper, and tries the other
every 64th pass. Sharing pays for heavy tests: with 3,000 chat feeds at 1,000 posts a second,
delivery fell from 4.4 ms at p50 alone to 0.9 ms shared. It loses for cheap tests that read the same
nodes of one snapshot from every thread: 10,000 single-key `get`s took 0.58 ms alone and 1.2 ms over
eight threads. With tests that read nothing it halved, so the choice is measured rather than fixed.

## Waiting as a future

`Watcher::changed` is `recv` as a future, built on `std::task::Waker` alone, so it runs on any
executor. A future that finds nothing new stores its waker in the watcher's gate while it still
holds the feed's mutex, so a commit between its look and its registration cannot slip past, and
lists the gate for the watch thread, which wakes it as it wakes a parked thread. Dropping the future
removes its waker. A `Watcher` keeps its place in an atomic, so that it is `Sync` and the future is
`Send`. A plain `watch` waited on as a future is woken through the watch thread too, so a commit
never runs an executor's wake code.

## Comparing against what was seen

`Db::watch_changes(test)` passes the test the snapshot the watcher returned last as well as the new
one. The first reference is the feed's snapshot, taken in the same mutex hold as the count, so it
cannot disagree with the place, and `Watcher::seen` returns it. The chat example used to keep its
marks in a mutex shared with its test and hold that mutex across `db.read()` for the same guarantee,
and to refresh them before each `recv`. The reference lives in the gate, so clones share it and a
return by one moves it for all, only ever forward by count. It holds its snapshot, and so its file
across a cycle, until the next return, like the snapshot a consumer keeps to diff against. A closed
feed holds no snapshot, so a watcher made on a dropped branch's handle takes the one that handle
reads, which is the last its branch had there and never changes. It ends at once, but `seen` still
answers as its documentation promises; the first version left it `None`, and the `Watcher::changed`
example, which expects it, panicked
(`a_changes_watcher_on_a_dropped_branch_has_seen_what_its_handle_reads`).

## Idle workers in `queue`

`queue` used to wake every idle worker on every commit, and to commit a dummy `epoch` change at
shutdown. Now workers filter on a ready job in their own queues, and the reaper on something falling
due sooner than it planned, against a value it stores before it parks. With 8 and 32 idle workers a
commit used to cost 12 and 47 voluntary context switches, and then cost 2, as with no workers at
all. The time from enqueue to handler stayed within run-to-run noise, 2.0 to 2.5 ms at the median.
The watch thread adds one switch per commit while workers are idle, its own park after each pass: in
`ferro-bench --bin watchers --watchers 0,8,32 --test false`, a buffered commit costs no voluntary
switch with no watchers and one with 8 or 32.
