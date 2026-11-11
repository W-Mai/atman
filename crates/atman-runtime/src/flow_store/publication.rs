//! Durable message publication, independent of its transport role or final-answer origin.

use std::collections::HashMap;
use std::io;

use atman_rt::{FlowInstance, InstanceActivity, InstanceState, TurnLease, TurnPhase};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::conversation::{ConversationPlan, ConversationReadKind};
use super::inbox::Inbox;
use super::{FlowFact, FlowStore, invalid};
use crate::event::{AtmanUuid, FlowRunId};
use crate::flow_store::position::FlowPosition;
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
    pub receipt: FlowPosition,
}

/// Publication identity and journal position without retained message content.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PublicationRef {
    pub id: Uuid,
    pub lease: TurnLease<AtmanUuid>,
    pub run: FlowRunId,
    pub context_run: Option<FlowRunId>,
    pub origin: crate::message::MessageOrigin,
    pub receipt: FlowPosition,
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
    preview: Option<String>,
}

impl Publications {
    pub(super) fn records(&self) -> &[PublicationRef] {
        &self.records
    }

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

    pub fn apply(&mut self, publication: &MessagePublication, receipt: FlowPosition) {
        if publication.context_run.is_none()
            && let Some(preview) =
                super::preview_messages(std::slice::from_ref(&publication.message))
        {
            self.preview = Some(preview);
        }
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
    pub(crate) fn published_preview(&self) -> Option<&str> {
        self.publications.preview.as_deref()
    }

    /// Compact latest assistant output for detached activity surfaces.
    pub fn preview(&mut self) -> io::Result<Option<String>> {
        Ok(crate::flow_store::preview_messages(
            &self.context_messages()?,
        ))
    }

    pub fn published_messages(&self) -> &[PublicationRef] {
        &self.publications.records
    }

    /// Read a complete publication from its verified journal position.
    pub fn published_message(&mut self, id: Uuid) -> io::Result<Option<PublishedMessage>> {
        self.read_publication(id, true)
    }

    fn read_publication(&mut self, id: Uuid, owned: bool) -> io::Result<Option<PublishedMessage>> {
        let Some(saved) = self.publications.get(id) else {
            return Ok(None);
        };
        let batch = if owned {
            self.journal.read_owned(&saved.receipt)?
        } else {
            self.journal.read_unresolved(&saved.receipt)?
        };
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
        if let Some(record) = self.read_publication(publication.id, false)? {
            return if record.publication == publication {
                self.published_message(publication.id)?
                    .ok_or_else(|| invalid("publication disappeared"))
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
        if publication.context_run.is_none() {
            self.names.observe_message(&publication.message);
        }
        self.cursor = receipt.clone();
        self.refresh_projection();
        self.published_message(publication.id)?
            .ok_or_else(|| invalid("publication disappeared"))
    }

    /// Input and output share journal order; restoring context never republishes effects or notifications.
    pub fn context_messages(&mut self) -> io::Result<Vec<Message>> {
        self.conversation(ConversationReadKind::ModelContext)
    }

    /// Complete shared conversation, unaffected by model-window checkpoints.
    pub fn history_messages(&mut self) -> io::Result<Vec<Message>> {
        self.conversation(ConversationReadKind::History)
    }

    fn conversation(&mut self, kind: ConversationReadKind) -> io::Result<Vec<Message>> {
        self.conversation_at(kind, self.inbox.head().branch, self.cursor.sequence())
    }

    pub(super) fn conversation_at(
        &mut self,
        kind: ConversationReadKind,
        branch: super::source_capture::ConversationBranchId,
        through: u64,
    ) -> io::Result<Vec<Message>> {
        let spans = self
            .branches
            .as_ref()
            .map(|projection| projection.history.ancestry_through(branch, through))
            .transpose()?;
        let plan = ConversationPlan::select(
            kind,
            self.activation.as_ref().map(|(_, receipt)| receipt),
            self.context_checkpoints()
                .iter()
                .map(|record| &record.receipt),
            self.inbox
                .inputs()
                .iter()
                .filter(|record| record.message_count > 0)
                .map(|record| &record.receipt),
            self.publications
                .records
                .iter()
                .filter(|record| record.context_run.is_none())
                .map(|record| &record.receipt),
            |receipt| {
                receipt.sequence() <= through
                    && spans.as_ref().is_none_or(|spans| {
                        let sequence = receipt.sequence();
                        spans
                            .get(spans.partition_point(|span| span.last_sequence < sequence))
                            .is_some_and(|span| span.first_sequence <= sequence)
                    })
            },
        );
        plan.read(|receipt| {
            if let Some(record) = self.checkpoints.latest()
                && &record.receipt == receipt
            {
                return Ok(vec![FlowFact::ContextCheckpoint(Box::new(
                    record.checkpoint.clone(),
                ))]);
            }
            Ok(self.journal.read_owned(receipt)?.facts)
        })
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
