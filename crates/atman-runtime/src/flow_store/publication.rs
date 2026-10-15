//! Durable message publication, independent of its transport role or final-answer origin.

use std::collections::HashMap;
use std::io;

use atman_rt::{FlowInstance, InstanceActivity, InstanceState, TurnLease, TurnPhase};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::inbox::InputContent;
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

#[derive(Default)]
pub(super) struct Publications {
    records: Vec<PublishedMessage>,
    indices: HashMap<Uuid, usize>,
}

impl Publications {
    fn get(&self, id: Uuid) -> Option<&PublishedMessage> {
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

    pub fn apply(&mut self, publication: MessagePublication, receipt: CommitReceipt) {
        self.indices.insert(publication.id, self.records.len());
        self.records.push(PublishedMessage {
            publication,
            receipt,
        });
    }
}

impl FlowStore {
    pub fn published_messages(&self) -> &[PublishedMessage] {
        &self.publications.records
    }

    /// Identical retries return the original receipt, even after the turn has ended.
    pub fn publish_message(
        &mut self,
        publication: MessagePublication,
    ) -> io::Result<PublishedMessage> {
        if let Some(record) = self.publications.get(publication.id) {
            return if record.publication == publication {
                Ok(record.clone())
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
        self.publications
            .apply(publication.clone(), receipt.clone());
        self.cursor = receipt.clone();
        self.refresh_projection();
        Ok(PublishedMessage {
            publication,
            receipt,
        })
    }

    /// Input and output share journal order; restoring context never republishes effects or notifications.
    pub fn context_messages(&self) -> Vec<Message> {
        let inputs = self
            .inbox
            .inputs()
            .iter()
            .filter_map(|record| match &record.input.content {
                InputContent::Context { messages }
                | InputContent::InvocationInput { messages, .. } => {
                    Some((record.receipt.sequence, messages.as_slice()))
                }
                InputContent::WaitingContinuation { .. } => None,
            });
        let publications = self
            .publications
            .records
            .iter()
            .filter(|record| record.publication.context_run.is_none())
            .map(|record| {
                (
                    record.receipt.sequence,
                    std::slice::from_ref(&record.publication.message),
                )
            });
        let mut batches: Vec<_> = inputs.chain(publications).collect();
        batches.sort_by_key(|(sequence, _)| *sequence);
        batches
            .into_iter()
            .flat_map(|(_, messages)| messages.iter().cloned())
            .collect()
    }
}
