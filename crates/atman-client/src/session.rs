use std::sync::Arc;

use atman_proto::{
    CancelRunRequest, CancelRunResponse, DaemonGeneration, EventCursor, FlowRunId, FormSubmission,
    GetSessionSnapshotRequest, GetSessionUpdatesRequest, GetSessionUpdatesResponse, InlineImage,
    InterjectSessionRequest, InterjectSessionResponse, InterjectionLevel,
    PROJECTION_EVENT_SCHEMA_VERSION, ProjectionChange, ProjectionDelta, ProjectionEventEnvelope,
    RequestId, Revision, SNAPSHOT_SCHEMA_VERSION, SendMessageRequest, SendMessageResponse,
    ServerEvent, SessionId, SessionProjection, SessionSignal, SessionSnapshot, SubmitFormRequest,
    SubmitFormResponse, rpc,
};
use futures::StreamExt;
use tokio::sync::{Mutex, broadcast, watch};

use crate::{Client, ClientError, TransportError};

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const MIN_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ReconcileError {
    #[error("snapshot schema version {received} is incompatible with client version {expected}")]
    SnapshotSchema { expected: u32, received: u32 },
    #[error(
        "projection event schema version {received} is incompatible with client version {expected}"
    )]
    EventSchema { expected: u32, received: u32 },
    #[error("daemon generation changed from {expected:?} to {received:?}")]
    DaemonGeneration {
        expected: DaemonGeneration,
        received: DaemonGeneration,
    },
    #[error("session update belongs to {received}, expected {expected}")]
    Session {
        expected: SessionId,
        received: SessionId,
    },
    #[error("session update cursor jumped from {current:?} to {received:?}")]
    CursorGap {
        current: EventCursor,
        received: EventCursor,
    },
    #[error("update page ended at cursor {actual:?}, but declared {declared:?}")]
    PageCursor {
        actual: EventCursor,
        declared: EventCursor,
    },
    #[error("projection delta expected revision {expected:?}, received base {received:?}")]
    RevisionBase {
        expected: Revision,
        received: Revision,
    },
    #[error("projection delta must advance revision {base:?} by one, received {received:?}")]
    RevisionStep { base: Revision, received: Revision },
    #[error("daemon requires a fresh session snapshot: {0:?}")]
    ResyncRequired(atman_proto::ResyncRequired),
}

