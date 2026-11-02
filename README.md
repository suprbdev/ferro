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
the contents. A commit that changes nothing is skipped. `Map::root_hash` and `Table::root_hash`
expose those hashes, `Map::value_hash(&key)` gives the hash of one value, and `hash_of(&value)` the
hash of any `Data` value, which makes an ETag one line.

| Type | Use it for |
| --- | --- |
| `Map<K, V>` | Ordered map, with `get_mut` handing out a real `&mut V` |
| `Set<T>` | Ordered set |
| `List<T>` | Append-mostly sequence with indexed access |
| `Table<T>` | Keyed rows with secondary indexes kept up to date for you |

`#[derive(Record)]` generates index lookups directly on the table: `by_status(&value)`,
`by_status_range(range)` and `by_status_counts()`, which needs no row reads.
`by_status_after(&value, &key)` continues after a row's key within that value, so a filtered page
costs a seek, not a walk over the rows already shown. An `Option<T>` index also gets
`by_field_some(&value)` and `by_field_some_after(&value, &key)`, and `#[index(each)]` indexes every
element of a collection field.

## History, diffs and branches

```rust
let old = db.at(12)?;
let then = db.as_of(unix_ms)?;
for commit in db.log().values().rev().take(20) {
    println!("v{} {}", commit.version, commit.message);
}

for change in old.orders.diff(&db.read().orders) {
    println!("{:?} changed", change.key());
}

let life: Vec<_> = db
    .history_of(|shop| shop.orders.get(&order_id))
    .take_while(|(_, row)| row.is_some())
    .collect();

db.restore(&old)?;
```

`diff` skips identical subtrees by hash, so it costs O(changes), not O(size). `restore` commits
an old value as a new version, so history is never rewritten.

`history_of` walks the log newest first and yields `(commit, value)` for each commit that changed
the projection, including a change back to an earlier value. It reads nothing until you ask and
stops when you stop, so `take`, `find` and the `take_while` above cost the commits actually
walked: for that `take_while`, the age of the row, not the length of the log. To show changes
oldest first, collect and reverse. `log().iter()` runs in version order like any collection, and
`.rev()` walks newest first the same lazy way.

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
You decide conflicts in Rust, and you merge each collection in your root yourself. Repeat merges
between two branches, in either direction, do not apply a change twice.

`db.drop_branch("what-if")` removes a branch you are done with. It refuses `main` and the branch of
the handle you call it on. Other handles on that branch keep reading what they had, commits through
them fail with `Error::NoBranch`, and its watchers end. The bytes stay in the file until the next
reclamation cycle.

```rust
let mut seen = db.read();
for snapshot in db.watch() {
    for change in seen.orders.diff(&snapshot.orders) {
        println!("{:?} changed", change.key());
    }
    seen = snapshot;
}
```

A `Watcher` does not queue. `recv` returns the newest snapshot it has not returned yet, so a slow
consumer skips versions, costs the writers nothing and uses no memory, and `diff` covers whatever
it skipped. `recv_timeout` and `try_recv` work like `std::sync::mpsc`. A watcher ends when the last
`Db` handle goes, and it does not keep the file open: a snapshot it had not taken by then is lost.
A read-only handle fires its watchers on `refresh`.

`db.head_hash()` is a hash over the whole history of a branch, for publishing somewhere an attacker
cannot edit. Check an old anchor with `db.log().hash_at(&version)` and `db.verify()`. A match shows
that version and everything before it are unchanged. It is not a signature and says nothing about
later commits, other branches or whether timestamps were true. See `DESIGN.md`.

## Keeping history bounded

Every version stays readable by default, so the file grows with every commit. Most programs want
recent history, not all of it:

```rust
use ferro::{Keep, Options, Reclaim};

let db = Options::new()
    .history(Keep::Last(1000))
    .reclaim(Reclaim { every_bytes: 64 << 20, ..Reclaim::default() })
    .open::<Shop>("shop.ferro")?;

let space = db.space()?;
println!("{} allocated, about {} live", space.allocated_bytes, space.live_bytes);

let report = db.compact(Keep::Last(10))?;
println!("freed down to {}, writers waited {:?}", report.after_bytes, report.pause);
```

