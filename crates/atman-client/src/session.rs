use std::collections::HashSet;
use std::sync::Arc;

use atman_proto::{
    AutoNameSessionRequest, AutoNameSessionResponse, CancelRunRequest, CancelRunResponse,
    CompactReviewDecision, CompactSessionRequest, CompactSessionResponse,
    CreatePermissionGroupRequest, CreatePermissionGroupResponse, DaemonGeneration, EventCursor,
    FlowRunId, FormSubmission, GetSessionSnapshotRequest, GetSessionTimelineBeforeRequest,
    GetSessionTimelineTailRequest, GetSessionUpdatesRequest, GetSessionUpdatesResponse,
    InlineImage, InspectResourceRequest, InspectResourceResponse, InstallSuggestedFlowRequest,
    InstallSuggestedFlowResponse, InterjectSessionRequest, InterjectSessionResponse,
    InterjectionLevel, ListPermissionRequestsRequest, ListPermissionRequestsResponse,
    ListResourcesRequest, ListResourcesResponse, MoveSessionRequest, MoveSessionResponse,
    PROJECTION_EVENT_SCHEMA_VERSION, PermissionRpcAction, PermissionRpcScope,
    PermissionRpcSelector, ProjectionChange, ProjectionDelta, ProjectionEventEnvelope, PromptId,
    ReleaseResourceRequest, ReleaseResourceResponse, ReloadSessionMcpRequest,
    ReloadSessionMcpResponse, RenameSessionRequest, RenameSessionResponse, RequestId,
    ResizeTerminalResourceRequest, ResizeTerminalResourceResponse, ResolveCompactReviewRequest,
    ResolveCompactReviewResponse, ResolvePermissionRequestsRequest,
    ResolvePermissionRequestsResponse, ResolvePromptRequest, ResolvePromptResponse, ResourceId,
    RetainResourceRequest, RetainResourceResponse, Revision, SNAPSHOT_SCHEMA_VERSION,
    SendMessageRequest, SendMessageResponse, ServerEvent, SessionId, SessionProjection,
    SessionSignal, SessionSnapshot, SessionTimelineBudget, SessionTimelinePage,
    SetSessionGoalRequest, SetSessionGoalResponse, StartRunRequest, StartRunResponse,
    SubmitFormRequest, SubmitFormResponse, SuggestFlowRequest, SuggestFlowResponse,
    TerminateResourceRequest, TerminateResourceResponse, TimelineCursor, TimelineItemId,
    TimelineSegment, TodoMutation, TrustProjection, UpdateSessionTodosRequest,
    UpdateSessionTodosResponse, UpdateSessionTrustRequest, UpdateSessionTrustResponse, rpc,
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
    snapshot: Arc<SessionSnapshot>,
    transcript_revision: u64,
    resources_revision: u64,
    bounded_transcript: bool,
}

impl SessionState {
    pub fn new(
        snapshot: SessionSnapshot,
        expected_generation: &DaemonGeneration,
    ) -> Result<Self, ReconcileError> {
        validate_snapshot(&snapshot, expected_generation)?;
        let transcript_revision = snapshot.projection.revision.0;
        Ok(Self {
            snapshot: Arc::new(snapshot),
            transcript_revision,
            resources_revision: transcript_revision,
            bounded_transcript: false,
        })
    }

    pub fn snapshot(&self) -> &SessionSnapshot {
        self.snapshot.as_ref()
    }

    pub fn projection(&self) -> &SessionProjection {
        &self.snapshot.projection
    }

    pub fn cursor(&self) -> EventCursor {
        self.snapshot.cursor
    }

    pub fn transcript_revision(&self) -> u64 {
        self.transcript_revision
    }

    pub fn resources_revision(&self) -> u64 {
        self.resources_revision
    }

    pub fn apply_updates(
        &mut self,
        response: &GetSessionUpdatesResponse,
    ) -> Result<AppliedUpdates, ReconcileError> {
        validate_updates(&self.snapshot, response)?;
        let snapshot = Arc::make_mut(&mut self.snapshot);
        let mut applied = 0;
        let mut signals = Vec::new();
        for envelope in &response.events {
            if envelope.cursor <= snapshot.cursor {
                continue;
            }
            let changes =
                apply_validated_envelope(snapshot, envelope, &mut signals, self.bounded_transcript);
            if changes.transcript {
                self.transcript_revision = self.transcript_revision.wrapping_add(1);
            }
            if changes.resources {
                self.resources_revision = self.resources_revision.wrapping_add(1);
            }
            applied += 1;
        }
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

#[derive(Debug, Clone, PartialEq)]
pub enum SessionUpdate {
    Changed {
        state: SessionState,
        signals: Vec<SessionSignal>,
    },
    HistoryPrepended {
        state: SessionState,
        loaded_items: usize,
    },
    Reset(Box<SessionState>),
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
    #[error("command result belongs to compact review {received}, expected {expected}")]
    CommandCompactReview { expected: String, received: String },
    #[error("command result belongs to prompt {received}, expected {expected}")]
    CommandPrompt {
        expected: PromptId,
        received: PromptId,
    },
    #[error("command result belongs to resource {received:?}, expected {expected:?}")]
    CommandResource {
        expected: ResourceId,
        received: ResourceId,
    },
    #[error("invalid session timeline: {0}")]
    Timeline(String),
}

impl SessionClientError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Client(error) => error.is_retryable(),
            Self::Transport(error) => error.is_retryable(),
            Self::Reconcile(_) => false,
            Self::CommittedCursorUnavailable { .. } => false,
            Self::CommandSession { .. }
            | Self::CommandRun { .. }
            | Self::CommandForm { .. }
            | Self::CommandCompactReview { .. }
            | Self::CommandPrompt { .. }
            | Self::CommandResource { .. }
            | Self::Timeline(_) => false,
        }
    }
}

#[derive(Clone)]
pub struct SessionClient {
    client: Client,
    session_id: SessionId,
    state: watch::Sender<SessionState>,
    signals: broadcast::Sender<SessionSignal>,
    updates: broadcast::Sender<SessionUpdate>,
    refresh_lock: Arc<Mutex<()>>,
    history: Option<Arc<Mutex<TimelineHistory>>>,
}

struct TimelineHistory {
    oldest: Option<TimelineCursor>,
    has_older: bool,
    loaded_items: HashSet<TimelineItemId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryLoadOutcome {
    pub loaded_items: usize,
    pub has_more: bool,
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
        Self::from_snapshot(client, snapshot)
    }

    pub(crate) async fn attach_windowed(
        client: Client,
        session_id: SessionId,
    ) -> Result<Self, SessionClientError> {
        let page = client
            .call::<rpc::GetSessionTimelineTail>(&GetSessionTimelineTailRequest {
                session_id: session_id.clone(),
                budget: SessionTimelineBudget {
                    turn_budget: Some(12),
                    byte_budget: Some(256 * 1024),
                },
            })
            .await?;
        validate_timeline_page(&page, &session_id, &client.capabilities().daemon_generation)?;
        Self::from_timeline_page(client, page)
    }

    pub(crate) fn from_snapshot(
        client: Client,
        snapshot: SessionSnapshot,
    ) -> Result<Self, SessionClientError> {
        let session_id = snapshot.projection.metadata.id.clone();
        validate_session(&snapshot, &session_id)?;
        let capabilities = client.capabilities();
        let state = SessionState::new(snapshot, &capabilities.daemon_generation)?;
        let (state, _) = watch::channel(state);
        let (signals, _) = broadcast::channel(capabilities.limits.subscriber_buffer.max(1));
        let (updates, _) = broadcast::channel(capabilities.limits.subscriber_buffer.max(1));
        Ok(Self {
            client,
            session_id,
            state,
            signals,
            updates,
            refresh_lock: Arc::new(Mutex::new(())),
            history: None,
        })
    }

