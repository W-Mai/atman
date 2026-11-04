//! Version-isolated session containers and persistent flow instances.
//!
//! Host-only storage: authorization and scheduling happen before commands reach this module. Each flow has one authoritative journal. Metadata files are disposable projections and never override facts. Blocking calls belong on storage workers.

pub mod access;
pub mod activity;
pub mod completion;
pub mod context;
pub mod continuation;
pub mod control;
mod deletion;
mod delivery_tombstone;
pub mod execution;
pub mod goal;
pub mod history;
mod history_assets;
mod images;
pub mod inbox;
pub mod inbox_view;
mod input;
pub mod input_observation;
pub mod launch;
mod legacy_removal;
pub mod migration;
pub mod name;
pub mod outbox;
pub mod planning;
pub mod process_watch;
pub mod profile;
pub mod publication;
pub mod relation;
mod replay;
pub mod subscription;
pub mod task_scope;
pub mod timer;
pub mod trace;
pub mod view;
pub mod waiter;
pub mod watch_edit;
pub mod watch_view;
pub mod workspace;

pub(crate) use deletion::ProjectDeletion;
pub use deletion::SessionRemoval;
pub use legacy_removal::LegacySessionRemoval;

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atman_rt::{
    FlowInstance, InstanceActivity, InstanceState, Interruption, RunId, TurnId, TurnLease,
    TurnOutcome, TurnPhase,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::event::AtmanUuid;
use crate::event_log::journal::{
    CommitReceipt, Journal, RecoveredTail, private_file_options, sync_parent,
};
use crate::source_program::RevisionBundle;
use inbox::{
    Acceptance, AcceptedEnvelope, CommittedInput, DeliveryRequest, Inbox, InboxSummary,
    InputContent, InputEnvelope,
};

pub type FlowInstanceId = atman_rt::FlowInstanceId<AtmanUuid>;
pub type FlowRef = atman_rt::FlowRef<AtmanUuid>;

const NAMESPACE: &str = "runtime-v1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionDescriptor {
    pub id: Uuid,
    // A projectless session is explicit; a missing binding is not an implicit default.
    #[serde(deserialize_with = "Option::deserialize")]
    pub project: Option<crate::storage::ProjectContext>,
    pub configuration: SessionConfiguration,
    pub title: String,
}

/// Configuration is an explicit session binding, not a fallback inferred from the current process.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "source", rename_all = "snake_case")]
pub enum SessionConfiguration {
    #[default]
    HostProvided,
    Atman(crate::config_hub::ConfigSource),
}

impl SessionDescriptor {
    /// Resolve recorded source namespaces without reading configuration or source files.
    pub fn source_roots(&self) -> crate::source_program::SourceRoots {
        self.source_roots_for(self.project.as_ref())
    }

    /// Resolve namespaces for an instance's captured project binding.
    pub fn source_roots_for(
        &self,
        project: Option<&crate::storage::ProjectContext>,
    ) -> crate::source_program::SourceRoots {
        crate::source_program::SourceRoots {
            project_root: project.map(|project| project.root().to_path_buf()),
            config_dir: match &self.configuration {
                SessionConfiguration::Atman(source) => Some(source.config_dir().to_path_buf()),
                SessionConfiguration::HostProvided => None,
            },
        }
    }

    fn validate(&self) -> io::Result<()> {
        if let Some(project) = &self.project {
            project.validate().map_err(invalid)?;
        }
        if let SessionConfiguration::Atman(source) = &self.configuration {
            source.validate().map_err(invalid)?;
        }
        Ok(())
    }
}

/// Recorded source closure; reading history never requires the original project directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EntrySnapshot {
    pub name: String,
    pub path: PathBuf,
    pub bundle: RevisionBundle,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FlowContent {
    Entry(EntrySnapshot),
    History(history::LegacyArchive),
}

impl From<EntrySnapshot> for FlowContent {
    fn from(entry: EntrySnapshot) -> Self {
        Self::Entry(entry)
    }
}

impl FlowContent {
    pub fn title(&self) -> &str {
        match self {
            Self::Entry(entry) => &entry.name,
            Self::History(history) => history.title(),
        }
    }

