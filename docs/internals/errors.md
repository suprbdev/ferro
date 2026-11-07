# Errors and checks on local reads

## Panics and `Result`

Collections read like native types, so `get` and iterators do not return `Result`. An I/O failure or
corruption during a read panics. `try_get` exists for callers that need to handle it. Inside
`update` a panic discards the transaction and leaves the database unchanged.

A decode error on read stays a panic. Every object a file holds was either written by this process,
which decodes each new key, value and root as it writes them and counts the items of each object
that take no room against the limit a decoder holds it to, or checked as its type by `receive`,
so an object that does not decode is a damaged file or a schema change that broke stored data, the
same classes as a bad hash. Turning it into an `Error` would put a `Result` on every `get`, iterator
step and comparison for a case a healthy file cannot reach, and the read would have nothing better
to do than stop. `try_get` and `try_for_each` surface it as an `Error` for a caller reading a file
it does not trust, and `verify_typed` finds it before any read does.

`Db::verify`, `common_ancestor`, `Db::have`, the bundle planner and the `admin` functions return
`Result`, so they walk the branch table and merge records through a crate-internal `try_entries`
that returns a read or decode error, not through `Map::iter`, which would panic on one.

## Checks on local reads

A hash proves that an object is the one its link named, not that a file that names it was written by
`ferro`. Bundles get the full node check described under [Checking
trees](sync-receive.md#checking-trees). Local reads trust more, but each node they take from the
file passes three checks that cost a few compares, so a crafted or damaged file gives an error where
it used to overflow a count, the stack, or the time a scan takes:

- Decoding a node sums its child counts with checked arithmetic.
- Each child link keeps its parent's level, and following a link checks that the node it reaches
  sits below that level and holds the count the link claims. A root link allows level 21, the
  highest `trailing_zeros / 3` gives. Levels fall by at least one a step, so no path is longer than
  22 nodes, which bounds every recursion over a tree: `scan`, `gather`, `flush` and the edits. A
  memo is set only after the check, so memo hits skip it, and a link that finds its node in the
  cache pays two compares. `Link::Stored` keeps the level in padding, so a link without a summary is
  still 72 bytes. On `ferro-bench`'s `reads` at 1M keys, the best of 18 runs moved gets by 1 to 2%
  and scans by less than 1%, inside the spread between runs, and a cold scan, which takes every node
  through the checks, by 2%.
- A node read from the file may not claim more entries than the log could hold. Counts that add up
  and levels that fall still allow a DAG that links one child from many places: 21 nodes that each
  link the same child three times claim 10^10 entries in 10 KB, and wider nodes claim up to 2^64.
  Iteration visits exactly what the root claims, so in practice it would not end. In a tree `ferro`
  wrote no node appears twice, since sibling ranges are disjoint, so every entry costs at least its
  one byte slot tag in a node body of its own. A stored body decompresses to at most 255 times its
  length plus 255, or the store rejects it, and a record adds at least two header bytes, so a log of
  `n` bytes holds fewer than `255 n` entries. That is the bound, checked against the bytes written
  so far: a crafted file can make a scan as long as an honest file of its size could, and no longer.
  Checking that each node falls between its parent's separators would rule sharing out entirely, as
  the bundle check does, but it would cost two key comparisons a level and carry the separators down
  every path into `reach`, for a shape no file `ferro` wrote can have.

Key order, the level each key hashes to, key ranges and summaries are not checked on local reads.
`Map::len` returns the count the root link claims before anything is read, and `Map::summary` the
summary it records. `verify_typed` checks the rest.
