# Schema evolution, unique and covering indexes

How the derive macros encode a struct so that it can change, how unique indexes keep their
constraint through inserts, updates and merges, and how a covering index stores fields beside its
entries and what that costs. The API is in [Changing your types](../guide/schema-evolution.md) and
[Tables and indexes](../guide/tables.md).

## Schema evolution

The derive macro writes structs as `(tag, length, payload)` fields, where the tag is a 16-bit FNV-1a
hash of the field name. Fields are sorted by tag, so reordering a struct does not change its hash.
Unknown fields are skipped. Missing fields use `Data::missing`, or `#[ferro(default)]`.
`#[ferro(rename = "old")]` keeps the old tag.

## Unique indexes

`#[unique]` on a field, or `#[unique(a, b)]` on the struct for a combination of fields, adds a
`Map<value, key>` to the generated table, with `by_x(&value)` for the row, `key_by_x(&value)` for
the key alone, and `unique_x()` for the whole map. On an `Option<T>` field `None` is not indexed, so
any number of rows can leave it empty.

A clash cannot be repaired where it is found. `Table::insert` returns the old row and has no error
channel, and the two ways of carrying on are both wrong: last writer wins deletes a row nobody
named, and overwriting the index leaves a row that `by_x` cannot find. So `try_insert` and
`try_update` return `Taken { index, holder }` and leave the table as it was, and `insert`, `update`
and dropping a `RowMut` panic before changing anything. Inside a commit that discards the
transaction, which is the honest result when a constraint the schema states does not hold. A
`RowMut` dropped while its thread is already panicking keeps the old row, since a second panic would
abort the process. A row may replace itself, and a row changing key may keep the value it holds.

`Table::merge` treats a clash as data, not a bug, and calls `resolve` once at most for each key. An
answer is the row for that key: `None` removes it, and a row with another key keeps our row. The
merge holds a clashing row back, and the key keeps its current row and values, until the rest of the
diff is applied, since a chain of renames clashes only on the way through. Held rows then go in
lowest key first as their values come free. A held row taken from theirs that still clashes goes to
`resolve` with `ours` set to the row holding the value, so returning `ours` keeps both of our rows.
Two rows that swap values reach `resolve` too, because neither can move first. A row that `resolve`
has already answered for, because both sides changed the key or because it answered a clash, is not
offered again. If it still clashes when nothing else can move, the key keeps our row. So a merge
never panics on a clash, the indexes always match the rows, and a row is removed only when theirs
removed it or `resolve` returned `None` for its key.

The index field is named `unique_x`, not `by_x`, so turning an `#[index]` into a `#[unique]`
rebuilds it instead of reading the old map with new types. The rebuild on decode covers unique
indexes. If two existing rows share a value it fails with `Error::Decode` naming the index, because
a constraint the data breaks cannot be honoured, so that version does not open until the data or the
attribute changes. The rebuild reads the rows through `table::stored_rows`, which returns a damaged
row node as an error, so `Db::open` fails with it instead of panicking.

## Covering indexes

A plain index is a `Map<(value, key), ()>`. `#[index(cover(a, b))]` makes it a `Map<(value, key),
(A, B)>`, so the fields sit in the leaf beside the entry they belong to, and `#[unique(cover(a, b))]`
makes a unique index a `Map<value, (key, (A, B))>`. The tree's shape depends on its keys alone, so
a covered index has the same nodes as a plain one, each larger by the covered bytes. A field the
index is keyed on may appear in the list. It is read back from the entry's key and not stored, so a
list naming nothing else stores `()` and the map is the plain one, byte for byte.

`IndexIter` holds its entry cursor as a trait object over the key and the projection, so every
lookup of an index, whether by value, range, cursor, prefix or `Some`, shares one implementation.
Rows come through the `Finder` as before, `keys` reads only the keys, and `covered` reads each value
in place in the leaf, clones it and builds the projection with a function the derive generates. It
takes no count on the leaf's value column. `uncovered` wraps the cursor so the iterator's type no
longer names the projection. A plain index passes a projection that is never called.

The index's field tag is a hash of `by_x cover(a,b)`, the stored fields in order, instead of
`by_x`. A table that adds, drops or reorders a covered field finds its index field missing on
decode and rebuilds every index of that table from the rows, as it does when an index is added, and
the next commit writes the rebuilt maps. The old field is skipped as unknown and not written again.
Naming only the index's own fields stores nothing, keeps the plain tag, and is not rebuilt. A
covering unique index rebuilds under the same check for two rows sharing a value.

Keeping the entries costs a comparison. An update that leaves the indexed value and the key alone
but changes a covered field overwrites the entry in place, where a plain index does nothing, and an
update that changes neither touches nothing. An `#[index(each)]` entry is rewritten for every
element when a covered field changes. `Table::merge` and sync move rows and let the indexes follow,
so they need nothing more, which `covered_merge.rs` checks against a plain-indexed twin.

### What it measured

