# Schema evolution, unique and covering indexes

How the derive macros encode a struct so that it can change, how unique indexes keep their
constraint through inserts, updates and merges, how a covering index stores fields beside its
entries and what that costs, and how a table knows which stored indexes it can read. The API is in
[Changing your types](../guide/schema-evolution.md) and [Tables and indexes](../guide/tables.md).

## Schema evolution

The derive macro writes structs as `(tag, length, payload)` fields, where the tag is a 16-bit FNV-1a
hash of the field name. Fields are sorted by tag, so reordering a struct does not change its hash.
Unknown fields are skipped. Missing fields use `Data::missing`, or `#[ferro(default)]`.
`#[ferro(rename = "old")]` keeps the old tag. A table's indexes are not told apart by tag, as
described under [Index fingerprints](#index-fingerprints).

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

The kind of an index is part of its fingerprint, so turning an `#[index]` into a `#[unique]`
rebuilds it instead of reading the old map with new types. The rebuild on decode covers unique
indexes, and checks the ones it rebuilds. If two existing rows share a value it fails with
`Error::Decode` naming the index, because a constraint the data breaks cannot be honoured, so that
version does not open until the data or the attribute changes. The rebuild reads the rows through
`table::stored_rows`, which returns a damaged row node as an error, so `Db::open` fails with it
instead of panicking.

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
hand-written `Data` impl, as described under [Comparing values](merging.md#comparing-values). An
`#[index(each)]` entry is rewritten for every element when a covered field changes. `Table::merge`
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

- its kind as the derive generates it: value, option for a value index on a field written
  `Option<T>`, composite, each, unique, unique option for a unique index that leaves `None` out, or
  unique composite;
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
- A derived struct by its stored fields in tag order, each name with its type, and a derived enum
  by its variants the same way. The struct's Rust name, the order its fields are declared in and its
  skipped fields take no part. A transparent struct is described as the field it wraps.
- A type met again inside its own description, directly or through others, by how many types up the
  first one is, so `struct Tree { kids: Vec<Tree> }` has a finite description.
- Anything else by `std::any::type_name`, unless its `Data` impl overrides `describe`.

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
keeps its fingerprint unless one of its fields has a default, and those indexes are rebuilt once
after upgrading.

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
checks `#[ferro(default)]` the same way.

### Where it is stored

A table encodes as a struct of fields, as before: its rows under the tag of `rows`, and every index
under the one tag of `indexes`, as its fingerprint in eight little-endian bytes followed by the map.
Index fields are written in fingerprint order, so the order indexes are declared in does not change
the encoding, and since the tag of `indexes` is below that of `rows` they come first. Decode reads
each index field's first eight bytes and keeps the map under the declared index with that
fingerprint. A field shorter than eight bytes, or whose fingerprint no declared index has, is
skipped. Each declared index left without a map is rebuilt: the rows are read once, every such index
is filled from them, unique ones are checked for clashes as they fill, and the indexes that were
found are kept as they are. `visit`, which `verify_typed` and the check on receiving a sync use,
reads index fields the same way and checks only those it can match.

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
