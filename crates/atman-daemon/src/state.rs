use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use atman_proto::{
    CloseSessionResponse, DaemonGeneration, DeleteSessionResponse, EventCursor, FlowRunId,
    GetSessionUpdatesResponse, ListProjectsResponse, ProjectSummary, PromptId, ResourceState,
    ResyncRequired, SNAPSHOT_SCHEMA_VERSION, SessionCloseStatus, SessionDeleteStatus, SessionId,
    SessionSnapshot, SessionStatus, SessionSummary,
};
use tokio_util::sync::CancellationToken;

use crate::idempotency::IdempotencyRegistry;
use crate::project_registry::{ProjectRecord, ProjectRegistry};
use crate::projection::RestoredProjection;
use crate::session_actor::{SessionActorHandle, SessionActorLease};

pub struct DaemonState {
    data_dir: PathBuf,
    daemon_generation: String,
    projects: ProjectRegistry,
    sessions: Mutex<HashMap<SessionId, SessionActorHandle>>,
    session_loads: Mutex<HashMap<SessionId, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    launcher: Mutex<Option<std::sync::Arc<crate::run::RunLauncher>>>,
    provider_lifecycles: Mutex<HashMap<PathBuf, atman_runtime::ProviderLifecycle>>,
    task_registry: atman_runtime::TaskRegistry,
    terminal_registry: std::sync::Arc<atman_runtime::tools::term::TermRegistry>,
    accepting_commands: AtomicBool,
    pub(crate) idempotency: IdempotencyRegistry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonShutdownReport {
    pub graceful: usize,
    pub forced: usize,
    pub remaining: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionUnloadOutcome {
    Unloaded,
    NotLoaded,
    Busy,
}

pub(crate) struct LoadedSession {
    pub session: std::sync::Arc<atman_runtime::Session>,
    pub projection: RestoredProjection,
}

#[derive(Clone)]
pub struct LiveRun {
    pub run_id: FlowRunId,
    pub turn_id: atman_runtime::event::TurnId,
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
        let task_registry = atman_runtime::TaskRegistry::new();
        let terminal_registry = std::sync::Arc::new(
            atman_runtime::tools::term::TermRegistry::new()
                .with_task_registry(task_registry.clone()),
        );
        Self {
            data_dir,
            daemon_generation,
            projects: ProjectRegistry::default(),
            sessions: Mutex::new(HashMap::new()),
            session_loads: Mutex::new(HashMap::new()),
            launcher: Mutex::new(None),
            provider_lifecycles: Mutex::new(HashMap::new()),
            task_registry,
            terminal_registry,
            accepting_commands: AtomicBool::new(true),
            idempotency: IdempotencyRegistry::default(),
        }
    }

    pub fn daemon_generation(&self) -> &str {
        &self.daemon_generation
    }

    pub fn begin_shutdown(&self) {
        self.accepting_commands.store(false, Ordering::Release);
    }

    pub fn is_accepting_commands(&self) -> bool {
        self.accepting_commands.load(Ordering::Acquire)
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

    pub(crate) fn resolve_project(&self, root: &Path) -> Result<ProjectRecord> {
        self.projects.resolve(root)
    }

    pub(crate) fn observe_project(
        &self,
        root: &Path,
        persisted_fingerprint: Option<&str>,
    ) -> Result<ProjectRecord> {
        self.projects.observe_persisted(root, persisted_fingerprint)
    }

    pub fn task_registry(&self) -> atman_runtime::TaskRegistry {
        self.task_registry.clone()
    }

    pub(crate) fn terminal_registry(
        &self,
    ) -> std::sync::Arc<atman_runtime::tools::term::TermRegistry> {
        self.terminal_registry.clone()
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
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?
            .lease()?;
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
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?
            .lease()?;
        actor.submit_form(id, submission).await
    }

    pub(crate) async fn resolve_compact_review(
        &self,
        session_id: &SessionId,
        id: String,
        decision: atman_proto::CompactReviewDecision,
        principal: &str,
    ) -> Result<crate::session_actor::CompactReviewResolutionCommit> {
        let actor = self
            .authorized_actor(session_id, principal)
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?
            .lease()?;
        actor.resolve_compact_review(id, decision).await
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
        self.register_session_with_runs(id, session, vec![run], owner_principal.into(), None)
            .await
    }

    pub async fn register_session(
        &self,
        id: SessionId,
        session: std::sync::Arc<atman_runtime::Session>,
        owner_principal: impl Into<String>,
    ) -> Result<()> {
        self.register_session_with_runs(id, session, Vec::new(), owner_principal.into(), None)
            .await
    }

    pub(crate) async fn admit_session_run(
        &self,
        id: SessionId,
        session: std::sync::Arc<atman_runtime::Session>,
        run: LiveRun,
        user_message: atman_runtime::message::Message,
        owner_principal: impl Into<String>,
    ) -> Result<std::sync::Arc<atman_runtime::context_state::ContextState>> {
        let owner_principal = owner_principal.into();
        self.register_session(id.clone(), session.clone(), owner_principal.clone())
            .await?;
        let actor = self
            .authorized_actor(&id, &owner_principal)
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?;
        anyhow::ensure!(
            actor.owns_session(&session),
            "session {id} is already registered with another runtime"
        );
        actor.lease()?.admit_run(run, user_message).await
    }

    async fn register_session_with_runs(
        &self,
        id: SessionId,
        session: std::sync::Arc<atman_runtime::Session>,
        initial_runs: Vec<LiveRun>,
        owner_principal: String,
        restored_projection: Option<RestoredProjection>,
    ) -> Result<()> {
        let existing = {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(entry) = sessions.get(&id) {
                Some(entry.clone())
            } else {
                self.task_registry
                    .bind_session(id.to_string(), session.sink().clone());
                sessions.insert(
                    id.clone(),
                    SessionActorHandle::spawn(
                        id.clone(),
                        session.clone(),
                        initial_runs.clone(),
                        owner_principal.clone(),
                        DaemonGeneration(self.daemon_generation.clone()),
                        restored_projection,
                        self.task_registry.clone(),
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
            let entry = entry.lease()?;
            for run in initial_runs {
                entry.add_run(run).await?;
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

    pub(crate) fn session_runtime_slot(
        &self,
        id: &SessionId,
        principal: &str,
    ) -> Result<std::sync::Arc<crate::run::SessionRuntimeSlot>> {
        Ok(self
            .authorized_actor(id, principal)
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?
            .runtime_slot())
    }

    fn loaded_runtime_session(
        &self,
        id: &SessionId,
        principal: &str,
    ) -> Result<Option<SessionActorLease>> {
        let actor = self.sessions.lock().unwrap().get(id).cloned();
        let Some(actor) = actor else {
            return Ok(None);
        };
        anyhow::ensure!(
            actor.owns(principal),
            "session {id} is owned by another principal"
        );
        Ok(Some(actor.lease()?))
    }

    pub(crate) async fn get_or_load_session<F, Fut>(
        &self,
        id: &SessionId,
        principal: &str,
        load: F,
    ) -> Result<SessionActorLease>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<LoadedSession>>,
    {
        let load_gate = self
            .session_loads
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_default()
            .clone();
        let _guard = load_gate.lock().await;
        if let Some(actor) = self.loaded_runtime_session(id, principal)? {
            return Ok(actor);
        }
        let restored = load().await?;
        let session = restored.session;
        self.register_session_with_runs(
            id.clone(),
            session.clone(),
            Vec::new(),
            principal.to_owned(),
            Some(restored.projection),
        )
        .await?;
        self.authorized_actor(id, principal)
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?
            .lease()
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
            actor.lease()?.snapshot().await?
        } else {
            let session_dir = self.sessions_root().join(id.to_string());
            let fallback_trust = match self.launcher() {
                Some(launcher) => launcher.trust_config()?,
                None => atman_runtime::trust::TrustConfig::default(),
            };
            let historical = crate::projection::load_historical_projection(
                id.clone(),
                &session_dir,
                fallback_trust,
            )
            .await?;
            let config_dir = self
                .launcher()
                .and_then(|launcher| launcher.config_dir.clone());
            let redactor = crate::bootstrap::build_redactor(config_dir.as_deref());
            let projection = crate::projection::redacted_projection(
                &historical.projection,
                redactor.as_deref(),
            )?;
            (historical.cursor, projection)
        };
        Ok(SessionSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            daemon_generation: DaemonGeneration(self.daemon_generation.clone()),
            cursor,
            projection,
        })
    }

    pub(crate) async fn attach_session_snapshot(
        self: &std::sync::Arc<Self>,
        id: &SessionId,
        principal: &str,
    ) -> Result<SessionSnapshot> {
        if self.launcher().is_none() {
            return self.session_snapshot(id, principal).await;
        }
        let (cursor, projection) = self
            .get_or_load_actor(id, principal)
            .await?
            .snapshot()
            .await?;
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
            return actor.lease()?.updates(after_cursor, limit).await;
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
        self: &std::sync::Arc<Self>,
        id: &SessionId,
        principal: &str,
    ) -> Result<
        Option<(
            tokio::sync::broadcast::Receiver<atman_proto::ProjectionEventEnvelope>,
            Option<std::sync::Arc<atman_runtime::redact::Redactor>>,
        )>,
    > {
        if self.launcher().is_none() && self.sessions.lock().unwrap().get(id).is_none() {
            anyhow::ensure!(
                self.sessions_root()
                    .join(id.to_string())
                    .join("events.jsonl")
                    .is_file(),
                "session not found: {id}"
            );
            return Ok(None);
        }
        Ok(Some(
            self.get_or_load_actor(id, principal)
                .await?
                .subscribe_updates()
                .await?,
        ))
    }

    pub fn finish_run(&self, session_id: &SessionId, run_id: &FlowRunId) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(session_id)
            .filter(|entry| entry.view().runs.contains_key(run_id))
            .is_some_and(|entry| entry.finish_run(run_id.clone()))
    }

    pub async fn unload_session_if_idle(&self, id: &SessionId) -> Result<bool> {
        let load_gate = self
            .session_loads
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_default()
            .clone();
        let _guard = load_gate.lock().await;
        Ok(matches!(
            self.unload_session_while_locked(id, None).await?,
            SessionUnloadOutcome::Unloaded
        ))
    }

    pub async fn close_session(
        &self,
        id: &SessionId,
        principal: &str,
    ) -> Result<CloseSessionResponse> {
        let load_gate = self
            .session_loads
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_default()
            .clone();
        let _guard = load_gate.lock().await;
        let status = match self
            .unload_session_while_locked(id, Some(principal))
            .await?
        {
            SessionUnloadOutcome::Unloaded => SessionCloseStatus::Closed,
            SessionUnloadOutcome::Busy => SessionCloseStatus::Busy,
            SessionUnloadOutcome::NotLoaded => {
                anyhow::ensure!(
                    self.sessions_root()
                        .join(id.to_string())
                        .join("events.jsonl")
                        .is_file(),
                    "session not found: {id}"
                );
                SessionCloseStatus::AlreadyClosed
            }
        };
        Ok(CloseSessionResponse {
            session_id: id.clone(),
            status,
        })
    }

    pub async fn delete_session(
        &self,
        id: &SessionId,
        principal: &str,
    ) -> Result<DeleteSessionResponse> {
        let load_gate = self
            .session_loads
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_default()
            .clone();
        let _guard = load_gate.lock().await;
        if self
            .unload_session_while_locked(id, Some(principal))
            .await?
            == SessionUnloadOutcome::Busy
        {
            return Ok(DeleteSessionResponse {
                session_id: id.clone(),
                status: SessionDeleteStatus::Busy,
                blocking_resources: Vec::new(),
            });
        }

        let sessions_root = self.sessions_root();
        let session_dir = sessions_root.join(id.to_string());
        let tombstone = sessions_root.join(format!(".deleting-{id}"));
        if !session_dir.exists() {
            if tombstone.exists() {
                remove_session_tree(tombstone).await?;
                return Ok(DeleteSessionResponse {
                    session_id: id.clone(),
                    status: SessionDeleteStatus::Deleted,
                    blocking_resources: Vec::new(),
                });
            }
            return Ok(DeleteSessionResponse {
                session_id: id.clone(),
                status: SessionDeleteStatus::NotFound,
                blocking_resources: Vec::new(),
            });
        }
        validate_session_tree(&session_dir)?;

        let snapshot = self.session_snapshot(id, principal).await?;
        let blocking_resources = snapshot
            .projection
            .resources
            .into_iter()
            .filter(|resource| resource_blocks_session_detach(resource.state))
            .map(|resource| resource.id)
            .collect::<Vec<_>>();
        if !blocking_resources.is_empty() {
            return Ok(DeleteSessionResponse {
                session_id: id.clone(),
                status: SessionDeleteStatus::UnsafeResources,
                blocking_resources,
            });
        }

        if tombstone.exists() {
            remove_session_tree(tombstone.clone()).await?;
        }
        std::fs::rename(&session_dir, &tombstone).with_context(|| {
            format!(
                "move session {} to deletion tombstone {}",
                session_dir.display(),
                tombstone.display()
            )
        })?;
        remove_session_tree(tombstone).await?;
        Ok(DeleteSessionResponse {
            session_id: id.clone(),
            status: SessionDeleteStatus::Deleted,
            blocking_resources: Vec::new(),
        })
    }

    async fn unload_session_while_locked(
        &self,
        id: &SessionId,
        principal: Option<&str>,
    ) -> Result<SessionUnloadOutcome> {
        let actor = self.sessions.lock().unwrap().get(id).cloned();
        let Some(actor) = actor else {
            return Ok(SessionUnloadOutcome::NotLoaded);
        };
        if let Some(principal) = principal {
            anyhow::ensure!(actor.owns(principal), "permission denied for session");
        }
        if !actor.try_unload().await? {
            return Ok(SessionUnloadOutcome::Busy);
        }
        let mut sessions = self.sessions.lock().unwrap();
        if sessions
            .get(id)
            .is_some_and(|registered| registered.is_same_actor(&actor))
        {
            sessions.remove(id);
            return Ok(SessionUnloadOutcome::Unloaded);
        }
        Ok(SessionUnloadOutcome::Busy)
    }

    pub async fn evict_idle_sessions(&self, idle_for: std::time::Duration) -> usize {
        let candidates = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, actor)| actor.view().has_been_idle_for(idle_for))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut evicted = 0;
        for session_id in candidates {
            match self.unload_session_if_idle(&session_id).await {
                Ok(true) => evicted += 1,
                Ok(false) => {}
                Err(error) => atman_runtime::notify!(
                    warn,
                    location = Log,
                    "idle session {session_id} could not be unloaded: {error:#}"
                ),
            }
        }
        evicted
    }

    pub async fn run_idle_eviction(
        self: std::sync::Arc<Self>,
        idle_for: std::time::Duration,
        interval: std::time::Duration,
        shutdown: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(interval) => {
                    self.evict_idle_sessions(idle_for).await;
                }
            }
        }
    }

    pub async fn shutdown(
        self: &std::sync::Arc<Self>,
        timeout: std::time::Duration,
    ) -> DaemonShutdownReport {
        self.begin_shutdown();
        let started_at = tokio::time::Instant::now();
        let force_budget = std::cmp::min(timeout / 4, std::time::Duration::from_secs(1));
        let graceful_deadline = started_at + timeout.saturating_sub(force_budget);

        let actors = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut beginnings = tokio::task::JoinSet::new();
        for actor in actors {
            beginnings.spawn(async move { actor.begin_shutdown().await });
        }
        let begin_budget = graceful_deadline.saturating_duration_since(tokio::time::Instant::now());
        let _ = tokio::time::timeout(begin_budget, async {
            while beginnings.join_next().await.is_some() {}
        })
        .await;

        let mut graceful = 0;
        loop {
            let session_ids = self
                .sessions
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            if session_ids.is_empty() || tokio::time::Instant::now() >= graceful_deadline {
                break;
            }
            let mut unloads = tokio::task::JoinSet::new();
            for session_id in session_ids {
                let state = self.clone();
                unloads.spawn(async move { state.unload_session_if_idle(&session_id).await });
            }
            let remaining =
                graceful_deadline.saturating_duration_since(tokio::time::Instant::now());
            let _ = tokio::time::timeout(remaining, async {
                while let Some(result) = unloads.join_next().await {
                    if matches!(result, Ok(Ok(true))) {
                        graceful += 1;
                    }
                }
            })
            .await;
            if !self.sessions.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        let remaining_actors = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, actor)| (id.clone(), actor.clone()))
            .collect::<Vec<_>>();
        let mut forced_shutdowns = tokio::task::JoinSet::new();
        for (session_id, actor) in remaining_actors {
            forced_shutdowns.spawn(async move {
                let result = actor.force_shutdown().await;
                (session_id, actor, result)
            });
        }
        let mut forced = 0;
        let _ = tokio::time::timeout(force_budget, async {
            while let Some(result) = forced_shutdowns.join_next().await {
                if let Ok((session_id, actor, Ok(()))) = result {
                    let mut sessions = self.sessions.lock().unwrap();
                    if sessions
                        .get(&session_id)
                        .is_some_and(|registered| registered.is_same_actor(&actor))
                    {
                        sessions.remove(&session_id);
                        forced += 1;
                    }
                }
            }
        })
        .await;

        DaemonShutdownReport {
            graceful,
            forced,
            remaining: self.sessions.lock().unwrap().len(),
        }
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
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?
            .lease()?;
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
            .ok_or_else(|| anyhow::anyhow!("permission denied for session"))?
            .lease()?;
        actor.interject(run_id, text, level, redirect_target).await
    }

