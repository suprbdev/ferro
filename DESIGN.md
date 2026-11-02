# ferro design

A ferro database is one typed Rust value, `T`. There is no query language: you read `T` with
ordinary Rust and change it inside a closure. Every commit produces a new immutable version
of `T`, and all earlier versions stay readable.

## Layers

- `codec.rs`: the `Data` trait, `Encoder`, `Decoder`, primitive impls.
- `store.rs`: the append-only file, superblocks, object reads with hash verification, node cache.
- `map.rs`: `Map<K, V>`, the persistent ordered tree. `Set` and `List` wrap it.
- `table.rs`: `Table<T: Record>`, a keyed collection with automatic secondary indexes.
- `db.rs`: `Db<T>`, `Snapshot<T>`, commits, history, branches, watchers.

## Objects and links

Everything on disk is an `Object { body, links }`. A link is a `Ptr { hash, offset }`.

- The hash is BLAKE3 over body length, link count, body, and the link hashes. Offsets are not
  hashed, so identical content has an identical hash wherever it sits. The file is a Merkle DAG.
- The offset lets a reader jump straight to a child, so no hash index is needed.
- Every read re-hashes the object and compares it with the pointer it came from.
- Links are stored in a side table and consumed in order by `Decoder::link`. They take no bytes
  in the body, which keeps encoded size independent of a value's position.

## Canonical tree

`Map` is a Merkle search tree. A key's level is `trailing_zeros(blake3(encoded key)) / 3`, so
about 1 in 8 keys is promoted per level. Fanout 8 was chosen by measurement at 300k keys: against
fanout 16 it writes 33% fewer bytes per commit for 27% slower gets. A node holds all keys of exactly its level within its
range, and children hold the gaps. Tree shape depends only on the key set, never on insertion
order. Consequences that the rest of the system relies on:

- `a == b` for two maps compares root hashes in O(1).
- A commit that changes nothing has the same root hash and is skipped.
- `diff` skips any subtree with matching hashes, so it costs O(changes), not O(size).

Insert and remove use treap-style `split` and `merge`. Mutation is copy-on-write through
`Arc::make_mut`. `Link::Mem` is a dirty in-memory node, and `Link::Stored` a persisted one.
`flush` writes dirty nodes bottom up during `Data::encode`.

Values over 512 encoded bytes spill to their own object, so a large value does not bloat its
node or get rewritten when a neighbour changes.

## File format and durability

Two 4 KiB superblock slots, then the log. A commit does this:

1. Append all new objects past the published tail, then fsync.
2. Write the superblock to the alternate slot with sequence, log end, head pointer, checksum.
3. fsync.

Open picks the valid slot with the highest sequence and truncates anything past its log end, so
a crash can lose only a commit that was never acknowledged. A failed write or fsync poisons the
store until reopen. Only one process may hold the writer lock. Read-only opens take no lock,
since log bytes under a published head never change, and `refresh` picks up new commits.

## Concurrency

One writer at a time, serialized by a mutex. `update` clones the root, runs the closure, flushes,
and publishes through `ArcSwap`. Readers never block and never see partial state. A `Snapshot<T>`
keeps its version alive for as long as it is held. Unpublished state lives only in the closure's
working copy, so a panic or an `Err` discards it.

## History

Each branch head holds a `Map<u64, Commit>` log, stored in ferro's own tree, which makes
version lookup O(log n). `Refs` is `Map<String, Head>` and the superblock points at it. Forking a
branch copies one `Head`. `restore` commits an old value as a new version, so history is never
rewritten.

`Db::history_of` walks the log and reports the versions where a projection of the root changed.

## Merging

`Db::common_ancestor` finds the newest commit two branches share. Forking copies the log, so the
logs agree up to the fork and differ after it, which makes this a binary search. `Map::merge` and
`Table::merge` then do a three-way merge driven by `diff(base, theirs)`, costing O(changes). A
key changed only by theirs is applied. A key changed the same way on both sides is left alone.
A key changed differently on both sides goes to a `resolve` closure. Merging runs inside an
ordinary commit, so a root with several collections merges each one in plain Rust.

## Compaction

Compaction is offline and needs no user types. It prunes each branch's log, then copies every
object still reachable into a new file. Hashes do not cover offsets, so objects move verbatim
and only their link offsets are rewritten; every object is re-verified on the way through. The
new file replaces the old with an atomic rename. A read-only handle holding the old inode keeps
reading its old snapshot, and `refresh` reports `Error::Replaced`.

## Schema evolution

The derive macro writes structs as `(tag, length, payload)` fields, where the tag is a 16-bit
FNV-1a hash of the field name. Fields are sorted by tag, so reordering a struct does not change
its hash. Unknown fields are skipped. Missing fields use `Data::missing`, or `#[ferro(default)]`.
`#[ferro(rename = "old")]` keeps the old tag.

## Errors

Collections read like native types, so `get` and iterators do not return `Result`. An I/O failure
or corruption during a read panics. `try_get` exists for callers that need to handle it. Inside
`update` a panic discards the transaction and leaves the database unchanged.
