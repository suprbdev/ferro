# Limitations

This is an experiment, not a production database.

- The file format may change between minor releases, with no migration. A build reads one format,
  `ferro::FILE_FORMAT`, now 7, and its sync messages have their own, `ferro::SYNC_FORMAT`, now 2.
  Neither changes in a patch release, so every `0.1.x` reads the files and the sync messages every
  other `0.1.x` writes. A file of another format fails to open with `Error::Format`, which names
  the format it holds, and is left as it was, and a sync message of another format is refused with
  `Error::Bundle`. Builds before format 7 report a file they cannot read as `Error::NotFerro`. Two
  builds of different formats cannot sync, so data moves across a format change only through your
  own code, reading with the old build and writing with the new one.
- It runs on Unix and Windows. On Windows the file must be on NTFS: a reclamation cycle renames the
  new file over one that snapshots still hold open, which needs POSIX rename semantics, and Windows
  cannot sync a directory, so the rename is made durable by flushing the renamed file, which relies
  on the NTFS journal. Every sync is a `FlushFileBuffers`, which flushes metadata too, like fsync
  rather than fdatasync. Performance on Windows was never measured, and CI skips the tests that
  intercept libc calls or kill processes there, so failed syncs and process death are tested on Unix
  only. Each change is tested on Linux alone, with clippy checking that the Windows build compiles:
  the tests last ran on Windows on 2026-11-03, and never on macOS or the BSDs.
- A single-key commit writes about 3.5 KB at 1M keys, so batch related changes into one commit.
  Secondary indexes add to that: a row inserted into a table of 200,000 rows with two indexes writes
  8.2 KB. The largest part is the hashes of the children of every node on the changed path, which
  reads check, as described under [Bytes of a small commit](../internals/commit-bytes.md). Without
  retention the file only grows.

- Version 0 is stored by the first commit, fork or pull that writes to a file, from `Default` or
  the `init` passed to `open_with` by the handle that writes it, and read back from the file after
  that. With `Options::shared`, the first closure on a branch with no commits stores it before it
  runs, even if it then changes nothing, so that every process starts from the same one. Until
  then a file holds nothing, and each open takes its own `init`. The file keeps version 0 for good,
  even once pruning makes `at(0)` return `NoVersion`, so a large initial value keeps its room. Each
  file stores its own: two files that share no commit merge against ours, as described under
  [Versions and hashes across files](sync.md#versions-and-hashes-across-files).

- One writer closure runs at a time on each branch, and placing commits and writing the refs take
  one lock for the whole file, as described under [Branches and the writer
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
- A reclamation cycle needs free disk for everything retained, and copies small table rows at
  431 MB/s on one thread and 1,437 MB/s on eight, measured on 2026-11-12, and no faster on more, as
  described under [Limits](retention.md#limits).
- Crash safety was never tested on real hardware. A frame past the superblock damaged in more than
  one bit of a 4 KiB block, in a block and in that block's entry in the frame's table, in more than
  8 blocks, or in a bit with only zeroes or only ones between it and an end of its sector, cannot
  be told from a torn write and is truncated like one, which under `Durability::Fsync` can drop the
  commits of the last sync, as described under [What was tested](durability.md#what-was-tested).
- After a criss-cross, a merge closure runs more than once, and it refuses when the base of the
  ancestors could lie in pruned history, as described under [Limits](merging.md#limits).
- Sync keeps a few bytes for every object it moves, about 5 on the receiver and 7 on the sender of
  a first pull and more for each commit, and a large bundle refused after it began to arrive leaves
  its frames in the file, without the disk its objects took on Linux, until the writer closes or the
  next reclamation cycle, as described under [Limits](sync.md#limits).
- A value nests at most 4,096 levels within one object, `codec::MAX_DEPTH`: each `Box`, `Arc`,
  `Vec`, `BTreeMap` or `BTreeSet` it holds opens one, so a chain of `Option<Box<Node>>` goes 4,096
  deep and a tree of `Vec<Node>` children 4,095. A commit refuses a deeper value with
  `Error::TooDeep`, counting on through the maps and tables it writes, two levels for each, and a
  read or a receive refuses one with `Error::Decode` or `Error::Bundle`. Reads, writes, checks and
  commits move to stack that ferro maps for the thread when the thread's own runs short, so they
  reach the limit on any thread, on Linux, Android, macOS and iOS; on Windows a value reads and
  writes only as deep as the thread's stack holds, and a deeper one fails with those errors. Where
  ferro can neither learn a thread's stack bounds nor switch stacks, as on the BSDs, the limit stays
  128.
  Dropping and cloning a value recurse in code that Rust generates, which ferro does not grow: at
  the limit a drop takes up to 590 KB of stack in a release build and 3.3 MB in a debug one, and a
  clone up to 3.3 MB in a release build, so a type meant to hold long chains should drop them in a
  loop, as described under [Nesting depth](../internals/storage.md#nesting-depth).
- Query planning goes by counts alone. You choose the indexes, and `and` and `or` combine lookups
  by key. A chain of `and`, however it nests, is led by the lookup with the fewest keys, counted
  from its index, and looks the others up fewest first, but it holds at most 64 lookups, and an
  `or`, or a combination already read from, is one lookup of the chain, planned on its own. The
  keys several walked lookups share are estimated as if they matched independently. A range or
  prefix lookup tests the keys led to it by reading their rows when they are a quarter of its keys
  or fewer, and joins otherwise only after its keys are read and sorted, and a factor of four
  decides both, whatever the rows cost to read. Each matching row is read separately unless the
  index covers the fields it needs, as described under [Combining
  lookups](tables.md#combining-lookups) and [Lookups](tables.md#lookups).

Each subsystem's own limits are in the internals: [sync](../internals/sync.md#limits),
[reclamation](../internals/reclamation.md#limits) and [Windows](../internals/windows.md).