    pub async fn rename_session(
        self: &std::sync::Arc<Self>,
        sid: &SessionId,
        title: Option<String>,
        principal: &str,
    ) -> Result<crate::RenameSessionCommit> {
        let actor = self.get_or_load_actor(sid, principal).await?;
        actor.rename(title).await
    }

    pub(crate) async fn auto_name_session(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        principal: &str,
    ) -> Result<crate::session_actor::AutoNameSessionCommit> {
        let launcher = self
            .launcher()
            .ok_or_else(|| anyhow::anyhow!("daemon started without a session launcher"))?;
        let actor = self.get_or_load_actor(session_id, principal).await?;
        let generation = actor
            .begin_auto_name(true)
            .await?
            .expect("forced session naming always starts");
        let executor = launcher.naming_executor(self).await?;
        let title = atman_runtime::session_naming::generate_session_title(
            &executor,
            &actor.runtime_session(),
        )
        .await?;
        actor.finish_auto_name(generation, title).await
    }

    pub(crate) async fn suggest_flow(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        principal: &str,
    ) -> Result<(
        String,
        atman_runtime::suggestion::RecentContext,
        atman_runtime::suggestion::Suggestion,
    )> {
        let launcher = self
            .launcher()
            .ok_or_else(|| anyhow::anyhow!("daemon started without a session launcher"))?;
        let actor = self.get_or_load_actor(session_id, principal).await?;
        let context = atman_runtime::suggestion::recent_context(
            &actor.runtime_session().messages(),
            atman_runtime::suggestion::DEFAULT_RECENT_TURNS,
        );
        let hub = crate::bootstrap::resolve_config_hub(launcher.config_dir.as_deref())?;
        let configured = hub.suggest_model()?.unwrap_or_else(|| "gpt-4o-mini".into());
        let model = atman_runtime::model_registry::resolve_alias(&configured);
        let executor = launcher.naming_executor(self).await?;
        let provider = executor
            .providers
            .resolve(&model)
            .ok_or_else(|| anyhow::anyhow!("no provider resolves suggestion model `{model}`"))?;
        let suggestion = atman_runtime::suggestion::generate(provider, &model, &context).await?;
        Ok((model, context, suggestion))
    }

