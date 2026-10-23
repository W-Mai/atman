//! Durable message publication, independent of its transport role or final-answer origin.

use std::collections::HashMap;
use std::io;

use atman_rt::{FlowInstance, InstanceActivity, InstanceState, TurnLease, TurnPhase};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::inbox::{Inbox, InputContent};
use super::{FlowFact, FlowStore, invalid};
use crate::event::{AtmanUuid, FlowRunId};
use crate::event_log::journal::CommitReceipt;
use crate::message::Message;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MessagePublication {
    pub id: Uuid,
    pub lease: TurnLease<AtmanUuid>,
    /// The producing invocation, including inline calls within the owning turn.
    pub run: FlowRunId,
    /// Isolated invocation context; absent for the instance's shared conversation.
    pub context_run: Option<FlowRunId>,
    pub message: Message,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PublishedMessage {
    pub publication: MessagePublication,
    pub receipt: CommitReceipt,
}

/// Publication identity and journal position without retained message content.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PublicationRef {
    pub id: Uuid,
    pub lease: TurnLease<AtmanUuid>,
    pub run: FlowRunId,
    pub context_run: Option<FlowRunId>,
    pub origin: crate::message::MessageOrigin,
    pub receipt: CommitReceipt,
}

impl PublicationRef {
    pub fn is_shared_final(&self) -> bool {
        self.context_run.is_none() && self.origin == crate::message::MessageOrigin::FinalAnswer
    }
}

#[derive(Default)]
pub(super) struct Publications {
    records: Vec<PublicationRef>,
    indices: HashMap<Uuid, usize>,
}

impl Publications {
    fn get(&self, id: Uuid) -> Option<&PublicationRef> {
        self.indices.get(&id).map(|&index| &self.records[index])
    }

    pub fn validate(
        &self,
        state: &FlowInstance<AtmanUuid>,
        publication: &MessagePublication,
    ) -> io::Result<()> {
        let InstanceState::Open(InstanceActivity::Active(active)) = state.state() else {
            return Err(invalid("message publication requires an active turn"));
        };
        if active.lease != publication.lease
            || active.phase == TurnPhase::Cancelling
            || publication.message.turn_id != publication.lease.turn
            || publication.id.is_nil()
            || publication
                .context_run
                .as_ref()
                .is_some_and(|run| run != &publication.run)
            || self.indices.contains_key(&publication.id)
        {
            return Err(invalid("stale or invalid message publication"));
        }
        // Pausing automation does not prevent the current turn from delivering its result.
        Ok(())
    }

    pub fn apply(&mut self, publication: &MessagePublication, receipt: CommitReceipt) {
        self.indices.insert(publication.id, self.records.len());
        self.records.push(PublicationRef {
            id: publication.id,
            lease: publication.lease.clone(),
            run: publication.run.clone(),
            context_run: publication.context_run.clone(),
            origin: publication.message.origin,
            receipt,
        });
    }
}

impl FlowStore {
    pub fn published_messages(&self) -> &[PublicationRef] {
        &self.publications.records
    }

    /// Read a complete publication from its verified journal position.
    pub fn published_message(&mut self, id: Uuid) -> io::Result<Option<PublishedMessage>> {
        let Some(saved) = self.publications.get(id) else {
            return Ok(None);
        };
        let batch = self.journal.inner.read_batch::<FlowFact>(&saved.receipt)?;
        let publication = batch
            .facts
            .into_iter()
            .find_map(|fact| match fact {
                FlowFact::MessagePublished(publication) if publication.id == id => {
                    Some(*publication)
                }
                _ => None,
            })
            .ok_or_else(|| invalid("message publication is absent from its recorded batch"))?;
        Ok(Some(PublishedMessage {
            publication,
            receipt: saved.receipt.clone(),
        }))
    }

    /// Identical retries return the original receipt, even after the turn has ended.
    pub fn publish_message(
        &mut self,
        publication: MessagePublication,
    ) -> io::Result<PublishedMessage> {
        if let Some(record) = self.published_message(publication.id)? {
            return if record.publication == publication {
                Ok(record)
            } else {
                Err(invalid(
                    "message publication identity conflicts with a committed message",
                ))
            };
        }
        self.publications.validate(&self.state, &publication)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::MessagePublished(Box::new(publication.clone()))])?;
        self.publications.apply(&publication, receipt.clone());
        self.cursor = receipt.clone();
        self.refresh_projection();
        Ok(PublishedMessage {
            publication,
            receipt,
        })
    }

    /// Input and output share journal order; restoring context never republishes effects or notifications.
    pub fn context_messages(&mut self) -> io::Result<Vec<Message>> {
        let mut messages = self.conversation(true)?;
        trim_context(&mut messages);
        Ok(messages)
    }

    /// Complete shared conversation, unaffected by model-window checkpoints.
    pub fn history_messages(&mut self) -> io::Result<Vec<Message>> {
        self.conversation(false)
    }

    fn conversation(&mut self, checkpointed: bool) -> io::Result<Vec<Message>> {
        let checkpoint = self.checkpoints.latest().filter(|_| checkpointed);
        let after = checkpoint.map_or(0, |record| record.receipt.sequence);
        let mut messages = match checkpoint {
            Some(record) => record.checkpoint.messages.clone(),
            None => self
                .activation
                .as_ref()
                .map_or_else(Vec::new, |(record, _)| {
                    record.input.inherited_context.clone()
                }),
        };
        let inputs: Vec<_> = self
            .inbox
            .inputs()
            .iter()
            .filter(|record| record.receipt.sequence > after && record.message_count > 0)
            .map(|record| record.receipt.clone())
            .collect();
        let mut batches = Vec::new();
        for receipt in inputs {
            let record = self
                .committed_input(&receipt)?
                .ok_or_else(|| invalid("input index changed during conversation read"))?;
            let messages = match record.input.content {
                InputContent::Context { messages }
                | InputContent::InvocationInput { messages, .. } => messages,
                InputContent::WaitingContinuation { .. } => {
                    unreachable!("reply input has no messages")
                }
            };
            batches.push((receipt.sequence, messages));
        }
        let ids: Vec<_> = self
            .publications
            .records
            .iter()
            .filter(|record| record.context_run.is_none() && record.receipt.sequence > after)
            .map(|record| record.id)
            .collect();
        for id in ids {
            let record = self
                .published_message(id)?
                .ok_or_else(|| invalid("publication index changed during conversation read"))?;
            batches.push((record.receipt.sequence, vec![record.publication.message]));
        }
        batches.sort_by_key(|(sequence, _)| *sequence);
        messages.extend(batches.into_iter().flat_map(|(_, messages)| messages));
        Ok(messages)
    }
}

pub(super) fn trim_context(messages: &mut Vec<Message>) {
    let start = messages
        .iter()
        .rposition(crate::compaction::is_compaction_summary)
        .unwrap_or(0);
    messages.drain(..start);
}

pub(super) fn extend_context(window: &mut Vec<Message>, messages: &[Message]) {
    let start = messages
        .iter()
        .rposition(crate::compaction::is_compaction_summary);
    if start.is_some() {
        window.clear();
    }
    window.extend(messages[start.unwrap_or(0)..].iter().cloned());
}

pub(super) fn message_count(
    inbox: &Inbox,
    publications: &Publications,
    activation: Option<&super::launch::Activation>,
) -> usize {
    activation.map_or(0, |record| record.input.inherited_context.len())
        + inbox
            .inputs()
            .iter()
            .map(|record| record.message_count)
            .sum::<usize>()
        + publications
            .records
            .iter()
            .filter(|record| record.context_run.is_none())
            .count()
}
