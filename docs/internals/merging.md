# Merging and the merge graph

How three-way merges run, how the merge base is found over the merge graph, what happens after a
criss-cross, and how merges interact with pruning. The API is in [Merging](../guide/merging.md).

## Three-way merges

`Map::merge` and `Table::merge` do a three-way merge driven by `diff(base, theirs)`, costing
O(changes). A key changed only by theirs is applied. A key changed the same way on both sides is
left alone. A key changed differently on both sides goes to a `resolve` closure. Merging runs inside
an ordinary commit, so a root with several collections merges each one in plain Rust.

`Db::merge_with` finds the base, or builds a virtual one after a criss-cross as described below,
runs the closure on our tip with the base and their snapshot, and commits. The log entry it writes
has a second link, to the entry of the other branch's tip: the merge's second parent. Commits are
then a graph, as in git, and `common_ancestor` returns a best common ancestor in it: a commit both
tips reach through `prev` and merge links that is not an ancestor of another such commit. Because it
follows merge links, a change counts as shared however it arrived, directly, through a third branch
(A merges B after B merged C, then A merges C), or through a fork of a branch that was already
merged, so a closure that adds counter deltas applies each delta once. The base is computed under
our branch's lane against the tip the closure runs on. A merge that keeps only our side still
commits, so the link is not lost, and merging a branch whose tip we already contain writes nothing.

## Comparing values

A three-way merge asks three questions of values: whether a value changed between the base and
theirs, which `diff` decides; whether ours already holds their value; and whether ours still holds
the base's. All three used to be `PartialEq`, and `==` is not what a file stores (#52). NaN is not
equal to itself, so a row holding NaN that neither side touched came out of `diff` whenever its leaf
held a change, then counted as changed on both sides and went to `resolve`. A resolver that blends
two rows into a new one wrote a new row each time, and two files syncing such rows never settled.
`==` can also hold between values that are stored differently: `0.0 == -0.0`, and a hand-written
`PartialEq` may leave a field out. A change from one to the other was then no change: `diff` left it
out, the file that pulled it kept its own value, and the other fast-forwarded to that and lost it.

Merges now compare values as stored. Two values are the same when they encode to the same bytes and
links, which is what root hashes, the commit no-op skip and sync's `Equal` already went by, and
`Data::same_encoding` answers it. Its default encodes both values into two reused buffers and
compares them. The impls ferro provides answer without encoding: integers, `bool`, `char`, `String`
and `Bytes` with `==`, which agrees with their encodings; floats by their bits; `Option`, `Vec`,
arrays, tuples, `Box`, `BTreeMap` and `BTreeSet` part by part; `Arc` by pointer, then by value;
`Map`, `Set`, `List` and `Table` by length and root hash, as their `==` already did; and `Ptr` by
hash. The derive compares a struct's stored fields, an enum's variant and then its stored fields,
and a transparent struct's one field. Skipped fields are not stored and take no part. A hand-written
`Data` impl gets the default unless it overrides the method, as it can `describe`.

`diff` skips an entry whose slot both sides share without reading it: the same column of a node
that was copied on write, the same `Arc`, or a spilled value with the same hash. It compares the
rest with `same_encoding`. `Map::merge` and `Table::merge` compare ours with theirs and with the
base the same way, after checking whether the two `Val`s point at one value. Nothing in `diff`,
`merge` or `try_merge` calls `==` any more, so their `PartialEq` bounds are gone. Covered index
entries already counted a covered field as unchanged only when `==` held and it encoded to the same
bytes, which `same_stored` now decides with `same_encoding`.

Keys are told apart by `Ord`, as before. A map holds one entry for keys that compare equal and keeps
the key it was first given, and the values an index is keyed on are keys of its map. `history_of`
and `bisect_changes` compare the projections a caller gives them, which need not be `Data`, so they
still use `PartialEq`. The commit no-op skip, sync's choice between up to date, fast-forward and
merge, and `==` on collections compared hashes already and are unchanged.

