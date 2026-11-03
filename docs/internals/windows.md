# Windows

`platform.rs` holds everything the store asks of the OS that differs between Unix and Windows:
positioned reads and writes, the writer lock, the identity of a file, a reader's sync, the directory
sync and the owner of a new generation. The rest of the store is shared.

## Reads, writes and the lock

Positioned I/O loops over `seek_read` and `seek_write`, which can return short counts as `pread` and
`pwrite` can. They also move the handle's file pointer, which nothing reads. Windows serialises
requests on a handle opened without `FILE_FLAG_OVERLAPPED`, which `std` never sets, so readers in
one process queue behind each other and behind frame writes, where `pread` lets them run side by
side. That costs throughput, not correctness.

`File::lock` would shut readers out. On Windows it is `LockFileEx` over the whole file, and Windows
byte-range locks are mandatory: any other handle, in this process or another, fails to read a locked
range with `ERROR_LOCK_VIOLATION`, so no read-only open could read beside a writer. The writer lock
is `LockFileEx` on the single byte at offset 2^62 instead, past the end of any real file, exclusive
for a writer and shared for a reader's scan, and always with `LOCKFILE_FAIL_IMMEDIATELY`, so it
behaves as `try_lock` does. A lock belongs to its handle, so a second writer in the same process
conflicts, as with `flock`, and closing the handle releases it. `Store::close` unlocks explicitly,
because Windows documents that it releases the locks of a process that dies after a delay that
"depends upon available system resources", so a writer that opens right after another was killed can
get `Error::Locked`.

The identity that `open_locked` checks after locking, and that `refresh` compares to notice a cycle,
is the 64-bit file index from `GetFileInformationByHandle`, read through a handle opened with no
access rights, as `std::fs::metadata` does. `std` keeps the index behind the unstable
`windows_by_handle` feature. On NTFS it is unique on the volume and survives a rename, which is all
both checks need. ReFS ids are 128 bits, and this 64-bit index is not guaranteed to tell two files
apart there.

## Durability

`sync_data` and `sync_all` both call `FlushFileBuffers`, which writes the file's cached data and
metadata and then flushes the device's write cache. Every commit therefore pays for an fsync, not an
fdatasync, and the durability point does not move: once the flush returns, the frame survives a
power cut, unless the disk ignores flushes or Windows was told not to send them ("Turn off Windows
write-cache buffer flushing on the device"), the same caveat as a Linux disk mounted without
barriers.

The flush needs a handle with write access. A read-only open that has to sync the frames a dead
writer left, as described under [Read-only opens](storage.md#read-only-opens), first checks that the
path still names its file. If it does, the reader opens the path for writing and flushes that. A
process that may not write the file gets `PermissionDenied` there, rather than reading frames that a
power cut could still take back, and opens normally once a writer has recovered the file. If the
path names another file, a cycle replaced this one and there is nothing to sync. A writer lets go of
a generation only after the rename that replaced it is durable, so no crash names it again, unless
the writer died or poisoned between the rename and its flush. Even then, `Durability::Fsync` synced
every frame before the swap, and `Buffered` may lose commits by contract. The reader keeps the old
view, and its next `refresh` moves to the new file.

The reader used to reopen its own file with `ReOpenFile` instead. That failed with access denied
whenever it had opened the path just before a swap and taken the lock just after the writer retired
the old file, because Windows refuses every new handle to a file that has lost its name. Checking
the path also means the reader never needs write access to a file that is not the database any more.

Windows has no directory sync. A directory opens only with `FILE_FLAG_BACKUP_SEMANTICS`, and nothing
documents what `FlushFileBuffers` does to one. Where Unix syncs the parent directory, after creating
a file and after a cycle's rename, Windows opens the path for writing and flushes the file it names.
NTFS logs a create or a rename as one transaction in its metadata journal, so after a crash the name
points at the old file or the new one, never at neither. The rename also changed the renamed file's
own record, and ferro relies on a flush of that file committing the journal through the rename.
PostgreSQL's `durable_rename` relies on the same thing, since it too skips the directory on Windows
and flushes the file under its new name, but no Win32 document promises it. With the directory sync
read as this flush, the argument under [Crashes](reclamation.md#crashes) holds unchanged: writers
stay paused until it returns, and a failed flush poisons the store. FAT32 and exFAT keep no journal
and are not supported.

## Renaming over an open file

A cycle renames a new file over one that is still open, since the old generation stays open for
every snapshot, `Log` and follower that holds it, and Windows normally refuses that. It works when
two things hold. Every handle on the file must share delete: `std` opens files with
`FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` unless told otherwise, and ferro never
tells it otherwise. And the rename must use POSIX semantics. `platform::rename` opens the new file
with `DELETE` access and calls `SetFileInformationByHandle(FileRenameInfoEx)` with
`FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS`, which Windows 10 1607 and
later accept where the file system supports it, as NTFS does. Only when that call is refused as
unsupported does it fall back to `std::fs::rename`. That tries `MoveFileExW` first, which an open
target refuses, and today retries with the same POSIX rename, but `std` documents that its choice of
calls may change, so ferro makes the call itself. The name then moves to the new file at once, and
the old file loses its name but stays whole for every handle until the last one closes, as an
unlinked inode does on Unix. Snapshots keep reading the old generation, its space comes back when
the last of them drops, and `refresh` finds a new file index at the path. `admin::compact` renames
the same way.

An open of the path that races the rename can still fail, which on Unix it cannot. If the lookup
found the old file just as it lost its name, Windows refuses the open with access denied, because
that file is now waiting to be deleted. `canonicalize` opens the path and then asks the handle for
its name, so if the handle reached the old file it fails with not found. Both last only for the
instant of a swap, after which the path names the new file. So on Windows every open of the path,
and the `canonicalize` in `Db::open`, retries those two errors up to 20 times a millisecond apart,
and a real permission error is reported about 20 ms late. CI found this: a reader opening in a loop
beside 60 cycles failed in about one run in two, and a probe saw about one `canonicalize` in 80 fail
and one read-only open in 150.

If either does not hold, because another program holds the file open without `FILE_SHARE_DELETE` or
the file system has no POSIX rename, the rename fails before anything has changed. The cycle then
fails like any failed rename: the temp file goes, the error lands in `Db::compact` or
`Space::last_error`, commits carry on in the old file, and the next trigger tries again. Nothing is
lost, but the file does not shrink. The cycle is the retry, so the rename does not loop on its own,
which would hold the `tip` mutex and stall commits.

Two other designs were weighed. Generation-numbered files, with a small file naming the current one,
never rename over an open file and work on any file system, but the path would stop being the
database, so copying or backing up one file would no longer work, and the small file needs its own
atomic, durable update, which is a rename again. Deleting the old file and then renaming the new one
into place needs no POSIX semantics, but a crash between the two leaves no database at the path.

A new generation keeps the old file's read-only attribute, which is all `set_permissions` carries on
Windows, and takes its ACL from the directory rather than from the old file. The owner is not
copied.

## Tests

CI runs the library's tests on `windows-latest`. Tests that interpose libc symbols through `dlsym`
(`fdatasync`, `fsync`, `pread64`, `pwrite64`, `rename`, `clock_gettime`), run a child under `sh`
with `ulimit -f`, kill or abort a child process, or make symlinks or check mode bits are
`#[cfg(unix)]`, and `reentry_sync.rs` runs on Linux only. The traced crash simulations replay
ferro's own writes and run on both. So on Windows failed syncs, failed renames and process death go
unexercised, and as on Unix, power loss was never tested on hardware.
