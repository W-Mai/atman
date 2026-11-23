//! Never enumerate a directory by reopening its string path.

use std::{
    ffi::{CStr, CString, OsStr},
    fs::File,
    io,
    mem::MaybeUninit,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path},
    ptr::NonNull,
};

use tokio_util::sync::CancellationToken;

use super::{BrowseError, MAX_SCANNED, valid_name};

const FLAGS: libc::c_int = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_RDONLY;
type DirectoryTrail = Vec<(u64, u64)>;

pub(super) fn walk(root: &Path, parts: &[String]) -> io::Result<(File, DirectoryTrail)> {
    walk_path(root, parts, false)
}

pub(super) fn walk_trusted(root: &Path, parts: &[String]) -> io::Result<(File, DirectoryTrail)> {
    walk_path(root, parts, true)
}

fn walk_path(root: &Path, parts: &[String], trusted: bool) -> io::Result<(File, DirectoryTrail)> {
    let mut directory = File::open("/")?;
    let mut metadata = directory.metadata()?;
    if trusted {
        require_trusted(&metadata, true)?;
    }
    let mut trail = vec![super::identity(&metadata)];
    for component in root.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_directory(&directory, name, false)?;
                metadata = directory.metadata()?;
                if trusted {
                    require_trusted(&metadata, true)?;
                }
                trail.push(super::identity(&metadata));
            }
            _ => return Err(io::Error::from(io::ErrorKind::InvalidInput)),
        }
    }
    #[cfg(target_os = "linux")]
    if parts.is_empty() {
        directory = open_directory(&directory, OsStr::new("."), true)?;
    }
    let mut metadata = directory.metadata()?;
    if trusted {
        require_trusted(&metadata, false)?;
    }
    let device = metadata.dev();
    for name in parts {
        directory = open_directory(&directory, OsStr::new(name), true)?;
        metadata = directory.metadata()?;
        if trusted {
            require_trusted(&metadata, false)?;
        }
        if metadata.dev() != device {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        trail.push((device, metadata.ino()));
    }
    Ok((directory, trail))
}

fn require_trusted(metadata: &std::fs::Metadata, sticky_parent: bool) -> io::Result<()> {
    // A sticky parent is safe only through a child owned by a trusted user.
    let owned = metadata.uid() == 0 || metadata.uid() == unsafe { libc::geteuid() };
    if !owned || (metadata.mode() & 0o022 != 0 && !(sticky_parent && metadata.mode() & 0o1000 != 0))
    {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    Ok(())
}

pub(super) fn check_project_config(workspace: &File) -> io::Result<()> {
    let directory = match open_directory(workspace, OsStr::new(".atman"), true) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    require_trusted(&directory.metadata()?, false)?;
    // O_NONBLOCK allows inspecting the inode without blocking on a FIFO.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c"config.toml".as_ptr(),
            libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_RDONLY,
        )
    };
    match open_result(fd) {
        Ok(file) => {
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.nlink() != 1 || !same_mount(&directory, &file)? {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            require_trusted(&metadata, false)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn open_directory(parent: &File, name: &OsStr, contained: bool) -> io::Result<File> {
    let name = CString::new(name.as_bytes())?;
    #[cfg(target_os = "linux")]
    if contained {
        match open_contained(parent, &name) {
            Ok(file) => return Ok(file),
            Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {}
            Err(error) => return Err(error),
        }
    }
    // The parent fd and nul-terminated component are live for the syscall.
    let file = open_result(unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), FLAGS) })?;
    if contained && !same_mount(parent, &file)? {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
fn open_contained(parent: &File, name: &CStr) -> io::Result<File> {
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = FLAGS as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_XDEV;
    // The live fd, name, and zero-initialized open_how are valid for this call.
    open_result(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent.as_raw_fd(),
            name.as_ptr(),
            &how,
            std::mem::size_of_val(&how),
        ) as libc::c_int
    })
}

