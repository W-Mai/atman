//! Typed view updates derived from committed facts. Reading does not execute effects or consume input.

use std::{
    io,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

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
    event_log::journal::{CommitReceipt, Journal, JournalInspection},
    flow_store::position::{FlowBatch, FlowPage, FlowPosition},
    message::Message,
    tools::tool_output::{OutputArtifactIdentity, OutputStore},
};

/// A bounded page of shared final-answer publications read from a recorded Flow journal.
#[derive(Debug)]
pub struct PublishedMessagePage {
    pub messages: Vec<super::publication::PublishedMessage>,
    pub next: Option<Uuid>,
    pub has_more: bool,
}

/// A single shared publication's durable identity and registered output metadata.
#[derive(Debug)]
pub struct PublishedMessageDetail {
    pub publication_id: Uuid,
    pub receipt: FlowPosition,
    pub artifacts: Vec<OutputArtifactIdentity>,
}

const MAX_OUTPUT_ARTIFACT_BYTES: usize = 32 * 1024 * 1024;

/// Validate a recorded Flow journal without rebuilding its execution projection. The callback
/// receives each decoded batch in commit order; callers decide which batch bodies to retain.
fn inspect_flow_journal(
    dir: &Path,
    catalog: &super::SessionProjection,
    id: &super::FlowInstanceId,
    mut observe: impl FnMut(&CommitReceipt, &[FlowFact]) -> io::Result<()>,
) -> io::Result<JournalInspection> {
    let registration = catalog
        .flows
        .iter()
        .find(|flow| &flow.id == id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "flow is not registered"))?;
    let path = dir.join("flows").join(id.to_string()).join("events.jsonl");
    let mut descriptor = None;
    let (journal, _) = Journal::inspect_with_prefix_reader(&path, id.0.0, |receipt, facts, _| {
        if let Some(FlowFact::Created(value)) = facts.first() {
            descriptor = Some((**value).clone());
        }
        observe(&receipt, &facts)
    })?;
    let descriptor = descriptor.ok_or_else(|| super::invalid("flow creation was not committed"))?;
    if descriptor.creation_request != registration.creation_request {
        return Err(super::invalid(
            "flow creation does not match session registration",
        ));
    }
    Ok(journal)
}

impl super::SessionInspection {
    /// Read shared final-answer publications without opening an execution owner. Journal
    /// validation still covers the complete committed prefix, while retained message bodies are
    /// materialized only for the requested page.
    pub fn read_flow_output_page(
        &self,
        id: &super::FlowInstanceId,
        after: Option<Uuid>,
        limit: NonZeroUsize,
    ) -> io::Result<PublishedMessagePage> {
        let flow_dir = self.dir.join("flows").join(id.to_string());
        let mut seen_after = after.is_none();
        let mut messages = Vec::with_capacity(limit.get());
        let mut has_more = false;
        inspect_flow_journal(&self.dir, &self.projection, id, |receipt, facts| {
            let mut selected_ids = Vec::new();
            for fact in facts {
                let FlowFact::MessagePublished(publication) = fact else {
                    continue;
                };
                if publication.context_run.is_some()
                    || publication.message.origin
                        != super::super::message::MessageOrigin::FinalAnswer
                {
                    continue;
                }
                if !seen_after {
                    if Some(publication.id) == after {
                        seen_after = true;
                    }
                    continue;
                }
                if messages.len() + selected_ids.len() < limit.get() {
                    selected_ids.push(publication.id);
                } else {
                    has_more = true;
                }
            }
            if selected_ids.is_empty() {
                return Ok(());
            }
            let resolved = super::images::resolve(&flow_dir, facts)?;
            for fact in resolved {
                let FlowFact::MessagePublished(publication) = fact else {
                    continue;
                };
                if selected_ids.contains(&publication.id) {
                    messages.push(super::publication::PublishedMessage {
                        publication: *publication,
                        receipt: receipt.clone().into(),
                    });
                }
            }
            Ok(())
        })?;
        if !seen_after {
            return Err(super::invalid("output cursor is not in this flow"));
        }
        let next = messages
            .last()
            .map(|message| message.publication.id)
            .or(after);
        Ok(PublishedMessagePage {
            messages,
            next,
            has_more,
        })
    }