`history` prunes as part of each commit and runs reclamation on a background thread whenever the log
has grown by `every_bytes` or by the live size, whichever is larger. `Keep::Since(unix_ms)` and
`Keep::Within(duration)` retain by time instead, and take effect at each cycle. `compact` runs one
cycle now, with writers still running, and can prune harder than `history` does.

A pruned version is gone: `at` returns `NoVersion` and `log` stops at the oldest retained commit.
`head_hash` does not change, because retained commits are not rewritten, and `merge_with` falls back
to the fork point, or returns `NoCommonAncestor` if that was pruned too.

A held `Snapshot`, `Log` or `History` keeps working for as long as you hold it, even past pruning,
and keeps its objects from being freed. A `Map` cloned out of a snapshot does not: once its version
is pruned and reclaimed, reading it or committing it returns `Error::Pruned`. A read-only process
follows moved objects by itself, but cannot keep a version alive, so one that lags behind the
retained window gets `Error::Pruned` until it calls `refresh`.

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
slots. A commit writes one checksummed frame holding its objects, fsyncs once, and then updates
the superblock without syncing it. Open starts from the newest valid superblock, walks forward over
every complete frame, and truncates the rest, so a crash loses at most a commit that was never
acknowledged. Concurrent writers share a sync: one leader's fsync covers everything queued.

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
cargo run --release -p ferro-cli -- space shop.ferro
cargo run --release -p ferro-cli -- reclaim shop.ferro --keep-last 1000
cargo run --release -p ferro-cli -- compact shop.ferro --keep-last 1000
```

`reclaim` frees dead space in place. `compact` copies what is reachable to a new dense file and
swaps it in, which needs no writer running and is the way to get a file that copies small. See
[Keeping history bounded](#keeping-history-bounded) for doing this from a running program.

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
`ferro-bench`. Each is a single run.

| Operation | Result |
| --- | --- |
| Bulk insert in one commit | 1.6 s |
| Random `get` | 1.0 µs |
| Full scan | 53 ns per key |
| Range read | 54 ns per key |
| Single-key commit, fsync | 460 µs, about 2,200 per second |
| Single-key commits, fsync, 8 writers | about 9,800 per second |
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
- A single-key commit writes about 8.5 KB at this size, so batch related changes into one commit.
  Secondary indexes add to that. Without retention the file only grows.
- Reclamation punches holes, so the log's apparent size keeps growing while its allocated size stays
  bounded. `ls -l` shows the former and `du` the latter. A copy with a tool that does not preserve
  holes, such as `scp` or `rsync` without `--sparse`, writes the apparent size. ext4 stops a file at
  16 TiB of apparent size, at which point a commit fails with an I/O error and offline `compact`
  resets it. A filesystem that cannot punch holes keeps its space until `compact`.
- One writer closure at a time. The closure runs under the writer lock, so a long closure blocks
  every branch. Only the fsync is shared, so commit throughput stops scaling at the cost of the
  closure and encoding. Calling `update`, `fork` or `drop_branch` from inside it deadlocks.
- A held `Snapshot` or `Map` keeps the file open. Reopening as a writer in that state gives
  `Error::Locked`.
- A `Map` cloned out of a snapshot does not keep its version alive, and a read-only handle in another
  process cannot pin anything. Either gets `Error::Pruned` once its version has been pruned and
  reclaimed, which `get` turns into a panic. `refresh` fixes the read-only handle.
- Crash safety was tested with simulated crash images, killed processes and a traced syscall
  order, and fsync failure and power cuts were only modelled by intercepting `fdatasync`, never
  tested on real hardware.
- A frame past the superblock that is damaged cannot be told from a torn write, so open stops
  there and truncates it, losing at most the commits of the last sync. Bit rot further back is
  reported by the hash checks, not truncated.
- Only direct merges are recorded. A change that arrives through a third branch, or through a
  fork of an already merged branch, is seen again. Nothing is lost, but a merge closure that is
  not idempotent, such as one adding a counter delta, applies it twice.
- Version 0 is not stored. `at(0)` and forks of an empty database use your current `Default`.
- Each `Table` lookup through an index reads the row separately, and no query planning happens.
  You choose the index.

See `DESIGN.md` for how it works.
