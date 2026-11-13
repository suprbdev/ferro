# Tables and indexes

A `Table<T>` holds rows keyed by the field marked `#[key]`, which cannot be `#[ferro(skip)]`, and
keeps a secondary index for every field marked `#[index]`. `#[derive(Record)]` generates the
indexes and their lookups. The `Order` row in [Getting started](getting-started.md#a-first-database)
is an example.

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
transaction. A panic in `f`, or while a `RowMut` is held, writes nothing back, so the table keeps
the row as it was. `Map::update` works the same way on a value, while a change made through the
plain `&mut V` that `Map::get_mut` or `List::get_mut` hands out stays made, as through any `&mut`.

## Secondary indexes

`#[derive(Record)]` generates index lookups directly on the table: `by_status(&value)`,
`by_status_range(range)` and `by_status_counts()`, which needs no row reads.
`by_status_after(&value, &key)` continues after a row's key within that value, so a filtered page
costs a seek, not a walk over the rows already shown. An index on a field written `Option<T>` also
gets `by_field_some(&value)` and `by_field_some_after(&value, &key)`, which take a `T`, and
`#[index(each)]` indexes every element of a collection field.

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
and `.keys()` and `.covered()` read the index alone. A row whose field is `None` is in none of them
and clashes with no other, however the field's `Option` is written: as `Option<T>`, through a
`type` alias, a `Box` or a `#[ferro(transparent)]` wrapper. On a field written `Option<T>`, the
lookups and the range take a `T`. A derive cannot see through an alias or a wrapper, so on those
they take the field's own type, as `key_by_code(&Some(7))`, with the same rows. There is no
`by_slug_after` or `by_slug_counts`, since each value has one row: the page after a slug is
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
index ran 855,000 a second on one thread where a plain index ran 183,000, and 9.3 million on 32
where it ran 1.5 million, and a lookup of 20 rows by a range 2.5 million on one thread where it ran
232,000. Covering one integer grew the indexes 17%, and a commit that changed it wrote 36% more and
took 28% more CPU. A commit that changed nothing covered cost the same. Cover what a hot page shows
and what seldom changes after the row is written, and leave wide strings and fields rewritten on
most commits out. A plain `#[index]` is unchanged and costs nothing more. [Covering
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

Every lookup knows how many keys it holds as soon as it is built, counted from the counts the
index's nodes keep, without reading its entries, and a combination picks how to read its lookups
from those counts the first time it is read, not from the order you wrote them in. A chain of `and`,
such as `a.and(b).and(c)`, is one `and` of up to 64 lookups: the one with the fewest keys leads,
those with less than four times its keys are walked beside it, and the rest are looked up, fewest
keys first, each only for a key every lookup before it holds. Written in another order, a chain
yields the rows and covered fields of its leftmost lookup, and costs the same, and an `and` nested
on the right, as in `a.and(b.and(c))`, joins the chain as its lookups, so it plans as
`a.and(b).and(c)` does. On 200,000 rows, measured on 2026-11-11, a lookup of 100,000 keys, then one
of 1,000, then one of 1,000 took 12 µs, the same as written smallest first, as [What planning
measured](../internals/schema-and-unique-indexes.md#what-planning-measured) describes. `explain()`
shows the plan without reading anything, one line a lookup, each side indented below its `and` or
`or`:

```rust
let find = shop.orders.find();
let plan = find
    .by_status(&Status::Pending)
    .and(find.by_customer("wile"))
    .and(shop.orders.by_day_range(first..=last))
    .explain();
assert_eq!(plan.sides[1].read, Read::Walked);
println!("{plan}");
```

```text
and, at most 12 keys, walked
  value, 48210 keys, searched
  value, 12 keys, walked
  range, 9124 keys, tested
```

An `or`, and a combination already read from, joins a chain as one side, planned on its own, and
a chain of 64 lookups given another becomes one side of a new chain.

`and` takes any lookup and keeps the covered fields of its left side, but `or` yields a key that
only its right side holds with that side's covered fields, so it compiles only when both sides cover
the same fields. Call `.uncovered()` on each side to combine lookups of differently covered indexes:

```rust
let either: Vec<_> = find
    .by_customer("wile")
    .uncovered()
    .or(find.by_status(&Status::Pending).uncovered())
    .keys()
    .collect();
```

A lookup made on the table itself, such as `by_x_range` or `by_x_prefix` of a unique or a non-unique
index, can be passed to `and` or `or` too. It is not in key order, so when the keys the rest of the
`and` leads to it are at most a quarter of its own, it is never walked, and each of those keys is
tested against the range by reading its row. With `day` indexed:

```rust
let find = snapshot.orders.find();
let this_month: Vec<_> = find
    .by_customer("wile")
    .and(snapshot.orders.by_day_range(first..=last))
    .collect();
```

On 200,000 rows, measured on 2026-11-06, 10 keys against a range of 100,000 took 1.5 µs where
reading and sorting the range took 2.4 ms, and 1,000 keys took 106 µs against 2.5 ms. Each key
tested costs a row read, about 100 ns with the rows cached, so `.keys()` and `.covered()` read
those rows too, though they yield none. A row read from the file costs dozens of times as much,
so once such reads come to about what reading the range would cost, the combination reads the
range's keys for the rest instead. When the range has less than four times the keys led to it, or
gives the combination its covered fields, its keys are read in full and sorted first, at 18 to 30
ns a key. Pass the range through `.uncovered()` to have it read that way from the start, reading no
rows for it. A combination reads the indexes and rows as they were when you built it, like any
lookup. [Combining lookups](../internals/schema-and-unique-indexes.md#combining-lookups) has the
measurements.

A range written first, as `orders.by_day_range(first..=last).into_matches().and(...)`, is tested
too when another side of the `and` reads the same table at the same version, since the combination
reads its rows through the range's own finder and the range borrows that side's. That side can be
an `or`, which lends the finder of a range it reads in full, and only a side whose every range is
itself tested has none to lend. A lookup of an `and` nested on the right lends like any other side
of the chain, as long as the nested `and`'s first lookup reads that table at that version as well.
On 2026-11-11, 10 keys against a range of 100,000 written that way took 1.4 µs, where reading the
range took 2.4 ms. Of two ranges, the one with fewer keys leads and the other is tested. In a chain,
the keys led to a range are estimated from the lookups walked before it, as if they matched
independently, so two lookups of 50,000 keys that share 12,500 and a range of 100,000 took 3.0 ms,
where reading the range in full took 4.6 ms. A range tests each key led to it once, from whichever
end the combination is read, so the tests stop once they number a quarter of the range's keys only
when the estimate was too low, and then cost about what reading the range would, as rows read from
the file do.

Lookups of two tables of the same row type, or of two versions of one table, such as a snapshot's
and the one inside a commit, combine as well. Each answers for the table and version it was made
on, and each row comes from a lookup that holds its key: the left side of an `and`, and the
leftmost side of an `or` that holds it, the lookup `.covered()` takes the fields from too. So an
`or` yields every row either table holds, and a row both hold from the left one:

```rust
let wile: Vec<_> = snapshot
    .orders
    .find()
    .by_customer("wile")
    .or(snapshot.archived.find().by_customer("wile"))
    .collect();
```

An `and` of two tables yields the left table's rows whose keys the right table's lookup holds as
well. Lookups of one table at one version read their rows as a single lookup does. On 200,000 rows,
measured on 2026-11-06, an `or` of two lookups of 50,000 rows took 6.8 ms read as rows, and 7.4 ms
with the second lookup made on a newer version of the table, since each version's rows are read
through a finder of their own. [Lookups of several
tables](../internals/schema-and-unique-indexes.md#lookups-of-several-tables) describes how, and
has the measurements.

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

`IndexIter`, `Matches` and the `table::Iter` that `iter`, `range` and `prefix` return all have both,
and a map's `Iter` lends `(&K, &V)` the same way. Read rows this way where the code uses a row where
it finds it: a page that copies the fields it shows, a total, a filter, a hot path that many threads
run over the same rows. Keep `next` and the iterator adapters where the rows themselves go on, into
a `Vec`, a channel or a response built later, since a lent row lasts only until the next step and a
clone of the row costs more than a `Val`. The loop around `next_ref` can `break`, `return` or use
`?` as it likes, and the lookup goes on from the next row.

On 200,000 rows, measured on 2026-11-05, 32 threads reading pages of the same 200 rows through
`next_ref` read 4.1 million pages a second where `next` read 1.5 million, and one thread 305,000
where it read 182,000. A lookup of about 20 scattered rows through `visit` read 5.1 million a second
on 32 threads where `next` read 4.5 million, and 240,000 on one where it read 236,000. [Rows lent in
place](../internals/read-scaling.md#rows-lent-in-place) has the measurements.

## Lookups

A `Table` lookup through an index reads each row separately unless you read it through `.covered()`.
You choose the indexes. A combination of them picks which lookup leads and how the others are read
from the keys each counts in its index, as described under [Combining lookups](#combining-lookups),
but it has no statistics of the values and no cost model of your data. `IndexIter::keys` gives the
primary keys without reading any row.

## How it works

[Schema evolution, unique and covering
indexes](../internals/schema-and-unique-indexes.md#unique-indexes) describes how a clash is refused
and how `Table::merge` resolves one, and how a covered index is stored and rebuilt.
