# Retention and reclamation

## Keeping recent history

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

Background cycles run only on a writer with a `history` other than `Keep::All`, and
`Reclaim { every_bytes: 0 }` turns them off. A cycle walks the log on the calling or background
thread and copies the trees it reaches, checking every object's hash, on up to seven more.
`Options::copy_threads` sets the total, which defaults to the number of cores up to 8, and
`copy_threads(1)` keeps a cycle on one thread. The copy is the same file at any number of threads,
as [The copy on several threads](../internals/reclamation.md#the-copy-on-several-threads) describes.
`space` also reports `cycles` run, the `longest_pause` and the `last_error` of a background cycle.
`compact` returns a `Compacted` with `before_bytes`, `after_bytes`, `objects` copied,
`commits_dropped`, `passes`, `pause` and `handed_out`, the trees it copied as units.

## What pruning means

A pruned version is gone, whether or not a cycle has reclaimed its bytes yet: `at` returns
`NoVersion`, so does `as_of` a time before the oldest retained commit, `log` and `history_of` stop
at the oldest retained commit, and a fork answers the same. Version 0, your `Default` before the
first commit, goes when version 1 does. `head_hash` does not change, because retained commits are
not rewritten. A branch also keeps the commits its retained merges brought in, back to its oldest
retained commit, so `merge_with` finds its base in what is retained and returns `NoCommonAncestor`
when the answer could depend on what was pruned.

## Handles across a cycle

Nothing you hold ever breaks. A `Snapshot`, a `Map` cloned from one, a `Log` or a `History` keeps
reading the file it came from, even past pruning, and a read-only process does the same until it
calls `refresh`, or until its follower sees the cycle if it was opened with `follow`. The price is
that the old file stays on disk until the last such handle is dropped, which `space.retired_bytes`
shows. A `watch_changes` watcher moves what it last returned into the new file, unless the cycle
pruned that version, and then holds the old file until it next returns. Committing a `Map` held from before a cycle writes it again by value, under the writer lock,
so `restore` of a snapshot held across a cycle costs time proportional to its size.

## Limits

- A reclamation cycle copies everything retained, so it needs that much free disk, and reads that
  miss the node cache are slower for a moment after each swap. On 8 threads, the default on a
  machine with 8 cores or more, it ran at 1,000 MB/s on a file of small table rows and 2,960 MB/s on
  one of 320 byte values, measured on 2026-11-07, against 640 and 2,140 MB/s on 4 threads and 237
  and 806 MB/s on one, so a 10 GB database takes about 10 seconds. Past 8 threads it gets no faster
  on table rows, because one thread walks the log.
- A held `Snapshot` or `Map` keeps the file it was read from open. If a cycle has replaced that file
  since, its disk space comes back only when the last such handle is dropped.
- Without retention the file only grows.

The `ferro` tool compacts a file with no writer running, as described in [The `ferro` tool](cli.md).

## How it works

[Reclamation](../internals/reclamation.md) describes a cycle, the copy on several threads,
generations, crash safety, the measurements and the alternatives that lost.
