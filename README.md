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

`get` and the iterators hand out a `Val<V>`, which derefs to `V`. A small value with no heap of its
own, such as an integer, a `()` or a struct of integers, is copied into it. Any other `Val` shares
its node's column of values and keeps that column alive, so call `into_owned` before holding many of
them for long.

**Reads in bulk.** `sum`, `count`, `for_each` and other folds on the plain iterators take whole
nodes at a time. `Map::for_each`, `try_for_each`, `for_each_range` and `try_for_each_range` lend
`&K` and `&V`, so they clone nothing, and the `try_` forms stop on `ControlFlow::Break`.
`Map::cursor` gives a `Cursor` that keeps its path through the tree, so `seek` and `seek_back` cost
the distance moved instead of a walk from the root, which suits sorted lookups and merge joins.
`get_many` and `contains_many` run a cursor over an iterator of keys, and keys out of order still
give the right answer, at the cost of a fresh walk each. `Map::floor` finds the last key at or
before a key in one walk.

```rust
frontier.sort_unstable();
let reached: u64 = degrees
    .get_many(&frontier)
    .filter_map(|(_, count)| count)
    .map(|count| u64::from(*count))
    .sum();

let mut shared = 0;
let mut other = right.cursor();
left.for_each(|key, _| shared += usize::from(other.seek(key)));
```

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
| `Map<K, V, S>` | The same, keeping a `Summary` of every subtree for O(log n) range totals |
| `Set<T>` | Ordered set, whose `range` has a `len` like the rest |
| `List<T>` | Append-mostly sequence with indexed access |
| `Table<T>` | Keyed rows with secondary indexes kept up to date for you |
| `Bytes` | Byte buffer value with `Deref<Target = [u8]>` and O(1) clone |

`Map` and `Set` also have `remove_range`, `take_range`, `split_off` and `append`, which cut and
join trees in O(log n) nodes however many entries move. `Table::drain_range` removes a key range
and returns the rows, which is O(rows) because every index has to be updated. `Map::apply` takes a
batch of puts and deletes and merges it in one descent of the tree, as `extend` does for puts.

A `Summary` folds entries in key order, and `Map<K, V, S>` keeps that fold for every subtree, so a
total over any key range takes O(log n) instead of a scan. A plain `Map<K, V>` pays nothing for this.

```rust
#[derive(Clone, Copy, Data)]
#[ferro(transparent)]
struct Net(i128);

impl Summary<(u32, i32), i64> for Net {
    fn of(_key: &(u32, i32), amount: &i64) -> Self {
        Net(i128::from(*amount))
    }

    fn combine(&self, other: &Self) -> Self {
        Net(self.0 + other.0)
    }
}

let postings: Map<(u32, i32), i64, Net> = Map::default();
let balance = postings
    .range_summary((account, i32::MIN)..=(account, date))
    .map_or(0, |net| net.0);
```

`combine` must be associative and is applied in key order, so it need not be commutative: first,
last, min and max work. `of` must depend only on what the key and value encode. An empty range gives
`None`. `summary` reads the whole map's fold in O(1), and `summary_by` takes predicates, as
`range_by` does. A summary costs bytes on each commit, 8% for an `i64` total and more as it widens, so
keep it small. Use a tuple rather than a `derive(Data)` struct, whose field tags repeat on every
link.

`#[derive(Record)]` generates index lookups directly on the table: `by_status(&value)`,
`by_status_range(range)` and `by_status_counts()`, which needs no row reads.
`by_status_after(&value, &key)` continues after a row's key within that value, so a filtered page
costs a seek, not a walk over the rows already shown. An `Option<T>` index also gets
`by_field_some(&value)` and `by_field_some_after(&value, &key)`, and `#[index(each)]` indexes every
element of a collection field.

`#[index(a, b)]` on the struct indexes a combination of fields in that order. It gets the same
`by_a_b(&(a, b))`, `by_a_b_range` and `by_a_b_counts`, and `by_a_b_prefix(&a)` for every row with
that `a`, ordered by `b`.

`#[unique]` on a field, or `#[unique(a, b)]` on the struct, keeps a value held by one row at most.
`by_slug(&value)` returns the row, `key_by_slug(&value)` only its key, and `unique_slug()` the whole
index. `try_insert` and `try_update` return `Err(Taken { index, holder })` on a clash. `insert`,
`update` and `get_mut` have no error channel, so they panic, which discards the transaction.
`Table::merge` passes a clash to your `resolve` closure with `ours` set to the row holding the value,
and asks about each key once at most. A row that still clashes after that keeps our row for its key,
so a merge never panics on a clash.

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

Three forms take a range of versions, for when you know where to look or can find out:

```rust
let created = db.bisect(.., |shop| shop.orders.contains(&order_id))?.expect("placed");
let ended = db.bisect(created.., |shop| !shop.orders.contains(&order_id))?;
let alive = created..ended.unwrap_or(u64::MAX);

let week: Vec<_> = db.history_of_range(from..=to, |shop| shop.orders.get(&order_id)).collect();
let edits = db.bisect_changes(alive, |shop| shop.orders.get(&order_id))?;
```

`bisect` finds the first version where a test holds, for a test that stays true once it turns true,
in about log2 of the log's length in probes. `history_of_range` is `history_of` between two versions
and costs the span. `bisect_changes` cuts the range in half until the ends agree, so it costs the
changes instead of the span, but it can miss a change and its reversal that fall between two probes,
so use it only for a value that never returns to an earlier one in that range. A row's absence comes
back when the row goes, which is why `alive` stops short of `ended`. It returns oldest first.

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

```rust
let watcher = db.watch_when(|shop| shop.stock.get("anvil").is_some_and(|left| *left == 0));
let closer = watcher.closer();
let worker = watcher.clone();
std::thread::spawn(move || {
    for snapshot in worker {
        handle(&snapshot);
    }
});
closer.close();
```

`watch_when` sleeps through commits whose snapshot fails the test. The test runs as the commit is
published, only while a watcher is parked, on the publishing thread once it holds no lock, so a test
may read, commit or refresh. The `update`, `drop_branch`, `compact` or `refresh` call that published
returns only after its tests have run, so keep them quick. `closer` gives a cloneable handle that
ends a watcher and its clones from any thread without a commit. Clones share the closer and the test
and keep their own place.

`db.head_hash()` is a hash over the whole history of a branch, for publishing somewhere an attacker
cannot edit. Check an old anchor with `db.log().hash_at(&version)` and `db.verify()`. A match shows
that version and everything before it are unchanged. It is not a signature and says nothing about
later commits, other branches or whether timestamps were true. See `DESIGN.md`.

## Syncing between files

Two files that share a history, or have none yet, can exchange commits, so each device works offline
and catches up later.

```rust
let laptop = Db::<Shop>::open("laptop.ferro")?;
let phone = Db::<Shop>::open("phone.ferro")?;

let pulled = laptop.pull(&phone, "phone", |shop, base, theirs| {
    let applied = shop.orders.merge(&base.orders, &theirs.orders, |conflict| newer(conflict));
    ("sync from the phone", applied)
})?;
println!("{:?}, {} bytes in", pulled.outcome, pulled.received_bytes);
```

`pull` copies the other file's branch into this one as the branch `phone`, then brings this branch up
to it. The outcome is `UpToDate`, `Ahead` when this branch already has all of it, `FastForward` when
this branch has nothing it lacks, or `Merged`. Only a merge calls your closure, which works as in
`merge_with`, against the newest state both branches have passed through. `push` is `pull` from the
other side.

Over a network the same pull is three calls, and only bytes cross: the phone sends its `have`, the
laptop answers with a `bundle`, and the phone applies it with `receive`.

```rust
let have: Vec<u8> = phone.have()?.into();
let bundle: Vec<u8> = laptop.bundle(&Have::from(have))?.into();
phone.receive("laptop", &Bundle::from(bundle), merge)?;
```

A have lists the commits the pulling file holds, a few hundred bytes. A bundle carries the commits
it lacks and the objects their trees reach that it does not hold, so it costs what changed: three
commits on a 9 MB file of 100,000 keys sent 15 KB. The first pull into an empty file carries
everything. `receive` hashes every object and checks every link before it writes, rejects a damaged
or foreign bundle with `Error::Bundle` without touching the file, and applies it in one frame, so a
crash leaves the previous head.

A pulled commit keeps the version, time, message and hash it had. A fast-forward adopts the other
branch, so afterwards `head_hash` is the same in both files. A merge commits at this branch's next
version, and the other side's commits stay on the `phone` branch under their own numbers, so after
a divergence compare commits across files by hash, not by version. A pull that brings new commits
replaces the `phone` branch, so do not commit to it. A bundle that arrives late or twice changes
nothing.

Devices converge only if your closure gives the same result whichever side is `ours`, as
last-writer-wins on a clock does. `examples/notes` is an offline-first notes app built this way:
each device directory holds its own file, and `sync` exchanges changes both ways.

```sh
cargo run --release -p notes -- --device laptop add "buy milk" --tag home
cargo run --release -p notes -- --device phone sync laptop
cargo run --release -p notes --bin notes-scale
```

## Keeping history bounded

Every version stays readable by default, so the file grows with every commit. Most programs want
recent history, not all of it:

```rust
use ferro::{Keep, Options, Reclaim};

let db = Options::new()
    .history(Keep::Last(1000))
    .reclaim(Reclaim { every_bytes: 64 << 20 })
    .open::<Shop>("shop.ferro")?;

let space = db.space()?;
println!("{} on disk, about {} live", space.file_bytes, space.live_bytes);

let report = db.compact(Keep::Last(10))?;
println!("down to {}, writers waited {:?}", report.after_bytes, report.pause);
```