`ferro-bench --bin scaling`, described under [The benchmark](read-scaling.md#the-benchmark), reads
its 200,000-row table and a twin whose `tier` and `balance` indexes both cover `balance`. The lookup
of about 20 rows by a `balance` range covers only its own field, so its index is the plain one, and
the page of 200 rows by `tier` stores the balance beside each entry. Operations a second with no
writer, medians of five rounds, and in brackets the speedup over one thread:

| Workload | Cache | Index | 1 thread | 8 threads | 32 threads |
| --- | --- | --- | --- | --- | --- |
| Index lookup | 1 GiB | plain | 219k | 1.56M (7.1) | 2.83M (12.9) |
| Index lookup | 1 GiB | covered | 2.40M | 10.6M (4.4) | 14.4M (6.0) |
| Page of 200 rows | 1 GiB | plain | 179k | 816k (4.6) | 957k (5.4) |
| Page of 200 rows | 1 GiB | covered | 854k | 3.03M (3.5) | 3.29M (3.8) |
| Index lookup | 16 MiB | plain | 13k | 68k (5.2) | 95k (7.3) |
| Index lookup | 16 MiB | covered | 2.07M | 9.86M (4.8) | 13.9M (6.7) |
| Page of 200 rows | 16 MiB | plain | 119k | 547k (4.6) | 688k (5.8) |
| Page of 200 rows | 16 MiB | covered | 832k | 3.37M (4.1) | 3.48M (4.2) |

Beside a writer committing balance updates as fast as it can, with the 1 GiB cache and medians of
three rounds, lookups went from 107k, 1.12M and 2.31M a second at 1, 8 and 32 threads to 1.50M,
10.1M and 15.1M, and pages from 168k, 769k and 939k to 858k, 3.12M and 3.46M. A covered page runs
29,000 instructions where reading its rows ran 80,000, and a covered lookup 5,000 where it ran
28,000. With the 16 MiB cache a plain lookup reads most of its rows from disk, while the index
alone stays cached. Covered pages scale less across threads than plain ones because what is left of
a page is mostly the shared count each thread takes on the same 25 index leaves, as [What is
left](read-scaling.md#what-is-left) describes.

Writes come from `ferro-bench --bin covered`, which builds 200,000 rows four ways, compacts each
file to its last version, then times 2,000 commits of each kind with `Durability::Buffered`. Every
variant but the first also keeps `#[index] balance`, covering its own field where `tier` is
covered, which stores nothing. Bytes are file growth per commit and CPU is the committing thread's,
medians of five rounds:

| `tier` index | File | Indexes | Balance update | Name update | Insert |
| --- | --- | --- | --- | --- | --- |
| none, and no `balance` index | 7.27 MiB | | 4,100 B, 33.5 us | 4,105 B, 31.9 us | 3,159 B, 20.6 us |
| `#[index]` | 11.17 MiB | 3.90 MiB | 7,669 B, 59.5 us | 4,169 B, 31.8 us | 7,517 B, 49.8 us |
| `cover(balance)` | 11.83 MiB | 4.56 MiB | 10,360 B, 76.0 us | 4,169 B, 31.9 us | 7,809 B, 52.0 us |
| `cover(name, email, balance)` | 15.13 MiB | 7.86 MiB | 11,791 B, 93.1 us | 8,283 B, 81.7 us | 9,198 B, 71.0 us |

Covering the balance grew the indexes 17%, and a commit that changed a balance wrote 35% more and
took 28% more CPU, because the `tier` entry is rewritten where the plain index left it alone. A
commit that changed nothing covered cost the same, and an insert 4% more. Covering two strings as
well doubled the indexes, and a name change then cost twice the bytes and 2.6 times the CPU.

In `bench-tables`, booking-style commits on the example's model with its customer index covering
six fields ran 5% more instructions for a hold, 10% for a confirm and 11% for a cancel, since a
confirm and a cancel change the covered status, and wrote 8.9% more per write. The same commits on
the plain model ran the instructions they did before covering existed, 611k, 747k and 924k, within
0.3%, and so did the plain workloads in `scaling`. Index lookups at 32 threads with the 1 GiB cache
fell into the two modes described under [Iterator frames](read-scaling.md#iterator-frames) in both
builds, with medians of 2.54M and 2.57M a second over eight alternating runs. The booking example's
own numbers are under [Example apps](../guide/examples.md#booking).

The feature stays. A page or lookup that needs only a few fields runs 3.4 to 11 times as fast with
everything cached and over 100 times as fast when the rows do not fit, and the cost lands only on
commits that change a covered field, by about a third. That pays for fields a hot page shows and
that seldom change, which is what it is for. It does not pay for a field rewritten on most commits
of a rarely read index, or for wide strings, and the guide says so.

### Left out

- A generated projection struct per index. It needs a name per index and the derives its fields may
  not have, where a tuple in list order needs neither. Its one gain, that reordering two fields of
  one type cannot swap them silently, is left to the reader of the list.
- `by_x_covered`, `by_x_range_covered` and the rest as separate methods: six more per index, where
  `.covered()` on the iterator every lookup already returns gives them all.
- Rebuilding only the index whose cover list changed. The decode rebuilds every index of the table,
  as it does when an index is added, and this was not measured.
