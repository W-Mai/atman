//! Typed view updates derived from committed facts. Reading does not execute effects or consume input.

use std::{io, num::NonZeroUsize, path::PathBuf};

use atman_rt::TurnLease;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    FlowContent, FlowFact, FlowRef, FlowStore, FlowTransition, completion, context, goal, history,
    inbox, outbox, planning, process_watch, publication, subscription, trace, waiter,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FlowContentSummary {
    Entry { name: String, path: PathBuf },
    History(history::LegacyArchive),
}

impl FlowContent {
    pub fn summary(&self) -> FlowContentSummary {
        match self {
            Self::Entry(entry) => FlowContentSummary::Entry {
                name: entry.name.clone(),
                path: entry.path.clone(),
            },
            Self::History(archive) => FlowContentSummary::History(archive.clone()),
        }
    }
}
use crate::{
    event::AtmanUuid,
    event_log::journal::{CommitReceipt, CommittedBatch, JournalPage},
    message::Message,
};

impl super::SessionInspection {
    /// Inspect recorded updates without acquiring execution ownership, repairing tails, or recovering a turn. Publish the collected projection only after this read succeeds.
    pub fn inspect_flow_updates(
        &self,
        id: &super::FlowInstanceId,
        mut apply: impl FnMut(CommitReceipt, Vec<FlowUpdate>) -> io::Result<()>,
    ) -> io::Result<crate::event_log::journal::JournalInspection> {
        if !self.projection.flows.iter().any(|flow| &flow.id == id) {
            return Err(super::invalid("flow is not registered in this session"));
        }
        crate::event_log::journal::Journal::inspect(
            &self
                .dir
                .join("flows")
                .join(id.to_string())
                .join("events.jsonl"),
            id.0.0,
            |receipt, facts: Vec<FlowFact>| {
                apply(receipt, facts.into_iter().map(FlowUpdate::from).collect())
            },
        )
    }
}

/// Read-only updates for one instance, in original commit order. Message origins, isolated contexts, and typed completion remain distinct; creation metadata omits the retained source bundle and activation omits execution authority.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum FlowUpdate {
    CommandAccepted {
        id: Uuid,
        request_id: Uuid,
        recipient: FlowRef,
        sender: inbox::SenderIdentity,
        accepted_at: chrono::DateTime<chrono::Utc>,
        name: String,
        user: inbox::UserContent,
    },
    Created {
        flow: FlowRef,
        content: FlowContentSummary,
        created_by: Option<FlowRef>,
    },
    Activated {
        envelope: Box<inbox::AcceptedEnvelope>,
        inherited_context: Vec<Message>,
    },
    WorkspaceRequested(Box<super::workspace::WorkspaceRequest>),
    Transition(FlowTransition),
    TurnCompleted(Box<completion::Completion>),
    EnvelopeAccepted(Box<inbox::AcceptedEnvelope>),
    InputCommitted(Box<inbox::CommittedInput>),
    MessagePublished(Box<publication::MessagePublication>),
    ContextCheckpoint(Box<context::ContextCheckpoint>),
    GoalUpdated(Box<goal::GoalUpdate>),
    PlanningUpdated(Box<planning::PlanningUpdate>),
    ExecutionEvents(Vec<trace::ExecutionEvent>),
    LegacyRecords(Vec<history::LegacyHistoryRecord>),
    ReplyWaiterRegistered(Box<waiter::ReplyWaiter>),
    ReplyWaiterCancelled {
        lease: TurnLease<AtmanUuid>,
        correlation: Uuid,
    },
    SendQueued(Box<outbox::SendIntent>),
    DeliveryConfirmed {
        request_id: Uuid,
        delivery: outbox::DeliveryReceipt,
    },
    FlowSubscribed(Box<subscription::SubscriptionRecord>),
    FlowUnsubscribed(Uuid),
    EventsObserved(Box<subscription::Observation>),
    FlowWatchesExpired(Box<subscription::FlowWatchExpiry>),
    ProcessWatchRegistered(Box<process_watch::ProcessWatchRecord>),
    ProcessWatchCancelled(Uuid),
    ProcessObserved(Box<process_watch::ProcessObservation>),
    ProcessWatchesExpired(Box<process_watch::ProcessWatchExpiry>),
}

