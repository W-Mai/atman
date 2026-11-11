use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use atman_proto::{FlowRunId, PromptId, SessionId, SessionStatus, SessionSummary};
use atman_runtime::event::{Event, EventSink};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// The operator authenticated by the owner-only socket and token-protected HTTP transport.
pub const LOCAL_OPERATOR: &str = "local-daemon";

struct PendingPrompt {
    submission: std::sync::Arc<tokio::sync::Mutex<()>>,
    tx: Option<oneshot::Sender<serde_json::Value>>,
    broadcast_sink: Option<EventSink>,
    form_context: Option<(
        SessionId,
        atman_runtime::form::CompositeForm,
        std::sync::Arc<atman_runtime::session::DeferredFormInbox>,
        Option<atman_runtime::flow_store::branches::HistoryHead>,
    )>,
}

pub struct DaemonState {
    pub(crate) session_service: std::sync::OnceLock<crate::flow_service::SessionService>,
    data_dir: PathBuf,
    daemon_generation: String,
    live: Mutex<HashMap<SessionId, LiveSessionEntry>>,
    prompts: Mutex<HashMap<PromptId, PendingPrompt>>,
    launcher: Mutex<Option<std::sync::Arc<crate::run::RunLauncher>>>,
    provider_lifecycles: Mutex<HashMap<PathBuf, atman_runtime::ProviderLifecycle>>,
    stopping: CancellationToken,
    live_changed: tokio::sync::watch::Sender<()>,
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
            session_service: Default::default(),
            data_dir,
            daemon_generation,
            live: Mutex::new(HashMap::new()),
            prompts: Mutex::new(HashMap::new()),
            launcher: Mutex::new(None),
            provider_lifecycles: Mutex::new(HashMap::new()),
            stopping: CancellationToken::new(),
            live_changed: tokio::sync::watch::channel(()).0,
        }
    }

    pub fn daemon_generation(&self) -> &str {
        &self.daemon_generation
    }

    pub fn shutdown_signal(&self) -> CancellationToken {
        self.stopping.clone()
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.is_cancelled()
    }

    /// Stop new requests and wake transports; execution cleanup is completed by `shutdown`.
    pub fn request_shutdown(&self) {
        let _live = self.live.lock().unwrap();
        self.stopping.cancel();
    }

    /// Drain both retained instances and registered legacy executions before process exit.
    pub async fn shutdown(self: &std::sync::Arc<Self>) -> Result<()> {
        self.request_shutdown();
        let state = self.clone();
        tokio::spawn(async move {
            let mut changes = state.live_changed.subscribe();
            let cancel_legacy = async {
                loop {
                    changes.borrow_and_update();
                    let live: Vec<_> = state
                        .live
                        .lock()
                        .unwrap()
                        .values()
                        .filter_map(|entry| {
                            entry
                                .live
                                .as_ref()
                                .map(|run| (run.cancel.clone(), entry.broker.clone()))
                        })
                        .collect();
                    if live.is_empty() {
                        break;
                    }
                    for (cancel, context) in live {
                        if let Some(context) = context {
                            context.cancel_flow();
                        }
                        cancel.cancel();
                    }
                    if changes.changed().await.is_err() {
                        break;
                    }
                }
            };
            let (result, ()) = tokio::join!(crate::flow_service::shutdown(&state), cancel_legacy);
            state.live.lock().unwrap().clear();
            state.provider_lifecycles.lock().unwrap().clear();
            result
        })
        .await?
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
        if let Some(lifecycle) = self.provider_lifecycles.lock().unwrap().get(&key).cloned() {
            return Ok(lifecycle);
        }
        let providers =
            atman_runtime::provider::ProviderRegistry::with_models(hub.models().clone());
        let lifecycle = atman_runtime::ProviderLifecycle::new(hub, providers)?;
        let selected = self
            .provider_lifecycles
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| lifecycle.clone())
            .clone();
        // Concurrent constructors share the source coordinator; discard unused adapters outside the map lock.
        drop(lifecycle);
        Ok(selected)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn register_pending_prompt(&self, id: PromptId) -> oneshot::Receiver<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.prompts.lock().unwrap().insert(
            id,
            PendingPrompt {
                submission: Default::default(),
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
                session.deferred_form_inbox().history_head(),
            ))
        });
        let mut prompts = self.prompts.lock().unwrap();
        prompts.insert(
            id.clone(),
            PendingPrompt {
                submission: Default::default(),
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
        drop(prompts);
        rx
    }

    pub async fn resolve_prompt(
        &self,
        id: &PromptId,
        answer: serde_json::Value,
    ) -> std::io::Result<bool> {
        self.resolve_prompt_for_session(id, answer, None).await
    }

    pub async fn resolve_prompt_for_session(
        &self,
        id: &PromptId,
        answer: serde_json::Value,
        session_id: Option<&SessionId>,
    ) -> std::io::Result<bool> {
        let guard = self
            .prompts
            .lock()
            .unwrap()
            .get(id)
            .map(|entry| entry.submission.clone());
        let Some(guard) = guard else { return Ok(false) };
        let _submission_guard = guard.lock().await;
        let (inbox, deferred) = {
            let mut prompts = self.prompts.lock().unwrap();
            let Some(entry) = prompts
                .get(id)
                .filter(|entry| std::sync::Arc::ptr_eq(&entry.submission, &guard))
            else {
                return Ok(false);
            };
            if entry.tx.is_some() {
                let entry = prompts.remove(id).unwrap();
                return Ok(Self::deliver_prompt(entry, id, answer));
            }
            let Some((owner, form, _, _)) = &entry.form_context else {
                return Ok(false);
            };
            if session_id != Some(owner) {
                return Ok(false);
            }
            let Ok(submission) =
                serde_json::from_value::<atman_runtime::form::FormSubmission>(answer.clone())
            else {
                return Ok(false);
            };
            if !form.accepts(&submission) {
                return Ok(false);
            }
            let (_, form, inbox, head) = entry.form_context.as_ref().unwrap();
            (
                inbox.clone(),
                atman_runtime::form::DeferredFormAnswer {
                    expected_head: head.clone(),
                    prompt_id: id.0.to_string(),
                    form: form.clone(),
                    submission,
                },
            )
        };
        inbox.record(deferred).await?;
        let mut prompts = self.prompts.lock().unwrap();
        if prompts
            .get(id)
            .is_some_and(|entry| std::sync::Arc::ptr_eq(&entry.submission, &guard))
        {
            let entry = prompts.remove(id).unwrap();
            Self::deliver_prompt(entry, id, answer);
        }
        Ok(true)
    }

    fn deliver_prompt(entry: PendingPrompt, id: &PromptId, answer: serde_json::Value) -> bool {
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

    pub fn register_live(&self, id: SessionId, entry: LiveSession) -> Result<()> {
        let mut live = self.live.lock().unwrap();
        anyhow::ensure!(!self.is_stopping(), "daemon is stopping");
        live.entry(id)
            .and_modify(|current| current.live = Some(entry.clone()))
            .or_insert(LiveSessionEntry {
                live: Some(entry),
                broker: None,
                permission_client: None,
                owner_principal: None,
            });
        drop(live);
        self.live_changed.send_modify(|_| {});
        Ok(())
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
        self.live_changed.send_modify(|_| {});
    }

    pub fn live_session(&self, id: &SessionId) -> Option<std::sync::Arc<atman_runtime::Session>> {
        self.live
            .lock()
            .unwrap()
            .get(id)
            .filter(|entry| entry.live.is_some())
            .and_then(|entry| entry.broker.clone())
    }

    pub async fn cancel_run(&self, run_id: &FlowRunId) -> Result<bool> {
        self.cancel_run_as(run_id, LOCAL_OPERATOR).await
    }

    pub(crate) async fn cancel_run_as(&self, run_id: &FlowRunId, principal: &str) -> Result<bool> {
        {
            let live = self.live.lock().unwrap();
            for entry in live.values() {
                if let Some(run) = entry.live.as_ref()
                    && &run.run_id == run_id
                {
                    anyhow::ensure!(
                        entry.owner_principal.as_deref() == Some(principal),
                        "permission denied for run"
                    );
                    if let Some(session) = &entry.broker {
                        session.cancel_flow();
                    }
                    run.cancel.cancel();
                    return Ok(true);
                }
            }
        }
        crate::flow_service::cancel_run(self, run_id, principal).await
    }

    pub fn rename_session(&self, sid: &SessionId, title: &str) -> Result<SessionSummary> {
        let path = self.sessions_root().join(sid.0.to_string());
        anyhow::ensure!(path.is_dir(), "legacy session not found: {sid}");
        atman_runtime::session_meta::SessionMeta::rename(&path, title)
            .with_context(|| format!("rename session {sid}"))?;
        self.list_legacy_sessions()?
            .into_iter()
            .find(|summary| &summary.id == sid)
            .ok_or_else(|| anyhow::anyhow!("session not found: {sid}"))
    }

    pub async fn list_sessions_query(
        self: &std::sync::Arc<Self>,
        project_root: Option<&str>,
        search: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<SessionSummary>> {
        let mut summaries = self.list_sessions().await?;
        if let Some(project_root) = project_root {
            let project = atman_runtime::session_meta::canonical_root(Path::new(project_root));
            let legacy_query = atman_runtime::session_meta::SessionDiscoveryQuery::current_project(
                Path::new(project_root),
            );
            summaries.retain(|summary| {
                if summary.persistent.is_none() {
                    let path = self.sessions_root().join(summary.id.to_string());
                    return legacy_query.matches_meta(
                        atman_runtime::session_meta::SessionMeta::load(&path).as_ref(),
                    );
                }
                summary.project_root.as_ref().is_some_and(|root| {
                    atman_runtime::session_meta::canonical_root(Path::new(root)) == project
                })
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

    pub async fn list_sessions(self: &std::sync::Arc<Self>) -> Result<Vec<SessionSummary>> {
        let state = self.clone();
        let mut rows = tokio::task::spawn_blocking(move || state.list_legacy_sessions()).await??;
        rows.extend(crate::flow_service::catalog::list(self).await?);
        rows.sort_by_key(|row| (std::cmp::Reverse(row.id.0), row.persistent.is_none()));
        Ok(rows)
    }

    fn list_legacy_sessions(&self) -> Result<Vec<SessionSummary>> {
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
                    event_count: Some(stats.event_count as usize),
                    persistent: None,
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
                    event_count: Some(0),
                    persistent: None,
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
