# ferro

An experimental embedded database for Rust where the database is one of your own types.

There is no query language, no schema file and no mapping layer. You define a struct, ferro persists
it in one file, and you read it with ordinary Rust. Every commit creates a new immutable version of
that value, and earlier versions stay readable.

ferro is an experiment, not a production database. Its file format is not stable.

## Key features

- **Your type is the database.** Derive `Data` and open a file of your struct. `db.read()` returns a
  `Snapshot` that derefs to it, and writes are closures over `&mut T` that commit atomically.
- **Persistent collections.** `Map`, `Set`, `List` and `Table` nest inside your type. Cloning is
  O(1), `==` compares root hashes in O(1), and `diff` costs O(changes).
- **Indexes without a query planner.** `#[derive(Record)]` keeps secondary, composite and unique
  indexes on a `Table` up to date and generates their lookups. `find()` combines equality lookups
  with `and` and `or` by key, and a covering index stores chosen fields beside its entries, so a
  page of results reads no rows.
- **Range totals in O(log n).** A `Summary` keeps a fold of every subtree of a map.
- **History.** `at`, `as_of`, `history_of`, `bisect` and `diff` read old versions, and `restore`
  commits one again as a new version.
- **Branches and merges.** A fork is one small commit. Merges are three-way, you resolve conflicts
  in Rust, and the base is found over the merge graph as git does, criss-crosses included.
- **Sync between files.** Two files exchange only the commits the other lacks, in one call or as
  bytes over any transport. Every object received is checked by hash and as its type.
- **Crash safety and integrity.** An append-only log with one fdatasync per batch of commits. Every
  read re-hashes its object, `verify` re-hashes the file, and `head_hash` is a tamper-evidence
  anchor.
- **Readers never wait.** Writers run one closure at a time per branch, closures on different
  branches run in parallel, queued frames share one fsync, and `coalesce` lets queued closures share
  a version.
- **Async without a runtime.** Awaited writes and watcher waits use `std::task::Waker`, so they run
  on any executor.
- **Watchers.** Subscribe to a branch, optionally filtered by a test. A commit costs the same with
  no watchers or ten thousand.
- **Bounded history.** Keep the last N versions or a time window. Reclamation copies what is kept
  into a new file while writers run, pausing them for a few milliseconds.
- **Schema evolution.** Fields are stored by name, so adding, removing, reordering and renaming
  fields needs no migration.
- **One file, several processes and a tool.** Other processes can read while one writes, several
  can write with `Options::shared` on 64-bit Linux, a handle opened with `Options::follow` picks up
  the others' commits and fires its watchers on its own, and the `ferro` tool inspects, verifies and
  compacts any file without your types.

## When to use it

ferro suits a program whose state is naturally one typed value, that wants history, undo, audit
trails or what-if branches without building them, and that fits on one machine. The example apps
include an issue tracker, a booking system, a ledger, a job queue, a chat server and an
offline-first notes app.

What you get for it: queries are iterator code that you test and refactor like the rest of your
program, a transaction is a closure that is serializable by construction, every change can be
explained or undone from the history, and corruption surfaces as an error rather than wrong data.

## Not a good fit (yet)

- **Data you cannot afford to lose.** It is an experiment, and the file format may change without
  migration. Power loss was modelled, never tested on real hardware.
- **Many tiny independent writes.** A single-key commit writes about 3.5 KB at 1M keys, and each
  branch runs one closure at a time. Batch related changes, or use `coalesce`.
- **Many processes on one branch without fsync.** With `Options::shared` several processes commit to
  one file, as fast as threads under fsync, but under `Buffered` each reads back what the others
  wrote, so on one branch they commit more slowly than threads. It needs 64-bit Linux and one
  machine.
- **Ad hoc queries.** There is no query language or planner. You choose the indexes, `and` and
  `or` combine equality lookups by key, a range joins them only after its keys are read and sorted,
  and each matching row is read separately unless the index covers the fields it needs.
