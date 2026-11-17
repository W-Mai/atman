//! Bounded diagnostics for a native host whose terminal has retired.

use atman_runtime::platform::ExclusiveFileLease;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
use uuid::Uuid;

const LIMIT: u64 = 4 * 1024 * 1024;
const TAIL: u64 = 1024 * 1024;

pub(crate) struct Diagnostics {
    path: PathBuf,
    file: File,
    _lease: ExclusiveFileLease,
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
}

impl Diagnostics {
    pub fn prepare(data: &Path) -> io::Result<Self> {
        let directory = data.join("run").join("host-logs");
        fs::create_dir_all(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        }
        let id = Uuid::new_v4();
        let lock = options().open(directory.join(format!("{id}.lock")))?;
        let lease = ExclusiveFileLease::try_acquire(lock)?;
        let path = directory.join(format!("{id}.log"));
        let file = options().append(true).open(&path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("host diagnostic is not a regular file"));
        }
        let diagnostics = Self {
            path,
            file,
            _lease: lease,
        };
        diagnostics.maintain()?;
        Ok(diagnostics)
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn maintain(&self) -> io::Result<()> {
        if self.file.metadata()?.len() > LIMIT {
            let mut reader = File::open(&self.path)?;
            reader.seek(SeekFrom::End(-(TAIL as i64)))?;
            let mut tail = Vec::with_capacity(TAIL as usize);
            reader.take(TAIL).read_to_end(&mut tail)?;
            atman_runtime::platform::atomic_replace(&self.path.with_extension("previous"), &tail)?;
            // Retain the redirected inode. Never unlink the current stdout/stderr file.
            options().create(false).open(&self.path)?.set_len(0)?;
        }
        let directory = self
            .path
            .parent()
            .expect("diagnostics have a containing directory");
        let mut retired = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "log")
                || !entry.file_type()?.is_file()
            {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| Uuid::parse_str(stem).ok())
            else {
                continue;
            };
            let lock_path = directory.join(format!("{id}.lock"));
            let lock = match options().create(false).open(lock_path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            match ExclusiveFileLease::try_acquire(lock) {
                Ok(lease) => retired.push((entry.metadata()?.modified()?, path, lease)),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        retired.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        for (_, path, _lease) in retired.into_iter().skip(4) {
            fs::remove_file(&path)?;
            match fs::remove_file(path.with_extension("previous")) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
