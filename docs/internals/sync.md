# Sync

How two files exchange commits: what a have says, what a bundle carries, how it is checked and
landed, and how the branch is brought up afterwards. How `receive` stages and type-checks a bundle
is in [Receiving a bundle](sync-receive.md). The API is in [Sync between files](../guide/sync.md).

`Db::pull(&other, remote, merge)` copies the branch of `other`, usually a handle on another file,
into this file as the branch `remote`, then brings this handle's branch up to it. It is three steps
that a transport can split: `have` on the pulling side lists what it holds, `bundle` on the other
side packs what that lacks, and `receive` checks and applies it. A have is a byte buffer and a
bundle a byte stream, so the same code runs over a file, a socket or HTTP: `bundle_to` writes to an
`io::Write` and `receive_from` reads from an `io::Read`, and `bundle` and `receive` are those two
over a `Vec<u8>`. `pull` runs the three in one process, and `push` is `pull` called from the other
side.

## Negotiation

A have lists commits as entry hash and version: the tip of every branch, then the pulling branch at
`last - 1`, `last - 3`, `last - 7` and so on down to `first`, about log2 of its length. The
two-branch file in the measurements below sends 137 bytes. The sender looks each one up in each of
its own chains at that version, O(log n) through skip links, and keeps those whose hash matches.
Entries link their parents, so a commit with the same hash in both files has the same history below
it. For each recognised commit the sender binary-searches the newest version its branch shares with
that commit's chain, O(log^2 n), and the newest of those is the floor: its commits above the floor
are new. Commits are matched by hash. A version alone means nothing across files.

The floor stays below the sender's head unless the have names the head. A commit below one the
receiver named is in the receiver's history, but not necessarily still in its file, since the
receiver may have pruned it from every branch. The floor used to reach the head whenever the sender
held the head below a recognised commit, which happens when the sender's branch is behind a commit
of the receiver's that the sender holds on another branch. The bundle was then empty, the receiver
could not land a head it did not hold and returned `StaleHave`, and a fresh have gave the same
answer, so pulls between the two files failed for good with no prune between have and bundle.
Sending the head costs its entry and what its tree changed from the anchors, and only when the
receiver holds the head without naming it.

## What is sent

The new commits' entries, and the objects their roots reach that the receiver lacks. ferro has no
index from hash to offset, so what the receiver lacks is found by comparing trees. The roots of the
recognised tips, and of the commit that set the floor, are anchors: trees the receiver is known to
hold. Each new root, oldest first, is compared with the root before it, and the first with the
anchors, one level at a time. The old side expands a level and records each object it reaches as
known. Then the new side expands a level, and a new object that is known or already sent is not
followed. An old object that the new tree still links is common to both and is not expanded.

The first version compared every new root with the anchors at once, and sent 185 objects and 95 KB
for two commits that changed one key each in a 5,000 key map. An old node that the first commit kept
and the second replaced counted as common, so nothing under it was known and the second commit's
copy went whole. Root by root it sends 12 objects and 7 KB.

A `Map`'s shape depends only on its contents, so consecutive roots line up level by level. A change
that adds or removes a level moves the unchanged subtrees one level up or down. The old side expands
before the new side looks, so a subtree that sits one level deeper in the old tree is still found,
and only one moved further is sent again. Before the old side expands a level, the new side reads
the links of the level it is on, and the old side skips what they name. Where a change added a
level, the new side is a level behind, and nothing the old side reached had been linked yet, so it
used to expand the whole old tree. In a bulk load of five million objects in ten commits, the commit
that added a level held 95 MiB besides its tables while it planned, and holds 26 now. Once the old
side has nothing left to expand, the rest of the new tree is walked depth first, so a clone holds a
path through it rather than its widest level.

