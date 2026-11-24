//! Browse the immutable conversation owned by a Fork's destination.

use std::path::{Path, PathBuf};

use super::*;
use crate::{
    flow_store::{
        images::fork_image,
        source_capture::{ForkLogAsset, InspectedForkImport},
    },
    projection::flow::{FlowRecord, ImportedHistory, ImportedHistoryRecord},
    tools::bash_bg::BgHandle,
};

const MAX_PAGE_LOGS: usize = 64;

pub struct InheritedLogSource {
    pub receipt: CommitReceipt,
    pub record: FlowRecordId,
    pub original: FlowRef,
    pub handle: String,
    pub bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InheritedLogCursor {
    pub activation: CommitReceipt,
    pub publication_digest: blake3::Hash,
    pub offset: usize,
}

pub struct InheritedLogPage {
    pub entries: Vec<InheritedLogSource>,
    pub publication_digest: blake3::Hash,
    pub next: Option<InheritedLogCursor>,
}

pub(super) struct ForkHistory {
    history: ImportedHistory,
    sources: Vec<FlowRef>,
    logs: Vec<ForkLogAsset>,
    digest: blake3::Hash,
    encoded_bytes: usize,
    file: PathBuf,
    stamp: ReadStamp,
}

impl ForkHistory {
    pub(super) fn new(value: InspectedForkImport) -> Self {
        Self {
            sources: value.sources().cloned().collect(),
            history: value.publication.history,
            logs: value.publication.logs,
            digest: value.digest,
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

    pub(super) fn log_index(
        &self,
        after: Option<&InheritedLogCursor>,
    ) -> io::Result<Option<InheritedLogPage>> {
        if !self.is_current() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Fork publication changed during the log directory read",
            ));
        }
        let start = match after {
            Some(cursor)
                if cursor.activation == *self.receipt()
                    && cursor.publication_digest == self.digest
                    && cursor.offset < self.logs.len()
                    && cursor.offset > 0
                    && cursor.offset % MAX_PAGE_LOGS == 0 =>
            {
                cursor.offset
            }
            Some(_) => {
                return Err(invalid(
                    "Inherited log continuation is outside the published directory",
                ));
            }
            None => 0,
        };
        if self.logs.is_empty() {
            return Ok(None);
        }
        let end = start.saturating_add(MAX_PAGE_LOGS).min(self.logs.len());
        let entries = self
            .logs
            .iter()
            .enumerate()
            .skip(start)
            .take(end - start)
            .map(|(index, log)| {
                Ok(InheritedLogSource {
                    receipt: self.receipt().clone(),
                    record: FlowRecordId {
                        commit: self.receipt().sequence,
                        fact: ConversationItemId::IMPORTED_FACT,
                        item: index,
                    },
                    original: log
                        .origin(&self.sources)
                        .ok_or_else(missing_record)?
                        .clone(),
                    handle: log.handle.clone(),
                    bytes: log.bytes,
                })
            })
            .collect::<io::Result<_>>()?;
        let next = (end < self.logs.len()).then(|| InheritedLogCursor {
            activation: self.receipt().clone(),
            publication_digest: self.digest,
            offset: end,
        });
        Ok(Some(InheritedLogPage {
            entries,
            publication_digest: self.digest,
            next,
        }))
    }

    pub(super) fn read_log(
        &self,
        directory: &Path,
        proof: &CommitReceipt,
        index: usize,
    ) -> io::Result<ConversationLogFile> {
        if proof != self.receipt() || !self.is_current() {
            return Err(missing_record());
        }
        let log = self.logs.get(index).ok_or_else(missing_record)?;
        let origin = log.origin(&self.sources).ok_or_else(missing_record)?;
        let filename = BgHandle::managed_log_name(&log.handle, &origin.session_id.to_string())
            .ok_or_else(missing_record)?;
        let digest = blake3::Hash::from_hex(&log.digest).map_err(invalid)?;
        Ok(ConversationLogFile {
            path: directory.join(filename),
            checkpoint: Some(work_log::ConversationLogCheckpoint {
                bytes: log.bytes,
                digest,
            }),
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_log_pages_keep_the_original_snapshot_and_contiguous_ordinals() {
        let publication = tempfile::NamedTempFile::new().unwrap();
        let file = publication.path().to_owned();
        std::fs::write(&file, b"published").unwrap();
        let original = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: crate::flow_store::FlowInstanceId::now(),
        };
        let fork = ForkHistory {
            history: ImportedHistory {
                receipt: CommitReceipt {
                    journal_id: Uuid::now_v7(),
                    sequence: 3,
                    digest: "accepted activation".into(),
                },
                records: Vec::new(),
            },
            logs: (0..131)
                .map(|index| ForkLogAsset {
                    handle: format!("bg_{}_{index}", original.session_id),
                    bytes: index,
                    digest: blake3::hash(b"retained bytes").to_hex().to_string(),
                })
                .collect(),
            sources: vec![original],
            digest: blake3::hash(b"published"),
            encoded_bytes: 9,
            stamp: ReadStamp::open(&file).unwrap(),
            file,
        };
        let first = fork.log_index(None).unwrap().unwrap();
        let second = fork.log_index(first.next.as_ref()).unwrap().unwrap();
        let last = fork.log_index(second.next.as_ref()).unwrap().unwrap();
        assert!(last.next.is_none());
        assert_eq!(last.publication_digest, fork.digest);
        assert_eq!(first.entries.len(), MAX_PAGE_LOGS);
        assert_eq!(second.entries.len(), MAX_PAGE_LOGS);
        assert!(
            first
                .entries
                .into_iter()
                .chain(second.entries)
                .chain(last.entries)
                .enumerate()
                .all(
                    |(index, entry)| entry.record.item == index && entry.receipt == *fork.receipt()
                )
        );
        let cursor = first.next.unwrap();
        for stale in [
            InheritedLogCursor {
                offset: 63,
                ..cursor.clone()
            },
            InheritedLogCursor {
                offset: 192,
                ..cursor.clone()
            },
            InheritedLogCursor {
                publication_digest: blake3::hash(b"other"),
                ..cursor.clone()
            },
            InheritedLogCursor {
                activation: CommitReceipt {
                    sequence: 4,
                    ..cursor.activation.clone()
                },
                ..cursor.clone()
            },
        ] {
            assert!(fork.log_index(Some(&stale)).is_err());
        }
        std::fs::write(publication.path(), b"replaced publication").unwrap();
        assert!(fork.log_index(Some(&cursor)).is_err());
    }
}
