# Limitations

This is an experiment, not a production database.

- The file format is not stable and may change without migration. It is now format 4, which added
  a table of checks to every frame: a file of format 3 does not open in this build, nor one of
  format 4 in an earlier build, and both report `Error::NotFerro`.
- It runs on Unix and Windows. On Windows the file must be on NTFS: a reclamation cycle renames the
  new file over one that snapshots still hold open, which needs POSIX rename semantics, and Windows
  cannot sync a directory, so the rename is made durable by flushing the renamed file, which relies
  on the NTFS journal. Every sync is a `FlushFileBuffers`, which flushes metadata too, like fsync
  rather than fdatasync. Performance on Windows was never measured, and CI skips the tests that
  intercept libc calls or kill processes there, so failed syncs and process death are tested on Unix
  only.
- A single-key commit writes about 3.5 KB at this size, so batch related changes into one commit.
  Secondary indexes add to that. Without retention the file only grows.

- Version 0 is not stored. `at(0)`, until version 1 is pruned, and forks of an empty database use
  your current `Default`, or the `init` passed to `open_with`.

- One writer closure runs at a time on each branch, and placing commits and writing their frames
  take one lock for the whole file, as described under [Branches and the writer
  lock](concurrency.md#branches-and-the-writer-lock).
- A write from inside a closure fails with `Error::Reentrant` wherever it could deadlock, and a
  closure that waits for another thread's commit in a way ferro cannot see can still deadlock, as
  described under [Writing from inside a closure](concurrency.md#writing-from-inside-a-closure).
- Several processes can write one file only if each opens it with `Options::shared`, which needs
  64-bit Linux and one machine. It was never run on Windows, where it is refused until it can be
  tested. A process sees the others' commits only when it commits or refreshes, unless it follows
  the file with `Options::follow`, and on one branch processes commit more slowly than threads,
  since each reads back what the others wrote, as described under [Several
  processes](concurrency.md#several-processes).
- A follower is woken by inotify only on 64-bit Linux. Elsewhere it looks every 10 ms, which delays
  a commit by 5 ms on average and wakes an idle core 100 times a second, and macOS's kqueue and
  Windows' change notifications are not used, as described under [Following other
  processes](concurrency.md#following-other-processes).
- A held `Snapshot` or `Map` keeps its file open, and the disk space of a file a cycle replaced
  comes back only when the last such handle is dropped, as described under [Several
  processes](concurrency.md#several-processes) and [Limits](retention.md#limits).
- A reclamation cycle needs free disk for everything retained, and copies small table rows at 237
  MB/s on one thread and 1,000 MB/s on eight, and no faster on more, as described under
  [Limits](retention.md#limits).
- Crash safety was never tested on real hardware. A frame past the superblock damaged in more than
  one bit of a 4 KiB block, in a block and in that block's entry in the frame's table, in more than
  8 blocks, or in a bit with only zeroes or only ones between it and an end of its sector, cannot
  be told from a torn write and is truncated like one, which under `Durability::Fsync` can drop the
  commits of the last sync, as described under [What was tested](durability.md#what-was-tested).
- After a criss-cross, a merge closure runs more than once, and it refuses when the base of the
  ancestors could lie in pruned history, as described under [Limits](merging.md#limits).
- Sync keeps memory for every object it moves and writes a large bundle twice, as described under
  [Limits](sync.md#limits).
- No query planning happens. You choose the indexes, and `and` and `or` combine equality lookups by
  key. A range or prefix lookup tests the keys of a side with a quarter of its keys or fewer by
  reading their rows, and joins otherwise only after its keys are read and sorted. Each matching
  row is read separately unless the index covers the fields it needs, as described under
  [Combining lookups](tables.md#combining-lookups) and [Lookups](tables.md#lookups).

Each subsystem's own limits are in the internals: [sync](../internals/sync.md#limits),
[reclamation](../internals/reclamation.md#limits) and [Windows](../internals/windows.md).