    pub(crate) async fn install_suggested_flow(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        flow_name: &str,
        source: &str,
        principal: &str,
    ) -> Result<String> {
        use std::io::Write;

        let launcher = self
            .launcher()
            .ok_or_else(|| anyhow::anyhow!("daemon started without a session launcher"))?;
        let actor = self.get_or_load_actor(session_id, principal).await?;
        let parsed_name = atman_runtime::suggestion::extract_flow_name(source)?;
        anyhow::ensure!(
            parsed_name == flow_name,
            "suggested flow name `{flow_name}` does not match source `{parsed_name}`"
        );
        let context = atman_runtime::suggestion::recent_context(
            &actor.runtime_session().messages(),
            atman_runtime::suggestion::DEFAULT_RECENT_TURNS,
        );
        let parsed = atman_dsl::parse::parse_file(source)
            .map_err(|error| anyhow::anyhow!("parse suggested flow: {error}"))?;
        if let Err(errors) =
            atman_runtime::validate::validate_with_tool_lookup(&parsed.flows[0], &|name| {
                context.tool_names.contains(name)
            })
        {
            anyhow::bail!(
                "suggested flow validation failed: {}",
                errors
                    .into_iter()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }

        let hub = crate::bootstrap::resolve_config_hub(launcher.config_dir.as_deref())?;
        let commands_dir = hub.config_dir().join("commands");
        std::fs::create_dir_all(&commands_dir)?;
        let mut suffix = 1usize;
        loop {
            let final_name = if suffix == 1 {
                flow_name.to_owned()
            } else {
                format!("{flow_name}_v{suffix}")
            };
            let target = commands_dir.join(format!("{final_name}.at"));
            let mut file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    suffix += 1;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let installed_source = if final_name == flow_name {
                source.to_owned()
            } else {
                let mut renamed = parsed.clone();
                renamed.flows[0].name.name = final_name.clone();
                atman_dsl::print::print_file(&renamed)
            };
            let write_result = file
                .write_all(format!("{}\n", installed_source.trim_end()).as_bytes())
                .and_then(|_| file.sync_all());
            drop(file);
            if let Err(error) = write_result {
                let _ = std::fs::remove_file(&target);
                return Err(error.into());
            }
            let trigger = format!("{final_name} ");
            if let Err(error) = hub.append_dsl_route(&final_name, &trigger) {
                let _ = std::fs::remove_file(&target);
                return Err(anyhow::anyhow!(error));
            }
            return Ok(final_name);
        }
    }

    pub(crate) async fn maybe_auto_name_session(
        &self,
        session: &std::sync::Arc<atman_runtime::Session>,
        executor: &atman_runtime::Executor,
    ) -> Result<Option<crate::session_actor::AutoNameSessionCommit>> {
        let session_id = SessionId(session.id().0);
        let actor = self
            .sessions
            .lock()
            .unwrap()
            .get(&session_id)
            .filter(|actor| actor.owns_session(session))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("session actor is unavailable"))?
            .lease()?;
        let Some(generation) = actor.begin_auto_name(false).await? else {
            return Ok(None);
        };
        let title =
            atman_runtime::session_naming::generate_session_title(executor, session).await?;
        actor.finish_auto_name(generation, title).await.map(Some)
    }

