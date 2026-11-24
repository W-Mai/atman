//! Browse the immutable conversation owned by a Fork's destination.

use std::path::{Path, PathBuf};

use super::*;
use crate::{
    flow_store::{images::fork_image, source_capture::InspectedForkImport},
    projection::flow::{FlowRecord, ImportedHistory, ImportedHistoryRecord},
};

pub(super) struct ForkHistory {
    history: ImportedHistory,
    encoded_bytes: usize,
    file: PathBuf,
    stamp: ReadStamp,
}

impl ForkHistory {
    pub(super) fn new(value: InspectedForkImport) -> Self {
        Self {
            history: value.publication.history,
            encoded_bytes: value.encoded_bytes,
            file: value.file,
            stamp: value.stamp,
        }
    }

    pub(super) fn receipt(&self) -> &CommitReceipt {
        &self.history.receipt
    }

    pub(super) fn is_current(&self) -> bool {
        ReadStamp::open(&self.file).is_ok_and(|current| current == self.stamp)
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.encoded_bytes.saturating_add(
            self.history
                .records
                .capacity()
                .saturating_mul(std::mem::size_of::<ImportedHistoryRecord>()),
        )
    }

    fn record(&self, item: ConversationItemId) -> io::Result<&ImportedHistoryRecord> {
        if !item.is_imported() || item.record.commit != self.receipt().sequence {
            return Err(missing_record());
        }
        self.history
            .record(item.record.item)
            .ok_or_else(missing_record)
    }

    pub(super) fn message(&self, item: ConversationItemId) -> io::Result<&Message> {
        self.record(item)?
            .record
            .shared_messages()
            .get(item.message)
            .filter(|message| message.role != MessageRole::System)
            .ok_or_else(missing_record)
    }

    pub(super) fn artifact(
        &self,
        item: ConversationItemId,
        part: u32,
        id: &str,
    ) -> io::Result<ToolResultArtifact> {
        let message = self.message(item)?;
        let FlowRecord::Publication(publication) = &self.record(item)?.record else {
            return Err(missing_record());
        };
        publication
            .tool_artifacts
            .iter()
            .find(|artifact| {
                artifact.part == part
                    && artifact.output.id == id
                    && artifact.matches_message(message)
            })
            .cloned()
            .ok_or_else(missing_record)
    }

    pub(super) fn image(
        &self,
        directory: &Path,
        item: ConversationItemId,
        part: u32,
    ) -> io::Result<ImageSource> {
        let Some(MessagePart::Image { source }) = self.message(item)?.parts.get(part as usize)
        else {
            return Err(missing_record());
        };
        fork_image(directory, source)
    }

    pub(super) fn add_earlier(
        &self,
        page: &mut ConversationPagePreparation,
        inline_limit: usize,
    ) -> io::Result<bool> {
        if !self.is_current() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Fork publication changed during the conversation read",
            ));
        }
        let before = page.request.before.filter(|before| before.is_imported());
        let end = before.map_or(self.history.records.len(), |before| before.record.item + 1);
        for record in self.history.records[..end].iter().rev() {
            cancelled(&page.control)?;
            let messages = record.record.shared_messages();
            let end = before
                .filter(|before| before.record.item == record.position.ordinal)
                .map_or(messages.len(), |before| before.message);
            for (index, message) in messages[..end].iter().enumerate().rev() {
                if message.role == MessageRole::System {
                    continue;
                }
                if page.selected.len() >= page.request.limit.get() {
                    return Ok(true);
                }
                let id = ConversationItemId {
                    record: FlowRecordId {
                        commit: self.receipt().sequence,
                        fact: ConversationItemId::IMPORTED_FACT,
                        item: record.position.ordinal,
                    },
                    message: index,
                };
                let body = bounded_message(message, inline_limit, &mut page.inline_bytes)?;
                let tool_artifacts = match &record.record {
                    FlowRecord::Publication(publication) => publication
                        .tool_artifacts
                        .iter()
                        .filter(|artifact| artifact.matches_message(message))
                        .cloned()
                        .collect(),
                    _ => Vec::new(),
                };
                page.selected.push(ConversationItem {
                    id,
                    body,
                    work: None,
                    user_turn: None,
                    completed_turn: None,
                    imported_from: Some(record.source.clone()),
                    tool_artifacts,
                });
            }
        }
        Ok(false)
    }
}
