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
grown by `every_bytes` or by its size after the last cycle, whichever is larger. A pull or receive
prunes too, both the branch it brings up and the branch it landed, once it has brought the one up
to the other and recorded it. A landed branch that a pull did not record, as when the merge failed
or every commit it brought already held this branch's value, keeps what landed until a later pull
records it or a cycle runs.
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
`space` reports the file's length as `file_bytes`, the bytes its log takes as `log_bytes`, and the
`live_bytes` estimate, which is at most `log_bytes`, itself at most `file_bytes`. The difference
between the last two is zeroes that a writer under `Durability::Fsync` preallocates past the log,
up to 4 MiB, and the staging frames that a receive of more than 1 MiB placed past the newest
commit, while it streams, or once it was refused, as headers over holes on Linux. An exclusive
writer truncates both when it closes and the next writer to open truncates them otherwise. The
staging frames of a refused receive that a commit followed count in `log_bytes` until the next
cycle, though on Linux their records take no disk. `space` also reports `cycles` run, the `longest_pause` and the `last_error` of
a background cycle. `compact` returns a `Compacted` with `before_bytes` and `after_bytes`, the
lengths of the old and the new file, `objects` copied, `commits_dropped`, `passes`, `pause` and
`handed_out`, the trees it copied as units.

## What pruning means

A pruned version is gone, whether or not a cycle has reclaimed its bytes yet: `at` returns
`NoVersion`, so does `as_of` a time before the oldest retained commit, `log` and `history_of` stop
at the oldest retained commit, and a fork answers the same. Version 0, the value the file stored
before the first commit, goes when version 1 does, though the file keeps its value, since it is
where a branch that a later pull brings in from version 1 starts. `head_hash` does not change,
because retained commits are
not rewritten. A branch also keeps the commits its retained merges brought in, back to its oldest
retained commit, so `merge_with` finds its base in what is retained and returns `NoCommonAncestor`
when the answer could depend on what was pruned.

## Handles across a cycle

Nothing you hold ever breaks. A `Snapshot`, a `Map` cloned from one, a `Log` or a `History` keeps
reading the file it came from, even past pruning, and a read-only process does the same until it
calls `refresh`, or until its follower sees the cycle if it was opened with `follow`. The price is
that the old file stays on disk until the last such handle is dropped, which `space.retired_bytes`
shows. A `watch_changes` watcher moves what it last returned into the new file, unless the cycle
pruned that version, and then holds the old file until it next returns. Committing a `Map` held
from before a cycle writes it again by value, under the writer lock, so `restore` of a snapshot held
across a cycle costs time proportional to its size.

## Limits

- A reclamation cycle copies everything retained, so it needs that much free disk, and reads that
  miss the node cache are slower for a moment after each swap. On 8 threads, the default on a
  machine with 8 cores or more, it ran at 1,078 MB/s on a file of small table rows and 2,958 MB/s on
  one of 320 byte values, measured on 2026-11-11, against 652 and 2,180 MB/s on 4 threads and 239
  and 794 MB/s on one, so a 10 GB database takes about 9 seconds. On 16 threads it gains about a
  tenth more on table rows at best, and on 32 it is slower than on 8, because one thread walks the
  log and writes its entries.
- A held `Snapshot` or `Map` keeps the file it was read from open. If a cycle has replaced that file
  since, its disk space comes back only when the last such handle is dropped.
- Without retention the file only grows.

The `ferro` tool compacts a file with no writer running, as described in [The `ferro` tool](cli.md).

## How it works

[Reclamation](../internals/reclamation.md) describes a cycle, the copy on several threads,
generations, crash safety, the measurements and the alternatives that lost.
