//! Fixed-memory validation and independent publication of managed UTF-8 resources.

use super::*;

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
    read_text_mode_with(path, max_bytes, &mut io::sink(), false, control)
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
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("resource destination has no parent"))?;
    fs::create_dir_all(parent)?;
    if !fs::symlink_metadata(parent)?.is_dir() {
        return Err(io::Error::other("resource destination is not a directory"));
    }
    sync_parent(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let actual = read_text_with(source, bytes, temporary.as_file_mut(), control)?;
    if actual.bytes != bytes || actual.digest != digest {
        return Err(io::Error::other("output source contents changed"));
    }
    validate(&actual)?;
    publish_copy(source, destination, temporary, &actual, control)
}

/// Capture the observed prefix of a managed append-only log without waiting for its writer to finish.
#[allow(dead_code)]
pub(crate) fn copy_append_only_text(
    source: &Path,
    destination: &Path,
    max_bytes: usize,
) -> io::Result<TextIdentity> {
    copy_append_only_text_with(source, destination, max_bytes, &CopyControl::default())
}

pub(crate) fn copy_append_only_text_with(
    source: &Path,
    destination: &Path,
    max_bytes: usize,
    control: &CopyControl,
) -> io::Result<TextIdentity> {
    control.check()?;
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("resource destination has no parent"))?;
    fs::create_dir_all(parent)?;
    if !fs::symlink_metadata(parent)?.is_dir() {
        return Err(io::Error::other("resource destination is not a directory"));
    }
    sync_parent(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let actual = read_text_mode_with(source, max_bytes, temporary.as_file_mut(), true, control)?;
    publish_copy(source, destination, temporary, &actual, control)?;
    Ok(actual)
}

fn publish_copy(
    source: &Path,
    destination: &Path,
    temporary: tempfile::NamedTempFile,
    actual: &TextIdentity,
    control: &CopyControl,
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
            let saved = inspect_text_with(destination, actual.bytes, control)?;
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
    read_text_mode(path, max_bytes, output, false)
}

#[allow(dead_code)]
fn read_text_mode(
    path: &Path,
    max_bytes: usize,
    output: &mut impl Write,
    append_only: bool,
) -> io::Result<TextIdentity> {
    read_text_mode_with(
        path,
        max_bytes,
        output,
        append_only,
        &CopyControl::default(),
    )
}

fn read_text_with(
    path: &Path,
    max_bytes: usize,
    output: &mut impl Write,
    control: &CopyControl,
) -> io::Result<TextIdentity> {
    read_text_mode_with(path, max_bytes, output, false, control)
}

fn read_text_mode_with(
    path: &Path,
    max_bytes: usize,
    output: &mut impl Write,
    append_only: bool,
    control: &CopyControl,
) -> io::Result<TextIdentity> {
    control.check()?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("resource source has no parent"))?;
    let directory = fs::symlink_metadata(parent)?;
    let before = fs::symlink_metadata(path)?;
    if !directory.is_dir() || !before.is_file() || before.len() > max_bytes as u64 {
        return Err(io::Error::other(
            "resource is not a bounded managed regular file",
        ));
    }
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    let consistent = |before: &fs::Metadata, after: &fs::Metadata| {
        unchanged(before, after)
            || (append_only && same_file(before, after) && after.len() > before.len())
    };
    if !opened.is_file() || opened.len() > max_bytes as u64 || !consistent(&before, &opened) {
        return Err(io::Error::other("resource identity changed while opening"));
    }
    let mut reader = (&file).take(if append_only { opened.len() } else { u64::MAX });
    let mut buffer = [0; CHUNK_BYTES + 3];
    let mut carry = 0;
    let mut bytes = 0usize;
    let mut lines = 0;
    let mut last = None;
    let mut digest = blake3::Hasher::new();
    loop {
        control.check()?;
        let count = reader.read(&mut buffer[carry..carry + CHUNK_BYTES])?;
        if count == 0 {
            if carry != 0 {
                return Err(io::Error::other("output source has incomplete UTF-8"));
            }
            break;
        }
        bytes = bytes
            .checked_add(count)
            .filter(|size| *size <= max_bytes)
            .ok_or_else(|| io::Error::other("resource exceeds its byte budget"))?;
        let fresh = &buffer[carry..carry + count];
        digest.update(fresh);
        lines += fresh.iter().filter(|byte| **byte == b'\n').count();
        last = fresh.last().copied();
        let end = carry + count;
        let valid = match std::str::from_utf8(&buffer[..end]) {
            Ok(_) => end,
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Err(io::Error::other("output source is not UTF-8")),
        };
        output.write_all(&buffer[..valid])?;
        control.check()?;
        buffer.copy_within(valid..end, 0);
        carry = end - valid;
    }
    let after = fs::symlink_metadata(path)?;
    let parent_after = fs::symlink_metadata(parent)?;
    if bytes as u64 != opened.len()
        || !after.is_file()
        || !parent_after.is_dir()
        || !same_file(&directory, &parent_after)
        || !consistent(&opened, &after)
        || !consistent(&opened, &file.metadata()?)
    {
        return Err(io::Error::other("resource changed while reading"));
    }
    Ok(TextIdentity {
        bytes,
        lines: lines + usize::from(last.is_some_and(|byte| byte != b'\n')),
        digest: digest.finalize().to_hex().to_string(),
    })
}

fn unchanged(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_file(left, right)
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
}

#[cfg(test)]
mod tests;
