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
generation now current. A bundle that never spilled is placed under the writer locks, in a frame of
its own that keeps the refs as they were and is written in one piece like a commit's, just before the
frame that lands it, so the landing can read the new head as `T` first, as described under
[Limits](sync.md#limits). A larger one is placed first, in pieces
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
pull](sync.md#a-stale-have-in-a-pull). The copies of the commits whose links changed go into one
more frame that keeps the refs, 1 MiB at a time as the old frame did, with a header written last
whose hash reads back the part already written. The head's root is read as `T`, and the landing
frame then holds only the refs that point `remote` at the head. Type checks and hashes are not
repeated: they depend only on the bundle and on objects whose hashes do not change.

A reclamation cycle can prune, swap the file and retire the old one while the bundle streams. Before
placing, `receive` resolves the anchors again in the generation now current: each base by hash and
version, and each hint by following its path again from the anchors' roots, one read per parent
object as when it was first resolved, which must end at the same hash. Placing holds the cycle mutex
from the first piece to the landing, so no swap can move the pieces from under the frame that links
them. A cycle that comes due meanwhile waits for the copy, which runs at the speed of the disk and
not of the peer, and a cycle already running when the bundle has been checked finishes before
placing starts. A bundle that never spilled needs no such hold, because it is placed under the locks
a swap takes.

Until the landing frame is durable, nothing names the placed objects. Readers bound every offset by
the published end of the log, and the pieces are whole frames that keep the previous refs, so a
crash anywhere leaves the previous head, and pieces that reached the disk stay in the log until the
next cycle drops them. A landing refused under the locks, because a base was pruned or dropped while
the bundle streamed, leaves its pieces the same way and writes nothing else: a 3 MB bundle refused
there left the file 3 MB larger until the next cycle. One refused because its head does not read as
`T` has written its objects, or the relinked copies of its commits, in the frame before the landing
by then, and leaves that frame the same way. A piece whose write fails, as when the disk
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
refusal by these checks now costs the database nothing. A head that passes them and does not read
as `T` is refused only after its objects are written, but such a head used to land, so it grew the
file by as much. Copying from the scratch file inside the landing frame keeps
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
checked as, the last 16 for each type. It keeps those of each type until as many links as it has
have reached it as that type, since a parent checked as two types reaches each child once as each
through one link, and a type that fewer links reach it as keeps its check until the walk ends. A
later link of the same type whose range contains one it was checked under is not checked again. A
link under a range no earlier check covers checks the object again, and if that check finds typed
links, decodes it again under one earlier kind, the one its parent's recheck carried down or else
the last one kept, to learn what that gave its children. A child is pushed again only if the new
check gives it a kind that the earlier one does not imply, which in a narrower range only the edge
children get, and it carries that kind with it, so a recheck goes down the edge of the subtree. The
counts only decide what is kept: an object reached more often than it was counted is checked again,
never let through. A link that resolves to an object the receiver holds is not walked into while the
receiver holds it as the type the link names: what the receiver holds is trusted. Its type comes
from the hint path. The anchor root is a `T`, and each hint step checks the held parent as its kind
to learn the kind of the link the step follows, one read per parent on a changed path, which the
hint resolution already paid. A held child agrees with a sent parent when it has the same type and
count, its trusted range lies inside the new one and its trusted parent's level is no higher.
Otherwise the held node is loaded and its leftmost and rightmost keys are found down its edges,
O(depth). A link to held data of another type is checked below it, as described under [Held data
linked as another type](#held-data-linked-as-another-type). Anything else rejects the bundle.

The check costs the bundle, not the database: every sent object is decoded once, plus one read per
parent on a hint path, the edges of a reused subtree whose range narrowed, and what held data
linked as another type reaches, read once. A node reached again under a range no earlier check
covers is checked again. Every check of a sent object, the first ones included, is charged its body
bytes, and a recheck that also decodes the object under an earlier kind is charged them twice. A
bundle whose checks pass twice its body bytes plus 16 MiB is refused, so its rechecks may decode
about as much as it carries plus 16 MiB. A node of X bytes that is most of what a bundle carries
can be reached under about 2 + 16 MiB / X ranges in all: a node of 17.6 MB under two, of 1 MiB
under 18 and of 800 KB under 22.

A recheck used to decode the node under every earlier kind, and was charged its body once more for
each, so k narrowings of a node of X bytes cost X(2 + 3 + … + (k + 1)) on top of its first check,
and one narrowing of a node over 16 MiB was refused. That refused honest clones of a
`Map<String, u8>` whose leaf held eight keys of 2.2 MB: with a key added before the leaf in a
second commit, which narrowed its range, or in one commit whose root held the map with and without
that key, and twelve commits that each narrowed a leaf of eight keys of 100 KB. All three now land.
A bundle that links one node under many ranges is still refused once its checks pass that budget,
after decoding at most twice its body bytes plus 16 MiB: one of 1.05 MB that links a 1 MiB node
under 64 narrowing ranges is refused 2 ms in, and one of 16.8 MB that links a 16 MiB node so
15 ms in, as before. A recheck of a small node costs more than its bytes, but each one follows a
link the bundle carries, of at least 33 bytes, or a recheck of its parent that paid for the bytes
naming it: a bundle of 22 MB that links a node of 23 bytes under 400,000 narrowing ranges lands in
0.72 s, against 0.57 s when the ranges widen and the node is checked once.

A tree linked as two types is checked as each, which spends its bytes twice and leaves the 16 MiB
for rechecks. The links an object keeps its checks for used to be counted for all its types
together, so an object reached through one link as two types lost its checks one link early, and its
last link checked it and everything below it again as if for the first time. A `Map<u64, u32>`
copied into a `Map<u64, u64>` holds the same nodes, since the values encode alike, so a clone of the
commit before the copy and the copy checks each node as both types, and a node that an edit after
the copy also links is reached once more than it is linked. A clone of 3,000,000 keys copied that
way and edited 100 times, 35.8 MB, was refused with "it reuses its objects under too many key
ranges" 0.69 s in: its first checks came to 27.1 MB, its checks as the second type to 27.0 MB, and
13 nodes the edits also link had checked 225,281 objects below them again, 16.8 MB, when the budget
ran out. One edit after a copy of 2,000,000 keys, 23.3 MB, was refused too, and so were two maps of
24,000 keys of 1 KB written alike in two fields as two types and edited three times, whose every
link reaches a node as both types. Counted for each type, all of them land: the copy of 3,000,000
keys and 100 edits in 0.62 s, after checking 54.1 MB, 74 bytes of it again under narrower ranges. A
copy of 1,000,000 keys and 100 edits, 11.4 MB, which landed before, lands in 0.20 s against 0.27 s,
at the median of five alternating runs. A type that fewer links reach an object as keeps its check
until the walk ends, 75 checks of 74 objects in the copy of 3,000,000 keys, and each check still
keeps at most 16 kinds. The budget is unchanged, and so is what a node linked under many ranges as
one type may cost: a node of 1 MiB lands under 18 narrowing ranges and is refused under 19 and 64,
and one of 16 MiB under 64, as before. `a_clone_of_a_map_copied_into_a_wider_type_and_edited_lands`
and `a_clone_of_two_maps_written_alike_as_two_types_lands` in `ferro/tests/sync.rs` clone such a
copy of 24,000 keys of 1 KB with three edits, and those two maps, with the copy of 3,000,000 keys
and 100 edits behind `--ignored`; each failed before.

Cloning a `Map<u64, String>` of 400,000 keys, 36 MB in 50,143 objects, took 164 ms against 102 ms
without the check, and a bundle editing 1,008 keys spread over it, 3.2 MB in 2,480 objects, took
18.8 ms against 15.5 ms. That was when every check was kept, keyed by hash and type with the kind
it was made under. Keeping only the checks of objects linked twice took the receive of a clone of a
million objects from 2.35 to 1.59 s and from 436 to 41 MiB of heap, with the other changes
described under [Memory for each object](sync.md#memory-for-each-object). Rechecking under one
earlier kind left a clone of that map in ten commits, 35.8 MB, at a median of 130 ms against
129 ms, and an edit of 1,008 keys at 19 ms against 18 ms, over eleven alternating runs.

Each decode stops past 2^24 items that take no bytes and no links in one object, the limit a commit
writes each object under, as described under [Zero-width items](storage.md#zero-width-items), so a
bundle of objects written under it never meets it. The checks also count those items across the
whole bundle, against a count its size allows, as described under [Zero-width items in a
bundle](#zero-width-items-in-a-bundle).

Links a type does not name are not checked: a raw `Ptr`, or a field this schema no longer knows.
Nothing reads them as a type. Spilled values are not checked for the size that made them spill.
Summaries are recomputed, as described under [Recomputing summaries](#recomputing-summaries), and
the indexes of each table are checked against its rows, as described under [Indexes that agree
with their rows](#indexes-that-agree-with-their-rows).

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

### Held data linked as another type

Objects are found by hash, and values of different types can encode alike, so a bundle can link
an object the receiver holds as one type as another. A `Map<u64, u32>` copied into a
`Map<u64, u64>` keeps every node, since both values are varints. A string spilled in one map and
the same bytes as `Bytes` in another are one object, and so is a value of a derived struct and of
another with the same field names. Such a link checks the held object as the new type and then
walks below it, held objects all the way down: each typed link the check finds is checked as the
kind it names, with the range its new parent gives it. Beside each new kind the walk carries the
kind the receiver holds the object under, which it learns by checking the held parent as the kind
it is held under, as a hint step does, and a child whose held kind implies its new one stops the
walk, as a nested map does whose type both structs share. A held object that does not decode as
the new type, or one below it that does not, or a key outside the range its new parent gives it,
rejects the bundle: a `u64` past `u32::MAX` in a leaf of a map linked as `Map<u64, u32>`, bytes
that are not UTF-8 linked as a `String`, or a held subtree hung under a new parent whose separator
falls among its deeper keys.

The walk reads through the view that recomputing summaries uses, described below, so the first read
of each held record is free and every later one is charged against that allowance. It counts the
zero-width items of every check it makes as the new type, the one on a free first read included,
since the new type decides how many there are, as described under [Zero-width items in a
bundle](#zero-width-items-in-a-bundle). A summary that must be recomputed from a value holding a
collection is checked after everything below it, as a deferred object of the bundle is. A bundle
whose walk would read more again than the allowance has left is refused with `Error::Bundle`,
"checking held data it links as another type reads more than its size allows".

The walk remembers each record it has checked as each type, by the record's offset in the file and
the type, with the count and level the check found, and so checks a copied tree once in a receive,
however many commits of the bundle link into it: a value shared by many keys of the copied map is
checked once, and so is each sibling of the paths that later commits rewrite. A check covers a link
under any key range. A node that passed under one range, with everything below it checked as well,
has its keys in order and each below it inside the separators around it, so it passes under any
range that holds its leftmost and rightmost keys. A later link is let through when the range the
receiver holds the record under, from its hint or from its held parent, lies inside the new one, as
it does for each sibling of a rewritten path, whose separators are the ones its old parent gave it.
Otherwise the walk reads down the record's two edges to its extreme keys through the view, O(depth)
reads at a charge, as a held node of its own type is checked against a new range. A later link
whose summary no check confirmed recomputes it, as above. An offset names one record for the whole
receive, which keeps the store of the generation it started in, and a record written twice at two
offsets is two records to the walk, each read once for free.

The memo keeps no key ranges, so what it holds does not grow with the keys. Each check takes 24
bytes, the offset, the count, the type as an index, the level and when it was last used, in a table
of at most 262,144 slots of 25 bytes, 6.25 MiB, holding up to 229,376 checks, and growing by
doubling, so 9.4 MiB at the peak while it moves into its largest size. When it is full the walk
drops the half that is cheapest to check again: the lowest levels first, leaves and spilled values,
and within a level those met longest ago. The nodes above the one the walk is at and the top of the
tree, whose subtrees a later link would walk again, go last, and a value that keys keep reaching
goes after those no key has reached since. A check dropped and needed again is made again, at a
charge. A copy of a million keys, 124,583 nodes, fits whole, and so does one of 500,000 keys over
100,000 distinct values; a copy of three million keys, 374,000 nodes, drops leaves, which a later
commit's link reads again one at a time.

A hint's kind comes from the held parent it names, checked as the kind that parent is held under,
and the sender names one parent for each object it hints, the first that reached it. When that
parent links the same object in more than one place as two types, as the root of a struct links two
maps that hold the same nodes as two types, the hint keeps the kind of each, up to four types, and
so does each hint below it, whose parent is then checked as each of its kinds. Two maps written
alike, kept in two fields as two types, then share every node, and a pull that edits both checks
the edits as each type and walks nothing it holds. Two maps that share subtrees under roots that
differ still get the kind of one of them, so a pull that links a shared subtree as the other type
walks it, once. A parent whose typed links are all of one type, as a map node's are unless it
spills values, is not searched for an object it links twice, and one that does is searched among
links of different types only: a pull of three one-key commits into a clone of 400,000 keys ran
3.40 million instructions at the median against 3.38, 0.7% more, in the same 1.2 ms, where
searching every parent cost 4.7%. The clone of those 400,000 keys of 80 bytes in ten commits,
35.8 MB, received in a median of 0.143 s against 0.144 s, and an edit of 1,008 of them, 2.9 MB, in
25 ms against 26 ms, over eleven alternating runs.

Measured on 2026-11-08 with the probes of #111, Buffered, a million keys a `Map<u64, u32>` copied
into a `Map<u64, u64>` in one commit, before and after, one run each:

| Case | Before | After |
| --- | --- | --- |
| The copy and one edit of the copy, in one pull, 2.6 KB | 0.267 s | 0.112 s |
| The copy and 30 edits, 73 KB | 4.07 s | 0.112 s |
| The copy and 40 edits, 98 KB | refused 4.57 s in, and on every retry | 0.110 s |
| The copy and 100 edits, 245 KB | refused 4.87 s in | 0.114 s |
| Three million keys, the copy and 100 edits, 251 KB | refused 4.93 s in | 0.346 s |
| The copy alone, both maps kept, 178 bytes | 0.135 s | 0.110 s |
| Then each pull of one edit to both maps, 2.6 KB | 0.133 s | under 1 ms |
| 100 edits to both in one pull, 248 KB | refused 4.92 s in | 4 ms |
| Three million keys, the copy alone | 0.415 s | 0.339 s |
| Three million keys, each pull of one edit to both | 0.405 s | under 1 ms |
| A `Map<u64, String>` of 500,000 keys over 10,000 distinct values of 700 bytes, copied into a `Map<u64, Bytes>`, 164 bytes | 0.425 s | 0.140 s |
| The same over 20,000 values | refused 0.65 s in | 0.152 s |
| The same over 100,000 values | refused 0.78 s in | 0.245 s |

A copy of a `Map<String, u32>` of 150,000 keys into a `Map<String, u64>` in a bundle of 154 bytes
took 6.5, 37.9 and 263.4 MiB of heap above what the pull started with for keys of 8 bytes, 1 KiB and
8 KiB, and now takes 1.3, 1.5 and 4.2 MiB. The copy of a million `u64` keys takes 9.7 MiB against
5.8, as its 124,583 checks move the table into its largest size, and one of three million the same
9.7 MiB against 6.2. `checking_held_data_linked_as_another_type_holds_no_more_for_long_keys` in
`ferro/tests/sync_memory.rs` copies 40,000 keys of 20 bytes and of 2 KiB and bounds the second's
heap to the first's plus 8 MiB: 0.4 and 0.8 MB, where the memo before took 1.9 and 22.7 MB.

The memo used to keep the kinds it checked each record under, with the last 16 key ranges of each,
for the 16,384 records it checked or found most recently, in two generations of 8,192 that dropped
the older when the newer filled. Each range held a copy of the two separators around it, so the heap
grew with the keys, and a walk over more than 16,384 nodes dropped the top of its own tree, the
nodes it checked first, before it was done. Every later commit in the bundle then walked most of the
copied tree again, from its second read on at a charge, until the allowance refused the bundle, on
every retry, though a fresh clone of the same head landed in 0.3 s. A spilled value shared by keys
far apart was dropped before its next key reached it, so a copy of more than about 16,000 distinct
shared values was refused. And a pull of two maps kept and written alike walked the whole copy on
every pull, as its hints gave the held nodes the kind of the first map alone. Weighed and left out:
keeping the range each record was checked under, counted in bytes with its separators, which would
bound the memory but drop the checks of long keys sooner, where the range the receiver holds a
record under already covers the honest case; keying the memo by hash, which takes 48 bytes a check
where the offset takes 24; and a record in the file of the held trees checked as each type, which
would spare the walk on later pulls of maps whose roots differ, but needs a place in the file and a
way to drop it when the tree goes.

A link to held data of another type used to be accepted only if that one object checked as the new
type and held no typed links. Map nodes with children or spilled values always hold some, so every
pull after a commit that copied a map of more than one node into another type was refused, retries
included, while a fresh clone of the same file landed, and so were the identical nodes of the two
maps when a spilled `String` and the same bytes as `Bytes` sat under the same key. Recording more
than one kind for each hinted object was weighed then: the receiver had reached such an object
under the one kind its hint gives, and its bytes have to be checked as the new type all the same.
A hint whose parent links the object in more than one place now records each, as above.

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

A value can hold a collection that `of` reads, `roles.contains(&Admin)` say, and that collection's
nodes are either in the bundle, staged and not yet in the file, or held. The walk decodes values
without a store, so it decodes a value for `of` only when that records no typed link and needs no
store. Any other value is visited as before, and the node or spilled value that holds it is marked
deferred. Once every root has been walked, and so every object the bundle carries has passed its
structural check, each deferred object is checked again through a view of the receiving file: a
read-only store of its own, with its own id and no cache, that shares the open file. While the
deferred pass runs, the bundle's staging is lent to the view, and a read through the view takes
anything staged in the bundle by its hash, on whichever thread reads, so an `of` that hands a
nested map to a thread of its own reads it as it would read its own. The pass takes the staging
back under a lock that each read of it holds, so a read still under way finishes first, and a read
after that finds only the file. Every node and value read through the view is decoded afresh and
belongs to the value that read it, staged or not, so nothing it decodes, and no memo or pin a read
leaves in one, reaches the receiving store's cache or a node its readers share, and the view writes
nothing. It lets a node claim as many entries as the file and the bundle's bodies could hold
together. So `of` reads a nested collection as a local read would, only after its nodes passed the
checks that bound how long a scan can take, and a value whose decode needs a store, such as a
`Table` whose index the receiver added and rebuilds, is accepted as a local read would accept it.

The summary itself can hold a collection, a `Set` of the values below say, and then each link to a
child and each spilled value's slot holds that collection's root link too. The walk checks those
nodes as nodes of their own type, as it would a collection in a value, but folding the summaries
takes `combine`, which reads them, and without a store it cannot. So a node whose stored summaries
record a typed link is deferred like one whose values do, and its recheck reads them through the
same view, charged by the same rule, once their nodes have passed. The check of a spilled value
needs no store: it compares the encoding of `S::of` with the summary of the slot, and a collection
encodes as its root link, whose hash the comparison reads. Before this, such a summary failed to
fold without a store, so `verify_typed` reported a healthy file as corrupt and `receive` refused
every bundle once the tree had a second level. A spilled value's slot stores the value's link
before the summary's, and the check also read them the other way round, so a summary holding a
collection got the value's link and the value the summary's, and an honest spilled entry was
refused; it now reads the value's link first.

A first version read the deferred objects through the receiving store itself, keeping only what it
decoded from the staging out of the cache. A staged node still found its way into a held one: when
the bundle carried a node the receiver also held, `of` reached it through a held parent from the
cache, and the read pinned the staged child, whose links lead into the staging, into that parent.
The receiver's own reads below it then followed staging offsets into its file and panicked, after a
refused bundle as after an accepted one. The next lent the staging through a scope on the vetting
thread alone, and an `of` that summed a nested map on a thread of its own found none of the
bundle's objects there, so an honest bundle was refused as one that does not decode.

An honest bundle's summaries always agree with the recompute. `flush` and the staged writer of a
large commit compute each node's summary from scratch, from its entries and its children's link
summaries, left to right in key order, and never by updating an old one, and a spilled value's
summary is `S::of` of the key and value when it spilled. `of` sees only what the key and value
encode, so the recompute starts from the inputs the writer had, and the same `combine` in the same
grouping gives the same bits, floats included. The history tests of `ferro/tests/summary.rs`,
`Float` among them, now pull each file they build and run `verify_typed` on it. An honest bundle can
still be refused for what the recompute would read, as described below, and for an `of` or
`combine` that panics on it.

An object linked more than once is checked once for each key range, as above. A node has one fold,
so a later link to it that claims a summary no earlier link claimed is refused. A value spilled
under two keys has a summary for each, so a later link whose key and summary no check of that
object confirmed reads and decodes the value again and computes `S::of` for that key. Nothing
decoded is kept between links: a decoded value can be many times its encoding, and a value linked
under many keys would otherwise stay in memory until its last link arrived. Each confirmed key and
summary is remembered by the hash of its encoding with the object and the types it is checked as,
up to 65,536 of them, so the same pair met again is not computed again.

A recheck summarizes each inline entry as it decodes it and drops the value before the next, where
it used to keep a node's values for the fold, which cost an edit of a map of maps measured below 6%
more instructions and time. An inline entry has no summary of its own to remember, and a bundle of
several commits carries each version of a node it rewrites, so the deferred pass would summarize
every entry once for each version, and read again each collection those entries hold. For a bundle
of more than one commit, each recheck instead takes an entry's summary from a memo kept for the
receive, by the types of the map and the hash of the key's encoding, the value's bytes and the
hashes of its links, and calls `S::of` only for an entry it has not met, after checking that its
value decodes the bytes it visits. The memo keeps two generations of up to 16,384 entries each and
drops the older when the newer fills, keeping what is found in it. An entry recurs only in another
version of its node, so a bundle of one commit keeps no memo: hashing each entry for one cost an
edit of 1,008 nested maps 2.4 ms of its 84 and found nothing.

Recomputing summaries can read far more than the bundle carries: a value spilled under many keys is
read once for each, and `of` reads a nested collection whole for each entry that holds it. The
receive makes one view, when it first needs one, for the deferred pass and for the hint path below,
and charges what it reads against an allowance of eight times the bundle's body bytes, plus 4 KiB
for each link the bundle carries, plus 256 MiB, past which the bundle is refused with
`Error::Bundle`, "recomputing its summaries reads more than its size allows", or "checking held
data it links as another type reads more than its size allows" when the read that ran out was the
walk described under [Held data linked as another
type](#held-data-linked-as-another-type). The first read of
each record of the receiver's file is free. The view keeps a bit for each 8 bytes of the file, in
chunks of 512 bytes that each cover 32 KiB, allocated as its reads first land in them. A map node's
record takes at least 8 bytes, and a record that starts in the same 8 bytes as a shorter one read
before it is charged as read again. A read of an object the bundle carries, a second read of a
record, and each check of a sent spilled value again under another key are charged. So reading what
the receiver holds once costs an honest bundle nothing, however much it is, and what a bundle can
make the receiver read again is paid for by what it carries. The zero-width items a read decodes
are counted when its bytes are charged, as described under [Zero-width items in a
bundle](#zero-width-items-in-a-bundle). A bundle of a few hundred bytes can so
make the receiver read, once, every collection held in the values of the nodes it rewrites, as
committing them cost the sender, and the view's record of what it read takes at most 1.7% of the
stretch of file those reads cover. The record lasts one receive, so a bundle that is refused, and
sent again, makes the receiver read what it holds again each time: a bundle of 1,517 bytes whose
summary did not check read 1.28 GB of held pages on each attempt, about 380 ms, where before the
free first read the allowance refused it after 256 MiB. A value of up to 4 KiB lands under any
number of keys, since each key's slot brings the allowance for one more read of it, and a value of 1
MiB lands under about 265 keys and no more, where each key costs the bundle a slot of some 25 bytes
and the receiver a read of the whole value. A nested map of 40,000 entries that `of` sums under
1,500 keys of one node is refused about a second in, where reading it each time would take several,
and a nested map stored once in the receiver's file and shared by several keys is charged from its
second read.

The deferred pass used to charge every read, held or staged. Fourteen keys in one node, each holding
a nested `Map<u64, u64>` of 2,500,000 entries that `of` sums, make a file of 403 MB, and a bundle
of 555 bytes that added a fifteenth key was refused 2.1 s in, on every attempt, for reading the
fourteen held maps once each; before the deferred pass it landed in 2.9 s. It now lands in 2.5 to
2.7 s, with 6.4 MiB at the peak of the heap. Three commits that each add a key land in the same
time, where without the memo even two were refused, since each later version of the node read all
fourteen again. Twelve commits that each change the same one of the fourteen maps land in 4.4 s, and
sixteen are refused 4.7 s in, as each version of that map is a new value to read whole. Measured and
left out:

- A set of the offsets read, in place of the bits: the same receive peaked at 108 MiB of heap for
  the five million or so records it read, and took 3.1 s.
- An allowance that adds the file's size, which needs no record at all. It would let a small
  bundle buy as many bytes of reads again of one object the receiver holds, a nested map shared
  under many keys say, as its file holds, so the work that bundle can buy would grow with the
  receiver's file. With the record, what it can buy again is its own allowance, and what it reads
  for free is the receiver's data, once.
- A cache of held objects in the view. It saves the decode of a second read but not the scan `of`
  makes, so a second read would still be charged, and a cached node would carry pins between reads
  of the view that the view now never keeps.

Held objects are trusted. The hint path learns the claims of the links in the held parents it reads,
and a held link that claims what its held parent records needs nothing more. One that claims
something else recomputes the held node's fold from what the node stores, or loads the held value
and computes its summary, remembered as above, through the same view and charged by the same rule,
so the collections `of` reads in a held value are charged too. So is the check of held data linked
as another type. The held parents on the hint path are checked through the receiving store without
their own claims, since only the kinds of their links are wanted. The recompute used to load a held
value through the receiving store and charge its body alone, and not the first time: with a note of
a 600-byte string and a map of 1,000,000 entries held, a bundle of 7,056 bytes that put the note
under 200 more keys took 15.0 s to land, summing the map once for each key. It is now refused
2.3 s in, after about twenty sums, and five keys land in 0.35 s.

`verify_typed` checks a file it owns, and trusts nothing in it, so its walk checks each object
without the store, as the receive does, and checks an object it marked deferred again with the
file's store once every object below it has passed. The deferred object goes on the walk's stack
under the links it found, so it comes off once they have all been checked and no list of deferred
objects is kept. It used to decode those values with the store as its walk reached them, before any
node of the collections they hold had been checked. A node's count is bounded only by the size of
the file, so a file of 64 MiB holding a nested map of 21 nodes, each linking the next three times,
made `of` walk ten billion entries over 69 s before the walk reached the first of those nodes and
found its keys at the wrong level. That now takes 0.07 s.

`verify_typed` remembers at most 4,096 confirmed pairs, and checks a link whose claim no earlier
check of its object confirmed when it reaches it, so that it holds no list of such links: the first
version kept every one with its kind for a pass after the walk, and verifying 2,000,000 keys that
share one value of 600 bytes took 785 MB at peak where it had taken 118 MB.

`of` and `combine` are the user's code, run on what a peer sent. A panic in either, from an
overflowing checked sum say, is caught around each node's fold and each value's summary, and
refuses the bundle with `Error::Bundle`, or makes `verify_typed` return `Error::Corrupt`, where a
local read of the same data would panic: `summary` returns the root link's without calling either,
but `range_summary` combines the links it covers and a commit folds every node it rewrites. A read
inside `of` that fails, an I/O error say, panics as reads do, and the check returns that error
rather than the panic. The panic hook still runs, and with `panic = "abort"` the process aborts, as
it would on that read.

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
| `Map<u64, String>`, 400,000 keys of 60 to 99 bytes | Clone, 35.6 MB | 124.0 ms | 120.0 ms | 1,055.1M to 1,056.4M (+0.1%) |
| | Edit, 2.8 MB | 16.9 ms | 17.0 ms | 147.5M to 146.6M (-0.6%) |
| | `verify_typed` | 161.3 ms | 160.1 ms | 1,045.4M to 1,044.4M (-0.1%) |
| The same with a `u64` sum of value lengths | Clone, 35.7 MB | 124.3 ms | 128.2 ms (+3.1%) | 1,059.1M to 1,146.7M (+8.3%) |
| | Edit, 2.8 MB | 17.0 ms | 18.3 ms (+8.0%) | 149.6M to 165.9M (+11%) |
| | `verify_typed` | 160.3 ms | 168.1 ms (+4.9%) | 1,049.1M to 1,149.3M (+9.6%) |
| `Map<u64, i64>` with an `i64` sum, 1,000,000 keys | Clone, 11.6 MB | 121.5 ms | 130.5 ms (+7.4%) | 1,381.1M to 1,492.1M (+8.0%) |
| | Edit, 0.9 MB | 13.7 ms | 15.2 ms (+11%) | 151.0M to 169.3M (+12%) |
| | `verify_typed` | 174.4 ms | 195.2 ms (+12%) | 1,598.0M to 1,738.6M (+8.8%) |
| A sum of lengths over 60,000 spilled values of 600 to 799 bytes | Clone, 44.2 MB | 133.9 ms | 137.6 ms (+2.8%) | 1,107.8M to 1,139.4M (+2.9%) |
| | Edit, 1.4 MB | 12.9 ms | 14.9 ms (+15%) | 131.5M to 150.3M (+14%) |
| | `verify_typed` | 177.5 ms | 186.0 ms (+4.8%) | 1,058.6M to 1,104.9M (+4.4%) |

The plain map's small differences are in the shared walk, which carries the larger `Kind`. Counted
by part, almost all of a clone's added instructions are in checking the sent nodes, 83M of the 88M
for the sum of lengths, most of it decoding each inline value where the check used to step over
it, which `of` needs. An edit adds less there, 7.3M of 15.9M, and the rest on the hint path,
where each held parent's links now carry their claims, and in the held links that compare them.
Over spilled values the sent nodes add 4.6M of the edit's 18.7M, and the hint path most of the
rest, since each held slot's claim is a key and a summary of its own. No map in these runs holds a
collection in its values, so none was deferred.

The free first read of a held record, the memo of entries and `verify_typed`'s check without the
store were measured the same way on 2026-11-06, over 11 rounds, with bundles a little different
from those above and a fifth map whose values are maps, so that every node a bundle carries is
deferred: 20,000 keys, each holding a `Map<u64, u64>` of 60 entries that `of` sums, built in ten
commits of 2,000 keys, and an edit that adds an entry to 1,008 random inner maps:

| Map | Bundle | Time before | Time after | Instructions |
| --- | --- | --- | --- | --- |
| `Map<u64, String>`, 400,000 keys of 60 to 99 bytes | Clone, 35.6 MB | 121.2 ms | 120.3 ms | 1,053.3M to 1,053.5M (+0.0%) |
| | Edit, 2.9 MB | 16.2 ms | 16.4 ms | 149.5M to 149.5M (-0.0%) |
| | `verify_typed` | 150.1 ms | 150.7 ms | 1,042.9M to 1,045.5M (+0.2%) |
| The same with a `u64` sum of value lengths | Clone, 35.6 MB | 127.0 ms | 127.3 ms | 1,145.3M to 1,145.5M (+0.0%) |
| | Edit, 2.9 MB | 17.6 ms | 17.5 ms | 168.9M to 168.9M (-0.0%) |
| | `verify_typed` | 165.5 ms | 163.3 ms | 1,145.7M to 1,150.9M (+0.5%) |
| `Map<u64, i64>` with an `i64` sum, 1,000,000 keys | Clone, 15.8 MB | 142.7 ms | 142.8 ms | 1,634.0M to 1,636.5M (+0.2%) |
| | Edit, 1.2 MB | 16.1 ms | 16.0 ms | 182.0M to 182.1M (+0.1%) |
| | `verify_typed` | 207.2 ms | 209.8 ms | 1,826.3M to 1,841.2M (+0.8%) |
| A sum of lengths over 60,000 spilled values of 600 to 799 bytes | Clone, 44.1 MB | 136.2 ms | 134.8 ms | 1,142.3M to 1,142.4M (+0.0%) |
| | Edit, 1.4 MB | 13.8 ms | 13.8 ms | 147.5M to 147.4M (-0.0%) |
| | `verify_typed` | 178.0 ms | 178.6 ms | 1,103.1M to 1,106.7M (+0.3%) |
| `Map<u64, Map<u64, u64>>` summing each inner map, 20,000 keys | Clone, 15.8 MB | 299.6 ms | 302.6 ms | 3,256.5M to 3,197.5M (-1.8%) |
| | Edit, 0.7 MB | 83.0 ms | 78.8 ms (-5.1%) | 824.8M to 775.8M (-5.9%) |
| | `verify_typed` | 490.9 ms | 501.8 ms (+2.2%) | 3,811.5M to 3,847.8M (+1.0%) |

A clone or an edit of a map whose values hold no collection runs none of the new code but a few
branches in the node check and the walk, and moved by 0.2% or less. The memo's lookup and the
summary of an entry of a recheck sit in a function of their own that is never inlined: inside the
node check, which the walk runs for every node, they took the summed clones to 0.24% and 0.32% more
instructions and up to 2% more time. `verify_typed` checks a node of a map with a summary without
the store first, as the receive's walk does, which tells a value that links from one that does not
as it decodes, up to 0.8% more, and checks each node of the map of maps twice. That map's edit
gains from summarizing each entry as it decodes, and its clone of ten commits from the memo too,
which found 274 of its 20,274 entries.

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

### Zero-width items in a bundle

An object may hold 2^24 items that take no bytes, such as the `()`s of a `Vec<()>`, and visiting
them takes about 3.3 ms, while a value that claims them is a varint of four bytes. Each check paid
only for its object's bytes, so a bundle bought 3.3 ms of checking for every object it sent that
claimed that many, whatever their size: 40,000 such values spilled under the keys of one node, a
bundle of 1.27 MB, took 131 s to check and landed (#110). A bundle could buy the same again with a
link: a node checked under each of many narrowing ranges, a spilled value checked for each key whose
summary it confirms, a nested map that `of` reads for each entry that holds it, and held data of
another type checked again under each range, each visit their object's items once more.

So a receive counts the zero-width items of every check whose bytes it charges: each check of a
sent object, the first one and each one after it, the decode under an earlier kind, the check that
confirms a spilled value's summary under another key, the recheck of a deferred object, and every
object read through the view at a charge, a sent object or a held record read again, whether `of`
reads it or the check of held data of another type does. The count is the decoder's own, so a check
that goes back to decode what it visited counts those items once, though it visits them twice. A
bundle whose checks would count more than 2^28 zero-width items, plus 16 for each body byte it
carries, is refused with `Error::Bundle`, "checking it would visit more zero-width items than its
size allows". The first read of each held record is not counted when it decodes the record as the
type the receiver holds it under, as its bytes are not charged and a local scan would visit those
items too, and neither are the held parents the hint path reads once each. The check of held data
linked as another type counts every check it makes as the new type, the one on the free first read
included, as the new type decides how many items there are: a `u64` of 200,000 read as a `Vec<()>`
is 200,000 of them.

That first check used to be counted only when its read was charged. A peer whose schema has
`units: Map<u64, u64>` where the receiver's has `units: Map<u64, Vec<()>>` set its `units` to a
copy of a `counts` map of 200,000 keys, each 200,000, and sent it in 173 bytes. The receiver held
`counts`, so every node of the copy was a free first read, and the bundle landed after 7.9 s and
4×10^10 zero-width items, against a count of 2.7×10^8 its size allows; with one value of 2^24 + 1 it
was refused 7.9 s in, for that object, on every resend. Both are now refused 0.06 s in. A copy into
a type with no zero-width items counts none, and one of 16,384 keys of 4 such items each lands.

The bundle of 1.27 MB above visited 2^24 items in 3.28 ms an object, about 0.2 ns an item, and the
clone of a `Map<u64, String>` of 400,000 keys of 80 bytes, 35.8 MB, was received in 0.153 s,
about 4.3 ns a byte. So 16 items for each byte carried cost at most about three quarters of what
receiving those bytes does, and the constant, 16 objects at the limit of one, about 53 ms.
That bundle is now refused 86 ms in, and one of 12.8 MB that sends 400,000 such values 245 ms in.
Tests forge a node checked under 400 narrowing ranges, a spilled value under 400 keys of a map with
a summary, a nested map that `of` sums under 400 keys, and a held node linked as another type under
400 ranges, each object with 2^24 `()`s. All four landed before and are refused now, and with
eight ranges, eight keys, four entries and eight ranges they land. An honest bundle carries up to 16
objects of 2^24 zero-width items, and one more for each MiB it carries: a pull of eight spilled
values of 2^24 `()`s each, sent in two commits, lands, and so does the pull of
`ferro/tests/zero_width.rs`, whose versions of one node and its spilled value hold several times
2^24 between them. The clone above received in a median of 0.154 s against 0.153 s, over nine
alternating runs, since a check that meets no zero-width item adds only a branch.

Charging the items against the allowance that recomputing summaries draws on, at a weight per item,
was weighed. At about their time, 16 items a byte, that allowance's 4 KiB for each link and its
256 MiB would let a bundle of 1 MB buy over a second of visits, and one of 10 MB about five, and
the first check of each sent object, which that allowance does not charge, would have to draw on
it as well. A count of its own keeps the bound in the items themselves and refuses with a message
that names them.

### Indexes that agree with their rows

A table stores its rows and each of its indexes as maps of their own, and the walk checks each map
only as a map. A peer whose schema had a type that encodes as the receiver's `Table<Row>` does, with
its index maps set apart from its rows, sent a table whose `by_n(&6)` missed the row that held 6,
whose `by_n(&9)` returned a row holding 6, whose `by_n(&8).len()` was 1 for a lookup that yielded
nothing, and in which `insert` of a row with a value no row held panicked with a clash, since a
write trusts what the index says (#112). It landed as a fast-forward, and `verify_typed` passed.

So each table value the walk finds is checked against its rows. The derived `decode` and `visit`
of a table with indexes note the bytes and links they read when the decoder belongs to a typed
check that collects them, which the check of each sent object and of held data linked as another
type does. Each value is kept once, by its type and the hash of its encoding, so the many versions
of a node that hold the same table, and the rechecks of a node under other ranges, note it once.
Once every root's tree has been walked and the deferred summaries settled, each noted table is
read through the view with the staging lent to it, as the deferred pass reads, and each index it
stores under a fingerprint the receiver knows must hold exactly the entries its rows give: the
same keys, compared by `Ord`, with values that encode the same, so a covered field must be the
row's. Every row must sit under its own key, and no two rows may share a value of such a unique
index. A table that fails refuses the bundle with `Error::Bundle`. An index the table lacks, or
holds under another fingerprint, is rebuilt from the rows when the table is read, as before, and
[Limits](sync.md#limits) describes what that leaves.

A table in a version's root is checked against the table of its type at the same place in the
root of the version's parent: the nth table of that type that a check of each root reads. A parent
the receiver holds is trusted, its tables with it; a parent the bundle carries is checked in the
same receive. The rows that changed between the two give the entries to take out of the parent's
indexes and those to put in. They are found as `Map::diff` finds them, skipping every subtree the
two share, but from the nodes' bytes: each value of a node the two do not share is visited, not
decoded, and compared by its bytes and the hashes of its links, so only the rows that changed are
decoded. An entry both sides hold with the same encoding is left alone, so a change to a field no
index names touches no index. What is left goes into each index in one batch, with `Map::apply`,
and the result must have the root hash of the index the table stores. Where it does not, the two
are diffed, so an index whose keys compare equal to the expected ones but are stored as other
bytes, as a type whose `==` ignores case leaves after an update, still agrees. An index left with
fewer entries than it should hold took an entry onto a key another entry held, which in a unique
index is two rows sharing a value. So the check costs what changed: the nodes of rows that differ
on both sides, the changed rows, the index nodes along their entries, and the hash of the nodes it
rewrote.

Any other table is checked by building its indexes from its rows, 65,536 rows at a time, and
comparing them the same way: the tables of a clone's first version, a table nested in a
collection, a table whose parent holds no table at that place or lacks one of its indexes, and
held data of another type. A table is checked against another only if the other's check is
planned already, so no chain of checks comes back to where it started, and every table rests on
one that was trusted or built. A table with several indexes, and a thousand or more rows to
work through, works on each index with a thread of its own.

The reads go through the view, so they are charged as the deferred pass's are. The first read of
each record of the receiver's file is free, and a read of a sent object or a second read of a
record is charged against the allowance. A clone reads what it carries a second time, and an
incremental pull reads the old versions of the rows it changes, once each. A bundle of a few
hundred bytes that hangs a table's index on a large tree of rows the receiver holds makes it read
those rows once for free and build the index from them, and every other such table it carries
pays for its reads.

`verify_typed` runs the same check on the file's store, over every table its walk finds: each
retained version's root tables against its parent's, versions taken oldest first, and every other
table by building its indexes.

Measured on 2026-11-08 with the receiver's file buffered, on a table of 200,000 rows of five
fields with a plain index on a `u32`, a unique index on a `u64` and a plain index on a `String`,
cloned into an empty file and then sent an edit of 1,000 random rows. Times are the middle of
three medians of five rounds each, the build before and after alternating:

| Bundle | Before | After |
| --- | --- | --- |
| Clone in ten commits of 20,000 rows, 30.6 MB | 379 ms | 749 ms |
| Edit of both plain indexed fields, 2.9 MB | 43.2 ms | 64.5 ms |
| Edit of a field no index names | 14.5 ms | 25.8 ms |
| `verify_typed` of the eleven versions | 606 ms | 1,010 ms |
| Clone in 100 commits of 2,000 rows | 1,906 ms | 2,958 ms |
| Edit of both plain indexed fields after it | 49.7 ms | 70.5 ms |
| `verify_typed` of the 101 versions | 2,249 ms | 3,278 ms |

Most of the cost is reading nodes a second time. The walk visits each sent node without building
its values, and the check reads the nodes of the rows that differ on both sides and the index
nodes along the changed entries through a view that caches nothing, each read a copy out of the
file or the staging, an LZ4 decompress and a decode. In the edit the diff of rows reads some 1,700
leaves in about 13 ms, where decoding their values as well took 4 ms more. A clone's index entries
fall all over each index in every commit, so each commit's check reads most of the index the commit
before left and hashes most of what it rewrote, which is what the commit carries. Putting the
entries of each changed row in with the derive's `reindex`, one map insert at a time, took the
clone to about 960 ms and the edit to about 85 ms, and dropping the indexes built took a quarter of
the clone's check before each index was dropped on its own thread.

Left out:

- Checking each table as the walk reaches it. Its rows and indexes have only all passed their own
  checks once the walk is done, and reading them needs the view.
- Comparing the diff of each index with the expected changes, which reads the changed nodes of the
  index on both sides, where applying the changes to the parent's index reads them on one side and
  hashes what it rewrote.
- Finding the parent of a table nested in a collection by its key, which would mean following the
  path to the collection through the parent version's tree. Such a table is built in each version
  that changes the node that holds it.
- Keeping the indexes built for one version to check the next. A map's nodes in memory do not keep
  their hashes, so each check would hash every node built so far, and a pull of many small commits
  would hash the whole index for each.
- Checking the tables of several versions on threads of their own. A clone's checks do not depend
  on each other, but each holds the indexes it builds, which in a clone of large commits is most of
  each index, so the memory a receive holds would grow with the threads.
- Checking that each row sits under its own key in a table without indexes, which no lookup reads.
