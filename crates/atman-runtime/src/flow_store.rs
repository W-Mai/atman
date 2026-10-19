//! Version-isolated session containers and persistent flow instances.
//!
//! Host-only storage: authorization and scheduling happen before commands reach this module. Each flow has one authoritative journal. Metadata files are disposable projections and never override facts. Blocking calls belong on storage workers.

pub mod access;
pub mod completion;
pub mod context;
pub mod continuation;
mod deletion;
pub mod goal;
pub mod history;
pub mod inbox;
mod input;
pub mod launch;
pub mod migration;
pub mod outbox;
pub mod planning;
pub mod process_watch;
pub mod profile;
pub mod publication;
mod replay;
pub mod subscription;
pub mod trace;
pub mod view;
pub mod waiter;
pub mod workspace;

pub(crate) use deletion::ProjectDeletion;

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
    Acceptance, AcceptedEnvelope, CommittedInput, DeliveryRequest, Inbox, InboxEntry, InputContent,
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
        crate::source_program::SourceRoots {
            project_root: self
                .project
                .as_ref()
                .map(|project| project.root().to_path_buf()),
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
    LegacyRecords(Vec<history::LegacyHistoryRecord>),
    ExecutionEvents(Vec<trace::ExecutionEvent>),
    Created(Box<FlowDescriptor>),
    Activated(Box<launch::Activation>),
    WorkspaceRequested(Box<workspace::WorkspaceRequest>),
    Transition(FlowTransition),
    TurnCompleted(Box<completion::Completion>),
    EnvelopeAccepted(Box<AcceptedEnvelope>),
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
    FlowSubscribed(Box<subscription::SubscriptionRecord>),
    FlowUnsubscribed(Uuid),
    EventsObserved(Box<subscription::Observation>),
    FlowWatchesExpired(Box<subscription::FlowWatchExpiry>),
    ProcessWatchRegistered(Box<process_watch::ProcessWatchRecord>),
    ProcessWatchCancelled(Uuid),
    ProcessObserved(Box<process_watch::ProcessObservation>),
    ProcessWatchesExpired(Box<process_watch::ProcessWatchExpiry>),
    DeliveryConfirmed {
        request_id: Uuid,
        delivery: outbox::DeliveryReceipt,
    },
}

/// All mutation paths share the content boundary, including input commit closures.
struct FlowJournal {
    inner: Journal,
    history_only: bool,
}

impl FlowJournal {
    fn validate(history_only: bool, facts: &[FlowFact]) -> io::Result<()> {
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
        self.inner.commit(facts)
    }

    fn read_after(
        &mut self,
        after: Option<&CommitReceipt>,
        limit: std::num::NonZeroUsize,
    ) -> io::Result<crate::event_log::journal::JournalPage<FlowFact>> {
        self.inner.read_after(after, limit)
    }
}