    fn from_timeline_page(
        client: Client,
        page: SessionTimelinePage,
    ) -> Result<Self, SessionClientError> {
        let capabilities = client.capabilities();
        let snapshot = snapshot_from_timeline(&page)?;
        let session_id = snapshot.projection.metadata.id.clone();
        let mut state = SessionState::new(snapshot, &capabilities.daemon_generation)?;
        state.bounded_transcript = true;
        let history = TimelineHistory {
            oldest: oldest_cursor(&page),
            has_older: page.older.has_more,
            loaded_items: timeline_item_ids(&page),
        };
        let (state, _) = watch::channel(state);
        let (signals, _) = broadcast::channel(capabilities.limits.subscriber_buffer.max(1));
        let (updates, _) = broadcast::channel(capabilities.limits.subscriber_buffer.max(1));
        Ok(Self {
            client,
            session_id,
            state,
            signals,
            updates,
            refresh_lock: Arc::new(Mutex::new(())),
            history: Some(Arc::new(Mutex::new(history))),
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

    pub fn subscribe_updates(&self) -> broadcast::Receiver<SessionUpdate> {
        self.updates.subscribe()
    }

    pub async fn load_older_history(&self) -> Result<HistoryLoadOutcome, SessionClientError> {
        let Some(history) = &self.history else {
            return Ok(HistoryLoadOutcome {
                loaded_items: 0,
                has_more: false,
            });
        };
        let _refresh_guard = self.refresh_lock.lock().await;
        let mut history = history.lock().await;
        let Some(before) = history.oldest.clone().filter(|_| history.has_older) else {
            return Ok(HistoryLoadOutcome {
                loaded_items: 0,
                has_more: false,
            });
        };
        let page = self
            .client
            .call::<rpc::GetSessionTimelineBefore>(&GetSessionTimelineBeforeRequest {
                session_id: self.session_id.clone(),
                before,
                budget: SessionTimelineBudget {
                    turn_budget: Some(12),
                    byte_budget: Some(256 * 1024),
                },
            })
            .await?;
        validate_timeline_page(
            &page,
            &self.session_id,
            &self.client.capabilities().daemon_generation,
        )?;
        let mut next = self.current();
        let loaded_items = merge_timeline_page(&mut next, &page, &mut history.loaded_items);
        history.oldest = oldest_cursor(&page).or(history.oldest.clone());
        history.has_older = page.older.has_more;
        if loaded_items > 0 {
            self.state.send_replace(next.clone());
            let _ = self.updates.send(SessionUpdate::HistoryPrepended {
                state: next,
                loaded_items,
            });
        }
        Ok(HistoryLoadOutcome {
            loaded_items,
            has_more: history.has_older,
        })
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
        let previous_transcript_revision = current.transcript_revision;
        let previous_resources_revision = current.resources_revision;
        let mut next = current;
        match next.apply_updates(&response) {
            Ok(applied) => {
                let outcome = RefreshOutcome::Applied {
                    events: applied.applied,
                    signals: applied.signals.clone(),
                    has_more: applied.has_more,
                };
                if applied.applied > 0 {
                    self.state.send_replace(next.clone());
                    let _ = self.updates.send(SessionUpdate::Changed {
                        state: next,
                        signals: applied.signals.clone(),
                    });
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
                let mut next = self
                    .fetch_fresh_state(&capabilities.daemon_generation)
                    .await?;
                next.transcript_revision = previous_transcript_revision.wrapping_add(1);
                next.resources_revision = previous_resources_revision.wrapping_add(1);
                self.state.send_replace(next.clone());
                let _ = self.updates.send(SessionUpdate::Reset(Box::new(next)));
                Ok(RefreshOutcome::Reconnected)
            }
            Err(error) if error.requires_resync() => {
                let mut next = self
                    .fetch_fresh_state(&capabilities.daemon_generation)
                    .await?;
                next.transcript_revision = previous_transcript_revision.wrapping_add(1);
                next.resources_revision = previous_resources_revision.wrapping_add(1);
                self.state.send_replace(next.clone());
                let _ = self.updates.send(SessionUpdate::Reset(Box::new(next)));
                Ok(RefreshOutcome::Resynced)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn fetch_fresh_state(
        &self,
        expected_generation: &DaemonGeneration,
    ) -> Result<SessionState, SessionClientError> {
        if let Some(history) = &self.history {
            let page = self
                .client
                .call::<rpc::GetSessionTimelineTail>(&GetSessionTimelineTailRequest {
                    session_id: self.session_id.clone(),
                    budget: SessionTimelineBudget {
                        turn_budget: Some(12),
                        byte_budget: Some(256 * 1024),
                    },
                })
                .await?;
            validate_timeline_page(&page, &self.session_id, expected_generation)?;
            let mut state = SessionState::new(snapshot_from_timeline(&page)?, expected_generation)?;
            state.bounded_transcript = true;
            *history.lock().await = TimelineHistory {
                oldest: oldest_cursor(&page),
                has_older: page.older.has_more,
                loaded_items: timeline_item_ids(&page),
            };
            return Ok(state);
        }
        let snapshot = self
            .client
            .call::<rpc::GetSessionSnapshot>(&GetSessionSnapshotRequest {
                session_id: self.session_id.clone(),
            })
            .await?;
        validate_session(&snapshot, &self.session_id)?;
        Ok(SessionState::new(snapshot, expected_generation)?)
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

    pub async fn start_run(
        &self,
        flow_path: impl Into<String>,
        flow_name: Option<String>,
        args: serde_json::Map<String, serde_json::Value>,
        reasoning: Option<String>,
        images: Vec<InlineImage>,
    ) -> Result<StartRunResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::StartRun>(&StartRunRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                flow_path: flow_path.into(),
                flow_name,
                args,
                reasoning,
                images,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn rename(
        &self,
        title: impl Into<String>,
    ) -> Result<RenameSessionResponse, SessionClientError> {
        self.set_title(Some(title.into())).await
    }

    pub async fn clear_title(&self) -> Result<RenameSessionResponse, SessionClientError> {
        self.set_title(None).await
    }

    pub async fn auto_name(&self) -> Result<AutoNameSessionResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::AutoNameSession>(&AutoNameSessionRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
            })
            .await?;
        self.validate_command_session(&response.session.id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn suggest_flow(&self) -> Result<SuggestFlowResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::SuggestFlow>(&SuggestFlowRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        Ok(response)
    }

    pub async fn install_suggested_flow(
        &self,
        flow_name: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<InstallSuggestedFlowResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::InstallSuggestedFlow>(&InstallSuggestedFlowRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                flow_name: flow_name.into(),
                source: source.into(),
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        Ok(response)
    }

    pub async fn move_to(
        &self,
        project_root: impl Into<String>,
    ) -> Result<MoveSessionResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::MoveSession>(&MoveSessionRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                project_root: project_root.into(),
            })
            .await?;
        self.validate_command_session(&response.session.id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn set_goal(
        &self,
        goal: Option<String>,
    ) -> Result<SetSessionGoalResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::SetSessionGoal>(&SetSessionGoalRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                goal,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn update_todos(
        &self,
        mutation: TodoMutation,
    ) -> Result<UpdateSessionTodosResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::UpdateSessionTodos>(&UpdateSessionTodosRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                mutation,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    async fn set_title(
        &self,
        title: Option<String>,
    ) -> Result<RenameSessionResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::RenameSession>(&RenameSessionRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                title,
            })
            .await?;
        self.validate_command_session(&response.session.id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn update_trust(
        &self,
        trust: TrustProjection,
    ) -> Result<UpdateSessionTrustResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::UpdateSessionTrust>(&UpdateSessionTrustRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                trust,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn reload_mcp(&self) -> Result<ReloadSessionMcpResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::ReloadSessionMcp>(&ReloadSessionMcpRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn compact(&self) -> Result<CompactSessionResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::CompactSession>(&CompactSessionRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
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

    pub async fn resolve_prompt(
        &self,
        prompt_id: PromptId,
        answer: serde_json::Value,
    ) -> Result<ResolvePromptResponse, SessionClientError> {
        let expected_prompt = prompt_id.clone();
        let response = self
            .client
            .command::<rpc::ResolvePrompt>(&ResolvePromptRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                prompt_id,
                answer,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        if response.prompt_id != expected_prompt {
            return Err(SessionClientError::CommandPrompt {
                expected: expected_prompt,
                received: response.prompt_id,
            });
        }
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn resolve_compact_review(
        &self,
        review_id: impl Into<String>,
        decision: CompactReviewDecision,
    ) -> Result<ResolveCompactReviewResponse, SessionClientError> {
        let review_id = review_id.into();
        let response = self
            .client
            .command::<rpc::ResolveCompactReview>(&ResolveCompactReviewRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                review_id: review_id.clone(),
                decision,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        if response.review_id != review_id {
            return Err(SessionClientError::CommandCompactReview {
                expected: review_id,
                received: response.review_id,
            });
        }
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn list_permissions(
        &self,
    ) -> Result<ListPermissionRequestsResponse, SessionClientError> {
        let response = self
            .client
            .call::<rpc::ListPermissionRequests>(&ListPermissionRequestsRequest {
                session_id: self.session_id.clone(),
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn list_resources(&self) -> Result<ListResourcesResponse, SessionClientError> {
        let response = self
            .client
            .call::<rpc::ListResources>(&ListResourcesRequest {
                session_id: self.session_id.clone(),
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn inspect_resource(
        &self,
        resource_id: ResourceId,
    ) -> Result<InspectResourceResponse, SessionClientError> {
        let expected_resource = resource_id.clone();
        let response = self
            .client
            .call::<rpc::InspectResource>(&InspectResourceRequest {
                session_id: self.session_id.clone(),
                resource_id,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.validate_command_resource(&expected_resource, &response.resource.id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn terminate_resource(
        &self,
        resource_id: ResourceId,
    ) -> Result<TerminateResourceResponse, SessionClientError> {
        let expected_resource = resource_id.clone();
        let response = self
            .client
            .command::<rpc::TerminateResource>(&TerminateResourceRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                resource_id,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.validate_command_resource(&expected_resource, &response.resource_id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn resize_terminal(
        &self,
        resource_id: ResourceId,
        rows: u16,
        cols: u16,
    ) -> Result<ResizeTerminalResourceResponse, SessionClientError> {
        let expected_resource = resource_id.clone();
        let response = self
            .client
            .command::<rpc::ResizeTerminalResource>(&ResizeTerminalResourceRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                resource_id,
                rows,
                cols,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.validate_command_resource(&expected_resource, &response.resource_id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn retain_resource(
        &self,
        resource_id: ResourceId,
    ) -> Result<RetainResourceResponse, SessionClientError> {
        let expected_resource = resource_id.clone();
        let response = self
            .client
            .command::<rpc::RetainResource>(&RetainResourceRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                resource_id,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.validate_command_resource(&expected_resource, &response.resource.id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn release_resource(
        &self,
        resource_id: ResourceId,
    ) -> Result<ReleaseResourceResponse, SessionClientError> {
        let expected_resource = resource_id.clone();
        let response = self
            .client
            .command::<rpc::ReleaseResource>(&ReleaseResourceRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                resource_id,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.validate_command_resource(&expected_resource, &response.resource.id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn create_permission_group(
        &self,
        request_ids: Vec<uuid::Uuid>,
        expected_request_revisions: std::collections::BTreeMap<uuid::Uuid, u64>,
        label: impl Into<String>,
    ) -> Result<CreatePermissionGroupResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::CreatePermissionGroup>(&CreatePermissionGroupRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                request_ids,
                expected_request_revisions,
                label: label.into(),
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
        self.refresh_through(response.cursor).await?;
        Ok(response)
    }

    pub async fn resolve_permissions(
        &self,
        selector: PermissionRpcSelector,
        action: PermissionRpcAction,
        scope: Option<PermissionRpcScope>,
        reason: Option<String>,
    ) -> Result<ResolvePermissionRequestsResponse, SessionClientError> {
        let response = self
            .client
            .command::<rpc::ResolvePermissionRequests>(&ResolvePermissionRequestsRequest {
                request_id: Some(RequestId::now()),
                session_id: self.session_id.clone(),
                selector,
                action,
                scope,
                reason,
            })
            .await?;
        self.validate_command_session(&response.session_id)?;
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

    fn validate_command_resource(
        &self,
        expected: &ResourceId,
        received: &ResourceId,
    ) -> Result<(), SessionClientError> {
        if received != expected {
            return Err(SessionClientError::CommandResource {
                expected: expected.clone(),
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

fn validate_timeline_page(
    page: &SessionTimelinePage,
    expected_session: &SessionId,
    expected_generation: &DaemonGeneration,
) -> Result<(), SessionClientError> {
    if &page.session_id != expected_session {
        return Err(ReconcileError::Session {
            expected: expected_session.clone(),
            received: page.session_id.clone(),
        }
        .into());
    }
    if &page.daemon_generation != expected_generation {
        return Err(ReconcileError::DaemonGeneration {
            expected: expected_generation.clone(),
            received: page.daemon_generation.clone(),
        }
        .into());
    }
    Ok(())
}

fn snapshot_from_timeline(
    page: &SessionTimelinePage,
) -> Result<SessionSnapshot, SessionClientError> {
    let live = page
        .live
        .as_ref()
        .ok_or_else(|| SessionClientError::Timeline("tail page has no live state".into()))?;
    let mut transcript = page
        .segments
        .iter()
        .flat_map(timeline_items)
        .map(|item| item.preview.clone())
        .collect::<Vec<_>>();
    transcript.sort_by_key(atman_proto::TranscriptItem::seq);
    let mut workflow_turns = HashSet::new();
    let workflows = page
        .segments
        .iter()
        .filter_map(|segment| match segment {
            TimelineSegment::Turn { segment } => segment.workflow.clone(),
            TimelineSegment::Session { .. } => None,
        })
        .filter(|workflow| workflow_turns.insert(workflow.turn_id.0))
        .collect();
    Ok(SessionSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        daemon_generation: page.daemon_generation.clone(),
        cursor: page.as_of_cursor,
        projection: SessionProjection {
            revision: page.projection_revision,
            metadata: live.metadata.clone(),
            lifecycle: live.lifecycle,
            runs: live.runs.clone(),
            transcript,
            workflows,
            compactions: live.compactions.clone(),
            goal: live.goal.clone(),
            todos: live.todos.clone(),
            plans: live.plans.clone(),
            context: live.context.clone(),
            trust: live.trust.clone(),
            interactions: live.interactions.clone(),
            resources: live.resources.clone(),
            usage: live.usage.clone(),
        },
    })
}

fn merge_timeline_page(
    state: &mut SessionState,
    page: &SessionTimelinePage,
    loaded_items: &mut HashSet<TimelineItemId>,
) -> usize {
    let snapshot = Arc::make_mut(&mut state.snapshot);
    let mut loaded = 0;
    for item in page.segments.iter().flat_map(timeline_items) {
        if loaded_items.insert(item.id.clone()) {
            snapshot.projection.transcript.push(item.preview.clone());
            loaded += 1;
        }
    }
    for workflow in page.segments.iter().filter_map(|segment| match segment {
        TimelineSegment::Turn { segment } => segment.workflow.as_ref(),
        TimelineSegment::Session { .. } => None,
    }) {
        if !snapshot
            .projection
            .workflows
            .iter()
            .any(|existing| existing.turn_id == workflow.turn_id)
        {
            snapshot.projection.workflows.push(workflow.clone());
        }
    }
    if loaded > 0 {
        snapshot
            .projection
            .transcript
            .sort_by_key(atman_proto::TranscriptItem::seq);
        state.transcript_revision = state.transcript_revision.wrapping_add(1);
    }
    loaded
}

fn oldest_cursor(page: &SessionTimelinePage) -> Option<TimelineCursor> {
    page.segments
        .iter()
        .flat_map(timeline_items)
        .min_by_key(|item| item.seq)
        .map(|item| TimelineCursor {
            seq: item.seq,
            item_id: item.id.clone(),
        })
}

fn timeline_item_ids(page: &SessionTimelinePage) -> HashSet<TimelineItemId> {
    page.segments
        .iter()
        .flat_map(timeline_items)
        .map(|item| item.id.clone())
        .collect()
}

fn timeline_items(segment: &TimelineSegment) -> impl Iterator<Item = &atman_proto::TimelineItem> {
    match segment {
        TimelineSegment::Turn { segment } => segment.items.iter(),
        TimelineSegment::Session { segment } => segment.items.iter(),
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

fn validate_updates(
    snapshot: &SessionSnapshot,
    response: &GetSessionUpdatesResponse,
) -> Result<(), ReconcileError> {
    if response.daemon_generation != snapshot.daemon_generation {
        return Err(ReconcileError::DaemonGeneration {
            expected: snapshot.daemon_generation.clone(),
            received: response.daemon_generation.clone(),
        });
    }
    if let Some(gap) = &response.resync_required {
        return Err(ReconcileError::ResyncRequired(gap.clone()));
    }
    let starting_cursor = snapshot.cursor;
    let mut cursor = snapshot.cursor;
    let mut revision = snapshot.projection.revision;
    for envelope in &response.events {
        if envelope.cursor <= cursor {
            continue;
        }
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
        let expected_cursor = EventCursor(cursor.0.saturating_add(1));
        if envelope.cursor != expected_cursor {
            return Err(ReconcileError::CursorGap {
                current: cursor,
                received: envelope.cursor,
            });
        }
        if let ServerEvent::ProjectionDelta { delta } = &envelope.event {
            validate_delta(revision, delta)?;
            revision = delta.revision;
        }
        cursor = envelope.cursor;
    }
    if response.next_cursor > cursor
        || (response.next_cursor > starting_cursor && response.next_cursor != cursor)
    {
        return Err(ReconcileError::PageCursor {
            actual: cursor,
            declared: response.next_cursor,
        });
    }
    Ok(())
}

fn apply_validated_envelope(
    snapshot: &mut SessionSnapshot,
    envelope: &ProjectionEventEnvelope,
    signals: &mut Vec<SessionSignal>,
    bounded_transcript: bool,
) -> SliceChanges {
    let changes = match &envelope.event {
        ServerEvent::ProjectionDelta { delta } => {
            apply_validated_delta(&mut snapshot.projection, delta, bounded_transcript);
            SliceChanges {
                transcript: delta.changes.iter().any(|change| {
                    matches!(
                        change,
                        ProjectionChange::RunUpsert { .. }
                            | ProjectionChange::RunRemove { .. }
                            | ProjectionChange::TranscriptAppend { .. }
                            | ProjectionChange::TranscriptReplace { .. }
                            | ProjectionChange::WorkflowUpsert { .. }
                            | ProjectionChange::WorkflowRemove { .. }
                            | ProjectionChange::CompactionsReplace { .. }
                            | ProjectionChange::InteractionUpsert { .. }
                            | ProjectionChange::InteractionRemove { .. }
                    )
                }),
                resources: delta.changes.iter().any(|change| {
                    matches!(
                        change,
                        ProjectionChange::ResourceUpsert { .. }
                            | ProjectionChange::ResourceRemove { .. }
                    )
                }),
            }
        }
        ServerEvent::Signal { signal } => {
            signals.push(signal.clone());
            SliceChanges::default()
        }
        ServerEvent::ResyncRequired { .. } => unreachable!("validated before applying updates"),
        ServerEvent::Heartbeat => SliceChanges::default(),
    };
    snapshot.cursor = envelope.cursor;
    changes
}

#[derive(Debug, Clone, Copy, Default)]
struct SliceChanges {
    transcript: bool,
    resources: bool,
}

fn validate_delta(revision: Revision, delta: &ProjectionDelta) -> Result<(), ReconcileError> {
    if delta.base_revision != revision {
        return Err(ReconcileError::RevisionBase {
            expected: revision,
            received: delta.base_revision,
        });
    }
    if delta.revision.0 != delta.base_revision.0.saturating_add(1) {
        return Err(ReconcileError::RevisionStep {
            base: delta.base_revision,
            received: delta.revision,
        });
    }
    Ok(())
}

fn apply_validated_delta(
    projection: &mut SessionProjection,
    delta: &ProjectionDelta,
    bounded_transcript: bool,
) {
    for change in &delta.changes {
        apply_change(projection, change, bounded_transcript);
    }
    projection.revision = delta.revision;
}

fn apply_change(
    projection: &mut SessionProjection,
    change: &ProjectionChange,
    bounded_transcript: bool,
) {
    match change {
        ProjectionChange::MetadataSet { metadata } => projection.metadata.clone_from(metadata),
        ProjectionChange::LifecycleSet { lifecycle } => projection.lifecycle = *lifecycle,
        ProjectionChange::RunUpsert { run } => upsert_run(&mut projection.runs, run.clone()),
        ProjectionChange::RunRemove { run_id } => projection.runs.retain(|run| &run.id != run_id),
        ProjectionChange::TranscriptAppend { items } => {
            projection.transcript.extend(items.iter().cloned())
        }
        ProjectionChange::TranscriptReplace { items } if bounded_transcript => {
            replace_bounded_transcript(projection, items)
        }
        ProjectionChange::TranscriptReplace { items } => projection.transcript.clone_from(items),
        ProjectionChange::WorkflowUpsert { workflow } => {
            upsert_workflow(&mut projection.workflows, workflow.clone())
        }
        ProjectionChange::WorkflowRemove { turn_id } => projection
            .workflows
            .retain(|workflow| &workflow.turn_id != turn_id),
        ProjectionChange::CompactionsReplace { compactions } => {
            projection.compactions.clone_from(compactions)
        }
        ProjectionChange::GoalSet { goal } => projection.goal.clone_from(goal),
        ProjectionChange::TodosReplace { todos } => projection.todos.clone_from(todos),
        ProjectionChange::PlansReplace { plans } => projection.plans.clone_from(plans),
        ProjectionChange::ContextSet { context } => projection.context.clone_from(context),
        ProjectionChange::TrustSet { trust } => projection.trust.clone_from(trust),
        ProjectionChange::InteractionUpsert { interaction } => {
            upsert_interaction(&mut projection.interactions, interaction)
        }
        ProjectionChange::InteractionRemove { target } => {
            remove_interaction(&mut projection.interactions, target)
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

fn upsert_workflow(
    workflows: &mut Vec<atman_proto::WorkflowProjection>,
    workflow: atman_proto::WorkflowProjection,
) {
    if let Some(existing) = workflows
        .iter_mut()
        .find(|existing| existing.turn_id == workflow.turn_id)
    {
        *existing = workflow;
    } else {
        workflows.push(workflow);
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

fn upsert_interaction(
    interactions: &mut atman_proto::InteractionProjection,
    interaction: &atman_proto::InteractionItem,
) {
    match interaction {
        atman_proto::InteractionItem::Prompt { prompt } => {
            interactions.prompts.retain(|item| item.id != prompt.id);
            interactions.prompts.push(prompt.clone());
        }
        atman_proto::InteractionItem::Approval { approval } => {
            interactions.approvals.retain(|item| item.id != approval.id);
            interactions.approvals.push(approval.as_ref().clone());
        }
        atman_proto::InteractionItem::ApprovalGroup { group } => {
            interactions
                .approval_groups
                .retain(|item| item.id != group.id);
            interactions.approval_groups.push(group.clone());
        }
        atman_proto::InteractionItem::Form { form } => {
            interactions.forms.retain(|item| item.id != form.id);
            interactions.forms.push(form.clone());
            interactions.forms.sort_by_key(|item| item.emitted_at);
        }
        atman_proto::InteractionItem::CompactReview { review } => {
            interactions
                .compact_reviews
                .retain(|item| item.id != review.id);
            interactions.compact_reviews.push(review.clone());
        }
        atman_proto::InteractionItem::Interjection { interjection } => {
            interactions
                .interjections
                .retain(|item| item.id != interjection.id);
            interactions.interjections.push(interjection.clone());
            interactions
                .interjections
                .sort_by_key(|item| item.created_at);
        }
    }
}

fn replace_bounded_transcript(
    projection: &mut SessionProjection,
    replacement: &[atman_proto::TranscriptItem],
) {
    let run_turns = projection
        .runs
        .iter()
        .filter_map(|run| run.turn_id.as_ref().map(|turn| (run.id.0, turn.0)))
        .collect::<std::collections::HashMap<_, _>>();
    let visible_turns = projection
        .transcript
        .iter()
        .filter_map(|item| transcript_turn_id(item, &run_turns))
        .collect::<HashSet<_>>();
    let minimum_seq = projection
        .transcript
        .iter()
        .map(atman_proto::TranscriptItem::seq)
        .min()
        .unwrap_or_default();
    projection.transcript = replacement
        .iter()
        .filter(|item| match transcript_turn_id(item, &run_turns) {
            Some(turn_id) => visible_turns.contains(&turn_id),
            None => item.seq() >= minimum_seq,
        })
        .cloned()
        .collect();
}

fn transcript_turn_id(
    item: &atman_proto::TranscriptItem,
    run_turns: &std::collections::HashMap<uuid::Uuid, uuid::Uuid>,
) -> Option<uuid::Uuid> {
    match item {
        atman_proto::TranscriptItem::Message { message, .. } => Some(message.turn_id.0),
        atman_proto::TranscriptItem::FileEdit {
            turn_id, run_id, ..
        } => turn_id.as_ref().map(|turn| turn.0).or_else(|| {
            run_id
                .as_ref()
                .and_then(|run| run_turns.get(&run.0).copied())
        }),
        atman_proto::TranscriptItem::ActivitySummary { turn_id, .. } => Some(turn_id.0),
        atman_proto::TranscriptItem::Diff { run_id, .. }
        | atman_proto::TranscriptItem::Compaction { run_id, .. } => run_id
            .as_ref()
            .and_then(|run| run_turns.get(&run.0).copied()),
        atman_proto::TranscriptItem::Mermaid { .. }
        | atman_proto::TranscriptItem::Notice { .. }
        | atman_proto::TranscriptItem::Extension { .. } => None,
    }
}

fn remove_interaction(
    interactions: &mut atman_proto::InteractionProjection,
    target: &atman_proto::InteractionTarget,
) {
    match target {
        atman_proto::InteractionTarget::Prompt { prompt_id } => {
            interactions.prompts.retain(|item| &item.id != prompt_id)
        }
        atman_proto::InteractionTarget::Approval { approval_id } => interactions
            .approvals
            .retain(|item| &item.id != approval_id),
        atman_proto::InteractionTarget::ApprovalGroup { group_id } => interactions
            .approval_groups
            .retain(|item| &item.id != group_id),
        atman_proto::InteractionTarget::Form { form_id } => {
            interactions.forms.retain(|item| &item.id != form_id)
        }
        atman_proto::InteractionTarget::CompactReview { review_id } => interactions
            .compact_reviews
            .retain(|item| &item.id != review_id),
        atman_proto::InteractionTarget::Interjection { interjection_id } => interactions
            .interjections
            .retain(|item| &item.id != interjection_id),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };

    use atman_proto::{
        CapabilitiesResponse, JsonRpcRequest, JsonRpcResponse, MethodCapability,
        PROJECTION_EVENT_SCHEMA_VERSION, ProtocolLimits, method_descriptor, methods,
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
            compactions: Vec::new(),
            workflows: Vec::new(),
            goal: None,
            todos: Vec::new(),
            plans: Vec::new(),
            context: Default::default(),
            trust: Default::default(),
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

    fn timeline_item(seq: u64, turn_id: atman_proto::TurnId) -> atman_proto::TimelineItem {
        let preview = atman_proto::TranscriptItem::Message {
            seq,
            ts: chrono::Utc::now(),
            run_id: None,
            context_id: None,
            checkpoint_index: None,
            message: atman_proto::MessageProjection {
                role: atman_proto::MessageRole::User,
                origin: atman_proto::MessageOrigin::User,
                turn_id: turn_id.clone(),
                parts: vec![atman_proto::MessagePart::Text {
                    text: format!("message {seq}"),
                }],
            },
        };
        atman_proto::TimelineItem {
            id: TimelineItemId(format!("message:{seq}")),
            seq,
            turn_id: Some(turn_id),
            run_id: None,
            kind: atman_proto::TimelineItemKind::Message,
            preview,
            detail: None,
        }
    }

    fn timeline_page(
        source: &SessionProjection,
        seq: u64,
        has_older: bool,
        include_live: bool,
    ) -> SessionTimelinePage {
        let turn_id = atman_proto::TurnId(uuid::Uuid::from_u128(seq as u128));
        let item = timeline_item(seq, turn_id.clone());
        SessionTimelinePage {
            session_id: source.metadata.id.clone(),
            daemon_generation: DaemonGeneration("generation-a".into()),
            as_of_cursor: EventCursor(7),
            projection_revision: source.revision,
            segments: vec![TimelineSegment::Turn {
                segment: atman_proto::TimelineTurnSegment {
                    id: atman_proto::TimelineSegmentId(format!("turn:{}", turn_id.0)),
                    turn_id,
                    start_seq: seq,
                    latest_seq: seq,
                    revision: Revision(seq),
                    state: atman_proto::TimelineTurnState::Complete,
                    items: vec![item],
                    workflow: None,
                },
            }],
            older: atman_proto::TimelineRemaining {
                has_more: has_older,
                estimated_segments: None,
            },
            newer: atman_proto::TimelineRemaining::default(),
            serialized_bytes: 1,
            live: include_live.then(|| atman_proto::TimelineLiveState {
                metadata: source.metadata.clone(),
                lifecycle: source.lifecycle,
                runs: source.runs.clone(),
                active_turns: Vec::new(),
                compactions: source.compactions.clone(),
                goal: source.goal.clone(),
                todos: source.todos.clone(),
                plans: source.plans.clone(),
                context: source.context.clone(),
                trust: source.trust.clone(),
                interactions: source.interactions.clone(),
                resources: source.resources.clone(),
                usage: source.usage.clone(),
            }),
        }
    }

    #[test]
    fn timeline_bootstrap_and_prepend_preserve_live_revision_and_cursor() {
        let source = projection(
            serde_json::from_value(serde_json::json!("018f7f24-1ab2-7c3d-8e4f-123456789abc"))
                .unwrap(),
            Revision(3),
        );
        let tail = timeline_page(&source, 2, true, true);
        let snapshot = snapshot_from_timeline(&tail).unwrap();
        let mut state = SessionState::new(snapshot, &tail.daemon_generation).unwrap();
        state.bounded_transcript = true;
        let mut loaded = timeline_item_ids(&tail);
        let before = timeline_page(&source, 1, false, false);

        let replacement = GetSessionUpdatesResponse {
            daemon_generation: tail.daemon_generation.clone(),
            events: vec![envelope(
                &state,
                8,
                ProjectionDelta {
                    base_revision: Revision(3),
                    revision: Revision(4),
                    changes: vec![ProjectionChange::TranscriptReplace {
                        items: vec![
                            timeline_item(8, atman_proto::TurnId(uuid::Uuid::from_u128(1))).preview,
                            timeline_item(8, atman_proto::TurnId(uuid::Uuid::from_u128(2))).preview,
                        ],
                    }],
                },
            )],
            next_cursor: EventCursor(8),
            has_more: false,
            resync_required: None,
        };
        state.apply_updates(&replacement).unwrap();
        let atman_proto::TranscriptItem::Message { message, .. } =
            &state.projection().transcript[0]
        else {
            panic!("expected message");
        };
        assert_eq!(state.projection().transcript.len(), 1);
        assert_eq!(message.turn_id.0, uuid::Uuid::from_u128(2));

        assert_eq!(merge_timeline_page(&mut state, &before, &mut loaded), 1);
        assert_eq!(state.cursor(), EventCursor(8));
        assert_eq!(state.projection().revision, Revision(4));
        assert_eq!(
            state
                .projection()
                .transcript
                .iter()
                .map(atman_proto::TranscriptItem::seq)
                .collect::<Vec<_>>(),
            vec![1, 8]
        );
    }

    #[test]
    fn exact_delta_applies_transactionally() {
        let mut state = state();
        let snapshot = Arc::as_ptr(&state.snapshot);
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
        assert_eq!(Arc::as_ptr(&state.snapshot), snapshot);
    }

    #[test]
    fn cloned_state_preserves_the_previous_snapshot_on_update() {
        let mut state = state();
        let previous = state.clone();
        assert!(Arc::ptr_eq(&state.snapshot, &previous.snapshot));
        let response = GetSessionUpdatesResponse {
            daemon_generation: state.snapshot.daemon_generation.clone(),
            events: vec![envelope(
                &state,
                8,
                ProjectionDelta {
                    base_revision: Revision(3),
                    revision: Revision(4),
                    changes: vec![ProjectionChange::GoalSet {
                        goal: Some("new goal".into()),
                    }],
                },
            )],
            next_cursor: EventCursor(8),
            has_more: false,
            resync_required: None,
        };

        state.apply_updates(&response).unwrap();

        assert!(!Arc::ptr_eq(&state.snapshot, &previous.snapshot));
        assert_eq!(state.projection().goal.as_deref(), Some("new goal"));
        assert_eq!(previous.projection().goal, None);
    }

    #[test]
    fn transcript_revision_only_advances_for_document_changes() {
        let mut state = state();
        let initial = state.transcript_revision();
        let initial_resources = state.resources_revision();
        let metadata = GetSessionUpdatesResponse {
            daemon_generation: state.snapshot.daemon_generation.clone(),
            events: vec![envelope(
                &state,
                8,
                ProjectionDelta {
                    base_revision: Revision(3),
                    revision: Revision(4),
                    changes: vec![ProjectionChange::GoalSet {
                        goal: Some("metadata only".into()),
                    }],
                },
            )],
            next_cursor: EventCursor(8),
            has_more: false,
            resync_required: None,
        };
        state.apply_updates(&metadata).unwrap();
        assert_eq!(state.transcript_revision(), initial);
        assert_eq!(state.resources_revision(), initial_resources);

        let document = GetSessionUpdatesResponse {
            daemon_generation: state.snapshot.daemon_generation.clone(),
            events: vec![envelope(
                &state,
                9,
                ProjectionDelta {
                    base_revision: Revision(4),
                    revision: Revision(5),
                    changes: vec![ProjectionChange::TranscriptAppend {
                        items: vec![atman_proto::TranscriptItem::Notice {
                            seq: 9,
                            ts: chrono::Utc::now(),
                            level: atman_proto::NoticeLevel::Info,
                            text: "durable note".into(),
                        }],
                    }],
                },
            )],
            next_cursor: EventCursor(9),
            has_more: false,
            resync_required: None,
        };
        state.apply_updates(&document).unwrap();
        assert_eq!(state.transcript_revision(), initial.wrapping_add(1));
        assert_eq!(state.resources_revision(), initial_resources);

        let resources = GetSessionUpdatesResponse {
            daemon_generation: state.snapshot.daemon_generation.clone(),
            events: vec![envelope(
                &state,
                10,
                ProjectionDelta {
                    base_revision: Revision(5),
                    revision: Revision(6),
                    changes: vec![ProjectionChange::ResourceUpsert {
                        resource: test_resource(),
                    }],
                },
            )],
            next_cursor: EventCursor(10),
            has_more: false,
            resync_required: None,
        };
        state.apply_updates(&resources).unwrap();
        assert_eq!(state.transcript_revision(), initial.wrapping_add(1));
        assert_eq!(
            state.resources_revision(),
            initial_resources.wrapping_add(1)
        );

        let heartbeat = ProjectionEventEnvelope {
            schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
            daemon_generation: state.snapshot.daemon_generation.clone(),
            session_id: state.snapshot.projection.metadata.id.clone(),
            cursor: EventCursor(11),
            ts: chrono::Utc::now(),
            event: ServerEvent::Heartbeat,
        };
        state
            .apply_updates(&GetSessionUpdatesResponse {
                daemon_generation: state.snapshot.daemon_generation.clone(),
                events: vec![heartbeat],
                next_cursor: EventCursor(11),
                has_more: false,
                resync_required: None,
            })
            .unwrap();
        assert_eq!(state.transcript_revision(), initial.wrapping_add(1));
        assert_eq!(
            state.resources_revision(),
            initial_resources.wrapping_add(1)
        );
    }

    #[test]
    fn trust_delta_updates_the_local_session_view() {
        let mut state = state();
        let trust = atman_proto::TrustProjection {
            mode: atman_proto::TrustMode::Reckless,
            ..Default::default()
        };
        let response = GetSessionUpdatesResponse {
            daemon_generation: state.snapshot.daemon_generation.clone(),
            events: vec![envelope(
                &state,
                8,
                ProjectionDelta {
                    base_revision: Revision(3),
                    revision: Revision(4),
                    changes: vec![ProjectionChange::TrustSet {
                        trust: trust.clone(),
                    }],
                },
            )],
            next_cursor: EventCursor(8),
            has_more: false,
            resync_required: None,
        };

        state.apply_updates(&response).unwrap();
        assert_eq!(state.projection().trust, trust);
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
            snapshot_schema_version: SNAPSHOT_SCHEMA_VERSION,
            event_schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
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
        let transcript_revision = session.current().transcript_revision();
        let resources_revision = session.current().resources_revision();
        let mut updates = session.subscribe_updates();

        assert_eq!(session.refresh().await.unwrap(), RefreshOutcome::Resynced);
        assert_eq!(
            session.current().projection().goal.as_deref(),
            Some("after")
        );
        assert_eq!(session.current().cursor(), EventCursor(2));
        assert_eq!(
            session.current().transcript_revision(),
            transcript_revision.wrapping_add(1)
        );
        assert_eq!(
            session.current().resources_revision(),
            resources_revision.wrapping_add(1)
        );
        assert_eq!(
            updates.recv().await.unwrap(),
            SessionUpdate::Reset(Box::new(session.current()))
        );
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
        let mut updates = session.subscribe_updates();
        let current = session.current();
        let projection_event = envelope(
            &current,
            2,
            ProjectionDelta {
                base_revision: Revision(1),
                revision: Revision(2),
                changes: vec![ProjectionChange::GoalSet {
                    goal: Some("streamed".into()),
                }],
            },
        );
        session.apply_event(projection_event).await.unwrap();
        assert_eq!(
            session.current().projection().goal.as_deref(),
            Some("streamed")
        );
        let projected_state = session.current();

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
        assert_eq!(
            updates.recv().await.unwrap(),
            SessionUpdate::Changed {
                state: projected_state,
                signals: Vec::new(),
            }
        );
        let signal_state = session.current();
        assert_eq!(
            updates.recv().await.unwrap(),
            SessionUpdate::Changed {
                state: signal_state,
                signals: vec![signal],
            }
        );
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
        let transcript_revision = session.current().transcript_revision();
        let resources_revision = session.current().resources_revision();

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
        assert_eq!(
            session.current().transcript_revision(),
            transcript_revision.wrapping_add(1)
        );
        assert_eq!(
            session.current().resources_revision(),
            resources_revision.wrapping_add(1)
        );
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
                            method_descriptor::<rpc::CreateSession>(),
                            method_descriptor::<rpc::ListSessions>(),
                            method_descriptor::<rpc::SendMessage>(),
                            method_descriptor::<rpc::StartRun>(),
                            method_descriptor::<rpc::ResolvePrompt>(),
                            method_descriptor::<rpc::SubmitForm>(),
                            method_descriptor::<rpc::ResolveCompactReview>(),
                            method_descriptor::<rpc::ListPermissionRequests>(),
                            method_descriptor::<rpc::CreatePermissionGroup>(),
                            method_descriptor::<rpc::ResolvePermissionRequests>(),
                            method_descriptor::<rpc::RenameSession>(),
                            method_descriptor::<rpc::UpdateSessionTrust>(),
                            method_descriptor::<rpc::CompactSession>(),
                            method_descriptor::<rpc::ListResources>(),
                            method_descriptor::<rpc::InspectResource>(),
                            method_descriptor::<rpc::TerminateResource>(),
                            method_descriptor::<rpc::ResizeTerminalResource>(),
                            method_descriptor::<rpc::RetainResource>(),
                            method_descriptor::<rpc::ReleaseResource>(),
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
                    methods::CREATE_SESSION => serde_json::to_value(SessionSnapshot {
                        schema_version: SNAPSHOT_SCHEMA_VERSION,
                        daemon_generation: DaemonGeneration("generation-a".into()),
                        cursor: EventCursor(1),
                        projection: projection(self.session_id.clone(), Revision(1)),
                    })?,
                    methods::LIST_SESSIONS => {
                        serde_json::to_value(vec![atman_proto::SessionSummary {
                            id: self.session_id.clone(),
                            event_count: 1,
                            message_count: 1,
                            first_ts: None,
                            updated_at: None,
                            status: atman_proto::SessionStatus::Running,
                            title: "Session".into(),
                            goal: None,
                            project_root: Some("/workspace".into()),
                            name_source: atman_proto::NameSource::Auto,
                        }])?
                    }
                    methods::SEND_MESSAGE => serde_json::to_value(SendMessageResponse {
                        session_id: self.session_id.clone(),
                        run_id: FlowRunId(
                            uuid::Uuid::parse_str("018f7f24-1ab2-7c3d-8e4f-123456789ad0").unwrap(),
                        ),
                        revision: Revision(2),
                        cursor: EventCursor(2),
                    })?,
                    methods::START_RUN => serde_json::to_value(StartRunResponse {
                        session_id: self.session_id.clone(),
                        run_id: FlowRunId(
                            uuid::Uuid::parse_str("018f7f24-1ab2-7c3d-8e4f-123456789ad1").unwrap(),
                        ),
                        revision: Revision(2),
                        cursor: EventCursor(2),
                    })?,
                    methods::RESOLVE_PROMPT => {
                        let params = request.params.as_ref().unwrap();
                        serde_json::to_value(ResolvePromptResponse {
                            resolved: true,
                            status: atman_proto::PromptResolutionStatus::Resolved,
                            session_id: self.session_id.clone(),
                            prompt_id: serde_json::from_value(params["prompt_id"].clone())?,
                            revision: Revision(2),
                            cursor: EventCursor(2),
                        })?
                    }
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
                    methods::RESOLVE_COMPACT_REVIEW => {
                        let params = request.params.as_ref().unwrap();
                        serde_json::to_value(ResolveCompactReviewResponse {
                            resolved: true,
                            status: atman_proto::CompactReviewResolutionStatus::Resolved,
                            session_id: self.session_id.clone(),
                            review_id: params["review_id"].as_str().unwrap().into(),
                            revision: Revision(2),
                            cursor: EventCursor(2),
                        })?
                    }
                    methods::LIST_PERMISSION_REQUESTS => {
                        serde_json::to_value(ListPermissionRequestsResponse {
                            session_id: self.session_id.clone(),
                            requests: Vec::new(),
                            groups: Vec::new(),
                            revision: Revision(2),
                            cursor: EventCursor(2),
                        })?
                    }
                    methods::CREATE_PERMISSION_GROUP => {
                        let params = request.params.as_ref().unwrap();
                        serde_json::to_value(CreatePermissionGroupResponse {
                            session_id: self.session_id.clone(),
                            group_id: uuid::Uuid::nil(),
                            request_ids: serde_json::from_value(params["request_ids"].clone())?,
                            revision: 1,
                            label: params["label"].as_str().unwrap().into(),
                            session_revision: Revision(2),
                            cursor: EventCursor(2),
                        })?
                    }
                    methods::RESOLVE_PERMISSION_REQUESTS => {
                        serde_json::to_value(ResolvePermissionRequestsResponse {
                            session_id: self.session_id.clone(),
                            resolutions: Vec::new(),
                            revision: Revision(2),
                            cursor: EventCursor(2),
                        })?
                    }
                    methods::RENAME_SESSION => {
                        let params = request.params.as_ref().unwrap();
                        serde_json::to_value(RenameSessionResponse {
                            session: atman_proto::SessionSummary {
                                id: self.session_id.clone(),
                                event_count: 1,
                                message_count: 1,
                                first_ts: None,
                                updated_at: None,
                                status: atman_proto::SessionStatus::Running,
                                title: params["title"]
                                    .as_str()
                                    .unwrap_or("Untitled session")
                                    .into(),
                                goal: None,
                                project_root: None,
                                name_source: atman_proto::NameSource::User,
                            },
                            revision: Revision(2),
                            cursor: EventCursor(2),
                        })?
                    }
                    methods::UPDATE_SESSION_TRUST => {
                        let params = request.params.as_ref().unwrap();
                        serde_json::to_value(atman_proto::UpdateSessionTrustResponse {
                            session_id: self.session_id.clone(),
                            trust: serde_json::from_value(params["trust"].clone())?,
                            revision: Revision(2),
                            cursor: EventCursor(2),
                        })?
                    }
                    methods::COMPACT_SESSION => {
                        serde_json::to_value(atman_proto::CompactSessionResponse {
                            session_id: self.session_id.clone(),
                            status: atman_proto::CompactionRequestStatus::Accepted,
                            operation_id: Some(atman_proto::CompactionOperationId(
                                uuid::Uuid::now_v7(),
                            )),
                            revision: Revision(2),
                            cursor: EventCursor(2),
                        })?
                    }
                    methods::LIST_RESOURCES => serde_json::to_value(ListResourcesResponse {
                        session_id: self.session_id.clone(),
                        resources: vec![test_resource()],
                        revision: Revision(2),
                        cursor: EventCursor(2),
                    })?,
                    methods::INSPECT_RESOURCE => serde_json::to_value(InspectResourceResponse {
                        session_id: self.session_id.clone(),
                        resource: test_resource(),
                        revision: Revision(2),
                        cursor: EventCursor(2),
                    })?,
                    methods::TERMINATE_RESOURCE => {
                        let params = request.params.as_ref().unwrap();
                        serde_json::to_value(TerminateResourceResponse {
                            session_id: self.session_id.clone(),
                            resource_id: serde_json::from_value(params["resource_id"].clone())?,
                            status: atman_proto::ResourceTerminationStatus::Terminating,
                            revision: Revision(2),
                            cursor: EventCursor(2),
                        })?
                    }
                    methods::RESIZE_TERMINAL_RESOURCE => {
                        let params = request.params.as_ref().unwrap();
                        serde_json::to_value(ResizeTerminalResourceResponse {
                            session_id: self.session_id.clone(),
                            resource_id: serde_json::from_value(params["resource_id"].clone())?,
                            rows: serde_json::from_value(params["rows"].clone())?,
                            cols: serde_json::from_value(params["cols"].clone())?,
                            status: atman_proto::TerminalResizeStatus::Resized,
                            revision: Revision(2),
                            cursor: EventCursor(2),
                        })?
                    }
                    methods::RETAIN_RESOURCE => serde_json::to_value(RetainResourceResponse {
                        session_id: self.session_id.clone(),
                        resource: test_resource(),
                        revision: Revision(2),
                        cursor: EventCursor(2),
                    })?,
                    methods::RELEASE_RESOURCE => serde_json::to_value(ReleaseResourceResponse {
                        session_id: self.session_id.clone(),
                        resource: test_resource(),
                        revision: Revision(2),
                        cursor: EventCursor(2),
                    })?,
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

    fn test_resource() -> atman_proto::ResourceProjection {
        atman_proto::ResourceProjection {
            id: ResourceId("task:018f7f24-1ab2-7c3d-8e4f-123456789ada".into()),
            kind: atman_proto::ResourceKind::BackgroundProcess,
            state: atman_proto::ResourceState::Running,
            owner_run_id: FlowRunId(
                uuid::Uuid::parse_str("018f7f24-1ab2-7c3d-8e4f-123456789adb").unwrap(),
            ),
            tool_use_id: None,
            label: "Inspect dependencies".into(),
            started_at: None,
            finished_at: None,
            details: Default::default(),
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
        let listed = client
            .list_sessions(Some("/workspace".into()), None, Some(10))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, session_id);
        let created = client
            .create_session(Some("/workspace".into()), Some("Session".into()))
            .await
            .unwrap();
        assert_eq!(created.session_id(), &session_id);
        assert_eq!(created.current().cursor(), EventCursor(1));
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

        let started = session
            .start_run("agent.at", None, serde_json::Map::new(), None, Vec::new())
            .await
            .unwrap();
        assert_eq!(started.session_id, *session.session_id());
        assert_eq!(started.cursor, EventCursor(2));

        let prompt_id =
            PromptId(uuid::Uuid::parse_str("018f7f24-1ab2-7c3d-8e4f-123456789ad2").unwrap());
        let prompt = session
            .resolve_prompt(prompt_id.clone(), serde_json::json!(true))
            .await
            .unwrap();
        assert_eq!(prompt.prompt_id, prompt_id);
        assert_eq!(prompt.cursor, EventCursor(2));

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

        let review = session
            .resolve_compact_review("review-1", CompactReviewDecision::AcceptAsIs)
            .await
            .unwrap();
        assert_eq!(review.review_id, "review-1");
        assert_eq!(review.cursor, EventCursor(2));

        let permissions = session.list_permissions().await.unwrap();
        assert!(permissions.requests.is_empty());
        assert_eq!(permissions.cursor, EventCursor(2));

        let resources = session.list_resources().await.unwrap();
        assert_eq!(resources.resources, vec![test_resource()]);
        let resource_id = test_resource().id;
        let inspected = session.inspect_resource(resource_id.clone()).await.unwrap();
        assert_eq!(inspected.resource.id, resource_id);
        let terminated = session
            .terminate_resource(resource_id.clone())
            .await
            .unwrap();
        assert_eq!(terminated.resource_id, resource_id);
        assert_eq!(
            terminated.status,
            atman_proto::ResourceTerminationStatus::Terminating
        );
        let resized = session
            .resize_terminal(resource_id.clone(), 42, 120)
            .await
            .unwrap();
        assert_eq!(resized.resource_id, resource_id);
        assert_eq!((resized.rows, resized.cols), (42, 120));
        assert_eq!(resized.status, atman_proto::TerminalResizeStatus::Resized);
        let retained = session.retain_resource(resource_id.clone()).await.unwrap();
        assert_eq!(retained.resource.id, resource_id);
        let released = session.release_resource(resource_id.clone()).await.unwrap();
        assert_eq!(released.resource.id, resource_id);

        let permission_id = uuid::Uuid::now_v7();
        let group = session
            .create_permission_group(
                vec![permission_id],
                std::collections::BTreeMap::from([(permission_id, 1)]),
                "shell calls",
            )
            .await
            .unwrap();
        assert_eq!(group.request_ids, vec![permission_id]);
        assert_eq!(group.cursor, EventCursor(2));

        let resolved = session
            .resolve_permissions(
                PermissionRpcSelector::Requests {
                    request_ids: vec![permission_id],
                    expected_request_revisions: std::collections::BTreeMap::from([(
                        permission_id,
                        1,
                    )]),
                },
                PermissionRpcAction::Deny,
                None,
                Some("not needed".into()),
            )
            .await
            .unwrap();
        assert_eq!(resolved.cursor, EventCursor(2));

        let renamed = session.rename("Renamed").await.unwrap();
        assert_eq!(renamed.session.title, "Renamed");
        assert_eq!(renamed.cursor, EventCursor(2));

        let cleared = session.clear_title().await.unwrap();
        assert_eq!(cleared.session.title, "Untitled session");
        assert_eq!(cleared.cursor, EventCursor(2));

        let trust = atman_proto::TrustProjection {
            mode: atman_proto::TrustMode::Eager,
            escalation: atman_proto::TrustEscalation::Allow,
            ..Default::default()
        };
        let updated = session.update_trust(trust.clone()).await.unwrap();
        assert_eq!(updated.trust, trust);
        assert_eq!(updated.cursor, EventCursor(2));

        let compacted = session.compact().await.unwrap();
        assert_eq!(
            compacted.status,
            atman_proto::CompactionRequestStatus::Accepted
        );
        assert_eq!(compacted.cursor, EventCursor(2));
    }
}
