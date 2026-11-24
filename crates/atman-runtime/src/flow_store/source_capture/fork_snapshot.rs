//! A complete data-only conversation paired with independently owned preparation assets.

use std::path::Path;

use super::*;
use super::{
    assets::{ForkLogSelection, inspect_message_assets},
    full_read::Capacity,
};
use crate::projection::flow::{ConversationHistoryRecord, FlowRecord};
use crate::retained_file::{CopyControl, StagingReservation};

/// Read-only history and model windows at one source boundary. Provenance does not grant execution authority.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ForkSnapshot {
    pub boundary: ResolvedForkBoundary,
    pub records: Vec<ConversationHistoryRecord>,
    pub baseline: Option<ForkBaseline>,
    pub model_context: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inheritance: Option<ForkInheritance>,
}

/// Independently readable preparation, not a durable destination acceptance. Keep this owner alive until publication owns its copies.
pub struct RetainedForkSnapshot {
    content: Option<(ForkSnapshot, RetainedSourceAssets)>,
    digest: String,
}

impl ForkSnapshot {
    fn from_conversation(conversation: ForkConversation) -> io::Result<(Self, Vec<ForkLogAsset>)> {
        let mut records = Vec::new();
        let mut inherited_logs = Vec::new();
        let inheritance = conversation.inherited.map(|prior| {
            let proof = ForkInheritance {
                publication_digest: prior.digest.to_hex().to_string(),
                sources: prior.sources().cloned().collect(),
            };
            records.extend(prior.publication.history.records.into_iter().map(|record| {
                ConversationHistoryRecord {
                    source: record.source,
                    user_turn: record.user_turn,
                    record: record.record,
                }
            }));
            inherited_logs = prior.publication.logs;
            proof
        });
        records.extend(conversation.transcript.into_history_records()?);
        let data = Self {
            boundary: conversation.boundary,
            records,
            baseline: conversation.baseline,
            model_context: conversation.model_context,
            inheritance,
        };
        if data
            .records
            .iter()
            .any(|entry| matches!(entry.record, FlowRecord::Legacy(_)))
        {
            return Err(invalid(
                "native Fork preparation cannot omit archived record assets",
            ));
        }
        Ok((data, inherited_logs))
    }

    fn native_records(&self) -> &[ConversationHistoryRecord] {
        let first = self
            .records
            .partition_point(|record| record.source.flow != self.boundary.source.flow);
        &self.records[first..]
    }

    fn messages(&self) -> impl Iterator<Item = &Message> + Clone {
        self.records
            .iter()
            .flat_map(|entry| record_messages(&entry.record))
            .chain(self.baseline.iter().flat_map(|baseline| &baseline.messages))
            .chain(&self.model_context)
    }

    fn context_windows(&self) -> impl Iterator<Item = &[Message]> {
        self.records
            .iter()
            .filter_map(|entry| match &entry.record {
                FlowRecord::Input(_)
                | FlowRecord::Checkpoint(_)
                | FlowRecord::Continuation { .. } => Some(record_messages(&entry.record)),
                _ => None,
            })
            .chain(
                self.baseline
                    .iter()
                    .map(|baseline| baseline.messages.as_slice()),
            )
            .chain(std::iter::once(self.model_context.as_slice()))
    }

    pub(super) fn review(
        conversation: ForkConversation,
        directory: &Path,
        max_snapshot_bytes: usize,
        max_asset_bytes: usize,
        control: &CopyControl,
    ) -> io::Result<ForkSourceReview> {
        let (data, inherited) = Self::from_conversation(conversation)?;
        let mut capacity = Capacity(max_snapshot_bytes);
        serde_json::to_writer(&mut capacity, &data).map_err(invalid)?;
        let (_, message_bytes) = inspect_message_assets(
            data.messages(),
            &OutputStore::at(directory),
            max_asset_bytes,
            control,
            |_, _, _| Ok(()),
        )?;
        let log_bytes = ForkLogSelection {
            flow: &data.boundary.source.flow,
            directory,
            native_records: data.native_records(),
            inherited: &inherited,
        }
        .measure(
            data.context_windows(),
            max_asset_bytes - message_bytes,
            control,
        )?;
        Ok(ForkSourceReview {
            boundary: data.boundary,
            history_records: data.records.len(),
            snapshot_bytes: max_snapshot_bytes - capacity.0,
            asset_bytes: message_bytes + log_bytes,
        })
    }

    pub(super) fn digest(&self) -> io::Result<String> {
        let mut hash = blake3::Hasher::new();
        serde_json::to_writer(&mut hash, self).map_err(invalid)?;
        Ok(hash.finalize().to_hex().to_string())
    }

    fn digest_without_image_paths(&mut self) -> io::Result<String> {
        // A retained image's content ID survives preparation in a different scratch directory.
        visit_snapshot_messages_mut(self, &mut |message| {
            super::super::images::messages(std::slice::from_mut(message), &mut |image| {
                if let ImageData::Artifact { path, .. } = &mut image.data {
                    path.clear();
                }
                Ok(())
            })
        })?;
        self.digest()
    }
}

