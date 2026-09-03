use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use atman_proto::{
    CompactReviewDecision, CompactReviewResolutionStatus, CreatePermissionGroupResponse,
    DaemonGeneration, EventCursor, FlowRunId, FormResolutionStatus, FormSubmission,
    GetSessionUpdatesResponse, ListPermissionRequestsResponse, NotificationLifecycle,
    NotificationLocation, NotificationStack, PROJECTION_EVENT_SCHEMA_VERSION, PermissionGroupView,
    PermissionRequestView, PermissionResolutionView, ProjectionDelta, ProjectionEventEnvelope,
    PromptId, PromptResolutionStatus, ResolvePermissionRequestsResponse, ResourceId, ResourceKind,
    ResourceState, ResourceTerminationStatus, ResyncRequired, RunCancellationStatus, ServerEvent,
    SessionId, SessionNotification, SessionProjection, SessionSignal, SessionSummary,
};
use atman_runtime::stream::StreamFrame;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::projection::{RestoredProjection, SessionProjector};
use crate::state::LiveRun;

const UPDATE_RETENTION: usize = 2_048;
const MAX_UPDATE_PAGE_SIZE: usize = 1_000;
const PROMPT_TERMINAL_RETENTION: usize = 256;
const FORM_TERMINAL_RETENTION: usize = 256;
const COMPACT_REVIEW_TERMINAL_RETENTION: usize = 256;
const LEASES_CLOSING: usize = 1 << (usize::BITS - 1);
const LEASE_COUNT_MASK: usize = !LEASES_CLOSING;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunAdmission {
    Concurrent,
    IdleSession,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionActorView {
    pub revision: u64,
    pub projection_revision: atman_proto::Revision,
    pub runtime_event_seq: u64,
    pub runs: HashMap<FlowRunId, LiveRunView>,
    pub idle_since: Option<std::time::Instant>,
}

#[derive(Debug, Clone)]
pub(crate) struct LiveRunView {
    pub started_at: chrono::DateTime<chrono::Utc>,
}

pub(crate) struct InterjectionCommit {
    pub injection_id: uuid::Uuid,
    pub revision: atman_proto::Revision,
    pub cursor: EventCursor,
}

pub struct RunCancellationCommit {
    pub status: RunCancellationStatus,
    pub revision: atman_proto::Revision,
    pub cursor: EventCursor,
}

pub(crate) struct PromptResolutionCommit {
    pub status: PromptResolutionStatus,
    pub revision: atman_proto::Revision,
    pub cursor: EventCursor,
}

pub(crate) struct FormResolutionCommit {
    pub status: FormResolutionStatus,
    pub revision: atman_proto::Revision,
    pub cursor: EventCursor,
}

pub(crate) struct CompactReviewResolutionCommit {
    pub status: CompactReviewResolutionStatus,
    pub revision: atman_proto::Revision,
    pub cursor: EventCursor,
}

pub struct RenameSessionCommit {
    pub session: SessionSummary,
    pub revision: atman_proto::Revision,
    pub cursor: EventCursor,
}

pub(crate) struct ResourceTerminationCommit {
    pub status: ResourceTerminationStatus,
    pub revision: atman_proto::Revision,
    pub cursor: EventCursor,
}

pub(crate) struct ResourceMutationCommit {
    pub resource: atman_proto::ResourceProjection,
    pub revision: atman_proto::Revision,
    pub cursor: EventCursor,
}

#[derive(Clone, Copy)]
enum WorkspaceMutationAction {
    Retain,
    Release,
}

struct PendingPrompt {
    responder: oneshot::Sender<serde_json::Value>,
}

impl SessionActorView {
    pub fn is_live(&self) -> bool {
        !self.runs.is_empty()
    }

    pub fn first_run_started_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.runs.values().map(|run| run.started_at).min()
    }

    pub fn has_been_idle_for(&self, duration: std::time::Duration) -> bool {
        self.idle_since
            .is_some_and(|idle_since| idle_since.elapsed() >= duration)
    }
}

#[derive(Clone)]
pub(crate) struct SessionActorHandle {
    actor_id: uuid::Uuid,
    session: Arc<atman_runtime::Session>,
    owner_principal: Arc<str>,
    tx: mpsc::UnboundedSender<Command>,
    view: watch::Receiver<SessionActorView>,
    leases: Arc<AtomicUsize>,
}

pub(crate) struct SessionActorLease {
    handle: SessionActorHandle,
}

impl std::ops::Deref for SessionActorLease {
    type Target = SessionActorHandle;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl Drop for SessionActorLease {
    fn drop(&mut self) {
        let previous = self.handle.leases.fetch_sub(1, Ordering::Release);
        debug_assert!(previous & LEASE_COUNT_MASK != 0);
    }
}

impl SessionActorHandle {
    pub fn spawn(
        session_id: SessionId,
        session: Arc<atman_runtime::Session>,
        initial_runs: Vec<LiveRun>,
        owner_principal: String,
        daemon_generation: DaemonGeneration,
        restored_projection: Option<RestoredProjection>,
        task_registry: atman_runtime::TaskRegistry,
    ) -> Self {
        let actor_id = uuid::Uuid::now_v7();
        let workspace_service =
            session
                .meta()
                .and_then(|meta| meta.project_root)
                .map(|project_root| {
                    atman_runtime::flow_workspace::FlowWorkspaceService::new(
                        project_root,
                        None,
                        &daemon_generation.0,
                    )
                    .expect("daemon generation was validated when daemon state was created")
                });
        let events_rx = session.sink().subscribe();
        let stream_rx = session.stream_subscribe();
        let goal_rx = session.subscribe_goal();
        let todos_rx = session.subscribe_todos();
        let plans_rx = session.subscribe_plans();
        let context_rx = session.subscribe_context();
        let trust_rx = session.subscribe_trust();
        let forms_rx = session.forms().subscribe();
        let compact_review_rx = session.compact_reviews().subscribe();
        let (mut projection, restored_event_cursor) = match restored_projection {
            Some(restored) => (restored.projector, Some(restored.event_cursor)),
            None => (
                SessionProjector::from_events(
                    session_id.clone(),
                    session.meta(),
                    &session.sink().snapshot_envelopes(),
                ),
                None,
            ),
        };
        projection.set_goal(goal_rx.borrow().clone());
        projection.set_todos(todos_rx.borrow().clone());
        projection.set_plans(plans_rx.borrow().clone());
        projection.set_context(context_rx.borrow().clone());
        projection.set_trust(trust_rx.borrow().clone());
        projection.set_forms(forms_rx.borrow().clone());
        projection.set_compact_review(compact_review_rx.borrow().clone());
        for run in &initial_runs {
            projection.register_run(run.run_id.clone(), run.flow_name.clone(), run.started_at);
        }
        let projection_cursor = EventCursor(projection.projection().revision.0);
        let event_cursor = restored_event_cursor
            .map(|cursor| EventCursor(cursor.0.max(projection_cursor.0)))
            .unwrap_or(projection_cursor);
        let leases = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::unbounded_channel();
        let (updates_tx, _) = broadcast::channel(UPDATE_RETENTION);
        let runs: HashMap<_, _> = initial_runs
            .into_iter()
            .map(|run| (run.run_id.clone(), run))
            .collect();
        let idle_since = runs.is_empty().then(std::time::Instant::now);
        let (view_tx, view) = watch::channel(view_for(1, &runs, &projection, idle_since));
        let actor = SessionActor {
            session_id,
            session: session.clone(),
            runs,
            prompts: HashMap::new(),
            prompt_terminals: VecDeque::new(),
            form_terminals: VecDeque::new(),
            compact_review_terminals: VecDeque::new(),
            revision: 1,
            idle_since,
            projection,
            event_cursor,
            daemon_generation,
            updates: VecDeque::new(),
            updates_tx,
            view_tx,
            rx,
            events_rx,
            stream_rx,
            goal_rx,
            todos_rx,
            plans_rx,
            context_rx,
            trust_rx,
            forms_rx,
            compact_review_rx,
            task_registry,
            workspace_service,
            workspace_mutations: HashSet::new(),
            leases: leases.clone(),
            command_tx: tx.clone(),
            _permission_client: session.permission_broker().register_client(),
        };
        tokio::spawn(actor.run());
        Self {
            actor_id,
            session,
            owner_principal: owner_principal.into(),
            tx,
            view,
            leases,
        }
    }

    pub fn owns(&self, principal: &str) -> bool {
        self.owner_principal.as_ref() == principal
    }

    pub fn owns_session(&self, session: &Arc<atman_runtime::Session>) -> bool {
        Arc::ptr_eq(&self.session, session)
    }

    pub fn is_same_actor(&self, other: &Self) -> bool {
        self.actor_id == other.actor_id
    }

