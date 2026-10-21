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
        super::inspect_registered_flow_with(&self.dir, &self.projection, id, |receipt, facts| {
            apply(
                receipt.clone(),
                facts.iter().cloned().map(FlowUpdate::from).collect(),
            )
        })
        .map(|read| read.journal)
    }

    /// Read a complete archived flow without opening a writer, execution services, or the original source. Executable flows return `None` after fact validation without building a transcript. Incomplete imports and journal tails are rejected; retained malformed source records remain readable.
    pub fn archived_view(
        &self,
        id: &super::FlowInstanceId,
    ) -> io::Result<Option<crate::flow_runtime::FlowView>> {
        let flow = FlowRef {
            session_id: self.projection.descriptor.id.into(),
            instance_id: id.clone(),
        };
        let mut transcript = None;
        let read = super::inspect_registered_flow_with(
            &self.dir,
            &self.projection,
            id,
            |receipt, facts| {
                if matches!(facts.first(), Some(FlowFact::Created(descriptor)) if matches!(descriptor.content, FlowContent::History(_)))
                {
                    transcript = Some(crate::projection::flow::FlowTranscript::new(flow.clone()));
                }
                if let Some(transcript) = &mut transcript {
                    transcript.apply(CommittedBatch {
                        receipt: receipt.clone(),
                        facts: facts.iter().cloned().map(FlowUpdate::from).collect(),
                    })?;
                }
                Ok(())
            },
        )?;
        let FlowContent::History(archive) = read.descriptor.content else {
            return Ok(None);
        };
        let transcript = transcript.ok_or_else(|| super::invalid("archive has no creation"))?;
        if read.journal.trailing_bytes != 0 || transcript.entries().len() as u64 != archive.records
        {
            return Err(super::invalid("archived flow is incomplete"));
        }
        crate::flow_runtime::FlowView::from_archive(transcript, read.state).map(Some)
    }
}

/// Accepted input metadata, shared by activation and subsequent inbox deliveries. Command sources and execution parameters remain in the private journal.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AcceptedInput {
    Command {
        id: Uuid,
        request_id: Uuid,
        recipient: FlowRef,
        sender: inbox::SenderIdentity,
        accepted_at: chrono::DateTime<chrono::Utc>,
        name: String,
        user: Option<Box<inbox::UserContent>>,
    },
    Envelope(Box<inbox::AcceptedEnvelope>),
}

impl AcceptedInput {
    pub fn recipient(&self) -> &FlowRef {
        match self {
            Self::Command { recipient, .. } => recipient,
            Self::Envelope(envelope) => &envelope.request.recipient,
        }
    }
}

impl From<inbox::AcceptedEnvelope> for AcceptedInput {
    fn from(value: inbox::AcceptedEnvelope) -> Self {
        if let inbox::InboxPayload::Command { entry, user, .. } = value.request.payload {
            Self::Command {
                id: value.id,
                request_id: value.request.request_id,
                recipient: value.request.recipient,
                sender: value.request.sender,
                accepted_at: value.accepted_at,
                name: entry.name,
                user: user.map(Box::new),
            }
        } else {
            Self::Envelope(Box::new(value))
        }
    }
}

