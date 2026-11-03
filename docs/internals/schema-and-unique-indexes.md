# Schema evolution and unique indexes

How the derive macros encode a struct so that it can change, and how unique indexes keep their
constraint through inserts, updates and merges. The API is in [Changing your
types](../guide/schema-evolution.md) and [Tables and indexes](../guide/tables.md).

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
