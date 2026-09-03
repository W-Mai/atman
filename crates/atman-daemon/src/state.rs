use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use atman_proto::{
    DaemonGeneration, EventCursor, FlowRunId, GetSessionUpdatesResponse, PromptId, ResyncRequired,
    SNAPSHOT_SCHEMA_VERSION, SessionId, SessionSnapshot, SessionStatus, SessionSummary,
};
use tokio_util::sync::CancellationToken;

use crate::idempotency::IdempotencyRegistry;
use crate::projection::SessionProjector;
use crate::session_actor::{RunAdmission, SessionActorHandle};

pub struct DaemonState {
    data_dir: PathBuf,
    daemon_generation: String,
    sessions: Mutex<HashMap<SessionId, SessionActorHandle>>,
    session_loads: Mutex<HashMap<SessionId, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    launcher: Mutex<Option<std::sync::Arc<crate::run::RunLauncher>>>,
    provider_lifecycles: Mutex<HashMap<PathBuf, atman_runtime::ProviderLifecycle>>,
    pub(crate) idempotency: IdempotencyRegistry,
}

pub(crate) struct LoadedSession {
    pub session: std::sync::Arc<atman_runtime::Session>,
    pub projection: SessionProjector,
}

#[derive(Clone)]
pub struct LiveRun {
    pub run_id: FlowRunId,
    pub flow_name: String,
    pub cancel: CancellationToken,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl DaemonState {
    pub fn new(data_dir: PathBuf) -> Self {
        Self::new_with_generation(data_dir, uuid::Uuid::now_v7().to_string())
    }

    pub fn new_with_generation(data_dir: PathBuf, daemon_generation: String) -> Self {
        assert!(
            !daemon_generation.is_empty(),
            "daemon generation must be non-empty"
        );
        Self {
            data_dir,
            daemon_generation,
            sessions: Mutex::new(HashMap::new()),
            session_loads: Mutex::new(HashMap::new()),
            launcher: Mutex::new(None),
            provider_lifecycles: Mutex::new(HashMap::new()),
            idempotency: IdempotencyRegistry::default(),
        }
    }

    pub fn daemon_generation(&self) -> &str {
        &self.daemon_generation
    }

    pub fn set_launcher(&self, launcher: std::sync::Arc<crate::run::RunLauncher>) {
        *self.launcher.lock().unwrap() = Some(launcher);
    }

    pub fn launcher(&self) -> Option<std::sync::Arc<crate::run::RunLauncher>> {
        self.launcher.lock().unwrap().clone()
    }

    pub(crate) fn provider_lifecycle_for(
        &self,
        config_dir: Option<&Path>,
    ) -> Result<atman_runtime::ProviderLifecycle> {
        let hub = crate::bootstrap::resolve_config_hub(config_dir)?;
        let key = hub.config_dir().to_path_buf();
        let mut lifecycles = self.provider_lifecycles.lock().unwrap();
        Ok(lifecycles
            .entry(key)
            .or_insert_with(|| {
                atman_runtime::ProviderLifecycle::new(
                    hub,
                    atman_runtime::provider::ProviderRegistry::new(),
                )
            })
            .clone())
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn register_pending_prompt(
        &self,
        session_id: &SessionId,
        id: PromptId,
        kind: &str,
        payload: serde_json::Value,
    ) -> tokio::sync::oneshot::Receiver<serde_json::Value> {
        let actor = self.sessions.lock().unwrap().get(session_id).cloned();
        match actor {
            Some(actor) => actor.register_prompt(id, kind.to_owned(), payload),
            None => {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                drop(sender);
                receiver
            }
        }
    }

    pub fn drop_pending_prompt(&self, session_id: &SessionId, id: &PromptId) {
        if let Some(actor) = self.sessions.lock().unwrap().get(session_id).cloned() {
            actor.drop_prompt(id.clone());
        }
    }

    pub(crate) async fn resolve_prompt(
        &self,
        session_id: &SessionId,
        id: PromptId,
        answer: serde_json::Value,
        principal: &str,
    ) -> Result<crate::session_actor::PromptResolutionCommit> {
        let actor = self
            .authorized_actor(session_id, principal)
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?;
        actor.resolve_prompt(id, answer).await
    }

    pub(crate) async fn submit_form(
        &self,
        session_id: &SessionId,
        id: String,
        submission: atman_proto::FormSubmission,
        principal: &str,
    ) -> Result<crate::session_actor::FormResolutionCommit> {
        let actor = self
            .authorized_actor(session_id, principal)
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?;
        actor.submit_form(id, submission).await
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub async fn register_session_run(
        &self,
        id: SessionId,
        session: std::sync::Arc<atman_runtime::Session>,
        run: LiveRun,
        owner_principal: impl Into<String>,
    ) -> Result<()> {
        self.register_session_with_runs(
            id,
            session,
            vec![run],
            owner_principal.into(),
            RunAdmission::Concurrent,
            None,
        )
        .await
    }

    pub async fn register_session_root_run(
        &self,
        id: SessionId,
        session: std::sync::Arc<atman_runtime::Session>,
        run: LiveRun,
        owner_principal: impl Into<String>,
    ) -> Result<()> {
        self.register_session_with_runs(
            id,
            session,
            vec![run],
            owner_principal.into(),
            RunAdmission::IdleSession,
            None,
        )
        .await
    }

    pub async fn register_session(
        &self,
        id: SessionId,
        session: std::sync::Arc<atman_runtime::Session>,
        owner_principal: impl Into<String>,
    ) -> Result<()> {
        self.register_session_with_runs(
            id,
            session,
            Vec::new(),
            owner_principal.into(),
            RunAdmission::Concurrent,
            None,
        )
        .await
    }

    async fn register_session_with_runs(
        &self,
        id: SessionId,
        session: std::sync::Arc<atman_runtime::Session>,
        initial_runs: Vec<LiveRun>,
        owner_principal: String,
        admission: RunAdmission,
        restored_projection: Option<SessionProjector>,
    ) -> Result<()> {
        let existing = {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(entry) = sessions.get(&id) {
                Some(entry.clone())
            } else {
                sessions.insert(
                    id.clone(),
                    SessionActorHandle::spawn(
                        id.clone(),
                        session.clone(),
                        initial_runs.clone(),
                        owner_principal.clone(),
                        DaemonGeneration(self.daemon_generation.clone()),
                        restored_projection,
                    ),
                );
                None
            }
        };
        if let Some(entry) = existing {
            anyhow::ensure!(
                entry.owns(&owner_principal),
                "session {id} is owned by another principal"
            );
            anyhow::ensure!(
                entry.owns_session(&session),
                "session {id} is already registered with another runtime"
            );
            for run in initial_runs {
                entry.add_run(run, admission).await?;
            }
        }
        Ok(())
    }

    fn authorized_actor(&self, id: &SessionId, principal: &str) -> Option<SessionActorHandle> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .filter(|entry| entry.owns(principal))
            .cloned()
    }

    fn loaded_runtime_session(
        &self,
        id: &SessionId,
        principal: &str,
    ) -> Result<Option<std::sync::Arc<atman_runtime::Session>>> {
        let actor = self.sessions.lock().unwrap().get(id).cloned();
        let Some(actor) = actor else {
            return Ok(None);
        };
        anyhow::ensure!(
            actor.owns(principal),
            "session {id} is owned by another principal"
        );
        Ok(Some(actor.runtime_session()))
    }

    pub(crate) async fn get_or_load_session<F, Fut>(
        &self,
        id: &SessionId,
        principal: &str,
        load: F,
    ) -> Result<std::sync::Arc<atman_runtime::Session>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<LoadedSession>>,
    {
        if let Some(session) = self.loaded_runtime_session(id, principal)? {
            return Ok(session);
        }
        let load_gate = self
            .session_loads
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_default()
            .clone();
        let _guard = load_gate.lock().await;
        if let Some(session) = self.loaded_runtime_session(id, principal)? {
            return Ok(session);
        }
        let restored = load().await?;
        let session = restored.session;
        self.register_session_with_runs(
            id.clone(),
            session.clone(),
            Vec::new(),
            principal.to_owned(),
            RunAdmission::Concurrent,
            Some(restored.projection),
        )
        .await?;
        Ok(session)
    }

    pub fn owns_live_session(&self, id: &SessionId, principal: &str) -> bool {
        self.authorized_actor(id, principal)
            .is_some_and(|actor| actor.view().is_live())
    }

    pub fn can_read_session(&self, id: &SessionId, principal: &str) -> bool {
        let sessions = self.sessions.lock().unwrap();
        match sessions.get(id) {
            Some(entry) => entry.owns(principal),
            None => self
                .sessions_root()
                .join(id.to_string())
                .join("events.jsonl")
                .is_file(),
        }
    }

    pub async fn session_snapshot(
        &self,
        id: &SessionId,
        principal: &str,
    ) -> Result<SessionSnapshot> {
        let actor = self.sessions.lock().unwrap().get(id).cloned();
        let (cursor, projection) = if let Some(actor) = actor {
            anyhow::ensure!(
                actor.owns(principal),
                "session {id} is owned by another principal"
            );
            actor.snapshot().await?
        } else {
            let session_dir = self.sessions_root().join(id.to_string());
            let projection =
                crate::projection::load_historical_projection(id.clone(), &session_dir).await?;
            let config_dir = self
                .launcher()
                .and_then(|launcher| launcher.config_dir.clone());
            let redactor = crate::bootstrap::build_redactor(config_dir.as_deref());
            let projection =
                crate::projection::redacted_projection(&projection, redactor.as_deref())?;
            (EventCursor(projection.revision.0), projection)
        };
        Ok(SessionSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            daemon_generation: DaemonGeneration(self.daemon_generation.clone()),
            cursor,
            projection,
        })
    }

    pub async fn session_updates(
        &self,
        id: &SessionId,
        principal: &str,
        after_cursor: EventCursor,
        limit: Option<usize>,
    ) -> Result<GetSessionUpdatesResponse> {
        let actor = self.sessions.lock().unwrap().get(id).cloned();
        if let Some(actor) = actor {
            anyhow::ensure!(
                actor.owns(principal),
                "session {id} is owned by another principal"
            );
            return actor.updates(after_cursor, limit).await;
        }

        let snapshot = self.session_snapshot(id, principal).await?;
        if after_cursor == snapshot.cursor {
            return Ok(GetSessionUpdatesResponse {
                daemon_generation: DaemonGeneration(self.daemon_generation.clone()),
                events: Vec::new(),
                next_cursor: snapshot.cursor,
                has_more: false,
                resync_required: None,
            });
        }
        Ok(GetSessionUpdatesResponse {
            daemon_generation: DaemonGeneration(self.daemon_generation.clone()),
            events: Vec::new(),
            next_cursor: snapshot.cursor,
            has_more: false,
            resync_required: Some(ResyncRequired {
                requested_after: after_cursor,
                available_from: snapshot.cursor,
                snapshot_revision: snapshot.projection.revision,
                reason: "session is idle and has no retained live update window".into(),
            }),
        })
    }

    pub(crate) async fn subscribe_session_updates(
        &self,
        id: &SessionId,
        principal: &str,
    ) -> Result<
        Option<(
            tokio::sync::broadcast::Receiver<atman_proto::ProjectionEventEnvelope>,
            Option<std::sync::Arc<atman_runtime::redact::Redactor>>,
        )>,
    > {
        let actor = self.sessions.lock().unwrap().get(id).cloned();
        let Some(actor) = actor else {
            anyhow::ensure!(
                self.sessions_root()
                    .join(id.to_string())
                    .join("events.jsonl")
                    .is_file(),
                "session not found: {id}"
            );
            return Ok(None);
        };
        anyhow::ensure!(
            actor.owns(principal),
            "session {id} is owned by another principal"
        );
        Ok(Some(actor.subscribe_updates().await?))
    }

    pub fn finish_run(&self, session_id: &SessionId, run_id: &FlowRunId) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(session_id)
            .filter(|entry| entry.view().runs.contains_key(run_id))
            .is_some_and(|entry| entry.finish_run(run_id.clone()))
    }

