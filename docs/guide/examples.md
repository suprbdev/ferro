# Example apps

Each app in `examples/` is a workspace package with its own tests, run with
`cargo run --release -p <name>`. `tracker`, `shop`, `chat`, `notes` and `booking` also have `make`
targets of the same name that pass `ARGS` through, and `shop-load`, `chat-load`, `notes-scale`,
`booking-stress` and `queue-bench` have targets of their own, listed in `make help`.

| App | What it is | Features it leans on |
| --- | --- | --- |
| [`tracker`](#tracker) | Command-line issue tracker | Iterator queries, history, undo, branches and merges |
| [`shop`](#shop) | axum HTTP shop with products, carts and orders, and a load tester | Tables, awaited writes, `coalesce`, read replicas |
| [`chat`](#chat) | axum multi-room chat with Server-Sent Events | Watchers, summaries, awaited writes |
| [`booking`](#booking) | Hotel reservations with holds, refunds and what-ifs | Unique and covering indexes, stamped commits, forks, fallible merges |
| [`notes`](sync.md#converging-devices) | Offline-first notes kept per device | Sync |
| `queue` | Durable job queue | `coalesce`, filtered watchers, `bisect_changes` |
| `ledger` | Double-entry ledger | Summaries for balances |
| `metrics` | Time-series metrics store | Rollups in a summarized map, reclamation |
| `wiki` | Wiki and document store | Drafts on branches, three-way merges of pages |
| `graph` | Social graph service | Adjacency as packed `Bytes` blocks in a map, pagerank and reach queries |

## tracker

`examples/tracker` is a complete command line issue tracker built this way. It has filtered listing,
per-issue history, `undo`, `at <version>`, branches and merges, and all of its queries are iterator
code.

```sh
cargo run --release -p tracker -- add "Fix login" --label bug --priority high
cargo run --release -p tracker -- list --label bug
cargo run --release -p tracker -- close 1
cargo run --release -p tracker -- history 1
cargo run --release -p tracker -- undo
```

## shop

`examples/shop` serves an HTTP API for products, carts, orders and their history, and `shop-load`
drives it. `make shop DB=target/demo/shop.ferro ADDR=127.0.0.1:3000` serves it, and `make shop-load`
loads a running server for `SECS` seconds. Its writes are awaited with `try_commit_with_async` by
default, and `--writes` selects the other ways measured in [Awaited
writes](../internals/async-writes.md#measurements). `--coalesce` turns on `coalesce`, measured in
[Coalescing](../internals/coalescing.md#measurements).

## chat

`examples/chat` is an axum service for multi-room chat: rooms, joining and leaving, posts, edits and
deletes by their author, reactions, read receipts, unread counts, newest-first history and word
search within a room.

Each room is a `Table` row holding its members, its messages and a word index. The messages are a
`Map<u64, Message, Live>` whose summary counts the ones not deleted, so an unread count is one
`range_summary` past the user's receipt, O(log n) however long the room's history. A history page is
a reversed `range` before a cursor.

Live updates are Server-Sent Events. Each subscriber is a `watch_changes` watcher whose test
compares its rooms and its own membership between the snapshot it delivered last and the new one,
and a `diff` between those two gives the events. The stream awaits `changed()`, so a subscriber
holds no thread, and dropping the stream when the client goes drops the watcher. Posts and the other
writes are awaited with `try_commit_with_async`, so they hold no thread either. The event id is the
version, so a client reconnecting with `Last-Event-ID` gets one diff from that version, or a reset
if that version is no longer retained.

```sh
cargo run --release -p chat -- --db chat.ferro
cargo run --release -p chat --bin chat-load -- --db load.ferro --rate 1000
```

`chat-load` seeds 1,000 rooms and 10,000 users in 5 rooms each, opens 300 feeds and posts from 32
connections, and a quarter of the deliveries send a read receipt. With fsync, at 1,000 posts a
second, a post took 0.18 ms at p50 and 0.41 ms at p99 and reached each member's feed in 0.47 ms and
1.0 ms. Flat out it took about 5,200 posts a second at 6.5 ms p50 and 10 ms p99 on 35 threads, where
writing through `spawn_blocking` took 3,900 at 8.2 and 15.7 ms on up to 131. With `--clients 3000`
it took 700 posts and 5,300 receipts a second at 43 ms p50, where running every test on the
committing thread had managed 126 posts at 206 ms. Without receipts, 3,000 feeds took 1,000 posts a
second at 0.35 ms per post and 2.1 ms to delivery. These ran on 2026-11-03, on a machine shared with
other work.

## booking

`examples/booking` sells hotel rooms by the night. A hold claims each night through a
`#[unique(room, night)]` index, so a second claim fails inside the commit and names the holder. A
hold blocks everyone until it is paid or lapses, and a sweeper or the next buyer releases it once it
lapses. Every write is a `try_commit_stamped` closure that returns a typed error. The till, each
customer's credit and the per-night counters move in the same commit as the rows they sum.

A customer's bookings page, `customer show` and `Hotels::lines_of`, reads its lines from
`#[index(cover(status, venue, room, first, nights, total))]` on the customer index, so it touches no
booking row. `booking-stress --browsers 4` runs four threads paging the newest 20 bookings of
random customers beside the writers. With 16 writer threads and the fsync on, medians of three
interleaved 5 second rounds measured on 2026-11-04, pages went from 2.14 million a second to 5.26
million, page p50 from 1.7 to 0.7 us and p99 from 4.0 to between 1.1 and 2.1 us. Bytes per write
rose from 10,900 to 12,400, and writes a second stayed within the spread of the runs, 4,000 to 5,700
either way. The full check confirms at every 50th version that the covered lines match the rows.

A confirm records its own version from the `Stamp`, so a receipt is `at` that version. Audit trails
and price history come from `history_of`. A what-if reprice or venue closure runs on a fork, which
you preview, then merge with `try_merge_with` or drop. If the merge cannot cancel a booking that
main took since the fork, it returns that `BookingError` and writes nothing.

```sh
cargo run --release -p booking -- seed
cargo run --release -p booking -- hold guest1@example.com "Harbour House/101" --from 2030-01-10 --nights 2
cargo run --release -p booking -- confirm 1
cargo run --release -p booking -- receipt 1
cargo run --release -p booking -- whatif close storm --venue "Harbour House"
cargo run --release -p booking -- check
cargo run --release -p booking --bin booking-stress -- --threads 64
```

`booking-stress` races threads for the same rooms and nights while holds lapse, then checks every
transition between versions and every 50th version in full. With 64 threads, the fsync on, and three
5 second rounds on 2026-11-03, the median rose from 5,300 writes a second alone to 12,900 with
`coalesce`. Bytes per write fell from 11.2 KB to 2.6 KB, and hold p99 fell from 13.7 to 6.1 ms. At
16 threads, where the writer was less busy, the gain was 9%. No round found a violation.

Without `coalesce` the limit is CPU, not the fsync. A booking write changes about ten small trees,
the rows and their indexes, and re-encodes each path under the writer lock. A node on such a path
now copies the encoded entries it did not change, and outside its closure a hold costs 21% less CPU
than it did, a confirm 37% less and a cancel 31% less, as [Commits of a few
rows](../internals/commit-cost.md) describes. With `Durability::Buffered` and 64 threads, median of
three interleaved rounds on 2026-11-03, the writes alone went from 5,100 to 6,100 a second, and with
`coalesce` from 14,500 to 15,600.
