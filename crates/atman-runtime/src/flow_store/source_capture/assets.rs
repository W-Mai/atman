//! Independently owned typed images and registered outputs for source snapshots.

use std::{
    collections::BTreeMap,
    io,
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