`stored_equality.rs` merges maps of floats and of a type whose `PartialEq` ignores a field, both in
memory and read back from a file: an untouched NaN, NaN with another payload, a zero changing sign
on one side or both, and the ignored field changing on one side or both. It does the same for
`Table::merge` and `Table::try_merge`, and has two and three devices sync tables holding NaN rows
under a resolver that blends two rows into a new one and counts the blends. Exchanges stop moving
anything within the six rounds the test allows, no row that neither side changed is blended, with
two devices only the row both changed is, and every device ends with the same rows bit for bit.
Before the change they were still merging after six rounds. A further test draws pairs of values of
a struct that uses every impl above, a derived enum and a transparent struct, and checks
`same_encoding` against comparing `hash_of`.

### What it costs

`cargo run --release -p ferro-bench --bin merge` gained three shapes: a map of 1,000,000 keys read
back from a file, with 100,000 keys changed on each side, 10,000 of them on both, merged by
`Map::merge` with `u64` values and with a struct of a string, a float and an integer, and by
`Table::merge` over rows with one index. Theirs changes a key in nearly every leaf, so `diff`
compares nearly all million entries. Each figure is the mean of five merges after a first, averaged
over three runs of each build taken in turns on a quiet machine, pinned to sixteen cores. The first
version of the change compared every pair by encoding both values. The middle column is two runs of
it:

| Shape | Before | Encoding both | After |
| --- | --- | --- | --- |
| `Map<u64, u64>::merge`, 110,000 keys written | 180.0 ms | 210.3 ms | 178.5 ms |
| `Map<u64, Reading>::merge`, the same | 293.5 ms | 352.1 ms | 296.3 ms |
| `Table<Visit>::merge`, the same, with one index | 443.8 ms | 518.4 ms | 448.0 ms |

Encoding cost 17 to 20%, which is why `same_encoding` is a method the built-in and derived impls
answer without encoding. With it the merges are within 1% of comparing with `==`, inside the spread
between runs of one build. The base-search shapes of the tables above compare no values and moved
by no more than their runs spread: 62 against 63 µs for the fork at 10,000, 51 against 52 µs for
both shapes of a side branch taken 1,000 times, 7.70 against 7.75 ms for the 1,000 features, 2.31
against 2.34 ms after syncing, and 211 against 213 µs for the whole criss-cross merge.

## Merges that fail

`merge_with` had no error channel, so a merge closure that found it could not go ahead had to panic,
and `examples/booking` caught the panic and turned its payload back into a message. `try_merge_with`
is the same merge with a closure that returns `Result<(S, R), E>` for any `E: From<Error>`, like
`try_commit_with`, and `merge_with` is now that with a closure that cannot fail. The closure runs on
a copy of our tip under our branch's lane, and the merge link and the `merged` record are only
written by the commit after it returns, so an `Err` drops the copy and nothing reaches the file: the
next merge finds the same base and runs again. The errors of the merge itself, such as `NoBranch`
for a dropped branch, come back through `E::from`. `Map::try_merge` and `Table::try_merge` do the
same for `resolve`, which matters when one conflict decides that the merge cannot happen. They stop
at the first `Err` and put back an O(1) clone of the collection taken before they started, since a
half merged collection is of no use to anyone but a closure that is about to fail.

## The other branch's state

A merge runs on our tip under our lane, while the other branch may be committing: its newest commit
can be placed and still waiting in its sync. The merge takes the other branch's published state, the
snapshot `read` on that branch returns, and that branch's merge records from the branch table
published with it. Our side takes our tip and the table under `tip`, which name the same commit while
we hold our lane. A record says that a version of the named branch is below its head's commit at the
recorded version, so it describes a state only when it comes from a table that names that state's
commit, and each side of the search now reads its records from such a table.

