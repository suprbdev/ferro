# Bytes of a small commit

Where the bytes of a single-key commit go, and the encodings that were measured to make them fewer.
None of them was kept. The CPU time of small commits is in [Commits of a few rows](commit-cost.md),
and a design that writes fewer nodes in the first place in [A delta log between
checkpoints](delta-log.md).

## Where the bytes go

`ferro-bench`'s `bytes` tool loads a map or a table, makes 1,000 single-key commits under
`Durability::Buffered`, reads each commit's frame back, splits every record into its header, body
and links, and decodes the body of every map node into its parts. The maps hold scrambled `u64`
keys, the workload of [Performance](../guide/performance.md); the table is `ferro-bench`'s
accounts, indexed on `tier` and `country`, where a balance update changes only the rows and an
insert changes both indexes too. Bytes per commit, measured on 2026-11-11:

| Part | `Map<u64, u64>`, 1M keys | `Map<u64, String>`, 1M keys | Table of 200,000 rows, update | Same, insert |
| --- | ---: | ---: | ---: | ---: |
| Frame header and check table | 77 | 78 | 82 | 88 |
| Log entry | 68 | 68 | 69 | 70 |
| Branch-table step | 80 | 80 | 80 | 80 |
| Root object | 27 | 27 | 115 | 111 |
| Node record headers, levels and key counts | 34 | 45 | 40 | 101 |
| Keys | 751 | 751 | 199 | 913 |
| Value tags | 79 | 79 | 68 | 191 |
| Values | 234 | 1,019 | 6,288 | 5,379 |
| Saved by LZ4 on node bodies | -5 | -353 | -3,661 | -3,162 |
| Child counts | 136 | 136 | 109 | 312 |
| Child link hashes | 1,364 | 1,364 | 1,166 | 3,439 |
| Node link distances | 240 | 241 | 203 | 633 |
| Total | 3,085 | 3,544 | 4,768 | 8,174 |

Link hashes, in nodes and in the entry, the step and the root object, are 48% of a `Map<u64, u64>`
commit, 42% of a `Map<u64, String>` one, 28% of a table update and 44% of a table insert. A
scrambled `u64` key is a varint of 9.5 bytes on average, so keys are another 21 to 24% of a map
commit. At 100,000 keys a map commit writes 2,821 and 3,197 bytes in the same proportions, and a
table of 20,000 rows 3,563 bytes an update and 6,013 an insert.

