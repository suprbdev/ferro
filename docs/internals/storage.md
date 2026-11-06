# Storage: objects, records and the file

How a value becomes content-addressed objects, how those objects are laid out as records, how a
commit writes them in one frame, and how a file recovers after a crash. The user-facing guarantees
are in [Durability and crash safety](../guide/durability.md).

## Objects and links

Everything on disk is an `Object { body, links }`. A link is a `Ptr { hash, offset }`.

- The hash is BLAKE3 over body length, link count, body, and the link hashes, truncated to 20 bytes
  (`HASH_BYTES`). Offsets are not hashed, so identical content has an identical hash wherever it
  sits. The file is a Merkle DAG.
- The offset lets a reader jump straight to a child, so no hash index is needed. It means something
  only inside one generation of the file, as [Generations](reclamation.md#generations) explains.
- Every read re-hashes the object and compares it with the pointer it came from.
- Links are stored in a side table and consumed in order by `Decoder::link`. They take no bytes in
  the body, which keeps encoded size independent of a value's position.

`hash_of(&value)` gives the hash a value has as an object of its own, whether or not it is stored as
one. `Map::value_hash(&key)` returns the same hash, from the stored pointer without reading a
spilled value, or by encoding an inline value that is already in memory.

Hashes are 160 bits, not 256. A link costs 20 hash bytes plus a varint offset, about 23 bytes
instead of 40, which cut the bytes written per single-key commit by about 19%. Second-preimage
resistance, which is what tamper evidence on read needs, is 2^160. Collision resistance is 2^80
instead of 2^128. Anyone who can pick the bytes of two objects and can afford about 2^80 hash
evaluations can make two different objects share a hash, and ferro would then treat them as the same
content in `==`, `diff` and write deduplication. Nothing in ferro authenticates against such an
attacker; the superblock checksum is unkeyed. Raising `HASH_BYTES` to 32 restores the full claim at
the cost of those bytes.

## Records

A record is `varint stored_len`, `varint (link_count << 1 | compressed)`, an optional
`varint raw_len`, the body, then the links. Each link is its hash followed by a zigzag varint of
`record offset - target offset`. Children are written before parents, so the deltas are small and
positive in the common case, and nothing in a record depends on where it sits except that delta.
Bodies of 96 bytes or more are LZ4 block compressed when that makes them smaller. Compression runs
after hashing and decompression before the hash check, so the hash covers the canonical body and two
files with different physical compression still agree on every root hash. The decoder rejects a
claimed expansion beyond LZ4's 255:1 limit before allocating. `lz4_flex` is built with `safe-decode`
and without `safe-encode`: the compressor is its unsafe implementation, unsafe code from a
dependency on the write path that only ever reads bytes ferro produced, and the decompressor is its
safe module, which forbids unsafe code, because it reads bodies from files and sync bundles that may
be damaged or hostile.

## Deduplication

A transaction skips an object whose hash it finds in `dedupe`, a bounded table from hash to offset,
through `Txn::known`. It holds every object this process wrote, and what it read for objects of at
least `SPILL_BYTES`, the size at which a value becomes its own object. Smaller objects are cheap to
write again, and registering every read cost 3 to 11% on cold gets through a writer. Only objects
read are registered, after their hash check. Offsets are not hashed, so a link in a node proves
nothing about the offset it carries until it is followed, and registering links would let a damaged
offset spread into new commits. A read-only handle registers nothing. A commit drafted outside
`tip`, as described under Drafting outside `tip`, takes only offsets below the start of its draft,
and looks its records up again, all but its log entry, if its frame has to move.

So rewriting a value costs nothing if this process read the old one, whichever process wrote it.
`get_mut` on a spilled value needs no special case: the slot becomes `Fresh`, the value is encoded
again, and the hash finds the old object. Keeping the pointer in the slot would save only that one
lookup, because the encode and hash are needed to know the value did not change, and it would cost
every slot eight bytes.

It does not close the gap for code that replaces a collection without reading it. In `chunkexp`,
1000 edits to a 100 KB page as a `List<String>`:

| Update | Warm process | New process per edit, before | New process per edit, after |
| --- | --- | --- | --- |
| Rebuilt from a fresh `collect` | 2.83 MB | 87.1 MB | 87.1 MB |
| Read, then `set` the chunks that differ | 2.75 MB | 4.53 MB | 2.76 MB |

A rebuild reads nothing, so nothing is registered. Finding an unread object by hash would need an
index on disk or a scan at open, and neither is justified by one write pattern that can read first.

The table is two-way set associative. A set is one 64-byte line holding two full hashes and their
offsets behind a one-byte lock, so a lookup or an insert touches one line. A hit moves to the front
of its set when it is looked up, under the same lock, and when it is inserted again, and a new entry
pushes out the older way. It has one entry for every 4 KiB of `cache_bytes`, between 1,024 and
65,536, and is allocated in 4 KiB chunks as entries first land in them, so an open costs nothing and
a writer that has made a few thousand commits holds about 2 MiB. It was a `quick_cache`, and a
booking commit registers and looks up about 36 objects. Once that cache held its 65,536 entries
every insert evicted through its clock lists, and every insert and lookup took several cache misses
in its shard map and slab: 14% of a booking commit's time went there. With the table, thread CPU
time outside the closure fell 7 to 12% for a booking hold and 4 to 9% for a confirm, and the hit
rate held: bytes per write went from 12,210 to 12,208.

Lookups did not always promote: a hit was moved only if the commit registered the object again,
which `put_packed`, since removed, did and `put` did not. Promoting every hit makes no difference at
the default size. Over 4,000 measured `tables` hold and confirm pairs with the default cache, hits
went from 15,373 to 15,391 of 340,455 lookups and bytes per write stayed at 12,208. With
`cache_bytes` at 4 MiB, which gives the minimum 1,024 entries, hits rose 6%, from 13,878 to 14,756,
and bytes per write fell from 12,225 to 12,214. Instructions per commit moved by less than 0.15%
either way and thread CPU time by less than the noise between runs. The map and table commits in
`ferro-bench` find nothing in the table, so promotion has nothing to do there.

## File format and durability

Two 4 KiB superblock slots, then the log. A superblock names the head and the log end, records the
size the file had when this generation began and how much of the previous one survived, and carries
an anchor, the newest position known to be durable. The superblock's magic, `FERRODB4`, is the
format's version. The log is a run of frames, one per commit. A frame is a 68-byte header (magic,
sequence, end offset, head pointer, xxh3-128 check), the records the commit wrote, and a table that
holds the xxh3-64 of each 4 KiB block of those records, the last block shorter, 8 bytes a block. The
check covers the header and the table, and through the table every record, so a frame proves by
itself that it reached the disk whole, and the table says which block of it did not, as described
under [Torn and damaged frames](#torn-and-damaged-frames). The head pointer names the branch table,
or a step that puts one branch's new head on the table or step the frame before named, as described
under [Branch-table steps](commit-path.md#branch-table-steps). A table's body is the varint count of
its branches and its one link the root of the `Map`, as it always was. A step's body is the byte
`0x7f`, the branch's name as a varint length and its bytes, and the `Head` as the table stores it:
the version, the first version kept, the root and the log's tip each as a flag byte and, when set, a
link, and the map of merge records. Its first link is the table or step before it, followed by the
head's links. A commit does this:

1. Write the frame at the log tail.
2. fdatasync. This is the durability point.
3. Write the superblock to the other slot, without syncing it.

Frames are placed at the tail in order, but each commit writes its own after releasing the lock that
placed it, so they can reach the file out of order. A sync waits until every frame up to the one it
covers is written before step 2, and the scan below keeps only frames that follow each other, so a
frame written ahead of one that was lost is lost with it. [Writing frames after
`tip`](commit-path.md#writing-frames-after-tip) has the rest.

In `Durability::Fsync` the superblock is written only after its frame is durable, so it anchors
itself and never names anything a crash can lose, and the next fdatasync carries it to disk. A crash
leaves it at most one sync behind. On Windows every fdatasync here is a `FlushFileBuffers`, as
described under [Windows](windows.md#durability).

Open picks the valid slot with the highest sequence and scans forward from its anchor. A frame
counts only if its magic, sequence (the previous one plus 1), end offset, table and check are all
right. The first that is not ends the log, unless it reached the disk whole and was damaged since,
which fails the open with `Error::Corrupt`, as described below. A torn tail, zeroes and garbage all
stop the scan, and an acknowledged commit survives a stale or torn superblock. A writer then syncs
the frames it accepted, truncates the rest, and rewrites the superblock, in that order. The
truncation is durable before any new frame is written, so a frame stranded past a torn one can
never line up with the log later.

### Torn and damaged frames

Under `Durability::Fsync` the frames of the last sync are durable and acknowledged, but the
superblock that names them was written after the sync and may not be: a crash can leave the
superblock one sync behind, and open finds those frames by scanning. Until the format above, open
could not tell a frame there that a torn write left part-written from one that reached the disk and
was damaged since, so it truncated both, and bit rot in that window dropped an acknowledged commit
without a word (#80).

A mark at the end of the frame cannot tell them apart. A frame reaches the disk as sectors, and a
power cut can keep any of them: the sector with the header and the one with a trailer can land
while one between them does not, and the power-cut tests below keep each sector by chance. What
tells them apart is what a sector that a write did not reach holds: its older bytes, all of them,
which are the zeroes of a reservation or of a new extension, or what a dead writer or a failed
write left. Those differ from the frame's bytes in many bits, where damage to a frame that reached
the disk changes few. So a frame that fails its checks is judged damaged if putting back one flipped
bit in its header and table together, and at most one in each 4 KiB block of its records, makes
every check pass, and torn otherwise. The table keeps the search cheap: only a block whose entry
fails is searched, by trying each of its 32,768 bits against the entry. A flip in the check shows
as a check one bit from the one computed, a flip in the table as an entry one bit from its block's
hash, and a flip in the magic or the mark is found by trying each of their 416 bits against the
check.

A sector that holds only a few of the frame's bytes, at either end of the frame, could keep older
bytes that differ from the frame's in one bit by chance: one byte with one bit set over a zero is a
chance in 32. So a flipped bit counts as damage only in a sector that holds at least 8 of the
frame's bytes, where that chance is about 2^-58; a sector is 512 bytes, the smallest a device
writes whole, and anything coarser tears along the same lines. A flipped bit among the first or
last 7 bytes of a frame, when they are all the frame has in their sector, reads as a torn write.

A damaged frame fails the open with `Error::Corrupt`, naming the frame's sequence, its offset and
the first byte found wrong, and the open writes nothing: no sync, no truncation, no superblock. A
read-only open that finds no writer alive scans and reports the same. Truncating the file at the
offset named gives up that commit and every one after it, and the file then opens. A shared writer
that catches up under the placement lock, where no live process can be writing, reports the same
and never writes over the frame, as described under [Catching up](processes.md#catching-up). A
scan that a write can race takes any frame that does not check for the end of the log, since it can
read a frame another process or thread is still writing in part: a shared writer catching up
without the placement lock, the leader of a shared sync reading past its own frame, and the quick
look a read-only open takes before it knows whether a writer is alive.

The judgement reads the bytes alone, whatever durability wrote the frame or opens the file. Under
`Durability::Buffered` a power cut may still take back every commit past the anchor: a torn frame
there is rolled back as before, and a power cut leaves no frame one bit from whole. A frame that
is, past the anchor, is reported the same way, though Buffered never promised its commit would
survive, since what damaged it was not a power cut, and truncating it would hide that.

Out of order frames and dead writers keep working. A frame that never reached the disk, with whole
frames after it, leaves its older bytes and reads as torn, so the scan stops there and the frames
after it are truncated, as before. A process killed while it writes stops at a page boundary, since
the kernel copies a write into the page cache a page at a time, and the rest of its frame keeps its
older bytes, so the next writer finds it torn and writes over it.

What it does not catch: two flipped bits in one block, one in the header and one in the table, or
one in a block's entry in the table and one in that block, which leaves no entry one bit from its
block's hash, read as a torn write and are truncated as before, and so does a sector that a lost or
misdirected write left holding other bytes whole. Nothing on disk records that a sync finished until
the next sync carries its superblock, so a frame of the last sync with a sector gone cannot be told
from one caught by the power cut, and only the bytes can tell damage apart. Damage before the anchor
is found by the hash checks on read and by `verify`, as before.

The tests, in `ferro/tests`: `crash.rs` flips each bit position of a frame past a stale superblock
under both durabilities, and of every header and table byte and a sample of the body of a frame of
more than three blocks, one bit in each of two blocks, and two in one block or one in a block and
one in its entry, which read as torn; it replaces each sector inside a frame with zeroes, ones or an
older frame's bytes, and leaves a hole of zeroes, whole or every other sector, before whole frames.
`buffered_crash.rs` flips a bit in a whole frame past the anchor of every crash image of a Buffered
recording, under every loss, and of an Fsync recording whose superblocks since the last sync were
lost, the frames of acknowledged commits among them. `group_commit.rs` flips one in the last frame
past the superblock of each image its intercepted fdatasync takes, frames that sync then made
durable, of acknowledged commits among them. `shared.rs` leaves a dead writer's frame cut at each of
its first page boundaries, over zeroes and at the end of the file, and the next writer writes over
it, and leaves it whole with a bit flipped, which the next writer reports without writing. Every
existing power-cut, crash and shared-process test opens every image it did before.

The table costs 8 bytes for each 4 KiB block of a frame's records, and 8 for a frame of one block.
The hash of each block replaces the one stream over the frame that the check was, so a commit runs
no more instructions. `tables`, one Buffered writer, median of 11 alternating runs against the tree
before: bytes per write went from 13,362 to 13,393, 0.23% more. Outside the closure a hold ran
566,607 instructions before and 565,577 after, a confirm 732,025 and 731,867, and a cancel 900,289
and 899,017, with thread CPU time of 73.7 and 73.5, 94.5 and 94.4, and 121.8 and 121.6 µs.
`ferro-bench`, median of seven alternating runs: a single-key commit at 1M keys wrote 3,525.8 bytes
before and 3,535.6 after, 0.28% more, and ran 180,192 and 180,052 instructions Buffered and
163,310 and 163,204 under Fsync, and a table update wrote 4,754.6 and 4,768.2 bytes, 0.29% more. A
Buffered single-key commit took 24.2 and 24.4 µs at the median, inside the spread of either build's
runs, 23.9 to 24.4 and 24.2 to 25.6 µs. `group`, single-key Buffered commits into 100,000 keys,
median of five alternating runs: 49,720 and 49,672 a second from one writer, p50 18 µs both, at
2,301 and 2,309 bytes a commit, and 41,929 and 42,009 from eight.

The search costs only an open that finds a frame that does not check. Trying every bit of a 4 KiB
block, 32,768 hashes, takes 3.4 ms here, so an open after a power cut that tore a frame takes
about that much longer, once, for the first block of the torn frame that fails. A flipped bit ends
its block's search when it is found. Opening a copy of a Buffered file of 40 commits of 3 keys,
frames of a median 2,621 bytes, scanned from its anchor, took 1.2 ms with every frame whole,
syncing and writing the superblock that recovery writes; 1.5 ms with a bit of the last frame
flipped, writing nothing; and 5.0 ms with a sector of it torn. With frames of 1.26 MB it took 2.1,
3.1 and 5.3 ms.

Measured and left out, on the frames of that file and of one of 40 commits of 40 keys, a median of
18,515 bytes a frame, each frame torn 100 times over zeroes and 100 times over
another frame's bytes, with each sector kept or not by chance, as the power-cut tests tear them:

- A trailer that repeats the frame's length and check, so that a frame with both ends is whole and
  one that then fails its check is damaged. It costs 24 bytes a frame, but a torn write that kept
  both end sectors and lost one between reads as damage: 21.3% of the 7,538 torn frames of the
  first file and 22.8% of the 7,800 of the second, each an open that fails after a power cut. A
  flip in either end reads as torn, so it reports 96.7% and 99.5% of single flips.
- A check of the whole frame in its header and a commit marker written last, in a second write. It
  fails the same way, since the marker can land before the sectors written ahead of it, and adds a
  write to every commit.
- The objects' own hashes and the frame's length, judging a frame whole when its header names an
  end within the file. It costs nothing, but a torn write that kept the header reads as damage,
  45.6% and 46.3% of the torn frames, and a reservation of zeroes past the log is always within
  the file.
- Syncing the superblock before acknowledging, so that nothing acknowledged is ever past it. It is
  exact and changes no format, but it is a second fdatasync on every commit. A frame and one
  fdatasync took 387 µs at the median on the test NVMe drive, and with the superblock synced after
  it 778 µs, twice a lone writer's commit under Fsync.
- Searching every bit of a frame against the check it had, with no table. It changes no format and
  costs nothing on a commit, but the search grows with the square of the frame: 1 ms for 2,300
  bytes, 40 ms for 13,300, 0.9 s for 64 KiB and four minutes for 1 MiB.
- Using the frames after a damaged one as evidence that it was synced, which a frame could carry as
  the anchor its process knew. It covers nothing in the case that matters most, a crash right after
  the last sync, when no later frame reached the disk.

The table judged none of the torn frames damaged, and reports every single flip but those in a
sector holding fewer than 8 of the frame's bytes, 5 of 107,500 positions and 4 of 755,871.

### Buffered durability and checkpoints

`Durability::Buffered` skips step 2, so a power cut can keep a superblock and lose the frames it
names, and open used to fail on that file. It may lose recent commits by contract, but it must
always open. So a Buffered superblock keeps the anchor of the last checkpoint: a position an
fdatasync has covered. The leader of the batch that takes the log past `Options::checkpoint_bytes`
of unsynced frames, 64 MiB by default, syncs once it has published and woken that batch, so only its
own commit waits for the disk, and a clean close syncs as well. A scan from the anchor ends at the
first frame the power cut damaged, whatever the superblock claims. Only frames past the anchor can
roll back, so a superblock that anchors itself still reports a hole in its log as corruption, not as
a short log.

A rollback rewrites both slots, syncing between them, so a reader that opens beside the new writer
trusts a superblock that names only surviving frames. A read-only open cannot rewrite anything. It
scans from the anchor when no writer holds the lock, and remembers the superblock it refused so that
`refresh` ignores it until a writer replaces it. Keeping the anchor in the superblock rather than
dedicating one slot to it means neither slot has to be synced on its own: every superblock that
survives names an anchor that was durable before that superblock was written.

Recovery reads what lies between the anchor and the tail. With the frames still in the page cache,
that took 13 ms for 64 MiB and 54 ms for 256 MiB, where a clean open takes 0.02 ms. The checkpoint
is the cost: the commit that crosses the line waits 27 ms for 64 MiB to reach the disk here, which
averages out to 1.4 µs on every Buffered commit of `ferro-bench`, 25.8 to 27.3 µs, while p99 does
not move. The anchor adds 44 bytes to a superblock, one more BLAKE3 block per commit. Syncing on
every Buffered commit would have closed the bug too, and made it Fsync. Writing the superblock only
at checkpoints would have left followers a checkpoint behind, because `refresh` must not scan past a
superblock while a writer is alive. The fix was tested only against modelled power cuts: every
prefix of the recorded writes with all, none, a random half or torn sectors of the unsynced ones
kept, and the superblocks kept without their frames.

### Read-only opens

A frame past the superblock may not be durable while a writer is alive, so `refresh` reads only the
superblock and never scans. A read-only open scans only when a frame follows the superblock or the
superblock does not anchor itself, and it can take a shared lock, which proves no writer is alive.
It holds that lock for the scan and syncs what it accepted, since a dead writer's frames may exist
only in the page cache. If a writer holds the lock, the superblock is current and the reader uses
it. A writer opening during that short scan retries for a few milliseconds before reporting
`Error::Locked`.

### Preallocating the log

fdatasync after extending a file has to commit the ext4 journal, which took 620 µs against 360 µs
for overwriting allocated blocks on the test NVMe. So in `Durability::Fsync` the log grows in
zero-filled chunks, doubling from 64 KiB to 4 MiB, and frames overwrite blocks that are already
written, which needs only the device flush. The zeroes must be real: `posix_fallocate` leaves
unwritten extents, and converting those commits the journal just the same (640 µs). The first commit
of a process reserves nothing, so a short run pays for no zeroes. The reservation is only an
optimization. A failed reservation stops further attempts without failing the commit, open discards
it, and a clean close truncates it, so the file can be up to 4 MiB longer than `size_on_disk` while
open.

### Failed writes and `refresh`

A failed write or fsync poisons the store until reopen. Only one process may hold the writer lock,
unless every writer opens with `Options::shared`, as described under [Writers in several
processes](processes.md). Read-only opens otherwise take no lock, since log bytes under a published
head never change, and `refresh` picks up new commits. It reads the new refs and the state of every
attached branch before it changes anything, holding the published refs so that no branch attaches
in between, and only then records the head, swaps in a replaced file and publishes. A refresh that fails partway, say on a
read error, therefore leaves the handle as it was, and the next one starts over and catches every
branch up. It used to record the head first, so the next call saw nothing new and the branches the
failed one missed stayed behind until another commit landed.
