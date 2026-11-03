#[cfg(unix)]
mod imp {
    use std::fs::{File, Metadata, OpenOptions, TryLockError};
    use std::io;
    use std::os::unix::fs::{MetadataExt, fchown};
    use std::path::{Path, PathBuf};

    pub(crate) use std::os::unix::fs::FileExt;

    pub(crate) fn file_id(file: &File) -> io::Result<u64> {
        Ok(file.metadata()?.ino())
    }

    pub(crate) fn path_id(path: &Path) -> io::Result<u64> {
        Ok(std::fs::metadata(path)?.ino())
    }

    pub(crate) fn open(options: &OpenOptions, path: &Path) -> io::Result<File> {
        options.open(path)
    }

    pub(crate) fn canonicalize(path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
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

    pub(crate) fn sync_reader(file: &File, _path: &Path) -> io::Result<()> {
        file.sync_data()
    }

    pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
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
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO,
        FileRenameInfoEx, GetFileInformationByHandle, LOCK_FILE_FLAGS, LOCKFILE_EXCLUSIVE_LOCK,
        LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, SYNCHRONIZE, SetFileInformationByHandle,
        UnlockFileEx,
    };
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
    use windows_sys::Win32::System::WindowsProgramming::{
        FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
    };

    const LOCK_AT: u64 = 1 << 62;
    const RACE_RETRIES: u32 = 20;

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

    pub(crate) fn open(options: &OpenOptions, path: &Path) -> io::Result<File> {
        racing(delete_pending, || options.open(path))
    }

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
        if unsafe { SetFileInformationByHandle(handle, FileRenameInfoEx, info.cast(), size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
        open(OpenOptions::new().write(true), path)?.sync_all()
    }

    pub(crate) fn copy_owner(_file: &File, _source: &Metadata) {}
}

pub(crate) use imp::*;
