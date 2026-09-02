use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use atman_proto::{FlowRunId, PromptId, SessionId, SessionStatus, SessionSummary};
use atman_runtime::event::{Event, EventSink};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

struct PendingPrompt {
    tx: oneshot::Sender<serde_json::Value>,
    broadcast_sink: Option<EventSink>,
}

pub struct DaemonState {
    data_dir: PathBuf,
    daemon_generation: String,
    sessions: Mutex<HashMap<SessionId, SessionActorEntry>>,
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

struct SessionActorEntry {
    session: std::sync::Arc<atman_runtime::Session>,
    runs: HashMap<FlowRunId, LiveRun>,
    _permission_client: atman_runtime::permission::PermissionClientGuard,
    owner_principal: String,
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

    pub fn register_session_run(
        &self,
        id: SessionId,
        session: std::sync::Arc<atman_runtime::Session>,
        run: LiveRun,
        owner_principal: impl Into<String>,
    ) -> Result<()> {
        let owner_principal = owner_principal.into();
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(entry) = sessions.get_mut(&id) {
            anyhow::ensure!(
                entry.owner_principal == owner_principal,
                "session {id} is owned by another principal"
            );
            anyhow::ensure!(
                std::sync::Arc::ptr_eq(&entry.session, &session),
                "session {id} is already registered with another runtime"
            );
            anyhow::ensure!(
                !entry.runs.contains_key(&run.run_id),
                "run {} is already registered",
                run.run_id
            );
            entry.runs.insert(run.run_id.clone(), run);
            return Ok(());
        }
        let permission_client = session.permission_broker().register_client();
        let mut runs = HashMap::new();
        runs.insert(run.run_id.clone(), run);
        sessions.insert(
            id,
            SessionActorEntry {
                session,
                runs,
                _permission_client: permission_client,
                owner_principal,
            },
        );
        Ok(())
    }

    pub fn authorized_live_session(
        &self,
        id: &SessionId,
        principal: &str,
    ) -> Option<std::sync::Arc<atman_runtime::Session>> {
        let sessions = self.sessions.lock().unwrap();
        let entry = sessions.get(id)?;
        (!entry.runs.is_empty() && entry.owner_principal == principal)
            .then(|| entry.session.clone())
    }

    pub fn authorized_session(
        &self,
        id: &SessionId,
        principal: &str,
    ) -> Option<std::sync::Arc<atman_runtime::Session>> {
        let sessions = self.sessions.lock().unwrap();
        let entry = sessions.get(id)?;
        (entry.owner_principal == principal).then(|| entry.session.clone())
    }

    pub fn owns_live_session(&self, id: &SessionId, principal: &str) -> bool {
        self.authorized_live_session(id, principal).is_some()
    }

    pub fn can_read_session(&self, id: &SessionId, principal: &str) -> bool {
        let sessions = self.sessions.lock().unwrap();
        match sessions.get(id) {
            Some(entry) => entry.owner_principal == principal,
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
            .get_mut(session_id)
            .is_some_and(|entry| entry.runs.remove(run_id).is_some())
    }

    pub fn remove_session(&self, id: &SessionId) -> bool {
        let removed = self.sessions.lock().unwrap().remove(id);
        let existed = removed.is_some();
        drop(removed);
        existed
    }

    pub fn live_session(&self, id: &SessionId) -> Option<std::sync::Arc<atman_runtime::Session>> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .filter(|entry| !entry.runs.is_empty())
            .map(|entry| entry.session.clone())
    }

    pub fn cancel_run(&self, run_id: &FlowRunId) -> bool {
        let cancel = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .find_map(|entry| entry.runs.get(run_id).map(|run| run.cancel.clone()));
        if let Some(cancel) = cancel {
            cancel.cancel();
            true
        } else {
            false
        }
    }

    pub fn rename_session(&self, sid: &SessionId, title: &str) -> Result<SessionSummary> {
        let path = self.sessions_root().join(sid.0.to_string());
        atman_runtime::session_meta::SessionMeta::rename(&path, title)
            .with_context(|| format!("rename session {sid}"))?;
        self.list_sessions()?
            .into_iter()
            .find(|summary| &summary.id == sid)
            .ok_or_else(|| anyhow::anyhow!("session not found: {sid}"))
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
                        .runs
                        .values()
                        .map(|run| run.started_at)
                        .min()
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
