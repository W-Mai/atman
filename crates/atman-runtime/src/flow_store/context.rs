//! Durable model-window replacement without rewriting conversation history.

use std::{collections::HashMap, io, ops::Range};

use atman_rt::{FlowInstance, InstanceState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FlowFact, FlowStore, invalid};
use crate::{event::AtmanUuid, event_log::journal::CommitReceipt, message::Message};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextCheckpoint {
    pub id: Uuid,
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommittedContextCheckpoint {
    pub checkpoint: ContextCheckpoint,
    pub receipt: CommitReceipt,
}

/// A durable checkpoint location without a retained copy of its model window.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextCheckpointRef {
    pub id: Uuid,
    pub receipt: CommitReceipt,
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
    pub fn latest(&self) -> Option<&CommittedContextCheckpoint> {
        self.latest.as_ref()
    }

    pub fn validate(
        &self,
        state: &FlowInstance<AtmanUuid>,
        checkpoint: &ContextCheckpoint,
        window: &[Message],
    ) -> io::Result<()> {
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
        Ok(())
    }

    pub fn apply(&mut self, checkpoint: ContextCheckpoint, receipt: CommitReceipt) {
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
            self.journal.inner.read_batch::<FlowFact>(&saved.receipt)?
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
        let window = self.context_messages()?;
        self.checkpoints
            .validate(&self.state, &checkpoint, &window)?;
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
