# Schema evolution, unique and covering indexes

How the derive macros encode a struct so that it can change, how unique indexes keep their
constraint through inserts, updates and merges and list their rows by range, how a covering index
stores fields beside its entries and what that costs, how a table knows which stored indexes it can
read, and how lookups on several indexes combine. The API is in [Changing your
types](../guide/schema-evolution.md) and [Tables and indexes](../guide/tables.md).

## Schema evolution

The derive macro writes structs as `(tag, length, payload)` fields, where the tag is a 16-bit FNV-1a
hash of the field name. Fields are sorted by tag, so reordering a struct does not change its hash.
Unknown fields are skipped. Missing fields use `Data::missing`, or `#[ferro(default)]`. Tags must
strictly ascend, and a field the type knows must decode to its end, as [One encoding for each
value](storage.md#one-encoding-for-each-value) describes; a table's index fields share one tag.
`#[ferro(rename = "old")]` keeps the old tag. A table's indexes are not told apart by tag, as
described under [Index fingerprints](#index-fingerprints).

A tag identifies a field only among the fields stored beside it. The derive refuses two fields, or
two variants, of one type with the same tag, but it cannot see a field or variant that was removed,
whose bytes stay in old data. A field added later whose tag is the removed one's, one in 65,536,
reads those bytes: `title_score: u32` followed by `flags: u32` gave every old row its score as
flags, and a new variant `Tax(u32)` decoded old `RefundPremium(9)` values as `Tax(9)`, where an
unknown variant tag fails to decode. `#[ferro(removed("title_score"))]` on a struct lists the stored
names it no longer has, on an enum its removed variants, and on a variant its removed fields. The
derive refuses a current field or variant whose tag is that of a listed name, with an error at both,
and a listed name the type still has. The list changes nothing in the encoding or the description,
so adding it rebuilds no index.

Widening tags was considered and left out. Thirty-two bits would make a collision with a removed
name one in four billion rather than make it impossible, and would change the encoding of every
struct, so every stored hash, every index fingerprint and every file. Storing the names themselves
costs their length in every row. A list of removed names costs nothing at run time and turns the
collision into a compile error, at the price of the user keeping the list.

## Unique indexes

`#[unique]` on a field, or `#[unique(a, b)]` on the struct for a combination of fields, adds a
`Map<value, key>` to the generated table, with `by_x(&value)` for the row, `key_by_x(&value)` for
the key alone, `by_x_range(range)` and a composite's `by_a_b_prefix(&a)` for rows in value order,
as described under [Ranges and prefixes](#ranges-and-prefixes), and `unique_x()` for the whole map.
On an `Option<T>` field `None` is not indexed, so any number of rows can leave it empty.

Whether a field is an `Option` is decided by its type, not by how it is written. The derive cannot
see through `type Code = Option<u32>`, so it used to key the index of a field written `Option<T>`
by `T` and index the aliased field as a plain value, `None` included: two rows without a code
clashed, and a file written under one spelling failed to open under the other (#125). Now the
index is a `Map<Option<T>, Key>` however the field is written, the generated code leaves out a
value whose `Data::absent` is true, which only `None` is, and the fingerprint takes the kind from
`Data::OPTIONAL`. What the index holds, the clashes it finds and its fingerprint are the same for
`Option<u32>`, an alias, `Box<Option<u32>>` and a `#[ferro(transparent)]` wrapper. The lookups
still follow the spelling, since naming `T` needs the syntax: on a field written `Option<T>`,
`by_x`, `key_by_x`, `covered_by_x` and `by_x_range` take a `T` and find `Some` through
`Map::range_by`, and on any other spelling they take the field's own type. The new layout has its
own kind, unique sparse, so a file written before it rebuilds each unique `Option` index once, the
first time it is read.

A clash cannot be repaired where it is found. `Table::insert` returns the old row and has no error
channel, and the two ways of carrying on are both wrong: last writer wins deletes a row nobody
named, and overwriting the index leaves a row that `by_x` cannot find. So `try_insert` and
`try_update` return `Taken { index, holder }` and leave the table as it was, and `insert`, `update`
and dropping a `RowMut` panic before changing anything. Inside a commit that discards the
transaction, which is the honest result when a constraint the schema states does not hold. A row may
replace itself, and a row changing key may keep the value it holds.

A load can also fail partway through a write, reading the file or in a `Data` impl that panics,
after the rows or an index have changed, which would leave a row that its indexes do not list. So a
table write first works out every index entry it changes, and for an index that only loses an
entry, or changes more than two, loads in place every node those edits will load, which changes no
entry. Then it adds the lone entry of each other index, loading next for the entry a changed value
removes beside it, and writes the rows, each in one edit or batch that a failure leaves as it was.
If one fails it takes back the entries it added: an entry just added has its path in memory and two
new halves beside it, so removing it loads nothing. Last it makes the removals and batches, which
have nothing left to load. An insert writes its row first,
since it learns the row it replaces from that, and takes it back the same way. A row under a new
key, in a table without an `each` index, gives each index at most one entry and removes none, so
the insert puts each entry in as it goes and takes back those before one that fails, working their
keys out again from the row. `retain`,
`take_range` and `remove_range` remove their rows as one batch and each index's entries as another.
`take_range` builds the indexes of the table it returns first, in one write of that table, and only
then cuts the rows out of its own, so a load that fails in either leaves the table as it was.
`Table::try_merge`, which writes as it goes, puts back the copy of the table it keeps for an `Err`
from `resolve` on a panic too. [Edits that fail halfway](tree.md#edits-that-fail-halfway) describes
what a map loads ahead.

A clash is found in the index alone, which every write trusts to hold exactly the entries the rows
give. Writes keep it so, and `receive` and `verify_typed` check every table they read against its
rows, as described under [Indexes that agree with their
rows](sync-receive.md#indexes-that-agree-with-their-rows). Before that check a peer whose type set
index maps apart from its rows could land a table whose unique index held a value for a row that
did not hold it, and `insert` of a row with that value, which no row held, panicked as a clash
(#112). Only a damaged file can hold such a table now, and `verify_typed` reports it as
`Error::Corrupt`; a receive refuses a table holding an index under a fingerprint its type does not
know, which a later schema declaring that index would otherwise trust unchecked (#115). The panic
is kept for it rather than turned into an error. `insert` has no error
channel, and telling a damaged index from a real clash would cost each write that finds a value
taken a read of the holder's row, for a file that `verify_typed` already flags. Lookups on such a
table miss rows or return others, as they would on any damaged node.

A panic in the closure `update` runs, or while a `RowMut` is held, writes nothing back, so the table
keeps the old row and its index entries rather than a row changed half way. `update` runs the
closure on a copy and writes it only once the closure returns, as `try_update` does, and a
`RowMut` notes whether its thread was panicking when it was made and drops the copy if a panic has
started since. A `RowMut` made during unwinding, in a `Drop`, writes its row as usual, except that
a clash keeps the old row, since a second panic would abort the process. Measured on 2026-11-07 on
a table of 1,000 rows with one index, an `update` that leaves the index entry alone took 119 ns
against 125 ns when it went through a `RowMut`, one that moves the entry 373 ns against 370 ns, and
a change through `get_mut` 127 ns either way.

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
removed it or `resolve` returned `None` for its key. `merge_sequences.rs` runs two tables through
random inserts, updates, key changes, drains and merges both ways, with every kind of unique and
covered index and random answers from `resolve`, and checks after each step that no value of a
unique index is held twice and that every index has the root hash of one rebuilt from the rows.

The kind of an index is part of its fingerprint, so turning an `#[index]` into a `#[unique]`
rebuilds it instead of reading the old map with new types. The rebuild on decode covers unique
indexes, and checks the ones it rebuilds. If two existing rows share a value it fails with
`Error::Decode` naming the index, because a constraint the data breaks cannot be honoured, so that
version does not open until the data or the attribute changes. The rebuild reads the rows through
`table::stored_rows`, which returns a damaged row node as an error, so `Db::open` fails with it
instead of panicking.

### Ranges and prefixes

`by_x_range(range)` and, on a composite, `by_a_b_prefix(&a)` return an `IndexIter`, so the rows of a
unique index come with `keys`, `covered`, `uncovered`, both ends, a length known at once and a place
in `and` and `or`, as a non-unique index's do. The derive builds the walk itself, with `Map::range`
on the index for a range and `Map::range_by` on the value's first field for a prefix, and hands the
`Iter` to `table::unique_rows`, or `unique_rows_covered` with the projection `covered_by_x` uses,
together with a function that gives the value a row holds, which a combination uses to test a row
against the lookup, as described under [Ranges and prefixes in a
combination](#ranges-and-prefixes-in-a-combination). Rows come in value order, and a composite's
prefix by the fields after the first. The bounds borrow as the value type does, so a `String` index
takes `&str` bounds, and on an `Option<X>` field they are bounds of `X`, since `None` has no entry.

The cursor behind every other lookup reads the primary key out of a `(value, key)` entry's own key
and reads the leaf's values only for `covered`. A unique index keeps the key in the value, so its
lookups put a cursor of their own in the `IndexIter`'s box. `Holders` steps an `Iter<V, Key>` and
clones the key it reads in place from the leaf's values. `CoveredHolders` steps an `Iter<V, (Key,
S)>` and clones the key alone for `keys` and for rows, or the key and the stored fields for
`covered`. A value whose encoding passes 512 bytes is stored apart from the leaf, as [Spilled
values](tree.md#spilled-values) describes, so a cover list whose fields pass that loads the value
for every key it yields, `keys` included. The rows are read through the finder every other lookup
uses, which keeps the last leaf it found.

A range of a unique index is not in key order, so it joins a combination as a range of a
non-unique one does: a much smaller side tests its keys against it, and otherwise its keys and
covered fields are read in full and sorted. The `find()` view has no unique lookups. `key_by_x`
gives the one key of a value, and a range in the view would be tested or collected all the same,
which passing the `IndexIter` to `and` or `or` already does. There is no `by_x_after` or
`by_x_counts`, since a value has one row: the page after a value is a range that excludes it.

`unique_range.rs` checks every range over a few bounds, inverted and empty ones among them, and
every prefix of six unique indexes, plain and covering, on `String`, integer and `Option` fields and
on composites, against a `BTreeMap` of random rows, read as rows, keys and covered fields from
either end and from both at once, in memory and from the file, and in `and` and `or` with a `find()`
lookup.

### What ranges and prefixes cost

The lookups that existed are the code they were. `IndexIter::over` and the entry cursor did not
change, and the new lookups are generic methods, which produce no code in a crate that does not
call them. Built before and after, the `bench-tables` and `bench-combine` binaries disassemble to
the same instructions in every function, apart from addresses, padding, the `TypeId` constants that
change with the crate's hash and the names of anonymous constants. Over six alternating runs of
each build, `bench-tables`, whose booking model has unique indexes that now get the new lookups
unused, ran a hold in 639,954 instructions against 640,190 before, a confirm in 823,700 against
823,692 and a cancel in 1,019,316 against 1,018,913, medians, and wrote the same 13,362 bytes a
commit. Its wall times came within 1% of the build before, run by run. The 156 cases and strategies
of `bench-combine` ran within 0.22% of the instructions they ran before. Their times moved with the
load of a machine shared with other work, by more than twice between runs of one build, and the
median ratio of each run to the run of the other build beside it was 0.984 over those six runs and
1.031 over four earlier ones.

The new lookups cost less than the walk they replace. A program written for the measurement built
200,000 rows, whose 10,000 rooms hold about 20 nights each under keys scattered across the table,
and read them after a compaction with the 1 GiB cache. `by_room_night_prefix(&room)` read a room's
rows in 3.0 to 3.3 µs over two runs, where walking `unique_room_night().range((room, 0)..=(room,
u32::MAX))` and calling `get` with each key took 3.7 to 3.9 µs. It ran 30,600 instructions against
26,700, in less time, as the finder does for rows scattered across a table, described under [Sorted
lookups and cursors](read-path.md#sorted-lookups-and-cursors). Its keys alone took 333 to 355 ns and
5,265 instructions, against 477 to 519 ns and 6,531 for the map's `(value, key)` pairs, which clone
each value and a `Val` of each key. On a covering `u64` index, 20 entries by `by_reference_range`
took 3.3 to 3.6 µs as rows where the map and `get` took 3.8 to 4.6 µs, and 367 to 419 ns through
`covered` where mapping the map's pairs to the same fields took 446 to 452 ns.

## Covering indexes

A plain index is a `Map<(value, key), ()>`. `#[index(cover(a, b))]` makes it a
`Map<(value, key), (A, B)>`, so the fields sit in the leaf beside the entry they belong to, and
`#[unique(cover(a, b))]` makes a unique index a `Map<value, (key, (A, B))>`. The tree's shape
depends on its keys alone, so a covered index has the same nodes as a plain one, each larger by the
covered bytes. A field the index is keyed on may appear in the list. It is read back from the
entry's key and not stored, so a list naming nothing else stores `()` and the map is the plain one,
byte for byte.

`IndexIter` holds its entry cursor as a trait object over the key and the projection, so every
lookup of an index, whether by value, range, cursor, prefix or `Some`, shares one implementation.
Rows come through the `Finder` as before, `keys` reads only the keys, and `covered` reads each value
in place in the leaf, clones it and builds the projection with a function the derive generates. It
takes no count on the leaf's value column. `uncovered` wraps the cursor so the iterator's type no
longer names the projection. A plain index passes a projection that is never called. A unique
index's range and prefix put a cursor over its map in the same box, as described under [Ranges and
prefixes](#ranges-and-prefixes).

The trait object costs a call through its vtable for every entry unless the compiler can see which
cursor the box holds, which it can when the lookup is built in the function that reads it. So the
generated lookup methods are `#[inline]`, the `index_*` functions behind them and `IndexIter::over`
are `#[inline(always)]`, and the cursor's methods and the iterators' `next` and `next_back` are
`#[inline]`. They used to be plain generic functions, which rustc compiles into one codegen unit of
the crate that uses them, so whether a read loop could inline them depended on where each landed.
Commit 2ba667f added `same_stored` and its helpers to the `ferro::table` functions that `scaling`
compiles. At its parent, the objects `-C save-temps` keeps show `index_range` and `IndexIter::over`
inlined into the benchmark's read loop and the cursor's methods in the loop's unit. After it, rustc
put all of them in one unit with `same_stored`, apart from the read loop, and every covered row went
through the vtable: a page of 200 took 31,870 instructions where it had taken 29,850, and ran 11%
fewer pages a second (#58), with no change to the read path. Inlined, a covered page takes 29,850
instructions again and a covered lookup 5,054 against 5,248, in `scaling` and in a separate program
that builds the same lookups in a function of its own, where the hints alone had left
`IndexIter::over` out of line. Over four runs of `scaling` alternating with the build before, with
the 1 GiB cache, a covered page ran 810,000 a second on one thread against 742,000 and 3.27M at
eight threads against 2.98M, and a covered lookup 2.32M against 2.26M on one thread.

The larger read loop then stopped inlining the drop of each row's `Val`, whose glue held the row's
own drop for a value held by copy, and a plain page took 82,880 instructions against 78,900. A value
is held by copy only when it has no destructor, so it is now held in a `ManuallyDrop`, which drops
nothing either way, and the glue is two reference counts. A plain page takes 79,400 instructions,
as it did when the cursor was last devirtualized, and a `for` loop over table rows 158 a row
against 187. It moved map folds over strings both ways: `iter()` scans in `scaling` take 61
instructions a key against 58, still under the 63 before [Scans](read-path.md#scans) changed, its
100-key ranges 8,612 against 8,288, and a filtered fold 58 against 61.

The covered fields an index stores are part of its fingerprint, in list order under the names their
rows store them by, each with its type. A table that adds, drops or reorders a covered field, or
stores one under another name or type, finds no stored map with that index's fingerprint on decode
and rebuilds that index from the rows, as it does when an index is added, and the next commit writes
the rebuilt map. A field renamed in Rust that keeps its stored name with `#[ferro(rename)]` keeps
the fingerprint. The old map is skipped and not written again. Naming only the index's own fields
stores nothing, keeps the plain index's fingerprint, and is not rebuilt. A covering unique index
rebuilds under the same check for two rows sharing a value.

Keeping the entries costs a comparison. An update that leaves the indexed value and the key alone
but changes a covered field overwrites the entry in place, where a plain index does nothing, and an
update that changes neither touches nothing. A covered field counts as unchanged only when `==`
holds and it encodes to the same bytes, so a float that turns from `0.0` to `-0.0` is rewritten.
The equal-looking case costs a `Data::same_encoding` call, which encodes the field twice only for a
hand-written `Data` impl, as described under [Comparing values](merging.md#comparing-values).

The same test applies to a covered field the index is keyed on, which `.covered()` reads from the
entry's key. A type whose `==` holds between values that encode differently, such as a name
compared without case, would otherwise keep the bytes the entry was first written with, so
`.covered()` gave the old spelling while the row and a rebuilt index gave the new one, and the
index's root hash differed from a rebuild's. The entry is removed and inserted again under the new
bytes. An indexed field that no cover list names is compared by `==` alone, which is all its
lookups need, so its entry keeps the old bytes, which `by_x_counts` returns. Such a type breaks
the rule that an indexed value's encoding agrees with its `Ord`, as described under [Changing your
types](../guide/schema-evolution.md), but what `.covered()` returns no longer depends on how the
rows got there. `covered_model.rs` checks a value, a composite and a self-covering index against a
rebuild after such an update.

An `#[index(each)]` entry is rewritten for every element when a covered field changes. `Table::merge`
and sync move rows and let the indexes follow, so they need nothing more, which `covered_merge.rs`
checks against a plain-indexed twin.

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
confirm and a cancel change the covered status, and wrote 8.9% more per write. In
[Performance](../guide/performance.md), a hold and a confirm took 86 and 106 µs of CPU on
2026-11-05, 4 and 14% more than the 82 and 93 µs measured on 2026-11-03, before the example's
customer index covered its bookings. The same commits on the plain model ran the instructions they
did before covering existed, 611k, 747k and 924k, within 0.3%, and so did the plain workloads in
`scaling`. Index lookups at 32 threads with the 1 GiB cache fell into the two modes described under
[Iterator frames](read-scaling.md#iterator-frames) in both builds, with medians of 2.54M and 2.57M a
second over eight alternating runs. The booking example's own numbers are under [Example
apps](../guide/examples.md#booking).

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

## Index fingerprints

A covered index once learned that its cover list changed only because the field tag it looked for,
a 16-bit hash of `by_x cover(a,b)`, was missing from the stored table. Three things slipped through.
Two lists could hash to the same tag, one change in 65,536, and `cover(total)` read as `cover(price,
speed)` panicked while `cover(size_id)` read as `cover(length_v2)` returned the old field's values.
`#[index] tags` and `#[index(each)] tags` used the same tag over maps of different key types. And
the keyed part of the tag hashed Rust names, so giving an indexed field another stored name kept a
stale index, while renaming it in Rust rebuilt one that was fine. The tag was standing in for an
identity it did not capture.

### What the fingerprint covers

Each index is identified by a 64-bit fingerprint of a description of what decides its encoding and
meaning:

- its kind: value, option for a value index on an `Option`, composite, each, unique, unique sparse
  for a unique index on an `Option`, which leaves `None` out, or unique composite. Whether a field
  is an `Option` comes from its type, through the hidden `Data::OPTIONAL`, which `Option` sets and
  `Box`, `Arc` and `#[ferro(transparent)]` types take from what they hold, not from how the field
  is written, so `Option<u32>`, an alias of it, `Box<Option<u32>>` and a transparent wrapper of it
  give one fingerprint;
- the key's type;
- the stored names of the fields it is keyed on, in order, each with its type;
- the stored names of the covered fields it stores, in list order, each with its type. Covered
  fields the index is keyed on come from the entry and are left out, so a list naming only them
  describes the plain index;
- for each of those fields that has `#[ferro(default)]` or `#[ferro(default = "...")]`, the hash
  of the value the default returns, as described under [Defaults](#defaults).

Rust names take no part. The generated field and methods keep them, so `by_amount` on a field
stored as `total` has the fingerprint `by_total` had, and a Rust-only rename rebuilds nothing.

A type is described by `Data::describe`, which follows the encoding:

- Primitives, `String` and `Bytes` by name. `Option`, `Vec`, arrays, tuples, `BTreeMap`,
  `BTreeSet`, `Map`, `Set`, `List` and `Table` by their shape and their parameters. `Box<T>` and
  `Arc<T>` as `T`, since they encode as `T`.
- A derived struct by its stored fields in tag order, each name with its type and, for a field with
  a default, the hash of the value the default returns, and a derived enum by its variants the same
  way. The struct's Rust name, the order its fields are declared in and its skipped fields take no
  part. A transparent struct is described as the field it wraps.
- A type met again inside its own description, directly or through others, by how many types up the
  first one is, so `struct Tree { kids: Vec<Tree> }` has a finite description.
- Anything else by `std::any::type_name` and, if its `Data::missing` returns a value, the hash of
  that value, as described under [Defaults](#defaults), unless its `Data` impl overrides
  `describe`.

The derive's code names a field's type, and the compiler resolves it, so `type Amount = i64`
describes as `i64`, and pointing the alias at another type changes the fingerprint. The description
is a byte string in which every word carries its length and every count a marker, so two different
descriptions cannot be the same bytes. The fingerprint is the first eight bytes of its BLAKE3 hash
in derive-key mode, under a context string of ferro's own.

The fingerprints of a table type are computed the first time it is encoded or decoded and kept in a
`OnceLock` for the life of the process. Decoding then compares eight bytes per index and hashes
nothing.

### Defaults

A row written before a field existed reads that field as its default, and an index keyed on the
field, or covering it, holds what the default returned when the index was built. Pointing
`#[ferro(default = "...")]` at a function that returns something else changed no fingerprint, so
the index kept the old value for those rows: a lookup by the new default missed them, a lookup by
the old one found rows that no longer read as it, and an update of such a row removed an entry
under the new value that was never there and left the old entry behind for good.

The description of a field with a default now goes on with the word `default` and the BLAKE3 hash
of the default value's encoding, marked so that it cannot be read as a word or a count. The value
comes from calling the function, or `Default::default` for `#[ferro(default)]`, once, when the
fingerprints are computed. Changing what a default returns then rebuilds every index keyed on or
covering that field, the next time the table is read, as any other change of layout does, and the
next commit stores the rebuilt map. A field without a default is described as before, so an index
keeps its fingerprint unless one of its fields has a default, or its type holds a struct or enum
with a defaulted field, and those indexes are rebuilt once after upgrading.

A default nested in the type of an indexed or covered field counts the same. With `#[index] meta:
Meta`, where `Meta` gained a `level` with a default, a row written before `level` existed reads it
as the default, and the index holds whatever `level` read as when it was built. The derive's
`Data::describe` of a struct or an enum variant describes each field's default as the fingerprint
does for the row's own fields, so the description of `Meta`, of `Option<Meta>`, of `Vec<Meta>` or of
any type holding it changes with the default, at any depth, and the index is rebuilt. A struct's
description held only names and types before, so pointing that default at another value kept the
fingerprint: rows read the new level while lookups and covered values gave the old one.

A field without a default reads as its type's `Data::missing` when stored data lacks it, and that
value can change too. ferro's own types return a fixed empty value there, or `None`, and describe
themselves; a derived struct or enum returns `None`, and a transparent one follows the field it
wraps. A hand-written impl can return anything, and the default `describe` held only its Rust name,
so one whose `missing` went from 1 to 2 kept its fingerprint, at the top level or nested in a
derived struct, and the index kept 1 for the rows that read 2. The default `describe` now goes on
with the word `missing` and the hash of the value when `missing` returns one, so that change
rebuilds the index as a changed default does. A type whose `missing` returns `None` is described as
before, and so is every type ferro provides and every derived one, since they override `describe`.
After upgrading, the only indexes rebuilt are those whose description reaches a hand-written impl
that returns a value from `missing` and keeps the default `describe`, once each. An impl that
overrides both must describe the value itself, as the `Data` docs say: ferro cannot add it there
without changing the description of every `Option`, `Vec` and collection, which return their empty
value, and so rebuilding every index that holds one.

The value is described rather than the path of the function. A path misses a function whose body
changed, and a `#[ferro(default)]` whose type's `Default` changed, and it rebuilds an index when the
function is moved or renamed but returns the same thing. The value changes exactly when the index
would hold something else. It assumes the default returns the same value every time. One that does
not, such as the current time or a fresh id, gives the index another fingerprint in every process,
so the index is rebuilt each time the program starts. Such a default was never coherent on an
indexed field, since every read of a row that lacks the field gives another value.

Which rows still lack the field depends on history, index or no index. A commit that rewrites a leaf
of the rows map encodes every row in it, so its neighbours store the default they read as, and a
later change of default reaches only the rows whose leaves were never rewritten. The rebuild makes
the index agree with the rows as they read now, whichever those are.

Handling the rows that lack the field another way was considered and left out:

- Marking decoded rows whose field was missing. A decoded row does not know which of its fields were
  read and which defaulted, and recording it would cost every row a flag for the benefit of rows
  written before one schema change.
- Writing the default into those rows when an index is built. The index is often built while
  reading an old version, through `at`, history or a merge base, and rewriting rows there would
  change the root hash of a version nobody changed, which merges and sync would then see as a
  change.
- Storing the default value beside the index. It would detect the same changes as the eight bytes of
  fingerprint, at the cost of the value's length in every commit.

`index_fingerprint.rs` reads rows written without a field through a type whose default for it
returns 1, then through one whose default returns 2, and checks that the index keyed on the field
and the index covering it are rebuilt and agree with a rebuild, and that a third type whose default
is another function returning 2 keeps them. A pair of transparent types whose `Default` impls differ
checks `#[ferro(default)]` the same way. A row whose indexed and covered fields hold a struct, an
`Option` of an enum whose variant has the defaulted field, and a `Vec` of the struct, is read the
same three ways, and every one of its four indexes is rebuilt when the nested default changes and
kept when it does not. Rows written without a field of a hand-written type whose `missing` reads a
static are read through a table with three indexes, one keyed on the field, one covering it and one
keyed on a derived struct that holds it, with the static at 1, then 2, then 2 again, and the three
indexes are rebuilt the second time and kept the third. The descriptions of the type, of a derived
struct holding it and of an `Option<Box<_>>` of it change with the static, and a hand-written type
whose `missing` returns `None` is described by its name alone, as before.

### Where it is stored

A table encodes as a struct of fields, as before: its rows under the tag of `rows`, and every index
under the one tag of `indexes`, as its fingerprint in eight little-endian bytes followed by the map.
Index fields are written in fingerprint order, so the order indexes are declared in does not change
the encoding, and since the tag of `indexes` is below that of `rows` they come first. Decode reads
each index field's first eight bytes and keeps the map under the declared index with that
fingerprint. A field shorter than eight bytes, or whose fingerprint no declared index has, is
skipped. Each declared index left without a map is rebuilt: the rows are read once, every such index
is filled from them, unique ones are checked for clashes as they fill, and the indexes that were
found are kept as they are. The rebuild fails on a row stored under a key other than its own, and
reads the elements of an `each` index over a `Set` with `IndexEach::try_values`, so a damaged set
fails the read rather than panicking. A decoder of the check that a table's indexes agree with its
rows rebuilds nothing, as `Decoder::rebuilds` says. `visit`, which `verify_typed` and the check on
receiving a sync use, reads index fields the same way and checks only those it can match, and a
receive refuses a table holding an index field it cannot match, as described under [Indexes that
agree with their rows](sync-receive.md#indexes-that-agree-with-their-rows).

Those checks then hold each index they matched to the rows. The derived `decode` and `visit` note
the table value they read when they run inside such a check, and the check reads each noted table
with the hidden `TableData::decode_stored`, which decodes as `decode` does, the last field of a
fingerprint winning, but rebuilds nothing and says which indexes were stored and whether one was
stored under a fingerprint no declared index has. The derive generates, for each index, an
`__entries_` function that gives the entries the index holds for a row, the same ones `reindex`
writes, returning the error of a set it cannot read, and a `__delta_` function that gives the
entries a row's change takes out and puts in: none when the fields the index names and stores
encode as they did, the elements that left and came of an `each` field, as `IndexEach::delta`
finds them, when only it changed, and all the entries of both versions otherwise. Against the same
table of the version before, `TableData::diff_slots` starts the diff of each stored index, takes
the rows that changed, and checks that each index changed by exactly what their deltas give; a
table with nothing to check against has `TableData::built_slots` gather the entries of all its
rows for each index, and `Differ::verify` sort them and walk the stored map beside them. Indexes
the table does not store under a known fingerprint are left to the rebuild above, as described
under [Indexes that agree with their rows](sync-receive.md#indexes-that-agree-with-their-rows).

Which table of the version before a table is checked against comes from the hidden `Data::pair` and
`Data::tables`, which the `Data` derive writes for each struct and enum, the `Record` derive for
each table type, and ferro for its own containers. `tables` says whether a type's values can hold a
table, and `pair` reads a value's encodings in two versions side by side and pairs the table at each
place, a field, a variant, an element or a key, with the table at the same place before. A table
inside a type whose `Data` is written by hand is never paired, and is checked against all its rows.

That is the whole format change. The tag of an index no longer depends on its name, and its field
holds eight bytes before the map. Nothing global moved: no file format version, and no change to
rows, maps, nodes, commits, or structs that are not tables. A table without indexes encodes as it
did, byte for byte.

A file written before fingerprints keeps each index under the tag of its old name. Decode does not
know those tags, so it skips them, rebuilds every index once, and the next commit writes them with
fingerprints. If an old name happens to hash to the tag of `indexes`, one in 65,536, its field is a
map's count and link, shorter than eight bytes or starting with bytes no fingerprint matches, and is
skipped all the same. `tagged-indexes.db`, written by the version before this change, checks that
value, option, each, composite, unique and covered indexes all come back equal to a rebuild, in the
newest version and in the first, and that the commit after stores them. A version written before
fingerprints, read later through `at`, history or a merge base, rebuilds its indexes on every read,
as any version written under another layout already did. An older ferro opening a newer file finds
none of its index tags and rebuilds its indexes, unless one of its index names hashes to the tag of
`indexes`, when it would read that field's fingerprint as part of a map. Going back is not
supported.

### What it does not see

- How a value compares. The description follows the encoding, so a change to `Ord` with the same
  bytes keeps the fingerprint: reordering the fields of a struct that derives `Ord`, a hand-written
  `Ord`, or a transparent wrapper that orders differently. The entries then sit where the old order
  put them, and lookups miss rows. The values of an indexed field are keys of its index's map, and
  [Changing your types](../guide/schema-evolution.md) already says not to change a key type's
  `Ord` on existing data.
- A hand-written `Data` impl that changes its encoding and keeps its type name. Its description is
  the name. Overriding `describe` fixes that. The name `type_name` gives may also change with the
  compiler, which costs one rebuild.
- A default that returns another value each time it is called, as described under
  [Defaults](#defaults).
- Changes that would have read correctly. The fingerprint changes whenever a description does, so
  `u32` to `u64`, `Vec<u8>` to `Bytes`, or a defaulted field added to a covered struct rebuilds an
  index that decoding would have handled. That costs a rebuild, where missing a change costs wrong
  answers.

### What it costs

Eight bytes per index in the root, which every commit rewrites, and nothing on the read path that
was not there. Both benches gained a timing of the root decode before the change, and each build ran
them twice, interleaved with the other build's runs.

In `bench-covered`, on 200,000 rows with two indexes, each commit wrote 16 bytes more: a balance
update with the plain index went from 7,649 to 7,665 bytes, a name update from 4,168 to 4,184 and an
insert from 8,363 to 8,379, under 0.4%. CPU per commit stayed within the spread of two runs of one
build, which reached 7%. The root grew from 27 to 43 bytes, and the variant without indexes kept its
13 bytes. Decoding that root took 130 and 147 ns before and 133 and 142 ns after, and 143 and 159 ns
against 138 and 142 ns with the widest cover. Opening the file of the plain variant as the one whose
`tier` index covers `balance` took 192 and 201 ms before, rebuilding both indexes, and 110 ms after,
rebuilding `tier` alone. Opening the narrow file as the wide variant went from 206 to 119 and 121
ms.

In `bench-tables`, whose booking model has nine indexes over six tables, the root grew from 183 to
255 bytes and each write from 13,297 to 13,367 bytes, 0.5% more. A hold ran 641,000 instructions
against 638,000, a confirm 823,000 against 822,000 and a cancel 1,023,000 against 1,018,000, at most
0.5% more, and wall and CPU time stayed within 2.5%. Decoding the root took 1,073 ns before and 995
and 1,010 ns after.

### Collisions

A stored index is mistaken for a declared one only if their fingerprints are equal while their
descriptions differ. With BLAKE3 truncated to 64 bits that happens for a given pair with probability
2^-64. Decode only ever compares the declared indexes with the few the last writer stored, so there
is no birthday effect across all the layouts a schema has had: an accidental match needs one of a
handful of pairs to collide, and a deliberate one needs a second preimage, about 2^64 hashes. Two
indexes of one table cannot have the same description, since their Rust names would clash, so equal
fingerprints there would be a hash collision too. `Fingerprints::new` checks for one and panics
naming the table the first time the table is used, so it would show in the first test that touches
it, not in a file.

### Left out

- Recognising indexes written before fingerprints instead of rebuilding them. Their tag is exactly
  the identity the bugs above came from, so trusting it on the first open would bring them back for
  every file that has one.
- A wider fingerprint. Every commit rewrites the root, which holds every index of every table, so
  each byte of fingerprint is paid per index per commit, and 64 bits already put a wrong match far
  below any other failure.
- Storing the description beside the fingerprint, which would detect a collision outright, at the
  cost of its length on every commit.
- A tag per index from its stored names. It would give back 16 bits of discrimination, but two
  indexes of a table could then share a tag, which the derive can only reject, and a user cannot fix
  that without renaming stored fields.

## Combining lookups

`find()`, which the derive generates on a table with a non-unique index, returns a view that borrows
the table. Its methods are the equality lookups of each such index, `by_x`, `by_x_after`,
`by_x_some`, `by_x_some_after` and a composite's `by_a_b`, and each returns a `Matches<T, C>`. `and`
and `or` on a `Matches` take another `Matches` or any `IndexIter`, through `IntoMatches`, and return
a `Matches` over the rows whose keys both lookups hold, or either does. A `Matches` yields rows in
key order from either end, and `keys`, `covered` and `uncovered` work as they do on an `IndexIter`,
with the covered fields of the leftmost lookup.

An index entry is `(value, key)`, so the entries of one value are in key order, and two such lookups
intersect by walking their keys, reading no row until a key is in both. A view lookup holds a clone
of its index map beside the range `range_by` built, so that it can seek within its value later, and
the rows map's finder, so it reads the version it was built from, as any lookup does. A lookup made
on the table, by range, prefix or value, is an `IndexIter` without that map. A range or prefix
lookup that a much smaller side looks keys up in is tested key by key, as described under [Ranges
and prefixes in a combination](#ranges-and-prefixes-in-a-combination). Otherwise it joins a
combination by reading its keys and covered fields into a vector and sorting it, which takes one
pass when the lookup was of one value and is sorted already.

### Two plans

`and` builds a node over its two sides and picks how to run it the first time it is read, from their
lengths. A view lookup knows its length exactly from `range_by`, which counts the entries between
the two ends it finds from the counts every link holds, a range or prefix lookup the same way, a
collected lookup the count of its entries, an `and` the smallest of its sides' and an `or` their
sum.

- Within a factor of four, both sides are walked. Each seeks to the other's key by stepping, and
  after 16 steps it builds its range again from the root, starting at the target, so a long gap
  costs one descent.
- Beyond that, the smaller side leads and steps, and each of its keys is looked up in the larger
  with `Cursor::seek`, or `seek_back` from the back. A cursor climbs only as far as the target
  needs, so a key nearby costs a search of one leaf. When the larger side's next key lies past the
  lead's, the lead seeks to it as above, which skips a run of keys the larger side lacks.

The larger side is only looked up, never consumed, so it keeps nothing between keys but its cursor,
and the lead alone decides when the walk ends. `or` walks every side and yields the smallest key,
once however many sides hold it, and when an `and` looks a key up in an `or`, every side of the `or`
looks it up.

Each walked side holds the next key at each end. When its range runs out at one end it takes the key
the other end holds, so a key that both ends reach is yielded once. A range built again for a seek
ends just before the key the other end holds, or just past the last key that end yielded. A
combination that has yielded rows and is then combined again is first read to the end into a sorted
vector, since a side that is looked up answers for its whole range.

`covered()` reads the fields of the leftmost lookup. Walked, it reads them with each entry, and
looked up, where its cursor stopped. A key it read before `covered()` was called is looked up in the
index again, and a collected lookup keeps the fields of every entry.

When an `and` is looked up rather than walked, it asks its sides in order of their lengths, fewest
first, and stops at the first that does not hold the key. A side that holds a later key answers
with it, and the `and` passes that on as its own answer, though the other sides were not asked about
it: the lead seeks to it and asks again, and a key is yielded only once every side answered with
that key itself. Asking every side until they agreed on a key, as `and` did before, ran 100 keys
against an `and` of 100,000 and 1,000 keys in 388,000 instructions, where stopping at the first side
that lacks the key ran 95,000.

### A chain of `and`

`a.and(b).and(c)` built a node over `a.and(b)` and `c`, so the order it was written in decided the
plan: with `b` the smallest, `a.and(b)` was planned alone, and `c` met it through the smaller of
their lengths, an upper bound. Now `and` called on a combination that is itself an `and` not yet
read adds the new side to that node, and when the side it is given is an `and` not yet read, it
adds each of that node's sides instead, so `a.and(b.and(c))` builds the node `a.and(b).and(c)`
builds. A node holds up to 64 sides, past which `and` builds a node over the 64 and the next. The
node keeps the left side, whose rows and covered fields it yields, apart from the others, which it
only asks about keys, and finds a side by its number through a `match`. It keeps the order of its
plan in an array of 64 bytes, and in the 64 bits of a word whether each side reads the table and
version of its left side.

A node given to `and` has already handed out its finders: each range took the finder of the lookup
it came with, and its left side the one its `Matches` read rows through, which `and` hands it
before taking the node apart. Every side but the left one only answers about keys, so those finders
are all a side needs, and the source numbers its sides carry, counted from that node's own left
side, are never asked: the node that takes the sides in asks its own left side alone which finder
reads a row. The given node's left side joins wrapped in `Blind`, as any side given to `and` is, so
it yields no covered fields. Each side keeps its flag only when the given node reads one table at
one version, the one the receiving node's left side reads, since a node records for each side
whether it reads its own left side's table and version and nothing else. A node already planned or
ordered is not taken apart, and one read from has already become a sorted list. `Blind`, which
`uncovered` wraps around a node, passes both the joining and the taking apart to the node inside.

Its plan comes from the sides' lengths when it is first read. They are sorted, fewest first; the
first leads, and a side with less than four times the lead's keys is walked beside it, as two sides
within the factor of four are. The others are looked up, in that order, each only for a key that
every walked side and every side before it holds, and when one answers with a later key, the lead
seeks to it. Two walked sides run the merge of two sides and one walked side with the first looked
up runs the lead of two, so the hot loop is the code of the two-sided node; three or more walked
sides leapfrog, each seeking to the key the one before it found. With two sides the node plans as it
did, through the same functions.

A range or prefix lookup in a chain is weighed against the keys that reach it rather than against
the lead's. Each walked side after the lead scales the keys left by its share of the table's rows,
as if the sides matched independently, and a range with at least four times the estimate is tested
against what the walked sides agree on, the rest of its test settled by the count of what the tests
cost, below. An equality lookup is weighed against the lead's keys alone, since looking one up for
more keys than estimated has no such bound.

### Who reads a range's rows

A tested range reads rows through a finder of its own. The one its `IndexIter` came with reads the
combination's rows when the range leads it through `into_matches`, so such a range was collected
whatever the other side held. `and` now hands the finder of the side it is given, when no stream on
that side took it and both sides read one version of one table, to a range on the left that has
none, and a plan whose lead is a range hands the lead's finder, which a walked range no longer
needs, to a range it looks keys up in. A walked `or` hands on the finder of any range among its
sides, all of which it walks, and a walked `and` plans itself first and hands on that of a range its
own plan walks, so a range written before `r.or(x)`, where `r` took that side's finder, borrows
`r`'s when that side is walked, and one written before `r.and(s)` when `r`, which joins the same
chain, is. The finder travels as a `Spare`, which a stream takes without asking its source number,
so `and` offers it only between sides it compared with `Finder::same_map` when it was built. Since
that comparison costs a few dozen instructions, `and` makes it only when a side holds a range not
yet read.

`Matches::explain` walks the streams without changing them and reports, for each, its length and how
it will be read: walked, looked up, tested or collected first. A node not yet planned reports the
plan it would choose and the finders it would hand over, from the same lengths and functions.

### Ranges and prefixes in a combination

A range or prefix lookup is not in key order, so it cannot be walked beside another side, but
whether it holds a key can be tested. For every index the derive generates a function, of the type
`table::Indexed`, that calls a closure on each value the index holds for a row: the field, a
composite's fields as a tuple, each element of an `#[index(each)]` field, and nothing for a unique
`Option` field that holds `None`. The helpers behind the range and prefix lookups, `index_range`,
`index_where`, `unique_rows` and `unique_rows_covered`, wrap their cursor in one that keeps that
function, and `into_matches` on such an `IndexIter` returns a stream that can either test keys or
collect its entries. The equality lookups made on the table, and a lookup passed through
`IndexIter::uncovered`, keep the plain cursor and are collected as before.

The stream chooses when the combination is first read. When `and` picks the plan in which one side
leads, it tells the other side how many keys the lead holds at most, or in a chain how many the
walked sides are estimated to agree on, and an `and` or `or` that is itself looked up passes that on
to each of its sides. A range with at least four times as many entries left as that is tested. One
with fewer is collected, and so is one that is walked, which happens within the factor of four and
when it leads. To test, the stream reads the
first and last entries the range has left without stepping past them: `Iter::front_key_with` and
`back_key_with` take a step and set the frame back. A key is in the range when its row holds a
value whose entry lies between those two. A non-unique entry is `(value, key)`, so the key is
compared as well when the value equals that of an end entry, and a unique entry is the value alone.
An index holds an entry for exactly the values its rows hold at the same version, so the test
answers as reading the entries between the two would, whether or not the range was read partly from
either end first.

The row is read through the finder of the lookup's own `IndexIter`. A combination keeps the
finders it reads rows through, one for each version of a table its lookups read, as [Lookups of
several tables](#lookups-of-several-tables) describes. `and` hands the finders of the side it is
given to that side's streams, since it reads no rows through them, and `or` hands over those that
read a version it already has a finder for, and each range that came without a finder takes the one
of its own version. A tested range so reads the table it was made on as it was when the range was
built, whatever table or version the other side reads. A range that leads a combination through
`into_matches` has no finder of its own, since the combination reads its rows through it, and
borrows one from the other side, as [Who reads a range's rows](#who-reads-a-ranges-rows)
describes, or is collected.

A probe of a tested range answers with the key when the range holds it, and otherwise with a new
answer, `Past(key)`, meaning that it holds nothing from the target up to that key. The lead then
moves to its first key after that one, through `pass`, and probes again, so each key of the lead
costs one row read. `pass` and `seek` share one function per stream with the strictness as a
constant, so the code `seek` compiles to is what it was. An `and` that is looked up answers `Past`
as soon as either side does, and an `or` answers with the nearest key a side holds unless a side
that answered `Past` may hold an earlier one. The last answer is kept, since `take` settles again on
the key that `fill` settled on.

`covered()` reads the fields of the leftmost lookup, which a range is only when it leads, so a
tested range never yields covered fields. A combination that has started and is combined again
reads its covered fields with its keys, and a range it was testing is then collected from the
entries it has left, which testing never stepped past.

A test costs one row read, and what that costs depends on where the row is. With the row cached
a test costs a few collected entries, but read from the file, with its spilled value, it costs
dozens. So the store counts, for each thread, the objects it reads from the file to decode, past
the cache, and the stream reads that count before and after each test. Under `keys()` and
`covered()`, which yield no rows, every such read counts, since collecting would have read no
row. When rows are yielded only the reads of keys the range does not hold count, since a row that
matches is read for the yield anyway, and found in the cache after the test. Once the reads
counted, weighted at 64 entries each, pass the number of entries the range had when testing began,
the stream stops testing and collects the entries it has left, which is the rent-or-buy rule: the
tests can cost about as much as the collection would, and no more, before the collection is paid
for. The keys tested count as well, against a quarter of those entries, since a cached test costs
about what four collected entries do. A lead counted exactly, or bounded, never reaches that, as a
range is tested only against a quarter of its entries or fewer, so cached rows stop the testing only
when a chain estimated too few keys. That holds since a range remembers the last key it tested from
each end: an `or` refills every side before it takes from one, so a side waiting on a key at the
front is asked about it again after each key taken from the back. With one remembered key, an `or`
read from both ends tested the same keys over and over, 3,755 tests for 197 keys, until it reached
the quarter and collected. A first build counted both in one sum, four entries a test and 64 a
read: a lead of exactly a quarter then left no room for reads, and 1,000 keys against a range of
4,000 with every row cached ran 47% more instructions, collecting after a stray read.

The test relies on the index agreeing with the rows, as every lookup does: an entry for exactly
the values the rows hold. The derive refuses an index on a `#[ferro(skip)]` field, which rows read
back as its default while the index keeps the values written, and on which the tested and the
collected plans gave different keys. A table whose stored index matches its fingerprint but not
its rows, such as one received from a peer that wrote it so, would answer by plan as well.

`combine_range.rs` checks random combinations of `find()` lookups with ranges and prefixes of plain,
each, `Option`, unique, unique `Option`, covering unique and composite indexes, some read partly
from either end first, against a filter over the rows: as rows, lent rows, keys and covered fields,
from either end and from both at once, in memory and from an uncached file, after the table
changed, inside a commit, and with the range taken from another version of the table. It also
checks every range of a field with 30 values against six leads, and counts the rows tested through
an `IndexEach` that counts its calls: in memory each key of the lead is tested once, through
`uncovered()`, from the lead and within the factor none, and from an uncached file fewer than the
lead holds, as keys and as rows.

### Lookups of several tables

A lookup reads the table it was made on as it was when it was built, and `and` and `or` combine by
key alone, so lookups of two tables of one row type, or of two versions of one table, such as a
snapshot's and the one inside a commit, combine like any others. Their keys are what each lookup's
own table gives. Their rows came through the leftmost lookup's finder, though, so a key that only
another table held found no row there and ended the iteration: an `or` of a table holding 1, 2, 3,
5 and 6 with one holding 1 to 6 gave all six keys but rows 1 to 3, and from the back 6 and 5. An
`and` whose left side was such an `or` stopped the same way, and with two versions it could yield
the leftmost version's row of a key that another version's lookup had matched.

A combination reads each key's row from a lookup that holds the key: from the left side of an
`and`, which holds every key the `and` yields, and from the leftmost side of an `or` that holds it.
That is the lookup whose covered fields `covered()` already gave, so a row and its covered fields
come from one version, and rows, lent rows, keys and covered fields, from either end, give the same
keys.

A `Matches` keeps the finder of its leftmost lookup and, behind one pointer that stays null while
there is none, a list of the finders of the other versions it reads, numbered from 1. `or`
compares each finder of the side it is given with those it has through `Finder::same_map`: one
root node, stored roots of one hash, or two empty maps are one version. A finder of a version it
has is handed to that side's streams, as for ranges above, and one of another version joins the
list, and the side's streams are renumbered to match. So lookups of one table at one version, the
common case, keep one finder and no list. A root lent through a hazard slot and one read through a
link whose node was not yet read compare unequal even when they are one map, which costs a second
finder and no wrong answer.

Each stream that reads entries, a lookup of one value, a collected list or a range, holds the
number of its finder. An `or` records which side it took each key from, and an `and` answers with
its left side, the side it took the key from or, when the other side leads, the side whose probe
last found it. `next`, `next_back` and `next_ref` ask only when the combination has a list, in a
function kept out of line, so one of one version runs the code it did and one more test. An `and`
never asks its right side, so once it is built it neither renumbers that side nor hands it finders
again. A combination that has yielded rows and is combined again is read to the end first, as
before, but with several versions its keys go into one list per finder, under an `or` of the lists,
so each keeps its finder.

The list's finders read rows, so they stay with the combination while it may yield rows. When the
combination becomes the right side of an `and` they are handed to its streams with the leftmost
one, and `keys()` and `covered()`, which yield no rows, hand over the list before the first read.
A range on the right side of an `or` of two versions is so tested when the `or` is the right side
of an `and`, or the left side of one read through `keys()`, but collected when that `and` yields
rows or covered fields.

`or` now takes each key from the side that holds the nearest one and compares only the sides after
it, since an earlier side holding the same key would have been the nearest. Taking it first gives
the key to compare with, where the key used to be cloned and every side asked.

`combine_range.rs` runs its random combinations over two tables with overlapping and disjoint keys,
over a table and a version of it with rows changed, removed and added, in memory and from an
uncached file, and over a table, a clone of it and another table, with each subtree of a query
moved at random to one of them. Its model gives each key the row of the lookup that should hold it,
and rows, rows from the back and from both ends, lent rows, visited rows, the first few, covered
fields of an `or` with another table's range, and combinations read partly and combined again are
compared with it whole. The counting test checks that a range on the other side of an `or` with
another version is still tested against a few keys. `combine.rs` runs the example above.

### What it measured

`ferro-bench --bin combine`, which `make bench-combine` runs, builds 200,000 rows with two
`#[index(each)]` fields. For each case it gives one value of each field to rows picked at random, to
two spans apart, or, for the second, to rows the first lacks. Every strategy's keys are checked
against a merge, and the figures are medians over three runs of three rounds on a machine shared
with other work, taken before the rows gained the fields of the range cases described under [What
ranges and prefixes measured](#what-ranges-and-prefixes-measured). Keys only, in microseconds:

| Lookups | Keys in both | Merge | Set | Leapfrog, `_after` | 32 steps, then `_after` | Leapfrog, cursor | `and` |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 50k and 50k | 12,557 | 1,255 | 2,190 | 13,765 | 1,409 | 2,216 | 1,609 |
| 50k and 50k, none shared | 0 | 1,247 | 1,986 | 9,943 | 1,348 | 1,677 | 1,398 |
| 50k and 50k, apart | 0 | 307 | 2,010 | 0.6 | 0.7 | 0.3 | 0.8 |
| 25k and 50k | 6,363 | 831 | 1,603 | 9,034 | 947 | 1,361 | 1,064 |
| 12k and 50k | 3,171 | 561 | 1,267 | 5,026 | 660 | 801 | 655 |
| 3k and 50k | 793 | 409 | 1,042 | 1,424 | 479 | 244 | 181 |
| 1k and 100k | 479 | 699 | 1,560 | 443 | 369 | 73 | 70 |
| 100 and 100k | 42 | 672 | 1,717 | 39 | 37 | 8.0 | 7.9 |
| 10 and 100k | 3 | 597 | 1,530 | 3.5 | 3.5 | 1.0 | 1.5 |
| 20 and 20 | 0 | 0.5 | 0.9 | 5.1 | 0.5 | 0.8 | 0.7 |

The merge steps the `keys()` of both lookups. The set collects the smaller lookup's keys into a
`HashSet` and filters the larger's. The `_after` leapfrog seeks each side with a `by_x_after`
lookup, built from the root, and the next column steps up to 32 keys before seeking that way. The
cursor leapfrog seeks both sides with `Cursor::seek` on the index maps, as `and` looks up its larger
side. From the back, `and` came within 5% of its figures here.

`and` follows the smaller side: 10 keys against 100,000 took 1.5 µs where the merge took 597, and a
gap between two spans of 50,000 cost one seek. The price is on even sizes, where it ran 1.28 times
as long as the merge, 18.5M instructions against 11.1M, for reaching each key through the node and
its sides rather than a loop over two iterators. The factor of four is where the plans cross. At 12k
against 50k, looking keys up ran 7.9M instructions against 10.3M for walking both, and at 25k
against 50k, walking ran 12.8M and 942 µs against 13.8M and 1,054 µs. Stepping 4 times rather than
16 before building a range again took the even case from 18.5M to 24.0M instructions, and 64 left it
unchanged.

Rows come through the finder an `IndexIter` uses, which keeps the last leaf it found. The 12,557
rows of the first case, which lie in key order across the table, took 4.34 ms with their keys
against 4.64 ms for the keys and a `get` each, and 25.4M instructions against 30.7M. Rows scattered
across the table took the same time either way.

Plain lookups are unchanged. Over four interleaved runs of each build, a lookup of one row ran 3,174
instructions against 3,164 before, and one of 20 rows 26,642 against 26,497, within the spread
between processes. A page of 200 rows, `len` and `keys` ran the same, and every time came within 1%.

### What ranges and prefixes measured

The range cases of `ferro-bench --bin combine` combine `find().by_group(&g)` with a range of a plain
`score` index, a range of a unique `rank` index or the prefix of a band of a composite `(band,
score)` index, on the same 200,000 rows. Each field is a random permutation of the rows, so both
sides' keys are scattered across the table, and every row is cached. The figures are medians over
three runs of each build, alternated, of three rounds each, pinned to one CCD of a 16-core Zen 5
machine. Keys only, in microseconds, before and after:

| Lead | Range | Plain range | Unique range | Prefix |
| --- | --- | --- | --- | --- |
| 10 | 10,000 | 152.8 → 1.4 | 150.4 → 1.3 | 152.9 → 1.4 |
| 10 | 100,000 | 2,428.7 → 1.5 | 2,416.6 → 1.4 | 2,449.0 → 1.5 |
| 100 | 10,000 | 157.7 → 8.3 | 153.8 → 8.5 | 155.5 → 8.5 |
| 100 | 100,000 | 2,453.1 → 9.0 | 2,427.6 → 9.0 | 2,457.0 → 9.3 |
| 1,000 | 10,000 | 180.9 → 95.8 | 177.7 → 95.5 | 179.7 → 98.4 |
| 1,000 | 100,000 | 2,525.9 → 105.5 | 2,517.1 → 105.3 | 2,543.2 → 107.6 |
| 10,000 | 10,000 | 396.3 → 394.4 | 392.0 → 390.8 | 393.6 → 391.3 |
| 10,000 | 100,000 | 2,995.1 → 884.2 | 2,967.1 → 876.8 | 3,007.1 → 898.4 |

10 keys against a range of 100,000 ran 24,080 instructions where they had run 42.4 million. From the
back the figures were the same within 1%, or within the 0.1 µs the benchmark prints. Read as rows,
the plain range took 2.0 µs against 2,429.6 for 10 keys, 12.7 against 2,445.6 for 100, 142.9 against
2,603.6 for 1,000 and 1,253.2 against 3,488.2 for 10,000, the difference from keys being the rows
that match, read again through the combination's own finder. Ranges of 1,000 against the lead of
10,000, where the range leads and is collected, took 55.5 µs against 57.5 and 1,099,120 instructions
against 1,091,462.

A tested key costs 88 to 105 ns, against 71 to 85 ns for reading the same rows through the lead's
`visit` and checking the field by hand, the rest being the lend through a second finder, the
generated function and the combination's steps. Reading and sorting a range costs 18 to 30 ns a
key, more the more keys it has, so the crossover depends on the sizes. Against score ranges of 1 to
128 times the lead's keys, in microseconds before and after:

| Range / lead | Lead of 100 | Lead of 1,000 | Lead of 10,000 |
| --- | --- | --- | --- |
| 1 | 2.6 → 2.5 | 27.5 → 26.9 | 401.1 → 398.9 |
| 2 | 4.2 → 4.1 | 47.2 → 45.9 | 745.2 → 732.9 |
| 4 | 7.0 → 8.0 | 80.3 → 88.6 | 1,291.7 → 766.4 |
| 8 | 12.7 → 8.0 | 146.3 → 88.9 | 2,413.3 → 834.4 |
| 16 | 24.3 → 8.1 | 345.8 → 89.5 | 4,633.5 → 882.5 |
| 32 | 48.8 → 8.1 | 769.3 → 91.6 | |
| 128 | 205.1 → 8.2 | 3,232.1 → 101.1 | |

Testing breaks even with collecting at about 4.7 times the lead's keys for a lead of 100, 4.5 for
1,000 and 2.1 for 10,000. The plan tests from 4 times, the factor that already decides between
walking both sides and looking keys up: at 4 it takes 14% and 10% longer than collecting for the
two smaller leads, a microsecond and 8 µs, though it runs 5% and 25% fewer instructions, since its
row reads miss the cache where the collection streams, and saves 525 µs for the larger.

Those rows were all cached. The cold cases of the benchmark hold 60,000 rows whose 4 KiB notes are
stored apart from their leaves, so a row read from the file reads its leaf and its note, 2.4
objects a test on average, about 7.5 µs where collecting read about 105 ns an entry from the file.
The first build tested whatever the rows cost, and against 1,000 keys a range of 4,000 took 7.6 ms
as keys, where reading it in full took 0.54 ms. Medians of two runs of each build, alternated, in
microseconds, keys first and rows after, the first build, the build that counts the reads from the
file, and the range collected:

| Lead | Range | Cache | Keys | Rows |
| --- | --- | --- | --- | --- |
| 10 | 50,000 | cold | 165 → 157 (5,073) | 170 → 161 (5,214) |
| 100 | 10,000 | cold | 1,025 → 1,526 (1,047) | 1,032 → 1,753 (1,312) |
| 100 | 50,000 | cold | 1,030 → 1,026 (5,075) | 1,058 → 1,053 (5,967) |
| 1,000 | 4,000 | cold | 7,567 → 755 (540) | 7,541 → 1,453 (1,262) |
| 1,000 | 16,000 | cold | 7,470 → 2,553 (1,771) | 7,559 → 4,698 (3,973) |
| 1,000 | 32,000 | cold | 7,475 → 4,903 (3,380) | 7,633 → 9,042 (7,605) |
| 1,000 | 60,000 | cold | 7,629 → 9,267 (6,257) | 7,796 → 7,885 (13,866) |
| 1,000 | 4,000 | 1 MiB | 6,384 → 367 (104) | 6,590 → 773 (155) |
| 1,000 | 16,000 | 1 MiB | 6,446 → 2,281 (1,148) | 7,834 → 4,039 (2,855) |
| 1,000 | 60,000 | 1 MiB | 6,403 → 9,581 (6,108) | 8,263 → 8,205 (12,633) |
| 1,000 | 16,000 | warm | 152 → 153 (327) | 175 → 178 (380) |

With both the rows and the index read from the file, the combination came within 1.5 times the
better of the two plans as keys and 1.7 times as rows. With the index cached but not the rows'
notes it took up to 3.5 times as long as collecting in the same runs, where the first build took
17 times as long. Counting cost 18 instructions a test, 1.2% of a warm one, and the warm figures
moved within the spread between runs. The reads of a matching row count only under `keys()` and
`covered()`: counted as rows too, 1,000 keys against the whole table took 14.2 ms as rows where
testing them all had taken 7.7 ms. Testing a row and then yielding it reads it from the file once:
with all 1,000 keys tested against the whole table, the first build took 0.17 ms more as rows than
as keys.

Collected, a range runs the code it did. Read alone through `into_matches().keys()` it ran 0.1 to
0.6% more instructions, and within the factor of four, where the range is walked beside the lead,
0.07 to 0.3% more. A first build of the stream asked whether it was collected through a function
that moved the whole cursor out of its `Option` on every call and compared keys through the
strictness flag inside `partition_point`, and ran 21 instructions a key more there and 8 to 17% more
in the merges, which moving the collection out of line and writing the four comparisons out
removed. The equality cases ran within 0.6% of their instructions before, and their times within 2%,
or 0.1 µs, but for `and` read as rows on the even cases, which came out 5% slower over three runs
and 2% slower over two more, at 2,370 µs, where the build before ranged from 2,260 to 2,380 µs
with the same instructions.

Lookups read alone are unchanged. Over two runs of `scaling` alternating with the build before, on
one thread with the 1 GiB cache, a lookup of about 20 rows by a range ran 28,779 instructions
against 28,761, read through `visit` 28,740 against 28,712, and through `covered` 5,811 against
5,795, and pages by equality ran the same within 0.1%. Their rates moved within the spread between
runs, and over three more runs of `tier-ref` and `index-visit` came within 0.5%. A first build ran
377 more instructions a lookup through `visit`: the range lookup's new cursor type moved
`IndexIter::visit` into the benchmark's read loop, where it had been compiled apart, and left
`Finder::lend` outside it, a call for every row. `Finder::lend` is now `#[inline(always)]`, so
where `visit` lands no longer matters.

### What several tables measured

The equality cases of `ferro-bench --bin combine` also run the `or` of the two lookups, as keys and
as rows, and the `or` of the first with the second made on a newer version of the table, a clone
whose last row's name changed, which shares every node but those on the way to that row. The
figures are medians over three runs of each build, alternated, of five rounds each, pinned to one
CCD of a 16-core Zen 5 machine, the build before taken with the same benchmark. In microseconds and
millions of instructions, before and after, with two versions after:

| Lookups | In either | `or` keys | `or` rows | Two versions, rows |
| --- | --- | --- | --- | --- |
| 50k and 50k | 87,362 | 2,328 → 2,116 (38.0 → 35.7) | 7,221 → 6,821 (64.4 → 62.3) | 7,379 (72.8) |
| 50k and 50k, none shared | 100,274 | 2,440 → 2,222 (41.0 → 38.3) | 7,568 → 7,584 (70.0 → 67.4) | 8,032 (79.1) |
| 25k and 50k | 68,889 | 1,720 → 1,548 (29.3 → 27.2) | 6,107 → 5,990 (51.8 → 49.9) | 6,942 (58.8) |
| 3k and 50k | 52,303 | 1,034 → 908 (21.5 → 19.6) | 4,554 → 4,105 (40.3 → 38.5) | 4,410 (42.8) |
| 1k and 100k | 100,142 | 1,929 → 1,670 (40.9 → 37.2) | 7,262 → 7,284 (69.9 → 66.4) | 7,294 (71.7) |
| 100k and 10 | 100,272 | 1,969 → 1,776 (41.3 → 39.5) | 7,203 → 7,060 (70.3 → 68.7) | 7,076 (72.3) |
| 20 and 20 | 43 | 1.2 → 1.1 (0.024 → 0.023) | 4.4 → 4.3 (0.077 → 0.076) | 4.4 (0.078) |

Taking each key from the nearest side first made `or` run 4 to 9% fewer instructions as keys and 1
to 5% fewer as rows. Read as keys, two versions ran the same as one. Read as rows, they ran 35 to 55
instructions a row more where nearly every row came from one version, the cost of asking the
stream and of the read kept out of line, and up to 130 more, and up to 16% longer, where the rows
alternated between the two, since each finder then keeps the leaf of fewer of the rows it reads.
The build before read every row of the two versions through the first finder: the rows were the
same here, since only one row's name differed, so it ran at the cost of one version.

The other cases ran the code they did. `and` ran within 0.01% of its instructions per key, and as
rows 0.05 to 0.09% more, about two instructions a row for the test of the list's pointer. Setting up
an `and` ran 10 to 95 instructions more, at most 0.4% of two lookups of 20 keys or of 10 keys
against a range. The range cases came within 0.4% of their instructions, and every time within the
spread between runs. A first build kept the list as a vector, handed finders down as a list even for
one version, read the row through a call that asked for the finder's number when the list was empty,
and found the side to record in `or` by counting the sides it went through: `or` ran 2.2% more
instructions, `and` as rows 5 more a row, and setting up a combination 220 to 260 more.

### What planning measured

The order cases of `ferro-bench --bin combine`, which `make bench-combine ARGS='--cases order'`
runs, write a combination in an order the earlier plans suited and in one they did not, on the
200,000 rows of the other cases, with a group's covered fields left out. The figures are means of
two runs of each build, alternated, of the medians of three rounds each, pinned to eight cores of a
machine shared with other work. In microseconds, before and after, as keys and as rows, written in
the order the earlier plans did not suit:

| Combination | Keys | Rows |
| --- | --- | --- |
| Score range of 100,000, then 10 keys | 2,433 → 1.4 | 2,461 → 1.8 |
| Score range of 100,000, then 1,000 keys | 2,591 → 107 | 2,726 → 138 |
| Score range of 100,000, then 10,000 keys | 2,991 → 933 | 3,747 → 1,251 |
| Score range of 100,000, then rank range of 1,000 | 2,470 → 115 | 2,565 → 140 |
| Score range of 100,000, then rank range of 10,000 | 3,132 → 1,130 | 3,652 → 1,395 |
| 100,000 keys, then 1,000, then a group of 100 | 17.9 → 4.1 | 18.0 → 4.0 |
| 100,000 keys, then 1,000, then a group of 1,000 | 47.2 → 12.2 | 47.5 → 12.4 |
| 50,000 and 50,000 sharing 12,500, then score range of 100,000 | 4,600 → 2,960 | 5,875 → 3,366 |
| 50,000 and 50,000 sharing 12,500, then score range of 150,000 | 6,121 → 2,715 | 8,382 → 3,295 |

Each of the first seven now runs within 1% of the instructions of the order the earlier plans
suited: 41.7 million instructions became 24,000 for the range written before 10 keys, and 1.03
million became 0.35 million for the second chain. The order the earlier plans suited ran 0.1 to
1.1% more instructions. The last two have no better order: the two lookups merged first and the
range was read in full. Testing the range against the estimate of 12,500 keys ran 30.0 and 30.8
million instructions where reading it had run 65.6 and 89.0, close to the 30.2 million of filtering
the merged keys' rows by hand. Against a score range of 30,000, within four times the estimate,
all three are walked, and the leapfrog ran 2.0% more instructions than the merges nested as
written; a score range of 2,000 before 1,000 keys and two ranges of 50,000 and 100,000, read in
full either way, ran 1.0% and 1.2% more.

The equality and range cases of the benchmark plan as they did. Built with one codegen unit, to
keep functions moving between units out of the count, setting up a combination of two equality
lookups ran 130 to 170 instructions more, 1.2% of two lookups of 20 keys, and one with a tested
range about 260 more and about 5 more a tested key, 0.3 to 0.4% for 1,000 keys or more. Built as
`make` builds them, collecting a range ran up to 1% more instructions in every build that changed
the stream types, though its code did not change. The cold cases came within 1% of their
instructions, and the warm ones within 1.8%, the most for 10 keys, once tests and reads from the
file kept apart counts, as above.

Taking an `and` apart when it is given to another, and growing a node from eight sides to 64, left
the plans and the work for each key as they were. Built with one codegen unit, every `and` ran 30
to 100 instructions more for the call that finds nothing to take apart, at most 0.7% of two lookups
of 20 keys and 0.07% or less from 100 keys up, and planning the chain of three lookups above ran
about 250 more, 0.3% of the 91,000 it ran. The chains with a tested range moved by up to 0.6%,
less than the 4% one build moved between two runs of different lengths.

### Left out

- The index map in every equality `IndexIter`, so that `by_x` itself could combine. A clone of the
  map takes counts on the store and on the index's root and gives them back on drop, and every
  lookup paid for it whether it combined or not: `len` of a 20-row lookup ran 2,282 instructions
  against 2,032 and took 24% longer, a one-row lookup ran 3,432 against 3,189 and took 15% longer,
  and 20 rows took 4% longer. Only a page of 200 rows hid it. The view clones the map only for
  lookups made to combine.
- An `IndexIter` that knows whether it is in key order and is collected only when it is not. It
  would let every lookup combine, but each `by_x` would carry the map as above.
- Collecting one side into a set. Probing a `HashSet` costs more than a step, so it ran at least 1.6
  times as long as the merge in every case, and since it walks the larger side whatever the sizes,
  10 keys against 100,000 took 1,530 µs.
- Leapfrogging through `by_x_after`. Each seek builds a lookup from the root, about 3,000
  instructions, so even sizes took 11 times as long as the merge, and 1k against 100k six times as
  long as a cursor. Stepping 32 keys first brought even sizes to 1.12 times the merge and left the
  skewed cases three to five times as slow as a cursor.
- Seeking both sides with a cursor and walking neither, 1.8 times as long as the merge on even
  sizes. Stepping a cursor up to 4 times before each seek came to 1.14 times the merge there and 82
  µs at 1k against 100k, against 70 for `and`, which keeps cursors for the side it only looks up.
- Testing a key against a range by seeking the range's index at `(value, key)`, with the value taken
  from a lead index that covers the range's field, rather than reading the row. In the benchmark the
  group index covers the score, and checking the covered score against the bounds and seeking the
  score index for those inside took 5.9 µs for 100 keys against a range of 100,000, 64.9 for 1,000
  and 839.8 for 10,000, against 9.0, 105.5 and 884.2 for reading rows. It needs a lead that covers
  the range's field, and the derive to tell which covered field of which index that is, and it saved
  35% of the time for 100 keys but 5% for 10,000. A caller whose lead covers the field and reads one
  version can filter its `covered()` by the bounds, which took 1.6, 14.8 and 176.7 µs.
- Testing below the factor of four. For a lead of 10,000 keys testing broke even at 2.1 times its
  keys, but for leads of 100 and 1,000 at 4.5 to 4.7, and within the factor walking both sides
  beats testing for small leads.
- A finder of its own for a range that leads a combination through `into_matches`, made from the
  one the combination reads its rows through. A finder lends the store and root through hazard
  slots of its own, and making a second from the first means lending both again. Such a range
  borrows the finder of another side of the same table and version instead, which every lookup
  comes with, and is collected when there is none.
- Testing a lookup passed through `IndexIter::uncovered`. The cursor it wraps has covered fields
  that the method does not require to be `Send`, which a stream holding them would need, and
  requiring it broke callers generic over the covered type. It is collected.
- Testing the equality lookups made on the table, such as `by_x(&value)` passed to `and`. The
  `find()` view's lookup of the same value walks and seeks, and the table's own lookups sit on the
  read paths whose code the cursor change would move.
- A larger factor for `keys()` and `covered()` in place of counting reads from the file. Rows read
  from the file cost as much as rows yielded: as rows, 1,000 keys against a range of 4,000 took
  7.5 ms tested and 1.3 ms collected. A factor of 64 would also give up the cached cases in
  between, where 1,000 keys against 32,000 took 92 µs tested and 769 collected.
- Predicting what the tests will cost from the lead's length and the reads of the first few. A
  view lookup knows its length, but an `and` or `or` only bounds it, and a loose bound would have
  the range read in full where testing its few keys cost little. Counting what was spent bounds the
  loss whatever the lead holds.
- Weighting a read from the file at 32 entries rather than 64. It stopped testing later, and the
  cold cases that stop took 28 to 36% longer as keys.
- Skipping a key whose row the leftmost table lacks, which would cost nothing while every row is
  there. The rows would then leave out keys that `keys()` yields, unless `keys()` read each key's
  row from that table, which is what it exists not to do, and a key the leftmost table holds in
  another state than the version that matched it would still come back with a row its lookup had
  not matched.
- Refusing to combine lookups of different tables or versions. A lifetime on `Matches` tying it to
  one table would refuse two tables when it compiles but not two versions of one, which share the
  type, and every combination would carry the lifetime. A check when it runs needs the same
  comparison of roots, and refuses what a tested range already answered for: a lookup of the table
  inside a commit combined with one of the snapshot before it.
- Reading each row from the leftmost table that holds the row rather than from a lookup that holds
  the key. It would need no numbers, but a table can hold a row its lookup did not match, and that
  row would disagree with the covered fields and with every lookup that matched the key.
- Returning the finder's number from `take` with each key. Every stream and every combination
  would pay for it on every key, where asking after `take` costs only combinations of several
  versions.
- A second finder made from a range's, so that a range on the right side of an `or` of two
  versions could test while the combination reads that version's rows through the first. Making
  one means lending the store and root through new hazard slots, for a case that collects the
  range as it did before ranges were tested.
- A cost model in place of the factor of four, with constants fitted to the crossover
  measurements. Fitted to those above, it would test a range from about twice the lead's keys for a
  lead of 10,000. With other work running on the machine, the crossover for that lead moved to about
  five times: at four times, testing took 1,879 µs where collecting took 1,589, and at eight times
  1,627 µs where collecting took 3,117, while leads of 100 and 1,000 crossed where they had. A
  test costs what the caches hold, which no count tells, so the factor stays.
- Estimating the keys that reach an equality lookup in a chain, as for a range. Looking an equality
  lookup up costs about four steps of walking it, so an estimate too low by more than four times, as
  for two lookups that hold the same keys, would cost more than walking it, with no count to stop
  it as a range's tests have.
- Merging the two walked sides with the fewest keys first and seeking the others to the key they
  agree on, as nested `and`s did. For two lookups of 50,000 and a range of 30,000, all three
  walked, that merged the range with one lookup first and ran 11.6% more instructions than the
  `and`s nested as written, where the leapfrog of all three runs 1.5% more.
- Leaving out `or`'s comparisons when its sides' keys lie apart. A key costs a comparison with the
  next key of each later side, a few instructions of the 390 or so a key of two lookups of 50,000
  costs, and a count tells how many keys a side holds, not where they lie.
- Testing a range first for a read that stops early, such as `take(20)`, and collecting it once the
  tests cost as much. An iterator does not tell how many keys it will be asked for, and a
  combination read to the end would take up to twice as long wherever collecting is the better
  plan, which the factor of four decides.
- Holding a chain's plan in vectors, so that a chain could hold any number of sides. Sorting the
  sides into a vector and splitting the walked ones from those looked up took four allocations, and
  planning the chain of three lookups above ran about 700 instructions more than with arrays of
  eight, where arrays of 64 run about 250 more.

### What is left

- Even sizes run 1.3 times as long as a merge written for them. A seek inside the range iterator,
  climbing its frames as a cursor does, would let a walked side skip without building its range
  again, and both that and a cursor that steps backwards would live in `map.rs`.
- A range or prefix lookup is read in full and sorted before it combines when the keys led to it
  are more than a quarter of its own, when it leads its `and`, when the combination yields its
  covered fields, after `IndexIter::uncovered`, and on the left of an `and` none of whose other
  sides reads its table at its version with a finder to spare. A unique index has no lookup in the
  view, since `key_by_x` gives its one key.
- The factor of four suits leads of up to a few thousand keys. Against larger ones testing would
  win from about twice the lead's keys on a quiet machine, but the plan within the factor walks
  both sides, and where the plans cross moves with what the caches hold.
- A chain plans once, from counts and from an estimate that its lookups match independently. When
  two walked lookups share most of their keys, a range tested against what they agree on stops at
  about what collecting it costs, so it takes up to about twice as long as collecting it would
  have. A chain holds 64 lookups, past which `and` builds a node over the 64 and the next.
- A lookup of an `and` nested on the right lends its finder to a range elsewhere in the chain only
  when the nested `and`'s left side reads the chain's table and version, since its node kept for
  each side only whether the side reads what that left side reads. Written as one chain from the
  left, the same lookups lend whenever the lookup and the range read one table at one version.
- A tested key costs a row read, so `keys()` and `covered()` read rows they did not before. Rows
  read from the file stop the testing once they cost about what collecting would, but a range
  whose entries are cached while its rows are not still takes up to 3.5 times as long as
  collecting it, since a read from the file is weighed at 64 entries, where it costs about 130
  cached ones and 30 read from the file.
- `Matches` knows only an upper bound on its length, through `size_hint`.
- Setting up a combination costs about 4,000 instructions, so two lookups of 20 keys took 0.7 µs
  against 0.5 for the merge. Planning chains added about 150 to that, and when a range is tested
  about 260, and 5 a tested key.
- A range on the right side of an `or` whose finder reads another version than any lookup to its
  left is read in full when the `or` is the left side of an `and` that looks keys up in it and
  yields rows, since the combination reads that version's rows through the range's finder.
- Two finders of one stored map can compare unequal when the map's root node was first read from
  the file between the making of the two, since the earlier one holds only the link and the later
  one only the node, and such a combination reads through both.
