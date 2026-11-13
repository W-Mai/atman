//! Small platform adapters shared by runtime persistence and process setup.

use std::io;
use std::path::{Path, PathBuf};

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
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    let destination_path = destination.to_path_buf();
    let temporary_path = temporary.to_path_buf();
    let destination = wide_path(&destination_path);
    let temporary = wide_path(&temporary_path);
    if destination_path_exists(destination.as_ptr()) {
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
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    } else {
        std::fs::rename(temporary_path, destination_path)
    }
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
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
