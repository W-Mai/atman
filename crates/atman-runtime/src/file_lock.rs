//! Owned advisory locks released independently of inherited file descriptors.

use std::{
    fs::File,
    io,
    ops::{Deref, DerefMut},
};

pub(crate) struct FileLock(File);

impl FileLock {
    pub fn exclusive(file: File) -> io::Result<Self> {
        lock(&file, true, false)?;
        Ok(Self(file))
    }

    pub fn try_exclusive(file: File) -> io::Result<Self> {
        lock(&file, true, true)?;
        Ok(Self(file))
    }

    pub fn try_shared(file: File) -> io::Result<Self> {
        lock(&file, false, true)?;
        Ok(Self(file))
    }

    #[cfg(test)]
    pub(crate) fn release(&self) -> io::Result<()> {
        unlock(&self.0)
    }
}

impl Deref for FileLock {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl DerefMut for FileLock {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Closing this descriptor alone can leave flock held by a forked child until exec.
        let _ = unlock(&self.0);
    }
}

#[cfg(not(windows))]
fn lock(file: &File, exclusive: bool, nonblocking: bool) -> io::Result<()> {
    match (exclusive, nonblocking) {
        (true, false) => fs2::FileExt::lock_exclusive(file),
        (true, true) => fs2::FileExt::try_lock_exclusive(file),
        (false, _) => fs2::FileExt::try_lock_shared(file),
    }
}

#[cfg(not(windows))]
fn unlock(file: &File) -> io::Result<()> {
    fs2::FileExt::unlock(file)
}

// Windows byte-range locks also deny ordinary reads through other handles. Keep ownership
// outside file contents while overlapping legacy fs2 whole-file locks. Locking this byte
// does not extend or modify the file.
#[cfg(windows)]
fn ownership_range() -> windows_sys::Win32::System::IO::OVERLAPPED {
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
    const OWNERSHIP_BYTE: u64 = i64::MAX as u64 - 1;
    OVERLAPPED {
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 {
                Offset: OWNERSHIP_BYTE as u32,
                OffsetHigh: (OWNERSHIP_BYTE >> 32) as u32,
            },
        },
        ..Default::default()
    }
}

#[cfg(windows)]
fn lock(file: &File, exclusive: bool, nonblocking: bool) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    let flags = if exclusive {
        LOCKFILE_EXCLUSIVE_LOCK
    } else {
        0
    } | if nonblocking {
        LOCKFILE_FAIL_IMMEDIATELY
    } else {
        0
    };
    let result =
        unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut ownership_range()) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    fn writable(path: &std::path::Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
    }

    #[test]
    fn ownership_excludes_writers_without_blocking_content_reads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal");
        std::fs::write(&path, b"retained facts").unwrap();
        let owner = FileLock::exclusive(writable(&path)).unwrap();
        assert!(FileLock::try_exclusive(writable(&path)).is_err());
        assert!(FileLock::try_shared(File::open(&path).unwrap()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"retained facts");
        let inherited = owner.try_clone().unwrap();
        drop(owner);
        let next = FileLock::try_exclusive(writable(&path)).unwrap();
        assert_eq!(
            next.metadata().unwrap().len(),
            b"retained facts".len() as u64
        );
        drop(inherited);
        assert!(FileLock::try_exclusive(writable(&path)).is_err());
    }

    #[test]
    fn shared_owners_retain_exclusion_until_the_last_release() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal");
        std::fs::write(&path, b"facts").unwrap();
        let first = FileLock::try_shared(File::open(&path).unwrap()).unwrap();
        let second = FileLock::try_shared(File::open(&path).unwrap()).unwrap();
        assert!(FileLock::try_exclusive(writable(&path)).is_err());
        drop(first);
        assert!(FileLock::try_exclusive(writable(&path)).is_err());
        drop(second);
        FileLock::try_exclusive(writable(&path)).unwrap();
    }

    #[test]
    fn ownership_contends_with_legacy_whole_file_locks_in_both_directions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal");
        std::fs::write(&path, b"facts").unwrap();
        let legacy = writable(&path);
        fs2::FileExt::try_lock_exclusive(&legacy).unwrap();
        assert!(FileLock::try_exclusive(writable(&path)).is_err());
        #[cfg(windows)]
        assert_eq!(std::fs::read(&path).unwrap_err().raw_os_error(), Some(33));
        fs2::FileExt::unlock(&legacy).unwrap();
        let owner = FileLock::try_exclusive(writable(&path)).unwrap();
        assert!(fs2::FileExt::try_lock_exclusive(&legacy).is_err());
        drop(owner);
        fs2::FileExt::try_lock_exclusive(&legacy).unwrap();
        fs2::FileExt::unlock(&legacy).unwrap();
    }
}

#[cfg(windows)]
fn unlock(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    let result = unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut ownership_range()) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
