//! Owned advisory locks released independently of inherited file descriptors.

use std::{
    fs::File,
    io,
    ops::{Deref, DerefMut},
};

pub(crate) struct FileLock(File);

impl FileLock {
    pub fn exclusive(file: File) -> io::Result<Self> {
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(Self(file))
    }

    pub fn try_exclusive(file: File) -> io::Result<Self> {
        fs2::FileExt::try_lock_exclusive(&file)?;
        Ok(Self(file))
    }

    pub fn try_shared(file: File) -> io::Result<Self> {
        fs2::FileExt::try_lock_shared(&file)?;
        Ok(Self(file))
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
        let _ = fs2::FileExt::unlock(&self.0);
    }
}
