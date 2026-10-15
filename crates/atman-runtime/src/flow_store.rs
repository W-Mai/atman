//! Version-isolated session containers and persistent flow instances.
//!
//! Host-only storage: authorization and scheduling happen before commands reach this module. Each flow has one authoritative journal. Metadata files are disposable projections and never override facts. Blocking calls belong on storage workers.

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

pub type FlowInstanceId = atman_rt::FlowInstanceId<AtmanUuid>;
pub type FlowRef = atman_rt::FlowRef<AtmanUuid>;

const NAMESPACE: &str = "runtime-v1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionDescriptor {
    pub id: Uuid,
    pub project_root: Option<PathBuf>,
    pub title: String,
}

/// Recorded source closure; reading history never requires the original project directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EntrySnapshot {
    pub name: String,
    pub path: PathBuf,
    pub bundle: RevisionBundle,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlowDescriptor {
    pub address: FlowRef,
    pub creation_request: Uuid,
    pub entry: EntrySnapshot,
    /// Audit provenance only; this does not confer control or propagate cancellation.
    pub created_by: Option<FlowRef>,
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

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum FlowFact {
    Created(Box<FlowDescriptor>),
    Transition(FlowTransition),
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
    FlowRegistered(FlowRegistration),
    DefaultFlowSelected(FlowInstanceId),
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionProjection {
    pub descriptor: SessionDescriptor,
    pub flows: Vec<FlowRegistration>,
    pub default_flow: Option<FlowInstanceId>,
}

impl SessionProjection {
    fn apply(&mut self, fact: SessionFact) -> io::Result<()> {
        match fact {
            SessionFact::Created(_) => return Err(invalid("duplicate session creation")),
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
        }
        Ok(())
    }
}

/// The namespace never overlaps published flat session logs.
pub struct RuntimeStore {
    root: PathBuf,
}

impl RuntimeStore {
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let root = std::path::absolute(data_dir)?.join(NAMESPACE);
        ensure_dir(&root.join("sessions"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create_session(&self, descriptor: SessionDescriptor) -> io::Result<SessionStore> {
        let dir = self.root.join("sessions").join(descriptor.id.to_string());
        create_dir(&dir)?;
        let owner = Arc::new(SessionOwner::acquire(&dir)?);
        ensure_dir(&dir.join("flows"))?;
        ensure_dir(&dir.join("artifacts"))?;
        let mut journal = Journal::create(&dir.join("events.jsonl"), descriptor.id)?;
        let cursor = journal.commit(&[SessionFact::Created(descriptor.clone())])?;
        let mut store = SessionStore {
            dir,
            owner,
            journal,
            projection: SessionProjection {
                descriptor,
                flows: Vec::new(),
                default_flow: None,
            },
            cursor,
            projection_error: None,
            recovered_tail: None,
        };
        store.refresh_projection();
        Ok(store)
    }

    pub fn open_session(&self, id: Uuid) -> io::Result<SessionStore> {
        let dir = self.root.join("sessions").join(id.to_string());
        let owner = Arc::new(SessionOwner::acquire(&dir)?);
        let mut projection: Option<SessionProjection> = None;
        let mut cursor = None;
        let (journal, recovered_tail) =
            Journal::open(&dir.join("events.jsonl"), id, |receipt, facts| {
                for fact in facts {
                    match (&mut projection, fact) {
                        (None, SessionFact::Created(descriptor)) if descriptor.id == id => {
                            projection = Some(SessionProjection {
                                descriptor,
                                flows: Vec::new(),
                                default_flow: None,
                            });
                        }
                        (Some(state), fact) => state.apply(fact)?,
                        _ => return Err(invalid("missing or mismatched session creation")),
                    }
                }
                cursor = Some(receipt);
                Ok(())
            })?;
        let mut store = SessionStore {
            dir,
            owner,
            journal,
            projection: projection.ok_or_else(|| invalid("session creation was not committed"))?,
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
}

impl SessionOwner {
    fn acquire(dir: &Path) -> io::Result<Self> {
        let mut options = private_file_options();
        let file = options
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(".owner.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file)?;
        Ok(Self { _lock: file })
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
        if descriptor.address.session_id.0 != self.projection.descriptor.id {
            return Err(invalid("flow belongs to another session"));
        }
        let id = descriptor.address.instance_id.clone();
        let existing = self
            .projection
            .flows
            .iter()
            .find(|flow| flow.id == id || flow.creation_request == descriptor.creation_request);
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
        let mut flow = match create_dir(&dir) {
            Ok(()) => FlowStore::create(&dir, self.owner.clone(), descriptor.clone())?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // A crash between flow creation and container registration leaves a prepared orphan.
                let store = FlowStore::open(&dir, self.owner.clone(), &descriptor.address)?;
                if store.descriptor != descriptor || store.state.state() != &InstanceState::Prepared
                {
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
    journal: Journal,
    descriptor: FlowDescriptor,
    state: FlowInstance<AtmanUuid>,
    cursor: CommitReceipt,
    projection_error: Option<io::Error>,
    pub recovered_tail: Option<RecoveredTail>,
}

impl FlowStore {
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
        let state = FlowInstance::prepared(descriptor.address.clone());
        Ok(Self {
            dir: dir.into(),
            _owner: owner,
            journal,
            descriptor,
            state,
            cursor,
            projection_error: None,
            recovered_tail: None,
        })
    }

    fn open(dir: &Path, owner: Arc<SessionOwner>, address: &FlowRef) -> io::Result<Self> {
        let mut projection: Option<(FlowDescriptor, FlowInstance<AtmanUuid>)> = None;
        let mut cursor = None;
        let (journal, recovered_tail) = Journal::open(
            &dir.join("events.jsonl"),
            address.instance_id.0.0,
            |receipt, facts| {
                for fact in facts {
                    match (&mut projection, fact) {
                        (None, FlowFact::Created(descriptor)) if &descriptor.address == address => {
                            projection =
                                Some((*descriptor, FlowInstance::prepared(address.clone())));
                        }
                        (Some((_, state)), FlowFact::Transition(mut transition)) => {
                            let original = transition.clone();
                            let revision = state.revision();
                            transition.apply(state)?;
                            if original != transition || revision == state.revision() {
                                return Err(invalid("noncanonical flow transition"));
                            }
                        }
                        _ => {
                            return Err(invalid("missing, duplicate, or mismatched flow creation"));
                        }
                    }
                }
                cursor = Some(receipt);
                Ok(())
            },
        )?;
        let (descriptor, state) =
            projection.ok_or_else(|| invalid("flow creation was not committed"))?;
        let mut store = Self {
            dir: dir.into(),
            _owner: owner,
            journal,
            descriptor,
            state,
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

    /// Apply one authorized transition to a private candidate, then publish it only after durable commit.
    pub fn transition(
        &mut self,
        expected_revision: u64,
        mut transition: FlowTransition,
    ) -> io::Result<CommitReceipt> {
        if expected_revision != self.state.revision() {
            return Err(invalid("stale flow state revision"));
        }
        let mut candidate = self.state.clone();
        transition.apply(&mut candidate)?;
        if candidate == self.state {
            return Ok(self.cursor.clone());
        }
        let cursor = self.journal.commit(&[FlowFact::Transition(transition)])?;
        self.state = candidate;
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
        let value = serde_json::json!({ "address": self.descriptor.address, "entry": self.descriptor.entry.name, "creation_request": self.descriptor.creation_request, "created_by": self.descriptor.created_by, "state": self.state.state(), "revision": self.state.revision(), "pauses": self.state.pauses() });
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
    let bytes = serde_json::to_vec(&Projection {
        schema: 1,
        cursor,
        value,
    })
    .map_err(invalid)?;
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

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
