use std::sync::Arc;

use atman_proto::{
    DaemonGeneration, EventCursor, GetSessionSnapshotRequest, GetSessionUpdatesRequest,
    GetSessionUpdatesResponse, PROJECTION_EVENT_SCHEMA_VERSION, ProjectionChange, ProjectionDelta,
    ProjectionEventEnvelope, Revision, SNAPSHOT_SCHEMA_VERSION, ServerEvent, SessionId,
    SessionProjection, SessionSignal, SessionSnapshot, rpc,
};
use tokio::sync::{Mutex, watch};

use crate::{Client, ClientError};

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
}

#[derive(Debug, thiserror::Error)]
pub enum SessionClientError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Reconcile(#[from] ReconcileError),
}

#[derive(Clone)]
pub struct SessionClient {
    client: Client,
    session_id: SessionId,
    state: watch::Sender<SessionState>,
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
        let state = SessionState::new(snapshot, &client.capabilities().daemon_generation)?;
        let (state, _) = watch::channel(state);
        Ok(Self {
            client,
            session_id,
            state,
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

    pub async fn refresh(&self) -> Result<RefreshOutcome, SessionClientError> {
        let _guard = self.refresh_lock.lock().await;
        let current = self.current();
        let response = self
            .client
            .call::<rpc::GetSessionUpdates>(&GetSessionUpdatesRequest {
                session_id: self.session_id.clone(),
                after_cursor: current.cursor(),
                limit: Some(self.client.capabilities().limits.max_event_page_size),
            })
            .await?;
        let mut next = current.clone();
        match next.apply_updates(&response) {
            Ok(applied) => {
                let outcome = RefreshOutcome::Applied {
                    events: applied.applied,
                    signals: applied.signals,
                    has_more: applied.has_more,
                };
                if next != current {
                    self.state.send_replace(next);
                }
                Ok(outcome)
            }
            Err(error) if error.requires_resync() => {
                let snapshot = self
                    .client
                    .call::<rpc::GetSessionSnapshot>(&GetSessionSnapshotRequest {
                        session_id: self.session_id.clone(),
                    })
                    .await?;
                validate_session(&snapshot, &self.session_id)?;
                let next =
                    SessionState::new(snapshot, &self.client.capabilities().daemon_generation)?;
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
            }
        }
    }
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
        ServerEvent::ResyncRequired { gap } => {
            return Err(ReconcileError::ResyncRequired(gap.clone()));
        }
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
    use crate::{ClientIdentity, RpcTransport, TransportError};

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
    fn duplicate_page_is_idempotent() {
        let mut state = state();
        let response = GetSessionUpdatesResponse {
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
                    methods::DAEMON_CAPABILITIES => serde_json::to_value(CapabilitiesResponse {
                        protocol_version: atman_proto::PROTOCOL_VERSION,
                        daemon_version: "test".into(),
                        daemon_generation: DaemonGeneration("generation-a".into()),
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
                    })?,
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
}
