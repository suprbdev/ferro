#[cfg(unix)]
mod imp {
    use std::fs::{File, Metadata, TryLockError};
    use std::io;
    use std::os::unix::fs::{MetadataExt, fchown};
    use std::path::Path;

    pub(crate) use std::os::unix::fs::FileExt;

    pub(crate) fn file_id(file: &File) -> io::Result<u64> {
        Ok(file.metadata()?.ino())
    }

    pub(crate) fn path_id(path: &Path) -> io::Result<u64> {
        Ok(std::fs::metadata(path)?.ino())
    }

    pub(crate) fn try_lock(file: &File) -> Result<(), TryLockError> {
        file.try_lock()
    }

    pub(crate) fn try_lock_shared(file: &File) -> Result<(), TryLockError> {
        file.try_lock_shared()
    }

    pub(crate) fn unlock(file: &File) -> io::Result<()> {
        file.unlock()
    }

    pub(crate) fn sync_reader(file: &File) -> io::Result<()> {
        file.sync_data()
    }

    pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
        match path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            Some(parent) => File::open(parent)?.sync_all(),
            None => Ok(()),
        }
    }

    pub(crate) fn copy_owner(file: &File, source: &Metadata) {
        let _ = fchown(file, Some(source.uid()), Some(source.gid()));
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::{File, Metadata, OpenOptions, TryLockError};
    use std::io::{self, ErrorKind};
    use std::os::windows::fs::{FileExt as _, OpenOptionsExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;

    use windows_sys::Win32::Foundation::{
        ERROR_LOCK_VIOLATION, ERROR_NOT_LOCKED, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFileInformationByHandle, LOCK_FILE_FLAGS, LOCKFILE_EXCLUSIVE_LOCK,
        LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, ReOpenFile, UnlockFileEx,
    };
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};

    const LOCK_AT: u64 = 1 << 62;

    pub(crate) trait FileExt {
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
        fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    }

    impl FileExt for File {
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

    pub(crate) fn file_id(file: &File) -> io::Result<u64> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow))
    }

    pub(crate) fn path_id(path: &Path) -> io::Result<u64> {
        file_id(&OpenOptions::new().access_mode(0).open(path)?)
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

    pub(crate) fn try_lock(file: &File) -> Result<(), TryLockError> {
        lock(file, LOCKFILE_EXCLUSIVE_LOCK)
    }

    pub(crate) fn try_lock_shared(file: &File) -> Result<(), TryLockError> {
        lock(file, 0)
    }

    pub(crate) fn unlock(file: &File) -> io::Result<()> {
        let mut range = lock_range();
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

    pub(crate) fn sync_reader(file: &File) -> io::Result<()> {
        match file.sync_data() {
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {}
            synced => return synced,
        }
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        let handle = unsafe { ReOpenFile(file.as_raw_handle(), FILE_GENERIC_WRITE, share, 0) };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        unsafe { File::from_raw_handle(handle) }.sync_data()
    }

    pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
        OpenOptions::new().write(true).open(path)?.sync_all()
    }

    pub(crate) fn copy_owner(_file: &File, _source: &Metadata) {}
}

pub(crate) use imp::*;
