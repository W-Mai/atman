//! Durable publication of immutable resource contents.

use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use crate::event_log::journal::{private_file_options, sync_parent};

mod text;
pub(crate) use text::{TextIdentity, copy_append_only_text_with, copy_text_with, inspect_text};

const MIN_FREE_BYTES: u64 = 64 * 1024 * 1024;

/// Cooperative cancellation and disk checks shared by one immutable capture.
/// The owner keeps the optional reservation alive until its staged directory is retired.
#[derive(Clone, Default)]
pub(crate) struct CopyControl {
    cancelled: Arc<AtomicBool>,
    staging_root: Option<PathBuf>,
}

#[allow(dead_code)]
impl CopyControl {
    pub(crate) fn new(staging_root: PathBuf) -> Self {
        Self::with_cancelled(staging_root, Arc::new(AtomicBool::new(false)))
    }

    pub(crate) fn with_cancelled(staging_root: PathBuf, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            staging_root: Some(staging_root),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn check(&self) -> io::Result<()> {
        if self.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "source capture cancelled",
            ));
        }
        if let Some(root) = &self.staging_root {
            let available = fs2::available_space(root)?;
            if available < MIN_FREE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "insufficient free space for source capture",
                ));
            }
        }
        Ok(())
    }
}

/// Host-shared upper bound for concurrent uncommitted staging operations.
#[allow(dead_code)]
pub(crate) struct StagingCapacity {
    root: PathBuf,
    limit: usize,
    reserved: AtomicUsize,
}

#[allow(dead_code)]
impl StagingCapacity {
    pub(crate) fn new(root: PathBuf, limit: usize) -> io::Result<Arc<Self>> {
        if limit == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staging capacity must be nonzero",
            ));
        }
        fs::create_dir_all(&root)?;
        Ok(Arc::new(Self {
            root,
            limit,
            reserved: AtomicUsize::new(0),
        }))
    }

    pub(crate) fn reserve(self: &Arc<Self>, bytes: usize) -> io::Result<StagingReservation> {
        if bytes > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "source capture exceeds staging capacity",
            ));
        }
        let available = fs2::available_space(&self.root)?;
        let required = u64::try_from(bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(MIN_FREE_BYTES);
        if available < required {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "insufficient free space for source capture",
            ));
        }
        let mut current = self.reserved.load(Ordering::Acquire);
        loop {
            let next = current.checked_add(bytes).ok_or_else(|| {
                io::Error::new(io::ErrorKind::StorageFull, "staging capacity overflow")
            })?;
            if next > self.limit {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "concurrent source captures exceed staging capacity",
                ));
            }
            match self.reserved.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(StagingReservation {
                        capacity: self.clone(),
                        bytes,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

pub(crate) struct StagingReservation {
    capacity: Arc<StagingCapacity>,
    bytes: usize,
}

impl StagingReservation {
    #[allow(dead_code)]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for StagingReservation {
    fn drop(&mut self) {
        self.capacity
            .reserved
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Read one managed regular file with a hard allocation bound and an opened-handle identity check.
pub(crate) fn read_bounded(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    read_bounded_with(path, max_bytes, &CopyControl::default())
}

pub(crate) fn read_bounded_with(
    path: &Path,
    max_bytes: usize,
    control: &CopyControl,
) -> io::Result<Vec<u8>> {
    control.check()?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("resource path has no parent"))?;
    let directory = fs::symlink_metadata(parent)?;
    let before = fs::symlink_metadata(path)?;
    if !directory.is_dir() || !before.is_file() || before.len() > max_bytes as u64 {
        return Err(io::Error::other(
            "resource is not a bounded managed regular file",
        ));
    }
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() > max_bytes as u64 || !same_file(&before, &opened) {
        return Err(io::Error::other("resource identity changed while opening"));
    }
    let limit = (max_bytes as u64)
        .checked_add(1)
        .ok_or_else(|| io::Error::other("resource size overflow"))?;
    let mut bytes = Vec::new();
    let mut reader = (&file).take(limit);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        control.check()?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > max_bytes {
            break;
        }
    }
    let after = fs::symlink_metadata(path)?;
    let parent_after = fs::symlink_metadata(parent)?;
    if bytes.len() > max_bytes
        || bytes.len() as u64 != opened.len()
        || !after.is_file()
        || !parent_after.is_dir()
        || !same_file(&directory, &parent_after)
        || !same_file(&opened, &after)
    {
        return Err(io::Error::other("resource changed while reading"));
    }
    Ok(bytes)
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    {
        left.len() == right.len()
            && left.created().ok() == right.created().ok()
            && left.modified().ok() == right.modified().ok()
    }
}

pub(crate) fn publish(path: &Path, bytes: &[u8]) -> io::Result<()> {
    publish_with_control(path, bytes, &CopyControl::default())
}

pub(crate) fn publish_with_control(
    path: &Path,
    bytes: &[u8],
    control: &CopyControl,
) -> io::Result<()> {
    control.check()?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("resource path has no parent"))?;
    fs::create_dir_all(parent)?;
    sync_parent(parent)?;
    match fs::symlink_metadata(path) {
        Ok(_) => return verify(path, bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut file = private_file_options().create_new(true).open(&temporary)?;
    let result = (|| {
        for chunk in bytes.chunks(64 * 1024) {
            control.check()?;
            file.write_all(chunk)?;
        }
        file.sync_all()?;
        drop(file);
        control.check()?;
        match fs::hard_link(&temporary, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    })();
    let cleanup = fs::remove_file(temporary);
    result?;
    cleanup?;
    verify(path, bytes)
}

fn verify(path: &Path, expected: &[u8]) -> io::Result<()> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other(
            "resource destination is not a regular file",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.write(true);
    let file = options.open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() != expected.len() as u64 {
        return Err(io::Error::other(
            "stored resource differs from expected contents",
        ));
    }
    let mut actual = Vec::new();
    (&file)
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut actual)?;
    if actual != expected {
        return Err(io::Error::other(
            "stored resource differs from expected contents",
        ));
    }
    file.sync_all()?;
    sync_parent(path)
}

#[cfg(test)]
mod control_tests {
    use super::*;

    #[test]
    fn cancellation_is_observed_by_copy_control() {
        let directory = tempfile::tempdir().unwrap();
        let control = CopyControl::new(directory.path().to_owned());
        control.cancel();
        assert_eq!(
            control.check().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn staging_capacity_serializes_concurrent_upper_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let capacity = StagingCapacity::new(directory.path().to_owned(), 10).unwrap();
        let first = capacity.reserve(8).unwrap();
        assert!(capacity.reserve(3).is_err());
        drop(first);
        let second = capacity.reserve(10).unwrap();
        assert_eq!(second.bytes(), 10);
    }
}
