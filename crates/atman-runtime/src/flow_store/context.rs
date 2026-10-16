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

pub fn context_digest(messages: &[Message]) -> io::Result<String> {
    let bytes = serde_json::to_vec(messages).map_err(invalid)?;
    Ok(format!("blake3:{}", blake3::hash(&bytes).to_hex()))
}

#[derive(Default)]
pub(super) struct Checkpoints {
    records: Vec<CommittedContextCheckpoint>,
    indices: HashMap<Uuid, usize>,
}

impl Checkpoints {
    pub fn latest(&self) -> Option<&CommittedContextCheckpoint> {
        self.records.last()
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
        self.records.push(CommittedContextCheckpoint {
            checkpoint,
            receipt,
        });
    }
}

impl FlowStore {
    pub fn context_checkpoints(&self) -> &[CommittedContextCheckpoint] {
        &self.checkpoints.records
    }

    /// Commit only against the model window that the host actually summarized.
    pub fn checkpoint_context(
        &mut self,
        checkpoint: ContextCheckpoint,
    ) -> io::Result<CommittedContextCheckpoint> {
        if let Some(&index) = self.checkpoints.indices.get(&checkpoint.id) {
            let saved = &self.checkpoints.records[index];
            return if saved.checkpoint == checkpoint {
                Ok(saved.clone())
            } else {
                Err(invalid(
                    "context checkpoint identity conflicts with recorded data",
                ))
            };
        }
        self.checkpoints
            .validate(&self.state, &checkpoint, &self.context_messages())?;
        let receipt = self
            .journal
            .commit(&[FlowFact::ContextCheckpoint(Box::new(checkpoint.clone()))])?;
        self.checkpoints.apply(checkpoint.clone(), receipt.clone());
        self.cursor = receipt.clone();
        Ok(CommittedContextCheckpoint {
            checkpoint,
            receipt,
        })
    }
}
