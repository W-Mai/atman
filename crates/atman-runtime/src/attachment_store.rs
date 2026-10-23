use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine;

use crate::error::RuntimeError;
use crate::message::{ImageData, ImageSource};

const ATTACHMENTS_DIR: &str = "attachments";
pub(crate) const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AttachmentStore {
    root: PathBuf,
    persistent: bool,
}

impl AttachmentStore {
    pub fn at(session_dir: impl AsRef<Path>) -> Self {
        let session_dir = session_dir.as_ref();
        Self {
            root: session_dir.join(ATTACHMENTS_DIR),
            persistent: !session_dir.as_os_str().is_empty(),
        }
    }

    pub fn import_path(&self, path: impl AsRef<Path>) -> Result<ImageSource, RuntimeError> {
        let path = path.as_ref();
        let bytes = read_image_file(path)?;
        self.import_bytes(&bytes, path.file_name().and_then(|name| name.to_str()))
    }

    pub fn import_base64(
        &self,
        data: &str,
        name: Option<&str>,
    ) -> Result<ImageSource, RuntimeError> {
        let bytes = decode_image(data)?;
        self.import_bytes(&bytes, name)
    }

    pub fn import_bytes(
        &self,
        bytes: &[u8],
        name: Option<&str>,
    ) -> Result<ImageSource, RuntimeError> {
        validate_size(bytes)?;
        let (media_type, extension) = detect_image_type(bytes)?;
        if !self.persistent {
            return Ok(ImageSource {
                media_type: media_type.into(),
                data: ImageData::Base64 {
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                },
                detail: crate::provider::ImageDetail::Auto,
            });
        }
        let id = blake3::hash(bytes).to_hex().to_string();
        let path = self.root.join(format!("{id}.{extension}"));
        let persist = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&self.root)?;
            crate::event_log::journal::sync_parent(&self.root)?;
            match std::fs::symlink_metadata(&path) {
                Ok(_) => return verify_persisted_image(&path, bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let temp_path = self
                .root
                .join(format!(".{id}.{}.tmp", uuid::Uuid::new_v4()));
            let mut file = crate::event_log::journal::private_file_options()
                .create_new(true)
                .open(&temp_path)?;
            let write_result = (|| -> std::io::Result<()> {
                file.write_all(bytes)?;
                file.sync_all()?;
                drop(file);
                match std::fs::hard_link(&temp_path, &path) {
                    Ok(()) => Ok(()),
                    // Another writer may have published the same content address.
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                    Err(error) => Err(error),
                }
            })();
            let cleanup = std::fs::remove_file(&temp_path);
            write_result?;
            cleanup?;
            verify_persisted_image(&path, bytes)
        })();
        persist.map_err(|error| RuntimeError::AttachmentError {
            reason: format!("cannot persist attachment: {error}"),
        })?;
        Ok(ImageSource {
            media_type: media_type.into(),
            data: ImageData::Artifact {
                id,
                path,
                name: name.map(ToOwned::to_owned),
            },
            detail: crate::provider::ImageDetail::Auto,
        })
    }
}

pub fn image_bytes(source: &ImageSource) -> Result<Vec<u8>, RuntimeError> {
    let bytes = match &source.data {
        ImageData::Base64 { data } => decode_image(data)?,
        ImageData::Path { path } | ImageData::Artifact { path, .. } => read_image_file(path)?,
    };
    validate_size(&bytes)?;
    let (actual_media_type, _) = detect_image_type(&bytes)?;
    if source.media_type != actual_media_type {
        return Err(RuntimeError::AttachmentError {
            reason: format!(
                "image media type mismatch: declared {}, detected {actual_media_type}",
                source.media_type
            ),
        });
    }
    if let ImageData::Artifact { id, .. } = &source.data {
        let actual_id = blake3::hash(&bytes).to_hex().to_string();
        if actual_id != *id {
            return Err(RuntimeError::AttachmentError {
                reason: format!("attachment integrity check failed for {id}"),
            });
        }
    }
    Ok(bytes)
}

fn decode_image(data: &str) -> Result<Vec<u8>, RuntimeError> {
    if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(RuntimeError::AttachmentError {
            reason: "encoded image exceeds the attachment size limit".into(),
        });
    }
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| RuntimeError::AttachmentError {
            reason: format!("invalid base64 image: {error}"),
        })
}

