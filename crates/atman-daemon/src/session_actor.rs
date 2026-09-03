use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use anyhow::{Context, Result};
use atman_proto::{
    CreatePermissionGroupResponse, DaemonGeneration, EventCursor, FlowRunId, FormResolutionStatus,
    FormSubmission, GetSessionUpdatesResponse, ListPermissionRequestsResponse,
    PROJECTION_EVENT_SCHEMA_VERSION, PermissionGroupView, PermissionRequestView,
    PermissionResolutionView, ProjectionDelta, ProjectionEventEnvelope, PromptId,
    PromptResolutionStatus, ResolvePermissionRequestsResponse, ResyncRequired, ServerEvent,
    SessionId, SessionProjection, SessionSummary,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::projection::SessionProjector;
use crate::state::LiveRun;

const UPDATE_RETENTION: usize = 2_048;
const MAX_UPDATE_PAGE_SIZE: usize = 1_000;
const PROMPT_TERMINAL_RETENTION: usize = 256;
const FORM_TERMINAL_RETENTION: usize = 256;

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
}

#[derive(Clone)]
pub(crate) struct SessionActorHandle {
    session: Arc<atman_runtime::Session>,
    owner_principal: Arc<str>,
    tx: mpsc::UnboundedSender<Command>,
    view: watch::Receiver<SessionActorView>,
}

impl SessionActorHandle {
    pub fn spawn(
        session_id: SessionId,
        session: Arc<atman_runtime::Session>,
        initial_runs: Vec<LiveRun>,
        owner_principal: String,
        daemon_generation: DaemonGeneration,
        restored_projection: Option<SessionProjector>,
    ) -> Self {
        let events_rx = session.sink().subscribe();
        let goal_rx = session.subscribe_goal();
        let todos_rx = session.subscribe_todos();
        let plans_rx = session.subscribe_plans();
        let context_rx = session.subscribe_context();
        let forms_rx = session.forms().subscribe();
        let mut projection = restored_projection.unwrap_or_else(|| {
            SessionProjector::from_events(
                session_id.clone(),
                session.meta(),
                &session.sink().snapshot_envelopes(),
            )
        });
        projection.set_goal(goal_rx.borrow().clone());
        projection.set_todos(todos_rx.borrow().clone());
        projection.set_plans(plans_rx.borrow().clone());
        projection.set_context(context_rx.borrow().clone());
        projection.set_forms(forms_rx.borrow().clone());
        for run in &initial_runs {
            projection.register_run(run.run_id.clone(), run.flow_name.clone(), run.started_at);
        }
        let event_cursor = EventCursor(projection.projection().revision.0);
        let (tx, rx) = mpsc::unbounded_channel();
        let (updates_tx, _) = broadcast::channel(UPDATE_RETENTION);
        let runs = initial_runs
            .into_iter()
            .map(|run| (run.run_id.clone(), run))
            .collect();
        let (view_tx, view) = watch::channel(view_for(1, &runs, &projection));
        let actor = SessionActor {
            session_id,
            session: session.clone(),
            runs,
            prompts: HashMap::new(),
            prompt_terminals: VecDeque::new(),
            form_terminals: VecDeque::new(),
            revision: 1,
            projection,
            event_cursor,
            daemon_generation,
            updates: VecDeque::new(),
            updates_tx,
            view_tx,
            rx,
            events_rx,
            goal_rx,
            todos_rx,
            plans_rx,
            context_rx,
            forms_rx,
            _permission_client: session.permission_broker().register_client(),
        };
        tokio::spawn(actor.run());
        Self {
            session,
            owner_principal: owner_principal.into(),
            tx,
            view,
        }
    }

    pub fn owns(&self, principal: &str) -> bool {
        self.owner_principal.as_ref() == principal
    }

