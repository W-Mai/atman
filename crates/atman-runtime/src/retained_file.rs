//! Durable publication of immutable resource contents.

use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
};

use crate::event_log::journal::{private_file_options, sync_parent};

mod text;
pub(crate) use text::{TextIdentity, copy_append_only_text, copy_text, inspect_text};

/// Read one managed regular file with a hard allocation bound and an opened-handle identity check.
pub(crate) fn read_bounded(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
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
    (&file).take(limit).read_to_end(&mut bytes)?;
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
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
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
