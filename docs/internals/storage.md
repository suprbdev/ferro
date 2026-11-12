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

### One encoding for each value

A value's hash is the hash of its encoding, so equal values must reach the file in one form: `==` on
collections, the skip of a commit that changes nothing, `Map::value_hash`, `diff` and merges compare
hashes or encodings. Every encoder writes one form, and decoding refuses, with `Error::Decode`, the
other forms a damaged file or a bundle could hold:

- A varint longer than its value needs, which ends in a zero byte. `Decoder::varint` checks the
  last byte of a varint of two bytes or more, as the record reader does, and a one-byte varint
  costs nothing more.
- Two keys of a `BTreeMap` or `BTreeSet` that its key type's `Ord` holds equal, of which the map
  kept the last and dropped the other.
- Struct fields whose tags do not strictly ascend, so a field stored twice no longer overwrites the
  first, a field the derive knows that its type does not read to the end, and a field that flags
  links and holds none.
- In the checks of a receive and of `verify_typed`, an object with bytes or links left after its
  value, and in a receive, a commit with bytes left after it.
- In the same checks, a map value stored against its size. A node keeps a value whose encoding,
  with the tag byte before it, takes at most `SPILL_BYTES`, 512, and writes a larger one as an
  object of its own, so a value inside its node takes under 512 bytes and one stored apart 512 or
  more. The check reads the sizes as stored, not as the value's type encodes it now: a spilled
  object keeps its bytes until its value is written again, and a value written again is placed by
  its new size, so a file written under an older schema passes too. It costs a compare a slot.
- In the same checks, a `List` whose map holds a key at or past its length. A list's keys are its
  positions, so `n` distinct keys below `n` are exactly `0..n`. `List::decode` gives its root link
  that range, and the node check holds every node to it as it holds a child to the separators
  around it, which costs nothing more.

