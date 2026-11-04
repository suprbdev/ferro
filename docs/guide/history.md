# History, versions and branches

Every commit makes a new version of your root value. Each commit on a branch takes the next version
number, and version 0 is the value before the first commit.

## Reading old versions

```rust
let old = db.at(12)?;
let then = db.as_of(unix_ms)?;
for commit in db.log().values().rev().take(20) {
    println!("v{} {}", commit.version, commit.message);
}

for change in old.orders.diff(&db.read().orders) {
    println!("{:?} changed", change.key());
}

let life: Vec<_> = db
    .history_of(|shop| shop.orders.get(&order_id))
    .while_some()
    .collect();

db.restore(&old)?;
```

`diff` skips identical subtrees by hash, so it costs O(changes), not O(size). `restore` commits an
old value as a new version, so history is never rewritten.

`as_of(t)` returns the newest commit on the branch whose `time_ms` is at most `t`, or version 0
before the first. A commit's time is the later of the clock and a floor, so times never go back
along a branch. The floor is the time of the commit before it on the branch, one millisecond more if
that commit read its `Stamp::time_ms`, and for a merge also the time of the commit it merges. A
clock that steps back therefore makes commits share a millisecond until it catches up, and `as_of`
returns the last commit of a shared millisecond. So `as_of(stamp.time_ms())` returns the commit that
stamp belongs to for as long as it is retained, while a time the closure reads from the clock itself
can fall before, on or after its commit's time.

## Walking history

`history_of` walks the log newest first and yields `(commit, value)` for each commit that changed
the projection, including a change back to an earlier value. It reads nothing until you ask and
stops when you stop, so `take`, `find` and the `while_some` above cost the commits actually walked:
for that `while_some`, the age of the row, not the length of the log. `while_some` is for a
projection that returns an `Option`, such as a row by key. It yields each value without its
`Option` and stops at the version before the row existed, where
`take_while(|(_, row)| row.is_some())` would, so it does not walk past the row's creation into the
rest of the log. Its `try_next` returns a read error where the iterator would panic. To show
changes oldest first, collect and reverse. `log().iter()` runs in version order like any
collection, and `.rev()` walks newest first the same lazy way.

Three forms take a range of versions, for when you know where to look or can find out:

```rust
let created = db.bisect(.., |shop| shop.orders.contains(&order_id))?.expect("placed");
let ended = db.bisect(created.., |shop| !shop.orders.contains(&order_id))?;
let alive = created..ended.unwrap_or(u64::MAX);

let week: Vec<_> = db.history_of_range(from..=to, |shop| shop.orders.get(&order_id)).collect();
let edits = db.bisect_changes(alive, |shop| shop.orders.get(&order_id))?;
```

`bisect` finds the first version where a test holds, for a test that stays true once it turns true,
in about log2 of the log's length in probes. `history_of_range` is `history_of` between two versions
and costs the span. `bisect_changes` cuts the range in half until the ends agree, so it costs the
changes instead of the span, but it can miss a change and its reversal that fall between two probes,
so use it only for a value that never returns to an earlier one in that range. A row's absence comes
back when the row goes, which is why `alive` stops short of `ended`. It returns oldest first.

## Stamping a commit

`commit_stamped` and `try_commit_stamped`, and their awaited forms `commit_stamped_async` and
`try_commit_stamped_async`, also hand the closure a `Stamp`, so a row can record the commit that
writes it:

```rust
db.try_commit_stamped(|shop, stamp| {
    let mut order = shop.orders.get_mut(&id).ok_or(ShopError::NoOrder(id))?;
    order.paid = Some((stamp.version(), stamp.time_ms()));
    Ok(("pay", ()))
})?;
```

`stamp.version()` is the version this commit will get, so `db.at(version)` later shows the state it
wrote. `stamp.time_ms()` is its time, fixed by the first call, and the commit keeps that millisecond
to itself: the next commit on the branch gets a later time, so `db.as_of(time_ms)` returns exactly
this commit. A branch that makes more than 1,000 such commits a second runs ahead of the clock until
the rate drops, so read the time only when you need it. A stamp describes the commit that would hold
the closure's changes. If the closure returns `Err`, panics or changes nothing, no commit holds
them, and its reading of the time claims nothing.

## Branches

```rust
let what_if = db.fork("what-if")?;
what_if.commit("try a price change", |shop| { /* ... */ })?;
```

A fork is one small commit whatever the database size. `db.branch(name)` opens a handle on a branch
that already exists, and `branches` lists them. Merging a branch back is described in
[Merging](merging.md).

`db.drop_branch("what-if")` removes a branch you are done with. It refuses `main` and the branch of
the handle you call it on, with `Error::BranchInUse`. Other handles on that branch keep reading what
they had, commits through them fail with `Error::NoBranch`, and its watchers end. The bytes stay in
the file until the next reclamation cycle.

## Tamper evidence

`db.head_hash()` is a hash over the whole history of a branch, for publishing somewhere an attacker
cannot edit. Check an old anchor with `db.log().hash_at(&version)` and `db.verify()`. A match shows
that version and everything before it are unchanged. It is not a signature and says nothing about
later commits, other branches or whether timestamps were true. See [`head_hash` and tamper
evidence](../internals/history.md#head_hash-and-tamper-evidence).

## How it works

- [History and the commit log](../internals/history.md): the chain of entries, skip links, commit
  times and dropping a branch.
- [Retention and reclamation](retention.md): what pruning does to old versions.