`history` prunes as part of each commit and reclaims on a background thread whenever the file has
grown by `every_bytes` or by its size after the last cycle, whichever is larger.
`Keep::Since(unix_ms)` and `Keep::Within(duration)` retain by time instead, and take effect at each
cycle. `compact` runs one cycle now, with writers still running, and can prune harder than `history`
does. A cycle copies what is retained to a new file and swaps it in. Writers wait a few milliseconds
for the swap, and readers do not wait at all.

A pruned version is gone: `at` returns `NoVersion` and `log` stops at the oldest retained commit.
`head_hash` does not change, because retained commits are not rewritten, and `merge_with` uses the other
branch's merge record if that is still retained, then the fork point, and returns
`NoCommonAncestor` only when none of them is.

Nothing you hold ever breaks. A `Snapshot`, a `Map` cloned from one, a `Log` or a `History` keeps
reading the file it came from, even past pruning, and a read-only process does the same until it
calls `refresh`. The price is that the old file stays on disk until the last such handle is dropped,
which `space.retired_bytes` shows. Committing a `Map` held from before a cycle writes it again by
value, under the writer lock, so `restore` of a snapshot held across a cycle costs time proportional
to its size.

## Sharing a version between writers

```rust
let db = Options::new().coalesce(true).open::<Jobs>("jobs.ferro")?;
```

Every closure normally gets a version of its own, and with it its own write of every tree path it
touched. With `coalesce`, closures that queue up while a sync is running share one write and one
version. They still run one at a time, each on its caller's thread, so they need not be `Send`, and
each sees what the ones before it did. An `Err` or a panic rolls back that closure alone.
On `examples/queue` at 32 workers that took jobs from 9,700 to 16,900 a second, bytes per job from
2,691 to 146 and p99 from 2.3 to 0.8 ms. A closure that runs long against an fsync does not gain:
at 50 µs and 8 writers it lost 16% of its throughput. See `DESIGN.md`.

What you give up is a version per closure. `Committed::version` is the group's, `at` cannot show
the state between two closures of a group, and `history_of` reports the group's net change once.
The commit's `message` holds every closure's message, one per line, and `messages()` lists them.
`changed` says the group made a new version, so a closure that changed nothing reports `true` if
another closure in its group did. If the group cannot be written, every closure in it gets
`Error::GroupFailed`. `restore` and `merge_with` always commit alone, and a writer alone behaves as
without the option. `examples/queue` uses it.

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

`Options::durability(Durability::Buffered)` skips the fsync. A power cut can then lose recent
commits, back to the last checkpoint at worst, and never the whole file: a checkpoint is an fsync
every `Options::checkpoint_bytes` of log, 64 MiB by default, and on clean close.

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
cargo run --release -p ferro-cli -- compact shop.ferro --keep-last 1000
```

`compact` copies what is reachable to a new file and swaps it in atomically, and needs no writer
running. See [Keeping history bounded](#keeping-history-bounded) for doing this from a running
program.

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

## A chat server

`examples/chat` is an axum service for multi-room chat: rooms, joining and leaving, posts, edits
and deletes by their author, reactions, read receipts, unread counts, newest-first history and word
search within a room.

Each room is a `Table` row holding its members, its messages and a word index. The messages are a
`Map<u64, Message, Live>` whose summary counts the ones not deleted, so an unread count is one
`range_summary` past the user's receipt, O(log n) however long the room's history. A history page
is a reversed `range` before a cursor.

Live updates are Server-Sent Events. Each subscriber has a `watch_when` watcher that wakes only when
one of its rooms or its own membership changed, and a `diff` between the last snapshot it delivered
and the new one gives the events. The event id is the version, so a client reconnecting with
`Last-Event-ID` gets one diff from that version, or a reset if that version is no longer retained.

```sh
cargo run --release -p chat -- --db chat.ferro
cargo run --release -p chat --bin chat-load -- --db load.ferro --rate 1000
```

`chat-load` seeds 1,000 rooms and 10,000 users in 5 rooms each, opens 300 feeds and posts from 32
connections, and a quarter of the deliveries send a read receipt. With fsync, at 1,000 posts a
second, a post took 0.9 ms at p50 and 2.7 ms at p99 and reached each member's feed in 0.8 ms and
2.5 ms. Flat out it took about 2,700 posts and 1,900 receipts a second at 11 ms p50, and 4,200 posts
with `coalesce`. These ran on a machine shared with other work. Every watcher's test runs on the
committing thread, so each commit pays for every connected feed.

## A booking system under contention

`examples/booking` sells hotel rooms by the night. A hold claims each night through a
`#[unique(room, night)]` index, so a second claim fails inside the commit and names the holder. A
hold blocks everyone until it is paid or lapses, and a sweeper or the next buyer releases it once
it lapses. Every write is a `try_commit_with` closure that returns a typed error. The till, each
customer's credit and the per-night counters move in the same commit as the rows they sum.

