//! Small platform adapters shared by runtime persistence and process setup.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(windows)]
static PROCESS_CREATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Coordinate synchronous native handle duplication and process creation on Windows.
///
/// ConPTY creation temporarily creates inheritable handles. Owned ConPTY creation and broadly inheriting child creation must share this boundary. Embedders must use the same boundary for their own broadly inheriting child creation.
///
/// The closure must return after creation. Waiting, output collection, permission checks and async suspension belong outside this boundary. Calls must not nest.
pub fn with_process_creation<T>(create: impl FnOnce() -> T) -> T {
    #[cfg(windows)]
    let _creation = PROCESS_CREATION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    create()
}

/// Exclusive file ownership released even when another descriptor retains the file.
pub struct ExclusiveFileLease(crate::file_lock::FileLock);

impl ExclusiveFileLease {
    /// Try ownership without waiting. The caller opens and validates its own file.
    pub fn try_acquire(file: std::fs::File) -> io::Result<Self> {
        crate::file_lock::FileLock::try_exclusive(file).map(Self)
    }

    /// Inspect the retained file without reopening its pathname.
    pub fn file(&self) -> &std::fs::File {
        &self.0
    }
}

/// Retain a file for positioned reads, with a separate file cursor on Windows.
pub(crate) fn positioned_reader(file: &std::fs::File) -> io::Result<std::fs::File> {
    #[cfg(not(windows))]
    {
        file.try_clone()
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::{
            Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE},
            Storage::FileSystem::{
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, ReOpenFile,
            },
        };
        // Reopen the existing file object rather than its path. A replaced pathname must
        // not redirect a retained snapshot, and DuplicateHandle would share its cursor.
        let handle = unsafe {
            ReOpenFile(
                file.as_raw_handle(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { std::fs::File::from_raw_handle(handle) })
        }
    }
}

/// Resolve the current user's home directory without assuming a Unix-only environment variable.
pub fn user_home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Write bytes to a same-directory temporary file and atomically install them at `path`.
pub fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temporary = parent.join(format!(
        ".{name}.atman-tmp-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        install_replacement(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn install_replacement(temporary: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(temporary, destination)
}

#[cfg(windows)]
fn install_replacement(temporary: &Path, destination: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    const MAX_REPLACE_ATTEMPTS: usize = 8;
    const RETRYABLE_ERRORS: [i32; 2] = [32, 33];
    let destination_path = destination.to_path_buf();
    let temporary_path = temporary.to_path_buf();
    let destination = wide_path(&destination_path);
    let temporary = wide_path(&temporary_path);
    if destination_path_exists(destination.as_ptr()) {
        for attempt in 0..MAX_REPLACE_ATTEMPTS {
            let result = unsafe {
                ReplaceFileW(
                    destination.as_ptr(),
                    temporary.as_ptr(),
                    std::ptr::null(),
                    REPLACEFILE_WRITE_THROUGH,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            if result != 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            let retryable = error
                .raw_os_error()
                .is_some_and(|code| RETRYABLE_ERRORS.contains(&code));
            if !retryable || attempt + 1 == MAX_REPLACE_ATTEMPTS {
                return Err(error);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        unreachable!("replace attempts always return or continue");
    } else {
        std::fs::rename(temporary_path, destination_path)
    }
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn destination_path_exists(path: *const u16) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetFileAttributesW;
    const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
    unsafe { GetFileAttributesW(path) != INVALID_FILE_ATTRIBUTES }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_replace_writes_new_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested").join("value.txt");
        atomic_replace(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
    }

    #[test]
    fn atomic_replace_replaces_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("value.txt");
        std::fs::write(&path, b"before").unwrap();
        atomic_replace(&path, b"after").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
    }
}
