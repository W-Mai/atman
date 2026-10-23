//! Durable publication of immutable resource contents.

use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
};

use crate::event_log::journal::{private_file_options, sync_parent};

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