The merge used to take the published state with the records of the table under `tip` (#96). A pull
or sync on the other branch that had placed its merge commit and was waiting in its sync had already
recorded the remote's version there, so the search counted the remote's commit as below a published
state that did not contain it. When our side held that commit too, it became a second best common
ancestor, the virtual base took its changes, and applying theirs against that base took them out of
ours again. With `main` and `side` forked at x, `main` pulling the remote's r and committing a, and
`side` committing b and then pulling r, `main.merge_with(&side)` during `side`'s sync merged b
against x merged with r and dropped r's key from `main`. `common_ancestors`, which read both from the
published table, answered x alone. A plain merge records nothing, so only a pull or a sync showed it.

Taking their tip with the table under `tip` would be consistent too, but their tip need not be
durable: under `Durability::Fsync` the closure would see a commit that a failed sync takes back,
which no reader may, and a merge that ends without a frame would have to wait for the other branch's
sync as well as for its own. Waiting for the other branch's commit to be published before merging
would make a merge wait on another branch's writers, which lanes exist to avoid. So a commit on the
other branch that is still syncing is left for the next merge, which finds it published: what `read`
shows on that branch is what is merged, and `common_ancestor` answers for the same two states.

That needs a published state and the published table to agree, and they did not always. The leader
of a sync stored its batch's table under the `published` lock and published each branch's state
after releasing it, so a merge that took the lock between the two found the other branch's new head,
records included, beside its old state, and dropped r's key the same way. The leader now publishes
the states before releasing the lock, as `refresh` and a cycle's swap already did. `rewrite_refs`,
which lands a pulled branch and moves or records ours after a pull, published the states it moved
only after its frame had settled. It now builds them once its frame is written and hands them to the
frame's entry in the queue, so the leader publishes them with the table. A merge holds its lane,
which `rewrite_refs` waits for, so only `common_ancestor` could meet that gap. Pruning still sets
each state after its table, but it moves only the oldest retained version, which the search takes
from the state and never pairs with a record. The steps that build a virtual base read the records
of both tables, as described below.

`merge.rs` holds the leader of `side`'s pull at the start of its sync, and again once it has
published the table and the states but still holds the lock, under both durabilities, and checks
that `main` keeps r's key, that the merge runs once against x with `side`'s published version, and
that merging both ways after the sync changes nothing. It does the same with a plain merge on
`side`, which passed before. The first hold lost r's key before the merge read the published table.
The second checks that `side`'s state is already published and that no merge runs while the lock
is held, so a leader that let the lock go before publishing the states fails it.

The merge link took its floor from the table under `tip` as well (#122). The floor is the oldest
version that the merged state retains, but it was raised to the `first` of the other branch's head
there. Under `Keep::Last` each commit raises `first`, so while commits of the other branch waited in
their sync, or, with `Options::shared`, while frames of another process that crossing our lane had
caught up with were not yet settled, that head could have pruned past the version being merged. The
merge then wrote no link at all, as if their tip were already ours. The next merge found the old
base: with `Keep::Last(2)` it refused with `NoCommonAncestor`, and in a shared file where the other
process had also merged `main` into `side`, it found the commit both had merged from and applied two
of `side`'s changes a second time. The floor now comes from the state the merge takes and nothing
else.

The table under `tip` was read so that a merge never claims more than the other branch still had,
but what the published state retains is in the file the merge writes to: no cycle swaps it while a
lane is held, and crossing a lane follows another process's cycle and rebinds every state first. A
floor below where the other branch's head now stops is also the shape every merge reaches once the
other branch commits and prunes after it, which a cycle copies and the search reads as before.

The steps that build a virtual base lost records the same way. They read records from the table
under `tip`, where a record counts for a commit only while the head that holds it keeps that commit
on its chain. A commit that the published state of the other branch holds can be below the `first`
of that branch's head under `tip`, so its records were dropped, and the step ran into the pruned
history they bridge and refused. The steps now read both tables. A record holds whichever table it
comes from, so the union is sound, and a start found in both is added once.

`merge.rs` holds the leader of `side`'s sync while a second commit of `side` is placed, under both
durabilities, merges, and checks that the merge linked what it merged: after a cycle `verify` and
`verify_typed` pass, `common_ancestors` answers `side`'s merged commit from both sides, the log and
`history_of` walk through the merge, and merging again applies each change once. `shared.rs` does
the same with a second shared writer committing on `side`, with one that also merges `main` into
`side` first, where the second merge applied two changes twice, and with a criss-cross over a
pull's record while the second writer commits on `side`, which refused before the steps read both
tables.

## Handles of other files

`merge_base` finds each side's records by branch name, so it trusted the handle it was given to be a
branch of its own file (#98). A handle on another file was looked up by its name: with b in another
file, `a.common_ancestors(&b)` took the records of a's own `main` as b's and answered a commit of a
third file that a had pulled, while `b.common_ancestors(&a)` answered version 0. A merge across
files ran its closure and then failed with `Corrupt("hash mismatch")`, when its commit linked an
entry of the other file. A handle on a dropped branch whose name had been forked again took the new
branch's records.

`common_ancestor`, `common_ancestors`, `merge_with` and `try_merge_with` now refuse a handle from
another open with `Error::ForeignHandle` before any closure runs. Two opens of one file keep their
own tables and states in memory, and a cycle may have swapped the file under one and not yet the
other, so a handle from another open of the same file counts as foreign too. No variant fitted:
`NoBranch` names a branch, which the other file may well have, and `Corrupt` says this file is
damaged. A dropped branch on either side gets `NoBranch`, checked with the states under the
`published` lock: either the branch is retired, or the published table, which loses the branch
before `drop_branch` retires it, has no head for it, and a search that finds no head for a branch
now answers `NoBranch` instead of searching without its records.

## Before merge links

Only direct merges used to be recorded, as `(their version, our version)` per branch name in the
head, and the base was the fork point found by binary search unless a record said otherwise. A
change that came through a third branch was seen again.

## Finding the base

Each entry carries three numbers for the search. Its generation is one more than the larger of its
parents' generations, so a parent always has a lower one. Its reach is the highest generation among
the merged parents of the merge entries on its chain, at or below it. Its last merge is the version
of the nearest merge entry on its chain. On a chain with no merges they are the version, 0 and 0,
which entries written before merge links existed already mean, so they are stored only once a chain
has a merge, as three varints.

The search paints down from both tips, as git's merge-base does, but in jumps. An item is a place on
a chain and the sides that reach it. Items are taken in generation order, so when one is taken every
path into it has arrived and its sides are final. A chain item does not step one commit at a time.
It moves down to the highest place where anything could still join it: where its chain meets another
item's, found by a binary search on versions that compares entry hashes, or the highest entry whose
generation is at most the reach of any item, since nothing that has not yet been opened can arrive
above that. The merge entries it passes become a separate item whose level is their reach, opened
from the top only when nothing higher is left. A place reached from both sides is a candidate. It
and everything below it is stale, and the search stops once one side has nothing left that is not
stale.

So a merge whose chains have no merges since they parted costs two binary searches, O(log^2 n), like
the old fork-point search. Merge entries are opened only while their merged parents could still
matter: when `main` has taken a side branch a thousand times, the next base is found after opening
the latest merge, not all of them. What is opened is roughly the merges made since the base, since
each of those could have brought in something the other side has. Measured with
`Durability::Buffered`, the first call and then the mean of 100 more:

| Shape | First | Then |
| --- | --- | --- |
| Fork at 10,000, then 10,000 and 1,000 commits | 0.15 ms | 49 µs |
| `main` took a side branch 1,000 times among 21,000 commits | 46 µs | 44 µs |
| A side branch took `main` 1,000 times | 46 µs | 44 µs |
| 1,000 features merged since a branch forked | 31 ms | 30 ms |
| The same branch after syncing, with 200 features merged since | 1.3 ms | 1.3 ms |

## Criss-crosses

When two branches have each merged an older state of the other, the tips have several best common
ancestors. The search returns every candidate it found, ordered by generation, then `time_ms`, then
entry hash, highest first. The order depends only on the set, so both sides and every run agree.
`common_ancestor` returns the first, which is the one every merge used to take, and
`common_ancestors` returns them all. The model tests compare that list with the true set of best
common ancestors on every merge, with and without pruning, and whenever the search answers they
match.

Merging against the first alone made what only the others carry look new: a closure adding counter
deltas applied it twice, and a three-way merge could undo a revert, since the base held the value
from before the change that was reverted. Merging is now recursive, as in git. With best ancestors
B1 to Bk in that order, the virtual base starts as B1. For each later Bi, the search runs again with
B1 to Bi-1 together as one side and Bi as the other, which is the ancestry a commit merging B1 to
Bi-1 would have. If Bi is among the answers, it is below something already merged and is skipped.
Otherwise the answers become the base for this step, virtual themselves if there are several, and
the closure merges Bi into a working copy of the virtual base built so far. The merge then runs
against the result. For counter deltas this is inclusion and exclusion over ancestor sets, so each
delta is counted once whatever the shape.

The closure is the user's, so it must be callable more than once, and the merge methods and the sync
methods take `FnMut` instead of `FnOnce`. Each step calls it as a merge with the virtual base built
so far as `ours`, so it resolves conflicts between the ancestors as it resolves any. Git writes
conflict markers into a virtual base, so that the final merge conflicts again. Ferro has no conflict
state to write, and a closure that resolves the same way whichever side is ours, such as
last-writer-wins, makes the virtual base agree with what each side already chose. What the steps
return is dropped. An `Err` from any step discards the whole merge, as an `Err` from the merge
itself does, and nothing is written. The virtual base is never written either. It lives in memory
for the merge, its commit has version 0, no root, the latest `time_ms` of what it merged and the
message the closure returned for it, and the merge commit still links only our tip and theirs, so
later searches see the same graph.

The steps start from commits below the tips, with the limits the first search reached them with, so
they read only what pruning keeps, and when the base of the ancestors could be in pruned history the
merge refuses with `NoCommonAncestor`. A record in the old `merged` map says that a version of the
named branch is below the head's commit at the recorded version, so a step that starts on that
head's chain at or above that version also starts from the recorded commit. Without that, a
criss-cross over a merge recorded before merge links lost the recorded branch's changes, because the
step found the fork point instead.

Other shapes were considered. Handing the closure every best ancestor leaves it unable to build the
right base, which needs the ancestors' own bases, recursively, so each caller would redo this. A
merge trait on the root type that ferro calls recursively has no room for what closures capture,
such as a policy, a clock or a device name, or for a value returned to the caller, and it would be a
second merge API. Keeping `FnOnce` with an opt-in recursive variant would leave the default wrong,
and the sync API alone has eight methods that take a merge closure. Whether a closure can run twice
is a static bound in Rust, so it cannot depend on whether a criss-cross occurs. Most closures are
`FnMut` already. In this workspace one test closure moved a captured string into its message and
needed a clone.

Without a criss-cross, the closure runs once and the only added work is ordering the candidates.
`cargo run --release -p ferro-bench --bin merge` builds the shapes above, its feature merges smaller
than those of the table, and a criss-cross of two branches after 11,000 commits, merged without its
commit:

| Shape | Before | After |
| --- | --- | --- |
| Fork at 10,000, then 10,000 and 1,000 commits | 48 µs | 49 µs |
| `main` took a side branch 1,000 times among 21,000 commits | 40 µs | 41 µs |
| A side branch took `main` 1,000 times | 40 µs | 41 µs |
| 1,000 features merged since a branch forked | 5.9 ms | 6.0 ms |
| The same branch after syncing, with 200 features merged since | 1.8 ms | 1.8 ms |
| The criss-cross, its search | 120 µs | 120 µs |
| The criss-cross, the whole merge | 120 µs | 166 µs |

The merge costs one more search, from the two ancestors to their fork point, and one more call of
the closure.

A step's own base can be virtual in turn, so building the base recurses down the criss-crosses
below the tips, and where three or more branches keep merging each other the same set of ancestors
comes up on many paths. At a level with three best ancestors, both later steps recursed into the
same set below, so a merge after n rounds of three branches merging each other ran the closure
2^(n+1) - 1 times, 2,047 after ten rounds. Devices that pull from each other while two handles of
one shared file pull into it make that shape on most pulls: one merge searched 2,000 to 4,800 times
and took seconds while it held its branch's lock, so the other handle's landings and merges waited
behind it, and a pull took up to 57 s in a debug build (#132). A merge now builds each virtual base
once, keyed by the commits it merges and the limits the search reached them with, and reuses it
wherever the recursion meets that set again, so the closure runs once for each set of ancestors.
`a_merge_after_rounds_of_three_branches_merging_each_other_builds_each_virtual_base_once` in
`ferro/tests/merge_model.rs` expects 21 calls after ten rounds. In a debug build, four shared files
each with a writer and two pulling handles, 1,200 pulls a run over three runs, went from a mean of
85 ms to 1.25 s and a slowest pull of 4.9 to 57 s, to a mean of 51 to 133 ms and a slowest of 0.7
to 1.8 s, measured on 2026-11-09. With one pulling handle a file the mean stayed at 5 to 10 ms.

## Merges and pruning

A branch keeps its retained versions and, through every merge entry among them, the commits that
merge brought in: the merged chain from the merged tip down to the oldest version that the state the
merge took retained, and no older than either that version's commit or the oldest retained commit
of the branch that keeps the merge. The version and its time are stored in the merge entry. Merge
entries reached that way bring in theirs under the later of the two time limits. A reclamation
cycle copies exactly that. Every limit only rises, and a merge never claims more than the state it
merged still had, so a commit that has gone never comes back through a later merge: a
nested merge does not take the looser limit of whichever branch reached it, which would let a branch
that never pruned reach, through a merge of a pruned branch, commits that branch had already let go.
The merged tip itself is kept whatever its age, with its value but nothing below it, because it is
the base the next merge of that branch is most likely to need, and it was retained when the merge
was made. Without pruning nothing is cut, and the commits of a dropped branch stay as long as a
merge brought them in.

The search reads only what that rule keeps, never whatever bytes are still on disk, so its answer
does not change when a cycle runs, and every path it follows was copied. Where a path runs into
pruned history, the entry it stopped at still names its parent by hash. If another path the rule
keeps reaches that parent, nothing is hidden there: the parent takes the sides of the path that
stopped, stale included, and the search goes on below it under the limits of the path that reached
it. That needs every chain through the parent to stop on it. Once the path has stopped, a chain
moving down treats the parent as a place where something joins it, and before that, the binary
search that finds where two chains meet looks one version below the higher of their lowest versions,
comparing the parents the two entries there name. The search runs on while a queued item could
still reach such a parent, which is any item whose level is at least the parent's version, since a
generation is never below its version.

What no path the rule keeps reaches is hidden. Below a path that is not stale, it is older than
where the path stopped, since a merge commit takes the later of its parents' times, and of lower
generation. The search then runs on until nothing that is not stale is left, and returns
`NoCommonAncestor` if the other side reached anything that old and that low, or if both sides ran
into pruned history, because the base could be hidden there. Below a stale path it is below a base
already found, so a base found elsewhere that could lie in there, no newer than where the path
stopped and at least two generations lower, could be an ancestor of another, and the search refuses
too. A merge link the search cannot follow names its parent but not the parent's version, so no
chain can be made to stop on it: a path that is not stale is cut there as before, and for a stale
path that parent always counts as hidden. Otherwise the answer is the one the whole history would
give.

Stale paths used to stop at pruned history without a trace (#45). With `Keep::Last(4)`, a commit n1
under n3 on one chain, with n2 between them, was retained on a branch forked at n1 and reached
through merge links on both sides, while the paths from n3 stopped at n2. `common_ancestors` then
answered n3 and n1, and `merge_with` refused because the step from n3 to n1 ran into pruned history.
n2 names n1, so the search now finds n1 under n3 and answers n3 alone. When no branch keeps n2, as
with `Keep::Last(3)`, the search refuses. Following named parents helps paths that are not stale as
well: in `merge_model`'s pruned graphs, with every commit at the same millisecond, 1,485 of 22,325
searches refused for one seed where 2,418 had, and with each commit a millisecond after the last,
1,662 of 22,325 where 2,716 had. Pulls gain the same way: when we have pruned the parent of our
oldest retained commit and the remote sends that commit again, the landed branch keeps it, and the
pull merges against it, or finds itself `Ahead`, where it used to refuse.

A pull's merge record names a commit of `remote` that is below our commit at the recorded version,
and the search starts from it on our side beside the tip. It used to hang there with nothing above
it, so once the pull's merge commit was pruned no path led down to it from our chain (#106). With
`Keep::Last(2)`, a pull merged at v5, a commit v6, and a fork at v6 with one commit on each branch,
both branches held the record, so both sides started from the pulled commit and it was a candidate
at once, beside v6. The path below v6 stopped at the pruned v5, the pulled commit was older and two
generations lower, and the search refused, although everything pruned was below v6. Between
branches forked after a pull, `common_ancestors` and every merge failed once the pull's merge commit
left the kept window. A local fork and `merge_with` records nothing, so only pulls and syncs showed
it. A recorded commit now also counts as a parent of the retained commit on the recording branch at
the recorded version, or of the oldest retained one when that version is pruned. A chain passing
that commit stops there and hands the recorded commit its sides, stale included, so v6 makes it
stale before it is taken, as v5's merge link would have. A record is written only for a commit below
ours at that version, so the search adds no edge the whole history lacks. The steps that build a
virtual base hang each record they add the same way, below the commit they start from, and a record
from before merge links whose commit is not of lower generation keeps only its place beside the
tip.

The edge starts at the recorded version and no lower, so a record never puts its commit below a
commit under that version. When two branches fork at f and each then pulls the same remote commit r,
both records are made above f, r is not below f, and once the commits under f are pruned, r, older
and two generations lower, could be among them: the search still refuses. `merge_model` builds the
pull above under `Keep::Last(2)` to `Keep::Last(4)` and after compactions to `Keep::Last(1)` to
`Keep::Last(3)`, a criss-cross after it, and both refusals. Its devices, which fork, merge, pull and
sync, now also run under bounded history with compactions, compare every answer with the whole
history, and require one whenever the two branches are at the same commit, which the old search
could refuse. In 6,000 random histories of that kind, 62 of 9,928 searches refused through a
candidate below another before, and none do now. Every refusal left comes from a path that is not
stale running into pruned history, such as a second pull whose remote commit is no newer than our
oldest retained commit and so could be among what we pruned.

A merged chain is kept down to where its `prev` links stop, and the search finds that place by
following them, taking a skip link only to a commit the `prev` links also reach. Two writers broke
that. A pull writes a sent commit's skip link with offset 0 when it lands below the `first` of the
branch the chain joins, though the `prev` links can go on below it through commits another merge
keeps. The search found a merged chain's bottom through `prev`, then went back down to it through
that skip link and read offset 0, and a sender comparing such a chain with the receiver's could do
the same. A step down now takes `prev` where a skip link is cut, as finding the bottom already did.
Reclamation kept a skip link whenever its target was retained, by any branch or merge, even where a
`prev` link between had been cut because the commit there was not. The search then took the skip
link past the gap and put the chain's bottom below a commit that was gone, and the next step down
failed. Reclamation now writes a skip link with offset 0 when it lands below where the copied
entry's `prev` links reach, which it knows because it copies `prev` first. A branch reaches down to
its `first` through `prev`, so its skip links above `first` all stay.

## Format

Log entries have three more flag bits: the merge link, followed by its floor version and time; the
three numbers; and a bit saying that some merge below has a limit, which tells the search when it
may stop early. Entries written before carry none of them and read as a chain without merges. The
head still holds the old `merged` map, so files written before stay readable. `merge_with` records
nothing new in it, and only sync does, as described under [Bringing the branch
up](sync.md#bringing-the-branch-up). A record left from before still counts: the recorded version of
the named branch is treated as reachable from the head that holds it, which is what the old merge
meant. A build from before this change rejects entries with the new bits.
