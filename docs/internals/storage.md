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
an anchor, the newest position known to be durable. The log is a run of frames, one per commit. A
frame is a 68-byte header (magic, sequence, end offset, head pointer, xxh3-128 check) followed by
the records the commit wrote. The check covers the header and every record, so a frame proves by
itself that it reached the disk whole. The head pointer names the branch table, or a step that puts
one branch's new head on the table or step the frame before named, as described under [Branch-table
steps](commit-path.md#branch-table-steps). A table's body is the varint count of its branches and
its one link the root of the `Map`, as it always was. A step's body is the byte `0x7f`, the branch's
name as a varint length and its bytes, and the `Head` as the table stores it: the version, the first
version kept, the root and the log's tip each as a flag byte and, when set, a link, and the map of
merge records. Its first link is the table or step before it, followed by the head's links. A commit
does this:

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
counts only if its magic, sequence (the previous one plus 1), end offset and check are all right,
and the first that is not ends the log. A torn tail, zeroes and garbage all stop the scan, and an
acknowledged commit survives a stale or torn superblock. A writer then syncs the frames it accepted,
truncates the rest, and rewrites the superblock, in that order. The truncation is durable before any
new frame is written, so a frame stranded past a torn one can never line up with the log later.

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
