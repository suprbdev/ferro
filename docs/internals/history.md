# History and the commit log

How a branch stores its commits, how versions and times are looked up, what pruning hides, and what
`head_hash` proves. The API is in [History, versions and branches](../guide/history.md).

## The commit chain

Each branch head holds a commit log stored as a chain of entries, newest first. An entry holds the
`Commit`, a link to the previous entry, and for even versions a second link to the entry at
`version - lowest_set_bit(version)` when that entry is at or above `first`, so a power of two has
none. A merge's entry also links the other branch's tip entry, as described under
[Merging](merging.md#three-way-merges). Appending a commit writes one small object and finds its
skip target by following skip links from the tip, which is O(1) amortized. Looking up a version or a
time follows skip links while they do not overshoot, which is O(log n). The head records the tip
pointer, the first retained version, and the head version. `Refs` is `Map<String, Head>`, and the
superblock points at it, or at a chain of steps over it that each carry one branch's new head, as
described under [Branch-table steps](commit-path.md#branch-table-steps). Forking a branch copies one
`Head`, so the chain is shared and nothing is rewritten. `restore` commits an old value as a new
version, so history is never rewritten.

Pruning only raises `first`. Walks stop there, and lookups below it return `NoVersion`. Retained
entries are not rewritten, so the entry at `first` still links `prev` and perhaps `skip` entries
that are gone. Those links are hashes nothing follows, and a copy into a new file writes them with
offset 0. `Options::history(Keep::Last(n))` raises `first` as part of each commit, which costs no
extra bytes. `Keep::Since` and `Keep::Within` need a lookup by time, so they apply at each
reclamation cycle.

A walk of a whole chain, as `verify`, `verify_typed`, a reclamation cycle and `admin::compact` make,
checks that the tip is at the version the head names as its last, that each `prev` link it follows
reaches the version just below, and that each retained skip link names by hash the entry the chain
reaches at its version, and fails with `Error::Corrupt` where one does not, as
[Fuzzing](testing.md#fuzzing) describes. Their walk of the history that merges brought in checks its
`prev` links the same way. A commit links the tip below it, and a received chain must descend one
version at a time, as [Applying](sync.md#applying) describes, so only a damaged file has such a
link.

## What pruning hides

Everything that names a version of a branch follows one rule: a version below `first` is gone. `at`
returns `NoVersion` for it, `as_of` a time before the oldest retained commit returns `NoVersion`,
`history_of` and `bisect_changes` report the oldest retained commit as a change because there is
nothing left to compare it with, and a fork copies `first`, so it answers the same. Version 0, the
value before the first commit, is not stored, and it counts as retained for exactly as long as
version 1 is. `at(0)` used to return the current `Default` even after pruning, while `as_of` for the
same moment returned `NoVersion`. The rule depends only on `first`, never on whether a reclamation
cycle has removed the bytes yet, so an answer does not change when one runs.

## The `Log` handle

The log used to be a `Map<u64, Commit>`, which rewrote a tree path per commit and was about 20% of
the bytes of a single-key commit. `Db::log()` now returns a `Log` handle with `len`, `get`, `first`,
`last`, `iter` and `values`. `iter` runs in version order, like a `Map`, and is double ended.
`next_back` follows `prev` links from the tip and reads only what it yields, so `.rev().take(n)`
costs O(n). The chain has no forward links, so the first `next` reads what is left of it once.

## `history_of` and ranges

`Db::history_of` walks `prev` links newest first and yields `(commit, value)` for each version whose
projection differs from the version before it. Only adjacent versions are compared, so a value that
comes back is reported again, and skip links cannot speed this up: a change and its reversal could
both fall between two probes. Version 1 is compared with the projection of the genesis value, and a
pruned chain reports its oldest retained commit, since what came before is gone. `History` is not
double ended, because reversing it would need the whole walk. Callers that show changes oldest first
collect and reverse, so the cost is in plain sight.

`Db::history_of_range(versions, project)` is the same walk between two versions. It finds its top
through skip links and reads one version below its bottom as a baseline, so it yields exactly what
filtering `history_of` by version would, for O(log n) plus the span. `history_of` is that with no
bounds. Both load nothing until the first `next`.

## `bisect` and `bisect_changes`

`Db::bisect(versions, test)` finds where a monotone test turns true, in about log2(n) probes of a
root each. A probe is a version lookup, so that is O(log^2 n) entry loads. Following skip links from
the tip was worked out and not built: it takes two probes a level for the sake of the entry loads,
and the probes are what cost.

`Db::bisect_changes(versions, project)` uses the probes the walk cannot trust. It compares the ends
of a span, stops if they agree, and otherwise cuts the span in two, so it costs O(changes × log n)
probes, not the span, and it can miss a change and its reversal that fall between two probes. It is
for a projection that never returns to an earlier value in that range, such as a row with a revision
or an attempt count, or `value_hash` of one. A row that appears and later goes is not one: its
absence comes back, so the range has to stop short of the row's end. A pruned chain and genesis
behave as in `history_of`, and results come oldest first.

`queue` carried 90 lines of its own bisection and now has none. On a 305,000 commit file, for one
job out of 100,000, `queue history` takes 1.2 ms, as the old bisection did, against 2.0 to 3.9 s for
`history_of`, which walks back to the start of the log because the job is absent at the tip. The two
`bisect` calls that find where the job begins and ends take 0.1 to 0.2 ms. For a job alive through
the whole file, `bisect_changes` takes 0.26 ms and `history_of_range` over its 304,734 versions
takes 1.0 s. A job that stays alive is where the walk still costs, which is why `queue` uses
`bisect_changes` and not `history_of_range`. `shop` starts `GET /orders/{id}/history` from the
request's snapshot, so it no longer walks commits newer than the one it answers for.

## `History::take_while`

A value's item is ready only when the walk reaches the start of that value's run. A row's creation
is followed by a run of `None` back to version 1 that reports nothing, so `Iterator::take_while`
would read the rest of the log to learn that no older item exists. `History::take_while` is an
inherent method for that reason. It tests the predicate against each older value as the walk enters
its run, paired with the commit last reported, and stops there. Its items match the `Iterator`
adaptor for a predicate on the value, on the commit, or on the conjunction of the two. The predicate
is `Fn` because it can run twice per item. The run at the tip has no commit to pair with, so a row
that is absent at the tip costs the age of that absence; find where it ended with `bisect` and start
`history_of_range` there.

`History::while_some` is this `take_while` with the predicate `Option::is_some` on the value, for a
projection that returns an `Option`. Its items drop the `Option`, which the predicate has already
ruled out, so it stops where the `take_while` form stops and walks the same commits.

## The cost of a step

A step loads one `Entry`, loads and decodes the root `T`, and runs the projection. Measured on a
50,000-commit shop in a fresh process: 0.8 µs for the entry, 1.3 µs for the root, then 1.3 µs for a
row in a quiet part of the tree and 11.5 µs for a recent row, whose nodes every commit rewrote, so
each step reads and decodes those nodes in full, values included. With the same commits cached the
whole step is 0.7 to 2 µs. Going through `at(v)` instead costs about 3 µs more per step, for the
skip-link descent from the tip. Entries hold the `Commit` behind an `Arc`, so walking the log and
`db.read()` do not copy the message.

## `head_hash` and tamper evidence

`Db::head_hash` is the hash of the tip entry, and `Log::hash_at` that of any retained entry. An
entry links its commit's root and its `prev` entry, and the hash covers link hashes, so one hash
covers every commit's version, time, message and `restored` field, and every root value, back to the
oldest retained entry, and commits to everything older as well, though that can no longer be
checked. Pruning and reclamation keep it, because offsets are not hashed and retained entries are
never rewritten.

Published where the writer of the file cannot change it, the hash is a tamper-evidence anchor. If
`hash_at(version)` matches an anchor for that version and `verify` passes, then the history up to
that version is what it was when the anchor was taken, subject to the limits of 160-bit hashes
described under [Objects and links](storage.md#objects-and-links). That is all it shows:

- It is not a signature. Hashes are unkeyed, so anyone who can write the file can rewrite history
  and get a consistent chain with a new hash. Only comparing with the anchor reveals that.
- It proves nothing about commits after the anchored version, or about history before the anchor was
  taken: a chain forged first and anchored afterwards verifies.
- `time_ms` and `message` are what the writer recorded. The hash shows they were not changed, not
  that they were true.
- It covers one branch's chain and, through merge links, the commits merged into it. Other branches,
  and the old `merged` records in the head, are outside it, and `verify` is what checks those
  objects.
- An anchor for a pruned version can no longer be checked, since `hash_at` returns `None` for it. An
  anchor for a retained version still matches.

## Commit times

A commit's `time_ms` is the later of the clock and a floor: the previous commit's time, plus one if
that commit is stamped, and for a merge also the time of the entry it links. So times never go back
along a chain, a clock that steps back makes commits share a millisecond until it catches up, and a
merge is never older than either parent, which the merge-base search relies on. `as_of(t)` follows
skip links from the tip to the newest entry whose time is at most `t`, so of several commits in one
millisecond it returns the last.

A time a closure reads from the clock says little about its commit. The commit takes its time after
the closure returns, and later commits can share that millisecond, so `as_of` of the reading can
land before the commit, on it, or on a later commit. `examples/booking` recorded the time of each
payment that way and found all three among 1,500 payments, 773 of them on a later commit.

`commit_stamped` and `try_commit_stamped` pass the closure a `Stamp`. Its version is the tip's plus
one, the version the commit gets since it is made under the same lock. The first call to `time_ms`
fixes the commit's time at the later of the clock and the floor, and the commit is then stamped: its
entry carries a flag, and the next commit's floor is one past its time, so no later commit on the
chain shares its millisecond and `as_of` of that time returns it. A commit whose closure reads only
the version is not stamped, so writers that do not ask keep the times they had. The flag lives in
the entry, so the rule holds after a reopen, on a fork, which shares the entry, and after a pull
that fast-forwards onto it. A closure that returns `Err`, panics or changes nothing writes no entry
and claims nothing.

The cost is drift. A branch that stamps more than 1,000 commits a second takes times ahead of the
clock by a millisecond for each one past that rate, and the clock catches up once the rate falls.
Stamping every commit would make `as_of` exact for any commit's time without asking, but a plain
commit takes about 24 µs with buffered durability, so a busy writer would run seconds ahead within a
minute and `as_of(now)` would stop returning the head.

The flag byte of an entry had one bit left. Bit 128 now says a varint of further flags follows the
byte, and bit 1 of that varint is the stamp, so the next flag needs no change to the byte. A varint
of 0, or one with an unknown bit, is rejected, so an entry has one encoding. Entries written before
have no such bit and read as not stamped. A build from before this change rejects a stamped entry.

## Dropping a branch

`Db::drop_branch` writes one frame: a `Refs` without the branch. Merge links name entries, not
branches, so a merge of the dropped branch still knows what it took, and those commits stay as
described under [Merges and pruning](merging.md#merges-and-pruning). The frame also removes the
branch's name from the old `merged` map of every other head, including records a fork inherited.
Those records are keyed by name, so one left behind would match any later branch of that name. Its
version would then be read against the new branch's log, which gives a wrong base, and merges would
report conflicts and re-apply old changes. The branch's own head and records go with it. Commits
read `merged` from the head in `tip.refs` rather than from a copy in the branch state, so nothing in
memory can write a removed record back.

A commit writes its head by name, so a handle on a dropped branch would quietly recreate it. Each
`Branch` therefore has a `retired` flag, which commits and merges from that branch check under the
`tip` mutex, and forks under the branch's lane, returning `Error::NoBranch`. `drop_branch` holds the
mutex through its sync and until the flag is set, so no commit fits between the frame and the flag.
That stalls the encoding of writers on every branch for one fsync, on an operation that is rare. It
also holds the dropped branch's lane, so a closure on it either ran and committed before the drop or
fails. Retiring also closes the feed, which ends the branch's watchers. Reads through an old handle
still work, because state is a value and its objects are there at least until a cycle, and after one
the handle keeps the old file open. A later fork of the same name is a new `Branch`, so old handles
stay dead.

Counting handles and refusing the drop while others exist was rejected: a clone parked on another
thread would block it forever, and handles in other processes cannot be counted anyway. A read-only
handle learns of the drop on `refresh`, and retires its `Branch` the same way instead of falling
back to the genesis value.
