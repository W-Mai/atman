//! A complete data-only conversation paired with independently owned preparation assets.

use std::path::Path;

use super::*;
use crate::projection::flow::{ConversationHistoryRecord, FlowRecord};

/// Read-only history and model windows at one source boundary. Provenance does not grant execution authority.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ForkSnapshot {
    pub boundary: ResolvedForkBoundary,
    pub records: Vec<ConversationHistoryRecord>,
    pub baseline: Option<ForkBaseline>,
    pub model_context: Vec<Message>,
}

/// Independently readable preparation, not a durable destination acceptance. Keep this owner alive until publication owns its copies.
pub struct RetainedForkSnapshot {
    content: Option<(ForkSnapshot, RetainedSourceAssets)>,
}

impl ForkSnapshot {
    fn messages(&self) -> impl Iterator<Item = &Message> + Clone {
        self.records
            .iter()
            .flat_map(|entry| record_messages(&entry.record))
            .chain(self.baseline.iter().flat_map(|baseline| &baseline.messages))
            .chain(&self.model_context)
    }
}

impl RetainedForkSnapshot {
    pub(super) fn prepare(
        conversation: ForkConversation,
        source_directory: &Path,
        staging_parent: &Path,
        max_asset_bytes: usize,
    ) -> io::Result<Self> {
        let mut data = ForkSnapshot {
            boundary: conversation.boundary,
            records: conversation.transcript.into_history_records()?,
            baseline: conversation.baseline,
            model_context: conversation.model_context,
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
        let flow = &data.boundary.source.flow;
        let mut assets = RetainedSourceAssets::capture(
            data.messages(),
            &OutputStore::at(source_directory),
            staging_parent,
            max_asset_bytes,
        )?
        .retain_bash_logs(flow, source_directory, &data.records, max_asset_bytes)?;
        // Each checkpoint and inherited baseline is an independent model window, not a continuation of visible call pairing.
        for entry in &data.records {
            if matches!(
                entry.record,
                FlowRecord::Input(_) | FlowRecord::Checkpoint(_) | FlowRecord::Continuation { .. }
            ) {
                assets = assets.retain_context_logs(
                    flow,
                    source_directory,
                    record_messages(&entry.record),
                    max_asset_bytes,
                )?;
            }
        }
        if let Some(baseline) = &data.baseline {
            assets = assets.retain_context_logs(
                flow,
                source_directory,
                &baseline.messages,
                max_asset_bytes,
            )?;
        }
        assets = assets.retain_context_logs(
            flow,
            source_directory,
            &data.model_context,
            max_asset_bytes,
        )?;
        for entry in &mut data.records {
            let messages = match &mut entry.record {
                FlowRecord::Input(input) => input.messages.as_mut_slice(),
                FlowRecord::Publication(publication) => {
                    std::slice::from_mut(&mut publication.message)
                }
                FlowRecord::Checkpoint(checkpoint) => checkpoint.messages.as_mut_slice(),
                FlowRecord::Continuation { messages, .. } => messages.as_mut_slice(),
                FlowRecord::Execution(_) | FlowRecord::Completion(_) | FlowRecord::Legacy(_) => {
                    continue;
                }
            };
            assets.resolve_images(messages)?;
        }
        if let Some(baseline) = &mut data.baseline {
            assets.resolve_images(&mut baseline.messages)?;
        }
        assets.resolve_images(&mut data.model_context)?;
        Ok(Self {
            content: Some((data, assets)),
        })
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

#[cfg(test)]
mod tests;