    fn initial_state(&self, address: FlowRef) -> io::Result<FlowInstance<AtmanUuid>> {
        let mut state = FlowInstance::prepared(address);
        if let Self::History(history) = self {
            history.validate()?;
            state.begin_close().map_err(invalid)?;
            state.finish_close().map_err(invalid)?;
        }
        Ok(state)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlowDescriptor {
    pub address: FlowRef,
    pub creation_request: Uuid,
    pub content: FlowContent,
    /// Audit provenance only; this does not confer control or propagate cancellation.
    pub created_by: Option<FlowRef>,
}

impl FlowDescriptor {
    pub fn entry(&self) -> io::Result<&EntrySnapshot> {
        match &self.content {
            FlowContent::Entry(entry) => Ok(entry),
            FlowContent::History(_) => Err(invalid("history-only flow has no executable entry")),
        }
    }

    pub fn entry_mut(&mut self) -> io::Result<&mut EntrySnapshot> {
        match &mut self.content {
            FlowContent::Entry(entry) => Ok(entry),
            FlowContent::History(_) => Err(invalid("history-only flow has no executable entry")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum FlowTransition {
    Activated,
    TurnStarted {
        turn: TurnId<AtmanUuid>,
        run: RunId<AtmanUuid>,
    },
    PhaseChanged {
        lease: TurnLease<AtmanUuid>,
        phase: TurnPhase,
    },
    ExecutionPaused {
        key: AtmanUuid,
    },
    ExecutionResumed {
        key: AtmanUuid,
    },
    InterruptionRecovered,
    TurnFinished {
        lease: TurnLease<AtmanUuid>,
        outcome: TurnOutcome,
    },
    CloseRequested,
    Closed,
}

impl FlowTransition {
    fn apply(&mut self, state: &mut FlowInstance<AtmanUuid>) -> io::Result<()> {
        let result = match self {
            Self::Activated => state.activate(),
            Self::TurnStarted { turn, run } => {
                state.begin_turn(turn.clone(), run.clone()).map(|_| ())
            }
            Self::PhaseChanged { lease, phase } => state.set_phase(lease, *phase),
            Self::ExecutionPaused { key } => state.pause(*key),
            Self::ExecutionResumed { key } => state.resume(key),
            Self::InterruptionRecovered => state.recover_interrupted(),
            Self::TurnFinished { lease, outcome } => state
                .finish_turn(lease, *outcome)
                .map(|actual| *outcome = actual),
            Self::CloseRequested => state.begin_close(),
            Self::Closed => state.finish_close(),
        };
        result.map_err(invalid)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum FlowFact {
    ImagesRetained(Vec<images::RetainedImage>),
    LegacyRecords(Vec<history::LegacyHistoryRecord>),
    ExecutionEvents(Vec<trace::ExecutionEvent>),
    Created(Box<FlowDescriptor>),
    Activated(Box<launch::Activation>),
    WorkspaceRequested(Box<workspace::WorkspaceRequest>),
    Transition(FlowTransition),
    Controlled(Box<control::ControlUpdate>),
    TurnCompleted(Box<completion::Completion>),
    EnvelopeAccepted(Box<AcceptedEnvelope>),
    InboxEdited(Box<inbox::InboxEditRequest>),
    InputCommitted(Box<CommittedInput>),
    ReplyWaiterRegistered(Box<waiter::ReplyWaiter>),
    ReplyWaiterCancelled {
        lease: TurnLease<AtmanUuid>,
        correlation: Uuid,
    },
    SendQueued(Box<outbox::SendIntent>),
    MessagePublished(Box<publication::MessagePublication>),
    ContextCheckpoint(Box<context::ContextCheckpoint>),
    GoalUpdated(Box<goal::GoalUpdate>),
    PlanningUpdated(Box<planning::PlanningUpdate>),
    NameUpdated(Box<name::NameUpdate>),
    FlowSubscribed(Box<subscription::SubscriptionRecord>),
    FlowUnsubscribed(Uuid),
    EventsObserved(Box<subscription::Observation>),
    FlowWatchesExpired(Box<subscription::FlowWatchExpiry>),
    ProcessWatchRegistered(Box<process_watch::ProcessWatchRecord>),
    ProcessWatchCancelled(Uuid),
    WatcherReplaced(Box<watch_edit::WatcherReplacement>),
    ProcessObserved(Box<process_watch::ProcessObservation>),
    ProcessWatchesExpired(Box<process_watch::ProcessWatchExpiry>),
    TimerScheduled(Box<timer::TimerRecord>),
    TimerCancelled(Uuid),
    TimersFired(Box<timer::TimerDelivery>),
    TaskScopeCreated(Box<task_scope::TaskScopeCreated>),
    TaskScopeCancelled(Uuid),
    TaskScopeBound(task_scope::TaskScopeRef),
    InputScopeInvalidated(Box<inbox::InputScopeInvalidation>),
    DeliveryConfirmed {
        request_id: Uuid,
        delivery: outbox::DeliveryReceipt,
    },
    DeliveryFailed {
        request_id: Uuid,
        failure: outbox::DeliveryFailure,
    },
}

/// All mutation paths share the content boundary, including input commit closures.
struct FlowJournal {
    inner: Journal,
    dir: PathBuf,
    history_only: bool,
    activity: Option<activity::RecordedActivity>,
}

impl FlowJournal {
    fn validate(history_only: bool, facts: &[FlowFact]) -> io::Result<()> {
        let (facts, _) = images::split(facts)?;
        FlowFact::validate_batch(facts)?;
        if history_only {
            if !matches!(facts, [FlowFact::LegacyRecords(_)]) {
                return Err(invalid("history-only flow rejects execution facts"));
            }
        } else if facts
            .iter()
            .any(|fact| matches!(fact, FlowFact::LegacyRecords(_)))
        {
            return Err(invalid("executable flow rejects legacy history batches"));
        }
        Ok(())
    }

    fn commit(&mut self, facts: &[FlowFact]) -> io::Result<CommitReceipt> {
        Self::validate(self.history_only, facts)?;
        let retained = images::retain(&self.dir, facts)?;
        let receipt = self.inner.commit(&retained)?;
        activity::observe(&mut self.activity, &receipt, facts);
        Ok(receipt)
    }

    fn read_after(
        &mut self,
        after: Option<&CommitReceipt>,
        limit: std::num::NonZeroUsize,
    ) -> io::Result<crate::event_log::journal::JournalPage<FlowFact>> {
        let mut page = self.inner.read_after::<FlowFact>(after, limit)?;
        for batch in &mut page.batches {
            batch.facts = images::resolve(&self.dir, &batch.facts)?;
        }
        Ok(page)
    }

    fn read_owned(
        &mut self,
        receipt: &CommitReceipt,
    ) -> io::Result<crate::event_log::journal::CommittedBatch<FlowFact>> {
        images::resolve_batch(&self.dir, self.inner.read_batch(receipt)?)
    }
}

impl FlowFact {
    fn validate_batch(facts: &[Self]) -> io::Result<()> {
        let (facts, _) = images::split(facts)?;
        if facts
            .iter()
            .any(|fact| matches!(fact, Self::NameUpdated(_)))
        {
            return if matches!(facts, [Self::NameUpdated(_)]) {
                Ok(())
            } else {
                Err(invalid("name updates require a dedicated batch"))
            };
        }
        if facts.iter().any(|fact| matches!(fact, Self::Controlled(_))) {
            return if matches!(facts, [Self::Controlled(_)]) {
                Ok(())
            } else {
                Err(invalid("execution control requires a dedicated batch"))
            };
        }
        if facts
            .iter()
            .any(|fact| matches!(fact, Self::InboxEdited(_)))
        {
            return if matches!(facts, [Self::InboxEdited(_)]) {
                Ok(())
            } else {
                Err(invalid("inbox edits require a dedicated batch"))
            };
        }
        if facts.iter().any(|fact| {
            matches!(
                fact,
                Self::Created(_) | Self::LegacyRecords(_) | Self::WorkspaceRequested(_)
            )
        }) {
            return if matches!(
                facts,
                [Self::Created(_)] | [Self::LegacyRecords(_)] | [Self::WorkspaceRequested(_)]
            ) {
                Ok(())
            } else {
                Err(invalid(
                    "creation and legacy history require dedicated batches",
                ))
            };
        }
        if facts
            .iter()
            .any(|fact| matches!(fact, Self::ExecutionEvents(_)))
        {
            return if matches!(facts, [Self::ExecutionEvents(_)]) {
                Ok(())
            } else {
                Err(invalid("execution diagnostics require their own batch"))
            };
        }
        if facts
            .iter()
            .any(|fact| matches!(fact, Self::Transition(FlowTransition::TurnFinished { .. })))
        {
            return Err(invalid("turn completion requires a result record"));
        }
        if facts
            .iter()
            .any(|fact| matches!(fact, Self::TurnCompleted(_)))
        {
            return if matches!(
                facts,
                [Self::TurnCompleted(_)]
                    | [
                        Self::TurnCompleted(_),
                        Self::Transition(FlowTransition::InterruptionRecovered)
                    ]
            ) {
                Ok(())
            } else {
                Err(invalid(
                    "turn completion permits only an optional interruption recovery in its batch",
                ))
            };
        }
        if facts.iter().any(|fact| matches!(fact, Self::Activated(_))) {
            return if match facts {
                [Self::Activated(activation)] => activation.goal_update().is_none(),
                [Self::Activated(activation), Self::GoalUpdated(goal)] => {
                    activation.goal_update().as_ref() == Some(goal.as_ref())
                }
                _ => false,
            } {
                Ok(())
            } else {
                Err(invalid(
                    "activation requires its input and exact initial goal in one batch",
                ))
            };
        }
        if facts.iter().any(|fact| {
            matches!(
                fact,
                Self::FlowSubscribed(_)
                    | Self::FlowUnsubscribed(_)
                    | Self::EventsObserved(_)
                    | Self::FlowWatchesExpired(_)
                    | Self::ProcessWatchRegistered(_)
                    | Self::ProcessWatchCancelled(_)
                    | Self::WatcherReplaced(_)
                    | Self::ProcessObserved(_)
                    | Self::ProcessWatchesExpired(_)
                    | Self::TimerScheduled(_)
                    | Self::TimerCancelled(_)
                    | Self::TimersFired(_)
                    | Self::TaskScopeCreated(_)
                    | Self::TaskScopeCancelled(_)
                    | Self::TaskScopeBound(_)
                    | Self::InputScopeInvalidated(_)
            )
        }) {
            return if facts.len() == 1
                || matches!(facts,
                [Self::ReplyWaiterRegistered(waiter), Self::FlowSubscribed(record)]
                    if record.subscription.reply.as_ref() == Some(waiter.as_ref()))
            {
                Ok(())
            } else {
                Err(invalid("subscription changes require their own batch"))
            };
        }
        if facts.iter().any(|fact| {
            matches!(
                fact,
                Self::MessagePublished(_)
                    | Self::ContextCheckpoint(_)
                    | Self::GoalUpdated(_)
                    | Self::PlanningUpdated(_)
            )
        }) {
            return if matches!(
                facts,
                [Self::MessagePublished(_)]
                    | [Self::ContextCheckpoint(_)]
                    | [Self::GoalUpdated(_)]
                    | [Self::PlanningUpdated(_)]
            ) {
                Ok(())
            } else {
                Err(invalid(
                    "context publication requires its own journal batch",
                ))
            };
        }
        if !facts
            .iter()
            .any(|fact| matches!(fact, Self::InputCommitted(_)))
        {
            return Ok(());
        }
        match facts {
            [Self::InputCommitted(input)]
                if matches!(
                    input.content,
                    InputContent::Context { .. } | InputContent::WaitingContinuation { .. }
                ) =>
            {
                Ok(())
            }
            [
                Self::Transition(FlowTransition::TurnStarted { turn, run }),
                Self::InputCommitted(input),
            ] if matches!(input.content, InputContent::InvocationInput { .. })
                && &input.lease.turn == turn
                && &input.lease.run == run =>
            {
                Ok(())
            }
            _ => Err(invalid(
                "invocation input and turn admission must share one batch",
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlowRegistration {
    pub id: FlowInstanceId,
    pub creation_request: Uuid,
    /// Project at registration, retained independently of later container changes.
    pub project: Option<crate::storage::ProjectContext>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionFact {
    Created(SessionDescriptor),
    TitleChanged(Option<String>),
    ProjectChanged(Option<crate::storage::ProjectContext>),
    FlowRegistered {
        id: FlowInstanceId,
        creation_request: Uuid,
    },
    DefaultFlowSelected(FlowInstanceId),
    FlowArchiveChanged {
        id: FlowInstanceId,
        archived: bool,
    },
    AccessGranted(access::FlowGrant),
    AccessRevoked(Uuid),
    LegacyImported(migration::LegacyMigration),
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionProjection {
    pub descriptor: SessionDescriptor,
    /// Current association for discovery and new instance registration.
    pub project: Option<crate::storage::ProjectContext>,
    /// Mutable display name; the creation descriptor remains unchanged.
    pub title: Option<String>,
    pub flows: Vec<FlowRegistration>,
    pub default_flow: Option<FlowInstanceId>,
    /// Reversible discovery metadata; archived instances remain readable and closed.
    pub archived_flows: std::collections::HashSet<FlowInstanceId>,
    pub access: access::SessionAccess,
    pub migrated_from: Option<migration::LegacyMigration>,
}

impl SessionProjection {
    fn replay(projection: &mut Option<Self>, id: Uuid, facts: Vec<SessionFact>) -> io::Result<()> {
        for fact in facts {
            match (projection.as_mut(), fact) {
                (None, SessionFact::Created(descriptor)) if descriptor.id == id => {
                    descriptor.validate()?;
                    *projection = Some(SessionProjection {
                        title: (!descriptor.title.is_empty()).then(|| descriptor.title.clone()),
                        project: descriptor.project.clone(),
                        descriptor,
                        flows: Vec::new(),
                        default_flow: None,
                        archived_flows: Default::default(),
                        access: Default::default(),
                        migrated_from: None,
                    });
                }
                (Some(state), fact) => state.apply(fact)?,
                _ => return Err(invalid("missing or mismatched session creation")),
            }
        }
        Ok(())
    }

    fn apply(&mut self, fact: SessionFact) -> io::Result<()> {
        match fact {
            SessionFact::Created(_) => return Err(invalid("duplicate session creation")),
            SessionFact::TitleChanged(title) => self.title = title,
            SessionFact::ProjectChanged(project) => {
                if let Some(project) = &project {
                    project.validate().map_err(invalid)?;
                }
                self.project = project;
            }
            SessionFact::FlowRegistered {
                id,
                creation_request,
            } => {
                if self
                    .flows
                    .iter()
                    .any(|flow| flow.id == id || flow.creation_request == creation_request)
                {
                    return Err(invalid("duplicate flow registration"));
                }
                self.flows.push(FlowRegistration {
                    id,
                    creation_request,
                    project: self.project.clone(),
                });
            }
            SessionFact::DefaultFlowSelected(id) => {
                if !self.flows.iter().any(|flow| flow.id == id) {
                    return Err(invalid("default flow is not registered"));
                }
                self.default_flow = Some(id);
            }
            SessionFact::FlowArchiveChanged { id, archived } => {
                if !self.flows.iter().any(|flow| flow.id == id) {
                    return Err(invalid("archived flow is not registered"));
                }
                if archived {
                    self.archived_flows.insert(id);
                } else {
                    self.archived_flows.remove(&id);
                }
            }
            SessionFact::AccessGranted(grant) => {
                if grant.grantee.session_id.0 != self.descriptor.id
                    || !self
                        .flows
                        .iter()
                        .any(|flow| flow.id == grant.grantee.instance_id)
                {
                    return Err(invalid("grant recipient is not registered in this session"));
                }
                self.access.grant(grant)?;
            }
            SessionFact::AccessRevoked(id) => self.access.revoke(id)?,
            SessionFact::LegacyImported(migration) => {
                migration.validate(self)?;
                self.migrated_from = Some(migration);
            }
        }
        Ok(())
    }
}

/// Recorded catalog state inspected without acquiring execution ownership or consulting disposable metadata.
#[derive(Debug)]
pub struct SessionInspection {
    pub dir: PathBuf,
    pub projection: SessionProjection,
    pub journal: crate::event_log::journal::JournalInspection,
}

/// Recorded flow facts without execution ownership, recovery, or cache writes. An active recorded turn is not evidence of a currently running owner.
#[derive(Debug)]
pub struct FlowInspection {
    pub display_title: String,
    pub name: name::NameSnapshot,
    pub archived: bool,
    /// Project captured by the session journal when the instance was registered.
    pub project: Option<crate::storage::ProjectContext>,
    pub descriptor: FlowDescriptor,
    pub state: FlowInstance<AtmanUuid>,
    pub journal: crate::event_log::journal::JournalInspection,
    pub relations: Vec<relation::FlowRelation>,
    pub watchers: Vec<watch_view::WatchRecord>,
    pub timers: Vec<timer::TimerRecord>,
    pub task_scopes: Vec<task_scope::TaskScopeRecord>,
    pub task_scope_binding: Option<task_scope::TaskScopeRef>,
    pub workspace: Option<workspace::WorkspaceRequest>,
    pub goal: Option<crate::memory::goal::GoalRecord>,
    pub pending_inputs: usize,
    /// Shared conversation messages, including inherited context or archived history and excluding isolated calls.
    pub message_count: usize,
    /// Compact latest assistant output for detached activity surfaces.
    pub preview: Option<String>,
    /// Unsettled sends retain their payloads and prevent removal of the owning Session.
    pub pending_sends: usize,
    pub activity: Option<activity::RecordedActivity>,
    pub execution: execution::ExecutionObservation,
}

pub(crate) fn preview_messages(messages: &[crate::message::Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        if message.role != crate::message::MessageRole::Assistant
            || message.origin == crate::message::MessageOrigin::Internal
        {
            return None;
        }
        let text = compact_preview(&message.text_concat());
        if !text.is_empty() {
            return Some(text);
        }
        let thinking = compact_preview(&message.thinking_concat());
        if !thinking.is_empty() {
            return Some(thinking);
        }
        let tools = message
            .parts
            .iter()
            .filter_map(|part| match part {
                crate::message::MessagePart::ToolUse { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        (!tools.is_empty()).then(|| tools.join(" "))
    })
}

pub(crate) fn compact_preview(text: &str) -> String {
    let tail: String = text.chars().rev().take(512).collect();
    tail.chars()
        .rev()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod preview_tests {
    #[test]
    fn shared_preview_keeps_public_output_tail_and_word_boundaries() {
        use crate::message::{Message, MessageOrigin};
        let turn = atman_rt::TurnId::now();
        let output = Message::assistant_text(
            turn.clone(),
            format!("old prefix {}\nlast answer", "界".repeat(600)),
        );
        let mut internal = Message::assistant_text(turn, "private rules");
        internal.origin = MessageOrigin::Internal;
        let preview = super::preview_messages(&[output, internal]).unwrap();
        assert!(preview.ends_with("last answer"));
        assert!(!preview.contains("old prefix") && !preview.contains("private rules"));
        assert!(preview.chars().count() <= 512);
    }
}

impl SessionInspection {
    /// Filesystem activity hint for directory ordering only; it is not an execution timestamp or journal cursor.
    pub fn activity_time(&self) -> Option<std::time::SystemTime> {
        std::iter::once(self.dir.join("events.jsonl"))
            .chain(self.projection.flows.iter().map(|flow| {
                self.dir
                    .join("flows")
                    .join(flow.id.to_string())
                    .join("events.jsonl")
            }))
            .filter_map(|path| path.metadata().and_then(|meta| meta.modified()).ok())
            .max()
    }

    pub fn inspect_flow(&self, id: &FlowInstanceId) -> io::Result<FlowInspection> {
        inspect_registered_flow(&self.dir, &self.projection, id)
    }

    /// Read all durable relations from the session and its flow journals without opening writers.
    pub fn relations(&self) -> io::Result<Vec<relation::FlowRelation>> {
        let mut relations = relation::access_relations(&self.projection);
        for registration in &self.projection.flows {
            relations.extend(self.inspect_flow(&registration.id)?.relations);
        }
        relations.sort_by_key(|relation| {
            (
                format!("{:?}", relation.source),
                format!("{:?}", relation.target),
                relation.kind,
                relation.id,
            )
        });
        relations.dedup();
        Ok(relations)
    }
}

fn inspect_registered_flow(
    dir: &Path,
    catalog: &SessionProjection,
    id: &FlowInstanceId,
) -> io::Result<FlowInspection> {
    inspect_registered_flow_with(dir, catalog, id, |_, _| Ok(()))
}

fn read_registered_flow(
    dir: &Path,
    catalog: &SessionProjection,
    id: &FlowInstanceId,
    mut observe: impl FnMut(&CommitReceipt, &[FlowFact]) -> io::Result<()>,
) -> io::Result<RecordedFlow> {
    let registration = catalog
        .flows
        .iter()
        .find(|flow| &flow.id == id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "flow is not registered"))?;
    let address = FlowRef {
        session_id: catalog.descriptor.id.into(),
        instance_id: id.clone(),
    };
    let mut replay = replay::FlowReplay::default();
    let mut watch_deliveries = watch_view::WatchDeliveries::default();
    let flow_dir = dir.join("flows").join(id.to_string());
    let (journal, reader) = Journal::inspect_with_reader(
        &dir.join("flows").join(id.to_string()).join("events.jsonl"),
        id.0.0,
        |receipt, facts| {
            observe(&receipt, &facts)?;
            for fact in &facts {
                watch_deliveries.observe(fact)?;
            }
            replay.apply(&flow_dir, &address, receipt, facts)
        },
    )?;
    let (descriptor, _, _) = replay
        .projection
        .as_ref()
        .ok_or_else(|| invalid("flow creation was not committed"))?;
    if descriptor.creation_request != registration.creation_request {
        return Err(invalid("flow creation does not match session registration"));
    }
    Ok(RecordedFlow {
        project: registration.project.clone(),
        replay,
        journal,
        reader,
        watch_deliveries,
    })
}

struct RecordedFlow {
    project: Option<crate::storage::ProjectContext>,
    replay: replay::FlowReplay,
    journal: crate::event_log::journal::JournalInspection,
    reader: crate::event_log::journal::JournalReader,
    watch_deliveries: watch_view::WatchDeliveries,
}

fn inspect_registered_flow_with(
    dir: &Path,
    catalog: &SessionProjection,
    id: &FlowInstanceId,
    observe: impl FnMut(&CommitReceipt, &[FlowFact]) -> io::Result<()>,
) -> io::Result<FlowInspection> {
    let RecordedFlow {
        project,
        replay,
        journal,
        watch_deliveries,
        reader: _,
    } = read_registered_flow(dir, catalog, id, observe)?;
    let (descriptor, state, inbox) = replay.projection.unwrap();
    let target = descriptor.address.clone();
    let mut relations = Vec::new();
    if let Some(source) = descriptor.created_by.clone() {
        relations.push(relation::FlowRelation::created(
            source,
            target.clone(),
            descriptor.creation_request,
        ));
    }
    relations.extend(
        replay
            .subscriptions
            .values()
            .filter(|record| {
                record.subscription.purpose == subscription::SubscriptionPurpose::Watcher
            })
            .map(|record| {
                relation::FlowRelation::watch(
                    target.clone(),
                    record.subscription.source.clone(),
                    record.subscription.id,
                    relation::watch_active(record.status),
                )
            }),
    );
    relations.sort_by_key(|relation| {
        (
            format!("{:?}", relation.source),
            format!("{:?}", relation.target),
            relation.kind,
            relation.id,
        )
    });
    relations.dedup();
    let watchers = watch_view::project(
        &replay.subscriptions,
        &replay.process_watches,
        &inbox,
        &watch_deliveries,
    );
    let execution = execution::ExecutionObservation::capture(
        &state,
        journal
            .cursor
            .as_ref()
            .ok_or_else(|| invalid("flow journal has no creation"))?,
        replay.trace.latest.as_ref(),
        None,
        false,
    );
    Ok(FlowInspection {
        name: replay.names.snapshot.clone(),
        display_title: replay.names.display_title(&descriptor.content),
        archived: catalog.archived_flows.contains(id),
        project,
        descriptor,
        state,
        journal,
        relations,
        watchers,
        timers: replay.timers.records().cloned().collect(),
        task_scopes: replay.task_scopes.records().cloned().collect(),
        task_scope_binding: replay.task_scopes.binding().cloned(),
        workspace: replay.workspace,
        goal: replay.goals.current().cloned(),
        pending_inputs: inbox.pending_entries().count(),
        message_count: replay.history.message_count
            + publication::message_count(
                &inbox,
                &replay.publications,
                replay.activation.as_ref().map(|(record, _)| record),
            ),
        preview: preview_messages(&replay.context_window),
        pending_sends: replay.outbox.pending().count(),
        activity: replay.activity,
        execution,
    })
}

/// The namespace never overlaps published flat session logs.
pub struct RuntimeStore {
    root: PathBuf,
}

impl RuntimeStore {
    /// Discover published session directories without creating a namespace or opening an owner lease. Temporary directories and symlinks are excluded.
    pub fn session_ids(data_dir: &Path) -> io::Result<Vec<Uuid>> {
        let entries = match std::fs::read_dir(data_dir.join(NAMESPACE).join("sessions")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(error),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            if let Some(name) = name.to_str()
                && let Ok(id) = Uuid::parse_str(name)
                && id.to_string() == name
                && !(Self {
                    root: data_dir.join(NAMESPACE),
                })
                .deletion_path(id)
                .try_exists()?
            {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Read recorded catalog facts without repairing logs, rewriting metadata, or recovering execution. The source project and configuration need not exist. Use the owning host to observe acknowledged live state.
    pub fn inspect_session(data_dir: &Path, id: Uuid) -> io::Result<SessionInspection> {
        Self {
            root: data_dir.join(NAMESPACE),
        }
        .ensure_session_available(id)?;
        let dir = std::path::absolute(data_dir)?
            .join(NAMESPACE)
            .join("sessions")
            .join(id.to_string());
        let mut projection = None;
        let journal = Journal::inspect(&dir.join("events.jsonl"), id, |_, facts| {
            SessionProjection::replay(&mut projection, id, facts)
        })?;
        Ok(SessionInspection {
            dir,
            projection: projection.ok_or_else(|| invalid("session creation was not recorded"))?,
            journal,
        })
    }

    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let root = std::path::absolute(data_dir)?.join(NAMESPACE);
        ensure_dir(&root.join("sessions"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create_session(&self, descriptor: SessionDescriptor) -> io::Result<SessionStore> {
        descriptor.validate()?;
        let _catalog = self.lock_catalog()?;
        self.create_session_locked(descriptor)
    }

    fn create_session_locked(&self, descriptor: SessionDescriptor) -> io::Result<SessionStore> {
        self.ensure_session_available(descriptor.id)?;
        let dir = self.root.join("sessions").join(descriptor.id.to_string());
        initialize_dir(&dir, |dir| {
            self.create_session_at(dir, descriptor.clone()).map(drop)
        })?;
        self.open_session_locked(descriptor.id)
    }

    fn lock_catalog(&self) -> io::Result<crate::file_lock::FileLock> {
        let file = private_file_options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(".catalog.lock"))?;
        crate::file_lock::FileLock::exclusive(file)
    }

    fn create_session_at(
        &self,
        dir: &Path,
        descriptor: SessionDescriptor,
    ) -> io::Result<SessionStore> {
        let owner = Arc::new(SessionOwner {
            _lock: SessionOwner::lock(dir)?,
            descriptor: descriptor.clone(),
        });
        ensure_dir(&dir.join("flows"))?;
        ensure_dir(&dir.join("artifacts"))?;
        let mut journal = Journal::create(&dir.join("events.jsonl"), descriptor.id)?;
        let cursor = journal.commit(&[SessionFact::Created(descriptor.clone())])?;
        let mut store = SessionStore {
            dir: dir.to_path_buf(),
            owner,
            journal,
            projection: SessionProjection {
                title: (!descriptor.title.is_empty()).then(|| descriptor.title.clone()),
                project: descriptor.project.clone(),
                descriptor,
                flows: Vec::new(),
                default_flow: None,
                archived_flows: Default::default(),
                access: Default::default(),
                migrated_from: None,
            },
            cursor,
            projection_error: None,
            recovered_tail: None,
        };
        store.refresh_projection();
        Ok(store)
    }

    pub fn open_session(&self, id: Uuid) -> io::Result<SessionStore> {
        let _catalog = self.lock_catalog()?;
        self.open_session_locked(id)
    }

    fn open_session_locked(&self, id: Uuid) -> io::Result<SessionStore> {
        self.ensure_session_available(id)?;
        let dir = self.root.join("sessions").join(id.to_string());
        let owner_lock = SessionOwner::lock(&dir)?;
        let mut projection: Option<SessionProjection> = None;
        let mut cursor = None;
        let (journal, recovered_tail) =
            Journal::open(&dir.join("events.jsonl"), id, |receipt, facts| {
                SessionProjection::replay(&mut projection, id, facts)?;
                cursor = Some(receipt);
                Ok(())
            })?;
        let projection = projection.ok_or_else(|| invalid("session creation was not committed"))?;
        let owner = Arc::new(SessionOwner {
            _lock: owner_lock,
            descriptor: projection.descriptor.clone(),
        });
        let mut store = SessionStore {
            dir,
            owner,
            journal,
            projection,
            cursor: cursor.ok_or_else(|| invalid("empty session journal"))?,
            projection_error: None,
            recovered_tail,
        };
        store.refresh_projection();
        Ok(store)
    }
}

struct SessionOwner {
    _lock: crate::file_lock::FileLock,
    descriptor: SessionDescriptor,
}

impl SessionOwner {
    fn lock(dir: &Path) -> io::Result<crate::file_lock::FileLock> {
        let mut options = private_file_options();
        let file = options
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(".owner.lock"))?;
        crate::file_lock::FileLock::try_exclusive(file).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "session is already open in another Atman process; close that session before opening it here",
                )
            } else {
                error
            }
        })
    }
}

/// One session owner; each returned flow keeps its execution lease alive independently.
pub struct SessionStore {
    dir: PathBuf,
    owner: Arc<SessionOwner>,
    journal: Journal,
    projection: SessionProjection,
    cursor: CommitReceipt,
    projection_error: Option<io::Error>,
    pub recovered_tail: Option<RecoveredTail>,
}

impl SessionStore {
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn projection(&self) -> &SessionProjection {
        &self.projection
    }
    pub fn cursor(&self) -> &CommitReceipt {
        &self.cursor
    }
    pub fn take_projection_error(&mut self) -> Option<io::Error> {
        self.projection_error.take()
    }

    /// Prepare and register before returning an addressable instance. Repeated creation requires the same immutable descriptor.
    pub fn create_flow(&mut self, descriptor: FlowDescriptor) -> io::Result<FlowStore> {
        self.create_flow_at(descriptor, None)
    }

    pub(crate) fn create_flow_at(
        &mut self,
        descriptor: FlowDescriptor,
        expected: Option<&CommitReceipt>,
    ) -> io::Result<FlowStore> {
        self.create_flow_with_access(descriptor, expected, None)
    }

    pub(crate) fn create_flow_with_access(
        &mut self,
        descriptor: FlowDescriptor,
        expected: Option<&CommitReceipt>,
        grant: Option<access::FlowGrant>,
    ) -> io::Result<FlowStore> {
        if descriptor.address.session_id.0 != self.projection.descriptor.id {
            return Err(invalid("flow belongs to another session"));
        }
        descriptor
            .content
            .initial_state(descriptor.address.clone())?;
        let id = descriptor.address.instance_id.clone();
        let existing = self
            .projection
            .flows
            .iter()
            .find(|flow| flow.id == id || flow.creation_request == descriptor.creation_request);
        if existing.is_none() && expected.is_some_and(|cursor| cursor != &self.cursor) {
            return Err(invalid(
                "flow inventory changed after inspection; call flow.instances again",
            ));
        }
        if let Some(existing) = existing {
            if existing.id != id || existing.creation_request != descriptor.creation_request {
                return Err(invalid(
                    "flow creation request conflicts with an existing registration",
                ));
            }
            let mut store = self.restore_flow(&id)?;
            if store.descriptor != descriptor {
                return Err(invalid("flow creation payload changed"));
            }
            store.interrupt_lost_turn()?;
            return Ok(store);
        }
        let dir = self.dir.join("flows").join(id.to_string());
        let project = self.projection.project.clone();
        let mut flow = match initialize_dir(&dir, |dir| {
            FlowStore::create(dir, self.owner.clone(), descriptor.clone(), project.clone())
                .map(drop)
        }) {
            Ok(()) => FlowStore::open(
                &dir,
                self.owner.clone(),
                &descriptor.address,
                project.clone(),
            )?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // A crash between flow creation and container registration leaves a prepared orphan.
                let store =
                    FlowStore::open(&dir, self.owner.clone(), &descriptor.address, project)?;
                if store.descriptor != descriptor || store.cursor.sequence != 1 {
                    return Err(invalid(
                        "unregistered flow does not match the prepared creation",
                    ));
                }
                store
            }
            Err(error) => return Err(error),
        };
        let mut facts = vec![SessionFact::FlowRegistered {
            id: id.clone(),
            creation_request: descriptor.creation_request,
        }];
        if let Some(grant) = grant {
            if grant.target != access::AccessTarget::Flow(descriptor.address.clone()) {
                return Err(invalid("creation grant must target the new instance"));
            }
            facts.push(SessionFact::AccessGranted(grant));
        }
        if self.projection.default_flow.is_none() {
            facts.push(SessionFact::DefaultFlowSelected(id));
        }
        self.commit(facts)?;
        flow.refresh_projection();
        Ok(flow)
    }

    /// Acquire the instance writer and durably interrupt any invocation left by its previous owner.
    pub fn open_flow(&self, id: &FlowInstanceId) -> io::Result<FlowStore> {
        let mut store = self.restore_flow(id)?;
        store.interrupt_lost_turn()?;
        Ok(store)
    }

    /// Inspect retained facts without opening a flow writer or recovering interrupted execution.
    pub fn inspect_flow(&self, id: &FlowInstanceId) -> io::Result<FlowInspection> {
        inspect_registered_flow(&self.dir, &self.projection, id)
    }

    fn restore_flow(&self, id: &FlowInstanceId) -> io::Result<FlowStore> {
        let registration = self
            .projection
            .flows
            .iter()
            .find(|flow| &flow.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "flow is not registered"))?;
        let store = FlowStore::open(
            &self.dir.join("flows").join(id.to_string()),
            self.owner.clone(),
            &FlowRef {
                session_id: self.projection.descriptor.id.into(),
                instance_id: id.clone(),
            },
            registration.project.clone(),
        )?;
        if store.descriptor.creation_request != registration.creation_request {
            return Err(invalid("flow creation does not match session registration"));
        }
        Ok(store)
    }

    /// Select a registered view default without changing execution or authority. Repeating the current selection does not write; stale selections cannot replace it.
    pub fn select_default(
        &mut self,
        expected: &CommitReceipt,
        id: FlowInstanceId,
    ) -> io::Result<CommitReceipt> {
        if self.projection.default_flow.as_ref() == Some(&id) {
            return Ok(self.cursor.clone());
        }
        if expected != &self.cursor {
            return Err(invalid(
                "session catalog changed; refresh before selecting a default",
            ));
        }
        self.commit(vec![SessionFact::DefaultFlowSelected(id)])
    }

    /// Change discovery metadata for a closed instance without altering its history, default binding, permissions, or execution. Restoring visibility does not reopen execution.
    pub fn set_flow_archived(
        &mut self,
        expected: &CommitReceipt,
        id: FlowInstanceId,
        archived: bool,
    ) -> io::Result<CommitReceipt> {
        self.set_flow_archived_observed(expected, id, archived, None)
    }

    pub(crate) fn set_flow_archived_observed(
        &mut self,
        expected: &CommitReceipt,
        id: FlowInstanceId,
        archived: bool,
        observed: Option<FlowInstance<AtmanUuid>>,
    ) -> io::Result<CommitReceipt> {
        if !self.projection.flows.iter().any(|flow| flow.id == id) {
            return Err(invalid("flow is not registered"));
        }
        if self.projection.archived_flows.contains(&id) == archived {
            return Ok(self.cursor.clone());
        }
        if expected != &self.cursor {
            return Err(invalid(
                "session catalog changed; refresh before changing flow visibility",
            ));
        }
        let mut reader = None;
        if archived {
            if let Some(state) = observed {
                if state.address().session_id.0 != self.projection.descriptor.id
                    || state.address().instance_id != id
                    || !matches!(state.state(), InstanceState::Closed)
                {
                    return Err(invalid("flow writer has not acknowledged closure"));
                }
            } else {
                let file = File::open(
                    self.dir
                        .join("flows")
                        .join(id.to_string())
                        .join("events.jsonl"),
                )?;
                reader = Some(crate::file_lock::FileLock::try_shared(file)?);
            }
            let inspection = self.inspect_flow(&id)?;
            if inspection.journal.trailing_bytes != 0
                || !matches!(inspection.state.state(), InstanceState::Closed)
            {
                return Err(invalid(
                    "only closed flows with complete journals can be archived",
                ));
            }
        }
        let result = self.commit(vec![SessionFact::FlowArchiveChanged { id, archived }]);
        drop(reader);
        result
    }

    /// Persist a display name without changing creation identity or flow execution. An already applied value is a no-op; a stale snapshot cannot replace a different title.
    pub fn set_title(
        &mut self,
        expected: &CommitReceipt,
        title: Option<String>,
    ) -> io::Result<CommitReceipt> {
        if self.projection.title == title {
            return Ok(self.cursor.clone());
        }
        if expected != &self.cursor {
            return Err(invalid("session catalog changed; refresh before renaming"));
        }
        self.commit(vec![SessionFact::TitleChanged(title)])
    }

    /// Change the container association for discovery and future registrations. Existing instances, files, and resources retain their captured bindings.
    pub fn set_project(
        &mut self,
        expected: &CommitReceipt,
        project: Option<crate::storage::ProjectContext>,
    ) -> io::Result<CommitReceipt> {
        if self.projection.project == project {
            return Ok(self.cursor.clone());
        }
        if expected != &self.cursor {
            return Err(invalid(
                "session catalog changed; refresh before changing project",
            ));
        }
        // Project deletion scans associations under this lock before removing shared data.
        let runtime = RuntimeStore {
            root: self.dir.parent().unwrap().parent().unwrap().to_path_buf(),
        };
        let _catalog = runtime.lock_catalog()?;
        self.commit(vec![SessionFact::ProjectChanged(project)])
    }

    fn commit(&mut self, facts: Vec<SessionFact>) -> io::Result<CommitReceipt> {
        let mut candidate = self.projection.clone();
        for fact in &facts {
            candidate.apply(fact.clone())?;
        }
        let cursor = self.journal.commit(&facts)?;
        self.projection = candidate;
        self.cursor = cursor.clone();
        self.refresh_projection();
        Ok(cursor)
    }

    fn refresh_projection(&mut self) {
        self.projection_error = write_projection(
            &self.dir.join("session.json"),
            &self.cursor,
            &self.projection,
        )
        .err();
    }
}

pub struct FlowStore {
    dir: PathBuf,
    _owner: Arc<SessionOwner>,
    project: Option<crate::storage::ProjectContext>,
    journal: FlowJournal,
    descriptor: FlowDescriptor,
    state: FlowInstance<AtmanUuid>,
    live_turn_clock: Option<execution::LiveTurnClock>,
    closed: Option<CommitReceipt>,
    inbox: Inbox,
    outbox: outbox::Outbox,
    publications: publication::Publications,
    subscriptions: subscription::Subscriptions,
    process_watches: process_watch::ProcessWatches,
    timers: timer::Timers,
    task_scopes: task_scope::TaskScopes,
    watch_replacements: watch_edit::WatchReplacements,
    activation: Option<(launch::Activation, CommitReceipt)>,
    workspace: Option<workspace::WorkspaceRequest>,
    completions: completion::Completions,
    checkpoints: context::Checkpoints,
    names: name::Names,
    goals: goal::Goals,
    planning: planning::Planning,
    controls: control::Controls,
    trace: trace::ExecutionTrace,
    history: history::HistoryProgress,
    cursor: CommitReceipt,
    projection_error: Option<io::Error>,
    pub recovered_tail: Option<RecoveredTail>,
}

impl FlowStore {
    /// Latest timestamped activity from committed facts, independent of filesystem modification time.
    pub fn activity(&self) -> Option<&activity::RecordedActivity> {
        self.journal.activity.as_ref()
    }

    pub fn session_descriptor(&self) -> &SessionDescriptor {
        &self._owner.descriptor
    }

    /// Immutable instance binding; does not follow the container's current project.
    pub fn project(&self) -> Option<&crate::storage::ProjectContext> {
        self.project.as_ref()
    }

    pub fn source_roots(&self) -> crate::source_program::SourceRoots {
        self.session_descriptor().source_roots_for(self.project())
    }

    fn create(
        dir: &Path,
        owner: Arc<SessionOwner>,
        descriptor: FlowDescriptor,
        project: Option<crate::storage::ProjectContext>,
    ) -> io::Result<Self> {
        ensure_dir(&dir.join("artifacts"))?;
        let mut journal = Journal::create(
            &dir.join("events.jsonl"),
            descriptor.address.instance_id.0.0,
        )?;
        let cursor = journal.commit(&[FlowFact::Created(Box::new(descriptor.clone()))])?;
        let state = descriptor
            .content
            .initial_state(descriptor.address.clone())?;
        let journal = FlowJournal {
            inner: journal,
            dir: dir.to_path_buf(),
            history_only: matches!(descriptor.content, FlowContent::History(_)),
            activity: None,
        };
        Ok(Self {
            dir: dir.into(),
            _owner: owner,
            project,
            journal,
            descriptor,
            state,
            live_turn_clock: None,
            closed: None,
            inbox: Inbox::default(),
            outbox: outbox::Outbox::default(),
            publications: publication::Publications::default(),
            subscriptions: subscription::Subscriptions::default(),
            process_watches: process_watch::ProcessWatches::default(),
            timers: timer::Timers::default(),
            task_scopes: task_scope::TaskScopes::default(),
            watch_replacements: watch_edit::WatchReplacements::default(),
            activation: None,
            workspace: None,
            completions: Default::default(),
            checkpoints: Default::default(),
            names: Default::default(),
            goals: Default::default(),
            planning: Default::default(),
            controls: Default::default(),
            trace: Default::default(),
            history: Default::default(),
            cursor,
            projection_error: None,
            recovered_tail: None,
        })
    }

    fn open(
        dir: &Path,
        owner: Arc<SessionOwner>,
        address: &FlowRef,
        project: Option<crate::storage::ProjectContext>,
    ) -> io::Result<Self> {
        let mut replay = replay::FlowReplay::default();
        let (journal, recovered_tail) = Journal::open(
            &dir.join("events.jsonl"),
            address.instance_id.0.0,
            |receipt, facts| replay.apply(dir, address, receipt, facts),
        )?;
        let replay::FlowReplay {
            projection,
            outbox,
            publications,
            subscriptions,
            process_watches,
            timers,
            task_scopes,
            watch_replacements,
            activation,
            workspace,
            closed,
            completions,
            checkpoints,
            names,
            goals,
            planning,
            controls,
            trace,
            history,
            cursor,
            activity,
            context_window: _,
        } = replay;
        let (descriptor, state, inbox) =
            projection.ok_or_else(|| invalid("flow creation was not committed"))?;
        let journal = FlowJournal {
            inner: journal,
            dir: dir.to_path_buf(),
            history_only: matches!(descriptor.content, FlowContent::History(_)),
            activity,
        };
        let mut store = Self {
            dir: dir.into(),
            _owner: owner,
            project,
            journal,
            descriptor,
            state,
            live_turn_clock: None,
            closed,
            inbox,
            outbox,
            publications,
            subscriptions,
            process_watches,
            timers,
            task_scopes,
            watch_replacements,
            activation,
            workspace,
            completions,
            checkpoints,
            names,
            goals,
            planning,
            controls,
            trace,
            history,
            cursor: cursor.ok_or_else(|| invalid("empty flow journal"))?,
            projection_error: None,
            recovered_tail,
        };
        store.refresh_projection();
        Ok(store)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn descriptor(&self) -> &FlowDescriptor {
        &self.descriptor
    }
    pub fn state(&self) -> &FlowInstance<AtmanUuid> {
        &self.state
    }
    pub fn cursor(&self) -> &CommitReceipt {
        &self.cursor
    }
    pub fn take_projection_error(&mut self) -> Option<io::Error> {
        self.projection_error.take()
    }

    pub fn inbox_entries(&self) -> &[InboxSummary] {
        self.inbox.entries()
    }

    pub fn pending_inputs(&self) -> impl Iterator<Item = &InputEnvelope> {
        self.inbox.pending()
    }

    /// Pending entries in delivery order, including their current edit revisions.
    pub fn pending_input_entries(&self) -> impl Iterator<Item = &InboxSummary> {
        self.inbox.pending_entries()
    }

    /// Authenticate the sender before querying an original request receipt.
    pub(crate) fn input_acceptance(
        &self,
        request: &DeliveryRequest,
    ) -> io::Result<Option<Acceptance>> {
        if request.recipient != self.descriptor.address {
            return Err(invalid("delivery targets another flow"));
        }
        self.inbox.previous(request)
    }

    /// Authenticate the sender before this call; authorize only new delivery after checking accepted retries.
    pub fn accept_input(
        &mut self,
        request: DeliveryRequest,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()>,
    ) -> io::Result<Acceptance> {
        if let Some(previous) = self.input_acceptance(&request)? {
            return Ok(previous);
        }
        if !self.state.accepts_input() {
            if matches!(
                self.state.state(),
                InstanceState::Closing { .. } | InstanceState::Closed
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    outbox::RecipientRejected(outbox::DeliveryFailure::RecipientClosed {
                        observed: self.cursor.clone(),
                    }),
                ));
            }
            return Err(invalid("flow does not accept new input"));
        }
        request.validate_source()?;
        continuation::validate_request(&request, &self.state, self.goals.current())?;
        authorize_new(&request)?;
        let envelope = AcceptedEnvelope {
            id: Uuid::now_v7(),
            accepted_at: chrono::Utc::now(),
            request,
        };
        let prepared = self.inbox.validate(&envelope)?;
        let mut candidate = self.state.clone();
        let recover = envelope.request.resumes_interruption()
            && matches!(
                candidate.state(),
                InstanceState::Open(atman_rt::InstanceActivity::Interrupted { .. })
            );
        let mut facts = vec![FlowFact::EnvelopeAccepted(Box::new(envelope.clone()))];
        if recover {
            candidate.recover_interrupted().map_err(invalid)?;
            facts.push(FlowFact::Transition(FlowTransition::InterruptionRecovered));
        }
        let receipt = self.journal.commit(&facts)?;
        let acceptance = self.inbox.apply(prepared, receipt.clone());
        if recover {
            self.publish_state(candidate, receipt);
        } else {
            self.cursor = receipt;
        }
        // Inbox bodies stay in the journal and its in-memory projection, not the metadata cache.
        Ok(acceptance)
    }

    /// Apply one authorized transition to a private candidate, then publish it only after durable commit.
    pub fn transition(
        &mut self,
        expected_revision: u64,
        mut transition: FlowTransition,
    ) -> io::Result<CommitReceipt> {
        if expected_revision != self.state.revision() {
            return Err(invalid("stale flow state revision"));
        }
        if let FlowTransition::TurnFinished { lease, outcome } = transition {
            return self
                .finish_turn(lease, completion::InvocationResult::Unobserved(outcome))
                .map(|record| record.receipt);
        }
        let mut candidate = self.state.clone();
        transition.apply(&mut candidate)?;
        if candidate == self.state {
            return Ok(self.cursor.clone());
        }
        let cursor = self.journal.commit(&[FlowFact::Transition(transition)])?;
        self.publish_state(candidate, cursor.clone());
        Ok(cursor)
    }

    fn publish_state(&mut self, candidate: FlowInstance<AtmanUuid>, cursor: CommitReceipt) {
        self.install_state(candidate);
        if matches!(self.state.state(), InstanceState::Closed) {
            self.closed = Some(cursor.clone());
        }
        self.inbox.state_changed(&self.state, &cursor);
        if matches!(
            self.state.state(),
            InstanceState::Closing { .. } | InstanceState::Closed
        ) {
            self.inbox.close(&cursor);
            self.subscriptions.close();
            self.process_watches.close();
            self.timers.close();
            self.task_scopes.close(&cursor);
        }
        self.cursor = cursor;
        self.refresh_projection();
    }

    /// Record loss of an admitted invocation without rerunning its unknown external effects.
    pub fn interrupt_lost_turn(&mut self) -> io::Result<Option<CommitReceipt>> {
        let lease = match self.state.state() {
            InstanceState::Open(InstanceActivity::Active(active))
            | InstanceState::Closing {
                active: Some(active),
            } => active.lease.clone(),
            _ => return Ok(None),
        };
        self.transition(
            self.state.revision(),
            FlowTransition::TurnFinished {
                lease,
                outcome: TurnOutcome::Interrupted(Interruption::HostLost),
            },
        )
        .map(Some)
    }

    fn refresh_projection(&mut self) {
        // Source bodies remain in the creation fact; metadata contains only the entry reference.
        let value = serde_json::json!({ "address": self.descriptor.address, "content": self.descriptor.content.summary(), "history": self.history, "creation_request": self.descriptor.creation_request, "created_by": self.descriptor.created_by, "state": self.state.state(), "revision": self.state.revision(), "pauses": self.state.pauses(), "workspace": self.workspace });
        self.projection_error =
            write_projection(&self.dir.join("flow.json"), &self.cursor, &value).err();
    }
}

fn write_projection(path: &Path, cursor: &CommitReceipt, value: &impl Serialize) -> io::Result<()> {
    #[derive(Serialize)]
    struct Projection<'a, T> {
        schema: u32,
        cursor: &'a CommitReceipt,
        value: &'a T,
    }
    write_json(
        path,
        &Projection {
            schema: 1,
            cursor,
            value,
        },
    )
}

fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(invalid)?;
    let tmp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = private_file_options()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

/// Initialize privately before publishing a directory. The caller owns its namespace lock.
fn initialize_dir(path: &Path, initialize: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<()> {
    if path.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "storage directory already exists",
        ));
    }
    let temporary = path.with_file_name(format!(".prepare-{}", Uuid::now_v7()));
    create_dir(&temporary)?;
    let result = (|| {
        initialize(&temporary)?;
        std::fs::rename(&temporary, path)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(temporary);
    }
    result
}

fn create_dir(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    sync_parent(path)
}

fn ensure_dir(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    match create_dir(path) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        result => result,
    }
}

pub(crate) fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
