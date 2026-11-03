# Watchers

A watcher hands out the newest snapshot of a branch each time it moves.

## Watching a branch

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
consumer skips versions, costs the writers nothing and uses no memory, and `diff` covers whatever it
skipped. `recv_timeout` and `try_recv` work like `std::sync::mpsc`. `changed()` is the same receive
as a future built on `std::task::Waker`, so it runs on any executor and an async server needs no
thread per subscriber. Dropping the future stops the wait. A watcher ends when the last `Db` handle
goes, and it does not keep the file open: a snapshot it had not taken by then is lost. A read-only
handle fires its watchers on `refresh`.

## Filtered watchers and closers

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

`watch_when` sleeps through commits whose snapshot fails the test. `closer` gives a cloneable handle
that ends a watcher and its clones from any thread without a commit. Clones share the closer and the
test and keep their own place.

## Changes since you last looked

```rust
let watcher = db.watch_changes(|seen, now| seen.stock.get("anvil") != now.stock.get("anvil"));
let mut seen = watcher.seen().unwrap();
while let Ok(now) = watcher.changed().await {
    for change in seen.stock.diff(&now.stock) {
        println!("{:?} changed", change.key());
    }
    seen = now;
}
```

`watch_changes` passes the test the snapshot the watcher returned last as well as the new one, so
"wake me when this changed since I looked" needs no state of its own. Until the first return it
compares against the snapshot that was current when the watcher was made, which `seen` returns, so a
consumer that starts from `seen` misses nothing that lands meanwhile. Clones share what was seen: a
return by one moves it for all.

## Where tests run

Tests run on a `ferro-watch` thread per branch, started when a filtered or async watcher first
waits, and only for watchers that are waiting. A commit stores its snapshot and wakes that thread,
so it costs the same with no watchers or ten thousand and never waits for a test. The thread tests
the newest snapshot for every waiting watcher and wakes those that pass; commits that land while it
works are folded into its next pass, and a woken watcher looks at the newest snapshot itself. A test
runs under no lock, so it may read, commit or refresh. It must not wait for a watcher, and a slow
test delays the other watchers of its branch, so keep tests quick. When a pass takes over a
millisecond, the thread tries splitting the next ones among a few helpers and keeps doing so while
that is faster.

## How it works

[Watchers](../internals/watchers.md) describes the feed, the watch thread and what they cost a
commit. `examples/chat` serves Server-Sent Events from `watch_changes` watchers, as described in
[Example apps](examples.md#chat).
