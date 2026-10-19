//! Managed image retention without changing the original historical event bytes.

use std::{collections::BTreeSet, fs, io, io::Read, path::Path};

use base64::{Engine, engine::general_purpose::STANDARD};

use super::{history::LegacyHistoryRecord, invalid};
use crate::{
    attachment_store::{MAX_IMAGE_BYTES, image_bytes},
    event::Event,
    message::{ImageData, ImageSource, MessagePart},
};

impl LegacyHistoryRecord {
    pub(super) fn retain_images(&mut self, session_dir: &Path) -> io::Result<()> {
        let Some(mut event) = self.project_event()? else {
            return Ok(());
        };
        visit_images(&mut event, |image| {
            let ImageData::Artifact { id, path, .. } = &image.data else {
                return Ok(());
            };
            let directory = session_dir.join("attachments");
            let recorded_directory = path
                .parent()
                .ok_or_else(|| invalid("legacy image has no parent directory"))?;
            if id.len() != 64
                || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
                || path.file_stem().and_then(|name| name.to_str()) != Some(id)
                || !fs::symlink_metadata(&directory)?.is_dir()
                || !fs::symlink_metadata(recorded_directory)?.is_dir()
                || recorded_directory.canonicalize()? != directory.canonicalize()?
                || !fs::symlink_metadata(path)?.is_file()
            {
                return Err(invalid(
                    "legacy image is outside its managed attachment store",
                ));
            }
            let mut bytes = Vec::new();
            fs::File::open(path)?
                .take(MAX_IMAGE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            let data = STANDARD.encode(&bytes);
            validate_image(image, id, &data)?;
            self.images.insert(id.clone(), data);
            Ok(())
        })
    }

    /// Validate retained content and resolve images without accessing the source filesystem.
    pub(crate) fn project_event(&self) -> io::Result<Option<Event>> {
        let parsed = serde_json::from_slice::<Event>(&self.bytes()?);
        let Ok(mut event) = parsed else {
            if !self.images.is_empty() {
                return Err(invalid("unparsed history cannot own retained images"));
            }
            return Ok(None);
        };
        let mut used = BTreeSet::new();
        visit_images(&mut event, |image| {
            if let ImageData::Artifact { id, .. } = &image.data {
                if let Some(data) = self.images.get(id) {
                    validate_image(image, id, data)?;
                    used.insert(id.clone());
                    image.data = ImageData::Base64 { data: data.clone() };
                }
            }
            Ok(())
        })?;
        if used.len() != self.images.len() {
            return Err(invalid(
                "retained image is not referenced by its historical event",
            ));
        }
        Ok(Some(event))
    }
}

fn validate_image(image: &ImageSource, id: &str, data: &str) -> io::Result<()> {
    if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(invalid("retained image exceeds the attachment size limit"));
    }
    let mut inline = image.clone();
    inline.data = ImageData::Base64 { data: data.into() };
    let bytes = image_bytes(&inline).map_err(invalid)?;
    if blake3::hash(&bytes).to_hex().as_str() != id {
        return Err(invalid(
            "retained image content does not match its artifact ID",
        ));
    }
    Ok(())
}

fn visit_images(
    event: &mut Event,
    mut visit: impl FnMut(&mut ImageSource) -> io::Result<()>,
) -> io::Result<()> {
    let messages = match event {
        Event::UserMsg { message, .. }
        | Event::AssistantMsg { message, .. }
        | Event::ToolResultMsg { message, .. }
        | Event::SystemMsg { message, .. }
        | Event::DeferredFormApplied { message, .. } => std::slice::from_mut(message),
        Event::Checkpoint { messages, .. } => messages.as_mut_slice(),
        _ => return Ok(()),
    };
    for message in messages {
        for part in &mut message.parts {
            if let MessagePart::Image { source } = part {
                visit(source)?;
            }
        }
    }
    Ok(())
}