Receipts come from `as_of` checked against `bisect`, audit trails and price history from
`history_of`. A what-if reprice or venue closure runs on a fork, which you preview, then merge
with `merge_with` or drop.

```sh
cargo run --release -p booking -- seed
cargo run --release -p booking -- hold guest1@example.com "Harbour House/101" --from 2030-01-10 --nights 2
cargo run --release -p booking -- confirm 1
cargo run --release -p booking -- receipt 1
cargo run --release -p booking -- whatif close storm --venue "Harbour House"
cargo run --release -p booking -- check
cargo run --release -p booking --bin booking-stress -- --threads 64
```

`booking-stress` races threads for the same rooms and nights while holds lapse, then checks
every transition between versions and every 50th version in full. With 64 threads, the fsync on,
and three 5 second rounds, the median rose from 5,300 writes a second alone to 12,900 with
`coalesce`. Bytes per write fell from 11.2 KB to 2.6 KB, and hold p99 fell from 13.7 to 6.1 ms. At
16 threads, where the writer was less busy, the gain was 9%. No round found a violation.

Without `coalesce` the limit is CPU, not the fsync. A booking write changes about ten small trees,
the rows and their indexes, and re-encodes each path under the writer lock. In one run with
`Durability::Buffered` the writes alone managed 4,800 a second, and with `coalesce` 17,300.

## Performance

Measured in release on one NVMe SSD with ext4, on 1M `u64` keys with short string values, using
`ferro-bench`. Each is a single run on an otherwise idle machine.

| Operation | Result |
| --- | --- |
| Bulk insert in one commit | 1.4 s |
| Random `get` | 0.37 µs |
| Full scan | 33 ns per key |
| Range read | 36 ns per key |
| Single-key commit, fsync | 430 µs, about 2,300 per second |
| Single-key commits, fsync, 8 writers with 8 readers | about 9,600 per second |
| Reads per second with those writers running | 38M |
| The same 8 writers with `coalesce`, median of five runs | about 15,600 per second |
| Single-key commit, `Durability::Buffered` | 27 µs |
| Bytes written per single-key commit | 3.5 KB |
| `diff` | 3.5 µs per changed key |

Against other embedded stores on the same workload, with 3 rounds and 1 GiB of cache each:

| Engine | Get | Scan | Durable commit | 8 writers, commits/s | File size |
| --- | ---: | ---: | ---: | ---: | ---: |
| ferro | 0.53 µs | 35 ns | 0.40 ms | 9,502 | 23 MiB |
| redb | 0.27 µs | 49 ns | 0.42 ms | 2,155 | 65 MiB |
| SQLite, `synchronous=FULL` | 0.90 µs | 51 ns | 0.47 ms | 1,027 | 57 MiB |
| fjall | 1.19 µs | 83 ns | 0.50 ms | 1,982 | 64 MiB |

ferro keeps every version and re-hashes objects as it reads them, and the others do neither.
SQLite and fjall bulk load about 2 times faster, and redb's `get` is about 2 times faster. These
numbers come from a warm cache, on a workload with few overwrites. Full results, including
caveats about sled, are in `benchmarks/compare/RESULTS.md`.

## Limitations

This is an experiment, not a production database.

- The file format is not stable and may change without migration.
- It runs on Unix only.
- A single-key commit writes about 3.5 KB at this size, so batch related changes into one commit.
  Secondary indexes add to that. Without retention the file only grows.
- A reclamation cycle copies everything retained, so it needs that much free disk, and reads that
  miss the node cache are slower for a moment after each swap.
- One writer closure at a time. The closure runs under the writer lock, so a long closure blocks
  every branch. Unless you opt in to `coalesce`, only the fsync is shared, so commit throughput
  stops scaling at the cost of the closure and encoding. Calling `update`, `fork` or `drop_branch`
  from inside it deadlocks. With `coalesce`, so does a closure that waits for another thread's commit
  to be acknowledged.
- A held `Snapshot` or `Map` keeps the file it was read from open, though not the writer lock, so a
  new writer can open once the last `Db` handle is gone. If a reclamation cycle has replaced that
  file since, its disk space comes back only when the last such handle is dropped.
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
- Sync builds and applies a bundle in memory, so the first pull of a large file needs its size in
  memory on both sides. Files that share no commit send everything on their first pull.
- Each `Table` lookup through an index reads the row separately, and no query planning happens.
  You choose the index. `IndexIter::keys` gives the primary keys without reading any row.

See `DESIGN.md` for how it works.
