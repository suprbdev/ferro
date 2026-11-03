# Tables and indexes

A `Table<T>` holds rows keyed by the field marked `#[key]`, and keeps a secondary index for every
field marked `#[index]`. `#[derive(Record)]` generates the indexes and their lookups. The `Order`
row in [Getting started](getting-started.md#a-first-database) is an example.

## Secondary indexes

`#[derive(Record)]` generates index lookups directly on the table: `by_status(&value)`,
`by_status_range(range)` and `by_status_counts()`, which needs no row reads.
`by_status_after(&value, &key)` continues after a row's key within that value, so a filtered page
costs a seek, not a walk over the rows already shown. An `Option<T>` index also gets
`by_field_some(&value)` and `by_field_some_after(&value, &key)`, and `#[index(each)]` indexes every
element of a collection field.

`#[index(a, b)]` on the struct indexes a combination of fields in that order. It gets the same
`by_a_b(&(a, b))`, `by_a_b_range` and `by_a_b_counts`, and `by_a_b_prefix(&a)` for every row with
that `a`, ordered by `b`.

## Unique indexes

`#[unique]` on a field, or `#[unique(a, b)]` on the struct, keeps a value held by one row at most.
`by_slug(&value)` returns the row, `key_by_slug(&value)` only its key, and `unique_slug()` the whole
index. `try_insert` and `try_update` return `Err(Taken { index, holder })` on a clash. `insert`,
`update` and `get_mut` have no error channel, so they panic, which discards the transaction.
`Table::merge` passes a clash to your `resolve` closure with `ours` set to the row holding the
value, and asks about each key once at most. A row that still clashes after that keeps our row for
its key, so a merge never panics on a clash.

## Lookups

Each `Table` lookup through an index reads the row separately, and no query planning happens. You
choose the index. `IndexIter::keys` gives the primary keys without reading any row.

## How it works

[Schema evolution and unique indexes](../internals/schema-and-unique-indexes.md#unique-indexes)
describes how a clash is refused and how `Table::merge` resolves one.