impl FlowFact {
    fn validate_batch(facts: &[Self]) -> io::Result<()> {
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
            return if matches!(facts, [Self::TurnCompleted(_)]) {
                Ok(())
            } else {
                Err(invalid(
                    "turn result and terminal state require one dedicated fact",
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
                    | Self::ProcessObserved(_)
                    | Self::ProcessWatchesExpired(_)
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
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionFact {
    Created(SessionDescriptor),
    TitleChanged(Option<String>),
    FlowRegistered(FlowRegistration),
    DefaultFlowSelected(FlowInstanceId),
    AccessGranted(access::FlowGrant),
    AccessRevoked(Uuid),
    LegacyImported(migration::LegacyMigration),
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionProjection {
    pub descriptor: SessionDescriptor,
    /// Mutable display name; the creation descriptor remains unchanged.
    pub title: Option<String>,
    pub flows: Vec<FlowRegistration>,
    pub default_flow: Option<FlowInstanceId>,
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
                        descriptor,
                        flows: Vec::new(),
                        default_flow: None,
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
            SessionFact::FlowRegistered(registration) => {
                if self.flows.iter().any(|flow| {
                    flow.id == registration.id
                        || flow.creation_request == registration.creation_request
                }) {
                    return Err(invalid("duplicate flow registration"));
                }
                self.flows.push(registration);
            }
            SessionFact::DefaultFlowSelected(id) => {
                if !self.flows.iter().any(|flow| flow.id == id) {
                    return Err(invalid("default flow is not registered"));
                }
                self.default_flow = Some(id);
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
    pub descriptor: FlowDescriptor,
    pub state: FlowInstance<AtmanUuid>,
    pub journal: crate::event_log::journal::JournalInspection,
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
}

fn inspect_registered_flow(
    dir: &Path,
    catalog: &SessionProjection,
    id: &FlowInstanceId,
) -> io::Result<FlowInspection> {
    inspect_registered_flow_with(dir, catalog, id, |_, _| Ok(()))
}

fn inspect_registered_flow_with(
    dir: &Path,
    catalog: &SessionProjection,
    id: &FlowInstanceId,
    mut observe: impl FnMut(&CommitReceipt, &[FlowFact]) -> io::Result<()>,
) -> io::Result<FlowInspection> {
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
    let journal = Journal::inspect(
        &dir.join("flows").join(id.to_string()).join("events.jsonl"),
        id.0.0,
        |receipt, facts| {
            observe(&receipt, &facts)?;
            replay.apply(&address, receipt, facts)
        },
    )?;
    let (descriptor, state, _) = replay
        .projection
        .ok_or_else(|| invalid("flow creation was not committed"))?;
    if descriptor.creation_request != registration.creation_request {
        return Err(invalid("flow creation does not match session registration"));
    }
    Ok(FlowInspection {
        descriptor,
        state,
        journal,
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
            {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Read recorded catalog facts without repairing logs, rewriting metadata, or recovering execution. The source project and configuration need not exist. Use the owning host to observe acknowledged live state.
    pub fn inspect_session(data_dir: &Path, id: Uuid) -> io::Result<SessionInspection> {
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
        let dir = self.root.join("sessions").join(descriptor.id.to_string());
        initialize_dir(&dir, |dir| {
            self.create_session_at(dir, descriptor.clone()).map(drop)
        })?;
        self.open_session_locked(descriptor.id)
    }

    fn lock_catalog(&self) -> io::Result<File> {
        let file = private_file_options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(".catalog.lock"))?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(file)
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
                descriptor,
                flows: Vec::new(),
                default_flow: None,
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
    _lock: File,
    descriptor: SessionDescriptor,
}

impl SessionOwner {
    fn lock(dir: &Path) -> io::Result<File> {
        let mut options = private_file_options();
        let file = options
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(".owner.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file)?;
        Ok(file)
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
        let mut flow = match initialize_dir(&dir, |dir| {
            FlowStore::create(dir, self.owner.clone(), descriptor.clone()).map(drop)
        }) {
            Ok(()) => FlowStore::open(&dir, self.owner.clone(), &descriptor.address)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // A crash between flow creation and container registration leaves a prepared orphan.
                let store = FlowStore::open(&dir, self.owner.clone(), &descriptor.address)?;
                if store.descriptor != descriptor || store.cursor.sequence != 1 {
                    return Err(invalid(
                        "unregistered flow does not match the prepared creation",
                    ));
                }
                store
            }
            Err(error) => return Err(error),
        };
        let mut facts = vec![SessionFact::FlowRegistered(FlowRegistration {
            id: id.clone(),
            creation_request: descriptor.creation_request,
        })];
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
        )?;
        if store.descriptor.creation_request != registration.creation_request {
            return Err(invalid("flow creation does not match session registration"));
        }
        Ok(store)
    }

    pub fn select_default(&mut self, id: FlowInstanceId) -> io::Result<CommitReceipt> {
        if self.projection.default_flow.as_ref() == Some(&id) {
            return Ok(self.cursor.clone());
        }
        self.commit(vec![SessionFact::DefaultFlowSelected(id)])
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
    journal: FlowJournal,
    descriptor: FlowDescriptor,
    state: FlowInstance<AtmanUuid>,
    closed: Option<CommitReceipt>,
    inbox: Inbox,
    outbox: outbox::Outbox,
    publications: publication::Publications,
    subscriptions: subscription::Subscriptions,
    process_watches: process_watch::ProcessWatches,
    activation: Option<(launch::Activation, CommitReceipt)>,
    workspace: Option<workspace::WorkspaceRequest>,
    completions: completion::Completions,
    checkpoints: context::Checkpoints,
    goals: goal::Goals,
    planning: planning::Planning,
    trace: trace::ExecutionTrace,
    history: history::HistoryProgress,
    cursor: CommitReceipt,
    projection_error: Option<io::Error>,
    pub recovered_tail: Option<RecoveredTail>,
}

impl FlowStore {
    pub fn session_descriptor(&self) -> &SessionDescriptor {
        &self._owner.descriptor
    }

    fn create(
        dir: &Path,
        owner: Arc<SessionOwner>,
        descriptor: FlowDescriptor,
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
            history_only: matches!(descriptor.content, FlowContent::History(_)),
        };
        Ok(Self {
            dir: dir.into(),
            _owner: owner,
            journal,
            descriptor,
            state,
            closed: None,
            inbox: Inbox::default(),
            outbox: outbox::Outbox::default(),
            publications: publication::Publications::default(),
            subscriptions: subscription::Subscriptions::default(),
            process_watches: process_watch::ProcessWatches::default(),
            activation: None,
            workspace: None,
            completions: Default::default(),
            checkpoints: Default::default(),
            goals: Default::default(),
            planning: Default::default(),
            trace: Default::default(),
            history: Default::default(),
            cursor,
            projection_error: None,
            recovered_tail: None,
        })
    }

    fn open(dir: &Path, owner: Arc<SessionOwner>, address: &FlowRef) -> io::Result<Self> {
        let mut replay = replay::FlowReplay::default();
        let (journal, recovered_tail) = Journal::open(
            &dir.join("events.jsonl"),
            address.instance_id.0.0,
            |receipt, facts| replay.apply(address, receipt, facts),
        )?;
        let replay::FlowReplay {
            projection,
            outbox,
            publications,
            subscriptions,
            process_watches,
            activation,
            workspace,
            closed,
            completions,
            checkpoints,
            goals,
            planning,
            trace,
            history,
            cursor,
        } = replay;
        let (descriptor, state, inbox) =
            projection.ok_or_else(|| invalid("flow creation was not committed"))?;
        let journal = FlowJournal {
            inner: journal,
            history_only: matches!(descriptor.content, FlowContent::History(_)),
        };
        let mut store = Self {
            dir: dir.into(),
            _owner: owner,
            journal,
            descriptor,
            state,
            closed,
            inbox,
            outbox,
            publications,
            subscriptions,
            process_watches,
            activation,
            workspace,
            completions,
            checkpoints,
            goals,
            planning,
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

    pub fn inbox_entries(&self) -> &[InboxEntry] {
        self.inbox.entries()
    }

    pub fn pending_inputs(&self) -> impl Iterator<Item = &AcceptedEnvelope> {
        self.inbox.pending()
    }

    /// Authenticate the sender before this call; authorize only new delivery after checking accepted retries.
    pub fn accept_input(
        &mut self,
        request: DeliveryRequest,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()>,
    ) -> io::Result<Acceptance> {
        if request.recipient != self.descriptor.address {
            return Err(invalid("delivery targets another flow"));
        }
        if let Some(previous) = self.inbox.previous(&request)? {
            return Ok(previous);
        }
        if !self.state.accepts_input() {
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
        self.inbox.validate(&envelope)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::EnvelopeAccepted(Box::new(envelope.clone()))])?;
        let acceptance = self.inbox.apply(envelope, receipt.clone());
        self.cursor = receipt;
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
        self.state = candidate;
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
        }
        self.cursor = cursor.clone();
        self.refresh_projection();
        Ok(cursor)
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
