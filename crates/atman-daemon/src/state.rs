use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use atman_proto::{FlowRunId, PromptId, SessionId, SessionStatus, SessionSummary};
use atman_runtime::event::{Event, EventSink};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::session_actor::SessionActorHandle;

struct PendingPrompt {
    tx: oneshot::Sender<serde_json::Value>,
    broadcast_sink: Option<EventSink>,
}

pub struct DaemonState {
    data_dir: PathBuf,
    daemon_generation: String,
    sessions: Mutex<HashMap<SessionId, SessionActorHandle>>,
    prompts: Mutex<HashMap<PromptId, PendingPrompt>>,
    launcher: Mutex<Option<std::sync::Arc<crate::run::RunLauncher>>>,
    provider_lifecycles: Mutex<HashMap<PathBuf, atman_runtime::ProviderLifecycle>>,
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
            prompts: Mutex::new(HashMap::new()),
            launcher: Mutex::new(None),
            provider_lifecycles: Mutex::new(HashMap::new()),
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

    pub fn register_pending_prompt(&self, id: PromptId) -> oneshot::Receiver<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.prompts.lock().unwrap().insert(
            id,
            PendingPrompt {
                tx,
                broadcast_sink: None,
            },
        );
        rx
    }

    pub fn register_pending_prompt_broadcast(
        &self,
        id: PromptId,
        kind: &str,
        payload: serde_json::Value,
        sink: EventSink,
    ) -> oneshot::Receiver<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.prompts.lock().unwrap().insert(
            id.clone(),
            PendingPrompt {
                tx,
                broadcast_sink: Some(sink.clone()),
            },
        );
        sink.emit(Event::PendingPrompt {
            prompt_id: id.0,
            kind: kind.to_string(),
            payload,
        });
        rx
    }

    pub fn resolve_prompt(&self, id: &PromptId, answer: serde_json::Value) -> bool {
        let Some(entry) = self.prompts.lock().unwrap().remove(id) else {
            return false;
        };
        if let Some(sink) = &entry.broadcast_sink {
            sink.emit(Event::PromptResolved {
                prompt_id: id.0,
                answer: answer.clone(),
            });
        }
        entry.tx.send(answer).is_ok()
    }

    pub fn drop_pending_prompt(&self, id: &PromptId) {
        if let Some(entry) = self.prompts.lock().unwrap().remove(id)
            && let Some(sink) = &entry.broadcast_sink
        {
            sink.emit(Event::PromptResolved {
                prompt_id: id.0,
                answer: serde_json::Value::Null,
            });
        }
    }

    pub fn pending_prompt_ids(&self) -> Vec<PromptId> {
        self.prompts.lock().unwrap().keys().cloned().collect()
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
        let owner_principal = owner_principal.into();
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
                        run.clone(),
                        owner_principal.clone(),
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
            entry.add_run(run).await?;
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

    pub async fn cancel_run(&self, run_id: &FlowRunId, principal: &str) -> Result<bool> {
        let actor = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .find(|entry| entry.owns(principal) && entry.view().runs.contains_key(run_id))
            .cloned();
        match actor {
            Some(actor) => actor.cancel_run(run_id.clone()).await,
            None => Ok(false),
        }
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