- **Very large files.** A reclamation cycle copies everything retained at 450 MB/s to 2.2 GB/s on
  four threads and needs that much free disk, and a first sync keeps about 40 bytes per object in
  memory on each side, about 2 GB for 50 million objects.
- **Windows beyond NTFS.** Other file systems are not supported, performance on Windows was never
  measured, and failed syncs and process death are tested on Unix only.
- **Adversaries.** Hashes are 160-bit and unkeyed, so collision resistance is 2^80, and `head_hash`
  is not a signature.

[Limitations](docs/guide/limitations.md) has the full list.

## Example

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

fn main() -> ferro::Result<()> {
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
    assert_eq!(before.orders.by_status(&Status::Pending).len(), 1);
    assert_eq!(shop.orders.by_status(&Status::Pending).len(), 0);
    for order in shop.orders.by_customer("wile").filter(|order| order.total_cents > 1000) {
        println!("{order:?}");
    }
    Ok(())
}
```

`make quickstart` runs a longer version,
[`ferro/examples/quickstart.rs`](ferro/examples/quickstart.rs). [Getting
started](docs/guide/getting-started.md) walks through it.

## Build, test and benchmark

The `Makefile` wraps the common commands, and `make help` lists every target.

| Command | Does |
| --- | --- |
| `make build` | Builds the workspace in release. |
| `make test` | Runs every test in the workspace, in release. |
| `make test-one T=sync` | Runs the library tests whose names match `T`. |
| `make ci` | Checks formatting, runs clippy for Linux, Windows and the fuzz crate, builds the API docs, then every test. |
| `make bench` | Runs the main benchmark and appends to `target/bench/results.jsonl`. |
| `make bench-compare` | Compares ferro with SQLite, redb and fjall. |
| `make fuzz FUZZ=open` | Fuzzes one target with nightly, after `make fuzz-seeds`. |

Cargo runs inside a `systemd-run` scope capped at `MEM` (16G by default), so a runaway test cannot
take the machine down. Set `CAP=` to run without the cap. [Development](docs/guide/development.md)
lists every target.

## Example apps

Each is a workspace package, run with `cargo run --release -p <name>`. `tracker`, `shop`, `chat`,
`notes` and `booking` also have `make` targets of the same name that take `ARGS`.

- `examples/tracker`: a command-line issue tracker with history, undo, branches and merges.
- `examples/shop`: an axum shop with products, carts and orders, and the `shop-load` load tester.
- `examples/chat`: an axum multi-room chat with live updates over Server-Sent Events.
- `examples/booking`: hotel reservations with holds, unique indexes, receipts and what-if forks.
- `examples/notes`: offline-first notes kept per device and synced between files.
- `examples/queue`: a durable job queue that uses `coalesce` and filtered watchers.
- `examples/ledger`: a double-entry ledger whose balances are summaries.
- `examples/metrics`: a time-series metrics store with rollups in a summarized map.
- `examples/wiki`: a wiki and document store with drafts on branches.
- `examples/graph`: a social graph service with pagerank and reach queries.

[Example apps](docs/guide/examples.md) has more on each, with load-test results.

## Documentation

The [documentation index](docs/README.md) lists every file.

- Guides: [getting started](docs/guide/getting-started.md),
  [collections](docs/guide/collections.md), [tables and indexes](docs/guide/tables.md), [history and
  branches](docs/guide/history.md), [merging](docs/guide/merging.md),
  [watchers](docs/guide/watchers.md), [sync](docs/guide/sync.md),
  [durability](docs/guide/durability.md), [retention](docs/guide/retention.md),
  [concurrency](docs/guide/concurrency.md), [changing your types](docs/guide/schema-evolution.md),
  [the `ferro` tool](docs/guide/cli.md), [performance](docs/guide/performance.md),
  [limitations](docs/guide/limitations.md) and [development](docs/guide/development.md).
- Internals: [design overview](docs/internals/overview.md), then one file per subsystem, from
  [storage](docs/internals/storage.md) to [reclamation](docs/internals/reclamation.md), and an index
  of [alternatives measured and rejected](docs/internals/alternatives.md).
