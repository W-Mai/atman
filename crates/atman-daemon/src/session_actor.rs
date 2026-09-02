use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::{Context, Result};
use atman_proto::{
    CreatePermissionGroupResponse, FlowRunId, ListPermissionRequestsResponse, PermissionGroupView,
    PermissionRequestView, PermissionResolutionView, ResolvePermissionRequestsResponse, SessionId,
    SessionSummary,
};
use tokio::sync::{mpsc, oneshot, watch};

use crate::state::LiveRun;

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionActorView {
    pub revision: u64,
    pub runs: HashMap<FlowRunId, LiveRunView>,
}

#[derive(Debug, Clone)]
pub(crate) struct LiveRunView {
    pub started_at: chrono::DateTime<chrono::Utc>,
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
        initial_run: LiveRun,
        owner_principal: String,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut runs = HashMap::new();
        runs.insert(initial_run.run_id.clone(), initial_run);
        let (view_tx, view) = watch::channel(view_for(1, &runs));
        let actor = SessionActor {
            session_id,
            session: session.clone(),
            runs,
            revision: 1,
            view_tx,
            rx,
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

    pub async fn add_run(&self, run: LiveRun) -> Result<()> {
        request(&self.tx, |reply| Command::AddRun { run, reply }).await?
    }

    pub fn finish_run(&self, run_id: FlowRunId) -> bool {
        self.tx.send(Command::FinishRun { run_id }).is_ok()
    }

    pub async fn cancel_run(&self, run_id: FlowRunId) -> Result<bool> {
        request(&self.tx, |reply| Command::CancelRun { run_id, reply }).await
    }

    pub async fn rename(&self, title: String) -> Result<SessionSummary> {
        request(&self.tx, |reply| Command::Rename { title, reply }).await?
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
        reply: oneshot::Sender<Result<()>>,
    },
    FinishRun {
        run_id: FlowRunId,
    },
    CancelRun {
        run_id: FlowRunId,
        reply: oneshot::Sender<bool>,
    },
    Rename {
        title: String,
        reply: oneshot::Sender<Result<SessionSummary>>,
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

struct SessionActor {
    session_id: SessionId,
    session: Arc<atman_runtime::Session>,
    runs: HashMap<FlowRunId, LiveRun>,
    revision: u64,
    view_tx: watch::Sender<SessionActorView>,
    rx: mpsc::UnboundedReceiver<Command>,
    _permission_client: atman_runtime::permission::PermissionClientGuard,
}

impl SessionActor {
    async fn run(mut self) {
        while let Some(command) = self.rx.recv().await {
            match command {
                Command::AddRun { run, reply } => {
                    let result = if self.runs.contains_key(&run.run_id) {
                        Err(anyhow::anyhow!("run {} is already registered", run.run_id))
                    } else {
                        self.runs.insert(run.run_id.clone(), run);
                        self.publish();
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
                Command::Rename { title, reply } => {
                    let result = self.rename(title);
                    let _ = reply.send(result);
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
                    let result = self.create_permission_group(
                        request_ids,
                        expected_request_revisions,
                        label,
                    );
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
    }

    fn publish(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.view_tx
            .send_replace(view_for(self.revision, &self.runs));
    }

    fn rename(&self, title: String) -> Result<SessionSummary> {
        atman_runtime::session_meta::SessionMeta::rename(self.session.dir(), title)
            .with_context(|| format!("rename session {}", self.session_id))?;
        session_summary(
            self.session.dir(),
            self.session_id.clone(),
            self.runs.values(),
        )
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

fn view_for(revision: u64, runs: &HashMap<FlowRunId, LiveRun>) -> SessionActorView {
    SessionActorView {
        revision,
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
