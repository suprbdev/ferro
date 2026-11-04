# Writers and concurrency

Readers never wait: a `Snapshot` is a value. Writers run one closure at a time on each branch, so a
transaction is serializable by construction.

## Branches and the writer lock

A closure runs under its branch's writer lock, so a long closure holds up the writers on that branch
only, and closures on different branches run at the same time. Each commit drafts its records under
that lock and writes its frame to the page cache under it too. Placing the records and writing the
refs take one lock for the whole file, so commit throughput across branches stops scaling at the
cost of that step, and on one branch, unless you opt in to `coalesce`, at the cost of the closure
and encoding, with only the fsync shared: one leader's fsync covers every frame queued. A
reclamation cycle, and `receive` once its bundle has arrived, wait for the closure running on every
branch before they hold up all of them.

## Awaited writes

`update_async`, `commit_async`, `commit_with_async`, `try_update_async`, `try_commit_async` and
`try_commit_with_async` return a future that resolves once the commit is published, which under
`Durability::Fsync` means durable. It holds no thread while it waits for the writer lock or the
sync: one `ferro-sync` thread per open database, started by the first awaited write, syncs each
batch and wakes the futures in it through `std::task::Waker`, so any executor works and ferro
depends on no runtime.

```rust
let order = db
    .try_commit_with_async(|shop| {
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
join the same groups as blocking ones. The stamped forms, `restore` and the merges have no awaited
form.

## Sharing a version between writers

```rust
let db = Options::new().coalesce(true).open::<Jobs>("jobs.ferro")?;
```

Every closure normally gets a version of its own, and with it its own write of every tree path it
touched. With `coalesce`, closures on one branch that queue up while a sync is running share one
write and one version. They still run one at a time, each on its caller's thread, so they need not
be `Send`, and each sees what the ones before it did. Each branch groups its own closures. An `Err`
or a panic rolls back that closure alone. On `examples/queue` at 32 workers that took jobs from
9,700 to 16,900 a second, bytes per job from 2,691 to 146 and p99 from 2.3 to 0.8 ms. A closure that
runs long against an fsync does not gain: at 50 µs and 8 writers it lost 16% of its throughput. A
closure waits for another that is still on its way to the lock only while handing over has lately
cost less than writing and syncing a version. So where a sync is nearly free, on tmpfs or with
`Durability::Buffered`, and the cores are busy, `coalesce` runs at about the speed of plain group
commit; it used to run up to ten times slower there. See
[Coalescing](../internals/coalescing.md#waiting-for-a-thread-on-its-way).

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

Other processes can read while one writes: `Options::new().read_only()` takes no lock, and `refresh`
picks up new commits.

Only one process may hold the writer lock. A held `Snapshot` or `Map` keeps the file it was read
from open, though not the writer lock, so a new writer can open once the last `Db` handle is gone.

## How it works

- [Writers and the commit path](../internals/commit-path.md): lanes, `tip`, group commit and
  drafting.
- [Coalescing](../internals/coalescing.md), [Awaited writes](../internals/async-writes.md) and
  [Writes from inside a closure](../internals/reentrancy.md).
- [Commits of a few rows](../internals/commit-cost.md): where a small commit's CPU time goes.