A bundle that spelled a value in one of these forms used to land and pass `verify_typed`: a map it
held compared unequal to a map with the same entries, and `value_hash` differed from the `hash_of`
the value it returned (#134). Such a bundle is now refused with `Error::Bundle`. A map that held a
one-byte value apart from its node, or a 512-byte value inside it, passed both the same way, and
compared unequal to a map of the same entries; a list whose positions skipped one passed too, its
`get` returned `None` below its length, its `last` returned the value before the last, and a `push`
overwrote the last (#135). Both are refused now. A spilled value is checked under a type of its
own, one for each map type, so that an object the check passed as a `V` elsewhere, as the root of a
commit whose type holds maps of itself, does not vouch for its size as a spilled value.

Keys of a `BTreeMap` or `BTreeSet` stored out of order are the one form no encoder writes that a
read accepts. An encoder writes them in the order of their type's `Ord`, and that order changes
under steps the schema guide allows: reordering the fields of a struct that derives `Ord`, or the
variants of an enum, or removing a field. A file written before such a step holds its keys in the
old order, and refusing them, as the build after #134 did, made the whole root of such a file fail
to open (#141). So decoding reads the entries in the order stored, checks that they ascend, sorts
them under the reading type's `Ord` only when they do not, and refuses two that compare equal: no
encoder writes those, and a map would keep one and drop the other. A step that makes two stored
keys equal, such as removing the only field that told them apart, fails the read with
`Error::Decode` rather than drop an entry.

The value read is the same whatever order its keys were stored in. What the stored order costs is a
second hash for an equal value: the object that holds it keeps its hash until the value is written
again, so a map that holds it compares unequal to one that holds the sorted form, `value_hash`
differs from the `hash_of` the value read, and the first commit that writes the value again writes
the sorted form and changes the hash, once. A bundle can carry keys out of order too, and lands
with the same cost, and nothing more: no entry is lost or changed, as a key stored twice would
change it. `verify_typed` reports neither. An evolved file holds that form honestly, nothing in the
bytes tells it from a forged one, and a warning on every such file would teach users to ignore it.

A type whose `Ord` compares a field the derive skips can hold two keys that compare unequal and
encode alike. The commit that writes them fails with `Error::Decode` before anything lands, since a
commit decodes what it drafted, so no file holds a value that cannot be read back.

What schema evolution needs stays as it was. A field whose tag the type does not know is skipped
whatever it holds, which is how a removed field and a field from a newer version of the type read. A
missing field reads as its default, an unknown variant tag fails as before, and a `Box`, an `Arc`,
a transparent newtype or `Bytes` in place of a `Vec<u8>` read what the field held, since they
encode alike. A table stores every index under one tag, so it reads its fields with
`Decoder::fields_sharing_tags`, which lets tags repeat but not descend, and still skips an index
under a fingerprint its type does not know. A local read of a root, a node or a spilled value does
not check that the object ends where its value does: bytes left over change no value read from it,
`verify_typed` reports them, and `admin::space` reads roots as a type that reads nothing. A
hand-written impl that reads a field through `Decoder::fields` can call `Decoder::end` on it, as
the derive does.

The varint check is the only one on the paths a read takes all the time. Against the build before
it, with each binary run alternately, three or four times, decoding a million large `u64`s took
161.5 instructions a value against 159.0, medium ones 90.0 against 87.8, and `(u32, u32)` pairs
199.1 against 195.1, with times within the spread of the runs except the pairs, 7.8 ns against 7.5.
A cold scan of a `Map<u64, u64>` took 1,113 instructions a key against 1,108, and gets,
warm scans, single-key commits, a bulk load and the table workload of `make bench` moved by under
0.5%, within the spread of the instruction counts between runs.

A `BTreeMap` or `BTreeSet` decodes by collecting its entries, checking that each key sorts after the
one before, sorting only when one does not, and building the tree from the sorted entries at once,
rather than inserting each entry as it is read. Measured with `make bench-reads ARGS='--only btree'`
against the build that checked and inserted each key, each binary run alternately three times, a
`BTreeMap<u64, u64>` of a million keys stored in order decodes in 12.0 ns a key against 57.7, 342
instructions against 1,000, and a `BTreeSet<String>` of a million keys in 36 ns against 156, 789
instructions against 2,909. A `Vec` of maps of four keys decodes in 17.5 ns a key against 17.1, 372
instructions against 355, the cost of the vector each map collects into. Stored out of order, the
large map decodes in 30 ns a key and the set in 212.

A visit, as `verify_typed` and a receive make, reads each entry once. While the keys ascend it keeps
only the last key and where each entry begins. At the first key out of order it reads the keys
before it again from those places, apart from the decoder, so that the links, tables and zero-width
items they hold are not noted or counted twice, and keeps every key from there on to sort. The build
of #141 went back to the start of the collection instead and visited its values again, and since
the maps inside each value did the same, maps nested out of order at every level cost four times
the key reads for each level: an object of 64 KB holding 32,766 entries took 358 million key reads
and 2.5 s to visit, one of 1 MB would have taken about ten minutes, and a receive charged the
bundle for its size alone (#144). That object now takes 49,149 key reads and 0.44 ms, and receiving
such a bundle costs about 100 µs a KB from 15 KB to 1 MB, as one stored in order does. The places
take 16 bytes an entry past the first eight, in a vector each thread keeps for the next collection
while it holds no more than 65,536 of them. Against the build of #141, each binary run alternately
three times, the large map visits in 7.7 ns a key against 7.2, 235 instructions against 226, maps
of four in 6.3 ns against 6.1, 146 instructions against 140, and out of order the large map in 17.2
ns a key, as before.

Hashes are 160 bits, not 256. A link costs 20 hash bytes plus a varint offset, about 23 bytes
instead of 40, which cut the bytes written per single-key commit by about 19%. Second-preimage
resistance, which is what tamper evidence on read needs, is 2^160. Collision resistance is 2^80
instead of 2^128. Anyone who can pick the bytes of two objects and can afford about 2^80 hash
evaluations can make two different objects share a hash, and ferro would then treat them as the same
content in `==`, `diff` and write deduplication. Nothing in ferro authenticates against such an
attacker; the superblock checksum is unkeyed. Raising `HASH_BYTES` to 32 restores the full claim at
the cost of those bytes.

### Zero-width items

An item of a sequence can encode to nothing: each `()` of a `Vec<()>`, and each `((), ())`,
`Box<()>` or `[(); 4]`. A `Vec<()>` of a billion items is a varint of five bytes, and decoding it
loops a billion times, so a forged length in a damaged file or a hostile bundle could keep a read or
a receive check running for as long as it liked. `Decoder::item`, which `Vec`, arrays, `BTreeMap`
and `BTreeSet` read each item through, counts the items that consume no bytes and no links, and
fails with `Error::Decode` past 2^24 of them in one object. The count is per object: struct fields
share their struct's, the keys, values and summaries of a map node share the node's, and a value
spilled into an object of its own, or a nested collection's nodes, each start at zero. 2^24 `()`s
take about 3.5 ms to visit, so the items no object pays bytes for cost a read or a check at most
that much an object. A receive also counts them across the whole bundle and refuses one whose
checks would visit more than its size allows, as described under [Zero-width items in a
bundle](sync-receive.md#zero-width-items-in-a-bundle).

`Encoder::item` counts the items it writes the same way and fails with `Error::TooManyZeroWidth`
past 2^24 in one object, so a commit refuses what a read would refuse instead of writing it, and
`try_hash_of` returns the error. Every impl that reads items through `Decoder::item` writes them
through `Encoder::item`, which the `Data` docs ask of a hand-written impl as well. A map node
encodes each inline value with its items counted apart and adds them to the node's count only if the
value stays inline, so a value that spills is counted in its own object alone. A node that reuses
the encoded entries of the node it replaces, because only its children changed, adds the count
recorded with those entries, and then counts its child summaries on top. The staged encode of a
large commit counts its nodes the same way. The receive check and `verify_typed` decode each object
with one decoder, as a read does, so they hold a file to the limit it was written under.

A commit used to check the new values of a node that kept most of its old ones each in a decoder of
its own, while a read decodes the whole node with one, so the counts of a node's values added up
only on the read. Three commits that each put six million `()`s under keys of one node all landed,
`verify` passed, and then `verify_typed` reported the node corrupt and every read of it panicked
(#109). A root `Vec<()>` of 2^24 + 1 items was refused by the check that decodes a new root, with
`Error::Decode`, while `hash_of` encoded it.

Counting per value instead, each key, value and summary of a node from zero, would accept every node
whose values fit one at a time, but a node's cost would then grow with its entries. An entry whose
value claims 2^24 `()`s takes about seven bytes of its node with its key, slot tag and child count,
so a hostile node of 1 MiB would buy some 150,000 times 3.5 ms, about nine minutes of one check,
where a bundle buys at most 3.5 ms for each object it sends, and no more in all than its size
allows. Capping a node's entries to keep that down
would refuse, at commit, any map with enough keys that hash to one level between two higher ones.
Counting per object refuses only values that fit alone but not together in one node, more than 2^24
items that take no room between them, which no realistic value holds.

### Nesting depth

Decoding, visiting, encoding and pairing a value recurse once for each level it nests, and Rust
aborts the process on a stack overflow rather than unwinding. A chain of 5,200 `Option<Box<Node>>`
in a bundle of about 25 KB aborted the receiver on an 8 MiB stack, and `Vec` and `BTreeMap` chains
did so below 4,000 levels (#118). Two things keep that from happening. Each level makes sure the
stack has room for it, and moves to a segment of stack that ferro maps for the thread when it does
not, so a value reads and writes on any thread however deep it nests. And a value nests at most
`MAX_DEPTH`, 4,096, levels within one object, which bounds the stack, and so the memory, that a
value can make a thread take, and the stack taken by the clones and drops that ferro does not grow.

A type can hold itself only behind a pointer or in a collection, since a type that holds itself
inline has no size, and of the types ferro implements `Data` for, that means a `Box`, an `Arc`, a
`Vec`, a `BTreeMap` or a `BTreeSet`; `Map`, `Set`, `List` and `Table` hold their entries in nodes of
their own, which decode apart. So those five impls each read and write what they hold one level
deeper, through `Decoder::nested` and `Encoder::nested`, and every recursion over a value, whatever
mix of structs, enums, `Option`s, tuples and arrays lies between, passes one of them at each turn. A
`Vec` or a map counts once, not once for each item, so a long `Vec<u64>` costs one check, and a
derived struct, a transparent one and an `Option` cost none. A decoder past the limit fails with
`Error::Decode`, "a value nests deeper than 4096 levels", which a receive turns into `Error::Bundle`
and `verify_typed` reports, and an encoder fails with `Error::TooDeep`, so a commit refuses what a
read would refuse. A hand-written impl for a pointer or a collection of its own, or one that boxes a
value of its own type that it decodes itself, has to call `nested` too, which the `Data` docs ask
of it.

Counting at each struct and each item, in `Decoder::fields` and `Decoder::item`, was tried first,
and catches recursion through derived types without asking anything of a hand-written collection.
It put a compare and two stores around every item of every sequence and a check in every struct,
and together with reading each field's header in a function of its own, to shrink the debug frame of
`fields`, it cost a release build 20 to 30% on decoding a struct of eight fields that holds three
collections, 70 to 90% on visiting it, and about a nanosecond on each `u64` of a `Vec`. Counting at
the five indirections, with `nested` always inlined, costs nothing measurable there.

#### Growing the stack

`nested` runs what it reads or writes through `stack::with_room`, which compares the address of a
local with the bounds of the stack the thread is on, kept in a thread-local: a load, a subtraction
and a compare. While at least 128 KiB, `RED_ZONE`, are left below, the level runs where it is.
Otherwise it runs on a segment: 16 MiB mapped above a guard page, whose pages the system provides
as they are first touched, switched to with `psm::on_stack`. The levels below check against the
segment's bounds in turn and move to the next segment when it runs short. A panic on a segment is
caught there and resumed once back on the stack that switched, so nothing unwinds across a switch.
The `same_encoding` of the same five impls runs through it too, and so does each level of a map's
tree that a commit writes.

A thread learns its stack's bounds from the system the first time it runs short:
`pthread_getattr_np` on Linux and Android, `pthread_get_stackaddr_np` on macOS and iOS, and
`GetCurrentThreadStackLimits` on Windows. A stack outside those bounds and the thread's busy
segments, such as a coroutine's or one another library switched to, never counts as having room,
so a recursion there moves to a segment at its first level. Windows has no segments: `psm` switches
stacks there only through fibers, and building it for Windows needs a C compiler for the target,
which `make lint-windows` has none of. A level there that finds less than `RED_ZONE` left fails
instead, with `Error::Decode` or `Error::TooDeep`, so a Windows thread reads values only as deep as
its own stack holds, a few hundred levels in a debug build on the 1 MiB of a main thread. Where the
bounds cannot be learned and there are no segments, as on the BSDs and under Miri, nothing is
checked, so `MAX_DEPTH` stays 128 there, which a decode fits within on a spawned thread's 2 MiB.

A segment is busy from before the switch to it until after the switch back, and no other work
takes or unmaps a busy segment: work takes the first idle one, or maps another. Stackful coroutines
on one thread can leave work suspended on several segments at once and finish it in any order, so
nothing may assume that the work last switched to returns first, as an earlier version did when it
counted segments in use and unmapped those past the count on each return: a coroutine that
returned before one it had interleaved with unmapped the other's segment under its frames. The
thread-local that says which stack the thread is on can name another stack after such a switch, so
a check that fails asks again where it is, the thread's own stack or a busy segment, before it
moves. A thread that exits leaves a segment still busy mapped.

A thread keeps one idle segment and unmaps the others. Mapping a segment and unmapping it after
each use is what `stacker::maybe_grow` does, and costs 2.9 µs each time, where switching to a kept
segment costs 14 ns. A value whose many small branches end just past the edge of a stack crosses
that edge once for each branch, which a forged value of a few bytes a branch can arrange, so keeping
the segment is what keeps the time a forged value costs in proportion to its bytes.

Segments are mapped with `MAP_STACK` and advised `MADV_NOHUGEPAGE`, since with transparent huge
pages a shallow use of a fresh segment took a 2 MiB page: 32 threads that each made one small
commit on a 2 MiB stack kept 220 kB each, 192 of them in huge pages, where they keep 26 kB now, as
on a 64 MiB stack. The checks on a segment first stop 256 KiB below its top: one that goes past
there marks the segment deep and goes on to its bottom, and a clone, which checks nothing, marks it
deep if `mincore` finds the page below those 256 KiB in memory, as it is once frames went past it.
When a deep segment falls idle, the pages below its top 256 KiB are given back with
`MADV_DONTNEED`, so a thread keeps at most that much of the deepest work it did on segments. 32
threads that each read a chain at the limit of a struct of 16 `u64`s, a `String` and the next link,
which takes 7 MB of stack, kept 8.6 MB each, 4.3 MB of it in huge pages, and keep 2.7 MB now, most
of it their own 2 MiB stacks; on stacks of 64 MiB they keep 7 MB.

Two other ways were weighed. A deep decode could run on a thread with a large stack of its own, but
a recursion cannot move there half way, so the whole read would start over there, the closures a
hand-written impl passes to `nested` need not be `Send`, and the thread-locals of a decode, such as
the one that keeps an index rebuild from recursing, would not follow it. And an explicit stack of
work for `Box`, `Vec` and the B-trees would take only their part of the recursion: a type that
holds itself recurses through its own `decode`, derived or hand-written, at every level.

Collections link their nodes, and a node decodes in a decoder of its own, starting from zero, as
the values a node holds are read only when asked for. The paths that cross from a value into the
nodes of a collection it holds within one call each keep the count bounded:

- Encoding: a map whose nodes a commit writes encodes them with encoders that start two levels
  below the value holding the map, since writing a node takes about the stack of two levels of a
  value, and `Encoder::child` starts as deep as its parent, so the levels of a value, of the maps it
  holds and of the values in their nodes add up, and a commit that writes 2,048 maps nested in each
  other's values refuses the next. Each map or table read from the file encodes as its root link
  alone and adds nothing, so a commit can stack maps deeper one commit at a time, which reads and
  pairs as below.
- Pairing: `Data::pair` of a map queues the pair of roots on the `Pairing` and returns, and
  `pair_roots` pairs the queued maps one at a time once the root value is paired, each with
  decoders of its own. Maps nested in the values of maps pair in a loop rather than a recursion,
  however deep, and the stack holds the levels of one value at a time.
- Index rebuilds: a table that decodes without one of the indexes its type declares rebuilds it
  from its rows, which decodes every row, and a row that held a stale table rebuilt that one within
  the same decode, so a chain of tables nested in rows took 14 KB of stack a level in a debug build
  and recursed through every level. A table that decodes while its thread already reads rows for a
  rebuild now reads its own rows from the file without caching them and without rebuilding the
  tables they hold, which its index entries do not need, so each read rebuilds one level and a
  level below rebuilds when it is read. Opening a file whose root holds 3,000 such tables takes 50
  KB of stack, as one of 30 does.
- The typed checks of a receive and of `verify_typed` follow links through a queue, each object
  checked by a decoder of its own.

`Data::tables` and `Data::describe` recurse over types rather than values, and stop at a type
already open, so no input reaches them.

#### Clones and drops

Cloning, dropping and comparing a value with `==` recurse through it as well, in code that Rust
generates for its type, which checks nothing. A commit clones the root value to give its closure a
copy to change, a group of coalesced commits keeps a clone to roll back to, and a merge clones its
base, so those clones run through `stack::roomy`, which moves them to a segment unless 12 MiB,
`DEEP_ROOM`, are left. That costs a switch to the kept segment and a `mincore` on each commit made
on a thread with a smaller stack. The closures of commits and merges run where they are called, not
on a segment: a stack overflow there is reported as one, where on a segment, whose guard page the
runtime does not know, it would end the process with a bare `SIGSEGV`. Drops are not grown: ferro
drops the values it decoded and the roots it replaced where the last reference to them goes, which
can be any thread that read them.

#### The stack each level takes

The stack each level took was measured by filling a thread's stack with a pattern, running the
operation, and finding the deepest byte it changed, for chains of 40 and 120 levels, and of 1,000
and 5,000 levels for the clones and drops, which need not be encodable. In bytes:

| Shape | Build | Decode | Visit | Encode | `same_encoding` | Clone | Drop |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `Option<Box<Node>>` field | debug | 2,656 | 2,368 | 1,440 | 352 | 256 | 96 |
| | release | 592 | 448 | 272 | 16 | 48 | 32 |
| `Vec<Node>` field | debug | 3,248 | 2,624 | 1,904 | 608 | 752 | 160 |
| | release | 880 | 464 | 224 | 64 | 144 | 64 |
| `BTreeMap<u8, Node>` field | debug | 3,952 | 3,296 | 2,112 | 896 | 1,440 | 784 |
| | release | 800 | 720 | 256 | 80 | 208 | 128 |
| enum variant `More(Arc<Node>)` | debug | 2,480 | 2,464 | 1,216 | 320 | 0 | 192 |
| | release | 528 | 480 | 192 | 48 | 0 | 32 |
| struct of 26 fields | debug | 5,088 | 2,768 | 3,776 | 352 | 1,072 | 128 |
| | release | 2,880 | 1,024 | 336 | 80 | 768 | 48 |

A node that also holds an `Option<Table<_>>` clones in 528 to 800 bytes a level in a release build
and 608 to 2,080 in a debug one, the most for `BTreeMap` children, and drops in 32 to 144 and 128
to 816. The derive's debug frames were larger before: a decode kept a slot for each field's `Result`
in the closure that reads the fields, and the temporaries of building the value in the frame that
recurses, so a struct of 34 fields took 16,928 bytes a level and `Option<Box<Node>>` 4,768. A field
now decodes through `codec::decode_into`, whose frame holds one field's temporaries and only while
that field decodes, the value is built in a closure called once the fields are read, and a visit
checks its required fields the same way.

So at the limit a decode takes up to 12 MB of stack in a release build and 21 MB in a debug one,
for a struct of a few dozen fields at every level, and that is the most a value, forged or not, can
make each thread that reads it take. A clone of the root at the limit takes at most 3.3 MB in a
release build and 8.5 MB in a debug one, within the segment a commit runs on. A drop at the limit
takes at most 590 KB in a release build, within the 2 MiB of a spawned thread, but up to 3.3 MB in
a debug build, for a tree of `BTreeMap` children that each hold a table: such a value, dropped on a
thread of 2 MiB in a debug build, overflows its stack. A type meant to hold long chains can drop
them in a loop, as the shapes of the tests do, which costs no stack for their depth. 4,096 keeps
the drops of a release build within a spawned thread's stack; 65,536 levels would let a drop in a
release build take 9 MB and a clone 52 MB, and let a value make each reader take 190 MB.

Each shape of the tests, at the limit with a table at the bottom, is committed, reopened, received
twice with the bundle's pairing and passes `verify_typed` on a 2 MiB stack, and decodes, visits and
encodes on a thread of 64 KiB. A tree of 4,095 levels with 40 leaves at each commits, reopens, reads
and verifies on a 2 MiB stack. A chain of a million levels, a byte each, is refused eight times over
in half a millisecond on a 64 KiB stack, and a bundle that carries it is refused before anything
lands. Two stackful coroutines on one thread that each leave a level suspended on a segment keep
their frames whichever finishes first, `tests/stack.rs` drives them with `corosensei`, and
`tests/depth.rs` passes under AddressSanitizer, which ferro tells of each switch, and
ThreadSanitizer.

The check costs nothing measurable on the paths that run it most. Over three alternating runs of
each build on a machine that other work shared, the fastest run of each against the build before:
23.3 against 23.1 µs to commit one entry under `Durability::Buffered`, in 0.5% more instructions,
for the switch and the `mincore` of `roomy` and the checks, and 409 against 412 µs under `Fsync`;
5.23 against 5.31 µs for a cold get and 308.5 ns for a warm one in both; 19.6 against 20.0 ns a
key to scan and 672 against 668 a key to bulk load; 5.0 ns an item to decode a `Vec<u64>` of large
values in both, and 14.3 against 14.1 ns to decode and 7.9 against 8.2 to visit an entry of a
`BTreeMap<u64, u64>`, each within 0.6% of the instructions it took before. A `Box<u64>` decodes
in 6.7 ns where the stack has room and in 21 ns on a thread so short of it that each decode
switches to the kept segment. The one outlier is a `BTreeSet<String>` of a million keys, which
decodes in 4% more instructions and visits in 46% more time on 1% more instructions, though no
check runs for its keys. It follows where the compiler places code rather than the work per key:
`visit_entries` is kept out of line because inlining it slowed the visit of the `BTreeMap` by
12%, and the same `#[inline(never)]` on the old build slows its visit of the set by 22% and of the
map by 23%.

## Records

A record is `varint stored_len`, `varint (link_count << 1 | compressed)`, an optional
`varint raw_len`, the body, then the links. Each link is its hash followed by a varint of twice the
distance back to its target, `record offset - target offset`. Children are written before parents,
so the distances are small and positive in the common case, and nothing in a record depends on where
it sits except those distances. A link's varint may also be padded: an odd value, the varint of
twice the distance plus one in all its bytes but the last, and a last byte of 1, so that it takes
more bytes than the distance needs. A commit drafted while other frames are placed pads a link to an
object before its frame when the distance would need a longer varint once the frame moves as far as
frames have lately moved, so that a move rewrites each such distance in place, as described under
[Moving a draft in place](commit-path.md#moving-a-draft-in-place). Records a sync stages in memory
keep a zigzag varint of the distance, since a record there may link an object of the file past it.
In a staging frame every link leads back, and the zigzag of a distance back is the even varint a
record in a frame holds.
Bodies of 96 bytes or more are LZ4 block compressed when that makes them smaller. Compression runs
after hashing and decompression before the hash check, so the hash covers the canonical body and two
files with different physical compression still agree on every root hash. The decoder rejects a
claimed expansion beyond LZ4's 255:1 limit before allocating. `lz4_flex` is built with `safe-decode`
and without `safe-encode`: the compressor is its unsafe implementation, unsafe code from a
dependency on the write path that only ever reads bytes ferro produced, and the decompressor is its
safe module, which forbids unsafe code, because it reads bodies from files and sync bundles that may
be damaged or hostile. [Bytes of a small commit](commit-bytes.md) splits a commit's records into
their parts and measures other encodings of records and nodes.

Every varint in a record takes the fewest bytes its value needs, as `push_varint` writes it, and a
reader refuses a longer one as damage. A padded distance is no exception: its value includes the
last byte of 1, so it is the shortest varint of that value, and a reader refuses an odd value whose
last byte is anything else. A longer form reads as the same number: the last byte with
its continuation bit set, then a zero byte. So a flipped high bit in the last byte of a record, the
end of its last link's offset, read on into the next byte when that byte was zero, as the first byte
of a frame's table is about once in 256 frames. The record then gave the same object one byte
longer, and no read and no `verify` saw the flip (#91). Both readers of records in the log, the one
that takes a whole record from bytes it already holds and the one that reads on from the file, check
it with one comparison on the last byte of each varint. Over six alternating runs of `ferro-bench`
and of its `reads` on a million keys, that added 0.12% to the median user-space instructions of a
cold `get` with a 1 MiB cache, 56,801 to 56,871, and 0.06% to those of a key of a cold scan, 1,086.3
to 1,086.9 for `u64` values, while the median times, 5,719 and 5,690 ns a get and 106.1 and 105.5
ns a key, moved less than the runs of either build spread.

A compressed body is the one part of a record whose bytes are not all checked. LZ4 can encode one
body in many ways: the decoder ignores the low four bits of the last sequence's token, which holds
only literals, and a match may name any earlier copy of the bytes it repeats. A flip there
decompresses to the same body, the hash matches, and nothing reports it. Nothing needs to, since the
body is all those bytes are for and the hash covers it. A flip that changes the body fails the hash
check, the decompression, or the check that it decompresses to its raw length. Refusing the rest
would take compressing every body again on read, and would tie the format to one version of the
compressor. A cycle and a sync copy a compressed body as it is, so such a flip goes with it, as
harmless in the copy.

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
an anchor, the newest position known to be durable. The superblock's magic, `FERRODB7`, is the
format's version. The log is a run of frames, one per commit. A frame is a 68-byte header (magic,
sequence, end offset, head pointer, xxh3-128 check), the records the commit wrote, and a table that
holds the xxh3-64 of each 4 KiB block of those records, the last block shorter, 8 bytes a block. The
check covers the header and the table, and through the table every record, so a frame proves by
itself that it reached the disk whole, and the table says which block of it did not, as described
under [Torn and damaged frames](#torn-and-damaged-frames). The head pointer names the branch table,
or a step that puts one branch's new head on the table or step the frame before named, as described
under [Branch-table steps](commit-path.md#branch-table-steps). A table's body is the byte `0x7e` and
the varint count of its branches, and its links the root of version 0 and the root of the `Map`. A
table with no version 0, which only a file converted from format 6 holds, is the count alone and the
root of the `Map`, as before format 7. A step's body is the byte
`0x7f`, the branch's name as a varint length and its bytes, and the `Head` as the table stores it:
the version, the first version kept, the root and the log's tip each as a flag byte and, when set, a
link, the map of merge records, and, when bit 2 of the root's flag byte is set, the sequence number
taken when the branch was created, as [A branch of the same
name](history.md#a-branch-of-the-same-name) describes. Its first link is the table or step before
it, followed by the head's links.

A staging frame, magic `FERROSTG`, holds the records of a large `receive` until a later frame links
them, as described under [Where checked objects wait](sync-receive.md#where-checked-objects-wait).
Its header has the same fields, the head pointer naming what the frame before named, but its check
covers the header alone and it has no table: the receive places it with room for 1 MiB of records,
or for one larger record, writes its header and makes the file at least as long, lets the locks go,
and writes the records afterwards. A scan, and a writer of another process catching up, check the
header and go on at the frame's end without reading the records, so they never wait for them. The
receive syncs its staging frames before any frame that links them is written, under either
durability, and a reader checks each record's hash as it reads it, as everywhere else. Format 6
added these frames: a build of format 5 would read one as the end of the log and truncate it and
every frame after it, so no file of format 5 opens in this build, nor one of format 6 in an earlier
one, and both report `Error::NotFerro`. A file of format 5 holds no staging frame, so its fixtures
were converted by giving their superblocks the new magic and check.

Format 7 stores version 0, as described under [Version 0](history.md#version-0). A build of format
6 would fail to decode a table that names it, so no file of format 6 opens in this build, nor one
of format 7 in an earlier one, and both report `Error::NotFerro`. The fixtures were converted the
same way: their tables name no version 0, and the first frame written to one writes a table that
does.

A commit does this:

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
never line up with the log later. Nothing at or below the newest superblock is truncated but what
lies past its anchor, which under `Durability::Buffered` a crash may have lost, as described under
[What a superblock names](#what-a-superblock-names), and staging frames that end the log, which a
writer opening with no writer alive drops: a landing is written after the frames it links, so
nothing can have linked them.

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
bit in its header and table together, and at most one in each of at most 8 of its 4 KiB blocks of
records, makes every check pass, with each such bit passing the rule on sectors below, and torn
otherwise. The table keeps the search cheap: only a block whose entry fails is searched, by trying
each of its 32,768 bits against the entry. A flip in the check shows as a check one bit from the
one computed, a flip in the table as an entry one bit from its block's hash, and a flip in the
magic or the mark is found by trying each of their 416 bits against the check.

Older bytes do not always differ from the frame's in many bits. What decides it is how many bits
the lost write would have changed, not how many of the frame's bytes its sector holds, and a frame
that holds little in a sector changes few. A frame's writes reach a sector in order of offset, but
for the header of a frame that spilled, which is written after its records, so a lost write leaves
the part of a sector from where that write began in it to the sector's end, or, in the sector
holding the end of such a header, the part from the sector's start to the header's end. That part
holds the zeroes of a reservation, of a new extension or of the room past an earlier write to the
sector, or the ones some devices read from an erased or deallocated block. A frame that held little
there differs from those in a single bit whatever the odds: a sparse value stored raw, as sync
stored a body that arrived uncompressed until it compressed it as described under
[Applying](sync.md#applying), a record whose first byte has one bit just past a spill that ended on
a sector's last byte, or the last byte of a spilled frame's check alone in its sector. A review
found the first two read as damaged after a modelled power cut (#81). So a flipped bit counts as
damage only if, on disk, the frame's bytes in its sector up to the flipped one, and those from it
on, each hold something other than all zeroes or all ones. A sector that a lost write left as
zeroes or ones, from some point on or up to the end of a header, then never mends, whatever the
frame held, with no chance to bound.

Older bytes of another kind, another file's that a file system exposed after a crash, differ from
the frame's at random, and they are older than every write to the sector, so the part they fill is
all the frame has there. They can be one bit from the frame's by chance only when the frame has few
bytes in the sector, at either end of the frame: one byte with one bit set over a random one is a
chance in 32. So the sector must also hold at least 8 of the frame's bytes, where that chance is
about 2^-58; a sector is 512 bytes, the smallest a device writes whole, and anything coarser tears
along the same lines. A flipped bit among the first or last 7 bytes of a frame, when they are all
the frame has in their sector, reads as a torn write, and so does one with only zeroes or only ones
between it and either end of its sector, such as a set bit lost from a run of zeroes. A dead
writer's bytes need not differ at random, since the next writer may write nearly the same frame,
which is why a shared writer truncates them before it writes, as described below.

The search is capped. A torn write leaves most blocks of what it did not reach failing, where bit
rot flips a bit or a few, so a frame with more than 8 blocks whose entries fail reads as torn
without a search, and a frame within the cap costs at most 8 block searches.

A damaged frame fails the open with `Error::Corrupt`, naming the frame's sequence, its offset and
the first byte found wrong, and the open writes nothing: no sync, no truncation, no superblock. A
read-only open that finds no writer alive scans and reports the same, holding the placement lock
shared as well as the writer lock, so a shared writer that opens meanwhile waits before it recovers
or writes anything the scan could read in part. Truncating the file at the offset named gives up
that commit and every one after it, and the file then opens. A shared writer that catches up under
the placement lock, where no live process can be writing, reports the same and never writes over the
frame, as described under [Catching up](processes.md#catching-up). Anything else it finds past the
last whole frame and past the newest superblock, part of a frame or the records of one that spilled
before a process died writing its header, behind a stale cycle marker or not, it truncates and syncs
before the next frame is written there, as recovery at open does, so a lost sector of the next frame
cannot hold a near copy of the dead one. A transaction dropped after it spilled syncs the truncation
of its records the same way. A scan that a write can race takes any frame that does not check for
the end of the log, since it can read a frame another process or thread is still writing in part: a
shared writer catching up without the placement lock, the leader of a shared sync reading past its
own frame, and the quick look a read-only open takes before it knows whether a writer is alive.

The judgement reads the bytes alone, whatever durability wrote the frame or opens the file. Under
`Durability::Buffered` a power cut may still take back every commit past the anchor: a torn frame
there is rolled back as before, and a power cut leaves no frame one bit from whole. A frame that
is, past the anchor, is reported the same way, though Buffered never promised its commit would
survive, since what damaged it was not a power cut, and truncating it would hide that.

Out of order frames and dead writers keep working. A frame that never reached the disk, with whole
frames after it, leaves its older bytes and reads as torn, so the scan stops there and the frames
after it are truncated, as before. A process killed while it writes stops at a page boundary, since
the kernel copies a write into the page cache a page at a time, and the rest of its frame keeps its
older bytes, so the next writer finds it torn, truncates it and writes in its place.

What it does not catch: two flipped bits in one block, one in the header and one in the table, or
one in a block's entry in the table and one in that block, which leaves no entry one bit from its
block's hash, flipped bits in more than 8 blocks, and a flipped bit with only zeroes or only ones
between it and either end of its sector, read as a torn write, and so does a sector that a lost or
misdirected write left holding other bytes whole. Past the newest superblock they are truncated as
before. In a frame that superblock names they fail a shared writer's catch-up and an open beside a
live shared writer, as described under [What a superblock names](#what-a-superblock-names), and an
open with no writer alive truncates them only past the anchor, where a crash may have torn the
frame. The other way round, a torn frame reads as damaged only if a lost sector kept older bytes one
bit from the frame's with something other than zeroes or ones on both sides of that bit: a near copy
of the frame, which a dead writer's bytes written over could be, and those are now truncated first,
so only a file a shared writer of an earlier build wrote over a dead one's frame can hold one.
Nothing on disk records that a sync finished until the next sync carries its superblock, so a frame
of the last sync with a sector gone cannot be told from one caught by the power cut, and only the
bytes can tell damage apart. Damage before the anchor is found by the hash checks on read and by
`verify`, as before.

The tests, in `ferro/tests`: `crash.rs` flips each bit position of a frame past a stale superblock
under both durabilities, and of every header and table byte and a sample of the body of a frame of
more than three blocks, one bit in each of two blocks, and two in one block or one in a block and
one in its entry, which read as torn; it replaces each sector inside a frame with zeroes, ones or an
older frame's bytes, and leaves a hole of zeroes, whole or every other sector, before whole frames.
`buffered_crash.rs` flips a bit in a whole frame past the anchor of every crash image of a Buffered
recording, under every loss, and of an Fsync recording whose superblocks since the last sync were
lost, the frames of acknowledged commits among them. It also records a Buffered and an Fsync file of
values of 300 and 400 KB that compression does not shrink, each with a KiB of zeroes holding one set
bit, three of them pulled from another file into staging frames, and every crash image of every
cut opens, under every loss, as does every image that lost one sector of a frame's write, for
each sector where that write's bytes hold at most 8 set bits and for its first and last. A bit
flipped in a whole frame past the anchor of those images, walking over the staging frames, is
reported after any cut, in frames past the staging frames among them.
`group_commit.rs` flips one in the last frame past the superblock of each image its intercepted
fdatasync takes, frames that sync then made durable, of acknowledged commits among them. `shared.rs`
leaves a dead writer's frame cut at each of its first page boundaries, over zeroes and at the end of
the file, and the next writer writes in its place, and leaves it whole with a bit flipped, which the
next writer reports without writing. It also leaves the first pages of a dead writer's frame that
differ from the next writer's in one bit of a sector, checks that the next writer truncates them and
syncs before it writes, and that its frame with that sector lost reads as torn; without the
truncation it read as damaged. It does the same with a stale cycle marker in front of those pages,
which the next writer took for the end of the log and wrote over without truncating, so that image
read as damaged too until the catch-up looked past the marker (#82). Its tests of frames below the
newest superblock are described under [What a superblock names](#what-a-superblock-names). The unit
tests in `ferro/src/store.rs` replay the writes of frames of zeroes or of ones with a bit set every
few hundred bytes, stored raw and spilled part way, losing each sector of each write in turn and
each by chance 200 times, the lost bytes left as they were or turned to ones, and none reads as
damaged. One loses the sector past a spill that ended on a sector's last byte, before a record whose
first byte is `0x40`, and one the sector holding only the last byte of a spilled frame's header, a
check byte with one bit set, which both read as damaged before. Another flips a bit in 1, 8, 9 and
12 blocks of a frame, and only the first two are reported. One opens a file with a damaged frame
past its superblock read-only while another descriptor holds the placement lock, which trusts the
superblock, and once it is released, which reports the frame, and one drops a draft that spilled and
checks that a sync follows the truncation. `compress.rs` pulls a 32 MiB value of zeroes with a byte
of 1 every 8 KiB, which the sender sends uncompressed, and checks that the receiving file stays
under twice the sender's and that a power cut over the pulled frame reads as torn. Every existing
power-cut, crash and shared-process test opens every image it did before.

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
about that much longer, once, for the first block of the torn frame that fails, and nothing when
more than 8 blocks fail. A flipped bit ends its block's search when it is found. Opening a copy of
a Buffered file of 40 commits of 3 keys, frames of a median 2,621 bytes, scanned from its anchor,
took 1.2 ms with every frame whole, syncing and writing the superblock that recovery writes; 1.5 ms
with a bit of the last frame flipped, writing nothing; and 5.0 ms with a sector of it torn. With
frames of 1.26 MB it took 2.1, 3.1 and 5.3 ms.

Before the cap the search ran over every failing block, so a frame with a bit flipped in each block
cost a search of each: an open of one of 2 MiB took 1.6 s and one of 16 MiB 12.5 s, about 3 ms a
block, which comes to about 13 minutes at 1 GiB, for a read-only open, a writer's open and every
catch-up of a shared writer under the placement lock alike. With the cap, a frame of 64 blocks
opened read-only in 3.1 ms with one block flipped in its last bit, 6.2 ms with two, 12.3 ms with
four and 24.7 ms with eight, the most it searches, and in 0.02 ms with 9, 16 or 64, which read as
torn; the frame of 16 MiB opened in 1.7 ms. Torn as the power-cut tests tear them, each sector kept
or not by chance, with the header and table kept so that the judgement reaches the blocks, frames
of 1, 2, 4 and 8 blocks of records left almost every block failing in each of 200 tears, and each
of more than 8 failing blocks, 112 of the 200 of 8 blocks and all those of 16 and 64, read as torn
in 0.01 to 0.02 ms, where it took a block's search, 3 ms, before. A read-only open holds the writer
lock shared while it judges, so a search within the cap can keep a writer opening meanwhile, which
retries for 5 ms, out with `Error::Locked`, as a long scan could before.

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

Those tears missed frames that hold little, and the rule on zeroes and ones came after a review
found torn frames of them read as damaged (#81). On a file of 40 commits of 3 keys and one of 40
keys, frames of a median 1,012 and 8,672 bytes, 36,039 and 345,829 positions, flipping each
position's bit left 5 and 15 unreported under the rule on sectors alone, and 6 or 7, as commit
times vary between runs, and 18 under the rule with zeroes and ones. Also measured and left out:

- Requiring each flipped bit to sit at least 8 bytes before the end of its sector, which bounds the
  chance that random older bytes past an earlier write mend. It leaves 761 and 4,999 of those
  positions unreported, 2.1% and 1.4%, and still mends a sector of a sparse value lost whole.
- Counting the set bits of the frame's bytes in a sector, the bits a lost write over zeroes would
  have changed. It mends a sector past a spill, where the earlier write's part holds the frame's
  bytes, and a sector of ones.
- A budget of hashes rather than of blocks. A block's search is at most 32,768 hashes, so 8 blocks
  is that budget, and counting blocks lets an open give up before it hashes anything.

### What a superblock names

Nothing at or below the newest superblock is ever truncated, and no frame is written there, except
what lies past its anchor when a crash may have taken it back. The judgement above is for what lies
past the newest superblock. A frame it names was whole in the page cache before the superblock was
written, and its commit was acknowledged, so if the frame no longer checks it was damaged since,
however its bytes read: two bits flipped in one block, a magic two bits from a frame's or a header
of zeroes read as torn or as the end of the log, and below the newest superblock each is reported as
`Error::Corrupt`, naming the frame, with nothing written. Where the line falls depends on what can
have happened since that superblock was written:

- An open with no writer alive cannot tell whether a crash came between, and scans from the anchor
  as it always has. Under `Durability::Fsync` the newest superblock anchors itself, so the scan
  starts at the end of the log it names and keeps all of it. Under `Durability::Buffered` a crash
  may take back what is past the anchor, and the scan rolls those frames back to the first that
  does not check, as Buffered allows: the frames past a superblock's anchor are the only ones it
  names that are ever truncated.
- A shared writer catching up under the placement lock is alive, so no crash has come since it
  opened, and every superblock written since names frames that were whole. It reads the newest
  superblock and fails with `Corrupt` if the frames it follows from its own end of the log stop
  before the end of the log that superblock names, under either durability, and judges a frame
  torn and truncates it only past that, as described under [Catching
  up](processes.md#catching-up). It places its own frame where the catch-up ended, so that is never
  below the newest superblock either.
- A shared writer that opens while another that has opened is alive knows the same, as described
  under [Opening and closing](processes.md#opening-and-closing): its scan from the anchor must reach
  the end of the log the newest superblock names, under either durability, or the open fails with
  `Corrupt` and writes nothing.
- A transaction dropped after it spilled truncates its records back to where its frame would have
  begun, which for a shared writer follows a catch-up under the placement lock it holds throughout,
  and a writer without the option truncates its reservation at close only past every frame it has
  written, so neither reaches below the newest superblock.

A lagging shared writer used to judge every frame from its own end of the log (#82). An idle
handle's commit after another had committed four frames, with two bits flipped in the second's
first block or its magic, read that frame as torn, truncated the file from 31,905 bytes to 20,022,
synced, wrote its own frame there and acknowledged it, under either durability; the other handle's
next commit failed reading past the end of the file, and every open then failed with `file is 20022
bytes but superblock expects 31905`. With the second frame's first 84 bytes zeroed it wrote over
that frame in place, and the file failed to open on a bad compressed body. A writer opening beside
a live Buffered one rolled the same frames back to the first, from 31,900 bytes to 14,104, and the
live writer's next commit failed the same way. `shared.rs` covers each of these, as listed under
[Tests](processes.md#tests).

Reading the newest superblock costs one read of both slots and their checks on every placement of
a shared writer. `tables` with `--shared 1`, median of nine alternating runs against the tree
before: outside the closure a hold ran 570,530 instructions before and 575,373 after, a confirm
738,487 and 740,719, and a cancel 900,826 and 904,098, with thread CPU time of 75.6 and 76.5, 95.8
and 96.6, and 123.1 and 123.9 µs, at 13,393 bytes a write both. Reading it only when the catch-up
ends on something other than zeroes would save that, but a frame zeroed below the newest
superblock would then be written over and acknowledged, as the third case above was.

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
of a process reserves nothing, so a short run pays for no zeroes. While a receive of the same writer stages records in the log, as described under [Where checked
objects wait](sync-receive.md#where-checked-objects-wait), commits add no zeroes, since the next
staging frame would be placed over them and its records would write the bytes a second time; a
shared writer's staging frame makes the file 4 MiB longer than itself, which other processes take
as reserved. Commits beside a receive then extend the file, as the receive's records do anyway.
The reservation is only an optimization. A failed reservation stops further attempts without failing the commit, open discards
it, and a clean close truncates it, so the file can be up to 4 MiB longer than `size_on_disk` while
open, and longer still by staging frames that end the log, which hold a receive's records while it
streams and stay, once it is refused, until the writer closes. Truncating those at the refusal would
take the log's end and sequence numbers back while other threads place frames, and a shared writer
cannot, since another process may have followed them.

### Failed writes and `refresh`

A failed write or fsync poisons the store until reopen. Only one process may hold the writer lock,
unless every writer opens with `Options::shared`, as described under [Writers in several
processes](processes.md). Read-only opens otherwise take no lock, since log bytes under a published
head never change, and `refresh` picks up new commits. It reads the new refs and the state of every
attached branch before it changes anything, holding the published refs so that no branch attaches in
between, and only then records the head, swaps in a replaced file and publishes. A refresh that
fails partway, say on a read error, therefore leaves the handle as it was, and the next one starts
over and catches every branch up. It used to record the head first, so the next call saw nothing new
and the branches the failed one missed stayed behind until another commit landed.
