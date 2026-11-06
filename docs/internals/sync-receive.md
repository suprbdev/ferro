# Receiving a bundle

Where `receive` keeps the objects of a bundle while it checks them, how it copes with commits it
already holds, and how it checks every object as the type that will read it. The protocol around
this is in [Sync](sync.md).

## Where checked objects wait

A bundle is read once and not kept, so its objects have to be somewhere between being hashed and
being published. They wait in a scratch file beside the database while the bundle streams and is
checked, and go into the log only once every check has passed. No writer lock is held while the
bundle is read, so a slow or stalled peer holds up no commit on the receiving file (#42).

`receive` reads the bundle's header, then resolves the anchors and hints against the published refs,
so a stale have fails before any object is read. It keeps the `Store` of that generation for the
rest of the transfer, so a reclamation cycle that swaps the file meanwhile leaves every object it
resolved readable. Each object is hashed as it arrives and appended to a staging buffer, behind its
hash, in the log's record format, with its links resolved to offsets: to an object sent earlier, by
its offset in the staging, to an anchor or hint, by its offset in the file, or, where a link names
something not sent, to offset 0 for now. Before a record would take the buffer past 1 MiB, the
buffer goes to the scratch file. The first spill creates it in the database's directory as
`<file>.receiving.<pid>.<n>` and unlinks it at once, or on Windows opens it to be deleted on close,
so neither a crash nor a failed receive leaves it behind, and a bundle under 1 MiB never creates
one. The checks above read the sent objects back from the staging without hashing them again, and
what the receiver holds from the generation `receive` started in. A bundle that fails any of them,
or a stream that errors or ends early, has written nothing to the database.

Only a commit may link to something not sent, and an object that does and is not one of the new
commits rejects the bundle. Which commit such a link names is only known once the chains are walked,
so each sent commit whose links differ from what the walk resolved is appended again with the right
ones, in generation order so that its parents' final copies come first. A clone into an empty file
rewrites none, and an incremental pull rewrites its few commits.

Placing copies the staged records into the log. Each is encoded again at its offset there, keeping
the body it was staged with, the LZ4 bytes it arrived with or those a body that arrived
uncompressed compressed to, as described under [Applying](sync.md#applying), so nothing is
compressed at placing. A link to a sent object takes
the offset it was placed at, and a link to an anchor or hint the offset of that object in the
generation now current. A bundle that never spilled is placed in the frame that lands it, under the
writer locks, which is written in one piece like a commit's. A larger one is placed first, in pieces
of 256 KiB, each a frame of its own that keeps the refs as they were, so nothing can find its
objects yet. A piece takes only the `tip` mutex, for about as long as a commit's encode, and commits
on every branch go on between pieces. The dedupe cache learns a piece's objects when the piece is
written, except its commits: until the landing, a sent commit may still link its parent at offset 0,
and a local commit that hashed the same would have reused that copy.

Under `Durability::Fsync` the pieces are synced before the landing, with no lock held, so the
landing's own sync has only its frame left to flush. The landing then takes every branch's lane and
`tip`, as before, and checks again against the heads it finds there what depends on this file: every
base must still be in one of its chains, or `receive` returns `StaleHave`, the commit below the
oldest one sent is looked up again, and every sent commit's links, generation and merge numbers are
worked out and checked against the commits this file holds now. A check that passed against the old
heads passes against newer ones unless a commit it relied on has been pruned, which only leaves less
to check. A `StaleHave` from these checks or from the first ones comes before the merge closure, so
a pull tries again from a fresh have, as described under [A stale have in a
pull](sync.md#a-stale-have-in-a-pull). The landing frame holds the copies of the commits whose links
changed and the refs that point `remote` at the head, and goes to the file 1 MiB at a time as the
old frame did, with a header written last whose hash reads back the part already written. Type
checks and hashes are not repeated: they depend only on the bundle and on objects whose hashes do
not change.

A reclamation cycle can prune, swap the file and retire the old one while the bundle streams. Before
placing, `receive` resolves the anchors again in the generation now current: each base by hash and
version, and each hint by following its path again from the anchors' roots, one read per parent
object as when it was first resolved, which must end at the same hash. Placing holds the cycle mutex from the
first piece to the landing, so no swap can move the pieces from under the frame that links them. A
cycle that comes due meanwhile waits for the copy, which runs at the speed of the disk and not of
the peer, and a cycle already running when the bundle has been checked finishes before placing
starts. A bundle that never spilled needs no such hold, because it is placed under the locks a swap
takes.

Until the landing frame is durable, nothing names the placed objects. Readers bound every offset by
the published end of the log, and the pieces are whole frames that keep the previous refs, so a
crash anywhere leaves the previous head, and pieces that reached the disk stay in the log until the
next cycle drops them. A landing refused under the locks, because a base was pruned or dropped while
the bundle streamed, leaves its pieces the same way and writes nothing else: a 3 MB bundle refused
there left the file 3 MB larger until the next cycle. A piece whose write fails, as when the disk
fills, fails the receive without poisoning the store, and the next frame is written over what
reached the file. A power cut at every write of a receive that placed a 3 MB bundle in pieces, with
a commit on the pulling branch while the stream was stalled and another on a second branch between
placing and landing, keeping or dropping each write not yet synced, always opened at the previous
head, that first commit, the landed remote or the merge, never lost a commit acknowledged before the
cut, and had nothing past the end of the log.

The receiver holds a 64 KiB read buffer, up to 1 MiB of staging not yet written, the object being
read, and for each object its hash, its offset in the staging and a count of the links to it, about
38 bytes an object, as described under [Memory for each object](sync.md#memory-for-each-object).
Placing drops the offsets in the staging first and holds each object's offset in the log in their
place, one piece and a window on the scratch file. An object is held whole, about four times over
while it is read, compressed again if it arrived uncompressed, as described under
[Applying](sync.md#applying), hashed and copied, so the largest value sent bounds memory too. A
hostile stream gets no more: 48 distinct objects of 1 MiB took 2.1 MiB of heap and never reached the
database. A 33 MB stream of 4,549 objects took 1.66 MB, where the frame it used to fill took 2.80
MB, and 256 commits with 64 KiB messages, 17 MB, took 2.2 MB.

Three other places were weighed. Writing the staged objects into the log as frames of their own
while the bundle streams writes each byte once, but a cycle that swaps the file meanwhile copies
only what the refs reach, so it would drop them, and holding the swap off until the stream ends lets
a slow peer stall reclamation instead of commits. It would also leave a refused bundle's bytes in
the file until the next cycle, so a hostile peer could grow the file with every attempt, where a
refusal now costs the database nothing. Copying from the scratch file inside the landing frame keeps
the crash story as it was, with no pieces, but holds every writer for a copy as large as the bundle.
Holding the checked objects in memory loses the bound above.

## Commits stored twice

A bundle can carry commits the receiver already holds where its have does not name them: on a branch
whose head the sender does not know, or only in a merge's history. They land as new records beside
the old ones, with the same hash and possibly other links. The receiver's copy of the oldest commit
a branch kept links its parent with offset 0, while the landed copy, whose parent came in the same
bundle, links it in full. Devices that keep a few versions and relay each other's commits make this
routine (#36). One device held a second device's v3 on the branch it had pulled from a third, which
had fast-forwarded onto it and kept nothing below it. A pull from the second device sent v2 to v4
and landed its branch with `first` at 2, through a new copy of v3 that links v2.

Every layer that keys objects by hash took the two copies of v3 for one. The node cache returned
whichever had been read first, so reading the landed branch reached the old copy's offset 0 and
failed with `Corrupt("pointer 0 outside log")`, and so did `verify`. Reclamation copied whichever it
met first, in the order of the branch names, so a cycle could leave the landed branch unreadable in
the new file even when the cache had held the right copy.

Hashes do not cover offsets, so for any other object the copies are interchangeable: a tree node
links only objects that are there. An entry is the one object whose links may be offset 0, so
entries are read and copied as the records they are. A cached entry remembers the offset it was
decoded from, and a load through a link to another copy decodes that copy without caching it, which
costs a read only where a commit is stored twice. Reclamation copies each stored entry once, keyed
by the offset it was read from, so every branch keeps the links it was written with, in the new file
as in the old. `verify` names a branch whose entries do not reach its `first`:
`commit log keeps versions from 2 but v3 links pruned history`.

The branch table links entries too, through each head's tip, so two tables with one hash can name
different copies. A remote that was dropped and then landed again from the same head gets a table
equal to the one it had before, except that its head is the new copy. `Txn::put` found the old table
in `dedupe` and the frame named it, while the handle kept the table it had encoded: `verify` walked
the branch from the new copy and the file's table from the old one, and failed on an offset 0 that
only the old copy carries, and a reopen read the old copy. The branch table is now written without
looking in `dedupe`, and so is each step a commit writes on it, as described under [Branch-table
steps](commit-path.md#branch-table-steps). Only the nodes a change touches are written, and they
hash like earlier ones only when the branches return to a state they had before, so this writes
nothing more in ordinary use.

A commit's own entry went through `dedupe` too, and two branches that make the same commit over two
copies of its parent write entries with one hash. With a fixed clock, a branch that kept only v6
after a cycle and a branch that had landed v1 to v6 again from a peer each committed the same change
as v7. The second commit took the first one's entry, so its chain ran into the copy of v6 whose
parent link is offset 0: `at(3)` and `verify` failed with `commit log keeps versions from 1 but v6
links pruned history`, and so did every reclamation cycle after it. An entry is now written without
looking in `dedupe`, like the branch table. Two entries hash alike only when their version, time,
message, root and links all do, so this too writes nothing more in ordinary use, and
`a_commit_equal_to_one_on_another_branch_keeps_the_history_of_its_own` in `ferro/tests/dedupe.rs`
checks it.

The other way was to land onto the receiver's existing record and start the landed branch where that
record's parent link stops. That throws away history the bundle brought, here v2, and it needs to
find the existing record, which for a commit held only in a merge's history takes an index from hash
to offset that `ferro` does not keep.

The node cache, which is keyed by hash, kept the same confusion for every handle that had not just
written the table (#39). A read-only handle that had loaded the table before the drop held decodes
of the table object and its node, and `refresh` after the second landing took them for the new
table's, which hash the same. Its branches started from the copy the old table named while the table
on disk named the new one, so `verify` and `verify_typed` failed on the same offset 0, and stayed
wrong while those decodes stayed cached. The writer was right only because the decodes it remembers
for the nodes it writes replace the old ones under their hash. It does not remember the table
object, so with a small cache, a `verify` that found the old table object still cached after the
remembered node had been evicted read the old node through it. With `cache_bytes` at every multiple
of 256 bytes up to 16 KiB, some size failed that way in each of 20 runs.

So the rule that served entries now covers every decode whose meaning depends on the offsets in its
links: a cached decode serves only the record it was decoded from. `Data::LOCATED`, a hidden
constant that defaults to false, marks such a type. `Entry` and `Head` set it, since they link
entries, and a `Map` takes it from its keys and values, which covers the branch table's object, its
nodes and a head stored as its own object. `Store::load` caches a marked value with the offset it
was decoded from, as it did for entries, and a map node already keeps the pointer it was decoded
from as its `origin`, so `fetch_node` compares that for a map of marked values. A hit for another
copy decodes the record without caching it, which costs a read only where an object is stored twice.
The constant is known at compile time, so every other map and value compiles to the lookup it had
before. In `bench-reads`, eight runs alternating with the build before, the medians moved by -0.3 to
+2.2% for random gets, -2.3 to -0.2% for sorted gets, -2.4 to +0.1% for cold scans and -1.8 to +1.5%
for `for` scans, inside a spread between runs of up to 18% for random gets.

Keying the cache by hash and offset separates the copies too, but every lookup would hash a longer
key, and copies of an ordinary tree node, which are interchangeable, would stop sharing a decode.
Resolving links by hash when they are followed needs the index from hash to offset that `ferro` does
not keep.

The combined model now verifies each device's refreshed reader after every step, and its three
devices run with caches of 1 MiB, 16 KiB and 2 KiB. Before the fix the reader check failed at seed
212 of the pruned sweep at a scale of 20.

### Table records found by hash

Dedupe was not the only way a table write could take a record an earlier write had made (#54). Like
any map, the table keeps the offset a changed node was read from, its origin, when the node's new
encoding hashes the same: `map::emit` hands back the origin instead of appending, so a change undone
before the next write costs nothing. A table node that did that while a head under it had moved to
another copy of its commit would name the old copy, as dedupe did. That cannot happen in a write the
handle makes. Every node of the table in memory was read from the newest table written or loaded: a
table write reads its table back from the records it wrote, and open, `refresh` and the swap of a
cycle load the table they end with. Until the next table write the table changes only by steps, and
a step puts a head whose version is one past the head it replaces. The version is hashed, so a head
a step changed never hashes like the one the origin holds. The write that ends the stretch makes its
own change, and each changes a hashed field of every head it touches: a fork adds a name,
`drop_branch` removes one and the merge records naming it, pruning raises `first`, a fast-forward
raises the version, a pull's merge records change `merged`, and the commit that would put a 65th
step raises the version as a step does. A landing is the one write that could put a head of the
same hash with another copy, and it writes nothing when the remote already holds the commit it
carries. So a node hashes like its origin only when each head under it is the one the origin's
record holds, decoded from that record, and the offset it keeps names the copies the table in memory
names.

A reclamation cycle does take table records by hash. Its passes copy the table and what it reaches
while commits go on, and the copy finds every object but an entry by its hash, so the table it
writes in the pause takes a node, or the whole table, from a pass whenever one hashes the same. A
remote dropped and landed again from the same head after a pass read the refs has a table equal by
hash to the one the pass copied, and the new file names the copy of the tip the first landing made,
while the handle held the second's. A copier that printed each record the pause took from a pass
showed it in `a_remote_landed_again_during_every_stage_of_a_cycle_verifies_and_reopens`. Nothing
reads the two copies differently. Nodes that hash alike hold heads that hash alike, which keep the
same versions from the same `first` over the same commits, and the pass that copied the node copied,
for each of those heads, its chain down to `first` and its merges newer than its first entry. That
is as far as the logs, `verify` and a merge's search for its base go, since the search marks a
merged commit older than the first entry bare. After the swap the handle reads its table from the
new file, so it names what the file names, and `verify`, a reopen and the logs pass in that test and
in the model below.

`devices_relanding_remotes_across_cycles_and_step_tables_count_each_event_once` gives the combined
model bursts of 60 to 150 commits on one branch, which carry it past the 64-step bound, remotes
dropped and landed again, and cycles that drop and land a remote again from a hook at their
`created`, `copied` or `synced` step, with a commit after it half the time.
`pruned_devices_relanding_remotes_across_cycles_and_step_tables_count_each_event_once` does the same
with pruned devices. Every step checks `verify` and `verify_typed` on each device, a refreshed and a
cold reader, and that each device counts every event once.

Measured and left out:

- Leaving the table to the pause, with the passes copying only what each head links, its root, its
  tip and its merge records, so that no pass has copied a table record for the pause to find. The
  pause grew, at the median of 41 cycles in each of two alternating runs, from 323 to 369 µs at 64
  branches and from 1.37 to 1.48 ms at 256, to change a copy that nothing reads differently, and a
  damaged table could no longer be planted between the copy and the pause, as
  `swapping_in_a_compacted_file_reports_a_damaged_branch_table` does.

Left open until #55:

- A head that lands while a cycle copies could run through entries a pass copied for a head that
  kept less history, with the links below that head's `first` cut, and the pause took those copies
  as they were. With remotes landed again from the hook, seed 73 of the pruned sweep at a scale of
  30 dropped a remote kept from v2, landed it again kept from v1 through the same records, and the
  cycle's file failed `verify` with `commit log keeps versions from 1 but v2 links pruned history`.
  Each entry's copy now records how much history it was made for, and a walk that needs more copies
  the record again, as described under [Copies made for less
  history](reclamation.md#copies-made-for-less-history). The pruned sweep lands remotes again from
  the hook as the other does.

## Checking trees

A `Map` decodes lazily, so decoding a root as `T` says nothing about the nodes under it. A bundle
whose root decoded and whose node did not used to land, pass `verify`, and panic on the first read
that reached the node. `receive` now checks each object it is sent, and no object it holds, as the
type that will read it.

`Data` has a `visit` method that walks one encoded value without building it. The derive implements
it field by field, `Option`, `Vec`, tuples and the other containers forward it, and the default
decodes and drops the value, so a hand-written `Data` gets a correct visit for free. A decoder can
carry a list of typed links. `Map::decode` adds its root link to that list as a node of its `K`, `V`
and `S` with the count it claims, so any value that holds a map, through a derived struct, a hand
written impl, a `Table`, `Set` or `List`, reports where its nodes are. A node check decodes the
keys, checks that they ascend, that each hashes to the node's level and that they fall inside the
range the parent gives the node, visits the inline values and summaries, and reports each spilled
value as a `V` and each child as a node one level down at most, with the separators around it as its
range. It returns the node's count and level, which must match what the link claimed.

The receive check is a worklist that starts at each new commit's root as `T`, oldest commit first,
and runs each root's tree to the end before the next. A link to a sent object checks that object as
the kind the link names and pushes its own links. Most objects are linked once, so their checks are
not kept. While the bundle streams, the receiver counts the links each object gets from other sent
objects, and only an object linked more than once, or also named by a hint, keeps the kinds it was
checked as, until its last link has reached it. A later link of the same type whose range contains
one it was checked under is not checked again. A link under a range no earlier check covers checks
the object again, and decodes it again under each earlier kind to learn what those gave its
children. A child is pushed again only if the new check gives it a kind that one of those does not
imply, which in a narrower range only the edge children get, and it carries the earlier kinds with
it, so a recheck goes down the edge of the subtree as it did when every check was kept. The counts
only decide what is kept: an object reached more often than it was counted is checked again, never
let through. A link that resolves to an object the receiver holds is not walked into: what the
receiver holds is trusted. Its type comes from the hint path. The anchor root is a `T`, and each
hint step checks the held parent as its kind to learn the kind of the link the step follows, one
read per parent on a changed path, which the hint resolution already paid. A held child agrees with
a sent parent when it has the same type and count, its trusted range lies inside the new one and its
trusted parent's level is no higher. Otherwise the held node is loaded and its leftmost and
rightmost keys are found down its edges, O(depth). A link that names held data of another type is
accepted only if that one object checks as the new type and holds no typed links of its own: a
string spilled in one map and the same bytes as `Bytes` in another are one object. Anything else
rejects the bundle.

The check costs the bundle, not the database: every sent object is decoded once, plus one read per
parent on a hint path and the edges of a reused subtree whose range narrowed. A node reached again
under a range its first check does not cover is checked again, and those rechecks, with the earlier
kinds each decodes it under, may not exceed twice the bundle's body bytes plus 16 MiB. Cloning a
`Map<u64, String>` of 400,000 keys, 36 MB in 50,143 objects, took 164 ms against 102 ms without the
check, and a bundle editing 1,008 keys spread over it, 3.2 MB in 2,480 objects, took 18.8 ms against
15.5 ms. That was when every check was kept, keyed by hash and type with the kind it was made under.
Keeping only the checks of objects linked twice took the receive of a clone of a million objects
from 2.35 to 1.59 s and from 436 to 41 MiB of heap, with the other changes described under [Memory
for each object](sync.md#memory-for-each-object).

Links a type does not name are not checked: a raw `Ptr`, or a field this schema no longer knows.
Nothing reads them as a type. Spilled values are not checked for the size that made them spill.
Summaries are recomputed, as described under [Recomputing summaries](#recomputing-summaries).

`Db::verify_typed` runs the same check over the whole file: the refs, and the roots of every
retained commit, as `T`, each object checked once per type. It costs a decode of everything
reachable, so it is a separate call from `verify`, which knows no types and only re-hashes.

A fuzz test parses an honest bundle with hints, applies one or two mutations to its tree objects,
flipping, setting, cutting or inserting a byte, or dropping, swapping or retargeting a link,
including to held objects through the hints, and re-hashes up to the head so the hash checks pass.
Every result must be refused, or read back whole: each map's `len` equal to its iteration, every
iterated key found by `get`, every table row found through its indexes, the summarized map's
`summary` and its two halves equal to the fold of its entries, and `verify_typed` passing.
Before the check, a single flipped byte landed and a read panicked. With it, 131,000 changed bundles
over random seeds landed and read back whole one time in seven, and were refused otherwise. The
first version checked levels and order inside a node but not the range a parent gives it, and the
fuzz test found a flipped key byte that put a key where iteration saw it and `get` did not.

### Recomputing summaries

A map node's summary is stored on the link to it and a spilled value's in its slot, and reads
trust both: `summary` returns the root link's, and a fold combines the links it covers without
reading below them. So the check recomputes them. The kind of each link to a node carries the
summary the link records, its claim. The check of a node of a map with a summary decodes its inline
values rather than visiting them, takes each spilled value's summary from its slot and each child's
from the child's link, folds them in key order, each child before the entry after it, and compares
the encoding of the result with the claim through `Data::same_encoding`, since `S` need not be
`PartialEq`. Each child's link carries its own claim down to the child's check, and the kind of a
spilled value carries the key and the summary of its slot, so the check of the value decodes it
and compares `S::of(key, value)`. From the leaves up, every summary that lands is then the fold of
the entries below it, and `verify_typed` runs the same check over a file.

An honest bundle always passes. `flush` and the staged writer of a large commit compute each node's
summary from scratch, from its entries and its children's link summaries, left to right in key
order, and never by updating an old one, and a spilled value's summary is `S::of` of the key and
value when it spilled. `of` sees only what the key and value encode, so the recompute starts from
the inputs the writer had, and the same `combine` in the same grouping gives the same bits, floats
included. The history tests of `ferro/tests/summary.rs`, `Float` among them, now pull each file
they build and run `verify_typed` on it.

An object linked more than once is checked once for each key range, as above. A node has one fold,
so a later link to it that claims a summary no earlier link claimed is refused. A value
spilled under two keys has a summary for each, so a later link whose key and summary no earlier
check confirmed decodes the value, once, keeps it with the object's checks until its last link, and
computes `S::of` for that key; each such decode counts towards the recheck budget. Held objects are
trusted. The hint path learns the claims of the links in the held parents it reads, and a held link
that claims what its held parent records needs nothing more. One that claims something else
recomputes the held node's fold from what the node stores, or loads the held value and computes
its summary. The held parents on the hint path are checked without their own claims, since only
the kinds of their links are wanted.

`of` and `combine` are the user's code, run on what a peer sent. A panic in either, from an
overflowing checked sum say, is caught around each node's fold and each value's summary, and
refuses the bundle with `Error::Bundle`, or makes `verify_typed` return `Error::Corrupt`, where a
local read of the same data would panic: `summary` returns the root link's without calling either,
but `range_summary` combines the links it covers and a commit folds every node it rewrites. The
panic hook still runs, and with `panic = "abort"` the process aborts, as it would on that read.

A map without a summary skips all of it. Whether `S` has a size is a constant, a node of a
zero-sized `S` is checked by the code that checked it before, and its links carry no claim. A first
version folded both kinds of map in one function, with the summary's parts behind that constant,
and a clone of a plain map ran 0.7% more user instructions for it, though none of the added code
ran; checking a plain node in a function of its own brought the count back.

Measured on 2026-11-06 with the receiver's file buffered, each bundle received into a fresh copy
of its file by a process of its own, the builds before and after alternating over 15 rounds, in
builds of one codegen unit so that a change elsewhere does not move inlining. The clones are of ten
commits into an empty file, and each edit bundle rewrites 1,008 random keys in one commit against
a file that holds the clone. Times are medians, and the user-space instructions varied by under
0.1% between runs of one build:

| Map | Bundle | Time before | Time after | Instructions |
| --- | --- | --- | --- | --- |
| `Map<u64, String>`, 400,000 keys of 60 to 99 bytes | Clone, 35.6 MB | 120.4 ms | 118.5 ms | 1,055.1M to 1,055.4M (+0.03%) |
| | Edit, 2.8 MB | 16.6 ms | 16.7 ms | 147.5M to 146.4M (-0.7%) |
| | `verify_typed` | 160.5 ms | 157.5 ms | 1,045.4M to 1,043.7M (-0.2%) |
| The same with a `u64` sum of value lengths | Clone, 35.7 MB | 119.5 ms | 127.3 ms (+6.6%) | 1,059.1M to 1,139.6M (+7.6%) |
| | Edit, 2.8 MB | 17.1 ms | 18.9 ms (+10%) | 149.6M to 165.4M (+11%) |
| | `verify_typed` | 160.4 ms | 168.2 ms (+4.9%) | 1,049.2M to 1,142.9M (+8.9%) |
| `Map<u64, i64>` with an `i64` sum, 1,000,000 keys | Clone, 11.6 MB | 121.2 ms | 128.7 ms (+6.2%) | 1,381.1M to 1,473.6M (+6.7%) |
| | Edit, 0.9 MB | 13.4 ms | 14.8 ms (+10%) | 151.0M to 168.6M (+12%) |
| | `verify_typed` | 175.5 ms | 193.3 ms (+10%) | 1,598.0M to 1,721.0M (+7.7%) |
| A sum of lengths over 60,000 spilled values of 600 to 799 bytes | Clone, 44.2 MB | 132.9 ms | 135.9 ms (+2.2%) | 1,107.8M to 1,136.7M (+2.6%) |
| | Edit, 1.4 MB | 12.8 ms | 15.2 ms (+19%) | 131.5M to 149.9M (+14%) |
| | `verify_typed` | 179.9 ms | 185.3 ms (+3.0%) | 1,058.6M to 1,102.5M (+4.2%) |

The plain map's small differences are in the shared walk, which carries the larger `Kind`. Counted
by part, almost all of a clone's added instructions are in checking the sent nodes, 76M of the 80M
for the sum of lengths, most of it decoding each inline value where the check used to step over
it, which `of` needs. An edit adds less there, 6.9M of 15.6M, and more on the hint path, 3.0M,
where each held parent's links now carry their claims. Over spilled values the hint path adds
4.8M of the edit's 18.5M, since each held slot's claim is a key and a summary of its own.

Measured and left out:

- Recomputing only the summaries a range query can reach. A fold uses a link whenever its range
  covers the whole child, and `summary` uses the root's, so every stored summary is reachable and
  there is nothing to skip.
- Recomputing on read: landing a received tree unchecked and refolding a subtree the first time a
  fold uses its link. The first `summary` of a clone would cost a scan of the map where it costs
  O(1), the first `range_summary` O(n) where it costs O(log n), the reads would need a mark of what
  was checked on every link, and a forged summary would surface as a panic in a read after the
  bundle landed, instead of a refused bundle. The receive check already decodes every node it is
  sent, so recomputing there adds the decode of the values and the fold, not a read.
- Comparing a hash of each summary's encoding, which would keep a check's state the size of a
  hash: a 64-bit hash is not proof against a peer that picks the forged summary, and a
  cryptographic one costs more than encoding the few bytes of a summary twice.
