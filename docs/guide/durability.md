# Durability and crash safety

The file is an append-only log of content-addressed objects behind two checksummed superblock slots.
A commit writes one checksummed frame holding its objects, fsyncs once, and then updates the
superblock without syncing it. Open starts from the newest valid superblock, walks forward over
every complete frame, and truncates the rest, so a crash loses at most a commit that was never
acknowledged. Concurrent writers share a sync: one leader's fsync covers everything queued.

`Options::durability(Durability::Buffered)` skips the fsync. A power cut can then lose recent
commits, back to the last checkpoint at worst, and never the whole file: a checkpoint is an fsync
every `Options::checkpoint_bytes` of log, 64 MiB by default, and on clean close.

Every read re-hashes the object against the pointer it came from, so corruption surfaces as an
error, not wrong data. The whole database is a Merkle tree, and `verify` re-hashes all of it.
`verify_typed` also decodes every object reachable from every retained version as your type, and
checks each map node's count, level and key range, at the cost of a full decode.

## Errors on read

Collection reads do not return `Result`, so that they feel native. If the disk fails or the file is
corrupt, they panic. Use `try_get` to handle that case. A panic inside a commit discards it. Sync
cannot bring in an object that fails to decode, so a decode error on read means a damaged file or a
type change that broke stored data. See [Errors and checks on local reads](../internals/errors.md).

## What was tested

Crash safety was tested with simulated crash images, killed processes and a traced syscall order,
and fsync failure and power cuts were only modelled by intercepting `fdatasync`, never tested on
real hardware.

A frame past the superblock that is damaged cannot be told from a torn write, so open stops there
and truncates it, losing at most the commits of the last sync. Bit rot further back is reported by
the hash checks, not truncated.

On Windows every sync is a `FlushFileBuffers`, and the file must be on NTFS, as described in
[Limitations](limitations.md).

## How it works

- [Storage](../internals/storage.md#file-format-and-durability): frames, superblocks, recovery and
  checkpoints.
- [Errors and checks on local reads](../internals/errors.md).
- [Fuzzing and testing](../internals/testing.md).