impl ReconcileError {
    fn requires_resync(&self) -> bool {
        matches!(
            self,
            Self::CursorGap { .. }
                | Self::PageCursor { .. }
                | Self::RevisionBase { .. }
                | Self::RevisionStep { .. }
                | Self::ResyncRequired(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionState {
    snapshot: SessionSnapshot,
}

impl SessionState {
    pub fn new(
        snapshot: SessionSnapshot,
        expected_generation: &DaemonGeneration,
    ) -> Result<Self, ReconcileError> {
        validate_snapshot(&snapshot, expected_generation)?;
        Ok(Self { snapshot })
    }

    pub fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    pub fn projection(&self) -> &SessionProjection {
        &self.snapshot.projection
    }

    pub fn cursor(&self) -> EventCursor {
        self.snapshot.cursor
    }

    pub fn apply_updates(
        &mut self,
        response: &GetSessionUpdatesResponse,
    ) -> Result<AppliedUpdates, ReconcileError> {
        if response.daemon_generation != self.snapshot.daemon_generation {
            return Err(ReconcileError::DaemonGeneration {
                expected: self.snapshot.daemon_generation.clone(),
                received: response.daemon_generation.clone(),
            });
        }
        if let Some(gap) = &response.resync_required {
            return Err(ReconcileError::ResyncRequired(gap.clone()));
        }
        let starting_cursor = self.snapshot.cursor;
        let mut next = self.clone();
        let mut applied = 0;
        let mut signals = Vec::new();
        for envelope in &response.events {
            if envelope.cursor <= next.snapshot.cursor {
                continue;
            }
            apply_envelope(&mut next.snapshot, envelope, &mut signals)?;
            applied += 1;
        }
        if response.next_cursor > next.snapshot.cursor
            || (response.next_cursor > starting_cursor
                && response.next_cursor != next.snapshot.cursor)
        {
            return Err(ReconcileError::PageCursor {
                actual: next.snapshot.cursor,
                declared: response.next_cursor,
            });
        }
        *self = next;
        Ok(AppliedUpdates {
            applied,
            signals,
            has_more: response.has_more,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppliedUpdates {
    pub applied: usize,
    pub signals: Vec<SessionSignal>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RefreshOutcome {
    Applied {
        events: usize,
        signals: Vec<SessionSignal>,
        has_more: bool,
    },
    Resynced,
    Reconnected,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionClientError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Reconcile(#[from] ReconcileError),
    #[error("daemon acknowledged cursor {target:?}, but the session remained at {current:?}")]
    CommittedCursorUnavailable {
        target: EventCursor,
        current: EventCursor,
    },
    #[error("command result belongs to session {received}, expected {expected}")]
    CommandSession {
        expected: SessionId,
        received: SessionId,
    },
    #[error("command result belongs to run {received}, expected {expected}")]
    CommandRun {
        expected: FlowRunId,
        received: FlowRunId,
    },
    #[error("command result belongs to form {received}, expected {expected}")]
    CommandForm { expected: String, received: String },
}

impl SessionClientError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Client(error) => error.is_retryable(),
            Self::Transport(error) => error.is_retryable(),
            Self::Reconcile(_) => false,
            Self::CommittedCursorUnavailable { .. } => false,
            Self::CommandSession { .. } | Self::CommandRun { .. } | Self::CommandForm { .. } => {
                false
            }
        }
    }
}

#[derive(Clone)]
pub struct SessionClient {
    client: Client,
    session_id: SessionId,
    state: watch::Sender<SessionState>,
    signals: broadcast::Sender<SessionSignal>,
    refresh_lock: Arc<Mutex<()>>,
}

impl SessionClient {
    pub(crate) async fn attach(
        client: Client,
        session_id: SessionId,
    ) -> Result<Self, SessionClientError> {
        let snapshot = client
            .call::<rpc::GetSessionSnapshot>(&GetSessionSnapshotRequest {
                session_id: session_id.clone(),
            })
            .await?;
        validate_session(&snapshot, &session_id)?;
        let capabilities = client.capabilities();
        let state = SessionState::new(snapshot, &capabilities.daemon_generation)?;
        let (state, _) = watch::channel(state);
        let (signals, _) = broadcast::channel(capabilities.limits.subscriber_buffer.max(1));
        Ok(Self {
            client,
            session_id,
            state,
            signals,
            refresh_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn current(&self) -> SessionState {
        self.state.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<SessionState> {
        self.state.subscribe()
    }

    pub fn subscribe_signals(&self) -> broadcast::Receiver<SessionSignal> {
        self.signals.subscribe()
    }

    pub async fn refresh(&self) -> Result<RefreshOutcome, SessionClientError> {
        let _guard = self.refresh_lock.lock().await;
        let current = self.current();
        let capabilities = self.client.capabilities();
        let response = self
            .client
            .call::<rpc::GetSessionUpdates>(&GetSessionUpdatesRequest {
                session_id: self.session_id.clone(),
                after_cursor: current.cursor(),
                limit: Some(capabilities.limits.max_event_page_size),
            })
            .await?;
        self.reconcile_response(current, response).await
    }

    pub async fn apply_event(
        &self,
        event: ProjectionEventEnvelope,
    ) -> Result<RefreshOutcome, SessionClientError> {
        let _guard = self.refresh_lock.lock().await;
        let current = self.current();
        let response = GetSessionUpdatesResponse {
            daemon_generation: event.daemon_generation.clone(),
            next_cursor: event.cursor,
            events: vec![event],
            has_more: false,
            resync_required: None,
        };
        self.reconcile_response(current, response).await
    }

    async fn reconcile_response(
        &self,
        current: SessionState,
        response: GetSessionUpdatesResponse,
    ) -> Result<RefreshOutcome, SessionClientError> {
        let capabilities = self.client.capabilities();
        let mut next = current.clone();
        match next.apply_updates(&response) {
            Ok(applied) => {
                let outcome = RefreshOutcome::Applied {
                    events: applied.applied,
                    signals: applied.signals.clone(),
                    has_more: applied.has_more,
                };
                if next != current {
                    self.state.send_replace(next);
                }
                for signal in applied.signals {
                    let _ = self.signals.send(signal);
                }
                Ok(outcome)
            }
            Err(ReconcileError::DaemonGeneration { received, .. }) => {
                let capabilities = self.client.capabilities();
                let capabilities = if capabilities.daemon_generation == received {
                    capabilities
                } else {
                    self.client.refresh_capabilities().await?
                };
                let snapshot = self
                    .client
                    .call::<rpc::GetSessionSnapshot>(&GetSessionSnapshotRequest {
                        session_id: self.session_id.clone(),
                    })
                    .await?;
                validate_session(&snapshot, &self.session_id)?;
                let next = SessionState::new(snapshot, &capabilities.daemon_generation)?;
                self.state.send_replace(next);
                Ok(RefreshOutcome::Reconnected)
            }
            Err(error) if error.requires_resync() => {
                let snapshot = self
                    .client
                    .call::<rpc::GetSessionSnapshot>(&GetSessionSnapshotRequest {
                        session_id: self.session_id.clone(),
                    })
                    .await?;
                validate_session(&snapshot, &self.session_id)?;
                let next = SessionState::new(snapshot, &capabilities.daemon_generation)?;
                self.state.send_replace(next);
                Ok(RefreshOutcome::Resynced)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn refresh_until_current(&self) -> Result<RefreshOutcome, SessionClientError> {
        let mut total = 0;
        let mut signals = Vec::new();
        loop {
            match self.refresh().await? {
                RefreshOutcome::Applied {
                    events,
                    signals: page_signals,
                    has_more: true,
                } => {
                    total += events;
                    signals.extend(page_signals);
                }
                RefreshOutcome::Applied {
                    events,
                    signals: page_signals,
                    has_more: false,
                } => {
                    signals.extend(page_signals);
                    return Ok(RefreshOutcome::Applied {
                        events: total + events,
                        signals,
                        has_more: false,
                    });
                }
                RefreshOutcome::Resynced => return Ok(RefreshOutcome::Resynced),
                RefreshOutcome::Reconnected => return Ok(RefreshOutcome::Reconnected),
            }
        }
    }

    pub async fn send_message(
        &self,
        text: impl Into<String>,
        reasoning: Option<String>,
        images: Vec<InlineImage>,
    ) -> Result<SendMessageResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::SendMessage>(&SendMessageRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                text: text.into(),
                reasoning,
                images,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn interject(
        &self,
        run_id: FlowRunId,
        text: impl Into<String>,
        level: InterjectionLevel,
        redirect_target: Option<String>,
    ) -> Result<InterjectSessionResponse, SessionClientError> {
        let expected_run = run_id.clone();
        let response = self
            .client
            .command::<rpc::InterjectSession>(&InterjectSessionRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                run_id,
                text: text.into(),
                level,
                redirect_target,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        if response.run_id != expected_run {
            return Err(SessionClientError::CommandRun {
                expected: expected_run,
                received: response.run_id,
            });
        }
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn cancel_run(
        &self,
        run_id: FlowRunId,
    ) -> Result<CancelRunResponse, SessionClientError> {
        let expected_run = run_id.clone();
        let response = self
            .client
            .command::<rpc::CancelRun>(&CancelRunRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                run_id,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        if response.run_id != expected_run {
            return Err(SessionClientError::CommandRun {
                expected: expected_run,
                received: response.run_id,
            });
        }
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn submit_form(
        &self,
        form_id: impl Into<String>,
        submission: FormSubmission,
    ) -> Result<SubmitFormResponse, SessionClientError> {
        let form_id = form_id.into();
        let response = self
            .client
            .command::<rpc::SubmitForm>(&SubmitFormRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                form_id: form_id.clone(),
                submission,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        if response.form_id != form_id {
            return Err(SessionClientError::CommandForm {
                expected: form_id,
                received: response.form_id,
            });
        }
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    async fn refresh_through(&self, target: EventCursor) -> Result<(), SessionClientError> {
        let mut stalled = 0;
        while self.current().cursor() < target {
            let before = self.current().cursor();
            self.refresh().await?;
            let after = self.current().cursor();
            if after == before {
                stalled += 1;
                if stalled >= 2 {
                    return Err(SessionClientError::CommittedCursorUnavailable {
                        target,
                        current: after,
                    });
                }
            } else {
                stalled = 0;
            }
        }
        Ok(())
    }

    fn validate_command_session(&self, received: &SessionId) -> Result<(), SessionClientError> {
        if received != &self.session_id {
            return Err(SessionClientError::CommandSession {
                expected: self.session_id.clone(),
                received: received.clone(),
            });
        }
        Ok(())
    }

    pub async fn synchronize(&self) -> Result<(), SessionClientError> {
        let mut reconnect_delay = MIN_RECONNECT_DELAY;
        loop {
            match self.synchronize_connection().await {
                Ok(SyncConnection::Polled) => {
                    reconnect_delay = MIN_RECONNECT_DELAY;
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                Ok(SyncConnection::StreamEnded) => {
                    tokio::time::sleep(reconnect_delay).await;
                    reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                }
                Err(error) if error.is_retryable() => {
                    tokio::time::sleep(reconnect_delay).await;
                    reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn synchronize_connection(&self) -> Result<SyncConnection, SessionClientError> {
        self.refresh_until_current().await?;
        let Some(mut events) = self
            .client
            .session_events(self.session_id.clone(), self.current().cursor())
            .await?
        else {
            return Ok(SyncConnection::Polled);
        };
        while let Some(event) = events.next().await {
            self.apply_event(event?).await?;
        }
        Ok(SyncConnection::StreamEnded)
    }
}

enum SyncConnection {
    Polled,
    StreamEnded,
}

fn validate_snapshot(
    snapshot: &SessionSnapshot,
    expected_generation: &DaemonGeneration,
) -> Result<(), ReconcileError> {
    if snapshot.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(ReconcileError::SnapshotSchema {
            expected: SNAPSHOT_SCHEMA_VERSION,
            received: snapshot.schema_version,
        });
    }
    if &snapshot.daemon_generation != expected_generation {
        return Err(ReconcileError::DaemonGeneration {
            expected: expected_generation.clone(),
            received: snapshot.daemon_generation.clone(),
        });
    }
    Ok(())
}

fn validate_session(
    snapshot: &SessionSnapshot,
    expected: &SessionId,
) -> Result<(), ReconcileError> {
    if &snapshot.projection.metadata.id != expected {
        return Err(ReconcileError::Session {
            expected: expected.clone(),
            received: snapshot.projection.metadata.id.clone(),
        });
    }
    Ok(())
}

fn apply_envelope(
    snapshot: &mut SessionSnapshot,
    envelope: &ProjectionEventEnvelope,
    signals: &mut Vec<SessionSignal>,
) -> Result<(), ReconcileError> {
    if envelope.schema_version != PROJECTION_EVENT_SCHEMA_VERSION {
        return Err(ReconcileError::EventSchema {
            expected: PROJECTION_EVENT_SCHEMA_VERSION,
            received: envelope.schema_version,
        });
    }
    if envelope.daemon_generation != snapshot.daemon_generation {
        return Err(ReconcileError::DaemonGeneration {
            expected: snapshot.daemon_generation.clone(),
            received: envelope.daemon_generation.clone(),
        });
    }
    if envelope.session_id != snapshot.projection.metadata.id {
        return Err(ReconcileError::Session {
            expected: snapshot.projection.metadata.id.clone(),
            received: envelope.session_id.clone(),
        });
    }
    if let ServerEvent::ResyncRequired { gap } = &envelope.event {
        return Err(ReconcileError::ResyncRequired(gap.clone()));
    }
    let expected_cursor = EventCursor(snapshot.cursor.0.saturating_add(1));
    if envelope.cursor != expected_cursor {
        return Err(ReconcileError::CursorGap {
            current: snapshot.cursor,
            received: envelope.cursor,
        });
    }
    match &envelope.event {
        ServerEvent::ProjectionDelta { delta } => apply_delta(&mut snapshot.projection, delta)?,
        ServerEvent::Signal { signal } => signals.push(signal.clone()),
        ServerEvent::ResyncRequired { .. } => unreachable!("handled before cursor validation"),
        ServerEvent::Heartbeat => {}
    }
    snapshot.cursor = envelope.cursor;
    Ok(())
}

fn apply_delta(
    projection: &mut SessionProjection,
    delta: &ProjectionDelta,
) -> Result<(), ReconcileError> {
    if delta.base_revision != projection.revision {
        return Err(ReconcileError::RevisionBase {
            expected: projection.revision,
            received: delta.base_revision,
        });
    }
    if delta.revision.0 != delta.base_revision.0.saturating_add(1) {
        return Err(ReconcileError::RevisionStep {
            base: delta.base_revision,
            received: delta.revision,
        });
    }
    for change in &delta.changes {
        apply_change(projection, change);
    }
    projection.revision = delta.revision;
    Ok(())
}

fn apply_change(projection: &mut SessionProjection, change: &ProjectionChange) {
    match change {
        ProjectionChange::MetadataSet { metadata } => projection.metadata.clone_from(metadata),
        ProjectionChange::LifecycleSet { lifecycle } => projection.lifecycle = *lifecycle,
        ProjectionChange::RunUpsert { run } => upsert_run(&mut projection.runs, run.clone()),
        ProjectionChange::RunRemove { run_id } => projection.runs.retain(|run| &run.id != run_id),
        ProjectionChange::TranscriptAppend { items } => {
            projection.transcript.extend(items.iter().cloned())
        }
        ProjectionChange::TranscriptReplace { items } => projection.transcript.clone_from(items),
        ProjectionChange::WorkflowsReplace { workflows } => {
            projection.workflows.clone_from(workflows)
        }
        ProjectionChange::GoalSet { goal } => projection.goal.clone_from(goal),
        ProjectionChange::TodosReplace { todos } => projection.todos.clone_from(todos),
        ProjectionChange::PlansReplace { plans } => projection.plans.clone_from(plans),
        ProjectionChange::ContextSet { context } => projection.context.clone_from(context),
        ProjectionChange::InteractionsSet { interactions } => {
            projection.interactions.clone_from(interactions)
        }
        ProjectionChange::ResourceUpsert { resource } => {
            upsert_resource(&mut projection.resources, resource.clone())
        }
        ProjectionChange::ResourceRemove { resource_id } => projection
            .resources
            .retain(|resource| &resource.id != resource_id),
        ProjectionChange::UsageSet { usage } => projection.usage.clone_from(usage),
    }
}

fn upsert_run(runs: &mut Vec<atman_proto::RunProjection>, run: atman_proto::RunProjection) {
    if let Some(existing) = runs.iter_mut().find(|existing| existing.id == run.id) {
        *existing = run;
    } else {
        runs.push(run);
    }
}

fn upsert_resource(
    resources: &mut Vec<atman_proto::ResourceProjection>,
    resource: atman_proto::ResourceProjection,
) {
    if let Some(existing) = resources
        .iter_mut()
        .find(|existing| existing.id == resource.id)
    {
        *existing = resource;
    } else {
        resources.push(resource);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };

    use atman_proto::{
        CapabilitiesResponse, EVENT_SCHEMA_VERSION, JsonRpcRequest, JsonRpcResponse,
        MethodCapability, ProtocolLimits, method_descriptor, methods,
    };
    use futures::future::BoxFuture;

    use super::*;
    use crate::{ClientIdentity, RpcTransport, SessionEventStream, TransportError};

    fn projection(session_id: SessionId, revision: Revision) -> SessionProjection {
        SessionProjection {
            revision,
            metadata: atman_proto::SessionMetadataProjection {
                id: session_id,
                title: "Session".into(),
                name_source: atman_proto::NameSource::Auto,
                project_root: None,
                created_at: None,
                updated_at: None,
            },
            lifecycle: atman_proto::SessionLifecycle::Idle,
            runs: Vec::new(),
            transcript: Vec::new(),
            workflows: Vec::new(),
            goal: None,
            todos: Vec::new(),
            plans: Vec::new(),
            context: Default::default(),
            interactions: Default::default(),
            resources: Vec::new(),
            usage: Default::default(),
        }
    }

    fn state() -> SessionState {
        let generation = DaemonGeneration("generation-a".into());
        SessionState::new(
            SessionSnapshot {
                schema_version: SNAPSHOT_SCHEMA_VERSION,
                daemon_generation: generation.clone(),
                cursor: EventCursor(7),
                projection: projection(
                    serde_json::from_value(serde_json::json!(
                        "018f7f24-1ab2-7c3d-8e4f-123456789abc"
                    ))
                    .unwrap(),
                    Revision(3),
                ),
            },
            &generation,
        )
        .unwrap()
    }

    fn envelope(
        state: &SessionState,
        cursor: u64,
        delta: ProjectionDelta,
    ) -> ProjectionEventEnvelope {
        ProjectionEventEnvelope {
            schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
            daemon_generation: state.snapshot.daemon_generation.clone(),
            session_id: state.snapshot.projection.metadata.id.clone(),
            cursor: EventCursor(cursor),
            ts: serde_json::from_value(serde_json::json!("2026-01-01T00:00:00Z")).unwrap(),
            event: ServerEvent::ProjectionDelta { delta },
        }
    }

    #[test]
    fn exact_delta_applies_transactionally() {
        let mut state = state();
        let response = GetSessionUpdatesResponse {
            daemon_generation: state.snapshot.daemon_generation.clone(),
            events: vec![envelope(
                &state,
                8,
                ProjectionDelta {
                    base_revision: Revision(3),
                    revision: Revision(4),
                    changes: vec![ProjectionChange::GoalSet {
                        goal: Some("Converge".into()),
                    }],
                },
            )],
            next_cursor: EventCursor(8),
            has_more: false,
            resync_required: None,
        };

        let applied = state.apply_updates(&response).unwrap();
        assert_eq!(applied.applied, 1);
        assert_eq!(state.cursor(), EventCursor(8));
        assert_eq!(state.projection().revision, Revision(4));
        assert_eq!(state.projection().goal.as_deref(), Some("Converge"));
    }

    #[test]
    fn invalid_later_delta_rolls_back_the_entire_page() {
        let mut state = state();
        let original = state.clone();
        let response = GetSessionUpdatesResponse {
            daemon_generation: state.snapshot.daemon_generation.clone(),
            events: vec![
                envelope(
                    &state,
                    8,
                    ProjectionDelta {
                        base_revision: Revision(3),
                        revision: Revision(4),
                        changes: vec![ProjectionChange::GoalSet {
                            goal: Some("temporary".into()),
                        }],
                    },
                ),
                envelope(
                    &state,
                    9,
                    ProjectionDelta {
                        base_revision: Revision(3),
                        revision: Revision(4),
                        changes: Vec::new(),
                    },
                ),
            ],
            next_cursor: EventCursor(9),
            has_more: false,
            resync_required: None,
        };

        assert!(matches!(
            state.apply_updates(&response),
            Err(ReconcileError::RevisionBase { .. })
        ));
        assert_eq!(state, original);
    }

    #[test]
    fn cursor_gap_and_generation_change_do_not_mutate_state() {
        let mut state = state();
        let original = state.clone();
        let mut event = envelope(
            &state,
            9,
            ProjectionDelta {
                base_revision: Revision(3),
                revision: Revision(4),
                changes: Vec::new(),
            },
        );
        let response = GetSessionUpdatesResponse {
            daemon_generation: state.snapshot.daemon_generation.clone(),
            events: vec![event.clone()],
            next_cursor: EventCursor(9),
            has_more: false,
            resync_required: None,
        };
        assert!(matches!(
            state.apply_updates(&response),
            Err(ReconcileError::CursorGap { .. })
        ));
        assert_eq!(state, original);

        event.cursor = EventCursor(8);
        event.daemon_generation = DaemonGeneration("generation-b".into());
        let response = GetSessionUpdatesResponse {
            daemon_generation: state.snapshot.daemon_generation.clone(),
            events: vec![event],
            next_cursor: EventCursor(8),
            has_more: false,
            resync_required: None,
        };
        assert!(matches!(
            state.apply_updates(&response),
            Err(ReconcileError::DaemonGeneration { .. })
        ));
        assert_eq!(state, original);
    }

    #[test]
    fn resync_event_is_recognized_before_cursor_gap_validation() {
        let mut state = state();
        let gap = atman_proto::ResyncRequired {
            requested_after: state.cursor(),
            available_from: EventCursor(99),
            snapshot_revision: state.projection().revision,
            reason: "retained updates were replaced".into(),
        };
        let event = ProjectionEventEnvelope {
            schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
            daemon_generation: state.snapshot.daemon_generation.clone(),
            session_id: state.snapshot.projection.metadata.id.clone(),
            cursor: EventCursor(99),
            ts: serde_json::from_value(serde_json::json!("2026-01-01T00:00:00Z")).unwrap(),
            event: ServerEvent::ResyncRequired { gap: gap.clone() },
        };
        let error = state
            .apply_updates(&GetSessionUpdatesResponse {
                daemon_generation: state.snapshot.daemon_generation.clone(),
                events: vec![event],
                next_cursor: EventCursor(99),
                has_more: false,
                resync_required: None,
            })
            .unwrap_err();
        assert_eq!(error, ReconcileError::ResyncRequired(gap));
    }

    #[test]
    fn duplicate_page_is_idempotent() {
        let mut state = state();
        let response = GetSessionUpdatesResponse {
            daemon_generation: state.snapshot.daemon_generation.clone(),
            events: vec![envelope(
                &state,
                8,
                ProjectionDelta {
                    base_revision: Revision(3),
                    revision: Revision(4),
                    changes: Vec::new(),
                },
            )],
            next_cursor: EventCursor(8),
            has_more: false,
            resync_required: None,
        };
        state.apply_updates(&response).unwrap();
        let after_first = state.clone();
        let applied = state.apply_updates(&response).unwrap();
        assert_eq!(applied.applied, 0);
        assert_eq!(state, after_first);
    }

    #[test]
    fn empty_page_still_detects_daemon_generation_change() {
        let mut state = state();
        let original = state.clone();
        let response = GetSessionUpdatesResponse {
            daemon_generation: DaemonGeneration("generation-b".into()),
            events: Vec::new(),
            next_cursor: state.cursor(),
            has_more: false,
            resync_required: None,
        };

        assert!(matches!(
            state.apply_updates(&response),
            Err(ReconcileError::DaemonGeneration { .. })
        ));
        assert_eq!(state, original);
    }

    #[test]
    fn ephemeral_signal_advances_cursor_without_changing_projection() {
        let mut state = state();
        let original_projection = state.projection().clone();
        let mut event = envelope(
            &state,
            8,
            ProjectionDelta {
                base_revision: Revision(3),
                revision: Revision(4),
                changes: Vec::new(),
            },
        );
        event.event = ServerEvent::Signal {
            signal: SessionSignal::Progress {
                run_id: serde_json::from_value(serde_json::json!(
                    "018f7f24-1ab2-7c3d-8e4f-123456789abd"
                ))
                .unwrap(),
                label: "working".into(),
            },
        };
        let applied = state
            .apply_updates(&GetSessionUpdatesResponse {
                daemon_generation: state.snapshot.daemon_generation.clone(),
                events: vec![event],
                next_cursor: EventCursor(8),
                has_more: false,
                resync_required: None,
            })
            .unwrap();

        assert_eq!(state.cursor(), EventCursor(8));
        assert_eq!(state.projection(), &original_projection);
        assert_eq!(applied.signals.len(), 1);
    }

    fn capabilities(generation: &str) -> CapabilitiesResponse {
        CapabilitiesResponse {
            protocol_version: atman_proto::PROTOCOL_VERSION,
            daemon_version: "test".into(),
            daemon_generation: DaemonGeneration(generation.into()),
            event_schema_version: EVENT_SCHEMA_VERSION,
            methods: [
                method_descriptor::<rpc::DaemonCapabilities>(),
                method_descriptor::<rpc::GetSessionSnapshot>(),
                method_descriptor::<rpc::GetSessionUpdates>(),
            ]
            .into_iter()
            .map(|method| MethodCapability {
                name: method.name.into(),
                kind: method.kind,
                revision: method.revision,
            })
            .collect(),
            limits: ProtocolLimits {
                max_event_page_size: 100,
                subscriber_buffer: 100,
            },
        }
    }

    struct RecoveringTransport {
        session_id: SessionId,
        snapshot_calls: AtomicUsize,
        requests: StdMutex<Vec<String>>,
    }

    impl RpcTransport for RecoveringTransport {
        fn send(
            &self,
            request: JsonRpcRequest,
        ) -> BoxFuture<'_, Result<JsonRpcResponse, TransportError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request.method.clone());
                let result = match request.method.as_str() {
                    methods::DAEMON_CAPABILITIES => {
                        serde_json::to_value(capabilities("generation-a"))?
                    }
                    methods::GET_SESSION_SNAPSHOT => {
                        let call = self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
                        let mut projection =
                            projection(self.session_id.clone(), Revision(call as u64 + 1));
                        projection.goal = Some(if call == 0 { "before" } else { "after" }.into());
                        serde_json::to_value(SessionSnapshot {
                            schema_version: SNAPSHOT_SCHEMA_VERSION,
                            daemon_generation: DaemonGeneration("generation-a".into()),
                            cursor: EventCursor(call as u64 + 1),
                            projection,
                        })?
                    }
                    methods::GET_SESSION_UPDATES => {
                        serde_json::to_value(GetSessionUpdatesResponse {
                            daemon_generation: DaemonGeneration("generation-a".into()),
                            events: Vec::new(),
                            next_cursor: EventCursor(2),
                            has_more: false,
                            resync_required: Some(atman_proto::ResyncRequired {
                                requested_after: EventCursor(1),
                                available_from: EventCursor(2),
                                snapshot_revision: Revision(2),
                                reason: "retention gap".into(),
                            }),
                        })?
                    }
                    method => panic!("unexpected method {method}"),
                };
                Ok(JsonRpcResponse::ok(request.id, result))
            })
        }
    }

    #[tokio::test]
    async fn session_client_resnapshots_after_retention_gap() {
        let session_id: SessionId =
            serde_json::from_value(serde_json::json!("018f7f24-1ab2-7c3d-8e4f-123456789abe"))
                .unwrap();
        let client = Client::connect(
            RecoveringTransport {
                session_id: session_id.clone(),
                snapshot_calls: AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
            },
            ClientIdentity::new("test", "1"),
        )
        .await
        .unwrap();
        let session = client.attach_session(session_id).await.unwrap();
        assert_eq!(
            session.current().projection().goal.as_deref(),
            Some("before")
        );

        assert_eq!(session.refresh().await.unwrap(), RefreshOutcome::Resynced);
        assert_eq!(
            session.current().projection().goal.as_deref(),
            Some("after")
        );
        assert_eq!(session.current().cursor(), EventCursor(2));
    }

    #[tokio::test]
    async fn session_client_applies_stream_events_and_broadcasts_signals() {
        let session_id: SessionId =
            serde_json::from_value(serde_json::json!("018f7f24-1ab2-7c3d-8e4f-123456789ac0"))
                .unwrap();
        let client = Client::connect(
            RecoveringTransport {
                session_id: session_id.clone(),
                snapshot_calls: AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
            },
            ClientIdentity::new("test", "1"),
        )
        .await
        .unwrap();
        let session = client.attach_session(session_id).await.unwrap();
        let current = session.current();
        session
            .apply_event(envelope(
                &current,
                2,
                ProjectionDelta {
                    base_revision: Revision(1),
                    revision: Revision(2),
                    changes: vec![ProjectionChange::GoalSet {
                        goal: Some("streamed".into()),
                    }],
                },
            ))
            .await
            .unwrap();
        assert_eq!(
            session.current().projection().goal.as_deref(),
            Some("streamed")
        );

        let mut signals = session.subscribe_signals();
        let mut signal_event = envelope(
            &session.current(),
            3,
            ProjectionDelta {
                base_revision: Revision(2),
                revision: Revision(3),
                changes: Vec::new(),
            },
        );
        let signal = SessionSignal::Progress {
            run_id: serde_json::from_value(serde_json::json!(
                "018f7f24-1ab2-7c3d-8e4f-123456789ac1"
            ))
            .unwrap(),
            label: "streaming".into(),
        };
        signal_event.event = ServerEvent::Signal {
            signal: signal.clone(),
        };
        session.apply_event(signal_event).await.unwrap();
        assert_eq!(signals.recv().await.unwrap(), signal);
        assert_eq!(session.current().cursor(), EventCursor(3));
    }

    struct StreamingTransport {
        session_id: SessionId,
    }

    impl RpcTransport for StreamingTransport {
        fn send(
            &self,
            request: JsonRpcRequest,
        ) -> BoxFuture<'_, Result<JsonRpcResponse, TransportError>> {
            Box::pin(async move {
                let result = match request.method.as_str() {
                    methods::DAEMON_CAPABILITIES => {
                        serde_json::to_value(capabilities("generation-a"))?
                    }
                    methods::GET_SESSION_SNAPSHOT => serde_json::to_value(SessionSnapshot {
                        schema_version: SNAPSHOT_SCHEMA_VERSION,
                        daemon_generation: DaemonGeneration("generation-a".into()),
                        cursor: EventCursor(1),
                        projection: projection(self.session_id.clone(), Revision(1)),
                    })?,
                    methods::GET_SESSION_UPDATES => {
                        serde_json::to_value(GetSessionUpdatesResponse {
                            daemon_generation: DaemonGeneration("generation-a".into()),
                            events: Vec::new(),
                            next_cursor: EventCursor(1),
                            has_more: false,
                            resync_required: None,
                        })?
                    }
                    method => panic!("unexpected method {method}"),
                };
                Ok(JsonRpcResponse::ok(request.id, result))
            })
        }

        fn session_events(
            &self,
            session_id: SessionId,
            after_cursor: EventCursor,
        ) -> BoxFuture<'_, Result<Option<SessionEventStream>, TransportError>> {
            let expected = self.session_id.clone();
            Box::pin(async move {
                assert_eq!(session_id, expected);
                let stream: SessionEventStream = if after_cursor == EventCursor(1) {
                    futures::stream::iter(vec![Ok(ProjectionEventEnvelope {
                        schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
                        daemon_generation: DaemonGeneration("generation-a".into()),
                        session_id,
                        cursor: EventCursor(2),
                        ts: serde_json::from_value(serde_json::json!("2026-01-01T00:00:00Z"))
                            .unwrap(),
                        event: ServerEvent::ProjectionDelta {
                            delta: ProjectionDelta {
                                base_revision: Revision(1),
                                revision: Revision(2),
                                changes: vec![ProjectionChange::GoalSet {
                                    goal: Some("live".into()),
                                }],
                            },
                        },
                    })])
                    .chain(futures::stream::pending())
                    .boxed()
                } else {
                    futures::stream::pending().boxed()
                };
                Ok(Some(stream))
            })
        }
    }

    #[tokio::test]
    async fn synchronize_drives_the_transport_stream_into_the_session_store() {
        let session_id: SessionId =
            serde_json::from_value(serde_json::json!("018f7f24-1ab2-7c3d-8e4f-123456789ac2"))
                .unwrap();
        let client = Client::connect(
            StreamingTransport {
                session_id: session_id.clone(),
            },
            ClientIdentity::new("test", "1"),
        )
        .await
        .unwrap();
        let session = client.attach_session(session_id).await.unwrap();
        let mut state = session.subscribe();
        let sync_session = session.clone();
        let sync = tokio::spawn(async move { sync_session.synchronize().await });
        tokio::time::timeout(std::time::Duration::from_secs(1), state.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.borrow().projection().goal.as_deref(), Some("live"));
        sync.abort();
    }

    struct RestartingTransport {
        session_id: SessionId,
        capability_calls: AtomicUsize,
        snapshot_calls: AtomicUsize,
    }

    impl RpcTransport for RestartingTransport {
        fn send(
            &self,
            request: JsonRpcRequest,
        ) -> BoxFuture<'_, Result<JsonRpcResponse, TransportError>> {
            Box::pin(async move {
                let result = match request.method.as_str() {
                    methods::DAEMON_CAPABILITIES => {
                        let call = self.capability_calls.fetch_add(1, Ordering::SeqCst);
                        serde_json::to_value(capabilities(if call == 0 {
                            "generation-a"
                        } else {
                            "generation-b"
                        }))?
                    }
                    methods::GET_SESSION_SNAPSHOT => {
                        let call = self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
                        let mut projection =
                            projection(self.session_id.clone(), Revision(call as u64 + 1));
                        projection.goal = Some(if call == 0 { "before" } else { "after" }.into());
                        serde_json::to_value(SessionSnapshot {
                            schema_version: SNAPSHOT_SCHEMA_VERSION,
                            daemon_generation: DaemonGeneration(
                                if call == 0 {
                                    "generation-a"
                                } else {
                                    "generation-b"
                                }
                                .into(),
                            ),
                            cursor: EventCursor(call as u64 + 1),
                            projection,
                        })?
                    }
                    methods::GET_SESSION_UPDATES => {
                        serde_json::to_value(GetSessionUpdatesResponse {
                            daemon_generation: DaemonGeneration("generation-b".into()),
                            events: Vec::new(),
                            next_cursor: EventCursor(2),
                            has_more: false,
                            resync_required: None,
                        })?
                    }
                    method => panic!("unexpected method {method}"),
                };
                Ok(JsonRpcResponse::ok(request.id, result))
            })
        }
    }

    #[tokio::test]
    async fn session_client_rehandshakes_and_resnapshots_after_daemon_restart() {
        let session_id: SessionId =
            serde_json::from_value(serde_json::json!("018f7f24-1ab2-7c3d-8e4f-123456789abf"))
                .unwrap();
        let client = Client::connect(
            RestartingTransport {
                session_id: session_id.clone(),
                capability_calls: AtomicUsize::new(0),
                snapshot_calls: AtomicUsize::new(0),
            },
            ClientIdentity::new("test", "1"),
        )
        .await
        .unwrap();
        let session = client.attach_session(session_id).await.unwrap();
        assert_eq!(client.capabilities().daemon_generation.0, "generation-a");

        assert_eq!(
            session.refresh().await.unwrap(),
            RefreshOutcome::Reconnected
        );
        assert_eq!(client.capabilities().daemon_generation.0, "generation-b");
        assert_eq!(
            session.current().projection().goal.as_deref(),
            Some("after")
        );
        assert_eq!(session.current().cursor(), EventCursor(2));
    }

    struct CommandTransport {
        session_id: SessionId,
        send_attempts: AtomicUsize,
        requests: Arc<StdMutex<Vec<JsonRpcRequest>>>,
    }

    impl RpcTransport for CommandTransport {
        fn send(
            &self,
            request: JsonRpcRequest,
        ) -> BoxFuture<'_, Result<JsonRpcResponse, TransportError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request.clone());
                if request.method == methods::SEND_MESSAGE
                    && self.send_attempts.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return Err(TransportError::Closed);
                }
                let result = match request.method.as_str() {
                    methods::DAEMON_CAPABILITIES => {
                        let mut capabilities = capabilities("generation-a");
                        for method in [
                            method_descriptor::<rpc::SendMessage>(),
                            method_descriptor::<rpc::SubmitForm>(),
                        ] {
                            capabilities.methods.push(MethodCapability {
                                name: method.name.into(),
                                kind: method.kind,
                                revision: method.revision,
                            });
                        }
                        serde_json::to_value(capabilities)?
                    }
                    methods::GET_SESSION_SNAPSHOT => serde_json::to_value(SessionSnapshot {
                        schema_version: SNAPSHOT_SCHEMA_VERSION,
                        daemon_generation: DaemonGeneration("generation-a".into()),
                        cursor: EventCursor(1),
                        projection: projection(self.session_id.clone(), Revision(1)),
                    })?,
                    methods::SEND_MESSAGE => serde_json::to_value(SendMessageResponse {
                        session_id: self.session_id.clone(),
                        run_id: FlowRunId(
                            uuid::Uuid::parse_str("018f7f24-1ab2-7c3d-8e4f-123456789ad0").unwrap(),
                        ),
                        revision: Revision(2),
                        cursor: EventCursor(2),
                    })?,
                    methods::SUBMIT_FORM => {
                        let params = request.params.as_ref().unwrap();
                        serde_json::to_value(SubmitFormResponse {
                            resolved: true,
                            status: atman_proto::FormResolutionStatus::Resolved,
                            session_id: self.session_id.clone(),
                            form_id: params["form_id"].as_str().unwrap().into(),
                            revision: Revision(2),
                            cursor: EventCursor(2),
                        })?
                    }
                    methods::GET_SESSION_UPDATES => {
                        let current = SessionState::new(
                            SessionSnapshot {
                                schema_version: SNAPSHOT_SCHEMA_VERSION,
                                daemon_generation: DaemonGeneration("generation-a".into()),
                                cursor: EventCursor(1),
                                projection: projection(self.session_id.clone(), Revision(1)),
                            },
                            &DaemonGeneration("generation-a".into()),
                        )
                        .unwrap();
                        serde_json::to_value(GetSessionUpdatesResponse {
                            daemon_generation: DaemonGeneration("generation-a".into()),
                            events: vec![envelope(
                                &current,
                                2,
                                ProjectionDelta {
                                    base_revision: Revision(1),
                                    revision: Revision(2),
                                    changes: vec![ProjectionChange::LifecycleSet {
                                        lifecycle: atman_proto::SessionLifecycle::Active,
                                    }],
                                },
                            )],
                            next_cursor: EventCursor(2),
                            has_more: false,
                            resync_required: None,
                        })?
                    }
                    method => panic!("unexpected method {method}"),
                };
                Ok(JsonRpcResponse::ok(request.id, result))
            })
        }
    }

    #[tokio::test]
    async fn session_commands_retry_with_one_id_and_reconcile_through_the_commit_cursor() {
        let session_id =
            SessionId(uuid::Uuid::parse_str("018f7f24-1ab2-7c3d-8e4f-123456789acf").unwrap());
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let client = Client::connect(
            CommandTransport {
                session_id: session_id.clone(),
                send_attempts: AtomicUsize::new(0),
                requests: requests.clone(),
            },
            ClientIdentity::new("test", "1"),
        )
        .await
        .unwrap();
        let session = client.attach_session(session_id).await.unwrap();

        let response = session
            .send_message("retry me", None, Vec::new())
            .await
            .unwrap();
        assert_eq!(response.cursor, EventCursor(2));
        assert_eq!(session.current().cursor(), EventCursor(2));
        assert_eq!(
            session.current().projection().lifecycle,
            atman_proto::SessionLifecycle::Active
        );

        let request_ids = requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.method == methods::SEND_MESSAGE)
            .map(|request| {
                request
                    .params
                    .as_ref()
                    .and_then(|params| params.get("request_id"))
                    .cloned()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(request_ids.len(), 2);
        assert_eq!(request_ids[0], request_ids[1]);
        assert!(!request_ids[0].is_null());

        let form = session
            .submit_form(
                "form-1",
                FormSubmission::Submitted {
                    answers: vec![atman_proto::FormAnswer::Confirmed { value: true }],
                },
            )
            .await
            .unwrap();
        assert_eq!(form.form_id, "form-1");
        assert_eq!(form.cursor, EventCursor(2));
    }
}
