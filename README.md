# ferro

An experimental embedded database for Rust where the database is one of your own types.

There is no query language, no schema file and no mapping layer. You define a struct, ferro
persists it, and you read it with ordinary Rust. Every commit creates a new immutable version of
that value, and all earlier versions stay readable.

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

## How it behaves

**Reads are plain values.** `db.read()` returns a `Snapshot<T>`, which derefs to `T`. It never
blocks, never changes, and stays valid for as long as you hold it. Collections work like their
standard library counterparts: `get`, `iter`, `range`, `keys`, `values`, `first`, `last`, with
`DoubleEndedIterator` and `ExactSizeIterator`. Filtering, grouping and joining are iterator code.

**Writes are closures.** `update`, `commit` and `commit_with` give you `&mut T`. When the closure
returns, the change commits atomically. If it panics or `try_update` returns `Err`, nothing
happens. Writers run one at a time, so a transaction is serializable by construction.

**Collections are values too.** `Map`, `Set`, `List` and `Table` nest freely inside your types.
Cloning one is O(1), and `==` compares root hashes in O(1) because the tree shape depends only on
the contents. A commit that changes nothing is skipped.

| Type | Use it for |
| --- | --- |
| `Map<K, V>` | Ordered map, with `get_mut` handing out a real `&mut V` |
| `Set<T>` | Ordered set |
| `List<T>` | Append-mostly sequence with indexed access |
| `Table<T>` | Keyed rows with secondary indexes kept up to date for you |

`#[derive(Record)]` generates index lookups directly on the table: `by_status(&value)`,
`by_status_range(range)` and `by_status_counts()`, which needs no row reads. An `Option<T>` index
also gets `by_field_some(&value)`, and `#[index(each)]` indexes every element of a collection
field.

## History, diffs and branches

```rust
let old = db.at(12)?;
let then = db.as_of(unix_ms)?;
for commit in db.log().values().rev() {
    println!("v{} {}", commit.version, commit.message);
}

for change in old.orders.diff(&db.read().orders) {
    println!("{:?} changed", change.key());
}

let versions = db.history_of(|shop| shop.orders.get(&order_id))?;

db.restore(&old)?;
```

`diff` skips identical subtrees by hash, so it costs O(changes), not O(size). `restore` commits
an old value as a new version, so history is never rewritten.

```rust
let what_if = db.fork("what-if")?;
what_if.commit("try a price change", |shop| { /* ... */ })?;

db.merge_with(&what_if, |shop, base, theirs| {
    let applied = shop.orders.merge(&base.orders, &theirs.orders, |conflict| {
        conflict.ours.as_deref().cloned()
    });
    ("merge what-if", applied)
})?;
```

A fork is one small commit whatever the database size. Merges are three-way and cost O(changes).
You decide conflicts in Rust, and you merge each collection in your root yourself.

## Changing your types

Your struct is the schema. Fields are stored by name hash, so old data keeps loading after you
edit the struct:

- Reordering fields is always safe.
- A removed field is skipped when old data is read.
- A new field needs a default: `Option`, `Vec`, `Map`, `Set`, `List` and `Table` start empty,
  and anything else takes `#[ferro(default)]` or `#[ferro(default = "path::to::fn")]`.
- To rename a field, use `#[ferro(rename = "old_name")]`.

Do not change the encoding or `Ord` of a key type on existing data. Tree placement depends on both.

## Durability

The file is an append-only log of content-addressed objects behind two checksummed superblock
slots. A commit writes its objects, fsyncs, writes the superblock, and fsyncs again. Recovery
needs no replay: open picks the newest valid superblock and truncates anything past it, so a crash
loses at most a commit that was never acknowledged.

Every read re-hashes the object against the pointer it came from, so corruption surfaces as an
error, not wrong data. The whole database is a Merkle tree, and `verify` re-hashes all of it.

Other processes can read while one writes: `Options::new().read_only()` takes no lock, and
`refresh` picks up new commits.

Collection reads do not return `Result`, so that they feel native. If the disk fails or the file
is corrupt, they panic. Use `try_get` to handle that case. A panic inside a commit discards it.

## Tools

`ferro` works on any database file without your types:

```sh
cargo run --release -p ferro-cli -- info shop.ferro
cargo run --release -p ferro-cli -- log shop.ferro --limit 20
cargo run --release -p ferro-cli -- verify shop.ferro
cargo run --release -p ferro-cli -- compact shop.ferro --keep-last 1000
```

Compaction is the only way the file shrinks. It runs offline, copies what is still reachable to a
new file, and swaps it in atomically. Versions it drops are gone: `at` then reports `NoVersion`.

## A real example

`examples/tracker` is a complete command line issue tracker built this way. It has filtered
listing, per-issue history, `undo`, `at <version>`, branches and merges, and all of its queries
are iterator code.

```sh
cargo run --release -p tracker -- add "Fix login" --label bug --priority high
cargo run --release -p tracker -- list --label bug
cargo run --release -p tracker -- close 1
cargo run --release -p tracker -- history 1
cargo run --release -p tracker -- undo
```

## Performance

Measured in release on one NVMe SSD with ext4, on 1M `u64` keys with short string values, using
`ferro/examples/perf.rs`. Each is a single run.

| Operation | Result |
| --- | --- |
| Bulk insert in one commit | 1.6 s |
| Random `get` | 1.0 µs |
| Full scan | 53 ns per key |
| Range read | 54 ns per key |
| Single-key commit, fsync | 1.1 ms, about 900 per second |
| Single-key commit, `Durability::Buffered` | 72 µs |
| Bytes written per single-key commit | 8.5 KB |
| `diff` | 5.7 µs per changed key |

The tracker with 100,000 issues in a 39 MB file, as whole process runs including open: seeding in
845 ms, indexed `list` in 2 ms, `show` in 1 ms, durable `close` in 1 ms, `stats` in 3 ms,
`history` of an issue in 3 ms, and a full text scan in 111 ms.

## Limitations

This is an experiment, not a production database.

- The file format is not stable and may change without migration.
- It runs on Unix only.
- Storage only grows until you compact. A single-key commit writes about 8.5 KB at this size, so
  batch related changes into one commit. Secondary indexes add to that.
- One writer at a time. The closure runs under the writer lock, so a long closure blocks every
  branch. Calling `update`, `fork`, `branch`, `branches` or `verify` from inside it deadlocks.
- A held `Snapshot` or `Map` keeps the file open. Reopening as a writer in that state gives
  `Error::Locked`.
- Crash safety was tested with simulated crash images, killed processes and a traced syscall
  order, never with a real power cut. Only log write failure was injected, not fsync failure.
- A corrupted newest superblock cannot be told from a torn write, so open rolls back to the
  previous commit, losing that one acknowledged commit.
- Version 0 is not stored. `at(0)` and forks of an empty database use your current `Default`.
- Each `Table` lookup through an index reads the row separately, and no query planning happens.
  You choose the index.

See `DESIGN.md` for how it works.
