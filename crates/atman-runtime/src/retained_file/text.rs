//! Fixed-memory validation and independent publication of managed UTF-8 resources.

use super::*;
use crate::event_log::journal::ReadStamp;

const CHUNK_BYTES: usize = 64 * 1024;

pub(crate) struct TextIdentity {
    pub(crate) bytes: usize,
    pub(crate) lines: usize,
    pub(crate) digest: String,
}

#[allow(dead_code)]
pub(crate) fn inspect_text(path: &Path, max_bytes: usize) -> io::Result<TextIdentity> {
    inspect_text_with(path, max_bytes, &CopyControl::default())
}

pub(crate) fn inspect_text_with(
    path: &Path,
    max_bytes: usize,
    control: &CopyControl,
) -> io::Result<TextIdentity> {
    read_text_with(path, max_bytes, &mut io::sink(), control)
}

/// The caller validates reference semantics before any destination name becomes visible.
#[allow(dead_code)]
pub(crate) fn copy_text(
    source: &Path,
    destination: &Path,
    bytes: usize,
    digest: &str,
    validate: impl FnOnce(&TextIdentity) -> io::Result<()>,
) -> io::Result<()> {
    copy_text_with(
        source,
        destination,
        bytes,
        digest,
        validate,
        &CopyControl::default(),
    )
}

pub(crate) fn copy_text_with(
    source: &Path,
    destination: &Path,
    bytes: usize,
    digest: &str,
    validate: impl FnOnce(&TextIdentity) -> io::Result<()>,
    control: &CopyControl,
) -> io::Result<()> {
    control.check()?;
    let mut temporary = stage_copy(destination)?;
    let actual = read_text_with(source, bytes, temporary.as_file_mut(), control)?;
    if actual.bytes != bytes || actual.digest != digest {
        return Err(io::Error::other("output source contents changed"));
    }
    validate(&actual)?;
    publish_copy(source, destination, temporary, &actual, control, false)
}

pub(crate) enum LogExtent<'a> {
    Observed { max_bytes: usize },
    Checkpoint { bytes: usize, digest: &'a str },
}

/// Publish the original byte prefix, including non-UTF-8 process output.
pub(crate) fn copy_log(
    source: &Path,
    destination: &Path,
    extent: LogExtent<'_>,
    control: &CopyControl,
) -> io::Result<TextIdentity> {
    control.check()?;
    let mut scan = match extent {
        LogExtent::Observed { max_bytes } => TextScan::open_log(source, max_bytes, control)?,
        LogExtent::Checkpoint { bytes, .. } => TextScan::open_prefix(source, bytes, control)?,
    };
    let mut temporary = stage_copy(destination)?;
    scan.advance(scan.total_bytes(), control, |_, chunk| {
        temporary.write_all(chunk)
    })?;
    let actual = scan.identity()?;
    if let LogExtent::Checkpoint { digest, .. } = extent
        && actual.digest != digest
    {
        return Err(invalid_data("process log does not match its checkpoint"));
    }
    publish_copy(source, destination, temporary, &actual, control, true)?;
    Ok(actual)
}

fn stage_copy(destination: &Path) -> io::Result<tempfile::NamedTempFile> {
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("resource destination has no parent"))?;
    fs::create_dir_all(parent)?;
    if !fs::symlink_metadata(parent)?.is_dir() {
        return Err(io::Error::other("resource destination is not a directory"));
    }
    sync_parent(parent)?;
    tempfile::NamedTempFile::new_in(parent)
}

fn publish_copy(
    source: &Path,
    destination: &Path,
    temporary: tempfile::NamedTempFile,
    actual: &TextIdentity,
    control: &CopyControl,
    raw: bool,
) -> io::Result<()> {
    control.check()?;
    temporary.as_file().sync_all()?;
    // This links the newly written file, never the source resource.
    control.check()?;
    match fs::hard_link(temporary.path(), destination) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if same_file(
                &fs::symlink_metadata(source)?,
                &fs::symlink_metadata(destination)?,
            ) {
                return Err(io::Error::other(
                    "resource destination shares its source file",
                ));
            }
            let saved = if raw {
                let mut scan = TextScan::open_prefix(destination, actual.bytes, control)?;
                let stamp = ReadStamp::of(&scan.source.file)?;
                if stamp.length() != actual.bytes as u64 {
                    return Err(invalid_data("stored process log length changed"));
                }
                scan.advance(actual.bytes, control, |_, _| Ok(()))?;
                if ReadStamp::open(destination)? != stamp {
                    return Err(invalid_data("stored process log changed"));
                }
                scan.identity()?
            } else {
                inspect_text_with(destination, actual.bytes, control)?
            };
            if saved.bytes != actual.bytes || saved.digest != actual.digest {
                return Err(io::Error::other(
                    "stored resource differs from expected contents",
                ));
            }
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(windows)]
            options.write(true);
            options.open(destination)?.sync_all()?;
        }
        Err(error) => return Err(error),
    }
    temporary.close()?;
    control.check()?;
    sync_parent(destination)
}

