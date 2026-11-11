//! Durable model-window replacement without rewriting conversation history.

use std::{collections::HashMap, io, ops::Range};

use atman_rt::{FlowInstance, InstanceState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FlowFact, FlowStore, invalid};
use crate::{event::AtmanUuid, flow_store::position::FlowPosition, message::Message};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextCheckpoint {
    pub id: Uuid,
    /// Conversation selection captured before preparing this replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_head: Option<super::branches::HistoryHead>,
    #[serde(
        default,
        skip_serializing_if = "ContextCheckpointPurpose::is_compaction"
    )]
    pub purpose: ContextCheckpointPurpose,
    /// Digest of the exact model window used to calculate this replacement.
    pub expected: String,
    pub messages: Vec<Message>,
    pub summary: String,
    /// Original message positions; end is exclusive. Empty for an in-place rewrite.
    pub range: Range<usize>,
    pub count: usize,
    pub before_tokens: u64,
    pub after_tokens: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextCheckpointPurpose {
    #[default]
    Compaction,
    Append {
        request_digest: String,
    },
}

impl ContextCheckpointPurpose {
    pub fn is_compaction(&self) -> bool {
        matches!(self, Self::Compaction)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommittedContextCheckpoint {
    pub checkpoint: ContextCheckpoint,
    pub receipt: FlowPosition,
}

/// A durable checkpoint location without a retained copy of its model window.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextCheckpointRef {
    pub id: Uuid,
    pub receipt: FlowPosition,
}

pub fn context_digest(messages: &[Message]) -> io::Result<String> {
    let bytes = serde_json::to_vec(messages).map_err(invalid)?;
    Ok(format!("blake3:{}", blake3::hash(&bytes).to_hex()))
}

#[derive(Default)]
pub(super) struct Checkpoints {
    records: Vec<ContextCheckpointRef>,
    indices: HashMap<Uuid, usize>,
    latest: Option<CommittedContextCheckpoint>,
}

impl Checkpoints {
    pub(super) fn select_latest(&mut self, checkpoint: Option<CommittedContextCheckpoint>) {
        self.latest = checkpoint;
    }
    pub(super) fn records(&self) -> &[ContextCheckpointRef] {
        &self.records
    }

    pub fn latest(&self) -> Option<&CommittedContextCheckpoint> {
        self.latest.as_ref()
    }

    pub fn validate(
        &self,
        state: &FlowInstance<AtmanUuid>,
        head: &super::branches::HistoryHead,
        checkpoint: &ContextCheckpoint,
        window: &[Message],
    ) -> io::Result<()> {
        head.validate_write(checkpoint.expected_head.as_ref())?;
        if !matches!(state.state(), InstanceState::Open(_))
            || checkpoint.id.is_nil()
            || self.indices.contains_key(&checkpoint.id)
            || checkpoint.range.start > checkpoint.range.end
            || checkpoint.range.end > window.len()
            || checkpoint.count > window.len()
            || checkpoint.expected != context_digest(window)?
        {
            return Err(invalid("stale or invalid context checkpoint"));
        }
        if let ContextCheckpointPurpose::Append { request_digest } = &checkpoint.purpose {
            let appended = checkpoint.messages.get(window.len()..);
            if request_digest.is_empty()
                || checkpoint.count != 0
                || checkpoint.range != (window.len()..window.len())
                || !checkpoint.summary.is_empty()
                || !checkpoint.messages.starts_with(window)
                || appended.is_none_or(|messages| {
                    messages.iter().any(|message| {
                        message.role != crate::message::MessageRole::System
                            || message.origin != crate::message::MessageOrigin::Internal
                            || message.parts.is_empty()
                            || message.parts.iter().any(|part| {
                                !matches!(part, crate::message::MessagePart::ContextRecord(_))
                            })
                    })
                })
            {
                return Err(invalid(
                    "context append must preserve the existing window and contain only internal records",
                ));
            }
        }
        Ok(())
    }

    pub fn apply(&mut self, checkpoint: ContextCheckpoint, receipt: FlowPosition) {
        self.indices.insert(checkpoint.id, self.records.len());
        self.records.push(ContextCheckpointRef {
            id: checkpoint.id,
            receipt: receipt.clone(),
        });
        self.latest = Some(CommittedContextCheckpoint {
            checkpoint,
            receipt,
        });
    }
}

impl FlowStore {
    pub fn context_checkpoints(&self) -> &[ContextCheckpointRef] {
        &self.checkpoints.records
    }

    pub fn latest_context_checkpoint(&self) -> Option<&CommittedContextCheckpoint> {
        self.checkpoints.latest()
    }

    /// Read one complete checkpoint from its verified journal position.
    pub fn context_checkpoint(
        &mut self,
        id: Uuid,
    ) -> io::Result<Option<CommittedContextCheckpoint>> {
        self.read_checkpoint(id, true)
    }

    fn read_checkpoint(
        &mut self,
        id: Uuid,
        owned: bool,
    ) -> io::Result<Option<CommittedContextCheckpoint>> {
        let Some(&index) = self.checkpoints.indices.get(&id) else {
            return Ok(None);
        };
        let saved = &self.checkpoints.records[index];
        let batch = if owned {
            self.journal.read_owned(&saved.receipt)?
        } else {
            self.journal.read_unresolved(&saved.receipt)?
        };
        let checkpoint = batch
            .facts
            .into_iter()
            .find_map(|fact| match fact {
                FlowFact::ContextCheckpoint(checkpoint) if checkpoint.id == id => Some(*checkpoint),
                _ => None,
            })
            .ok_or_else(|| invalid("context checkpoint is absent from its recorded batch"))?;
        Ok(Some(CommittedContextCheckpoint {
            checkpoint,
            receipt: saved.receipt.clone(),
        }))
    }

    /// Commit only against the model window that the host actually summarized.
    pub fn checkpoint_context(
        &mut self,
        checkpoint: ContextCheckpoint,
    ) -> io::Result<CommittedContextCheckpoint> {
        if let Some(saved) = self.read_checkpoint(checkpoint.id, false)? {
            return if saved.checkpoint == checkpoint {
                self.context_checkpoint(checkpoint.id)?
                    .ok_or_else(|| invalid("checkpoint disappeared"))
            } else {
                Err(invalid(
                    "context checkpoint identity conflicts with recorded data",
                ))
            };
        }
        self.inbox
            .head()
            .validate_write(checkpoint.expected_head.as_ref())?;
        let window = self.context_messages()?;
        self.checkpoints
            .validate(&self.state, self.inbox.head(), &checkpoint, &window)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::ContextCheckpoint(Box::new(checkpoint.clone()))])?;
        self.checkpoints.apply(checkpoint.clone(), receipt.clone());
        self.cursor = receipt.clone();
        let owned = self
            .context_checkpoint(checkpoint.id)?
            .ok_or_else(|| invalid("checkpoint disappeared"))?;
        self.checkpoints.latest = Some(owned.clone());
        Ok(owned)
    }
}
