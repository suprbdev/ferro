# Tables and indexes

A `Table<T>` holds rows keyed by the field marked `#[key]`, and keeps a secondary index for every
field marked `#[index]`. `#[derive(Record)]` generates the indexes and their lookups. The `Order`
row in [Getting started](getting-started.md#a-first-database) is an example.

## Rows

`get`, `first`, `last`, `iter` and `range` read rows by key, and `insert` and `remove` write them.
`range` yields rows in key order as `iter` and the index lookups do, `keys` gives the keys alone,
and `rows()` gives the underlying `Map` for `(key, row)` pairs. For a tuple key, `prefix(&head)`
gives the rows whose key starts with `head`, as described under [Tuple
keys](collections.md#tuple-keys). Each yields rows as `Val`s, and `next_ref` and `visit` lend them
instead, as described under [Reading rows in place](#reading-rows-in-place). `get_mut` hands out a
`RowMut` that updates the indexes when it drops. `update(&key, f)` runs `f` on the row and returns
what `f` returns, or `None` when no row has that key, so a missing row becomes your own error in one
line:

```rust
let total = shop
    .orders
    .update(&id, |order| {
        order.status = Status::Shipped;
        order.total_cents
    })
    .ok_or(ShopError::NoOrder(id))?;
```

A closure that can fail returns a `Result`, and `update(..).ok_or(..)??` passes either error on.
Inside a commit, returning that error discards the half-changed row with the rest of the
transaction. `Map::update` works the same way on a value.

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
index. `by_slug_range(range)` returns the rows whose value lies in `range`, in value order, and a
composite `#[unique(room, night)]` also has `by_room_night_prefix(&room)`, a room's rows ordered by
night. A range of the composite reads one room's stays from a night on:

```rust
let from_tonight: Vec<_> = snapshot
    .stays
    .by_room_night_range((room, tonight)..=(room, Night::MAX))
    .keys()
    .collect();
```

Both return the same `IndexIter` as the lookups of a non-unique index, so they run from either end,
and `.keys()` and `.covered()` read the index alone. On an `Option` field the range is over the
inner type, and a row holding `None` is in none of them. There is no `by_slug_after` or
`by_slug_counts`, since each value has one row: the page after a slug is
`by_slug_range((Bound::Excluded(slug), Bound::Unbounded))`.

`try_insert` and `try_update` return `Err(Taken { index, holder })` on a clash, and `try_update`
otherwise returns `Ok` with what `update` would. `insert`, `update` and `get_mut` have no error
channel, so they panic, which discards the transaction. `Table::merge` passes a clash to your
`resolve` closure with `ours` set to the row holding the value, and asks about each key once at
most. A row that still clashes after that keeps our row for its key, so a merge never panics on a
clash.

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
the fields beside each element's entry. `#[unique(cover(...))]` stores them beside the key,
`covered_by_x(&value)` returns `Some((key, fields))`, and `by_x_range(..).covered()` the pairs in
value order. An each index cannot cover the field it indexes, a unique index cannot cover its own
fields, and no index covers the key, which every entry already holds, or a `#[ferro(skip)]` field. A
cover list stores at most 12 fields besides the ones the index is keyed on.

The tuple follows the list, so reordering two covered fields of one type swaps them in code that
destructures the tuple, and the compiler cannot tell.

A covered field is copied into every entry, and changing it rewrites the entry where a plain index
is left alone. On 200,000 rows, measured on 2026-11-05, a page of 200 rows read through a covered
index went from 183,000 to 855,000 a second on one thread and from 1.5 million to 9.3 million on
32, and a lookup of 20 rows by a range from 232,000 to 2.5 million on one thread. Covering one
integer grew the indexes 17%, and a commit that changed it wrote 36% more and took 28% more CPU. A
commit that changed nothing covered cost the same. Cover what a hot page shows and what seldom
changes after the row is written, and leave wide strings and fields rewritten on most commits out.
A plain `#[index]` is unchanged and costs nothing more. [Covering
indexes](../internals/schema-and-unique-indexes.md#covering-indexes) has the measurements.

Changing a cover list rebuilds that index the next time the table is read, as [Changing your
types](schema-evolution.md#indexes) describes.

## Combining lookups

`find()` on a table returns a view with the same equality lookups, `by_x(&value)`, `by_x_after`,
`by_x_some`, `by_x_some_after` and a composite's `by_a_b(&(a, b))`, which return `Matches` instead
of an `IndexIter`. `and` and `or` combine them by primary key and read a row only for a key that
matches:

```rust
let find = shop.orders.find();
let pending: Vec<_> = find
    .by_customer("wile")
    .and(find.by_status(&Status::Pending))
    .rev()
    .take(20)
    .collect();
```

`Matches` yields rows in key order from either end and takes further `and` and `or` calls. `.keys()`
gives the keys alone, and `.covered()` the covered fields of the leftmost lookup, so a combination
whose first index covers what you show reads no rows at all. An index entry is `(value, key)`, so
the keys of one value come in key order, and two lookups of similar size are walked side by side.
When one has at least four times the keys of the other, only the smaller is walked and each of its
keys is looked up in the larger, so 10 keys against 100,000 take 1.5 µs where walking both took
600 µs.

A lookup made on the table itself, such as `by_x_range` or `by_x_prefix` of a unique or a non-unique
index, can be passed to `and` or `or` too. It is not in key order, so when the other side of an
`and` has at most a quarter of its keys, only that side is walked, and each of its keys is tested
against the range by reading its row. With `day` indexed:

```rust
let find = snapshot.orders.find();
let this_month: Vec<_> = find
    .by_customer("wile")
    .and(snapshot.orders.by_day_range(first..=last))
    .collect();
```

On 200,000 rows, measured on 2026-11-06, 10 keys against a range of 100,000 took 1.5 µs where
reading and sorting the range took 2.4 ms, and 1,000 keys took 106 µs against 2.5 ms. Each key
tested costs a row read, about 100 ns with the rows cached, and `.keys()` and `.covered()` read
those rows too. When the range has less than four times the keys of the other side, leads the
combination through `into_matches()`, or gives the combination its covered fields, its keys are
read in full and sorted first, at 18 to 30 ns a key. A combination reads the indexes and rows as
they were when you built it, like any lookup. [Combining
lookups](../internals/schema-and-unique-indexes.md#combining-lookups) has the measurements.

## Reading rows in place

Every row a lookup, `Matches`, `iter`, `range` or `prefix` yields is a `Val`, which shares the row
with the table, so it can be kept, sent to another thread and read after the snapshot is gone. For
a row with a destructor, such as one holding a `String`, that share is a reference count on the
column of the node that holds the row, and every thread reading the same rows writes it.
`next_ref` steps to the next row and lends `&T` instead, until the lookup moves again, and `visit`
lends every row left to a closure:

```rust
let mut pending = snapshot.orders.by_status(&Status::Pending);
let mut page = Vec::with_capacity(20);
while page.len() < 20
    && let Some(order) = pending.next_ref()
{
    page.push((order.id, order.customer.clone(), order.total_cents));
}

let mut owed = 0;
snapshot.orders.by_customer("wile").visit(|order| owed += order.total_cents);
```

`IndexIter`, `Matches` and the `Values` that `iter`, `range` and `prefix` return all have both, and
a map's `Iter` lends `(&K, &V)` the same way. Read rows this way where the code uses a row where
it finds it: a page that copies the fields it shows, a total, a filter, a hot path that many
threads run over the same rows. Keep `next` and the iterator adapters where the rows themselves
go on, into a `Vec`, a channel or a response built later, since a lent row lasts only until the
next step and a clone of the row costs more than a `Val`. The loop around `next_ref` can `break`,
`return` or use `?` as it likes, and the lookup goes on from the next row.

On 200,000 rows, measured on 2026-11-05, 32 threads reading pages of the same 200 rows through
`next_ref` read 4.1 million pages a second where `next` read 1.5 million, and one thread 305,000
where it read 182,000. A lookup of about 20 scattered rows through `visit` went from 4.5 million to
5.1 million a second on 32 threads, and from 236,000 to 240,000 on one. [Rows lent in
place](../internals/read-scaling.md#rows-lent-in-place) has the measurements.

## Lookups

A `Table` lookup through an index reads each row separately unless you read it through `.covered()`,
and no query planning happens. You choose the indexes, and combine them as described under
[Combining lookups](#combining-lookups). `IndexIter::keys` gives the primary keys without reading
any row.

## How it works

[Schema evolution, unique and covering
indexes](../internals/schema-and-unique-indexes.md#unique-indexes) describes how a clash is refused
and how `Table::merge` resolves one, and how a covered index is stored and rebuilt.