#[allow(dead_code)]
fn read_text(path: &Path, max_bytes: usize, output: &mut impl Write) -> io::Result<TextIdentity> {
    read_text_with(path, max_bytes, output, &CopyControl::default())
}

fn read_text_with(
    path: &Path,
    max_bytes: usize,
    output: &mut impl Write,
    control: &CopyControl,
) -> io::Result<TextIdentity> {
    scan_text_with(path, max_bytes, control, |_, chunk| output.write_all(chunk))
}

/// Collect a window from the same stream that establishes the complete registered digest.
pub(crate) fn read_verified_text_window(
    path: &Path,
    bytes: usize,
    digest: &blake3::Hash,
    window: std::ops::Range<usize>,
    control: &CopyControl,
) -> io::Result<String> {
    let mut result = String::new();
    let identity = scan_text_with(path, bytes, control, |start, chunk| {
        let end = start + chunk.len();
        if window.start < end && start < window.end {
            let chunk = std::str::from_utf8(chunk).map_err(io::Error::other)?;
            result.push_str(utf8_window(
                chunk,
                window.start.saturating_sub(start)..window.end.min(end) - start,
            )?);
        }
        Ok(())
    })?;
    if identity.bytes != bytes || identity.digest != digest.to_hex().as_str() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registered artifact identity changed",
        ));
    }
    Ok(result)
}

pub(crate) fn utf8_window(text: &str, window: std::ops::Range<usize>) -> io::Result<&str> {
    let mut end = window.end.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(window.start..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid UTF-8 range boundary"))
}

fn scan_text_with(
    path: &Path,
    max_bytes: usize,
    control: &CopyControl,
    output: impl FnMut(usize, &[u8]) -> io::Result<()>,
) -> io::Result<TextIdentity> {
    let mut scan = TextScan::open(path, max_bytes, control)?;
    scan.advance(usize::MAX, control, output)?;
    scan.identity()
}

pub(super) struct TextSource {
    path: PathBuf,
    directory: fs::Metadata,
    opened: fs::Metadata,
    pub(super) file: fs::File,
    prefix_stamp: Option<ReadStamp>,
}

#[derive(Clone, Copy)]
enum ScanExtent {
    Text(usize),
    Log {
        max_bytes: usize,
        committed: Option<usize>,
    },
}

impl TextSource {
    fn open(path: &Path, extent: ScanExtent) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("resource source has no parent"))?;
        let directory = fs::symlink_metadata(parent)?;
        let before = fs::symlink_metadata(path)?;
        if !directory.is_dir() || !before.is_file() {
            return Err(io::Error::other(
                "resource is not a bounded managed regular file",
            ));
        }
        let max_bytes = match extent {
            ScanExtent::Text(max_bytes) | ScanExtent::Log { max_bytes, .. } => max_bytes,
        };
        if before.len() > max_bytes as u64 {
            return Err(invalid_data("resource exceeds its byte budget"));
        }
        let file = managed_read_options().open(path)?;
        let opened = file.metadata()?;
        let prefix_stamp = if let ScanExtent::Log { committed, .. } = extent {
            let stamp = ReadStamp::of(&file)?;
            let bytes = committed.map_or(opened.len(), |bytes| bytes as u64);
            if opened.len() < bytes || stamp.length() < bytes {
                return Err(invalid_data("resource ended before its log prefix"));
            }
            Some(stamp)
        } else {
            None
        };
        if !opened.is_file()
            || opened.len() > max_bytes as u64
            || (prefix_stamp.is_none() && !unchanged(&before, &opened))
        {
            return Err(invalid_data("resource identity changed while opening"));
        }
        let mut source = Self {
            path: path.to_owned(),
            directory,
            opened,
            file,
            prefix_stamp,
        };
        if source.prefix_stamp.is_some() {
            source.validate()?;
        }
        Ok(source)
    }

    pub(super) fn validate(&mut self) -> io::Result<()> {
        let after = fs::symlink_metadata(&self.path)?;
        let parent = fs::symlink_metadata(self.path.parent().expect("validated resource parent"))?;
        if !after.is_file() || !parent.is_dir() || !same_file(&self.directory, &parent) {
            return Err(invalid_data("resource changed while reading"));
        }
        if let Some(previous) = &mut self.prefix_stamp {
            let opened = ReadStamp::of(&self.file)?;
            let named = ReadStamp::open(&self.path)?;
            if !opened.continues_from(previous) || !named.continues_from(&opened) {
                return Err(invalid_data("resource prefix changed while reading"));
            }
            *previous = named;
        } else if !unchanged(&self.opened, &after)
            || !unchanged(&self.opened, &self.file.metadata()?)
        {
            return Err(invalid_data("resource changed while reading"));
        }
        Ok(())
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    fn bytes(&self) -> usize {
        self.opened.len() as usize
    }
}

