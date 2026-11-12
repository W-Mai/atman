//! Independently owned typed images and registered outputs for source snapshots.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use super::super::invalid;
use crate::{
    attachment_store::{AttachmentStore, MAX_IMAGE_BYTES, retained_image_bytes},
    message::{ImageData, ImageSource, Message, MessagePart},
    tools::tool_output::{OutputSource, OutputStore},
};

/// Uncommitted assets with independent file ownership. Keep this owner alive through destination admission; dropping it retires its scratch directory on storage workers.
///
/// This retains typed images and registered tool outputs. Process logs, history records and accepted-operation recovery belong to the enclosing snapshot protocol.
pub struct RetainedSourceAssets {
    directory: Option<tempfile::TempDir>,
    images: BTreeMap<String, (String, PathBuf)>,
    outputs: Vec<OutputSource>,
    bytes: usize,
}

impl RetainedSourceAssets {
    /// Blocking preparation for owner-captured messages, under the original source retention lease.
    pub fn capture<'a>(
        messages: impl IntoIterator<Item = &'a Message> + Clone,
        outputs: &OutputStore,
        staging_parent: &Path,
        max_bytes: usize,
    ) -> io::Result<Self> {
        let sources = outputs
            .capture_references_with_budget(messages.clone(), max_bytes)
            .map_err(invalid)?;
        let output_bytes = sources.iter().try_fold(0usize, |total, source| {
            total
                .checked_add(source.bytes())
                .ok_or_else(|| invalid("snapshot output size overflow"))
        })?;
        let mut remaining = max_bytes
            .checked_sub(output_bytes)
            .ok_or_else(|| invalid("snapshot assets exceed the copy byte budget"))?;
        let directory = tempfile::Builder::new()
            .prefix("atman-source-assets-")
            .tempdir_in(staging_parent)?;
        let images_store = AttachmentStore::at(directory.path());
        let mut images = BTreeMap::new();
        for message in messages.clone() {
            for part in &message.parts {
                let sources = match part {
                    MessagePart::Image { source } => std::slice::from_ref(source),
                    MessagePart::ContextRecord(record) => record.body().images(),
                    _ => continue,
                };
                for source in sources {
                    match &source.data {
                        ImageData::Base64 { .. } => {
                            // Inline data belongs to the bounded message payload, not the external copy budget.
                            retained_image_bytes(source, MAX_IMAGE_BYTES).map_err(invalid)?;
                        }
                        ImageData::Path { .. } => {
                            return Err(invalid("snapshot images require committed ownership"));
                        }
                        ImageData::Artifact { id, .. } => {
                            if let Some((previous, _)) = images.get(id) {
                                if previous != &source.media_type {
                                    return Err(invalid("snapshot image references disagree"));
                                }
                                continue;
                            }
                            let bytes = retained_image_bytes(source, remaining).map_err(invalid)?;
                            let retained =
                                images_store.import_bytes(&bytes, None).map_err(invalid)?;
                            let ImageData::Artifact {
                                id: copied_id,
                                path,
                                ..
                            } = retained.data
                            else {
                                return Err(invalid("snapshot image has no independent storage"));
                            };
                            if &copied_id != id {
                                return Err(invalid("snapshot image identity changed"));
                            }
                            remaining -= bytes.len();
                            images.insert(id.clone(), (source.media_type.clone(), path));
                        }
                    }
                }
            }
        }
        let outputs = OutputStore::at(directory.path()).import_references(messages, &sources)?;
        Ok(Self {
            directory: Some(directory),
            images,
            outputs,
            bytes: max_bytes - remaining,
        })
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn outputs(&self) -> &[OutputSource] {
        &self.outputs
    }

    pub fn directory(&self) -> &Path {
        self.directory
            .as_ref()
            .expect("asset owner retains its directory")
            .path()
    }

    /// Resolve only typed image references; arbitrary message text and tool arguments remain unchanged.
    pub fn resolve_images(&self, messages: &mut [Message]) -> io::Result<()> {
        super::super::images::messages(messages, &mut |source: &mut ImageSource| match &mut source
            .data
        {
            ImageData::Base64 { .. } => Ok(()),
            ImageData::Path { .. } => Err(invalid("snapshot images require committed ownership")),
            ImageData::Artifact { id, path, .. } => {
                let (media, retained_path) = self
                    .images
                    .get(id)
                    .ok_or_else(|| invalid("image is absent from captured snapshot assets"))?;
                if media != &source.media_type {
                    return Err(invalid("snapshot image media type changed"));
                }
                *path = retained_path.clone();
                Ok(())
            }
        })
    }
}

impl Drop for RetainedSourceAssets {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            crate::read_work::retire(directory);
        }
    }
}

#[cfg(test)]
mod tests;
