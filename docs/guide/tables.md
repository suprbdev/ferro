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
that `a`, ordered by `b`. Any of these can store fields beside the entries, as described under
[Covering indexes](#covering-indexes).

## Unique indexes

`#[unique]` on a field, or `#[unique(a, b)]` on the struct, keeps a value held by one row at most.
`by_slug(&value)` returns the row, `key_by_slug(&value)` only its key, and `unique_slug()` the whole
index. `try_insert` and `try_update` return `Err(Taken { index, holder })` on a clash. `insert`,
`update` and `get_mut` have no error channel, so they panic, which discards the transaction.
`Table::merge` passes a clash to your `resolve` closure with `ours` set to the row holding the
value, and asks about each key once at most. A row that still clashes after that keeps our row for
its key, so a merge never panics on a clash.

## Covering indexes

`cover(...)` in an index attribute stores the fields it names beside every entry of that index, so
a lookup can answer from the index without reading the rows:

```rust
#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Booking {
    #[key]
    id: u64,
    #[index(cover(status, first, nights, total))]
    customer: u32,
    status: Status,
    first: u32,
    nights: u32,
    total: i64,
    prices: Vec<i64>,
}

let newest: Vec<_> = snapshot.bookings.by_customer(&7).covered().rev().take(20).collect();
for (id, (status, first, nights, total)) in newest {
    println!("#{id} {status:?} {first} x{nights} {total}");
}
```

Every lookup of a covered index still returns an `IndexIter` that yields rows, and `.covered()`
turns it into `(key, fields)` pairs in index order, from either end. That holds for `by_x`,
`by_x_range`, `by_x_after`, `by_x_some`, `by_x_prefix` and the rest, so a range over the index
returns its covered fields in order. One covered field comes back as itself and several as a tuple
in the order listed. `.uncovered()` drops the fields from the type, for code that keeps several
lookups in one `Vec<IndexIter<T>>` and reads the shortest.

A cover list may name the fields the index is keyed on. Those come from the entry and are not
stored twice, so `#[index(cover(balance))] balance` stores exactly what `#[index] balance` does and
lets `by_balance_range(..).covered()` return the balances without the rows. On a composite index,
`#[index(customer, first, cover(first, total))]` lists a customer's rows by `first` with their
totals through `by_customer_first_prefix(&customer).covered()`. `#[index(each, cover(...))]` stores
the fields beside each element's entry. `#[unique(cover(...))]` stores them beside the key, and
`covered_by_x(&value)` returns `Some((key, fields))`. An each index cannot cover the field it
indexes, a unique index cannot cover its own fields, and no index covers the key, which every entry
already holds.

The tuple follows the list, so reordering two covered fields of one type swaps them in code that
destructures the tuple, and the compiler cannot tell.

A covered field is copied into every entry, and changing it rewrites the entry where a plain index
is left alone. On 200,000 rows a page of 200 rows read through a covered index went from 179,000 to
854,000 a second on one thread and from 957,000 to 3.3 million on 32, and a lookup of 20 rows by a
range from 219,000 to 2.4 million on one thread. Covering one integer grew the indexes 17%, and a
commit that changed it wrote 35% more and took 28% more CPU. A commit that changed nothing covered
cost the same. Cover what a hot page shows and what seldom changes after the row is written, and
leave wide strings and fields rewritten on most commits out. A plain `#[index]` is unchanged and
costs nothing more. [Covering indexes](../internals/schema-and-unique-indexes.md#covering-indexes)
has the measurements.

Changing a cover list rebuilds that index the next time the table is read, as [Changing your
types](schema-evolution.md#indexes) describes.

## Lookups

A `Table` lookup through an index reads each row separately unless you read it through `.covered()`,
and no query planning happens. You choose the index. `IndexIter::keys` gives the primary keys
without reading any row.

## How it works

[Schema evolution, unique and covering
indexes](../internals/schema-and-unique-indexes.md#unique-indexes) describes how a clash is refused
and how `Table::merge` resolves one, and how a covered index is stored and rebuilt.
