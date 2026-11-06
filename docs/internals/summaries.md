# Summaries

How `Map<K, V, S>` keeps a fold of every subtree. The API is in
[Collections](../guide/collections.md#summaries).

## How a summary is kept

`Map<K, V, S>` with `S: Summary<K, V>` keeps a fold of every subtree. `of` summarizes one entry and
`combine` joins two neighbours, left then right, so `S` needs only to be associative: min, max,
first, last and concatenation work, not just sums. There is no identity element, because an empty
range is `None`. `range_summary` and `summary_by` fold a range in O(log n), and `summary` reads the
root in O(1). Rank in a range is a difference of two prefixes, but a general summary has no
subtraction, so a fold walks both edges of the range and combines the links between them.

Nothing ever updates a summary. `flush` computes each node's from its entries and its children's
links, bottom up in key order, and stores it on the link in the parent next to `count`. So a summary
is a function of the tree below it, the tree is a function of the key set, and root hashes stay
independent of history. A `combine` that is not quite associative keeps that property: a float sum
rounds the same way for the same contents, whatever edits led there. `split`, `merge`, `absorb` and
the rest move links without looking at them, and any node they change is dirty, so none of them
needed to know. Applying a delta to the old summary would have been cheaper per edit and would have
broken canonical form for floats, so it is not done even for sums. Because the stored summary is
that fold and nothing else, `receive` and `verify_typed` can recompute it from a node's bytes and
refuse one that differs, as described under [Recomputing
summaries](sync-receive.md#recomputing-summaries).

`of` must depend only on the encoded key and value, so a `#[ferro(skip)]` field must not feed it. A
read of a dirty tree has no stored summary for the nodes changed in this transaction and folds them
on demand, stopping at stored links, so it costs O(log n) plus the dirty nodes in the range. A
spilled value keeps its summary in the slot, so neither a fold nor a commit reads the value. Without
that, a commit would read every spilled neighbour on its path.

`S` defaults to `()`, a zero sized type, so the plain `Map<K, V>` has the layout, the bytes and the
speed it had. `()` encodes to nothing, `a_plain_map_keeps_its_format` pins a root hash from before
summaries existed, and alternating `ferro-bench` runs moved no byte metric and no timing beyond
noise. A default type parameter takes no part in inference, which is why `HashMap::new` exists only
for the default hasher, and `Map::new` is the same: it is defined for `S = ()` alone, so
`Map::new()` still infers, and no caller in the workspace changed. A summarized map starts from
`Map::default()` or `collect`.

Changing `S` changes a map's encoding under the same field tag, as changing `V` does. Rename the
field to rebuild it.

## Where the summary is stored

The summary is on the link, not in the child, because a parent can then be rebuilt and a covered
child skipped without reading that child. Inside the node a commit saves bytes, but `flush` has to
read every clean sibling on the path. At 1M `u64` keys with an `i64` sum, against 3,095 bytes for a
plain single-key commit:

| Summary stored | Bytes per commit | Commit, 1 MiB cache | Fold, 1 MiB cache |
| --- | --- | --- | --- |
| On the link | 3,333 (+7.7%) | 28 µs | 11 µs |
| In the node | 3,125 (+1.0%) | 80 µs | 57 µs |

Bytes follow the width of `S`, since a rewritten interior node repeats about nine summaries. Bulk
bytes barely move, because every node has one link either way. `ferro-bench`'s `summary` tool, at 1M
keys:

| Summary | Bytes per commit | Commit | Bulk bytes per key |
| --- | --- | --- | --- |
| None | 3,095 | 33 µs | 17.8 |
| `i64` | 3,333 (+7.7%) | 36 µs | 18.1 |
| `i128` | 3,403 (+10%) | 37 µs | 18.3 |
| Four integers in a tuple | 3,874 (+25%) | 41 µs | 19.0 |
| The same four in a `derive(Data)` struct | 4,623 (+49%) | 51 µs | 20.5 |

Field tags repeat on every link, which is the last row, so a wide summary should be a tuple. `i128`
is the natural total for `i64` values and was 16 fixed bytes, so it now encodes as two zigzag
varints, which took the ledger's file growth from 10.3% to 5.6%. Against a plain map over five runs,
full scans took 0% more with the `i64` total and 5% more with the others, and gets 1% more with a
total and 9% more with four fields. Many short scans of a map with a wide summary pay more: 56 bytes
on each link cost `metrics` 25% on its scan path, which folds avoid.

## Against hand-kept rollups

Hand-kept alternatives were built into `ledger` and measured at 1M entries. Every row gave the same
balances:

| Approach | Added lines | File | Bytes per commit | Backdated commit | Balance as of | Trial balance | Cold trial balance |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Scan postings | 0 | 151 MB | 12,178 | 11,492 | 16 ms | 440 ms | 328 ms |
| Summary | 15 | 160 MB | 13,144 | 12,362 | 0.7 µs | 30 µs | 0.8 ms |
| Daily rollup | 10 | 176 MB | 15,544 | 14,905 | 16 µs | 0.7 ms | 5.6 ms |
| Running balance every 32 days | 37 | 157 MB | 14,210 | 14,017 | 85 µs | 1.6 ms | 9.3 ms |
| Rollups by 1, 32 and 1024 days | 28 | 184 MB | 17,224 | 18,037 | 2.6 µs | 65 µs | 0.8 ms |

The running balance has the smallest file of the hand-kept forms because a bulk commit touches one
row per account per block, but each backdated entry rewrites every later block. The hand-kept forms
answer only at day boundaries and are state that can drift from the postings. A summary works on any
key range and is computed from them, and `verify_typed` recomputes every stored fold from the
postings below it. `audit` still compares the balances with the journal, which the postings are
built from and which no summary covers.

`metrics` keeps rollups in a summarized map and folds a bucket that spans at least 8 of them, which
is where folding starts to beat scanning. On 4.32M rollups of 1,000 series: 978 ms to 5.6 ms for one
bucket, 1,067 ms to 79 ms at one day per bucket, and 889 ms to 192 ms at six hours, for 9% more
bytes per rollup. `Rollup` is a fold of its own points, so it needed one `combine`.

## Measured and left out

- A `#[sum]` attribute on `Record`. A summary needs the value in the tree, and an index holds only
  keys, so this means a second tree with one entry per row. For total balance by country on 200k
  rows that was 7,333 bytes per update against 4,765 and a 22% larger file, and 0.75 µs a read. A
  hand-kept `Map<country, i128>` costs 4,887 bytes and 0.02 µs, against 16 ms to walk the index. A
  summary only wins on sub-ranges of an ordered key, and for those a `Map<(group, key), V, S>` with
  `summary_by` is a few lines.
- A summary on `Table` rows. It adds a type parameter to `Record`, `IndexIter` and every table
  iterator, for sums over primary key ranges that nothing here needs.
- A cached summary on dirty nodes. It would need invalidating on every path through `Arc::make_mut`,
  to speed up reads inside a transaction, which are rare.