    pub(crate) async fn move_session(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        project_root: &std::path::Path,
        principal: &str,
    ) -> Result<crate::session_actor::MoveSessionCommit> {
        let launcher = self
            .launcher()
            .ok_or_else(|| anyhow::anyhow!("daemon started without a session launcher"))?;
        let (project_root, project_index) = launcher.session_rebase_context(self, project_root)?;
        self.get_or_load_actor(session_id, principal)
            .await?
            .move_session(project_root, project_index)
            .await
    }

    pub(crate) async fn set_session_goal(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        goal: Option<String>,
        principal: &str,
    ) -> Result<crate::session_actor::GoalMutationCommit> {
        self.get_or_load_actor(session_id, principal)
            .await?
            .set_goal(goal)
            .await
    }

    pub(crate) async fn update_session_todos(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        mutation: atman_proto::TodoMutation,
        principal: &str,
    ) -> Result<crate::session_actor::TodoMutationCommit> {
        self.get_or_load_actor(session_id, principal)
            .await?
            .update_todos(mutation)
            .await
    }

    pub(crate) async fn update_session_trust(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        trust: atman_proto::TrustProjection,
        principal: &str,
    ) -> Result<crate::session_actor::TrustUpdateCommit> {
        let launcher = self
            .launcher()
            .ok_or_else(|| anyhow::anyhow!("daemon started without a session launcher"))?;
        self.get_or_load_actor(session_id, principal)
            .await?
            .update_trust(trust, launcher)
            .await
    }

    pub(crate) async fn reload_session_mcp(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        principal: &str,
    ) -> Result<crate::session_actor::McpReloadCommit> {
        let launcher = self
            .launcher()
            .ok_or_else(|| anyhow::anyhow!("daemon started without a session launcher"))?;
        let configs = launcher.mcp_configs()?;
        self.get_or_load_actor(session_id, principal)
            .await?
            .reload_mcp(configs)
            .await
    }

    pub(crate) async fn sanitize_session_attachments(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        dry_run: bool,
        principal: &str,
    ) -> Result<crate::session_actor::AttachmentSanitizeCommit> {
        self.get_or_load_actor(session_id, principal)
            .await?
            .sanitize_attachments(dry_run)
            .await
    }

    pub(crate) async fn import_session_messages(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        messages: Vec<atman_proto::ImportedMessage>,
        principal: &str,
    ) -> Result<crate::session_actor::MessageImportCommit> {
        self.get_or_load_actor(session_id, principal)
            .await?
            .import_messages(messages)
            .await
    }

    pub(crate) async fn request_session_compaction(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        principal: &str,
    ) -> Result<crate::session_actor::CompactionRequestCommit> {
        let launcher = self
            .launcher()
            .ok_or_else(|| anyhow::anyhow!("daemon started without a session launcher"))?;
        let actor = self.get_or_load_actor(session_id, principal).await?;
        let providers = launcher.compaction_providers(self).await?;
        actor.request_compaction(providers).await
    }

    async fn get_or_load_actor(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        principal: &str,
    ) -> Result<SessionActorLease> {
        if let Some(actor) = self.authorized_actor(session_id, principal) {
            return actor.lease();
        }
        let launcher = self
            .launcher()
            .ok_or_else(|| anyhow::anyhow!("run launcher is not configured"))?;
        let state = self.clone();
        let load_id = session_id.clone();
        self.get_or_load_session(session_id, principal, move || async move {
            tokio::task::spawn_blocking(move || launcher.open_existing_session(&state, &load_id))
                .await
                .context("join session replay task")?
        })
        .await
    }