fn open_result(fd: libc::c_int) -> io::Result<File> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // The open syscall returned a new descriptor owned by this File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn same_mount(parent: &File, child: &File) -> io::Result<bool> {
    fn mount_id(file: &File) -> io::Result<u64> {
        let mut stat = MaybeUninit::<libc::statx>::uninit();
        // An empty path with AT_EMPTY_PATH queries exactly the opened descriptor.
        let result = unsafe {
            libc::syscall(
                libc::SYS_statx,
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_NO_AUTOMOUNT,
                libc::STATX_MNT_ID,
                stat.as_mut_ptr(),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // statx initialized the record on success. Only consume an acknowledged field.
        let stat = unsafe { stat.assume_init() };
        if stat.stx_mask & libc::STATX_MNT_ID == 0 {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }
        Ok(stat.stx_mnt_id)
    }
    Ok(mount_id(parent)? == mount_id(child)?)
}

#[cfg(target_os = "macos")]
fn same_mount(parent: &File, child: &File) -> io::Result<bool> {
    fn stat(file: &File) -> io::Result<libc::statfs> {
        let mut mount = MaybeUninit::uninit();
        if unsafe { libc::fstatfs(file.as_raw_fd(), mount.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // fstatfs initialized the mount record on success.
        Ok(unsafe { mount.assume_init() })
    }
    Ok(stat(parent)?.f_mntonname == stat(child)?.f_mntonname)
}

struct DirectoryStream(NonNull<libc::DIR>);

impl DirectoryStream {
    fn new(file: &File) -> io::Result<Self> {
        let duplicate = file.try_clone()?;
        let stream = NonNull::new(unsafe { libc::fdopendir(duplicate.as_raw_fd()) })
            .ok_or_else(io::Error::last_os_error)?;
        // fdopendir owns the duplicate only after success.
        let _ = duplicate.into_raw_fd();
        Ok(Self(stream))
    }

    fn next(&mut self) -> io::Result<Option<&libc::dirent>> {
        // errno is thread-local. Clear it before distinguishing EOF from an error.
        unsafe {
            #[cfg(target_os = "linux")]
            let errno = libc::__errno_location();
            #[cfg(target_os = "macos")]
            let errno = libc::__error();
            *errno = 0;
            if let Some(entry) = libc::readdir(self.0.as_ptr()).as_ref() {
                return Ok(Some(entry));
            }
            match *errno {
                0 => Ok(None),
                error => Err(io::Error::from_raw_os_error(error)),
            }
        }
    }
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

pub(super) fn names(
    directory: &File,
    cancellation: &CancellationToken,
) -> super::Result<Vec<String>> {
    let mut stream = DirectoryStream::new(directory).map_err(super::directory_error)?;
    let mut count = 0;
    let mut names = Vec::new();
    let device = directory.metadata().map_err(super::directory_error)?.dev();
    while let Some(entry) = stream.next().map_err(super::directory_error)? {
        if cancellation.is_cancelled() {
            return Err(BrowseError::Unavailable);
        }
        // readdir supplies a nul-terminated name valid until the next call.
        let name = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) };
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        count += 1;
        if count > MAX_SCANNED {
            return Err(BrowseError::TooLarge);
        }
        if !matches!(entry.d_type, libc::DT_DIR | libc::DT_UNKNOWN) {
            continue;
        }
        let Ok(text) = name.to_str() else {
            continue;
        };
        if valid_name(text) && child_is_directory(directory, name, device) {
            names.push(text.to_owned());
        }
    }
    Ok(names)
}

fn child_is_directory(directory: &File, name: &CStr, device: u64) -> bool {
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return false;
    }
    // fstatat initialized metadata for this component without following links.
    let metadata = unsafe { metadata.assume_init() };
    #[allow(clippy::unnecessary_cast)]
    let child_device = metadata.st_dev as u64;
    metadata.st_mode & libc::S_IFMT == libc::S_IFDIR && child_device == device
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_stream_stays_with_the_opened_directory_after_path_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let base = fixture.path().canonicalize().unwrap();
        let root = base.join("allowed");
        let selected = root.join("selected");
        let outside = base.join("outside");
        std::fs::create_dir_all(selected.join("expected")).unwrap();
        std::fs::create_dir_all(outside.join("outside-name")).unwrap();
        let (directory, _) = walk(&root, &["selected".into()]).unwrap();
        std::fs::rename(&selected, root.join("relocated")).unwrap();
        std::os::unix::fs::symlink(&outside, &selected).unwrap();
        assert_eq!(
            names(&directory, &CancellationToken::new()).unwrap(),
            ["expected"]
        );
        assert!(walk(&root, &["selected".into()]).is_err());
    }
}
