# Writers and concurrency

Readers never wait: a `Snapshot` is a value. Writers run one closure at a time on each branch, so a
transaction is serializable by construction.

## Branches and the writer lock

A closure runs under its branch's writer lock, so a long closure holds up the writers on that branch
only, and closures on different branches run at the same time. Each commit drafts its records under
that lock and writes its frame to the page cache under it too. Placing the records and writing the
refs take one lock for the whole file, so commit throughput across branches stops scaling at the
cost of that step, and on one branch, unless you opt in to `coalesce`, at the cost of the closure
and encoding, with only the fsync shared: one leader's fsync covers every frame queued. Writing the
refs is one small record per commit however many branches there are, so with one writer on each of
16 branches, single-key commits and `Durability::Buffered`, the file took about 124,000 commits a
second on the test machine on 2026-11-04, against 50,000 for one writer on one branch. A
reclamation cycle, and `receive` once its bundle has arrived, wait for the closure running on every
branch before they hold up all of them.

## Awaited writes

`update_async`, `commit_async`, `commit_with_async`, `commit_stamped_async`, `try_update_async`,
`try_commit_async`, `try_commit_with_async` and `try_commit_stamped_async` return a future that
resolves once the commit is published, which under `Durability::Fsync` means durable. It holds no
thread while it waits for the writer lock or the sync: one `ferro-sync` thread per open database,
started by the first awaited write, syncs each batch and wakes the futures in it through
`std::task::Waker`, so any executor works and ferro depends on no runtime.

```rust
let order = db
    .try_commit_with_async(|shop| -> Result<_, ShopError> {
        let left = shop.stock.get_mut(&id).ok_or(ShopError::NoProduct(id))?;
        *left = left.checked_sub(qty).ok_or(ShopError::SoldOut(id))?;
        Ok((format!("order {id}"), *left))
    })
    .await?;
```

The closure runs in the poll that finds its branch's writer lock free, on the thread that polls,
under the same lock as a blocking write. So it need not be `Send` or `'static`, the future is `Send`
when the closure and its result are, a panic unwinds out of that poll, and awaited and blocking
writers share one sync. If the lock is taken, the future waits in line and the writer that lets go
wakes the first in line, so futures get the lock in the order they asked for it, though a blocking
writer can get in ahead of them. A write dropped before its closure ran, polled or not, writes
nothing. Once the closure has run its outcome is decided: dropping the future gives up only the
answer, the commit lands as if awaited, and closing the database first writes and syncs it. So
whether a dropped write landed is exactly whether its closure ran. With `coalesce`, awaited closures
join the same groups as blocking ones. `restore` and the merges have no awaited form.

## Sharing a version between writers

```rust
let db = Options::new().coalesce(true).open::<Jobs>("jobs.ferro")?;
```