impl RetainedForkSnapshot {
    #[allow(dead_code)]
    pub(super) fn prepare(
        conversation: ForkConversation,
        source_directory: &Path,
        staging_parent: &Path,
        max_asset_bytes: usize,
    ) -> io::Result<Self> {
        Self::prepare_with_control(
            conversation,
            source_directory,
            staging_parent,
            max_asset_bytes,
            CopyControl::default(),
            None,
        )
    }

    pub(super) fn prepare_with_control(
        conversation: ForkConversation,
        source_directory: &Path,
        staging_parent: &Path,
        max_asset_bytes: usize,
        control: CopyControl,
        reservation: Option<StagingReservation>,
    ) -> io::Result<Self> {
        control.check()?;
        let (mut data, inherited) = ForkSnapshot::from_conversation(conversation)?;
        let assets = RetainedSourceAssets::capture_with_control(
            data.messages(),
            &OutputStore::at(source_directory),
            staging_parent,
            max_asset_bytes,
            control.clone(),
            reservation,
        )?;
        // Each checkpoint and inherited baseline is an independent model window, not a continuation of visible call pairing.
        let assets = ForkLogSelection {
            flow: &data.boundary.source.flow,
            directory: source_directory,
            native_records: data.native_records(),
            inherited: &inherited,
        }
        .retain(assets, data.context_windows(), max_asset_bytes, &control)?;
        let digest = data.digest_without_image_paths()?;
        visit_snapshot_messages_mut(&mut data, &mut |message| {
            assets.resolve_images(std::slice::from_mut(message))
        })?;
        Ok(Self {
            content: Some((data, assets)),
            digest,
        })
    }

    pub(super) fn digest(&self) -> &str {
        &self.digest
    }

    pub fn data(&self) -> &ForkSnapshot {
        &self
            .content
            .as_ref()
            .expect("retained snapshot owns its content")
            .0
    }

    pub fn assets(&self) -> &RetainedSourceAssets {
        &self
            .content
            .as_ref()
            .expect("retained snapshot owns its content")
            .1
    }

    pub(crate) fn publish_assets(
        &self,
        destination: &Path,
        data: &mut ForkSnapshot,
    ) -> io::Result<()> {
        let mut messages = Vec::new();
        visit_snapshot_messages(data, &mut |message| {
            messages.push(message.clone());
            Ok(())
        })?;
        self.assets().publish_outputs(destination, &messages)?;
        self.assets().publish_messages(destination, &mut messages)?;
        let mut index = 0;
        visit_snapshot_messages_mut(data, &mut |message| {
            let replacement = messages
                .get(index)
                .ok_or_else(|| invalid("snapshot message publication count changed"))?;
            *message = replacement.clone();
            index += 1;
            Ok(())
        })?;
        if index != messages.len() {
            return Err(invalid("snapshot message publication count changed"));
        }
        Ok(())
    }
}

impl Drop for RetainedForkSnapshot {
    fn drop(&mut self) {
        if let Some(content) = self.content.take() {
            crate::read_work::retire(content);
        }
    }
}

fn record_messages(record: &FlowRecord) -> &[Message] {
    match record {
        FlowRecord::Input(input) => &input.messages,
        FlowRecord::Publication(publication) => std::slice::from_ref(&publication.message),
        FlowRecord::Checkpoint(checkpoint) => &checkpoint.messages,
        FlowRecord::Continuation { messages, .. } => messages,
        FlowRecord::Execution(_) | FlowRecord::Completion(_) | FlowRecord::Legacy(_) => &[],
    }
}

fn visit_snapshot_messages(
    snapshot: &ForkSnapshot,
    visit: &mut impl FnMut(&Message) -> io::Result<()>,
) -> io::Result<()> {
    for entry in &snapshot.records {
        for message in record_messages(&entry.record) {
            visit(message)?;
        }
    }
    if let Some(baseline) = &snapshot.baseline {
        for message in &baseline.messages {
            visit(message)?;
        }
    }
    for message in &snapshot.model_context {
        visit(message)?;
    }
    Ok(())
}

fn visit_snapshot_messages_mut(
    snapshot: &mut ForkSnapshot,
    visit: &mut impl FnMut(&mut Message) -> io::Result<()>,
) -> io::Result<()> {
    for entry in &mut snapshot.records {
        match &mut entry.record {
            FlowRecord::Input(input) => {
                for message in &mut input.messages {
                    visit(message)?;
                }
            }
            FlowRecord::Publication(publication) => visit(&mut publication.message)?,
            FlowRecord::Checkpoint(checkpoint) => {
                for message in &mut checkpoint.messages {
                    visit(message)?;
                }
            }
            FlowRecord::Continuation { messages, .. } => {
                for message in messages {
                    visit(message)?;
                }
            }
            FlowRecord::Execution(_) | FlowRecord::Completion(_) | FlowRecord::Legacy(_) => {}
        }
    }
    if let Some(baseline) = &mut snapshot.baseline {
        for message in &mut baseline.messages {
            visit(message)?;
        }
    }
    for message in &mut snapshot.model_context {
        visit(message)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
