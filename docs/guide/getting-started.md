# Getting started

ferro is an experimental embedded database for Rust where the database is one of your own types.

There is no query language, no schema file and no mapping layer. You define a struct, ferro persists
it, and you read it with ordinary Rust. Every commit creates a new immutable version of that value,
and all earlier versions stay readable.

## Adding ferro

ferro lives in this workspace with its derive macros in `ferro-derive`, which it re-exports. The
example apps depend on it by path:

```toml
[dependencies]
ferro = { path = "../../ferro" }
```

## A first database

```rust
use ferro::{Data, Db, Map, Record, Table};

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Pending,
    Shipped,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Order {
    #[key]
    id: u64,
    #[index]
    customer: String,
    #[index]
    status: Status,
    total_cents: u64,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Shop {
    stock: Map<String, u32>,
    orders: Table<Order>,
    next_order: u64,
}

let db = Db::<Shop>::open("shop.ferro")?;

db.commit("receive stock", |shop| {
    shop.stock.insert("anvil".to_string(), 5);
})?;

let placed = db.commit_with(|shop| {
    *shop.stock.get_mut("anvil").unwrap() -= 1;
    shop.next_order += 1;
    let id = shop.next_order;
    shop.orders.insert(Order {
        id,
        customer: "wile".to_string(),
        status: Status::Pending,
        total_cents: 4999,
    });
    (format!("order #{id}"), id)
})?;

let before = db.read();

db.commit("ship", |shop| {
    shop.orders.get_mut(&placed.value).unwrap().status = Status::Shipped;
})?;

let shop = db.read();
let big: Vec<_> = shop
    .orders
    .by_customer("wile")
    .filter(|order| order.total_cents > 1000)
    .collect();

assert_eq!(before.orders.by_status(&Status::Pending).len(), 1);
assert_eq!(shop.orders.by_status(&Status::Pending).len(), 0);
```

A runnable version is in `ferro/examples/quickstart.rs`:

```sh
cargo run --release -p ferro --example quickstart
```

`make quickstart` runs the same.

## Reading

`db.read()` returns a `Snapshot<T>`, which derefs to `T`. It never blocks, never changes, and stays
valid for as long as you hold it. Collections work like their standard library counterparts: `get`,
`iter`, `range`, `keys`, `values`, `first`, `last`, with `DoubleEndedIterator` and
`ExactSizeIterator`. Filtering, grouping and joining are iterator code.

`get` and the iterators hand out a `Val<V>`, which derefs to `V`. A value with no destructor and at
most 16 bytes, such as an integer, a `()` or a pair of `u64`, is copied into it. Any other `Val`
shares its node's column of values and keeps that column alive, so call `into_owned` before holding
many of them for long.

`Snapshot::version` and `Snapshot::commit` name the version a snapshot shows. Reading in bulk,
cursors and summaries are in [Collections](collections.md), and index lookups in [Tables and
indexes](tables.md).

## Writing

`update`, `commit` and `commit_with` give you `&mut T`. When the closure returns, the change commits
atomically. If it panics or `try_update` returns `Err`, nothing happens. Writers run one at a time,
so a transaction is serializable by construction. `commit_with` and its `try_` form return a
`Committed` with your value and the `version` and `time_ms` of the commit that holds the change, or
of the head it ran on if it changed nothing.

A `try_` closure can return any error type that converts from `ferro::Error`. When its errors all
come through `?` and the caller uses `?` on the result too, Rust cannot tell which type is meant, so
name it on the closure:

```rust
let done = db.try_commit_with(|shop| -> Result<_, ShopError> {
    let order = shop.orders.get(&id).ok_or(ShopError::NoOrder(id))?;
    Ok((format!("look at #{id}"), order.total_cents))
})?;
```

A write can also be awaited from async code, as described in [Writers and
concurrency](concurrency.md#awaited-writes).

## Options

`Db::open(path)` opens or creates a file with the defaults below. `Options` changes them, and
`Options::open::<T>(path)` opens with the result:

| Option | Default | Effect |
| --- | --- | --- |
| `durability(Durability)` | `Durability::Fsync` | `Buffered` skips the fsync on each commit. See [Durability and crash safety](durability.md). |
| `checkpoint_bytes(u64)` | 64 MiB | Under `Buffered`, how much unsynced log a checkpoint covers. |
| `cache_bytes(u64)` | 1 GiB | Approximate bound on the decoded node cache. See [Cache weight](../internals/read-scaling.md#cache-weight). |
| `read_only(bool)` | `false` | Opens without the writer lock. See [Writers and concurrency](concurrency.md#several-processes). |
| `history(Keep)` | `Keep::All` | How much history to retain. See [Retention and reclamation](retention.md). |
| `reclaim(Reclaim)` | `every_bytes: 64 << 20` | When background reclamation cycles run. |
| `coalesce(bool)` | `false` | Lets queued closures share one version. See [Sharing a version between writers](concurrency.md#sharing-a-version-between-writers). |
| `shared(bool)` | `false` | Lets other processes that open the file with it write too. See [Several processes](concurrency.md#several-processes). |
| `follow(bool)` | `false` | Keeps a read-only handle or a shared writer up to date with other processes' commits on a thread of its own. See [Following other processes](concurrency.md#following-other-processes). |
| `lock_wait(Duration)` | zero | How long `open` waits for another writer to close before it returns `Error::Locked`. |
| `copy_threads(usize)` | cores, up to 8 | Threads a reclamation cycle copies and checks on. See [Retention and reclamation](retention.md). |

`open` needs `T: Default`, since version 0, the value before the first commit, is `T::default()`.
`Db::open_with(path, init)` and `Options::open_with` take that value from `init` instead.

## Next

- [Collections](collections.md) and [Tables and indexes](tables.md) for what goes inside your type.
- [History, versions and branches](history.md) for reading old versions.
- [Limitations](limitations.md) before you rely on it.