    pub(crate) async fn terminate_resource(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        resource_id: atman_proto::ResourceId,
        principal: &str,
    ) -> Result<crate::session_actor::ResourceTerminationCommit> {
        self.get_or_load_actor(session_id, principal)
            .await?
            .terminate_resource(resource_id)
            .await
    }

    pub(crate) async fn resize_terminal(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        resource_id: atman_proto::ResourceId,
        rows: u16,
        cols: u16,
        principal: &str,
    ) -> Result<crate::session_actor::TerminalResizeCommit> {
        self.get_or_load_actor(session_id, principal)
            .await?
            .resize_terminal(resource_id, rows, cols, self.terminal_registry())
            .await
    }

    pub(crate) async fn retain_resource(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        resource_id: atman_proto::ResourceId,
        principal: &str,
    ) -> Result<crate::session_actor::ResourceMutationCommit> {
        self.get_or_load_actor(session_id, principal)
            .await?
            .retain_resource(resource_id)
            .await
    }

    pub(crate) async fn release_resource(
        self: &std::sync::Arc<Self>,
        session_id: &SessionId,
        resource_id: atman_proto::ResourceId,
        principal: &str,
    ) -> Result<crate::session_actor::ResourceMutationCommit> {
        self.get_or_load_actor(session_id, principal)
            .await?
            .release_resource(resource_id)
            .await
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

    pub fn list_projects_query(
        &self,
        search: Option<&str>,
        limit: Option<usize>,
    ) -> Result<ListProjectsResponse> {
        let summaries = self.list_sessions()?;
        let sessions_root = self.sessions_root();
        let mut projects = HashMap::<atman_proto::ProjectId, ProjectSummary>::new();
        for summary in summaries {
            let session_dir = sessions_root.join(summary.id.to_string());
            let Some(meta) = atman_runtime::session_meta::SessionMeta::load(&session_dir) else {
                continue;
            };
            let Some(root) = meta.project_root.as_deref().or(meta.start_path.as_deref()) else {
                continue;
            };
            let project = self.observe_project(root, meta.project_fingerprint.as_deref())?;
            let candidate_time = meta.created_at.or(summary.first_ts);
            let entry = projects
                .entry(project.id.clone())
                .or_insert_with(|| ProjectSummary {
                    id: project.id.clone(),
                    name: project.name(),
                    root: project.root.display().to_string(),
                    session_count: 0,
                    active_session_count: 0,
                    last_session_at: None,
                });
            entry.session_count += 1;
            entry.active_session_count += usize::from(summary.status == SessionStatus::Running);
            entry.last_session_at = entry.last_session_at.max(candidate_time);
        }
        for project in self.projects.list() {
            projects
                .entry(project.id.clone())
                .or_insert_with(|| ProjectSummary {
                    id: project.id.clone(),
                    name: project.name(),
                    root: project.root.display().to_string(),
                    session_count: 0,
                    active_session_count: 0,
                    last_session_at: None,
                });
        }

        let mut projects = projects.into_values().collect::<Vec<_>>();
        if let Some(search) = search.map(str::trim).filter(|value| !value.is_empty()) {
            let needle = search.to_lowercase();
            projects.retain(|project| {
                project.id.0.to_lowercase().contains(&needle)
                    || project.name.to_lowercase().contains(&needle)
                    || project.root.to_lowercase().contains(&needle)
            });
        }
        projects.sort_by(|left, right| {
            right
                .last_session_at
                .cmp(&left.last_session_at)
                .then_with(|| left.root.cmp(&right.root))
        });
        let total = projects.len();
        if let Some(limit) = limit {
            projects.truncate(limit);
        }
        Ok(ListProjectsResponse { projects, total })
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
                let updated_at = std::fs::metadata(entry.path().join("events.jsonl"))
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .map(chrono::DateTime::<chrono::Utc>::from)
                    .or(stats.first_ts);
                let status = if live_ids.contains_key(&sid) {
                    SessionStatus::Running
                } else {
                    SessionStatus::Finished
                };
                let meta = atman_runtime::session_meta::SessionMeta::load(&entry.path());
                out.push(SessionSummary {
                    id: sid.clone(),
                    event_count: stats.event_count as usize,
                    message_count: stats.message_count as usize,
                    first_ts: stats.first_ts,
                    updated_at,
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
                    message_count: 0,
                    first_ts: Some(*started_at),
                    updated_at: Some(*started_at),
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

fn validate_session_tree(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect session directory {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "session path is not a directory: {}",
        path.display()
    );
    Ok(())
}

async fn remove_session_tree(path: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        validate_session_tree(&path)?;
        std::fs::remove_dir_all(&path)
            .with_context(|| format!("remove session directory {}", path.display()))
    })
    .await
    .context("join session deletion task")?
}

pub(crate) fn resource_blocks_session_detach(state: ResourceState) -> bool {
    match state {
        ResourceState::Starting
        | ResourceState::Running
        | ResourceState::Dirty
        | ResourceState::Terminating
        | ResourceState::Retained
        | ResourceState::Orphaned => true,
        ResourceState::Exited
        | ResourceState::Failed
        | ResourceState::Released
        | ResourceState::Lost => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use atman_runtime::flow_authority::EffectiveAuthority;
    use atman_runtime::permission::PermissionBroker;
    use atman_runtime::tool::{Tool, ToolArgs, ToolCtx};
    use atman_runtime::tools::agent_ctrl::FlowRegistry;
    use atman_runtime::tools::term::TermSpawn;
    use atman_runtime::trust::{TrustConfig, TrustMode};
    use atman_runtime::{Tier, Value};

    use super::*;
    use crate::projection::SessionProjector;

    fn live_run(flow_name: &str) -> LiveRun {
        LiveRun {
            run_id: FlowRunId(uuid::Uuid::now_v7()),
            turn_id: atman_runtime::event::TurnId::now(),
            flow_name: flow_name.into(),
            cancel: CancellationToken::new(),
            started_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn deletion_blocks_every_non_terminal_resource_state() {
        for state in [
            ResourceState::Starting,
            ResourceState::Running,
            ResourceState::Dirty,
            ResourceState::Terminating,
            ResourceState::Retained,
            ResourceState::Orphaned,
        ] {
            assert!(resource_blocks_session_detach(state));
        }
        for state in [
            ResourceState::Exited,
            ResourceState::Failed,
            ResourceState::Released,
            ResourceState::Lost,
        ] {
            assert!(!resource_blocks_session_detach(state));
        }
    }

    #[tokio::test]
    async fn mcp_reload_projects_pending_status_before_runtime_start() {
        let state = DaemonState::new(tempfile::tempdir().unwrap().path().to_path_buf());
        let session = Arc::new(atman_runtime::Session::open_ephemeral());
        let session_id = SessionId(session.id().0);
        let run = live_run("agent");
        state
            .register_session_run(session_id.clone(), session.clone(), run, "owner")
            .await
            .unwrap();
        let actor = state.authorized_actor(&session_id, "owner").unwrap();
        let config = atman_runtime::mcp::McpServerConfig::stdio(
            "local-tools",
            "tool-server",
            Vec::new(),
            Tier::One,
            1_000,
        );

        let commit = actor
            .lease()
            .unwrap()
            .reload_mcp(vec![config.clone()])
            .await
            .unwrap();

        assert_eq!(commit.active_runs, 1);
        let context = session.subscribe_context().borrow().clone();
        assert!(matches!(
            context.mcp_servers.as_slice(),
            [atman_runtime::mcp::McpServerStatus {
                name,
                state: atman_runtime::mcp::McpServerState::Pending,
                ..
            }] if name == "local-tools"
        ));
    }

    #[tokio::test]
    async fn concurrent_root_admission_isolates_context_and_cancellation() {
        let state = DaemonState::new(tempfile::tempdir().unwrap().path().to_path_buf());
        let session = Arc::new(atman_runtime::Session::open_ephemeral());
        let session_id = SessionId(session.id().0);
        let first = live_run("first");
        let first_id = first.run_id.clone();
        let first_turn = first.turn_id.clone();
        let first_cancel = first.cancel.clone();
        let first_context = state
            .admit_session_run(
                session_id.clone(),
                session.clone(),
                first,
                atman_runtime::message::Message::user_text(first_turn, "first message"),
                "owner",
            )
            .await
            .unwrap();
        let second = live_run("second");
        let second_id = second.run_id.clone();
        let second_turn = second.turn_id.clone();
        let second_cancel = second.cancel.clone();
        let second_context = state
            .admit_session_run(
                session_id.clone(),
                session.clone(),
                second,
                atman_runtime::message::Message::user_text(second_turn, "second message"),
                "owner",
            )
            .await
            .unwrap();

        assert!(!Arc::ptr_eq(&first_context, &second_context));
        assert!(Arc::ptr_eq(&second_context, &session.context()));
        assert_eq!(first_context.messages()[0].text_concat(), "first message");
        assert_eq!(first_context.messages().len(), 1);
        assert_eq!(second_context.messages()[0].text_concat(), "first message");
        assert_eq!(second_context.messages()[1].text_concat(), "second message");

        let cancellation = state
            .cancel_run(&session_id, &first_id, "owner")
            .await
            .unwrap();
        assert_eq!(
            cancellation.status,
            atman_proto::RunCancellationStatus::Accepted
        );
        assert!(first_cancel.is_cancelled());
        assert!(!second_cancel.is_cancelled());

        let snapshot = state.session_snapshot(&session_id, "owner").await.unwrap();
        assert_eq!(snapshot.projection.runs.len(), 2);
        assert_eq!(
            snapshot
                .projection
                .runs
                .iter()
                .find(|run| run.id == first_id)
                .unwrap()
                .state,
            atman_proto::RunLifecycle::Cancelling
        );
        assert_eq!(
            snapshot
                .projection
                .runs
                .iter()
                .find(|run| run.id == second_id)
                .unwrap()
                .state,
            atman_proto::RunLifecycle::Starting
        );
        assert_eq!(
            session
                .sink()
                .snapshot_envelopes()
                .iter()
                .filter(|event| matches!(
                    event.event,
                    atman_runtime::event::Event::ContextHeadSelected { .. }
                ))
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_resize_uses_the_daemon_registry_and_projects_dimensions() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(DaemonState::new(root.path().join("data")));
        let session = Arc::new(atman_runtime::Session::open_ephemeral());
        let session_id = SessionId(session.id().0);
        state
            .register_session(session_id.clone(), session.clone(), "owner")
            .await
            .unwrap();

        let trust = TrustConfig {
            mode: TrustMode::Reckless,
            ..TrustConfig::default()
        };
        let flows = Arc::new(FlowRegistry::new());
        let broker = PermissionBroker::shared(flows.clone());
        let run_id = atman_runtime::event::FlowRunId::now();
        let identity = flows
            .register_root(
                "terminal-resize-test".into(),
                run_id.clone(),
                EffectiveAuthority::root(&trust, true, None),
            )
            .unwrap();
        let terminal_registry = state.terminal_registry();
        let mut ctx = ToolCtx::new()
            .with_term_registry(terminal_registry.clone())
            .with_task_registry(state.task_registry())
            .with_session_dir(root.path().join("session"))
            .with_session_id(session_id.to_string())
            .with_events(session.sink().clone())
            .with_trust(trust)
            .with_flow_registry(flows)
            .with_permission_broker(broker)
            .with_anchors(None, Some(run_id), None)
            .with_fs_access(atman_runtime::fs_access::FsAccessPolicy::workspace_write(
                root.path().to_path_buf(),
            ));
        ctx.flow_identity = Some(identity);
        let args = ToolArgs {
            positional: Vec::new(),
            named: vec![
                ("cmd".into(), Value::Str("sleep 30".into())),
                ("rows".into(), Value::Int(12)),
                ("cols".into(), Value::Int(40)),
            ],
        };
        let call_ctx = atman_runtime::approval::authorize_tool_invocation(
            &ctx.for_tool_invocation(Tier::Four),
            "terminal-resize-call",
            "term.spawn",
            &args,
            &TermSpawn,
        )
        .await
        .unwrap();
        let Value::Struct(fields) = TermSpawn.call(args, &call_ctx).await.unwrap() else {
            panic!("term.spawn must return a struct")
        };
        let handle = fields
            .iter()
            .find_map(|(name, value)| match (name.as_str(), value) {
                ("handle", Value::Str(handle)) => Some(handle.clone()),
                _ => None,
            })
            .unwrap();
        let entry = terminal_registry.get(&handle).unwrap();
        let task_id = entry.task_id.lock().unwrap().clone().unwrap();
        let resource_id = atman_proto::ResourceId::task(task_id.0);

        let request = atman_proto::ResizeTerminalResourceRequest {
            request_id: Some(atman_proto::RequestId::now()),
            session_id: session_id.clone(),
            resource_id: resource_id.clone(),
            rows: 42,
            cols: 120,
        };
        let resized = crate::dispatch_as(
            state.clone(),
            atman_proto::JsonRpcRequest::for_method::<atman_proto::rpc::ResizeTerminalResource>(
                1, &request,
            )
            .unwrap(),
            "owner",
        )
        .await
        .into_method_output::<atman_proto::rpc::ResizeTerminalResource>()
        .unwrap();
        assert_eq!(resized.status, atman_proto::TerminalResizeStatus::Resized);
        assert_eq!((entry.snapshot().rows, entry.snapshot().cols), (42, 120));
        let retry = crate::dispatch_as(
            state.clone(),
            atman_proto::JsonRpcRequest::for_method::<atman_proto::rpc::ResizeTerminalResource>(
                2, &request,
            )
            .unwrap(),
            "owner",
        )
        .await
        .into_method_output::<atman_proto::rpc::ResizeTerminalResource>()
        .unwrap();
        assert_eq!(retry.cursor, resized.cursor);
        let snapshot = state.session_snapshot(&session_id, "owner").await.unwrap();
        let resource = snapshot
            .projection
            .resources
            .iter()
            .find(|resource| resource.id == resource_id)
            .unwrap();
        assert_eq!(resource.details["rows"], "42");
        assert_eq!(resource.details["cols"], "120");
        assert_eq!(snapshot.cursor, resized.cursor);

        terminal_registry.kill_all();
    }

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
                        let projection = SessionProjector::from_events(
                            projection_session_id,
                            session.meta(),
                            &[],
                        );
                        Ok(LoadedSession {
                            projection: RestoredProjection {
                                event_cursor: EventCursor(projection.projection().revision.0),
                                projector: projection,
                            },
                            session,
                        })
                    })
                    .await
            }
        };

        let (first, second) = tokio::join!(load(state.clone()), load(state));
        let first = first.unwrap();
        let second = second.unwrap();

        assert!(Arc::ptr_eq(
            &first.runtime_session(),
            &second.runtime_session()
        ));
        assert_eq!(load_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn clients_attached_to_one_session_share_the_runtime_slot() {
        let state = Arc::new(DaemonState::new(
            tempfile::tempdir().unwrap().path().to_path_buf(),
        ));
        let session = Arc::new(atman_runtime::Session::open_ephemeral());
        let session_id = SessionId(session.id().0);
        state
            .register_session(session_id.clone(), session, "owner")
            .await
            .unwrap();

        let first = state.session_runtime_slot(&session_id, "owner").unwrap();
        let second = state.session_runtime_slot(&session_id, "owner").unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert!(first.current_host().is_none());
    }

    #[tokio::test]
    async fn idle_unload_waits_for_clients_and_managed_tasks() {
        let state = Arc::new(DaemonState::new(
            tempfile::tempdir().unwrap().path().to_path_buf(),
        ));
        let session = Arc::new(atman_runtime::Session::open_ephemeral());
        let session_id = SessionId(session.id().0);
        state
            .register_session(session_id.clone(), session, "owner")
            .await
            .unwrap();

        let lease = state
            .loaded_runtime_session(&session_id, "owner")
            .unwrap()
            .unwrap();
        assert!(!state.unload_session_if_idle(&session_id).await.unwrap());
        drop(lease);

        let (updates, _) = state
            .subscribe_session_updates(&session_id, "owner")
            .await
            .unwrap()
            .unwrap();
        assert!(!state.unload_session_if_idle(&session_id).await.unwrap());
        drop(updates);

        let task_id = state.task_registry().register(
            atman_runtime::TaskKind::Bash,
            "inspect workspace".into(),
            "bg-1".into(),
            atman_runtime::TaskOwner::new(session_id.to_string(), None),
            CancellationToken::new(),
        );
        assert!(!state.unload_session_if_idle(&session_id).await.unwrap());
        state
            .task_registry()
            .finish(&task_id, atman_runtime::TaskStatus::Ok);

        assert!(state.unload_session_if_idle(&session_id).await.unwrap());
        assert!(state.session_revision(&session_id).is_none());
    }

    #[tokio::test]
    async fn idle_unload_persists_a_replayable_projection_snapshot() {
        let data_dir = tempfile::tempdir().unwrap();
        let state = Arc::new(DaemonState::new(data_dir.path().to_path_buf()));
        let session = Arc::new(atman_runtime::Session::open(data_dir.path()).unwrap());
        let session_id = SessionId(session.id().0);
        let turn_id = atman_runtime::event::TurnId::now();
        session.append_message(
            atman_runtime::message::Message::user_text(turn_id, "persisted transcript"),
            None,
        );
        state
            .register_session(session_id.clone(), session, "owner")
            .await
            .unwrap();
        let live = state.session_snapshot(&session_id, "owner").await.unwrap();

        assert!(state.unload_session_if_idle(&session_id).await.unwrap());
        assert!(
            data_dir
                .path()
                .join("sessions")
                .join(session_id.to_string())
                .join(".projection-snapshots")
                .is_dir()
        );
        let restored = state.session_snapshot(&session_id, "owner").await.unwrap();
        assert_eq!(restored.projection.transcript, live.projection.transcript);
        assert_eq!(restored.projection.usage, live.projection.usage);
    }

    #[tokio::test]
    async fn attach_snapshot_and_updates_share_one_restored_actor() {
        let root = tempfile::tempdir().unwrap();
        let project_root = root.path().join("project");
        let config_dir = root.path().join("config");
        let data_dir = root.path().join("data");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            "[storage]\nscope = \"global\"\n",
        )
        .unwrap();
        let launcher = Arc::new(
            crate::run::RunLauncher::new(project_root.clone(), Some(config_dir.clone()), None)
                .unwrap(),
        );
        let first_state = Arc::new(DaemonState::new(data_dir.clone()));
        first_state.set_launcher(launcher.clone());
        let session_id = launcher
            .create_session(
                first_state.clone(),
                Some(project_root.to_str().unwrap()),
                None,
                "owner",
            )
            .await
            .unwrap();
        assert!(
            first_state
                .unload_session_if_idle(&session_id)
                .await
                .unwrap()
        );
        drop(first_state);

        let state = Arc::new(DaemonState::new(data_dir));
        state.set_launcher(launcher);
        assert!(state.session_revision(&session_id).is_none());

        let snapshot = state
            .attach_session_snapshot(&session_id, "owner")
            .await
            .unwrap();
        let attached = state.authorized_actor(&session_id, "owner").unwrap();
        let subscription = state
            .subscribe_session_updates(&session_id, "owner")
            .await
            .unwrap();
        assert!(subscription.is_some());
        let updates = state
            .session_updates(&session_id, "owner", snapshot.cursor, None)
            .await
            .unwrap();
        assert!(updates.events.is_empty());
        assert!(attached.is_same_actor(&state.authorized_actor(&session_id, "owner").unwrap()));
    }

    #[tokio::test]
    async fn suggestion_install_validates_tools_and_renames_collisions() {
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let state = Arc::new(DaemonState::new(root.path().join("data")));
        state.set_launcher(Arc::new(
            crate::run::RunLauncher::new(
                root.path().to_path_buf(),
                Some(config.path().to_path_buf()),
                None,
            )
            .unwrap(),
        ));
        let session = Arc::new(atman_runtime::Session::open_ephemeral());
        let turn_id = atman_runtime::event::TurnId::now();
        session.append_message(
            atman_runtime::message::Message {
                role: atman_runtime::message::MessageRole::Assistant,
                parts: vec![atman_runtime::message::MessagePart::ToolUse {
                    id: "call".into(),
                    name: "remote.search".into(),
                    input: serde_json::json!({"query": "docs"}),
                    intent: None,
                }],
                turn_id,
                origin: atman_runtime::message::MessageOrigin::Internal,
            },
            None,
        );
        let session_id = SessionId(session.id().0);
        state
            .register_session(session_id.clone(), session, "owner")
            .await
            .unwrap();
        let source = "flow search() -> string { return remote.search(query: \"atman\") }";

        let installed = state
            .install_suggested_flow(&session_id, "search", source, "owner")
            .await
            .unwrap();
        assert_eq!(installed, "search");
        assert_eq!(
            std::fs::read_to_string(config.path().join("commands/search.at")).unwrap(),
            format!("{source}\n")
        );
        let collision = state
            .install_suggested_flow(&session_id, "search", source, "owner")
            .await
            .unwrap();
        assert_eq!(collision, "search_v2");
        let collision_source =
            std::fs::read_to_string(config.path().join("commands/search_v2.at")).unwrap();
        assert!(collision_source.contains("flow search_v2("));
        assert!(atman_dsl::parse::parse_file(&collision_source).is_ok());
        let routes = std::fs::read_to_string(config.path().join("routes.at")).unwrap();
        assert!(routes.contains("route \"search_v2 \" { flow: search_v2 }"));
        let invalid = state
            .install_suggested_flow(
                &session_id,
                "write",
                "flow write() -> string { return fs.write(\"x\", content: \"y\") }",
                "owner",
            )
            .await
            .unwrap_err();
        assert!(invalid.to_string().contains("undefined tool `fs.write`"));
        assert!(!config.path().join("commands/write.at").exists());
    }

    #[tokio::test]
    async fn snapshot_observes_a_trust_change_without_waiting_for_the_watch_loop() {
        let state = Arc::new(DaemonState::new(
            tempfile::tempdir().unwrap().path().to_path_buf(),
        ));
        let session = Arc::new(atman_runtime::Session::open_ephemeral());
        let session_id = SessionId(session.id().0);
        state
            .register_session(session_id.clone(), session.clone(), "owner")
            .await
            .unwrap();
        session
            .update_trust(
                atman_runtime::trust::TrustConfig {
                    mode: atman_runtime::trust::TrustMode::Reckless,
                    ..Default::default()
                },
                |_| Ok(()),
            )
            .unwrap();

        let snapshot = state.session_snapshot(&session_id, "owner").await.unwrap();
        assert_eq!(
            snapshot.projection.trust.mode,
            atman_proto::TrustMode::Reckless
        );
    }

    #[tokio::test]
    async fn explicit_close_reports_busy_without_interrupting_clients() {
        let state = Arc::new(DaemonState::new(
            tempfile::tempdir().unwrap().path().to_path_buf(),
        ));
        let session = Arc::new(atman_runtime::Session::open_ephemeral());
        let session_id = SessionId(session.id().0);
        state
            .register_session(session_id.clone(), session, "owner")
            .await
            .unwrap();
        let (updates, _) = state
            .subscribe_session_updates(&session_id, "owner")
            .await
            .unwrap()
            .unwrap();

        let response = state.close_session(&session_id, "owner").await.unwrap();

        assert_eq!(response.status, SessionCloseStatus::Busy);
        assert!(state.session_revision(&session_id).is_some());
        drop(updates);
    }

    #[tokio::test]
    async fn idle_sweep_evicts_only_quiescent_session_actors() {
        let state = Arc::new(DaemonState::new(
            tempfile::tempdir().unwrap().path().to_path_buf(),
        ));
        let idle = Arc::new(atman_runtime::Session::open_ephemeral());
        let idle_id = SessionId(idle.id().0);
        state
            .register_session(idle_id.clone(), idle, "owner")
            .await
            .unwrap();
        let attached = Arc::new(atman_runtime::Session::open_ephemeral());
        let attached_id = SessionId(attached.id().0);
        state
            .register_session(attached_id.clone(), attached, "owner")
            .await
            .unwrap();
        let (updates, _) = state
            .subscribe_session_updates(&attached_id, "owner")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            state.evict_idle_sessions(std::time::Duration::ZERO).await,
            1
        );
        assert!(state.session_revision(&idle_id).is_none());
        assert!(state.session_revision(&attached_id).is_some());

        drop(updates);
        assert_eq!(
            state.evict_idle_sessions(std::time::Duration::ZERO).await,
            1
        );
        assert!(state.session_revision(&attached_id).is_none());
    }

    #[tokio::test]
    async fn shutdown_drains_idle_actors_and_forces_stuck_runs() {
        let state = Arc::new(DaemonState::new(
            tempfile::tempdir().unwrap().path().to_path_buf(),
        ));
        let idle = Arc::new(atman_runtime::Session::open_ephemeral());
        let idle_id = SessionId(idle.id().0);
        state
            .register_session(idle_id, idle, "owner")
            .await
            .unwrap();

        let active = Arc::new(atman_runtime::Session::open_ephemeral());
        let active_id = SessionId(active.id().0);
        let cancel = CancellationToken::new();
        state
            .register_session_run(
                active_id,
                active,
                LiveRun {
                    run_id: FlowRunId(uuid::Uuid::now_v7()),
                    turn_id: atman_runtime::event::TurnId::now(),
                    flow_name: "stuck".into(),
                    cancel: cancel.clone(),
                    started_at: chrono::Utc::now(),
                },
                "owner",
            )
            .await
            .unwrap();

        let report = state.shutdown(std::time::Duration::from_millis(400)).await;

        assert!(!state.is_accepting_commands());
        assert!(cancel.is_cancelled());
        assert_eq!(report.graceful, 1);
        assert_eq!(report.forced, 1);
        assert_eq!(report.remaining, 0);
    }
}