Every closure normally gets a version of its own, and with it its own write of every tree path it
touched. With `coalesce`, closures on one branch that queue up while a sync is running share one
write and one version. They still run one at a time, each on its caller's thread, so they need not
be `Send`, and each sees what the ones before it did. Each branch groups its own closures. An `Err`
or a panic rolls back that closure alone. On `examples/queue` at 32 workers, measured on
2026-11-03, that took jobs from 9,700 to 16,900 a second, bytes per job from 2,691 to 146 and p99
from 2.3 to 0.8 ms. A closure that runs long against an fsync does not gain: at 50 µs it lost 20%
of its throughput with 8 writers and 19% with 32 on 2026-11-04. A closure waits for another that
is still on its way to the lock only while handing over has lately cost less than writing and
syncing a version. So where a sync is nearly free, on tmpfs or with `Durability::Buffered`, and the
cores are busy, `coalesce` runs at about the speed of plain group commit; it used to run up to ten
times slower there. See [Coalescing](../internals/coalescing.md#waiting-for-a-thread-on-its-way).

What you give up is a version per closure. `Committed::version` and `time_ms` are the group's, `at`
cannot show the state between two closures of a group, and `history_of` reports the group's net
change once. A `Stamp` names the group's version and time too, the same for every closure in it, so
`at(stamp.version())` shows closures that ran after yours. The first closure to read `time_ms` fixes
the group's time, and if that closure then fails, the next one to read it fixes it again. The
commit's `message` holds every closure's message joined by newlines, and `messages()` lists them,
each whole even if it has newlines of its own. `changed` says the group made a new version, so a
closure that changed nothing reports `true` if another closure in its group did. If the group cannot
be written, every closure in it gets `Error::GroupFailed`. `restore` and `merge_with` always commit
alone, and a writer alone behaves as without the option. `examples/queue` uses it.

## Writing from inside a closure

A write from inside a closure fails with `Error::Reentrant` wherever it could deadlock, rather than
hanging. That covers merge closures, a watcher test run inside a closure, and a `Drop` that runs
under a writer lock too. A closure may write to another file, and without `coalesce` to a branch of
its own file whose name sorts after its own, since cycles and `receive` take branches in name order.
Everything else on its own file is refused, and so is a write into another file while a closure
there is writing back into this one. A closure that waits on a channel or a watcher for another
thread's commit can still deadlock, because ferro cannot see the wait: on its own branch, on a
branch that sorts before its own while a cycle or `receive` waits for this closure, and with
`coalesce` also when that commit's closure has already run.

## Several processes

Other processes can read while one writes: `Options::new().read_only(true)` takes no lock, and
`refresh` picks up new commits, or a follower does so on its own, as described under [Following
other processes](#following-other-processes). A second writer gets `Error::Locked` at once, or with
`Options::lock_wait(duration)` waits up to that long for the first to close, checking every 10 ms.

Several processes can also write, if every one of them opens the file with `shared`:

```rust
let db = Options::new().shared(true).open::<Jobs>("jobs.ferro")?;
```

Each commit then lands on top of the newest one from any process. A closure runs under a lock on its
branch that holds across processes, so it starts from the newest commit on its branch and never runs
again, and closures on different branches run at the same time in different processes as they do in
one. A process sees the others' commits when it next commits to the file, attaches a branch or calls
`refresh`, which also fires its watchers. Until then `read` shows what it last saw, as on a
read-only handle, unless it follows the file. Under `Durability::Fsync` one fdatasync covers the
commits every process has queued, as group commit does within a process.

The option is off by default. A writer without it gets `Error::Locked` while shared writers have the
file open, and they get it while it does. On one process it costs about 2 µs a commit for its
locks: one writer made 46,100 single-key Buffered commits a second with it and 51,300 without, on
2026-11-07 on eight cores. With fsync, 8 processes made 10,126 commits a second on one branch where
8 threads of one process made 10,014. Under `Durability::Buffered` processes on one branch take
turns, and each reads back the nodes the last one rewrote, where threads share one cache: 2
processes made 35,759 and 8 made 24,742, against 43,581 and 38,806 for 2 and 8 threads. On branches
of their own 4 processes made 87,512 and 4 threads 90,661.

A reclamation cycle runs in one process at a time, and the others move to the new file at their next
commit or `refresh`, or as soon as their follower sees the rename. A process killed in the middle of
a commit holds up the others only until the operating system releases its locks, and nothing it
acknowledged is lost. It needs open file description locks, so it runs on 64-bit Linux only, and the
processes must share one machine and a local file system. Elsewhere, Windows included, the open
fails with an `Unsupported` I/O error.

A held `Snapshot` or `Map` keeps the file it was read from open, though not the writer lock, so a
new writer can open once the last `Db` handle is gone. How it works is in [Writers in several
processes](../internals/processes.md).

## Following other processes

A read-only handle or a shared writer opened with `follow` keeps up with the other processes on its
own, so a process that serves reads or watches another's writes needs no refresh loop:

```rust
let db = Options::new().read_only(true).follow(true).open::<Jobs>("jobs.ferro")?;
for jobs in db.watch() {
    println!("{} jobs waiting", jobs.len());
}
```

A thread of its own, `ferro-follow`, waits until the file may have changed, reads the newest
superblock, and refreshes when that names a commit the handle has not seen, so `read` shows each
commit soon after the writer acknowledges it and the watchers of every branch it moved fire. It
follows the file a reclamation cycle renames over the path too. On a shared writer it catches up
only with what a superblock names, and waits for a lock another process holds only to open the
file a cycle renamed, to sync its own process's commits, or, under `Durability::Fsync`, to sync a
commit that a process using `Durability::Buffered` named without syncing it. Under `Buffered` it
syncs the file once every `checkpoint_bytes` it follows, as every writer does. A writer without
`shared` is the only writer and starts no thread.

On 64-bit Linux inotify wakes the thread when the file is written or a file is renamed over it. On
the machine the other figures here come from, shared with other work, a commit from another
process reached a watcher at a median of 0.17 ms after the writer acknowledged it, and 0.9 ms at
p99, and an idle follower woke once a second for 3 ms of CPU a minute. Under a writer making
96,000 commits a second it refreshed once a millisecond, for 3% of a core, or 6% on a shared
writer, and the writer made as many commits as with no follower. Elsewhere, Windows included, or
where inotify cannot be had, as once the user's inotify instances run out, it looks every 10 ms
instead: a commit waited 5 ms at the median, and an idle follower woke 100 times a second for 0.1
to 0.5% of a core. `Db::following` says which it does. The `shop` example's replicas follow their
writer this way.

The thread ends when the last `Db` handle on the file is dropped, and keeps neither the file nor a
lock, as watchers do. That drop waits for a look in progress, which on a shared writer that is
opening the file a cycle renamed can mean waiting for another process's lock, as an open does. A
look that fails, as when the file was removed or its superblocks are damaged, changes nothing:
`read` shows what it showed, watchers go on waiting, and the thread looks again at the next change.
`Db::following` reports the error until a look succeeds, and `refresh` still works, to look at once
or to get the error itself. A shared writer whose writes failed is poisoned until it is reopened,
so its follower reports `Error::Poisoned` and stops. How it works and what it costs is in
[Followers](../internals/processes.md#followers).

## How it works

- [Writers and the commit path](../internals/commit-path.md): lanes, `tip`, group commit and
  drafting.
- [Coalescing](../internals/coalescing.md), [Awaited writes](../internals/async-writes.md) and
  [Writes from inside a closure](../internals/reentrancy.md).
- [Commits of a few rows](../internals/commit-cost.md): where a small commit's CPU time goes.
- [Writers in several processes](../internals/processes.md): the locks, catching up, superblocks
  and cycles across processes, and [followers](../internals/processes.md#followers).
