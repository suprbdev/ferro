//! What the store asks of the operating system where Unix and Windows differ.
//!
//! Positioned reads and writes, the writer lock, the identity of a file, a read-only open's sync,
//! the directory sync after a create or rename, renaming over an open file, the owner of a new
//! generation and self-deleting scratch files each have one function here, with the same contract
//! on every target. The byte-range locks behind `Options::shared` and the early writeback of a copy
//! exist only on 64-bit Linux, and [`RANGES`] says whether they do. So does the inotify [`Watch`]
//! that a follower waits on, whose [`Watch::new`] fails with `Unsupported` elsewhere.
//! [Windows](https://github.com/archlemon/ferro/blob/main/docs/internals/windows.md) and
//! [Writers in several processes](https://github.com/archlemon/ferro/blob/main/docs/internals/processes.md)
//! describe both sides.

#[cfg(unix)]
mod imp {
    use std::fs::{File, Metadata, OpenOptions, TryLockError};
    use std::io;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, fchown};
    use std::path::{Path, PathBuf};

    pub(crate) use std::os::unix::fs::FileExt;

    /// The identity of an open file, its inode, which stays the same across a rename.
    ///
    /// Compared with [`path_id`] to tell whether a path still names this file.
    pub(crate) fn file_id(file: &File) -> io::Result<u64> {
        Ok(file.metadata()?.ino())
    }

    /// The identity of the file `path` names now, comparable with [`file_id`].
    pub(crate) fn path_id(path: &Path) -> io::Result<u64> {
        Ok(std::fs::metadata(path)?.ino())
    }

    /// Opens `path` with `options`.
    pub(crate) fn open(options: &OpenOptions, path: &Path) -> io::Result<File> {
        options.open(path)
    }

    /// The absolute path of `path` with every link resolved.
    pub(crate) fn canonicalize(path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    /// Takes the writer lock on `file`, exclusive, without waiting.
    ///
    /// The lock belongs to the handle, so a second handle in the same process conflicts, and
    /// closing the handle releases it.
    pub(crate) fn try_lock(file: &File) -> Result<(), TryLockError> {
        file.try_lock()
    }

    /// Takes the writer lock on `file` shared, without waiting, as a reader's scan and a shared
    /// writer do.
    pub(crate) fn try_lock_shared(file: &File) -> Result<(), TryLockError> {
        file.try_lock_shared()
    }

    /// Releases the writer lock on `file`, exclusive or shared.
    pub(crate) fn unlock(file: &File) -> io::Result<()> {
        file.unlock()
    }

    /// Makes the frames of a file opened read-only durable, as a read-only open must before it
    /// trusts frames a dead writer left.
    pub(crate) fn sync_reader(file: &File, _path: &Path) -> io::Result<()> {
        file.sync_data()
    }

    /// Renames `from` to `to`, replacing `to` atomically even while other handles hold it open.
    /// The replaced file stays whole for every handle until the last one closes.
    pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    /// Makes a create or rename of `path` durable by syncing its parent directory.
    pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
        match path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            Some(parent) => File::open(parent)?.sync_all(),
            None => Ok(()),
        }
    }

    /// Gives `file` the owner and group in `source`, as far as the process may. A failure is
    /// ignored, since the file stays usable by the process that made it.
    pub(crate) fn copy_owner(file: &File, source: &Metadata) {
        let _ = fchown(file, Some(source.uid()), Some(source.gid()));
    }

    /// Creates a new file at `path`, readable and writable by the owner alone, whose space comes
    /// back when the returned handle closes, even if the process dies.
    ///
    /// The name is removed at once, so nothing else can open the file. Fails if `path` exists.
    pub(crate) fn scratch(path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        std::fs::remove_file(path)?;
        Ok(file)
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::{File, Metadata, OpenOptions, TryLockError};
    use std::io::{self, ErrorKind};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{FileExt as _, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::Duration;
    use std::{mem, ptr};

    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_LOCK_VIOLATION,
        ERROR_NOT_LOCKED, ERROR_NOT_SUPPORTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_DELETE_ON_CLOSE,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO, FileRenameInfoEx,
        GetFileInformationByHandle, LOCK_FILE_FLAGS, LOCKFILE_EXCLUSIVE_LOCK,
        LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, SYNCHRONIZE, SetFileInformationByHandle,
        UnlockFileEx,
    };
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
    use windows_sys::Win32::System::WindowsProgramming::{
        FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
    };

    const LOCK_AT: u64 = 1 << 62;
    const RACE_RETRIES: u32 = 20;

    /// Positioned reads and writes with the names and contract of Unix's `FileExt`.
    ///
    /// They loop over `seek_read` and `seek_write`, which can return short counts, and move the
    /// handle's file pointer, which nothing reads.
    pub(crate) trait FileExt {
        /// Reads into `buf` from `offset`, returning how many bytes it read, 0 at the file's end.
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
        /// Fills `buf` from `offset`, failing with `UnexpectedEof` if the file ends first.
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
        /// Writes all of `buf` at `offset`, failing with `WriteZero` if a write makes no progress.
        fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    }

    impl FileExt for File {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            self.seek_read(buf, offset)
        }

        fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
            while !buf.is_empty() {
                match self.seek_read(buf, offset) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            ErrorKind::UnexpectedEof,
                            "failed to fill whole buffer",
                        ));
                    }
                    Ok(read) => {
                        buf = &mut buf[read..];
                        offset += read as u64;
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }

        fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
            while !buf.is_empty() {
                match self.seek_write(buf, offset) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            ErrorKind::WriteZero,
                            "failed to write whole buffer",
                        ));
                    }
                    Ok(written) => {
                        buf = &buf[written..];
                        offset += written as u64;
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }
    }

    /// The identity of an open file, the 64-bit file index Windows keeps for it, which on NTFS is
    /// unique on the volume and stays the same across a rename.
    ///
    /// Compared with [`path_id`] to tell whether a path still names this file.
    pub(crate) fn file_id(file: &File) -> io::Result<u64> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the handle belongs to `file`, which is borrowed for the call, and `info` is a
        // live, writable `BY_HANDLE_FILE_INFORMATION` that the call fills in.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow))
    }

    /// The identity of the file `path` names now, comparable with [`file_id`], read through a
    /// handle opened with no access rights.
    pub(crate) fn path_id(path: &Path) -> io::Result<u64> {
        file_id(&open(OpenOptions::new().access_mode(0), path)?)
    }

    fn racing<T>(
        lost: impl Fn(&io::Error) -> bool,
        mut attempt: impl FnMut() -> io::Result<T>,
    ) -> io::Result<T> {
        let mut retries = 0;
        loop {
            match attempt() {
                Err(error) if retries < RACE_RETRIES && lost(&error) => {
                    retries += 1;
                    thread::sleep(Duration::from_millis(1));
                }
                done => return done,
            }
        }
    }

    fn delete_pending(error: &io::Error) -> bool {
        error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
    }

    /// Opens `path` with `options`.
    ///
    /// An open that races a cycle's rename can find the old file just as it loses its name, and
    /// Windows refuses it with access denied, so that error is retried up to 20 times a
    /// millisecond apart.
    pub(crate) fn open(options: &OpenOptions, path: &Path) -> io::Result<File> {
        racing(delete_pending, || options.open(path))
    }

    /// The absolute path of `path` with every link resolved, retrying access denied and not found
    /// as [`open`] retries access denied, since both last only for the instant of a swap.
    pub(crate) fn canonicalize(path: &Path) -> io::Result<PathBuf> {
        racing(
            |error| delete_pending(error) || error.kind() == ErrorKind::NotFound,
            || std::fs::canonicalize(path),
        )
    }

    fn lock_range() -> OVERLAPPED {
        OVERLAPPED {
            Anonymous: OVERLAPPED_0 {
                Anonymous: OVERLAPPED_0_0 {
                    Offset: LOCK_AT as u32,
                    OffsetHigh: (LOCK_AT >> 32) as u32,
                },
            },
            ..OVERLAPPED::default()
        }
    }

    fn lock(file: &File, flags: LOCK_FILE_FLAGS) -> Result<(), TryLockError> {
        let mut range = lock_range();
        let flags = flags | LOCKFILE_FAIL_IMMEDIATELY;
        // SAFETY: the handle belongs to `file`, which is borrowed for the call, and `range` is a
        // live `OVERLAPPED` naming the lock's offset. `std` opens files without
        // `FILE_FLAG_OVERLAPPED` and the lock fails immediately rather than waiting, so the call
        // completes before it returns and keeps no use of `range` after.
        if unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut range) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Err(TryLockError::WouldBlock)
        } else {
            Err(TryLockError::Error(error))
        }
    }

    /// Takes the writer lock on `file`, exclusive, without waiting.
    ///
    /// The lock is `LockFileEx` on the one byte at 2^62, past the end of any real file, since
    /// Windows byte-range locks are mandatory and a lock over the file would stop every other
    /// handle from reading it. It belongs to the handle, so a second handle in the same process
    /// conflicts, and closing the handle releases it.
    pub(crate) fn try_lock(file: &File) -> Result<(), TryLockError> {
        lock(file, LOCKFILE_EXCLUSIVE_LOCK)
    }

    /// Takes the writer lock on `file` shared, without waiting, as a reader's scan does.
    pub(crate) fn try_lock_shared(file: &File) -> Result<(), TryLockError> {
        lock(file, 0)
    }

    /// Releases the writer lock on `file`, exclusive or shared. A handle that holds no lock is
    /// not an error.
    pub(crate) fn unlock(file: &File) -> io::Result<()> {
        let mut range = lock_range();
        // SAFETY: the handle belongs to `file`, which is borrowed for the call, and `range` is a
        // live `OVERLAPPED` naming the locked byte. Unlocking never waits, so the call keeps no
        // use of `range` after it returns.
        if unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut range) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NOT_LOCKED as i32) {
            Ok(())
        } else {
            Err(error)
        }
    }

    /// Makes the frames of a file opened read-only durable, as a read-only open must before it
    /// trusts frames a dead writer left.
    ///
    /// A flush needs write access, which a read-only handle lacks. If its own flush is denied and
    /// `path` still names this file, the path is opened for writing and flushed, so a process that
    /// may not write the file gets `PermissionDenied`. If `path` names another file, a cycle
    /// replaced this one and there is nothing to sync.
    pub(crate) fn sync_reader(file: &File, path: &Path) -> io::Result<()> {
        match file.sync_data() {
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {}
            synced => return synced,
        }
        let id = file_id(file)?;
        match path_id(path) {
            Ok(named) if named == id => open(OpenOptions::new().write(true), path)?.sync_data(),
            Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Renames `from` to `to`, replacing `to` atomically even while other handles hold it open.
    /// The replaced file stays whole for every handle until the last one closes.
    ///
    /// It uses a POSIX-semantics rename, falling back to [`std::fs::rename`] only where that is
    /// refused as unsupported. Every handle on `to` must share delete, as `std` opens files.
    pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        match posix_rename(from, to) {
            Err(error)
                if matches!(
                    error.raw_os_error().map(|code| code as u32),
                    Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER)
                ) =>
            {
                std::fs::rename(from, to)
            }
            renamed => renamed,
        }
    }

    fn posix_rename(from: &Path, to: &Path) -> io::Result<()> {
        let source = OpenOptions::new()
            .access_mode(DELETE | SYNCHRONIZE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(from)?;
        let name: Vec<u16> = std::path::absolute(to)?
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect();
        let at = mem::offset_of!(FILE_RENAME_INFO, FileName);
        let size = u32::try_from(at + name.len() * 2)
            .map_err(|_| io::Error::from(ErrorKind::InvalidFilename))?;
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: `buffer` is zeroed, 8-byte aligned, which covers every field of
        // `FILE_RENAME_INFO`, and at least `size` bytes long, which is the struct up to `FileName`
        // plus room for every unit of `name`. So `info` points to a valid `FILE_RENAME_INFO` with
        // a null root directory, the writes stay inside `buffer`, and `name` is a separate
        // allocation the copy cannot overlap.
        unsafe {
            (*info).Anonymous.Flags =
                FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
            (*info).FileNameLength = size - at as u32 - 2;
            ptr::copy_nonoverlapping(
                name.as_ptr(),
                (&raw mut (*info).FileName).cast(),
                name.len(),
            );
        }
        let handle = source.as_raw_handle();
        // SAFETY: the handle belongs to `source`, which stays open through the call, and `info`
        // points to the `size` bytes of the `FILE_RENAME_INFO` built above, which `buffer` keeps
        // alive until the function returns.
        if unsafe { SetFileInformationByHandle(handle, FileRenameInfoEx, info.cast(), size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Makes a create or rename of `path` durable. Windows has no directory sync, so this opens
    /// `path` for writing and flushes the file it names, which commits the file system's journal
    /// through the create or rename on NTFS.
    pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
        open(OpenOptions::new().write(true), path)?.sync_all()
    }

    /// Does nothing: a new file takes its access control list from its directory, and its owner
    /// is not copied.
    pub(crate) fn copy_owner(_file: &File, _source: &Metadata) {}

    /// Creates a new file at `path`, shared with no other handle, which Windows deletes when the
    /// returned handle closes, even if the process dies. Fails if `path` exists.
    pub(crate) fn scratch(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(0)
            .custom_flags(FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_DELETE_ON_CLOSE)
            .open(path)
    }
}

pub(crate) use imp::*;

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod ranges {
    use std::fs::{File, TryLockError};
    use std::io;
    use std::os::fd::AsRawFd;

    /// Whether this target has the byte-range locks [`lock_range`] takes, which
    /// `Options::shared` needs.
    pub(crate) const RANGES: bool = true;

    fn ofd(
        file: &File,
        command: libc::c_int,
        kind: libc::c_int,
        at: u64,
        len: u64,
    ) -> io::Result<libc::flock> {
        // SAFETY: `flock` is a C struct of plain integers, for which all zero bytes are a valid
        // value.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = kind as libc::c_short;
        lock.l_whence = libc::SEEK_SET as libc::c_short;
        lock.l_start = at as libc::off_t;
        lock.l_len = len as libc::off_t;
        loop {
            // SAFETY: the descriptor belongs to `file`, which is borrowed for the call, and every
            // command passed here takes a pointer to a `flock`, which `lock` is, initialized and
            // live through the call.
            if unsafe { libc::fcntl(file.as_raw_fd(), command, &mut lock) } != -1 {
                return Ok(lock);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Locks the `len` bytes of `file` from `at`, exclusive or shared, waiting for other holders
    /// when `wait` is set and failing with [`TryLockError::WouldBlock`] when it is not.
    ///
    /// The lock is an open file description lock: it belongs to the open file, so two handles of
    /// one file conflict even within a process, every thread locking through one handle shares
    /// what it holds, and the kernel releases it when the handle closes or the process dies. A
    /// range this handle already holds is granted again at once, so callers keep their own record
    /// of which thread holds what.
    pub(crate) fn lock_range(
        file: &File,
        at: u64,
        len: u64,
        exclusive: bool,
        wait: bool,
    ) -> Result<(), TryLockError> {
        let kind = if exclusive {
            libc::F_WRLCK
        } else {
            libc::F_RDLCK
        };
        let command = if wait {
            libc::F_OFD_SETLKW
        } else {
            libc::F_OFD_SETLK
        };
        match ofd(file, command, kind, at, len) {
            Ok(_) => Ok(()),
            Err(error) if matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) => {
                Err(TryLockError::WouldBlock)
            }
            Err(error) => Err(TryLockError::Error(error)),
        }
    }

    /// Releases this handle's lock on the `len` bytes of `file` from `at`.
    pub(crate) fn unlock_range(file: &File, at: u64, len: u64) -> io::Result<()> {
        ofd(file, libc::F_OFD_SETLK, libc::F_UNLCK, at, len).map(|_| ())
    }

    /// Whether another open file holds a lock on any of the `len` bytes of `file` from `at`.
    /// Locks this handle holds do not count.
    pub(crate) fn range_held(file: &File, at: u64, len: u64) -> io::Result<bool> {
        let lock = ofd(file, libc::F_OFD_GETLK, libc::F_WRLCK, at, len)?;
        Ok(lock.l_type != libc::F_UNLCK as libc::c_short)
    }

    /// Whether another open file holds an exclusive lock on any of the `len` bytes of `file` from
    /// `at`. Shared locks, and locks this handle holds, do not count.
    pub(crate) fn range_excluded(file: &File, at: u64, len: u64) -> io::Result<bool> {
        let lock = ofd(file, libc::F_OFD_GETLK, libc::F_RDLCK, at, len)?;
        Ok(lock.l_type != libc::F_UNLCK as libc::c_short)
    }

    /// The first byte of an exclusive lock that another open file holds on any of the `len` bytes
    /// of `file` from `at`, or `None` if there is none. The kernel reports the first such lock it
    /// finds, and of the locks one open holds, the one that starts first; it merges one open's
    /// locks on neighbouring bytes into one, so the first byte can lie before `at`.
    pub(crate) fn excluded_at(file: &File, at: u64, len: u64) -> io::Result<Option<u64>> {
        let lock = ofd(file, libc::F_OFD_GETLK, libc::F_RDLCK, at, len)?;
        if lock.l_type == libc::F_UNLCK as libc::c_short {
            return Ok(None);
        }
        Ok(u64::try_from(lock.l_start).ok())
    }

    /// Asks the kernel to start writing the `len` bytes of `file` from `at` to disk, without
    /// waiting for them and without making them durable. A failure is ignored, since the later
    /// sync covers the range either way.
    pub(crate) fn start_writeback(file: &File, at: u64, len: u64) {
        // SAFETY: the descriptor belongs to `file`, which is borrowed for the call, and
        // `sync_file_range` takes no pointers. `SYNC_FILE_RANGE_WRITE` only starts writeback of
        // the range, so no memory or file contents change.
        unsafe {
            libc::sync_file_range(
                file.as_raw_fd(),
                at as libc::off64_t,
                len as libc::off64_t,
                libc::SYNC_FILE_RANGE_WRITE,
            )
        };
    }
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
mod ranges {
    use std::fs::{File, TryLockError};
    use std::io;

    /// Whether this target has the byte-range locks [`lock_range`] takes, which
    /// `Options::shared` needs. Only 64-bit Linux does.
    pub(crate) const RANGES: bool = false;

    /// Does nothing: this target has no call that starts writeback early, so the later sync
    /// writes the range.
    pub(crate) fn start_writeback(_file: &File, _at: u64, _len: u64) {}

    /// Fails with an `Unsupported` error, since this target has no open file description locks.
    pub(crate) fn lock_range(
        _file: &File,
        _at: u64,
        _len: u64,
        _exclusive: bool,
        _wait: bool,
    ) -> Result<(), TryLockError> {
        Err(TryLockError::Error(io::ErrorKind::Unsupported.into()))
    }

    /// Does nothing, since no range can be locked on this target.
    pub(crate) fn unlock_range(_file: &File, _at: u64, _len: u64) -> io::Result<()> {
        Ok(())
    }

    /// Returns `false`, since no range can be locked on this target.
    pub(crate) fn range_held(_file: &File, _at: u64, _len: u64) -> io::Result<bool> {
        Ok(false)
    }

    /// Returns `false`, since no range can be locked on this target.
    pub(crate) fn range_excluded(_file: &File, _at: u64, _len: u64) -> io::Result<bool> {
        Ok(false)
    }

    /// Returns `None`, since no range can be locked on this target.
    pub(crate) fn excluded_at(_file: &File, _at: u64, _len: u64) -> io::Result<Option<u64>> {
        Ok(None)
    }
}

pub(crate) use ranges::*;

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod notify {
    use std::ffi::CString;
    use std::fs::File;
    use std::io::{self, ErrorKind, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const FILE_EVENTS: u32 = libc::IN_MODIFY;
    const DIR_EVENTS: u32 = libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_ONLYDIR;
    const EVENT_BYTES: usize = 16;

    /// What one wait on a [`Watch`] ended with.
    enum Woke {
        Bell,
        Events,
        Timeout,
    }

    fn checked(result: libc::c_int) -> io::Result<libc::c_int> {
        match result {
            -1 => Err(io::Error::last_os_error()),
            done => Ok(done),
        }
    }

    fn owned(fd: libc::c_int) -> File {
        // SAFETY: `fd` is a descriptor that the call before it just returned, open and owned by
        // nothing else, so the `File` becomes its only owner and closes it once.
        File::from(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn add(events: &File, path: &CString, mask: u32) -> io::Result<libc::c_int> {
        // SAFETY: the descriptor belongs to `events`, which is borrowed for the call, and `path`
        // is a NUL-terminated string that lives through it and is only read.
        checked(unsafe { libc::inotify_add_watch(events.as_raw_fd(), path.as_ptr(), mask) })
    }

    /// Ends a [`Watch`]'s waits from another thread, this one and every later one.
    pub(crate) struct Bell(File);

    impl Bell {
        /// Wakes the watch's wait and makes every later one return at once.
        pub(crate) fn ring(&self) {
            let _ = (&self.0).write(&1u64.to_ne_bytes());
        }
    }

    /// An inotify watch on a database file, for writes to it, and on its directory, for a file
    /// created, removed or renamed at its name, which a follower waits on.
    ///
    /// When the name moves to another file, the watch moves to that file too, so it follows the
    /// file a reclamation cycle renames over the path.
    pub(crate) struct Watch {
        events: File,
        bell: Arc<Bell>,
        path: CString,
        name: Vec<u8>,
        dir: libc::c_int,
        file: Option<libc::c_int>,
    }

    impl Watch {
        /// Watches the file at `path` and its directory.
        ///
        /// # Errors
        ///
        /// The error of the inotify or eventfd call that failed, such as `EMFILE` once the user's
        /// inotify instances are used up, or of watching the directory or the file.
        pub(crate) fn new(path: &Path) -> io::Result<Watch> {
            let name = path
                .file_name()
                .ok_or(ErrorKind::InvalidInput)?
                .as_bytes()
                .to_vec();
            let dir = match path.parent() {
                Some(dir) if !dir.as_os_str().is_empty() => dir,
                _ => Path::new("."),
            };
            let dir = CString::new(dir.as_os_str().as_bytes())?;
            let path = CString::new(path.as_os_str().as_bytes())?;
            // SAFETY: `inotify_init1` takes only flags and returns a new descriptor or -1.
            let events =
                checked(unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) });
            let events = owned(events?);
            // SAFETY: `eventfd` takes only a starting count and flags and returns a new
            // descriptor or -1.
            let bell = checked(unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) });
            let bell = Arc::new(Bell(owned(bell?)));
            let dir = add(&events, &dir, DIR_EVENTS)?;
            let mut watch = Watch {
                events,
                bell,
                path,
                name,
                dir,
                file: None,
            };
            watch.rewatch()?;
            Ok(watch)
        }

        /// The bell that ends this watch's waits.
        pub(crate) fn bell(&self) -> Arc<Bell> {
            self.bell.clone()
        }

        fn rewatch(&mut self) -> io::Result<()> {
            let file = match add(&self.events, &self.path, FILE_EVENTS) {
                Ok(wd) => Some(wd),
                Err(error) if error.kind() == ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            if let Some(old) = self.file
                && file != Some(old)
            {
                // SAFETY: the descriptor belongs to `self.events`, which is borrowed for the
                // call, and removing a watch takes only its number; one the kernel removed
                // already fails without effect.
                unsafe { libc::inotify_rm_watch(self.events.as_raw_fd(), old) };
            }
            self.file = file;
            Ok(())
        }

        fn poll(&self, events: bool, timeout: Duration) -> io::Result<Woke> {
            let mut fds = [
                libc::pollfd {
                    fd: self.bell.0.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.events.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let count = if events { 2 } else { 1 };
            let spec = libc::timespec {
                tv_sec: timeout.as_secs().min(i64::MAX as u64) as libc::time_t,
                tv_nsec: timeout.subsec_nanos() as libc::c_long,
            };
            loop {
                // SAFETY: `fds` is a live array of at least `count` initialized `pollfd`s whose
                // descriptors belong to this watch, `spec` is a live `timespec`, and a null signal
                // mask leaves the thread's mask as it is. The call keeps no pointer past its
                // return.
                let ready =
                    unsafe { libc::ppoll(fds.as_mut_ptr(), count, &spec, std::ptr::null()) };
                if ready == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                return Ok(match (fds[0].revents, ready) {
                    (0, 0) => Woke::Timeout,
                    (0, _) => Woke::Events,
                    _ => Woke::Bell,
                });
            }
        }

        fn drain(&mut self) -> io::Result<bool> {
            let mut buf = [0u8; 4096];
            let mut changed = false;
            let mut moved = false;
            loop {
                let got = match (&self.events).read(&mut buf) {
                    Ok(0) => break,
                    Ok(got) => got,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                };
                let mut at = 0;
                while at + EVENT_BYTES <= got {
                    let word = |from: usize| -> [u8; 4] {
                        buf[at + from..at + from + 4].try_into().unwrap_or_default()
                    };
                    let wd = libc::c_int::from_ne_bytes(word(0));
                    let mask = u32::from_ne_bytes(word(4));
                    let len = u32::from_ne_bytes(word(12)) as usize;
                    let named =
                        &buf[(at + EVENT_BYTES).min(got)..(at + EVENT_BYTES + len).min(got)];
                    let named = &named[..named.iter().position(|b| *b == 0).unwrap_or(named.len())];
                    at += EVENT_BYTES + len;
                    if mask & libc::IN_Q_OVERFLOW != 0 {
                        changed = true;
                        moved = true;
                    } else if Some(wd) == self.file {
                        changed = true;
                        moved |= mask & libc::IN_IGNORED != 0;
                    } else if wd == self.dir && named == self.name.as_slice() {
                        changed = true;
                        moved = true;
                    }
                }
            }
            if moved {
                self.rewatch()?;
            }
            Ok(changed)
        }

        /// Waits until the file is written, a file is created, removed or renamed at its name,
        /// the inotify queue overflows, or `timeout` passes, and returns `false` once the bell has
        /// rung.
        ///
        /// # Errors
        ///
        /// The error of a failed `ppoll` or read of the events, or of watching a new file at the
        /// name.
        pub(crate) fn wait(&mut self, timeout: Duration) -> io::Result<bool> {
            let deadline = Instant::now().checked_add(timeout);
            loop {
                let left =
                    deadline.map_or(timeout, |at| at.saturating_duration_since(Instant::now()));
                match self.poll(true, left)? {
                    Woke::Bell => return Ok(false),
                    Woke::Timeout => return Ok(true),
                    Woke::Events if self.drain()? => return Ok(true),
                    Woke::Events => {}
                }
            }
        }

        /// Waits for `pause` or the bell, leaving whatever the file does meanwhile for the next
        /// [`Watch::wait`], and returns `false` once the bell has rung.
        ///
        /// # Errors
        ///
        /// The error of a failed `ppoll`.
        pub(crate) fn rest(&self, pause: Duration) -> io::Result<bool> {
            Ok(!matches!(self.poll(false, pause)?, Woke::Bell))
        }
    }
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
mod notify {
    use std::io;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    /// Ends a [`Watch`]'s waits. No target but 64-bit Linux has one, since none has a [`Watch`].
    pub(crate) enum Bell {}

    impl Bell {
        /// Wakes the watch's wait and makes every later one return at once.
        pub(crate) fn ring(&self) {
            match *self {}
        }
    }

    /// A watch on a database file and its directory, which only 64-bit Linux has, through
    /// inotify. Elsewhere a follower looks at the file at intervals instead.
    pub(crate) enum Watch {}

    impl Watch {
        /// Fails with an `Unsupported` error, since this target has no watch.
        pub(crate) fn new(_path: &Path) -> io::Result<Watch> {
            Err(io::ErrorKind::Unsupported.into())
        }

        /// The bell that ends this watch's waits.
        pub(crate) fn bell(&self) -> Arc<Bell> {
            match *self {}
        }

        /// Waits until the file changes or `timeout` passes, and returns `false` once the bell
        /// has rung.
        pub(crate) fn wait(&mut self, _timeout: Duration) -> io::Result<bool> {
            match *self {}
        }

        /// Waits for `pause` or the bell, and returns `false` once the bell has rung.
        pub(crate) fn rest(&self, _pause: Duration) -> io::Result<bool> {
            match *self {}
        }
    }
}

pub(crate) use notify::*;
