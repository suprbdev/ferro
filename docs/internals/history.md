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
extra bytes, and as a pull brings a branch up to the branch it landed, as [Bringing the branch
up](sync.md#bringing-the-branch-up) describes. `Keep::Since` and `Keep::Within` need a lookup by
time, so they apply at each reclamation cycle.

Readers take a head and its chain as they find them, and the walks of whole chains check what they
rely on, as [What readers rely on](#what-readers-rely-on) lists.

## What readers rely on

`verify`, `verify_typed`, a reclamation cycle and `admin::compact` walk every retained chain and the
history that merges brought in through the same two walks, so a file that breaks one of these rules
fails all four with `Error::Corrupt`, and a cycle or `compact` leaves it as it was. Each rule is one
that a reader or the copy relies on, and no writer breaks one: a commit links the tip below it and
takes its numbers from its parents, and a received chain is checked against the same rules, as
[Applying](sync.md#applying) describes. [Fuzzing](testing.md#fuzzing) records what each rule's
absence led to.

- A head with a tip keeps versions `first` to its version, and `first` is at least 1. `len` and
  every lookup and walk rely on it, decoding the head checks it, and pruning and a commit's new
  chain check it again.
- A head with no tip names version 0 and no root. `read`, `info` and the version of the next commit
  rely on it, and decoding the head checks it.
- The tip is at the version the head names. `at`, `Log::last`, sync's have and the version of a
  branch's next commit rely on it. The chain walk checks it, and so does every read of the tip on
  its own: opening or refreshing a branch, `info`, `Log::last` and planning a bundle.
- The head's root is the tip's. `read` and `at` of the head's version rely on it to agree, as does a
  commit's test for a change, and the chain walk checks it.
- An entry is at version 1 or above, its reach is below its generation, and its last merge is its
  own version exactly when it is a merge, with a floor of 1 or above. The base search relies on it,
  and decoding the entry checks it.
- Each `prev` link reaches the version just below. `history_of`, `Log::iter` and the copy's search
  for a skip target rely on it, and both walks check it.
- No link above `first` is cut. Lookups rely on it, and the chain walk checks it.
- Each kept skip link names by hash the entry the chain reaches at its version. `at`, `as_of`,
  `hash_at`, the base search and sync rely on it. Both walks check it, and so does the copy, for a
  chain whose lower part another branch's walk took.
- In merged history, no skip link leads past where the `prev` links stop, or past where the history
  kept by time stops to a commit that time keeps. `lowest_kept` relies on it, so that the base
  search reads only what a cycle keeps, and the merged-history walk checks it.
- An entry is no older than the entries it links by `prev` and merge links. `as_of`, `Keep::Since`,
  `Keep::Within`, the time bound of merged history and the base search rely on it, and both walks
  check it.
- A merge floor is no higher than the merged commit's version. The base search and the copy of
  merged history rely on it, and both walks check it.
- Generation, reach, last merge and the bounded bit follow from the parents. The base search and
  sync rely on them, and both walks check them.
- A branch table runs back at most 1,024 steps to a whole table. Loading the table relies on it and
  checks it.

A receive checks the rules on times, merge floors and merge numbers on each sent commit through
the same function, so a file that passes `verify` can be pulled from. Where a parent lies in pruned
history, below where a walk stops, or behind a cut link, only what that parent cannot change is
checked: the generation must exceed the known parent's, and so on. An entry above version 1 with no
`prev` link counts as one whose parent is pruned. Checking a merge entry reads the commit it merged
once more. Where a walk of merged history stops because another walk took the chain from there, it
checks the skip links it passed against the hashes a chain walk recorded, when one took that chain,
and otherwise reads on, without walking, until they have met the entries they name. Reading on
every time made `verify` 29% slower on a file of 202,000 commits and 2,000 merges, and the
recorded hashes bring that to 1%, as [Fuzzing](testing.md#fuzzing) records.

`verify` and `verify_typed` also check the branch table itself, which `verify` once only re-hashed:
they decode it and the steps on top of it and hold the nodes of their maps to the checks of
`verify_typed`, since a table whose names were out of order hid `main` from every lookup while the
re-hash passed (#135). And each merge record must name a branch the table holds. Dropping a branch
removes the records that name it, and landing a new tracking branch removes those that name it
there, but a record that outlived its branch made a branch forked later under that name look merged
up to the recorded version, so merging it dropped what it had added by then (#135). A pull that
finds its tracking branch's head already below ours writes its record in a step of its own after
the merge search, and if the tracking branch was dropped meanwhile, or replaced by another of its
name, it now writes none. Bundles carry no records: a landed branch starts with none. On a file of 10,000 branches the table checks added 2 ms to the 43 ms `Db::verify`
took, and on one of two branches and a million keys nothing measurable.

Some things are left unchecked because no reader is misled by them. Beyond naming a branch the
table holds, the old `merged` records are claims a structure cannot confirm, and readers already
ignore one whose version is past the commit a search starts from or outside the named branch's
retained versions. A merge's time bound past
the merged commit's time makes that commit bare, which the search and the copy treat alike. A
missing skip link only makes lookups slower. An entry with no root reads as `NoVersion`. A commit
after a stamped one may share its millisecond in a file another peer sent, since a receive does not
refuse it.

## What pruning hides

Everything that names a version of a branch follows one rule: a version below `first` is gone. `at`
returns `NoVersion` for it, `as_of` a time before the oldest retained commit returns `NoVersion`,
`history_of` and `bisect_changes` report the oldest retained commit as a change because there is
nothing left to compare it with, and a fork copies `first`, so it answers the same. Version 0, the
value before the first commit, counts as retained for exactly as long as version 1 is, though its
value stays in the file, as [Version 0](#version-0) describes. `at(0)` used to return the current
`Default` even after pruning, while `as_of` for the same moment returned `NoVersion`. The rule
depends only on `first`, never on whether a reclamation cycle has removed the bytes yet, so an
answer does not change when one runs.

## Version 0

Version 0 is the value before a branch's first commit. It has no log entry: the chain starts at
version 1, and a head with no tip names no root. Its value is stored once for the whole file, as a
root the branch table names, which every branch in the file starts from. A table's body starts with
the byte `0x7e` and its first link is that root, as described under [File format and
durability](storage.md#file-format-and-durability). Steps do not carry it, since they link back to
a table.

The first frame a file writes, a commit, a fork or a pull, always writes a table, and that table
stores the `init` value of the process that writes it. That process then holds the value as it
decodes from what was written, as it holds a commit's, so a field the file does not store reads the
same before and after a cycle or a reopen. Every later table names the same root, and a reclamation
cycle and `compact` copy it as they copy the table. A table that names none, as in a file converted
from format 6, makes the next frame write a table rather than a step, so the file stores it then.

An open of a file whose table names version 0 never calls `init`, and reads the value only when
something first asks for it. Damage to its records fails what reads them: `at(0)`, `history_of`
and `bisect_changes` as far as version 1, a merge against it, a reclamation cycle and `compact`,
which copy it, and `verify`. While `main` has a commit, the open does not read it. When `main` has
none, its value is version 0, and the open reads it to give the handle `main`, so damage fails the
open too. A value that is a collection reads only its top record when it is loaded, so damage
below that fails where the collection is read, as it does for any version.

A process that opened the file before anything was written, with an `init` of its own, takes the
stored value when it catches up, refreshes or follows, and gives it to each branch with no commits.
It takes it only once every such branch has its new state, so a catch-up that fails partway leaves
the process as it was, and the next one makes the whole change. A reclamation cycle likewise points
version 0 at the new file only once the swap can no longer fail, so a failed swap holds nothing of
the abandoned copy open.

With `Options::shared`, a closure can run on a branch with no commits while another process writes
the file's first frame, such as the staging frame of a pull, which needs no branch's lock. The
closure would then have started from an `init` that the file does not keep, and a commit cannot run
its closure again. So before such a closure runs in a file that does not store version 0, the
crossing that takes its branch's lock writes a frame that stores it, or takes the one another
process stored first. It writes that frame even if the closure then changes nothing, once for the
file, and a file opened without `shared` never needs it, since its writers share one process.

Before, version 0 was `init` called at each open, so `at(0)`, `history_of` and `bisect_changes`
comparing version 1 with it, a fork of an empty branch, and a merge of branches that share no
commit all used whatever `Default` was at that open, not what the database started from. Two other
places to store it were weighed:

- A log entry at version 0. Its chain would start at 0, which the base search, the graph numbers,
  `Log::len`, the rule that `first` is at least 1, the copy's pruning and sync's anchors all assume
  is not so, and a bundle would have to carry the root too. Two files created apart would hold
  different entries at version 0, so whether they share a commit would depend on when and with
  what value each was created.
- A root in each head. Every step would carry one more link, a 20-byte hash and its distance, on
  every commit, and a branch that sync lands would need a value that the bundle does not carry, so
  it would take the file's anyway.

A file that nothing was written to stays two superblocks, 8 KiB, and opens with `init` each time,
except a shared file in which a closure ran on an empty branch, as above. A file's first frame
holds the value of version 0, a few bytes for an empty collection, and each table names it with one
more byte and a link, once every 64 commits. The value is kept when pruning drops version 1:
`at(0)` then returns `NoVersion` as the rule above says, but the file keeps the value, since a
branch that a later pull lands from version 1, and a merge of branches that share no commit, start
from it.

Sync does not send version 0, and the bundle format is unchanged. A pull between files that share
no commit merges against the receiving file's version 0, as it merged against that file's `init`
before, and `common_ancestor` answers it for them as before. A branch a pull lands from version 1
reads the receiving file's version 0 as its own, though the sending file may store another. Two
files created apart never share a commit through version 0, so a first pull between them still
sends everything.

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

### A branch of the same name

Another process learns of a drop when it catches up, and a read-only handle when it refreshes, and
by then the name can belong to a new branch: a drop and a fork of the same name between two
catch-ups leave a table that still has the name. The first version compared the head it found with
the one it had, by log and root, took the new branch for the old one moved, and kept the old handles
on it: a write through one landed on the new branch, and its watchers fired with the new branch's
lower version (#101). The outcome hung on whether the process happened to catch up between the drop
and the fork.

So each `Head` records `born`, a sequence number taken when the branch was created. A fork
reads it as the number after the newest frame, under `tip`, where every frame is placed, and with
`Options::shared` after catching up under the placement lock, so it is past the number of the frame
that dropped any earlier branch of that name. A receive that creates a remote-tracking branch does
the same. Commits, fast-forwards, receives into a branch that exists, pruning and cycles carry it
over, and `main`, which no fork creates, keeps 0. Sequence numbers never go back within a file: a
cycle seals its copy at the old file's newest number, and `admin::compact` now does too where it
sealed at 1, which let a read-only handle that outlived an offline compaction meet the same number
again. A `Branch` keeps the `born` of the head it was attached from, and catching up, `refresh` and
following a cycle retire every attached branch but `main` whose name is gone or whose head has
another `born`, so its writes fail with `Error::NoBranch`, its reads keep what they had and its
watchers end, as when the drop is in the same process. Comparing heads for a move counts `born` too,
so attaching from the published table waits for a new branch's head as it waits for a moved one. The
cost is one comparison per attached branch on heads each catch-up loads anyway.

Two ways that leave the format alone were rejected:

- Looking for the handle's tip, version v with entry hash h, in the new branch's chain, a lookup of
  O(log n) per attached branch: the branch is the old one if its chain holds h at v. But a fork
  shares its parent's chain. A branch dropped before a commit of its own and forked again from the
  same parent holds the very entry the handle holds, and so does one forked from a branch that was
  forked from the dropped one. Both read as the old branch moved on.
- Reading the table of every frame a catch-up follows, not just the last, for a drop of an attached
  name, which costs a lookup per attached branch in every table a frame rewrites: it finds every
  drop in the frames a process reads, and those are not all. A process or a read-only handle that
  two cycles pass while it is idle opens the newest file, and the frames of the generation between
  are gone with the file that held them.

In the encoding, bit 2 of the flag byte before the root link says that `born` follows the map of
merge records, as a varint. A head whose `born` is 0 does not set it and one that sets it with 0 is
rejected, so a head has one encoding. Heads written before have no such bit and read as 0. A branch
from before this change that is dropped and forked again gets a number, so its old handles retire;
only a drop and fork both made by a build from before go unseen, as they always did. `main` never
sets the bit, so a file whose only branch is `main` is written byte for byte as before. A build from
before this change rejects a head with the bit, so it cannot read a table once a branch has been
forked since.

`a_branch_another_handle_dropped_and_forked_again_leaves_the_old_handles_dead` drops and forks again
in one shared writer while another holds a handle and a watcher on the branch, with and without
commits of the old branch's own, so that the new branch shares the handle's entry, and with and
without a cycle between, which the other writer follows. The write through the old handle must fail
with `NoBranch`, its read stay and its watcher end, and a handle the other writer attaches
afterwards must write to the new branch.
`a_read_only_handle_retires_a_branch_dropped_and_forked_again_on_refresh` does the same for a
read-only handle, with and without a compaction, and
`a_branch_forked_again_after_an_offline_compaction_is_a_new_branch` forks the name again after
`admin::compact` with zero to seven commits between, one of which gave the new branch the old one's
number while the copy was sealed at 1.