impl From<FlowFact> for FlowUpdate {
    fn from(fact: FlowFact) -> Self {
        match fact {
            FlowFact::Created(descriptor) => Self::Created {
                flow: descriptor.address,
                content: descriptor.content.summary(),
                created_by: descriptor.created_by,
            },
            FlowFact::Activated(activation) => Self::Activated {
                envelope: Box::new(activation.envelope()),
                inherited_context: activation.input.inherited_context,
            },
            FlowFact::WorkspaceRequested(value) => Self::WorkspaceRequested(value),
            FlowFact::Transition(value) => Self::Transition(value),
            FlowFact::TurnCompleted(value) => Self::TurnCompleted(value),
            FlowFact::EnvelopeAccepted(value) => {
                if let inbox::InboxPayload::Command { entry, user, .. } = value.request.payload {
                    Self::CommandAccepted {
                        id: value.id,
                        request_id: value.request.request_id,
                        recipient: value.request.recipient,
                        sender: value.request.sender,
                        accepted_at: value.accepted_at,
                        name: entry.name,
                        user,
                    }
                } else {
                    Self::EnvelopeAccepted(value)
                }
            }
            FlowFact::InputCommitted(value) => Self::InputCommitted(value),
            FlowFact::MessagePublished(value) => Self::MessagePublished(value),
            FlowFact::ContextCheckpoint(value) => Self::ContextCheckpoint(value),
            FlowFact::GoalUpdated(value) => Self::GoalUpdated(value),
            FlowFact::PlanningUpdated(value) => Self::PlanningUpdated(value),
            FlowFact::ExecutionEvents(value) => Self::ExecutionEvents(value),
            FlowFact::LegacyRecords(value) => Self::LegacyRecords(value),
            FlowFact::ReplyWaiterRegistered(value) => Self::ReplyWaiterRegistered(value),
            FlowFact::ReplyWaiterCancelled { lease, correlation } => {
                Self::ReplyWaiterCancelled { lease, correlation }
            }
            FlowFact::SendQueued(value) => Self::SendQueued(value),
            FlowFact::DeliveryConfirmed {
                request_id,
                delivery,
            } => Self::DeliveryConfirmed {
                request_id,
                delivery,
            },
            FlowFact::FlowSubscribed(value) => Self::FlowSubscribed(value),
            FlowFact::FlowUnsubscribed(value) => Self::FlowUnsubscribed(value),
            FlowFact::EventsObserved(value) => Self::EventsObserved(value),
            FlowFact::FlowWatchesExpired(value) => Self::FlowWatchesExpired(value),
            FlowFact::ProcessWatchRegistered(value) => Self::ProcessWatchRegistered(value),
            FlowFact::ProcessWatchCancelled(value) => Self::ProcessWatchCancelled(value),
            FlowFact::ProcessObserved(value) => Self::ProcessObserved(value),
            FlowFact::ProcessWatchesExpired(value) => Self::ProcessWatchesExpired(value),
        }
    }
}

impl FlowStore {
    pub fn read_updates(
        &mut self,
        after: Option<&CommitReceipt>,
        limit: NonZeroUsize,
    ) -> io::Result<JournalPage<FlowUpdate>> {
        let page = self.journal.read_after(after, limit)?;
        Ok(JournalPage {
            batches: page
                .batches
                .into_iter()
                .map(|batch| CommittedBatch {
                    receipt: batch.receipt,
                    facts: batch.facts.into_iter().map(FlowUpdate::from).collect(),
                })
                .collect(),
            next: page.next,
            has_more: page.has_more,
        })
    }
}
