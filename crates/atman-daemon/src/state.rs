use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use atman_proto::{FlowRunId, PromptId, SessionId, SessionStatus, SessionSummary};
use atman_runtime::event::{Event, EventSink};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

struct PendingPrompt {
    tx: Option<oneshot::Sender<serde_json::Value>>,
    broadcast_sink: Option<EventSink>,
    form_context: Option<(
        SessionId,
        atman_runtime::form::CompositeForm,
        std::sync::Arc<atman_runtime::session::DeferredFormInbox>,
    )>,
}

pub struct DaemonState {
    data_dir: PathBuf,
    daemon_generation: String,
    live: Mutex<HashMap<SessionId, LiveSessionEntry>>,
    prompts: Mutex<HashMap<PromptId, PendingPrompt>>,
    launcher: Mutex<Option<std::sync::Arc<crate::run::RunLauncher>>>,
    provider_lifecycles: Mutex<HashMap<PathBuf, atman_runtime::ProviderLifecycle>>,
}

#[derive(Clone)]
pub struct LiveSession {
    pub run_id: FlowRunId,
    pub flow_name: String,
    pub cancel: CancellationToken,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

struct LiveSessionEntry {
    live: Option<LiveSession>,
    broker: Option<std::sync::Arc<atman_runtime::Session>>,
    permission_client: Option<atman_runtime::permission::PermissionClientGuard>,
    owner_principal: Option<String>,
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
            live: Mutex::new(HashMap::new()),
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
                tx: Some(tx),
                broadcast_sink: None,
                form_context: None,
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
        self.register_pending_prompt_broadcast_with_session(id, kind, payload, sink, None)
    }

    pub fn register_pending_prompt_broadcast_with_session(
        &self,
        id: PromptId,
        kind: &str,
        payload: serde_json::Value,
        sink: EventSink,
        session: Option<&atman_runtime::Session>,
    ) -> oneshot::Receiver<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        let form_context = session.and_then(|session| {
            if kind != "form_ask" {
                return None;
            }
            let form =
                serde_json::from_value::<atman_runtime::form::CompositeForm>(payload.clone())
                    .or_else(|_| {
                        serde_json::from_value::<atman_runtime::form::FormKind>(payload.clone())
                            .map(|kind| atman_runtime::form::CompositeForm {
                                questions: vec![atman_runtime::form::FormQuestion {
                                    id: "question".into(),
                                    kind,
                                }],
                            })
                    })
                    .ok()?;
            Some((
                SessionId(session.id().0),
                form,
                session.deferred_form_inbox(),
            ))
        });
        self.prompts.lock().unwrap().insert(
            id.clone(),
            PendingPrompt {
                tx: Some(tx),
                broadcast_sink: Some(sink.clone()),
                form_context,
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
        self.resolve_prompt_for_session(id, answer, None)
    }

    pub fn resolve_prompt_for_session(
        &self,
        id: &PromptId,
        answer: serde_json::Value,
        session_id: Option<&SessionId>,
    ) -> bool {
        let mut prompts = self.prompts.lock().unwrap();
        let Some(entry) = prompts.get(id) else {
            return false;
        };
        let late = entry.tx.is_none();
        let submission = if late {
            let Some((owner, form, _)) = &entry.form_context else {
                return false;
            };
            if session_id != Some(owner) {
                return false;
            }
            let Ok(submission) =
                serde_json::from_value::<atman_runtime::form::FormSubmission>(answer.clone())
            else {
                return false;
            };
            if !form.accepts(&submission) {
                return false;
            }
            Some(submission)
        } else {
            None
        };
        let entry = prompts
            .remove(id)
            .expect("pending prompt was checked under the same lock");
        if let (Some(submission), Some((_, form, inbox))) = (submission, entry.form_context) {
            inbox.record(atman_runtime::form::DeferredFormAnswer {
                prompt_id: id.0.to_string(),
                form,
                submission,
            });
        }
        if let Some(sink) = &entry.broadcast_sink {
            sink.emit(Event::PromptResolved {
                prompt_id: id.0,
                answer: answer.clone(),
            });
        }
        if let Some(tx) = entry.tx {
            tx.send(answer).is_ok()
        } else {
            true
        }
    }

    pub fn expire_pending_prompt(&self, id: &PromptId) -> bool {
        let mut prompts = self.prompts.lock().unwrap();
        let Some(entry) = prompts.get_mut(id) else {
            return false;
        };
        if entry.form_context.is_none() {
            drop(prompts);
            self.drop_pending_prompt(id);
            return false;
        }
        if entry.tx.is_none() {
            return true;
        }
        entry.tx.take();
        if let Some(sink) = &entry.broadcast_sink {
            sink.emit(Event::PromptExpired { prompt_id: id.0 });
        }
        true
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

    pub fn register_live(&self, id: SessionId, entry: LiveSession) {
        let mut live = self.live.lock().unwrap();
        live.entry(id)
            .and_modify(|current| current.live = Some(entry.clone()))
            .or_insert(LiveSessionEntry {
                live: Some(entry),
                broker: None,
                permission_client: None,
                owner_principal: None,
            });
    }

    pub fn register_broker(
        &self,
        id: SessionId,
        session: std::sync::Arc<atman_runtime::Session>,
        owner_principal: impl Into<String>,
    ) {
        let permission_client = session.permission_broker().register_client();
        let mut live = self.live.lock().unwrap();
        let entry = live.entry(id).or_insert(LiveSessionEntry {
            live: None,
            broker: None,
            permission_client: None,
            owner_principal: None,
        });
        entry.broker = Some(session);
        entry.permission_client = Some(permission_client);
        entry.owner_principal = Some(owner_principal.into());
    }

    pub fn authorized_live_session(
        &self,
        id: &SessionId,
        principal: &str,
    ) -> Option<std::sync::Arc<atman_runtime::Session>> {
        let live = self.live.lock().unwrap();
        let entry = live.get(id)?;
        (entry.live.is_some() && entry.owner_principal.as_deref() == Some(principal))
            .then(|| entry.broker.clone())
            .flatten()
    }

    pub fn owns_live_session(&self, id: &SessionId, principal: &str) -> bool {
        self.authorized_live_session(id, principal).is_some()
    }

    pub fn deregister_broker(&self, id: &SessionId) {
        if let Some(entry) = self.live.lock().unwrap().get_mut(id) {
            entry.broker = None;
            entry.permission_client = None;
            entry.owner_principal = None;
        }
    }

    pub fn deregister_live(&self, id: &SessionId) {
        self.live.lock().unwrap().remove(id);
    }

    pub fn live_session(&self, id: &SessionId) -> Option<std::sync::Arc<atman_runtime::Session>> {
        self.live
            .lock()
            .unwrap()
            .get(id)
            .filter(|entry| entry.live.is_some())
            .and_then(|entry| entry.broker.clone())
    }

    pub fn cancel_run(&self, run_id: &FlowRunId) -> bool {
        let live = self.live.lock().unwrap();
        for entry in live.values().filter_map(|entry| entry.live.as_ref()) {
            if &entry.run_id == run_id {
                entry.cancel.cancel();
                return true;
            }
        }
        false
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
            let live = self.live.lock().unwrap();
            live.iter()
                .filter_map(|(sid, entry)| {
                    entry
                        .live
                        .as_ref()
                        .map(|live| (sid.clone(), live.started_at))
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