/// Read-only updates for one instance, in original commit order. Message origins, isolated contexts, and typed completion remain distinct; creation metadata omits the retained source bundle and activation omits execution authority.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum FlowUpdate {
    Created {
        flow: FlowRef,
        content: FlowContentSummary,
        created_by: Option<FlowRef>,
    },
    Activated {
        flow: FlowRef,
        input: Option<Box<AcceptedInput>>,
        /// Captured initialization command, distinct from ordinary accepted work.
        startup_input: Option<Uuid>,
        inherited_context: Vec<Message>,
    },
    WorkspaceRequested(Box<super::workspace::WorkspaceRequest>),
    Transition(FlowTransition),
    Controlled(Box<super::control::ControlUpdate>),
    TurnCompleted(Box<completion::Completion>),
    InputAccepted(Box<AcceptedInput>),
    InboxEdited(Box<inbox::InboxEditRequest>),
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
    DeliveryFailed {
        request_id: Uuid,
        failure: outbox::DeliveryFailure,
    },
    FlowSubscribed(Box<subscription::SubscriptionRecord>),
    FlowUnsubscribed(Uuid),
    EventsObserved(Box<subscription::Observation>),
    FlowWatchesExpired(Box<subscription::FlowWatchExpiry>),
    ProcessWatchRegistered(Box<process_watch::ProcessWatchRecord>),
    ProcessWatchCancelled(Uuid),
    WatcherReplaced(Box<super::watch_edit::WatcherReplacement>),
    ProcessObserved(Box<process_watch::ProcessObservation>),
    ProcessWatchesExpired(Box<process_watch::ProcessWatchExpiry>),
    TimerScheduled(Box<super::timer::TimerRecord>),
    TimerCancelled(Uuid),
    TimersFired(Box<super::timer::TimerDelivery>),
    TaskScopeCreated(Box<super::task_scope::TaskScopeCreated>),
    TaskScopeCancelled(Uuid),
    InputScopeInvalidated(Box<inbox::InputScopeInvalidation>),
    TaskScopeBound(super::task_scope::TaskScopeRef),
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
                startup_input: if matches!(
                    activation.input.request.start,
                    super::launch::EntryStart::Command { .. }
                ) {
                    activation.envelope().map(|input| input.id)
                } else {
                    None
                },
                flow: activation.input.request.recipient.clone(),
                input: activation
                    .envelope()
                    .map(|envelope| Box::new(envelope.into())),
                inherited_context: activation.input.inherited_context,
            },
            FlowFact::WorkspaceRequested(value) => Self::WorkspaceRequested(value),
            FlowFact::Transition(value) => Self::Transition(value),
            FlowFact::Controlled(value) => Self::Controlled(value),
            FlowFact::TurnCompleted(value) => Self::TurnCompleted(value),
            FlowFact::EnvelopeAccepted(value) => Self::InputAccepted(Box::new((*value).into())),
            FlowFact::InboxEdited(value) => Self::InboxEdited(value),
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
            FlowFact::DeliveryFailed {
                request_id,
                failure,
            } => Self::DeliveryFailed {
                request_id,
                failure,
            },
            FlowFact::FlowSubscribed(value) => Self::FlowSubscribed(value),
            FlowFact::FlowUnsubscribed(value) => Self::FlowUnsubscribed(value),
            FlowFact::EventsObserved(value) => Self::EventsObserved(value),
            FlowFact::FlowWatchesExpired(value) => Self::FlowWatchesExpired(value),
            FlowFact::ProcessWatchRegistered(value) => Self::ProcessWatchRegistered(value),
            FlowFact::ProcessWatchCancelled(value) => Self::ProcessWatchCancelled(value),
            FlowFact::WatcherReplaced(value) => Self::WatcherReplaced(value),
            FlowFact::ProcessObserved(value) => Self::ProcessObserved(value),
            FlowFact::ProcessWatchesExpired(value) => Self::ProcessWatchesExpired(value),
            FlowFact::TimerScheduled(value) => Self::TimerScheduled(value),
            FlowFact::TimerCancelled(value) => Self::TimerCancelled(value),
            FlowFact::TimersFired(value) => Self::TimersFired(value),
            FlowFact::TaskScopeCreated(value) => Self::TaskScopeCreated(value),
            FlowFact::TaskScopeCancelled(value) => Self::TaskScopeCancelled(value),
            FlowFact::InputScopeInvalidated(value) => Self::InputScopeInvalidated(value),
            FlowFact::TaskScopeBound(value) => Self::TaskScopeBound(value),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event_log::legacy,
        flow_store::{FlowDescriptor, FlowInstanceId, RuntimeStore, SessionDescriptor},
    };

    #[test]
    fn archived_views_validate_facts_and_never_repair_incomplete_history() {
        for corrupt in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("old.jsonl");
            std::fs::write(&source, b"invalid\n").unwrap();
            let old = Uuid::now_v7();
            let inspected = legacy::inspect(&source, old).unwrap();
            let runtime = RuntimeStore::open(temp.path()).unwrap();
            let id = Uuid::now_v7();
            let mut session = runtime
                .create_session(SessionDescriptor {
                    id,
                    project: None,
                    configuration: Default::default(),
                    title: "History".into(),
                })
                .unwrap();
            let instance = FlowInstanceId::now();
            let mut flow = session
                .create_flow(FlowDescriptor {
                    address: FlowRef {
                        session_id: id.into(),
                        instance_id: instance.clone(),
                    },
                    creation_request: Uuid::now_v7(),
                    created_by: None,
                    content: FlowContent::History(history::LegacyArchive {
                        session_id: old,
                        source: inspected.source,
                        owner: legacy::LegacyFlowKey::Unassigned,
                        records: 1,
                    }),
                })
                .unwrap();
            let read = RuntimeStore::inspect_session(temp.path(), id).unwrap();
            assert!(read.archived_view(&instance).is_err());
            let mut record =
                history::LegacyHistoryRecord::new(inspected.records[0].clone(), b"invalid\n");
            let path = flow.dir().join("events.jsonl");
            if corrupt {
                record.raw_base64 = "Y29ycnVwdA==".into();
                // Valid journal framing must not bypass retained-record validation.
                flow.journal
                    .inner
                    .commit(&[FlowFact::LegacyRecords(vec![record])])
                    .unwrap();
                assert!(read.inspect_flow_updates(&instance, |_, _| Ok(())).is_err());
            } else {
                flow.append_legacy_history(vec![record]).unwrap();
                assert!(read.archived_view(&instance).is_ok());
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                file.write_all(b"unfinished journal tail").unwrap();
                assert!(
                    read.inspect_flow_updates(&instance, |_, _| Ok(()))
                        .unwrap()
                        .trailing_bytes
                        > 0
                );
            }
            let before = std::fs::read(&path).unwrap();
            assert!(read.archived_view(&instance).is_err());
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
    }
}