    pub fn owns_session(&self, session: &Arc<atman_runtime::Session>) -> bool {
        Arc::ptr_eq(&self.session, session)
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

    pub async fn cancel_run(&self, run_id: FlowRunId) -> Result<bool> {
        request(&self.tx, |reply| Command::CancelRun { run_id, reply }).await
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
        let _ = self.tx.send(Command::RegisterPrompt {
            id,
            kind,
            payload,
            responder,
        });
        receiver
    }

    pub fn drop_prompt(&self, id: PromptId) {
        let _ = self.tx.send(Command::DropPrompt { id });
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

    pub async fn rename(&self, title: String) -> Result<SessionSummary> {
        request(&self.tx, |reply| Command::Rename { title, reply }).await?
    }

    pub async fn snapshot(&self) -> Result<(EventCursor, SessionProjection)> {
        let (cursor, projection) = request(&self.tx, |reply| Command::Snapshot { reply }).await?;
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
        .await?;
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
        reply: oneshot::Sender<bool>,
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
    },
    DropPrompt {
        id: PromptId,
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
    Rename {
        title: String,
        reply: oneshot::Sender<Result<SessionSummary>>,
    },
    Snapshot {
        reply: oneshot::Sender<(EventCursor, SessionProjection)>,
    },
    Updates {
        after_cursor: EventCursor,
        limit: Option<usize>,
        reply: oneshot::Sender<GetSessionUpdatesResponse>,
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
    Goal(Result<(), watch::error::RecvError>),
    Todos(Result<(), watch::error::RecvError>),
    Plans(Result<(), watch::error::RecvError>),
    Context(Result<(), watch::error::RecvError>),
    Forms(Result<(), watch::error::RecvError>),
}

struct SessionActor {
    session_id: SessionId,
    session: Arc<atman_runtime::Session>,
    runs: HashMap<FlowRunId, LiveRun>,
    prompts: HashMap<PromptId, PendingPrompt>,
    prompt_terminals: VecDeque<(PromptId, PromptResolutionStatus)>,
    form_terminals: VecDeque<(String, FormResolutionStatus)>,
    revision: u64,
    projection: SessionProjector,
    event_cursor: EventCursor,
    daemon_generation: DaemonGeneration,
    updates: VecDeque<ProjectionEventEnvelope>,
    updates_tx: broadcast::Sender<ProjectionEventEnvelope>,
    view_tx: watch::Sender<SessionActorView>,
    rx: mpsc::UnboundedReceiver<Command>,
    events_rx: broadcast::Receiver<atman_runtime::event::EventEnvelope>,
    goal_rx: watch::Receiver<Option<String>>,
    todos_rx: watch::Receiver<Vec<atman_runtime::memory::todo::Todo>>,
    plans_rx: watch::Receiver<Vec<atman_runtime::memory::plan::Plan>>,
    context_rx: watch::Receiver<atman_runtime::ContextSnapshot>,
    forms_rx: watch::Receiver<Vec<atman_runtime::form::PendingForm>>,
    _permission_client: atman_runtime::permission::PermissionClientGuard,
}

impl SessionActor {
    async fn run(mut self) {
        loop {
            let input = tokio::select! {
                command = self.rx.recv() => ActorInput::Command(command),
                event = self.events_rx.recv() => ActorInput::Event(Box::new(event)),
                changed = self.goal_rx.changed() => ActorInput::Goal(changed),
                changed = self.todos_rx.changed() => ActorInput::Todos(changed),
                changed = self.plans_rx.changed() => ActorInput::Plans(changed),
                changed = self.context_rx.changed() => ActorInput::Context(changed),
                changed = self.forms_rx.changed() => ActorInput::Forms(changed),
            };
            match input {
                ActorInput::Command(None) => break,
                ActorInput::Command(Some(command)) => self.handle_command(command),
                ActorInput::Event(event) => match *event {
                    Ok(event) => self.apply_runtime_event(&event),
                    Err(broadcast::error::RecvError::Lagged(_)) => self.catch_up_projection(),
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                ActorInput::Goal(Ok(())) => {
                    let goal = self.goal_rx.borrow_and_update().clone();
                    if let Some(delta) = self.projection.set_goal(goal) {
                        self.publish_projection_delta(delta);
                    }
                }
                ActorInput::Todos(Ok(())) => {
                    let todos = self.todos_rx.borrow_and_update().clone();
                    if let Some(delta) = self.projection.set_todos(todos) {
                        self.publish_projection_delta(delta);
                    }
                }
                ActorInput::Plans(Ok(())) => {
                    let plans = self.plans_rx.borrow_and_update().clone();
                    if let Some(delta) = self.projection.set_plans(plans) {
                        self.publish_projection_delta(delta);
                    }
                }
                ActorInput::Context(Ok(())) => {
                    let context = self.context_rx.borrow_and_update().clone();
                    if let Some(delta) = self.projection.set_context(context) {
                        self.publish_projection_delta(delta);
                    }
                }
                ActorInput::Forms(Ok(())) => {
                    let forms = self.forms_rx.borrow_and_update().clone();
                    if let Some(delta) = self.projection.set_forms(forms) {
                        self.publish_projection_delta(delta);
                    }
                }
                ActorInput::Goal(Err(_))
                | ActorInput::Todos(Err(_))
                | ActorInput::Plans(Err(_))
                | ActorInput::Context(Err(_))
                | ActorInput::Forms(Err(_)) => break,
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
                let cancelled = self.runs.get(&run_id).is_some_and(|run| {
                    run.cancel.cancel();
                    true
                });
                let _ = reply.send(cancelled);
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
            } => self.register_prompt(id, kind, payload, responder),
            Command::DropPrompt { id } => self.drop_prompt(id),
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
            Command::Rename { title, reply } => {
                let result = self.rename(title);
                let _ = reply.send(result);
            }
            Command::Snapshot { reply } => {
                let _ = reply.send((self.event_cursor, self.projection.snapshot()));
            }
            Command::Updates {
                after_cursor,
                limit,
                reply,
            } => {
                let _ = reply.send(self.updates_response(after_cursor, limit));
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
        self.revision = self.revision.saturating_add(1);
        self.view_tx
            .send_replace(view_for(self.revision, &self.runs, &self.projection));
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
        self.event_cursor.0 = self.event_cursor.0.saturating_add(1);
        let envelope = ProjectionEventEnvelope {
            schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
            daemon_generation: self.daemon_generation.clone(),
            session_id: self.session_id.clone(),
            cursor: self.event_cursor,
            ts: chrono::Utc::now(),
            event: ServerEvent::ProjectionDelta { delta },
        };
        self.updates.push_back(envelope.clone());
        while self.updates.len() > UPDATE_RETENTION {
            self.updates.pop_front();
        }
        let _ = self.updates_tx.send(envelope);
        self.publish();
    }

    fn rename(&mut self, title: String) -> Result<SessionSummary> {
        atman_runtime::session_meta::SessionMeta::rename(self.session.dir(), title)
            .with_context(|| format!("rename session {}", self.session_id))?;
        let summary = session_summary(
            self.session.dir(),
            self.session_id.clone(),
            self.runs.values(),
        )?;
        if let Some(delta) = self.projection.set_metadata(self.session.meta()) {
            self.publish_projection_delta(delta);
        }
        Ok(summary)
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
        self.projection.set_forms(self.forms_rx.borrow().clone());
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
        }
    }

    fn create_permission_group(
        &self,
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
        Ok(CreatePermissionGroupResponse {
            group_id: group.group_id.0,
            request_ids: group
                .request_ids
                .into_iter()
                .map(|request_id| request_id.0)
                .collect(),
            revision: group.revision,
            label: group.label,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_permissions(
        &self,
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
        Ok(ResolvePermissionRequestsResponse {
            resolutions: results
                .into_iter()
                .map(|result| PermissionResolutionView {
                    request_id: result.request_id.0,
                    outcome: format!("{:?}", result.outcome),
                })
                .collect(),
        })
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
) -> SessionActorView {
    SessionActorView {
        revision,
        projection_revision: projection.projection().revision,
        runtime_event_seq: projection.last_runtime_seq(),
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