    pub fn remove_session(&self, id: &SessionId) -> bool {
        let removed = self.sessions.lock().unwrap().remove(id);
        let existed = removed.is_some();
        drop(removed);
        existed
    }

    pub fn has_live_runs(&self, id: &SessionId) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|entry| entry.view().is_live())
    }

    pub fn has_live_run(&self, id: &SessionId, run_id: &FlowRunId) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|entry| entry.view().runs.contains_key(run_id))
    }

    pub fn session_revision(&self, id: &SessionId) -> Option<u64> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|entry| entry.view().revision)
    }

    pub fn session_projection_revision(&self, id: &SessionId) -> Option<atman_proto::Revision> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|entry| entry.view().projection_revision)
    }

    pub fn session_runtime_event_seq(&self, id: &SessionId) -> Option<u64> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|entry| entry.view().runtime_event_seq)
    }

    pub fn is_authorized_session(&self, id: &SessionId, principal: &str) -> bool {
        self.authorized_actor(id, principal).is_some()
    }

    pub async fn cancel_run(
        &self,
        session_id: &SessionId,
        run_id: &FlowRunId,
        principal: &str,
    ) -> Result<crate::RunCancellationCommit> {
        let actor = self
            .authorized_actor(session_id, principal)
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?;
        actor.cancel_run(run_id.clone()).await
    }

    pub(crate) async fn interject_run(
        &self,
        session_id: &SessionId,
        run_id: FlowRunId,
        text: String,
        level: atman_runtime::injection::InjectionLevel,
        redirect_target: Option<String>,
        principal: &str,
    ) -> Result<crate::session_actor::InterjectionCommit> {
        let actor = self
            .authorized_actor(session_id, principal)
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?;
        actor.interject(run_id, text, level, redirect_target).await
    }

    pub async fn rename_session(
        &self,
        sid: &SessionId,
        title: &str,
        principal: &str,
    ) -> Result<SessionSummary> {
        let actor = self.sessions.lock().unwrap().get(sid).cloned();
        if let Some(actor) = actor {
            anyhow::ensure!(
                actor.owns(principal),
                "session {sid} is owned by another principal"
            );
            return actor.rename(title.to_owned()).await;
        }
        let path = self.sessions_root().join(sid.0.to_string());
        atman_runtime::session_meta::SessionMeta::rename(&path, title)
            .with_context(|| format!("rename session {sid}"))?;
        self.list_sessions()?
            .into_iter()
            .find(|summary| &summary.id == sid)
            .ok_or_else(|| anyhow::anyhow!("session not found: {sid}"))
    }

    pub async fn list_permission_requests(
        &self,
        session_id: &SessionId,
        principal: &str,
    ) -> Result<atman_proto::ListPermissionRequestsResponse> {
        let actor = self
            .authorized_actor(session_id, principal)
            .filter(|actor| actor.view().is_live())
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?;
        actor.list_permissions().await
    }

    pub async fn create_permission_group(
        &self,
        request: atman_proto::CreatePermissionGroupRequest,
        principal: &str,
    ) -> Result<atman_proto::CreatePermissionGroupResponse> {
        let actor = self
            .authorized_actor(&request.session_id, principal)
            .filter(|actor| actor.view().is_live())
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?;
        actor
            .create_permission_group(
                request.request_ids,
                request.expected_request_revisions,
                request.label,
            )
            .await
    }

    pub async fn resolve_permission_requests(
        &self,
        request: atman_proto::ResolvePermissionRequestsRequest,
        principal: &str,
        action: atman_runtime::permission::PermissionAction,
        scope: Option<atman_runtime::permission::GrantScope>,
    ) -> Result<atman_proto::ResolvePermissionRequestsResponse> {
        let actor = self
            .authorized_actor(&request.session_id, principal)
            .filter(|actor| actor.view().is_live())
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?;
        let (request_ids, expected_request_revisions, group) = match request.selector {
            atman_proto::PermissionRpcSelector::Requests {
                request_ids,
                expected_request_revisions,
            } => (request_ids, expected_request_revisions, None),
            atman_proto::PermissionRpcSelector::Group {
                group_id,
                expected_group_revision,
            } => (
                Vec::new(),
                Default::default(),
                Some((group_id, expected_group_revision)),
            ),
        };
        actor
            .resolve_permissions(
                request_ids,
                expected_request_revisions,
                group,
                action,
                scope,
                request.reason,
                principal.to_owned(),
            )
            .await
    }

    pub fn list_sessions_query(
        &self,
        project_root: Option<&str>,
        search: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<SessionSummary>> {
        let mut summaries = self.list_sessions()?;
        if let Some(project_root) = project_root {
            let query = atman_runtime::session_meta::SessionDiscoveryQuery::current_project(
                std::path::Path::new(project_root),
            );
            let sessions_root = self.sessions_root();
            summaries.retain(|summary| {
                let path = sessions_root.join(summary.id.to_string());
                query.matches_meta(atman_runtime::session_meta::SessionMeta::load(&path).as_ref())
            });
        }
        if let Some(search) = search.map(str::trim).filter(|value| !value.is_empty()) {
            let needle = search.to_lowercase();
            summaries.retain(|summary| {
                summary.id.to_string().to_lowercase().contains(&needle)
                    || summary.title.to_lowercase().contains(&needle)
                    || summary
                        .project_root
                        .as_ref()
                        .is_some_and(|root| root.to_lowercase().contains(&needle))
            });
        }
        if let Some(limit) = limit {
            summaries.truncate(limit);
        }
        Ok(summaries)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let live_ids: HashMap<SessionId, chrono::DateTime<chrono::Utc>> = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .iter()
                .filter_map(|(sid, entry)| {
                    entry
                        .view()
                        .first_run_started_at()
                        .map(|started_at| (sid.clone(), started_at))
                })
                .collect()
        };

        let mut out: Vec<SessionSummary> = Vec::new();
        let mut seen: std::collections::HashSet<SessionId> = std::collections::HashSet::new();

        let sessions_root = self.sessions_root();
        if sessions_root.exists() {
            for entry in std::fs::read_dir(&sessions_root)
                .with_context(|| format!("read_dir {}", sessions_root.display()))?
            {
                let entry = entry?;
                if !entry.path().is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let Ok(uuid) = uuid::Uuid::parse_str(&name) else {
                    continue;
                };
                let sid = SessionId(uuid);
                let stats =
                    atman_runtime::session_meta::SessionStats::load_or_rebuild(&entry.path())
                        .unwrap_or_default();
                let status = if live_ids.contains_key(&sid) {
                    SessionStatus::Running
                } else {
                    SessionStatus::Finished
                };
                let meta = atman_runtime::session_meta::SessionMeta::load(&entry.path());
                out.push(SessionSummary {
                    id: sid.clone(),
                    event_count: stats.event_count as usize,
                    first_ts: stats.first_ts,
                    status,
                    title: meta
                        .as_ref()
                        .and_then(|m| m.title.clone())
                        .unwrap_or_else(|| "Untitled session".into()),
                    goal: atman_runtime::memory::goal::GoalStore::at(entry.path())
                        .get()
                        .ok(),
                    project_root: meta
                        .as_ref()
                        .and_then(|m| m.project_root.as_ref())
                        .map(|p| p.display().to_string()),
                    name_source: match meta
                        .as_ref()
                        .map(|metadata| metadata.name_source)
                        .unwrap_or_default()
                    {
                        atman_runtime::session_meta::NameSource::Auto => {
                            atman_proto::NameSource::Auto
                        }
                        atman_runtime::session_meta::NameSource::User => {
                            atman_proto::NameSource::User
                        }
                    },
                });
                seen.insert(sid);
            }
        }
        for (sid, started_at) in &live_ids {
            if !seen.contains(sid) {
                out.push(SessionSummary {
                    id: sid.clone(),
                    event_count: 0,
                    first_ts: Some(*started_at),
                    status: SessionStatus::Running,
                    title: "Untitled session".into(),
                    goal: None,
                    project_root: None,
                    name_source: atman_proto::NameSource::Auto,
                });
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.id.0));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn concurrent_session_loads_share_one_runtime() {
        let state = Arc::new(DaemonState::new(
            tempfile::tempdir().unwrap().path().to_path_buf(),
        ));
        let session = Arc::new(atman_runtime::Session::open_ephemeral());
        let session_id = SessionId(session.id().0);
        let load_count = Arc::new(AtomicUsize::new(0));
        let load = |state: Arc<DaemonState>| {
            let session = session.clone();
            let session_id = session_id.clone();
            let projection_session_id = session_id.clone();
            let load_count = load_count.clone();
            async move {
                state
                    .get_or_load_session(&session_id, "owner", move || async move {
                        load_count.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                        Ok(LoadedSession {
                            projection: SessionProjector::from_events(
                                projection_session_id,
                                session.meta(),
                                &[],
                            ),
                            session,
                        })
                    })
                    .await
            }
        };

        let (first, second) = tokio::join!(load(state.clone()), load(state));
        let first = first.unwrap();
        let second = second.unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(load_count.load(Ordering::SeqCst), 1);
    }
}