A map commit rewrites 7.4 node records at 1M keys, 6.3 of them interior nodes, holding 79 keys and
68 links. The 1M-key map holds 124,831 nodes of 8.0 keys on average, and its interior nodes have 8.1
children, but the nodes on a commit's path have 10.7 keys and 10.8 children, and 3.2 of the 6.3
interior ones more than 8: a key lands in a node with a chance in proportion to the keys under it,
as [Node layout](read-path.md#node-layout) found for gets. A new path has to carry the hash of every
child of every node on it, since a node is checked against its hash on every read and its hash
covers its children's, so the large nodes a commit lands in write most of its bytes.

## Encodings measured

Each was sized with the `bytes` tool on the frames above, by encoding their records again under it,
LZ4 included where a body was compressed. The first was also built and timed. Bytes saved per
commit at 1M keys and 200,000 rows:

| Encoding | `Map<u64, u64>` | `Map<u64, String>` | Table update | Table insert |
| --- | ---: | ---: | ---: | ---: |
| Compact nodes, built: keys after the key before, untagged values, no counts in leaves | 7.3% | 4.4% | 2.5% | 3.7% |
| Of which keys after the key before, sized | 4.3% | 3.9% | 3.3% | 3.3% |
| Of which untagged values, sized | 2.6% | 0.3% | 0.1% | 0.8% |
| Of which no counts in leaves, sized | 0.4% | 0.3% | 0.1% | 0.1% |
| `u64` keys as bit-packed gaps, sized | 6.5% | 5.8% | 2.6% | 2.9% |
| No hash on a link into the same frame, sized | 6.4% | 5.6% | 3.5% | 5.2% |
| LZ4 over the whole frame instead of each body, sized | -0.3% | 3.7% | 10.2% | 5.6% |
| Distances from the link before, sized | 1.4% | 1.2% | 0.5% | 1.4% |
| Interior nodes stored as chunks of 8 children, bound | 30% | 30% | 28% | 43% |

### Compact nodes

A node's body is its level, its key count, its keys, a tag byte before each value that says whether
it is inline or spilled, and for each of its gaps a varint count and, when the gap holds a child,
the child's summary and link. The prototype changed three things:

- Each key after the first was written after the key before it, through a hidden pair of methods
  on `Data` that default to `encode` and `decode`: an integer as its distance from the one before,
  a `String` as the bytes it shares with the one before and the rest, and a tuple its first item
  that way and each later one too while the items before it are equal.
- A bit of the level byte said whether any value spilled, and only then did values carry tags.
- A leaf, at level 0, holds no children, so it wrote no counts for its gaps.

Median of five runs of `ferro-bench` alternating with the build before, on a machine other agents
were also using:

| | Before | Compact nodes |
| --- | ---: | ---: |
| Bytes per single-key commit, 1M keys | 3,535.6 | 3,380.8 (-4.4%) |
| Bulk load, bytes per key | 23.8 | 20.6 (-13.4%) |
| Table, bytes per row and per update | 69.6 and 4,768.2 | 61.2 and 4,651.1 (-12.1% and -2.5%) |
| Buffered commit, instructions | 178,623 | 172,692 (-3.3%) |
| Buffered commit, five runs | 23.2 to 101.1 µs, median 27.8 | 22.5 to 24.4 µs, median 23.5 |
| Fsync commit, instructions and median | 164,786, 429 µs | 159,984, 420 µs |
| Cold get with a 1 MiB cache, instructions and time | 52,461, 5.53 µs | 49,105, 5.32 µs |
| Warm get, scan and range | 331 ns, 20.2 and 32.3 ns a key | 331 ns, 20.0 and 32.3 ns a key |
| Bulk load, instructions a key | 3,973 | 3,957 |

The `tables` tool, three alternating runs, wrote 12,417 bytes a booking write against 12,848
(-3.4%), with 2.1 to 3.1% fewer instructions outside the closure and CPU time within the spread of
the runs. The single-key commits of the `bytes` tool came to 2,859, 3,390, 4,651 and 7,871 bytes,
the savings in the table above, and at 100,000 keys and 20,000 rows to 6.8, 4.9, 1.8 and 4.0% fewer.

So the encoding costs nothing and saves 2.5 to 7.3% of a single-key commit, and 12 to 13% of a bulk
load, where seven nodes in eight are leaves. It was left out as too small for what it takes. Every
type whose `describe` matches another's must write the same distances, or a table keeps an index
whose bytes a rebuild would not give: the property test
`every_index_equals_a_rebuild_whichever_version_opens_the_file` found that a `#[ferro(transparent)]`
newtype of an `i64` wrote its keys whole while `i64` wrote distances, so the derive, `Box`, `Arc`
and every wrapper would have to forward the methods. The tests that forge node bytes for `receive`
and `verify_typed`, the pinned root hashes and the fixtures would have to be written again, as nine
test targets showed. It is the first change to make if the size of a file matters more than that.

### `u64` keys as bit-packed gaps

The gaps between sorted scrambled keys are about 2^44 apart in a leaf at 1M keys, which a varint
writes in 7 bytes and a bit-packed column, a width byte and the gaps at that width, in 5.5. It saves
6.5% of a `Map<u64, u64>` commit, 2.2 points more than distances as varints, but only for integer
keys: a tuple or a string has no width to pack to. With untagged values and no counts in leaves it
comes to 9.5, 6.3, 2.7 and 4.3%.

### No hash on a link into the same frame

A commit's new nodes link each other, the root object links the top one, and the entry and the step
link the root object, so 6.4 of a map commit's 68 node links, and most links of the entry, the step
and the root object, lead into the frame that holds them. Leaving their hashes out saves 3.5 to
6.4%, but a read of a record that holds such a link has to hash the record it leads to before it
can check its own hash, and that record's own such links before that. Hashing the records so
linked in one frame took 3.5 µs for a `Map<u64, u64>` commit, 4.3 µs for a `Map<u64, String>` one,
and 8.2 and 12.4 µs for a table update and insert, against 5.5 µs for a whole cold get at 1M keys.
A bulk load's frame links everything in it, so a read of its root would hash the whole tree. Sync,
the reclamation copy, `verify` and `dedupe` all take a record's links' hashes from the record. Not
built.

### LZ4 over the whole frame

The hashes, 28 to 48% of a commit, and the keys do not compress, and LZ4 already finds the
repeats inside a node's body, so compressing a frame's records together saves 10.2% of a table
update, where rows of one leaf repeat each other's fields, 3.7% of a `Map<u64, String>` commit and
nothing for `Map<u64, u64>`. A record would then have no offset of its own in the file, so a link
would have to name its frame and its place in it, a cold read would decompress the frame, 84 ns to
1.2 µs for these, against about 2.1 µs a miss now, and a bulk load's frame would have to be
compressed in blocks. Not built.

### Distances from the link before

Siblings written by one bulk load sit close together, so a link's distance from the link before it
in the same record is often shorter than its distance back from the record. Written that way it
saves 0.5 to 1.4%, and 0.7 to 1.6% with a bit that picks the shorter of the two. A moved draft
rewrites its links' distances in place, as described under [Moving a draft in
place](commit-path.md#moving-a-draft-in-place), which a distance between two older records would
not need, but a distance from a link into the frame would. Not built.

### Interior nodes stored as chunks

A node with more than 8 children could be stored as a head that links its children in chunks of 8,
each chunk holding the keys between its children, so that a commit rewrites the head and one chunk
instead of the whole node. Bounded from each node's mean entry and link sizes, with each chunk's
link and count in the head, it saves 28 to 43% of a commit, the only change measured that passes
20%, since the large nodes write most of the bytes. It is the tree's shape in another form, and
costs reads the way a smaller fanout does: a get would read a head and a chunk where it reads one
node now, for each of the 3.2 large nodes on a 7.4-node path. Fanout 4 writes about 23% fewer bytes
than fanout 8 and was left out for 12% slower warm gets and 17% larger bulk loads, as described
under [Shape and fanout](tree.md#shape-and-fanout). Not built.

### The log entry, the step and the frame

The step and the log entry both link the commit's root, so dropping the step's copy, and reading the
root from the entry, would save 22 bytes, 0.7%. The frame header's 68 bytes hold its magic, the
sequence number, the end and the head's offset as 8 bytes each, which varints of the frame's length
and of the head's distance would cut by about 20 bytes, 0.6%, and a frame of one block could let its
check cover its records and drop its 8-byte table, 0.3%. Not built.

## What would make a difference

Every child of every node on the path carries 20 bytes of hash that a read needs to check what it
reads, and those hashes and the keys are 63 to 73% of a map commit. What the encodings above leave
is the number of entries a commit rewrites, which is the tree's shape, and the number of commits,
which batching changes into one commit and [`coalesce`](coalescing.md) already reduce: 16 writers on
one branch with `coalesce` write 1,515 bytes a closure against 2,464 without it, as
[Performance](../guide/performance.md#writers-and-branches) gives.