A sent merge commit links the commit it merged, and the receiver needs that commit to find later
merge bases. If the receiver named it in its have, its mark goes into the bundle's list of commits
the receiver holds, and the link resolves to it there. Otherwise the merged commit is sent, with its
chain down to the newest entry a recognised commit's chain shares with it, and the same for the
merges inside it, so far as the sender keeps them under the retention rule for merges, described
under [Merges and pruning](merging.md#merges-and-pruning). A merged commit the sender has let go is
not sent, and the receiver writes the link with offset 0, which the merge-base search treats as
pruned history. The receiver may keep more of its history than the sender did, so a merge can claim
older commits than arrived. The search finds where a merged chain ends by its links, not only by the
merge's limits, and a chain cut short reads as pruned there.

A sent object that links a known one names it with a hint: the anchor or earlier hint it hangs from,
and the index of the link there. The receiver follows the path in its own file and checks that it
ends at the same hash, which costs one read of each parent on a changed path. A link to a sent
object needs nothing, because objects go children first. Bodies are LZ4 compressed when that is
smaller, as in the file.

The sender plans without bodies. The tree comparison reads only the links of each record, and keeps
the hash and offset of each object it will send and whether it links anything, with the links of the
first 64K or so objects and links so that a small bundle reads nothing twice. An object that links
nothing is not read for its links again. The sent commits are then visited in generation order, each
depth first through what it links that is to be sent, so a commit's trees and parents are out before
it and the stack is as deep as a tree, not as a history. That walk names the hints, so the header,
with the anchors, the hints and the object count, goes first. Then each object is read once more,
its hash checked, and written in 64 KiB chunks with the LZ4 bytes the file already holds for it, so
nothing is compressed twice. Planning holds about 45 bytes for each object sent, as described under
[Memory for each object](#memory-for-each-object). `pull` runs the sender on a second thread once it
sends 256 objects, with eight chunks in flight between the two.

## Applying

`receive` checks everything before anything it writes can be found:

- Every object is hashed as the bundle is read. Nothing is reserved from a count the bundle claims:
  lists grow as entries are read, and a body is read into memory as it arrives. The bodies, after
  LZ4, may come to at most 16 times the bytes read so far plus 16 MiB, checked before a body is
  read, and no body may pass the 4 GiB a record can hold. The sender sends a body uncompressed when
  compressing it would break that rule, so a bundle from `bundle` always passes. The stream must end
  after the last object.
- The new commits are walked from the head through `prev`, and their versions must descend one at a
  time. A head at 2^63 or above is rejected, so a fast-forward to it cannot leave the next commit to
  wrap the version past `u64::MAX`. The commit below the oldest one sent is looked up in the
  receiver's chains by hash and version. Found, the landed branch continues into the receiver's copy
  of it and keeps the lowest `first` among the chains that hold it, so a fast-forward does not cut
  our history short to where another branch was pruned. Not found, the link is written with offset 0
  and the branch starts at the oldest sent commit, the way a pruned chain does.
- Every commit the bundle says the receiver holds must still be in one of its chains, and so must
  the head of an empty bundle, which the have named. If the receiver pruned or dropped one after it
  sent its have, even while the bundle was streaming, `receive` returns `Error::StaleHave`, lands
  nothing, and a bundle built from a fresh have lands. `pull` returns it as well rather than
  retrying, so a caller sees the same error whether it pulls in one process or over a network, and
  decides itself whether to try again.
- Every link resolves to an object earlier in the bundle, to a hint whose path ends at the same
  hash, or, for a commit's `prev` or `skip`, to the receiver's entry at that version, checked by
  hash, or to offset 0 below the landed branch's `first`, which nothing follows. A sent commit's
  `prev` or `skip` that lands on a sent commit must name the one at that version, so a lookup
  through skip links cannot reach a commit the `prev` chain does not hold. Anything else rejects the
  bundle, and so does an object nothing links to.
- A sent commit's merge link resolves to a commit in the bundle, which must decode as a commit, to a
  commit the bundle says the receiver holds, or to offset 0. Each merged chain in the bundle is
  checked like the landed branch: versions descend one at a time, and every `prev` and `skip` names
  the commit at that version, in the bundle or in the receiver's chain it joins. A commit must use
  exactly the links it declares.
- Every sent commit's generation, reach, last merge and bounded bit are recomputed from its parents
  and must match, its time must not be before either parent's, and its merge floor must not be past
  the merged commit's version. Where a parent lies in pruned history, only what can be checked is:
  the generation must exceed the known parent's, and so on. The numbers are a function of the graph
  alone, so a commit hashes the same in every file, and a forged generation or merge link is
  rejected rather than landed.
- No sent commit may be dated more than a day past the receiver's clock. Commit times only go up
  along a chain and a merge takes the later of its parents' times, so a forged time of `u64::MAX`
  would have pinned every later commit there. A device whose clock runs more than a day fast cannot
  be pulled from until the puller's clock catches up.
- Every object the bundle sends is checked as the type it will be read as, as described under
  [Checking trees](sync-receive.md#checking-trees).

These checks run with no writer lock held, against the heads `receive` started from, as described
under [Where checked objects wait](sync-receive.md#where-checked-objects-wait). Under the writer
locks, the ones that depend on what this file holds run again against the heads found there, and the
sent commits and a `Refs` that points `remote` at the new head go into one frame, synced like a
commit, so a crash leaves the previous head: a torn frame is not replayed. Branch handles on
`remote` are rebound and its watchers fire. Their notices drop once the pull has released `tip`, so
a `watch_when` test may commit or pull. A bad bundle is `Error::Bundle` and leaves the file as it
was. Flipping every byte of a 3,358 byte bundle with three masks gave 10,073 rejections and one flip
inside an LZ4 body that decoded to the same bytes, and so landed the same commit.

## Bringing the branch up

A second step compares the pulling branch with `remote`:

- The same tip, or the same root: `UpToDate`.
- Our tip is the remote chain's entry at our version: `FastForward`. Our head takes the remote chain
  in a frame with no commit. The chains share our entries, so versions carry on from ours, and
  `head_hash` is now the same in both files.
- Their tip is our entry at their version: `Ahead`.
- Otherwise the merge-base search of `merge_with` runs under the writer lock against our tip and the
  landed `remote`. If their tip is the base, we already hold it through a merge: `Ahead`. Otherwise
  `merge` runs against that base, or against the virtual base after a criss-cross, as a commit on
  our branch that links their tip as its second parent.

Each outcome that shows their tip is an ancestor of ours records `(their version, our version)`
under `remote` in our head's `merged` map. The merge link says the same thing, but a record survives
our side of it being pruned, and the search treats it as a known ancestor of our tip, so a branch
pruned past its last sync with an idle remote still finds itself `Ahead`. Equal roots without that
ancestry record nothing, since a record there would make the remote's history look like ours. A
crash between the landing and this step leaves `remote` landed and our branch as it was. The next
pull's have names the landed tip, so its bundle is empty and the step runs again. `try_pull`,
`try_push` and `try_receive` take a merge closure that returns a `Result`, and an `Err` from it
leaves the file as that crash would, with the error returned in the caller's type. Undoing the
landing as well would take a frame of its own and make the next pull send the same commits again.

## Commit identity

An entry's hash covers its version, time, message and root, and its parent's hash, so a pulled
commit can only be the same commit in both files if it keeps its version. It lands verbatim, with
the version and time the other file gave it, and `remote` mirrors the other file's branch. A pulled
commit joins our branch only by fast-forward, and then sits at the version it has there. A merge
adds one commit at our next version, timed by our clock and never before our previous commit, and
the commits it merged stay on `remote` under their own numbers. So a version never changes meaning
on a branch: a pull appends to our chain or adopts one that extends it. After a divergence, `at(7)`
on main and on `remote` can be different commits, so compare across files by hash, with `head_hash`
and `Log::hash_at`.

Each commit's time is at least its previous commit's, as described under [Commit
times](history.md#commit-times), so time never goes back along a chain and `as_of` works after a
fast-forward. A pulled entry keeps its stamp flag, so the next commit after a fast-forward onto a
stamped commit takes a later millisecond. A device whose clock runs ahead holds the times of our
later commits at its reading until our clock passes it. `receive` refuses commits more than a day
past our clock, so that hold lasts a day at most.

## The merge base

A sync merge links the remote commit it merged, and pulled commits carry their merge links, so after
A merges B and B merges A each side's merge is an ancestor of the other's next commit, in every file
that holds them. The base is therefore the one `merge_with` finds, a best common ancestor over the
whole merge graph, with the records under our head as known ancestors of our tip. It replaces a
search by content: our roots since the last record went into a set, and the remote chain was walked
for the first root in it, then the record, then the shared prefix, then genesis. Matching by content
lost a revert routed through a third device. If A took B's change by a merge, and C took it by
fast-forward and set the value back, C's commit had the same root as one of A's older commits, so it
looked like a state A already held, and the revert was dropped on A's first pull from C. The graph
base is B's commit, which both have, and the revert applies. A change that reaches us through a
third device is a common ancestor once it is, so it is merged once, and a remote that has nothing we
lack is found to be an ancestor of our tip and reported `Ahead`.

Devices that pull from each other while a third relays make criss-crosses routinely: when A merges
B's commit and B merges one of A's that C passed on, each has merged an older state of the other,
and the next pull between them has two best common ancestors. The pull then merges against their
virtual base, as `merge_with` does. `examples/notes` needs no change for that: its merge is a
function, so it is `FnMut`, and since every note only moves forward under its last-writer-wins join,
a three-way merge against any common ancestor gives the join. The virtual base spares it conflicts
over changes both sides already hold, which it counted among the notes it applied.

When the base could lie in pruned history, the search returns `NoCommonAncestor` and the pull leaves
our branch alone, with `remote` landed. That happens when the remote has pruned past the commit we
last shared with it: its bundle no longer connects to anything we hold, and merging against genesis,
as before, made every key on our side look like our own addition and brought back what the remote
had deleted since. A device with bounded history therefore needs to keep what it commits between
syncs.

A pull is not symmetric: the closure gets `ours` and `theirs`. Devices converge only if it gives the
same result whichever side is ours, as last-writer-wins on a Lamport clock does. `examples/notes`
syncs with a pull each way, and a third pull when the second merged, so that each side has recorded
the other's last commit. A sync after that sends two empty bundles and writes nothing.

## Measurements

On one NVMe SSD with ext4, in release:

| Case | Sent |
| --- | --- |
| `Map<u64, String>`, 100,000 keys, 9.0 MB file: first pull into an empty file | 8.95 MB, 12,680 objects |
| The same, three commits of one key each | 15.2 KB, 22 objects, and a 137 byte have |
| `notes-scale`, 100,000 notes, 14.4 MB file: first sync | 14.3 MB, 41,093 objects, 220 to 240 ms |
| The same after 6 edits on one device and 8 commits on the other, one note edited on both | 106 KB, 139 objects, 11 to 13 ms, both ways and a merge on each side |
| The same, synced again | 430 bytes, no objects, nothing written |

A notes edit rewrites a path of four or five nodes in the rows and, for a tag, in the index, 6 to 8
KB a commit, and every commit since the last sync is sent, not only the final state.

Streaming was measured with `cargo run --release -p ferro-bench --bin clone`, whose `build`, `pull`,
`send`, `receive` and `incremental` steps each run in a process of their own, on a
`Map<u64, String>` of a million keys with 320 byte values, loaded in ten commits: a 330 MB file
whose clone sends 330 MB in 124,937 objects. Each case ran with fsync three times, alternating with
the in-memory version on a machine other work was sharing, and these are the medians:

| Case | In memory | Streaming |
| --- | --- | --- |
| `pull` into an empty file | 2.63 s, 1,085 MiB RSS, 1,494 MiB heap | 2.25 s, 65 MiB RSS, 56 MiB heap |
| `bundle` into a file | 1.37 s, 677 MiB RSS, 1,125 MiB heap | 1.20 s, 19 MiB RSS, 13.5 MiB heap |
| `receive` from that file into an empty one | 1.69 s, 1,071 MiB RSS, 1,276 MiB heap | 1.40 s, 63 MiB RSS, 56 MiB heap |
| `pull` of three one-key commits into the clone, 200 times | median 2.78 ms, fastest 1.57 ms | median 2.58 ms, fastest 1.54 ms |
| The same with `Durability::Buffered` | median 0.98 ms, fastest 0.65 ms | median 0.93 ms, fastest 0.60 ms |

The heap that was left was the receiver's bookkeeping, 467 bytes an object, and the sender's, 113,
since cut to about 38 and 43, as described under [Memory for each object](#memory-for-each-object).
It got faster mostly because neither side compresses a body the file already holds compressed, and
`pull` runs the two sides on two threads. A small pull costs what it did: its bundle is below 1 MiB,
so the frame is written in one piece, and the planning reads that streaming added are served from
the links the tree comparison kept.

Receiving through a scratch file was measured the same way, with a `receive-busy` step that commits
one key on another branch of the receiving file every millisecond while it receives the bundle from
a file. Each case ran with fsync six times, alternating with the build that held the writer locks
for the stream, on a faster machine that other work was sharing, and these are the medians:

| Case | Locks held for the stream | Scratch file |
| --- | --- | --- |
| `pull` into an empty file | 0.82 s, 64 MiB RSS, 55.6 MiB heap | 0.92 s, 64 MiB RSS, 55.6 MiB heap |
| `receive` from a file into an empty one | 0.66 s, 63 MiB RSS, 55.6 MiB heap | 0.78 s, 63 MiB RSS, 55.6 MiB heap |
| Commits on another branch during that `receive` | 1, which waited 0.67 s | 454, median 0.38 ms, p99 4.1 ms, slowest 6 to 12 ms |
| `pull` of three one-key commits into the clone, 200 times | median 1.31 ms, fastest 1.05 ms | median 1.32 ms, fastest 1.07 ms |
| The same with `Durability::Buffered` | median 0.54 ms, fastest 0.35 ms | median 0.57 ms, fastest 0.37 ms |

A clone costs about a seventh more, the second write and read of its bytes, and no more memory. A
small pull still lands in one frame, but copies its bundle once more and checks its chains twice,
about 25 µs.

## Memory for each object

What a sync holds grows with the objects it moves, not with their bytes, so a large first pull is
bounded by a few columns an object. The receiver keeps, for each object in the bundle:

- its hash, 20 bytes, and its offset in the staging, 8, in columns of 4,096 entries that grow
  without copying;
- a count of the links other sent objects make to it, a byte that stops at 255, which also finds
  the objects nothing links to;
- its position in a hashbrown table from hash to position, 4 bytes and a control byte a slot at
  between 44 and 88% full, so 6 to 11 bytes an object, split into 16 tables so that growing one
  copies a sixteenth.

That is 35 to 40 bytes. Placing drops the staging offsets and keeps each object's offset in the log
in their place. A check is kept only while a later link may still need it, as described under
[Checking trees](sync-receive.md#checking-trees). Each sent commit adds its graph facts, root and
segment, 88 bytes, and 40 to 60 more in indexes and the order it is checked in. Each hint an
incremental bundle names adds the object it resolves to, the kind it holds with its parent's key
range, and its place in the header, about 140 bytes. Hints are resolved grouped by parent, so one
parent's links and kinds are held at a time, where every parent read used to be kept, about 2.3 KB
each, until the last hint.

The sender keeps, for each object it will send, its hash and offset, whether it links anything and
its state in the post-order walk, 30 bytes in columns, a slot in the same kind of table, and its
place in the order, 4 bytes. A commit adds 80 bytes, and each object the old side reaches in an
incremental pull its hash, its origin as a position and link index, and a slot, about 40. The
comparison holds the level it is on while the old side has anything left to expand, and walks the
rest of the new tree depth first.

Before, the receiver kept a map from hash to staging offset, 33 bytes a slot, and every check it
made, by hash and type: an 81-byte slot, the 96-byte kind it was made under and, for a node, a
48-byte key range, 330 to 470 bytes an object. The sender kept the set of hashes to send and a map
from each to its visit state, 21 and 22 bytes a slot, built while the set was still held, and the
order as 28-byte pointers, 72 to 90 bytes an object, and a clone of one commit also held a set of
every hash its single step had linked.

Measured with `cargo run --release -p ferro-bench --bin clone`, each step in a process of its own,
Buffered, on `Map<u64, String>` files of 16-byte values loaded in ten commits unless named, with the
peak heap from a counting allocator and times the medians of three runs alternating with the build
before, on a machine other work was sharing:

| Case | Sent before | Sent after | Received before | Received after |
| --- | --- | --- | --- | --- |
| 800,000 keys, 99,882 objects | 10.3 MiB, 0.11 s | 7.0 MiB, 0.11 s | 29.2 MiB, 0.17 s | 6.9 MiB, 0.14 s |
| 8M keys, 999,734 objects | 88.5 MiB, 1.36 s | 45.2 MiB, 1.27 s | 436.3 MiB, 2.35 s | 40.5 MiB, 1.59 s |
| 40M keys, 5,000,625 objects | 346.6 MiB, 8.02 s | 205.0 MiB, 5.70 s | 1,741.3 MiB, 13.60 s | 179.8 MiB, 9.49 s |
| 8M keys in one commit, 999,654 objects | 139.6 MiB, 1.32 s | 45.2 MiB, 0.94 s | 436.3 MiB, 2.33 s | 40.4 MiB, 1.50 s |
| 1M keys in 100,000 commits, 895,489 objects | 72.6 MiB, 2.69 s | 47.6 MiB, 2.01 s | 268.2 MiB, 3.58 s | 45.5 MiB, 2.18 s |
| 200,000 of the 8M keys edited, into the clone: 236,262 objects, 709,104 hints | 173.5 MiB, 1.31 s | 142.2 MiB, 1.18 s | 321.8 MiB, 1.65 s | 114.9 MiB, 1.05 s |

At five million objects that is 43 bytes an object sent and 38 received, against 73 and 365. Past
the columns, the receiver holds a 64 KiB read buffer, up to 1 MiB of staging and, while placing, a
piece, and the sender its cache of links, a few MiB at most. The edit's sender also holds what its
old side reached, 857,288 objects of the old tree with their origins, and while it compares the
widest level, the set of what its new side has linked.

`a_clone_of_a_large_tree_holds_a_few_dozen_bytes_for_each_object` and
`a_clone_of_a_long_history_holds_a_few_dozen_bytes_for_each_object` in `ferro/tests/sync_memory.rs`
clone 499,886 objects loaded in eight commits and 407,377 in 50,000, counting the heap of each side
on its thread, and bound the sender to 8 MiB plus 44 or 64 bytes an object and the receiver to 4 MiB
plus 48 or 64. They measure 25.1 MB sent for each and 21.5 and 24.0 MB received, where the build
before took 47.6 and 38.0 MB to send and 229.7 and 140.7 MB to receive.

Small syncs got no slower. A pull of three one-key commits into the clone of the 330 MB file, 200
times, ran 4.19 million instructions at the median against 5.14 million, 18% fewer, in 1.79 ms
against 1.84 with fsync and 0.54 against 0.57 Buffered, the medians of three runs alternating with
the build before. `notes-scale` sent the same 106,337 bytes in 139 objects after its edits, in 12.1
ms against 12.8, and its first sync of 14.3 MB in 41,093 objects took 174 ms against 217. The first
pull of the 330 MB file of 124,937 objects with fsync peaks at 10.7 MiB of heap and 16.6 MiB RSS,
against 55.6 and 64.5, in 0.80 s against 1.59 at the median of five alternating runs, which ranged
from 0.77 to 2.03 s.

Measured and left out:

- A std `HashMap` from hash to position, with the hashes in their column as now: each slot holds a
  hash and a position, 25 bytes, and the one table copies itself whole as it grows. It took 101.7
  MiB to receive the million objects and 402.9 the five million, against 40.5 and 179.8, and 108.4
  and 408.9 to send them, against 45.2 and 205.0. One table of positions instead of 16 took 41.7 and
  48.4 MiB at a million, and the same at five million, whose table had grown well before the end.
- Every check kept, keyed by position instead of hash: 428.0 MiB to receive the million objects and
  311.5 for the 100,000 commits, as much as the map by hash had held, and 2.5 s against 1.5.
- Interned types: a slot an object naming the type it was checked as by a two-byte index, with its
  count, level and key range. That took 115.3 MiB for the million objects and 106.0 for the
  history, against 40.5 and 45.5, and 1.64 s against 1.50. Types intern well, but a node is checked
  under the key range its parent gives it, and ranges are as many as nodes, 48 bytes each for `u64`
  keys and more for strings.
- Counting links but keeping a check past its last link: the same 40.5 MiB for the tree, whose
  objects are linked once, and 84.0 against 45.5 for the history, where each commit links again the
  siblings of the path it changed.
- Five-byte staging offsets: 177.9 against 179.8 MiB at five million objects and nothing at a
  million, because placing, which holds the 8-byte offsets in the log, is the peak. Six-byte offsets
  in the log as well would save 2 bytes an object and limit a file to 256 TiB.
- Keeping only the first 8 bytes of each hash and reading the rest back from the scratch file to
  confirm a match, 12 bytes an object less. The million-object receive read the scratch file 2.0
  million times more and took 1.69 s against 1.50, and five million 10.0 million times, 9.72 s
  against 8.82, before placing, which would read again. An index kept on disk would pay at least
  those reads, and for the offsets and counts read with them. The sender holds no copy of what it
  sends to read a hash back from, and would hash each object again.
- A sorted array of the objects to send, searched by halving during the post-order walk: 1.83 s
  against 1.00 to send the million objects and 15.3 against 5.7 for five million, to save at most
  the table's 6 to 11 bytes an object, and only after the comparison, which needs a table as it
  goes.
- Streaming the plan in bounded chunks: the header carries the object count and the hints before
  any object, so the plan must be finished before the first object goes out, and the set of what is
  sent, the largest part of it, is needed until the last link is walked. Walking the order again as
  the objects are written would save its 4 bytes an object and keep the table alive instead.

## Limits

- A bundle streams, but the receiver keeps about 38 bytes for each object and the sender about 43,
  so the first pull of a file of 50 million objects needs about 2 GB on each side. Each commit adds
  about 140 bytes on the receiver and 80 on the sender, and each hint about 140 on the receiver, so
  a pull of scattered edits, which names about three held nodes for each one it sends, costs a few
  hundred bytes an object there. Each object is held whole, so one huge value needs a few times its
  size. A hostile stream costs the same, for what it carries and not for what it claims.
- `receive` writes a bundle over 1 MiB twice, to the scratch file as it arrives and then into the
  log, so it needs free disk for both until it lands and costs a second write and read of the
  bundle. A crash while it places the bundle, or a landing refused because a base went while the
  bundle streamed, leaves what was placed in the log until the next reclamation cycle.
- Placing a large bundle holds the cycle mutex, so a cycle that is running when the bundle has been
  checked delays the landing until it ends, and one that comes due waits for the copy. The landing
  waits for the closure running on each branch, as a cycle's pause does.
- Files that share no commit share no anchors, so their first pull sends everything, even trees both
  happen to hold.
- Each commit in a have is looked up in every branch of the sender, so recognising them costs the
  receiver's branches plus log n, times the sender's branches, times log n.
- A subtree that moved more than one level deeper than where the receiver holds it is sent again.
- A value that holds a `Ptr` into pruned history cannot be sent.
- Each pull that brings a head `remote` does not hold replaces `remote`, including commits made on
  it here. If the remote branch's history no longer contains what was landed before, records under
  that name are dropped, and if nothing retained connects the two branches the pull refuses with
  `NoCommonAncestor`.
- A merged commit the receiver holds only inside another commit's merged history, not on a chain its
  have names, is sent again, and so is a commit on a branch whose head the sender does not hold.
  Both are stored a second time, as described under [Commits stored
  twice](sync-receive.md#commits-stored-twice), until the copy the receiver had stops being
  retained.
- The receiver checks every sent object as its own `T`, so peers need schemas that decode each
  other's trees, not only their roots. A table without one of its indexes, or holding one under
  another fingerprint, passes, because the check visits only the indexes whose fingerprint it knows
  and does not rebuild the others. Reading it rebuilds the index, and if two rows
  share a value of a unique index that read fails, so a peer can still send a table that does not
  read. Checking it at receive would mean reading every row.
- A summary is checked as an `S`, not recomputed, so a peer can send a subtree whose summary is not
  the fold of its entries, and `summary` returns what it sent.