    /// Read one shared final publication's identity and registered output metadata without
    /// opening an execution owner. The optional receipt binds the lookup to the caller's
    /// observed journal boundary.
    pub fn read_flow_output_detail(
        &self,
        id: &super::FlowInstanceId,
        publication_id: Uuid,
        expected_receipt: Option<&CommitReceipt>,
    ) -> io::Result<PublishedMessageDetail> {
        let flow_dir = self.dir.join("flows").join(id.to_string());
        let output_store = OutputStore::at(&flow_dir);
        let mut detail = None;
        let mut rejected = false;
        inspect_flow_journal(&self.dir, &self.projection, id, |receipt, facts| {
            let Some(publication) = facts.iter().find_map(|fact| match fact {
                FlowFact::MessagePublished(publication) if publication.id == publication_id => {
                    Some(publication.as_ref())
                }
                _ => None,
            }) else {
                return Ok(());
            };
            if publication.context_run.is_some()
                || publication.message.origin != super::super::message::MessageOrigin::FinalAnswer
            {
                rejected = true;
                return Ok(());
            }
            if expected_receipt.is_some_and(|expected| expected != receipt) {
                return Err(super::invalid(
                    "output publication receipt does not match the requested cursor",
                ));
            }
            if detail.is_some() {
                return Err(super::invalid("output publication identity is duplicated"));
            }
            let artifacts = output_store
                .artifact_identities(
                    std::slice::from_ref(&publication.message),
                    MAX_OUTPUT_ARTIFACT_BYTES,
                )
                .map_err(|error| io::Error::other(error.to_string()))?;
            detail = Some(PublishedMessageDetail {
                publication_id,
                receipt: receipt.clone().into(),
                artifacts,
            });
            Ok(())
        })?;
        if rejected {
            return Err(super::invalid("publication is not a shared final output"));
        }
        detail.ok_or_else(|| super::invalid("output publication is not in this flow"))
    }

    /// Read a bounded page of typed Flow updates without opening an execution owner. Bodies and
    /// retained images are resolved only for batches in the requested page.
    pub fn read_flow_events_page(
        &self,
        id: &super::FlowInstanceId,
        after: Option<FlowPosition>,
        limit: NonZeroUsize,
    ) -> io::Result<FlowPage<FlowUpdate>> {
        let after_receipt = match after.as_ref() {
            None => None,
            Some(FlowPosition::Durable(receipt)) => Some(receipt),
            Some(FlowPosition::Transient { .. }) => {
                return Err(super::invalid("event cursor is not durable"));
            }
        };
        let flow_dir = self.dir.join("flows").join(id.to_string());
        let mut seen_after = after.is_none();
        let mut batches = Vec::with_capacity(limit.get());
        let mut has_more = false;
        inspect_flow_journal(&self.dir, &self.projection, id, |receipt, facts| {
            if !seen_after {
                if after_receipt.is_some_and(|cursor| cursor == receipt) {
                    seen_after = true;
                }
                return Ok(());
            }
            if batches.len() >= limit.get() {
                has_more = true;
                return Ok(());
            }
            let updates = super::images::resolve(&flow_dir, facts)?
                .into_iter()
                .map(FlowUpdate::from)
                .collect();
            batches.push(FlowBatch {
                receipt: receipt.clone().into(),
                facts: updates,
            });
            Ok(())
        })?;
        if !seen_after {
            return Err(super::invalid("event cursor is not in this flow"));
        }
        let next = batches.last().map(|batch| batch.receipt.clone()).or(after);
        Ok(FlowPage {
            batches,
            next,
            has_more,
        })
    }