fn read_image_file(path: &Path) -> Result<Vec<u8>, RuntimeError> {
    let read = (|| -> std::io::Result<Vec<u8>> {
        if !std::fs::metadata(path)?.is_file() {
            return Err(std::io::Error::other("image source is not a regular file"));
        }
        let file = std::fs::File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("image source is not a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    })();
    let bytes = read.map_err(|error| RuntimeError::AttachmentError {
        reason: format!("cannot read {}: {error}", path.display()),
    })?;
    validate_size(&bytes)?;
    Ok(bytes)
}

fn verify_persisted_image(path: &Path, expected: &[u8]) -> std::io::Result<()> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(std::io::Error::other(
            "attachment destination is not a regular file",
        ));
    }
    let actual = read_image_file(path).map_err(std::io::Error::other)?;
    if actual != expected {
        return Err(std::io::Error::other(
            "stored attachment does not match its content address",
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.write(true);
    options.open(path)?.sync_all()?;
    crate::event_log::journal::sync_parent(path)
}

pub fn image_base64(source: &ImageSource) -> Result<String, RuntimeError> {
    if let ImageData::Base64 { data } = &source.data {
        image_bytes(source)?;
        return Ok(data.clone());
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(image_bytes(source)?))
}

pub fn display_name(source: &ImageSource) -> String {
    match &source.data {
        ImageData::Artifact { id, name, .. } => name.clone().unwrap_or_else(|| id.clone()),
        ImageData::Path { path } => path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image")
            .to_string(),
        ImageData::Base64 { .. } => "image".into(),
    }
}

fn validate_size(bytes: &[u8]) -> Result<(), RuntimeError> {
    if bytes.is_empty() {
        return Err(RuntimeError::AttachmentError {
            reason: "image is empty".into(),
        });
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(RuntimeError::AttachmentError {
            reason: format!(
                "image is too large: {} bytes exceeds the {} byte limit",
                bytes.len(),
                MAX_IMAGE_BYTES
            ),
        });
    }
    Ok(())
}

fn detect_image_type(bytes: &[u8]) -> Result<(&'static str, &'static str), RuntimeError> {
    let detected = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(("image/jpeg", "jpg"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("image/gif", "gif"))
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(("image/webp", "webp"))
    } else {
        None
    };
    detected.ok_or_else(|| RuntimeError::AttachmentError {
        reason: "unsupported image format; expected PNG, JPEG, GIF, or WebP".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_1X1: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
    ];

    #[test]
    fn import_is_content_addressed_and_deduplicated() {
        let session = tempfile::tempdir().unwrap();
        let store = AttachmentStore::at(session.path());
        let first = store.import_bytes(PNG_1X1, Some("first.png")).unwrap();
        let second = store.import_bytes(PNG_1X1, Some("second.png")).unwrap();

        let ImageData::Artifact {
            id: first_id,
            path: first_path,
            ..
        } = first.data
        else {
            panic!("expected artifact")
        };
        let ImageData::Artifact {
            id: second_id,
            path: second_path,
            ..
        } = second.data
        else {
            panic!("expected artifact")
        };
        assert_eq!(first_id, second_id);
        assert_eq!(first_path, second_path);
        assert_eq!(std::fs::read(first_path).unwrap(), PNG_1X1);
    }

    #[test]
    fn integrity_mismatch_is_rejected() {
        let session = tempfile::tempdir().unwrap();
        let store = AttachmentStore::at(session.path());
        let source = store.import_bytes(PNG_1X1, None).unwrap();
        let ImageData::Artifact { path, .. } = &source.data else {
            panic!("expected artifact")
        };
        std::fs::write(path, b"not an image").unwrap();
        assert!(image_bytes(&source).is_err());
        assert!(store.import_bytes(PNG_1X1, None).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"not an image");
    }

    #[test]
    fn concurrent_imports_publish_one_verified_file_without_temporary_entries() {
        let session = tempfile::tempdir().unwrap();
        let store = AttachmentStore::at(session.path());
        let barrier = std::sync::Barrier::new(8);
        let images = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        store.import_bytes(PNG_1X1, Some("image.png")).unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(images.iter().all(|image| image == &images[0]));
        assert_eq!(image_bytes(&images[0]).unwrap(), PNG_1X1);
        assert_eq!(std::fs::read_dir(&store.root).unwrap().count(), 1);
        let ImageData::Artifact { path, .. } = &images[0].data else {
            panic!("artifact")
        };
        std::fs::remove_file(path).unwrap();
        std::fs::create_dir(path).unwrap();
        assert!(store.import_bytes(PNG_1X1, None).is_err());
        assert_eq!(std::fs::read_dir(&store.root).unwrap().count(), 1);
    }

    #[test]
    fn file_and_encoded_inputs_share_the_attachment_size_limit() {
        let session = tempfile::tempdir().unwrap();
        let path = session.path().join("large.png");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(PNG_1X1).unwrap();
        file.set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();
        let store = AttachmentStore::at(session.path());
        assert!(store.import_path(&path).is_err());
        assert!(
            image_bytes(&ImageSource {
                media_type: "image/png".into(),
                data: ImageData::Path { path },
                detail: crate::provider::ImageDetail::Auto,
            })
            .is_err()
        );
        let oversized = "A".repeat(MAX_IMAGE_BYTES.div_ceil(3) * 4 + 1);
        assert!(store.import_base64(&oversized, None).is_err());
        assert!(
            image_bytes(&ImageSource {
                media_type: "image/png".into(),
                data: ImageData::Base64 { data: oversized },
                detail: crate::provider::ImageDetail::Auto,
            })
            .is_err()
        );
        assert!(!store.root.exists());
        let inline = AttachmentStore::at("").import_bytes(PNG_1X1, None).unwrap();
        assert!(matches!(inline.data, ImageData::Base64 { .. }));
        assert_eq!(image_bytes(&inline).unwrap(), PNG_1X1);
    }

    #[cfg(unix)]
    #[test]
    fn source_symlinks_are_imported_but_managed_destination_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let session = tempfile::tempdir().unwrap();
        let source = session.path().join("source.png");
        std::fs::write(&source, PNG_1X1).unwrap();
        let link = session.path().join("link.png");
        symlink(&source, &link).unwrap();
        let store = AttachmentStore::at(session.path());
        let image = store.import_path(&link).unwrap();
        let ImageData::Artifact { path, .. } = &image.data else {
            panic!("artifact")
        };
        std::fs::remove_file(path).unwrap();
        symlink(&source, path).unwrap();
        assert!(store.import_path(&link).is_err());
        assert!(std::fs::symlink_metadata(path).unwrap().is_symlink());
        assert_eq!(std::fs::read(&source).unwrap(), PNG_1X1);
    }
}
