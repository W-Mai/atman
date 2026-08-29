use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;

use crate::error::RuntimeError;
use crate::message::{ImageData, ImageSource};

const ATTACHMENTS_DIR: &str = "attachments";
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

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
        let bytes = std::fs::read(path).map_err(|error| RuntimeError::AttachmentError {
            reason: format!("cannot read {}: {error}", path.display()),
        })?;
        self.import_bytes(&bytes, path.file_name().and_then(|name| name.to_str()))
    }

    pub fn import_base64(
        &self,
        data: &str,
        name: Option<&str>,
    ) -> Result<ImageSource, RuntimeError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| RuntimeError::AttachmentError {
                reason: format!("invalid base64 image: {error}"),
            })?;
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
        if !path.is_file() {
            std::fs::create_dir_all(&self.root).map_err(|error| RuntimeError::AttachmentError {
                reason: format!("cannot create attachment store: {error}"),
            })?;
            let temp_path = self
                .root
                .join(format!(".{id}.{}.tmp", uuid::Uuid::new_v4()));
            let write_result = (|| -> std::io::Result<()> {
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temp_path)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                std::fs::rename(&temp_path, &path)
            })();
            if let Err(error) = write_result {
                let _ = std::fs::remove_file(&temp_path);
                if !path.is_file() {
                    return Err(RuntimeError::AttachmentError {
                        reason: format!("cannot persist attachment: {error}"),
                    });
                }
            }
        }
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
        ImageData::Base64 { data } => base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| RuntimeError::AttachmentError {
                reason: format!("invalid base64 image: {error}"),
            })?,
        ImageData::Path { path } | ImageData::Artifact { path, .. } => std::fs::read(path)
            .map_err(|error| RuntimeError::AttachmentError {
                reason: format!("cannot read {}: {error}", path.display()),
            })?,
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
    }
}