    /// Inspect recorded updates without acquiring execution ownership, repairing tails, or recovering a turn. Publish the collected projection only after this read succeeds.
    pub fn inspect_flow_updates(
        &self,
        id: &super::FlowInstanceId,
        mut apply: impl FnMut(FlowPosition, Vec<FlowUpdate>) -> io::Result<()>,
    ) -> io::Result<crate::event_log::journal::JournalInspection> {
        super::inspect_registered_flow_with(&self.dir, &self.projection, id, |receipt, facts| {
            apply(
                receipt.clone().into(),
                super::images::resolve(&self.dir.join("flows").join(id.to_string()), facts)?
                    .into_iter()
                    .map(FlowUpdate::from)
                    .collect(),
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
                    transcript.apply(FlowBatch {
                        receipt: receipt.clone().into(),
                        facts: super::images::resolve(
                            &self.dir.join("flows").join(id.to_string()),
                            facts,
                        )?
                        .into_iter()
                        .map(FlowUpdate::from)
                        .collect(),
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
    /// Authored queue input identity; compact commands have already passed source validation.
    pub(crate) fn editable_user_id(&self) -> Option<Uuid> {
        match self {
            Self::Command {
                id,
                sender: inbox::SenderIdentity::User(_),
                user: Some(_),
                ..
            } => Some(*id),
            Self::Envelope(envelope) if envelope.request.is_editable_user_input() => {
                Some(envelope.id)
            }
            _ => None,
        }
    }

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
        #[serde(default)]
        continued_from: Option<super::history::LegacyArchive>,
    },
    WorkspaceRequested(Box<super::workspace::WorkspaceRequest>),
    Transition(FlowTransition),
    Controlled(Box<super::control::ControlUpdate>),
    TurnCompleted(Box<completion::Completion>),
    InputAccepted(Box<AcceptedInput>),
    InboxEdited(Box<inbox::InboxEditRequest>),
    InboxRepairCancelled(Box<inbox::InboxRepairCancellation>),
    InputCommitted(Box<inbox::CommittedInput>),
    MessagePublished(Box<publication::MessagePublication>),
    ContextCheckpoint(Box<context::ContextCheckpoint>),
    HistoryHeadChanged(Box<super::branches::HistoryHeadChange>),
    HistoryOperation(Box<super::history_operation::HistoryOperationEvent>),
    GoalUpdated(Box<goal::GoalUpdate>),
    PlanningUpdated(Box<planning::PlanningUpdate>),
    NameUpdated(Box<super::name::NameUpdate>),
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
            FlowFact::ImagesRetained(_) => {
                unreachable!("ownership bindings are resolved before view projection")
            }
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
                continued_from: activation.input.continued_from,
            },
            FlowFact::WorkspaceRequested(value) => Self::WorkspaceRequested(value),
            FlowFact::Transition(value) => Self::Transition(value),
            FlowFact::Controlled(value) => Self::Controlled(value),
            FlowFact::TurnCompleted(value) => Self::TurnCompleted(value),
            FlowFact::EnvelopeAccepted(value) => Self::InputAccepted(Box::new((*value).into())),
            FlowFact::InboxEdited(value) => Self::InboxEdited(value),
            FlowFact::InboxRepairCancelled(value) => Self::InboxRepairCancelled(value),
            FlowFact::InputCommitted(value) => Self::InputCommitted(value),
            FlowFact::MessagePublished(value) => Self::MessagePublished(value),
            FlowFact::ContextCheckpoint(value) => Self::ContextCheckpoint(value),
            FlowFact::HistoryHeadChanged(value) => Self::HistoryHeadChanged(value),
            FlowFact::HistoryOperation(value) => Self::HistoryOperation(value),
            FlowFact::GoalUpdated(value) => Self::GoalUpdated(value),
            FlowFact::PlanningUpdated(value) => Self::PlanningUpdated(value),
            FlowFact::NameUpdated(value) => Self::NameUpdated(value),
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
        after: Option<&FlowPosition>,
        limit: NonZeroUsize,
    ) -> io::Result<FlowPage<FlowUpdate>> {
        let page = self.journal.read_after(after, limit)?;
        Ok(FlowPage {
            batches: page
                .batches
                .into_iter()
                .map(|batch| FlowBatch {
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
            let path = flow.dir().unwrap().join("events.jsonl");
            if corrupt {
                record.raw_base64 = "Y29ycnVwdA==".into();
                // Valid journal framing must not bypass retained-record validation.
                let super::super::FlowJournalBackend::Durable { journal, .. } =
                    &mut flow.journal.backend
                else {
                    panic!("persistent test history");
                };
                journal
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
