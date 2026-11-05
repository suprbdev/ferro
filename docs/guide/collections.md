# Collections

`Map`, `Set`, `List` and `Table` nest freely inside your types. Cloning one is O(1), and `==`
compares root hashes in O(1) because the tree shape depends only on the contents. A commit that
changes nothing is skipped. `Map::root_hash` and `Table::root_hash` expose those hashes,
`Map::value_hash(&key)` gives the hash of one value, and `hash_of(&value)` the hash of any `Data`
value, which makes an ETag one line.

| Type | Use it for |
| --- | --- |
| `Map<K, V>` | Ordered map, with `get_mut` handing out a real `&mut V` |
| `Map<K, V, S>` | The same, keeping a `Summary` of every subtree for O(log n) range totals |
| `Set<T>` | Ordered set, whose `range` has a `len` like the rest |
| `List<T>` | Append-mostly sequence with indexed access |
| `Table<T>` | Keyed rows with secondary indexes kept up to date for you |
| `Bytes` | Byte buffer value with `Deref<Target = [u8]>` and O(1) clone |

## Reading in bulk

`sum`, `count`, `for_each` and other folds on the plain iterators take whole nodes at a time.
`Map::for_each`, `try_for_each`, `for_each_range` and `try_for_each_range` lend `&K` and `&V`, so
they clone nothing, and the `try_` forms stop on `ControlFlow::Break`. Any iterator of a map, and
a table's lookups, lend entries too: `next_ref` steps and lends the next one until the iterator
moves again, and `visit` lends every one left to a closure, without the reference count that a
`Val` of a value with a destructor takes, as [Reading rows in
place](tables.md#reading-rows-in-place) describes. `Map::cursor` gives a
`Cursor` that keeps its path through the tree, so `seek` and `seek_back` cost the distance moved
instead of a walk from the root, which suits sorted lookups and merge joins. `get_many` and
`contains_many` run a cursor over an iterator of keys, and keys out of order still give the right
answer, at the cost of a fresh walk each. `Map::floor` finds the last key at or before a key in one
walk.

```rust
frontier.sort_unstable();
let reached: u64 = degrees
    .get_many(&frontier)
    .filter_map(|(_, count)| count)
    .map(|count| u64::from(*count))
    .sum();

let mut shared = 0;
let mut other = right.cursor();
left.for_each(|key, _| shared += usize::from(other.seek(key)));
```

## Tuple keys

A map, set or table keyed by a tuple of two to four fields often groups its entries by the first.
`prefix(&head)` reads every entry whose first field is `head`, in key order and from either end,
and `Map::prefix_summary(&head)` folds them, so no call has to name the smallest and largest value
of every other field:

```rust
let follows = graph.follows.prefix(&user).len();
let next = state.ready.prefix("mail").next();
```

The head can be borrowed, as `&str` for a `String` field, and each call costs what the same
`range` does. A key type of your own gets these by implementing `Prefix` for its leading field. A
range within a group, such as one account's postings between two dates, still takes bounds.

## Cutting, joining and batching

`Map` and `Set` also have `remove_range`, `take_range`, `split_off` and `append`, which cut and join
trees in O(log n) nodes however many entries move. `Table::drain_range` removes a key range and
returns the rows, which is O(rows) because every index has to be updated. `Map::apply` takes a batch
of puts and deletes and merges it in one descent of the tree, as `extend` does for puts.

## Summaries

A `Summary` folds entries in key order, and `Map<K, V, S>` keeps that fold for every subtree, so a
total over any key range takes O(log n) instead of a scan. A plain `Map<K, V>` pays nothing for
this.

```rust
#[derive(Clone, Copy, Data)]
#[ferro(transparent)]
struct Net(i128);

impl Summary<(u32, i32), i64> for Net {
    fn of(_key: &(u32, i32), amount: &i64) -> Self {
        Net(i128::from(*amount))
    }

    fn combine(&self, other: &Self) -> Self {
        Net(self.0 + other.0)
    }
}

let postings: Map<(u32, i32), i64, Net> = Map::default();
let balance = postings
    .range_summary((account, i32::MIN)..=(account, date))
    .map_or(0, |net| net.0);
```

`combine` must be associative and is applied in key order, so it need not be commutative: first,
last, min and max work. `of` must depend only on what the key and value encode. An empty range gives
`None`. `summary` reads the whole map's fold in O(1), and `summary_by` takes predicates, as
`range_by` does. A summary costs bytes on each commit, 8% for an `i64` total and more as it widens,
so keep it small. Use a tuple rather than a `derive(Data)` struct, whose field tags repeat on every
link.

## How it works

- [The Merkle search tree](../internals/tree.md): shape, flush, batches and spilled values.
- [Summaries](../internals/summaries.md): how a fold is kept on every link, and what it costs.
- [The read path](../internals/read-path.md): memos, prefetching, value columns, scans and cursors.
