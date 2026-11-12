//! Independently owned typed images and registered outputs for source snapshots.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use super::super::invalid;
use crate::{
    attachment_store::{AttachmentStore, MAX_IMAGE_BYTES, retained_image_bytes_with},
    message::{ImageData, ImageSource, Message, MessagePart},
    retained_file::{CopyControl, StagingReservation},
    tools::tool_output::{OutputSource, OutputStore},
};

mod logs;
pub use logs::RetainedBashLog;

/// Uncommitted assets with independent file ownership. Keep this owner alive through destination admission; dropping it retires its scratch directory on storage workers.
///
/// History records and accepted-operation recovery belong to the enclosing snapshot protocol.
pub struct RetainedSourceAssets {
    directory: Option<tempfile::TempDir>,
    reservation: Option<StagingReservation>,
    images: BTreeMap<String, (String, PathBuf)>,
    outputs: Vec<OutputSource>,
    logs: Vec<RetainedBashLog>,
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
        Self::capture_with_control(
            messages,
            outputs,
            staging_parent,
            max_bytes,
            CopyControl::default(),
            None,
        )
    }

    pub(crate) fn capture_with_control<'a>(
        messages: impl IntoIterator<Item = &'a Message> + Clone,
        outputs: &OutputStore,
        staging_parent: &Path,
        max_bytes: usize,
        control: CopyControl,
        reservation: Option<StagingReservation>,
    ) -> io::Result<Self> {
        control.check()?;
        let sources = outputs
            .capture_references_with_budget_and_control(messages.clone(), max_bytes, &control)
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
                            retained_image_bytes_with(source, MAX_IMAGE_BYTES, &control)
                                .map_err(invalid)?;
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
                            let bytes = retained_image_bytes_with(source, remaining, &control)
                                .map_err(invalid)?;
                            let retained = images_store
                                .import_bytes_with_control(&bytes, None, &control)
                                .map_err(invalid)?;
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
        let outputs = OutputStore::at(directory.path())
            .import_references_with_control(messages, &sources, &control)?;
        Ok(Self {
            directory: Some(directory),
            reservation,
            images,
            outputs,
            logs: Vec::new(),
            bytes: max_bytes - remaining,
        })
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn identity_digest(&self) -> String {
        let mut digest = blake3::Hasher::new();
        digest.update(b"atman.retained-fork-assets\0");
        for (id, (media_type, _)) in &self.images {
            digest.update(&(id.len() as u64).to_le_bytes());
            digest.update(id.as_bytes());
            digest.update(&(media_type.len() as u64).to_le_bytes());
            digest.update(media_type.as_bytes());
        }
        for output in &self.outputs {
            let identity = output.identity_digest();
            digest.update(&(identity.len() as u64).to_le_bytes());
            digest.update(identity.as_bytes());
        }
        for log in &self.logs {
            digest.update(&(log.filename.len() as u64).to_le_bytes());
            digest.update(log.filename.as_bytes());
            digest.update(&(log.bytes as u64).to_le_bytes());
            digest.update(log.digest.as_bytes());
        }
        digest.finalize().to_hex().to_string()
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

    pub(crate) fn publish_outputs(
        &self,
        destination: &Path,
        messages: &[Message],
    ) -> io::Result<()> {
        OutputStore::at(destination).import_references(messages, &self.outputs)?;
        Ok(())
    }

    /// Publish captured images and managed process logs into one destination Flow directory,
    /// then rewrite only the typed paths that point at those captured files.
    pub(crate) fn publish_messages(
        &self,
        destination: &Path,
        messages: &mut [Message],
    ) -> io::Result<()> {
        let attachment_store = AttachmentStore::at(destination);
        let mut used_images = BTreeSet::new();
        for message in messages {
            for part in &mut message.parts {
                match part {
                    MessagePart::Image { source } => {
                        self.publish_image(&attachment_store, source, &mut used_images)?;
                    }
                    MessagePart::ContextRecord(record) => {
                        record.visit_images(&mut |source| {
                            self.publish_image(&attachment_store, source, &mut used_images)
                        })?;
                    }
                    MessagePart::ToolResult { content, .. } => {
                        rewrite_log_path(content, &self.logs, destination)?;
                    }
                    _ => {}
                }
            }
        }
        if used_images != self.images.keys().cloned().collect() {
            return Err(invalid(
                "captured image set does not match imported history",
            ));
        }
        for log in &self.logs {
            let source = self.directory().join(&log.filename);
            let target = destination.join(&log.filename);
            let identity = crate::retained_file::copy_append_only_text_with(
                &source,
                &target,
                log.bytes,
                &CopyControl::default(),
            )?;
            if identity.bytes != log.bytes || identity.digest != log.digest {
                return Err(invalid("captured process log changed before publication"));
            }
        }
        Ok(())
    }

    fn publish_image(
        &self,
        destination: &AttachmentStore,
        source: &mut ImageSource,
        used: &mut BTreeSet<String>,
    ) -> io::Result<()> {
        let ImageData::Artifact { id, path, name } = &source.data else {
            if matches!(source.data, ImageData::Path { .. }) {
                return Err(invalid("captured image has an unmanaged path"));
            }
            return Ok(());
        };
        let id = id.clone();
        let path = path.clone();
        let name = name.clone();
        let Some((media_type, retained_path)) = self.images.get(&id) else {
            return Err(invalid("captured image is absent from the asset set"));
        };
        if media_type != &source.media_type || path != *retained_path {
            return Err(invalid(
                "captured image identity changed before publication",
            ));
        }
        let bytes = fs::read(retained_path)?;
        let imported = destination
            .import_bytes(&bytes, name.as_deref())
            .map_err(invalid)?;
        let ImageData::Artifact {
            id: imported_id, ..
        } = &imported.data
        else {
            return Err(invalid("destination image has no retained artifact"));
        };
        if imported_id != &id {
            return Err(invalid("destination image identity changed"));
        }
        source.data = imported.data;
        used.insert(id);
        Ok(())
    }
}

fn rewrite_log_path(
    content: &mut String,
    logs: &[RetainedBashLog],
    destination: &Path,
) -> io::Result<()> {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(content) else {
        return Ok(());
    };
    let Some(path) = value.get("log_path").and_then(serde_json::Value::as_str) else {
        return Ok(());
    };
    let log = logs
        .iter()
        .find(|log| log.source_path == Path::new(path))
        .ok_or_else(|| invalid("managed process log path changed before publication"))?;
    value["log_path"] =
        serde_json::Value::String(destination.join(&log.filename).display().to_string());
    *content = serde_json::to_string(&value).map_err(invalid)?;
    Ok(())
}

impl Drop for RetainedSourceAssets {
    fn drop(&mut self) {
        let directory = self.directory.take();
        let reservation = self.reservation.take();
        if let Some(directory) = directory {
            // Release the capacity only after the worker has removed the scratch tree.
            crate::read_work::retire((directory, reservation));
        } else {
            drop(reservation);
        }
    }
}

#[cfg(test)]
mod tests;