    pub fn lease(&self) -> Result<SessionActorLease> {
        let mut current = self.leases.load(Ordering::Acquire);
        loop {
            if current & LEASES_CLOSING != 0 {
                anyhow::bail!("session actor is closing");
            }
            anyhow::ensure!(
                current & LEASE_COUNT_MASK != LEASE_COUNT_MASK,
                "session actor lease count overflow"
            );
            let next = current + 1;
            match self.leases.compare_exchange_weak(
                current,
                next,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(SessionActorLease {
                        handle: self.clone(),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    pub fn view(&self) -> SessionActorView {
        self.view.borrow().clone()
    }

    pub async fn add_run(&self, run: LiveRun, admission: RunAdmission) -> Result<()> {
        request(&self.tx, |reply| Command::AddRun {
            run,
            admission,
            reply,
        })
        .await?
    }

    pub fn runtime_session(&self) -> Arc<atman_runtime::Session> {
        self.session.clone()
    }

    pub fn finish_run(&self, run_id: FlowRunId) -> bool {
        self.tx.send(Command::FinishRun { run_id }).is_ok()
    }

    pub async fn cancel_run(&self, run_id: FlowRunId) -> Result<RunCancellationCommit> {
        request(&self.tx, |reply| Command::CancelRun { run_id, reply }).await?
    }

    pub async fn interject(
        &self,
        run_id: FlowRunId,
        text: String,
        level: atman_runtime::injection::InjectionLevel,
        redirect_target: Option<String>,
    ) -> Result<InterjectionCommit> {
        request(&self.tx, |reply| Command::Interject {
            run_id,
            text,
            level,
            redirect_target,
            reply,
        })
        .await?
    }

    pub fn register_prompt(
        &self,
        id: PromptId,
        kind: String,
        payload: serde_json::Value,
    ) -> oneshot::Receiver<serde_json::Value> {
        let (responder, receiver) = oneshot::channel();
        let Ok(lease) = self.lease() else {
            return receiver;
        };
        let _ = self.tx.send(Command::RegisterPrompt {
            id,
            kind,
            payload,
            responder,
            _lease: lease,
        });
        receiver
    }

    pub fn drop_prompt(&self, id: PromptId) {
        if let Ok(lease) = self.lease() {
            let _ = self.tx.send(Command::DropPrompt { id, _lease: lease });
        }
    }

    pub async fn resolve_prompt(
        &self,
        id: PromptId,
        answer: serde_json::Value,
    ) -> Result<PromptResolutionCommit> {
        request(&self.tx, |reply| Command::ResolvePrompt {
            id,
            answer,
            reply,
        })
        .await?
    }

    pub async fn submit_form(
        &self,
        id: String,
        submission: FormSubmission,
    ) -> Result<FormResolutionCommit> {
        request(&self.tx, |reply| Command::SubmitForm {
            id,
            submission,
            reply,
        })
        .await?
    }

    pub async fn resolve_compact_review(
        &self,
        id: String,
        decision: CompactReviewDecision,
    ) -> Result<CompactReviewResolutionCommit> {
        request(&self.tx, |reply| Command::ResolveCompactReview {
            id,
            decision,
            reply,
        })
        .await?
    }

    pub async fn rename(&self, title: String) -> Result<RenameSessionCommit> {
        request(&self.tx, |reply| Command::Rename { title, reply }).await?
    }

    pub async fn terminate_resource(
        &self,
        resource_id: ResourceId,
    ) -> Result<ResourceTerminationCommit> {
        request(&self.tx, |reply| Command::TerminateResource {
            resource_id,
            reply,
        })
        .await?
    }

    pub async fn retain_resource(&self, resource_id: ResourceId) -> Result<ResourceMutationCommit> {
        request(&self.tx, |reply| Command::RetainResource {
            resource_id,
            reply,
        })
        .await?
    }

    pub async fn release_resource(
        &self,
        resource_id: ResourceId,
    ) -> Result<ResourceMutationCommit> {
        request(&self.tx, |reply| Command::ReleaseResource {
            resource_id,
            reply,
        })
        .await?
    }

    pub async fn try_unload(&self) -> Result<bool> {
        request(&self.tx, |reply| Command::TryUnload { reply }).await?
    }

    pub async fn begin_shutdown(&self) -> Result<()> {
        request(&self.tx, |reply| Command::BeginShutdown { reply }).await?
    }

    pub async fn force_shutdown(&self) -> Result<()> {
        request(&self.tx, |reply| Command::ForceShutdown { reply }).await?
    }

    pub async fn snapshot(&self) -> Result<(EventCursor, SessionProjection)> {
        let (cursor, projection) = request(&self.tx, |reply| Command::Snapshot { reply }).await??;
        let redactor = self.session.sink().redactor();
        Ok((
            cursor,
            crate::projection::redacted_projection(&projection, redactor.as_deref())?,
        ))
    }

    pub async fn updates(
        &self,
        after_cursor: EventCursor,
        limit: Option<usize>,
    ) -> Result<GetSessionUpdatesResponse> {
        let updates = request(&self.tx, |reply| Command::Updates {
            after_cursor,
            limit,
            reply,
        })
        .await??;
        let redactor = self.session.sink().redactor();
        crate::projection::redacted_updates(&updates, redactor.as_deref())
    }

    pub async fn subscribe_updates(
        &self,
    ) -> Result<(
        broadcast::Receiver<ProjectionEventEnvelope>,
        Option<Arc<atman_runtime::redact::Redactor>>,
    )> {
        let receiver = request(&self.tx, |reply| Command::SubscribeUpdates { reply }).await?;
        Ok((receiver, self.session.sink().redactor()))
    }
    pub async fn list_permissions(&self) -> Result<ListPermissionRequestsResponse> {
        request(&self.tx, |reply| Command::ListPermissions { reply }).await
    }

    pub async fn create_permission_group(
        &self,
        request_ids: Vec<uuid::Uuid>,
        expected_request_revisions: std::collections::BTreeMap<uuid::Uuid, u64>,
        label: String,
    ) -> Result<CreatePermissionGroupResponse> {
        request(&self.tx, |reply| Command::CreatePermissionGroup {
            request_ids,
            expected_request_revisions,
            label,
            reply,
        })
        .await?
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn resolve_permissions(
        &self,
        request_ids: Vec<uuid::Uuid>,
        expected_request_revisions: std::collections::BTreeMap<uuid::Uuid, u64>,
        group: Option<(uuid::Uuid, u64)>,
        action: atman_runtime::permission::PermissionAction,
        scope: Option<atman_runtime::permission::GrantScope>,
        reason: Option<String>,
        principal_id: String,
    ) -> Result<ResolvePermissionRequestsResponse> {
        request(&self.tx, |reply| Command::ResolvePermissions {
            request_ids,
            expected_request_revisions,
            group,
            action,
            scope,
            reason,
            principal_id,
            reply,
        })
        .await?
    }
}

async fn request<T>(
    tx: &mpsc::UnboundedSender<Command>,
    command: impl FnOnce(oneshot::Sender<T>) -> Command,
) -> Result<T> {
    let (reply, response) = oneshot::channel();
    tx.send(command(reply))
        .map_err(|_| anyhow::anyhow!("session actor stopped"))?;
    response
        .await
        .map_err(|_| anyhow::anyhow!("session actor stopped before replying"))
}

enum Command {
    AddRun {
        run: LiveRun,
        admission: RunAdmission,
        reply: oneshot::Sender<Result<()>>,
    },
    FinishRun {
        run_id: FlowRunId,
    },
    CancelRun {
        run_id: FlowRunId,
        reply: oneshot::Sender<Result<RunCancellationCommit>>,
    },
    Interject {
        run_id: FlowRunId,
        text: String,
        level: atman_runtime::injection::InjectionLevel,
        redirect_target: Option<String>,
        reply: oneshot::Sender<Result<InterjectionCommit>>,
    },
    RegisterPrompt {
        id: PromptId,
        kind: String,
        payload: serde_json::Value,
        responder: oneshot::Sender<serde_json::Value>,
        _lease: SessionActorLease,
    },
    DropPrompt {
        id: PromptId,
        _lease: SessionActorLease,
    },
    ResolvePrompt {
        id: PromptId,
        answer: serde_json::Value,
        reply: oneshot::Sender<Result<PromptResolutionCommit>>,
    },
    SubmitForm {
        id: String,
        submission: FormSubmission,
        reply: oneshot::Sender<Result<FormResolutionCommit>>,
    },
    ResolveCompactReview {
        id: String,
        decision: CompactReviewDecision,
        reply: oneshot::Sender<Result<CompactReviewResolutionCommit>>,
    },
    Rename {
        title: String,
        reply: oneshot::Sender<Result<RenameSessionCommit>>,
    },
    TerminateResource {
        resource_id: ResourceId,
        reply: oneshot::Sender<Result<ResourceTerminationCommit>>,
    },
    RetainResource {
        resource_id: ResourceId,
        reply: oneshot::Sender<Result<ResourceMutationCommit>>,
    },
    ReleaseResource {
        resource_id: ResourceId,
        reply: oneshot::Sender<Result<ResourceMutationCommit>>,
    },
    TryUnload {
        reply: oneshot::Sender<Result<bool>>,
    },
    BeginShutdown {
        reply: oneshot::Sender<Result<()>>,
    },
    ForceShutdown {
        reply: oneshot::Sender<Result<()>>,
    },
    WorkspaceMutationFinished {
        action: WorkspaceMutationAction,
        resource_id: ResourceId,
        owner_run_id: FlowRunId,
        result: Box<Result<atman_runtime::git_workspace::WorkspaceRecord, String>>,
        reply: oneshot::Sender<Result<ResourceMutationCommit>>,
    },
    Snapshot {
        reply: oneshot::Sender<Result<(EventCursor, SessionProjection)>>,
    },
    Updates {
        after_cursor: EventCursor,
        limit: Option<usize>,
        reply: oneshot::Sender<Result<GetSessionUpdatesResponse>>,
    },
    SubscribeUpdates {
        reply: oneshot::Sender<broadcast::Receiver<ProjectionEventEnvelope>>,
    },
    ListPermissions {
        reply: oneshot::Sender<ListPermissionRequestsResponse>,
    },
    CreatePermissionGroup {
        request_ids: Vec<uuid::Uuid>,
        expected_request_revisions: std::collections::BTreeMap<uuid::Uuid, u64>,
        label: String,
        reply: oneshot::Sender<Result<CreatePermissionGroupResponse>>,
    },
    ResolvePermissions {
        request_ids: Vec<uuid::Uuid>,
        expected_request_revisions: std::collections::BTreeMap<uuid::Uuid, u64>,
        group: Option<(uuid::Uuid, u64)>,
        action: atman_runtime::permission::PermissionAction,
        scope: Option<atman_runtime::permission::GrantScope>,
        reason: Option<String>,
        principal_id: String,
        reply: oneshot::Sender<Result<ResolvePermissionRequestsResponse>>,
    },
}

enum ActorInput {
    Command(Option<Command>),
    Event(Box<Result<atman_runtime::event::EventEnvelope, broadcast::error::RecvError>>),
    Signal(Box<Result<StreamFrame, broadcast::error::RecvError>>),
    Goal(Result<(), watch::error::RecvError>),
    Todos(Result<(), watch::error::RecvError>),
    Plans(Result<(), watch::error::RecvError>),
    Context(Result<(), watch::error::RecvError>),
    Trust(Result<(), watch::error::RecvError>),
    Forms(Result<(), watch::error::RecvError>),
    CompactReview(Result<(), watch::error::RecvError>),
}

struct SessionActor {
    session_id: SessionId,
    session: Arc<atman_runtime::Session>,
    runs: HashMap<FlowRunId, LiveRun>,
    prompts: HashMap<PromptId, PendingPrompt>,
    prompt_terminals: VecDeque<(PromptId, PromptResolutionStatus)>,
    form_terminals: VecDeque<(String, FormResolutionStatus)>,
    compact_review_terminals: VecDeque<(String, CompactReviewResolutionStatus)>,
    revision: u64,
    idle_since: Option<std::time::Instant>,
    projection: SessionProjector,
    event_cursor: EventCursor,
    daemon_generation: DaemonGeneration,
    updates: VecDeque<ProjectionEventEnvelope>,
    updates_tx: broadcast::Sender<ProjectionEventEnvelope>,
    view_tx: watch::Sender<SessionActorView>,
    rx: mpsc::UnboundedReceiver<Command>,
    events_rx: broadcast::Receiver<atman_runtime::event::EventEnvelope>,
    stream_rx: broadcast::Receiver<StreamFrame>,
    goal_rx: watch::Receiver<Option<String>>,
    todos_rx: watch::Receiver<Vec<atman_runtime::memory::todo::Todo>>,
    plans_rx: watch::Receiver<Vec<atman_runtime::memory::plan::Plan>>,
    context_rx: watch::Receiver<atman_runtime::ContextSnapshot>,
    trust_rx: watch::Receiver<atman_runtime::trust::TrustConfig>,
    forms_rx: watch::Receiver<Vec<atman_runtime::form::PendingForm>>,
    compact_review_rx: watch::Receiver<Option<atman_runtime::session::PendingCompactReview>>,
    task_registry: atman_runtime::TaskRegistry,
    workspace_service: Option<atman_runtime::flow_workspace::FlowWorkspaceService>,
    workspace_mutations: HashSet<ResourceId>,
    leases: Arc<AtomicUsize>,
    command_tx: mpsc::UnboundedSender<Command>,
    _permission_client: atman_runtime::permission::PermissionClientGuard,
}

impl SessionActor {
    async fn run(mut self) {
        loop {
            let input = tokio::select! {
                command = self.rx.recv() => ActorInput::Command(command),
                event = self.events_rx.recv() => ActorInput::Event(Box::new(event)),
                signal = self.stream_rx.recv() => ActorInput::Signal(Box::new(signal)),
                changed = self.goal_rx.changed() => ActorInput::Goal(changed),
                changed = self.todos_rx.changed() => ActorInput::Todos(changed),
                changed = self.plans_rx.changed() => ActorInput::Plans(changed),
                changed = self.context_rx.changed() => ActorInput::Context(changed),
                changed = self.trust_rx.changed() => ActorInput::Trust(changed),
                changed = self.forms_rx.changed() => ActorInput::Forms(changed),
                changed = self.compact_review_rx.changed() => ActorInput::CompactReview(changed),
            };
            self.record_activity();
            match input {
                ActorInput::Command(None) => break,
                ActorInput::Command(Some(Command::TryUnload { reply })) => {
                    let result = self.prepare_unload();
                    let should_stop = matches!(result, Ok(true));
                    if should_stop {
                        if let Err(error) = self.persist_projection_snapshot().await {
                            eprintln!(
                                "warning: failed to persist projection snapshot for {}: {error:#}",
                                self.session_id
                            );
                        }
                        self.session.shutdown().await;
                        self.task_registry
                            .unbind_session(&self.session_id.to_string());
                    }
                    let _ = reply.send(result);
                    if should_stop {
                        break;
                    }
                }
                ActorInput::Command(Some(Command::ForceShutdown { reply })) => {
                    let result = self.prepare_forced_shutdown();
                    if let Err(error) = self.persist_projection_snapshot().await {
                        eprintln!(
                            "warning: failed to persist projection snapshot for {}: {error:#}",
                            self.session_id
                        );
                    }
                    self.session.shutdown().await;
                    self.task_registry
                        .unbind_session(&self.session_id.to_string());
                    let _ = reply.send(result);
                    break;
                }
                ActorInput::Command(Some(command)) => self.handle_command(command),
                ActorInput::Event(event) => match *event {
                    Ok(event) => self.apply_runtime_event(&event),
                    Err(broadcast::error::RecvError::Lagged(_)) => self.catch_up_projection(),
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                ActorInput::Signal(signal) => match *signal {
                    Ok(frame) => self.apply_runtime_signal(frame),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                ActorInput::Goal(Ok(()))
                | ActorInput::Todos(Ok(()))
                | ActorInput::Plans(Ok(()))
                | ActorInput::Context(Ok(()))
                | ActorInput::Trust(Ok(()))
                | ActorInput::Forms(Ok(()))
                | ActorInput::CompactReview(Ok(())) => self.refresh_watch_projections(),
                ActorInput::Goal(Err(_))
                | ActorInput::Todos(Err(_))
                | ActorInput::Plans(Err(_))
                | ActorInput::Context(Err(_))
                | ActorInput::Trust(Err(_))
                | ActorInput::Forms(Err(_))
                | ActorInput::CompactReview(Err(_)) => break,
            }
        }
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::AddRun {
                run,
                admission,
                reply,
            } => {
                let result = if self.runs.contains_key(&run.run_id) {
                    Err(anyhow::anyhow!("run {} is already registered", run.run_id))
                } else if admission == RunAdmission::IdleSession && !self.runs.is_empty() {
                    Err(anyhow::anyhow!(
                        "session {} already has an active root run",
                        self.session_id
                    ))
                } else {
                    let delta = self.projection.register_run(
                        run.run_id.clone(),
                        run.flow_name.clone(),
                        run.started_at,
                    );
                    self.runs.insert(run.run_id.clone(), run);
                    if let Some(delta) = delta {
                        self.publish_projection_delta(delta);
                    } else {
                        self.publish();
                    }
                    Ok(())
                };
                let _ = reply.send(result);
            }
            Command::FinishRun { run_id } => {
                if self.runs.remove(&run_id).is_some() {
                    self.publish();
                }
            }
            Command::CancelRun { run_id, reply } => {
                let result = self.cancel_run(run_id);
                let _ = reply.send(result);
            }
            Command::Interject {
                run_id,
                text,
                level,
                redirect_target,
                reply,
            } => {
                let result = self.interject(run_id, text, level, redirect_target);
                let _ = reply.send(result);
            }
            Command::RegisterPrompt {
                id,
                kind,
                payload,
                responder,
                _lease: _,
            } => self.register_prompt(id, kind, payload, responder),
            Command::DropPrompt { id, _lease: _ } => self.drop_prompt(id),
            Command::ResolvePrompt { id, answer, reply } => {
                let result = self.resolve_prompt(id, answer);
                let _ = reply.send(result);
            }
            Command::SubmitForm {
                id,
                submission,
                reply,
            } => {
                let result = self.submit_form(id, submission);
                let _ = reply.send(result);
            }
            Command::ResolveCompactReview {
                id,
                decision,
                reply,
            } => {
                let result = self.resolve_compact_review(id, decision);
                let _ = reply.send(result);
            }
            Command::Rename { title, reply } => {
                let result = self.rename(title);
                let _ = reply.send(result);
            }
            Command::TerminateResource { resource_id, reply } => {
                let result = self.terminate_resource(resource_id);
                let _ = reply.send(result);
            }
            Command::RetainResource { resource_id, reply } => {
                self.begin_workspace_mutation(WorkspaceMutationAction::Retain, resource_id, reply);
            }
            Command::ReleaseResource { resource_id, reply } => {
                self.begin_workspace_mutation(WorkspaceMutationAction::Release, resource_id, reply);
            }
            Command::WorkspaceMutationFinished {
                action,
                resource_id,
                owner_run_id,
                result,
                reply,
            } => {
                let result =
                    self.finish_workspace_mutation(action, resource_id, owner_run_id, *result);
                let _ = reply.send(result);
            }
            Command::TryUnload { .. } => unreachable!("try_unload is handled by the actor loop"),
            Command::BeginShutdown { reply } => {
                let result = self.begin_shutdown();
                let _ = reply.send(result);
            }
            Command::ForceShutdown { .. } => {
                unreachable!("force_shutdown is handled by the actor loop")
            }
            Command::Snapshot { reply } => {
                self.refresh_watch_projections();
                let target_seq = self.session.sink().next_seq_peek().saturating_sub(1);
                let result = self
                    .catch_up_through(target_seq)
                    .map(|()| (self.event_cursor, self.projection.snapshot()));
                let _ = reply.send(result);
            }
            Command::Updates {
                after_cursor,
                limit,
                reply,
            } => {
                self.refresh_watch_projections();
                let target_seq = self.session.sink().next_seq_peek().saturating_sub(1);
                let result = self
                    .catch_up_through(target_seq)
                    .map(|()| self.updates_response(after_cursor, limit));
                let _ = reply.send(result);
            }
            Command::SubscribeUpdates { reply } => {
                let _ = reply.send(self.updates_tx.subscribe());
            }
            Command::ListPermissions { reply } => {
                let _ = reply.send(self.list_permissions());
            }
            Command::CreatePermissionGroup {
                request_ids,
                expected_request_revisions,
                label,
                reply,
            } => {
                let result =
                    self.create_permission_group(request_ids, expected_request_revisions, label);
                let _ = reply.send(result);
            }
            Command::ResolvePermissions {
                request_ids,
                expected_request_revisions,
                group,
                action,
                scope,
                reason,
                principal_id,
                reply,
            } => {
                let result = self.resolve_permissions(
                    request_ids,
                    expected_request_revisions,
                    group,
                    action,
                    scope,
                    reason,
                    principal_id,
                );
                let _ = reply.send(result);
            }
        }
    }

    fn publish(&mut self) {
        if self.runs.is_empty() {
            self.idle_since.get_or_insert_with(std::time::Instant::now);
        } else {
            self.idle_since = None;
        }
        self.revision = self.revision.saturating_add(1);
        self.view_tx.send_replace(view_for(
            self.revision,
            &self.runs,
            &self.projection,
            self.idle_since,
        ));
    }

    fn record_activity(&mut self) {
        if self.runs.is_empty() {
            let idle_since = std::time::Instant::now();
            self.idle_since = Some(idle_since);
            self.view_tx.send_modify(|view| {
                view.idle_since = Some(idle_since);
            });
        }
    }

    fn prepare_unload(&mut self) -> Result<bool> {
        let interactions = &self.projection.projection().interactions;
        let has_pending_interactions = !self.prompts.is_empty()
            || !interactions.prompts.is_empty()
            || !interactions.forms.is_empty()
            || interactions.compact_review.is_some()
            || interactions.approvals.iter().any(|approval| {
                matches!(
                    approval.state,
                    atman_proto::ApprovalState::Evaluating | atman_proto::ApprovalState::Pending
                )
            })
            || interactions
                .interjections
                .iter()
                .any(|interjection| interjection.state == atman_proto::InterjectionState::Pending);
        let session_id = self.session_id.to_string();
        if !self.runs.is_empty()
            || has_pending_interactions
            || !self.workspace_mutations.is_empty()
            || self.updates_tx.receiver_count() != 0
            || self.task_registry.has_running_in_session(&session_id)
        {
            return Ok(false);
        }
        let leases = self.leases.load(Ordering::Acquire);
        if leases != LEASES_CLOSING
            && self
                .leases
                .compare_exchange(0, LEASES_CLOSING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Ok(false);
        }
        let target_seq = self.session.sink().next_seq_peek().saturating_sub(1);
        if let Err(error) = self.catch_up_through(target_seq) {
            if leases == 0 {
                self.leases.store(0, Ordering::Release);
            }
            return Err(error);
        }
        Ok(true)
    }

    async fn persist_projection_snapshot(&mut self) -> Result<()> {
        let watermark = self
            .session
            .flush_writer()
            .await
            .ok_or_else(|| anyhow::anyhow!("session event writer is not running"))?;
        self.catch_up_through(watermark.seq)?;
        let session_id = self.session_id.clone();
        let session_dir = self.session.dir().to_path_buf();
        let projector = std::mem::replace(
            &mut self.projection,
            SessionProjector::new(self.session_id.clone(), None),
        );
        let event_cursor = self.event_cursor;
        let redactor = self.session.sink().redactor();
        tokio::task::spawn_blocking(move || {
            crate::projection_snapshot::save(
                &session_id,
                &session_dir,
                watermark,
                event_cursor,
                &projector,
                redactor.as_deref(),
            )
        })
        .await
        .context("join projection snapshot writer")?
    }

    fn begin_shutdown(&mut self) -> Result<()> {
        self.leases.fetch_or(LEASES_CLOSING, Ordering::AcqRel);
        if let Some(delta) = self
            .projection
            .set_lifecycle(atman_proto::SessionLifecycle::Closing)
        {
            self.publish_projection_delta(delta);
        }

        for run in self.runs.values() {
            run.cancel.cancel();
        }
        let task_filter = atman_runtime::TaskFilter {
            session_id: Some(self.session_id.to_string()),
            ..Default::default()
        };
        for task in self.task_registry.list(&task_filter) {
            if task.is_running() {
                self.task_registry.kill_from_operator(&task.id);
            }
        }
        let prompt_ids = self.prompts.keys().cloned().collect::<Vec<_>>();
        for prompt_id in prompt_ids {
            self.drop_prompt(prompt_id);
        }
        self.session.forms().cancel_all();
        if let Some(review) = self.session.compact_reviews().list_pending() {
            self.session.compact_reviews().decide(
                &review.review_id,
                atman_runtime::session::CompactReviewDecision::Reject,
            );
        }
        let target_seq = self.session.sink().next_seq_peek().saturating_sub(1);
        self.catch_up_through(target_seq)
    }

    fn prepare_forced_shutdown(&mut self) -> Result<()> {
        self.begin_shutdown()?;
        if let Some(delta) = self
            .projection
            .set_lifecycle(atman_proto::SessionLifecycle::Closed)
        {
            self.publish_projection_delta(delta);
        }
        let target_seq = self.session.sink().next_seq_peek().saturating_sub(1);
        self.catch_up_through(target_seq)
    }

    fn interject(
        &mut self,
        run_id: FlowRunId,
        text: String,
        level: atman_runtime::injection::InjectionLevel,
        redirect_target: Option<String>,
    ) -> Result<InterjectionCommit> {
        let run = self.runs.get(&run_id).ok_or_else(|| {
            anyhow::anyhow!("run {run_id} is not active in session {}", self.session_id)
        })?;
        let cancel = run.cancel.clone();
        let (injection_id, event) = self
            .session
            .enqueue_injection_for_run(
                text,
                level,
                redirect_target,
                Some(atman_runtime::event::FlowRunId(run_id.0)),
            )
            .map_err(anyhow::Error::from)?;
        self.catch_up_through(event.seq)?;
        if level == atman_runtime::injection::InjectionLevel::L4HardStop {
            cancel.cancel();
        }
        Ok(InterjectionCommit {
            injection_id: injection_id.0,
            revision: self.projection.projection().revision,
            cursor: self.event_cursor,
        })
    }

    fn cancel_run(&mut self, run_id: FlowRunId) -> Result<RunCancellationCommit> {
        let status = match self.runs.get(&run_id) {
            None => RunCancellationStatus::NotFound,
            Some(run) if run.cancel.is_cancelled() => RunCancellationStatus::AlreadyRequested,
            Some(run) => {
                let cancel = run.cancel.clone();
                let event = self.session.sink().emit_returning_envelope(
                    atman_runtime::event::Event::RunCancelRequested {
                        run_id: atman_runtime::event::FlowRunId(run_id.0),
                    },
                );
                self.catch_up_through(event.seq)?;
                cancel.cancel();
                RunCancellationStatus::Accepted
            }
        };
        Ok(RunCancellationCommit {
            status,
            revision: self.projection.projection().revision,
            cursor: self.event_cursor,
        })
    }

    fn register_prompt(
        &mut self,
        id: PromptId,
        kind: String,
        payload: serde_json::Value,
        responder: oneshot::Sender<serde_json::Value>,
    ) {
        if self.prompts.contains_key(&id) {
            atman_runtime::notify!(error, "duplicate pending prompt id {id}");
            return;
        }
        self.prompts.insert(id.clone(), PendingPrompt { responder });
        let event = self.session.sink().emit_returning_envelope(
            atman_runtime::event::Event::PendingPrompt {
                prompt_id: id.0,
                kind,
                payload,
            },
        );
        if let Err(error) = self.catch_up_through(event.seq) {
            self.prompts.remove(&id);
            atman_runtime::notify!(error, "project pending prompt {id} failed: {error:#}");
        }
    }

    fn drop_prompt(&mut self, id: PromptId) {
        let Some(entry) = self.prompts.remove(&id) else {
            return;
        };
        drop(entry);
        let event = self.session.sink().emit_returning_envelope(
            atman_runtime::event::Event::PromptResolved {
                prompt_id: id.0,
                answer: serde_json::Value::Null,
            },
        );
        if let Err(error) = self.catch_up_through(event.seq) {
            atman_runtime::notify!(error, "project abandoned prompt {id} failed: {error:#}");
        }
        self.remember_prompt_terminal(id, PromptResolutionStatus::Abandoned);
    }

    fn resolve_prompt(
        &mut self,
        id: PromptId,
        answer: serde_json::Value,
    ) -> Result<PromptResolutionCommit> {
        let status = if let Some(entry) = self.prompts.remove(&id) {
            let event = self.session.sink().emit_returning_envelope(
                atman_runtime::event::Event::PromptResolved {
                    prompt_id: id.0,
                    answer: answer.clone(),
                },
            );
            self.catch_up_through(event.seq)?;
            let _ = entry.responder.send(answer);
            self.remember_prompt_terminal(id, PromptResolutionStatus::AlreadyResolved);
            PromptResolutionStatus::Resolved
        } else {
            self.prompt_terminals
                .iter()
                .rev()
                .find(|(prompt_id, _)| prompt_id == &id)
                .map(|(_, status)| *status)
                .unwrap_or(PromptResolutionStatus::NotFound)
        };
        Ok(PromptResolutionCommit {
            status,
            revision: self.projection.projection().revision,
            cursor: self.event_cursor,
        })
    }

    fn remember_prompt_terminal(&mut self, id: PromptId, status: PromptResolutionStatus) {
        self.prompt_terminals.push_back((id, status));
        while self.prompt_terminals.len() > PROMPT_TERMINAL_RETENTION {
            self.prompt_terminals.pop_front();
        }
    }

    fn submit_form(
        &mut self,
        id: String,
        submission: FormSubmission,
    ) -> Result<FormResolutionCommit> {
        let submission = runtime_form_submission(submission);
        let status = match self
            .session
            .forms()
            .submit_with_commit(&id, submission)
            .map_err(anyhow::Error::msg)?
        {
            Some(commit) => {
                let event = commit
                    .event
                    .ok_or_else(|| anyhow::anyhow!("session form resolution was not persisted"))?;
                self.catch_up_through(event.seq)?;
                FormResolutionStatus::Resolved
            }
            None => self
                .form_terminals
                .iter()
                .rev()
                .find(|(form_id, _)| form_id == &id)
                .map(|(_, status)| *status)
                .unwrap_or(FormResolutionStatus::NotFound),
        };
        Ok(FormResolutionCommit {
            status,
            revision: self.projection.projection().revision,
            cursor: self.event_cursor,
        })
    }

    fn remember_form_terminal(&mut self, id: String, status: FormResolutionStatus) {
        self.form_terminals.retain(|(form_id, _)| form_id != &id);
        self.form_terminals.push_back((id, status));
        while self.form_terminals.len() > FORM_TERMINAL_RETENTION {
            self.form_terminals.pop_front();
        }
    }

    fn resolve_compact_review(
        &mut self,
        id: String,
        decision: CompactReviewDecision,
    ) -> Result<CompactReviewResolutionCommit> {
        let published_seq = self.session.sink().next_seq_peek().saturating_sub(1);
        self.catch_up_through(published_seq)?;
        let status = match self
            .session
            .compact_reviews()
            .decide_with_commit(&id, runtime_compact_review_decision(decision))
        {
            Some(commit) => {
                let event = commit.event.ok_or_else(|| {
                    anyhow::anyhow!("session compact review resolution was not persisted")
                })?;
                self.catch_up_through(event.seq)?;
                CompactReviewResolutionStatus::Resolved
            }
            None => self
                .compact_review_terminals
                .iter()
                .rev()
                .find(|(review_id, _)| review_id == &id)
                .map(|(_, status)| *status)
                .unwrap_or(CompactReviewResolutionStatus::NotFound),
        };
        Ok(CompactReviewResolutionCommit {
            status,
            revision: self.projection.projection().revision,
            cursor: self.event_cursor,
        })
    }

    fn remember_compact_review_terminal(
        &mut self,
        id: String,
        status: CompactReviewResolutionStatus,
    ) {
        self.compact_review_terminals
            .retain(|(review_id, _)| review_id != &id);
        self.compact_review_terminals.push_back((id, status));
        while self.compact_review_terminals.len() > COMPACT_REVIEW_TERMINAL_RETENTION {
            self.compact_review_terminals.pop_front();
        }
    }

    fn apply_runtime_event(&mut self, event: &atman_runtime::event::EventEnvelope) {
        if let atman_runtime::event::Event::FormResolved {
            form_id, abandoned, ..
        } = &event.event
        {
            self.remember_form_terminal(
                form_id.clone(),
                if *abandoned {
                    FormResolutionStatus::Abandoned
                } else {
                    FormResolutionStatus::AlreadyResolved
                },
            );
        }
        if let atman_runtime::event::Event::CompactReviewResolved {
            review_id,
            abandoned,
            ..
        } = &event.event
        {
            self.remember_compact_review_terminal(
                review_id.clone(),
                if *abandoned {
                    CompactReviewResolutionStatus::Abandoned
                } else {
                    CompactReviewResolutionStatus::AlreadyResolved
                },
            );
        }
        if let Some(delta) = self.projection.apply_envelope(event) {
            self.publish_projection_delta(delta);
        }
    }

    fn catch_up_through(&mut self, target_seq: u64) -> Result<()> {
        while self.projection.last_runtime_seq() < target_seq {
            match self.events_rx.try_recv() {
                Ok(event) => self.apply_runtime_event(&event),
                Err(broadcast::error::TryRecvError::Lagged(_)) => self.catch_up_projection(),
                Err(broadcast::error::TryRecvError::Empty) => {
                    anyhow::bail!(
                        "runtime event {target_seq} was published but is not available to the session actor"
                    );
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    anyhow::bail!("runtime event stream closed before event {target_seq}");
                }
            }
        }
        Ok(())
    }

    fn publish_projection_delta(&mut self, delta: ProjectionDelta) {
        debug_assert_eq!(delta.revision, self.projection.projection().revision);
        self.publish_server_event(ServerEvent::ProjectionDelta { delta });
        self.publish();
    }

    fn publish_signal(&mut self, signal: SessionSignal) {
        self.publish_server_event(ServerEvent::Signal { signal });
    }

    fn publish_server_event(&mut self, event: ServerEvent) {
        self.event_cursor.0 = self.event_cursor.0.saturating_add(1);
        let envelope = ProjectionEventEnvelope {
            schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
            daemon_generation: self.daemon_generation.clone(),
            session_id: self.session_id.clone(),
            cursor: self.event_cursor,
            ts: chrono::Utc::now(),
            event,
        };
        self.updates.push_back(envelope.clone());
        while self.updates.len() > UPDATE_RETENTION {
            self.updates.pop_front();
        }
        let _ = self.updates_tx.send(envelope);
    }

    fn apply_runtime_signal(&mut self, frame: StreamFrame) {
        let published_seq = self.session.sink().next_seq_peek().saturating_sub(1);
        if self.projection.last_runtime_seq() < published_seq
            && let Err(error) = self.catch_up_through(published_seq)
        {
            eprintln!(
                "warning: failed to order live signal after durable session events for {}: {error:#}",
                self.session_id
            );
            return;
        }
        let signal = match frame {
            StreamFrame::LlmChunk {
                text,
                run_id: Some(run_id),
                ..
            } if !text.is_empty() => self
                .known_run_id(&run_id)
                .map(|run_id| SessionSignal::LlmText { run_id, text }),
            StreamFrame::ThinkingChunk {
                text,
                run_id: Some(run_id),
            } if !text.is_empty() => self
                .known_run_id(&run_id)
                .map(|run_id| SessionSignal::Thinking { run_id, text }),
            StreamFrame::ToolCallDraft {
                index,
                call_id,
                name,
                arguments_delta,
                run_id: Some(run_id),
            } => self
                .known_run_id(&run_id)
                .map(|run_id| SessionSignal::ToolCallDraft {
                    run_id,
                    index,
                    call_id,
                    name,
                    arguments_delta,
                }),
            StreamFrame::LlmDone {
                total_tokens,
                run_id: Some(run_id),
            } => self
                .known_run_id(&run_id)
                .map(|run_id| SessionSignal::LlmDone {
                    run_id,
                    total_tokens,
                }),
            StreamFrame::LlmRetry {
                run_id: Some(run_id),
            } => self
                .known_run_id(&run_id)
                .map(|run_id| SessionSignal::LlmRetry { run_id }),
            StreamFrame::Notification(frame) => self
                .notification_signal(frame)
                .map(|notification| SessionSignal::Notification { notification }),
            StreamFrame::Note(message) => Some(SessionSignal::Notification {
                notification: SessionNotification {
                    run_id: None,
                    level: atman_proto::NoticeLevel::Info,
                    location: NotificationLocation::Inline,
                    lifecycle: NotificationLifecycle::Persistent,
                    stack: NotificationStack::Append,
                    message,
                },
            }),
            StreamFrame::CompactionSummary {
                phase: atman_runtime::stream::CompactionPhase::Running,
                range_start,
                range_end,
                before_tokens,
                compacted_count,
                ..
            } => Some(SessionSignal::CompactionStarted {
                range_start: range_start as u64,
                range_end: range_end as u64,
                before_tokens,
                compacted_count: compacted_count as u64,
            }),
            StreamFrame::CompactionDelta {
                range_start,
                range_end,
                text,
            } if !text.is_empty() => Some(SessionSignal::CompactionText {
                range_start: range_start as u64,
                range_end: range_end as u64,
                text,
            }),
            StreamFrame::CompactionSummary {
                phase: atman_runtime::stream::CompactionPhase::Failed,
                range_start,
                range_end,
                summary,
                ..
            } => Some(SessionSignal::CompactionFailed {
                range_start: range_start as u64,
                range_end: range_end as u64,
                reason: summary,
            }),
            StreamFrame::TerminalChunk { handle, bytes, .. } if !bytes.is_empty() => self
                .resource_id_for_handle(&handle)
                .map(|resource_id| SessionSignal::TerminalBytes { resource_id, bytes }),
            StreamFrame::BashChunk {
                handle, kind, line, ..
            } if !line.is_empty() => {
                self.resource_id_for_handle(&handle)
                    .map(|resource_id| SessionSignal::ProcessLine {
                        resource_id,
                        stream: kind,
                        line,
                    })
            }
            _ => None,
        };
        if let Some(signal) = signal {
            self.publish_signal(signal);
        }
    }

    fn resource_id_for_handle(&self, handle: &str) -> Option<ResourceId> {
        self.task_registry
            .lookup_by_handle_in_session(handle, &self.session_id.to_string())
            .map(|task| crate::projection::task_resource_id(&task.id))
    }

    fn notification_signal(
        &self,
        frame: atman_runtime::stream::NotificationFrame,
    ) -> Option<SessionNotification> {
        let run_id = match frame.run_id {
            Some(run_id) => Some(self.known_run_id(&run_id)?),
            None => None,
        };
        let level = match frame.level {
            atman_runtime::notify::NotifyLevel::Debug => atman_proto::NoticeLevel::Debug,
            atman_runtime::notify::NotifyLevel::Info => atman_proto::NoticeLevel::Info,
            atman_runtime::notify::NotifyLevel::Success => atman_proto::NoticeLevel::Success,
            atman_runtime::notify::NotifyLevel::Warn => atman_proto::NoticeLevel::Warning,
            atman_runtime::notify::NotifyLevel::Error => atman_proto::NoticeLevel::Error,
        };
        let location = match frame.location {
            atman_runtime::notify::NotifyLocation::Inline => NotificationLocation::Inline,
            atman_runtime::notify::NotifyLocation::Toast => NotificationLocation::Toast,
            atman_runtime::notify::NotifyLocation::Status => NotificationLocation::Status,
            atman_runtime::notify::NotifyLocation::Modal => NotificationLocation::Modal,
            atman_runtime::notify::NotifyLocation::Stdout => NotificationLocation::Stdout,
            atman_runtime::notify::NotifyLocation::Stderr => NotificationLocation::Stderr,
            atman_runtime::notify::NotifyLocation::Log => return None,
        };
        let lifecycle = match frame.lifecycle {
            atman_runtime::notify::NotifyLifecycle::Persistent => NotificationLifecycle::Persistent,
            atman_runtime::notify::NotifyLifecycle::Ttl(duration) => NotificationLifecycle::Ttl {
                duration_ms: duration.as_millis().try_into().unwrap_or(u64::MAX),
            },
            atman_runtime::notify::NotifyLifecycle::Dismissible => {
                NotificationLifecycle::Dismissible
            }
            atman_runtime::notify::NotifyLifecycle::UntilReplaced => {
                NotificationLifecycle::UntilReplaced
            }
        };
        let stack = match frame.stack {
            atman_runtime::notify::NotifyStack::Append => NotificationStack::Append,
            atman_runtime::notify::NotifyStack::Replace { key } => {
                NotificationStack::Replace { key }
            }
            atman_runtime::notify::NotifyStack::Dedupe { key, window } => {
                NotificationStack::Dedupe {
                    key,
                    window_ms: window.as_millis().try_into().unwrap_or(u64::MAX),
                }
            }
            atman_runtime::notify::NotifyStack::MergeCount { key, window } => {
                NotificationStack::MergeCount {
                    key,
                    window_ms: window.as_millis().try_into().unwrap_or(u64::MAX),
                }
            }
            atman_runtime::notify::NotifyStack::Coalesce { key } => {
                NotificationStack::Coalesce { key }
            }
        };
        Some(SessionNotification {
            run_id,
            level,
            location,
            lifecycle,
            stack,
            message: frame.message,
        })
    }

    fn known_run_id(&self, run_id: &str) -> Option<FlowRunId> {
        let run_id = parse_run_id(run_id)?;
        self.projection
            .projection()
            .runs
            .iter()
            .any(|run| run.id == run_id)
            .then_some(run_id)
    }

    fn refresh_watch_projections(&mut self) {
        let goal = self.goal_rx.borrow_and_update().clone();
        if let Some(delta) = self.projection.set_goal(goal) {
            self.publish_projection_delta(delta);
        }
        let todos = self.todos_rx.borrow_and_update().clone();
        if let Some(delta) = self.projection.set_todos(todos) {
            self.publish_projection_delta(delta);
        }
        let plans = self.plans_rx.borrow_and_update().clone();
        if let Some(delta) = self.projection.set_plans(plans) {
            self.publish_projection_delta(delta);
        }
        let context = self.context_rx.borrow_and_update().clone();
        if let Some(delta) = self.projection.set_context(context) {
            self.publish_projection_delta(delta);
        }
        let trust = self.trust_rx.borrow_and_update().clone();
        if let Some(delta) = self.projection.set_trust(trust) {
            self.publish_projection_delta(delta);
        }
        let forms = self.forms_rx.borrow_and_update().clone();
        if let Some(delta) = self.projection.set_forms(forms) {
            self.publish_projection_delta(delta);
        }
        let review = self.compact_review_rx.borrow_and_update().clone();
        if let Some(delta) = self.projection.set_compact_review(review) {
            self.publish_projection_delta(delta);
        }
    }

    fn rename(&mut self, title: String) -> Result<RenameSessionCommit> {
        atman_runtime::session_meta::SessionMeta::rename(self.session.dir(), title)
            .with_context(|| format!("rename session {}", self.session_id))?;
        let session = session_summary(
            self.session.dir(),
            self.session_id.clone(),
            self.runs.values(),
        )?;
        if let Some(delta) = self.projection.set_metadata(self.session.meta()) {
            self.publish_projection_delta(delta);
        }
        Ok(RenameSessionCommit {
            session,
            revision: self.projection.projection().revision,
            cursor: self.event_cursor,
        })
    }

    fn terminate_resource(&mut self, resource_id: ResourceId) -> Result<ResourceTerminationCommit> {
        let published_seq = self.session.sink().next_seq_peek().saturating_sub(1);
        self.catch_up_through(published_seq)?;
        let resource = self
            .projection
            .projection()
            .resources
            .iter()
            .find(|resource| resource.id == resource_id);
        let status = match resource {
            None => ResourceTerminationStatus::NotFound,
            Some(resource) if resource_state_is_terminal(resource.state) => {
                ResourceTerminationStatus::AlreadyTerminal
            }
            Some(resource) if resource.state == ResourceState::Terminating => {
                ResourceTerminationStatus::Terminating
            }
            Some(resource)
                if !matches!(
                    resource.kind,
                    ResourceKind::Terminal | ResourceKind::BackgroundProcess
                ) =>
            {
                ResourceTerminationStatus::Unsupported
            }
            Some(_) => match task_id_from_resource_id(&resource_id) {
                Some(task_id) => match self.task_registry.kill_from_operator(&task_id) {
                    atman_runtime::task_registry::KillOutcome::Killed { .. } => {
                        let published_seq = self.session.sink().next_seq_peek().saturating_sub(1);
                        self.catch_up_through(published_seq)?;
                        ResourceTerminationStatus::Terminating
                    }
                    atman_runtime::task_registry::KillOutcome::NotRunning => {
                        ResourceTerminationStatus::AlreadyTerminal
                    }
                    atman_runtime::task_registry::KillOutcome::NotFound => {
                        ResourceTerminationStatus::Unavailable
                    }
                    atman_runtime::task_registry::KillOutcome::SelfKillRejected => unreachable!(
                        "operator-triggered resource termination has no caller flow identity"
                    ),
                },
                None => ResourceTerminationStatus::Unavailable,
            },
        };
        Ok(ResourceTerminationCommit {
            status,
            revision: self.projection.projection().revision,
            cursor: self.event_cursor,
        })
    }

    fn begin_workspace_mutation(
        &mut self,
        action: WorkspaceMutationAction,
        resource_id: ResourceId,
        reply: oneshot::Sender<Result<ResourceMutationCommit>>,
    ) {
        let result = self.prepare_workspace_mutation(action, &resource_id);
        let (service, workspace_id, owner_run_id) = match result {
            Ok(WorkspaceMutationPreparation::Complete(commit)) => {
                let _ = reply.send(Ok(commit));
                return;
            }
            Ok(WorkspaceMutationPreparation::Execute {
                service,
                workspace_id,
                owner_run_id,
            }) => (service, workspace_id, owner_run_id),
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };

        self.workspace_mutations.insert(resource_id.clone());
        let owner_session = self.session_id.to_string();
        let owner_flow = owner_run_id.to_string();
        let command_tx = self.command_tx.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || match action {
                WorkspaceMutationAction::Retain => {
                    service.retain(&workspace_id, &owner_session, &owner_flow)
                }
                WorkspaceMutationAction::Release => {
                    service.release(&workspace_id, &owner_session, &owner_flow)
                }
            })
            .await
            .map_err(|error| format!("workspace action task failed: {error}"))
            .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = command_tx.send(Command::WorkspaceMutationFinished {
                action,
                resource_id,
                owner_run_id,
                result: Box::new(result),
                reply,
            });
        });
    }

    fn prepare_workspace_mutation(
        &mut self,
        action: WorkspaceMutationAction,
        resource_id: &ResourceId,
    ) -> Result<WorkspaceMutationPreparation> {
        let published_seq = self.session.sink().next_seq_peek().saturating_sub(1);
        self.catch_up_through(published_seq)?;
        let resource = self
            .projection
            .projection()
            .resources
            .iter()
            .find(|resource| &resource.id == resource_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("resource not found: {}", resource_id.0))?;
        anyhow::ensure!(
            resource.kind == ResourceKind::Workspace,
            "resource {} does not support workspace lifecycle actions",
            resource_id.0
        );
        let already_complete = match action {
            WorkspaceMutationAction::Retain => resource.state == ResourceState::Retained,
            WorkspaceMutationAction::Release => resource.state == ResourceState::Released,
        };
        if already_complete {
            return Ok(WorkspaceMutationPreparation::Complete(
                self.resource_mutation_commit(resource),
            ));
        }
        anyhow::ensure!(
            !self.workspace_mutations.contains(resource_id),
            "resource {} already has a workspace lifecycle action in progress",
            resource_id.0
        );
        let workspace_id = workspace_id_from_resource_id(resource_id)
            .ok_or_else(|| anyhow::anyhow!("invalid workspace resource id: {}", resource_id.0))?;
        let service = self
            .workspace_service
            .clone()
            .ok_or_else(|| anyhow::anyhow!("session has no project workspace service"))?;
        Ok(WorkspaceMutationPreparation::Execute {
            service,
            workspace_id: workspace_id.to_owned(),
            owner_run_id: resource.owner_run_id,
        })
    }

    fn finish_workspace_mutation(
        &mut self,
        action: WorkspaceMutationAction,
        resource_id: ResourceId,
        owner_run_id: FlowRunId,
        result: Result<atman_runtime::git_workspace::WorkspaceRecord, String>,
    ) -> Result<ResourceMutationCommit> {
        self.workspace_mutations.remove(&resource_id);
        let record = result.map_err(|error| {
            anyhow::anyhow!(
                "workspace {} failed for resource {}: {error}",
                action.as_str(),
                resource_id.0
            )
        })?;
        let state = record.lifecycle_state().as_str().to_owned();
        let event = self.session.sink().emit_returning_envelope(
            atman_runtime::event::Event::WorkspaceLifecycle {
                run_id: atman_runtime::event::FlowRunId(owner_run_id.0),
                workspace_id: record.id,
                path: record.worktree_path.display().to_string(),
                state,
                cleanup_error: None,
            },
        );
        self.catch_up_through(event.seq)?;
        let resource = self
            .projection
            .projection()
            .resources
            .iter()
            .find(|resource| resource.id == resource_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("workspace lifecycle event did not update resource"))?;
        Ok(self.resource_mutation_commit(resource))
    }

    fn resource_mutation_commit(
        &self,
        resource: atman_proto::ResourceProjection,
    ) -> ResourceMutationCommit {
        ResourceMutationCommit {
            resource,
            revision: self.projection.projection().revision,
            cursor: self.event_cursor,
        }
    }

    fn catch_up_projection(&mut self) {
        let requested_after = self.event_cursor;
        let previous_revision = self.projection.projection().revision;
        for event in self.session.sink().snapshot_envelopes() {
            self.projection.apply_envelope(&event);
        }
        self.projection.set_goal(self.goal_rx.borrow().clone());
        self.projection.set_todos(self.todos_rx.borrow().clone());
        self.projection.set_plans(self.plans_rx.borrow().clone());
        self.projection
            .set_context(self.context_rx.borrow().clone());
        self.projection.set_trust(self.trust_rx.borrow().clone());
        self.projection.set_forms(self.forms_rx.borrow().clone());
        self.projection
            .set_compact_review(self.compact_review_rx.borrow().clone());
        self.projection.rebase_after_rebuild(previous_revision);
        self.event_cursor.0 = self.event_cursor.0.saturating_add(1);
        self.updates.clear();
        let _ = self.updates_tx.send(ProjectionEventEnvelope {
            schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
            daemon_generation: self.daemon_generation.clone(),
            session_id: self.session_id.clone(),
            cursor: self.event_cursor,
            ts: chrono::Utc::now(),
            event: ServerEvent::ResyncRequired {
                gap: ResyncRequired {
                    requested_after,
                    available_from: self.event_cursor,
                    snapshot_revision: self.projection.projection().revision,
                    reason: "runtime event stream lagged; fetch a fresh session snapshot".into(),
                },
            },
        });
        self.publish();
    }

    fn updates_response(
        &self,
        after_cursor: EventCursor,
        limit: Option<usize>,
    ) -> GetSessionUpdatesResponse {
        let first_available = self.updates.front().map(|event| event.cursor);
        let coverage_start = first_available
            .map(|cursor| EventCursor(cursor.0.saturating_sub(1)))
            .unwrap_or(self.event_cursor);
        if after_cursor > self.event_cursor || after_cursor < coverage_start {
            return GetSessionUpdatesResponse {
                daemon_generation: self.daemon_generation.clone(),
                events: Vec::new(),
                next_cursor: self.event_cursor,
                has_more: false,
                resync_required: Some(ResyncRequired {
                    requested_after: after_cursor,
                    available_from: first_available.unwrap_or(self.event_cursor),
                    snapshot_revision: self.projection.projection().revision,
                    reason: if after_cursor > self.event_cursor {
                        "requested cursor is ahead of this session actor".into()
                    } else {
                        "requested cursor is outside the retained update window".into()
                    },
                }),
            };
        }

        let limit = limit
            .unwrap_or(MAX_UPDATE_PAGE_SIZE)
            .clamp(1, MAX_UPDATE_PAGE_SIZE);
        let events = self
            .updates
            .iter()
            .filter(|event| event.cursor > after_cursor)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let next_cursor = events
            .last()
            .map(|event| event.cursor)
            .unwrap_or(after_cursor);
        GetSessionUpdatesResponse {
            daemon_generation: self.daemon_generation.clone(),
            has_more: self
                .updates
                .back()
                .is_some_and(|event| event.cursor > next_cursor),
            events,
            next_cursor,
            resync_required: None,
        }
    }

    fn list_permissions(&self) -> ListPermissionRequestsResponse {
        let (requests, groups) = self
            .session
            .permission_broker()
            .user_list(&self.session_id.to_string());
        ListPermissionRequestsResponse {
            session_id: self.session_id.clone(),
            requests: requests
                .into_iter()
                .map(|request| PermissionRequestView {
                    request_id: request.request_id.0,
                    session_id: request.session_id,
                    requesting_run_id: FlowRunId(request.requesting_run_id.0),
                    tool: request.intent.tool_name,
                    tier: format!("{:?}", request.intent.tier),
                    state: format!("{:?}", request.state),
                    target: request
                        .escalation_path
                        .last()
                        .map(|hop| format!("{:?}", hop.target))
                        .unwrap_or_default(),
                    revision: request.revision,
                })
                .collect(),
            groups: groups
                .into_iter()
                .map(|group| PermissionGroupView {
                    group_id: group.group_id.0,
                    label: group.label,
                    request_ids: group.request_ids.into_iter().map(|id| id.0).collect(),
                    revision: group.revision,
                })
                .collect(),
            revision: self.projection.projection().revision,
            cursor: self.event_cursor,
        }
    }

    fn create_permission_group(
        &mut self,
        request_ids: Vec<uuid::Uuid>,
        expected_request_revisions: std::collections::BTreeMap<uuid::Uuid, u64>,
        label: String,
    ) -> Result<CreatePermissionGroupResponse> {
        let ids: BTreeSet<_> = request_ids
            .into_iter()
            .map(atman_runtime::permission::PermissionRequestId)
            .collect();
        let revisions: HashMap<_, _> = expected_request_revisions
            .into_iter()
            .map(|(id, revision)| (atman_runtime::permission::PermissionRequestId(id), revision))
            .collect();
        let group = self.session.permission_broker().user_create_group(
            &self.session_id.to_string(),
            ids,
            label,
            &revisions,
        )?;
        let published_seq = self.session.sink().next_seq_peek().saturating_sub(1);
        self.catch_up_through(published_seq)?;
        Ok(CreatePermissionGroupResponse {
            session_id: self.session_id.clone(),
            group_id: group.group_id.0,
            request_ids: group
                .request_ids
                .into_iter()
                .map(|request_id| request_id.0)
                .collect(),
            revision: group.revision,
            label: group.label,
            session_revision: self.projection.projection().revision,
            cursor: self.event_cursor,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_permissions(
        &mut self,
        request_ids: Vec<uuid::Uuid>,
        expected_request_revisions: std::collections::BTreeMap<uuid::Uuid, u64>,
        group: Option<(uuid::Uuid, u64)>,
        action: atman_runtime::permission::PermissionAction,
        scope: Option<atman_runtime::permission::GrantScope>,
        reason: Option<String>,
        principal_id: String,
    ) -> Result<ResolvePermissionRequestsResponse> {
        let request_ids = request_ids
            .into_iter()
            .map(atman_runtime::permission::PermissionRequestId)
            .collect();
        let revisions = expected_request_revisions
            .into_iter()
            .map(|(id, revision)| (atman_runtime::permission::PermissionRequestId(id), revision))
            .collect();
        let group = group
            .map(|(id, revision)| (atman_runtime::permission::PermissionGroupId(id), revision));
        let results = self.session.permission_broker().user_resolve(
            &self.session_id.to_string(),
            Some(principal_id),
            request_ids,
            &revisions,
            group,
            action,
            scope,
            reason,
        )?;
        let published_seq = self.session.sink().next_seq_peek().saturating_sub(1);
        self.catch_up_through(published_seq)?;
        Ok(ResolvePermissionRequestsResponse {
            session_id: self.session_id.clone(),
            resolutions: results
                .into_iter()
                .map(|result| PermissionResolutionView {
                    request_id: result.request_id.0,
                    outcome: format!("{:?}", result.outcome),
                })
                .collect(),
            revision: self.projection.projection().revision,
            cursor: self.event_cursor,
        })
    }
}

enum WorkspaceMutationPreparation {
    Complete(ResourceMutationCommit),
    Execute {
        service: atman_runtime::flow_workspace::FlowWorkspaceService,
        workspace_id: String,
        owner_run_id: FlowRunId,
    },
}

impl WorkspaceMutationAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Retain => "retain",
            Self::Release => "release",
        }
    }
}

fn runtime_form_submission(submission: FormSubmission) -> atman_runtime::form::FormSubmission {
    match submission {
        FormSubmission::Submitted { answers } => atman_runtime::form::FormSubmission::Submitted {
            answers: answers.into_iter().map(runtime_form_answer).collect(),
        },
        FormSubmission::Rejected => atman_runtime::form::FormSubmission::Rejected,
    }
}

fn resource_state_is_terminal(state: ResourceState) -> bool {
    matches!(
        state,
        ResourceState::Exited
            | ResourceState::Failed
            | ResourceState::Released
            | ResourceState::Lost
            | ResourceState::Orphaned
    )
}

fn task_id_from_resource_id(resource_id: &ResourceId) -> Option<atman_runtime::TaskId> {
    resource_id
        .0
        .strip_prefix("task:")
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .map(atman_runtime::TaskId)
}

fn parse_run_id(run_id: &str) -> Option<FlowRunId> {
    uuid::Uuid::parse_str(run_id).ok().map(FlowRunId)
}

fn workspace_id_from_resource_id(resource_id: &ResourceId) -> Option<&str> {
    resource_id.0.strip_prefix("workspace:")
}

fn runtime_compact_review_decision(
    decision: CompactReviewDecision,
) -> atman_runtime::session::CompactReviewDecision {
    match decision {
        CompactReviewDecision::AcceptAsIs => {
            atman_runtime::session::CompactReviewDecision::AcceptAsIs
        }
        CompactReviewDecision::AcceptEdited { summary } => {
            atman_runtime::session::CompactReviewDecision::AcceptEdited { summary }
        }
        CompactReviewDecision::Reject => atman_runtime::session::CompactReviewDecision::Reject,
    }
}

fn runtime_form_answer(answer: atman_proto::FormAnswer) -> atman_runtime::form::FormAnswer {
    match answer {
        atman_proto::FormAnswer::Confirmed { value } => {
            atman_runtime::form::FormAnswer::Confirmed { value }
        }
        atman_proto::FormAnswer::Selected { index, label } => {
            atman_runtime::form::FormAnswer::Selected { index, label }
        }
        atman_proto::FormAnswer::MultiSelected { indices, labels } => {
            atman_runtime::form::FormAnswer::MultiSelected { indices, labels }
        }
        atman_proto::FormAnswer::TextEntered { text } => {
            atman_runtime::form::FormAnswer::TextEntered { text }
        }
        atman_proto::FormAnswer::Cancelled => atman_runtime::form::FormAnswer::Cancelled,
    }
}

fn view_for(
    revision: u64,
    runs: &HashMap<FlowRunId, LiveRun>,
    projection: &SessionProjector,
    idle_since: Option<std::time::Instant>,
) -> SessionActorView {
    SessionActorView {
        revision,
        projection_revision: projection.projection().revision,
        runtime_event_seq: projection.last_runtime_seq(),
        idle_since,
        runs: runs
            .iter()
            .map(|(id, run)| {
                (
                    id.clone(),
                    LiveRunView {
                        started_at: run.started_at,
                    },
                )
            })
            .collect(),
    }
}

pub(crate) fn session_summary<'a>(
    session_dir: &std::path::Path,
    session_id: SessionId,
    runs: impl Iterator<Item = &'a LiveRun>,
) -> Result<SessionSummary> {
    let started_at = runs.map(|run| run.started_at).min();
    let stats =
        atman_runtime::session_meta::SessionStats::load_or_rebuild(session_dir).unwrap_or_default();
    let meta = atman_runtime::session_meta::SessionMeta::load(session_dir);
    Ok(SessionSummary {
        id: session_id,
        event_count: stats.event_count as usize,
        first_ts: stats.first_ts.or(started_at),
        status: if started_at.is_some() {
            atman_proto::SessionStatus::Running
        } else {
            atman_proto::SessionStatus::Finished
        },
        title: meta
            .as_ref()
            .and_then(|meta| meta.title.clone())
            .unwrap_or_else(|| "Untitled session".into()),
        goal: atman_runtime::memory::goal::GoalStore::at(session_dir)
            .get()
            .ok(),
        project_root: meta
            .as_ref()
            .and_then(|meta| meta.project_root.as_ref())
            .map(|path| path.display().to_string()),
        name_source: match meta
            .as_ref()
            .map(|meta| meta.name_source)
            .unwrap_or_default()
        {
            atman_runtime::session_meta::NameSource::Auto => atman_proto::NameSource::Auto,
            atman_runtime::session_meta::NameSource::User => atman_proto::NameSource::User,
        },
    })
}
