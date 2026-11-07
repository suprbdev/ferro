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
checks each map node's count, level and key range and recomputes each summary, at the cost of a
full decode.

## Errors on read

Collection reads do not return `Result`, so that they feel native. If the disk fails or the file is
corrupt, they panic. Use `try_get` to handle that case. A panic inside a commit discards it. Sync
cannot bring in an object that fails to decode, so a decode error on read means a damaged file or a
type change that broke stored data. See [Errors and checks on local reads](../internals/errors.md).

## What was tested

Crash safety was tested with simulated crash images, killed processes and a traced syscall order,
and fsync failure and power cuts were only modelled by intercepting `fdatasync`, never tested on
real hardware.

Open scans the frames past the newest superblock, where the commits of the last sync can sit
acknowledged under `Durability::Fsync` before the superblock naming them reaches the disk. A frame
there that a torn write left part-written is truncated, losing only commits that were never
acknowledged, or under `Durability::Buffered` commits past the last checkpoint. A frame that reached
the disk whole and has one flipped bit in it, or one in each of up to 8 of its 4 KiB blocks of
records, fails the open with `Error::Corrupt` naming its offset, and the file is left as it was, in
either durability: a power cut never leaves such a frame. Truncating the file at that offset gives
up that commit and the ones after it. Heavier damage there cannot be told from a torn write and is
truncated, and so is a flipped bit with only zeroes or only ones between it and an end of its
512-byte sector, since a sector a write did not reach can hold just that whatever the frame held.
The tests flip bits in frames past the superblock of the crash images above, Fsync frames of
acknowledged commits among them, and check that every torn image still opens, frames of sparse
values stored raw and of a pull that spilled among them. Bit rot further back is reported by the
hash checks, not truncated, unless it leaves every object as it was, as a flipped bit in a
compressed body can, as described under [Records](../internals/storage.md#records). [Torn and
damaged frames](../internals/storage.md#torn-and-damaged-frames) describes the rule and its limits.

On Windows every sync is a `FlushFileBuffers`, and the file must be on NTFS, as described in
[Limitations](limitations.md).

## How it works

- [Storage](../internals/storage.md#file-format-and-durability): frames, superblocks, recovery and
  checkpoints.
- [Errors and checks on local reads](../internals/errors.md).
- [Fuzzing and testing](../internals/testing.md).