pub(super) struct TextScan {
    pub(super) source: TextSource,
    buffer: Box<[u8]>,
    carry: usize,
    bytes: usize,
    lines: usize,
    last: Option<u8>,
    digest: blake3::Hasher,
    prefix_bytes: Option<usize>,
}

impl TextScan {
    pub(super) fn open(path: &Path, max_bytes: usize, control: &CopyControl) -> io::Result<Self> {
        Self::open_mode(path, ScanExtent::Text(max_bytes), control)
    }

    pub(super) fn open_prefix(
        path: &Path,
        bytes: usize,
        control: &CopyControl,
    ) -> io::Result<Self> {
        Self::open_mode(
            path,
            ScanExtent::Log {
                max_bytes: usize::MAX,
                committed: Some(bytes),
            },
            control,
        )
    }

    pub(super) fn open_log(
        path: &Path,
        max_bytes: usize,
        control: &CopyControl,
    ) -> io::Result<Self> {
        Self::open_mode(
            path,
            ScanExtent::Log {
                max_bytes,
                committed: None,
            },
            control,
        )
    }

    fn open_mode(path: &Path, extent: ScanExtent, control: &CopyControl) -> io::Result<Self> {
        control.check()?;
        let source = TextSource::open(path, extent)?;
        let prefix_bytes = match extent {
            ScanExtent::Text(_) => None,
            ScanExtent::Log { committed, .. } => Some(committed.unwrap_or_else(|| source.bytes())),
        };
        Ok(Self {
            source,
            buffer: vec![0; CHUNK_BYTES + 3].into_boxed_slice(),
            carry: 0,
            bytes: 0,
            lines: 0,
            last: None,
            digest: blake3::Hasher::new(),
            prefix_bytes,
        })
    }

    pub(super) fn scanned_bytes(&self) -> usize {
        self.bytes
    }

    pub(super) fn total_bytes(&self) -> usize {
        self.prefix_bytes.unwrap_or_else(|| self.source.bytes())
    }

    pub(super) fn advance(
        &mut self,
        budget: usize,
        control: &CopyControl,
        mut output: impl FnMut(usize, &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        control.check()?;
        let stop = self.bytes.saturating_add(budget).min(self.total_bytes());
        while self.bytes < stop {
            control.check()?;
            let capacity = CHUNK_BYTES.min(stop - self.bytes);
            let count = self
                .source
                .file
                .read(&mut self.buffer[self.carry..self.carry + capacity])?;
            if count == 0 {
                return Err(invalid_data("resource ended before its observed length"));
            }
            self.bytes += count;
            let end = self.carry + count;
            let fresh = &self.buffer[self.carry..end];
            self.digest.update(fresh);
            if self.prefix_bytes.is_some() {
                output(self.bytes - count, fresh)?;
                continue;
            }
            self.lines += fresh.iter().filter(|byte| **byte == b'\n').count();
            self.last = fresh.last().copied();
            let valid = match std::str::from_utf8(&self.buffer[..end]) {
                Ok(_) => end,
                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                Err(_) => return Err(invalid_data("output source is not UTF-8")),
            };
            output(self.bytes - end, &self.buffer[..valid])?;
            control.check()?;
            self.buffer.copy_within(valid..end, 0);
            self.carry = end - valid;
        }
        self.source.validate()?;
        if self.bytes == self.total_bytes() && self.carry != 0 {
            return Err(invalid_data("output source has incomplete UTF-8"));
        }
        Ok(())
    }

    pub(super) fn identity(&self) -> io::Result<TextIdentity> {
        if self.bytes != self.total_bytes() || self.carry != 0 {
            return Err(invalid_data("text verification is incomplete"));
        }
        Ok(TextIdentity {
            bytes: self.bytes,
            lines: self.lines + usize::from(self.last.is_some_and(|byte| byte != b'\n')),
            digest: self.digest.finalize().to_hex().to_string(),
        })
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn unchanged(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_file(left, right)
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
}

#[cfg(test)]
mod tests;
